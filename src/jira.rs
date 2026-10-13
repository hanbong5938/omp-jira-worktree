//! Jira REST client (Cloud v3 / Data Center v2). Credentials come only from the
//! configured token command argv; requests never follow redirects, never retry,
//! and read at most 2 MiB per response.

use std::io::Read;
use std::sync::Mutex;

use base64::Engine;
use serde_json::{Map, Value, json};

use crate::model::{
    AuthKind, Comment, CommentResult, Deployment, Issue, JiraApi, JiraConfig, JiraError,
    JiraErrorKind, MAX_COMMENT_CHARS, Page, RUN_TIMEOUT, StatusRef, Transition, TransitionResult,
    normalize_issue_key,
};
use crate::text::{plain_to_adf, sanitize_for_terminal, to_plain_text};

const FIELDS: [&str; 6] = [
    "summary",
    "status",
    "issuetype",
    "assignee",
    "updated",
    "description",
];
const PAGE_SIZE: usize = 50;
const MAX_BODY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SERVER_MESSAGE: usize = 300;

#[derive(Clone)]
struct Credential {
    header: String,
    token: String,
}

struct RawResponse {
    status: u16,
    content_type: String,
    text: String,
}

enum Sent {
    Response(RawResponse),
    Failed { dispatched: bool, error: JiraError },
}

fn err(kind: JiraErrorKind, message: impl Into<String>) -> JiraError {
    JiraError {
        kind,
        message: message.into(),
        status: None,
    }
}

fn err_status(kind: JiraErrorKind, message: impl Into<String>, status: u16) -> JiraError {
    JiraError {
        kind,
        message: message.into(),
        status: Some(status),
    }
}

fn protocol(message: impl AsRef<str>) -> JiraError {
    err(
        JiraErrorKind::Protocol,
        format!("Jira protocol error: {}", message.as_ref()),
    )
}

fn json_escaped(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted[1..quoted.len() - 1].to_string()
}

/// Every textual form a credential could be echoed as: raw, JSON-escaped, and with `/` escaped as `\/`.
fn credential_forms(header: &str, token: &str, email: Option<&str>) -> Vec<String> {
    let mut bases = vec![header.to_string(), token.to_string()];
    if let Some(space) = header.find(' ')
        && space > 0
    {
        bases.push(header[space + 1..].to_string());
    }
    if let Some(email) = email {
        bases.push(format!("{email}:{token}"));
    }
    let mut forms: Vec<String> = Vec::new();
    for base in bases.iter().filter(|b| !b.is_empty()) {
        let escaped = json_escaped(base);
        for form in [
            base.clone(),
            escaped.clone(),
            base.replace('/', "\\/"),
            escaped.replace('/', "\\/"),
        ] {
            if !forms.contains(&form) {
                forms.push(form);
            }
        }
    }
    // Longest first so a full header is replaced before its payload or token substring.
    forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
    forms
}

fn redact_credential(text: &str, header: &str, token: &str, email: Option<&str>) -> String {
    let mut out = text.to_string();
    for form in credential_forms(header, token, email) {
        if out.contains(&form) {
            out = out.replace(&form, "[redacted]");
        }
    }
    out
}

/// Strip control/escape sequences from server-provided text and cap it at 300 characters.
pub fn sanitize_server_message(text: &str) -> String {
    let clean = sanitize_for_terminal(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if clean.chars().count() > MAX_SERVER_MESSAGE {
        let mut cut: String = clean.chars().take(MAX_SERVER_MESSAGE - 1).collect();
        cut.push('…');
        cut
    } else {
        clean
    }
}

fn is_html(raw: &RawResponse) -> bool {
    if raw.content_type.contains("text/html") {
        return true;
    }
    let start = raw.text.trim_start().to_ascii_lowercase();
    start.starts_with("<!doctype") || start.starts_with("<html")
}

/// Extract Jira's `errorMessages`/`errors` (or a short text body) for display.
fn server_message(raw: &RawResponse) -> String {
    if raw.text.is_empty() {
        return String::new();
    }
    if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&raw.text) {
        let mut parts: Vec<String> = Vec::new();
        if let Some(Value::Array(messages)) = parsed.get("errorMessages") {
            parts.extend(
                messages
                    .iter()
                    .filter_map(|m| m.as_str().map(str::to_string)),
            );
        }
        if let Some(Value::Object(errors)) = parsed.get("errors") {
            for (field, m) in errors {
                if let Some(m) = m.as_str() {
                    parts.push(format!("{field}: {m}"));
                }
            }
        }
        if let Some(Value::String(m)) = parsed.get("message") {
            parts.push(m.clone());
        }
        return sanitize_server_message(&parts.join("; "));
    }
    if is_html(raw) {
        return String::new();
    }
    sanitize_server_message(&raw.text)
}

fn status_error(raw: &RawResponse, what: &str) -> JiraError {
    let detail = server_message(raw);
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    let status = raw.status;
    match status {
        401 => err_status(
            JiraErrorKind::Unauthorized,
            format!(
                "Jira authentication failed (401) for {what}; check jira.email/jira.auth and the tokenCommand credential{suffix}"
            ),
            401,
        ),
        403 => err_status(
            JiraErrorKind::Forbidden,
            format!("Jira permission denied (403) for {what}{suffix}"),
            403,
        ),
        404 => err_status(
            JiraErrorKind::NotFound,
            format!(
                "Jira returned 404 for {what}: not found or not visible to this account, or wrong baseUrl/deployment{suffix}"
            ),
            404,
        ),
        429 => err_status(
            JiraErrorKind::RateLimited,
            format!("Jira rate limit reached (429) for {what}; try again later{suffix}"),
            429,
        ),
        _ if is_html(raw) => err_status(
            JiraErrorKind::Html,
            format!("Jira returned an HTML page (HTTP {status}) for {what}; check jira.baseUrl"),
            status,
        ),
        _ => err_status(
            JiraErrorKind::Http,
            format!("Jira request failed (HTTP {status}) for {what}{suffix}"),
            status,
        ),
    }
}

fn parse_json(raw: &RawResponse, what: &str) -> Result<Value, JiraError> {
    if is_html(raw) {
        return Err(err_status(
            JiraErrorKind::Html,
            format!("Jira returned an HTML page instead of JSON for {what}; check jira.baseUrl"),
            raw.status,
        ));
    }
    serde_json::from_str(&raw.text).map_err(|_| {
        err_status(
            JiraErrorKind::InvalidJson,
            format!("Jira returned invalid JSON for {what}"),
            raw.status,
        )
    })
}

fn string_field(value: Option<&Value>, field: &str) -> Result<String, JiraError> {
    match value {
        Some(Value::String(s)) => Ok(sanitize_for_terminal(s)),
        _ => Err(protocol(format!("missing or invalid {field}"))),
    }
}

fn status_ref(value: Option<&Value>, field: &str) -> Result<StatusRef, JiraError> {
    let Some(Value::Object(obj)) = value else {
        return Err(protocol(format!("missing or invalid {field}")));
    };
    Ok(StatusRef {
        id: string_field(obj.get("id"), &format!("{field}.id"))?,
        name: string_field(obj.get("name"), &format!("{field}.name"))?,
    })
}

fn optional_total(value: Option<&Value>) -> Option<usize> {
    const MAX_SAFE: u64 = (1 << 53) - 1;
    value
        .and_then(Value::as_u64)
        .filter(|n| *n <= MAX_SAFE)
        .map(|n| n as usize)
}

fn parse_issue(value: &Value, base_url: &str) -> Result<Issue, JiraError> {
    let Value::Object(obj) = value else {
        return Err(protocol("issue entry is not an object"));
    };
    let key = normalize_issue_key(&string_field(obj.get("key"), "issue key")?).map_err(protocol)?;
    let Some(Value::Object(fields)) = obj.get("fields") else {
        return Err(protocol(format!("issue {key} has no fields object")));
    };
    let Some(Value::Object(issue_type)) = fields.get("issuetype") else {
        return Err(protocol(format!("issue {key} has no issuetype")));
    };
    let assignee = match fields.get("assignee") {
        None | Some(Value::Null) => String::new(),
        Some(Value::Object(a)) => a
            .get("displayName")
            .and_then(Value::as_str)
            .map(sanitize_for_terminal)
            .unwrap_or_default(),
        Some(_) => return Err(protocol(format!("issue {key} has an invalid assignee"))),
    };
    Ok(Issue {
        summary: string_field(fields.get("summary"), &format!("{key} summary"))?,
        status: status_ref(fields.get("status"), &format!("{key} status"))?,
        issue_type: string_field(issue_type.get("name"), &format!("{key} issuetype.name"))?,
        assignee,
        updated: string_field(fields.get("updated"), &format!("{key} updated"))?,
        description: sanitize_for_terminal(&to_plain_text(
            fields.get("description").unwrap_or(&Value::Null),
        )),
        url: format!("{base_url}/browse/{key}"),
        key,
    })
}

fn parse_comment(value: &Value) -> Result<Comment, JiraError> {
    let Value::Object(obj) = value else {
        return Err(protocol("comment entry is not an object"));
    };
    let id = string_field(obj.get("id"), "comment id")?;
    let author = obj
        .get("author")
        .and_then(|a| a.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Ok(Comment {
        author: sanitize_for_terminal(author),
        created: string_field(obj.get("created"), &format!("comment {id} created"))?,
        body: sanitize_for_terminal(&to_plain_text(obj.get("body").unwrap_or(&Value::Null))),
        id,
    })
}

fn parse_transition(value: &Value) -> Result<Transition, JiraError> {
    let Value::Object(obj) = value else {
        return Err(protocol("transition entry is not an object"));
    };
    let id = string_field(obj.get("id"), "transition id")?;
    let requires_fields = match obj.get("fields") {
        None => false,
        Some(Value::Object(fields)) => fields
            .values()
            .any(|f| f.get("required").and_then(Value::as_bool) == Some(true)),
        Some(_) => return Err(protocol(format!("transition {id} has invalid fields"))),
    };
    Ok(Transition {
        name: string_field(obj.get("name"), &format!("transition {id} name"))?,
        to: status_ref(obj.get("to"), &format!("transition {id} to"))?,
        // A missing hasScreen is treated as "has a screen" so the UI routes to Jira web instead of POSTing blind.
        has_screen: obj.get("hasScreen") != Some(&Value::Bool(false)),
        requires_fields,
        is_available: obj.get("isAvailable").and_then(Value::as_bool),
        id,
    })
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Epoch milliseconds of `YYYY-MM-DDTHH:MM[:SS[.fff]][Z|±HH:MM|±HHMM]`; no offset means UTC.
fn parse_timestamp_ms(raw: &str) -> Option<i64> {
    let b = raw.trim().as_bytes();
    let num = |s: &[u8]| -> Option<i64> {
        if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(s).ok()?.parse().ok()
    };
    if b.len() < 16
        || b[4] != b'-'
        || b[7] != b'-'
        || (b[10] != b'T' && b[10] != b' ')
        || b[13] != b':'
    {
        return None;
    }
    let (y, mo, d, h, mi) = (
        num(&b[0..4])?,
        num(&b[5..7])?,
        num(&b[8..10])?,
        num(&b[11..13])?,
        num(&b[14..16])?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 {
        return None;
    }
    let mut i = 16;
    let mut sec = 0;
    let mut ms = 0;
    if b.get(i) == Some(&b':') {
        sec = num(b.get(i + 1..i + 3)?)?;
        if sec > 59 {
            return None;
        }
        i += 3;
        if b.get(i) == Some(&b'.') {
            let start = i + 1;
            let mut end = start;
            while end < b.len() && b[end].is_ascii_digit() {
                end += 1;
            }
            if end == start {
                return None;
            }
            let frac = &b[start..end.min(start + 3)];
            ms = num(frac)? * 10_i64.pow(3 - frac.len() as u32);
            i = end;
        }
    }
    let offset_min = match &b[i..] {
        [] | [b'Z'] => 0,
        [sign @ (b'+' | b'-'), rest @ ..] => {
            let (oh, om) = match rest {
                [h1, h2, b':', m1, m2] => (num(&[*h1, *h2])?, num(&[*m1, *m2])?),
                [h1, h2, m1, m2] => (num(&[*h1, *h2])?, num(&[*m1, *m2])?),
                _ => return None,
            };
            let total = oh * 60 + om;
            if *sign == b'+' { total } else { -total }
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    Some((((days * 24 + h) * 60 + mi - offset_min) * 60 + sec) * 1000 + ms)
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

/// Newest `created` first; equal timestamps ordered by id (numeric when both are numeric), descending.
fn compare_comments_newest_first(a: &Comment, b: &Comment) -> std::cmp::Ordering {
    let ta = parse_timestamp_ms(&a.created);
    let tb = parse_timestamp_ms(&b.created);
    match (ta, tb) {
        (Some(x), Some(y)) if x != y => return y.cmp(&x),
        (Some(_), Some(_)) => {}
        _ if a.created != b.created => return b.created.cmp(&a.created),
        _ => {}
    }
    if is_digits(&a.id) && is_digits(&b.id) {
        let ia = a.id.trim_start_matches('0');
        let ib = b.id.trim_start_matches('0');
        return (ib.len(), ib).cmp(&(ia.len(), ia));
    }
    b.id.cmp(&a.id)
}

fn io_is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

fn error_chain_is_timeout(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = current {
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && io_is_timeout(io)
        {
            return true;
        }
        if err.to_string().contains("timed out") {
            return true;
        }
        current = err.source();
    }
    false
}

pub struct JiraClient {
    config: JiraConfig,
    api_base: String,
    agent: ureq::Agent,
    credential: Mutex<Option<Credential>>,
}

impl JiraClient {
    pub fn new(cfg: &JiraConfig) -> Self {
        let version = match cfg.deployment {
            Deployment::Cloud => "3",
            Deployment::Dc => "2",
        };
        let agent = ureq::AgentBuilder::new()
            .timeout(cfg.timeout)
            .redirects(0)
            .build();
        JiraClient {
            api_base: format!("{}/rest/api/{version}", cfg.base_url),
            config: cfg.clone(),
            agent,
            credential: Mutex::new(None),
        }
    }

    fn load_credential(&self) -> Result<Credential, JiraError> {
        let file = self
            .config
            .token_command
            .first()
            .map(String::as_str)
            .unwrap_or("");
        let args: Vec<&str> = self
            .config
            .token_command
            .iter()
            .skip(1)
            .map(String::as_str)
            .collect();
        let result = crate::process::run(file, &args, None, RUN_TIMEOUT, None).map_err(|_| {
            err(
                JiraErrorKind::Credential,
                format!(
                    "Jira credential command could not be started (jira.tokenCommand[0] = {})",
                    serde_json::to_string(file).unwrap_or_default()
                ),
            )
        })?;
        if result.killed {
            return Err(err(
                JiraErrorKind::Credential,
                "Jira credential command timed out or was killed",
            ));
        }
        if result.code != Some(0) {
            let code = result
                .code
                .map_or_else(|| "null".to_string(), |c| c.to_string());
            return Err(err(
                JiraErrorKind::Credential,
                format!("Jira credential command failed with exit code {code}; output withheld"),
            ));
        }
        let token = result
            .stdout
            .strip_suffix('\n')
            .unwrap_or(&result.stdout)
            .to_string();
        if token.is_empty() {
            return Err(err(
                JiraErrorKind::Credential,
                "Jira credential command printed an empty token",
            ));
        }
        if token.contains(['\r', '\n']) {
            return Err(err(
                JiraErrorKind::Credential,
                "Jira credential command printed more than one line; output withheld",
            ));
        }
        let header = match self.config.auth {
            AuthKind::Basic => {
                let pair = format!("{}:{token}", self.config.email.as_deref().unwrap_or(""));
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(pair)
                )
            }
            AuthKind::Bearer => format!("Bearer {token}"),
        };
        Ok(Credential { header, token })
    }

    /// Lazily resolve and cache the credential; a failure is not cached so a later call retries it.
    fn get_credential(&self) -> Result<Credential, JiraError> {
        let mut guard = self.credential.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(cred) = guard.as_ref() {
            return Ok(cred.clone());
        }
        let cred = self.load_credential()?;
        *guard = Some(cred.clone());
        Ok(cred)
    }

    /// One request, no retry. Returns a classified failure so POST callers can tell dispatch state.
    fn send(&self, method: &str, path: &str, body: Option<&Value>, what: &str) -> Sent {
        let cred = match self.get_credential() {
            Ok(c) => c,
            Err(error) => {
                return Sent::Failed {
                    dispatched: false,
                    error,
                };
            }
        };
        let timeout_ms = self.config.timeout.as_millis();
        let classify = |timed_out: bool| {
            if timed_out {
                err(
                    JiraErrorKind::Timeout,
                    format!("Jira request timed out after {timeout_ms} ms: {what}"),
                )
            } else {
                err(
                    JiraErrorKind::Network,
                    format!("Jira request failed (network error or refused redirect): {what}"),
                )
            }
        };
        let url = format!("{}{}", self.api_base, path);
        let request = self
            .agent
            .request(method, &url)
            .set("Accept", "application/json")
            .set("Authorization", &cred.header);
        let result = match body {
            Some(b) => request
                .set("Content-Type", "application/json")
                .send_string(&b.to_string()),
            None => request.call(),
        };
        let response = match result {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => {
                return Sent::Failed {
                    dispatched: true,
                    error: classify(error_chain_is_timeout(&t)),
                };
            }
        };
        let status = response.status();
        let content_type = response.header("content-type").unwrap_or("").to_string();
        let mut bytes = Vec::new();
        if let Err(e) = response
            .into_reader()
            .take(MAX_BODY_BYTES + 1)
            .read_to_end(&mut bytes)
        {
            return Sent::Failed {
                dispatched: true,
                error: classify(error_chain_is_timeout(&e)),
            };
        }
        if bytes.len() as u64 > MAX_BODY_BYTES {
            return Sent::Failed {
                dispatched: true,
                error: err(
                    JiraErrorKind::TooLarge,
                    "Jira response exceeded the 2 MiB limit",
                ),
            };
        }
        let text = String::from_utf8_lossy(&bytes);
        let text = redact_credential(
            &text,
            &cred.header,
            &cred.token,
            self.config.email.as_deref(),
        );
        Sent::Response(RawResponse {
            status,
            content_type,
            text,
        })
    }

    fn request_json(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        what: &str,
    ) -> Result<Value, JiraError> {
        match self.send(method, path, body, what) {
            Sent::Failed { error, .. } => Err(error),
            Sent::Response(raw) if !(200..300).contains(&raw.status) => {
                Err(status_error(&raw, what))
            }
            Sent::Response(raw) => parse_json(&raw, what),
        }
    }

    fn push_issues(
        &self,
        entries: &[Value],
        keys: &mut Vec<String>,
        items: &mut Vec<Issue>,
    ) -> Result<(), JiraError> {
        for entry in entries {
            let issue = parse_issue(entry, &self.config.base_url)?;
            if keys.contains(&issue.key) {
                return Err(protocol(format!(
                    "duplicate issue {} across search pages",
                    issue.key
                )));
            }
            keys.push(issue.key.clone());
            items.push(issue);
        }
        Ok(())
    }

    fn search_cloud(&self, jql: &str) -> Result<Page<Issue>, JiraError> {
        let max_results = self.config.max_results;
        let mut items = Vec::new();
        let mut keys = Vec::new();
        let mut tokens: Vec<String> = Vec::new();
        let mut token: Option<String> = None;
        let mut requests = 0;
        loop {
            if requests > max_results {
                return Err(protocol(
                    "search exceeded the request budget without reaching the result cap",
                ));
            }
            requests += 1;
            let page_size = PAGE_SIZE.min(max_results - items.len());
            let mut body = json!({ "jql": jql, "fields": FIELDS, "maxResults": page_size });
            if let Some(t) = &token {
                body["nextPageToken"] = Value::String(t.clone());
            }
            let data = self.request_json("POST", "/search/jql", Some(&body), "issue search")?;
            let Some(issues) = data.get("issues").and_then(Value::as_array) else {
                return Err(protocol("search response has no issues array"));
            };
            if issues.len() > page_size {
                return Err(protocol("search page returned more issues than requested"));
            }
            self.push_issues(issues, &mut keys, &mut items)?;
            let total = optional_total(data.get("total"));
            if total.is_some_and(|t| items.len() > t) {
                return Err(protocol("search returned more issues than its total"));
            }
            let next_token = match data.get("nextPageToken") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                Some(_) => return Err(protocol("invalid nextPageToken")),
            };
            match data.get("isLast") {
                Some(Value::Bool(true)) if next_token.is_some() => {
                    return Err(protocol("isLast is true but a nextPageToken was returned"));
                }
                Some(Value::Bool(false)) if next_token.is_none() => {
                    return Err(protocol(
                        "isLast is false but no nextPageToken was returned",
                    ));
                }
                _ => {}
            }
            let Some(next_token) = next_token else {
                if total.is_some_and(|t| items.len() < t) {
                    return Err(protocol("search ended before reaching its total"));
                }
                return Ok(Page {
                    items,
                    total,
                    has_more: false,
                });
            };
            if token.as_ref() == Some(&next_token) || tokens.contains(&next_token) {
                return Err(protocol("search repeated a nextPageToken"));
            }
            tokens.push(next_token.clone());
            token = Some(next_token);
            if items.len() >= max_results {
                return Ok(Page {
                    items,
                    total,
                    has_more: true,
                });
            }
        }
    }

    fn search_dc(&self, jql: &str) -> Result<Page<Issue>, JiraError> {
        let max_results = self.config.max_results;
        let mut items = Vec::new();
        let mut keys = Vec::new();
        let mut requests = 0;
        loop {
            if requests > max_results {
                return Err(protocol(
                    "search exceeded the request budget without reaching the result cap",
                ));
            }
            requests += 1;
            let start_at = items.len();
            let page_size = PAGE_SIZE.min(max_results - items.len());
            let body = json!({ "jql": jql, "fields": FIELDS, "maxResults": page_size, "startAt": start_at });
            let data = self.request_json("POST", "/search", Some(&body), "issue search")?;
            let Some(issues) = data.get("issues").and_then(Value::as_array) else {
                return Err(protocol("search response has no issues array"));
            };
            if data.get("startAt").and_then(Value::as_u64) != Some(start_at as u64) {
                let got = data
                    .get("startAt")
                    .map_or_else(|| "undefined".to_string(), Value::to_string);
                return Err(protocol(format!(
                    "search page startAt {got} does not match {start_at}"
                )));
            }
            if issues.len() > page_size {
                return Err(protocol("search page returned more issues than requested"));
            }
            let Some(total) = optional_total(data.get("total")) else {
                return Err(protocol("search response has no total"));
            };
            self.push_issues(issues, &mut keys, &mut items)?;
            if items.len() > total {
                return Err(protocol("search returned more issues than its total"));
            }
            if items.len() == total {
                return Ok(Page {
                    items,
                    total: Some(total),
                    has_more: false,
                });
            }
            if issues.is_empty() {
                return Err(protocol(
                    "search returned an empty page before reaching its total",
                ));
            }
            if items.len() >= max_results {
                return Ok(Page {
                    items,
                    total: Some(total),
                    has_more: true,
                });
            }
        }
    }
}

impl JiraApi for JiraClient {
    fn search(&self, jql: &str) -> Result<Page<Issue>, JiraError> {
        match self.config.deployment {
            Deployment::Cloud => self.search_cloud(jql),
            Deployment::Dc => self.search_dc(jql),
        }
    }

    fn issue(&self, raw_key: &str) -> Result<Issue, JiraError> {
        let key = normalize_issue_key(raw_key).map_err(protocol)?;
        let path = format!("/issue/{key}?fields={}", FIELDS.join(","));
        let data = self.request_json("GET", &path, None, &format!("issue {key}"))?;
        let issue = parse_issue(&data, &self.config.base_url)?;
        if issue.key != key {
            return Err(protocol(format!(
                "requested {key} but Jira returned {}",
                issue.key
            )));
        }
        Ok(issue)
    }

    /// Up to `limit` comments, newest first. Whenever not every comment was fetched, `has_more` is
    /// true: the fetched set is then not guaranteed to be the newest N (the server may ignore
    /// `orderBy=-created`).
    fn comments(&self, raw_key: &str, limit: usize) -> Result<Page<Comment>, JiraError> {
        let key = normalize_issue_key(raw_key).map_err(protocol)?;
        if limit == 0 {
            return Ok(Page {
                items: Vec::new(),
                total: None,
                has_more: false,
            });
        }
        let mut items: Vec<Comment> = Vec::new();
        let mut ids: Vec<String> = Vec::new();
        let mut total;
        let mut requests = 0;
        loop {
            if requests > limit {
                return Err(protocol("comment paging exceeded the request budget"));
            }
            requests += 1;
            let start_at = items.len();
            let page_size = PAGE_SIZE.min(limit - items.len());
            let path = format!(
                "/issue/{key}/comment?orderBy=-created&startAt={start_at}&maxResults={page_size}"
            );
            let data = self.request_json("GET", &path, None, &format!("comments of {key}"))?;
            let Some(comments) = data.get("comments").and_then(Value::as_array) else {
                return Err(protocol("comment response has no comments array"));
            };
            if let Some(s) = data.get("startAt")
                && s.as_u64() != Some(start_at as u64)
            {
                return Err(protocol(format!(
                    "comment page startAt {s} does not match {start_at}"
                )));
            }
            if comments.len() > page_size {
                return Err(protocol(
                    "comment page returned more comments than requested",
                ));
            }
            total = optional_total(data.get("total"));
            let Some(t) = total else {
                return Err(protocol("comment response has no total"));
            };
            for entry in comments {
                let comment = parse_comment(entry)?;
                if ids.contains(&comment.id) {
                    return Err(protocol(format!(
                        "duplicate comment {} across pages",
                        comment.id
                    )));
                }
                ids.push(comment.id.clone());
                items.push(comment);
            }
            if items.len() > t {
                return Err(protocol("comments exceeded their total"));
            }
            if items.len() == t || items.len() >= limit {
                break;
            }
            if comments.is_empty() {
                return Err(protocol(
                    "comments returned an empty page before reaching their total",
                ));
            }
        }
        let complete = Some(items.len()) == total;
        items.sort_by(compare_comments_newest_first);
        Ok(Page {
            items,
            total,
            has_more: !complete,
        })
    }

    fn transitions(&self, raw_key: &str) -> Result<Vec<Transition>, JiraError> {
        let key = normalize_issue_key(raw_key).map_err(protocol)?;
        let path = format!("/issue/{key}/transitions?expand=transitions.fields");
        let data = self.request_json("GET", &path, None, &format!("transitions of {key}"))?;
        let Some(entries) = data.get("transitions").and_then(Value::as_array) else {
            return Err(protocol("transitions response has no transitions array"));
        };
        let mut out: Vec<Transition> = Vec::with_capacity(entries.len());
        for entry in entries {
            let transition = parse_transition(entry)?;
            if out.iter().any(|t| t.id == transition.id) {
                return Err(protocol(format!("duplicate transition {}", transition.id)));
            }
            out.push(transition);
        }
        Ok(out)
    }

    /// Single POST, never retried. 2xx → acknowledged (unverified; callers re-read status).
    /// Definite 4xx or failure before dispatch → rejected. 5xx/timeout/transport drop/unreadable
    /// response → unknown.
    fn post_transition(&self, raw_key: &str, transition_id: &str) -> TransitionResult {
        let key = match normalize_issue_key(raw_key) {
            Ok(k) => k,
            Err(message) => {
                return TransitionResult::Rejected {
                    message: format!("{message}; nothing was sent"),
                };
            }
        };
        if transition_id.is_empty() {
            return TransitionResult::Rejected {
                message: "transition id is empty; nothing was sent".to_string(),
            };
        }
        let what = format!("transition {transition_id} of {key}");
        let body = json!({ "transition": { "id": transition_id } });
        match self.send(
            "POST",
            &format!("/issue/{key}/transitions"),
            Some(&body),
            &what,
        ) {
            Sent::Failed {
                dispatched: false,
                error,
            } => TransitionResult::Rejected {
                message: format!("{} (request not sent)", error.message),
            },
            Sent::Failed {
                dispatched: true,
                error,
            } => TransitionResult::Unknown {
                message: format!(
                    "{}; the transition may or may not have been applied",
                    error.message
                ),
                observed_status: None,
            },
            Sent::Response(raw) if (200..300).contains(&raw.status) => {
                TransitionResult::Acknowledged {
                    observed_status: None,
                    verified: false,
                }
            }
            Sent::Response(raw) if (400..500).contains(&raw.status) => TransitionResult::Rejected {
                message: status_error(&raw, &what).message,
            },
            Sent::Response(raw) => TransitionResult::Unknown {
                message: format!(
                    "{}; the transition may or may not have been applied",
                    status_error(&raw, &what).message
                ),
                observed_status: None,
            },
        }
    }

    /// Single POST, never retried. Empty/oversized bodies and failures before dispatch are rejected
    /// without sending; 4xx → rejected; 5xx/timeout/transport drop/unparseable 2xx → unknown.
    fn add_comment(&self, raw_key: &str, body: &str) -> CommentResult {
        let key = match normalize_issue_key(raw_key) {
            Ok(k) => k,
            Err(message) => {
                return CommentResult::Rejected {
                    message: format!("{message}; nothing was sent"),
                };
            }
        };
        if body.trim().is_empty() {
            return CommentResult::Rejected {
                message: "comment is empty; nothing was sent".to_string(),
            };
        }
        if body.chars().count() > MAX_COMMENT_CHARS {
            return CommentResult::Rejected {
                message: format!(
                    "comment exceeds {MAX_COMMENT_CHARS} characters; nothing was sent"
                ),
            };
        }
        let what = format!("comment on {key}");
        let payload = match self.config.deployment {
            Deployment::Cloud => {
                let mut map = Map::new();
                map.insert("body".to_string(), plain_to_adf(body));
                Value::Object(map)
            }
            Deployment::Dc => json!({ "body": body }),
        };
        let unsure = "the comment may or may not have been added";
        match self.send(
            "POST",
            &format!("/issue/{key}/comment"),
            Some(&payload),
            &what,
        ) {
            Sent::Failed {
                dispatched: false,
                error,
            } => CommentResult::Rejected {
                message: format!("{} (request not sent)", error.message),
            },
            Sent::Failed {
                dispatched: true,
                error,
            } => CommentResult::Unknown {
                message: format!("{}; {unsure}", error.message),
            },
            Sent::Response(raw) if (200..300).contains(&raw.status) => {
                if raw.text.trim().is_empty() {
                    return CommentResult::Created { comment: None };
                }
                match parse_json(&raw, &what).and_then(|v| parse_comment(&v)) {
                    Ok(comment) => CommentResult::Created {
                        comment: Some(comment),
                    },
                    Err(error) => CommentResult::Unknown {
                        message: format!("{}; {unsure}", error.message),
                    },
                }
            }
            Sent::Response(raw) if (400..500).contains(&raw.status) => CommentResult::Rejected {
                message: status_error(&raw, &what).message,
            },
            Sent::Response(raw) => CommentResult::Unknown {
                message: format!("{}; {unsure}", status_error(&raw, &what).message),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
    use std::time::Duration;

    const TOKEN: &str = "tok-SECRET-12345";
    const EMAIL: &str = "you@example.com";

    #[derive(Clone, Debug)]
    struct Rec {
        method: String,
        path: String,
        query: String,
        authorization: Option<String>,
        body: String,
    }

    struct Reply {
        status: u16,
        content_type: &'static str,
        body: String,
        headers: Vec<(String, String)>,
        delay: Option<Duration>,
        drop: bool,
    }

    fn reply(status: u16, body: Value) -> Reply {
        Reply {
            status,
            content_type: "application/json",
            body: body.to_string(),
            headers: Vec::new(),
            delay: None,
            drop: false,
        }
    }

    fn raw_reply(status: u16, content_type: &'static str, body: &str) -> Reply {
        Reply {
            content_type,
            body: body.to_string(),
            ..reply(status, Value::Null)
        }
    }

    struct Server {
        url: String,
        requests: Arc<Mutex<Vec<Rec>>>,
    }

    impl Server {
        fn requests(&self) -> Vec<Rec> {
            self.requests
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }
    }

    fn serve(handler: impl Fn(&Rec) -> Reply + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<Rec>>> = Arc::default();
        let recorded = requests.clone();
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let recorded = recorded.clone();
                let handler = handler.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let target = parts.next().unwrap_or("").to_string();
                    let (path, query) = match target.split_once('?') {
                        Some((p, q)) => (p.to_string(), format!("?{q}")),
                        None => (target.clone(), String::new()),
                    };
                    let mut length = 0;
                    let mut authorization = None;
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                            break;
                        }
                        if let Some((name, value)) = h.split_once(':') {
                            let name = name.trim().to_ascii_lowercase();
                            if name == "content-length" {
                                length = value.trim().parse().unwrap_or(0);
                            } else if name == "authorization" {
                                authorization = Some(value.trim().to_string());
                            }
                        }
                    }
                    let mut body = vec![0; length];
                    let _ = reader.read_exact(&mut body);
                    let rec = Rec {
                        method,
                        path,
                        query,
                        authorization,
                        body: String::from_utf8_lossy(&body).into_owned(),
                    };
                    recorded
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(rec.clone());
                    let r = handler(&rec);
                    if let Some(d) = r.delay {
                        std::thread::sleep(d);
                    }
                    if r.drop {
                        return;
                    }
                    let mut out = format!(
                        "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        r.status,
                        r.content_type,
                        r.body.len()
                    );
                    for (n, v) in &r.headers {
                        out.push_str(&format!("{n}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    out.push_str(&r.body);
                    let _ = stream.write_all(out.as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        Server { url, requests }
    }

    fn config(base_url: &str) -> JiraConfig {
        JiraConfig {
            base_url: base_url.to_string(),
            deployment: Deployment::Cloud,
            auth: AuthKind::Basic,
            email: Some(EMAIL.to_string()),
            token_command: vec!["printf".into(), "%s\n".into(), TOKEN.into()],
            max_results: 100,
            timeout: Duration::from_secs(2),
        }
    }

    fn client(base_url: &str) -> JiraClient {
        JiraClient::new(&config(base_url))
    }

    fn client_with(base_url: &str, patch: impl FnOnce(&mut JiraConfig)) -> JiraClient {
        let mut cfg = config(base_url);
        patch(&mut cfg);
        JiraClient::new(&cfg)
    }

    fn body_field(body: &str, name: &str) -> Value {
        serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get(name).cloned())
            .unwrap_or(Value::Null)
    }

    fn issue(key: &str, description: Value) -> Value {
        json!({
            "key": key,
            "fields": {
                "summary": format!("Summary {key}"),
                "status": { "id": "1", "name": "To Do" },
                "issuetype": { "name": "Task" },
                "assignee": { "displayName": "Kim" },
                "updated": "2026-10-01T00:00:00.000+0000",
                "description": description,
            }
        })
    }

    fn keys(page: &Page<Issue>) -> Vec<&str> {
        page.items.iter().map(|i| i.key.as_str()).collect()
    }

    #[test]
    fn cloud_follows_two_token_pages() {
        let server = serve(|r| {
            if body_field(&r.body, "nextPageToken").is_null() {
                reply(
                    200,
                    json!({ "issues": [issue("A-1", Value::Null), issue("A-2", Value::Null)], "nextPageToken": "p2", "isLast": false }),
                )
            } else {
                reply(
                    200,
                    json!({ "issues": [issue("A-3", Value::Null)], "isLast": true }),
                )
            }
        });
        let page = client(&server.url).search("project = A").unwrap();
        assert_eq!(keys(&page), ["A-1", "A-2", "A-3"]);
        assert!(!page.has_more);
        assert_eq!(page.items[0].url, format!("{}/browse/A-1", server.url));
        let reqs = server.requests();
        assert!(
            reqs.iter()
                .all(|r| r.method == "POST" && r.path == "/rest/api/3/search/jql")
        );
        assert_eq!(body_field(&reqs[0].body, "fields"), json!(FIELDS));
    }

    #[test]
    fn cloud_cap_reached_reports_has_more() {
        let n = Arc::new(AtomicUsize::new(0));
        let server = serve(move |_| {
            let n = n.fetch_add(1, Ordering::SeqCst) + 1;
            reply(
                200,
                json!({ "issues": [issue(&format!("A-{}", 2 * n - 1), Value::Null), issue(&format!("A-{}", 2 * n), Value::Null)], "nextPageToken": format!("t{n}") }),
            )
        });
        let page = client_with(&server.url, |c| c.max_results = 2)
            .search("x")
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.has_more);
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn cloud_repeated_token_rejects() {
        let n = Arc::new(AtomicUsize::new(0));
        let server = serve(move |_| {
            let n = n.fetch_add(1, Ordering::SeqCst) + 1;
            reply(
                200,
                json!({ "issues": [issue(&format!("A-{n}"), Value::Null)], "nextPageToken": "same" }),
            )
        });
        let e = client(&server.url).search("x").unwrap_err();
        assert!(e.message.contains("repeated"), "{e}");
    }

    #[test]
    fn cloud_duplicate_key_rejects() {
        let server = serve(|r| {
            if body_field(&r.body, "nextPageToken").is_null() {
                reply(
                    200,
                    json!({ "issues": [issue("A-1", Value::Null)], "nextPageToken": "p2" }),
                )
            } else {
                reply(
                    200,
                    json!({ "issues": [issue("A-1", Value::Null)], "isLast": true }),
                )
            }
        });
        let e = client(&server.url).search("x").unwrap_err();
        assert!(e.message.contains("duplicate"), "{e}");
    }

    #[test]
    fn cloud_later_page_500_rejects() {
        let server = serve(|r| {
            if body_field(&r.body, "nextPageToken").is_null() {
                reply(
                    200,
                    json!({ "issues": [issue("A-1", Value::Null)], "nextPageToken": "p2" }),
                )
            } else {
                reply(500, json!({ "errorMessages": ["boom"] }))
            }
        });
        let e = client(&server.url).search("x").unwrap_err();
        assert!(e.message.contains("HTTP 500"), "{e}");
    }

    #[test]
    fn dc_follows_two_offset_pages() {
        let server = serve(|r| {
            let start = body_field(&r.body, "startAt");
            let issues = if start == json!(0) {
                json!([issue("D-1", Value::Null), issue("D-2", Value::Null)])
            } else {
                json!([issue("D-3", Value::Null)])
            };
            reply(
                200,
                json!({ "startAt": start, "total": 3, "issues": issues }),
            )
        });
        let page = client_with(&server.url, |c| c.deployment = Deployment::Dc)
            .search("x")
            .unwrap();
        assert_eq!(keys(&page), ["D-1", "D-2", "D-3"]);
        assert_eq!(page.total, Some(3));
        assert!(!page.has_more);
        let paths: Vec<String> = server.requests().into_iter().map(|r| r.path).collect();
        assert_eq!(paths, ["/rest/api/2/search", "/rest/api/2/search"]);
    }

    #[test]
    fn dc_empty_page_before_total_rejects() {
        let server = serve(|r| {
            let start = body_field(&r.body, "startAt");
            let issues = if start == json!(0) {
                json!([issue("D-1", Value::Null)])
            } else {
                json!([])
            };
            reply(
                200,
                json!({ "startAt": start, "total": 5, "issues": issues }),
            )
        });
        let e = client_with(&server.url, |c| c.deployment = Deployment::Dc)
            .search("x")
            .unwrap_err();
        assert!(e.message.contains("empty page"), "{e}");
    }

    #[test]
    fn redirect_is_refused() {
        let target = serve(|_| reply(200, json!({ "issues": [] })));
        let location = format!("{}/steal", target.url);
        let origin = serve(move |_| {
            let mut r = raw_reply(302, "text/plain", "");
            r.headers.push(("Location".into(), location.clone()));
            r
        });
        assert!(client(&origin.url).search("x").is_err());
        assert!(target.requests().is_empty());
    }

    #[test]
    fn token_only_in_authorization_header() {
        let server = serve(|_| {
            reply(
                200,
                json!({ "issues": [issue("A-1", Value::Null)], "isLast": true }),
            )
        });
        client(&server.url).search("x").unwrap();
        let req = &server.requests()[0];
        let expected = base64::engine::general_purpose::STANDARD.encode(format!("{EMAIL}:{TOKEN}"));
        assert_eq!(
            req.authorization.as_deref(),
            Some(format!("Basic {expected}").as_str())
        );
        assert!(!req.body.contains(TOKEN));
        assert!(!req.query.contains(TOKEN));
    }

    #[test]
    fn bearer_sends_raw_token() {
        let server = serve(|_| reply(200, json!({ "issues": [], "isLast": true })));
        client_with(&server.url, |c| {
            c.auth = AuthKind::Bearer;
            c.email = None;
        })
        .search("x")
        .unwrap();
        assert_eq!(
            server.requests()[0].authorization.as_deref(),
            Some(format!("Bearer {TOKEN}").as_str())
        );
    }

    #[test]
    fn token_command_failure_hides_output_and_sends_nothing() {
        let server = serve(|_| reply(200, json!({ "issues": [] })));
        let jira = client_with(&server.url, |c| {
            c.token_command = vec![
                "sh".into(),
                "-c".into(),
                "echo SECRET-OUT; echo SECRET-ERR >&2; exit 1".into(),
            ];
        });
        let e = jira.search("x").unwrap_err();
        assert_eq!(e.kind, JiraErrorKind::Credential);
        assert!(!e.message.contains("SECRET"));
        assert!(server.requests().is_empty());
        // Comment writes fail closed as rejected, not unknown, when nothing was sent.
        assert!(matches!(
            jira.add_comment("A-1", "hi"),
            CommentResult::Rejected { .. }
        ));
        assert!(server.requests().is_empty());
    }

    #[test]
    fn server_error_echoing_token_is_redacted_and_truncated() {
        let server = serve(|_| {
            reply(
                400,
                json!({ "errorMessages": [format!("bad {TOKEN} \u{1b}[31mred\u{1b}[0m {}", "x".repeat(1000))] }),
            )
        });
        let e = client(&server.url).issue("a-1").unwrap_err();
        assert!(!e.message.contains(TOKEN));
        assert!(!e.message.contains('\u{1b}'));
        assert!(e.message.chars().count() < 500);
    }

    #[test]
    fn server_error_echoing_basic_credential_is_redacted() {
        // Chosen so the base64 payload contains "/" (exercises the JSON `\/` escape form).
        let token = "k??~~?>";
        let pair = format!("{EMAIL}:{token}");
        let b64 = base64::engine::general_purpose::STANDARD.encode(&pair);
        assert!(b64.contains('/'));
        let cases = [
            (format!("Basic {b64}"), 400),
            (b64.clone(), 500),
            (b64.replace('/', "\\/"), 400),
            (pair.clone(), 500),
        ];
        for (echoed, status) in cases {
            let body = format!(
                "{{\"errorMessages\":[\"{} echo {echoed} end\"]}}",
                "y".repeat(290)
            );
            let server = serve(move |_| Reply {
                body: body.clone(),
                ..reply(status, Value::Null)
            });
            let jira = client_with(&server.url, |c| {
                c.token_command = vec!["printf".into(), "%s\n".into(), token.into()]
            });
            let e = jira.issue("a-1").unwrap_err();
            for secret in [
                token.to_string(),
                b64.clone(),
                b64[..12].to_string(),
                pair.clone(),
                b64.replace('/', "\\/"),
            ] {
                assert!(
                    !e.message.contains(&secret),
                    "{} leaked {secret}",
                    e.message
                );
            }
        }
    }

    #[test]
    fn unauthorized_not_found_and_html_are_distinguished() {
        let status = Arc::new(AtomicU16::new(401));
        let s = status.clone();
        let server = serve(move |_| match s.load(Ordering::SeqCst) {
            200 => raw_reply(200, "text/html", "<!DOCTYPE html><html></html>"),
            st => reply(st, json!({})),
        });
        let jira = client(&server.url);
        let e = jira.issue("A-1").unwrap_err();
        assert_eq!(e.kind, JiraErrorKind::Unauthorized);
        assert!(e.message.contains("401"));
        status.store(404, Ordering::SeqCst);
        assert!(
            jira.issue("A-1")
                .unwrap_err()
                .message
                .contains("not found or not visible")
        );
        status.store(200, Ordering::SeqCst);
        let e = jira.issue("A-1").unwrap_err();
        assert_eq!(e.kind, JiraErrorKind::Html);
    }

    #[test]
    fn issue_normalizes_key_and_rejects_mismatch() {
        let server = serve(|r| {
            if r.path.ends_with("ABC-7") {
                reply(
                    200,
                    issue(
                        "ABC-7",
                        json!({ "type": "doc", "version": 1, "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "설명" }] }] }),
                    ),
                )
            } else {
                reply(200, issue("ABC-9", Value::Null))
            }
        });
        let jira = client(&server.url);
        let got = jira.issue("abc-7").unwrap();
        assert_eq!(got.key, "ABC-7");
        assert_eq!(got.description, "설명");
        assert_eq!(server.requests()[0].path, "/rest/api/3/issue/ABC-7");
        assert!(jira.issue("ABC-8").unwrap_err().message.contains("ABC-9"));
    }

    #[test]
    fn comments_limit_zero_and_newest_first() {
        let all = json!([
            { "id": "1", "author": { "displayName": "a" }, "created": "2026-01-01T00:00:00.000+0000", "body": "old" },
            { "id": "3", "author": { "displayName": "b" }, "created": "2026-03-01T00:00:00.000+0000", "body": "new" },
            { "id": "2", "author": { "displayName": "c" }, "created": "2026-02-01T00:00:00.000+0000", "body": "mid" },
        ]);
        let server = serve(move |r| {
            let param = |name: &str| -> usize {
                r.query
                    .trim_start_matches('?')
                    .split('&')
                    .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            };
            let (start, max) = (param("startAt"), param("maxResults"));
            let list = all.as_array().unwrap();
            let slice: Vec<Value> = list.iter().skip(start).take(max).cloned().collect();
            reply(
                200,
                json!({ "startAt": start, "maxResults": max, "total": list.len(), "comments": slice }),
            )
        });
        let jira = client(&server.url);
        let empty = jira.comments("A-1", 0).unwrap();
        assert!(empty.items.is_empty() && !empty.has_more);
        assert!(server.requests().is_empty());
        let full = jira.comments("A-1", 10).unwrap();
        let ids: Vec<&str> = full.items.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["3", "2", "1"]);
        assert!(!full.has_more);
        let partial = jira.comments("A-1", 2).unwrap();
        assert_eq!(partial.items.len(), 2);
        assert!(partial.has_more);
    }

    #[test]
    fn transitions_map_screen_and_field_requirements() {
        let server = serve(|_| {
            reply(
                200,
                json!({ "transitions": [
                { "id": "31", "name": "Start", "to": { "id": "3", "name": "In Progress" }, "hasScreen": false, "isAvailable": true },
                { "id": "41", "name": "Resolve", "to": { "id": "5", "name": "Done" }, "hasScreen": false, "isAvailable": false, "fields": { "resolution": { "required": true } } },
                { "id": "51", "name": "Reopen", "to": { "id": "1", "name": "To Do" } },
            ] }),
            )
        });
        let list = client_with(&server.url, |c| c.deployment = Deployment::Dc)
            .transitions("A-1")
            .unwrap();
        let st = |id: &str, name: &str| StatusRef {
            id: id.into(),
            name: name.into(),
        };
        assert_eq!(
            list,
            vec![
                Transition {
                    id: "31".into(),
                    name: "Start".into(),
                    to: st("3", "In Progress"),
                    has_screen: false,
                    requires_fields: false,
                    is_available: Some(true)
                },
                Transition {
                    id: "41".into(),
                    name: "Resolve".into(),
                    to: st("5", "Done"),
                    has_screen: false,
                    requires_fields: true,
                    is_available: Some(false)
                },
                Transition {
                    id: "51".into(),
                    name: "Reopen".into(),
                    to: st("1", "To Do"),
                    has_screen: true,
                    requires_fields: false,
                    is_available: None
                },
            ]
        );
        assert_eq!(server.requests()[0].query, "?expand=transitions.fields");
    }

    #[test]
    fn post_transition_classifies_204_400_500() {
        let status = Arc::new(AtomicU16::new(204));
        let s = status.clone();
        let server = serve(move |_| match s.load(Ordering::SeqCst) {
            204 => raw_reply(204, "text/plain", ""),
            st => reply(st, json!({ "errorMessages": ["nope"] })),
        });
        let jira = client(&server.url);
        assert_eq!(
            jira.post_transition("A-1", "31"),
            TransitionResult::Acknowledged {
                observed_status: None,
                verified: false
            }
        );
        assert_eq!(
            serde_json::from_str::<Value>(&server.requests()[0].body).unwrap(),
            json!({ "transition": { "id": "31" } })
        );
        status.store(400, Ordering::SeqCst);
        match jira.post_transition("A-1", "31") {
            TransitionResult::Rejected { message } => assert!(message.contains("nope")),
            other => panic!("{other:?}"),
        }
        status.store(500, Ordering::SeqCst);
        assert!(matches!(
            jira.post_transition("A-1", "31"),
            TransitionResult::Unknown { .. }
        ));
        assert_eq!(server.requests().len(), 3);
    }

    #[test]
    fn post_transition_timeout_after_dispatch_is_unknown() {
        let server = serve(|_| Reply {
            delay: Some(Duration::from_millis(1500)),
            ..raw_reply(204, "text/plain", "")
        });
        let jira = client_with(&server.url, |c| c.timeout = Duration::from_millis(500));
        assert!(matches!(
            jira.post_transition("A-1", "31"),
            TransitionResult::Unknown { .. }
        ));
        assert_eq!(server.requests().len(), 1);
    }

    fn created_comment() -> Value {
        json!({ "id": "10001", "author": { "displayName": "Kim" }, "created": "2026-10-13T09:00:00.000+0900", "body": "hello" })
    }

    #[test]
    fn add_comment_cloud_sends_adf_and_parses_created() {
        let server = serve(|_| reply(201, created_comment()));
        let result = client(&server.url).add_comment("k-1", "line one\nline two");
        let expected = Comment {
            id: "10001".into(),
            author: "Kim".into(),
            created: "2026-10-13T09:00:00.000+0900".into(),
            body: "hello".into(),
        };
        assert_eq!(
            result,
            CommentResult::Created {
                comment: Some(expected)
            }
        );
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/rest/api/3/issue/K-1/comment");
        assert_eq!(
            body_field(&reqs[0].body, "body"),
            plain_to_adf("line one\nline two")
        );
    }

    #[test]
    fn add_comment_dc_sends_plain_string() {
        let server = serve(|_| reply(201, created_comment()));
        let result = client_with(&server.url, |c| c.deployment = Deployment::Dc)
            .add_comment("K-1", "plain text");
        assert!(matches!(
            result,
            CommentResult::Created { comment: Some(_) }
        ));
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].path, "/rest/api/2/issue/K-1/comment");
        assert_eq!(body_field(&reqs[0].body, "body"), json!("plain text"));
    }

    #[test]
    fn add_comment_400_is_rejected_with_server_message() {
        let server = serve(|_| {
            reply(
                400,
                json!({ "errors": { "comment": "Comment body can not be empty!" } }),
            )
        });
        match client(&server.url).add_comment("K-1", "x") {
            CommentResult::Rejected { message } => assert!(
                message.contains("Comment body can not be empty!"),
                "{message}"
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn add_comment_500_is_unknown() {
        let server = serve(|_| reply(500, json!({ "errorMessages": ["boom"] })));
        assert!(matches!(
            client(&server.url).add_comment("K-1", "x"),
            CommentResult::Unknown { .. }
        ));
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn add_comment_dropped_connection_is_unknown() {
        let server = serve(|_| Reply {
            drop: true,
            ..reply(201, Value::Null)
        });
        assert!(matches!(
            client(&server.url).add_comment("K-1", "x"),
            CommentResult::Unknown { .. }
        ));
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn comment_order_handles_offsets_and_ids() {
        let c = |id: &str, created: &str| Comment {
            id: id.into(),
            author: String::new(),
            created: created.into(),
            body: String::new(),
        };
        let mut list = [
            c("9", "2026-01-01T10:00:00.000+0900"),
            c("10", "2026-01-01T01:00:00.000+0000"),
            c("8", "2026-01-01T02:00:00.000+0000"),
        ];
        list.sort_by(compare_comments_newest_first);
        let ids: Vec<&str> = list.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["8", "10", "9"]);
    }
}
