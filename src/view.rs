//! Pure renderers: app `State` → TSP `View<Msg>`. No I/O and no session
//! access; the runtime in `app` renders, focuses and reveals what these return.

use tern_sdk::ui::{self, Gap, Node, NodeKey, Span, Tone, View};

use crate::app::{
    Base, Choice, Compose, ComposeStep, Detail, Flow, Level, LoadStatus, Loader, Msg, Notice,
    Outcome, Pane, PromptKind, State, TStep, TextBuf, TransitionFlow, WStep, Wizard, action_items,
    existing_items, kind_items, repo_items, status_text, transition_items, validate_comment,
};
use crate::launch;
use crate::model::{CheckoutPlan, CheckoutSelection, MAX_COMMENT_CHARS, Snapshot};
use crate::text::{format_timestamp, single_line};

/// Ids derive from the tree: `main`'s children are `main.<key>`.
pub(crate) const ISSUES_ID: &str = "main.issues";
pub(crate) const CHOICES_ID: &str = "main.choices";
pub(crate) const DESCRIPTION_ID: &str = "main.desc";
pub(crate) const COMMENTS_ID: &str = "main.comments";
pub(crate) const SEARCH_INPUT_ID: &str = "main.search";
pub(crate) const JQL_INPUT_ID: &str = "main.jql-input";
pub(crate) const REPO_PATH_ID: &str = "main.repo-path";
pub(crate) const NEW_NAME_ID: &str = "main.new-name";
pub(crate) const EXISTING_PATH_ID: &str = "main.existing-path";
pub(crate) const COMMENT_EDITOR_ID: &str = "main.comment";
pub(crate) const INSTRUCTION_EDITOR_ID: &str = "main.instruction";

type N = Node<Msg>;

/// A screen's body (`main`'s children) and its key help line.
struct Screen {
    body: Vec<N>,
    help: String,
}

impl Screen {
    fn new(body: Vec<N>, help: impl Into<String>) -> Self {
        Screen {
            body,
            help: help.into(),
        }
    }
}

pub(crate) fn render(state: &State) -> View<Msg> {
    let Screen { body, help } = match &state.flow {
        Some(Flow::Prompt { kind, buf }) => prompt(state, *kind, buf),
        Some(Flow::Filter { .. }) => filter(state),
        Some(Flow::Compose(c)) => compose(c),
        Some(Flow::Transition(t)) => transition(state, t),
        Some(Flow::Wizard(w)) => wizard(state, w),
        None => match (state.base, &state.detail) {
            (Base::Detail, Some(detail)) => detail_screen(detail),
            _ => list_screen(state),
        },
    };
    let mut dock: Vec<N> = Vec::new();
    if let Some(n) = &state.notice {
        dock.push(notice(n, "notice"));
    }
    dock.push(ui::text(help).tone(Tone::Muted).key("help").into());
    View::new().main(body).dock(dock)
}

/// The field that should own the caret, if any.
pub(crate) fn focus_id(state: &State) -> Option<&'static str> {
    match state.flow.as_ref()? {
        Flow::Prompt {
            kind: PromptKind::Search,
            ..
        } => Some(SEARCH_INPUT_ID),
        Flow::Prompt {
            kind: PromptKind::Jql,
            ..
        } => Some(JQL_INPUT_ID),
        Flow::Compose(c) if c.step == ComposeStep::Editing => Some(COMMENT_EDITOR_ID),
        Flow::Wizard(w) => match w.step {
            WStep::RepoPath { .. } => Some(REPO_PATH_ID),
            WStep::NewName { .. } => Some(NEW_NAME_ID),
            WStep::ExistingPath { .. } => Some(EXISTING_PATH_ID),
            WStep::Instruction => Some(INSTRUCTION_EDITOR_ID),
            _ => None,
        },
        _ => None,
    }
}

/// The selected list row to keep in view, if any.
pub(crate) fn selection_id(state: &State) -> Option<String> {
    if state.flow.is_some() {
        let (items, selected) = state.flow_choices()?;
        return (!items.is_empty()).then(|| format!("{CHOICES_ID}.{selected}"));
    }
    (state.base == Base::List && !state.list.items().is_empty())
        .then(|| format!("{ISSUES_ID}.{}", state.list.selected))
}

// ---------------------------------------------------------------------------
// Building blocks

fn tone(level: Level) -> Tone {
    match level {
        Level::Info => Tone::Info,
        Level::Success => Tone::Success,
        Level::Warning => Tone::Warning,
        Level::Error => Tone::Error,
    }
}

fn token(level: Level) -> &'static str {
    match level {
        Level::Info => "info",
        Level::Success => "success",
        Level::Warning => "warning",
        Level::Error => "error",
    }
}

fn heading(key: impl Into<NodeKey>, text: impl Into<String>) -> N {
    ui::text(vec![ui::span(text).style("strong")])
        .key(key)
        .into()
}

fn line(key: impl Into<NodeKey>, text: impl Into<String>) -> N {
    ui::text(Into::<String>::into(text)).key(key).into()
}

fn muted(key: impl Into<NodeKey>, text: impl Into<String>) -> N {
    ui::text(Into::<String>::into(text))
        .tone(Tone::Muted)
        .key(key)
        .into()
}

fn toned(key: impl Into<NodeKey>, text: impl Into<String>, level: Level) -> N {
    ui::text(Into::<String>::into(text))
        .tone(tone(level))
        .key(key)
        .into()
}

fn lines(key: impl Into<NodeKey>, body: &[String]) -> N {
    ui::text(body.join("\n")).key(key).into()
}

fn notice(n: &Notice, key: &str) -> N {
    toned(key, n.text.clone(), n.level)
}

fn busy(label: impl Into<String>) -> N {
    ui::row::<Msg>()
        .key("busy")
        .gap(Gap::Sm)
        .child(ui::spinner::<Msg>().key("spin"))
        .child(ui::text::<Msg>(Into::<String>::into(label)).key("label"))
        .into()
}

/// Buttons that act like the named keys.
fn buttons(items: &[(&str, &'static str)]) -> N {
    let mut row = ui::row::<Msg>().key("buttons").gap(Gap::Sm);
    for (label, press) in items {
        row = row.child(ui::html::button::<Msg>(*label).on_click(Msg::Press(press)));
    }
    row.into()
}

fn choice_list(choices: &[Choice], selected: usize) -> N {
    ui::list::<Msg>()
        .key("choices")
        .selected(format!("{CHOICES_ID}.{selected}"))
        .children(choices.iter().enumerate().map(|(i, c)| {
            ui::item::<Msg>(c.label.clone())
                .key(i)
                .detail(c.detail.clone())
                .disabled(c.disabled)
                .on_select(move |_| Msg::Pick(i))
                .on_activate(move |_| Msg::Open(i))
        }))
        .into()
}

fn input(key: &str, buf: &TextBuf, placeholder: &str) -> N {
    ui::input::<Msg>()
        .key(key)
        .text(buf.text.clone())
        .cursor(buf.utf16_cursor())
        .placeholder(placeholder)
        .on_focus(Msg::Focus)
        .into()
}

fn editor(key: &str, buf: &TextBuf, placeholder: &str) -> N {
    ui::editor::<Msg>()
        .key(key)
        .text(buf.text.clone())
        .cursor(buf.utf16_cursor())
        .placeholder(placeholder)
        .sendable(true)
        .on_send(|text| Msg::Send(text.to_owned()))
        .on_focus(Msg::Focus)
        .into()
}

fn load_badge<T>(loader: &Loader<T>) -> Option<(&'static str, Level)> {
    match loader.status {
        LoadStatus::Loading if loader.stale => {
            Some(("[loading… showing stale data]", Level::Warning))
        }
        LoadStatus::Loading => Some(("[loading…]", Level::Warning)),
        LoadStatus::Error if loader.stale => Some(("[error · stale data]", Level::Error)),
        LoadStatus::Error => Some(("[error]", Level::Error)),
        _ if loader.stale => Some(("[stale]", Level::Warning)),
        _ => None,
    }
}

fn title_with_badge<T>(key: &str, title: String, loader: &Loader<T>) -> N {
    let mut spans = vec![ui::span(title).style("strong")];
    if let Some((badge, level)) = load_badge(loader) {
        spans.push(Span::from(" "));
        spans.push(ui::span(badge).style(token(level)));
    }
    ui::text(spans).key(key).into()
}

fn with_message(mut body: Vec<N>, message: &Option<Notice>) -> Vec<N> {
    if let Some(n) = message {
        body.insert(1.min(body.len()), notice(n, "message"));
    }
    body
}

// ---------------------------------------------------------------------------
// Issue list and detail

const LIST_HELP: &str = "Esc quit · j/k move · Enter detail · / search · J jql · f filter · r refresh · c comment · w checkout · d omp · s status";

fn list_screen(state: &State) -> Screen {
    let list = &state.list;
    let items = list.items();
    let mut title = format!("Jira · {}", single_line(&list.query.label));
    if let Some(page) = &list.loader.data {
        let more = if page.has_more { "+" } else { "" };
        let total = page.total.map(|t| format!(" of {t}")).unwrap_or_default();
        title.push_str(&format!(" ({}{more}{total})", items.len()));
    }
    let mut body = vec![
        title_with_badge("title", title, &list.loader),
        muted("jql", format!("JQL: {}", single_line(&list.query.jql))),
    ];
    if items.is_empty() {
        body.push(match (list.loader.status, &list.loader.error) {
            (LoadStatus::Loading, _) => muted("empty", "Loading…"),
            (LoadStatus::Error, error) => toned(
                "empty",
                format!("Search failed: {}", error.as_deref().unwrap_or_default()),
                Level::Error,
            ),
            _ => muted("empty", "No issues match this query."),
        });
        return Screen::new(body, LIST_HELP);
    }
    let selected = list.selected.min(items.len() - 1);
    body.push(
        ui::list::<Msg>()
            .key("issues")
            .selected(format!("{ISSUES_ID}.{selected}"))
            .children(items.iter().enumerate().map(|(i, issue)| {
                let assignee = single_line(&issue.assignee);
                let assignee = if assignee.is_empty() {
                    "Unassigned".to_owned()
                } else {
                    assignee
                };
                let label = vec![
                    ui::span(single_line(&issue.key)).style("strong"),
                    Span::from(format!("  {}", single_line(&issue.summary))),
                ];
                ui::item::<Msg>(label)
                    .key(i)
                    .detail(format!("{} · {assignee}", single_line(&issue.status.name)))
                    .value(format_timestamp(&issue.updated, true))
                    .on_select(move |_| Msg::Pick(i))
                    .on_activate(move |_| Msg::Open(i))
            }))
            .into(),
    );
    let more = list.loader.data.as_ref().is_some_and(|p| p.has_more);
    body.push(match (&list.loader.status, &list.loader.error) {
        (LoadStatus::Error, error) => toned(
            "status",
            format!(
                "Refresh failed: {} — actions need a fresh fetch (r)",
                error.as_deref().unwrap_or_default()
            ),
            Level::Error,
        ),
        _ => muted(
            "status",
            format!(
                "{} / {}{}",
                selected + 1,
                items.len(),
                if more {
                    " · more results exist on the server"
                } else {
                    ""
                }
            ),
        ),
    });
    Screen::new(body, LIST_HELP)
}

fn detail_screen(detail: &Detail) -> Screen {
    let issue = detail.issue.data.as_ref();
    let title = match issue {
        Some(issue) => format!(
            "{} · {}",
            single_line(&detail.key),
            single_line(&issue.summary)
        ),
        None => single_line(&detail.key),
    };
    let mut body = vec![title_with_badge("title", title, &detail.issue)];
    if let Some(issue) = issue {
        let assignee = single_line(&issue.assignee);
        body.push(
            ui::kv::<Msg>()
                .key("fields")
                .item("Status", status_text(&issue.status))
                .item("Type", single_line(&issue.issue_type))
                .item(
                    "Assignee",
                    if assignee.is_empty() {
                        "Unassigned".to_owned()
                    } else {
                        assignee
                    },
                )
                .item("Updated", format_timestamp(&issue.updated, false))
                .item("URL", single_line(&issue.url))
                .into(),
        );
        if detail.issue.status == LoadStatus::Error {
            body.push(toned(
                "refresh",
                format!(
                    "Refresh failed: {} — actions need a fresh fetch (r)",
                    detail.issue.error.as_deref().unwrap_or_default()
                ),
                Level::Error,
            ));
        }
    }

    let description = match issue {
        Some(issue) if issue.description.trim().is_empty() => muted("text", "(no description)"),
        Some(issue) => line("text", issue.description.clone()),
        None if detail.issue.status == LoadStatus::Error => toned(
            "text",
            format!(
                "Issue fetch failed: {}",
                detail.issue.error.as_deref().unwrap_or_default()
            ),
            Level::Error,
        ),
        None => muted("text", "Loading…"),
    };
    body.push(pane(
        "desc",
        "Description",
        detail.focus == Pane::Description,
        vec![description],
    ));
    body.push(pane(
        "comments",
        "Comments",
        detail.focus == Pane::Comments,
        comment_nodes(detail),
    ));
    Screen::new(
        body,
        "Esc back · Tab pane · j/k scroll · PgUp/PgDn · g/G top/bottom · r refresh · c comment · w checkout · d omp · s status",
    )
}

fn pane(key: &str, name: &str, focused: bool, children: Vec<N>) -> N {
    let head = if focused {
        format!("▸ {name}")
    } else {
        name.to_owned()
    };
    let section = ui::section::<Msg>().key(key).head(head).children(children);
    if focused {
        section.tone(Tone::Accent).into()
    } else {
        section.into()
    }
}

fn comment_nodes(detail: &Detail) -> Vec<N> {
    let Some(loader) = &detail.comments else {
        return vec![muted(
            "skipped",
            "Comments not fetched (handoff.maxComments = 0).",
        )];
    };
    let Some(page) = &loader.data else {
        return vec![match loader.status {
            LoadStatus::Error => toned(
                "error",
                format!(
                    "Comments fetch failed: {}",
                    loader.error.as_deref().unwrap_or_default()
                ),
                Level::Error,
            ),
            _ => muted("loading", "Loading…"),
        }];
    };
    let count = page.items.len();
    let mut summary = format!("{count} comment{}", if count == 1 { "" } else { "s" });
    if page.has_more {
        summary.push_str(" — more comments not shown");
    }
    if let Some((badge, _)) = load_badge(loader) {
        summary.push(' ');
        summary.push_str(badge);
    }
    let mut out = vec![muted("summary", summary)];
    if loader.status == LoadStatus::Error {
        out.push(toned(
            "error",
            format!(
                "Refresh failed: {}",
                loader.error.as_deref().unwrap_or_default()
            ),
            Level::Error,
        ));
    }
    for (i, comment) in page.items.iter().enumerate() {
        let author = single_line(&comment.author);
        let head = vec![
            ui::span(if author.is_empty() {
                "?".to_owned()
            } else {
                author
            })
            .style("strong"),
            Span::from(format!(" · {}", format_timestamp(&comment.created, false))),
        ];
        let text = if comment.body.trim().is_empty() {
            "(empty)".to_owned()
        } else {
            comment.body.clone()
        };
        out.push(
            ui::card::<Msg>()
                .key(i)
                .head(head)
                .child(ui::text::<Msg>(text).key("body"))
                .into(),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Prompts and filters

fn prompt(state: &State, kind: PromptKind, buf: &TextBuf) -> Screen {
    let body = match kind {
        PromptKind::Search => vec![
            heading("title", "Search Jira issues"),
            muted(
                "template",
                format!("Template: {}", single_line(&state.config.search_jql)),
            ),
            input("search", buf, "text to find"),
        ],
        PromptKind::Jql => vec![
            heading("title", "Raw JQL (sent as-is)"),
            input("jql-input", buf, "JQL"),
        ],
    };
    Screen::new(body, "Enter run · Esc cancel")
}

fn filter(state: &State) -> Screen {
    let (items, selected) = state.flow_choices().unwrap_or_default();
    Screen::new(
        vec![
            heading("title", "Jira filter"),
            choice_list(&items, selected),
        ],
        "j/k move · Enter load · Esc cancel",
    )
}

// ---------------------------------------------------------------------------
// Comment composer

fn compose(c: &Compose) -> Screen {
    let key = single_line(&c.key);
    let preview = validate_comment(&c.draft.text)
        .unwrap_or_default()
        .to_owned();
    let (body, help) = match c.step {
        ComposeStep::Editing => {
            let count = c.draft.text.trim().chars().count();
            let level = if count > MAX_COMMENT_CHARS {
                Level::Error
            } else {
                Level::Info
            };
            let mut body = vec![
                heading("title", format!("Comment on {key}")),
                editor(
                    "comment",
                    &c.draft,
                    "Write a comment… (Ctrl+Enter sends, Esc cancels)",
                ),
                if count > MAX_COMMENT_CHARS {
                    toned(
                        "count",
                        format!("{count} / {MAX_COMMENT_CHARS} characters"),
                        level,
                    )
                } else {
                    muted("count", format!("{count} / {MAX_COMMENT_CHARS} characters"))
                },
            ];
            if c.uncertain {
                body.push(toned(
                    "uncertain",
                    "The previous post had an UNKNOWN outcome — check the issue in Jira before posting again.",
                    Level::Warning,
                ));
            }
            (
                body,
                "Ctrl+Enter (or Ctrl+S) send · Enter newline · Esc cancel",
            )
        }
        ComposeStep::Discard => (
            vec![
                heading("title", format!("Discard the comment draft for {key}?")),
                line("text", "Your text will be lost; nothing is posted."),
                buttons(&[("Discard", "enter"), ("Keep editing", "escape")]),
            ],
            "Enter/y discard · Esc/n keep editing",
        ),
        ComposeStep::Recheck => (
            vec![
                heading("title", "Did you check Jira?"),
                toned(
                    "text",
                    format!(
                        "The previous post to {key} had an UNKNOWN outcome: the comment may already exist. Open the issue in Jira and confirm it is not there before posting again."
                    ),
                    Level::Warning,
                ),
                buttons(&[("I checked — it is not there", "enter"), ("Back", "escape")]),
            ],
            "Enter I checked Jira · Esc back",
        ),
        ComposeStep::Confirm => (
            vec![
                heading("title", format!("Post this comment to {key}?")),
                ui::kv::<Msg>()
                    .key("facts")
                    .item("Issue", key.clone())
                    .item("Characters", preview.chars().count().to_string())
                    .into(),
                toned(
                    "once",
                    "The comment is posted once and never retried.",
                    Level::Warning,
                ),
                ui::card::<Msg>()
                    .key("preview")
                    .head("Preview")
                    .child(ui::text::<Msg>(preview.clone()).key("body"))
                    .into(),
                buttons(&[("Post comment", "enter"), ("Back", "escape")]),
            ],
            "Enter post once · Esc back to editing",
        ),
        ComposeStep::Posting { .. } => (
            vec![
                heading("title", format!("Comment on {key}")),
                busy(format!("Posting comment to {key}…")),
                ui::card::<Msg>()
                    .key("preview")
                    .head("Preview")
                    .child(ui::text::<Msg>(preview).key("body"))
                    .into(),
            ],
            "Waiting for Jira…",
        ),
    };
    Screen::new(with_message(body, &c.message), help)
}

// ---------------------------------------------------------------------------
// Transitions

fn transition(state: &State, t: &TransitionFlow) -> Screen {
    let key = single_line(&t.key);
    let site = single_line(&state.config.jira.base_url);
    let (body, help): (Vec<N>, &str) = match &t.step {
        TStep::Loading { .. } => (
            vec![
                heading("title", format!("{key} 상태 전환")),
                busy(format!("Loading transitions for {key}…")),
            ],
            "Esc cancel",
        ),
        TStep::Blocked { since, url } => (
            vec![
                heading("title", "Jira 이력을 확인했습니까?"),
                lines(
                    "text",
                    &[
                        format!(
                            "이전 전환 요청(시작 {})의 결과가 불확실하여(Pending/unknown) 자동 전환을 막았습니다.",
                            single_line(since)
                        ),
                        format!("사이트: {site}"),
                        format!("이슈: {key} — {}", single_line(url)),
                        "Jira 웹에서 이슈 이력을 직접 확인했으면 계속하여 새 전환을 선택하세요."
                            .into(),
                    ],
                ),
                buttons(&[("이력을 확인했습니다", "enter"), ("Cancel", "escape")]),
            ],
            "Enter/y I checked the history · Esc/n cancel",
        ),
        TStep::Acknowledging { .. } => (
            vec![
                heading("title", format!("{key} 상태 전환")),
                busy("Recording acknowledgement…"),
            ],
            "Waiting…",
        ),
        TStep::Pick { choices, selected } => (
            vec![
                heading(
                    "title",
                    format!(
                        "{key} 상태 전환 (현재 {})",
                        status_text(&choices.issue.status)
                    ),
                ),
                muted(
                    "info",
                    "Transitions that need a screen or required fields are done on the Jira web.",
                ),
                choice_list(&transition_items(choices), *selected),
            ],
            "j/k move · Enter choose · Esc cancel",
        ),
        TStep::Confirm { choices, index } => {
            let chosen = &choices.transitions[*index];
            (
                vec![
                    heading("title", "Jira 상태 전환"),
                    ui::kv::<Msg>()
                        .key("facts")
                        .item("사이트", site.clone())
                        .item("이슈", key.clone())
                        .item(
                            "상태",
                            format!(
                                "{} → {}",
                                status_text(&choices.issue.status),
                                status_text(&chosen.to)
                            ),
                        )
                        .item(
                            "전환",
                            format!(
                                "{} (id {})",
                                single_line(&chosen.name),
                                single_line(&chosen.id)
                            ),
                        )
                        .into(),
                    toned(
                        "once",
                        "확인하면 이 전환 요청을 한 번 보냅니다. 다른 필드는 변경하지 않습니다.",
                        Level::Warning,
                    ),
                    buttons(&[("Send transition", "enter"), ("Back", "escape")]),
                ],
                "Enter send once · Esc back",
            )
        }
        TStep::Applying { .. } => (
            vec![
                heading("title", format!("{key} 상태 전환")),
                busy("Sending transition…"),
            ],
            "Waiting for Jira…",
        ),
        TStep::Finished {
            title,
            lines: text,
            level,
        } => (
            vec![
                heading("title", title.clone()),
                toned("text", text.join("\n"), *level),
                buttons(&[("Close", "enter")]),
            ],
            "Enter/Esc close",
        ),
    };
    Screen::new(with_message(body, &t.message), help)
}

// ---------------------------------------------------------------------------
// Checkout wizard

fn wizard(state: &State, w: &Wizard) -> Screen {
    let key = single_line(&w.key);
    let repo_name = w
        .repository
        .as_ref()
        .map(|r| single_line(&r.name))
        .unwrap_or_default();
    let (body, help): (Vec<N>, String) = match &w.step {
        WStep::Resolving { .. } => (
            vec![heading("title", format!("Checkout for {key}")), busy(format!("Resolving repositories for {key}…"))],
            "Esc cancel".into(),
        ),
        WStep::Repo { selected } => (
            vec![
                heading("title", format!("Checkout for {key} — 1/5 repository")),
                muted(
                    "info",
                    "★ marks the repository mapped to this Jira project; mapping only changes the default. Nothing is created until the final confirmation.",
                ),
                choice_list(&repo_items(w), *selected),
            ],
            "j/k move · Enter choose · Esc close".into(),
        ),
        WStep::RepoPath { buf } => (
            vec![
                heading("title", "Repository path (absolute, ~/, or relative to the current directory)"),
                input("repo-path", buf, "/path/to/repository"),
            ],
            "Enter resolve · Esc back".into(),
        ),
        WStep::ResolvingPath { path, .. } => (
            vec![heading("title", format!("Checkout for {key}")), busy(format!("Resolving {path}…"))],
            "Esc back".into(),
        ),
        WStep::Kind { selected } => {
            let (backend, root) = w
                .repository
                .as_ref()
                .map(|r| (r.backend.label(), r.root.clone()))
                .unwrap_or_default();
            (
                vec![
                    heading("title", format!("{repo_name} [{backend}] — 2/5 checkout")),
                    muted("info", format!("Repository: {root}")),
                    choice_list(&kind_items(w), *selected),
                ],
                "j/k move · Enter choose · Esc back".into(),
            )
        }
        WStep::NewName { buf } => (
            vec![
                heading("title", format!("New checkout name (empty = {key})")),
                input("new-name", buf, &key),
                muted("info", "The base is resolved to concrete ids before the final confirmation."),
            ],
            "Enter plan (read-only) · Esc back".into(),
        ),
        WStep::Planning { .. } => (
            vec![heading("title", format!("{repo_name} — 2/5 checkout")), busy("Planning checkout (read-only)…")],
            "Esc back".into(),
        ),
        WStep::Listing { .. } => (
            vec![heading("title", format!("{repo_name} — 2/5 existing checkout")), busy(format!("Listing checkouts of {repo_name}…"))],
            "Esc back".into(),
        ),
        WStep::Existing { selected } => (
            vec![
                heading("title", format!("{repo_name} — 2/5 existing checkout")),
                muted(
                    "info",
                    "Prunable or unavailable entries cannot be selected. The selected checkout is re-validated before use.",
                ),
                choice_list(&existing_items(w), *selected),
            ],
            "j/k move · Enter choose · Esc back".into(),
        ),
        WStep::ExistingPath { checkout, buf } => (
            vec![
                heading("title", format!("Path of {} (absolute)", single_line(&checkout.name))),
                input("existing-path", buf, "/path/to/workspace"),
            ],
            "Enter plan (read-only) · Esc back".into(),
        ),
        WStep::RecoveryAck { message, path, .. } => (
            vec![
                heading("title", "Recovery acknowledgement"),
                lines(
                    "text",
                    &[
                        message.clone(),
                        String::new(),
                        format!("Path: {path}"),
                        String::new(),
                        "I inspected this checkout manually and want to open it anyway. It stays unproven (not marked Ready)."
                            .into(),
                    ],
                ),
                buttons(&[("Open anyway", "enter"), ("Back", "escape")]),
            ],
            "Enter acknowledge · Esc back".into(),
        ),
        WStep::Action { selected } => {
            let destination = w.plan.as_ref().map(|p| p.destination().to_owned()).unwrap_or_default();
            (
                vec![
                    heading("title", format!("{repo_name} — 3/5 action")),
                    muted("info", format!("Destination: {destination}")),
                    choice_list(&action_items(), *selected),
                ],
                "j/k move · Enter choose · Esc back".into(),
            )
        }
        WStep::Snapshotting { .. } => (
            vec![heading("title", format!("{key} — 4/5 snapshot")), busy(format!("Fetching fresh snapshot of {key}…"))],
            "Esc back".into(),
        ),
        WStep::Preview => {
            let mut body = vec![heading("title", format!("{key} — 4/5 snapshot preview"))];
            if let Some(snapshot) = &w.snapshot {
                body.push(lines("text", &snapshot_preview_lines(snapshot, &state.config.omp.profile)));
            }
            body.push(buttons(&[("Continue", "enter"), ("Back", "escape")]));
            (body, "Enter continue · Esc back".into())
        }
        WStep::PartialAccept => {
            let reasons: Vec<String> = w
                .snapshot
                .iter()
                .flat_map(|s| s.partial_reasons.iter())
                .map(|r| format!("- {}", single_line(r)))
                .collect();
            let mut text = vec!["The snapshot is incomplete:".to_owned()];
            text.extend(reasons);
            text.push(String::new());
            text.push("Continue with this incomplete context?".into());
            (
                vec![
                    heading("title", "Incomplete Jira snapshot"),
                    toned("text", text.join("\n"), Level::Warning),
                    buttons(&[("Continue", "enter"), ("Back", "escape")]),
                ],
                "Enter/y continue · Esc/n back".into(),
            )
        }
        WStep::Instruction => (
            vec![
                heading("title", "Task instruction for the new omp session (separate from the Jira data)"),
                editor("instruction", &w.instruction, "Task instruction"),
            ],
            "Ctrl+Enter (or Ctrl+S) continue · Enter newline · Esc back".into(),
        ),
        WStep::Confirm => {
            let mut body = vec![heading("title", format!("{key} — 5/5 confirm"))];
            if let Some(plan) = &w.plan {
                body.push(lines("text", &final_confirm_lines(state, w, plan)));
            }
            body.push(buttons(&[("Confirm", "enter"), ("Back", "escape")]));
            (body, "Enter confirm · Esc back".into())
        }
        WStep::Executing { .. } => (
            vec![
                heading("title", format!("Checkout for {key}")),
                busy("Preparing checkout, writing the handoff and launching… (cannot be cancelled)"),
            ],
            "Waiting…".into(),
        ),
        WStep::Outcome(outcome) => outcome_screen(outcome),
        WStep::RelaunchConfirm(_) => (
            vec![
                heading("title", "Re-launch omp?"),
                toned(
                    "text",
                    "Did you inspect your existing tabs and splits and confirm no omp session for this checkout was started? The same checkout and handoff file will be reused.",
                    Level::Warning,
                ),
                buttons(&[("Re-launch", "enter"), ("Back", "escape")]),
            ],
            "Enter re-launch · Esc back".into(),
        ),
        WStep::Message { title, lines: text, .. } => (
            vec![heading("title", title.clone()), lines("text", text), buttons(&[("Continue", "enter")])],
            "Enter/Esc continue".into(),
        ),
    };
    Screen::new(with_message(body, &w.message), help)
}

fn outcome_screen(outcome: &Outcome) -> (Vec<N>, String) {
    let mut actions: Vec<(&str, &'static str)> = vec![("Close", "enter")];
    let mut help = "Enter/Esc close".to_owned();
    if outcome.retry.is_some() {
        actions.push(("Retry", "r"));
        help.push_str(" · r retry");
    }
    if outcome.relaunch.is_some() {
        actions.push(("Re-launch…", "l"));
        help.push_str(" · l re-launch (after inspecting tabs/splits)");
    }
    (
        vec![
            toned("title", outcome.title.clone(), outcome.level),
            lines("text", &outcome.lines),
            buttons(&actions),
        ],
        help,
    )
}

fn snapshot_preview_lines(snapshot: &Snapshot, profile: &str) -> Vec<String> {
    let mut out = Vec::new();
    if snapshot.partial_reasons.is_empty() {
        out.push("Snapshot complete.".to_owned());
    } else {
        out.push("⚠ INCOMPLETE SNAPSHOT — explicit confirmation will be required:".to_owned());
        out.extend(
            snapshot
                .partial_reasons
                .iter()
                .map(|r| format!("  - {}", single_line(r))),
        );
    }
    out.push(format!(
        "Captured {} for {}.",
        format_timestamp(&snapshot.captured_at, false),
        single_line(&snapshot.issue_key)
    ));
    out.push(
        "After final confirmation this snapshot and your task instruction are written to a private handoff file (0600) that stays on disk and may also be stored in the new omp session."
            .into(),
    );
    out.push(format!(
        "Jira data will be sent to the model provider of omp profile \"{profile}\"."
    ));
    out.push(String::new());
    out.push("──────── snapshot ────────".into());
    out.push(snapshot.text.clone());
    out
}

fn final_confirm_lines(state: &State, w: &Wizard, plan: &CheckoutPlan) -> Vec<String> {
    let repository = plan.repository();
    let mut out = vec![
        format!("Issue:        {}", single_line(&w.key)),
        format!(
            "Repository:   {}  ({})",
            single_line(&repository.name),
            repository.root
        ),
        format!("Backend:      {}", repository.backend.label()),
        format!("Identity:     {}", repository.key),
    ];
    match plan.selection() {
        CheckoutSelection::New { name, .. } => {
            out.push(format!("Checkout:     NEW \"{name}\""));
            match plan.base_ids() {
                [] => out.push("Base id:      (none)".into()),
                [one] => out.push(format!("Base id:      {one}")),
                ids => {
                    out.push("Base ids:".into());
                    out.extend(ids.iter().map(|id| format!("  {id}")));
                }
            }
        }
        CheckoutSelection::Existing {
            checkout,
            recovery_acknowledged,
        } => {
            let ack = if *recovery_acknowledged {
                " (recovery acknowledged)"
            } else {
                ""
            };
            out.push(format!("Checkout:     EXISTING \"{}\"{ack}", checkout.name));
            out.push("Base ids:     (existing checkout — no new base)".into());
        }
    }
    out.push(format!("Destination:  {}", plan.destination()));
    out.push(format!("Action:       {}", w.action.label()));
    if let Some(placement) = w.action.placement() {
        let profile = &state.config.omp.profile;
        out.push(format!("omp profile:  {profile}"));
        if let Some(snapshot) = &w.snapshot {
            out.push(if snapshot.partial_reasons.is_empty() {
                "Snapshot:     complete".into()
            } else {
                format!(
                    "Snapshot:     PARTIAL — {}",
                    snapshot.partial_reasons.join("; ")
                )
            });
        }
        let handoff = state.handoff_dir.join("<id>.md");
        let omp = launch::omp_argv(&state.config.omp, plan.destination(), &handoff);
        let mut argv = vec!["tern".to_owned()];
        argv.extend(launch::tern_argv(placement, plan.destination(), &omp));
        out.push(String::new());
        out.push("Launch (the handoff file id is assigned when it is written):".into());
        out.push(format!("  {}", launch::posix_quote(&argv)));
        out.push(String::new());
        out.push(format!(
            "Jira issue data (description and comments) will be sent to the model provider of omp profile \"{profile}\"."
        ));
        out.push(
            "The handoff file stays on private disk and may also be stored in the omp session."
                .into(),
        );
        out.push(String::new());
        out.push("Task instruction:".into());
        out.push(w.instruction.text.clone());
    }
    out.push(String::new());
    out.push("Nothing has been created yet. Enter confirms; Esc goes back.".into());
    out
}
