/**
 * Real-program smoke: wires the exported client/checkout/handoff/launch/transition
 * functions to a loopback Jira Cloud v3 fixture and temporary Git/jj repositories.
 *
 *   bun scripts/smoke.ts          run the scenario, print a summary, exit 0 / 1
 *   bun scripts/smoke.ts --serve  keep the fixture alive for a manual `omp` session
 *
 * The loopback server is a test fixture, never a product fallback.
 */
import { createHash } from "node:crypto";
import * as fsp from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { createCheckoutService } from "../src/checkouts.ts";
import { loadConfig } from "../src/config.ts";
import type { Config, ExtensionCommandContext, PreparedCheckout, StatusRef } from "../src/contracts.ts";
import { DEFAULT_INSTRUCTION, RUN_TIMEOUT_MS } from "../src/contracts.ts";
import { createHandoffService } from "../src/handoff.ts";
import { createJiraClient } from "../src/jira.ts";
import { createLauncher, posixQuote } from "../src/launch.ts";
import { run } from "../src/process.ts";
import { createStateStore, stateRoot } from "../src/state.ts";
import { createTransitionService, type TransitionContext } from "../src/transitions.ts";

const PROJECT_ROOT = path.dirname(fileURLToPath(new URL("../package.json", import.meta.url)));
const EMAIL = "smoke@example.test";
const FAKE_TOKEN = "fixture-token-not-a-secret-7f3a9c";
const EXPECTED_AUTH = `Basic ${Buffer.from(`${EMAIL}:${FAKE_TOKEN}`).toString("base64")}`;
const KOREAN_DESCRIPTION = "로그인 화면에서 한글 입력이 깨지는 문제를 수정합니다.";
const SERVE = process.argv.includes("--serve");

class SmokeFailure extends Error {
	constructor(message: string) {
		super(message);
		this.name = "SmokeFailure";
	}
}

function check(condition: boolean, reason: string): asserts condition {
	if (!condition) throw new SmokeFailure(reason);
}

// ---------------------------------------------------------------------------
// Loopback Jira Cloud v3 fixture
// ---------------------------------------------------------------------------

type FixtureIssue = { key: string; summary: string; status: StatusRef; description: unknown; comments: unknown[] };

const TO_DO: StatusRef = { id: "1", name: "To Do" };
const IN_PROGRESS: StatusRef = { id: "3", name: "In Progress" };
const DONE: StatusRef = { id: "5", name: "Done" };
const BLOCKED: StatusRef = { id: "4", name: "Blocked" };

function adf(text: string): unknown {
	return { type: "doc", version: 1, content: [{ type: "paragraph", content: [{ type: "text", text }] }] };
}

type Fixture = {
	url: string;
	counts: Map<string, number>;
	unauthorized: () => number;
	status: (key: string) => StatusRef | undefined;
	stop: () => void;
};

function startFixture(log: boolean): Fixture {
	const issues = new Map<string, FixtureIssue>([
		[
			"TEST-1",
			{
				key: "TEST-1",
				summary: "한글 로그인 버그",
				status: { ...TO_DO },
				description: adf(KOREAN_DESCRIPTION),
				comments: [
					{ id: "10001", author: { displayName: "검토자" }, created: "2026-10-01T09:00:00.000+0900", body: adf("재현 절차를 확인했습니다.") },
					{ id: "10002", author: { displayName: "Reporter" }, created: "2026-10-02T09:00:00.000+0900", body: adf("Safari에서도 발생합니다.") },
				],
			},
		],
		[
			"TEST-2",
			{ key: "TEST-2", summary: "Second fixture issue", status: { ...TO_DO }, description: adf("두 번째 이슈"), comments: [] },
		],
	]);
	const counts = new Map<string, number>();
	let unauthorized = 0;
	const encoder = new TextEncoder();

	const issueJson = (issue: FixtureIssue) => ({
		id: issue.key.replace(/\D/g, ""),
		key: issue.key,
		fields: {
			summary: issue.summary,
			status: issue.status,
			issuetype: { name: "Task" },
			assignee: { displayName: "Smoke Tester" },
			updated: "2026-10-13T09:00:00.000+0900",
			description: issue.description,
		},
	});
	const transitionsJson = () => ({
		transitions: [
			{ id: "31", name: "Start Progress", to: IN_PROGRESS, hasScreen: false, isAvailable: true, fields: {} },
			{
				id: "41",
				name: "Resolve",
				to: DONE,
				hasScreen: false,
				isAvailable: true,
				fields: { resolution: { required: true, name: "Resolution" } },
			},
			{ id: "51", name: "Block", to: BLOCKED, hasScreen: false, isAvailable: true, fields: {} },
		],
	});

	async function handle(req: Request): Promise<Response> {
		const url = new URL(req.url);
		if (req.headers.get("authorization") !== EXPECTED_AUTH) {
			unauthorized++;
			return Response.json({ errorMessages: ["unauthorized"] }, { status: 401 });
		}
		const base = "/rest/api/3";
		if (req.method === "POST" && url.pathname === `${base}/search/jql`) {
			return Response.json({ issues: [...issues.values()].map(issueJson), isLast: true });
		}
		const match = /^\/rest\/api\/3\/issue\/([^/]+)(\/comment|\/transitions)?$/.exec(url.pathname);
		const issue = match ? issues.get(decodeURIComponent(match[1] ?? "")) : undefined;
		if (!match || !issue) return Response.json({ errorMessages: ["Issue does not exist"] }, { status: 404 });
		const sub = match[2];
		if (req.method === "GET" && sub === undefined) return Response.json(issueJson(issue));
		if (req.method === "GET" && sub === "/comment") {
			const startAt = Number(url.searchParams.get("startAt") ?? "0");
			const maxResults = Number(url.searchParams.get("maxResults") ?? "50");
			const page = issue.comments.slice(startAt, startAt + maxResults);
			return Response.json({ startAt, maxResults, total: issue.comments.length, comments: page });
		}
		if (req.method === "GET" && sub === "/transitions") return Response.json(transitionsJson());
		if (req.method === "POST" && sub === "/transitions") {
			const body: unknown = await req.json();
			const id =
				typeof body === "object" && body !== null && "transition" in body &&
				typeof body.transition === "object" && body.transition !== null && "id" in body.transition
					? String(body.transition.id)
					: "";
			if (id === "31") {
				issue.status = { ...IN_PROGRESS };
				return new Response(null, { status: 204 });
			}
			if (id === "51") {
				// Headers go out, then the body stream errors: the client sees a dropped connection.
				const stream = new ReadableStream<Uint8Array>({
					start(controller) {
						controller.enqueue(encoder.encode("{"));
						setTimeout(() => controller.error(new Error("fixture: connection dropped")), 20);
					},
				});
				return new Response(stream, { status: 200, headers: { "content-type": "application/json" } });
			}
			return Response.json({ errorMessages: [`transition ${id} requires fields`] }, { status: 400 });
		}
		return Response.json({ errorMessages: ["unsupported"] }, { status: 405 });
	}

	const server = Bun.serve({
		hostname: "127.0.0.1",
		port: 0,
		async fetch(req) {
			const key = `${req.method} ${new URL(req.url).pathname}`;
			counts.set(key, (counts.get(key) ?? 0) + 1);
			const response = await handle(req);
			if (log) console.log(`${new Date().toISOString()} ${key} ${response.status}`);
			return response;
		},
		error(error) {
			if (log) console.log(`fixture error: ${error.message}`);
			return new Response("fixture error", { status: 500 });
		},
	});
	return {
		url: `http://127.0.0.1:${server.port}`,
		counts,
		unauthorized: () => unauthorized,
		status: (key) => issues.get(key)?.status,
		stop: () => void server.stop(true),
	};
}

// ---------------------------------------------------------------------------
// Temporary roots, config and repositories
// ---------------------------------------------------------------------------

type Roots = {
	top: string;
	configFile: string;
	stateHome: string;
	worktreeRoot: string;
	gitRepo: string;
	jjRepo: string;
	tokenFile: string;
	gitDirtyFile: string;
};

async function sh(file: string, args: string[], cwd: string): Promise<string> {
	const result = await run(file, args, { cwd, timeoutMs: RUN_TIMEOUT_MS });
	if (result.code !== 0 || result.killed) {
		throw new SmokeFailure(`${file} ${args.join(" ")} failed (${result.code}): ${result.stderr.trim()}`);
	}
	return result.stdout;
}

async function buildRoots(serverUrl: string): Promise<Roots> {
	const top = await fsp.realpath(await fsp.mkdtemp(path.join(os.tmpdir(), "omp-jira-smoke-")));
	const dir = async (...parts: string[]) => {
		const p = path.join(top, ...parts);
		await fsp.mkdir(p, { recursive: true, mode: 0o700 });
		return p;
	};
	const configHome = await dir("xdg-config");
	const stateHome = await dir("xdg-state");
	const worktreeRoot = await dir("checkouts");
	const toolHome = await dir("tool-config");
	const gitRepo = await dir("repos", "git-src");
	const jjRepo = path.join(await dir("repos"), "jj-src");

	const tokenFile = path.join(top, "fixture-token.txt");
	await fsp.writeFile(tokenFile, `${FAKE_TOKEN}\n`, { mode: 0o600 });

	const gitConfig = path.join(toolHome, "gitconfig");
	await fsp.writeFile(gitConfig, "[user]\n\tname = Smoke Tester\n\temail = smoke@example.test\n[init]\n\tdefaultBranch = main\n");
	const jjConfig = path.join(toolHome, "jj.toml");
	await fsp.writeFile(jjConfig, '[user]\nname = "Smoke Tester"\nemail = "smoke@example.test"\n[ui]\npaginate = "never"\n');
	Object.assign(process.env, {
		XDG_CONFIG_HOME: configHome,
		XDG_STATE_HOME: stateHome,
		GIT_CONFIG_GLOBAL: gitConfig,
		GIT_CONFIG_NOSYSTEM: "1",
		GIT_AUTHOR_NAME: "Smoke Tester",
		GIT_AUTHOR_EMAIL: EMAIL,
		GIT_COMMITTER_NAME: "Smoke Tester",
		GIT_COMMITTER_EMAIL: EMAIL,
		JJ_CONFIG: jjConfig,
		JJ_USER: "Smoke Tester",
		JJ_EMAIL: EMAIL,
	});

	const configFile = path.join(configHome, "omp-jira-worktree", "config.json");
	await fsp.mkdir(path.dirname(configFile), { recursive: true, mode: 0o700 });
	const rawConfig = {
		jira: {
			baseUrl: serverUrl,
			deployment: "cloud",
			auth: "basic",
			email: EMAIL,
			tokenCommand: ["/bin/cat", tokenFile],
			maxResults: 50,
			timeoutMs: 5000,
		},
		search: { jql: 'project = TEST AND text ~ "{query}" ORDER BY updated DESC' },
		worktree: { repos: { TEST: gitRepo }, prefer: "git", root: worktreeRoot },
		handoff: { maxComments: 5, maxDescriptionChars: 2000, maxCommentChars: 500, maxChars: 8000 },
		omp: { profile: "default" },
	};
	await fsp.writeFile(configFile, `${JSON.stringify(rawConfig, null, "\t")}\n`, { mode: 0o600 });
	process.env.OMP_JIRA_WORKTREE_CONFIG = configFile;

	await sh("git", ["init", "-q", gitRepo], top);
	await fsp.writeFile(path.join(gitRepo, "README.md"), "# smoke\n");
	await sh("git", ["add", "README.md"], gitRepo);
	await sh("git", ["commit", "-q", "-m", "initial"], gitRepo);
	const gitDirtyFile = path.join(gitRepo, "README.md");
	await fsp.writeFile(gitDirtyFile, "# smoke\nuncommitted edit — must survive\n");
	await fsp.writeFile(path.join(gitRepo, "untracked.txt"), "untracked\n");

	await sh("jj", ["git", "init", "--no-colocate", jjRepo], top);
	await fsp.writeFile(path.join(jjRepo, "hello.txt"), "hello jj\n");
	await sh("jj", ["--no-pager", "-R", jjRepo, "commit", "-m", "initial"], jjRepo);

	return { top, configFile, stateHome, worktreeRoot, gitRepo, jjRepo, tokenFile, gitDirtyFile };
}

// ---------------------------------------------------------------------------
// Scenario
// ---------------------------------------------------------------------------

type Scripted = {
	ctx: TransitionContext;
	notes: string[];
	confirmTitles: string[];
	selects: number;
};

function scriptedCtx(pickId: string | undefined, confirms: boolean[]): Scripted {
	const state: Scripted = { ctx: undefined as unknown as TransitionContext, notes: [], confirmTitles: [], selects: 0 };
	const queue = [...confirms];
	const select = async (_title: string, options: readonly (string | { label: string })[]) => {
		state.selects++;
		if (pickId === undefined) return undefined;
		for (const option of options) {
			const label = typeof option === "string" ? option : option.label;
			if (label.includes(`[#${pickId}]`)) return label;
		}
		return undefined;
	};
	const confirm = async (title: string) => {
		state.confirmTitles.push(title);
		const next = queue.shift();
		if (next === undefined) throw new SmokeFailure(`unexpected confirm: ${title}`);
		return next;
	};
	const notify = (message: string) => {
		state.notes.push(message);
	};
	state.ctx = {
		hasUI: true,
		ui: { select, confirm, notify } as unknown as TransitionContext["ui"],
	};
	return state;
}

function postCount(fixture: Fixture): number {
	return fixture.counts.get("POST /rest/api/3/issue/TEST-1/transitions") ?? 0;
}

async function scenario(fixture: Fixture, roots: Roots): Promise<void> {
	const signal = new AbortController().signal;
	const config: Config = await loadConfig();
	check(config.jira.baseUrl === fixture.url, `config baseUrl ${config.jira.baseUrl} != ${fixture.url}`);
	const jira = createJiraClient(config, run, fetch);
	const state = createStateStore(stateRoot());
	const checkouts = createCheckoutService(config, run, state);
	const handoff = createHandoffService(config, jira, state);
	const launch = createLauncher(config, PROJECT_ROOT, {});
	const transition = createTransitionService(config, jira, state);

	// Jira read path
	const found = await jira.search(config.search.jql, signal);
	check(found.items.some((i) => i.key === "TEST-1"), "search did not return TEST-1");
	const issue = await jira.issue("TEST-1", signal);
	check(issue.status.id === "1" && issue.status.name === "To Do", `TEST-1 status ${JSON.stringify(issue.status)}`);
	check(issue.description.includes(KOREAN_DESCRIPTION), "ADF description did not round-trip Korean text");
	const comments = await jira.comments("TEST-1", 10, signal);
	check(comments.items.length === 2 && !comments.hasMore, `expected 2 comments, got ${comments.items.length}`);

	// Git New / reuse / Existing
	const dirtyBefore = await fsp.readFile(roots.gitDirtyFile);
	const gitRepo = await checkouts.resolveRepository(roots.gitRepo, "git", signal);
	check(gitRepo.backend === "git", `git repo resolved as ${gitRepo.backend}`);
	const head = (await sh("git", ["-C", roots.gitRepo, "rev-parse", "HEAD"], roots.gitRepo)).trim();
	const gitPlan = await checkouts.planCheckout(gitRepo, { kind: "new", name: "TEST-1" }, signal);
	check(gitPlan.baseIds.length === 1 && gitPlan.baseIds[0] === head, `git plan base ${gitPlan.baseIds.join(",")} != HEAD ${head}`);
	const gitPrepared = await checkouts.prepareCheckout(gitPlan, signal);
	check(gitPrepared.created, "git New did not create a worktree");
	const branchId = (await sh("git", ["-C", roots.gitRepo, "rev-parse", "refs/heads/TEST-1"], roots.gitRepo)).trim();
	check(branchId === head, `branch TEST-1 at ${branchId}, expected ${head}`);
	check((await fsp.stat(gitPrepared.checkout.path)).isDirectory(), "git worktree path missing");
	check(gitPrepared.checkout.path === gitPlan.destination, "git worktree path differs from plan destination");
	check(Buffer.compare(dirtyBefore, await fsp.readFile(roots.gitDirtyFile)) === 0, "source dirty file changed");
	const gitAgain = await checkouts.prepareCheckout(
		await checkouts.planCheckout(gitRepo, { kind: "new", name: "TEST-1" }, signal),
		signal,
	);
	check(!gitAgain.created && gitAgain.checkout.path === gitPrepared.checkout.path, "git New re-run did not reuse");
	const gitList = await checkouts.listCheckouts(gitRepo, signal);
	const gitExisting = gitList.find((c) => c.name === "TEST-1");
	check(gitExisting !== undefined, "git listCheckouts missing TEST-1");
	const existingPrepared = await checkouts.prepareCheckout(
		await checkouts.planCheckout(gitRepo, { kind: "existing", checkout: gitExisting, recoveryAcknowledged: false }, signal),
		signal,
	);
	check(
		!existingPrepared.created && existingPrepared.checkout.path === gitPrepared.checkout.path,
		"git Existing did not open the same worktree",
	);

	// jj New
	const jjRepo = await checkouts.resolveRepository(roots.jjRepo, "jj", signal);
	check(jjRepo.backend === "jj", `jj repo resolved as ${jjRepo.backend}`);
	const jjPlan = await checkouts.planCheckout(jjRepo, { kind: "new", name: "TEST-1" }, signal);
	const jjPrepared: PreparedCheckout = await checkouts.prepareCheckout(jjPlan, signal);
	check(jjPrepared.created, "jj New did not create a workspace");
	const jjNames = (await checkouts.listCheckouts(jjRepo, signal)).map((c) => c.name);
	check(jjNames.includes("TEST-1"), `jj workspaces ${jjNames.join(",")} lack TEST-1`);
	const jjParents = (
		await sh(
			"jj",
			[
				"--no-pager",
				"--color=never",
				"-R",
				jjPrepared.checkout.path,
				"log",
				"--no-graph",
				"-r",
				"@",
				"-T",
				'parents.map(|c| c.commit_id() ++ "\\n").join("")',
			],
			jjPrepared.checkout.path,
		)
	)
		.split("\n")
		.filter((line) => line !== "");
	check(
		jjParents.length === jjPlan.baseIds.length && jjParents.every((id, i) => id === jjPlan.baseIds[i]),
		`jj parents ${jjParents.join(",")} != plan ${jjPlan.baseIds.join(",")}`,
	);
	check((await fsp.stat(path.join(jjPrepared.checkout.path, ".jj"))).isDirectory(), "jj workspace has no .jj");
	check(
		(await fsp.lstat(path.join(jjPrepared.checkout.path, ".git")).catch(() => undefined)) === undefined,
		"jj workspace unexpectedly has .git",
	);

	// Snapshot
	const snapshot = await handoff.buildSnapshot("TEST-1", signal);
	const snapshotPath = await handoff.saveSnapshot(snapshot, DEFAULT_INSTRUCTION);
	const fileMode = (await fsp.stat(snapshotPath)).mode & 0o777;
	const dirMode = (await fsp.stat(path.dirname(snapshotPath))).mode & 0o777;
	check(fileMode === 0o600, `snapshot file mode ${fileMode.toString(8)} != 600`);
	check(dirMode === 0o700, `snapshot dir mode ${dirMode.toString(8)} != 700`);
	const snapshotText = await fsp.readFile(snapshotPath, "utf8");
	check(!snapshotText.includes(FAKE_TOKEN), "snapshot contains the fixture token");
	check(snapshotText.includes(KOREAN_DESCRIPTION), "snapshot lacks the Korean description");

	// Launch without any multiplexer → manual
	const launched = await launch(gitPrepared, snapshotPath, "pane", { ui: {} } as unknown as ExtensionCommandContext);
	check(launched.state === "manual", `launch state ${launched.state}, expected manual`);
	const manualCommand = posixQuote(launched.argv);

	// Transitions
	const posts0 = postCount(fixture);
	const t31 = scriptedCtx("31", [true]);
	await transition("TEST-1", t31.ctx, signal);
	check(postCount(fixture) - posts0 === 1, `transition 31 sent ${postCount(fixture) - posts0} POSTs, expected 1`);
	const afterStart = await jira.issue("TEST-1", signal);
	check(afterStart.status.id === "3" && afterStart.status.name === "In Progress", `status after 31: ${JSON.stringify(afterStart.status)}`);
	check(t31.notes.some((n) => n.includes("전환 완료 확인")), `31 notes: ${t31.notes.join(" | ")}`);

	const posts1 = postCount(fixture);
	const t41 = scriptedCtx("41", []);
	await transition("TEST-1", t41.ctx, signal);
	check(postCount(fixture) === posts1, "transition 41 sent a POST");
	check(t41.notes.some((n) => n.includes("Jira 웹")), `41 notes lack web guidance: ${t41.notes.join(" | ")}`);

	const tCancel = scriptedCtx("31", [false]);
	await transition("TEST-1", tCancel.ctx, signal);
	check(postCount(fixture) === posts1, "cancelled transition sent a POST");

	const t51 = scriptedCtx("51", [true]);
	await transition("TEST-1", t51.ctx, signal);
	check(postCount(fixture) === posts1 + 1, `transition 51 sent ${postCount(fixture) - posts1} POSTs, expected 1`);
	check(t51.notes.some((n) => n.includes("불확실")), `51 notes lack unknown result: ${t51.notes.join(" | ")}`);
	const recordPath = path.join(
		stateRoot(),
		"transitions",
		`${createHash("sha256").update(JSON.stringify([config.jira.baseUrl, "TEST-1"])).digest("hex")}.json`,
	);
	const record: unknown = JSON.parse(await fsp.readFile(recordPath, "utf8"));
	const recordState = typeof record === "object" && record !== null && "state" in record ? record.state : undefined;
	check(recordState === "unknown", `transition record state ${String(recordState)}, expected unknown`);

	const tBlocked = scriptedCtx("31", [false]);
	await transition("TEST-1", tBlocked.ctx, signal);
	check(postCount(fixture) === posts1 + 1, "blocked run sent a POST");
	check(tBlocked.selects === 0, "blocked run reached transition selection");
	check(tBlocked.confirmTitles.length === 1, `blocked run confirms: ${tBlocked.confirmTitles.join(",")}`);
	check(
		tBlocked.notes.some((n) => n.includes("새 전환 요청을 보내지 않았습니다")),
		`blocked notes: ${tBlocked.notes.join(" | ")}`,
	);

	const finalIssue = await jira.issue("TEST-1", signal);
	check(finalIssue.status.id === "3", `final status ${JSON.stringify(finalIssue.status)}`);
	check(fixture.unauthorized() === 0, `${fixture.unauthorized()} unauthorized requests`);

	const rows: [string, string][] = [
		...[...fixture.counts.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([k, v]): [string, string] => [`api ${k}`, String(v)]),
		["git branch", `TEST-1 @ ${branchId}`],
		["git worktree", gitPrepared.checkout.path],
		["git base id", head],
		["jj workspace", `TEST-1 @ ${jjPrepared.checkout.path}`],
		["jj parents", jjParents.join(", ")],
		["jj plan baseIds", jjPlan.baseIds.join(", ")],
		["snapshot", snapshotPath],
		["snapshot modes", `file ${fileMode.toString(8).padStart(4, "0")}, dir ${dirMode.toString(8).padStart(4, "0")}`],
		["launch", `${launched.state}: ${launched.reason}`],
		["manual command", manualCommand],
		["final status", `${finalIssue.status.name} (${finalIssue.status.id})`],
	];
	const width = Math.max(...rows.map(([k]) => k.length));
	console.log("omp-jira-worktree smoke: PASS");
	for (const [k, v] of rows) console.log(`  ${k.padEnd(width)}  ${v}`);
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

async function main(): Promise<number> {
	const fixture = startFixture(SERVE);
	let roots: Roots | undefined;
	try {
		roots = await buildRoots(fixture.url);
		if (SERVE) {
			const lines = [
				"omp-jira-worktree fixture is serving (Ctrl-C to stop; fixtures are kept)",
				`  server URL      ${fixture.url}`,
				`  config file     ${roots.configFile}`,
				`  XDG_STATE_HOME  ${roots.stateHome}`,
				`  state root      ${path.join(roots.stateHome, "omp-jira-worktree")}`,
				`  worktree root   ${roots.worktreeRoot}`,
				`  git repo        ${roots.gitRepo}`,
				`  jj repo         ${roots.jjRepo}`,
				`  token fixture   ${roots.tokenFile}`,
				"",
				"Run:",
				`  OMP_JIRA_WORKTREE_CONFIG=${posixQuote([roots.configFile])} XDG_STATE_HOME=${posixQuote([roots.stateHome])} omp --cwd ${posixQuote([roots.gitRepo])} --extension ${posixQuote([PROJECT_ROOT])}`,
				"",
			];
			console.log(lines.join("\n"));
			const { promise, resolve } = Promise.withResolvers<string>();
			process.once("SIGINT", () => resolve("SIGINT"));
			process.once("SIGTERM", () => resolve("SIGTERM"));
			const sig = await promise;
			fixture.stop();
			console.log(`${sig}: fixture stopped; files kept at ${roots.top}`);
			return 0;
		}
		await scenario(fixture, roots);
		return 0;
	} catch (error) {
		const reason = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
		console.error(`omp-jira-worktree smoke: FAIL — ${reason}`);
		if (SERVE && roots) console.error(`fixtures kept at ${roots.top}`);
		return 1;
	} finally {
		if (!SERVE) {
			fixture.stop();
			if (roots) await fsp.rm(roots.top, { recursive: true, force: true });
		}
	}
}

process.exit(await main());
