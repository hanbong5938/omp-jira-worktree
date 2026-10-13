import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Config, StateStore, StatusRef } from "../src/contracts.ts";
import { createJiraClient } from "../src/jira.ts";
import { run } from "../src/process.ts";
import { createStateStore } from "../src/state.ts";
import { createTransitionService, type TransitionContext } from "../src/transitions.ts";

const KEY = "TEST-1";
const TODO: StatusRef = { id: "10000", name: "To Do" };
const IN_PROGRESS: StatusRef = { id: "3", name: "In Progress" };
const RESOLVED: StatusRef = { id: "5", name: "Resolved" };

type RawTransition = {
	id: string;
	name: string;
	to: StatusRef;
	hasScreen: boolean;
	isAvailable?: boolean;
	fields: Record<string, { required: boolean; name: string }>;
};

const START: RawTransition = { id: "31", name: "Start Progress", to: IN_PROGRESS, hasScreen: false, isAvailable: true, fields: {} };
const RESOLVE_SCREEN: RawTransition = { id: "41", name: "Resolve", to: RESOLVED, hasScreen: true, isAvailable: true, fields: {} };
const NEEDS_FIELD: RawTransition = {
	id: "51",
	name: "Estimate",
	to: RESOLVED,
	hasScreen: false,
	isAvailable: true,
	fields: { customfield_1: { required: true, name: "Estimate" } },
};
const UNAVAILABLE: RawTransition = { id: "61", name: "Blocked", to: RESOLVED, hasScreen: false, isAvailable: false, fields: {} };

type Fixture = {
	status: StatusRef;
	/** Transition list for the n-th (0-based) transitions GET. */
	transitionsFor: (n: number) => RawTransition[];
	transitionGets: number;
	postBodies: unknown[];
	onPost: (request: Request) => Response | Promise<Response>;
	hung: Array<() => void>;
};

let tmp: string;
let server: Bun.Server<undefined>;
let fixture: Fixture;
let config: Config;
let store: StateStore;

function makeConfig(baseUrl: string, root: string): Config {
	return {
		jira: {
			baseUrl,
			deployment: "cloud",
			auth: "bearer",
			email: null,
			tokenCommand: ["/bin/echo", "fixture-token"],
			maxResults: 50,
			timeoutMs: 1000,
		},
		filters: [],
		search: { jql: 'text ~ "{query}" ORDER BY updated DESC' },
		worktree: { repos: {}, prefer: "jj", root, gitBase: null, jjBase: null },
		handoff: { maxComments: 50, maxDescriptionChars: 6000, maxCommentChars: 12000, maxChars: 64000 },
		omp: { binary: "omp", profile: "default" },
	};
}

function issueBody(): unknown {
	return {
		key: KEY,
		fields: {
			summary: "Fixture issue",
			status: fixture.status,
			issuetype: { name: "Task" },
			assignee: null,
			updated: "2026-10-01T00:00:00.000+0000",
			description: null,
		},
	};
}

beforeEach(async () => {
	tmp = await mkdtemp(join(tmpdir(), "omp-jira-transitions-"));
	fixture = {
		status: TODO,
		transitionsFor: () => [START, RESOLVE_SCREEN, NEEDS_FIELD, UNAVAILABLE],
		transitionGets: 0,
		postBodies: [],
		onPost: () => {
			fixture.status = IN_PROGRESS;
			return new Response(null, { status: 204 });
		},
		hung: [],
	};
	server = Bun.serve({
		hostname: "127.0.0.1",
		port: 0,
		fetch: async (request) => {
			const url = new URL(request.url);
			if (url.pathname === `/rest/api/3/issue/${KEY}` && request.method === "GET") return Response.json(issueBody());
			if (url.pathname === `/rest/api/3/issue/${KEY}/transitions`) {
				if (request.method === "GET") {
					const list = fixture.transitionsFor(fixture.transitionGets);
					fixture.transitionGets += 1;
					return Response.json({ transitions: list });
				}
				if (request.method === "POST") {
					fixture.postBodies.push(await request.json());
					return fixture.onPost(request);
				}
			}
			return Response.json({ errorMessages: ["not found"] }, { status: 404 });
		},
	});
	config = makeConfig(`http://127.0.0.1:${server.port}`, join(tmp, "checkouts"));
	store = createStateStore(join(tmp, "state"));
});

afterEach(async () => {
	for (const release of fixture.hung) release();
	await server.stop(true);
	await rm(tmp, { recursive: true, force: true });
});

type Note = { message: string; type: "info" | "warning" | "error" | undefined };
type SelectAnswer = (labels: string[]) => string | undefined;

/** Scripted UI: answers are consumed in order; an exhausted script cancels. */
class FakeUi {
	readonly selects: { title: string; labels: string[] }[] = [];
	readonly confirms: { title: string; message: string }[] = [];
	readonly notes: Note[] = [];
	readonly #selectAnswers: SelectAnswer[];
	readonly #confirmAnswers: boolean[];
	readonly #waiters: { match: (note: Note) => boolean; resolve: (note: Note) => void }[] = [];

	constructor(script: { select?: SelectAnswer[]; confirm?: boolean[] }) {
		this.#selectAnswers = [...(script.select ?? [])];
		this.#confirmAnswers = [...(script.confirm ?? [])];
	}

	ctx(hasUI = true): TransitionContext {
		return {
			hasUI,
			ui: {
				select: async (title: string, options: readonly (string | { label: string })[]) => {
					const labels = options.map((o) => (typeof o === "string" ? o : o.label));
					this.selects.push({ title, labels });
					return this.#selectAnswers.shift()?.(labels);
				},
				confirm: async (title: string, message: string) => {
					this.confirms.push({ title, message });
					return this.#confirmAnswers.shift() ?? false;
				},
				notify: (message: string, type?: "info" | "warning" | "error") => {
					const note = { message, type };
					this.notes.push(note);
					for (const waiter of this.#waiters.filter((w) => w.match(note))) {
						this.#waiters.splice(this.#waiters.indexOf(waiter), 1);
						waiter.resolve(note);
					}
				},
			},
		};
	}

	waitForNote(match: (note: Note) => boolean): Promise<Note> {
		const found = this.notes.find(match);
		if (found !== undefined) return Promise.resolve(found);
		const { promise, resolve } = Promise.withResolvers<Note>();
		this.#waiters.push({ match, resolve });
		return promise;
	}
}

const choose =
	(id: string): SelectAnswer =>
	(labels) =>
		labels.find((label) => label.includes(`[#${id}]`));
const cancel: SelectAnswer = () => undefined;

function service() {
	return createTransitionService(config, createJiraClient(config, run, fetch), store);
}

function recordPath(): string {
	const digest = createHash("sha256").update(JSON.stringify([config.jira.baseUrl, KEY])).digest("hex");
	return `transitions/${digest}.json`;
}

function readStoredRecord(): Promise<unknown> {
	return store.readRecord(recordPath(), (value) => value);
}

describe("runTransition", () => {
	test("cancel at the picker sends no POST", async () => {
		const ui = new FakeUi({ select: [cancel] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(ui.selects).toHaveLength(1);
		expect(ui.selects[0]?.title).toContain("To Do");
		expect(ui.confirms).toHaveLength(0);
		expect(fixture.postBodies).toHaveLength(0);
		expect(await readStoredRecord()).toBeUndefined();
	});

	test("cancel at the final confirm sends no POST; the confirm shows site, key, statuses and transition", async () => {
		const ui = new FakeUi({ select: [choose("31")], confirm: [false] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(ui.confirms).toHaveLength(1);
		const message = ui.confirms[0]?.message ?? "";
		expect(message).toContain(config.jira.baseUrl);
		expect(message).toContain(KEY);
		expect(message).toContain("To Do");
		expect(message).toContain("In Progress");
		expect(message).toContain("Start Progress");
		expect(message).toContain("31");
		expect(fixture.postBodies).toHaveLength(0);
	});

	test("screen-required transition points to Jira web and never POSTs", async () => {
		const ui = new FakeUi({ select: [choose("41")], confirm: [true] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(ui.confirms).toHaveLength(0);
		expect(ui.notes.some((n) => n.message.includes("Resolve") && n.message.includes(`/browse/${KEY}`))).toBe(true);
		expect(fixture.postBodies).toHaveLength(0);
	});

	test("required-field transition points to Jira web and never POSTs", async () => {
		const ui = new FakeUi({ select: [choose("51")], confirm: [true] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(ui.confirms).toHaveLength(0);
		expect(ui.notes.some((n) => n.message.includes("Estimate") && n.message.includes(`/browse/${KEY}`))).toBe(true);
		expect(fixture.postBodies).toHaveLength(0);
	});

	test("unavailable transition cannot be chosen and re-prompts", async () => {
		const ui = new FakeUi({ select: [choose("61"), cancel], confirm: [true] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(ui.selects).toHaveLength(2);
		expect(ui.confirms).toHaveLength(0);
		expect(ui.notes.some((n) => n.type === "warning" && n.message.includes("Blocked"))).toBe(true);
		expect(fixture.postBodies).toHaveLength(0);
	});

	test("transition list changed between confirm and in-lock re-read discards approval and re-prompts", async () => {
		const moved: RawTransition = { ...START, to: RESOLVED };
		fixture.transitionsFor = (n) => (n === 0 ? [START] : [moved]);
		const ui = new FakeUi({ select: [choose("31"), cancel], confirm: [true] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(fixture.postBodies).toHaveLength(0);
		expect(ui.selects).toHaveLength(2);
		expect(ui.notes.some((n) => n.type === "warning")).toBe(true);
		expect(await readStoredRecord()).toBeUndefined();
	});

	test("status changed after selection discards approval at the in-lock re-read", async () => {
		const changeStatusThenPick: SelectAnswer = (labels) => {
			fixture.status = { id: "4", name: "Reopened" };
			return choose("31")(labels);
		};
		const ui = new FakeUi({ select: [changeStatusThenPick, cancel], confirm: [true] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(fixture.postBodies).toHaveLength(0);
		expect(ui.selects).toHaveLength(2);
	});

	test("confirm sends exactly one POST and verifies the target status", async () => {
		const ui = new FakeUi({ select: [choose("31")], confirm: [true] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(fixture.postBodies).toEqual([{ transition: { id: "31" } }]);
		expect(ui.notes.some((n) => n.type === "info" && n.message.includes("In Progress"))).toBe(true);
		expect(await readStoredRecord()).toMatchObject({
			version: 1,
			key: KEY,
			site: config.jira.baseUrl,
			transitionId: "31",
			fromStatusId: TODO.id,
			toStatusId: IN_PROGRESS.id,
			state: "acknowledged",
		});
	});

	test("2xx with a different observed status is acknowledged but unverified, without resend", async () => {
		fixture.onPost = () => new Response(null, { status: 204 });
		const ui = new FakeUi({ select: [choose("31")], confirm: [true] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(fixture.postBodies).toHaveLength(1);
		expect(ui.notes.some((n) => n.type === "warning" && n.message.includes("서버가 전환 요청 수락"))).toBe(true);
		expect(await readStoredRecord()).toMatchObject({ state: "acknowledged" });
	});

	test("definite 4xx is rejected with the server message", async () => {
		fixture.onPost = () => Response.json({ errorMessages: ["Transition is not valid"] }, { status: 400 });
		const ui = new FakeUi({ select: [choose("31")], confirm: [true] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(fixture.postBodies).toHaveLength(1);
		expect(ui.notes.some((n) => n.type === "error" && n.message.includes("Transition is not valid"))).toBe(true);
		expect(await readStoredRecord()).toMatchObject({ state: "rejected" });
	});

	test("500 after the server changed status is unknown, POSTed once, and the target is only observed", async () => {
		fixture.onPost = () => {
			fixture.status = IN_PROGRESS;
			return Response.json({ errorMessages: ["boom"] }, { status: 500 });
		};
		const ui = new FakeUi({ select: [choose("31")], confirm: [true] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(fixture.postBodies).toHaveLength(1);
		const note = ui.notes.find((n) => n.type === "warning");
		expect(note?.message).toContain("현재 상태 관측: In Progress");
		expect(ui.notes.some((n) => n.message.includes("전환 완료"))).toBe(false);
		expect(await readStoredRecord()).toMatchObject({ state: "unknown" });
	});

	test("500 with the old status still reported is unknown, not 'not applied'", async () => {
		fixture.onPost = () => Response.json({ errorMessages: ["boom"] }, { status: 500 });
		const ui = new FakeUi({ select: [choose("31")], confirm: [true] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(fixture.postBodies).toHaveLength(1);
		expect(ui.notes.find((n) => n.type === "warning")?.message).toContain("현재 상태 관측: To Do");
		expect(await readStoredRecord()).toMatchObject({ state: "unknown" });
	});

	test("server hanging past timeoutMs is unknown and not retried", async () => {
		fixture.onPost = () => {
			const { promise, resolve } = Promise.withResolvers<Response>();
			fixture.hung.push(() => resolve(new Response(null, { status: 204 })));
			return promise;
		};
		const ui = new FakeUi({ select: [choose("31")], confirm: [true] });
		await service()(KEY, ui.ctx(), new AbortController().signal);
		expect(fixture.postBodies).toHaveLength(1);
		expect(ui.notes.some((n) => n.type === "warning" && n.message.includes("전환 결과 불확실"))).toBe(true);
		expect(await readStoredRecord()).toMatchObject({ state: "unknown" });
	});

	test("unknown record blocks the next action until the user confirms Jira history was inspected", async () => {
		fixture.onPost = () => Response.json({ errorMessages: ["boom"] }, { status: 500 });
		await service()(KEY, new FakeUi({ select: [choose("31")], confirm: [true] }).ctx(), new AbortController().signal);
		expect(fixture.postBodies).toHaveLength(1);

		const declined = new FakeUi({ select: [choose("31")], confirm: [false, true] });
		await service()(KEY, declined.ctx(), new AbortController().signal);
		expect(declined.confirms).toHaveLength(1);
		expect(declined.confirms[0]?.message).toContain(`/browse/${KEY}`);
		expect(declined.selects).toHaveLength(0);
		expect(fixture.postBodies).toHaveLength(1);
	});

	test("pending record from a previous process blocks POST unless inspection is confirmed", async () => {
		await store.writeRecord(recordPath(), {
			version: 1,
			attempt: "previous-attempt",
			key: KEY,
			site: config.jira.baseUrl,
			transitionId: "31",
			fromStatusId: TODO.id,
			toStatusId: IN_PROGRESS.id,
			state: "pending",
			startedAt: "2026-10-12T00:00:00.000Z",
		});
		store = createStateStore(join(tmp, "state"));

		const declined = new FakeUi({ select: [choose("31")], confirm: [false, true] });
		await service()(KEY, declined.ctx(), new AbortController().signal);
		expect(declined.confirms).toHaveLength(1);
		expect(declined.confirms[0]?.message).toContain("To Do");
		expect(declined.selects).toHaveLength(0);
		expect(fixture.postBodies).toHaveLength(0);
		expect(await readStoredRecord()).toMatchObject({ state: "pending", attempt: "previous-attempt" });

		const inspected = new FakeUi({ select: [choose("31")], confirm: [true, true] });
		await service()(KEY, inspected.ctx(), new AbortController().signal);
		expect(inspected.selects).toHaveLength(1);
		expect(fixture.postBodies).toEqual([{ transition: { id: "31" } }]);
		const record = await readStoredRecord();
		expect(record).toMatchObject({ state: "acknowledged" });
		expect(record).not.toMatchObject({ attempt: "previous-attempt" });
	});

	test("held site+issue lock makes a concurrent transition report busy without POST", async () => {
		const held = Promise.withResolvers<void>();
		const locked = Promise.withResolvers<void>();
		const holder = store.withLocks([`transition:${config.jira.baseUrl}:${KEY}`], async () => {
			locked.resolve();
			await held.promise;
		});
		await locked.promise;
		try {
			const ui = new FakeUi({ select: [choose("31")], confirm: [true] });
			await service()(KEY, ui.ctx(), new AbortController().signal);
			expect(fixture.postBodies).toHaveLength(0);
			expect(ui.notes.some((n) => n.type === "error")).toBe(true);
			expect(await readStoredRecord()).toBeUndefined();
		} finally {
			held.resolve();
			await holder;
		}
	});

	test("closing the caller after POST dispatch does not cancel the write; the outcome is still recorded", async () => {
		const posted = Promise.withResolvers<void>();
		const release = Promise.withResolvers<void>();
		fixture.onPost = async () => {
			posted.resolve();
			await release.promise;
			fixture.status = IN_PROGRESS;
			return new Response(null, { status: 204 });
		};
		const caller = new AbortController();
		const ui = new FakeUi({ select: [choose("31")], confirm: [true] });
		const running = service()(KEY, ui.ctx(), caller.signal);
		await posted.promise;
		caller.abort();
		await running;
		release.resolve();
		const note = await ui.waitForNote((n) => n.type === "info" && n.message.includes("전환 완료"));
		expect(note.message).toContain("In Progress");
		expect(fixture.postBodies).toHaveLength(1);
		expect(await readStoredRecord()).toMatchObject({ state: "acknowledged" });
	});

	test("non-UI context is refused before any request", async () => {
		const ui = new FakeUi({ select: [choose("31")], confirm: [true] });
		await expect(service()(KEY, ui.ctx(false), new AbortController().signal)).rejects.toThrow();
		expect(fixture.transitionGets).toBe(0);
		expect(fixture.postBodies).toHaveLength(0);
	});
});
