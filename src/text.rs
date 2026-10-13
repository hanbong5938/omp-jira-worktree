//! Plain-text normalization for Jira bodies (v2 strings and v3 ADF),
//! terminal sanitization of untrusted text, and small formatting helpers.

use serde_json::{Value, json};

/// Removes ANSI CSI/OSC/DCS/escape sequences, C0 (except LF/TAB), DEL and C1
/// controls; CRLF/CR become LF and tabs become two spaces.
pub fn sanitize_for_terminal(text: &str) -> String {
    let chars: Vec<char> = normalize_newlines(text).chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        // OSC / DCS / SOS / PM / APC strings, terminated by BEL, ESC \ or ST.
        let string_start = match (c, next) {
            ('\u{1b}', Some(']' | 'P' | 'X' | '^' | '_')) => Some(i + 2),
            ('\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}', _) => Some(i + 1),
            _ => None,
        };
        if let Some(mut j) = string_start {
            while j < chars.len() {
                match chars[j] {
                    '\u{07}' | '\u{9c}' => {
                        j += 1;
                        break;
                    }
                    '\u{1b}' if chars.get(j + 1) == Some(&'\\') => {
                        j += 2;
                        break;
                    }
                    _ => j += 1,
                }
            }
            i = j;
            continue;
        }
        // CSI: ESC [ or 0x9b, params, intermediates, final byte.
        let csi_start = match (c, next) {
            ('\u{1b}', Some('[')) => Some(i + 2),
            ('\u{9b}', _) => Some(i + 1),
            _ => None,
        };
        if let Some(mut j) = csi_start {
            while j < chars.len() && ('0'..='?').contains(&chars[j]) {
                j += 1;
            }
            while j < chars.len() && (' '..='/').contains(&chars[j]) {
                j += 1;
            }
            if j < chars.len() && ('@'..='~').contains(&chars[j]) {
                i = j + 1;
                continue;
            }
            // Unterminated: drop only the introducer.
            i = csi_start.unwrap_or(i + 1);
            continue;
        }
        if c == '\u{1b}' {
            let mut j = i + 1;
            while j < chars.len() && (' '..='/').contains(&chars[j]) {
                j += 1;
            }
            if j < chars.len() && ('0'..='~').contains(&chars[j]) {
                j += 1;
            }
            i = j;
            continue;
        }
        match c {
            '\t' => out.push_str("  "),
            '\n' => out.push('\n'),
            '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => {}
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Sanitized text with whitespace runs collapsed to one space and trimmed.
pub fn single_line(text: &str) -> String {
    sanitize_for_terminal(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// At most `max` characters; ends with `…` when cut.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Normalizes a Jira body (v2 string, v3 ADF document, or null) into plain text.
pub fn to_plain_text(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    normalize_newlines(&render(value, 0))
}

fn attr<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
    node.get("attrs")
        .filter(|a| a.is_object())
        .and_then(|a| a.get(name))
}

fn children(node: &Value) -> &[Value] {
    node.get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn node_type(node: &Value) -> Option<&str> {
    node.get("type").and_then(Value::as_str)
}

fn inline(nodes: &[Value]) -> String {
    nodes.iter().map(|n| render(n, 0)).collect()
}

fn blocks(nodes: &[Value], depth: usize) -> String {
    nodes
        .iter()
        .map(|n| render(n, depth))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn list(node: &Value, depth: usize, ordered: bool) -> String {
    let mut index = attr(node, "order").and_then(Value::as_i64).unwrap_or(1);
    let indent = "  ".repeat(depth);
    let mut lines = Vec::new();
    for item in children(node) {
        let marker = if ordered {
            let m = format!("{index}. ");
            index += 1;
            m
        } else {
            "- ".to_owned()
        };
        let body = if item.is_object() {
            render_list_item(item, depth)
        } else {
            render(item, depth)
        };
        let mut parts = body.split('\n');
        lines.push(format!("{indent}{marker}{}", parts.next().unwrap_or("")));
        for line in parts {
            if line.starts_with(&indent) {
                lines.push(line.to_owned());
            } else {
                lines.push(format!("{indent}  {line}"));
            }
        }
    }
    lines.join("\n")
}

fn render_list_item(item: &Value, depth: usize) -> String {
    children(item)
        .iter()
        .map(|child| match node_type(child) {
            Some(t @ ("bulletList" | "orderedList")) => list(child, depth + 1, t == "orderedList"),
            _ => render(child, depth),
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn render(value: &Value, depth: usize) -> String {
    match value {
        Value::String(s) => return s.clone(),
        Value::Array(items) => return blocks(items, depth),
        Value::Object(_) => {}
        _ => return String::new(),
    }
    let str_attr = |name: &str| attr(value, name).and_then(Value::as_str);
    match node_type(value) {
        Some("text") => value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        Some("hardBreak") => "\n".to_owned(),
        Some("mention") => match str_attr("text") {
            None | Some("") => "@user".to_owned(),
            Some(t) if t.starts_with('@') => t.to_owned(),
            Some(t) => format!("@{t}"),
        },
        Some("inlineCard" | "blockCard") => str_attr("url").unwrap_or("").to_owned(),
        Some("emoji") => match attr(value, "text").or_else(|| attr(value, "shortName")) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        },
        Some("paragraph" | "heading" | "codeBlock") => inline(children(value)),
        Some("blockquote") => blocks(children(value), depth)
            .split('\n')
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        Some("bulletList") => list(value, depth, false),
        Some("orderedList") => list(value, depth, true),
        Some("listItem") => render_list_item(value, depth),
        Some("doc") => blocks(children(value), depth),
        _ => {
            let kids = children(value);
            let all_inline = kids
                .iter()
                .all(|k| matches!(node_type(k), Some("text" | "hardBreak" | "mention")));
            if all_inline {
                inline(kids)
            } else {
                blocks(kids, depth)
            }
        }
    }
}

/// Wraps plain text in an ADF v1 document: paragraphs split on blank lines,
/// single newlines become `hardBreak`. Empty input yields a doc with no content.
pub fn plain_to_adf(text: &str) -> Value {
    fn flush(paragraphs: &mut Vec<Value>, lines: &mut Vec<&str>) {
        if lines.is_empty() {
            return;
        }
        let mut content = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                content.push(json!({"type": "hardBreak"}));
            }
            content.push(json!({"type": "text", "text": line}));
        }
        paragraphs.push(json!({"type": "paragraph", "content": content}));
        lines.clear();
    }
    let normalized = normalize_newlines(text);
    let mut paragraphs = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in normalized.split('\n') {
        if line.trim().is_empty() {
            flush(&mut paragraphs, &mut current);
        } else {
            current.push(line);
        }
    }
    flush(&mut paragraphs, &mut current);
    json!({"type": "doc", "version": 1, "content": paragraphs})
}

/// `YYYY-MM-DD HH:MM` (or `YYYY-MM-DD` when `date_only`) read from the ISO
/// string as given; unparseable input falls back to `single_line(raw)`.
pub fn format_timestamp(raw: &str, date_only: bool) -> String {
    let b = raw.trim().as_bytes();
    let digits =
        |r: std::ops::Range<usize>| b.len() >= r.end && b[r].iter().all(u8::is_ascii_digit);
    let date_ok = digits(0..4)
        && b.get(4) == Some(&b'-')
        && digits(5..7)
        && b.get(7) == Some(&b'-')
        && digits(8..10);
    if !date_ok {
        return single_line(raw);
    }
    let s = raw.trim();
    let day = &s[..10];
    if date_only {
        return day.to_owned();
    }
    let time_ok = matches!(b.get(10), Some(b'T' | b' '))
        && digits(11..13)
        && b.get(13) == Some(&b':')
        && digits(14..16);
    if time_ok {
        format!("{day} {}", &s[11..16])
    } else if b.len() == 10 {
        format!("{day} 00:00")
    } else {
        single_line(raw)
    }
}

/// Escapes `\` and `"` for use inside a JQL string literal.
pub fn escape_jql_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Replaces the first `{query}` in `template` with the escaped query, literally.
pub fn build_search_jql(template: &str, query: &str) -> String {
    template.replacen("{query}", &escape_jql_string(query), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_escapes_and_controls() {
        let input = "a\u{1b}[31mred\u{1b}[0m b\u{1b}]0;title\u{07}c\u{1b}]8;;http://x\u{1b}\\d\u{9b}2Je\u{1b}cf\r\ng\rh\u{0}\u{7f}\u{85}\ti";
        assert_eq!(sanitize_for_terminal(input), "ared bcdef\ng\nh  i");
    }

    #[test]
    fn sanitize_drops_unterminated_osc() {
        assert_eq!(sanitize_for_terminal("ok\u{1b}]52;c;secret"), "ok");
    }

    #[test]
    fn single_line_and_truncate() {
        assert_eq!(single_line("  a\n\tb  c "), "a b c");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(truncate_chars("abc", 3), "abc");
    }

    #[test]
    fn plain_text_from_string_and_null() {
        assert_eq!(to_plain_text(&Value::Null), "");
        assert_eq!(to_plain_text(&json!("a\r\nb")), "a\nb");
    }

    #[test]
    fn adf_flattening() {
        let doc = json!({"type": "doc", "version": 1, "content": [
            {"type": "heading", "content": [{"type": "text", "text": "Title"}]},
            {"type": "paragraph", "content": [
                {"type": "text", "text": "hi "},
                {"type": "mention", "attrs": {"text": "Kim"}},
                {"type": "hardBreak"},
                {"type": "inlineCard", "attrs": {"url": "https://x"}},
                {"type": "emoji", "attrs": {"shortName": ":smile:"}}
            ]},
            {"type": "bulletList", "content": [
                {"type": "listItem", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "one"}]},
                    {"type": "orderedList", "attrs": {"order": 3}, "content": [
                        {"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "nested"}]}]}
                    ]}
                ]},
                {"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "two"}]}]}
            ]},
            {"type": "blockquote", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "q"}]}]},
            {"type": "panel", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "p"}]}]}
        ]});
        assert_eq!(
            to_plain_text(&doc),
            "Title\nhi @Kim\nhttps://x:smile:\n- one\n  3. nested\n- two\n> q\np"
        );
    }

    #[test]
    fn plain_to_adf_shape() {
        assert_eq!(
            plain_to_adf(""),
            json!({"type": "doc", "version": 1, "content": []})
        );
        assert_eq!(
            plain_to_adf("a\r\nb\n\n\nc"),
            json!({"type": "doc", "version": 1, "content": [
                {"type": "paragraph", "content": [
                    {"type": "text", "text": "a"}, {"type": "hardBreak"}, {"type": "text", "text": "b"}
                ]},
                {"type": "paragraph", "content": [{"type": "text", "text": "c"}]}
            ]})
        );
        assert_eq!(to_plain_text(&plain_to_adf("x\ny\n\nz")), "x\ny\nz");
    }

    #[test]
    fn timestamps() {
        assert_eq!(
            format_timestamp("2026-03-04T05:06:07.000+0900", false),
            "2026-03-04 05:06"
        );
        assert_eq!(
            format_timestamp("2026-03-04T05:06:07.000+0900", true),
            "2026-03-04"
        );
        assert_eq!(format_timestamp("not a\ndate", false), "not a date");
    }

    #[test]
    fn jql_building_is_literal() {
        assert_eq!(escape_jql_string(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(
            build_search_jql(r#"text ~ "{query}""#, "$& \"x\""),
            r#"text ~ "$& \"x\"""#
        );
    }
}
