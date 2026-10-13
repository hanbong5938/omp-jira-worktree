import { createHash, randomUUID } from "node:crypto";
import type { Stats } from "node:fs";
import * as fsp from "node:fs/promises";
import path from "node:path";
import {
	type Backend,
	type Checkout,
	type CheckoutPlan,
	type CheckoutPort,
	type CheckoutSelection,
	type Config,
	type FsIdentity,
	freezePlan,
	NATIVE_ADD_TIMEOUT_MS,
	type PreparedCheckout,
	type Repository,
	RUN_TIMEOUT_MS,
	type Run,
	type RunResult,
	type StateStore,
} from "./contracts.ts";

/** Minimum supported jj version (major, minor, patch). */
const JJ_MINIMUM = [0, 46, 0] as const;
// Each template is passed as one argv element; `\\n` is a literal backslash + n for jj.
const JJ_NAME_TEMPLATE = 'json(name) ++ "\\n"';
const JJ_CURRENT_TEMPLATE = 'if(target.current_working_copy(), json(name) ++ "\\n")';
const JJ_COMMIT_TEMPLATE = 'commit_id ++ "\\n"';
const JJ_PARENTS_TEMPLATE = 'parents.map(|c| c.commit_id() ++ "\\n").join("")';
const CONTROL_CHARS = /[\u0000-\u001f\u007f-\u009f]/;
const COMMIT_ID = /^[0-9a-f]{40}(?:[0-9a-f]{24})?$/;
const JJ_COMMIT_ID = /^[0-9a-f]{40,64}$/;

type ExistingSelection = Extract<CheckoutSelection, { kind: "existing" }>;
type ResolvedCheckout = Checkout & { path: string };

type DestinationIndex = { version: 1; backend: Backend; repositoryKey: string; destination: string; name: string };
type ReceiptState = "pending" | "failed" | "ready";
type Receipt = {
	version: 1;
	backend: Backend;
	repositoryKey: string;
	name: string;
	destination: string;
	baseIds: string[];
	attempt: string;
	state: ReceiptState;
	identity?: FsIdentity;
};
type ReceiptLookup = { kind: "none" } | { kind: "ok"; receipt: Receipt } | { kind: "invalid"; reason: string };

type GitWorktree = {
	path: string;
	head?: string;
	branch?: string;
	detached: boolean;
	bare: boolean;
	locked: boolean;
	prunable: boolean;
	prunableReason?: string;
};

/**
 * A receipt (or its absence of proof) blocks automatic use. `selection: "existing"` can be
 * retried with `recoveryAcknowledged: true`; `selection: "new"` must be reopened through Existing.
 */
export class RecoveryRequiredError extends Error {
	readonly checkoutName: string;
	readonly path: string;
	readonly selection: "new" | "existing";
	constructor(selection: "new" | "existing", checkoutName: string, checkoutPath: string, detail: string) {
		const guidance =
			selection === "new"
				? "no native add will be retried; inspect that path manually, then select it as Existing and explicitly acknowledge recovery"
				: "inspect that path manually, then explicitly acknowledge recovery to open it (it is not promoted to Ready)";
		super(`checkout '${checkoutName}' at ${checkoutPath}: ${detail}; ${guidance}`);
		this.name = "RecoveryRequiredError";
		this.checkoutName = checkoutName;
		this.path = checkoutPath;
		this.selection = selection;
	}
}

function errnoCode(error: unknown): string | undefined {
	if (error instanceof Error && "code" in error && typeof error.code === "string") return error.code;
	return undefined;
}

function message(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function sha256(text: string): string {
	return createHash("sha256").update(text).digest("hex");
}

function stripNewline(text: string): string {
	return text.endsWith("\n") ? text.slice(0, -1) : text;
}

function summarize(result: RunResult): string {
	const text = (result.stderr.trim() || result.stdout.trim()).replace(/\s+/g, " ");
	return text.length > 400 ? `${text.slice(0, 400)}…` : text || "(no output)";
}

function isWithin(child: string, parent: string): boolean {
	return child === parent || child.startsWith(parent.endsWith(path.sep) ? parent : parent + path.sep);
}

/** ASCII `[A-Za-z0-9._-]`; anything else becomes `-`; empty/`.`/`..` become `checkout`. */
function slug(text: string): string {
	const value = text.replace(/[^A-Za-z0-9._-]/g, "-");
	return value === "" || value === "." || value === ".." ? "checkout" : value;
}

async function lstatOrUndefined(target: string): Promise<Stats | undefined> {
	try {
		return await fsp.lstat(target);
	} catch (error) {
		if (errnoCode(error) === "ENOENT") return undefined;
		throw error;
	}
}

async function realpathOrUndefined(target: string): Promise<string | undefined> {
	try {
		return await fsp.realpath(target);
	} catch {
		return undefined;
	}
}

/** Nearest existing ancestor's realpath + remaining (missing) components. */
async function canonicalizeMissing(target: string): Promise<string> {
	const rest: string[] = [];
	let current = target;
	for (;;) {
		if (await lstatOrUndefined(current)) {
			let real: string;
			try {
				real = await fsp.realpath(current);
			} catch (error) {
				throw new Error(`cannot resolve ${current} (dangling symlink?): ${message(error)}`);
			}
			if (!(await fsp.stat(real)).isDirectory()) throw new Error(`${current} is not a directory`);
			return path.join(real, ...rest.reverse());
		}
		const parent = path.dirname(current);
		if (parent === current) throw new Error(`no existing ancestor for ${target}`);
		rest.push(path.basename(current));
		current = parent;
	}
}

async function fsIdentity(root: string, backend: Backend): Promise<FsIdentity> {
	const rootStats = await fsp.lstat(root, { bigint: true });
	if (!rootStats.isDirectory()) throw new Error(`${root} is not a directory`);
	const markerPath = path.join(root, backend === "git" ? ".git" : ".jj");
	const marker = await fsp.lstat(markerPath, { bigint: true });
	if (backend === "jj" ? !marker.isDirectory() : !(marker.isDirectory() || marker.isFile())) {
		throw new Error(`invalid checkout marker ${markerPath}`);
	}
	return {
		rootDev: String(rootStats.dev),
		rootIno: String(rootStats.ino),
		markerDev: String(marker.dev),
		markerIno: String(marker.ino),
	};
}

function sameIdentity(a: FsIdentity | undefined, b: FsIdentity): boolean {
	return (
		a !== undefined &&
		a.rootDev === b.rootDev &&
		a.rootIno === b.rootIno &&
		a.markerDev === b.markerDev &&
		a.markerIno === b.markerIno
	);
}

function sameIds(a: readonly string[], b: readonly string[]): boolean {
	return a.length === b.length && a.every((id, index) => id === b[index]);
}

function parseReceipt(value: unknown): Receipt {
	if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("receipt is not an object");
	const record = value as Record<string, unknown>;
	const allowed = ["version", "backend", "repositoryKey", "name", "destination", "baseIds", "attempt", "state", "identity"];
	const unknown = Object.keys(record).filter(key => !allowed.includes(key));
	if (unknown.length > 0) throw new Error(`unknown receipt fields: ${unknown.join(", ")}`);
	const { version, backend, repositoryKey, name, destination, baseIds, attempt, state, identity } = record;
	if (version !== 1) throw new Error("unsupported receipt version");
	if (backend !== "git" && backend !== "jj") throw new Error("invalid receipt backend");
	for (const [field, text] of [
		["repositoryKey", repositoryKey],
		["name", name],
		["destination", destination],
		["attempt", attempt],
	] as const) {
		if (typeof text !== "string" || text === "") throw new Error(`invalid receipt ${field}`);
	}
	if (!Array.isArray(baseIds) || baseIds.length === 0 || !baseIds.every(id => typeof id === "string" && id !== "")) {
		throw new Error("invalid receipt baseIds");
	}
	if (state !== "pending" && state !== "failed" && state !== "ready") throw new Error("invalid receipt state");
	if ((state === "ready") !== (identity !== undefined)) throw new Error("receipt identity does not match its state");
	const receipt: Receipt = {
		version,
		backend,
		repositoryKey: repositoryKey as string,
		name: name as string,
		destination: destination as string,
		baseIds: baseIds as string[],
		attempt: attempt as string,
		state,
	};
	if (identity !== undefined) {
		if (typeof identity !== "object" || identity === null) throw new Error("invalid receipt identity");
		const fields = identity as Record<string, unknown>;
		const keys = ["rootDev", "rootIno", "markerDev", "markerIno"] as const;
		if (Object.keys(fields).length !== keys.length || !keys.every(key => typeof fields[key] === "string")) {
			throw new Error("invalid receipt identity");
		}
		receipt.identity = {
			rootDev: fields.rootDev as string,
			rootIno: fields.rootIno as string,
			markerDev: fields.markerDev as string,
			markerIno: fields.markerIno as string,
		};
	}
	return receipt;
}

function parseDestinationIndex(value: unknown): DestinationIndex {
	if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("receipt index is not an object");
	const record = value as Record<string, unknown>;
	const allowed = ["version", "backend", "repositoryKey", "destination", "name"];
	const unknown = Object.keys(record).filter(key => !allowed.includes(key));
	if (unknown.length > 0) throw new Error(`unknown receipt index fields: ${unknown.join(", ")}`);
	const { version, backend, repositoryKey, destination, name } = record;
	if (version !== 1) throw new Error("unsupported receipt index version");
	if (backend !== "git" && backend !== "jj") throw new Error("invalid receipt index backend");
	for (const [field, text] of [
		["repositoryKey", repositoryKey],
		["destination", destination],
		["name", name],
	] as const) {
		if (typeof text !== "string" || text === "") throw new Error(`invalid receipt index ${field}`);
	}
	return {
		version,
		backend,
		repositoryKey: repositoryKey as string,
		destination: destination as string,
		name: name as string,
	};
}

/** NUL-delimited `git worktree list --porcelain -z`: fields end in NUL, records in an extra NUL. */
function parseGitWorktrees(output: string): GitWorktree[] {
	const worktrees: GitWorktree[] = [];
	let current: GitWorktree | undefined;
	for (const field of output.split("\0")) {
		if (field === "") {
			if (current) worktrees.push(current);
			current = undefined;
			continue;
		}
		const space = field.indexOf(" ");
		const key = space < 0 ? field : field.slice(0, space);
		const value = space < 0 ? undefined : field.slice(space + 1);
		if (key === "worktree") {
			if (current || !value) throw new Error("malformed git worktree list output");
			current = { path: value, detached: false, bare: false, locked: false, prunable: false };
			continue;
		}
		if (!current) throw new Error("malformed git worktree list output");
		if (key === "HEAD") current.head = value;
		else if (key === "branch") current.branch = value?.startsWith("refs/heads/") ? value.slice("refs/heads/".length) : value;
		else if (key === "detached") current.detached = true;
		else if (key === "bare") current.bare = true;
		else if (key === "locked") current.locked = true;
		else if (key === "prunable") {
			current.prunable = true;
			current.prunableReason = value;
		}
	}
	if (current) worktrees.push(current);
	return worktrees;
}

function parseJsonNames(output: string, label: string): string[] {
	return output
		.split("\n")
		.filter(line => line !== "")
		.map(line => {
			let name: unknown;
			try {
				name = JSON.parse(line);
			} catch {
				throw new Error(`${label}: jj template produced invalid JSON`);
			}
			if (typeof name !== "string" || name === "") throw new Error(`${label}: jj template produced an invalid name`);
			return name;
		});
}

export function createCheckoutService(config: Config, run: Run, state: StateStore): CheckoutPort {
	async function exec(
		file: string,
		args: string[],
		signal: AbortSignal,
		cwd?: string,
		timeoutMs = RUN_TIMEOUT_MS,
	): Promise<RunResult> {
		signal.throwIfAborted();
		const result = await run(file, args, { cwd, signal, timeoutMs });
		signal.throwIfAborted();
		return result;
	}

	async function git(root: string, args: string[], signal: AbortSignal): Promise<string> {
		const result = await exec("git", ["-C", root, ...args], signal, root);
		if (result.code !== 0 || result.killed) {
			throw new Error(`git ${args[0] ?? ""} in ${root} failed${result.killed ? " (timed out)" : ""}: ${summarize(result)}`);
		}
		return result.stdout;
	}

	/** Read-only jj invocation prefix: never snapshot or update a working copy. */
	const JJ_READ_PREFIX = ["--no-pager", "--color=never", "--ignore-working-copy", "-R"];

	async function jj(root: string, args: string[], signal: AbortSignal): Promise<string> {
		const result = await exec("jj", [...JJ_READ_PREFIX, root, ...args], signal, root);
		if (result.code !== 0 || result.killed) {
			throw new Error(`jj ${args.slice(0, 2).join(" ")} in ${root} failed${result.killed ? " (timed out)" : ""}: ${summarize(result)}`);
		}
		return result.stdout;
	}

	async function requireJj(signal: AbortSignal): Promise<void> {
		let result: RunResult;
		try {
			result = await exec("jj", ["--version"], signal);
		} catch (error) {
			if (signal.aborted) throw error;
			throw new Error(`jj cannot be started (${message(error)}); jj >= ${JJ_MINIMUM.join(".")} is required for jj repositories`);
		}
		const match = /(\d+)\.(\d+)\.(\d+)/.exec(result.stdout);
		if (result.code !== 0 || !match) throw new Error(`cannot read jj version: ${summarize(result)}`);
		const version = [Number(match[1]), Number(match[2]), Number(match[3])];
		for (let index = 0; index < JJ_MINIMUM.length; index++) {
			const have = version[index] ?? 0;
			const need = JJ_MINIMUM[index] ?? 0;
			if (have > need) return;
			if (have < need) {
				throw new Error(`jj ${match[0]} is unsupported; jj >= ${JJ_MINIMUM.join(".")} is required`);
			}
		}
	}

	async function gitIdentity(root: string, signal: AbortSignal): Promise<Repository> {
		const marker = await lstatOrUndefined(path.join(root, ".git"));
		if (!marker || !(marker.isDirectory() || marker.isFile())) {
			throw new Error(`malformed or missing Git marker at ${path.join(root, ".git")}`);
		}
		const top = await realpathOrUndefined(stripNewline(await git(root, ["rev-parse", "--show-toplevel"], signal)));
		if (top !== root) throw new Error(`Git marker at ${root} is not a Git checkout root (toplevel: ${top ?? "unresolvable"})`);
		const commonDir = stripNewline(await git(root, ["rev-parse", "--path-format=absolute", "--git-common-dir"], signal));
		const store = await realpathOrUndefined(commonDir);
		if (!store) throw new Error(`Git common dir ${commonDir} cannot be resolved`);
		const storeBase = path.basename(store);
		const name =
			storeBase === ".git" ? path.basename(path.dirname(store)) : storeBase.replace(/\.git$/, "") || path.basename(top);
		return { backend: "git", key: `git:${store}`, root, name };
	}

	/** `.jj/repo` is the store directory or a pointer file whose path is relative to `.jj`. */
	async function jjStore(root: string): Promise<string> {
		const markerPath = path.join(root, ".jj");
		const marker = await lstatOrUndefined(markerPath);
		if (!marker?.isDirectory()) throw new Error(`malformed jj workspace marker: ${markerPath}`);
		const repoPath = path.join(markerPath, "repo");
		const repoEntry = await lstatOrUndefined(repoPath);
		if (!repoEntry) throw new Error(`broken jj workspace marker: ${repoPath} is missing`);
		const target = repoEntry.isFile() ? path.resolve(markerPath, await fsp.readFile(repoPath, "utf8")) : repoPath;
		const store = await realpathOrUndefined(target);
		if (!store || !(await fsp.stat(store)).isDirectory()) throw new Error(`jj repository store ${target} is not a directory`);
		return store;
	}

	async function jjIdentity(root: string, signal: AbortSignal): Promise<Repository> {
		const store = await jjStore(root);
		await requireJj(signal);
		const reported = await realpathOrUndefined(stripNewline(await jj(root, ["workspace", "root"], signal)));
		if (reported !== root) throw new Error(`jj reports workspace root ${reported ?? "(unresolvable)"} for ${root}`);
		return { backend: "jj", key: `jj:${store}`, root, name: path.basename(path.dirname(path.dirname(store))) };
	}

	function identify(root: string, backend: Backend, signal: AbortSignal): Promise<Repository> {
		return backend === "git" ? gitIdentity(root, signal) : jjIdentity(root, signal);
	}

	async function verifyRepository(repo: Repository, signal: AbortSignal): Promise<void> {
		const actual = await identify(repo.root, repo.backend, signal);
		if (actual.key !== repo.key || actual.root !== repo.root) {
			throw new Error(`repository at ${repo.root} changed identity (${repo.key} → ${actual.key}); select the repository again`);
		}
	}

	async function gitWorktrees(repo: Repository, signal: AbortSignal): Promise<GitWorktree[]> {
		return parseGitWorktrees(await git(repo.root, ["worktree", "list", "--porcelain", "-z"], signal));
	}

	async function jjNames(root: string, signal: AbortSignal): Promise<string[]> {
		const names = parseJsonNames(await jj(root, ["workspace", "list", "-T", JJ_NAME_TEMPLATE], signal), "jj workspace list");
		if (names.length === 0) throw new Error(`jj workspace list for ${root} returned no workspaces`);
		return names;
	}

	async function jjRoot(root: string, name: string, signal: AbortSignal): Promise<{ path?: string; reason?: string }> {
		const result = await exec("jj", [...JJ_READ_PREFIX, root, "workspace", "root", "--name", name], signal, root);
		if (result.code !== 0 || result.killed) return { reason: `workspace root lookup failed: ${summarize(result)}` };
		const raw = stripNewline(result.stdout);
		const real = raw ? await realpathOrUndefined(raw) : undefined;
		return real ? { path: real } : { reason: `recorded workspace root ${raw || "(empty)"} is not reachable` };
	}

	/** Re-validate a checkout path against native identity; never mutates it. */
	async function validateCheckoutAt(
		repo: Repository,
		checkoutPath: string,
		expected: { name: string; detached: boolean; head?: string },
		signal: AbortSignal,
	): Promise<ResolvedCheckout> {
		const real = await realpathOrUndefined(checkoutPath);
		if (!real) throw new Error(`checkout path ${checkoutPath} does not exist`);
		const actual = await identify(real, repo.backend, signal);
		if (actual.key !== repo.key) throw new Error(`${real} belongs to ${actual.key}, not ${repo.key}`);
		const base = { repositoryKey: repo.key, name: expected.name, path: real, locked: false, prunable: false };
		if (repo.backend === "jj") {
			if (!(await jjNames(repo.root, signal)).includes(expected.name)) {
				throw new Error(`jj workspace '${expected.name}' is not listed in ${repo.root}`);
			}
			const recorded = await jjRoot(repo.root, expected.name, signal);
			if (recorded.path !== undefined && recorded.path !== real) {
				throw new Error(`jj workspace '${expected.name}' is recorded at ${recorded.path}, not ${real}`);
			}
			const current = parseJsonNames(await jj(real, ["workspace", "list", "-T", JJ_CURRENT_TEMPLATE], signal), "jj workspace list");
			if (current.length > 1) {
				throw new Error(`ambiguous jj workspace identity at ${real}: ${current.length} workspaces share the working-copy commit`);
			}
			if (current[0] !== expected.name) {
				throw new Error(`${real} is jj workspace ${JSON.stringify(current[0] ?? null)}, not '${expected.name}'`);
			}
			return { ...base, detached: false, source: real === repo.root };
		}
		const worktrees = await gitWorktrees(repo, signal);
		let index = -1;
		for (const [position, worktree] of worktrees.entries()) {
			if ((await realpathOrUndefined(worktree.path)) === real) index = position;
		}
		const entry = worktrees[index];
		if (!entry) throw new Error(`${real} is not a registered worktree of ${repo.root}`);
		if (entry.bare || entry.prunable) throw new Error(`worktree ${real} is ${entry.bare ? "bare" : "prunable"}`);
		const symbolic = await exec("git", ["-C", real, "symbolic-ref", "--quiet", "HEAD"], signal, real);
		const head = stripNewline(await git(real, ["rev-parse", "--verify", "HEAD"], signal));
		if (expected.detached) {
			if (symbolic.code !== 1) throw new Error(`worktree ${real} is not on a detached HEAD: ${summarize(symbolic)}`);
			if (expected.head !== undefined && head !== expected.head) {
				throw new Error(`detached worktree ${real} moved from ${expected.head} to ${head}; select it again`);
			}
		} else {
			if (symbolic.code !== 0) throw new Error(`worktree ${real} is not on branch '${expected.name}': ${summarize(symbolic)}`);
			const ref = stripNewline(symbolic.stdout);
			if (ref !== `refs/heads/${expected.name}` || entry.branch !== expected.name) {
				throw new Error(`worktree ${real} has ${ref}, not branch '${expected.name}'`);
			}
		}
		return { ...base, head, detached: expected.detached, source: index === 0, locked: entry.locked };
	}

	function receiptPath(repo: Repository, name: string): string {
		return `receipts/${sha256(JSON.stringify([repo.key, name]))}.json`;
	}

	async function loadReceipt(repo: Repository, name: string): Promise<ReceiptLookup> {
		let receipt: Receipt | undefined;
		try {
			receipt = await state.readRecord(receiptPath(repo, name), parseReceipt);
		} catch (error) {
			return { kind: "invalid", reason: `receipt cannot be trusted (${message(error)})` };
		}
		if (!receipt) return { kind: "none" };
		if (receipt.repositoryKey !== repo.key || receipt.name !== name || receipt.backend !== repo.backend) {
			return { kind: "invalid", reason: "receipt belongs to a different repository or checkout" };
		}
		return { kind: "ok", receipt };
	}

	/** Index from a checkout destination to the receipt of the attempt that created it; survives branch switches. */
	function destinationIndexPath(repo: Repository, destination: string): string {
		return `receipts/by-destination/${sha256(JSON.stringify([repo.key, destination]))}.json`;
	}

	async function loadReceiptAt(repo: Repository, destination: string): Promise<ReceiptLookup> {
		let index: DestinationIndex | undefined;
		try {
			index = await state.readRecord(destinationIndexPath(repo, destination), parseDestinationIndex);
		} catch (error) {
			return { kind: "invalid", reason: `receipt index cannot be trusted (${message(error)})` };
		}
		if (!index) return { kind: "none" };
		if (index.repositoryKey !== repo.key || index.destination !== destination || index.backend !== repo.backend) {
			return { kind: "invalid", reason: "receipt index belongs to a different repository or destination" };
		}
		const lookup = await loadReceipt(repo, index.name);
		if (lookup.kind === "none") {
			return { kind: "invalid", reason: `the receipt of the attempt that created it as '${index.name}' is missing` };
		}
		if (lookup.kind === "ok" && lookup.receipt.destination !== destination) {
			return { kind: "invalid", reason: `the receipt of the attempt that created it as '${index.name}' points to ${lookup.receipt.destination}` };
		}
		return lookup;
	}

	async function destinationFor(repo: Repository, name: string): Promise<string> {
		const root = config.worktree.root;
		if (!path.isAbsolute(root)) throw new Error(`worktree.root must be absolute: ${root}`);
		const raw = path.join(
			root,
			`${slug(repo.name)}-${sha256(repo.key).slice(0, 16)}`,
			`${slug(name)}-${sha256(name).slice(0, 8)}`,
		);
		const destination = path.join(await canonicalizeMissing(path.dirname(raw)), path.basename(raw));
		const store = repo.key.slice(repo.key.indexOf(":") + 1);
		if (isWithin(destination, repo.root) || isWithin(destination, store)) {
			throw new Error(`checkout destination ${destination} is inside the source repository; change worktree.root`);
		}
		return destination;
	}

	async function validateName(repo: Repository, name: string, signal: AbortSignal): Promise<void> {
		if (name === "" || name.startsWith("-") || CONTROL_CHARS.test(name)) {
			throw new Error(`invalid checkout name ${JSON.stringify(name)}: empty, leading '-', or control characters`);
		}
		if (repo.backend === "jj") {
			if (name === "." || name === ".." || name.includes("/") || name.includes("\\")) {
				throw new Error(`invalid jj workspace name ${JSON.stringify(name)}: must be a single path component`);
			}
			return;
		}
		if (name.includes("@{-")) throw new Error(`invalid branch name ${JSON.stringify(name)}: '@{-N}' shorthand is not allowed`);
		const result = await exec("git", ["-C", repo.root, "check-ref-format", "--branch", name], signal, repo.root);
		if (result.code !== 0 || stripNewline(result.stdout) !== name) {
			throw new Error(`invalid branch name ${JSON.stringify(name)}: ${summarize(result)}`);
		}
	}

	async function resolveBaseIds(repo: Repository, base: string | undefined, signal: AbortSignal): Promise<string[]> {
		if (repo.backend === "git") {
			const spec = base ?? config.worktree.gitBase ?? "HEAD";
			const result = await exec(
				"git",
				["-C", repo.root, "rev-parse", "--verify", "--end-of-options", `${spec}^{commit}`],
				signal,
				repo.root,
			);
			if (result.code !== 0) {
				if (spec === "HEAD") {
					throw new Error(`source HEAD in ${repo.root} has no commit (unborn branch); commit first or set worktree.gitBase`);
				}
				throw new Error(`base ${JSON.stringify(spec)} does not resolve to a commit: ${summarize(result)}`);
			}
			const id = stripNewline(result.stdout);
			if (!COMMIT_ID.test(id)) throw new Error(`git returned an unexpected commit id for ${JSON.stringify(spec)}`);
			return [id];
		}
		const revset = base ?? config.worktree.jjBase ?? "parents(@)";
		const output = await jj(repo.root, ["log", "--no-graph", `--revision=${revset}`, "-T", JJ_COMMIT_TEMPLATE], signal);
		const ids = output.split("\n").filter(line => line !== "");
		if (ids.length === 0) throw new Error(`jj revset ${JSON.stringify(revset)} resolved to no commits`);
		if (!ids.every(id => JJ_COMMIT_ID.test(id))) throw new Error(`jj returned unexpected commit ids for ${JSON.stringify(revset)}`);
		return ids;
	}

	/** New must not touch an existing branch/workspace/path. */
	async function assertNewTarget(repo: Repository, name: string, destination: string, signal: AbortSignal): Promise<void> {
		if (await lstatOrUndefined(destination)) {
			throw new Error(`checkout destination ${destination} already exists and is never overwritten; inspect it and select Existing`);
		}
		if (repo.backend === "jj") {
			if ((await jjNames(repo.root, signal)).includes(name)) {
				throw new Error(`jj workspace '${name}' already exists; select Existing to open it`);
			}
			return;
		}
		const worktrees = await gitWorktrees(repo, signal);
		const holder = worktrees.find(worktree => worktree.branch === name);
		if (holder) throw new Error(`branch '${name}' is already checked out at ${holder.path}; select Existing to open it`);
		if (worktrees.some(worktree => worktree.path === destination)) {
			throw new Error(`a worktree is already registered at ${destination}; inspect it manually`);
		}
		const ref = await exec("git", ["-C", repo.root, "show-ref", "--verify", "--quiet", `refs/heads/${name}`], signal, repo.root);
		if (ref.code === 0) {
			throw new Error(`branch '${name}' already exists but is not checked out; New never reuses or overwrites a branch — choose another name`);
		}
		if (ref.code !== 1) throw new Error(`cannot check branch '${name}': ${summarize(ref)}`);
	}

	/** Same New request: reuse only a fully matching Ready receipt; any other receipt blocks native add. */
	async function reuseReady(
		repo: Repository,
		name: string,
		destination: string,
		baseIds: readonly string[],
		signal: AbortSignal,
	): Promise<ResolvedCheckout | undefined> {
		const lookup = await loadReceipt(repo, name);
		if (lookup.kind === "none") return undefined;
		if (lookup.kind === "invalid") throw new RecoveryRequiredError("new", name, destination, lookup.reason);
		const { receipt } = lookup;
		if (receipt.state !== "ready") {
			throw new RecoveryRequiredError("new", name, receipt.destination, `a previous native add attempt is ${receipt.state}`);
		}
		if (receipt.destination !== destination || !sameIds(receipt.baseIds, baseIds)) {
			throw new RecoveryRequiredError(
				"new",
				name,
				receipt.destination,
				`an earlier checkout was created with a different destination or base (${receipt.baseIds.join(", ")})`,
			);
		}
		const checkout = await validateCheckoutAt(repo, destination, { name, detached: false }, signal);
		if (!sameIdentity(receipt.identity, await fsIdentity(destination, repo.backend))) {
			throw new RecoveryRequiredError("new", name, destination, "the checkout directory or marker was replaced after creation");
		}
		return checkout;
	}

	async function validateExisting(repo: Repository, selection: ExistingSelection, signal: AbortSignal): Promise<ResolvedCheckout> {
		const selected = selection.checkout;
		if (selected.repositoryKey !== repo.key) throw new Error(`checkout '${selected.name}' belongs to another repository`);
		if (selected.path === undefined) {
			throw new Error(`checkout '${selected.name}' has no known path${selected.unavailableReason ? ` (${selected.unavailableReason})` : ""}; enter its path`);
		}
		if (selected.prunable) throw new Error(`worktree ${selected.path} is prunable and cannot be opened`);
		const checkout = await validateCheckoutAt(
			repo,
			selected.path,
			{ name: selected.name, detached: selected.detached, head: selected.head },
			signal,
		);
		if (selection.recoveryAcknowledged) return checkout;
		// Receipts are found by destination (branch switches and detached HEADs keep it) and, for named
		// checkouts, by name; a detached Git worktree's listed name is not a branch name.
		const lookups = [await loadReceiptAt(repo, checkout.path)];
		if (!(repo.backend === "git" && checkout.detached)) lookups.push(await loadReceipt(repo, checkout.name));
		let current: FsIdentity | undefined;
		for (const lookup of lookups) {
			if (lookup.kind === "none") continue;
			if (lookup.kind === "invalid") throw new RecoveryRequiredError("existing", checkout.name, checkout.path, lookup.reason);
			const { receipt } = lookup;
			if (receipt.state !== "ready") {
				throw new RecoveryRequiredError("existing", checkout.name, checkout.path, `its native add attempt is ${receipt.state}`);
			}
			if (receipt.destination !== checkout.path) {
				throw new RecoveryRequiredError("existing", checkout.name, checkout.path, `its receipt points to ${receipt.destination}`);
			}
			current ??= await fsIdentity(checkout.path, repo.backend);
			if (!sameIdentity(receipt.identity, current)) {
				throw new RecoveryRequiredError("existing", checkout.name, checkout.path, "the checkout directory or marker was replaced after creation");
			}
		}
		return checkout;
	}

	async function createNew(
		repo: Repository,
		name: string,
		destination: string,
		baseIds: readonly string[],
		signal: AbortSignal,
	): Promise<PreparedCheckout> {
		const parent = path.dirname(destination);
		await fsp.mkdir(parent, { recursive: true, mode: 0o700 });
		if ((await fsp.realpath(parent)) !== parent) throw new Error(`checkout parent ${parent} changed (symlink?); plan again`);
		if (await lstatOrUndefined(destination)) throw new Error(`checkout destination ${destination} appeared; it is never overwritten`);

		const recordPath = receiptPath(repo, name);
		const pending: Receipt = {
			version: 1,
			backend: repo.backend,
			repositoryKey: repo.key,
			name,
			destination,
			baseIds: [...baseIds],
			attempt: randomUUID(),
			state: "pending",
		};
		// The destination index is durable before the Pending receipt, so every receipt stays reachable by path.
		await state.writeRecord(destinationIndexPath(repo, destination), {
			version: 1,
			backend: repo.backend,
			repositoryKey: repo.key,
			destination,
			name,
		} satisfies DestinationIndex);
		await state.writeRecord(recordPath, pending);

		const markFailed = async (): Promise<string> => {
			try {
				await state.writeRecord(recordPath, { ...pending, state: "failed" });
				return "; the attempt is recorded as Failed";
			} catch (error) {
				return `; the Failed receipt could not be saved (${message(error)}), so the Pending receipt remains`;
			}
		};
		const recovery = `select it as Existing and acknowledge recovery after inspecting ${destination}`;

		const [file, args]: [string, string[]] =
			repo.backend === "git"
				? ["git", ["-C", repo.root, "worktree", "add", "-b", name, "--", destination, baseIds[0] ?? ""]]
				: [
						"jj",
						[
							"--no-pager",
							"--color=never",
							"-R",
							repo.root,
							"workspace",
							"add",
							"--no-colocate",
							"--name",
							name,
							...baseIds.flatMap(id => ["-r", id]),
							"--",
							destination,
						],
					];
		let result: RunResult;
		try {
			result = await run(file, args, { cwd: repo.root, signal, timeoutMs: NATIVE_ADD_TIMEOUT_MS });
		} catch (error) {
			throw new Error(`${file} could not start (${message(error)})${await markFailed()}; no native add will be retried — ${recovery}`);
		}
		if (result.code !== 0 || result.killed) {
			const outcome = signal.aborted ? "was cancelled" : result.killed ? "timed out or was killed" : `failed (exit ${result.code})`;
			throw new Error(
				`native ${repo.backend === "git" ? "git worktree add" : "jj workspace add"} ${outcome}: ${summarize(result)}; ` +
					`any partial checkout at ${destination} is kept${await markFailed()}; no native add will be retried — ${recovery}`,
			);
		}

		let checkout: ResolvedCheckout;
		let identity: FsIdentity;
		try {
			checkout = await validateCheckoutAt(repo, destination, { name, detached: false }, signal);
			const parents =
				repo.backend === "git"
					? [checkout.head ?? ""]
					: (await jj(destination, ["log", "--no-graph", "--revision=@", "-T", JJ_PARENTS_TEMPLATE], signal))
							.split("\n")
							.filter(line => line !== "");
			if (!sameIds(parents, baseIds)) throw new Error(`new checkout is based on ${parents.join(", ")}, not ${baseIds.join(", ")}`);
			identity = await fsIdentity(destination, repo.backend);
		} catch (error) {
			throw new Error(
				`native add exited 0 but verification failed: ${message(error)}; the checkout is kept and its Pending receipt requires manual inspection — ${recovery}`,
			);
		}
		try {
			await state.writeRecord(recordPath, { ...pending, state: "ready", identity });
		} catch (error) {
			throw new Error(`checkout created at ${destination} but its Ready receipt could not be saved (${message(error)}); ${recovery}`);
		}
		return { repository: { ...repo }, checkout, created: true };
	}

	return {
		async resolveRepository(input: string, prefer: Backend, signal: AbortSignal): Promise<Repository> {
			if (!path.isAbsolute(input)) throw new Error(`repository path must be absolute: ${input}`);
			const real = await realpathOrUndefined(input);
			if (!real) throw new Error(`repository path does not exist: ${input}`);
			// The nearest marker wins, even if broken: never climb to an ancestor repository.
			let root = real;
			for (;;) {
				const hasGit = (await lstatOrUndefined(path.join(root, ".git"))) !== undefined;
				const hasJj = (await lstatOrUndefined(path.join(root, ".jj"))) !== undefined;
				if (hasGit || hasJj) return identify(root, hasGit && hasJj ? prefer : hasJj ? "jj" : "git", signal);
				const parent = path.dirname(root);
				if (parent === root) throw new Error(`no Git or jj checkout contains ${input}`);
				root = parent;
			}
		},

		async listCheckouts(repo: Repository, signal: AbortSignal): Promise<Checkout[]> {
			if (repo.backend === "jj") {
				const checkouts: Checkout[] = [];
				for (const name of await jjNames(repo.root, signal)) {
					const recorded = await jjRoot(repo.root, name, signal);
					checkouts.push({
						repositoryKey: repo.key,
						name,
						...(recorded.path !== undefined ? { path: recorded.path } : {}),
						detached: false,
						source: recorded.path === repo.root,
						locked: false,
						prunable: false,
						...(recorded.reason !== undefined ? { unavailableReason: recorded.reason } : {}),
					});
				}
				return checkouts;
			}
			const checkouts: Checkout[] = [];
			for (const [index, worktree] of (await gitWorktrees(repo, signal)).entries()) {
				const real = await realpathOrUndefined(worktree.path);
				const unavailableReason = worktree.bare
					? "bare repository has no working tree"
					: worktree.prunable
						? `prunable: ${worktree.prunableReason ?? "working tree is missing"}`
						: real === undefined
							? "working tree path is missing"
							: undefined;
				checkouts.push({
					repositoryKey: repo.key,
					name: worktree.branch ?? path.basename(worktree.path),
					path: real ?? worktree.path,
					...(worktree.head !== undefined ? { head: worktree.head } : {}),
					detached: worktree.detached,
					source: index === 0 && !worktree.bare,
					locked: worktree.locked,
					prunable: worktree.prunable,
					...(unavailableReason !== undefined ? { unavailableReason } : {}),
				});
			}
			return checkouts;
		},

		async planCheckout(repo: Repository, selection: CheckoutSelection, signal: AbortSignal): Promise<CheckoutPlan> {
			await verifyRepository(repo, signal);
			if (selection.kind === "existing") {
				const checkout = await validateExisting(repo, selection, signal);
				return freezePlan({
					repository: repo,
					selection: { kind: "existing", checkout, recoveryAcknowledged: selection.recoveryAcknowledged },
					destination: checkout.path,
					baseIds: [],
					identity: await fsIdentity(checkout.path, repo.backend),
				});
			}
			const { name } = selection;
			await validateName(repo, name, signal);
			const destination = await destinationFor(repo, name);
			const baseIds = await resolveBaseIds(repo, selection.base, signal);
			if (!(await reuseReady(repo, name, destination, baseIds, signal))) {
				await assertNewTarget(repo, name, destination, signal);
			}
			return freezePlan({
				repository: repo,
				selection: { kind: "new", name, ...(selection.base !== undefined ? { base: selection.base } : {}) },
				destination,
				baseIds,
			});
		},

		async prepareCheckout(plan: CheckoutPlan, signal: AbortSignal): Promise<PreparedCheckout> {
			const repo: Repository = { ...plan.repository };
			const selection = plan.selection;
			const name = selection.kind === "new" ? selection.name : selection.checkout.name;
			return state.withLocks(
				[`destination:${plan.destination}`, `checkout:${JSON.stringify([repo.key, name])}`],
				async () => {
					signal.throwIfAborted();
					await verifyRepository(repo, signal);
					if (selection.kind === "existing") {
						if (!plan.identity) throw new Error(`checkout plan for '${name}' has no filesystem identity; select it again`);
						const checkout = await validateExisting(repo, selection, signal);
						if (checkout.path !== plan.destination) {
							throw new Error(`checkout '${name}' now resolves to ${checkout.path}, not ${plan.destination}; select it again`);
						}
						if (!sameIdentity(plan.identity, await fsIdentity(checkout.path, repo.backend))) {
							throw new Error(
								`checkout '${name}' at ${checkout.path} was replaced since it was selected (directory or ${repo.backend === "git" ? ".git" : ".jj"} marker changed); select it again`,
							);
						}
						return { repository: repo, checkout, created: false };
					}
					await validateName(repo, name, signal);
					const destination = await destinationFor(repo, name);
					if (destination !== plan.destination) {
						throw new Error(`checkout destination changed from ${plan.destination} to ${destination}; plan again`);
					}
					if (plan.baseIds.length === 0 || (repo.backend === "git" && plan.baseIds.length !== 1)) {
						throw new Error("checkout plan has no valid base commit ids");
					}
					const reused = await reuseReady(repo, name, destination, plan.baseIds, signal);
					if (reused) return { repository: repo, checkout: reused, created: false };
					await assertNewTarget(repo, name, destination, signal);
					return createNew(repo, name, destination, plan.baseIds, signal);
				},
			);
		},
	};
}
