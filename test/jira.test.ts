import { afterEach, describe, expect, test } from "bun:test";
import { parseConfig } from "../src/config.ts";
import type { Config, JiraPort } from "../src/contracts.ts";
import { createJiraClient } from "../src/jira.ts";
import { run } from "../src/process.ts";
import { isRecord, sanitizeForTerminal, toPlainText } from "../src/text.ts";

const TOKEN = "tok-SECRET-12345";
const EMAIL = "you@example.com";

function bodyField(body: string, name: string): unknown {
	const parsed: unknown = JSON.parse(body);
	return isRecord(parsed) ? parsed[name] : undefined;
}

type Recorded = { method: string; path: string; search: string; authorization: string | null; body: string };
type Handler = (req: Request, url: URL, body: string) => Response | Promise<Response>;

const stops: (() => void)[] = [];

afterEach(() => {
	for (const stop of stops.splice(0)) stop();
});

function serve(handler: Handler) {
	const requests: Recorded[] = [];
	const server = Bun.serve({
		port: 0,
		hostname: "127.0.0.1",
		async fetch(req) {
			const url = new URL(req.url);
			const body = await req.text();
			requests.push({
				method: req.method,
				path: url.pathname,
				search: url.search,
				authorization: req.headers.get("authorization"),
				body,
			});
			return handler(req, url, body);
		},
	});
	stops.push(() => server.stop(true));
	return { url: `http://127.0.0.1:${server.port}`, requests };
}

function client(baseUrl: string, patch: Record<string, unknown> = {}): { jira: JiraPort; config: Config } {
	const config = parseConfig({
		jira: {
			baseUrl,
			deployment: "cloud",
			auth: "basic",
			email: EMAIL,
			tokenCommand: ["printf", "%s\n", TOKEN],
			timeoutMs: 2000,
			...patch,
		},
	});
	return { jira: createJiraClient(config, run, fetch), config };
}

function issue(key: string, description: unknown = null) {
	return {
		key,
		fields: {
			summary: `Summary ${key}`,
			status: { id: "1", name: "To Do" },
			issuetype: { name: "Task" },
			assignee: { displayName: "Kim" },
			updated: "2026-10-01T00:00:00.000+0000",
			description,
		},
	};
}

const signal = () => new AbortController().signal;

describe("Cloud search", () => {
	test("follows two token pages and returns the exact issue set", async () => {
		const server = serve((_req, _url, body) => {
			if (bodyField(body, "nextPageToken") === undefined) {
				return Response.json({ issues: [issue("A-1"), issue("A-2")], nextPageToken: "p2", isLast: false });
			}
			return Response.json({ issues: [issue("A-3")], isLast: true });
		});
		const { jira } = client(server.url);
		const page = await jira.search("project = A", signal());
		expect(page.items.map((i) => i.key)).toEqual(["A-1", "A-2", "A-3"]);
		expect(page.hasMore).toBe(false);
		expect(page.items[0]?.url).toBe(`${server.url}/browse/A-1`);
		expect(server.requests.every((r) => r.method === "POST" && r.path === "/rest/api/3/search/jql")).toBe(true);
		expect(bodyField(server.requests[0]?.body ?? "{}", "fields")).toEqual([
			"summary",
			"status",
			"issuetype",
			"assignee",
			"updated",
			"description",
		]);
	});

	test("cap reached reports hasMore", async () => {
		let n = 0;
		const server = serve(() => {
			n++;
			return Response.json({ issues: [issue(`A-${2 * n - 1}`), issue(`A-${2 * n}`)], nextPageToken: `t${n}` });
		});
		const { jira } = client(server.url, { maxResults: 2 });
		const page = await jira.search("x", signal());
		expect(page.items).toHaveLength(2);
		expect(page.hasMore).toBe(true);
		expect(server.requests).toHaveLength(1);
	});

	test("repeated token rejects instead of returning a partial list", async () => {
		let n = 0;
		const server = serve(() => {
			n++;
			return Response.json({ issues: [issue(`A-${n}`)], nextPageToken: "same" });
		});
		const { jira } = client(server.url);
		await expect(jira.search("x", signal())).rejects.toThrow("repeated");
	});

	test("duplicate key across pages rejects", async () => {
		const server = serve((_req, _url, body) => {
			const first = bodyField(body, "nextPageToken") === undefined;
			return Response.json(first ? { issues: [issue("A-1")], nextPageToken: "p2" } : { issues: [issue("A-1")], isLast: true });
		});
		const { jira } = client(server.url);
		await expect(jira.search("x", signal())).rejects.toThrow("duplicate");
	});

	test("later-page 500 rejects", async () => {
		const server = serve((_req, _url, body) => {
			const first = bodyField(body, "nextPageToken") === undefined;
			return first
				? Response.json({ issues: [issue("A-1")], nextPageToken: "p2" })
				: Response.json({ errorMessages: ["boom"] }, { status: 500 });
		});
		const { jira } = client(server.url);
		await expect(jira.search("x", signal())).rejects.toThrow("HTTP 500");
	});
});

describe("DC search", () => {
	test("follows two offset pages", async () => {
		const server = serve((_req, _url, body) => {
			const startAt = bodyField(body, "startAt");
			const issues = startAt === 0 ? [issue("D-1"), issue("D-2")] : [issue("D-3")];
			return Response.json({ startAt, total: 3, issues });
		});
		const { jira } = client(server.url, { deployment: "dc" });
		const page = await jira.search("x", signal());
		expect(page.items.map((i) => i.key)).toEqual(["D-1", "D-2", "D-3"]);
		expect(page).toMatchObject({ total: 3, hasMore: false });
		expect(server.requests.map((r) => r.path)).toEqual(["/rest/api/2/search", "/rest/api/2/search"]);
	});

	test("empty page before total rejects", async () => {
		const server = serve((_req, _url, body) => {
			const startAt = bodyField(body, "startAt");
			return Response.json({ startAt, total: 5, issues: startAt === 0 ? [issue("D-1")] : [] });
		});
		const { jira } = client(server.url, { deployment: "dc" });
		await expect(jira.search("x", signal())).rejects.toThrow("empty page");
	});
});

describe("security boundaries", () => {
	test("redirect is refused and the target never sees Authorization", async () => {
		const target = serve(() => Response.json({ issues: [] }));
		const origin = serve(() => new Response(null, { status: 302, headers: { Location: `${target.url}/steal` } }));
		const { jira } = client(origin.url);
		await expect(jira.search("x", signal())).rejects.toThrow();
		expect(target.requests).toHaveLength(0);
	});

	test("token only appears in the Authorization header", async () => {
		const server = serve(() => Response.json({ issues: [issue("A-1")], isLast: true }));
		const { jira } = client(server.url);
		await jira.search("x", signal());
		const req = server.requests[0];
		expect(req?.authorization).toBe(`Basic ${Buffer.from(`${EMAIL}:${TOKEN}`).toString("base64")}`);
		expect(req?.body).not.toContain(TOKEN);
		expect(req?.search).not.toContain(TOKEN);
	});

	test("bearer auth sends the raw token", async () => {
		const server = serve(() => Response.json({ issues: [], isLast: true }));
		const { jira } = client(server.url, { auth: "bearer", email: undefined });
		await jira.search("x", signal());
		expect(server.requests[0]?.authorization).toBe(`Bearer ${TOKEN}`);
	});

	test("tokenCommand failure hides its output and sends nothing", async () => {
		const server = serve(() => Response.json({ issues: [] }));
		const { jira } = client(server.url, { tokenCommand: ["sh", "-c", "echo SECRET-OUT; echo SECRET-ERR >&2; exit 1"] });
		const error = await jira.search("x", signal()).then(
			() => undefined,
			(e: unknown) => e,
		);
		expect(error).toBeInstanceOf(Error);
		expect(String(error instanceof Error ? error.message : error)).not.toContain("SECRET");
		expect(server.requests).toHaveLength(0);
	});

	test("server error echoing the token is redacted and truncated", async () => {
		const server = serve(() =>
			Response.json({ errorMessages: [`bad ${TOKEN} \u001b[31mred\u001b[0m ${"x".repeat(1000)}`] }, { status: 400 }),
		);
		const { jira } = client(server.url);
		const error = await jira.issue("a-1", signal()).then(
			() => undefined,
			(e: unknown) => (e instanceof Error ? e.message : ""),
		);
		expect(error).toBeString();
		expect(error).not.toContain(TOKEN);
		expect(error).not.toContain("\u001b");
		expect(error?.length ?? 0).toBeLessThan(500);
	});

	describe("server error echoing the basic credential is redacted", () => {
		// Chosen so the base64 payload contains "/" (exercises the JSON `\/` escape form).
		const token = "k??~~?>";
		const pair = `${EMAIL}:${token}`;
		const b64 = Buffer.from(pair).toString("base64");
		const cases: [string, string, number][] = [
			["Authorization header value", `Basic ${b64}`, 400],
			["base64 payload alone", b64, 500],
			["JSON-escaped base64 with \\/", b64.replaceAll("/", "\\/"), 400],
			["email:token", pair, 500],
		];
		for (const [label, echoed, status] of cases) {
			test(label, async () => {
				expect(b64).toContain("/");
				const server = serve(
					() =>
						new Response(`{"errorMessages":["${"y".repeat(290)} echo ${echoed} end"]}`, {
							status,
							headers: { "content-type": "application/json" },
						}),
				);
				const { jira } = client(server.url, { tokenCommand: ["printf", "%s\n", token] });
				const error = await jira.issue("a-1", signal()).then(
					() => undefined,
					(e: unknown) => (e instanceof Error ? e.message : ""),
				);
				expect(error).toBeString();
				for (const secret of [token, b64, b64.slice(0, 12), pair, b64.replaceAll("/", "\\/")]) {
					expect(error).not.toContain(secret);
				}
			});
		}
	});

	test("401, 404 and HTML responses are distinguished", async () => {
		let status = 401;
		const server = serve(() =>
			status === 200
				? new Response("<!DOCTYPE html><html></html>", { headers: { "content-type": "text/html" } })
				: Response.json({}, { status }),
		);
		const { jira } = client(server.url);
		await expect(jira.issue("A-1", signal())).rejects.toThrow("401");
		status = 404;
		await expect(jira.issue("A-1", signal())).rejects.toThrow("not found or not visible");
		status = 200;
		await expect(jira.issue("A-1", signal())).rejects.toThrow("HTML");
	});
});

describe("issue, comments, transitions", () => {
	test("issue normalizes key and converts ADF description", async () => {
		const adf = {
			type: "doc",
			content: [
				{ type: "paragraph", content: [{ type: "text", text: "한글 \u001b[31m설명\u001b[0m\u0007" }] },
				{
					type: "bulletList",
					content: [{ type: "listItem", content: [{ type: "paragraph", content: [{ type: "text", text: "항목" }] }] }],
				},
			],
		};
		const server = serve(() => Response.json(issue("ABC-7", adf)));
		const { jira } = client(server.url);
		const result = await jira.issue("abc-7", signal());
		expect(result.key).toBe("ABC-7");
		expect(result.description).toBe("한글 설명\n- 항목");
		expect(server.requests[0]?.path).toBe("/rest/api/3/issue/ABC-7");
	});

	test("issue rejects a mismatched returned key", async () => {
		const server = serve(() => Response.json(issue("ABC-8")));
		const { jira } = client(server.url);
		await expect(jira.issue("ABC-7", signal())).rejects.toThrow("ABC-8");
	});

	test("comments: limit 0 makes no request; pages are sorted newest first", async () => {
		const all = [
			{ id: "1", author: { displayName: "a" }, created: "2026-01-01T00:00:00.000+0000", body: "old" },
			{ id: "3", author: { displayName: "b" }, created: "2026-03-01T00:00:00.000+0000", body: "new" },
			{ id: "2", author: { displayName: "c" }, created: "2026-02-01T00:00:00.000+0000", body: "mid" },
		];
		const server = serve((_req, url) => {
			const startAt = Number(url.searchParams.get("startAt"));
			const max = Number(url.searchParams.get("maxResults"));
			return Response.json({ startAt, maxResults: max, total: all.length, comments: all.slice(startAt, startAt + max) });
		});
		const { jira } = client(server.url);
		expect(await jira.comments("A-1", 0, signal())).toEqual({ items: [], hasMore: false });
		expect(server.requests).toHaveLength(0);
		const full = await jira.comments("A-1", 10, signal());
		expect(full.items.map((c) => c.id)).toEqual(["3", "2", "1"]);
		expect(full.hasMore).toBe(false);
		const partial = await jira.comments("A-1", 2, signal());
		expect(partial.items).toHaveLength(2);
		expect(partial.hasMore).toBe(true);
	});

	test("transitions map screen/field requirements", async () => {
		const server = serve(() =>
			Response.json({
				transitions: [
					{ id: "31", name: "Start", to: { id: "3", name: "In Progress" }, hasScreen: false, isAvailable: true },
					{
						id: "41",
						name: "Resolve",
						to: { id: "5", name: "Done" },
						hasScreen: false,
						isAvailable: false,
						fields: { resolution: { required: true } },
					},
				],
			}),
		);
		const { jira } = client(server.url, { deployment: "dc" });
		const list = await jira.transitions("A-1", signal());
		expect(list).toEqual([
			{ id: "31", name: "Start", to: { id: "3", name: "In Progress" }, hasScreen: false, requiresFields: false, isAvailable: true },
			{ id: "41", name: "Resolve", to: { id: "5", name: "Done" }, hasScreen: false, requiresFields: true, isAvailable: false },
		]);
		expect(server.requests[0]?.search).toBe("?expand=transitions.fields");
	});

	test("postTransition classifies 204, 400 and 500", async () => {
		let status = 204;
		const server = serve(() =>
			status === 204 ? new Response(null, { status }) : Response.json({ errorMessages: ["nope"] }, { status }),
		);
		const { jira } = client(server.url);
		expect(await jira.postTransition("A-1", "31", signal())).toEqual({ state: "acknowledged", verified: false });
		expect(JSON.parse(server.requests[0]?.body ?? "{}")).toEqual({ transition: { id: "31" } });
		status = 400;
		expect((await jira.postTransition("A-1", "31", signal())).state).toBe("rejected");
		status = 500;
		expect((await jira.postTransition("A-1", "31", signal())).state).toBe("unknown");
		expect(server.requests).toHaveLength(3);
	});

	test("postTransition timeout after dispatch is unknown", async () => {
		// The client's own per-request timeout (real AbortSignal.timeout, minimum 1000 ms) is under test;
		// the server holds the response until the client has given up, then releases it.
		const hold = Promise.withResolvers<void>();
		const server = serve(async () => {
			await hold.promise;
			return new Response(null, { status: 204 });
		});
		const { jira } = client(server.url, { timeoutMs: 1000 });
		const result = await jira.postTransition("A-1", "31", signal());
		hold.resolve();
		expect(result.state).toBe("unknown");
		expect(server.requests).toHaveLength(1);
	});
});

describe("text", () => {
	test("sanitizeForTerminal strips ANSI, OSC and control characters", () => {
		const input = "a\u001b[1;31mb\u001b]8;;http://x\u0007link\u001b]8;;\u0007\u0000\u007f\u0085c\td\r\ne";
		expect(sanitizeForTerminal(input)).toBe("ablinkc  d\ne");
	});

	test("toPlainText reads v2 strings and ADF mentions, cards and breaks", () => {
		expect(toPlainText("plain v2")).toBe("plain v2");
		expect(toPlainText(null)).toBe("");
		const adf = {
			type: "doc",
			content: [
				{
					type: "paragraph",
					content: [
						{ type: "mention", attrs: { text: "@홍길동" } },
						{ type: "text", text: " 확인" },
						{ type: "hardBreak" },
						{ type: "inlineCard", attrs: { url: "https://x.test/1" } },
					],
				},
				{ type: "unknownPanel", content: [{ type: "paragraph", content: [{ type: "text", text: "안쪽" }] }] },
				{
					type: "orderedList",
					content: [
						{ type: "listItem", content: [{ type: "paragraph", content: [{ type: "text", text: "하나" }] }] },
						{ type: "listItem", content: [{ type: "paragraph", content: [{ type: "text", text: "둘" }] }] },
					],
				},
			],
		};
		expect(toPlainText(adf)).toBe("@홍길동 확인\nhttps://x.test/1\n안쪽\n1. 하나\n2. 둘");
	});
});
