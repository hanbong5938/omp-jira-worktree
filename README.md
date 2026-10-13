# jira-tern

[English](README.md) | [한국어](README.ko.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

A [Tern](https://docs.stencil.so/tern/) app written in Rust: browse Jira issues (search / JQL / filters, description + comments), write comments, create or reuse a **Git worktree** or **native jj workspace** for an issue, start a **new omp process** in that checkout with a Jira snapshot attached, and run **explicit** Jira status transitions. The UI is native Tern (Tern Surface Protocol). Tern only; macOS/Linux.

## Requirements

- Tern
- Rust toolchain ≥ 1.89 (`cargo`), needed at first launch to build the app
- Git
- jj ≥ 0.46 (optional; only for jj repositories)
- omp (for agent handoff)

## Install

```sh
tern plugin install github.com/hanbong5938/jira-tern
# or, from a clone:
tern plugin link .
```

The plugin adds two palette commands, **Jira: open (tab)** and **Jira: open (split right)**. Both run `scripts/jira-tern.sh` in the focused pane's directory (home when unknown). The script builds the app with `cargo build --release --locked` into `<plugin>/target` when the binary is missing or older than the sources, then runs it.

## Configure

The config is read from `$JIRA_TERN_CONFIG` (absolute path) or `${XDG_CONFIG_HOME:-~/.config}/jira-tern/config.json`. There is no project-level config and no env-var credential fallback. Start from [`config.example.json`](./config.example.json).

| Key | Meaning |
| --- | --- |
| `jira.baseUrl` | Site URL; HTTPS required (loopback HTTP allowed for local fixtures). Context path kept. |
| `jira.deployment` | `cloud` (REST v3, token paging) or `dc` (REST v2, offset paging). No automatic fallback. |
| `jira.auth` / `jira.email` | `basic` (email + API token) or `bearer` (PAT). |
| `jira.tokenCommand` | argv executed without a shell; stdout (one trailing LF stripped) is the token. Never logged or written. |
| `jira.maxResults` / `jira.timeoutMs` | Total result cap per query (1–1000); per-request timeout (1000–120000 ms). |
| `filters` | Named JQL filters for `f`; the first is the initial list. |
| `search.jql` | Template for `/`; must contain `{query}` exactly once. |
| `worktree.repos` | Jira project key → repo path (default repo choice in the wizard). |
| `worktree.prefer` | `jj` or `git` when a repo has both `.jj` and `.git`. |
| `worktree.root` | Where new checkouts are created (default `~/.local/share/jira-tern/checkouts`). |
| `worktree.gitBase` / `worktree.jjBase` | Base commit-ish / revset for new checkouts (default `HEAD` / `parents(@)`). |
| `handoff.*` | Comment count and character caps for the snapshot. |
| `omp.binary` / `omp.profile` | Child omp executable and `--profile`. |

macOS Keychain example token command: `["security", "find-generic-password", "-s", "jira-api-token", "-a", "<you>", "-w"]`.

## Use

| Key | Action |
| --- | --- |
| `j`/`k`, arrows | Move / scroll |
| `Enter` | Issue detail (fresh issue + comments; `Tab` switches pane, `PgUp`/`PgDn`) |
| `/` | Text search via `search.jql` |
| `J` | Raw JQL |
| `f` | Named filter |
| `r` | Refetch |
| `c` | Write a comment |
| `w` | Checkout wizard (default: checkout only) |
| `d` | Checkout wizard with omp launch preselected (split) |
| `s` | Status transition |
| `Esc` | Back / close |

### Comments

`c` opens a comment editor for the issue. `Ctrl+Enter` sends after a confirmation. The comment is posted exactly once, never retried:

- **2xx**: created; the comments are reloaded.
- **4xx**: rejected; the server message is shown and your text is kept.
- **5xx, timeout, or dropped connection**: unknown. The comment may have been saved; check the issue in Jira before posting it again.

### Checkout wizard

Repository → New name or Existing checkout → action (checkout only / omp split / omp tab) → snapshot preview + task-instruction editor → single final confirmation. Nothing is created, written, posted, or launched before the final confirmation.

- New Git checkouts run `git worktree add -b <name> -- <dest> <commit>` once, at a base commit resolved before confirmation. New jj checkouts run `jj workspace add --no-colocate --name <name> -r <parent>… -- <dest>` with the parent commit ids shown at confirmation.
- A durable receipt (`${XDG_STATE_HOME:-~/.local/state}/jira-tern/receipts/`) is written as Pending before the native command and as Ready only after the result is verified. A Pending/Failed receipt is never retried automatically: inspect the path and choose **Existing** with the recovery acknowledgement.
- Nothing is ever reset, stashed, forced, fetched, removed, or forgotten. A failed terminal launch does not roll back the checkout.
- Locks live under `${XDG_STATE_HOME:-~/.local/state}/jira-tern/locks/`. A lock left by a crashed process is reported, never stolen; remove it manually after checking.

### omp handoff

The snapshot (bounded issue fields, description, comments, partial reasons) and your task instruction are written to `${XDG_STATE_HOME:-~/.local/state}/jira-tern/handoffs/<id>.md` (file 0600, dir 0700). The file is kept after launch. The child is opened with `tern new tab` or `tern split` (right of the focused pane) and runs:

```
omp --cwd <checkout> --profile <profile> @<handoff.md> -- <fixed message>
```

If the `tern` CLI cannot be started, the command and cwd are shown for you to run manually. "Launch requested" does not mean the agent has started. After an uncertain launch result, re-launch is a separate action that asks you to check existing panes first.

Jira data in the handoff is sent to the model provider of the selected omp profile.

### Status transitions

`s` lists the transitions available for the issue. A transition that needs a screen or required fields is not posted; the Jira link is shown so you can do it on the web. After the final confirmation, the issue and transitions are re-read under a lock. If anything changed, the approval is discarded. Otherwise a Pending record is saved and `{transition:{id}}` is posted exactly once. The results are:

- **2xx**: acknowledged. A fresh GET checks the status.
- **4xx**: rejected.
- **5xx, timeout, or dropped connection**: unknown. The next transition for that issue is blocked until you confirm you checked the Jira history.

Jira has no conditional transition API, so a race with another Jira user between the re-read and the POST cannot be fully excluded.

## Development

```sh
cargo test     # real git/jj temp repos + loopback Jira servers
cargo clippy
```

## License

MIT
