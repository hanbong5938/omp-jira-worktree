//! The Tern app: screen state, key handling, background jobs and the event
//! loop. Rendering lives in `view`. Every network, git and file operation runs
//! on a background thread and reports back over a channel tagged with a
//! generation id; results of superseded requests are dropped.

use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use tern_sdk::wire::{RevealAt, ScrollBy, feature};
use tern_sdk::{Event, Input, Key, Options, Session, SurfaceOptions, Waker};

use crate::checkouts::{CheckoutError, CheckoutService};
use crate::config::{self, ConfigError};
use crate::handoff;
use crate::jira::JiraClient;
use crate::launch;
use crate::model::{
    Checkout, CheckoutPlan, CheckoutSelection, Comment, CommentResult, Config, DEFAULT_INSTRUCTION,
    Issue, JiraApi, JiraError, LaunchPlacement, LaunchResult, MAX_COMMENT_CHARS, Page,
    PreparedCheckout, Repository, Snapshot, StatusRef, Transition, TransitionResult, home_dir,
    normalize_issue_key, state_root,
};
use crate::state::StateStore;
use crate::text::{build_search_jql, single_line, truncate_chars};
use crate::transitions::{TransitionChoices, TransitionError, TransitionService};
use crate::view;

/// Initial query when `config.filters` is empty.
pub(crate) const DEFAULT_FILTER_LABEL: &str = "My open issues";
pub(crate) const DEFAULT_FILTER_JQL: &str =
    "assignee = currentUser() AND resolution = Unresolved ORDER BY updated DESC";
const STALE_ACTION_NOTICE: &str =
    "Data is not fresh — press r and wait for a successful fetch before this action.";
const PAGE_ROWS: isize = 10;
/// Server messages shown in notices are clipped to this many characters.
const MESSAGE_LIMIT: usize = 300;

const USAGE: &str = "usage: jira-tern [ISSUE-KEY]

Browse Jira issues, write comments, run status transitions, and create Git
worktrees / jj workspaces with an omp handoff. Runs inside Tern only.

  ISSUE-KEY   open this issue's detail first (e.g. ABC-123)
  -h, --help  show this help";

// ---------------------------------------------------------------------------
// Messages, commands and job results

/// What view handlers report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Msg {
    /// A button: handled exactly like the named key.
    Press(&'static str),
    /// A list row was selected (clicked).
    Pick(usize),
    /// A list row was activated (double click / Enter in Tern).
    Open(usize),
    /// The editor submitted its text.
    Send(String),
    /// A field was clicked and wants the keys.
    Focus,
}

/// Work for the runtime: background jobs and view operations.
pub(crate) enum Cmd {
    Search {
        generation: u64,
        jql: String,
    },
    Issue {
        generation: u64,
        key: String,
    },
    Comments {
        generation: u64,
        key: String,
        limit: usize,
    },
    AddComment {
        generation: u64,
        key: String,
        body: String,
    },
    LoadTransitions {
        generation: u64,
        key: String,
    },
    ApplyTransition {
        generation: u64,
        key: String,
        from: StatusRef,
        transition: Transition,
    },
    AckTransition {
        generation: u64,
        key: String,
        attempt: String,
    },
    ResolveRepos {
        generation: u64,
        candidates: Vec<RepoCandidate>,
    },
    ResolvePath {
        generation: u64,
        path: String,
    },
    ListCheckouts {
        generation: u64,
        repository: Repository,
    },
    Plan {
        generation: u64,
        repository: Repository,
        selection: CheckoutSelection,
    },
    Snapshot {
        generation: u64,
        key: String,
    },
    Execute {
        generation: u64,
        plan: CheckoutPlan,
        handoff: Option<HandoffRequest>,
    },
    Launch {
        generation: u64,
        ctx: LaunchCtx,
    },
    Reveal(String, RevealAt),
    Scroll(String, ScrollBy),
}

/// Snapshot, instruction and placement for an omp launch after the checkout.
pub(crate) struct HandoffRequest {
    pub snapshot: Snapshot,
    pub instruction: String,
    pub placement: LaunchPlacement,
}

/// A finished background job.
pub(crate) enum Done {
    Search {
        generation: u64,
        result: Result<Page<Issue>, JiraError>,
    },
    Issue {
        generation: u64,
        result: Result<Issue, JiraError>,
    },
    Comments {
        generation: u64,
        result: Result<Page<Comment>, JiraError>,
    },
    Comment {
        generation: u64,
        result: CommentResult,
    },
    TransitionsLoaded {
        generation: u64,
        result: Result<TransitionChoices, TransitionError>,
    },
    TransitionApplied {
        generation: u64,
        result: Result<TransitionResult, TransitionError>,
    },
    TransitionAcknowledged {
        generation: u64,
        result: Result<(), TransitionError>,
    },
    Repos {
        generation: u64,
        choices: Vec<RepoChoice>,
    },
    RepoPath {
        generation: u64,
        path: String,
        result: Result<Repository, String>,
    },
    Checkouts {
        generation: u64,
        result: Result<Vec<Checkout>, String>,
    },
    Planned {
        generation: u64,
        result: Result<CheckoutPlan, CheckoutError>,
    },
    Snapshot {
        generation: u64,
        result: Result<Snapshot, JiraError>,
    },
    Executed {
        generation: u64,
        outcome: ExecOutcome,
    },
}

/// What happened after the final wizard confirmation.
pub(crate) enum ExecOutcome {
    /// `prepare_checkout` failed; lines describe why.
    CheckoutFailed(Vec<String>),
    /// Checkout only: ready.
    Ready(PreparedCheckout),
    /// The checkout is ready but the handoff file could not be written.
    HandoffFailed {
        prepared: PreparedCheckout,
        message: String,
    },
    /// Launch attempted: `Err` holds the re-validation problem (nothing launched).
    Launched {
        ctx: LaunchCtx,
        result: Result<LaunchResult, Vec<String>>,
    },
}

/// Everything needed to (re-)launch omp in a prepared checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchCtx {
    pub prepared: PreparedCheckout,
    pub snapshot_path: PathBuf,
    pub placement: LaunchPlacement,
    pub recovery_acknowledged: bool,
}

// ---------------------------------------------------------------------------
// Small shared types

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Notice {
    pub level: Level,
    pub text: String,
}

impl Notice {
    fn new(level: Level, text: impl Into<String>) -> Self {
        Notice {
            level,
            text: text.into(),
        }
    }
}

/// A selectable row of a picker step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    pub label: String,
    pub detail: String,
    pub disabled: bool,
}

impl Choice {
    fn new(label: impl Into<String>, detail: impl Into<String>, disabled: bool) -> Self {
        Choice {
            label: label.into(),
            detail: detail.into(),
            disabled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueryRef {
    pub label: String,
    pub jql: String,
}

/// Named filters to offer; the default JQL stands in when none are configured.
pub(crate) fn filter_choices(config: &Config) -> Vec<QueryRef> {
    if config.filters.is_empty() {
        return vec![QueryRef {
            label: DEFAULT_FILTER_LABEL.into(),
            jql: DEFAULT_FILTER_JQL.into(),
        }];
    }
    config
        .filters
        .iter()
        .map(|f| QueryRef {
            label: f.name.clone(),
            jql: f.jql.clone(),
        })
        .collect()
}

/// Single-line, sanitized error text (never empty).
fn err_text(error: &dyn std::fmt::Display) -> String {
    let text = single_line(&error.to_string());
    if text.is_empty() {
        "unknown error".into()
    } else {
        text
    }
}

fn clip(text: &str) -> String {
    truncate_chars(&single_line(text), MESSAGE_LIMIT)
}

pub(crate) fn status_text(status: &StatusRef) -> String {
    format!(
        "{} ({})",
        single_line(&status.name),
        single_line(&status.id)
    )
}

/// Whether `key` types exactly `text` (no Ctrl/Alt/Meta).
fn is_char(key: &Key, text: &str) -> bool {
    key.typed() == Some(text)
}

fn is_confirm(key: &Key) -> bool {
    key.is("enter") || is_char(key, "y")
}

/// Line/page/end scrolling shared by scrollable text panes.
fn scroll_key(key: &Key) -> Option<ScrollBy> {
    if key.is("up") || is_char(key, "k") {
        Some(ScrollBy::LineUp)
    } else if key.is("down") || is_char(key, "j") {
        Some(ScrollBy::LineDown)
    } else if key.is("page_up") {
        Some(ScrollBy::PageUp)
    } else if key.is("page_down") {
        Some(ScrollBy::PageDown)
    } else if key.is("end") || is_char(key, "G") {
        Some(ScrollBy::End)
    } else {
        None
    }
}

/// Scrolls the static wizard text; returns whether `key` was a scroll key.
fn scroll_wizard_text(key: &Key, cmds: &mut Vec<Cmd>) -> bool {
    let by = if key.is("home") || is_char(key, "g") {
        ScrollBy::Start
    } else if let Some(by) = scroll_key(key) {
        by
    } else {
        return false;
    };
    cmds.push(Cmd::Scroll(view::WIZARD_TEXT_ID.to_owned(), by));
    true
}

fn is_back(key: &Key) -> bool {
    key.is("escape") || is_char(key, "n")
}

fn is_submit(key: &Key) -> bool {
    key.is("ctrl+enter") || key.is("ctrl+s")
}

/// `current` moved by `delta`, clamped to `0..len` (0 when empty).
pub(crate) fn step_index(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let next = (current.min(len - 1) as isize).saturating_add(delta);
    next.clamp(0, len as isize - 1) as usize
}

/// The selection delta of a list navigation key.
fn nav_delta(key: &Key) -> Option<isize> {
    if key.is("up") || is_char(key, "k") {
        Some(-1)
    } else if key.is("down") || is_char(key, "j") {
        Some(1)
    } else if key.is("page_up") {
        Some(-PAGE_ROWS)
    } else if key.is("page_down") {
        Some(PAGE_ROWS)
    } else if key.is("home") || is_char(key, "g") {
        Some(isize::MIN)
    } else if key.is("end") || is_char(key, "G") {
        Some(isize::MAX)
    } else {
        None
    }
}

/// List navigation keys; true when `key` moved the selection.
fn nav(key: &Key, selected: &mut usize, len: usize) -> bool {
    let Some(delta) = nav_delta(key) else {
        return false;
    };
    *selected = step_index(*selected, delta, len);
    true
}

// ---------------------------------------------------------------------------
// Text buffer (program-owned editor/input text)

/// Text and caret (in chars) of an input or editor the program owns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TextBuf {
    pub text: String,
    pub cursor: usize,
}

impl TextBuf {
    /// `text` with the caret at its end.
    pub(crate) fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        TextBuf { text, cursor }
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(i, _)| i)
    }

    pub(crate) fn insert(&mut self, text: &str) {
        let at = self.byte(self.cursor);
        self.text.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    /// The caret in UTF-16 code units, as Tern expects it.
    pub(crate) fn utf16_cursor(&self) -> usize {
        self.text
            .chars()
            .take(self.cursor)
            .map(char::len_utf16)
            .sum()
    }

    /// Char index of every line start.
    fn line_starts(&self) -> Vec<usize> {
        std::iter::once(0)
            .chain(
                self.text
                    .chars()
                    .enumerate()
                    .filter(|(_, c)| *c == '\n')
                    .map(|(i, _)| i + 1),
            )
            .collect()
    }

    fn line_bounds(&self) -> (usize, usize) {
        let starts = self.line_starts();
        let line = starts.iter().rposition(|&s| s <= self.cursor).unwrap_or(0);
        let end = starts.get(line + 1).map_or(self.len(), |next| next - 1);
        (starts[line], end)
    }

    fn vertical(&mut self, delta: isize) {
        let starts = self.line_starts();
        let line = starts.iter().rposition(|&s| s <= self.cursor).unwrap_or(0);
        let column = self.cursor - starts[line];
        let target = line as isize + delta;
        if target < 0 {
            self.cursor = 0;
        } else if target as usize >= starts.len() {
            self.cursor = self.len();
        } else {
            let target = target as usize;
            let end = starts.get(target + 1).map_or(self.len(), |next| next - 1);
            self.cursor = (starts[target] + column).min(end);
        }
    }

    /// Applies an editing key; false when the key is not an edit.
    pub(crate) fn edit(&mut self, key: &Key, multiline: bool) -> bool {
        let plain = !key.ctrl && !key.alt && !key.meta;
        match key.name.as_str() {
            "backspace" => {
                if self.cursor > 0 {
                    let at = self.byte(self.cursor - 1);
                    self.text.remove(at);
                    self.cursor -= 1;
                }
            }
            "delete" => {
                if self.cursor < self.len() {
                    let at = self.byte(self.cursor);
                    self.text.remove(at);
                }
            }
            "left" => self.cursor = self.cursor.saturating_sub(1),
            "right" => self.cursor = (self.cursor + 1).min(self.len()),
            "home" if multiline => self.cursor = self.line_bounds().0,
            "end" if multiline => self.cursor = self.line_bounds().1,
            "home" => self.cursor = 0,
            "end" => self.cursor = self.len(),
            "up" if multiline => self.vertical(-1),
            "down" if multiline => self.vertical(1),
            "enter" if multiline && plain => self.insert("\n"),
            "paste" => {
                let pasted = key
                    .text
                    .as_deref()
                    .unwrap_or_default()
                    .replace("\r\n", "\n")
                    .replace('\r', "\n");
                let pasted = if multiline {
                    pasted
                } else {
                    pasted.replace('\n', " ")
                };
                self.insert(&pasted);
            }
            _ => match key.typed() {
                Some(text) => self.insert(text),
                None => return false,
            },
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Generation-guarded loads

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadStatus {
    Loading,
    Ready,
    Empty,
    Error,
}

/// One logical resource (list query, issue, comments). Only the latest
/// request's result is applied; failures keep previous data, marked stale.
#[derive(Debug, Clone)]
pub(crate) struct Loader<T> {
    pub status: LoadStatus,
    /// Key of the latest request (JQL or issue key).
    pub key: String,
    /// Last successfully applied data, possibly from an earlier key.
    pub data: Option<T>,
    pub data_key: Option<String>,
    /// `data` does not reflect a fresh successful response for `key`.
    pub stale: bool,
    pub error: Option<String>,
    pending: Option<u64>,
}

pub(crate) enum Applied<T> {
    /// The result belongs to a superseded request.
    Dropped,
    Failed,
    Loaded {
        previous: Option<T>,
        previous_key: Option<String>,
    },
}

impl<T> Loader<T> {
    pub(crate) fn new() -> Self {
        Loader {
            status: LoadStatus::Loading,
            key: String::new(),
            data: None,
            data_key: None,
            stale: false,
            error: None,
            pending: None,
        }
    }

    pub(crate) fn start(&mut self, generation: u64, key: &str) {
        self.status = LoadStatus::Loading;
        self.key = key.to_owned();
        self.stale = self.data.is_some();
        self.error = None;
        self.pending = Some(generation);
    }

    pub(crate) fn finish(
        &mut self,
        generation: u64,
        result: Result<T, String>,
        is_empty: impl Fn(&T) -> bool,
    ) -> Applied<T> {
        if self.pending != Some(generation) {
            return Applied::Dropped;
        }
        self.pending = None;
        match result {
            Ok(data) => {
                self.status = if is_empty(&data) {
                    LoadStatus::Empty
                } else {
                    LoadStatus::Ready
                };
                self.stale = false;
                self.error = None;
                let previous = self.data.replace(data);
                let previous_key = self.data_key.replace(self.key.clone());
                Applied::Loaded {
                    previous,
                    previous_key,
                }
            }
            Err(error) => {
                self.status = LoadStatus::Error;
                self.stale = self.data.is_some();
                self.error = Some(error);
                Applied::Failed
            }
        }
    }

    /// True only after a successful response for the current key with no later failure.
    pub(crate) fn fresh(&self) -> bool {
        matches!(self.status, LoadStatus::Ready | LoadStatus::Empty) && !self.stale
    }
}

/// List query and selection.
pub(crate) struct ListState {
    pub query: QueryRef,
    pub selected: usize,
    /// False until the first load (an initial issue key opens detail without a list request).
    pub started: bool,
    pub loader: Loader<Page<Issue>>,
}

impl ListState {
    fn new(query: QueryRef) -> Self {
        ListState {
            query,
            selected: 0,
            started: false,
            loader: Loader::new(),
        }
    }

    pub(crate) fn load(&mut self, query: QueryRef, generation: u64) {
        self.query = query;
        self.started = true;
        self.loader.start(generation, &self.query.jql);
    }

    /// A new query resets the selection; a refresh of the same JQL keeps the selected issue by key.
    pub(crate) fn apply(&mut self, generation: u64, result: Result<Page<Issue>, String>) {
        let Applied::Loaded {
            previous,
            previous_key,
        } = self
            .loader
            .finish(generation, result, |p| p.items.is_empty())
        else {
            return;
        };
        let items = self.items();
        if previous_key.as_deref() != Some(self.loader.key.as_str()) {
            self.selected = 0;
            return;
        }
        let keep = previous.and_then(|p| p.items.get(self.selected).map(|i| i.key.clone()));
        let found = keep.and_then(|k| items.iter().position(|i| i.key == k));
        self.selected = found.unwrap_or_else(|| step_index(self.selected, 0, items.len()));
    }

    pub(crate) fn items(&self) -> &[Issue] {
        self.loader
            .data
            .as_ref()
            .map_or(&[], |p| p.items.as_slice())
    }

    pub(crate) fn move_by(&mut self, delta: isize) {
        self.selected = step_index(self.selected, delta, self.items().len());
    }

    /// The selected row of the displayed (possibly stale) data.
    pub(crate) fn selected_issue(&self) -> Option<&Issue> {
        self.items().get(self.selected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pane {
    Description,
    Comments,
}

pub(crate) struct Detail {
    pub key: String,
    pub issue: Loader<Issue>,
    /// `None` when `handoff.max_comments` is 0 (comments are not fetched).
    pub comments: Option<Loader<Page<Comment>>>,
    pub focus: Pane,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Base {
    List,
    Detail,
}

// ---------------------------------------------------------------------------
// Flows (modal screens over the list/detail)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptKind {
    Search,
    Jql,
}

#[allow(
    clippy::large_enum_variant,
    reason = "one Flow per app, replaced on screen changes only"
)]
pub(crate) enum Flow {
    Prompt { kind: PromptKind, buf: TextBuf },
    Filter { selected: usize },
    Compose(Compose),
    Transition(TransitionFlow),
    Wizard(Box<Wizard>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposeStep {
    Editing,
    /// Esc on a non-empty draft: discard?
    Discard,
    /// A previous post had an unknown outcome: confirm Jira was checked first.
    Recheck,
    Confirm,
    Posting {
        generation: u64,
    },
}

pub(crate) struct Compose {
    pub key: String,
    pub draft: TextBuf,
    pub step: ComposeStep,
    pub message: Option<Notice>,
    /// The last post of this draft had an unknown outcome.
    pub uncertain: bool,
}

/// The body to post, or why it is refused.
pub(crate) fn validate_comment(text: &str) -> Result<&str, String> {
    let body = text.trim();
    if body.is_empty() {
        return Err("The comment is empty; nothing to post.".into());
    }
    let count = body.chars().count();
    if count > MAX_COMMENT_CHARS {
        return Err(format!(
            "The comment has {count} characters; Jira accepts at most {MAX_COMMENT_CHARS}. Shorten it before posting."
        ));
    }
    Ok(body)
}

impl Compose {
    pub(crate) fn new(key: String) -> Self {
        Compose {
            key,
            draft: TextBuf::default(),
            step: ComposeStep::Editing,
            message: None,
            uncertain: false,
        }
    }

    /// Validates the draft and moves to confirmation (after the recheck warning when
    /// the previous post had an unknown outcome).
    pub(crate) fn request_send(&mut self) {
        match validate_comment(&self.draft.text) {
            Err(message) => self.message = Some(Notice::new(Level::Warning, message)),
            Ok(_) => {
                self.message = None;
                self.step = if self.uncertain {
                    ComposeStep::Recheck
                } else {
                    ComposeStep::Confirm
                };
            }
        }
    }
}

pub(crate) enum TStep {
    Loading {
        generation: u64,
    },
    Blocked {
        since: String,
        attempt: String,
        url: String,
    },
    Acknowledging {
        generation: u64,
    },
    Pick {
        choices: TransitionChoices,
        selected: usize,
    },
    Confirm {
        choices: TransitionChoices,
        index: usize,
    },
    Applying {
        generation: u64,
        url: String,
    },
    Finished {
        title: String,
        lines: Vec<String>,
        level: Level,
    },
}

pub(crate) struct TransitionFlow {
    pub key: String,
    pub step: TStep,
    pub message: Option<Notice>,
}

pub(crate) fn transition_items(choices: &TransitionChoices) -> Vec<Choice> {
    choices
        .transitions
        .iter()
        .map(|t| {
            let label = format!(
                "{} → {} [#{}]",
                single_line(&t.name),
                single_line(&t.to.name),
                single_line(&t.id)
            );
            match TransitionService::eligibility(t) {
                Ok(()) => Choice::new(label, format!("→ {}", status_text(&t.to)), false),
                Err(reason) => Choice::new(
                    label,
                    format!(
                        "{} · {}",
                        single_line(&reason),
                        single_line(&choices.issue.url)
                    ),
                    true,
                ),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckoutAction {
    Checkout,
    Split,
    Tab,
}

pub(crate) const ACTIONS: [CheckoutAction; 3] = [
    CheckoutAction::Checkout,
    CheckoutAction::Split,
    CheckoutAction::Tab,
];

impl CheckoutAction {
    pub(crate) fn placement(self) -> Option<LaunchPlacement> {
        match self {
            CheckoutAction::Checkout => None,
            CheckoutAction::Split => Some(LaunchPlacement::Split),
            CheckoutAction::Tab => Some(LaunchPlacement::Tab),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            CheckoutAction::Checkout => "checkout only",
            CheckoutAction::Split => "start omp in a split (right)",
            CheckoutAction::Tab => "start omp in a new tab",
        }
    }

    fn index(self) -> usize {
        ACTIONS.iter().position(|a| *a == self).unwrap_or(0)
    }
}

pub(crate) fn action_items() -> Vec<Choice> {
    ACTIONS
        .iter()
        .map(|a| {
            let detail = match a {
                CheckoutAction::Checkout => {
                    "create/reuse the checkout and show its path; no snapshot, no agent"
                }
                CheckoutAction::Split => "fresh Jira snapshot → new omp in a split to the right",
                CheckoutAction::Tab => "fresh Jira snapshot → new omp in a new tab",
            };
            Choice::new(a.label(), detail, false)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoCandidate {
    pub path: String,
    pub tags: Vec<String>,
    pub mapped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoChoice {
    pub path: String,
    pub tags: Vec<String>,
    pub mapped: bool,
    pub result: Result<Repository, String>,
}

/// Duplicates (same repository key) merge their tags; failures stay as unselectable entries.
pub(crate) fn merge_choices(choices: Vec<RepoChoice>) -> Vec<RepoChoice> {
    let mut merged: Vec<RepoChoice> = Vec::new();
    for choice in choices {
        let key = choice.result.as_ref().ok().map(|r| r.key.clone());
        if let Some(key) = key
            && let Some(same) = merged
                .iter_mut()
                .find(|o| matches!(&o.result, Ok(r) if r.key == key))
        {
            same.tags.extend(choice.tags);
            same.mapped |= choice.mapped;
            continue;
        }
        merged.push(choice);
    }
    merged
}

fn initial_repo(choices: &[RepoChoice]) -> usize {
    choices
        .iter()
        .position(|c| c.mapped && c.result.is_ok())
        .or_else(|| choices.iter().position(|c| c.result.is_ok()))
        .unwrap_or(choices.len())
}

pub(crate) fn checkout_disabled(c: &Checkout) -> bool {
    c.prunable || (c.path.is_some() && c.unavailable_reason.is_some())
}

/// Where a message screen leads.
pub(crate) enum Next {
    Step(Box<WStep>),
    ListExisting(Option<String>),
}

/// A wizard result screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub title: String,
    pub lines: Vec<String>,
    pub level: Level,
    /// Re-validation failed before launch; `r` retries (nothing was launched).
    pub retry: Option<LaunchCtx>,
    /// The launch outcome is unknown; `l` re-launches after a confirmation.
    pub relaunch: Option<LaunchCtx>,
}

pub(crate) enum WStep {
    Resolving {
        generation: u64,
    },
    Repo {
        selected: usize,
    },
    RepoPath {
        buf: TextBuf,
    },
    ResolvingPath {
        generation: u64,
        path: String,
    },
    Kind {
        selected: usize,
    },
    NewName {
        buf: TextBuf,
    },
    Planning {
        generation: u64,
        selection: CheckoutSelection,
    },
    Listing {
        generation: u64,
    },
    Existing {
        selected: usize,
    },
    ExistingPath {
        checkout: Checkout,
        buf: TextBuf,
    },
    RecoveryAck {
        checkout: Checkout,
        message: String,
        path: String,
    },
    Action {
        selected: usize,
    },
    Snapshotting {
        generation: u64,
    },
    Preview,
    PartialAccept,
    Instruction,
    Confirm,
    Executing {
        generation: u64,
    },
    Outcome(Outcome),
    RelaunchConfirm(Outcome),
    Message {
        title: String,
        lines: Vec<String>,
        next: Next,
    },
}

pub(crate) struct Wizard {
    pub key: String,
    pub step: WStep,
    pub choices: Vec<RepoChoice>,
    pub repository: Option<Repository>,
    pub existing: Vec<Checkout>,
    pub existing_prefer: Option<String>,
    pub plan: Option<CheckoutPlan>,
    pub action: CheckoutAction,
    pub snapshot: Option<Snapshot>,
    pub instruction: TextBuf,
    pub message: Option<Notice>,
}

impl Wizard {
    fn backend_noun(&self) -> &'static str {
        match self.repository.as_ref().map(|r| r.backend) {
            Some(crate::model::Backend::Jj) => "workspace",
            _ => "worktree",
        }
    }
}

pub(crate) fn repo_items(w: &Wizard) -> Vec<Choice> {
    let mut items: Vec<Choice> = w
        .choices
        .iter()
        .map(|c| {
            let star = if c.mapped { "★ " } else { "" };
            match &c.result {
                Ok(r) => Choice::new(
                    format!("{star}{} [{}]", single_line(&r.name), r.backend.label()),
                    format!("{} · {}", r.root, c.tags.join(", ")),
                    false,
                ),
                Err(e) => Choice::new(
                    format!("{star}{} — unavailable", c.path),
                    format!("{} · {}", c.tags.join(", "), e),
                    true,
                ),
            }
        })
        .collect();
    items.push(Choice::new(
        "Enter a repository path…",
        "absolute, ~/, or relative to the current directory",
        false,
    ));
    items
}

pub(crate) fn kind_items(w: &Wizard) -> Vec<Choice> {
    vec![
        Choice::new(
            "New checkout",
            format!(
                "name defaults to {}; base resolved to concrete ids before confirmation",
                w.key
            ),
            false,
        ),
        Choice::new(
            "Existing checkout…",
            format!("reuse a listed {}", w.backend_noun()),
            false,
        ),
    ]
}

pub(crate) fn existing_items(w: &Wizard) -> Vec<Choice> {
    w.existing
        .iter()
        .map(|c| {
            let flags: Vec<&str> = [
                (c.source, "source"),
                (c.detached, "detached"),
                (c.locked, "locked"),
                (c.prunable, "prunable"),
            ]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, f)| *f)
            .collect();
            let label = if flags.is_empty() {
                single_line(&c.name)
            } else {
                format!("{} [{}]", single_line(&c.name), flags.join(", "))
            };
            if c.prunable {
                let path = c.path.as_deref().unwrap_or("path unknown");
                return Choice::new(
                    label,
                    format!("{path} · prunable/missing — not selectable"),
                    true,
                );
            }
            match (&c.path, &c.unavailable_reason) {
                (None, reason) => Choice::new(
                    label,
                    format!(
                        "path unknown ({}) — you will be asked for the path",
                        reason.as_deref().unwrap_or("lookup failed")
                    ),
                    false,
                ),
                (Some(path), Some(reason)) => {
                    Choice::new(label, format!("{path} · unavailable: {reason}"), true)
                }
                (Some(path), None) => {
                    let head = c
                        .head
                        .as_deref()
                        .map(|h| format!(" · {}", h.chars().take(12).collect::<String>()));
                    Choice::new(label, format!("{path}{}", head.unwrap_or_default()), false)
                }
            }
        })
        .collect()
}

pub(crate) fn filter_items(config: &Config) -> Vec<Choice> {
    filter_choices(config)
        .into_iter()
        .map(|q| Choice::new(single_line(&q.label), single_line(&q.jql), false))
        .collect()
}

/// `~`, `~/…`, absolute, or relative to `cwd`.
pub(crate) fn expand_path(input: &str, cwd: &str) -> String {
    let trimmed = input.trim();
    let home = home_dir();
    if trimmed == "~" {
        return home.to_string_lossy().into_owned();
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return home.join(rest).to_string_lossy().into_owned();
    }
    let path = std::path::Path::new(trimmed);
    if path.is_absolute() {
        trimmed.to_owned()
    } else {
        std::path::Path::new(cwd)
            .join(path)
            .to_string_lossy()
            .into_owned()
    }
}

// ---------------------------------------------------------------------------
// App state

pub(crate) struct State {
    pub config: Config,
    pub cwd: String,
    /// Where handoff files are written (shown in the final confirmation).
    pub handoff_dir: PathBuf,
    pub list: ListState,
    pub detail: Option<Detail>,
    pub base: Base,
    pub flow: Option<Flow>,
    pub notice: Option<Notice>,
    pub quit: bool,
    generation: u64,
}

impl State {
    pub(crate) fn new(
        config: Config,
        cwd: String,
        handoff_dir: PathBuf,
        initial_key: Option<String>,
    ) -> (Self, Vec<Cmd>) {
        let query = filter_choices(&config).remove(0);
        let mut state = State {
            config,
            cwd,
            handoff_dir,
            list: ListState::new(query.clone()),
            detail: None,
            base: Base::List,
            flow: None,
            notice: None,
            quit: false,
            generation: 0,
        };
        let mut cmds = Vec::new();
        match initial_key {
            Some(key) => state.open_detail(key, &mut cmds),
            None => state.load_query(query, &mut cmds),
        }
        (state, cmds)
    }

    fn next_gen(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// A Jira/state write or the checkout/launch execution is in flight; quitting is refused.
    pub(crate) fn write_in_flight(&self) -> bool {
        match &self.flow {
            Some(Flow::Compose(c)) => matches!(c.step, ComposeStep::Posting { .. }),
            Some(Flow::Transition(t)) => {
                matches!(t.step, TStep::Applying { .. } | TStep::Acknowledging { .. })
            }
            Some(Flow::Wizard(w)) => matches!(w.step, WStep::Executing { .. }),
            _ => false,
        }
    }

    /// The selectable rows of the current flow step and the selected index.
    pub(crate) fn flow_choices(&self) -> Option<(Vec<Choice>, usize)> {
        match self.flow.as_ref()? {
            Flow::Filter { selected } => Some((filter_items(&self.config), *selected)),
            Flow::Transition(t) => match &t.step {
                TStep::Pick { choices, selected } => Some((transition_items(choices), *selected)),
                _ => None,
            },
            Flow::Wizard(w) => match &w.step {
                WStep::Repo { selected } => Some((repo_items(w), *selected)),
                WStep::Kind { selected } => Some((kind_items(w), *selected)),
                WStep::Existing { selected } => Some((existing_items(w), *selected)),
                WStep::Action { selected } => Some((action_items(), *selected)),
                _ => None,
            },
            _ => None,
        }
    }

    fn flow_selected_mut(&mut self) -> Option<&mut usize> {
        match self.flow.as_mut()? {
            Flow::Filter { selected } => Some(selected),
            Flow::Transition(TransitionFlow {
                step: TStep::Pick { selected, .. },
                ..
            }) => Some(selected),
            Flow::Wizard(w) => match &mut w.step {
                WStep::Repo { selected }
                | WStep::Kind { selected }
                | WStep::Existing { selected }
                | WStep::Action { selected } => Some(selected),
                _ => None,
            },
            _ => None,
        }
    }

    fn set_selection(&mut self, index: usize) {
        if self.flow.is_some() {
            let len = self.flow_choices().map_or(0, |(items, _)| items.len());
            if let Some(selected) = self.flow_selected_mut() {
                *selected = step_index(index, 0, len);
            }
        } else if self.base == Base::List {
            self.list.selected = step_index(index, 0, self.list.items().len());
        }
    }

    // -- list / detail -------------------------------------------------------

    fn load_query(&mut self, query: QueryRef, cmds: &mut Vec<Cmd>) {
        let generation = self.next_gen();
        self.base = Base::List;
        self.detail = None;
        self.list.load(query, generation);
        cmds.push(Cmd::Search {
            generation,
            jql: self.list.query.jql.clone(),
        });
    }

    fn refresh_list(&mut self, cmds: &mut Vec<Cmd>) {
        let query = self.list.query.clone();
        let generation = self.next_gen();
        self.list.load(query, generation);
        cmds.push(Cmd::Search {
            generation,
            jql: self.list.query.jql.clone(),
        });
    }

    fn open_detail(&mut self, key: String, cmds: &mut Vec<Cmd>) {
        let comments = (self.config.handoff.max_comments > 0).then(Loader::new);
        self.detail = Some(Detail {
            key,
            issue: Loader::new(),
            comments,
            focus: Pane::Description,
        });
        self.base = Base::Detail;
        self.refresh_detail(cmds);
    }

    fn refresh_detail(&mut self, cmds: &mut Vec<Cmd>) {
        let issue_gen = self.next_gen();
        let comments_gen = self.next_gen();
        let limit = self.config.handoff.max_comments;
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        let key = detail.key.clone();
        detail.issue.start(issue_gen, &key);
        cmds.push(Cmd::Issue {
            generation: issue_gen,
            key: key.clone(),
        });
        if let Some(comments) = detail.comments.as_mut() {
            comments.start(comments_gen, &key);
            cmds.push(Cmd::Comments {
                generation: comments_gen,
                key,
                limit,
            });
        }
    }

    fn refresh_comments(&mut self, key: &str, cmds: &mut Vec<Cmd>) {
        let generation = self.next_gen();
        let limit = self.config.handoff.max_comments;
        if let Some(detail) = self.detail.as_mut().filter(|d| d.key == key)
            && let Some(comments) = detail.comments.as_mut()
        {
            comments.start(generation, key);
            cmds.push(Cmd::Comments {
                generation,
                key: key.to_owned(),
                limit,
            });
        }
    }

    fn close_detail(&mut self, cmds: &mut Vec<Cmd>) {
        self.detail = None;
        self.base = Base::List;
        if !self.list.started {
            let query = self.list.query.clone();
            self.load_query(query, cmds);
        }
    }

    /// After a status change: refetch the current query generation and the open detail.
    fn refresh_after_transition(&mut self, cmds: &mut Vec<Cmd>) {
        if self.list.started {
            self.refresh_list(cmds);
        }
        self.refresh_detail(cmds);
    }

    // -- input ---------------------------------------------------------------

    pub(crate) fn on_key(&mut self, key: &Key) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        self.notice = None;
        if key.is("ctrl+c") {
            if self.write_in_flight() {
                self.notice = Some(Notice::new(
                    Level::Warning,
                    "A write is in flight; wait for its result before quitting.",
                ));
            } else {
                self.quit = true;
            }
            return cmds;
        }
        match self.flow.take() {
            Some(flow) => self.flow = self.flow_key(flow, key, &mut cmds),
            None => match self.base {
                Base::List => self.list_key(key, &mut cmds),
                Base::Detail => self.detail_key(key, &mut cmds),
            },
        }
        cmds
    }

    pub(crate) fn on_msg(&mut self, msg: Msg) -> Vec<Cmd> {
        match msg {
            Msg::Press(name) => {
                // Single characters type themselves so `is_char` matches them like real keys.
                let text = (name.chars().count() == 1).then(|| name.to_owned());
                self.on_key(&Key {
                    text,
                    ..Key::named(name)
                })
            }
            Msg::Pick(index) => {
                self.set_selection(index);
                Vec::new()
            }
            Msg::Open(index) => {
                self.set_selection(index);
                self.on_key(&Key::named("enter"))
            }
            Msg::Send(text) => {
                match self.flow.as_mut() {
                    Some(Flow::Compose(c)) if c.step == ComposeStep::Editing => {
                        c.draft = TextBuf::new(text);
                        c.request_send();
                    }
                    Some(Flow::Wizard(w)) if matches!(w.step, WStep::Instruction) => {
                        w.instruction = TextBuf::new(text);
                        w.step = instruction_step(w);
                    }
                    _ => {}
                }
                Vec::new()
            }
            Msg::Focus => Vec::new(),
        }
    }

    fn require_fresh_issue(&mut self, from_list: bool) -> Option<String> {
        if from_list {
            let Some(issue) = self.list.selected_issue() else {
                self.notice = Some(Notice::new(Level::Warning, "No issue selected."));
                return None;
            };
            let key = issue.key.clone();
            if !self.list.loader.fresh() {
                self.notice = Some(Notice::new(Level::Warning, STALE_ACTION_NOTICE));
                return None;
            }
            return Some(key);
        }
        let detail = self.detail.as_ref()?;
        if !detail.issue.fresh() {
            self.notice = Some(Notice::new(Level::Warning, STALE_ACTION_NOTICE));
            return None;
        }
        Some(detail.key.clone())
    }

    fn list_key(&mut self, key: &Key, cmds: &mut Vec<Cmd>) {
        if key.is("escape") || is_char(key, "q") {
            self.quit = true;
            return;
        }
        if let Some(delta) = nav_delta(key) {
            self.list.move_by(delta);
            return;
        }
        if is_char(key, "J") {
            let buf = TextBuf::new(self.list.query.jql.clone());
            self.flow = Some(Flow::Prompt {
                kind: PromptKind::Jql,
                buf,
            });
        } else if is_char(key, "/") {
            self.flow = Some(Flow::Prompt {
                kind: PromptKind::Search,
                buf: TextBuf::default(),
            });
        } else if is_char(key, "f") {
            let current = filter_choices(&self.config)
                .iter()
                .position(|q| q.jql == self.list.query.jql);
            self.flow = Some(Flow::Filter {
                selected: current.unwrap_or(0),
            });
        } else if is_char(key, "r") {
            self.refresh_list(cmds);
        } else if key.is("enter") {
            if let Some(issue) = self.list.selected_issue() {
                let key = issue.key.clone();
                self.open_detail(key, cmds);
            }
        } else if is_char(key, "c") {
            if let Some(issue_key) = self.require_fresh_issue(true) {
                self.open_detail(issue_key.clone(), cmds);
                self.flow = Some(Flow::Compose(Compose::new(issue_key)));
            }
        } else if is_char(key, "w") || is_char(key, "d") {
            if let Some(issue_key) = self.require_fresh_issue(true) {
                self.start_wizard(issue_key, is_char(key, "d"), cmds);
            }
        } else if is_char(key, "s")
            && let Some(issue_key) = self.require_fresh_issue(true)
        {
            self.start_transition(issue_key, cmds);
        }
    }

    fn detail_key(&mut self, key: &Key, cmds: &mut Vec<Cmd>) {
        let Some(detail) = self.detail.as_mut() else {
            self.base = Base::List;
            return;
        };
        if key.is("escape") || is_char(key, "q") {
            self.close_detail(cmds);
            return;
        }
        let pane = match detail.focus {
            Pane::Description => view::DESCRIPTION_ID,
            Pane::Comments => view::COMMENTS_ID,
        };
        if key.is("tab") || key.is("shift+tab") {
            detail.focus = match detail.focus {
                Pane::Description => Pane::Comments,
                Pane::Comments => Pane::Description,
            };
            let target = match detail.focus {
                Pane::Description => view::DESCRIPTION_ID,
                Pane::Comments => view::COMMENTS_ID,
            };
            cmds.push(Cmd::Reveal(target.to_owned(), RevealAt::Start));
            return;
        }
        let scroll = scroll_key(key);
        if let Some(by) = scroll {
            cmds.push(Cmd::Scroll(pane.to_owned(), by));
            return;
        }
        if key.is("home") || is_char(key, "g") {
            cmds.push(Cmd::Reveal(pane.to_owned(), RevealAt::Start));
        } else if is_char(key, "r") {
            self.refresh_detail(cmds);
        } else if is_char(key, "c") {
            let issue_key = detail.key.clone();
            self.flow = Some(Flow::Compose(Compose::new(issue_key)));
        } else if is_char(key, "w") || is_char(key, "d") {
            if let Some(issue_key) = self.require_fresh_issue(false) {
                self.start_wizard(issue_key, is_char(key, "d"), cmds);
            }
        } else if is_char(key, "s")
            && let Some(issue_key) = self.require_fresh_issue(false)
        {
            self.start_transition(issue_key, cmds);
        }
    }

    fn flow_key(&mut self, flow: Flow, key: &Key, cmds: &mut Vec<Cmd>) -> Option<Flow> {
        match flow {
            Flow::Prompt { kind, mut buf } => {
                if key.is("escape") {
                    return None;
                }
                if key.is("enter") {
                    let text = buf.text.trim().to_owned();
                    if text.is_empty() {
                        return None;
                    }
                    let query = match kind {
                        PromptKind::Search => QueryRef {
                            label: format!("Search: {text}"),
                            jql: build_search_jql(&self.config.search_jql, &text),
                        },
                        PromptKind::Jql => QueryRef {
                            label: "JQL".into(),
                            jql: buf.text.clone(),
                        },
                    };
                    self.load_query(query, cmds);
                    return None;
                }
                buf.edit(key, false);
                Some(Flow::Prompt { kind, buf })
            }
            Flow::Filter { mut selected } => {
                let choices = filter_choices(&self.config);
                if key.is("escape") || is_char(key, "q") {
                    return None;
                }
                if key.is("enter") {
                    let query = choices[step_index(selected, 0, choices.len())].clone();
                    self.load_query(query, cmds);
                    return None;
                }
                nav(key, &mut selected, choices.len());
                Some(Flow::Filter { selected })
            }
            Flow::Compose(c) => self.compose_key(c, key, cmds).map(Flow::Compose),
            Flow::Transition(t) => self.transition_key(t, key, cmds).map(Flow::Transition),
            Flow::Wizard(w) => self
                .wizard_key(*w, key, cmds)
                .map(|w| Flow::Wizard(Box::new(w))),
        }
    }

    // -- comment -------------------------------------------------------------

    fn compose_key(&mut self, mut c: Compose, key: &Key, cmds: &mut Vec<Cmd>) -> Option<Compose> {
        match c.step {
            ComposeStep::Editing => {
                if key.is("escape") {
                    if c.draft.text.trim().is_empty() {
                        return None;
                    }
                    c.step = ComposeStep::Discard;
                } else if is_submit(key) {
                    c.request_send();
                } else {
                    c.draft.edit(key, true);
                }
            }
            ComposeStep::Discard => {
                if is_confirm(key) {
                    return None;
                }
                if is_back(key) {
                    c.step = ComposeStep::Editing;
                }
            }
            ComposeStep::Recheck => {
                if is_confirm(key) {
                    c.step = ComposeStep::Confirm;
                } else if is_back(key) {
                    c.step = ComposeStep::Editing;
                }
            }
            ComposeStep::Confirm => {
                if key.is("enter") {
                    match validate_comment(&c.draft.text) {
                        Ok(body) => {
                            let body = body.to_owned();
                            let generation = self.next_gen();
                            c.step = ComposeStep::Posting { generation };
                            c.message = None;
                            cmds.push(Cmd::AddComment {
                                generation,
                                key: c.key.clone(),
                                body,
                            });
                        }
                        Err(message) => {
                            c.step = ComposeStep::Editing;
                            c.message = Some(Notice::new(Level::Warning, message));
                        }
                    }
                } else if is_back(key) {
                    c.step = ComposeStep::Editing;
                }
            }
            ComposeStep::Posting { .. } => {
                if key.is("escape") {
                    c.message = Some(Notice::new(
                        Level::Info,
                        "The comment request was sent; it cannot be cancelled. Wait for the result.",
                    ));
                }
            }
        }
        Some(c)
    }

    fn compose_done(
        &mut self,
        mut c: Compose,
        result: CommentResult,
        cmds: &mut Vec<Cmd>,
    ) -> Option<Compose> {
        match result {
            CommentResult::Created { .. } => {
                self.notice = Some(Notice::new(
                    Level::Success,
                    format!("{}: comment posted.", c.key),
                ));
                let key = c.key.clone();
                self.refresh_comments(&key, cmds);
                None
            }
            CommentResult::Rejected { message } => {
                c.step = ComposeStep::Editing;
                c.message = Some(Notice::new(
                    Level::Error,
                    format!(
                        "Jira rejected the comment: {}. Your text is kept.",
                        clip(&message)
                    ),
                ));
                Some(c)
            }
            CommentResult::Unknown { message } => {
                c.step = ComposeStep::Editing;
                c.uncertain = true;
                c.message = Some(Notice::new(
                    Level::Warning,
                    format!(
                        "Comment outcome UNKNOWN ({}). It may have been saved — check the issue in Jira before posting again. Your text is kept.",
                        clip(&message)
                    ),
                ));
                Some(c)
            }
        }
    }

    // -- transitions ---------------------------------------------------------

    fn start_transition(&mut self, key: String, cmds: &mut Vec<Cmd>) {
        let generation = self.next_gen();
        cmds.push(Cmd::LoadTransitions {
            generation,
            key: key.clone(),
        });
        self.flow = Some(Flow::Transition(TransitionFlow {
            key,
            step: TStep::Loading { generation },
            message: None,
        }));
    }

    fn close_transition(&mut self, cmds: &mut Vec<Cmd>) -> Option<TransitionFlow> {
        self.refresh_after_transition(cmds);
        None
    }

    fn transition_key(
        &mut self,
        t: TransitionFlow,
        key: &Key,
        cmds: &mut Vec<Cmd>,
    ) -> Option<TransitionFlow> {
        let TransitionFlow {
            key: issue_key,
            step,
            ..
        } = t;
        let mut message = None;
        let step = match step {
            TStep::Loading { .. } if key.is("escape") => return self.close_transition(cmds),
            TStep::Blocked {
                since,
                attempt,
                url,
            } => {
                if is_confirm(key) {
                    let generation = self.next_gen();
                    cmds.push(Cmd::AckTransition {
                        generation,
                        key: issue_key.clone(),
                        attempt,
                    });
                    TStep::Acknowledging { generation }
                } else if is_back(key) {
                    self.notice = Some(Notice::new(
                        Level::Warning,
                        format!(
                            "{issue_key}: 이전 전환 결과가 불확실하여 새 전환 요청을 보내지 않았습니다."
                        ),
                    ));
                    return self.close_transition(cmds);
                } else {
                    TStep::Blocked {
                        since,
                        attempt,
                        url,
                    }
                }
            }
            TStep::Pick {
                choices,
                mut selected,
            } => {
                if key.is("escape") || is_char(key, "q") {
                    return self.close_transition(cmds);
                }
                if key.is("enter") {
                    let index = step_index(selected, 0, choices.transitions.len());
                    match choices
                        .transitions
                        .get(index)
                        .map(TransitionService::eligibility)
                    {
                        Some(Ok(())) => TStep::Confirm { choices, index },
                        Some(Err(reason)) => {
                            message = Some(Notice::new(
                                Level::Warning,
                                format!(
                                    "{} {}",
                                    single_line(&reason),
                                    single_line(&choices.issue.url)
                                ),
                            ));
                            TStep::Pick { choices, selected }
                        }
                        None => TStep::Pick { choices, selected },
                    }
                } else {
                    nav(key, &mut selected, choices.transitions.len());
                    TStep::Pick { choices, selected }
                }
            }
            TStep::Confirm { choices, index } => {
                if key.is("enter") {
                    let generation = self.next_gen();
                    let transition = choices.transitions[index].clone();
                    cmds.push(Cmd::ApplyTransition {
                        generation,
                        key: issue_key.clone(),
                        from: choices.issue.status.clone(),
                        transition,
                    });
                    TStep::Applying {
                        generation,
                        url: choices.issue.url.clone(),
                    }
                } else if is_back(key) {
                    TStep::Pick {
                        choices,
                        selected: index,
                    }
                } else {
                    TStep::Confirm { choices, index }
                }
            }
            TStep::Finished {
                title,
                lines,
                level,
            } => {
                if key.is("enter") || key.is("escape") || is_char(key, "q") {
                    return self.close_transition(cmds);
                }
                TStep::Finished {
                    title,
                    lines,
                    level,
                }
            }
            step @ (TStep::Applying { .. } | TStep::Acknowledging { .. }) => {
                if key.is("escape") {
                    message = Some(Notice::new(
                        Level::Info,
                        "The request was sent; it cannot be cancelled. Wait for the result.",
                    ));
                }
                step
            }
            step @ TStep::Loading { .. } => step,
        };
        Some(TransitionFlow {
            key: issue_key,
            step,
            message,
        })
    }

    fn transition_done(
        &mut self,
        t: TransitionFlow,
        done: Done,
        cmds: &mut Vec<Cmd>,
    ) -> Option<TransitionFlow> {
        let TransitionFlow { key, step, message } = t;
        let finished = |title: &str, line: String, level: Level| TStep::Finished {
            title: title.to_owned(),
            lines: vec![line],
            level,
        };
        let (step, message) = match (step, done) {
            (TStep::Loading { generation }, Done::TransitionsLoaded { generation: got, result }) if generation == got => match result {
                Ok(choices) => match choices.blocked.clone() {
                    Some(blocked) => (
                        TStep::Blocked {
                            since: blocked.started_at,
                            attempt: blocked.attempt,
                            url: choices.issue.url.clone(),
                        },
                        message,
                    ),
                    None if choices.transitions.is_empty() => (
                        finished(
                            "No transitions",
                            format!("{key}: 현재 상태({})에서 가능한 전환이 없습니다.", status_text(&choices.issue.status)),
                            Level::Info,
                        ),
                        message,
                    ),
                    None => (TStep::Pick { choices, selected: 0 }, message),
                },
                Err(error) => (
                    finished("Status change failed", format!("{key}: 상태 전환을 진행하지 못했습니다 — {}", clip(&error.to_string())), Level::Error),
                    None,
                ),
            },
            (TStep::Acknowledging { generation }, Done::TransitionAcknowledged { generation: got, result }) if generation == got => {
                match result {
                    Ok(()) => {
                        let generation = self.next_gen();
                        cmds.push(Cmd::LoadTransitions { generation, key: key.clone() });
                        (TStep::Loading { generation }, None)
                    }
                    Err(TransitionError::Blocked { .. }) => {
                        let generation = self.next_gen();
                        cmds.push(Cmd::LoadTransitions { generation, key: key.clone() });
                        let text = format!(
                            "{key}: 확인하던 전환 요청과 다른 새 요청이 기록되어 승인하지 않았습니다. 이력을 다시 확인하세요."
                        );
                        (TStep::Loading { generation }, Some(Notice::new(Level::Warning, text)))
                    }
                    Err(error) => (
                        finished("Status change failed", format!("{key}: {}", clip(&error.to_string())), Level::Error),
                        None,
                    ),
                }
            }
            (TStep::Applying { generation, url }, Done::TransitionApplied { generation: got, result }) if generation == got => {
                match result {
                    Ok(TransitionResult::Acknowledged { observed_status, verified }) => {
                        let line = match (&observed_status, verified) {
                            (Some(observed), true) => format!("{key}: 전환 완료 확인 — 현재 상태 {}", status_text(observed)),
                            (observed, _) => format!(
                                "{key}: 서버가 전환 요청 수락, 현재 상태 확인 불가/다름 (현재 {}). 재전송하지 않습니다.",
                                observed.as_ref().map_or("조회 실패".to_owned(), status_text)
                            ),
                        };
                        let level = if verified { Level::Success } else { Level::Warning };
                        (finished("Transition acknowledged", line, level), None)
                    }
                    Ok(TransitionResult::Rejected { message }) => (
                        finished("Transition rejected", format!("{key}: Jira가 전환을 거부했습니다 — {}", clip(&message)), Level::Error),
                        None,
                    ),
                    Ok(TransitionResult::Unknown { message, observed_status }) => (
                        TStep::Finished {
                            title: "Transition outcome UNKNOWN".into(),
                            lines: vec![
                                format!(
                                    "{key}: 전환 결과 불확실 ({}). 현재 상태 관측: {} — 요청 성공/미실행의 증거가 아닙니다. 자동 재시도하지 않습니다.",
                                    clip(&message),
                                    observed_status.as_ref().map_or("조회 실패".to_owned(), status_text)
                                ),
                                format!("Jira 이력을 확인하세요: {}", single_line(&url)),
                                "The next transition for this issue is blocked until you confirm you checked the Jira history.".into(),
                            ],
                            level: Level::Warning,
                        },
                        None,
                    ),
                    Err(TransitionError::Blocked { attempt, record_started_at }) => {
                        (TStep::Blocked { since: record_started_at, attempt, url }, None)
                    }
                    Err(TransitionError::Changed(detail)) => {
                        let generation = self.next_gen();
                        cmds.push(Cmd::LoadTransitions { generation, key: key.clone() });
                        let text = format!(
                            "{key}: 확인 이후 상태나 전환 목록이 바뀌어 승인을 폐기했습니다. 다시 선택하세요. ({})",
                            clip(&detail)
                        );
                        (TStep::Loading { generation }, Some(Notice::new(Level::Warning, text)))
                    }
                    Err(error) => (
                        finished("Status change failed", format!("{key}: {}", clip(&error.to_string())), Level::Error),
                        None,
                    ),
                }
            }
            (step, _) => (step, message),
        };
        Some(TransitionFlow { key, step, message })
    }

    // -- checkout wizard -----------------------------------------------------

    fn repo_candidates(&self, issue_key: &str) -> Vec<RepoCandidate> {
        let project = issue_key.rfind('-').map_or(issue_key, |i| &issue_key[..i]);
        let repos = &self.config.worktree.repos;
        let mut out = vec![RepoCandidate {
            path: self.cwd.clone(),
            tags: vec!["current directory".into()],
            mapped: false,
        }];
        if let Some((_, path)) = repos.iter().find(|(p, _)| p == project) {
            out.push(RepoCandidate {
                path: path.clone(),
                tags: vec![format!("mapped to {project}")],
                mapped: true,
            });
        }
        for (p, path) in repos.iter().filter(|(p, _)| p != project) {
            out.push(RepoCandidate {
                path: path.clone(),
                tags: vec![format!("configured for {p}")],
                mapped: false,
            });
        }
        out
    }

    fn start_wizard(&mut self, key: String, omp: bool, cmds: &mut Vec<Cmd>) {
        let generation = self.next_gen();
        cmds.push(Cmd::ResolveRepos {
            generation,
            candidates: self.repo_candidates(&key),
        });
        self.flow = Some(Flow::Wizard(Box::new(Wizard {
            key,
            step: WStep::Resolving { generation },
            choices: Vec::new(),
            repository: None,
            existing: Vec::new(),
            existing_prefer: None,
            plan: None,
            action: if omp {
                CheckoutAction::Split
            } else {
                CheckoutAction::Checkout
            },
            snapshot: None,
            instruction: TextBuf::new(DEFAULT_INSTRUCTION),
            message: None,
        })));
    }

    fn listing(&mut self, w: &mut Wizard, prefer: Option<String>, cmds: &mut Vec<Cmd>) -> WStep {
        let Some(repository) = w.repository.clone() else {
            return WStep::Repo {
                selected: initial_repo(&w.choices),
            };
        };
        let generation = self.next_gen();
        w.existing_prefer = prefer;
        cmds.push(Cmd::ListCheckouts {
            generation,
            repository,
        });
        WStep::Listing { generation }
    }

    fn planning(&mut self, w: &Wizard, selection: CheckoutSelection, cmds: &mut Vec<Cmd>) -> WStep {
        let Some(repository) = w.repository.clone() else {
            return WStep::Repo {
                selected: initial_repo(&w.choices),
            };
        };
        let generation = self.next_gen();
        cmds.push(Cmd::Plan {
            generation,
            repository,
            selection: selection.clone(),
        });
        WStep::Planning {
            generation,
            selection,
        }
    }

    fn existing_step(w: &Wizard) -> WStep {
        let items = existing_items(w);
        let preferred =
            w.existing.iter().zip(&items).position(|(c, item)| {
                Some(&c.name) == w.existing_prefer.as_ref() && !item.disabled
            });
        let selected = preferred
            .or_else(|| items.iter().position(|i| !i.disabled))
            .unwrap_or(0);
        WStep::Existing { selected }
    }

    fn wizard_key(&mut self, mut w: Wizard, key: &Key, cmds: &mut Vec<Cmd>) -> Option<Wizard> {
        w.message = None;
        let step = std::mem::replace(&mut w.step, WStep::Preview);
        w.step = match step {
            WStep::Resolving { .. } if key.is("escape") => return None,
            step @ WStep::Resolving { .. } => step,
            WStep::Repo { mut selected } => {
                let len = w.choices.len() + 1;
                if key.is("escape") || is_char(key, "q") {
                    return None;
                }
                if key.is("enter") {
                    let index = step_index(selected, 0, len);
                    match w.choices.get(index).map(|c| c.result.clone()) {
                        None => WStep::RepoPath {
                            buf: TextBuf::new(self.cwd.clone()),
                        },
                        Some(Ok(repository)) => {
                            w.repository = Some(repository);
                            WStep::Kind { selected: 0 }
                        }
                        Some(Err(error)) => {
                            w.message =
                                Some(Notice::new(Level::Warning, format!("Unavailable: {error}")));
                            WStep::Repo { selected }
                        }
                    }
                } else {
                    nav(key, &mut selected, len);
                    WStep::Repo { selected }
                }
            }
            WStep::RepoPath { mut buf } => {
                if key.is("escape") {
                    WStep::Repo {
                        selected: initial_repo(&w.choices),
                    }
                } else if key.is("enter") {
                    if buf.text.trim().is_empty() {
                        WStep::Repo {
                            selected: initial_repo(&w.choices),
                        }
                    } else {
                        let path = expand_path(&buf.text, &self.cwd);
                        let generation = self.next_gen();
                        cmds.push(Cmd::ResolvePath {
                            generation,
                            path: path.clone(),
                        });
                        WStep::ResolvingPath { generation, path }
                    }
                } else {
                    buf.edit(key, false);
                    WStep::RepoPath { buf }
                }
            }
            WStep::ResolvingPath { .. } if key.is("escape") => WStep::Repo {
                selected: initial_repo(&w.choices),
            },
            step @ WStep::ResolvingPath { .. } => step,
            WStep::Kind { mut selected } => {
                if key.is("escape") {
                    WStep::Repo {
                        selected: initial_repo(&w.choices),
                    }
                } else if key.is("enter") {
                    if selected == 0 {
                        WStep::NewName {
                            buf: TextBuf::new(w.key.clone()),
                        }
                    } else {
                        self.listing(&mut w, None, cmds)
                    }
                } else {
                    nav(key, &mut selected, 2);
                    WStep::Kind { selected }
                }
            }
            WStep::NewName { mut buf } => {
                if key.is("escape") {
                    WStep::Kind { selected: 0 }
                } else if key.is("enter") {
                    let name = buf.text.trim();
                    let name = if name.is_empty() {
                        w.key.clone()
                    } else {
                        name.to_owned()
                    };
                    self.planning(&w, CheckoutSelection::New { name, base: None }, cmds)
                } else {
                    buf.edit(key, false);
                    WStep::NewName { buf }
                }
            }
            WStep::Planning { selection, .. } if key.is("escape") => match selection {
                CheckoutSelection::New { .. } => WStep::Kind { selected: 0 },
                CheckoutSelection::Existing { .. } => Self::existing_step(&w),
            },
            step @ WStep::Planning { .. } => step,
            WStep::Listing { .. } if key.is("escape") => WStep::Kind { selected: 1 },
            step @ WStep::Listing { .. } => step,
            WStep::Existing { mut selected } => {
                let len = w.existing.len();
                if key.is("escape") {
                    WStep::Kind { selected: 1 }
                } else if key.is("enter") {
                    let index = step_index(selected, 0, len);
                    match w.existing.get(index).cloned() {
                        Some(checkout) if checkout_disabled(&checkout) => {
                            let detail = existing_items(&w).swap_remove(index).detail;
                            w.message = Some(Notice::new(Level::Warning, detail));
                            WStep::Existing { selected }
                        }
                        Some(checkout) if checkout.path.is_none() => WStep::ExistingPath {
                            checkout,
                            buf: TextBuf::default(),
                        },
                        Some(checkout) => self.planning(
                            &w,
                            CheckoutSelection::Existing {
                                checkout,
                                recovery_acknowledged: false,
                            },
                            cmds,
                        ),
                        None => WStep::Existing { selected },
                    }
                } else {
                    nav(key, &mut selected, len);
                    WStep::Existing { selected }
                }
            }
            WStep::ExistingPath {
                mut checkout,
                mut buf,
            } => {
                if key.is("escape") {
                    Self::existing_step(&w)
                } else if key.is("enter") {
                    if buf.text.trim().is_empty() {
                        Self::existing_step(&w)
                    } else {
                        // The missing path was the only unavailability; planning re-validates it.
                        checkout.path = Some(expand_path(&buf.text, &self.cwd));
                        checkout.unavailable_reason = None;
                        self.planning(
                            &w,
                            CheckoutSelection::Existing {
                                checkout,
                                recovery_acknowledged: false,
                            },
                            cmds,
                        )
                    }
                } else {
                    buf.edit(key, false);
                    WStep::ExistingPath { checkout, buf }
                }
            }
            WStep::RecoveryAck {
                checkout,
                message,
                path,
            } => {
                if key.is("enter") {
                    self.planning(
                        &w,
                        CheckoutSelection::Existing {
                            checkout,
                            recovery_acknowledged: true,
                        },
                        cmds,
                    )
                } else if is_back(key) {
                    Self::existing_step(&w)
                } else {
                    scroll_wizard_text(key, cmds);
                    WStep::RecoveryAck {
                        checkout,
                        message,
                        path,
                    }
                }
            }
            WStep::Action { mut selected } => {
                if key.is("escape") {
                    WStep::Kind { selected: 0 }
                } else if key.is("enter") {
                    w.action = ACTIONS[step_index(selected, 0, ACTIONS.len())];
                    // Leaving the handoff step backwards discards the snapshot; every pass fetches it fresh.
                    w.snapshot = None;
                    if w.action == CheckoutAction::Checkout {
                        WStep::Confirm
                    } else {
                        let generation = self.next_gen();
                        cmds.push(Cmd::Snapshot {
                            generation,
                            key: w.key.clone(),
                        });
                        WStep::Snapshotting { generation }
                    }
                } else {
                    nav(key, &mut selected, ACTIONS.len());
                    WStep::Action { selected }
                }
            }
            WStep::Snapshotting { .. } if key.is("escape") => WStep::Action {
                selected: w.action.index(),
            },
            step @ WStep::Snapshotting { .. } => step,
            WStep::Preview => {
                if key.is("escape") {
                    w.snapshot = None;
                    WStep::Action {
                        selected: w.action.index(),
                    }
                } else if key.is("enter") {
                    let partial = w
                        .snapshot
                        .as_ref()
                        .is_some_and(|s| !s.partial_reasons.is_empty());
                    if partial {
                        WStep::PartialAccept
                    } else {
                        WStep::Instruction
                    }
                } else {
                    scroll_wizard_text(key, cmds);
                    WStep::Preview
                }
            }
            WStep::PartialAccept => {
                if is_confirm(key) {
                    WStep::Instruction
                } else if is_back(key) {
                    WStep::Preview
                } else {
                    scroll_wizard_text(key, cmds);
                    WStep::PartialAccept
                }
            }
            WStep::Instruction => {
                if key.is("escape") {
                    WStep::Preview
                } else if is_submit(key) {
                    instruction_step(&mut w)
                } else {
                    w.instruction.edit(key, true);
                    WStep::Instruction
                }
            }
            WStep::Confirm => {
                if key.is("escape") {
                    if w.action == CheckoutAction::Checkout {
                        WStep::Action {
                            selected: w.action.index(),
                        }
                    } else {
                        WStep::Instruction
                    }
                } else if key.is("enter") {
                    match self.execute(&w, cmds) {
                        Some(step) => step,
                        None => WStep::Confirm,
                    }
                } else {
                    scroll_wizard_text(key, cmds);
                    WStep::Confirm
                }
            }
            step @ WStep::Executing { .. } => {
                if key.is("escape") {
                    w.message = Some(Notice::new(
                        Level::Info,
                        "Changes are being applied; this cannot be cancelled. Wait for the result.",
                    ));
                }
                step
            }
            WStep::Outcome(outcome) => {
                if key.is("enter") || key.is("escape") || is_char(key, "q") {
                    return None;
                }
                if is_char(key, "r")
                    && let Some(ctx) = outcome.retry.clone()
                {
                    let generation = self.next_gen();
                    cmds.push(Cmd::Launch { generation, ctx });
                    WStep::Executing { generation }
                } else if is_char(key, "l") && outcome.relaunch.is_some() {
                    WStep::RelaunchConfirm(outcome)
                } else {
                    scroll_wizard_text(key, cmds);
                    WStep::Outcome(outcome)
                }
            }
            WStep::RelaunchConfirm(outcome) => {
                if key.is("enter") {
                    match outcome.relaunch.clone() {
                        Some(ctx) => {
                            let generation = self.next_gen();
                            cmds.push(Cmd::Launch { generation, ctx });
                            WStep::Executing { generation }
                        }
                        None => WStep::Outcome(outcome),
                    }
                } else if is_back(key) {
                    WStep::Outcome(outcome)
                } else {
                    scroll_wizard_text(key, cmds);
                    WStep::RelaunchConfirm(outcome)
                }
            }
            WStep::Message { title, lines, next } => {
                if key.is("enter") || key.is("escape") {
                    match next {
                        Next::Step(step) => *step,
                        Next::ListExisting(prefer) => self.listing(&mut w, prefer, cmds),
                    }
                } else {
                    scroll_wizard_text(key, cmds);
                    WStep::Message { title, lines, next }
                }
            }
        };
        Some(w)
    }

    /// The final confirmation: the only place the wizard mutates anything.
    fn execute(&mut self, w: &Wizard, cmds: &mut Vec<Cmd>) -> Option<WStep> {
        let plan = w.plan.clone()?;
        let handoff = match w.action.placement() {
            None => None,
            Some(placement) => Some(HandoffRequest {
                snapshot: w.snapshot.clone()?,
                instruction: w.instruction.text.clone(),
                placement,
            }),
        };
        let generation = self.next_gen();
        cmds.push(Cmd::Execute {
            generation,
            plan,
            handoff,
        });
        Some(WStep::Executing { generation })
    }

    fn wizard_done(&mut self, mut w: Wizard, done: Done) -> Wizard {
        let step = std::mem::replace(&mut w.step, WStep::Preview);
        w.step = match (step, done) {
            (
                WStep::Resolving { generation },
                Done::Repos {
                    generation: got,
                    choices,
                },
            ) if generation == got => {
                w.choices = merge_choices(choices);
                WStep::Repo {
                    selected: initial_repo(&w.choices),
                }
            }
            (
                WStep::ResolvingPath { generation, .. },
                Done::RepoPath {
                    generation: got,
                    path,
                    result,
                },
            ) if generation == got => match result {
                Ok(repository) => {
                    w.repository = Some(repository);
                    WStep::Kind { selected: 0 }
                }
                Err(error) => WStep::Message {
                    title: "Repository unavailable".into(),
                    lines: vec![path, String::new(), error],
                    next: Next::Step(Box::new(WStep::Repo {
                        selected: initial_repo(&w.choices),
                    })),
                },
            },
            (
                WStep::Listing { generation },
                Done::Checkouts {
                    generation: got,
                    result,
                },
            ) if generation == got => match result {
                Ok(list) if list.is_empty() => {
                    let root = w
                        .repository
                        .as_ref()
                        .map(|r| r.root.clone())
                        .unwrap_or_default();
                    WStep::Message {
                        title: "No checkouts".into(),
                        lines: vec![format!("{root} has no listed {}s.", w.backend_noun())],
                        next: Next::Step(Box::new(WStep::Kind { selected: 1 })),
                    }
                }
                Ok(list) => {
                    w.existing = list;
                    Self::existing_step(&w)
                }
                Err(error) => WStep::Message {
                    title: "Cannot list checkouts".into(),
                    lines: vec![error],
                    next: Next::Step(Box::new(WStep::Kind { selected: 1 })),
                },
            },
            (
                WStep::Planning {
                    generation,
                    selection,
                },
                Done::Planned {
                    generation: got,
                    result,
                },
            ) if generation == got => self.plan_result(&mut w, selection, result),
            (
                WStep::Snapshotting { generation },
                Done::Snapshot {
                    generation: got,
                    result,
                },
            ) if generation == got => match result {
                Ok(snapshot) => {
                    w.snapshot = Some(snapshot);
                    WStep::Preview
                }
                Err(error) => WStep::Message {
                    title: "Snapshot unavailable".into(),
                    lines: vec![
                        err_text(&error),
                        String::new(),
                        "Handoff needs the issue itself; nothing was created.".into(),
                    ],
                    next: Next::Step(Box::new(WStep::Action {
                        selected: w.action.index(),
                    })),
                },
            },
            (
                WStep::Executing { generation },
                Done::Executed {
                    generation: got,
                    outcome,
                },
            ) if generation == got => self.exec_result(&mut w, outcome),
            (step, _) => step,
        };
        w
    }

    fn plan_result(
        &mut self,
        w: &mut Wizard,
        selection: CheckoutSelection,
        result: Result<CheckoutPlan, CheckoutError>,
    ) -> WStep {
        let error = match result {
            Ok(plan) => {
                w.plan = Some(plan);
                return WStep::Action {
                    selected: w.action.index(),
                };
            }
            Err(error) => error,
        };
        let text = err_text(&error);
        let is_new = matches!(selection, CheckoutSelection::New { .. });
        match error {
            CheckoutError::RecoveryRequired { name, path, existing, .. } if is_new || !existing => WStep::Message {
                title: "Recovery required".into(),
                lines: vec![
                    text,
                    String::new(),
                    format!("Path: {path}"),
                    "A previous creation attempt did not finish; it will not be retried automatically.".into(),
                    "Inspect that path manually. To continue, choose it under Existing and acknowledge the recovery warning."
                        .into(),
                ],
                next: Next::ListExisting(Some(name)),
            },
            CheckoutError::RecoveryRequired { path, .. } => match selection {
                CheckoutSelection::Existing { checkout, recovery_acknowledged: false } => {
                    WStep::RecoveryAck { checkout, message: text, path }
                }
                _ => WStep::Message {
                    title: "Cannot plan this checkout".into(),
                    lines: vec![text],
                    next: Next::Step(Box::new(Self::existing_step(w))),
                },
            },
            CheckoutError::Failed(_) => WStep::Message {
                title: "Cannot plan this checkout".into(),
                lines: vec![text],
                next: Next::Step(Box::new(if is_new { WStep::Kind { selected: 0 } } else { Self::existing_step(w) })),
            },
        }
    }

    fn exec_result(&mut self, w: &mut Wizard, outcome: ExecOutcome) -> WStep {
        match outcome {
            ExecOutcome::CheckoutFailed(mut lines) => {
                let destination = w
                    .plan
                    .as_ref()
                    .map(|p| p.destination().to_owned())
                    .unwrap_or_default();
                w.plan = None;
                w.snapshot = None;
                lines.extend([
                    String::new(),
                    format!("Destination: {destination}"),
                    "Choose the checkout again.".into(),
                ]);
                WStep::Message {
                    title: "Checkout failed".into(),
                    lines,
                    next: Next::Step(Box::new(WStep::Kind { selected: 0 })),
                }
            }
            ExecOutcome::Ready(prepared) => WStep::Outcome(Outcome {
                title: if prepared.created {
                    "Checkout created".into()
                } else {
                    "Checkout ready".into()
                },
                lines: vec![
                    format!("Path: {}", prepared.path),
                    format!(
                        "Repository: {} [{}]",
                        prepared.repository.name,
                        prepared.repository.backend.label()
                    ),
                    format!("Name: {}", prepared.checkout.name),
                ],
                level: Level::Success,
                retry: None,
                relaunch: None,
            }),
            ExecOutcome::HandoffFailed { prepared, message } => WStep::Outcome(Outcome {
                title: "Handoff file not written".into(),
                lines: vec![
                    message,
                    String::new(),
                    format!(
                        "The checkout is ready at {}; omp was not started.",
                        prepared.path
                    ),
                ],
                level: Level::Error,
                retry: None,
                relaunch: None,
            }),
            ExecOutcome::Launched { ctx, result } => WStep::Outcome(launch_outcome(ctx, result)),
        }
    }

    // -- job results ---------------------------------------------------------

    pub(crate) fn on_done(&mut self, done: Done) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        match done {
            Done::Search { generation, result } => self
                .list
                .apply(generation, result.map_err(|e| err_text(&e))),
            Done::Issue { generation, result } => {
                if let Some(detail) = self.detail.as_mut() {
                    detail
                        .issue
                        .finish(generation, result.map_err(|e| err_text(&e)), |_| false);
                }
            }
            Done::Comments { generation, result } => {
                if let Some(comments) = self.detail.as_mut().and_then(|d| d.comments.as_mut()) {
                    comments.finish(generation, result.map_err(|e| err_text(&e)), |p| {
                        p.items.is_empty()
                    });
                }
            }
            done => {
                if let Some(flow) = self.flow.take() {
                    self.flow = self.flow_done(flow, done, &mut cmds);
                }
            }
        }
        cmds
    }

    fn flow_done(&mut self, flow: Flow, done: Done, cmds: &mut Vec<Cmd>) -> Option<Flow> {
        match (flow, done) {
            (Flow::Compose(c), Done::Comment { generation, result })
                if c.step == ComposeStep::Posting { generation } =>
            {
                self.compose_done(c, result, cmds).map(Flow::Compose)
            }
            (Flow::Transition(t), done) => {
                self.transition_done(t, done, cmds).map(Flow::Transition)
            }
            (Flow::Wizard(w), done) => Some(Flow::Wizard(Box::new(self.wizard_done(*w, done)))),
            (flow, _) => Some(flow),
        }
    }
}

/// Confirm when the instruction is non-empty; otherwise stay with a warning.
fn instruction_step(w: &mut Wizard) -> WStep {
    if w.instruction.text.trim().is_empty() {
        w.message = Some(Notice::new(
            Level::Warning,
            "The task instruction cannot be empty.",
        ));
        WStep::Instruction
    } else {
        w.message = None;
        WStep::Confirm
    }
}

fn launch_result_lines(result: &LaunchResult, ctx: &LaunchCtx) -> Vec<String> {
    let mut lines = vec![
        format!("Checkout:     {}", ctx.prepared.path),
        format!("Handoff file: {}", ctx.snapshot_path.display()),
        String::new(),
    ];
    match result {
        LaunchResult::Requested { block, warning } => {
            lines.push("터미널 실행 요청됨 (terminal launch requested).".into());
            if let Some(block) = block {
                lines.push(format!("Block id: {block}"));
            }
            if let Some(warning) = warning {
                lines.push(format!("Warning: {warning}"));
            }
            lines.push("This only confirms Tern accepted the request; it does not prove omp or the model is ready.".into());
        }
        LaunchResult::Manual { argv, cwd, reason }
        | LaunchResult::Unknown { argv, cwd, reason } => {
            lines.push(if matches!(result, LaunchResult::Manual { .. }) {
                "Automatic launch unavailable — run this yourself:".into()
            } else {
                "Launch result UNKNOWN — a tab/split may or may not have been created. Inspect your terminal before retrying."
                    .into()
            });
            lines.push(format!("Reason: {reason}"));
            lines.push(String::new());
            lines.push("Command (POSIX shell, display only):".into());
            lines.push(format!(
                "cd {} && {}",
                launch::posix_quote(std::slice::from_ref(cwd)),
                launch::posix_quote(argv)
            ));
            lines.push(String::new());
            lines.push(format!("cwd:  {cwd}"));
            lines.push(format!(
                "argv: {}",
                serde_json::to_string(argv).unwrap_or_default()
            ));
        }
    }
    lines
}

fn launch_outcome(ctx: LaunchCtx, result: Result<LaunchResult, Vec<String>>) -> Outcome {
    match result {
        Err(mut problem) => {
            problem.extend([
                String::new(),
                format!("Checkout:     {}", ctx.prepared.path),
                format!("Handoff file: {}", ctx.snapshot_path.display()),
            ]);
            Outcome {
                title: "Checkout re-validation failed — omp not started".into(),
                lines: problem,
                level: Level::Error,
                retry: Some(ctx),
                relaunch: None,
            }
        }
        Ok(result) => {
            let mut lines = launch_result_lines(&result, &ctx);
            lines.push(String::new());
            lines.push("The checkout is kept regardless of the launch outcome.".into());
            let (title, level, relaunch) = match result {
                LaunchResult::Requested { .. } => ("Launch requested", Level::Success, None),
                LaunchResult::Manual { .. } => ("Launch manual", Level::Warning, None),
                LaunchResult::Unknown { .. } => ("Launch unknown", Level::Warning, Some(ctx)),
            };
            Outcome {
                title: title.into(),
                lines,
                level,
                retry: None,
                relaunch,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Background jobs

#[derive(Clone)]
struct Services {
    config: Arc<Config>,
    jira: Arc<dyn JiraApi>,
    checkouts: Arc<CheckoutService>,
    transitions: Arc<TransitionService>,
    store: StateStore,
}

struct Runner {
    services: Services,
    tx: mpsc::Sender<Done>,
    waker: Option<Waker>,
}

impl Runner {
    fn spawn(&self, job: impl FnOnce(&Services) -> Done + Send + 'static) {
        let services = self.services.clone();
        let tx = self.tx.clone();
        let waker = self.waker.clone();
        std::thread::spawn(move || {
            let done = job(&services);
            if tx.send(done).is_ok()
                && let Some(waker) = waker
            {
                waker.wake();
            }
        });
    }

    /// Starts a job; view operations are returned for after the next render.
    fn run(&self, cmd: Cmd) -> Option<Cmd> {
        match cmd {
            Cmd::Search { generation, jql } => self.spawn(move |s| Done::Search {
                generation,
                result: s.jira.search(&jql),
            }),
            Cmd::Issue { generation, key } => self.spawn(move |s| Done::Issue {
                generation,
                result: s.jira.issue(&key),
            }),
            Cmd::Comments {
                generation,
                key,
                limit,
            } => self.spawn(move |s| Done::Comments {
                generation,
                result: s.jira.comments(&key, limit),
            }),
            Cmd::AddComment {
                generation,
                key,
                body,
            } => self.spawn(move |s| Done::Comment {
                generation,
                result: s.jira.add_comment(&key, &body),
            }),
            Cmd::LoadTransitions { generation, key } => {
                self.spawn(move |s| Done::TransitionsLoaded {
                    generation,
                    result: s.transitions.load(&key),
                })
            }
            Cmd::ApplyTransition {
                generation,
                key,
                from,
                transition,
            } => self.spawn(move |s| Done::TransitionApplied {
                generation,
                result: s.transitions.apply(&key, &from, &transition),
            }),
            Cmd::AckTransition {
                generation,
                key,
                attempt,
            } => self.spawn(move |s| Done::TransitionAcknowledged {
                generation,
                result: s.transitions.acknowledge_unknown(&key, &attempt),
            }),
            Cmd::ResolveRepos {
                generation,
                candidates,
            } => self.spawn(move |s| {
                let prefer = s.config.worktree.prefer;
                let choices = candidates
                    .into_iter()
                    .map(|c| RepoChoice {
                        result: s
                            .checkouts
                            .resolve_repository(&c.path, prefer)
                            .map_err(|e| err_text(&e)),
                        path: c.path,
                        tags: c.tags,
                        mapped: c.mapped,
                    })
                    .collect();
                Done::Repos {
                    generation,
                    choices,
                }
            }),
            Cmd::ResolvePath { generation, path } => self.spawn(move |s| {
                let result = s
                    .checkouts
                    .resolve_repository(&path, s.config.worktree.prefer)
                    .map_err(|e| err_text(&e));
                Done::RepoPath {
                    generation,
                    path,
                    result,
                }
            }),
            Cmd::ListCheckouts {
                generation,
                repository,
            } => self.spawn(move |s| Done::Checkouts {
                generation,
                result: s
                    .checkouts
                    .list_checkouts(&repository)
                    .map_err(|e| err_text(&e)),
            }),
            Cmd::Plan {
                generation,
                repository,
                selection,
            } => self.spawn(move |s| Done::Planned {
                generation,
                result: s.checkouts.plan_checkout(&repository, selection),
            }),
            Cmd::Snapshot { generation, key } => self.spawn(move |s| Done::Snapshot {
                generation,
                result: handoff::build_snapshot(s.jira.as_ref(), &s.config.handoff, &key),
            }),
            Cmd::Execute {
                generation,
                plan,
                handoff,
            } => self.spawn(move |s| Done::Executed {
                generation,
                outcome: execute_job(s, plan, handoff),
            }),
            Cmd::Launch { generation, ctx } => self.spawn(move |s| Done::Executed {
                generation,
                outcome: launch_job(s, ctx),
            }),
            ui @ (Cmd::Reveal(..) | Cmd::Scroll(..)) => return Some(ui),
        }
        None
    }
}

fn checkout_error_lines(error: CheckoutError) -> Vec<String> {
    let text = err_text(&error);
    match error {
        CheckoutError::RecoveryRequired { path, .. } => vec![
            text,
            format!("Path: {path}"),
            "Choose it under Existing and acknowledge recovery to continue.".into(),
        ],
        CheckoutError::Failed(_) => vec![text],
    }
}

/// Prepare → write the handoff → re-validate → launch, in plan order.
fn execute_job(s: &Services, plan: CheckoutPlan, handoff: Option<HandoffRequest>) -> ExecOutcome {
    let recovery_acknowledged = matches!(
        plan.selection(),
        CheckoutSelection::Existing {
            recovery_acknowledged: true,
            ..
        }
    );
    let prepared = match s.checkouts.prepare_checkout(&plan) {
        Ok(prepared) => prepared,
        Err(error) => return ExecOutcome::CheckoutFailed(checkout_error_lines(error)),
    };
    let Some(HandoffRequest {
        snapshot,
        instruction,
        placement,
    }) = handoff
    else {
        return ExecOutcome::Ready(prepared);
    };
    let snapshot_path = match handoff::save_snapshot(&s.store, &snapshot, &instruction) {
        Ok(path) => path,
        Err(error) => {
            return ExecOutcome::HandoffFailed {
                prepared,
                message: err_text(&error),
            };
        }
    };
    launch_job(
        s,
        LaunchCtx {
            prepared,
            snapshot_path,
            placement,
            recovery_acknowledged,
        },
    )
}

/// Re-validates the approved checkout, then launches once.
fn launch_job(s: &Services, ctx: LaunchCtx) -> ExecOutcome {
    let result = match revalidate(s, &ctx) {
        Some(problem) => Err(problem),
        None => Ok(launch::launch_omp(
            &s.config.omp,
            &ctx.prepared,
            &ctx.snapshot_path,
            ctx.placement,
        )),
    };
    ExecOutcome::Launched { ctx, result }
}

fn revalidate(s: &Services, ctx: &LaunchCtx) -> Option<Vec<String>> {
    let selection = CheckoutSelection::Existing {
        checkout: ctx.prepared.checkout.clone(),
        recovery_acknowledged: ctx.recovery_acknowledged,
    };
    let plan = match s
        .checkouts
        .plan_checkout(&ctx.prepared.repository, selection)
    {
        Ok(plan) => plan,
        Err(error) => return Some(checkout_error_lines(error)),
    };
    let current = match s.checkouts.prepare_checkout(&plan) {
        Ok(current) => current,
        Err(error) => return Some(checkout_error_lines(error)),
    };
    if current.checkout.path != ctx.prepared.checkout.path
        || current.checkout.name != ctx.prepared.checkout.name
    {
        return Some(vec![format!(
            "The checkout changed: now {} at {}.",
            current.checkout.name, current.path
        )]);
    }
    None
}

// ---------------------------------------------------------------------------
// Entry point

pub fn main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let initial_key = match args.as_slice() {
        [] => None,
        [flag] if flag == "-h" || flag == "--help" => {
            println!("{USAGE}");
            return 0;
        }
        [arg] if !arg.starts_with('-') => match normalize_issue_key(arg) {
            Ok(key) => Some(key),
            Err(error) => {
                eprintln!("jira-tern: {error}");
                return 2;
            }
        },
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let config = match config::load() {
        Ok(config) => config,
        Err(error @ ConfigError::Missing(_)) => {
            eprintln!("jira-tern: {error}");
            eprintln!(
                "Copy config.example.json to that path (or set JIRA_TERN_CONFIG) and edit it."
            );
            return 2;
        }
        Err(error) => {
            eprintln!("jira-tern: invalid config: {error}");
            return 2;
        }
    };
    let store = match StateStore::new(state_root()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("jira-tern: state directory unavailable: {error}");
            return 2;
        }
    };
    let options = Options::new()
        .app("jira-tern")
        .version(env!("CARGO_PKG_VERSION"))
        .feature(feature::SEND);
    let mut session = match Session::<Msg>::connect(options) {
        Ok(Some(session)) => session,
        Ok(None) => {
            eprintln!("jira-tern must run inside Tern");
            return 2;
        }
        Err(error) => {
            eprintln!("jira-tern: cannot talk to the terminal: {error}");
            return 1;
        }
    };
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| home_dir().to_string_lossy().into_owned());
    let jira: Arc<dyn JiraApi> = Arc::new(JiraClient::new(&config.jira));
    let services = Services {
        config: Arc::new(config.clone()),
        jira: jira.clone(),
        checkouts: Arc::new(CheckoutService::new(config.worktree.clone(), store.clone())),
        transitions: Arc::new(TransitionService::new(
            jira,
            store.clone(),
            config.jira.base_url.clone(),
        )),
        store: store.clone(),
    };
    let handoff_dir = store.root().join("handoffs");
    let (tx, rx) = mpsc::channel();
    let runner = Runner {
        services,
        tx,
        waker: session.waker(),
    };
    let (state, cmds) = State::new(config, cwd, handoff_dir, initial_key);
    let result = event_loop(&mut session, &runner, &rx, state, cmds);
    let closed = session.close();
    match result.and(closed) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("jira-tern: {error}");
            1
        }
    }
}

fn event_loop(
    session: &mut Session<Msg>,
    runner: &Runner,
    rx: &mpsc::Receiver<Done>,
    mut state: State,
    mut cmds: Vec<Cmd>,
) -> Result<(), tern_sdk::Error> {
    let sf = session.open(SurfaceOptions::screen().title("Jira").keep(false))?;
    // Without a waker, poll so background results still arrive.
    let timeout = if runner.waker.is_some() {
        None
    } else {
        Some(Duration::from_millis(100))
    };
    let mut focused: Option<&'static str> = None;
    let mut revealed: Option<String> = None;
    let mut wizard_step: Option<std::mem::Discriminant<WStep>> = None;
    loop {
        let ui_ops: Vec<Cmd> = cmds.drain(..).filter_map(|cmd| runner.run(cmd)).collect();
        session.render(sf, view::render(&state))?;
        let step = match &state.flow {
            Some(Flow::Wizard(w)) => Some(std::mem::discriminant(&w.step)),
            _ => None,
        };
        if step.is_some() && step != wizard_step {
            session.scroll(sf, view::WIZARD_TITLE_ID, ScrollBy::Start)?;
        }
        wizard_step = step;
        let focus = view::focus_id(&state);
        if focus != focused {
            session.focus(sf, focus)?;
            focused = focus;
        }
        let reveal = view::selection_id(&state);
        if reveal != revealed {
            if let Some(id) = &reveal {
                session.reveal(sf, id, RevealAt::Nearest)?;
            }
            revealed = reveal;
        }
        for op in ui_ops {
            match op {
                Cmd::Reveal(id, at) => session.reveal(sf, &id, at)?,
                Cmd::Scroll(id, by) => session.scroll(sf, &id, by)?,
                _ => {}
            }
        }
        loop {
            match session.next(timeout)? {
                Some(Input::Key(key)) => cmds.extend(state.on_key(&key)),
                Some(Input::Msg(msg, _)) => {
                    if msg == Msg::Focus {
                        focused = None;
                    }
                    cmds.extend(state.on_msg(msg));
                }
                Some(Input::Event(Event::Gone(_))) => return Ok(()),
                Some(Input::Event(_)) | Some(Input::Clipboard(_)) => continue,
                None => {}
            }
            break;
        }
        for done in rx.try_iter() {
            cmds.extend(state.on_done(done));
        }
        if state.quit {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AuthKind, Backend, Deployment, FsIdentity, HandoffConfig, JiraConfig, OmpConfig,
        WorktreeConfig,
    };

    fn config() -> Config {
        Config {
            jira: JiraConfig {
                base_url: "https://jira.example.com".into(),
                deployment: Deployment::Cloud,
                auth: AuthKind::Bearer,
                email: None,
                token_command: vec!["true".into()],
                max_results: 100,
                timeout: Duration::from_secs(10),
            },
            filters: Vec::new(),
            search_jql: "text ~ \"{query}\"".into(),
            worktree: WorktreeConfig {
                repos: Vec::new(),
                prefer: Backend::Git,
                root: "/tmp/checkouts".into(),
                git_base: None,
                jj_base: None,
            },
            handoff: HandoffConfig {
                max_comments: 5,
                max_description_chars: 1000,
                max_comment_chars: 500,
                max_chars: 5000,
            },
            omp: OmpConfig {
                binary: "omp".into(),
                profile: "default".into(),
            },
        }
    }

    fn state() -> State {
        State::new(
            config(),
            "/work".into(),
            PathBuf::from("/state/handoffs"),
            None,
        )
        .0
    }

    fn issue(key: &str) -> Issue {
        Issue {
            key: key.into(),
            summary: format!("summary {key}"),
            status: StatusRef {
                id: "1".into(),
                name: "Open".into(),
            },
            issue_type: "Task".into(),
            assignee: "me".into(),
            updated: "2026-01-02T03:04:05.000+0000".into(),
            description: String::new(),
            url: format!("https://jira.example.com/browse/{key}"),
        }
    }

    fn page(keys: &[&str]) -> Page<Issue> {
        Page {
            items: keys.iter().map(|k| issue(k)).collect(),
            total: Some(keys.len()),
            has_more: false,
        }
    }

    fn query(jql: &str) -> QueryRef {
        QueryRef {
            label: jql.into(),
            jql: jql.into(),
        }
    }

    fn key(name: &str) -> Key {
        Key::named(name)
    }

    fn ctrl(name: &str) -> Key {
        Key {
            ctrl: true,
            ..Key::named(name)
        }
    }

    fn typed(text: &str) -> Key {
        Key {
            text: Some(text.into()),
            ..Key::named(text.to_lowercase())
        }
    }

    #[test]
    fn selection_moves_are_clamped_to_the_list() {
        assert_eq!(step_index(0, -1, 3), 0);
        assert_eq!(step_index(1, isize::MAX, 3), 2);
        assert_eq!(step_index(2, isize::MIN, 3), 0);
        assert_eq!(step_index(7, 0, 3), 2);
        assert_eq!(step_index(4, 1, 0), 0);

        let mut list = ListState::new(query("a"));
        list.load(query("a"), 1);
        list.apply(1, Ok(page(&["A-1", "A-2"])));
        list.move_by(5);
        assert_eq!(list.selected_issue().map(|i| i.key.as_str()), Some("A-2"));
        list.move_by(-9);
        assert_eq!(list.selected, 0);
    }

    #[test]
    fn a_late_response_from_a_stale_query_never_overwrites_the_newer_one() {
        let mut list = ListState::new(query("project = A"));
        list.load(query("project = A"), 1);
        list.load(query("project = B"), 2);
        list.apply(2, Ok(page(&["B-1", "B-2", "B-3"])));
        list.move_by(2);
        assert_eq!(list.selected_issue().map(|i| i.key.as_str()), Some("B-3"));

        list.apply(1, Ok(page(&["A-1"])));
        assert_eq!(list.loader.data_key.as_deref(), Some("project = B"));
        let keys: Vec<&str> = list.items().iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["B-1", "B-2", "B-3"]);
        assert_eq!(list.selected_issue().map(|i| i.key.as_str()), Some("B-3"));
        assert!(list.loader.fresh());
    }

    #[test]
    fn refresh_keeps_the_selected_issue_and_a_failed_refresh_marks_data_stale() {
        let mut list = ListState::new(query("project = B"));
        list.load(query("project = B"), 1);
        list.apply(1, Ok(page(&["B-1", "B-2", "B-3"])));
        list.move_by(2);

        list.load(query("project = B"), 2);
        list.apply(2, Ok(page(&["B-3", "B-1"])));
        assert_eq!(list.selected_issue().map(|i| i.key.as_str()), Some("B-3"));

        list.load(query("project = B"), 3);
        assert!(!list.loader.fresh());
        list.apply(3, Err("HTTP 503 from Jira".into()));
        assert_eq!(list.loader.status, LoadStatus::Error);
        assert!(list.loader.stale);
        assert!(
            list.loader
                .error
                .as_deref()
                .is_some_and(|e| e.contains("503"))
        );
        assert!(!list.loader.fresh());
        assert_eq!(list.selected_issue().map(|i| i.key.as_str()), Some("B-3"));
    }

    #[test]
    fn stale_job_results_are_dropped_by_state() {
        let mut s = state();
        let first = match s.list.loader.pending {
            Some(generation) => generation,
            None => panic!("initial search not started"),
        };
        let cmds = s.on_key(&typed("r"));
        assert!(
            matches!(cmds.as_slice(), [Cmd::Search { generation, .. }] if *generation != first)
        );
        s.on_done(Done::Search {
            generation: first,
            result: Ok(page(&["OLD-1"])),
        });
        assert!(s.list.items().is_empty());
        assert_eq!(s.list.loader.status, LoadStatus::Loading);
    }

    fn compose_state() -> State {
        let mut s = state();
        s.flow = Some(Flow::Compose(Compose::new("ABC-1".into())));
        s
    }

    fn compose(s: &State) -> &Compose {
        match &s.flow {
            Some(Flow::Compose(c)) => c,
            _ => panic!("compose flow closed"),
        }
    }

    #[test]
    fn empty_or_too_long_drafts_are_refused_before_confirmation() {
        assert!(validate_comment("  \n\t ").is_err());
        assert_eq!(validate_comment("  hi \n"), Ok("hi"));
        assert!(validate_comment(&"x".repeat(MAX_COMMENT_CHARS)).is_ok());
        assert!(validate_comment(&"x".repeat(MAX_COMMENT_CHARS + 1)).is_err());

        let mut s = compose_state();
        s.on_key(&typed(" "));
        let cmds = s.on_key(&ctrl("enter"));
        assert!(cmds.is_empty());
        assert_eq!(compose(&s).step, ComposeStep::Editing);
        assert!(compose(&s).message.is_some());

        if let Some(Flow::Compose(c)) = s.flow.as_mut() {
            c.draft = TextBuf::new("y".repeat(MAX_COMMENT_CHARS + 1));
        }
        s.on_key(&ctrl("enter"));
        assert_eq!(compose(&s).step, ComposeStep::Editing);
    }

    #[test]
    fn an_unknown_comment_outcome_requires_a_second_confirmation_to_post_again() {
        let mut s = compose_state();
        for ch in ["h", "i"] {
            s.on_key(&typed(ch));
        }
        s.on_key(&ctrl("enter"));
        assert_eq!(compose(&s).step, ComposeStep::Confirm);
        let cmds = s.on_key(&key("enter"));
        let generation = match cmds.as_slice() {
            [
                Cmd::AddComment {
                    generation,
                    key,
                    body,
                },
            ] if key == "ABC-1" && body == "hi" => *generation,
            _ => panic!("expected one AddComment"),
        };
        assert!(s.write_in_flight());

        s.on_done(Done::Comment {
            generation,
            result: CommentResult::Unknown {
                message: "timeout".into(),
            },
        });
        assert_eq!(compose(&s).step, ComposeStep::Editing);
        assert_eq!(compose(&s).draft.text, "hi");

        s.on_key(&ctrl("enter"));
        assert_eq!(compose(&s).step, ComposeStep::Recheck);
        assert!(s.on_key(&key("enter")).is_empty());
        assert_eq!(compose(&s).step, ComposeStep::Confirm);
        let cmds = s.on_key(&key("enter"));
        assert!(matches!(cmds.as_slice(), [Cmd::AddComment { .. }]));
    }

    #[test]
    fn a_rejected_comment_keeps_the_draft_and_a_created_one_reloads_comments() {
        let mut s = compose_state();
        s.detail = Some(Detail {
            key: "ABC-1".into(),
            issue: Loader::new(),
            comments: Some(Loader::new()),
            focus: Pane::Description,
        });
        s.on_key(&typed("x"));
        s.on_key(&ctrl("s"));
        let generation = match s.on_key(&key("enter")).as_slice() {
            [Cmd::AddComment { generation, .. }] => *generation,
            _ => panic!("expected AddComment"),
        };
        s.on_done(Done::Comment {
            generation,
            result: CommentResult::Rejected {
                message: "body invalid".into(),
            },
        });
        assert_eq!(compose(&s).draft.text, "x");
        assert!(!compose(&s).uncertain);

        s.on_key(&ctrl("enter"));
        assert_eq!(compose(&s).step, ComposeStep::Confirm);
        let generation = match s.on_key(&key("enter")).as_slice() {
            [Cmd::AddComment { generation, .. }] => *generation,
            _ => panic!("expected AddComment"),
        };
        let cmds = s.on_done(Done::Comment {
            generation,
            result: CommentResult::Created { comment: None },
        });
        assert!(s.flow.is_none());
        assert!(matches!(cmds.as_slice(), [Cmd::Comments { key, .. }] if key == "ABC-1"));
    }

    #[test]
    fn the_wizard_executes_nothing_before_the_final_confirmation() {
        let mut s = state();
        let repository = Repository {
            backend: Backend::Git,
            key: "/r/.git".into(),
            root: "/r".into(),
            name: "r".into(),
        };
        let plan = CheckoutPlan {
            repository: repository.clone(),
            selection: CheckoutSelection::New {
                name: "ABC-1".into(),
                base: None,
            },
            destination: "/tmp/checkouts/r/ABC-1".into(),
            base_ids: vec!["abc".into()],
            identity: None::<FsIdentity>,
        };
        let cmds = {
            s.start_wizard("ABC-1".into(), false, &mut Vec::new());
            let Some(Flow::Wizard(w)) = s.flow.as_mut() else {
                panic!("wizard not open")
            };
            w.repository = Some(repository);
            w.plan = Some(plan);
            w.step = WStep::Action { selected: 0 };
            let mut all = Vec::new();
            all.extend(s.on_key(&key("enter")));
            all
        };
        assert!(cmds.is_empty());
        assert!(matches!(&s.flow, Some(Flow::Wizard(w)) if matches!(w.step, WStep::Confirm)));
        let cmds = s.on_key(&key("enter"));
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Execute { handoff: None, .. }]
        ));
        assert!(s.write_in_flight());
        assert!(s.on_key(&key("escape")).is_empty());
        assert!(
            matches!(&s.flow, Some(Flow::Wizard(w)) if matches!(w.step, WStep::Executing { .. }))
        );
    }

    #[test]
    fn editor_caret_is_reported_in_utf16_units() {
        let mut buf = TextBuf::default();
        buf.insert("a😀");
        assert_eq!(buf.utf16_cursor(), 3);
        buf.edit(&key("enter"), true);
        buf.insert("é");
        buf.edit(&key("up"), true);
        assert_eq!(buf.cursor, 1);
        buf.edit(&key("backspace"), true);
        assert_eq!(buf.text, "😀\né");
    }

    #[test]
    fn stale_transition_ack_requires_fresh_prompt() {
        let mut s = state();
        let generation = s.next_gen();
        s.flow = Some(Flow::Transition(TransitionFlow {
            key: "A-1".into(),
            step: TStep::Acknowledging { generation },
            message: None,
        }));
        let cmds = s.on_done(Done::TransitionAcknowledged {
            generation,
            result: Err(TransitionError::Blocked {
                attempt: "attempt-y".into(),
                record_started_at: "2026-10-13T00:00:00.000Z".into(),
            }),
        });
        let load = match cmds.as_slice() {
            [Cmd::LoadTransitions { generation, key }] if key == "A-1" => *generation,
            other => panic!("expected reload, got {}", other.len()),
        };
        assert!(matches!(
            &s.flow,
            Some(Flow::Transition(TransitionFlow {
                step: TStep::Loading { generation },
                message: Some(Notice { level: Level::Warning, .. }),
                ..
            })) if *generation == load
        ));

        // A completion from another generation leaves the flow untouched.
        s.on_done(Done::TransitionsLoaded {
            generation: load + 100,
            result: Err(TransitionError::Changed("stale".into())),
        });
        assert!(matches!(
            &s.flow,
            Some(Flow::Transition(TransitionFlow { step: TStep::Loading { generation }, .. }))
                if *generation == load
        ));

        s.on_done(Done::TransitionsLoaded {
            generation: load,
            result: Ok(TransitionChoices {
                issue: issue("A-1"),
                transitions: Vec::new(),
                blocked: Some(crate::transitions::BlockedAttempt {
                    attempt: "attempt-y".into(),
                    started_at: "2026-10-13T00:00:00.000Z".into(),
                }),
            }),
        });
        assert!(matches!(
            &s.flow,
            Some(Flow::Transition(TransitionFlow {
                step: TStep::Blocked { attempt, since, .. },
                ..
            })) if attempt == "attempt-y" && since == "2026-10-13T00:00:00.000Z"
        ));
    }
}
