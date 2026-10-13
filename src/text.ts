/**
 * Plain-text normalization for Jira bodies (v2 strings and v3 ADF) and
 * terminal-safe sanitization of untrusted text.
 */

type AdfNode = { type?: unknown; text?: unknown; attrs?: unknown; content?: unknown };

/** Canonical plain-object guard for this package; fields remain `unknown` and must be checked by callers. */
export function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function attr(node: AdfNode, name: string): unknown {
	return isRecord(node.attrs) ? node.attrs[name] : undefined;
}

function children(node: AdfNode): unknown[] {
	return Array.isArray(node.content) ? node.content : [];
}

function inline(nodes: unknown[]): string {
	return nodes.map((n) => render(n, 0)).join("");
}

function blocks(nodes: unknown[], depth: number): string {
	return nodes
		.map((n) => render(n, depth))
		.filter((s) => s.length > 0)
		.join("\n");
}

function list(node: AdfNode, depth: number, ordered: boolean): string {
	const startAttr = attr(node, "order");
	let index = typeof startAttr === "number" && Number.isSafeInteger(startAttr) ? startAttr : 1;
	const indent = "  ".repeat(depth);
	const lines: string[] = [];
	for (const item of children(node)) {
		const marker = ordered ? `${index++}. ` : "- ";
		const body = isRecord(item) ? renderListItem(item, depth) : render(item, depth);
		const [first = "", ...rest] = body.split("\n");
		lines.push(`${indent}${marker}${first}`);
		for (const line of rest) lines.push(line.startsWith(indent) ? line : `${indent}  ${line}`);
	}
	return lines.join("\n");
}

function renderListItem(item: AdfNode, depth: number): string {
	return children(item)
		.map((child) => {
			if (isRecord(child) && (child.type === "bulletList" || child.type === "orderedList")) {
				return list(child, depth + 1, child.type === "orderedList");
			}
			return render(child, depth);
		})
		.filter((s) => s.length > 0)
		.join("\n");
}

function render(value: unknown, depth: number): string {
	if (typeof value === "string") return value;
	if (Array.isArray(value)) return blocks(value, depth);
	if (!isRecord(value)) return "";
	const node: AdfNode = value;
	switch (node.type) {
		case "text":
			return typeof node.text === "string" ? node.text : "";
		case "hardBreak":
			return "\n";
		case "mention": {
			const text = attr(node, "text");
			if (typeof text !== "string" || text.length === 0) return "@user";
			return text.startsWith("@") ? text : `@${text}`;
		}
		case "inlineCard":
		case "blockCard": {
			const url = attr(node, "url");
			return typeof url === "string" ? url : "";
		}
		case "emoji": {
			const text = attr(node, "text") ?? attr(node, "shortName");
			return typeof text === "string" ? text : "";
		}
		case "paragraph":
		case "heading":
			return inline(children(node));
		case "codeBlock":
			return inline(children(node));
		case "blockquote":
			return blocks(children(node), depth)
				.split("\n")
				.map((line) => `> ${line}`)
				.join("\n");
		case "bulletList":
			return list(node, depth, false);
		case "orderedList":
			return list(node, depth, true);
		case "listItem":
			return renderListItem(node, depth);
		case "doc":
			return blocks(children(node), depth);
		default: {
			// Unknown node: keep the text of its children. Inline-only children join inline.
			const kids = children(node);
			const allInline = kids.every(
				(k) => isRecord(k) && (k.type === "text" || k.type === "hardBreak" || k.type === "mention"),
			);
			return allInline ? inline(kids) : blocks(kids, depth);
		}
	}
}

/** Normalize a Jira body (v2 string, v3 ADF document, or null/missing) into plain text. */
export function toPlainText(value: unknown): string {
	if (value === null || value === undefined) return "";
	return render(value, 0).replace(/\r\n?/g, "\n");
}

// CSI: ESC [ params intermediates final; also 8-bit CSI (0x9b).
const CSI = /(?:\u001b\[|\u009b)[0-?]*[ -/]*[@-~]/g;
// OSC / DCS / SOS / PM / APC strings terminated by BEL or ST (ESC \ or 0x9c); unterminated runs to end.
const STRING_SEQ = /(?:\u001b[\]PX^_]|[\u0090\u0098\u009d\u009e\u009f])[\s\S]*?(?:\u0007|\u001b\\|\u009c|$)/g;
// Remaining two-byte ESC sequences (e.g. ESC c, ESC ( B).
const ESC_SEQ = /\u001b[ -/]*[0-~]?/g;
// C0 except \n and \t, DEL, C1.
const CONTROL = /[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g;

/**
 * Remove ANSI CSI/OSC/escape sequences and C0 (except LF/TAB), C1 and DEL
 * control characters; CRLF/CR become LF and tabs become spaces.
 */
export function sanitizeForTerminal(text: string): string {
	return text
		.replace(/\r\n?/g, "\n")
		.replace(STRING_SEQ, "")
		.replace(CSI, "")
		.replace(ESC_SEQ, "")
		.replace(CONTROL, "")
		.replace(/\t/g, "  ");
}
