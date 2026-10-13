import { execFile } from "node:child_process";
import { RUN_MAX_BUFFER, type Run, type RunResult } from "./contracts.ts";

/**
 * Real `Run` adapter: argv-only `execFile` (no shell) with timeout and a 2 MiB
 * per-stream buffer. Never rejects for non-zero exit, timeout, or abort
 * (`killed: true`); spawn failures (ENOENT, EACCES, …) reject so callers can
 * distinguish "missing binary" from "command failed".
 */
export const run: Run = (file, args, options) => {
	const { promise, resolve, reject } = Promise.withResolvers<RunResult>();
	execFile(
		file,
		[...args],
		{
			cwd: options.cwd,
			signal: options.signal,
			timeout: options.timeoutMs,
			maxBuffer: RUN_MAX_BUFFER,
			encoding: "utf8",
			shell: false,
			windowsHide: true,
		},
		(error, stdout, stderr) => {
			if (!error) {
				resolve({ code: 0, stdout, stderr, killed: false });
				return;
			}
			const err = error as Error & { code?: unknown; killed?: boolean; signal?: string | null };
			const out = String(stdout ?? "");
			const errText = String(stderr ?? "");
			if (err.name === "AbortError" || err.code === "ABORT_ERR") {
				resolve({ code: -1, stdout: out, stderr: errText, killed: true });
				return;
			}
			if (err.code === "ERR_CHILD_PROCESS_STDIO_MAXBUFFER") {
				resolve({ code: -1, stdout: out, stderr: errText, killed: true });
				return;
			}
			if (typeof err.code === "string") {
				reject(new Error(`failed to start ${file}: ${err.code}`));
				return;
			}
			resolve({
				code: typeof err.code === "number" ? err.code : -1,
				stdout: out,
				stderr: errText,
				killed: Boolean(err.killed) || err.signal != null,
			});
		},
	);
	return promise;
};
