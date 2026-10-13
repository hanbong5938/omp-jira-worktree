/**
 * User configuration loading and strict validation. Reads only the user-level
 * config file; environment variables select the file location and never
 * supply credentials. Importing this module has no side effects.
 */
import { homedir } from "node:os";
import { isAbsolute, join } from "node:path";
import type { Backend, Config } from "./contracts.ts";
import { isRecord } from "./text.ts";

/** Thrown when the config file does not exist; carries the exact path for `/jira` guidance. */
export class ConfigMissingError extends Error {
	readonly path: string;
	constructor(path: string) {
		super(`omp-jira-worktree config not found: ${path} (see config.example.json in the extension package)`);
		this.name = "ConfigMissingError";
		this.path = path;
	}
}

/** Validation or parse failure with a JSON-path-like location prefix. */
export class ConfigError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "ConfigError";
	}
}

/** Resolved config file path: absolute `OMP_JIRA_WORKTREE_CONFIG`, else XDG config location. */
export function configPath(): string {
	const override = process.env.OMP_JIRA_WORKTREE_CONFIG;
	if (override !== undefined && override !== "") {
		if (!isAbsolute(override)) {
			throw new ConfigError(`OMP_JIRA_WORKTREE_CONFIG must be an absolute path: ${JSON.stringify(override)}`);
		}
		return override;
	}
	const xdg = process.env.XDG_CONFIG_HOME;
	const base = xdg !== undefined && isAbsolute(xdg) ? xdg : join(homedir(), ".config");
	return join(base, "omp-jira-worktree", "config.json");
}

export async function loadConfig(): Promise<Config> {
	const path = configPath();
	const file = Bun.file(path);
	if (!(await file.exists())) throw new ConfigMissingError(path);
	let raw: unknown;
	try {
		raw = JSON.parse(await file.text());
	} catch (error) {
		throw new ConfigError(`${path}: invalid JSON: ${error instanceof Error ? error.message : String(error)}`);
	}
	return parseConfig(raw);
}

function fail(path: string, message: string): never {
	throw new ConfigError(`${path}: ${message}`);
}

function object(value: unknown, path: string, keys: readonly string[]): Record<string, unknown> {
	if (!isRecord(value)) fail(path, "must be an object");
	for (const key of Object.keys(value)) {
		if (!keys.includes(key)) fail(`${path}.${key}`, "unknown key");
	}
	return value;
}

function optionalObject(value: unknown, path: string, keys: readonly string[]): Record<string, unknown> {
	return value === undefined ? {} : object(value, path, keys);
}

function nonEmptyString(value: unknown, path: string): string {
	if (typeof value !== "string" || value.trim().length === 0) fail(path, "must be a non-empty string");
	return value;
}

function oneOf<T extends string>(value: unknown, path: string, allowed: readonly T[]): T {
	const match = allowed.find((a) => a === value);
	if (match === undefined) fail(path, `must be one of ${allowed.map((a) => JSON.stringify(a)).join(", ")}`);
	return match;
}

function integer(value: unknown, path: string, fallback: number, min: number, max: number): number {
	if (value === undefined) return fallback;
	if (typeof value !== "number" || !Number.isSafeInteger(value)) fail(path, "must be an integer");
	if (value < min || value > max) fail(path, `must be between ${min} and ${max}`);
	return value;
}

function nullableBase(value: unknown, path: string): string | null {
	if (value === undefined || value === null) return null;
	return nonEmptyString(value, path);
}

function expandPath(value: unknown, path: string): string {
	const text = nonEmptyString(value, path);
	if (text.startsWith("~/")) return join(homedir(), text.slice(2));
	if (!isAbsolute(text)) fail(path, "must be an absolute path or start with ~/");
	return text;
}

const LOOPBACK_HOSTS: Record<string, true> = { localhost: true, "127.0.0.1": true, "[::1]": true };

function normalizeBaseUrl(value: unknown, path: string): string {
	const text = nonEmptyString(value, path);
	let url: URL;
	try {
		url = new URL(text);
	} catch {
		fail(path, "must be a valid URL");
	}
	if (url.username !== "" || url.password !== "" || text.includes("@")) fail(path, "must not contain userinfo");
	if (url.search !== "" || text.includes("?")) fail(path, "must not contain a query");
	if (url.hash !== "" || text.includes("#")) fail(path, "must not contain a fragment");
	if (url.protocol === "http:") {
		if (LOOPBACK_HOSTS[url.hostname] !== true) {
			fail(path, "must use https (http is allowed only for localhost, 127.0.0.1, [::1])");
		}
	} else if (url.protocol !== "https:") {
		fail(path, "must use https");
	}
	return `${url.origin}${url.pathname.replace(/\/+$/, "")}`;
}

const PROJECT_KEY = /^[A-Za-z][A-Za-z0-9_]*$/;

/** Validate a parsed config document and fill defaults. Exported for direct validation of in-memory values. */
export function parseConfig(raw: unknown): Config {
	const root = object(raw, "$", ["jira", "filters", "search", "worktree", "handoff", "omp"]);

	if (root.jira === undefined) fail("$.jira", "is required");
	const jira = object(root.jira, "$.jira", [
		"baseUrl",
		"deployment",
		"auth",
		"email",
		"tokenCommand",
		"maxResults",
		"timeoutMs",
	]);
	if (jira.baseUrl === undefined) fail("$.jira.baseUrl", "is required");
	const baseUrl = normalizeBaseUrl(jira.baseUrl, "$.jira.baseUrl");
	if (jira.deployment === undefined) fail("$.jira.deployment", "is required");
	const deployment = oneOf(jira.deployment, "$.jira.deployment", ["cloud", "dc"] as const);
	if (jira.auth === undefined) fail("$.jira.auth", "is required");
	const auth = oneOf(jira.auth, "$.jira.auth", ["basic", "bearer"] as const);
	let email: string | null = null;
	if (jira.email !== undefined && jira.email !== null) email = nonEmptyString(jira.email, "$.jira.email");
	if (auth === "basic" && email === null) fail("$.jira.email", "is required when auth is \"basic\"");
	if (jira.tokenCommand === undefined) fail("$.jira.tokenCommand", "is required");
	if (!Array.isArray(jira.tokenCommand) || jira.tokenCommand.length === 0) {
		fail("$.jira.tokenCommand", "must be a non-empty array of strings (argv)");
	}
	const tokenCommand = jira.tokenCommand.map((arg: unknown, i: number) => {
		if (typeof arg !== "string") fail(`$.jira.tokenCommand[${i}]`, "must be a string");
		return arg;
	});
	if ((tokenCommand[0] ?? "") === "") fail("$.jira.tokenCommand[0]", "must be a non-empty executable");

	let filters: Config["filters"] = [];
	if (root.filters !== undefined) {
		if (!Array.isArray(root.filters)) fail("$.filters", "must be an array");
		filters = root.filters.map((entry: unknown, i: number) => {
			const f = object(entry, `$.filters[${i}]`, ["name", "jql"]);
			return {
				name: nonEmptyString(f.name, `$.filters[${i}].name`),
				jql: nonEmptyString(f.jql, `$.filters[${i}].jql`),
			};
		});
	}

	const search = optionalObject(root.search, "$.search", ["jql"]);
	const searchJql =
		search.jql === undefined ? 'text ~ "{query}" ORDER BY updated DESC' : nonEmptyString(search.jql, "$.search.jql");
	if (searchJql.split("{query}").length !== 2) fail("$.search.jql", "must contain {query} exactly once");

	const worktree = optionalObject(root.worktree, "$.worktree", ["repos", "prefer", "root", "gitBase", "jjBase"]);
	const repos: Record<string, string> = {};
	if (worktree.repos !== undefined) {
		if (!isRecord(worktree.repos)) fail("$.worktree.repos", "must be an object");
		for (const [project, repoPath] of Object.entries(worktree.repos)) {
			if (!PROJECT_KEY.test(project)) fail(`$.worktree.repos.${project}`, "key must be a Jira project key");
			const upper = project.toUpperCase();
			if (upper in repos) fail(`$.worktree.repos.${project}`, "duplicate project key");
			repos[upper] = expandPath(repoPath, `$.worktree.repos.${project}`);
		}
	}
	const prefer: Backend =
		worktree.prefer === undefined ? "jj" : oneOf(worktree.prefer, "$.worktree.prefer", ["jj", "git"] as const);
	const checkoutRoot = expandPath(
		worktree.root === undefined ? "~/.local/share/omp-jira-worktree/checkouts" : worktree.root,
		"$.worktree.root",
	);

	const handoff = optionalObject(root.handoff, "$.handoff", [
		"maxComments",
		"maxDescriptionChars",
		"maxCommentChars",
		"maxChars",
	]);
	const omp = optionalObject(root.omp, "$.omp", ["binary", "profile"]);

	return {
		jira: {
			baseUrl,
			deployment,
			auth,
			email,
			tokenCommand,
			maxResults: integer(jira.maxResults, "$.jira.maxResults", 50, 1, 1000),
			timeoutMs: integer(jira.timeoutMs, "$.jira.timeoutMs", 20_000, 1000, 120_000),
		},
		filters,
		search: { jql: searchJql },
		worktree: {
			repos,
			prefer,
			root: checkoutRoot,
			gitBase: nullableBase(worktree.gitBase, "$.worktree.gitBase"),
			jjBase: nullableBase(worktree.jjBase, "$.worktree.jjBase"),
		},
		handoff: {
			maxComments: integer(handoff.maxComments, "$.handoff.maxComments", 50, 0, 1000),
			maxDescriptionChars: integer(
				handoff.maxDescriptionChars,
				"$.handoff.maxDescriptionChars",
				6000,
				1,
				Number.MAX_SAFE_INTEGER,
			),
			maxCommentChars: integer(
				handoff.maxCommentChars,
				"$.handoff.maxCommentChars",
				12_000,
				1,
				Number.MAX_SAFE_INTEGER,
			),
			maxChars: integer(handoff.maxChars, "$.handoff.maxChars", 64_000, 1024, 262_144),
		},
		omp: {
			binary: omp.binary === undefined ? "omp" : nonEmptyString(omp.binary, "$.omp.binary"),
			profile: omp.profile === undefined ? "default" : nonEmptyString(omp.profile, "$.omp.profile"),
		},
	};
}
