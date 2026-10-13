import { describe, expect, test } from "bun:test";
import { visibleWidth } from "@oh-my-pi/pi-tui";
import { runBrowser } from "../src/browser.ts";
import type { Comment, Config, ExtensionCommandContext, Issue, Page, Services } from "../src/contracts.ts";
import {
	buildSearchJql,
	type DetailView,
	IssueListModel,
	type LoadState,
	type Paint,
	renderDetail,
	renderList,
	type SearchFn,
} from "../src/ui.ts";

const plain: Paint = { fg: (_color, text) => text, bold: text => text, inverse: text => text };

const CONFIG: Config = {
	jira: {
		baseUrl: "https://jira.example.test",
		deployment: "cloud",
		auth: "basic",
		email: "dev@example.test",
		tokenCommand: ["false"],
		maxResults: 50,
		timeoutMs: 20_000,
	},
	filters: [],
	search: { jql: 'text ~ "{query}" ORDER BY updated DESC' },
	worktree: { repos: {}, prefer: "jj", root: "/tmp/omp-jira-worktree-test", gitBase: null, jjBase: null },
	handoff: { maxComments: 50, maxDescriptionChars: 6000, maxCommentChars: 12000, maxChars: 64000 },
	omp: { binary: "omp", profile: "default" },
};

const LONG_KOREAN = "한글 요약과 이모지 😀👍🏽👨‍👩‍👧‍👦 를 포함한 아주 긴 제목입니다 ".repeat(8);
const HOSTILE =
	"\u001b[31m빨간\u001b[0m 글자 \u001b]8;;https://evil.example\u0007링크\u001b]8;;\u0007\ttab\r\nCRLF \u0007bell \u009bc1 " +
	"가".repeat(200) +
	" " +
	"x".repeat(300);

function issue(key: string, summary = LONG_KOREAN): Issue {
	return {
		key,
		summary,
		status: { id: "3", name: "진행 중 🚧 아주 긴 상태 이름" },
		issueType: "작업",
		assignee: "홍길동 (매우 긴 표시 이름 👩‍💻)",
		updated: "2026-10-01T09:30:00.000+0900",
		description: `${HOSTILE}\n\n${LONG_KOREAN}\n${"- 목록 항목 😀\n".repeat(30)}`,
		url: `https://jira.example.test/browse/${key}`,
	};
}

function page(keys: string[]): Page<Issue> {
	return { items: keys.map(key => issue(key)), hasMore: false };
}

function ready<T>(key: string, data: T): LoadState<T> {
	return { status: "ready", key, data, dataKey: key, stale: false, error: undefined };
}

const comments: Page<Comment> = {
	items: Array.from({ length: 12 }, (_, index) => ({
		id: String(index + 1),
		author: `작성자 ${index} 🙂`,
		created: "2026-10-02T10:00:00.000+0900",
		body: `${LONG_KOREAN}\n${HOSTILE}`,
	})),
	hasMore: true,
};

function detailView(focus: DetailView["focus"] = "description"): DetailView {
	return {
		key: "ABC-1",
		issue: ready("ABC-1", issue("ABC-1")),
		comments: { kind: "loaded", load: ready("ABC-1", comments) },
		focus,
		descriptionScroll: 0,
		commentsScroll: 3,
		notice: undefined,
	};
}

function expectWidthSafe(lines: readonly string[], width: number, height: number): void {
	expect(lines.length).toBeLessThanOrEqual(height);
	for (const line of lines) {
		expect(visibleWidth(line)).toBeLessThanOrEqual(width);
		// Jira-provided escape/control sequences never reach the terminal.
		expect(line).not.toMatch(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/);
	}
}

describe("search JQL", () => {
	test("escapes backslash and double quote inside the template literal", () => {
		expect(buildSearchJql(CONFIG.search.jql, 'say "hi" C:\\path')).toBe(
			'text ~ "say \\"hi\\" C:\\\\path" ORDER BY updated DESC',
		);
	});

	test("replacement patterns in the query stay literal", () => {
		expect(buildSearchJql('summary ~ "{query}"', "a$&b$1")).toBe('summary ~ "a$&b$1"');
	});
});

describe("rendering stays within the terminal", () => {
	for (const width of [40, 80, 120]) {
		test(`issue list at width ${width}`, () => {
			const keys = Array.from({ length: 40 }, (_, index) => `KOREAN_PROJECT-${index + 1}`);
			const { lines } = renderList(
				{
					query: { label: `필터 ${LONG_KOREAN}`, jql: `text ~ "${LONG_KOREAN}"` },
					load: ready("q", page(keys)),
					selected: 25,
					scroll: 0,
					notice: undefined,
				},
				width,
				24,
				plain,
			);
			expectWidthSafe(lines, width, 24);
			// The selected row (index 25) is scrolled into the visible window.
			expect(lines.filter(line => line.startsWith("› "))).toHaveLength(1);
		});

		test(`issue detail at width ${width}`, () => {
			for (const focus of ["description", "comments"] as const) {
				const { lines } = renderDetail(detailView(focus), width, 24, plain);
				expectWidthSafe(lines, width, 24);
			}
		});
	}

	test("detail panes stack vertically below width 80 and sit side by side from 80", () => {
		for (const width of [40, 60, 79]) {
			const { lines } = renderDetail(detailView(), width, 30, plain);
			const description = lines.findIndex(line => line.startsWith("▸ Description"));
			const commentsTitle = lines.findIndex(line => line.startsWith("  Comments"));
			expect(description).toBeGreaterThanOrEqual(0);
			expect(commentsTitle).toBeGreaterThan(description);
		}
		for (const width of [80, 120]) {
			const { lines } = renderDetail(detailView(), width, 30, plain);
			const titles = lines.filter(line => line.startsWith("▸ Description"));
			expect(titles).toHaveLength(1);
			expect(titles[0]).toContain("│");
			expect(titles[0]).toContain("Comments");
		}
	});
});

describe("list generation guard", () => {
	function deferredSearch() {
		const requests = new Map<string, PromiseWithResolvers<Page<Issue>> & { signal: AbortSignal }>();
		const search: SearchFn = (jql, signal) => {
			const deferred = Promise.withResolvers<Page<Issue>>();
			requests.set(jql, { ...deferred, signal });
			return deferred.promise;
		};
		const take = (jql: string) => {
			const request = requests.get(jql);
			if (!request) throw new Error(`no request for ${jql}`);
			return request;
		};
		return { search, take };
	}

	test("a late response from stale query A never overwrites query B or its selection", async () => {
		const { search, take } = deferredSearch();
		const model = new IssueListModel({ label: "A", jql: "project = A" }, () => {});
		const first = model.load({ label: "A", jql: "project = A" }, search);
		const requestA = take("project = A");
		const second = model.load({ label: "B", jql: "project = B" }, search);

		take("project = B").resolve(page(["B-1", "B-2", "B-3"]));
		await second;
		model.move(2);
		expect(model.selectedIssue?.key).toBe("B-3");

		requestA.resolve(page(["A-1"]));
		await first;
		expect(requestA.signal.aborted).toBe(true);
		expect(model.loader.state.dataKey).toBe("project = B");
		expect(model.loader.state.data?.items.map(item => item.key)).toEqual(["B-1", "B-2", "B-3"]);
		expect(model.selectedIssue?.key).toBe("B-3");
		expect(model.loader.fresh).toBe(true);
	});

	test("refresh keeps the selected issue; a failed refresh keeps data only as stale", async () => {
		const { search, take } = deferredSearch();
		const model = new IssueListModel({ label: "B", jql: "project = B" }, () => {});
		const initial = model.load({ label: "B", jql: "project = B" }, search);
		take("project = B").resolve(page(["B-1", "B-2", "B-3"]));
		await initial;
		model.move(2);

		const reordered = model.refresh(search);
		take("project = B").resolve(page(["B-3", "B-1"]));
		await reordered;
		expect(model.selectedIssue?.key).toBe("B-3");

		const failing = model.refresh(search);
		expect(model.loader.fresh).toBe(false);
		take("project = B").reject(new Error("HTTP 503 from Jira"));
		await failing;
		expect(model.loader.state.status).toBe("error");
		expect(model.loader.state.stale).toBe(true);
		expect(model.loader.state.error).toContain("503");
		expect(model.loader.fresh).toBe(false);
		expect(model.selectedIssue?.key).toBe("B-3");
	});
});

describe("runBrowser outside the interactive TUI", () => {
	function countingServices(calls: string[]): Services {
		const record = (name: string): Promise<never> => {
			calls.push(name);
			return Promise.reject(new Error(`unexpected call: ${name}`));
		};
		return {
			config: CONFIG,
			jira: {
				search: () => record("jira.search"),
				issue: () => record("jira.issue"),
				comments: () => record("jira.comments"),
				transitions: () => record("jira.transitions"),
				postTransition: () => record("jira.postTransition"),
			},
			checkouts: {
				resolveRepository: () => record("checkouts.resolveRepository"),
				listCheckouts: () => record("checkouts.listCheckouts"),
				planCheckout: () => record("checkouts.planCheckout"),
				prepareCheckout: () => record("checkouts.prepareCheckout"),
			},
			buildSnapshot: () => record("buildSnapshot"),
			saveSnapshot: () => record("saveSnapshot"),
			launchOmp: () => record("launchOmp"),
			runTransition: () => record("runTransition"),
		};
	}

	for (const mode of ["rpc", "json", "print"] as const) {
		test(`mode ${mode} rejects before any service call`, async () => {
			const calls: string[] = [];
			const services = countingServices(calls);
			const ctx = { mode, hasUI: mode === "rpc", cwd: "/tmp" } as ExtensionCommandContext;
			await expect(runBrowser(ctx, services)).rejects.toThrow("interactive TUI required");
			await expect(runBrowser(ctx, services, "ABC-1")).rejects.toThrow("interactive TUI required");
			expect(calls).toEqual([]);
		});
	}
});
