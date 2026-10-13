/**
 * Pure rendering and state helpers for the Jira browser. Nothing here touches
 * the host TUI, the network, or the filesystem; `browser.ts` owns mounting.
 * Every rendered line is width-safe through the host display-cell helpers.
 */
import { truncateToWidth, visibleWidth, wrapTextWithAnsi, type ThemeColor } from "@oh-my-pi/pi-tui";
import type { CheckoutPlan, Comment, Config, Issue, LaunchResult, Page, Snapshot } from "./contracts.ts";
import { posixQuote } from "./launch.ts";
import { sanitizeForTerminal } from "./text.ts";

// ---------------------------------------------------------------------------
// Queries

/** Initial query when `config.filters` is empty. */
export const DEFAULT_FILTER: QueryRef = {
	label: "My open issues",
	jql: "assignee = currentUser() AND resolution = Unresolved ORDER BY updated DESC",
};

export type QueryRef = { label: string; jql: string };

/** Escape text for use inside a double-quoted JQL string literal. */
export function escapeJqlString(text: string): string {
	return text.replace(/[\\"]/g, ch => `\\${ch}`);
}

/** Substitute an escaped search term into the configured `{query}` template. */
export function buildSearchJql(template: string, query: string): string {
	const escaped = escapeJqlString(query);
	// Function replacer: `$&`/`$1` in user text must stay literal.
	return template.replace("{query}", () => escaped);
}

/** Named filters to offer; the default JQL stands in when none are configured. */
export function filterChoices(config: Config): QueryRef[] {
	if (config.filters.length === 0) return [DEFAULT_FILTER];
	return config.filters.map(filter => ({ label: filter.name, jql: filter.jql }));
}

export function initialQuery(config: Config): QueryRef {
	return filterChoices(config)[0] ?? DEFAULT_FILTER;
}

// ---------------------------------------------------------------------------
// Text helpers

/** Minimal styling surface; the host `Theme` satisfies it structurally. */
export type Paint = {
	fg(color: ThemeColor, text: string): string;
	bold(text: string): string;
	inverse(text: string): string;
};

/** Sanitized, whitespace-collapsed single line for table cells and headers. */
export function singleLine(text: string): string {
	return sanitizeForTerminal(text).replace(/\s+/g, " ").trim();
}

/** Truncate to `width` cells (never wider). `pad` fills to exactly `width`. */
export function fit(text: string, width: number, pad = false): string {
	if (width <= 0) return "";
	return truncateToWidth(text, width, undefined, pad);
}

/** Wrap sanitized multi-line text to `width` cells; every row is width-safe. */
export function wrapText(text: string, width: number): string[] {
	if (width <= 0) return [];
	const rows: string[] = [];
	for (const raw of sanitizeForTerminal(text).split("\n")) {
		if (raw.trim() === "") {
			rows.push("");
			continue;
		}
		for (const row of wrapTextWithAnsi(raw, width)) rows.push(fit(row, width));
	}
	while (rows.length > 0 && rows[rows.length - 1] === "") rows.pop();
	return rows;
}

function pad2(value: number): string {
	return value < 10 ? `0${value}` : String(value);
}

/** `YYYY-MM-DD HH:mm` (local time) or `YYYY-MM-DD` when `dateOnly`; unparseable input is shown sanitized. */
export function formatTimestamp(value: string, dateOnly = false): string {
	const ms = Date.parse(value);
	if (Number.isNaN(ms)) return singleLine(value);
	const date = new Date(ms);
	const day = `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}`;
	return dateOnly ? day : `${day} ${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
}

// ---------------------------------------------------------------------------
// Request state with a generation guard

export type LoadStatus = "loading" | "ready" | "empty" | "error";

export type LoadState<T> = {
	readonly status: LoadStatus;
	/** Key of the latest request (query JQL or issue key). */
	readonly key: string;
	/** Last successfully applied data, possibly from an earlier key. */
	readonly data: T | undefined;
	readonly dataKey: string | undefined;
	/** `data` does not reflect a fresh successful response for `key`. */
	readonly stale: boolean;
	readonly error: string | undefined;
};

type Request<T> = {
	key: string;
	fetcher: (signal: AbortSignal) => Promise<T>;
	onApplied: ((data: T, previous: LoadState<T>) => void) | undefined;
};

export function errorMessage(error: unknown): string {
	const raw = error instanceof Error ? error.message : String(error);
	return singleLine(raw) || "unknown error";
}

/**
 * One logical resource (list query, issue detail, comments). Each request gets
 * a new generation and AbortController; responses from superseded or aborted
 * generations are dropped, so a slow request A can never overwrite B.
 * Failures keep the previous data but mark it stale.
 */
export class GenerationLoader<T> {
	#generation = 0;
	#controller: AbortController | undefined;
	#inFlight: Request<T> | undefined;
	#suspended: Request<T> | undefined;
	#disposed = false;
	#state: LoadState<T> = {
		status: "loading",
		key: "",
		data: undefined,
		dataKey: undefined,
		stale: false,
		error: undefined,
	};
	readonly #isEmpty: (data: T) => boolean;
	readonly #onChange: () => void;

	constructor(isEmpty: (data: T) => boolean, onChange: () => void) {
		this.#isEmpty = isEmpty;
		this.#onChange = onChange;
	}

	get state(): LoadState<T> {
		return this.#state;
	}

	/** True only after a successful response for the current key with no later failure. */
	get fresh(): boolean {
		const { status, stale } = this.#state;
		return (status === "ready" || status === "empty") && !stale;
	}

	/** Start a request; the returned promise never rejects. */
	load(
		key: string,
		fetcher: (signal: AbortSignal) => Promise<T>,
		onApplied?: (data: T, previous: LoadState<T>) => void,
	): Promise<void> {
		if (this.#disposed) return Promise.resolve();
		this.#controller?.abort();
		this.#suspended = undefined;
		const generation = ++this.#generation;
		const controller = new AbortController();
		const request: Request<T> = { key, fetcher, onApplied };
		this.#controller = controller;
		this.#inFlight = request;
		this.#state = {
			...this.#state,
			status: "loading",
			key,
			stale: this.#state.data !== undefined,
			error: undefined,
		};
		this.#onChange();
		const current = () => generation === this.#generation && !controller.signal.aborted && !this.#disposed;
		let pending: Promise<T>;
		try {
			pending = fetcher(controller.signal);
		} catch (error) {
			pending = Promise.reject(error);
		}
		return pending
			.then(
				data => {
					if (!current()) return;
					this.#controller = undefined;
					this.#inFlight = undefined;
					const previous = this.#state;
					this.#state = {
						status: this.#isEmpty(data) ? "empty" : "ready",
						key,
						data,
						dataKey: key,
						stale: false,
						error: undefined,
					};
					request.onApplied?.(data, previous);
					this.#onChange();
				},
				error => {
					if (!current()) return;
					this.#controller = undefined;
					this.#inFlight = undefined;
					this.#state = {
						...this.#state,
						status: "error",
						stale: this.#state.data !== undefined,
						error: errorMessage(error),
					};
					this.#onChange();
				},
			)
			.catch(error => {
				// A throwing UI callback must not leave a detached rejection; surface it as the request error.
				this.#state = { ...this.#state, status: "error", stale: this.#state.data !== undefined, error: errorMessage(error) };
			});
	}

	/** Abort the in-flight request but remember it so `resume()` can reissue it. */
	suspend(): void {
		if (!this.#controller || !this.#inFlight) return;
		this.#suspended = this.#inFlight;
		this.#inFlight = undefined;
		this.#generation++;
		this.#controller.abort();
		this.#controller = undefined;
	}

	/** Reissue a request interrupted by `suspend()`. */
	resume(): void {
		const request = this.#suspended;
		if (!request || this.#disposed) return;
		this.#suspended = undefined;
		void this.load(request.key, request.fetcher, request.onApplied);
	}

	dispose(): void {
		if (this.#disposed) return;
		this.#disposed = true;
		this.#generation++;
		this.#controller?.abort();
		this.#controller = undefined;
		this.#inFlight = undefined;
		this.#suspended = undefined;
	}
}

// ---------------------------------------------------------------------------
// Issue list model

export type SearchFn = (jql: string, signal: AbortSignal) => Promise<Page<Issue>>;

/**
 * List query + selection. Only the latest query generation is applied; a
 * refresh of the same JQL keeps the selected issue (by key), a new query
 * resets selection and scroll.
 */
export class IssueListModel {
	query: QueryRef;
	selected = 0;
	scroll = 0;
	/** False until the first `load`; `/jira <KEY>` opens detail without a list request. */
	started = false;
	readonly loader: GenerationLoader<Page<Issue>>;

	constructor(query: QueryRef, onChange: () => void) {
		this.query = query;
		this.loader = new GenerationLoader<Page<Issue>>(page => page.items.length === 0, onChange);
	}

	load(query: QueryRef, search: SearchFn): Promise<void> {
		this.query = query;
		this.started = true;
		const jql = query.jql;
		return this.loader.load(
			jql,
			signal => search(jql, signal),
			(page, previous) => {
				if (previous.dataKey !== jql) {
					this.selected = 0;
					this.scroll = 0;
					return;
				}
				const keep = previous.data?.items[this.selected]?.key;
				const index = keep === undefined ? -1 : page.items.findIndex(issue => issue.key === keep);
				this.selected = index >= 0 ? index : clampIndex(this.selected, page.items.length);
			},
		);
	}

	refresh(search: SearchFn): Promise<void> {
		return this.load(this.query, search);
	}

	move(delta: number): void {
		this.selected = clampIndex(this.selected + delta, this.loader.state.data?.items.length ?? 0);
	}

	/** Selected row of the displayed data (which may be stale; check `loader.fresh` before acting). */
	get selectedIssue(): Issue | undefined {
		return this.loader.state.data?.items[this.selected];
	}
}

// ---------------------------------------------------------------------------
// Scroll math

export function clampScroll(offset: number, total: number, visible: number): number {
	const max = Math.max(0, total - Math.max(0, visible));
	return Math.max(0, Math.min(offset, max));
}

/** Keep `selected` inside the `visible`-row window starting at `scroll`. */
export function ensureVisible(selected: number, scroll: number, visible: number, total: number): number {
	if (visible <= 0) return 0;
	let top = scroll;
	if (selected < top) top = selected;
	if (selected >= top + visible) top = selected - visible + 1;
	return clampScroll(top, total, visible);
}

export function clampIndex(index: number, length: number): number {
	if (length <= 0) return 0;
	return Math.max(0, Math.min(index, length - 1));
}

function rangeLabel(scroll: number, visible: number, total: number): string {
	if (total === 0) return "0/0";
	const end = Math.min(total, scroll + visible);
	return `${scroll + 1}-${end}/${total}`;
}

// ---------------------------------------------------------------------------
// Issue list

type Column = { title: string; width: number; cell: (issue: Issue) => string };

const ROW_PREFIX_WIDTH = 2;

/** Column set for a terminal width; narrow widths drop assignee/updated. */
export function listColumns(width: number): Column[] {
	const key: Column = { title: "Key", width: 11, cell: issue => singleLine(issue.key) };
	if (width >= 100) {
		key.width = 12;
		return withSummary(width, [
			key,
			{ title: "Status", width: 14, cell: issue => singleLine(issue.status.name) },
			{ title: "Assignee", width: 16, cell: issue => singleLine(issue.assignee) || "-" },
			{ title: "Updated", width: 16, cell: issue => formatTimestamp(issue.updated) },
		]);
	}
	if (width >= 72) {
		return withSummary(width, [
			key,
			{ title: "Status", width: 11, cell: issue => singleLine(issue.status.name) },
			{ title: "Assignee", width: 12, cell: issue => singleLine(issue.assignee) || "-" },
			{ title: "Updated", width: 10, cell: issue => formatTimestamp(issue.updated, true) },
		]);
	}
	return withSummary(width, [key, { title: "Status", width: 10, cell: issue => singleLine(issue.status.name) }]);
}

function withSummary(width: number, columns: Column[]): Column[] {
	const used = ROW_PREFIX_WIDTH + columns.reduce((sum, column) => sum + column.width + 1, 0);
	return [...columns, { title: "Summary", width: Math.max(0, width - used), cell: issue => singleLine(issue.summary) }];
}

export function formatListHeader(width: number, paint: Paint): string {
	const cells = listColumns(width).map(column => fit(column.title, column.width, true));
	return paint.fg("muted", fit(`${" ".repeat(ROW_PREFIX_WIDTH)}${cells.join(" ")}`, width));
}

export function formatIssueRow(issue: Issue, width: number, selected: boolean, paint: Paint): string {
	const cells = listColumns(width).map(column => fit(column.cell(issue), column.width, true));
	const line = fit(`${selected ? "› " : "  "}${cells.join(" ")}`, width, true);
	return selected ? paint.inverse(line) : line;
}

export type ListView = {
	query: QueryRef;
	load: LoadState<Page<Issue>>;
	selected: number;
	scroll: number;
	notice: string | undefined;
};

/** Rows reserved around the issue table besides help: title, JQL, header, status. */
const LIST_CHROME_ROWS = 4;

// Esc first: if the host clips the overlay's bottom row, the close key must stay on the first help line.
const LIST_HELP = [
	"Esc close",
	"j/k move",
	"Enter detail",
	"/ search",
	"J jql",
	"f filter",
	"r refresh",
	"w checkout",
	"d omp",
	"s status",
] as const;

/** Pack help items into as many ` · `-joined lines as the width needs; items are never split. */
export function helpLines(items: readonly string[], width: number): string[] {
	const lines: string[] = [];
	let current = "";
	for (const item of items) {
		// ASCII separator: `·`/`→` are East Asian Ambiguous and render 2 cells wide in CJK terminals.
		const next = current ? `${current}  ${item}` : item;
		if (current && visibleWidth(next) > width) {
			lines.push(current);
			current = item;
		} else {
			current = next;
		}
	}
	if (current) lines.push(current);
	return lines.map(line => fit(line, width));
}

export function listVisibleRows(width: number, height: number): number {
	return Math.max(1, height - LIST_CHROME_ROWS - helpLines(LIST_HELP, width).length);
}

function statusBadge(load: LoadState<unknown>, paint: Paint): string {
	switch (load.status) {
		case "loading":
			return paint.fg("warning", load.stale ? "[loading… showing stale data]" : "[loading…]");
		case "error":
			return paint.fg("error", load.stale ? "[error · stale data]" : "[error]");
		case "empty":
		case "ready":
			return load.stale ? paint.fg("warning", "[stale]") : "";
	}
}

export function renderList(view: ListView, width: number, height: number, paint: Paint): { lines: string[]; scroll: number } {
	const items = view.load.data?.items ?? [];
	const visible = listVisibleRows(width, height);
	const selected = clampIndex(view.selected, items.length);
	const scroll = ensureVisible(selected, view.scroll, visible, items.length);
	const page = view.load.data;
	const count = page ? `${items.length}${page.hasMore ? "+" : ""}${page.total !== undefined ? ` of ${page.total}` : ""}` : "";
	const lines: string[] = [];
	lines.push(fit(`${paint.bold(`Jira · ${singleLine(view.query.label)}`)} ${count ? `(${count}) ` : ""}${statusBadge(view.load, paint)}`, width));
	lines.push(paint.fg("dim", fit(`JQL: ${singleLine(view.query.jql)}`, width)));
	lines.push(formatListHeader(width, paint));
	if (items.length === 0) {
		const message =
			view.load.status === "loading"
				? "Loading…"
				: view.load.status === "error"
					? `Search failed: ${view.load.error ?? ""}`
					: "No issues match this query.";
		lines.push(fit(view.load.status === "error" ? paint.fg("error", message) : paint.fg("muted", message), width));
		for (let i = 1; i < visible; i++) lines.push("");
	} else {
		for (let i = 0; i < visible; i++) {
			const issue = items[scroll + i];
			lines.push(issue ? formatIssueRow(issue, width, scroll + i === selected, paint) : "");
		}
	}
	const status =
		view.load.status === "error" && items.length > 0
			? paint.fg("error", `Refresh failed: ${view.load.error ?? ""} — actions need a fresh fetch (r)`)
			: view.notice
				? paint.fg("warning", singleLine(view.notice))
				: items.length > 0
					? paint.fg("dim", `${rangeLabel(scroll, visible, items.length)}${page?.hasMore ? " · more results exist on the server" : ""}`)
					: "";
	lines.push(fit(status, width));
	for (const line of helpLines(LIST_HELP, width)) lines.push(paint.fg("muted", line));
	return { lines: lines.slice(0, Math.max(0, height)), scroll };
}

// ---------------------------------------------------------------------------
// Issue detail

export type DetailFocus = "description" | "comments";

export type PaneLayout = { width: number; rows: number };

export type DetailLayout = {
	stacked: boolean;
	description: PaneLayout;
	comments: PaneLayout;
};

/** Header rows above the panes; help rows below are passed in. */
const DETAIL_HEADER_ROWS = 4;
const PANE_SEPARATOR = " │ ";

/** Split the pane area: side by side at width >= 80, stacked below that. */
export function detailLayout(width: number, height: number, helpRows = 1): DetailLayout {
	const body = Math.max(2, height - DETAIL_HEADER_ROWS - helpRows);
	if (width < 80) {
		const content = Math.max(0, body - 2);
		const description = Math.ceil(content / 2);
		return {
			stacked: true,
			description: { width, rows: description },
			comments: { width, rows: content - description },
		};
	}
	const left = Math.floor((width - PANE_SEPARATOR.length) / 2);
	const right = width - PANE_SEPARATOR.length - left;
	const rows = Math.max(0, body - 1);
	return { stacked: false, description: { width: left, rows }, comments: { width: right, rows } };
}

export type CommentsState = { kind: "skipped" } | { kind: "loaded"; load: LoadState<Page<Comment>> };

export type DetailView = {
	key: string;
	issue: LoadState<Issue>;
	comments: CommentsState;
	focus: DetailFocus;
	descriptionScroll: number;
	commentsScroll: number;
	notice: string | undefined;
};

export function descriptionLines(load: LoadState<Issue>, width: number): string[] {
	const issue = load.data;
	if (!issue) {
		if (load.status === "error") return wrapText(`Issue fetch failed: ${load.error ?? ""}`, width);
		return ["Loading…"];
	}
	const body = wrapText(issue.description, width);
	return body.length > 0 ? body : ["(no description)"];
}

export function commentLines(state: CommentsState, width: number, paint: Paint): string[] {
	if (state.kind === "skipped") return wrapText("Comments not fetched (handoff.maxComments = 0).", width);
	const { load } = state;
	const page = load.data;
	if (!page) {
		if (load.status === "error") return wrapText(`Comments fetch failed: ${load.error ?? ""}`, width);
		return ["Loading…"];
	}
	const rows: string[] = [];
	const summary = `${page.items.length} comment${page.items.length === 1 ? "" : "s"}${page.hasMore ? " (조회한 댓글, 일부)" : ""}`;
	rows.push(paint.fg("dim", fit(summary, width)));
	if (load.status === "error") rows.push(paint.fg("error", fit(`Refresh failed: ${load.error ?? ""}`, width)));
	for (const comment of page.items) {
		rows.push("");
		rows.push(paint.fg("accent", fit(`${singleLine(comment.author) || "?"} · ${formatTimestamp(comment.created)}`, width)));
		const body = wrapText(comment.body, width);
		rows.push(...(body.length > 0 ? body : ["(empty)"]));
	}
	return rows;
}

export type DetailRender = {
	lines: string[];
	layout: DetailLayout;
	descriptionScroll: number;
	commentsScroll: number;
	descriptionTotal: number;
	commentsTotal: number;
};

function paneTitle(
	name: string,
	focused: boolean,
	scroll: number,
	rows: number,
	total: number,
	width: number,
	paint: Paint,
): string {
	const more = `${scroll > 0 ? "↑" : ""}${scroll + rows < total ? "↓" : ""}`;
	const text = `${focused ? "▸ " : "  "}${name} ${rangeLabel(scroll, rows, total)}${more ? ` ${more}` : ""}`;
	const title = fit(text, width, true);
	return focused ? paint.fg("accent", paint.bold(title)) : paint.fg("muted", title);
}

function detailHelp(focus: "description" | "comments", scrollHint: string): string[] {
	const otherName = focus === "description" ? "Comments" : "Description";
	return ["Esc list", `Tab: ${otherName}`, scrollHint, "r refresh", "w checkout", "d omp", "s status"];
}

export function renderDetail(view: DetailView, width: number, height: number, paint: Paint): DetailRender {
	// Reserve help rows for the longest scroll hint so the pane height stays stable while scrolling.
	const helpRows = helpLines(detailHelp("description", "j/k PgUp/PgDn scroll Description (top)"), width).length;
	const layout = detailLayout(width, height, helpRows);
	const descAll = descriptionLines(view.issue, layout.description.width);
	const commentAll = commentLines(view.comments, layout.comments.width, paint);
	const descriptionScroll = clampScroll(view.descriptionScroll, descAll.length, layout.description.rows);
	const commentsScroll = clampScroll(view.commentsScroll, commentAll.length, layout.comments.rows);
	const lines: string[] = [];

	const issue = view.issue.data;
	const meta = issue
		? [singleLine(issue.status.name), singleLine(issue.issueType), singleLine(issue.assignee) || "unassigned", formatTimestamp(issue.updated)]
				.filter(Boolean)
				.join(" · ")
		: "";
	lines.push(fit(`${paint.bold(singleLine(view.key))}  ${meta} ${statusBadge(view.issue, paint)}`, width));
	lines.push(fit(issue ? singleLine(issue.summary) : "", width));
	lines.push(paint.fg("dim", fit(issue ? singleLine(issue.url) : "", width)));
	const status =
		view.issue.status === "error"
			? paint.fg("error", `Issue fetch failed: ${view.issue.error ?? ""}${view.issue.stale ? " — showing stale data; press r" : ""}`)
			: view.notice
				? paint.fg("warning", singleLine(view.notice))
				: "";
	lines.push(fit(status, width));

	const descView = descAll.slice(descriptionScroll, descriptionScroll + layout.description.rows);
	const commentView = commentAll.slice(commentsScroll, commentsScroll + layout.comments.rows);
	const descTitle = paneTitle(
		"Description",
		view.focus === "description",
		descriptionScroll,
		layout.description.rows,
		descAll.length,
		layout.description.width,
		paint,
	);
	const commentTitle = paneTitle(
		"Comments",
		view.focus === "comments",
		commentsScroll,
		layout.comments.rows,
		commentAll.length,
		layout.comments.width,
		paint,
	);
	if (layout.stacked) {
		lines.push(descTitle);
		for (let i = 0; i < layout.description.rows; i++) lines.push(fit(descView[i] ?? "", width));
		lines.push(commentTitle);
		for (let i = 0; i < layout.comments.rows; i++) lines.push(fit(commentView[i] ?? "", width));
	} else {
		const sep = paint.fg("border", PANE_SEPARATOR);
		lines.push(fit(`${descTitle}${sep}${commentTitle}`, width));
		for (let i = 0; i < layout.description.rows; i++) {
			const left = fit(descView[i] ?? "", layout.description.width, true);
			const right = fit(commentView[i] ?? "", layout.comments.width);
			lines.push(fit(`${left}${sep}${right}`, width));
		}
	}

	const focusName = view.focus === "description" ? "Description" : "Comments";
	const scroll = view.focus === "description" ? descriptionScroll : commentsScroll;
	const rows = view.focus === "description" ? layout.description.rows : layout.comments.rows;
	const total = view.focus === "description" ? descAll.length : commentAll.length;
	const position = scroll === 0 ? (total > rows ? "top" : "") : scroll + rows >= total ? "end" : "";
	const scrollHint = `${total > rows ? `j/k PgUp/PgDn scroll ${focusName}` : `${focusName} fits`}${position ? ` (${position})` : ""}`;
	for (const line of helpLines(detailHelp(view.focus, scrollHint), width)) lines.push(paint.fg("muted", line));
	return {
		lines: lines.slice(0, Math.max(0, height)),
		layout,
		descriptionScroll,
		commentsScroll,
		descriptionTotal: descAll.length,
		commentsTotal: commentAll.length,
	};
}

// ---------------------------------------------------------------------------
// Scrollable text screens (snapshot preview, final confirm, results)

export type TextScreenView = { title: string; body: readonly string[]; help: string; scroll: number };

export function textScreenRows(height: number): number {
	return Math.max(1, height - 3);
}

/** Title, rule, wrapped body window, help. Body entries are wrapped (sanitized) to width. */
export function renderTextScreen(
	view: TextScreenView,
	width: number,
	height: number,
	paint: Paint,
): { lines: string[]; scroll: number; total: number } {
	const wrapped: string[] = [];
	for (const entry of view.body) {
		const rows = wrapText(entry, width);
		wrapped.push(...(rows.length > 0 ? rows : [""]));
	}
	const visible = textScreenRows(height);
	const scroll = clampScroll(view.scroll, wrapped.length, visible);
	const lines = [paint.bold(fit(singleLine(view.title), width)), paint.fg("border", fit("─".repeat(width), width))];
	for (let i = 0; i < visible; i++) lines.push(wrapped[scroll + i] ?? "");
	const more = wrapped.length > visible ? ` · ${rangeLabel(scroll, visible, wrapped.length)}` : "";
	lines.push(paint.fg("muted", fit(`${view.help}${more}`, width)));
	return { lines: lines.slice(0, Math.max(0, height)), scroll, total: wrapped.length };
}

export function snapshotPreviewBody(snapshot: Snapshot, profile: string): string[] {
	const body: string[] = [];
	if (snapshot.partialReasons.length > 0) {
		body.push("⚠ INCOMPLETE SNAPSHOT — explicit confirmation will be required:");
		for (const reason of snapshot.partialReasons) body.push(`  - ${reason}`);
	} else {
		body.push("Snapshot complete.");
	}
	body.push(
		`Captured ${formatTimestamp(snapshot.capturedAt)} for ${snapshot.issueKey}.`,
		"After final confirmation this snapshot and your task instruction are written to a private handoff file (0600) that stays on disk and may also be stored in the new omp session.",
		`Jira data will be sent to the model provider of omp profile "${profile}".`,
		"",
		"──────── snapshot ────────",
		snapshot.text,
	);
	return body;
}

export type CheckoutAction = "checkout" | "pane" | "window";

export function actionLabel(action: CheckoutAction): string {
	switch (action) {
		case "checkout":
			return "checkout only";
		case "pane":
			return "start omp in a new pane";
		case "window":
			return "start omp in a new tab";
	}
}

export type FinalSummary = {
	issueKey: string;
	plan: CheckoutPlan;
	action: CheckoutAction;
	profile: string;
	snapshot: Snapshot | undefined;
	instruction: string | undefined;
};

export function finalConfirmBody(summary: FinalSummary): string[] {
	const { plan } = summary;
	const selection = plan.selection;
	const body = [
		`Issue:        ${summary.issueKey}`,
		`Repository:   ${plan.repository.name}  (${plan.repository.root})`,
		`Backend:      ${plan.repository.backend}`,
		`Identity:     ${plan.repository.key}`,
	];
	if (selection.kind === "new") {
		body.push(`Checkout:     NEW "${selection.name}"`);
		body.push(plan.baseIds.length > 1 ? "Base ids:" : `Base id:      ${plan.baseIds[0] ?? "(none)"}`);
		if (plan.baseIds.length > 1) for (const id of plan.baseIds) body.push(`  ${id}`);
	} else {
		body.push(`Checkout:     EXISTING "${selection.checkout.name}"${selection.recoveryAcknowledged ? " (recovery acknowledged)" : ""}`);
		body.push("Base ids:     (existing checkout — no new base)");
	}
	body.push(`Destination:  ${plan.destination}`, `Action:       ${actionLabel(summary.action)}`);
	if (summary.action !== "checkout") {
		body.push(`omp profile:  ${summary.profile}`);
		const snapshot = summary.snapshot;
		if (snapshot) {
			body.push(
				snapshot.partialReasons.length > 0
					? `Snapshot:     PARTIAL — ${snapshot.partialReasons.join("; ")}`
					: "Snapshot:     complete",
			);
		}
		body.push(
			"",
			`Jira issue data (description and comments) will be sent to the model provider of omp profile "${summary.profile}".`,
			"The handoff file stays on private disk and may also be stored in the omp session.",
		);
		if (summary.instruction !== undefined) body.push("", "Task instruction:", summary.instruction);
	}
	body.push("", "Nothing has been created yet. Enter confirms; Esc goes back.");
	return body;
}

export function launchResultBody(result: LaunchResult, checkoutPath: string, snapshotPath: string): string[] {
	const body = [`Checkout:     ${checkoutPath}`, `Handoff file: ${snapshotPath}`, ""];
	switch (result.state) {
		case "requested":
			body.push("터미널 실행 요청됨 (terminal launch requested).");
			if (result.id !== undefined) body.push(`Terminal id: ${result.id}`);
			if (result.warning !== undefined) body.push(`Warning: ${result.warning}`);
			body.push("This only confirms the multiplexer accepted the request; it does not prove omp or the model is ready.");
			return body;
		case "manual":
		case "unknown":
			body.push(
				result.state === "manual"
					? "Automatic launch unavailable — run this yourself:"
					: "Launch result UNKNOWN — a pane/tab may or may not have been created. Inspect your terminal before retrying.",
				`Reason: ${result.reason}`,
				"",
				"Command (POSIX shell, display only):",
				`cd ${posixQuote([result.cwd])} && ${posixQuote(result.argv)}`,
				"",
				`cwd:  ${result.cwd}`,
				`argv: ${JSON.stringify(result.argv)}`,
			);
			return body;
	}
}

/** Ensure select labels are unique so a returned label maps back to one value. */
export function uniqueLabels(labels: readonly string[]): string[] {
	const seen = new Map<string, number>();
	return labels.map(label => {
		const count = (seen.get(label) ?? 0) + 1;
		seen.set(label, count);
		return count === 1 ? label : `${label} (${count})`;
	});
}
