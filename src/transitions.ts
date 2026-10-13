/**
 * Explicit Jira status transitions: fresh lookup → picker → final confirm →
 * site+issue lock → re-read and tuple check → durable Pending → single POST.
 *
 * Jira REST has no conditional transition, so the final read→write race with
 * external Jira actors is narrowed (lock + re-read) but not eliminated.
 */
import { createHash, randomUUID } from "node:crypto";
import type {
	Config,
	ExtensionCommandContext,
	Issue,
	JiraPort,
	StateStore,
	StatusRef,
	Transition,
	TransitionResult,
} from "./contracts.ts";
import { normalizeIssueKey } from "./contracts.ts";
import { isRecord, sanitizeForTerminal } from "./text.ts";

/** The subset of the command context the transition flow uses. */
export type TransitionContext = {
	hasUI: ExtensionCommandContext["hasUI"];
	ui: Pick<ExtensionCommandContext["ui"], "select" | "confirm" | "notify">;
};

/** Assignable to `Services['runTransition']`. */
export type TransitionRunner = (key: string, ctx: TransitionContext, signal: AbortSignal) => Promise<void>;

type RecordState = "pending" | "acknowledged" | "rejected" | "unknown";

type TransitionRecord = {
	version: 1;
	attempt: string;
	key: string;
	site: string;
	transitionId: string;
	fromStatusId: string;
	toStatusId: string;
	state: RecordState;
	startedAt: string;
};

/** Approved tuple; any difference on re-read discards the approval. */
type Approval = {
	from: StatusRef;
	transition: { id: string; name: string };
	to: StatusRef;
	eligibility: { isAvailable: boolean | undefined; hasScreen: boolean; requiresFields: boolean };
};

const RECORD_STATES: readonly RecordState[] = ["pending", "acknowledged", "rejected", "unknown"];
const MESSAGE_LIMIT = 300;

export function createTransitionService(config: Config, jira: JiraPort, state: StateStore): TransitionRunner {
	const site = config.jira.baseUrl;
	/** Writes that outlive the caller's UI; each promise settles on its own (never rejects). */
	const inflight = new Set<Promise<void>>();

	function notify(ctx: TransitionContext, message: string, type: "info" | "warning" | "error"): void {
		try {
			ctx.ui.notify(message, type);
		} catch {
			// The UI may already be gone; the durable record carries the outcome.
		}
	}

	/** Bounded fresh GET that is independent of the caller's (browser) signal. */
	async function observeStatus(key: string): Promise<StatusRef | undefined> {
		const controller = new AbortController();
		const timer = setTimeout(() => controller.abort(new Error("status lookup timed out")), config.jira.timeoutMs);
		try {
			return (await jira.issue(key, controller.signal)).status;
		} catch {
			return undefined;
		} finally {
			clearTimeout(timer);
		}
	}

	async function persist(ctx: TransitionContext, path: string, record: TransitionRecord): Promise<void> {
		try {
			await state.writeRecord(path, record);
		} catch (error) {
			notify(ctx, `${record.key}: 전환 기록 저장 실패 (${describe(error)}). 다음 전환 전에 Jira 이력을 확인하세요.`, "error");
		}
	}

	/** POST once with an own timeout controller, then report and record the outcome. Never throws. */
	async function executeWrite(ctx: TransitionContext, path: string, pending: TransitionRecord, approval: Approval) {
		const controller = new AbortController();
		const timer = setTimeout(() => controller.abort(new Error("transition request timed out")), config.jira.timeoutMs);
		let result: TransitionResult;
		try {
			result = await jira.postTransition(pending.key, pending.transitionId, controller.signal);
		} catch (error) {
			result = { state: "unknown", message: describe(error) };
		} finally {
			clearTimeout(timer);
		}

		const key = pending.key;
		const target = `${clean(approval.to.name)} (${clean(approval.to.id)})`;
		if (result.state === "acknowledged") {
			const observed = await observeStatus(key);
			await persist(ctx, path, { ...pending, state: "acknowledged" });
			if (observed !== undefined && observed.id === approval.to.id) {
				notify(ctx, `${key}: 전환 완료 확인 — 현재 상태 ${statusText(observed)}`, "info");
			} else {
				const seen = observed === undefined ? "조회 실패" : statusText(observed);
				notify(
					ctx,
					`${key}: 서버가 전환 요청 수락, 현재 상태 확인 불가/다름 (기대 ${target}, 현재 ${seen}). 재전송하지 않습니다.`,
					"warning",
				);
			}
			return;
		}
		if (result.state === "rejected") {
			await persist(ctx, path, { ...pending, state: "rejected" });
			notify(ctx, `${key}: Jira가 전환을 거부했습니다 — ${clip(result.message)}`, "error");
			return;
		}
		await persist(ctx, path, { ...pending, state: "unknown" });
		const observed = result.observedStatus ?? (await observeStatus(key));
		const seen = observed === undefined ? "조회 실패" : statusText(observed);
		notify(
			ctx,
			`${key}: 전환 결과 불확실 (${clip(result.message)}). 현재 상태 관측: ${seen} — 요청 성공/미실행의 증거가 아닙니다. 자동 재시도하지 않습니다. Jira 이력을 확인하세요: ${pending.site}`,
			"warning",
		);
	}

	/**
	 * Shows the uncertain record with a fresh status and asks for explicit
	 * confirmation that Jira history was inspected.
	 */
	async function confirmInspected(
		ctx: TransitionContext,
		record: TransitionRecord,
		key: string,
		signal: AbortSignal,
	): Promise<boolean> {
		const issue = await jira.issue(key, signal);
		const label = record.state === "pending" ? "완료되지 않은 전환 요청(Pending)" : "결과 불확실(unknown)";
		const message = [
			`이전 ${label} 기록이 있어 자동 전환을 막았습니다.`,
			`사이트: ${clean(record.site)}`,
			`이슈: ${key} — ${clean(issue.url)}`,
			`요청: 전환 #${clean(record.transitionId)} (${clean(record.fromStatusId)} → ${clean(record.toStatusId)}), 시작 ${clean(record.startedAt)}`,
			`현재 상태 관측: ${statusText(issue.status)}`,
			"Jira 웹에서 이슈 이력을 직접 확인했으면 계속하여 새 전환을 선택하세요.",
		].join("\n");
		return ctx.ui.confirm("Jira 이력을 확인했습니까?", message, { signal });
	}

	async function pick(
		ctx: TransitionContext,
		issue: Issue,
		transitions: Transition[],
		signal: AbortSignal,
	): Promise<Transition | undefined> {
		const byLabel = new Map<string, Transition>();
		const options = transitions.map((t) => {
			const label = `${clean(t.name)} → ${clean(t.to.name)} [#${clean(t.id)}]${suffix(t)}`;
			byLabel.set(label, t);
			return { label, description: `→ ${statusText(t.to)}` };
		});
		const title = `${issue.key} 상태 전환 (현재 ${statusText(issue.status)})`;
		for (;;) {
			const choice = await ctx.ui.select(title, options, { signal });
			if (choice === undefined) return undefined;
			const chosen = byLabel.get(choice);
			if (chosen === undefined) return undefined;
			if (chosen.isAvailable === false) {
				notify(ctx, `'${clean(chosen.name)}' 전환은 현재 Jira에서 사용할 수 없습니다. 다른 전환을 선택하세요.`, "warning");
				continue;
			}
			return chosen;
		}
	}

	/** One explicit action: returns when done, cancelled, or blocked. */
	async function runOnce(ctx: TransitionContext, key: string, signal: AbortSignal): Promise<void> {
		const digest = createHash("sha256").update(JSON.stringify([site, key])).digest("hex");
		const path = `transitions/${digest}.json`;
		const existing = await state.readRecord(path, parseRecord);
		let inspectedAttempt: string | undefined;
		if (existing !== undefined && (existing.state === "pending" || existing.state === "unknown")) {
			if (!(await confirmInspected(ctx, existing, key, signal))) {
				notify(ctx, `${key}: 이전 전환 결과가 불확실하여 새 전환 요청을 보내지 않았습니다.`, "warning");
				return;
			}
			inspectedAttempt = existing.attempt;
		}

		for (;;) {
			const [issue, transitions] = await Promise.all([jira.issue(key, signal), jira.transitions(key, signal)]);
			if (transitions.length === 0) {
				notify(ctx, `${key}: 현재 상태(${statusText(issue.status)})에서 가능한 전환이 없습니다.`, "info");
				return;
			}
			const chosen = await pick(ctx, issue, transitions, signal);
			if (chosen === undefined) return;
			if (chosen.hasScreen || chosen.requiresFields) {
				const reason = chosen.hasScreen ? "전환 입력 화면이 필요합니다" : "필수 입력 필드가 있습니다";
				notify(
					ctx,
					`'${clean(chosen.name)}' 전환은 ${reason}. Jira 웹에서 진행하세요: ${clean(issue.url)}`,
					"info",
				);
				return;
			}
			const approval = freezeApproval(issue, chosen);

			const confirmed = await ctx.ui.confirm(
				"Jira 상태 전환",
				[
					`사이트: ${clean(site)}`,
					`이슈: ${key}`,
					`상태: ${statusText(approval.from)} → ${statusText(approval.to)}`,
					`전환: ${clean(approval.transition.name)} (id ${clean(approval.transition.id)})`,
					"확인하면 이 전환 요청을 한 번 보냅니다. 다른 필드는 변경하지 않습니다.",
				].join("\n"),
				{ signal },
			);
			if (!confirmed) return;

			let started = false;
			const locked = state.withLocks([`transition:${site}:${key}`], async (): Promise<"stale" | "blocked" | "done"> => {
				const current = await state.readRecord(path, parseRecord);
				const uncertain = current !== undefined && (current.state === "pending" || current.state === "unknown");
				if (uncertain && current.attempt !== inspectedAttempt) return "blocked";
				const [freshIssue, freshTransitions] = await Promise.all([
					jira.issue(key, signal),
					jira.transitions(key, signal),
				]);
				const fresh = freshTransitions.find((t) => t.id === approval.transition.id);
				if (fresh === undefined || !sameApproval(approval, freezeApproval(freshIssue, fresh))) return "stale";

				const pending: TransitionRecord = {
					version: 1,
					attempt: randomUUID(),
					key,
					site,
					transitionId: approval.transition.id,
					fromStatusId: approval.from.id,
					toStatusId: approval.to.id,
					state: "pending",
					startedAt: new Date().toISOString(),
				};
				await state.writeRecord(path, pending);
				started = true;
				await executeWrite(ctx, path, pending, approval);
				return "done";
			});
			const tracked = locked.then(
				() => undefined,
				(error: unknown) => {
					if (started) notify(ctx, `${key}: 전환 잠금 해제 실패 (${describe(error)})`, "error");
				},
			);
			inflight.add(tracked);
			void tracked.finally(() => inflight.delete(tracked));

			const outcome = await settleOrAbort(locked, signal);
			if (outcome.kind === "aborted") {
				if (started) notify(ctx, `${key}: 전환 요청은 취소되지 않고 계속 진행 중입니다. 결과는 알림으로 표시됩니다.`, "info");
				return;
			}
			if (outcome.kind === "error") {
				if (started) return; // reported by `tracked`
				throw outcome.error;
			}
			if (outcome.value === "stale") {
				notify(ctx, `${key}: 확인 이후 상태나 전환 목록이 바뀌어 승인을 폐기했습니다. 다시 선택하세요.`, "warning");
				continue;
			}
			if (outcome.value === "blocked") {
				notify(ctx, `${key}: 다른 전환 요청 기록이 생겨 요청을 보내지 않았습니다. Jira 이력을 확인하세요.`, "warning");
			}
			return;
		}
	}

	return async (rawKey, ctx, signal) => {
		if (!ctx.hasUI) throw new Error("Jira 상태 전환에는 interactive TUI가 필요합니다");
		const key = normalizeIssueKey(rawKey);
		try {
			await runOnce(ctx, key, signal);
		} catch (error) {
			if (signal.aborted) return;
			notify(ctx, `${key}: 상태 전환을 진행하지 못했습니다 — ${describe(error)}`, "error");
		}
	};
}

type Settled<T> = { kind: "value"; value: T } | { kind: "error"; error: unknown } | { kind: "aborted" };

/** Waits for `promise` unless `signal` aborts first; never cancels `promise`. */
async function settleOrAbort<T>(promise: Promise<T>, signal: AbortSignal): Promise<Settled<T>> {
	const { promise: aborted, resolve } = Promise.withResolvers<Settled<T>>();
	const onAbort = () => resolve({ kind: "aborted" });
	if (signal.aborted) onAbort();
	else signal.addEventListener("abort", onAbort, { once: true });
	try {
		return await Promise.race([
			promise.then(
				(value): Settled<T> => ({ kind: "value", value }),
				(error: unknown): Settled<T> => ({ kind: "error", error }),
			),
			aborted,
		]);
	} finally {
		signal.removeEventListener("abort", onAbort);
	}
}

function freezeApproval(issue: Issue, t: Transition): Approval {
	return Object.freeze({
		from: Object.freeze({ id: issue.status.id, name: issue.status.name }),
		transition: Object.freeze({ id: t.id, name: t.name }),
		to: Object.freeze({ id: t.to.id, name: t.to.name }),
		eligibility: Object.freeze({ isAvailable: t.isAvailable, hasScreen: t.hasScreen, requiresFields: t.requiresFields }),
	});
}

function sameApproval(a: Approval, b: Approval): boolean {
	return (
		a.from.id === b.from.id &&
		a.from.name === b.from.name &&
		a.transition.id === b.transition.id &&
		a.transition.name === b.transition.name &&
		a.to.id === b.to.id &&
		a.to.name === b.to.name &&
		a.eligibility.isAvailable === b.eligibility.isAvailable &&
		a.eligibility.hasScreen === b.eligibility.hasScreen &&
		a.eligibility.requiresFields === b.eligibility.requiresFields
	);
}

function parseRecord(value: unknown): TransitionRecord {
	if (!isRecord(value) || value.version !== 1) throw new Error("transition record: unsupported shape or version");
	const text = (field: string): string => {
		const v = value[field];
		if (typeof v !== "string" || v === "") throw new Error(`transition record: invalid ${field}`);
		return v;
	};
	const recordState = value.state;
	const match = RECORD_STATES.find((s) => s === recordState);
	if (match === undefined) throw new Error("transition record: invalid state");
	return {
		version: 1,
		attempt: text("attempt"),
		key: text("key"),
		site: text("site"),
		transitionId: text("transitionId"),
		fromStatusId: text("fromStatusId"),
		toStatusId: text("toStatusId"),
		state: match,
		startedAt: text("startedAt"),
	};
}

function suffix(t: Transition): string {
	if (t.isAvailable === false) return " — 선택 불가";
	if (t.hasScreen || t.requiresFields) return " — Jira 웹에서 진행";
	return "";
}

function clean(text: string): string {
	return sanitizeForTerminal(text).replace(/\s+/g, " ").trim();
}

function statusText(status: StatusRef): string {
	return `${clean(status.name)} (${clean(status.id)})`;
}

function clip(text: string): string {
	const cleaned = clean(text);
	return cleaned.length > MESSAGE_LIMIT ? `${cleaned.slice(0, MESSAGE_LIMIT)}…` : cleaned;
}

function describe(error: unknown): string {
	return clip(error instanceof Error ? error.message : String(error));
}
