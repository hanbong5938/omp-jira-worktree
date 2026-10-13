# omp-jira-worktree

[English](README.md) | [한국어](README.ko.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

omp extension: browse Jira issues (search / JQL / filters, description + comments), create or reuse a **Git worktree** or **native jj workspace** for an issue, start a **new omp process** in that checkout with a Jira snapshot attached, and run **explicit** Jira status transitions.

Verified with omp 18.10.3, Bun 1.4.2, Git 2.54.0, jj 0.46.0 (jj ≥ 0.46.0 required for jj; Git and browsing work without jj). macOS/Linux only.

## Install

```sh
git clone https://github.com/hanbong5938/omp-jira-worktree.git
cd omp-jira-worktree && bun install
omp plugin link "$PWD"   # or: omp --extension "$PWD"
```

## Configure

The config is read from `$OMP_JIRA_WORKTREE_CONFIG` (absolute path) or `${XDG_CONFIG_HOME:-~/.config}/omp-jira-worktree/config.json`. There is no project-level config and no env-var credential fallback. Start from [`config.example.json`](./config.example.json).

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
| `worktree.root` | Where new checkouts are created. |
| `worktree.gitBase` / `worktree.jjBase` | Base commit-ish / revset for new checkouts (default `HEAD` / `parents(@)`). |
| `handoff.*` | Comment count and character caps for the snapshot. |
| `omp.binary` / `omp.profile` | Child omp executable and `--profile`. The child does not inherit the parent profile implicitly. |

macOS Keychain example token command: `["security", "find-generic-password", "-s", "jira-api-token", "-a", "<you>", "-w"]`.

## Use

`/jira` opens the browser; `/jira ABC-123` opens that issue. Interactive TUI only.

| Key | Action |
| --- | --- |
| `j`/`k`, arrows | Move / scroll |
| `Enter` | Issue detail (fresh issue + comments; `Tab` switches pane, `PgUp`/`PgDn`) |
| `/` | Text search via `search.jql` |
| `J` | Raw JQL |
| `f` | Named filter |
| `r` | Refetch |
| `w` | Checkout wizard (default: checkout only) |
| `d` | Checkout wizard with omp launch preselected (pane) |
| `s` | Status transition |
| `Esc` | Back / close |

### Checkout wizard

Repository → New name or Existing checkout → action (checkout only / omp pane / omp tab) → snapshot preview + task-instruction editor → single final confirmation. Nothing is created, written, posted, or launched before the final confirmation.

- New Git checkouts run `git worktree add -b <name> -- <dest> <commit>` once, at a base commit resolved before confirmation. New jj checkouts run `jj workspace add --no-colocate --name <name> -r <parent>… -- <dest>` with the parent commit ids shown at confirmation.
- A durable receipt (`$XDG_STATE_HOME/omp-jira-worktree/receipts/`) is written as Pending before the native command and as Ready only after the result is verified. A Pending/Failed receipt is never retried automatically: inspect the path and choose **Existing** with the recovery acknowledgement.
- Nothing is ever reset, stashed, forced, fetched, removed, or forgotten. A failed terminal launch does not roll back the checkout.
- Locks live under `$XDG_STATE_HOME/omp-jira-worktree/locks/`. A lock left by a crashed process is reported, never stolen; remove it manually after checking.

### omp handoff

The snapshot (bounded issue fields, description, comments, partial reasons) and your task instruction are written to `$XDG_STATE_HOME/omp-jira-worktree/handoffs/<uuid>.md` (file 0600, dir 0700). The file is kept after launch and may also be stored in the child omp session. The child is started as:

```
omp --cwd <checkout> --profile <profile> --extension <this extension.ts> @<handoff.md> -- <fixed message>
```

through the host terminal launcher (tmux, Zellij, Herdr, CMUX, Orca). Herdr/CMUX/Orca ask you to confirm the target shell is POSIX. Without a supported multiplexer, or when you decline, the command and cwd are shown for you to run manually; nothing is typed into the composer. "Terminal launch requested" does not mean the agent has started. After an uncertain launch result, re-launch is a separate action that asks you to check existing panes first.

Jira data in the handoff is sent to the model provider of the selected omp profile.

### Status transitions

`s` lists the transitions available for the issue. A transition that needs a screen or required fields is not posted; the Jira link is shown so you can do it on the web. After the final confirmation, the issue and transitions are re-read under a lock. If anything changed, the approval is discarded. Otherwise a Pending record is saved and `{transition:{id}}` is posted exactly once. The results are:

- **2xx**: acknowledged. A fresh GET checks the status.
- **4xx**: rejected.
- **5xx, timeout, or dropped connection**: unknown. The next transition for that issue is blocked until you confirm you checked the Jira history.

Jira has no conditional transition API, so a race with another Jira user between the re-read and the POST cannot be fully excluded.

## Development

```sh
bun run check   # tsc --noEmit
bun test        # real git/jj temp repos + loopback Jira servers
bun run smoke   # end-to-end with the real modules against a loopback Jira fixture
bun scripts/smoke.ts --serve   # keep the fixture running for manual TUI testing
```
## License

MIT
