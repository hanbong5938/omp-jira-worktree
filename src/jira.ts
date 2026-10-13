/**
 * Jira REST client (Cloud v3 / Data Center v2). Credentials come only from the
 * configured tokenCommand argv; requests never follow redirects, never retry,
 * and read at most 2 MiB per response.
 */
import {
	type Comment,
	type Config,
	type Issue,
	type JiraPort,
	normalizeIssueKey,
	type Page,
	RUN_TIMEOUT_MS,
	type Run,
	type RunResult,
	type StatusRef,
	type Transition,
	type TransitionResult,
} from "./contracts.ts";
import { isRecord, sanitizeForTerminal, toPlainText } from "./text.ts";

const FIELDS = ["summary", "status", "issuetype", "assignee", "updated", "description"] as const;
const PAGE_SIZE = 50;
const MAX_BODY_BYTES = 2 * 1024 * 1024;
const MAX_SERVER_MESSAGE = 300;

export type JiraErrorKind =
	| "credential"
	| "unauthorized"
	| "forbidden"
	| "not-found"
	| "rate-limited"
	| "http"
	| "html"
	| "invalid-json"
	| "too-large"
	| "timeout"
	| "aborted"
	| "network"
	| "protocol";

/** Error raised by the Jira client. Messages never contain credentials or tokenCommand output. */
export class JiraError extends Error {
	readonly kind: JiraErrorKind;
	readonly status: number | undefined;
	constructor(kind: JiraErrorKind, message: string, status?: number) {
		super(message);
		this.name = "JiraError";
		this.kind = kind;
		this.status = status;
	}
}

/** Every textual form a credential could be echoed as: raw, JSON-escaped, and with `/` escaped as `\/`. */
function credentialForms(header: string, token: string, email: string | undefined): string[] {
	const bases = [header, token];
	const space = header.indexOf(" ");
	if (space > 0) bases.push(header.slice(space + 1));
	if (email !== undefined) bases.push(`${email}:${token}`);
	const forms = new Set<string>();
	for (const base of bases) {
		if (base.length === 0) continue;
		const escaped = JSON.stringify(base).slice(1, -1);
		for (const form of [base, escaped, base.replaceAll("/", "\\/"), escaped.replaceAll("/", "\\/")]) forms.add(form);
	}
	// Longest first so a full header is replaced before its payload or token substring.
	return [...forms].sort((a, b) => b.length - a.length);
}

/** Redact every credential form from a full response body (before sanitizing or truncating it). */
function redactCredential(text: string, header: string, token: string, email: string | undefined): string {
	let out = text;
	for (const form of credentialForms(header, token, email)) {
		if (out.includes(form)) out = out.split(form).join("[redacted]");
	}
	return out;
}

/** Strip control/escape sequences from server-provided text and cap it at 300 characters. */
export function sanitizeServerMessage(text: string): string {
	const clean = sanitizeForTerminal(text).replace(/\s+/g, " ").trim();
	return clean.length > MAX_SERVER_MESSAGE ? `${clean.slice(0, MAX_SERVER_MESSAGE - 1)}…` : clean;
}

function protocol(message: string): JiraError {
	return new JiraError("protocol", `Jira protocol error: ${message}`);
}

type RawResponse = { status: number; contentType: string; text: string };

async function readCapped(response: Response): Promise<string> {
	const body = response.body;
	if (body === null) return "";
	const reader = body.getReader();
	const chunks: Uint8Array[] = [];
	let size = 0;
	for (;;) {
		const { done, value } = await reader.read();
		if (done) break;
		size += value.byteLength;
		if (size > MAX_BODY_BYTES) {
			await reader.cancel().catch(() => undefined);
			throw new JiraError("too-large", "Jira response exceeded the 2 MiB limit");
		}
		chunks.push(value);
	}
	return new TextDecoder().decode(Buffer.concat(chunks));
}

/** Extract Jira's `errorMessages`/`errors` (or a short text body) for display. */
function serverMessage(raw: RawResponse): string {
	if (raw.text.length === 0) return "";
	try {
		const parsed: unknown = JSON.parse(raw.text);
		if (isRecord(parsed)) {
			const parts: string[] = [];
			if (Array.isArray(parsed.errorMessages)) {
				for (const m of parsed.errorMessages) if (typeof m === "string") parts.push(m);
			}
			if (isRecord(parsed.errors)) {
				for (const [field, m] of Object.entries(parsed.errors)) if (typeof m === "string") parts.push(`${field}: ${m}`);
			}
			if (typeof parsed.message === "string") parts.push(parsed.message);
			return sanitizeServerMessage(parts.join("; "));
		}
	} catch {
		// Non-JSON body: fall through to a short text excerpt unless it is HTML.
	}
	if (isHtml(raw)) return "";
	return sanitizeServerMessage(raw.text);
}

function isHtml(raw: RawResponse): boolean {
	return raw.contentType.includes("text/html") || /^\s*<(?:!doctype|html)/i.test(raw.text);
}

function statusError(raw: RawResponse, what: string): JiraError {
	const detail = serverMessage(raw);
	const suffix = detail.length > 0 ? `: ${detail}` : "";
	switch (raw.status) {
		case 401:
			return new JiraError(
				"unauthorized",
				`Jira authentication failed (401) for ${what}; check jira.email/jira.auth and the tokenCommand credential${suffix}`,
				401,
			);
		case 403:
			return new JiraError("forbidden", `Jira permission denied (403) for ${what}${suffix}`, 403);
		case 404:
			return new JiraError(
				"not-found",
				`Jira returned 404 for ${what}: not found or not visible to this account, or wrong baseUrl/deployment${suffix}`,
				404,
			);
		case 429:
			return new JiraError("rate-limited", `Jira rate limit reached (429) for ${what}; try again later${suffix}`, 429);
		default:
			if (isHtml(raw)) {
				return new JiraError("html", `Jira returned an HTML page (HTTP ${raw.status}) for ${what}; check jira.baseUrl`, raw.status);
			}
			return new JiraError("http", `Jira request failed (HTTP ${raw.status}) for ${what}${suffix}`, raw.status);
	}
}

function parseJson(raw: RawResponse, what: string): unknown {
	if (isHtml(raw)) {
		throw new JiraError("html", `Jira returned an HTML page instead of JSON for ${what}; check jira.baseUrl`, raw.status);
	}
	try {
		return JSON.parse(raw.text);
	} catch {
		throw new JiraError("invalid-json", `Jira returned invalid JSON for ${what}`, raw.status);
	}
}

function str(value: unknown, field: string): string {
	if (typeof value !== "string") throw protocol(`missing or invalid ${field}`);
	return sanitizeForTerminal(value);
}

function statusRef(value: unknown, field: string): StatusRef {
	if (!isRecord(value)) throw protocol(`missing or invalid ${field}`);
	return { id: str(value.id, `${field}.id`), name: str(value.name, `${field}.name`) };
}

function optionalTotal(value: unknown): number | undefined {
	return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : undefined;
}

function parseIssue(value: unknown, baseUrl: string): Issue {
	if (!isRecord(value)) throw protocol("issue entry is not an object");
	const key = normalizeIssueKey(str(value.key, "issue key"));
	const fields = value.fields;
	if (!isRecord(fields)) throw protocol(`issue ${key} has no fields object`);
	const issueType = fields.issuetype;
	if (!isRecord(issueType)) throw protocol(`issue ${key} has no issuetype`);
	const assignee = fields.assignee;
	let assigneeName = "";
	if (assignee !== null && assignee !== undefined) {
		if (!isRecord(assignee)) throw protocol(`issue ${key} has an invalid assignee`);
		assigneeName = typeof assignee.displayName === "string" ? sanitizeForTerminal(assignee.displayName) : "";
	}
	return {
		key,
		summary: str(fields.summary, `${key} summary`),
		status: statusRef(fields.status, `${key} status`),
		issueType: str(issueType.name, `${key} issuetype.name`),
		assignee: assigneeName,
		updated: str(fields.updated, `${key} updated`),
		description: sanitizeForTerminal(toPlainText(fields.description)),
		url: `${baseUrl}/browse/${key}`,
	};
}

function parseComment(value: unknown): Comment {
	if (!isRecord(value)) throw protocol("comment entry is not an object");
	const id = str(value.id, "comment id");
	const author = isRecord(value.author) && typeof value.author.displayName === "string" ? value.author.displayName : "";
	return {
		id,
		author: sanitizeForTerminal(author),
		created: str(value.created, `comment ${id} created`),
		body: sanitizeForTerminal(toPlainText(value.body)),
	};
}

function parseTransition(value: unknown): Transition {
	if (!isRecord(value)) throw protocol("transition entry is not an object");
	const id = str(value.id, "transition id");
	let requiresFields = false;
	if (value.fields !== undefined) {
		if (!isRecord(value.fields)) throw protocol(`transition ${id} has invalid fields`);
		requiresFields = Object.values(value.fields).some((field) => isRecord(field) && field.required === true);
	}
	const transition: Transition = {
		id,
		name: str(value.name, `transition ${id} name`),
		to: statusRef(value.to, `transition ${id} to`),
		// A missing hasScreen is treated as "has a screen" so the UI routes to Jira web instead of POSTing blind.
		hasScreen: value.hasScreen !== false,
		requiresFields,
	};
	if (typeof value.isAvailable === "boolean") transition.isAvailable = value.isAvailable;
	return transition;
}

/** Newest `created` first; equal timestamps ordered by id (numeric when both are numeric), descending. */
function compareCommentsNewestFirst(a: Comment, b: Comment): number {
	const ta = Date.parse(a.created);
	const tb = Date.parse(b.created);
	if (!Number.isNaN(ta) && !Number.isNaN(tb) && ta !== tb) return tb - ta;
	if (Number.isNaN(ta) || Number.isNaN(tb)) {
		if (a.created !== b.created) return a.created < b.created ? 1 : -1;
	}
	if (/^\d+$/.test(a.id) && /^\d+$/.test(b.id)) {
		const ia = BigInt(a.id);
		const ib = BigInt(b.id);
		return ia === ib ? 0 : ia < ib ? 1 : -1;
	}
	return a.id === b.id ? 0 : a.id < b.id ? 1 : -1;
}

export function createJiraClient(config: Config, run: Run, fetchImpl: typeof fetch): JiraPort {
	const { baseUrl, deployment, auth, email, timeoutMs, maxResults } = config.jira;
	const apiBase = `${baseUrl}/rest/api/${deployment === "cloud" ? "3" : "2"}`;
	let credential: Promise<{ header: string; token: string }> | undefined;

	async function loadCredential(): Promise<{ header: string; token: string }> {
		const [file = "", ...args] = config.jira.tokenCommand;
		let result: RunResult;
		try {
			result = await run(file, args, { timeoutMs: RUN_TIMEOUT_MS });
		} catch {
			throw new JiraError("credential", `Jira credential command could not be started (jira.tokenCommand[0] = ${JSON.stringify(file)})`);
		}
		if (result.killed) throw new JiraError("credential", "Jira credential command timed out or was killed");
		if (result.code !== 0) {
			throw new JiraError("credential", `Jira credential command failed with exit code ${result.code}; output withheld`);
		}
		const token = result.stdout.endsWith("\n") ? result.stdout.slice(0, -1) : result.stdout;
		if (token.length === 0) throw new JiraError("credential", "Jira credential command printed an empty token");
		if (/[\r\n]/.test(token)) throw new JiraError("credential", "Jira credential command printed more than one line; output withheld");
		const header =
			auth === "basic" ? `Basic ${Buffer.from(`${email ?? ""}:${token}`).toString("base64")}` : `Bearer ${token}`;
		return { header, token };
	}

	/** Lazily resolve and cache the credential; a failure is not cached so `r` can retry it explicitly. */
	function getCredential(): Promise<{ header: string; token: string }> {
		if (credential === undefined) {
			const pending = loadCredential();
			credential = pending;
			pending.catch(() => {
				if (credential === pending) credential = undefined;
			});
		}
		return credential;
	}

	type Sent = { kind: "response"; raw: RawResponse } | { kind: "failed"; dispatched: boolean; error: JiraError };

	/** One request, no retry. Returns a classified failure instead of throwing so POST callers can tell dispatch state. */
	async function send(
		method: "GET" | "POST",
		path: string,
		body: unknown,
		signal: AbortSignal,
		what: string,
	): Promise<Sent> {
		let cred: { header: string; token: string };
		try {
			cred = await getCredential();
		} catch (error) {
			return {
				kind: "failed",
				dispatched: false,
				error: error instanceof JiraError ? error : new JiraError("credential", "Jira credential unavailable"),
			};
		}
		if (signal.aborted) {
			return { kind: "failed", dispatched: false, error: new JiraError("aborted", `Jira request cancelled: ${what}`) };
		}
		const timeout = AbortSignal.timeout(timeoutMs);
		const combined = AbortSignal.any([signal, timeout]);
		const headers: Record<string, string> = { Accept: "application/json", Authorization: cred.header };
		if (body !== undefined) headers["Content-Type"] = "application/json";
		const classify = (): JiraError => {
			if (timeout.aborted) return new JiraError("timeout", `Jira request timed out after ${timeoutMs} ms: ${what}`);
			if (signal.aborted) return new JiraError("aborted", `Jira request cancelled: ${what}`);
			return new JiraError("network", `Jira request failed (network error or refused redirect): ${what}`);
		};
		let response: Response;
		try {
			response = await fetchImpl(`${apiBase}${path}`, {
				method,
				headers,
				body: body === undefined ? undefined : JSON.stringify(body),
				redirect: "error",
				signal: combined,
			});
		} catch {
			return { kind: "failed", dispatched: true, error: classify() };
		}
		try {
			const text = await readCapped(response);
			const redacted = redactCredential(text, cred.header, cred.token, email ?? undefined);
			return {
				kind: "response",
				raw: { status: response.status, contentType: response.headers.get("content-type") ?? "", text: redacted },
			};
		} catch (error) {
			return { kind: "failed", dispatched: true, error: error instanceof JiraError ? error : classify() };
		}
	}

	async function requestJson(method: "GET" | "POST", path: string, body: unknown, signal: AbortSignal, what: string) {
		const sent = await send(method, path, body, signal, what);
		if (sent.kind === "failed") throw sent.error;
		if (sent.raw.status < 200 || sent.raw.status >= 300) throw statusError(sent.raw, what);
		return parseJson(sent.raw, what);
	}

	async function searchCloud(jql: string, signal: AbortSignal): Promise<Page<Issue>> {
		const items: Issue[] = [];
		const keys = new Set<string>();
		const tokens = new Set<string>();
		let token: string | undefined;
		let total: number | undefined;
		for (let requests = 0; ; requests++) {
			if (requests > maxResults) throw protocol("search exceeded the request budget without reaching the result cap");
			const pageSize = Math.min(PAGE_SIZE, maxResults - items.length);
			const body: Record<string, unknown> = { jql, fields: [...FIELDS], maxResults: pageSize };
			if (token !== undefined) body.nextPageToken = token;
			const data = await requestJson("POST", "/search/jql", body, signal, "issue search");
			if (!isRecord(data) || !Array.isArray(data.issues)) throw protocol("search response has no issues array");
			if (data.issues.length > pageSize) throw protocol("search page returned more issues than requested");
			for (const entry of data.issues) {
				const issue = parseIssue(entry, baseUrl);
				if (keys.has(issue.key)) throw protocol(`duplicate issue ${issue.key} across search pages`);
				keys.add(issue.key);
				items.push(issue);
			}
			total = optionalTotal(data.total);
			if (total !== undefined && items.length > total) throw protocol("search returned more issues than its total");
			const next = data.nextPageToken;
			if (next !== undefined && next !== null && (typeof next !== "string" || next.length === 0)) {
				throw protocol("invalid nextPageToken");
			}
			const nextToken = typeof next === "string" ? next : undefined;
			if (data.isLast === true && nextToken !== undefined) throw protocol("isLast is true but a nextPageToken was returned");
			if (data.isLast === false && nextToken === undefined) throw protocol("isLast is false but no nextPageToken was returned");
			if (nextToken === undefined) {
				if (total !== undefined && items.length < total) throw protocol("search ended before reaching its total");
				return { items, total, hasMore: false };
			}
			if (nextToken === token || tokens.has(nextToken)) throw protocol("search repeated a nextPageToken");
			tokens.add(nextToken);
			token = nextToken;
			if (items.length >= maxResults) return { items, total, hasMore: true };
		}
	}

	async function searchDc(jql: string, signal: AbortSignal): Promise<Page<Issue>> {
		const items: Issue[] = [];
		const keys = new Set<string>();
		let total: number | undefined;
		for (let requests = 0; ; requests++) {
			if (requests > maxResults) throw protocol("search exceeded the request budget without reaching the result cap");
			const startAt = items.length;
			const pageSize = Math.min(PAGE_SIZE, maxResults - items.length);
			const data = await requestJson(
				"POST",
				"/search",
				{ jql, fields: [...FIELDS], maxResults: pageSize, startAt },
				signal,
				"issue search",
			);
			if (!isRecord(data) || !Array.isArray(data.issues)) throw protocol("search response has no issues array");
			if (data.startAt !== startAt) throw protocol(`search page startAt ${String(data.startAt)} does not match ${startAt}`);
			if (data.issues.length > pageSize) throw protocol("search page returned more issues than requested");
			total = optionalTotal(data.total);
			if (total === undefined) throw protocol("search response has no total");
			for (const entry of data.issues) {
				const issue = parseIssue(entry, baseUrl);
				if (keys.has(issue.key)) throw protocol(`duplicate issue ${issue.key} across search pages`);
				keys.add(issue.key);
				items.push(issue);
			}
			if (items.length > total) throw protocol("search returned more issues than its total");
			if (items.length === total) return { items, total, hasMore: false };
			if (data.issues.length === 0) throw protocol("search returned an empty page before reaching its total");
			if (items.length >= maxResults) return { items, total, hasMore: true };
		}
	}

	return {
		search(jql, signal) {
			return deployment === "cloud" ? searchCloud(jql, signal) : searchDc(jql, signal);
		},

		async issue(rawKey, signal) {
			const key = normalizeIssueKey(rawKey);
			const data = await requestJson(
				"GET",
				`/issue/${encodeURIComponent(key)}?fields=${FIELDS.join(",")}`,
				undefined,
				signal,
				`issue ${key}`,
			);
			const issue = parseIssue(data, baseUrl);
			if (issue.key !== key) throw protocol(`requested ${key} but Jira returned ${issue.key}`);
			return issue;
		},

		/**
		 * Up to `limit` comments, sorted newest `created` first (ties by id). `limit` 0 returns an empty,
		 * complete page without a request. Whenever not every comment was fetched (`total` > items),
		 * `hasMore` is true: the fetched set is then not guaranteed to be the newest N (the server may
		 * ignore `orderBy=-created`), so consumers must label it "조회한 댓글, 일부".
		 */
		async comments(rawKey, limit, signal) {
			const key = normalizeIssueKey(rawKey);
			if (!Number.isSafeInteger(limit) || limit < 0) throw new RangeError("comment limit must be a non-negative integer");
			if (limit === 0) return { items: [], hasMore: false };
			const items: Comment[] = [];
			const ids = new Set<string>();
			let total: number | undefined;
			for (let requests = 0; ; requests++) {
				if (requests > limit) throw protocol("comment paging exceeded the request budget");
				const startAt = items.length;
				const pageSize = Math.min(PAGE_SIZE, limit - items.length);
				const data = await requestJson(
					"GET",
					`/issue/${encodeURIComponent(key)}/comment?orderBy=-created&startAt=${startAt}&maxResults=${pageSize}`,
					undefined,
					signal,
					`comments of ${key}`,
				);
				if (!isRecord(data) || !Array.isArray(data.comments)) throw protocol("comment response has no comments array");
				if (data.startAt !== undefined && data.startAt !== startAt) {
					throw protocol(`comment page startAt ${String(data.startAt)} does not match ${startAt}`);
				}
				if (data.comments.length > pageSize) throw protocol("comment page returned more comments than requested");
				total = optionalTotal(data.total);
				if (total === undefined) throw protocol("comment response has no total");
				for (const entry of data.comments) {
					const comment = parseComment(entry);
					if (ids.has(comment.id)) throw protocol(`duplicate comment ${comment.id} across pages`);
					ids.add(comment.id);
					items.push(comment);
				}
				if (items.length > total) throw protocol("comments exceeded their total");
				if (items.length === total || items.length >= limit) break;
				if (data.comments.length === 0) throw protocol("comments returned an empty page before reaching their total");
			}
			const complete = items.length === total;
			items.sort(compareCommentsNewestFirst);
			return { items, total, hasMore: !complete };
		},

		async transitions(rawKey, signal) {
			const key = normalizeIssueKey(rawKey);
			const data = await requestJson(
				"GET",
				`/issue/${encodeURIComponent(key)}/transitions?expand=transitions.fields`,
				undefined,
				signal,
				`transitions of ${key}`,
			);
			if (!isRecord(data) || !Array.isArray(data.transitions)) throw protocol("transitions response has no transitions array");
			const seen = new Set<string>();
			return data.transitions.map((entry: unknown) => {
				const transition = parseTransition(entry);
				if (seen.has(transition.id)) throw protocol(`duplicate transition ${transition.id}`);
				seen.add(transition.id);
				return transition;
			});
		},

		/**
		 * Single POST, never retried. 2xx → acknowledged (unverified; callers re-read status).
		 * Definite 4xx or failure before dispatch → rejected. 5xx/timeout/transport drop/abort after
		 * dispatch/unreadable response → unknown.
		 */
		async postTransition(rawKey, id, signal): Promise<TransitionResult> {
			const key = normalizeIssueKey(rawKey);
			if (id.length === 0) return { state: "rejected", message: "transition id is empty; nothing was sent" };
			const what = `transition ${id} of ${key}`;
			const sent = await send(
				"POST",
				`/issue/${encodeURIComponent(key)}/transitions`,
				{ transition: { id } },
				signal,
				what,
			);
			if (sent.kind === "failed") {
				if (!sent.dispatched) return { state: "rejected", message: `${sent.error.message} (request not sent)` };
				return { state: "unknown", message: `${sent.error.message}; the transition may or may not have been applied` };
			}
			const { status } = sent.raw;
			if (status >= 200 && status < 300) return { state: "acknowledged", verified: false };
			if (status >= 400 && status < 500) return { state: "rejected", message: statusError(sent.raw, what).message };
			return {
				state: "unknown",
				message: `${statusError(sent.raw, what).message}; the transition may or may not have been applied`,
			};
		},
	};
}
