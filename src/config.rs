//! User configuration loading and strict validation. Environment variables
//! select the file location and never supply credentials.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::model::{
    AuthKind, Backend, Config, Deployment, Filter, HandoffConfig, JiraConfig, OmpConfig,
    WorktreeConfig, home_dir,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The config file does not exist at this path.
    Missing(PathBuf),
    /// Parse or validation failure, prefixed with a `$.`-style location.
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Missing(path) => write!(
                f,
                "jira-tern config not found: {} (see config.example.json in the jira-tern repository)",
                path.display()
            ),
            ConfigError::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

type Result<T, E = ConfigError> = std::result::Result<T, E>;

/// Absolute `$JIRA_TERN_CONFIG`, else `${XDG_CONFIG_HOME:-~/.config}/jira-tern/config.json`.
pub fn config_path() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("JIRA_TERN_CONFIG").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(&value);
        if !path.is_absolute() {
            return Err(ConfigError::Invalid(format!(
                "JIRA_TERN_CONFIG must be an absolute path: {:?}",
                value.to_string_lossy()
            )));
        }
        return Ok(path);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home_dir().join(".config"));
    Ok(base.join("jira-tern").join("config.json"))
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    load_from(&path)
}

fn load_from(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfigError::Missing(path.to_owned()));
        }
        Err(e) => return Err(ConfigError::Invalid(format!("{}: {e}", path.display()))),
    };
    parse(&text).map_err(|e| match e {
        ConfigError::Invalid(msg) if msg.starts_with("invalid JSON") => {
            ConfigError::Invalid(format!("{}: {msg}", path.display()))
        }
        other => other,
    })
}

/// Parses and validates a config document, filling defaults.
pub fn parse(json: &str) -> Result<Config> {
    let raw: Value = serde_json::from_str(json)
        .map_err(|e| ConfigError::Invalid(format!("invalid JSON: {e}")))?;
    parse_value(&raw)
}

fn fail<T>(path: &str, message: &str) -> Result<T> {
    Err(ConfigError::Invalid(format!("{path}: {message}")))
}

fn object<'a>(value: &'a Value, path: &str, keys: &[&str]) -> Result<&'a Map<String, Value>> {
    let Some(map) = value.as_object() else {
        return fail(path, "must be an object");
    };
    for key in map.keys() {
        if !keys.contains(&key.as_str()) {
            return fail(&format!("{path}.{key}"), "unknown key");
        }
    }
    Ok(map)
}

fn optional_object<'a>(
    value: Option<&'a Value>,
    path: &str,
    keys: &[&str],
) -> Result<Option<&'a Map<String, Value>>> {
    value.map(|v| object(v, path, keys)).transpose()
}

fn non_empty_string(value: Option<&Value>, path: &str) -> Result<String> {
    match value.and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Ok(s.to_owned()),
        _ => fail(path, "must be a non-empty string"),
    }
}

fn one_of<'a>(value: &Value, path: &str, allowed: &[&'a str]) -> Result<&'a str> {
    match allowed.iter().find(|a| value.as_str() == Some(**a)) {
        Some(a) => Ok(*a),
        None => {
            let list = allowed
                .iter()
                .map(|a| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(", ");
            fail(path, &format!("must be one of {list}"))
        }
    }
}

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

fn integer(value: Option<&Value>, path: &str, fallback: u64, min: u64, max: u64) -> Result<u64> {
    let Some(value) = value else {
        return Ok(fallback);
    };
    let number = match value {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else if let Some(f) = n.as_f64() {
                (f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER as f64).then_some(f as i64)
            } else {
                None
            }
        }
        _ => None,
    };
    let Some(number) = number.filter(|n| n.unsigned_abs() <= MAX_SAFE_INTEGER) else {
        return fail(path, "must be an integer");
    };
    if number < min as i64 || number > max as i64 {
        return fail(path, &format!("must be between {min} and {max}"));
    }
    Ok(number as u64)
}

fn nullable_base(value: Option<&Value>, path: &str) -> Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(v) => non_empty_string(Some(v), path).map(Some),
    }
}

fn expand_path(value: Option<&Value>, path: &str) -> Result<String> {
    let text = non_empty_string(value, path)?;
    if let Some(rest) = text.strip_prefix("~/") {
        return Ok(home_dir().join(rest).to_string_lossy().into_owned());
    }
    if !Path::new(&text).is_absolute() {
        return fail(path, "must be an absolute path or start with ~/");
    }
    Ok(text)
}

fn normalize_base_url(value: Option<&Value>, path: &str) -> Result<String> {
    let text = non_empty_string(value, path)?;
    let (scheme, rest) = if let Some(rest) = strip_prefix_ci(&text, "https://") {
        ("https", rest)
    } else if let Some(rest) = strip_prefix_ci(&text, "http://") {
        ("http", rest)
    } else if text.contains("://") || text.contains(':') {
        return fail(path, "must use https");
    } else {
        return fail(path, "must be a valid URL");
    };
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return fail(path, "must be a valid URL");
    }
    if text.contains('@') {
        return fail(path, "must not contain userinfo");
    }
    if text.contains('?') {
        return fail(path, "must not contain a query");
    }
    if text.contains('#') {
        return fail(path, "must not contain a fragment");
    }
    let (authority, pathname) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let authority = authority.to_ascii_lowercase();
    let Some((host, port)) = split_host_port(&authority) else {
        return fail(path, "must be a valid URL");
    };
    if host.is_empty() {
        return fail(path, "must be a valid URL");
    }
    if scheme == "http" && !matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
        return fail(
            path,
            "must use https (http is allowed only for localhost, 127.0.0.1, [::1])",
        );
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    let origin = match port {
        Some(p) if p != default_port => format!("{scheme}://{host}:{p}"),
        _ => format!("{scheme}://{host}"),
    };
    Ok(format!("{origin}{}", pathname.trim_end_matches('/')))
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Splits `host[:port]` (IPv6 hosts bracketed); `None` for malformed input.
fn split_host_port(authority: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']')?;
        let (host, tail) = authority.split_at(end + 1);
        match tail {
            "" => (host, None),
            _ => (host, Some(tail.strip_prefix(':')?)),
        }
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let valid_host = host.starts_with('[')
        || host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    if !valid_host {
        return None;
    }
    match port {
        None | Some("") => Some((host, None)),
        Some(p) if p.chars().all(|c| c.is_ascii_digit()) => Some((host, Some(p.parse().ok()?))),
        Some(_) => None,
    }
}

fn is_project_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_value(raw: &Value) -> Result<Config> {
    let root = object(
        raw,
        "$",
        &["jira", "filters", "search", "worktree", "handoff", "omp"],
    )?;

    let Some(jira_raw) = root.get("jira") else {
        return fail("$.jira", "is required");
    };
    let jira = object(
        jira_raw,
        "$.jira",
        &[
            "baseUrl",
            "deployment",
            "auth",
            "email",
            "tokenCommand",
            "maxResults",
            "timeoutMs",
        ],
    )?;
    if !jira.contains_key("baseUrl") {
        return fail("$.jira.baseUrl", "is required");
    }
    let base_url = normalize_base_url(jira.get("baseUrl"), "$.jira.baseUrl")?;
    let Some(deployment) = jira.get("deployment") else {
        return fail("$.jira.deployment", "is required");
    };
    let deployment = match one_of(deployment, "$.jira.deployment", &["cloud", "dc"])? {
        "cloud" => Deployment::Cloud,
        _ => Deployment::Dc,
    };
    let Some(auth) = jira.get("auth") else {
        return fail("$.jira.auth", "is required");
    };
    let auth = match one_of(auth, "$.jira.auth", &["basic", "bearer"])? {
        "basic" => AuthKind::Basic,
        _ => AuthKind::Bearer,
    };
    let email = match jira.get("email") {
        None | Some(Value::Null) => None,
        Some(v) => Some(non_empty_string(Some(v), "$.jira.email")?),
    };
    if auth == AuthKind::Basic && email.is_none() {
        return fail("$.jira.email", "is required when auth is \"basic\"");
    }
    let Some(token_raw) = jira.get("tokenCommand") else {
        return fail("$.jira.tokenCommand", "is required");
    };
    let token_items = match token_raw.as_array() {
        Some(items) if !items.is_empty() => items,
        _ => {
            return fail(
                "$.jira.tokenCommand",
                "must be a non-empty array of strings (argv)",
            );
        }
    };
    let mut token_command = Vec::with_capacity(token_items.len());
    for (i, arg) in token_items.iter().enumerate() {
        let Some(arg) = arg.as_str() else {
            return fail(&format!("$.jira.tokenCommand[{i}]"), "must be a string");
        };
        token_command.push(arg.to_owned());
    }
    if token_command[0].is_empty() {
        return fail("$.jira.tokenCommand[0]", "must be a non-empty executable");
    }

    let mut filters = Vec::new();
    if let Some(raw_filters) = root.get("filters") {
        let Some(entries) = raw_filters.as_array() else {
            return fail("$.filters", "must be an array");
        };
        for (i, entry) in entries.iter().enumerate() {
            let f = object(entry, &format!("$.filters[{i}]"), &["name", "jql"])?;
            filters.push(Filter {
                name: non_empty_string(f.get("name"), &format!("$.filters[{i}].name"))?,
                jql: non_empty_string(f.get("jql"), &format!("$.filters[{i}].jql"))?,
            });
        }
    }

    let search = optional_object(root.get("search"), "$.search", &["jql"])?;
    let search_jql = match search.and_then(|s| s.get("jql")) {
        None => "text ~ \"{query}\" ORDER BY updated DESC".to_owned(),
        Some(v) => non_empty_string(Some(v), "$.search.jql")?,
    };
    if search_jql.matches("{query}").count() != 1 {
        return fail("$.search.jql", "must contain {query} exactly once");
    }

    let worktree = optional_object(
        root.get("worktree"),
        "$.worktree",
        &["repos", "prefer", "root", "gitBase", "jjBase"],
    )?;
    let wt = |key: &str| worktree.and_then(|w| w.get(key));
    let mut repos = BTreeMap::new();
    if let Some(raw_repos) = wt("repos") {
        let Some(map) = raw_repos.as_object() else {
            return fail("$.worktree.repos", "must be an object");
        };
        for (project, repo_path) in map {
            let path = format!("$.worktree.repos.{project}");
            if !is_project_key(project) {
                return fail(&path, "key must be a Jira project key");
            }
            let upper = project.to_ascii_uppercase();
            if repos.contains_key(&upper) {
                return fail(&path, "duplicate project key");
            }
            let expanded = expand_path(Some(repo_path), &path)?;
            repos.insert(upper, expanded);
        }
    }
    let prefer = match wt("prefer") {
        None => Backend::Jj,
        Some(v) => match one_of(v, "$.worktree.prefer", &["jj", "git"])? {
            "jj" => Backend::Jj,
            _ => Backend::Git,
        },
    };
    let default_root = Value::String("~/.local/share/jira-tern/checkouts".to_owned());
    let root_path = expand_path(Some(wt("root").unwrap_or(&default_root)), "$.worktree.root")?;
    let git_base = nullable_base(wt("gitBase"), "$.worktree.gitBase")?;
    let jj_base = nullable_base(wt("jjBase"), "$.worktree.jjBase")?;

    let handoff = optional_object(
        root.get("handoff"),
        "$.handoff",
        &[
            "maxComments",
            "maxDescriptionChars",
            "maxCommentChars",
            "maxChars",
        ],
    )?;
    let ho = |key: &str| handoff.and_then(|h| h.get(key));
    let omp = optional_object(root.get("omp"), "$.omp", &["binary", "profile"])?;
    let om = |key: &str| omp.and_then(|o| o.get(key));

    let max_results = integer(jira.get("maxResults"), "$.jira.maxResults", 50, 1, 1000)?;
    let timeout_ms = integer(
        jira.get("timeoutMs"),
        "$.jira.timeoutMs",
        20_000,
        1000,
        120_000,
    )?;
    let handoff = HandoffConfig {
        max_comments: integer(ho("maxComments"), "$.handoff.maxComments", 50, 0, 1000)? as usize,
        max_description_chars: integer(
            ho("maxDescriptionChars"),
            "$.handoff.maxDescriptionChars",
            6000,
            1,
            MAX_SAFE_INTEGER,
        )? as usize,
        max_comment_chars: integer(
            ho("maxCommentChars"),
            "$.handoff.maxCommentChars",
            12_000,
            1,
            MAX_SAFE_INTEGER,
        )? as usize,
        max_chars: integer(ho("maxChars"), "$.handoff.maxChars", 64_000, 1024, 262_144)? as usize,
    };
    let omp = OmpConfig {
        binary: match om("binary") {
            None => "omp".to_owned(),
            Some(v) => non_empty_string(Some(v), "$.omp.binary")?,
        },
        profile: match om("profile") {
            None => "default".to_owned(),
            Some(v) => non_empty_string(Some(v), "$.omp.profile")?,
        },
    };

    Ok(Config {
        jira: JiraConfig {
            base_url,
            deployment,
            auth,
            email,
            token_command,
            max_results: max_results as usize,
            timeout: Duration::from_millis(timeout_ms),
        },
        filters,
        search_jql,
        worktree: WorktreeConfig {
            repos: repos.into_iter().collect(),
            prefer,
            root: root_path,
            git_base,
            jj_base,
        },
        handoff,
        omp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal() -> Value {
        json!({"jira": {
            "baseUrl": "https://example.atlassian.net/",
            "deployment": "cloud",
            "auth": "basic",
            "email": "me@example.com",
            "tokenCommand": ["printf", "token"]
        }})
    }

    fn with_jira(patch: Value) -> Value {
        let mut doc = minimal();
        let jira = doc["jira"].as_object_mut().unwrap();
        for (k, v) in patch.as_object().unwrap() {
            if v.is_null() {
                jira.remove(k);
            } else {
                jira.insert(k.clone(), v.clone());
            }
        }
        doc
    }

    fn with_root(key: &str, value: Value) -> Value {
        let mut doc = minimal();
        doc.as_object_mut().unwrap().insert(key.to_owned(), value);
        doc
    }

    fn load(doc: &Value) -> Result<Config> {
        parse(&doc.to_string())
    }

    #[test]
    fn minimal_config_fills_defaults() {
        let c = load(&minimal()).unwrap();
        assert_eq!(c.jira.base_url, "https://example.atlassian.net");
        assert_eq!(c.jira.max_results, 50);
        assert_eq!(c.jira.timeout, Duration::from_millis(20_000));
        assert!(c.filters.is_empty());
        assert!(c.search_jql.contains("{query}"));
        assert_eq!(
            c.worktree,
            WorktreeConfig {
                repos: vec![],
                prefer: Backend::Jj,
                root: home_dir()
                    .join(".local/share/jira-tern/checkouts")
                    .to_string_lossy()
                    .into_owned(),
                git_base: None,
                jj_base: None,
            }
        );
        assert_eq!(
            c.handoff,
            HandoffConfig {
                max_comments: 50,
                max_description_chars: 6000,
                max_comment_chars: 12000,
                max_chars: 64000
            }
        );
        assert_eq!(
            c.omp,
            OmpConfig {
                binary: "omp".into(),
                profile: "default".into()
            }
        );
    }

    #[test]
    fn context_path_preserved_and_loopback_http_allowed() {
        let doc = with_jira(json!({
            "baseUrl": "http://127.0.0.1:8080/jira//", "deployment": "dc", "auth": "bearer", "email": null
        }));
        let c = load(&doc).unwrap();
        assert_eq!(c.jira.base_url, "http://127.0.0.1:8080/jira");
        assert_eq!(c.jira.email, None);
        assert_eq!(c.jira.deployment, Deployment::Dc);
    }

    #[test]
    fn repo_paths_expand_and_sort() {
        let c = load(&with_root(
            "worktree",
            json!({"repos": {"zed": "/z", "ABC": "~/src/abc"}}),
        ))
        .unwrap();
        assert_eq!(
            c.worktree.repos,
            vec![
                (
                    "ABC".to_owned(),
                    home_dir().join("src/abc").to_string_lossy().into_owned()
                ),
                ("ZED".to_owned(), "/z".to_owned()),
            ]
        );
        let err = load(&with_root("worktree", json!({"repos": {"ABC": "src/abc"}}))).unwrap_err();
        assert!(err.to_string().contains("$.worktree.repos.ABC"), "{err}");
    }

    #[test]
    fn rejects_invalid_documents_with_paths() {
        let cases: Vec<(Value, &str)> = vec![
            (with_root("extra", json!(1)), "$.extra"),
            (with_jira(json!({"token": "x"})), "$.jira.token"),
            (with_jira(json!({"maxResults": 1001})), "$.jira.maxResults"),
            (with_jira(json!({"maxResults": 1.5})), "$.jira.maxResults"),
            (with_jira(json!({"timeoutMs": 999})), "$.jira.timeoutMs"),
            (
                with_root("handoff", json!({"maxChars": 100})),
                "$.handoff.maxChars",
            ),
            (with_jira(json!({"email": null})), "$.jira.email"),
            (
                with_root("search", json!({"jql": "project = X"})),
                "$.search.jql",
            ),
            (
                with_root(
                    "search",
                    json!({"jql": "summary ~ \"{query}\" OR text ~ \"{query}\""}),
                ),
                "$.search.jql",
            ),
            (
                with_jira(json!({"baseUrl": "http://jira.example.com"})),
                "$.jira.baseUrl",
            ),
            (
                with_jira(json!({"baseUrl": "https://jira.example.com/?a=1"})),
                "$.jira.baseUrl",
            ),
            (
                with_jira(json!({"baseUrl": "https://jira.example.com/#x"})),
                "$.jira.baseUrl",
            ),
            (
                with_jira(json!({"baseUrl": "https://u:p@jira.example.com"})),
                "$.jira.baseUrl",
            ),
            (
                with_jira(json!({"baseUrl": "ftp://jira.example.com"})),
                "$.jira.baseUrl",
            ),
            (
                with_jira(json!({"tokenCommand": null})),
                "$.jira.tokenCommand",
            ),
            (
                with_jira(json!({"tokenCommand": "security find"})),
                "$.jira.tokenCommand",
            ),
            (
                with_jira(json!({"tokenCommand": []})),
                "$.jira.tokenCommand",
            ),
            (
                with_jira(json!({"tokenCommand": [""]})),
                "$.jira.tokenCommand[0]",
            ),
            (
                with_jira(json!({"tokenCommand": ["a", 1]})),
                "$.jira.tokenCommand[1]",
            ),
            (
                with_jira(json!({"deployment": "server"})),
                "$.jira.deployment",
            ),
            (
                with_root("worktree", json!({"prefer": "hg"})),
                "$.worktree.prefer",
            ),
            (
                with_root("worktree", json!({"gitBase": ""})),
                "$.worktree.gitBase",
            ),
            (
                with_root("worktree", json!({"repos": {"1AB": "/x"}})),
                "$.worktree.repos.1AB",
            ),
            (
                with_root("worktree", json!({"repos": {"abc": "/x", "ABC": "/y"}})),
                "$.worktree.repos.",
            ),
            (
                with_root("filters", json!([{"name": "x"}])),
                "$.filters[0].jql",
            ),
            (json!({}), "$.jira"),
        ];
        for (doc, path) in cases {
            let err = load(&doc).expect_err(path);
            assert!(matches!(err, ConfigError::Invalid(_)));
            assert!(err.to_string().contains(path), "expected {path} in {err}");
        }
    }

    #[test]
    fn invalid_json_and_missing_file() {
        assert!(matches!(parse("{"), Err(ConfigError::Invalid(_))));
        let dir = std::env::temp_dir().join(format!("jira-tern-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("absent.json");
        assert_eq!(
            load_from(&missing).unwrap_err(),
            ConfigError::Missing(missing.clone())
        );
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{").unwrap();
        let err = load_from(&bad).unwrap_err();
        assert!(
            err.to_string().starts_with(&bad.display().to_string()),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
