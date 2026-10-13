//! Native Git worktrees and jj workspaces. Planning is read-only; preparation runs
//! under state locks, records a durable receipt around the single native add, and
//! never resets, forces, fetches or removes anything.

use std::fmt;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model::{
    Backend, Checkout, CheckoutPlan, CheckoutSelection, FsIdentity, NATIVE_ADD_TIMEOUT,
    PreparedCheckout, RUN_TIMEOUT, Repository, WorktreeConfig,
};
use crate::process::{self, RunResult};
use crate::state::{StateError, StateStore, random_id, sha256_hex};

/// Minimum supported jj version (major, minor, patch).
const JJ_MINIMUM: [u64; 3] = [0, 46, 0];
// Each template is one argv element; `\\n` is a literal backslash + n for jj.
const JJ_NAME_TEMPLATE: &str = "json(name) ++ \"\\n\"";
const JJ_CURRENT_TEMPLATE: &str = "if(target.current_working_copy(), json(name) ++ \"\\n\")";
const JJ_COMMIT_TEMPLATE: &str = "commit_id ++ \"\\n\"";
const JJ_PARENTS_TEMPLATE: &str = "parents.map(|c| c.commit_id() ++ \"\\n\").join(\"\")";
/// Read-only jj invocation prefix: never snapshot or update a working copy.
const JJ_READ_PREFIX: [&str; 4] = ["--no-pager", "--color=never", "--ignore-working-copy", "-R"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutError {
    /// A receipt (or missing proof) blocks automatic use. `existing == true` can be
    /// retried with `recovery_acknowledged`; `existing == false` (a New request) must
    /// be reopened through Existing.
    RecoveryRequired {
        name: String,
        path: String,
        existing: bool,
        detail: String,
    },
    Failed(String),
}

impl fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckoutError::RecoveryRequired {
                name,
                path,
                existing,
                detail,
            } => {
                let guidance = if *existing {
                    "inspect that path manually, then explicitly acknowledge recovery to open it (it is not promoted to Ready)"
                } else {
                    "no native add will be retried; inspect that path manually, then select it as Existing and explicitly acknowledge recovery"
                };
                write!(f, "checkout '{name}' at {path}: {detail}; {guidance}")
            }
            CheckoutError::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for CheckoutError {}

impl From<StateError> for CheckoutError {
    fn from(error: StateError) -> Self {
        CheckoutError::Failed(error.to_string())
    }
}

type R<T> = Result<T, CheckoutError>;

fn fail<T>(message: impl Into<String>) -> R<T> {
    Err(CheckoutError::Failed(message.into()))
}

fn recovery(existing: bool, name: &str, path: &str, detail: impl Into<String>) -> CheckoutError {
    CheckoutError::RecoveryRequired {
        name: name.to_string(),
        path: path.to_string(),
        existing,
        detail: detail.into(),
    }
}

/// `(file, args, cwd, timeout)` → result. Production uses `process::run`; tests intercept native adds.
type Runner =
    Arc<dyn Fn(&str, &[String], Option<&Path>, Duration) -> io::Result<RunResult> + Send + Sync>;

fn jj_bin() -> String {
    std::env::var("JJ_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "jj".to_string())
}

fn strip_newline(text: &str) -> &str {
    text.strip_suffix('\n').unwrap_or(text)
}

fn summarize(result: &RunResult) -> String {
    let raw = if result.stderr.trim().is_empty() {
        result.stdout.trim()
    } else {
        result.stderr.trim()
    };
    let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        "(no output)".to_string()
    } else if text.chars().count() > 400 {
        format!("{}…", text.chars().take(400).collect::<String>())
    } else {
        text
    }
}

fn is_within(child: &str, parent: &str) -> bool {
    child == parent
        || child.starts_with(&if parent.ends_with('/') {
            parent.to_string()
        } else {
            format!("{parent}/")
        })
}

/// ASCII `[A-Za-z0-9._-]`; anything else becomes `-`; empty/`.`/`..` become `checkout`.
fn slug(text: &str) -> String {
    let value: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if value.is_empty() || value == "." || value == ".." {
        "checkout".to_string()
    } else {
        value
    }
}

fn sha(text: &str) -> String {
    sha256_hex(text.as_bytes())
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn dirname(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(path_str)
        .unwrap_or_else(|| "/".to_string())
}

fn lstat_opt(target: &Path) -> R<Option<fs::Metadata>> {
    match fs::symlink_metadata(target) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => fail(format!("cannot inspect {}: {error}", target.display())),
    }
}

fn realpath_opt(target: &str) -> Option<String> {
    fs::canonicalize(target).ok().map(|p| path_str(&p))
}

/// Nearest existing ancestor's realpath + remaining (missing) components.
fn canonicalize_missing(target: &Path) -> R<PathBuf> {
    let mut rest = Vec::new();
    let mut current = target.to_path_buf();
    loop {
        if lstat_opt(&current)?.is_some() {
            let real = fs::canonicalize(&current).map_err(|e| {
                CheckoutError::Failed(format!(
                    "cannot resolve {} (dangling symlink?): {e}",
                    current.display()
                ))
            })?;
            if !fs::metadata(&real).map(|m| m.is_dir()).unwrap_or(false) {
                return fail(format!("{} is not a directory", current.display()));
            }
            let mut out = real;
            out.extend(rest.iter().rev());
            return Ok(out);
        }
        let Some(parent) = current.parent().map(Path::to_path_buf) else {
            return fail(format!("no existing ancestor for {}", target.display()));
        };
        if let Some(name) = current.file_name() {
            rest.push(name.to_os_string());
        }
        current = parent;
    }
}

fn fs_identity(root: &str, backend: Backend) -> R<FsIdentity> {
    let root_meta = fs::symlink_metadata(root)
        .map_err(|e| CheckoutError::Failed(format!("cannot inspect {root}: {e}")))?;
    if !root_meta.is_dir() {
        return fail(format!("{root} is not a directory"));
    }
    let marker_path = Path::new(root).join(if backend == Backend::Git {
        ".git"
    } else {
        ".jj"
    });
    let marker = fs::symlink_metadata(&marker_path).map_err(|e| {
        CheckoutError::Failed(format!("cannot inspect {}: {e}", marker_path.display()))
    })?;
    let valid = match backend {
        Backend::Jj => marker.is_dir(),
        Backend::Git => marker.is_dir() || marker.is_file(),
    };
    if !valid {
        return fail(format!("invalid checkout marker {}", marker_path.display()));
    }
    Ok(FsIdentity {
        root_dev: root_meta.dev(),
        root_ino: root_meta.ino(),
        marker_dev: marker.dev(),
        marker_ino: marker.ino(),
    })
}

fn same_identity(a: Option<FsIdentity>, b: FsIdentity) -> bool {
    a == Some(b)
}

fn has_control_chars(text: &str) -> bool {
    text.chars()
        .any(|c| matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}'))
}

fn is_hex(text: &str) -> bool {
    text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_git_commit_id(id: &str) -> bool {
    (id.len() == 40 || id.len() == 64) && is_hex(id)
}

fn is_jj_commit_id(id: &str) -> bool {
    (40..=64).contains(&id.len()) && is_hex(id)
}

/// First `N.N.N` in `text`.
fn parse_version(text: &str) -> Option<([u64; 3], String)> {
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_digit() || (start > 0 && bytes[start - 1].is_ascii_digit()) {
            continue;
        }
        let mut parts = [0u64; 3];
        let mut index = start;
        let mut ok = true;
        for (n, part) in parts.iter_mut().enumerate() {
            let begin = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if index == begin {
                ok = false;
                break;
            }
            *part = text[begin..index].parse().unwrap_or(u64::MAX);
            if n < 2 {
                if index < bytes.len() && bytes[index] == b'.' {
                    index += 1;
                } else {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            return Some((parts, text[start..index].to_string()));
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ReceiptState {
    Pending,
    Failed,
    Ready,
}

impl ReceiptState {
    fn label(self) -> &'static str {
        match self {
            ReceiptState::Pending => "pending",
            ReceiptState::Failed => "failed",
            ReceiptState::Ready => "ready",
        }
    }
}

/// Identity stored as decimal strings (dev/ino may exceed JSON-safe integers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IdentityRecord {
    root_dev: String,
    root_ino: String,
    marker_dev: String,
    marker_ino: String,
}

impl IdentityRecord {
    fn from_identity(id: FsIdentity) -> Self {
        Self {
            root_dev: id.root_dev.to_string(),
            root_ino: id.root_ino.to_string(),
            marker_dev: id.marker_dev.to_string(),
            marker_ino: id.marker_ino.to_string(),
        }
    }

    fn to_identity(&self) -> Option<FsIdentity> {
        Some(FsIdentity {
            root_dev: self.root_dev.parse().ok()?,
            root_ino: self.root_ino.parse().ok()?,
            marker_dev: self.marker_dev.parse().ok()?,
            marker_ino: self.marker_ino.parse().ok()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt {
    version: u32,
    backend: Backend,
    repository_key: String,
    name: String,
    destination: String,
    base_ids: Vec<String>,
    attempt: String,
    state: ReceiptState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<IdentityRecord>,
}

impl Receipt {
    fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported receipt version".into());
        }
        for (field, text) in [
            ("repositoryKey", &self.repository_key),
            ("name", &self.name),
            ("destination", &self.destination),
            ("attempt", &self.attempt),
        ] {
            if text.is_empty() {
                return Err(format!("invalid receipt {field}"));
            }
        }
        if self.base_ids.is_empty() || self.base_ids.iter().any(String::is_empty) {
            return Err("invalid receipt baseIds".into());
        }
        if (self.state == ReceiptState::Ready) != self.identity.is_some() {
            return Err("receipt identity does not match its state".into());
        }
        if self
            .identity
            .as_ref()
            .is_some_and(|id| id.to_identity().is_none())
        {
            return Err("invalid receipt identity".into());
        }
        Ok(())
    }

    fn identity(&self) -> Option<FsIdentity> {
        self.identity.as_ref().and_then(IdentityRecord::to_identity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DestinationIndex {
    version: u32,
    backend: Backend,
    repository_key: String,
    destination: String,
    name: String,
}

enum ReceiptLookup {
    None,
    Ok(Receipt),
    Invalid(String),
}

#[derive(Debug, Clone, Default)]
struct GitWorktree {
    path: String,
    head: Option<String>,
    branch: Option<String>,
    detached: bool,
    bare: bool,
    locked: bool,
    prunable: bool,
    prunable_reason: Option<String>,
}

/// NUL-delimited `git worktree list --porcelain -z`: fields end in NUL, records in an extra NUL.
fn parse_git_worktrees(output: &str) -> R<Vec<GitWorktree>> {
    let malformed = || CheckoutError::Failed("malformed git worktree list output".into());
    let mut worktrees = Vec::new();
    let mut current: Option<GitWorktree> = None;
    for field in output.split('\0') {
        if field.is_empty() {
            if let Some(done) = current.take() {
                worktrees.push(done);
            }
            continue;
        }
        let (key, value) = match field.split_once(' ') {
            Some((k, v)) => (k, Some(v)),
            None => (field, None),
        };
        if key == "worktree" {
            let Some(path) = value.filter(|v| !v.is_empty()) else {
                return Err(malformed());
            };
            if current.is_some() {
                return Err(malformed());
            }
            current = Some(GitWorktree {
                path: path.to_string(),
                ..GitWorktree::default()
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            return Err(malformed());
        };
        match key {
            "HEAD" => entry.head = value.map(str::to_string),
            "branch" => {
                entry.branch =
                    value.map(|v| v.strip_prefix("refs/heads/").unwrap_or(v).to_string());
            }
            "detached" => entry.detached = true,
            "bare" => entry.bare = true,
            "locked" => entry.locked = true,
            "prunable" => {
                entry.prunable = true;
                entry.prunable_reason = value.map(str::to_string);
            }
            _ => {}
        }
    }
    if let Some(done) = current {
        worktrees.push(done);
    }
    Ok(worktrees)
}

fn parse_json_names(output: &str, label: &str) -> R<Vec<String>> {
    output
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).map_err(|_| {
                CheckoutError::Failed(format!("{label}: jj template produced invalid JSON"))
            })?;
            match value.as_str() {
                Some(name) if !name.is_empty() => Ok(name.to_string()),
                _ => fail(format!("{label}: jj template produced an invalid name")),
            }
        })
        .collect()
}

struct Expected<'a> {
    name: &'a str,
    detached: bool,
    head: Option<&'a str>,
}

/// Path of a validated checkout (always set by `validate_checkout_at`).
fn checkout_path(checkout: &Checkout) -> String {
    checkout.path.clone().unwrap_or_default()
}

pub struct CheckoutService {
    worktree: WorktreeConfig,
    state: StateStore,
    runner: Runner,
}

fn default_run(
    file: &str,
    args: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
) -> io::Result<RunResult> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    process::run(file, &refs, cwd, timeout, None)
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

impl CheckoutService {
    pub fn new(worktree: WorktreeConfig, state: StateStore) -> Self {
        let runner: Runner = Arc::new(default_run);
        Self {
            worktree,
            state,
            runner,
        }
    }

    #[cfg(test)]
    fn with_runner(worktree: WorktreeConfig, state: StateStore, runner: Runner) -> Self {
        Self {
            worktree,
            state,
            runner,
        }
    }

    fn exec(
        &self,
        file: &str,
        args: &[String],
        cwd: Option<&str>,
        timeout: Duration,
    ) -> io::Result<RunResult> {
        let bin = if file == "jj" {
            jj_bin()
        } else {
            file.to_string()
        };
        (self.runner)(&bin, args, cwd.map(Path::new), timeout)
    }

    fn exec_or_fail(&self, file: &str, args: &[String], cwd: Option<&str>) -> R<RunResult> {
        self.exec(file, args, cwd, RUN_TIMEOUT)
            .map_err(|e| CheckoutError::Failed(format!("{file} cannot be started: {e}")))
    }

    fn git(&self, root: &str, args: &[&str]) -> R<String> {
        let mut argv = vec!["-C".to_string(), root.to_string()];
        argv.extend(args.iter().map(|a| a.to_string()));
        let result = self.exec_or_fail("git", &argv, Some(root))?;
        if result.code != Some(0) || result.killed {
            return fail(format!(
                "git {} in {root} failed{}: {}",
                args.first().copied().unwrap_or(""),
                if result.killed { " (timed out)" } else { "" },
                summarize(&result)
            ));
        }
        Ok(result.stdout)
    }

    fn jj_argv(root: &str, args: &[&str]) -> Vec<String> {
        JJ_READ_PREFIX
            .iter()
            .map(|a| a.to_string())
            .chain(std::iter::once(root.to_string()))
            .chain(args.iter().map(|a| a.to_string()))
            .collect()
    }

    fn jj(&self, root: &str, args: &[&str]) -> R<String> {
        let result = self.exec_or_fail("jj", &Self::jj_argv(root, args), Some(root))?;
        if result.code != Some(0) || result.killed {
            return fail(format!(
                "jj {} in {root} failed{}: {}",
                args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
                if result.killed { " (timed out)" } else { "" },
                summarize(&result)
            ));
        }
        Ok(result.stdout)
    }

    fn require_jj(&self) -> R<()> {
        let minimum = JJ_MINIMUM.map(|n| n.to_string()).join(".");
        let result = self
            .exec("jj", &["--version".to_string()], None, RUN_TIMEOUT)
            .map_err(|e| {
                CheckoutError::Failed(format!(
                    "jj cannot be started ({e}); jj >= {minimum} is required for jj repositories"
                ))
            })?;
        let parsed = parse_version(&result.stdout);
        let (version, text) = match parsed {
            Some(found) if result.code == Some(0) => found,
            _ => return fail(format!("cannot read jj version: {}", summarize(&result))),
        };
        if version < JJ_MINIMUM {
            return fail(format!(
                "jj {text} is unsupported; jj >= {minimum} is required"
            ));
        }
        Ok(())
    }

    fn git_identity(&self, root: &str) -> R<Repository> {
        let marker_path = Path::new(root).join(".git");
        match lstat_opt(&marker_path)? {
            Some(meta) if meta.is_dir() || meta.is_file() => {}
            _ => {
                return fail(format!(
                    "malformed or missing Git marker at {}",
                    marker_path.display()
                ));
            }
        }
        let top = realpath_opt(strip_newline(
            &self.git(root, &["rev-parse", "--show-toplevel"])?,
        ));
        if top.as_deref() != Some(root) {
            return fail(format!(
                "Git marker at {root} is not a Git checkout root (toplevel: {})",
                top.as_deref().unwrap_or("unresolvable")
            ));
        }
        let common = self.git(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common = strip_newline(&common);
        let Some(store) = realpath_opt(common) else {
            return fail(format!("Git common dir {common} cannot be resolved"));
        };
        let store_base = basename(&store);
        let name = if store_base == ".git" {
            basename(&dirname(&store))
        } else {
            let stripped = store_base.strip_suffix(".git").unwrap_or(&store_base);
            if stripped.is_empty() {
                basename(root)
            } else {
                stripped.to_string()
            }
        };
        Ok(Repository {
            backend: Backend::Git,
            key: format!("git:{store}"),
            root: root.to_string(),
            name,
        })
    }

    /// `.jj/repo` is the store directory or a pointer file whose path is relative to `.jj`.
    fn jj_store(root: &str) -> R<String> {
        let marker_path = Path::new(root).join(".jj");
        if !lstat_opt(&marker_path)?.is_some_and(|m| m.is_dir()) {
            return fail(format!(
                "malformed jj workspace marker: {}",
                marker_path.display()
            ));
        }
        let repo_path = marker_path.join("repo");
        let Some(entry) = lstat_opt(&repo_path)? else {
            return fail(format!(
                "broken jj workspace marker: {} is missing",
                repo_path.display()
            ));
        };
        let target = if entry.is_file() {
            let pointer = fs::read_to_string(&repo_path).map_err(|e| {
                CheckoutError::Failed(format!("cannot read {}: {e}", repo_path.display()))
            })?;
            marker_path.join(pointer)
        } else {
            repo_path
        };
        let target = path_str(&target);
        match realpath_opt(&target) {
            Some(store) if fs::metadata(&store).is_ok_and(|m| m.is_dir()) => Ok(store),
            _ => fail(format!("jj repository store {target} is not a directory")),
        }
    }

    fn jj_identity(&self, root: &str) -> R<Repository> {
        let store = Self::jj_store(root)?;
        self.require_jj()?;
        let reported = realpath_opt(strip_newline(&self.jj(root, &["workspace", "root"])?));
        if reported.as_deref() != Some(root) {
            return fail(format!(
                "jj reports workspace root {} for {root}",
                reported.as_deref().unwrap_or("(unresolvable)")
            ));
        }
        Ok(Repository {
            backend: Backend::Jj,
            key: format!("jj:{store}"),
            root: root.to_string(),
            name: basename(&dirname(&dirname(&store))),
        })
    }

    fn identify(&self, root: &str, backend: Backend) -> R<Repository> {
        match backend {
            Backend::Git => self.git_identity(root),
            Backend::Jj => self.jj_identity(root),
        }
    }

    fn verify_repository(&self, repo: &Repository) -> R<()> {
        let actual = self.identify(&repo.root, repo.backend)?;
        if actual.key != repo.key || actual.root != repo.root {
            return fail(format!(
                "repository at {} changed identity ({} → {}); select the repository again",
                repo.root, repo.key, actual.key
            ));
        }
        Ok(())
    }

    fn git_worktrees(&self, repo: &Repository) -> R<Vec<GitWorktree>> {
        parse_git_worktrees(&self.git(&repo.root, &["worktree", "list", "--porcelain", "-z"])?)
    }

    fn jj_names(&self, root: &str) -> R<Vec<String>> {
        let names = parse_json_names(
            &self.jj(root, &["workspace", "list", "-T", JJ_NAME_TEMPLATE])?,
            "jj workspace list",
        )?;
        if names.is_empty() {
            return fail(format!(
                "jj workspace list for {root} returned no workspaces"
            ));
        }
        Ok(names)
    }

    /// Recorded root of a jj workspace: `(path, unavailable_reason)`.
    fn jj_root(&self, root: &str, name: &str) -> R<(Option<String>, Option<String>)> {
        let result = self.exec_or_fail(
            "jj",
            &Self::jj_argv(root, &["workspace", "root", "--name", name]),
            Some(root),
        )?;
        if result.code != Some(0) || result.killed {
            return Ok((
                None,
                Some(format!(
                    "workspace root lookup failed: {}",
                    summarize(&result)
                )),
            ));
        }
        let raw = strip_newline(&result.stdout);
        let real = if raw.is_empty() {
            None
        } else {
            realpath_opt(raw)
        };
        Ok(match real {
            Some(path) => (Some(path), None),
            None => (
                None,
                Some(format!(
                    "recorded workspace root {} is not reachable",
                    if raw.is_empty() { "(empty)" } else { raw }
                )),
            ),
        })
    }

    /// Re-validate a checkout path against native identity; never mutates it.
    fn validate_checkout_at(
        &self,
        repo: &Repository,
        path: &str,
        expected: Expected<'_>,
    ) -> R<Checkout> {
        let Some(real) = realpath_opt(path) else {
            return fail(format!("checkout path {path} does not exist"));
        };
        let actual = self.identify(&real, repo.backend)?;
        if actual.key != repo.key {
            return fail(format!(
                "{real} belongs to {}, not {}",
                actual.key, repo.key
            ));
        }
        let mut checkout = Checkout {
            repository_key: repo.key.clone(),
            name: expected.name.to_string(),
            path: Some(real.clone()),
            head: None,
            detached: false,
            source: false,
            locked: false,
            prunable: false,
            unavailable_reason: None,
        };
        if repo.backend == Backend::Jj {
            if !self
                .jj_names(&repo.root)?
                .iter()
                .any(|n| n == expected.name)
            {
                return fail(format!(
                    "jj workspace '{}' is not listed in {}",
                    expected.name, repo.root
                ));
            }
            if let (Some(recorded), _) = self.jj_root(&repo.root, expected.name)?
                && recorded != real
            {
                return fail(format!(
                    "jj workspace '{}' is recorded at {recorded}, not {real}",
                    expected.name
                ));
            }
            let current = parse_json_names(
                &self.jj(&real, &["workspace", "list", "-T", JJ_CURRENT_TEMPLATE])?,
                "jj workspace list",
            )?;
            if current.len() > 1 {
                return fail(format!(
                    "ambiguous jj workspace identity at {real}: {} workspaces share the working-copy commit",
                    current.len()
                ));
            }
            if current.first().map(String::as_str) != Some(expected.name) {
                let found = current
                    .first()
                    .map(|n| format!("{n:?}"))
                    .unwrap_or_else(|| "null".into());
                return fail(format!(
                    "{real} is jj workspace {found}, not '{}'",
                    expected.name
                ));
            }
            checkout.source = real == repo.root;
            return Ok(checkout);
        }
        let worktrees = self.git_worktrees(repo)?;
        let mut index = None;
        for (position, worktree) in worktrees.iter().enumerate() {
            if realpath_opt(&worktree.path).as_deref() == Some(real.as_str()) {
                index = Some(position);
            }
        }
        let Some((index, entry)) = index.map(|i| (i, &worktrees[i])) else {
            return fail(format!(
                "{real} is not a registered worktree of {}",
                repo.root
            ));
        };
        if entry.bare || entry.prunable {
            return fail(format!(
                "worktree {real} is {}",
                if entry.bare { "bare" } else { "prunable" }
            ));
        }
        let symbolic = self.exec_or_fail(
            "git",
            &strings(&["-C", &real, "symbolic-ref", "--quiet", "HEAD"]),
            Some(&real),
        )?;
        let head = strip_newline(&self.git(&real, &["rev-parse", "--verify", "HEAD"])?).to_string();
        if expected.detached {
            if symbolic.code != Some(1) {
                return fail(format!(
                    "worktree {real} is not on a detached HEAD: {}",
                    summarize(&symbolic)
                ));
            }
            if let Some(want) = expected.head
                && head != want
            {
                return fail(format!(
                    "detached worktree {real} moved from {want} to {head}; select it again"
                ));
            }
        } else {
            if symbolic.code != Some(0) {
                return fail(format!(
                    "worktree {real} is not on branch '{}': {}",
                    expected.name,
                    summarize(&symbolic)
                ));
            }
            let reference = strip_newline(&symbolic.stdout);
            if reference != format!("refs/heads/{}", expected.name)
                || entry.branch.as_deref() != Some(expected.name)
            {
                return fail(format!(
                    "worktree {real} has {reference}, not branch '{}'",
                    expected.name
                ));
            }
        }
        checkout.head = Some(head);
        checkout.detached = expected.detached;
        checkout.source = index == 0;
        checkout.locked = entry.locked;
        Ok(checkout)
    }

    fn receipt_path(repo: &Repository, name: &str) -> String {
        format!(
            "receipts/{}.json",
            sha(&serde_json::json!([repo.key, name]).to_string())
        )
    }

    fn load_receipt(&self, repo: &Repository, name: &str) -> ReceiptLookup {
        let receipt = match self
            .state
            .read_record::<Receipt>(&Self::receipt_path(repo, name))
        {
            Ok(Some(receipt)) => receipt,
            Ok(None) => return ReceiptLookup::None,
            Err(error) => {
                return ReceiptLookup::Invalid(format!("receipt cannot be trusted ({error})"));
            }
        };
        if let Err(reason) = receipt.validate() {
            return ReceiptLookup::Invalid(format!("receipt cannot be trusted ({reason})"));
        }
        if receipt.repository_key != repo.key
            || receipt.name != name
            || receipt.backend != repo.backend
        {
            return ReceiptLookup::Invalid(
                "receipt belongs to a different repository or checkout".into(),
            );
        }
        ReceiptLookup::Ok(receipt)
    }

    /// Index from a checkout destination to the receipt of the attempt that created it; survives branch switches.
    fn destination_index_path(repo: &Repository, destination: &str) -> String {
        format!(
            "receipts/by-destination/{}.json",
            sha(&serde_json::json!([repo.key, destination]).to_string())
        )
    }

    fn load_receipt_at(&self, repo: &Repository, destination: &str) -> ReceiptLookup {
        let index = match self
            .state
            .read_record::<DestinationIndex>(&Self::destination_index_path(repo, destination))
        {
            Ok(Some(index)) => index,
            Ok(None) => return ReceiptLookup::None,
            Err(error) => {
                return ReceiptLookup::Invalid(format!(
                    "receipt index cannot be trusted ({error})"
                ));
            }
        };
        if index.version != 1
            || index.repository_key.is_empty()
            || index.destination.is_empty()
            || index.name.is_empty()
        {
            return ReceiptLookup::Invalid(
                "receipt index cannot be trusted (invalid fields)".into(),
            );
        }
        if index.repository_key != repo.key
            || index.destination != destination
            || index.backend != repo.backend
        {
            return ReceiptLookup::Invalid(
                "receipt index belongs to a different repository or destination".into(),
            );
        }
        match self.load_receipt(repo, &index.name) {
            ReceiptLookup::None => ReceiptLookup::Invalid(format!(
                "the receipt of the attempt that created it as '{}' is missing",
                index.name
            )),
            ReceiptLookup::Ok(receipt) if receipt.destination != destination => {
                ReceiptLookup::Invalid(format!(
                    "the receipt of the attempt that created it as '{}' points to {}",
                    index.name, receipt.destination
                ))
            }
            other => other,
        }
    }

    fn destination_for(&self, repo: &Repository, name: &str) -> R<String> {
        let root = &self.worktree.root;
        if !Path::new(root).is_absolute() {
            return fail(format!("worktree.root must be absolute: {root}"));
        }
        let parent =
            Path::new(root).join(format!("{}-{}", slug(&repo.name), &sha(&repo.key)[..16]));
        let leaf = format!("{}-{}", slug(name), &sha(name)[..8]);
        let destination = path_str(&canonicalize_missing(&parent)?.join(leaf));
        let store = repo
            .key
            .split_once(':')
            .map(|(_, s)| s)
            .unwrap_or(&repo.key);
        if is_within(&destination, &repo.root) || is_within(&destination, store) {
            return fail(format!(
                "checkout destination {destination} is inside the source repository; change worktree.root"
            ));
        }
        Ok(destination)
    }

    fn validate_name(&self, repo: &Repository, name: &str) -> R<()> {
        if name.is_empty() || name.starts_with('-') || has_control_chars(name) {
            return fail(format!(
                "invalid checkout name {name:?}: empty, leading '-', or control characters"
            ));
        }
        if repo.backend == Backend::Jj {
            if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
                return fail(format!(
                    "invalid jj workspace name {name:?}: must be a single path component"
                ));
            }
            return Ok(());
        }
        if name.contains("@{-") {
            return fail(format!(
                "invalid branch name {name:?}: '@{{-N}}' shorthand is not allowed"
            ));
        }
        let result = self.exec_or_fail(
            "git",
            &strings(&["-C", &repo.root, "check-ref-format", "--branch", name]),
            Some(&repo.root),
        )?;
        if result.code != Some(0) || strip_newline(&result.stdout) != name {
            return fail(format!(
                "invalid branch name {name:?}: {}",
                summarize(&result)
            ));
        }
        Ok(())
    }

    fn resolve_base_ids(&self, repo: &Repository, base: Option<&str>) -> R<Vec<String>> {
        if repo.backend == Backend::Git {
            let spec = base.or(self.worktree.git_base.as_deref()).unwrap_or("HEAD");
            let result = self.exec_or_fail(
                "git",
                &strings(&[
                    "-C",
                    &repo.root,
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{spec}^{{commit}}"),
                ]),
                Some(&repo.root),
            )?;
            if result.code != Some(0) {
                if spec == "HEAD" {
                    return fail(format!(
                        "source HEAD in {} has no commit (unborn branch); commit first or set worktree.gitBase",
                        repo.root
                    ));
                }
                return fail(format!(
                    "base {spec:?} does not resolve to a commit: {}",
                    summarize(&result)
                ));
            }
            let id = strip_newline(&result.stdout).to_string();
            if !is_git_commit_id(&id) {
                return fail(format!("git returned an unexpected commit id for {spec:?}"));
            }
            return Ok(vec![id]);
        }
        let revset = base
            .or(self.worktree.jj_base.as_deref())
            .unwrap_or("parents(@)");
        let output = self.jj(
            &repo.root,
            &[
                "log",
                "--no-graph",
                &format!("--revision={revset}"),
                "-T",
                JJ_COMMIT_TEMPLATE,
            ],
        )?;
        let ids: Vec<String> = output
            .split('\n')
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if ids.is_empty() {
            return fail(format!("jj revset {revset:?} resolved to no commits"));
        }
        if !ids.iter().all(|id| is_jj_commit_id(id)) {
            return fail(format!("jj returned unexpected commit ids for {revset:?}"));
        }
        Ok(ids)
    }

    /// New must not touch an existing branch/workspace/path.
    fn assert_new_target(&self, repo: &Repository, name: &str, destination: &str) -> R<()> {
        if lstat_opt(Path::new(destination))?.is_some() {
            return fail(format!(
                "checkout destination {destination} already exists and is never overwritten; inspect it and select Existing"
            ));
        }
        if repo.backend == Backend::Jj {
            if self.jj_names(&repo.root)?.iter().any(|n| n == name) {
                return fail(format!(
                    "jj workspace '{name}' already exists; select Existing to open it"
                ));
            }
            return Ok(());
        }
        let worktrees = self.git_worktrees(repo)?;
        if let Some(holder) = worktrees.iter().find(|w| w.branch.as_deref() == Some(name)) {
            return fail(format!(
                "branch '{name}' is already checked out at {}; select Existing to open it",
                holder.path
            ));
        }
        if worktrees.iter().any(|w| w.path == destination) {
            return fail(format!(
                "a worktree is already registered at {destination}; inspect it manually"
            ));
        }
        let reference = self.exec_or_fail(
            "git",
            &strings(&[
                "-C",
                &repo.root,
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ]),
            Some(&repo.root),
        )?;
        match reference.code {
            Some(0) => fail(format!(
                "branch '{name}' already exists but is not checked out; New never reuses or overwrites a branch — choose another name"
            )),
            Some(1) => Ok(()),
            _ => fail(format!(
                "cannot check branch '{name}': {}",
                summarize(&reference)
            )),
        }
    }

    /// Same New request: reuse only a fully matching Ready receipt; any other receipt blocks native add.
    fn reuse_ready(
        &self,
        repo: &Repository,
        name: &str,
        destination: &str,
        base_ids: &[String],
    ) -> R<Option<Checkout>> {
        let receipt = match self.load_receipt(repo, name) {
            ReceiptLookup::None => return Ok(None),
            ReceiptLookup::Invalid(reason) => {
                return Err(recovery(false, name, destination, reason));
            }
            ReceiptLookup::Ok(receipt) => receipt,
        };
        if receipt.state != ReceiptState::Ready {
            return Err(recovery(
                false,
                name,
                &receipt.destination,
                format!("a previous native add attempt is {}", receipt.state.label()),
            ));
        }
        if receipt.destination != destination || receipt.base_ids != base_ids {
            return Err(recovery(
                false,
                name,
                &receipt.destination,
                format!(
                    "an earlier checkout was created with a different destination or base ({})",
                    receipt.base_ids.join(", ")
                ),
            ));
        }
        let checkout = self.validate_checkout_at(
            repo,
            destination,
            Expected {
                name,
                detached: false,
                head: None,
            },
        )?;
        if !same_identity(receipt.identity(), fs_identity(destination, repo.backend)?) {
            return Err(recovery(
                false,
                name,
                destination,
                "the checkout directory or marker was replaced after creation",
            ));
        }
        Ok(Some(checkout))
    }

    fn validate_existing(
        &self,
        repo: &Repository,
        selected: &Checkout,
        recovery_acknowledged: bool,
    ) -> R<Checkout> {
        if selected.repository_key != repo.key {
            return fail(format!(
                "checkout '{}' belongs to another repository",
                selected.name
            ));
        }
        let Some(path) = selected.path.as_deref() else {
            let reason = selected
                .unavailable_reason
                .as_ref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default();
            return fail(format!(
                "checkout '{}' has no known path{reason}; enter its path",
                selected.name
            ));
        };
        if selected.prunable {
            return fail(format!("worktree {path} is prunable and cannot be opened"));
        }
        let checkout = self.validate_checkout_at(
            repo,
            path,
            Expected {
                name: &selected.name,
                detached: selected.detached,
                head: selected.head.as_deref(),
            },
        )?;
        if recovery_acknowledged {
            return Ok(checkout);
        }
        // Receipts are found by destination (branch switches and detached HEADs keep it) and,
        // for named checkouts, by name; a detached Git worktree's listed name is not a branch name.
        let real = checkout_path(&checkout);
        let mut lookups = vec![self.load_receipt_at(repo, &real)];
        if !(repo.backend == Backend::Git && checkout.detached) {
            lookups.push(self.load_receipt(repo, &checkout.name));
        }
        let mut current: Option<FsIdentity> = None;
        for lookup in lookups {
            let receipt = match lookup {
                ReceiptLookup::None => continue,
                ReceiptLookup::Invalid(reason) => {
                    return Err(recovery(true, &checkout.name, &real, reason));
                }
                ReceiptLookup::Ok(receipt) => receipt,
            };
            if receipt.state != ReceiptState::Ready {
                return Err(recovery(
                    true,
                    &checkout.name,
                    &real,
                    format!("its native add attempt is {}", receipt.state.label()),
                ));
            }
            if receipt.destination != real {
                return Err(recovery(
                    true,
                    &checkout.name,
                    &real,
                    format!("its receipt points to {}", receipt.destination),
                ));
            }
            let identity = match current {
                Some(identity) => identity,
                None => {
                    let identity = fs_identity(&real, repo.backend)?;
                    current = Some(identity);
                    identity
                }
            };
            if !same_identity(receipt.identity(), identity) {
                return Err(recovery(
                    true,
                    &checkout.name,
                    &real,
                    "the checkout directory or marker was replaced after creation",
                ));
            }
        }
        Ok(checkout)
    }

    fn create_new(
        &self,
        repo: &Repository,
        name: &str,
        destination: &str,
        base_ids: &[String],
    ) -> R<PreparedCheckout> {
        let parent = dirname(destination);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&parent)
            .map_err(|e| CheckoutError::Failed(format!("cannot create {parent}: {e}")))?;
        if realpath_opt(&parent).as_deref() != Some(parent.as_str()) {
            return fail(format!(
                "checkout parent {parent} changed (symlink?); plan again"
            ));
        }
        if lstat_opt(Path::new(destination))?.is_some() {
            return fail(format!(
                "checkout destination {destination} appeared; it is never overwritten"
            ));
        }

        let record_path = Self::receipt_path(repo, name);
        let pending = Receipt {
            version: 1,
            backend: repo.backend,
            repository_key: repo.key.clone(),
            name: name.to_string(),
            destination: destination.to_string(),
            base_ids: base_ids.to_vec(),
            attempt: random_id(),
            state: ReceiptState::Pending,
            identity: None,
        };
        // The destination index is durable before the Pending receipt, so every receipt stays reachable by path.
        self.state.write_record(
            &Self::destination_index_path(repo, destination),
            &DestinationIndex {
                version: 1,
                backend: repo.backend,
                repository_key: repo.key.clone(),
                destination: destination.to_string(),
                name: name.to_string(),
            },
        )?;
        self.state.write_record(&record_path, &pending)?;

        let mark_failed = || -> String {
            let failed = Receipt {
                state: ReceiptState::Failed,
                ..pending.clone()
            };
            match self.state.write_record(&record_path, &failed) {
                Ok(()) => "; the attempt is recorded as Failed".to_string(),
                Err(error) => {
                    format!(
                        "; the Failed receipt could not be saved ({error}), so the Pending receipt remains"
                    )
                }
            }
        };
        let recovery_hint = format!(
            "select it as Existing and acknowledge recovery after inspecting {destination}"
        );

        let (file, args): (&str, Vec<String>) = match repo.backend {
            Backend::Git => (
                "git",
                strings(&[
                    "-C",
                    &repo.root,
                    "worktree",
                    "add",
                    "-b",
                    name,
                    "--",
                    destination,
                    &base_ids[0],
                ]),
            ),
            Backend::Jj => {
                let mut args = strings(&[
                    "--no-pager",
                    "--color=never",
                    "-R",
                    &repo.root,
                    "workspace",
                    "add",
                    "--no-colocate",
                    "--name",
                    name,
                ]);
                for id in base_ids {
                    args.push("-r".into());
                    args.push(id.clone());
                }
                args.push("--".into());
                args.push(destination.to_string());
                ("jj", args)
            }
        };
        let result = match self.exec(file, &args, Some(&repo.root), NATIVE_ADD_TIMEOUT) {
            Ok(result) => result,
            Err(error) => {
                return fail(format!(
                    "{file} could not start ({error}){}; no native add will be retried — {recovery_hint}",
                    mark_failed()
                ));
            }
        };
        if result.code != Some(0) || result.killed {
            let outcome = if result.killed {
                "timed out or was killed".to_string()
            } else {
                format!(
                    "failed (exit {})",
                    result
                        .code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "unknown".into())
                )
            };
            let native = if repo.backend == Backend::Git {
                "git worktree add"
            } else {
                "jj workspace add"
            };
            return fail(format!(
                "native {native} {outcome}: {}; any partial checkout at {destination} is kept{}; no native add will be retried — {recovery_hint}",
                summarize(&result),
                mark_failed()
            ));
        }

        let verified = (|| -> R<(Checkout, FsIdentity)> {
            let checkout = self.validate_checkout_at(
                repo,
                destination,
                Expected {
                    name,
                    detached: false,
                    head: None,
                },
            )?;
            let parents: Vec<String> = match repo.backend {
                Backend::Git => vec![checkout.head.clone().unwrap_or_default()],
                Backend::Jj => self
                    .jj(
                        destination,
                        &[
                            "log",
                            "--no-graph",
                            "--revision=@",
                            "-T",
                            JJ_PARENTS_TEMPLATE,
                        ],
                    )?
                    .split('\n')
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect(),
            };
            if parents != base_ids {
                return fail(format!(
                    "new checkout is based on {}, not {}",
                    parents.join(", "),
                    base_ids.join(", ")
                ));
            }
            Ok((checkout, fs_identity(destination, repo.backend)?))
        })();
        let (checkout, identity) = verified.map_err(|error| {
            CheckoutError::Failed(format!(
                "native add exited 0 but verification failed: {error}; the checkout is kept and its Pending receipt requires manual inspection — {recovery_hint}"
            ))
        })?;
        let ready = Receipt {
            state: ReceiptState::Ready,
            identity: Some(IdentityRecord::from_identity(identity)),
            ..pending.clone()
        };
        if let Err(error) = self.state.write_record(&record_path, &ready) {
            return fail(format!(
                "checkout created at {destination} but its Ready receipt could not be saved ({error}); {recovery_hint}"
            ));
        }
        Ok(PreparedCheckout {
            repository: repo.clone(),
            path: checkout_path(&checkout),
            checkout,
            created: true,
        })
    }

    pub fn resolve_repository(&self, path: &str, prefer: Backend) -> R<Repository> {
        if !Path::new(path).is_absolute() {
            return fail(format!("repository path must be absolute: {path}"));
        }
        let Some(real) = realpath_opt(path) else {
            return fail(format!("repository path does not exist: {path}"));
        };
        // The nearest marker wins, even if broken: never climb to an ancestor repository.
        let mut root = PathBuf::from(real);
        loop {
            let has_git = lstat_opt(&root.join(".git"))?.is_some();
            let has_jj = lstat_opt(&root.join(".jj"))?.is_some();
            if has_git || has_jj {
                let backend = if has_git && has_jj {
                    prefer
                } else if has_jj {
                    Backend::Jj
                } else {
                    Backend::Git
                };
                return self.identify(&path_str(&root), backend);
            }
            if !root.pop() {
                return fail(format!("no Git or jj checkout contains {path}"));
            }
        }
    }

    pub fn list_checkouts(&self, repo: &Repository) -> R<Vec<Checkout>> {
        if repo.backend == Backend::Jj {
            let mut checkouts = Vec::new();
            for name in self.jj_names(&repo.root)? {
                let (path, reason) = self.jj_root(&repo.root, &name)?;
                checkouts.push(Checkout {
                    repository_key: repo.key.clone(),
                    source: path.as_deref() == Some(repo.root.as_str()),
                    name,
                    path,
                    head: None,
                    detached: false,
                    locked: false,
                    prunable: false,
                    unavailable_reason: reason,
                });
            }
            return Ok(checkouts);
        }
        let mut checkouts = Vec::new();
        for (index, worktree) in self.git_worktrees(repo)?.into_iter().enumerate() {
            let real = realpath_opt(&worktree.path);
            let unavailable_reason = if worktree.bare {
                Some("bare repository has no working tree".to_string())
            } else if worktree.prunable {
                Some(format!(
                    "prunable: {}",
                    worktree
                        .prunable_reason
                        .as_deref()
                        .unwrap_or("working tree is missing")
                ))
            } else if real.is_none() {
                Some("working tree path is missing".to_string())
            } else {
                None
            };
            checkouts.push(Checkout {
                repository_key: repo.key.clone(),
                name: worktree
                    .branch
                    .clone()
                    .unwrap_or_else(|| basename(&worktree.path)),
                path: Some(real.unwrap_or_else(|| worktree.path.clone())),
                head: worktree.head.clone(),
                detached: worktree.detached,
                source: index == 0 && !worktree.bare,
                locked: worktree.locked,
                prunable: worktree.prunable,
                unavailable_reason,
            });
        }
        Ok(checkouts)
    }

    pub fn plan_checkout(
        &self,
        repo: &Repository,
        selection: CheckoutSelection,
    ) -> R<CheckoutPlan> {
        self.verify_repository(repo)?;
        match selection {
            CheckoutSelection::Existing {
                checkout,
                recovery_acknowledged,
            } => {
                let checkout = self.validate_existing(repo, &checkout, recovery_acknowledged)?;
                let destination = checkout_path(&checkout);
                let identity = fs_identity(&destination, repo.backend)?;
                Ok(CheckoutPlan {
                    repository: repo.clone(),
                    selection: CheckoutSelection::Existing {
                        checkout,
                        recovery_acknowledged,
                    },
                    destination,
                    base_ids: Vec::new(),
                    identity: Some(identity),
                })
            }
            CheckoutSelection::New { name, base } => {
                self.validate_name(repo, &name)?;
                let destination = self.destination_for(repo, &name)?;
                let base_ids = self.resolve_base_ids(repo, base.as_deref())?;
                if self
                    .reuse_ready(repo, &name, &destination, &base_ids)?
                    .is_none()
                {
                    self.assert_new_target(repo, &name, &destination)?;
                }
                Ok(CheckoutPlan {
                    repository: repo.clone(),
                    selection: CheckoutSelection::New { name, base },
                    destination,
                    base_ids,
                    identity: None,
                })
            }
        }
    }

    pub fn prepare_checkout(&self, plan: &CheckoutPlan) -> R<PreparedCheckout> {
        let repo = &plan.repository;
        let name = match &plan.selection {
            CheckoutSelection::New { name, .. } => name.clone(),
            CheckoutSelection::Existing { checkout, .. } => checkout.name.clone(),
        };
        let keys = [
            format!("destination:{}", plan.destination),
            format!("checkout:{}", serde_json::json!([repo.key, name])),
        ];
        self.state
            .with_locks(&keys, || self.prepare_locked(plan, &name))?
    }

    fn prepare_locked(&self, plan: &CheckoutPlan, name: &str) -> R<PreparedCheckout> {
        let repo = &plan.repository;
        self.verify_repository(repo)?;
        if let CheckoutSelection::Existing {
            checkout,
            recovery_acknowledged,
        } = &plan.selection
        {
            let Some(planned) = plan.identity else {
                return fail(format!(
                    "checkout plan for '{name}' has no filesystem identity; select it again"
                ));
            };
            let checkout = self.validate_existing(repo, checkout, *recovery_acknowledged)?;
            let path = checkout_path(&checkout);
            if path != plan.destination {
                return fail(format!(
                    "checkout '{name}' now resolves to {path}, not {}; select it again",
                    plan.destination
                ));
            }
            if !same_identity(Some(planned), fs_identity(&path, repo.backend)?) {
                let marker = if repo.backend == Backend::Git {
                    ".git"
                } else {
                    ".jj"
                };
                return fail(format!(
                    "checkout '{name}' at {path} was replaced since it was selected (directory or {marker} marker changed); select it again"
                ));
            }
            return Ok(PreparedCheckout {
                repository: repo.clone(),
                checkout,
                path,
                created: false,
            });
        }
        self.validate_name(repo, name)?;
        let destination = self.destination_for(repo, name)?;
        if destination != plan.destination {
            return fail(format!(
                "checkout destination changed from {} to {destination}; plan again",
                plan.destination
            ));
        }
        if plan.base_ids.is_empty() || (repo.backend == Backend::Git && plan.base_ids.len() != 1) {
            return fail("checkout plan has no valid base commit ids");
        }
        if let Some(checkout) = self.reuse_ready(repo, name, &destination, &plan.base_ids)? {
            return Ok(PreparedCheckout {
                repository: repo.clone(),
                path: checkout_path(&checkout),
                checkout,
                created: false,
            });
        }
        self.assert_new_target(repo, name, &destination)?;
        self.create_new(repo, name, &destination, &plan.base_ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        temp: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("jt-checkout-{}", random_id()));
            fs::create_dir_all(&dir).unwrap();
            let temp = fs::canonicalize(dir).unwrap();
            fs::write(temp.join("gitconfig"), "").unwrap();
            fs::write(
                temp.join("jj.toml"),
                "user.name = \"Test\"\nuser.email = \"test@example.invalid\"\n",
            )
            .unwrap();
            Fixture { temp }
        }
        fn state_dir(&self) -> PathBuf {
            self.temp.join("state")
        }
        fn worktree_root(&self) -> String {
            path_str(&self.temp.join("checkouts"))
        }
        fn sh(&self, cmd: &[&str], cwd: &Path) -> String {
            let program = if cmd[0] == "jj" {
                jj_bin()
            } else {
                cmd[0].to_string()
            };
            let output = Command::new(program)
                .args(&cmd[1..])
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .env("GIT_CONFIG_GLOBAL", self.temp.join("gitconfig"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("JJ_CONFIG", self.temp.join("jj.toml"))
                .env("JJ_USER", "Test")
                .env("JJ_EMAIL", "test@example.invalid")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{cmd:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        fn config(&self) -> WorktreeConfig {
            WorktreeConfig {
                repos: Vec::new(),
                prefer: Backend::Jj,
                root: self.worktree_root(),
                git_base: None,
                jj_base: None,
            }
        }
        fn service(&self, runner: Runner) -> CheckoutService {
            CheckoutService::with_runner(
                self.config(),
                StateStore::new(self.state_dir()).unwrap(),
                runner,
            )
        }
        fn receipt_states(&self) -> Vec<String> {
            let dir = self.state_dir().join("receipts");
            let Ok(entries) = fs::read_dir(&dir) else {
                return Vec::new();
            };
            entries
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .map(|p| {
                    let value: serde_json::Value =
                        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap();
                    value["state"].as_str().unwrap().to_string()
                })
                .collect()
        }
        fn script(&self, name: &str, body: &str) -> String {
            let file = self.temp.join(name);
            fs::write(&file, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
            path_str(&file)
        }
        fn git_repo(&self, name: &str) -> GitRepo {
            let root = self.temp.join(name);
            fs::create_dir(&root).unwrap();
            self.sh(&["git", "init", "-q", "-b", "main"], &root);
            fs::write(root.join("README"), "one\n").unwrap();
            self.sh(&["git", "add", "README"], &root);
            self.sh(&["git", "commit", "-q", "-m", "one"], &root);
            let first = self.sh(&["git", "rev-parse", "HEAD"], &root);
            fs::write(root.join("README"), "two\n").unwrap();
            self.sh(&["git", "commit", "-q", "-am", "two"], &root);
            let second = self.sh(&["git", "rev-parse", "HEAD"], &root);
            fs::write(root.join("README"), "dirty tracked edit\n").unwrap();
            fs::write(root.join("scratch.txt"), "untracked bytes\n").unwrap();
            GitRepo {
                root: path_str(&root),
                first,
                second,
            }
        }
        fn jj_repo(&self, name: &str) -> (String, String) {
            let root = self.temp.join(name);
            self.sh(
                &[
                    "jj",
                    "--no-pager",
                    "git",
                    "init",
                    "--no-colocate",
                    &path_str(&root),
                ],
                &self.temp,
            );
            fs::write(root.join("a.txt"), "committed\n").unwrap();
            self.sh(&["jj", "--no-pager", "commit", "-m", "one"], &root);
            let parent = self.sh(
                &[
                    "jj",
                    "--no-pager",
                    "--ignore-working-copy",
                    "log",
                    "--no-graph",
                    "-r",
                    "@-",
                    "-T",
                    "commit_id",
                ],
                &root,
            );
            fs::write(root.join("a.txt"), "dirty edit\n").unwrap();
            (path_str(&root), parent)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::set_permissions(
                self.state_dir().join("receipts"),
                fs::Permissions::from_mode(0o700),
            );
            let _ = fs::remove_dir_all(&self.temp);
        }
    }

    struct GitRepo {
        root: String,
        first: String,
        second: String,
    }

    fn real_run(
        file: &str,
        args: &[String],
        cwd: Option<&Path>,
        timeout: Duration,
    ) -> io::Result<RunResult> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        process::run(file, &refs, cwd, timeout, None)
    }

    type Intercept =
        dyn Fn(&str, &[String], Option<&Path>, Duration) -> io::Result<RunResult> + Send + Sync;

    /// Real runner counting native adds and optionally replacing them.
    fn counting(intercept: Option<Box<Intercept>>) -> (Runner, Arc<AtomicUsize>) {
        let adds = Arc::new(AtomicUsize::new(0));
        let counter = adds.clone();
        let runner: Runner = Arc::new(
            move |file: &str, args: &[String], cwd: Option<&Path>, timeout: Duration| {
                let joined = args.join(" ");
                if joined.contains("worktree add") || joined.contains("workspace add") {
                    counter.fetch_add(1, Ordering::SeqCst);
                    if let Some(intercept) = &intercept {
                        return intercept(file, args, cwd, timeout);
                    }
                }
                real_run(file, args, cwd, timeout)
            },
        );
        (runner, adds)
    }

    fn plain() -> Runner {
        Arc::new(real_run)
    }

    fn jj_available() -> bool {
        Command::new(jj_bin())
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| parse_version(&String::from_utf8_lossy(&o.stdout)))
            .is_some_and(|(v, _)| v >= JJ_MINIMUM)
    }

    fn new_sel(name: &str, base: Option<&str>) -> CheckoutSelection {
        CheckoutSelection::New {
            name: name.to_string(),
            base: base.map(str::to_string),
        }
    }

    fn existing(checkout: &Checkout, ack: bool) -> CheckoutSelection {
        CheckoutSelection::Existing {
            checkout: checkout.clone(),
            recovery_acknowledged: ack,
        }
    }

    fn find(list: Vec<Checkout>, pred: impl Fn(&Checkout) -> bool) -> Checkout {
        list.into_iter().find(|c| pred(c)).expect("checkout listed")
    }

    fn err_text<T: fmt::Debug>(result: R<T>) -> String {
        result.unwrap_err().to_string()
    }

    fn is_recovery<T: fmt::Debug>(result: R<T>) -> bool {
        matches!(result, Err(CheckoutError::RecoveryRequired { .. }))
    }

    #[test]
    fn git_new_creates_branch_at_base_then_reuses_and_opens_existing() {
        let fx = Fixture::new();
        let repo_fx = fx.git_repo("source");
        let (runner, adds) = counting(None);
        let service = fx.service(runner);
        let repo = service
            .resolve_repository(&repo_fx.root, Backend::Jj)
            .unwrap();
        assert_eq!(
            repo,
            Repository {
                backend: Backend::Git,
                key: format!("git:{}/.git", repo_fx.root),
                root: repo_fx.root.clone(),
                name: "source".into()
            }
        );

        let plan = service
            .plan_checkout(&repo, new_sel("TEST-1", Some(&repo_fx.first)))
            .unwrap();
        assert_eq!(plan.base_ids(), std::slice::from_ref(&repo_fx.first));
        assert!(
            plan.destination()
                .starts_with(&format!("{}/source-", fx.worktree_root()))
        );
        assert!(basename(plan.destination()).starts_with("TEST-1-"));
        // Planning is read-only.
        assert!(!Path::new(&fx.worktree_root()).exists());
        assert!(!fx.state_dir().exists());

        let prepared = service.prepare_checkout(&plan).unwrap();
        assert!(prepared.created);
        assert_eq!(prepared.path, plan.destination());
        let dest = Path::new(plan.destination());
        assert_eq!(fx.sh(&["git", "rev-parse", "HEAD"], dest), repo_fx.first);
        assert_eq!(
            fx.sh(&["git", "symbolic-ref", "--short", "HEAD"], dest),
            "TEST-1"
        );
        assert_eq!(fs::read_to_string(dest.join("README")).unwrap(), "one\n");
        assert!(!dest.join("scratch.txt").exists());
        assert_eq!(
            fs::read_to_string(Path::new(&repo_fx.root).join("README")).unwrap(),
            "dirty tracked edit\n"
        );
        assert_eq!(fx.receipt_states(), ["ready"]);

        let again = service
            .plan_checkout(&repo, new_sel("TEST-1", Some(&repo_fx.first)))
            .unwrap();
        let reused = service.prepare_checkout(&again).unwrap();
        assert!(!reused.created);
        assert_eq!(reused.path, plan.destination());
        assert_eq!(adds.load(Ordering::SeqCst), 1);

        let listed = find(service.list_checkouts(&repo).unwrap(), |c| {
            c.name == "TEST-1"
        });
        assert_eq!(listed.path.as_deref(), Some(plan.destination()));
        assert!(!listed.source);
        let existing_plan = service
            .plan_checkout(&repo, existing(&listed, false))
            .unwrap();
        assert!(existing_plan.base_ids().is_empty());
        let opened = service.prepare_checkout(&existing_plan).unwrap();
        assert_eq!(opened.path, plan.destination());
        assert!(!opened.created);
        assert_eq!(adds.load(Ordering::SeqCst), 1);

        // A Ready worktree is found by destination even after its HEAD is detached.
        fx.sh(&["git", "checkout", "-q", "--detach"], dest);
        let detached = find(service.list_checkouts(&repo).unwrap(), |c| {
            c.path.as_deref() == Some(plan.destination())
        });
        assert!(detached.detached);
        let reopened = service
            .prepare_checkout(
                &service
                    .plan_checkout(&repo, existing(&detached, false))
                    .unwrap(),
            )
            .unwrap();
        assert!(reopened.checkout.detached);
    }

    #[test]
    fn git_default_base_is_source_head() {
        let fx = Fixture::new();
        let repo_fx = fx.git_repo("source");
        let service = fx.service(plain());
        let repo = service
            .resolve_repository(&repo_fx.root, Backend::Git)
            .unwrap();
        let plan = service
            .plan_checkout(&repo, new_sel("TEST-5", None))
            .unwrap();
        assert_eq!(plan.base_ids(), std::slice::from_ref(&repo_fx.second));
    }

    #[test]
    fn git_detached_existing_worktree_is_allowed() {
        let fx = Fixture::new();
        let repo_fx = fx.git_repo("source");
        let detached_path = path_str(&fx.temp.join("detached"));
        fx.sh(
            &[
                "git",
                "worktree",
                "add",
                "-q",
                "--detach",
                &detached_path,
                &repo_fx.first,
            ],
            Path::new(&repo_fx.root),
        );
        let service = fx.service(plain());
        let repo = service
            .resolve_repository(&repo_fx.root, Backend::Git)
            .unwrap();
        let listed = find(service.list_checkouts(&repo).unwrap(), |c| c.detached);
        assert_eq!(listed.path.as_deref(), Some(detached_path.as_str()));
        assert_eq!(listed.head.as_deref(), Some(repo_fx.first.as_str()));
        let prepared = service
            .prepare_checkout(
                &service
                    .plan_checkout(&repo, existing(&listed, false))
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(prepared.path, detached_path);
        assert!(prepared.checkout.detached);
    }

    #[test]
    fn git_worktree_replaced_after_planning_is_rejected() {
        let fx = Fixture::new();
        let repo_fx = fx.git_repo("source");
        let root = Path::new(&repo_fx.root);
        let external = path_str(&fx.temp.join("external"));
        fx.sh(
            &[
                "git",
                "worktree",
                "add",
                "-q",
                "-b",
                "EXT-1",
                &external,
                &repo_fx.first,
            ],
            root,
        );
        let service = fx.service(plain());
        let repo = service
            .resolve_repository(&repo_fx.root, Backend::Git)
            .unwrap();
        let listed = find(service.list_checkouts(&repo).unwrap(), |c| {
            c.name == "EXT-1"
        });
        let plan = service
            .plan_checkout(&repo, existing(&listed, false))
            .unwrap();
        assert!(plan.identity.is_some());

        fx.sh(
            &[
                "git",
                "worktree",
                "move",
                &external,
                &format!("{external}.old"),
            ],
            root,
        );
        fx.sh(
            &["git", "worktree", "add", "-q", "-f", &external, "EXT-1"],
            root,
        );
        let error = err_text(service.prepare_checkout(&plan));
        assert!(
            error.contains("was replaced since it was selected")
                && error.contains("select it again"),
            "{error}"
        );

        let fresh = service
            .plan_checkout(&repo, existing(&listed, false))
            .unwrap();
        assert_eq!(service.prepare_checkout(&fresh).unwrap().path, external);
    }

    #[test]
    fn git_conflicts_and_wrong_common_dir_change_nothing() {
        let fx = Fixture::new();
        let repo_fx = fx.git_repo("source");
        let (runner, adds) = counting(None);
        let service = fx.service(runner);
        let repo = service
            .resolve_repository(&repo_fx.root, Backend::Git)
            .unwrap();

        let error = err_text(service.plan_checkout(&repo, new_sel("main", None)));
        assert!(
            error.contains("already checked out") && error.contains("Existing"),
            "{error}"
        );
        fx.sh(
            &["git", "branch", "TEST-3", &repo_fx.first],
            Path::new(&repo_fx.root),
        );
        let error = err_text(service.plan_checkout(&repo, new_sel("TEST-3", None)));
        assert!(
            error.contains("already exists but is not checked out"),
            "{error}"
        );
        assert_eq!(
            fx.sh(&["git", "rev-parse", "TEST-3"], Path::new(&repo_fx.root)),
            repo_fx.first
        );
        for bad in ["-x", "a..b", "@{-1}", "bad\u{7}name"] {
            let error = err_text(service.plan_checkout(&repo, new_sel(bad, None)));
            assert!(error.contains("invalid"), "{bad}: {error}");
        }

        let other = fx.git_repo("other");
        let foreign = Checkout {
            repository_key: repo.key.clone(),
            name: "main".into(),
            path: Some(other.root.clone()),
            head: None,
            detached: false,
            source: false,
            locked: false,
            prunable: false,
            unavailable_reason: None,
        };
        let error = err_text(service.plan_checkout(&repo, existing(&foreign, true)));
        assert!(error.contains("belongs to git:"), "{error}");
        assert_eq!(
            fs::read_to_string(Path::new(&other.root).join("scratch.txt")).unwrap(),
            "untracked bytes\n"
        );
        assert!(!Path::new(&fx.worktree_root()).exists());
        assert_eq!(adds.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn git_replaced_marker_requires_acknowledged_recovery() {
        let fx = Fixture::new();
        let repo_fx = fx.git_repo("source");
        let (runner, adds) = counting(None);
        let service = fx.service(runner);
        let repo = service
            .resolve_repository(&repo_fx.root, Backend::Git)
            .unwrap();
        let plan = service
            .plan_checkout(&repo, new_sel("TEST-1", None))
            .unwrap();
        service.prepare_checkout(&plan).unwrap();

        let marker = Path::new(plan.destination()).join(".git");
        let content = fs::read_to_string(&marker).unwrap();
        fs::remove_file(&marker).unwrap();
        fs::write(&marker, &content).unwrap();

        assert!(is_recovery(
            service.plan_checkout(&repo, new_sel("TEST-1", None))
        ));
        let listed = find(service.list_checkouts(&repo).unwrap(), |c| {
            c.name == "TEST-1"
        });
        assert!(is_recovery(
            service.plan_checkout(&repo, existing(&listed, false))
        ));
        let recovered = service
            .prepare_checkout(
                &service
                    .plan_checkout(&repo, existing(&listed, true))
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(recovered.path, plan.destination());
        assert_eq!(fs::read_to_string(&marker).unwrap(), content);
        assert_eq!(adds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unborn_head_and_broken_nearer_markers_are_errors() {
        let fx = Fixture::new();
        let unborn = fx.temp.join("unborn");
        fs::create_dir(&unborn).unwrap();
        fx.sh(&["git", "init", "-q", "-b", "main"], &unborn);
        let service = fx.service(plain());
        let repo = service
            .resolve_repository(&path_str(&unborn), Backend::Git)
            .unwrap();
        assert!(err_text(service.plan_checkout(&repo, new_sel("TEST-1", None))).contains("unborn"));

        let repo_fx = fx.git_repo("source");
        let nested = Path::new(&repo_fx.root).join("nested");
        fs::create_dir_all(nested.join(".jj")).unwrap();
        assert!(
            err_text(service.resolve_repository(&path_str(&nested), Backend::Jj))
                .contains("broken jj workspace marker")
        );
        let inner = Path::new(&repo_fx.root).join("plain");
        fs::create_dir(&inner).unwrap();
        assert_eq!(
            service
                .resolve_repository(&path_str(&inner), Backend::Jj)
                .unwrap()
                .root,
            repo_fx.root
        );
    }

    fn hook<F>(f: F) -> Option<Box<Intercept>>
    where
        F: Fn(&str, &[String], Option<&Path>, Duration) -> io::Result<RunResult>
            + Send
            + Sync
            + 'static,
    {
        Some(Box::new(f))
    }

    fn fail_after(fx: &Fixture) -> Option<Box<Intercept>> {
        let script = fx.script(
            "fail-after.sh",
            "\"$@\" >/dev/null 2>&1\necho \"simulated crash\" >&2\nexit 1",
        );
        hook(move |file, args, cwd, timeout| {
            let mut argv = vec![file.to_string()];
            argv.extend_from_slice(args);
            real_run(&script, &argv, cwd, timeout)
        })
    }

    #[test]
    fn failing_native_add_is_recorded_never_retried_and_opens_only_acknowledged() {
        let fx = Fixture::new();
        let repo_fx = fx.git_repo("source");
        let (runner, adds) = counting(fail_after(&fx));
        let service = fx.service(runner);
        let repo = service
            .resolve_repository(&repo_fx.root, Backend::Git)
            .unwrap();
        let plan = service
            .plan_checkout(&repo, new_sel("TEST-1", None))
            .unwrap();
        let error = err_text(service.prepare_checkout(&plan));
        assert!(
            error.contains("failed (exit 1)") && error.contains("kept") && error.contains("Failed"),
            "{error}"
        );
        assert_eq!(fx.receipt_states(), ["failed"]);
        assert!(Path::new(plan.destination()).exists());

        assert!(is_recovery(
            service.plan_checkout(&repo, new_sel("TEST-1", None))
        ));
        assert!(is_recovery(service.prepare_checkout(&plan)));
        assert_eq!(adds.load(Ordering::SeqCst), 1);

        let listed = find(service.list_checkouts(&repo).unwrap(), |c| {
            c.name == "TEST-1"
        });
        match service.plan_checkout(&repo, existing(&listed, false)) {
            Err(CheckoutError::RecoveryRequired { existing, .. }) => assert!(existing),
            other => panic!("expected recovery, got {other:?}"),
        }
        let recovered = service
            .prepare_checkout(
                &service
                    .plan_checkout(&repo, existing(&listed, true))
                    .unwrap(),
            )
            .unwrap();
        assert!(!recovered.created);
        assert_eq!(recovered.path, plan.destination());
        assert_eq!(fx.receipt_states(), ["failed"]);

        // Still blocked after the branch is switched.
        let dest = Path::new(plan.destination());
        fx.sh(&["git", "switch", "-q", "-c", "TEST-1-other"], dest);
        let switched = find(service.list_checkouts(&repo).unwrap(), |c| {
            c.path.as_deref() == Some(plan.destination())
        });
        assert_eq!(switched.name, "TEST-1-other");
        assert!(is_recovery(
            service.plan_checkout(&repo, existing(&switched, false))
        ));
        assert_eq!(adds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn timed_out_native_add_is_recorded_failed() {
        let fx = Fixture::new();
        let repo_fx = fx.git_repo("source");
        let hang = fx.script("hang.sh", "\"$@\" >/dev/null 2>&1\nexec sleep 30");
        let (runner, adds) = counting(hook(move |file, args, cwd, _timeout| {
            let mut argv = vec![file.to_string()];
            argv.extend_from_slice(args);
            real_run(&hang, &argv, cwd, Duration::from_secs(1))
        }));
        let service = fx.service(runner);
        let repo = service
            .resolve_repository(&repo_fx.root, Backend::Git)
            .unwrap();
        let plan = service
            .plan_checkout(&repo, new_sel("TEST-1", None))
            .unwrap();
        assert!(err_text(service.prepare_checkout(&plan)).contains("timed out"));
        assert_eq!(fx.receipt_states(), ["failed"]);
        assert!(is_recovery(
            service.plan_checkout(&repo, new_sel("TEST-1", None))
        ));
        assert_eq!(adds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unsaved_failed_receipt_leaves_pending_which_still_blocks() {
        let fx = Fixture::new();
        let repo_fx = fx.git_repo("source");
        let receipts = fx.state_dir().join("receipts");
        let lock_dir = receipts.clone();
        let (runner, adds) = counting(hook(move |file, args, cwd, timeout| {
            real_run(file, args, cwd, timeout)?;
            fs::set_permissions(&lock_dir, fs::Permissions::from_mode(0o500)).unwrap();
            Ok(RunResult {
                code: Some(1),
                stdout: String::new(),
                stderr: "simulated exit".into(),
                killed: false,
            })
        }));
        let service = fx.service(runner);
        let repo = service
            .resolve_repository(&repo_fx.root, Backend::Git)
            .unwrap();
        let plan = service
            .plan_checkout(&repo, new_sel("TEST-1", None))
            .unwrap();
        assert!(err_text(service.prepare_checkout(&plan)).contains("Pending receipt remains"));
        fs::set_permissions(&receipts, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fx.receipt_states(), ["pending"]);
        assert!(is_recovery(service.prepare_checkout(&plan)));
        assert_eq!(adds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ready_receipt_write_failure_is_not_success() {
        let fx = Fixture::new();
        let repo_fx = fx.git_repo("source");
        let receipts = fx.state_dir().join("receipts");
        let lock_dir = receipts.clone();
        let (runner, _) = counting(hook(move |file, args, cwd, timeout| {
            let result = real_run(file, args, cwd, timeout);
            fs::set_permissions(&lock_dir, fs::Permissions::from_mode(0o500)).unwrap();
            result
        }));
        let service = fx.service(runner);
        let repo = service
            .resolve_repository(&repo_fx.root, Backend::Git)
            .unwrap();
        let plan = service
            .plan_checkout(&repo, new_sel("TEST-1", None))
            .unwrap();
        assert!(
            err_text(service.prepare_checkout(&plan)).contains("Ready receipt could not be saved")
        );
        fs::set_permissions(&receipts, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fx.receipt_states(), ["pending"]);
        assert!(Path::new(plan.destination()).exists());
    }

    #[test]
    fn concurrent_prepare_of_same_checkout_is_rejected() {
        let fx = Fixture::new();
        let repo_fx = fx.git_repo("source");
        let entered = Arc::new(std::sync::Barrier::new(2));
        let gate = Arc::new(std::sync::Barrier::new(2));
        let (entered_in, gate_in) = (entered.clone(), gate.clone());
        let (runner, adds) = counting(hook(move |file, args, cwd, timeout| {
            entered_in.wait();
            gate_in.wait();
            real_run(file, args, cwd, timeout)
        }));
        let service = Arc::new(fx.service(runner));
        let repo = service
            .resolve_repository(&repo_fx.root, Backend::Git)
            .unwrap();
        let plan = service
            .plan_checkout(&repo, new_sel("TEST-1", None))
            .unwrap();
        let first = {
            let service = service.clone();
            let plan = plan.clone();
            std::thread::spawn(move || service.prepare_checkout(&plan))
        };
        entered.wait();
        let rival = fx.service(plain());
        let error = err_text(rival.prepare_checkout(&plan));
        assert!(error.contains("state lock is busy"), "{error}");
        gate.wait();
        assert!(first.join().unwrap().unwrap().created);
        assert_eq!(adds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn jj_new_creates_named_workspace_and_reuses_it() {
        if !jj_available() {
            eprintln!("skipping: jj >= 0.46 not available");
            return;
        }
        let fx = Fixture::new();
        let (root, parent) = fx.jj_repo("jjsource");
        let (runner, adds) = counting(None);
        let service = fx.service(runner);
        let repo = service.resolve_repository(&root, Backend::Git).unwrap();
        assert_eq!(
            repo,
            Repository {
                backend: Backend::Jj,
                key: format!("jj:{root}/.jj/repo"),
                root: root.clone(),
                name: "jjsource".into()
            }
        );
        let plan = service
            .plan_checkout(&repo, new_sel("TEST-2", None))
            .unwrap();
        assert_eq!(plan.base_ids(), std::slice::from_ref(&parent));
        assert!(!Path::new(&fx.worktree_root()).exists());
        let prepared = service.prepare_checkout(&plan).unwrap();
        assert!(prepared.created);
        let dest = Path::new(plan.destination());
        assert!(dest.join(".jj").is_dir());
        assert!(!dest.join(".git").exists());
        assert_eq!(
            fs::read_to_string(dest.join("a.txt")).unwrap(),
            "committed\n"
        );
        assert_eq!(
            fs::read_to_string(Path::new(&root).join("a.txt")).unwrap(),
            "dirty edit\n"
        );

        let listed = service.list_checkouts(&repo).unwrap();
        assert!(
            listed.iter().any(|c| c.name == "default"
                && c.path.as_deref() == Some(root.as_str())
                && c.source)
        );
        let entry = find(listed, |c| c.name == "TEST-2");
        assert_eq!(entry.path.as_deref(), Some(plan.destination()));
        assert!(!entry.source);

        let again = service
            .prepare_checkout(
                &service
                    .plan_checkout(&repo, new_sel("TEST-2", None))
                    .unwrap(),
            )
            .unwrap();
        assert!(!again.created);
        let opened = service
            .prepare_checkout(
                &service
                    .plan_checkout(&repo, existing(&entry, false))
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(opened.path, plan.destination());
        assert_eq!(adds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn jj_failing_add_is_recorded_and_mismatched_identity_rejected() {
        if !jj_available() {
            eprintln!("skipping: jj >= 0.46 not available");
            return;
        }
        let fx = Fixture::new();
        let (root, _) = fx.jj_repo("jjsource");
        let (other, _) = fx.jj_repo("otherjj");
        let fail = fx.script("fail.sh", "echo \"simulated failure\" >&2\nexit 1");
        let (runner, adds) = counting(hook(move |_file, _args, cwd, timeout| {
            real_run(&fail, &[], cwd, timeout)
        }));
        let service = fx.service(runner);
        let repo = service.resolve_repository(&root, Backend::Jj).unwrap();
        let plan = service
            .plan_checkout(&repo, new_sel("TEST-2", None))
            .unwrap();
        assert!(err_text(service.prepare_checkout(&plan)).contains("jj workspace add failed"));
        assert_eq!(fx.receipt_states(), ["failed"]);
        assert!(is_recovery(
            service.plan_checkout(&repo, new_sel("TEST-2", None))
        ));
        assert_eq!(adds.load(Ordering::SeqCst), 1);

        let base = Checkout {
            repository_key: repo.key.clone(),
            name: "default".into(),
            path: Some(other.clone()),
            head: None,
            detached: false,
            source: false,
            locked: false,
            prunable: false,
            unavailable_reason: None,
        };
        assert!(
            err_text(service.plan_checkout(&repo, existing(&base, true)))
                .contains("belongs to jj:")
        );
        let unlisted = Checkout {
            name: "TEST-9".into(),
            path: Some(root.clone()),
            ..base
        };
        assert!(
            err_text(service.plan_checkout(&repo, existing(&unlisted, true)))
                .contains("not listed")
        );
        for bad in ["", ".", "..", "-x", "a/b", "a\\b"] {
            assert!(
                err_text(service.plan_checkout(&repo, new_sel(bad, None))).contains("invalid"),
                "{bad}"
            );
        }
    }
}
