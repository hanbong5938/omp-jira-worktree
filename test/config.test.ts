import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { ConfigError, ConfigMissingError, configPath, loadConfig } from "../src/config.ts";

const ENV_KEYS = ["OMP_JIRA_WORKTREE_CONFIG", "XDG_CONFIG_HOME"] as const;
let saved: Record<string, string | undefined> = {};
let dir = "";

const minimal = {
	jira: {
		baseUrl: "https://example.atlassian.net/",
		deployment: "cloud",
		auth: "basic",
		email: "you@example.com",
		tokenCommand: ["printf", "x"],
	},
};

async function load(doc: unknown) {
	const path = join(dir, "config.json");
	await writeFile(path, JSON.stringify(doc));
	process.env.OMP_JIRA_WORKTREE_CONFIG = path;
	return loadConfig();
}

function withJira(patch: Record<string, unknown>) {
	return { ...minimal, jira: { ...minimal.jira, ...patch } };
}

beforeEach(async () => {
	saved = Object.fromEntries(ENV_KEYS.map((k) => [k, process.env[k]]));
	dir = await mkdtemp(join(tmpdir(), "omp-jira-config-"));
});

afterEach(async () => {
	for (const k of ENV_KEYS) {
		const value = saved[k];
		if (value === undefined) delete process.env[k];
		else process.env[k] = value;
	}
	await rm(dir, { recursive: true, force: true });
});

describe("loadConfig", () => {
	test("missing file reports the exact XDG path", async () => {
		delete process.env.OMP_JIRA_WORKTREE_CONFIG;
		process.env.XDG_CONFIG_HOME = dir;
		const expected = join(dir, "omp-jira-worktree", "config.json");
		expect(configPath()).toBe(expected);
		const error = await loadConfig().then(
			() => undefined,
			(e: unknown) => e,
		);
		expect(error).toBeInstanceOf(ConfigMissingError);
		expect(error instanceof ConfigMissingError && error.path).toBe(expected);
	});

	test("relative override path is rejected", () => {
		process.env.OMP_JIRA_WORKTREE_CONFIG = "relative/config.json";
		expect(() => configPath()).toThrow(ConfigError);
	});

	test("minimal config fills defaults and strips trailing slash", async () => {
		const config = await load(minimal);
		expect(config.jira.baseUrl).toBe("https://example.atlassian.net");
		expect(config.jira.maxResults).toBe(50);
		expect(config.jira.timeoutMs).toBe(20000);
		expect(config.filters).toEqual([]);
		expect(config.search.jql).toContain("{query}");
		expect(config.worktree).toEqual({
			repos: {},
			prefer: "jj",
			root: join(homedir(), ".local/share/omp-jira-worktree/checkouts"),
			gitBase: null,
			jjBase: null,
		});
		expect(config.handoff).toEqual({ maxComments: 50, maxDescriptionChars: 6000, maxCommentChars: 12000, maxChars: 64000 });
		expect(config.omp).toEqual({ binary: "omp", profile: "default" });
	});

	test("context path is preserved and loopback http allowed", async () => {
		const config = await load(withJira({ baseUrl: "http://127.0.0.1:8080/jira//", deployment: "dc", auth: "bearer", email: undefined }));
		expect(config.jira.baseUrl).toBe("http://127.0.0.1:8080/jira");
		expect(config.jira.email).toBeNull();
	});

	test("repo paths expand ~/ and reject relative paths", async () => {
		const config = await load({ ...minimal, worktree: { repos: { ABC: "~/src/abc" } } });
		expect(config.worktree.repos).toEqual({ ABC: join(homedir(), "src/abc") });
		await expect(load({ ...minimal, worktree: { repos: { ABC: "src/abc" } } })).rejects.toThrow("$.worktree.repos.ABC");
	});

	const invalid: [string, unknown, string][] = [
		["unknown key", { ...minimal, extra: 1 }, "$.extra"],
		["unknown nested key", withJira({ token: "x" }), "$.jira.token"],
		["maxResults range", withJira({ maxResults: 1001 }), "$.jira.maxResults"],
		["timeoutMs range", withJira({ timeoutMs: 999 }), "$.jira.timeoutMs"],
		["maxChars range", { ...minimal, handoff: { maxChars: 100 } }, "$.handoff.maxChars"],
		["basic without email", withJira({ email: undefined }), "$.jira.email"],
		["search without {query}", { ...minimal, search: { jql: "project = X" } }, "$.search.jql"],
		["search with two {query}", { ...minimal, search: { jql: "summary ~ \"{query}\" OR text ~ \"{query}\"" } }, "$.search.jql"],
		["http non-loopback", withJira({ baseUrl: "http://jira.example.com" }), "$.jira.baseUrl"],
		["baseUrl with query", withJira({ baseUrl: "https://jira.example.com/?a=1" }), "$.jira.baseUrl"],
		["baseUrl with fragment", withJira({ baseUrl: "https://jira.example.com/#x" }), "$.jira.baseUrl"],
		["baseUrl with userinfo", withJira({ baseUrl: "https://u:p@jira.example.com" }), "$.jira.baseUrl"],
		["missing tokenCommand", withJira({ tokenCommand: undefined }), "$.jira.tokenCommand"],
		["tokenCommand as string", withJira({ tokenCommand: "security find" }), "$.jira.tokenCommand"],
		["bad deployment", withJira({ deployment: "server" }), "$.jira.deployment"],
		["bad prefer", { ...minimal, worktree: { prefer: "hg" } }, "$.worktree.prefer"],
		["empty base", { ...minimal, worktree: { gitBase: "" } }, "$.worktree.gitBase"],
	];
	for (const [name, doc, path] of invalid) {
		test(`rejects ${name}`, async () => {
			await expect(load(doc)).rejects.toThrow(path);
		});
	}

	test("invalid JSON is a ConfigError", async () => {
		const path = join(dir, "config.json");
		await writeFile(path, "{");
		process.env.OMP_JIRA_WORKTREE_CONFIG = path;
		await expect(loadConfig()).rejects.toBeInstanceOf(ConfigError);
	});
});
