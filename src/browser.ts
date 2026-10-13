/**
 * `/jira` browser: issue list, detail, and the checkout/omp wizard.
 *
 * `runBrowser` is an outer intent loop. Exactly one custom overlay is mounted
 * at a time; whenever a modal step is needed (input/select/editor/confirm,
 * wizard screens, transitions, terminal launch) the overlay returns an intent
 * through `done(intent)` and unmounts, the loop awaits that step, and then
 * remounts the browser with its preserved list/detail state. All state lives
 * in per-call objects; nothing is module-global.
 */
import { homedir } from "node:os";
import { isAbsolute, join, resolve } from "node:path";
import { getSelectListTheme, matchesKey, SelectList, type Component, type KeyId, type TUI } from "@oh-my-pi/pi-tui";
import { RecoveryRequiredError } from "./checkouts.ts";
import {
	DEFAULT_INSTRUCTION,
	normalizeIssueKey,
	type Checkout,
	type CheckoutPlan,
	type CheckoutSelection,
	type Comment,
	type ExtensionCommandContext,
	type Issue,
	type LaunchResult,
	type Page,
	type PreparedCheckout,
	type Repository,
	type Services,
	type Snapshot,
} from "./contracts.ts";
import {
	actionLabel,
	buildSearchJql,
	type CheckoutAction,
	type DetailFocus,
	errorMessage,
	filterChoices,
	finalConfirmBody,
	fit,
	GenerationLoader,
	initialQuery,
	IssueListModel,
	launchResultBody,
	listVisibleRows,
	type Paint,
	type QueryRef,
	renderDetail,
	renderList,
	renderTextScreen,
	singleLine,
	snapshotPreviewBody,
	textScreenRows,
	uniqueLabels,
	wrapText,
} from "./ui.ts";

const OVERLAY = { overlay: true } as const;
const MIN_ROWS = 8;

/**
 * Rows available to a full-height overlay. The host frame keeps the last terminal row for the
 * hardware cursor, so rendering all `rows` lines pushes the bottom (help) line off screen.
 */
function screenRows(tui: TUI): number {
	return Math.max(MIN_ROWS, tui.terminal.rows - 1);
}

// ---------------------------------------------------------------------------
// Browser state (list + detail), preserved across overlay remounts

type Intent =
	| { kind: "close" }
	| { kind: "search" }
	| { kind: "jql" }
	| { kind: "filter" }
	| { kind: "wizard"; issueKey: string; omp: boolean }
	| { kind: "transition"; issueKey: string };

type DetailState = {
	key: string;
	issue: GenerationLoader<Issue>;
	/** Undefined when `handoff.maxComments` is 0 (comments are not fetched). */
	comments: GenerationLoader<Page<Comment>> | undefined;
	focus: DetailFocus;
	descriptionScroll: number;
	commentsScroll: number;
	descriptionRows: number;
	commentsRows: number;
	notice: string | undefined;
};

class BrowserSession {
	readonly list: IssueListModel;
	detail: DetailState | undefined;
	screen: "list" | "detail" = "list";
	listNotice: string | undefined;
	listPageRows = 1;
	readonly #services: Services;
	#render: () => void = () => {};

	constructor(services: Services) {
		this.#services = services;
		this.list = new IssueListModel(initialQuery(services.config), () => this.#render());
	}

	readonly search = (jql: string, signal: AbortSignal): Promise<Page<Issue>> => this.#services.jira.search(jql, signal);

	/** Mount: route loader updates to the live overlay and reissue reads interrupted by the last unmount. */
	attach(render: () => void): void {
		this.#render = render;
		this.list.loader.resume();
		this.detail?.issue.resume();
		this.detail?.comments?.resume();
	}

	/** Unmount: abort in-flight reads (remembered for `attach`). */
	detach(): void {
		this.#render = () => {};
		this.list.loader.suspend();
		this.detail?.issue.suspend();
		this.detail?.comments?.suspend();
	}

	loadQuery(query: QueryRef): void {
		this.screen = "list";
		this.listNotice = undefined;
		void this.list.load(query, this.search);
	}

	refreshList(): void {
		this.listNotice = undefined;
		void this.list.refresh(this.search);
	}

	openDetail(key: string): void {
		this.#disposeDetail();
		const render = () => this.#render();
		this.detail = {
			key,
			issue: new GenerationLoader<Issue>(() => false, render),
			comments:
				this.#services.config.handoff.maxComments > 0
					? new GenerationLoader<Page<Comment>>(page => page.items.length === 0, render)
					: undefined,
			focus: "description",
			descriptionScroll: 0,
			commentsScroll: 0,
			descriptionRows: 1,
			commentsRows: 1,
			notice: undefined,
		};
		this.screen = "detail";
		this.refreshDetail();
	}

	refreshDetail(): void {
		const detail = this.detail;
		if (!detail) return;
		const { jira, config } = this.#services;
		detail.notice = undefined;
		void detail.issue.load(detail.key, signal => jira.issue(detail.key, signal));
		void detail.comments?.load(detail.key, signal => jira.comments(detail.key, config.handoff.maxComments, signal));
	}

	closeDetail(): void {
		this.#disposeDetail();
		this.screen = "list";
		if (!this.list.started) this.loadQuery(this.list.query);
	}

	/** After a status change: refetch the current query generation and the open detail. */
	refreshAfterTransition(): void {
		if (this.list.started) this.refreshList();
		this.refreshDetail();
	}

	dispose(): void {
		this.#render = () => {};
		this.list.loader.dispose();
		this.#disposeDetail();
	}

	#disposeDetail(): void {
		this.detail?.issue.dispose();
		this.detail?.comments?.dispose();
		this.detail = undefined;
	}
}

const STALE_ACTION_NOTICE = "Data is not fresh — press r and wait for a successful fetch before this action.";

class BrowserView implements Component {
	readonly #session: BrowserSession;
	readonly #tui: TUI;
	readonly #paint: Paint;
	readonly #done: (intent: Intent) => void;
	#closed = false;

	constructor(session: BrowserSession, tui: TUI, paint: Paint, done: (intent: Intent) => void) {
		this.#session = session;
		this.#tui = tui;
		this.#paint = paint;
		this.#done = done;
		session.attach(() => tui.requestRender());
	}

	render(width: number): readonly string[] {
		const session = this.#session;
		const height = screenRows(this.#tui);
		const detail = session.detail;
		if (session.screen === "detail" && detail) {
			const out = renderDetail(
				{
					key: detail.key,
					issue: detail.issue.state,
					comments: detail.comments ? { kind: "loaded", load: detail.comments.state } : { kind: "skipped" },
					focus: detail.focus,
					descriptionScroll: detail.descriptionScroll,
					commentsScroll: detail.commentsScroll,
					notice: detail.notice,
				},
				width,
				height,
				this.#paint,
			);
			detail.descriptionScroll = out.descriptionScroll;
			detail.commentsScroll = out.commentsScroll;
			detail.descriptionRows = out.layout.description.rows;
			detail.commentsRows = out.layout.comments.rows;
			return out.lines;
		}
		const list = session.list;
		const out = renderList(
			{ query: list.query, load: list.loader.state, selected: list.selected, scroll: list.scroll, notice: session.listNotice },
			width,
			height,
			this.#paint,
		);
		list.scroll = out.scroll;
		session.listPageRows = listVisibleRows(width, height);
		return out.lines;
	}

	handleInput(data: string): void {
		if (this.#closed) return;
		if (this.#session.screen === "detail" && this.#session.detail) this.#detailInput(this.#session.detail, data);
		else this.#listInput(data);
		if (!this.#closed) this.#tui.requestRender();
	}

	dispose(): void {
		this.#closed = true;
		this.#session.detach();
	}

	#finish(intent: Intent): void {
		if (this.#closed) return;
		this.#closed = true;
		this.#done(intent);
	}

	#listInput(data: string): void {
		const session = this.#session;
		const list = session.list;
		session.listNotice = undefined;
		if (matchesKey(data, "escape") || matchesKey(data, "q")) return this.#finish({ kind: "close" });
		// Shifted letters first: `J` must not fall through to `j`.
		if (matchesKey(data, "shift+j")) return this.#finish({ kind: "jql" });
		if (matchesKey(data, "shift+g") || matchesKey(data, "end")) return list.move(Number.MAX_SAFE_INTEGER);
		if (matchesKey(data, "up") || matchesKey(data, "k")) return list.move(-1);
		if (matchesKey(data, "down") || matchesKey(data, "j")) return list.move(1);
		if (matchesKey(data, "pageUp")) return list.move(-session.listPageRows);
		if (matchesKey(data, "pageDown")) return list.move(session.listPageRows);
		if (matchesKey(data, "home") || matchesKey(data, "g")) return list.move(-Number.MAX_SAFE_INTEGER);
		if (matchesKey(data, "/")) return this.#finish({ kind: "search" });
		if (matchesKey(data, "f")) return this.#finish({ kind: "filter" });
		if (matchesKey(data, "r")) return session.refreshList();
		const issue = list.selectedIssue;
		if (matchesKey(data, "enter")) {
			if (issue) session.openDetail(issue.key);
			return;
		}
		const action = matchesKey(data, "w") ? "w" : matchesKey(data, "d") ? "d" : matchesKey(data, "s") ? "s" : undefined;
		if (!action) return;
		if (!issue) {
			session.listNotice = "No issue selected.";
			return;
		}
		if (!list.loader.fresh) {
			session.listNotice = STALE_ACTION_NOTICE;
			return;
		}
		this.#finish(
			action === "s"
				? { kind: "transition", issueKey: issue.key }
				: { kind: "wizard", issueKey: issue.key, omp: action === "d" },
		);
	}

	#detailInput(detail: DetailState, data: string): void {
		detail.notice = undefined;
		if (matchesKey(data, "escape") || matchesKey(data, "q")) return this.#session.closeDetail();
		if (matchesKey(data, "tab") || matchesKey(data, "shift+tab")) {
			detail.focus = detail.focus === "description" ? "comments" : "description";
			return;
		}
		const page = detail.focus === "description" ? detail.descriptionRows : detail.commentsRows;
		const delta = matchesKey(data, "shift+g") || matchesKey(data, "end")
			? Number.MAX_SAFE_INTEGER
			: matchesKey(data, "up") || matchesKey(data, "k")
				? -1
				: matchesKey(data, "down") || matchesKey(data, "j")
					? 1
					: matchesKey(data, "pageUp")
						? -Math.max(1, page - 1)
						: matchesKey(data, "pageDown")
							? Math.max(1, page - 1)
							: matchesKey(data, "home") || matchesKey(data, "g")
								? -Number.MAX_SAFE_INTEGER
								: 0;
		if (delta !== 0) {
			// render() clamps against the wrapped content height.
			if (detail.focus === "description") detail.descriptionScroll = Math.max(0, detail.descriptionScroll + delta);
			else detail.commentsScroll = Math.max(0, detail.commentsScroll + delta);
			return;
		}
		if (matchesKey(data, "r")) return this.#session.refreshDetail();
		const action = matchesKey(data, "w") ? "w" : matchesKey(data, "d") ? "d" : matchesKey(data, "s") ? "s" : undefined;
		if (!action) return;
		if (!detail.issue.fresh) {
			detail.notice = STALE_ACTION_NOTICE;
			return;
		}
		this.#finish(
			action === "s"
				? { kind: "transition", issueKey: detail.key }
				: { kind: "wizard", issueKey: detail.key, omp: action === "d" },
		);
	}
}

// ---------------------------------------------------------------------------
// Generic wizard screens (each is the only mounted overlay while shown)

type TextKey<R> = { keys: readonly KeyId[]; result: R };

class TextView<R> implements Component {
	readonly #tui: TUI;
	readonly #paint: Paint;
	readonly #title: string;
	readonly #body: readonly string[];
	readonly #help: string;
	readonly #keys: readonly TextKey<R>[];
	readonly #escape: R;
	readonly #done: (result: R) => void;
	#scroll = 0;
	#rows = 1;
	#closed = false;

	constructor(
		tui: TUI,
		paint: Paint,
		view: { title: string; body: readonly string[]; help: string },
		keys: readonly TextKey<R>[],
		escape: R,
		done: (result: R) => void,
	) {
		this.#tui = tui;
		this.#paint = paint;
		this.#title = view.title;
		this.#body = view.body;
		this.#help = view.help;
		this.#keys = keys;
		this.#escape = escape;
		this.#done = done;
	}

	render(width: number): readonly string[] {
		const height = screenRows(this.#tui);
		const out = renderTextScreen(
			{ title: this.#title, body: this.#body, help: this.#help, scroll: this.#scroll },
			width,
			height,
			this.#paint,
		);
		this.#scroll = out.scroll;
		this.#rows = textScreenRows(height);
		return out.lines;
	}

	handleInput(data: string): void {
		if (this.#closed) return;
		if (matchesKey(data, "escape")) return this.#close(this.#escape);
		for (const entry of this.#keys) {
			if (entry.keys.some(key => matchesKey(data, key))) return this.#close(entry.result);
		}
		if (matchesKey(data, "up") || matchesKey(data, "k")) this.#scroll = Math.max(0, this.#scroll - 1);
		else if (matchesKey(data, "down") || matchesKey(data, "j")) this.#scroll += 1;
		else if (matchesKey(data, "pageUp")) this.#scroll = Math.max(0, this.#scroll - Math.max(1, this.#rows - 1));
		else if (matchesKey(data, "pageDown")) this.#scroll += Math.max(1, this.#rows - 1);
		else if (matchesKey(data, "home") || matchesKey(data, "g")) this.#scroll = 0;
		else if (matchesKey(data, "end") || matchesKey(data, "shift+g")) this.#scroll = Number.MAX_SAFE_INTEGER;
		else return;
		this.#tui.requestRender();
	}

	dispose(): void {
		this.#closed = true;
	}

	#close(result: R): void {
		this.#closed = true;
		this.#done(result);
	}
}

function showText<R>(
	ctx: ExtensionCommandContext,
	view: { title: string; body: readonly string[]; help: string },
	keys: readonly TextKey<R>[],
	escape: R,
): Promise<R> {
	return ctx.ui.custom<R>((tui, theme, _keybindings, done) => new TextView(tui, theme, view, keys, escape, done), OVERLAY);
}

/** Error/notice screen; any of Enter/Esc returns. */
async function showMessage(ctx: ExtensionCommandContext, title: string, body: readonly string[]): Promise<void> {
	await showText<undefined>(ctx, { title, body, help: "Enter/Esc continue" }, [{ keys: ["enter"], result: undefined }], undefined);
}

type PickItem<T> = { label: string; description?: string; value: T; disabled?: boolean };

class PickView<T> implements Component {
	readonly #tui: TUI;
	readonly #paint: Paint;
	readonly #title: string;
	readonly #info: readonly string[];
	readonly #items: readonly PickItem<T>[];
	readonly #list: SelectList;
	readonly #done: (value: T | undefined) => void;
	#closed = false;

	constructor(
		tui: TUI,
		paint: Paint,
		title: string,
		info: readonly string[],
		items: readonly PickItem<T>[],
		initialIndex: number,
		done: (value: T | undefined) => void,
	) {
		this.#tui = tui;
		this.#paint = paint;
		this.#title = title;
		this.#info = info;
		this.#items = items;
		this.#done = done;
		this.#list = new SelectList(
			items.map((item, index) => ({
				value: String(index),
				label: singleLine(item.label),
				...(item.description === undefined ? {} : { description: singleLine(item.description) }),
				...(item.disabled ? { disabled: true } : {}),
			})),
			8,
			getSelectListTheme(),
			{ search: "never" },
		);
		this.#list.setSelectedIndex(initialIndex);
		this.#list.onSelect = selected => {
			const item = this.#items[Number(selected.value)];
			if (item && !item.disabled) this.#close(item.value);
		};
		this.#list.onCancel = () => this.#close(undefined);
	}

	render(width: number): readonly string[] {
		const height = screenRows(this.#tui);
		const info = this.#info.flatMap(line => {
			const rows = wrapText(line, width);
			return rows.length > 0 ? rows : [""];
		});
		const infoRows = info.slice(0, Math.max(0, Math.floor(height / 2) - 2));
		this.#list.setMaxVisible(Math.max(3, height - infoRows.length - 4));
		const lines = [
			this.#paint.bold(fit(singleLine(this.#title), width)),
			...infoRows,
			"",
			...this.#list.render(width).map(line => fit(line, width)),
			this.#paint.fg("muted", fit("↑/↓ or j/k move · Enter select · Esc back", width)),
		];
		return lines.slice(0, height);
	}

	handleInput(data: string): void {
		if (this.#closed) return;
		if (matchesKey(data, "escape")) return this.#close(undefined);
		if (matchesKey(data, "j")) this.#list.handleInput("\u001b[B");
		else if (matchesKey(data, "k")) this.#list.handleInput("\u001b[A");
		else this.#list.handleInput(data);
		if (!this.#closed) this.#tui.requestRender();
	}

	invalidate(): void {
		this.#list.invalidate();
	}

	dispose(): void {
		this.#closed = true;
	}

	#close(value: T | undefined): void {
		if (this.#closed) return;
		this.#closed = true;
		this.#done(value);
	}
}

function pick<T>(
	ctx: ExtensionCommandContext,
	title: string,
	info: readonly string[],
	items: readonly PickItem<T>[],
	initialIndex = 0,
): Promise<T | undefined> {
	return ctx.ui.custom<T | undefined>(
		(tui, theme, _keybindings, done) => new PickView(tui, theme, title, info, items, initialIndex, done),
		OVERLAY,
	);
}

type Outcome<T> = { kind: "ok"; value: T } | { kind: "error"; error: unknown } | { kind: "cancelled" };

/** Progress line for one async step. `cancel` is set only for read-only steps; unmount also cancels them. */
class ProgressView implements Component {
	readonly #paint: Paint;
	readonly #label: string;
	readonly #cancel: (() => void) | undefined;

	constructor(paint: Paint, label: string, cancel: (() => void) | undefined) {
		this.#paint = paint;
		this.#label = label;
		this.#cancel = cancel;
	}

	render(width: number): readonly string[] {
		return [
			this.#paint.bold(fit(singleLine(this.#label), width)),
			this.#paint.fg(
				"muted",
				fit(this.#cancel ? "Esc cancel (read-only step)" : "Writing — this step cannot be cancelled; please wait.", width),
			),
		];
	}

	handleInput(data: string): void {
		if (this.#cancel && matchesKey(data, "escape")) this.#cancel();
	}

	dispose(): void {
		this.#cancel?.();
	}
}

/**
 * Run `task` behind a progress overlay. Read-only steps are cancellable (Esc
 * or unmount aborts the signal). Write steps get a signal the UI never aborts,
 * so dismissal can never be mistaken for success or cancellation.
 */
function withProgress<T>(
	ctx: ExtensionCommandContext,
	label: string,
	cancellable: boolean,
	task: (signal: AbortSignal) => Promise<T>,
): Promise<Outcome<T>> {
	return ctx.ui.custom<Outcome<T>>((_tui, theme, _keybindings, done) => {
		const controller = new AbortController();
		let settled = false;
		const settle = (outcome: Outcome<T>) => {
			if (settled) return;
			settled = true;
			done(outcome);
		};
		let pending: Promise<T>;
		try {
			pending = task(controller.signal);
		} catch (error) {
			pending = Promise.reject(error);
		}
		void pending.then(
			value => settle(controller.signal.aborted ? { kind: "cancelled" } : { kind: "ok", value }),
			error => settle(controller.signal.aborted ? { kind: "cancelled" } : { kind: "error", error }),
		);
		return new ProgressView(
			theme,
			label,
			cancellable
				? () => {
						controller.abort();
						settle({ kind: "cancelled" });
					}
				: undefined,
		);
	}, OVERLAY);
}

// ---------------------------------------------------------------------------
// Checkout / omp wizard

type RepoCandidate = { path: string; tags: string[]; mapped: boolean };
type RepoChoice = RepoCandidate & ({ repository: Repository; error?: undefined } | { repository?: undefined; error: string });

function expandPath(input: string, cwd: string): string {
	const trimmed = input.trim();
	if (trimmed === "~") return homedir();
	if (trimmed.startsWith("~/")) return join(homedir(), trimmed.slice(2));
	return isAbsolute(trimmed) ? trimmed : resolve(cwd, trimmed);
}

function repoCandidates(ctx: ExtensionCommandContext, services: Services, issueKey: string): RepoCandidate[] {
	const projectKey = issueKey.slice(0, issueKey.lastIndexOf("-"));
	const repos = services.config.worktree.repos;
	const candidates: RepoCandidate[] = [{ path: ctx.cwd, tags: ["current directory"], mapped: false }];
	const mappedPath = repos[projectKey];
	if (mappedPath !== undefined) candidates.push({ path: mappedPath, tags: [`mapped to ${projectKey}`], mapped: true });
	for (const [project, path] of Object.entries(repos)) {
		if (project !== projectKey) candidates.push({ path, tags: [`configured for ${project}`], mapped: false });
	}
	return candidates;
}

/** Resolve every candidate; failures stay as non-selectable entries, duplicates merge by repository key. */
async function resolveRepoChoices(
	ctx: ExtensionCommandContext,
	services: Services,
	issueKey: string,
): Promise<RepoChoice[] | undefined> {
	const prefer = services.config.worktree.prefer;
	const candidates = repoCandidates(ctx, services, issueKey);
	const outcome = await withProgress(ctx, `Resolving repositories for ${issueKey}…`, true, signal =>
		Promise.all(
			candidates.map(candidate =>
				services.checkouts.resolveRepository(candidate.path, prefer, signal).then(
					(repository): RepoChoice => ({ ...candidate, repository }),
					(error: unknown): RepoChoice => ({ ...candidate, error: errorMessage(error) }),
				),
			),
		),
	);
	if (outcome.kind !== "ok") return undefined;
	const merged: RepoChoice[] = [];
	for (const choice of outcome.value) {
		const same = choice.repository && merged.find(other => other.repository?.key === choice.repository?.key);
		if (same) {
			same.tags.push(...choice.tags);
			same.mapped ||= choice.mapped;
			continue;
		}
		merged.push({ ...choice, tags: [...choice.tags] });
	}
	return merged;
}

const MANUAL_PATH = Symbol("manual-path");

/** Step 1. Returns undefined when the user backs out of the wizard. */
async function chooseRepository(
	ctx: ExtensionCommandContext,
	services: Services,
	issueKey: string,
	choices: RepoChoice[],
): Promise<Repository | undefined> {
	for (;;) {
		const items: PickItem<RepoChoice | typeof MANUAL_PATH>[] = choices.map(choice =>
			choice.repository
				? {
						label: `${choice.mapped ? "★ " : ""}${choice.repository.name} [${choice.repository.backend}]`,
						description: `${choice.repository.root} · ${choice.tags.join(", ")}`,
						value: choice,
					}
				: {
						label: `${choice.mapped ? "★ " : ""}${choice.path} — unavailable`,
						description: `${choice.tags.join(", ")} · ${choice.error}`,
						value: choice,
						disabled: true,
					},
		);
		items.push({ label: "Enter a repository path…", value: MANUAL_PATH });
		const mappedIndex = choices.findIndex(choice => choice.mapped && choice.repository);
		const firstOk = choices.findIndex(choice => choice.repository);
		const initial = mappedIndex >= 0 ? mappedIndex : firstOk >= 0 ? firstOk : items.length - 1;
		const picked = await pick<RepoChoice | typeof MANUAL_PATH>(
			ctx,
			`Checkout for ${issueKey} — 1/5 repository`,
			[
				"★ marks the repository mapped to this Jira project; mapping only changes the default.",
				"Nothing is created until the final confirmation.",
			],
			items,
			initial,
		);
		if (picked === undefined) return undefined;
		if (picked !== MANUAL_PATH) {
			if (picked.repository) return picked.repository;
			continue;
		}
		const input = await ctx.ui.input("Repository path (absolute, ~/, or relative to cwd)", ctx.cwd);
		if (input === undefined || input.trim() === "") continue;
		const path = expandPath(input, ctx.cwd);
		const outcome = await withProgress(ctx, `Resolving ${path}…`, true, signal =>
			services.checkouts.resolveRepository(path, services.config.worktree.prefer, signal),
		);
		if (outcome.kind === "ok") return outcome.value;
		if (outcome.kind === "error") await showMessage(ctx, "Repository unavailable", [path, "", errorMessage(outcome.error)]);
	}
}

function checkoutPickItem(checkout: Checkout): PickItem<Checkout> {
	const flags = [
		checkout.source ? "source" : "",
		checkout.detached ? "detached" : "",
		checkout.locked ? "locked" : "",
		checkout.prunable ? "prunable" : "",
	].filter(Boolean);
	const label = `${checkout.name}${flags.length > 0 ? ` [${flags.join(", ")}]` : ""}`;
	if (checkout.prunable) {
		return { label, description: `${checkout.path ?? "path unknown"} · prunable/missing — not selectable`, value: checkout, disabled: true };
	}
	if (checkout.path === undefined) {
		return {
			label,
			description: `path unknown (${checkout.unavailableReason ?? "lookup failed"}) — you will be asked for the path`,
			value: checkout,
		};
	}
	if (checkout.unavailableReason !== undefined) {
		return { label, description: `${checkout.path} · unavailable: ${checkout.unavailableReason}`, value: checkout, disabled: true };
	}
	return { label, description: `${checkout.path}${checkout.head ? ` · ${checkout.head.slice(0, 12)}` : ""}`, value: checkout };
}

type PlanStep = { kind: "plan"; plan: CheckoutPlan } | { kind: "back" } | { kind: "existing"; preferName: string };

async function planSelection(
	ctx: ExtensionCommandContext,
	services: Services,
	repository: Repository,
	selection: CheckoutSelection,
): Promise<PlanStep> {
	let current = selection;
	for (;;) {
		const outcome = await withProgress(ctx, "Planning checkout (read-only)…", true, signal =>
			services.checkouts.planCheckout(repository, current, signal),
		);
		if (outcome.kind === "ok") return { kind: "plan", plan: outcome.value };
		if (outcome.kind === "cancelled") return { kind: "back" };
		const error = outcome.error;
		if (!(error instanceof RecoveryRequiredError)) {
			await showMessage(ctx, "Cannot plan this checkout", [errorMessage(error)]);
			return { kind: "back" };
		}
		if (current.kind === "new" || error.selection === "new") {
			await showMessage(ctx, "Recovery required", [
				errorMessage(error),
				"",
				`Path: ${error.path}`,
				"A previous creation attempt did not finish; it will not be retried automatically.",
				"Inspect that path manually. To continue, choose it under Existing and acknowledge the recovery warning.",
			]);
			return { kind: "existing", preferName: error.checkoutName };
		}
		if (current.recoveryAcknowledged) {
			await showMessage(ctx, "Cannot plan this checkout", [errorMessage(error)]);
			return { kind: "back" };
		}
		const acknowledged = await ctx.ui.confirm(
			"Recovery acknowledgement",
			`${singleLine(error.message)}\n\nPath: ${error.path}\n\nI inspected this checkout manually and want to open it anyway. It stays unproven (not marked Ready).`,
		);
		if (!acknowledged) return { kind: "back" };
		current = { kind: "existing", checkout: current.checkout, recoveryAcknowledged: true };
	}
}

/** Step 2 (Existing). */
async function chooseExisting(
	ctx: ExtensionCommandContext,
	services: Services,
	repository: Repository,
	preferName: string | undefined,
): Promise<CheckoutPlan | undefined> {
	const listed = await withProgress(ctx, `Listing checkouts of ${repository.name}…`, true, signal =>
		services.checkouts.listCheckouts(repository, signal),
	);
	if (listed.kind === "cancelled") return undefined;
	if (listed.kind === "error") {
		await showMessage(ctx, "Cannot list checkouts", [errorMessage(listed.error)]);
		return undefined;
	}
	const items = listed.value.map(checkoutPickItem);
	if (items.length === 0) {
		await showMessage(ctx, "No checkouts", [`${repository.root} has no listed ${repository.backend === "git" ? "worktrees" : "workspaces"}.`]);
		return undefined;
	}
	const preferred = items.findIndex(item => item.value.name === preferName && !item.disabled);
	let initial = preferred >= 0 ? preferred : Math.max(0, items.findIndex(item => !item.disabled));
	for (;;) {
		const picked = await pick<Checkout>(
			ctx,
			`${repository.name} — 2/5 existing checkout`,
			["Prunable or unavailable entries cannot be selected.", "The selected checkout is re-validated before use."],
			items,
			initial,
		);
		if (picked === undefined) return undefined;
		initial = Math.max(0, items.findIndex(item => item.value === picked));
		let checkout = picked;
		if (checkout.path === undefined) {
			const input = await ctx.ui.input(`Path of ${checkout.name} (absolute)`, "/path/to/workspace");
			if (input === undefined || input.trim() === "") continue;
			// The missing path was the only unavailability; planCheckout re-validates the supplied path.
			const { unavailableReason: _missing, ...known } = checkout;
			checkout = { ...known, path: expandPath(input, ctx.cwd) };
		}
		const step = await planSelection(ctx, services, repository, { kind: "existing", checkout, recoveryAcknowledged: false });
		if (step.kind === "plan") return step.plan;
	}
}

/** Step 2. Returns undefined to go back to step 1. */
async function chooseCheckout(
	ctx: ExtensionCommandContext,
	services: Services,
	repository: Repository,
	issueKey: string,
): Promise<CheckoutPlan | undefined> {
	let initial = 0;
	for (;;) {
		const kind = await pick<"new" | "existing">(
			ctx,
			`${repository.name} [${repository.backend}] — 2/5 checkout`,
			[`Repository: ${repository.root}`],
			[
				{ label: "New checkout", description: `name defaults to ${issueKey}; base resolved to concrete ids before confirmation`, value: "new" as const },
				{ label: "Existing checkout…", description: `reuse a listed ${repository.backend === "git" ? "worktree" : "workspace"}`, value: "existing" as const },
			],
			initial,
		);
		if (kind === undefined) return undefined;
		if (kind === "existing") {
			initial = 1;
			const plan = await chooseExisting(ctx, services, repository, undefined);
			if (plan) return plan;
			continue;
		}
		initial = 0;
		const input = await ctx.ui.input(`New checkout name (empty = ${issueKey})`, issueKey);
		if (input === undefined) continue;
		const name = input.trim() === "" ? issueKey : input.trim();
		const step = await planSelection(ctx, services, repository, { kind: "new", name });
		if (step.kind === "plan") return step.plan;
		if (step.kind === "existing") {
			const plan = await chooseExisting(ctx, services, repository, step.preferName);
			if (plan) return plan;
		}
	}
}

/** Step 3. */
function chooseAction(ctx: ExtensionCommandContext, plan: CheckoutPlan, initial: CheckoutAction): Promise<CheckoutAction | undefined> {
	const actions: CheckoutAction[] = ["checkout", "pane", "window"];
	return pick<CheckoutAction>(
		ctx,
		`${plan.repository.name} — 3/5 action`,
		[`Destination: ${plan.destination}`],
		actions.map(action => ({
			label: actionLabel(action),
			description:
				action === "checkout"
					? "create/reuse the checkout and show its path; no snapshot, no agent"
					: `fresh Jira snapshot → new omp (${action === "pane" ? "pane, default" : "tab"})`,
			value: action,
		})),
		actions.indexOf(initial),
	);
}

type Handoff = { snapshot: Snapshot; instruction: string };

/** Step 4: fresh snapshot, full preview, partial confirmation, separate instruction. Undefined = back to step 3. */
async function prepareHandoff(
	ctx: ExtensionCommandContext,
	services: Services,
	issueKey: string,
	previous: Handoff | undefined,
): Promise<Handoff | undefined> {
	let snapshot = previous?.snapshot;
	if (!snapshot) {
		const built = await withProgress(ctx, `Fetching fresh snapshot of ${issueKey}…`, true, signal =>
			services.buildSnapshot(issueKey, signal),
		);
		if (built.kind === "cancelled") return undefined;
		if (built.kind === "error") {
			await showMessage(ctx, "Snapshot unavailable", [errorMessage(built.error), "", "Handoff needs the issue itself; nothing was created."]);
			return undefined;
		}
		snapshot = built.value;
	}
	const profile = services.config.omp.profile;
	let instruction = previous?.instruction ?? DEFAULT_INSTRUCTION;
	for (;;) {
		const next = await showText<"continue" | "back">(
			ctx,
			{
				title: `${issueKey} — 4/5 snapshot preview`,
				body: snapshotPreviewBody(snapshot, profile),
				help: "j/k PgUp/PgDn scroll · Enter continue · Esc back",
			},
			[{ keys: ["enter"], result: "continue" as const }],
			"back" as const,
		);
		if (next === "back") return undefined;
		if (snapshot.partialReasons.length > 0) {
			const accepted = await ctx.ui.confirm(
				"Incomplete Jira snapshot",
				`The snapshot is incomplete:\n${snapshot.partialReasons.map(reason => `- ${singleLine(reason)}`).join("\n")}\n\nContinue with this incomplete context?`,
			);
			if (!accepted) continue;
		}
		for (;;) {
			const edited = await ctx.ui.editor("Task instruction for the new omp session (separate from the Jira data)", instruction);
			if (edited === undefined) break;
			if (edited.trim() === "") {
				ctx.ui.notify("The task instruction cannot be empty.", "warning");
				continue;
			}
			instruction = edited;
			return { snapshot, instruction };
		}
	}
}

/** Re-validate the approved checkout right before launch; returns an error description or undefined when unchanged. */
async function revalidateCheckout(
	ctx: ExtensionCommandContext,
	services: Services,
	prepared: PreparedCheckout,
	recoveryAcknowledged: boolean,
): Promise<string[] | undefined> {
	const planned = await withProgress(ctx, "Re-validating checkout (read-only)…", true, signal =>
		services.checkouts.planCheckout(prepared.repository, { kind: "existing", checkout: prepared.checkout, recoveryAcknowledged }, signal),
	);
	if (planned.kind === "cancelled") return ["Re-validation was cancelled."];
	if (planned.kind === "error") return revalidationError(planned.error);
	const checked = await withProgress(ctx, "Re-validating checkout…", false, signal =>
		services.checkouts.prepareCheckout(planned.value, signal),
	);
	if (checked.kind !== "ok") {
		return checked.kind === "error" ? revalidationError(checked.error) : ["Re-validation did not complete."];
	}
	const current = checked.value.checkout;
	if (current.path !== prepared.checkout.path || current.name !== prepared.checkout.name) {
		return [`The checkout changed: now ${current.name} at ${current.path}.`];
	}
	return undefined;
}

function revalidationError(error: unknown): string[] {
	if (error instanceof RecoveryRequiredError) {
		return [errorMessage(error), `Path: ${error.path}`, "Choose it under Existing and acknowledge recovery to continue."];
	}
	return [errorMessage(error)];
}

/** Show a launch result; after an unknown outcome, re-launch only on a new explicit, confirmed action. */
async function launch(
	ctx: ExtensionCommandContext,
	services: Services,
	prepared: PreparedCheckout,
	snapshotPath: string,
	placement: "pane" | "window",
	recoveryAcknowledged: boolean,
): Promise<void> {
	for (;;) {
		const problem = await revalidateCheckout(ctx, services, prepared, recoveryAcknowledged);
		if (problem) {
			const next = await showText<"close" | "retry">(
				ctx,
				{
					title: "Checkout re-validation failed — omp not started",
					body: [...problem, "", `Checkout:     ${prepared.checkout.path}`, `Handoff file: ${snapshotPath}`],
					help: "r retry · Enter/Esc close",
				},
				[
					{ keys: ["enter"], result: "close" as const },
					{ keys: ["r"], result: "retry" as const },
				],
				"close" as const,
			);
			if (next === "close") return;
			continue;
		}
		// No overlay while launching: the launcher may ask its own confirm dialogs.
		let result: LaunchResult | undefined;
		let thrown: string | undefined;
		try {
			result = await services.launchOmp(prepared, snapshotPath, placement, ctx);
		} catch (error) {
			thrown = errorMessage(error);
		}
		const uncertain = thrown !== undefined || result?.state === "unknown";
		const body = result
			? launchResultBody(result, prepared.checkout.path, snapshotPath)
			: [
					`Checkout:     ${prepared.checkout.path}`,
					`Handoff file: ${snapshotPath}`,
					"",
					`Launch outcome UNKNOWN: ${thrown ?? ""}`,
					"A pane/tab may or may not have been created. Inspect your terminal before retrying.",
				];
		body.push("", "The checkout is kept regardless of the launch outcome.");
		for (;;) {
			const next = await showText<"close" | "relaunch">(
				ctx,
				{
					title: result?.state === "requested" ? "터미널 실행 요청됨" : `Launch ${result?.state ?? "unknown"}`,
					body,
					help: uncertain ? "Enter close · l re-launch (after inspecting panes/tabs)" : "Enter/Esc close",
				},
				uncertain
					? [
							{ keys: ["enter"], result: "close" as const },
							{ keys: ["l"], result: "relaunch" as const },
						]
					: [{ keys: ["enter"], result: "close" as const }],
				"close" as const,
			);
			if (next === "close") return;
			const inspected = await ctx.ui.confirm(
				"Re-launch omp?",
				"Did you inspect your existing panes/tabs and confirm no omp session for this checkout was started? The same checkout and handoff file will be reused.",
			);
			if (inspected) break;
		}
	}
}

/** Final step: mutate only after confirmation, in plan order. Returns false to go back to step 2. */
async function execute(
	ctx: ExtensionCommandContext,
	services: Services,
	plan: CheckoutPlan,
	action: CheckoutAction,
	handoff: Handoff | undefined,
): Promise<boolean> {
	const prepared = await withProgress(ctx, `Preparing checkout at ${plan.destination}…`, false, signal =>
		services.checkouts.prepareCheckout(plan, signal),
	);
	if (prepared.kind !== "ok") {
		const error = prepared.kind === "error" ? prepared.error : new Error("checkout preparation did not complete");
		const recovery = error instanceof RecoveryRequiredError ? [`Path: ${error.path}`, "Choose it under Existing and acknowledge recovery to continue."] : [];
		await showMessage(ctx, "Checkout failed", [errorMessage(error), ...recovery, "", `Destination: ${plan.destination}`, "Choose the checkout again."]);
		return false;
	}
	const checkout = prepared.value;
	if (action === "checkout" || !handoff) {
		await showMessage(ctx, checkout.created ? "Checkout created" : "Checkout ready", [
			`Path: ${checkout.checkout.path}`,
			`Repository: ${checkout.repository.name} [${checkout.repository.backend}]`,
			`Name: ${checkout.checkout.name}`,
		]);
		return true;
	}
	const saved = await withProgress(ctx, "Writing private handoff file…", false, () =>
		services.saveSnapshot(handoff.snapshot, handoff.instruction),
	);
	if (saved.kind !== "ok") {
		await showMessage(ctx, "Handoff file not written", [
			saved.kind === "error" ? errorMessage(saved.error) : "handoff write did not complete",
			"",
			`The checkout is ready at ${checkout.checkout.path}; omp was not started.`,
		]);
		return true;
	}
	const acknowledged = plan.selection.kind === "existing" && plan.selection.recoveryAcknowledged;
	await launch(ctx, services, checkout, saved.value, action === "window" ? "window" : "pane", acknowledged);
	return true;
}

async function runWizard(ctx: ExtensionCommandContext, services: Services, issueKey: string, omp: boolean): Promise<void> {
	let choices: RepoChoice[] | undefined;
	let repository: Repository | undefined;
	let plan: CheckoutPlan | undefined;
	let action: CheckoutAction = omp ? "pane" : "checkout";
	let handoff: Handoff | undefined;
	let step: "repo" | "checkout" | "action" | "handoff" | "confirm" = "repo";
	for (;;) {
		switch (step) {
			case "repo": {
				choices ??= await resolveRepoChoices(ctx, services, issueKey);
				if (!choices) return;
				repository = await chooseRepository(ctx, services, issueKey, choices);
				if (!repository) return;
				step = "checkout";
				break;
			}
			case "checkout": {
				if (!repository) {
					step = "repo";
					break;
				}
				plan = await chooseCheckout(ctx, services, repository, issueKey);
				step = plan ? "action" : "repo";
				break;
			}
			case "action": {
				if (!plan) {
					step = "checkout";
					break;
				}
				const chosen = await chooseAction(ctx, plan, action);
				if (!chosen) {
					step = "checkout";
					break;
				}
				action = chosen;
				// Leaving step 4 backwards discards the snapshot so the next pass fetches it fresh.
				handoff = undefined;
				step = action === "checkout" ? "confirm" : "handoff";
				break;
			}
			case "handoff": {
				handoff = await prepareHandoff(ctx, services, issueKey, handoff);
				step = handoff ? "confirm" : "action";
				break;
			}
			case "confirm": {
				if (!plan) {
					step = "checkout";
					break;
				}
				const usesOmp = action !== "checkout";
				const answer = await showText<"confirm" | "back">(
					ctx,
					{
						title: `${issueKey} — 5/5 confirm`,
						body: finalConfirmBody({
							issueKey,
							plan,
							action,
							profile: services.config.omp.profile,
							snapshot: usesOmp ? handoff?.snapshot : undefined,
							instruction: usesOmp ? handoff?.instruction : undefined,
						}),
						help: "Enter confirm · Esc back · j/k scroll",
					},
					[{ keys: ["enter"], result: "confirm" as const }],
					"back" as const,
				);
				if (answer === "back") {
					step = usesOmp ? "handoff" : "action";
					break;
				}
				if (await execute(ctx, services, plan, action, usesOmp ? handoff : undefined)) return;
				plan = undefined;
				handoff = undefined;
				step = "checkout";
				break;
			}
		}
	}
}

// ---------------------------------------------------------------------------
// Entry point

async function handleIntent(
	ctx: ExtensionCommandContext,
	services: Services,
	session: BrowserSession,
	intent: Exclude<Intent, { kind: "close" }>,
): Promise<void> {
	switch (intent.kind) {
		case "search": {
			const text = await ctx.ui.input("Search Jira issues", "text to find");
			const term = text?.trim();
			if (term) session.loadQuery({ label: `Search: ${term}`, jql: buildSearchJql(services.config.search.jql, term) });
			return;
		}
		case "jql": {
			const text = await ctx.ui.input("Raw JQL (sent as-is)", session.list.query.jql);
			if (text !== undefined && text.trim() !== "") session.loadQuery({ label: "JQL", jql: text });
			return;
		}
		case "filter": {
			const choices = filterChoices(services.config);
			const labels = uniqueLabels(choices.map(choice => singleLine(choice.label)));
			const current = choices.findIndex(choice => choice.jql === session.list.query.jql);
			const picked = await ctx.ui.select("Jira filter", labels, { initialIndex: Math.max(0, current) });
			const choice = picked === undefined ? undefined : choices[labels.indexOf(picked)];
			if (choice) session.loadQuery(choice);
			return;
		}
		case "transition": {
			try {
				await services.runTransition(intent.issueKey, ctx, new AbortController().signal);
			} catch (error) {
				ctx.ui.notify(`Status change failed: ${errorMessage(error)}`, "error");
			}
			session.refreshAfterTransition();
			return;
		}
		case "wizard": {
			try {
				await runWizard(ctx, services, intent.issueKey, intent.omp);
			} catch (error) {
				ctx.ui.notify(`Checkout wizard failed: ${errorMessage(error)}`, "error");
			}
			return;
		}
	}
}

/**
 * Open the Jira browser. `initialKey` opens a fresh issue detail directly.
 * Requires the interactive TUI; other modes reject before any request.
 */
export async function runBrowser(ctx: ExtensionCommandContext, services: Services, initialKey?: string): Promise<void> {
	if (ctx.mode !== "tui" || !ctx.hasUI) throw new Error("interactive TUI required");
	const key = initialKey === undefined ? undefined : normalizeIssueKey(initialKey);
	const session = new BrowserSession(services);
	try {
		if (key === undefined) session.loadQuery(session.list.query);
		else session.openDetail(key);
		for (;;) {
			const intent = await ctx.ui.custom<Intent>(
				(tui, theme, _keybindings, done) => new BrowserView(session, tui, theme, done),
				OVERLAY,
			);
			if (intent.kind === "close") return;
			await handleIntent(ctx, services, session, intent);
		}
	} finally {
		session.dispose();
	}
}
