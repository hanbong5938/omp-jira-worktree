import { fileURLToPath } from "node:url";
import type { ExtensionAPI, ExtensionCommandContext } from "@oh-my-pi/pi-coding-agent";
import { runBrowser } from "./src/browser.ts";
import { createCheckoutService } from "./src/checkouts.ts";
import { ConfigMissingError, loadConfig } from "./src/config.ts";
import { type JiraPort, normalizeIssueKey, type Services } from "./src/contracts.ts";
import { createHandoffService } from "./src/handoff.ts";
import { createJiraClient } from "./src/jira.ts";
import { createLauncher } from "./src/launch.ts";
import { run } from "./src/process.ts";
import { createStateStore, stateRoot } from "./src/state.ts";
import { createTransitionService } from "./src/transitions.ts";

const EXTENSION_PATH = fileURLToPath(import.meta.url);
const EXAMPLE_PATH = fileURLToPath(new URL("./config.example.json", import.meta.url));
const USAGE = "Usage: /jira            browse issues\n       /jira <KEY>      open issue detail (e.g. /jira ABC-123)";

/** Abort reads when the session shuts down; Jira writes (postTransition) keep their own lifetime. */
function withShutdown(jira: JiraPort, shutdown: AbortSignal): JiraPort {
	const combine = (signal: AbortSignal) => AbortSignal.any([signal, shutdown]);
	return {
		search: (jql, signal) => jira.search(jql, combine(signal)),
		issue: (key, signal) => jira.issue(key, combine(signal)),
		comments: (key, limit, signal) => jira.comments(key, limit, combine(signal)),
		transitions: (key, signal) => jira.transitions(key, combine(signal)),
		postTransition: (key, id, signal) => jira.postTransition(key, id, signal),
	};
}

export default function ompJiraWorktree(pi: ExtensionAPI): void {
	// Session-scoped state lives in this closure; module globals are shared across sessions.
	let shutdown = new AbortController();
	let browserOpen = false;

	async function assemble(): Promise<Services> {
		const config = await loadConfig();
		const state = createStateStore(stateRoot());
		const jira = withShutdown(createJiraClient(config, run, fetch), shutdown.signal);
		const handoff = createHandoffService(config, jira, state);
		return {
			config,
			jira,
			checkouts: createCheckoutService(config, run, state),
			buildSnapshot: (key, signal) => handoff.buildSnapshot(key, AbortSignal.any([signal, shutdown.signal])),
			saveSnapshot: handoff.saveSnapshot,
			launchOmp: createLauncher(config, EXTENSION_PATH),
			runTransition: createTransitionService(config, jira, state),
		};
	}

	async function handle(args: string, ctx: ExtensionCommandContext): Promise<void> {
		if (ctx.mode !== "tui" || !ctx.hasUI) {
			throw new Error("/jira: interactive TUI required");
		}
		const trimmed = args.trim();
		let initialKey: string | undefined;
		if (trimmed) {
			if (/\s/.test(trimmed)) {
				ctx.ui.notify(USAGE, "warning");
				return;
			}
			try {
				initialKey = normalizeIssueKey(trimmed);
			} catch {
				ctx.ui.notify(USAGE, "warning");
				return;
			}
		}
		if (browserOpen) {
			ctx.ui.notify("/jira is already open in this session.", "warning");
			return;
		}
		let services: Services;
		try {
			services = await assemble();
		} catch (error) {
			if (error instanceof ConfigMissingError) {
				ctx.ui.notify(
					`omp-jira-worktree config not found: ${error.path}\nCopy and edit the example: ${EXAMPLE_PATH}`,
					"error",
				);
				return;
			}
			throw error;
		}
		browserOpen = true;
		try {
			await runBrowser(ctx, services, initialKey);
		} finally {
			browserOpen = false;
		}
	}

	pi.registerCommand("jira", {
		description: "Browse Jira issues, create Git worktrees / jj workspaces, start omp with Jira context",
		handler: handle,
	});

	pi.on("session_shutdown", () => {
		shutdown.abort();
		shutdown = new AbortController();
	});
}
