import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { TerminalLaunchRequest, TerminalLaunchResult } from "@oh-my-pi/pi-coding-agent/subprocess/terminal-launch";
import type { Config, ExtensionCommandContext, LaunchResult, PreparedCheckout } from "../src/contracts.ts";
import { LAUNCH_MESSAGE } from "../src/contracts.ts";
import { createLauncher, posixQuote } from "../src/launch.ts";

const TOKEN_FIXTURE = "FIXTURE-TOKEN-7f3c9e";
const EMAIL_FIXTURE = "fixture-user@example.com";
const EXTENSION_PATH = "/opt/omp-jira-worktree/extension.ts";
const NO_MUX: NodeJS.ProcessEnv = {};
const TMUX: NodeJS.ProcessEnv = { TMUX: "/tmp/tmux-501/default,123,0", TMUX_PANE: "%3" };
const HERDR: NodeJS.ProcessEnv = { HERDR_ENV: "1", HERDR_PANE_ID: "pane-1", HERDR_WORKSPACE_ID: "ws-1" };
const SCREEN: NodeJS.ProcessEnv = { STY: "1234.pts-0.host" };

const config: Config = {
	jira: {
		baseUrl: "https://jira.example.test",
		deployment: "cloud",
		auth: "basic",
		email: EMAIL_FIXTURE,
		tokenCommand: ["printf", TOKEN_FIXTURE],
		maxResults: 50,
		timeoutMs: 20_000,
	},
	filters: [],
	search: { jql: 'text ~ "{query}"' },
	worktree: { repos: {}, prefer: "jj", root: "/tmp/checkouts", gitBase: null, jjBase: null },
	handoff: { maxComments: 50, maxDescriptionChars: 6000, maxCommentChars: 12000, maxChars: 64000 },
	omp: { binary: "omp", profile: "work" },
};

let checkoutPath = "";
let snapshotPath = "";
let prepared: PreparedCheckout;

beforeAll(async () => {
	checkoutPath = await mkdtemp(join(tmpdir(), "jira-launch-"));
	snapshotPath = join(checkoutPath, "state", "handoffs", "0b7c.md");
	prepared = {
		repository: { backend: "git", key: "git:/repo/.git", root: "/repo", name: "repo" },
		checkout: {
			repositoryKey: "git:/repo/.git",
			name: "TEST-1",
			path: checkoutPath,
			detached: false,
			source: false,
			locked: false,
			prunable: false,
		},
		created: true,
	};
});

afterAll(async () => {
	await rm(checkoutPath, { recursive: true, force: true });
});

type UiFake = {
	confirms: string[];
	requests: TerminalLaunchRequest[];
	ctx: ExtensionCommandContext;
};

/** Minimal UI surface used by the launcher; the cast is confined to this test fixture. */
function fakeContext(options: {
	confirm?: boolean;
	openTerminal?: (request: TerminalLaunchRequest) => Promise<TerminalLaunchResult>;
}): UiFake {
	const confirms: string[] = [];
	const requests: TerminalLaunchRequest[] = [];
	const ui: Pick<ExtensionCommandContext["ui"], "confirm" | "openTerminal"> = {
		confirm: (title) => {
			confirms.push(title);
			return Promise.resolve(options.confirm ?? false);
		},
	};
	const open = options.openTerminal;
	if (open) {
		ui.openTerminal = (request) => {
			requests.push(request);
			return open(request);
		};
	}
	return { confirms, requests, ctx: { ui } as unknown as ExtensionCommandContext };
}

function expectedArgv(): string[] {
	return [
		"omp",
		"--cwd",
		checkoutPath,
		"--profile",
		"work",
		"--extension",
		EXTENSION_PATH,
		`@${snapshotPath}`,
		"--",
		LAUNCH_MESSAGE,
	];
}

function expectManualOrUnknown(result: LaunchResult, state: "manual" | "unknown") {
	expect(result.state).toBe(state);
	if (result.state === "requested") throw new Error("unexpected requested result");
	expect(result.argv).toEqual(expectedArgv());
	expect(result.cwd).toBe(checkoutPath);
	expect(result.reason.length).toBeGreaterThan(0);
	return result;
}

describe("launchOmp", () => {
	test("no multiplexer → manual with exact argv and cwd, no terminal request", async () => {
		const fake = fakeContext({ openTerminal: () => Promise.resolve({ multiplexer: "tmux", placement: "pane" }) });
		const result = await createLauncher(config, EXTENSION_PATH, NO_MUX)(prepared, snapshotPath, "pane", fake.ctx);

		const manual = expectManualOrUnknown(result, "manual");
		expect(fake.requests).toHaveLength(0);
		const argv = manual.argv;
		const at = argv.indexOf(`@${snapshotPath}`);
		const separator = argv.indexOf("--");
		expect(at).toBeGreaterThan(0);
		expect(separator).toBe(at + 1);
		for (const word of argv) {
			expect(word).not.toContain(TOKEN_FIXTURE);
			expect(word).not.toContain(EMAIL_FIXTURE);
		}
	});

	test("openTerminal unavailable → manual", async () => {
		const fake = fakeContext({});
		const result = await createLauncher(config, EXTENSION_PATH, TMUX)(prepared, snapshotPath, "pane", fake.ctx);

		expectManualOrUnknown(result, "manual");
	});

	test("openTerminal throws → unknown, requested exactly once", async () => {
		const fake = fakeContext({ openTerminal: () => Promise.reject(new Error("tmux split-window exited 1")) });
		const result = await createLauncher(config, EXTENSION_PATH, TMUX)(prepared, snapshotPath, "pane", fake.ctx);

		const unknown = expectManualOrUnknown(result, "unknown");
		expect(unknown.reason).toContain("tmux split-window exited 1");
		expect(fake.requests).toHaveLength(1);
	});

	test("malformed openTerminal response → unknown", async () => {
		const fake = fakeContext({
			openTerminal: () => Promise.resolve({ multiplexer: "tmux", placement: "pane", id: 42 } as unknown as TerminalLaunchResult),
		});
		const result = await createLauncher(config, EXTENSION_PATH, TMUX)(prepared, snapshotPath, "pane", fake.ctx);

		expectManualOrUnknown(result, "unknown");
		expect(fake.requests).toHaveLength(1);
	});

	test("successful response → requested with native id and warning", async () => {
		const fake = fakeContext({
			openTerminal: () =>
				Promise.resolve({ multiplexer: "tmux", placement: "window", id: "@7", warning: "started in background" }),
		});
		const result = await createLauncher(config, EXTENSION_PATH, TMUX)(prepared, snapshotPath, "window", fake.ctx);

		expect(result).toEqual({ state: "requested", id: "@7", warning: "started in background" });
		expect(fake.confirms).toHaveLength(0);
		expect(fake.requests).toEqual([{ multiplexer: "tmux", placement: "window", command: expectedArgv(), cwd: checkoutPath }]);
	});

	test("unsupported multiplexer (screen) → manual, openTerminal never called", async () => {
		const fake = fakeContext({ openTerminal: () => Promise.resolve({ multiplexer: "tmux", placement: "pane" }) });
		const result = await createLauncher(config, EXTENSION_PATH, SCREEN)(prepared, snapshotPath, "pane", fake.ctx);

		expectManualOrUnknown(result, "manual");
		expect(fake.requests).toHaveLength(0);
	});

	test("Herdr with POSIX shell declined → manual, openTerminal never called", async () => {
		const fake = fakeContext({
			confirm: false,
			openTerminal: () => Promise.resolve({ multiplexer: "herdr", placement: "pane", id: "pane-2" }),
		});
		const result = await createLauncher(config, EXTENSION_PATH, HERDR)(prepared, snapshotPath, "pane", fake.ctx);

		expectManualOrUnknown(result, "manual");
		expect(fake.confirms).toHaveLength(1);
		expect(fake.requests).toHaveLength(0);
	});

	test("Herdr with POSIX shell confirmed → request asserts posix grammar", async () => {
		const fake = fakeContext({
			confirm: true,
			openTerminal: () => Promise.resolve({ multiplexer: "herdr", placement: "pane", id: "pane-2" }),
		});
		const result = await createLauncher(config, EXTENSION_PATH, HERDR)(prepared, snapshotPath, "pane", fake.ctx);

		expect(result).toEqual({ state: "requested", id: "pane-2" });
		expect(fake.requests).toHaveLength(1);
		expect(fake.confirms).toHaveLength(1);
		expect(fake.requests).toEqual([
			{ multiplexer: "herdr", placement: "pane", command: expectedArgv(), cwd: checkoutPath, shellGrammar: "posix" },
		]);
	});
});

describe("posixQuote", () => {
	test("single-quotes words that need it and round-trips through sh", async () => {
		const argv = ["omp", "--cwd", "/tmp/a b", "@/x/it's.md", "--", LAUNCH_MESSAGE, "", "$HOME;`id`"];
		const quoted = posixQuote(argv);

		expect(quoted.startsWith("omp --cwd '/tmp/a b' ")).toBe(true);
		const proc = Bun.spawn(["/bin/sh", "-c", `for a in ${quoted}; do printf '%s\\0' "$a"; done`], { stdout: "pipe" });
		const out = await new Response(proc.stdout).text();
		await proc.exited;
		expect(out.split("\0").slice(0, -1)).toEqual(argv);
	});
});
