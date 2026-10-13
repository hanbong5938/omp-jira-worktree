import { createHash, randomUUID } from "node:crypto";
import { constants, type Stats } from "node:fs";
import * as fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import type { StateStore } from "./contracts.ts";

const DIR_MODE = 0o700;
const FILE_MODE = 0o600;
const CREATE_FLAGS = constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW;
const READ_FLAGS = constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK;
const OWNER_FILE = "owner";

/** `${XDG_STATE_HOME || ~/.local/state}/omp-jira-worktree`; a relative XDG_STATE_HOME is ignored per the XDG spec. */
export function stateRoot(): string {
	const xdg = process.env.XDG_STATE_HOME;
	const base = xdg && path.isAbsolute(xdg) ? xdg : path.join(os.homedir(), ".local", "state");
	return path.join(base, "omp-jira-worktree");
}

/** A lock directory already exists; it is never waited on or stolen. */
export class StateLockBusyError extends Error {
	readonly lockPath: string;
	constructor(lockPath: string) {
		super(
			`state lock is busy: ${lockPath}. Another omp-jira-worktree operation holds it, or a crashed process left it behind. ` +
				`If no such operation is running, inspect ${path.join(lockPath, OWNER_FILE)} (pid/attempt) and remove the lock directory manually.`,
		);
		this.name = "StateLockBusyError";
		this.lockPath = lockPath;
	}
}

function errnoCode(error: unknown): string | undefined {
	if (error instanceof Error && "code" in error && typeof error.code === "string") return error.code;
	return undefined;
}

function message(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function assertPrivate(stats: Stats, target: string, kind: "directory" | "file"): void {
	if (stats.isSymbolicLink()) throw new Error(`state ${kind} ${target} is a symlink; refusing to use it (inspect manually)`);
	if (kind === "directory" ? !stats.isDirectory() : !stats.isFile()) {
		throw new Error(`state path ${target} is not a regular ${kind}; inspect manually`);
	}
	if ((stats.mode & 0o077) !== 0) {
		const mode = (stats.mode & 0o777).toString(8).padStart(4, "0");
		const expected = kind === "directory" ? "0700" : "0600";
		throw new Error(`state ${kind} ${target} has group/other permissions (mode ${mode}, expected ${expected}); inspect manually`);
	}
	const uid = process.getuid?.();
	if (uid !== undefined && stats.uid !== uid) {
		throw new Error(`state ${kind} ${target} is owned by uid ${stats.uid}, not the current user; inspect manually`);
	}
}

async function lstatOrUndefined(target: string): Promise<Stats | undefined> {
	try {
		return await fsp.lstat(target);
	} catch (error) {
		if (errnoCode(error) === "ENOENT") return undefined;
		throw error;
	}
}

async function fsyncDirectory(dir: string): Promise<void> {
	const handle = await fsp.open(dir, constants.O_RDONLY);
	try {
		await handle.sync();
	} finally {
		await handle.close();
	}
}

/** Open a private regular file without following symlinks; undefined when absent. */
async function readPrivateFile(target: string): Promise<string | undefined> {
	let handle: fsp.FileHandle;
	try {
		handle = await fsp.open(target, READ_FLAGS);
	} catch (error) {
		const code = errnoCode(error);
		if (code === "ENOENT") return undefined;
		if (code === "ELOOP") throw new Error(`state file ${target} is a symlink; refusing to use it (inspect manually)`);
		throw error;
	}
	try {
		assertPrivate(await handle.stat(), target, "file");
		return await handle.readFile("utf8");
	} finally {
		await handle.close();
	}
}

async function createExclusive(target: string, text: string): Promise<void> {
	let handle: fsp.FileHandle;
	try {
		handle = await fsp.open(target, CREATE_FLAGS, FILE_MODE);
	} catch (error) {
		if (errnoCode(error) === "EEXIST") throw new Error(`state file ${target} already exists; refusing to overwrite it`);
		throw error;
	}
	try {
		await handle.writeFile(text, "utf8");
		await handle.sync();
	} catch (error) {
		await handle.close();
		await fsp.unlink(target);
		throw error;
	}
	await handle.close();
}

export function createStateStore(root: string): StateStore {
	if (!path.isAbsolute(root)) throw new Error(`state root must be an absolute path: ${root}`);
	const base = path.normalize(root);

	function resolve(relativePath: string): { absolute: string; parts: string[] } {
		if (
			relativePath === "" ||
			path.isAbsolute(relativePath) ||
			relativePath.includes("\\") ||
			relativePath.includes("\0")
		) {
			throw new Error(`invalid state path: ${JSON.stringify(relativePath)}`);
		}
		const parts = relativePath.split("/");
		if (parts.some(part => part === "" || part === "." || part === "..")) {
			throw new Error(`invalid state path: ${JSON.stringify(relativePath)}`);
		}
		return { absolute: path.join(base, ...parts), parts };
	}

	/** Create missing private directories (0700) from the root down; validate existing ones. */
	async function ensureDirectories(dirParts: string[]): Promise<string> {
		const rootStats = await lstatOrUndefined(base);
		if (!rootStats) await fsp.mkdir(base, { recursive: true, mode: DIR_MODE });
		assertPrivate(await fsp.lstat(base), base, "directory");
		let current = base;
		for (const part of dirParts) {
			const parent = current;
			current = path.join(current, part);
			if (!(await lstatOrUndefined(current))) {
				try {
					await fsp.mkdir(current, { mode: DIR_MODE });
					await fsyncDirectory(parent);
				} catch (error) {
					if (errnoCode(error) !== "EEXIST") throw error;
				}
			}
			assertPrivate(await fsp.lstat(current), current, "directory");
		}
		return current;
	}

	/** Validate existing directories for reads; false when any component is absent. */
	async function directoriesExist(dirParts: string[]): Promise<boolean> {
		let current = base;
		for (const part of [undefined, ...dirParts]) {
			if (part !== undefined) current = path.join(current, part);
			const stats = await lstatOrUndefined(current);
			if (!stats) return false;
			assertPrivate(stats, current, "directory");
		}
		return true;
	}

	async function releaseLocks(held: string[], attempt: string): Promise<Error | undefined> {
		const failures: string[] = [];
		for (const lockPath of [...held].reverse()) {
			const ownerPath = path.join(lockPath, OWNER_FILE);
			try {
				const text = await readPrivateFile(ownerPath);
				if (text !== undefined) {
					const owner: unknown = JSON.parse(text);
					if (typeof owner !== "object" || owner === null || !("attempt" in owner) || owner.attempt !== attempt) {
						failures.push(`${lockPath} is not owned by this attempt; left in place`);
						continue;
					}
					await fsp.unlink(ownerPath);
				}
				await fsp.rmdir(lockPath);
			} catch (error) {
				failures.push(`${lockPath}: ${message(error)}`);
			}
		}
		if (failures.length === 0) return undefined;
		return new Error(`could not release state locks (inspect and remove manually): ${failures.join("; ")}`);
	}

	return {
		async withLocks<T>(keys: string[], action: () => Promise<T>): Promise<T> {
			const hashes = [...new Set(keys.map(key => createHash("sha256").update(key).digest("hex")))].sort();
			const locksDir = await ensureDirectories(["locks"]);
			const attempt = randomUUID();
			const held: string[] = [];
			let outcome: { ok: true; value: T } | { ok: false; error: unknown };
			try {
				for (const hash of hashes) {
					const lockPath = path.join(locksDir, hash);
					try {
						await fsp.mkdir(lockPath, { mode: DIR_MODE });
					} catch (error) {
						if (errnoCode(error) === "EEXIST") throw new StateLockBusyError(lockPath);
						throw error;
					}
					held.push(lockPath);
					await createExclusive(
						path.join(lockPath, OWNER_FILE),
						`${JSON.stringify({ pid: process.pid, attempt, createdAt: new Date().toISOString() })}\n`,
					);
				}
				outcome = { ok: true, value: await action() };
			} catch (error) {
				outcome = { ok: false, error };
			}
			const releaseError = await releaseLocks(held, attempt);
			if (!outcome.ok) {
				if (releaseError) throw new AggregateError([outcome.error, releaseError], `${message(outcome.error)}; ${releaseError.message}`);
				throw outcome.error;
			}
			if (releaseError) throw releaseError;
			return outcome.value;
		},

		async readRecord<T>(relativePath: string, parse: (value: unknown) => T): Promise<T | undefined> {
			const { absolute, parts } = resolve(relativePath);
			if (!(await directoriesExist(parts.slice(0, -1)))) return undefined;
			const text = await readPrivateFile(absolute);
			if (text === undefined) return undefined;
			let value: unknown;
			try {
				value = JSON.parse(text);
			} catch (error) {
				throw new Error(`corrupt state record ${absolute}: ${message(error)}; it is never overwritten — inspect it manually`);
			}
			try {
				return parse(value);
			} catch (error) {
				throw new Error(`invalid state record ${absolute}: ${message(error)}; it is never overwritten — inspect it manually`);
			}
		},

		async writeRecord(relativePath: string, value: unknown): Promise<void> {
			const { absolute, parts } = resolve(relativePath);
			const text = JSON.stringify(value);
			if (text === undefined) throw new Error(`state record ${absolute} has no JSON representation`);
			const dir = await ensureDirectories(parts.slice(0, -1));
			const existing = await readPrivateFile(absolute);
			if (existing !== undefined) {
				try {
					JSON.parse(existing);
				} catch (error) {
					throw new Error(`corrupt state record ${absolute}: ${message(error)}; refusing to overwrite it — inspect it manually`);
				}
			}
			const temp = path.join(dir, `.${path.basename(absolute)}.${randomUUID()}.tmp`);
			await createExclusive(temp, `${text}\n`);
			try {
				await fsp.rename(temp, absolute);
			} catch (error) {
				await fsp.unlink(temp);
				throw error;
			}
			await fsyncDirectory(dir);
		},

		async writePrivateFile(relativePath: string, text: string): Promise<string> {
			const { absolute, parts } = resolve(relativePath);
			const dir = await ensureDirectories(parts.slice(0, -1));
			await createExclusive(absolute, text);
			await fsyncDirectory(dir);
			return absolute;
		},
	};
}
