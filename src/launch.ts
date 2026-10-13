/**
 * Start a child omp session for a prepared checkout through the host's
 * terminal-launch capability. Anything short of a host response is a manual
 * result; a failed or malformed response after dispatch is unknown. Never retries.
 */
import { isAbsolute } from "node:path";
import type {
	TerminalLaunchMultiplexer,
	TerminalLaunchPlacement,
	TerminalLaunchRequest,
} from "@oh-my-pi/pi-coding-agent/subprocess/terminal-launch";
import { classifyTerminalMultiplexer } from "@oh-my-pi/pi-tui";
import type { TerminalMultiplexer } from "@oh-my-pi/pi-tui/terminal-multiplexer";
import type { Config, ExtensionCommandContext, LaunchResult, PreparedCheckout, Services } from "./contracts.ts";
import { LAUNCH_MESSAGE } from "./contracts.ts";
import { isRecord, sanitizeForTerminal } from "./text.ts";

const MESSAGE_LIMIT = 300;
const SAFE_WORD = /^[A-Za-z0-9_@%+=:,./-]+$/;

type ProviderInfo = Readonly<{
	displayName: string;
	/** User-facing label of each placement in the provider's own vocabulary. */
	labels: Readonly<Record<TerminalLaunchPlacement, string>>;
	/** The provider types the command into the new interactive shell, so the user must confirm POSIX grammar. */
	posixShellInput: boolean;
}>;

/**
 * Providers extensions may request through `ctx.ui.openTerminal`. Keyed by the
 * host's supported-provider type so a host change breaks type-check.
 */
const PROVIDERS = {
	tmux: { displayName: "tmux", labels: { pane: "pane", window: "window" }, posixShellInput: false },
	zellij: { displayName: "Zellij", labels: { pane: "pane", window: "tab" }, posixShellInput: false },
	herdr: { displayName: "Herdr", labels: { pane: "pane", window: "tab" }, posixShellInput: true },
	cmux: { displayName: "CMUX", labels: { pane: "pane", window: "workspace" }, posixShellInput: true },
	orca: { displayName: "Orca", labels: { pane: "pane", window: "tab" }, posixShellInput: true },
} as const satisfies Record<TerminalLaunchMultiplexer, ProviderInfo>;

function supported(multiplexer: TerminalMultiplexer | null): multiplexer is TerminalLaunchMultiplexer {
	return multiplexer !== null && Object.hasOwn(PROVIDERS, multiplexer);
}

/**
 * Explicit provider-default request: no target, focus, direction, or execution.
 * Shell-input providers carry `shellGrammar: "posix"`, which callers only build after the user confirmed it.
 */
function buildRequest(
	multiplexer: TerminalLaunchMultiplexer,
	placement: TerminalLaunchPlacement,
	command: readonly string[],
	cwd: string,
): TerminalLaunchRequest {
	const pane = placement === "pane";
	switch (multiplexer) {
		case "tmux":
			return pane
				? { multiplexer, placement: "pane", command, cwd }
				: { multiplexer, placement: "window", command, cwd };
		case "zellij":
			return pane
				? { multiplexer, placement: "pane", command, cwd }
				: { multiplexer, placement: "window", command, cwd };
		case "herdr":
			return pane
				? { multiplexer, placement: "pane", command, cwd, shellGrammar: "posix" }
				: { multiplexer, placement: "window", command, cwd, shellGrammar: "posix" };
		case "cmux":
			return pane
				? { multiplexer, placement: "pane", command, cwd, shellGrammar: "posix" }
				: { multiplexer, placement: "window", command, cwd, shellGrammar: "posix" };
		case "orca":
			return pane
				? { multiplexer, placement: "pane", command, cwd, shellGrammar: "posix" }
				: { multiplexer, placement: "window", command, cwd, shellGrammar: "posix" };
	}
}

/** POSIX single-quote rendering of argv for display only; never submitted to a shell or composer. */
export function posixQuote(argv: readonly string[]): string {
	return argv.map((word) => (SAFE_WORD.test(word) ? word : `'${word.replaceAll("'", "'\\''")}'`)).join(" ");
}

function message(text: string): string {
	const line = sanitizeForTerminal(text).replace(/\s+/g, " ").trim();
	return line.length > MESSAGE_LIMIT ? `${line.slice(0, MESSAGE_LIMIT - 1)}…` : line;
}

export function createLauncher(
	config: Config,
	extensionPath: string,
	env: NodeJS.ProcessEnv = process.env,
): Services["launchOmp"] {
	if (!isAbsolute(extensionPath)) throw new TypeError("extensionPath must be absolute");

	return async (
		prepared: PreparedCheckout,
		snapshotPath: string,
		placement: "pane" | "window",
		ctx: ExtensionCommandContext,
	): Promise<LaunchResult> => {
		if (!isAbsolute(snapshotPath)) throw new TypeError("snapshotPath must be absolute");
		const cwd = prepared.checkout.path;
		const argv = [
			config.omp.binary,
			"--cwd",
			cwd,
			"--profile",
			config.omp.profile,
			"--extension",
			extensionPath,
			`@${snapshotPath}`,
			"--",
			LAUNCH_MESSAGE,
		];
		const manual = (reason: string): LaunchResult => ({ state: "manual", argv: [...argv], cwd, reason: message(reason) });

		const multiplexer = classifyTerminalMultiplexer(env);
		if (!supported(multiplexer)) {
			return manual(
				multiplexer === null
					? "지원되는 터미널 멀티플렉서(tmux, Zellij, Herdr, CMUX, Orca) 안에서 실행 중이 아닙니다."
					: `${multiplexer}에서는 터미널 실행을 지원하지 않습니다 (지원: tmux, Zellij, Herdr, CMUX, Orca).`,
			);
		}
		const info = PROVIDERS[multiplexer];
		const placementLabel = info.labels[placement];
		const ui = ctx.ui;
		const openTerminal = ui.openTerminal;
		if (typeof openTerminal !== "function") {
			return manual("현재 모드에서는 터미널 실행 기능(openTerminal)을 사용할 수 없습니다.");
		}

		if (info.posixShellInput) {
			let accepted: boolean;
			try {
				accepted = await ui.confirm(
					`${info.displayName} 대상 shell 확인`,
					`${info.displayName}는 새 ${placementLabel}의 대화형 shell에 명령 텍스트를 입력합니다. 그 shell이 POSIX 문법(sh/bash/zsh)인가요? fish, Nushell, PowerShell 등이거나 확실하지 않으면 '아니요'를 선택하세요.`,
				);
			} catch (error) {
				return manual(`대상 shell을 확인하지 못했습니다: ${error instanceof Error ? error.message : String(error)}`);
			}
			if (!accepted) return manual(`${info.displayName} 대상 shell이 POSIX임을 확인하지 않았습니다.`);
		}

		const request = buildRequest(multiplexer, placement, argv, cwd);

		let response: unknown;
		try {
			response = await openTerminal.call(ui, request);
		} catch (error) {
			const detail = error instanceof Error ? error.message : String(error);
			return {
				state: "unknown",
				argv: [...argv],
				cwd,
				reason: message(`터미널 실행 요청 결과를 알 수 없습니다: ${detail}`),
			};
		}
		if (
			!isRecord(response) ||
			(response.id !== undefined && typeof response.id !== "string") ||
			(response.warning !== undefined && typeof response.warning !== "string")
		) {
			return { state: "unknown", argv: [...argv], cwd, reason: "터미널 실행 응답 형식이 올바르지 않습니다." };
		}
		return {
			state: "requested",
			...(typeof response.id === "string" ? { id: message(response.id) } : {}),
			...(typeof response.warning === "string" ? { warning: message(response.warning) } : {}),
		};
	};
}
