import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import * as fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createStateStore, StateLockBusyError } from "../src/state.ts";

let temp: string;
let root: string;

beforeEach(async () => {
	temp = await fsp.realpath(await fsp.mkdtemp(path.join(os.tmpdir(), "ojw-state-")));
	root = path.join(temp, "state");
});

afterEach(async () => {
	await fsp.rm(temp, { recursive: true, force: true });
});

const asObject = (value: unknown) => value as Record<string, unknown>;

describe("state records", () => {
	test("round-trips a record in private directories and files", async () => {
		const store = createStateStore(root);
		expect(await store.readRecord("receipts/a.json", asObject)).toBeUndefined();
		await store.writeRecord("receipts/a.json", { state: "pending" });
		await store.writeRecord("receipts/a.json", { state: "ready" });
		expect(await store.readRecord("receipts/a.json", asObject)).toEqual({ state: "ready" });
		expect((await fsp.stat(root)).mode & 0o777).toBe(0o700);
		expect((await fsp.stat(path.join(root, "receipts"))).mode & 0o777).toBe(0o700);
		expect((await fsp.stat(path.join(root, "receipts/a.json"))).mode & 0o777).toBe(0o600);
		expect((await fsp.readdir(path.join(root, "receipts"))).sort()).toEqual(["a.json"]);
	});

	test("rejects traversal and absolute relative paths", async () => {
		const store = createStateStore(root);
		for (const bad of ["../x.json", "/etc/passwd", "a/../b.json", "", "a//b"]) {
			await expect(store.writeRecord(bad, {})).rejects.toThrow("invalid state path");
			await expect(store.readRecord(bad, asObject)).rejects.toThrow("invalid state path");
		}
	});

	test("refuses a record with group/other permission bits without overwriting it", async () => {
		const store = createStateStore(root);
		await store.writeRecord("r.json", { v: 1 });
		const file = path.join(root, "r.json");
		await fsp.chmod(file, 0o644);
		await expect(store.readRecord("r.json", asObject)).rejects.toThrow("group/other permissions");
		await expect(store.writeRecord("r.json", { v: 2 })).rejects.toThrow("group/other permissions");
		expect(await fsp.readFile(file, "utf8")).toBe('{"v":1}\n');
	});

	test("refuses a permissive state directory", async () => {
		const store = createStateStore(root);
		await store.writeRecord("r.json", { v: 1 });
		await fsp.chmod(root, 0o755);
		await expect(store.readRecord("r.json", asObject)).rejects.toThrow("group/other permissions");
		await expect(store.writeRecord("r.json", { v: 2 })).rejects.toThrow("group/other permissions");
	});

	test("refuses a symlinked record without touching its target", async () => {
		const store = createStateStore(root);
		await store.writeRecord("seed.json", {});
		const target = path.join(temp, "elsewhere.json");
		await fsp.writeFile(target, '{"outside":true}', { mode: 0o600 });
		await fsp.symlink(target, path.join(root, "r.json"));
		await expect(store.readRecord("r.json", asObject)).rejects.toThrow("symlink");
		await expect(store.writeRecord("r.json", { v: 2 })).rejects.toThrow("symlink");
		expect(await fsp.readFile(target, "utf8")).toBe('{"outside":true}');
	});

	test("reports a corrupt record and never overwrites it", async () => {
		const store = createStateStore(root);
		await store.writeRecord("r.json", { v: 1 });
		const file = path.join(root, "r.json");
		await fsp.writeFile(file, "{not json");
		await expect(store.readRecord("r.json", asObject)).rejects.toThrow("corrupt state record");
		await expect(store.writeRecord("r.json", { v: 2 })).rejects.toThrow("corrupt state record");
		expect(await fsp.readFile(file, "utf8")).toBe("{not json");
	});

	test("surfaces parser rejection as an invalid record", async () => {
		const store = createStateStore(root);
		await store.writeRecord("r.json", { v: 1 });
		await expect(
			store.readRecord("r.json", () => {
				throw new Error("wrong shape");
			}),
		).rejects.toThrow("wrong shape");
	});
});

describe("private files", () => {
	test("creates exclusively with 0600 under 0700 parents and never overwrites", async () => {
		const store = createStateStore(root);
		const written = await store.writePrivateFile("handoffs/x.md", "hello");
		expect(written).toBe(path.join(root, "handoffs/x.md"));
		expect(await fsp.readFile(written, "utf8")).toBe("hello");
		expect((await fsp.stat(written)).mode & 0o777).toBe(0o600);
		expect((await fsp.stat(path.dirname(written))).mode & 0o777).toBe(0o700);
		await expect(store.writePrivateFile("handoffs/x.md", "other")).rejects.toThrow("already exists");
		expect(await fsp.readFile(written, "utf8")).toBe("hello");
	});
});

describe("locks", () => {
	test("a concurrent holder of the same key gets a busy error naming the lock", async () => {
		const first = createStateStore(root);
		const second = createStateStore(root);
		const gate = Promise.withResolvers<void>();
		const entered = Promise.withResolvers<void>();
		const holding = first.withLocks(["dest:/a", "name:x"], async () => {
			entered.resolve();
			await gate.promise;
			return "first";
		});
		await entered.promise;
		let ran = false;
		const busy = await second
			.withLocks(["name:x"], async () => {
				ran = true;
			})
			.catch((error: unknown) => error);
		expect(busy).toBeInstanceOf(StateLockBusyError);
		const lockPath = (busy as StateLockBusyError).lockPath;
		expect((busy as Error).message).toContain(lockPath);
		expect((busy as Error).message).toContain("manually");
		expect(ran).toBe(false);
		gate.resolve();
		expect(await holding).toBe("first");
		expect(await second.withLocks(["name:x"], async () => "second")).toBe("second");
		expect(await fsp.readdir(path.join(root, "locks"))).toEqual([]);
	});

	test("partial acquisition releases only the locks it took", async () => {
		const store = createStateStore(root);
		const gate = Promise.withResolvers<void>();
		const entered = Promise.withResolvers<void>();
		const holding = store.withLocks(["b"], async () => {
			entered.resolve();
			await gate.promise;
		});
		await entered.promise;
		const heldBefore = (await fsp.readdir(path.join(root, "locks"))).sort();
		await expect(store.withLocks(["a", "b", "c"], async () => undefined)).rejects.toBeInstanceOf(StateLockBusyError);
		expect((await fsp.readdir(path.join(root, "locks"))).sort()).toEqual(heldBefore);
		gate.resolve();
		await holding;
		expect(await fsp.readdir(path.join(root, "locks"))).toEqual([]);
	});

	test("a lock left by a crashed process is never stolen", async () => {
		const store = createStateStore(root);
		let lockPath = "";
		await store.withLocks(["k"], async () => {
			lockPath = path.join(root, "locks", (await fsp.readdir(path.join(root, "locks")))[0] ?? "");
			const owner = JSON.parse(await fsp.readFile(path.join(lockPath, "owner"), "utf8")) as Record<string, unknown>;
			expect(owner.pid).toBe(process.pid);
			expect(typeof owner.attempt).toBe("string");
		});
		await fsp.mkdir(lockPath, { mode: 0o700 });
		await fsp.writeFile(path.join(lockPath, "owner"), JSON.stringify({ pid: 999999, attempt: "dead" }), { mode: 0o600 });
		await expect(store.withLocks(["k"], async () => undefined)).rejects.toThrow(lockPath);
		expect(await fsp.readFile(path.join(lockPath, "owner"), "utf8")).toContain("dead");
	});

	test("releases locks when the action fails and propagates the error", async () => {
		const store = createStateStore(root);
		await expect(
			store.withLocks(["k"], async () => {
				throw new Error("boom");
			}),
		).rejects.toThrow("boom");
		expect(await fsp.readdir(path.join(root, "locks"))).toEqual([]);
	});
});
