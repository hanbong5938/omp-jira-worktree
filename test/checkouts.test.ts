import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import * as fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createCheckoutService, RecoveryRequiredError } from "../src/checkouts.ts";
import type { Checkout, CheckoutPort, Config, Repository, Run } from "../src/contracts.ts";
import { run } from "../src/process.ts";
import { createStateStore, StateLockBusyError } from "../src/state.ts";

const TIMEOUT = 60_000;
const signal = new AbortController().signal;

const ENV_KEYS = [
	"GIT_AUTHOR_NAME",
	"GIT_AUTHOR_EMAIL",
	"GIT_COMMITTER_NAME",
	"GIT_COMMITTER_EMAIL",
	"GIT_CONFIG_GLOBAL",
	"GIT_CONFIG_NOSYSTEM",
	"JJ_CONFIG",
	"JJ_USER",
	"JJ_EMAIL",
] as const;
const savedEnv: Partial<Record<(typeof ENV_KEYS)[number], string>> = {};
let envDir: string;

beforeAll(async () => {
	envDir = await fsp.realpath(await fsp.mkdtemp(path.join(os.tmpdir(), "ojw-env-")));
	await fsp.writeFile(path.join(envDir, "gitconfig"), "");
	await fsp.writeFile(path.join(envDir, "jj.toml"), 'user.name = "Test"\nuser.email = "test@example.invalid"\n');
	for (const key of ENV_KEYS) savedEnv[key] = process.env[key];
	Object.assign(process.env, {
		GIT_AUTHOR_NAME: "Test",
		GIT_AUTHOR_EMAIL: "test@example.invalid",
		GIT_COMMITTER_NAME: "Test",
		GIT_COMMITTER_EMAIL: "test@example.invalid",
		GIT_CONFIG_GLOBAL: path.join(envDir, "gitconfig"),
		GIT_CONFIG_NOSYSTEM: "1",
		JJ_CONFIG: path.join(envDir, "jj.toml"),
		JJ_USER: "Test",
		JJ_EMAIL: "test@example.invalid",
	});
});

afterAll(async () => {
	for (const key of ENV_KEYS) {
		const value = savedEnv[key];
		if (value === undefined) delete process.env[key];
		else process.env[key] = value;
	}
	await fsp.rm(envDir, { recursive: true, force: true });
});

let temp: string;
let stateDir: string;
let worktreeRoot: string;

beforeEach(async () => {
	temp = await fsp.realpath(await fsp.mkdtemp(path.join(os.tmpdir(), "ojw-checkout-")));
	stateDir = path.join(temp, "state");
	worktreeRoot = path.join(temp, "checkouts");
});

afterEach(async () => {
	const receiptsDir = path.join(stateDir, "receipts");
	await fsp.chmod(receiptsDir, 0o700).catch(() => undefined);
	await fsp.rm(temp, { recursive: true, force: true });
});

function sh(cmd: string[], cwd: string): string {
	const result = Bun.spawnSync({ cmd, cwd, env: process.env, stdout: "pipe", stderr: "pipe" });
	if (result.exitCode !== 0) throw new Error(`${cmd.join(" ")} failed: ${result.stderr.toString()}`);
	return result.stdout.toString().trim();
}

function makeConfig(): Config {
	return {
		jira: {
			baseUrl: "https://example.invalid",
			deployment: "cloud",
			auth: "bearer",
			email: null,
			tokenCommand: ["false"],
			maxResults: 50,
			timeoutMs: 20_000,
		},
		filters: [],
		search: { jql: 'text ~ "{query}"' },
		worktree: { repos: {}, prefer: "jj", root: worktreeRoot, gitBase: null, jjBase: null },
		handoff: { maxComments: 50, maxDescriptionChars: 6000, maxCommentChars: 12000, maxChars: 64000 },
		omp: { binary: "omp", profile: "default" },
	};
}

/** Real `run`, counting native add invocations and optionally replacing them. */
function countingRun(intercept?: Run): { run: Run; adds: () => number } {
	let adds = 0;
	const wrapped: Run = (file, args, options) => {
		const joined = args.join(" ");
		if (joined.includes("worktree add") || joined.includes("workspace add")) {
			adds++;
			if (intercept) return intercept(file, args, options);
		}
		return run(file, args, options);
	};
	return { run: wrapped, adds: () => adds };
}

function service(runner: Run): CheckoutPort {
	return createCheckoutService(makeConfig(), runner, createStateStore(stateDir));
}

async function exists(target: string): Promise<boolean> {
	return fsp.lstat(target).then(
		() => true,
		() => false,
	);
}

async function receipts(): Promise<Record<string, unknown>[]> {
	const dir = path.join(stateDir, "receipts");
	if (!(await exists(dir))) return [];
	const names = (await fsp.readdir(dir)).filter(name => name.endsWith(".json"));
	return Promise.all(
		names.map(async name => JSON.parse(await fsp.readFile(path.join(dir, name), "utf8")) as Record<string, unknown>),
	);
}

async function fakeScript(name: string, body: string): Promise<string> {
	const file = path.join(temp, name);
	await fsp.writeFile(file, `#!/bin/sh\n${body}\n`, { mode: 0o755 });
	return file;
}

type GitFixture = { root: string; first: string; second: string; dirty: Record<string, string> };

async function gitFixture(name = "source"): Promise<GitFixture> {
	const root = path.join(temp, name);
	await fsp.mkdir(root);
	sh(["git", "init", "-q", "-b", "main"], root);
	await fsp.writeFile(path.join(root, "README"), "one\n");
	sh(["git", "add", "README"], root);
	sh(["git", "commit", "-q", "-m", "one"], root);
	const first = sh(["git", "rev-parse", "HEAD"], root);
	await fsp.writeFile(path.join(root, "README"), "two\n");
	sh(["git", "commit", "-q", "-am", "two"], root);
	const second = sh(["git", "rev-parse", "HEAD"], root);
	const dirty = { README: "dirty tracked edit\n", "scratch.txt": "untracked bytes\n" };
	for (const [file, text] of Object.entries(dirty)) await fsp.writeFile(path.join(root, file), text);
	return { root, first, second, dirty };
}

async function expectDirtyPreserved(root: string, dirty: Record<string, string>): Promise<void> {
	for (const [file, text] of Object.entries(dirty)) expect(await fsp.readFile(path.join(root, file), "utf8")).toBe(text);
}

describe("git worktrees", () => {
	test(
		"new creates a branch at the chosen base in a separate worktree, then reuses it",
		async () => {
			const fixture = await gitFixture();
			const counting = countingRun();
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "jj", signal);
			expect(repo).toEqual({ backend: "git", key: `git:${fixture.root}/.git`, root: fixture.root, name: "source" });

			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-1", base: fixture.first }, signal);
			expect(plan.baseIds).toEqual([fixture.first]);
			expect(plan.destination.startsWith(`${worktreeRoot}/source-`)).toBe(true);
			expect(path.basename(plan.destination).startsWith("TEST-1-")).toBe(true);
			// Planning is read-only.
			expect(await exists(worktreeRoot)).toBe(false);
			expect(await exists(stateDir)).toBe(false);

			const prepared = await checkouts.prepareCheckout(plan, signal);
			expect(prepared.created).toBe(true);
			expect(prepared.checkout.path).toBe(plan.destination);
			expect(sh(["git", "rev-parse", "HEAD"], plan.destination)).toBe(fixture.first);
			expect(sh(["git", "symbolic-ref", "--short", "HEAD"], plan.destination)).toBe("TEST-1");
			expect(await fsp.readFile(path.join(plan.destination, "README"), "utf8")).toBe("one\n");
			expect(await exists(path.join(plan.destination, "scratch.txt"))).toBe(false);
			await expectDirtyPreserved(fixture.root, fixture.dirty);
			expect((await receipts()).map(receipt => receipt.state)).toEqual(["ready"]);

			const again = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-1", base: fixture.first }, signal);
			const reused = await checkouts.prepareCheckout(again, signal);
			expect(reused.created).toBe(false);
			expect(reused.checkout.path).toBe(plan.destination);
			expect(counting.adds()).toBe(1);

			const listed = (await checkouts.listCheckouts(repo, signal)).find(checkout => checkout.name === "TEST-1");
			expect(listed?.path).toBe(plan.destination);
			expect(listed?.source).toBe(false);
			if (!listed) throw new Error("TEST-1 not listed");
			const existingPlan = await checkouts.planCheckout(
				repo,
				{ kind: "existing", checkout: listed, recoveryAcknowledged: false },
				signal,
			);
			expect(existingPlan.baseIds).toEqual([]);
			const existing = await checkouts.prepareCheckout(existingPlan, signal);
			expect(existing.checkout.path).toBe(plan.destination);
			expect(existing.created).toBe(false);
			expect(counting.adds()).toBe(1);

			// A Ready worktree is found by destination even after its HEAD is detached.
			sh(["git", "checkout", "-q", "--detach"], plan.destination);
			const detached = (await checkouts.listCheckouts(repo, signal)).find(checkout => checkout.path === plan.destination);
			if (!detached?.detached) throw new Error("detached TEST-1 not listed");
			const reopened = await checkouts.prepareCheckout(
				await checkouts.planCheckout(repo, { kind: "existing", checkout: detached, recoveryAcknowledged: false }, signal),
				signal,
			);
			expect(reopened.checkout.detached).toBe(true);
		},
		TIMEOUT,
	);

	test(
		"default base is the source HEAD commit",
		async () => {
			const fixture = await gitFixture();
			const checkouts = service(run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);
			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-5" }, signal);
			expect(plan.baseIds).toEqual([fixture.second]);
		},
		TIMEOUT,
	);

	test(
		"detached existing worktree is allowed after path+HEAD validation",
		async () => {
			const fixture = await gitFixture();
			const detachedPath = path.join(temp, "detached");
			sh(["git", "worktree", "add", "-q", "--detach", detachedPath, fixture.first], fixture.root);
			const checkouts = service(run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);
			const listed = (await checkouts.listCheckouts(repo, signal)).find(checkout => checkout.detached);
			expect(listed?.path).toBe(detachedPath);
			expect(listed?.head).toBe(fixture.first);
			if (!listed) throw new Error("detached worktree not listed");
			const plan = await checkouts.planCheckout(repo, { kind: "existing", checkout: listed, recoveryAcknowledged: false }, signal);
			const prepared = await checkouts.prepareCheckout(plan, signal);
			expect(prepared.checkout.path).toBe(detachedPath);
			expect(prepared.checkout.detached).toBe(true);
		},
		TIMEOUT,
	);

	test(
		"an external worktree replaced at the same path after planning is rejected",
		async () => {
			const fixture = await gitFixture();
			const external = path.join(temp, "external");
			sh(["git", "worktree", "add", "-q", "-b", "EXT-1", external, fixture.first], fixture.root);
			const checkouts = service(run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);
			const listed = (await checkouts.listCheckouts(repo, signal)).find(checkout => checkout.name === "EXT-1");
			if (!listed) throw new Error("EXT-1 not listed");
			const plan = await checkouts.planCheckout(repo, { kind: "existing", checkout: listed, recoveryAcknowledged: false }, signal);
			expect(plan.identity).toBeDefined();

			// Move the original away (keeping its inodes alive) and add another worktree of EXT-1 at the same path.
			sh(["git", "worktree", "move", external, `${external}.old`], fixture.root);
			sh(["git", "worktree", "add", "-q", "-f", external, "EXT-1"], fixture.root);
			await expect(checkouts.prepareCheckout(plan, signal)).rejects.toThrow(/was replaced since it was selected.*select it again/);

			const fresh = await checkouts.planCheckout(repo, { kind: "existing", checkout: listed, recoveryAcknowledged: false }, signal);
			expect((await checkouts.prepareCheckout(fresh, signal)).checkout.path).toBe(external);
		},
		TIMEOUT,
	);

	test(
		"branch conflicts and wrong common dir are errors that change nothing",
		async () => {
			const fixture = await gitFixture();
			const counting = countingRun();
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);

			await expect(checkouts.planCheckout(repo, { kind: "new", name: "main" }, signal)).rejects.toThrow(
				/already checked out.*Existing/,
			);
			sh(["git", "branch", "TEST-3", fixture.first], fixture.root);
			await expect(checkouts.planCheckout(repo, { kind: "new", name: "TEST-3" }, signal)).rejects.toThrow(
				/already exists but is not checked out/,
			);
			expect(sh(["git", "rev-parse", "TEST-3"], fixture.root)).toBe(fixture.first);
			for (const bad of ["-x", "a..b", "@{-1}", "bad\u0007name"]) {
				await expect(checkouts.planCheckout(repo, { kind: "new", name: bad }, signal)).rejects.toThrow(/invalid/);
			}

			const other = await gitFixture("other");
			await expect(
				checkouts.planCheckout(
					repo,
					{
						kind: "existing",
						checkout: {
							repositoryKey: repo.key,
							name: "main",
							path: other.root,
							detached: false,
							source: false,
							locked: false,
							prunable: false,
						},
						recoveryAcknowledged: true,
					},
					signal,
				),
			).rejects.toThrow(/belongs to git:/);
			await expectDirtyPreserved(other.root, other.dirty);
			await expectDirtyPreserved(fixture.root, fixture.dirty);
			expect(await exists(worktreeRoot)).toBe(false);
			expect(counting.adds()).toBe(0);
		},
		TIMEOUT,
	);

	test(
		"a replaced checkout marker blocks reuse and requires acknowledged recovery",
		async () => {
			const fixture = await gitFixture();
			const counting = countingRun();
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);
			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-1" }, signal);
			await checkouts.prepareCheckout(plan, signal);

			const marker = path.join(plan.destination, ".git");
			const content = await fsp.readFile(marker, "utf8");
			await fsp.rm(marker);
			await fsp.writeFile(marker, content);

			await expect(checkouts.planCheckout(repo, { kind: "new", name: "TEST-1" }, signal)).rejects.toBeInstanceOf(
				RecoveryRequiredError,
			);
			const listed = (await checkouts.listCheckouts(repo, signal)).find(checkout => checkout.name === "TEST-1");
			if (!listed) throw new Error("TEST-1 not listed");
			await expect(
				checkouts.planCheckout(repo, { kind: "existing", checkout: listed, recoveryAcknowledged: false }, signal),
			).rejects.toBeInstanceOf(RecoveryRequiredError);
			const recovered = await checkouts.prepareCheckout(
				await checkouts.planCheckout(repo, { kind: "existing", checkout: listed, recoveryAcknowledged: true }, signal),
				signal,
			);
			expect(recovered.checkout.path).toBe(plan.destination);
			expect(await fsp.readFile(marker, "utf8")).toBe(content);
			expect(counting.adds()).toBe(1);
		},
		TIMEOUT,
	);

	test(
		"unborn HEAD and broken nearer markers are clear errors",
		async () => {
			const unborn = path.join(temp, "unborn");
			await fsp.mkdir(unborn);
			sh(["git", "init", "-q", "-b", "main"], unborn);
			const checkouts = service(run);
			const repo = await checkouts.resolveRepository(unborn, "git", signal);
			await expect(checkouts.planCheckout(repo, { kind: "new", name: "TEST-1" }, signal)).rejects.toThrow(/unborn/);

			const fixture = await gitFixture();
			const nested = path.join(fixture.root, "nested");
			await fsp.mkdir(path.join(nested, ".jj"), { recursive: true });
			await expect(checkouts.resolveRepository(nested, "jj", signal)).rejects.toThrow(/broken jj workspace marker/);
			const inner = path.join(fixture.root, "plain");
			await fsp.mkdir(inner);
			expect((await checkouts.resolveRepository(inner, "jj", signal)).root).toBe(fixture.root);
		},
		TIMEOUT,
	);
});

describe("native add failures", () => {
	test(
		"a failing native add leaves a Failed receipt, is never retried, and opens only via acknowledged Existing",
		async () => {
			const fixture = await gitFixture();
			const failAfter = await fakeScript("fail-after.sh", '"$@" >/dev/null 2>&1\necho "simulated crash" >&2\nexit 1');
			const counting = countingRun((file, args, options) => run(failAfter, [file, ...args], options));
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);
			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-1" }, signal);
			await expect(checkouts.prepareCheckout(plan, signal)).rejects.toThrow(/failed \(exit 1\).*kept.*Failed/);
			expect((await receipts()).map(receipt => receipt.state)).toEqual(["failed"]);
			expect(await exists(plan.destination)).toBe(true);

			await expect(checkouts.planCheckout(repo, { kind: "new", name: "TEST-1" }, signal)).rejects.toBeInstanceOf(
				RecoveryRequiredError,
			);
			await expect(checkouts.prepareCheckout(plan, signal)).rejects.toBeInstanceOf(RecoveryRequiredError);
			expect(counting.adds()).toBe(1);

			const listed = (await checkouts.listCheckouts(repo, signal)).find(checkout => checkout.name === "TEST-1");
			if (!listed) throw new Error("TEST-1 not listed");
			const blocked = await checkouts
				.planCheckout(repo, { kind: "existing", checkout: listed, recoveryAcknowledged: false }, signal)
				.catch((error: unknown) => error);
			expect(blocked).toBeInstanceOf(RecoveryRequiredError);
			expect((blocked as RecoveryRequiredError).selection).toBe("existing");
			const recovered = await checkouts.prepareCheckout(
				await checkouts.planCheckout(repo, { kind: "existing", checkout: listed, recoveryAcknowledged: true }, signal),
				signal,
			);
			expect(recovered.created).toBe(false);
			expect(recovered.checkout.path).toBe(plan.destination);
			expect((await receipts()).map(receipt => receipt.state)).toEqual(["failed"]);
			expect(counting.adds()).toBe(1);
		},
		TIMEOUT,
	);

	test(
		"a Failed worktree stays blocked after detaching HEAD or switching branch",
		async () => {
			const fixture = await gitFixture();
			const failAfter = await fakeScript("fail-after.sh", '"$@" >/dev/null 2>&1\necho "simulated crash" >&2\nexit 1');
			const counting = countingRun((file, args, options) => run(failAfter, [file, ...args], options));
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);
			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-1" }, signal);
			await expect(checkouts.prepareCheckout(plan, signal)).rejects.toThrow(/failed \(exit 1\)/);
			const listedAt = async (): Promise<Checkout> => {
				const listed = (await checkouts.listCheckouts(repo, signal)).find(checkout => checkout.path === plan.destination);
				if (!listed) throw new Error("failed worktree not listed");
				return listed;
			};

			sh(["git", "checkout", "-q", "--detach"], plan.destination);
			const detached = await listedAt();
			expect(detached.detached).toBe(true);
			await expect(
				checkouts.planCheckout(repo, { kind: "existing", checkout: detached, recoveryAcknowledged: false }, signal),
			).rejects.toBeInstanceOf(RecoveryRequiredError);
			const recovered = await checkouts.prepareCheckout(
				await checkouts.planCheckout(repo, { kind: "existing", checkout: detached, recoveryAcknowledged: true }, signal),
				signal,
			);
			expect(recovered.checkout.path).toBe(plan.destination);
			expect(recovered.checkout.detached).toBe(true);

			sh(["git", "switch", "-q", "-c", "TEST-1-other"], plan.destination);
			const switched = await listedAt();
			expect(switched.name).toBe("TEST-1-other");
			await expect(
				checkouts.planCheckout(repo, { kind: "existing", checkout: switched, recoveryAcknowledged: false }, signal),
			).rejects.toBeInstanceOf(RecoveryRequiredError);
			expect((await receipts()).map(receipt => receipt.state)).toEqual(["failed"]);
			expect(counting.adds()).toBe(1);
		},
		TIMEOUT,
	);

	test(
		"a timed-out native add is recorded as Failed and not retried",
		async () => {
			const fixture = await gitFixture();
			const hang = await fakeScript("hang.sh", '"$@" >/dev/null 2>&1\nexec sleep 30');
			const counting = countingRun((file, args, options) => run(hang, [file, ...args], { ...options, timeoutMs: 1000 }));
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);
			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-1" }, signal);
			await expect(checkouts.prepareCheckout(plan, signal)).rejects.toThrow(/timed out/);
			expect((await receipts()).map(receipt => receipt.state)).toEqual(["failed"]);
			await expect(checkouts.planCheckout(repo, { kind: "new", name: "TEST-1" }, signal)).rejects.toBeInstanceOf(
				RecoveryRequiredError,
			);
			expect(counting.adds()).toBe(1);
		},
		TIMEOUT,
	);

	test(
		"when the Failed receipt cannot be saved the Pending receipt remains and still blocks retry",
		async () => {
			const fixture = await gitFixture();
			const receiptsDir = path.join(stateDir, "receipts");
			const counting = countingRun(async (file, args, options) => {
				await run(file, args, options);
				await fsp.chmod(receiptsDir, 0o500);
				return { code: 1, stdout: "", stderr: "simulated exit", killed: false };
			});
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);
			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-1" }, signal);
			await expect(checkouts.prepareCheckout(plan, signal)).rejects.toThrow(/Pending receipt remains/);
			await fsp.chmod(receiptsDir, 0o700);
			expect((await receipts()).map(receipt => receipt.state)).toEqual(["pending"]);
			await expect(checkouts.prepareCheckout(plan, signal)).rejects.toBeInstanceOf(RecoveryRequiredError);
			expect(counting.adds()).toBe(1);
		},
		TIMEOUT,
	);

	test(
		"a Ready receipt write failure is not reported as success",
		async () => {
			const fixture = await gitFixture();
			const receiptsDir = path.join(stateDir, "receipts");
			const counting = countingRun(async (file, args, options) => {
				const result = await run(file, args, options);
				await fsp.chmod(receiptsDir, 0o500);
				return result;
			});
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);
			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-1" }, signal);
			await expect(checkouts.prepareCheckout(plan, signal)).rejects.toThrow(/Ready receipt could not be saved/);
			await fsp.chmod(receiptsDir, 0o700);
			expect((await receipts()).map(receipt => receipt.state)).toEqual(["pending"]);
			expect(await exists(plan.destination)).toBe(true);
		},
		TIMEOUT,
	);

	test(
		"a concurrent prepare of the same checkout is rejected while the first holds the locks",
		async () => {
			const fixture = await gitFixture();
			const gate = Promise.withResolvers<void>();
			const entered = Promise.withResolvers<void>();
			const counting = countingRun(async (file, args, options) => {
				entered.resolve();
				await gate.promise;
				return run(file, args, options);
			});
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);
			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-1" }, signal);
			const first = checkouts.prepareCheckout(plan, signal);
			await entered.promise;
			const rival = service(countingRun().run);
			await expect(rival.prepareCheckout(plan, signal)).rejects.toBeInstanceOf(StateLockBusyError);
			gate.resolve();
			expect((await first).created).toBe(true);
			expect(counting.adds()).toBe(1);
		},
		TIMEOUT,
	);

	test(
		"a failing jj workspace add is recorded and never retried",
		async () => {
			const fixture = await jjFixture(false);
			const fail = await fakeScript("fail.sh", 'echo "simulated failure" >&2\nexit 1');
			const counting = countingRun((_file, _args, options) => run(fail, [], options));
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "jj", signal);
			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-2" }, signal);
			await expect(checkouts.prepareCheckout(plan, signal)).rejects.toThrow(/jj workspace add failed/);
			expect((await receipts()).map(receipt => receipt.state)).toEqual(["failed"]);
			await expect(checkouts.planCheckout(repo, { kind: "new", name: "TEST-2" }, signal)).rejects.toBeInstanceOf(
				RecoveryRequiredError,
			);
			expect(counting.adds()).toBe(1);
		},
		TIMEOUT,
	);

	test(
		"a Failed jj workspace stays blocked after it is renamed",
		async () => {
			const fixture = await jjFixture(false);
			const failAfter = await fakeScript("fail-after.sh", '"$@" >/dev/null 2>&1\necho "simulated crash" >&2\nexit 1');
			const counting = countingRun((file, args, options) => run(failAfter, [file, ...args], options));
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "jj", signal);
			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-2" }, signal);
			await expect(checkouts.prepareCheckout(plan, signal)).rejects.toThrow(/jj workspace add failed/);

			sh(["jj", "--no-pager", "workspace", "rename", "TEST-2-renamed"], plan.destination);
			const listed = (await checkouts.listCheckouts(repo, signal)).find(checkout => checkout.name === "TEST-2-renamed");
			expect(listed?.path).toBe(plan.destination);
			if (!listed) throw new Error("renamed workspace not listed");
			await expect(
				checkouts.planCheckout(repo, { kind: "existing", checkout: listed, recoveryAcknowledged: false }, signal),
			).rejects.toBeInstanceOf(RecoveryRequiredError);
			const recovered = await checkouts.prepareCheckout(
				await checkouts.planCheckout(repo, { kind: "existing", checkout: listed, recoveryAcknowledged: true }, signal),
				signal,
			);
			expect(recovered.checkout.path).toBe(plan.destination);
			expect((await receipts()).map(receipt => receipt.state)).toEqual(["failed"]);
			expect(counting.adds()).toBe(1);
		},
		TIMEOUT,
	);
});

type JjFixture = { root: string; parent: string; dirty: Record<string, string> };

async function jjFixture(colocate: boolean, name = "jjsource"): Promise<JjFixture> {
	const root = path.join(temp, name);
	sh(["jj", "--no-pager", "git", "init", colocate ? "--colocate" : "--no-colocate", root], temp);
	await fsp.writeFile(path.join(root, "a.txt"), "committed\n");
	sh(["jj", "--no-pager", "commit", "-m", "one"], root);
	const parent = sh(["jj", "--no-pager", "--ignore-working-copy", "log", "--no-graph", "-r", "@-", "-T", "commit_id"], root);
	const dirty = { "a.txt": "dirty edit\n", "b.txt": "untracked\n" };
	for (const [file, text] of Object.entries(dirty)) await fsp.writeFile(path.join(root, file), text);
	return { root, parent, dirty };
}

async function expectNativeJjWorkspace(destination: string, parent: string): Promise<void> {
	expect((await fsp.lstat(path.join(destination, ".jj"))).isDirectory()).toBe(true);
	expect(await exists(path.join(destination, ".git"))).toBe(false);
	const parents = sh(
		["jj", "--no-pager", "--ignore-working-copy", "-R", destination, "log", "--no-graph", "-r", "@", "-T", 'parents.map(|c| c.commit_id()).join(",")'],
		destination,
	);
	expect(parents).toBe(parent);
	expect(await fsp.readFile(path.join(destination, "a.txt"), "utf8")).toBe("committed\n");
}

describe("native jj workspaces", () => {
	test(
		"non-colocated: new creates a named workspace on the planned parents and reuses it",
		async () => {
			const fixture = await jjFixture(false);
			const counting = countingRun();
			const checkouts = service(counting.run);
			const repo = await checkouts.resolveRepository(fixture.root, "git", signal);
			expect(repo).toEqual({ backend: "jj", key: `jj:${fixture.root}/.jj/repo`, root: fixture.root, name: "jjsource" });

			const plan = await checkouts.planCheckout(repo, { kind: "new", name: "TEST-2" }, signal);
			expect(plan.baseIds).toEqual([fixture.parent]);
			expect(await exists(worktreeRoot)).toBe(false);
			const prepared = await checkouts.prepareCheckout(plan, signal);
			expect(prepared.created).toBe(true);
			await expectNativeJjWorkspace(plan.destination, fixture.parent);
			await expectDirtyPreserved(fixture.root, fixture.dirty);

			const listed = await checkouts.listCheckouts(repo, signal);
			expect(listed.map(checkout => [checkout.name, checkout.path, checkout.source])).toEqual(
				expect.arrayContaining([
					["default", fixture.root, true],
					["TEST-2", plan.destination, false],
				]),
			);

			const again = await checkouts.prepareCheckout(await checkouts.planCheckout(repo, { kind: "new", name: "TEST-2" }, signal), signal);
			expect(again.created).toBe(false);
			expect(again.checkout.path).toBe(plan.destination);

			const entry = listed.find(checkout => checkout.name === "TEST-2");
			if (!entry) throw new Error("TEST-2 not listed");
			const existing = await checkouts.prepareCheckout(
				await checkouts.planCheckout(repo, { kind: "existing", checkout: entry, recoveryAcknowledged: false }, signal),
				signal,
			);
			expect(existing.checkout.path).toBe(plan.destination);
			expect(counting.adds()).toBe(1);
		},
		TIMEOUT,
	);

	test(
		"colocated: prefer selects the backend and jj new stays native-only",
		async () => {
			const fixture = await jjFixture(true);
			const checkouts = service(run);
			const asGit = await checkouts.resolveRepository(fixture.root, "git", signal);
			expect(asGit.backend).toBe("git");
			expect(asGit.key).toBe(`git:${fixture.root}/.git`);
			const asJj = await checkouts.resolveRepository(fixture.root, "jj", signal);
			expect(asJj.backend).toBe("jj");
			expect(asJj.key).toBe(`jj:${fixture.root}/.jj/repo`);

			const plan = await checkouts.planCheckout(asJj, { kind: "new", name: "TEST-2" }, signal);
			await checkouts.prepareCheckout(plan, signal);
			await expectNativeJjWorkspace(plan.destination, fixture.parent);
			await expectDirtyPreserved(fixture.root, fixture.dirty);

			const gitPlan = await checkouts.planCheckout(asGit, { kind: "new", name: "TEST-1" }, signal);
			const gitPrepared = await checkouts.prepareCheckout(gitPlan, signal);
			expect(sh(["git", "symbolic-ref", "--short", "HEAD"], gitPrepared.checkout.path)).toBe("TEST-1");
			expect(gitPrepared.checkout.path).not.toBe(plan.destination);
		},
		TIMEOUT,
	);

	test(
		"mismatched jj identity is rejected",
		async () => {
			const fixture = await jjFixture(false);
			const other = await jjFixture(false, "otherjj");
			const checkouts = service(run);
			const repo: Repository = await checkouts.resolveRepository(fixture.root, "jj", signal);
			const base = { repositoryKey: repo.key, detached: false, source: false, locked: false, prunable: false };
			await expect(
				checkouts.planCheckout(
					repo,
					{ kind: "existing", checkout: { ...base, name: "default", path: other.root }, recoveryAcknowledged: true },
					signal,
				),
			).rejects.toThrow(/belongs to jj:/);
			await expect(
				checkouts.planCheckout(
					repo,
					{ kind: "existing", checkout: { ...base, name: "TEST-9", path: fixture.root }, recoveryAcknowledged: true },
					signal,
				),
			).rejects.toThrow(/not listed/);
			for (const bad of ["", ".", "..", "-x", "a/b", "a\\b"]) {
				await expect(checkouts.planCheckout(repo, { kind: "new", name: bad }, signal)).rejects.toThrow(/invalid/);
			}
			await expectDirtyPreserved(other.root, other.dirty);
		},
		TIMEOUT,
	);
});
