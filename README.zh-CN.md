# omp-jira-worktree

[English](README.md) | [한국어](README.ko.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

omp 扩展：浏览 Jira issue（搜索 / JQL / 过滤器，描述 + 评论），为 issue 创建或复用 **Git worktree** 或 **原生 jj workspace**，在该检出中附带 Jira 快照启动**新的 omp 进程**，并执行**显式的** Jira 状态流转。

已在 omp 18.10.3、Bun 1.4.2、Git 2.54.0、jj 0.46.0 上验证（使用 jj 需要 jj ≥ 0.46.0；Git 与浏览功能无需 jj）。仅支持 macOS/Linux。

## 安装

```sh
git clone https://github.com/hanbong5938/omp-jira-worktree.git
cd omp-jira-worktree && bun install
omp plugin link "$PWD"   # or: omp --extension "$PWD"
```

## 配置

配置从 `$OMP_JIRA_WORKTREE_CONFIG`（绝对路径）或 `${XDG_CONFIG_HOME:-~/.config}/omp-jira-worktree/config.json` 读取。不存在项目级配置，也没有基于环境变量的凭据回退。可以从 [`config.example.json`](./config.example.json) 开始。

| 键 | 含义 |
| --- | --- |
| `jira.baseUrl` | 站点 URL；必须使用 HTTPS（本地测试夹具允许回环 HTTP）。保留上下文路径。 |
| `jira.deployment` | `cloud`（REST v3，令牌分页）或 `dc`（REST v2，偏移分页）。不会自动回退。 |
| `jira.auth` / `jira.email` | `basic`（邮箱 + API 令牌）或 `bearer`（PAT）。 |
| `jira.tokenCommand` | 不经 shell 执行的 argv；其 stdout（去除末尾一个 LF）即为令牌。绝不会被记录或写入。 |
| `jira.maxResults` / `jira.timeoutMs` | 每次查询的结果总数上限（1–1000）；单次请求超时（1000–120000 ms）。 |
| `filters` | 供 `f` 使用的具名 JQL 过滤器；第一个为初始列表。 |
| `search.jql` | `/` 使用的模板；必须且只能包含一次 `{query}`。 |
| `worktree.repos` | Jira 项目键 → 仓库路径（向导中的默认仓库选择）。 |
| `worktree.prefer` | 当仓库同时含有 `.jj` 和 `.git` 时选择 `jj` 或 `git`。 |
| `worktree.root` | 新检出的创建位置。 |
| `worktree.gitBase` / `worktree.jjBase` | 新检出的基准 commit-ish / revset（默认 `HEAD` / `parents(@)`）。 |
| `handoff.*` | 快照中的评论数量与字符数上限。 |
| `omp.binary` / `omp.profile` | 子 omp 可执行文件与 `--profile`。子进程不会隐式继承父进程的 profile。 |

macOS 钥匙串令牌命令示例：`["security", "find-generic-password", "-s", "jira-api-token", "-a", "<you>", "-w"]`。

## 使用

`/jira` 打开浏览器；`/jira ABC-123` 打开该 issue。仅支持交互式 TUI。

| 按键 | 操作 |
| --- | --- |
| `j`/`k`、方向键 | 移动 / 滚动 |
| `Enter` | issue 详情（重新获取 issue + 评论；`Tab` 切换面板，`PgUp`/`PgDn`） |
| `/` | 通过 `search.jql` 进行文本搜索 |
| `J` | 原始 JQL |
| `f` | 具名过滤器 |
| `r` | 重新获取 |
| `w` | 检出向导（默认：仅检出） |
| `d` | 预选 omp 启动的检出向导（面板） |
| `s` | 状态流转 |
| `Esc` | 返回 / 关闭 |

### 检出向导

仓库 → 新名称或已有检出 → 操作（仅检出 / omp 面板 / omp 标签页）→ 快照预览 + 任务指令编辑器 → 单次最终确认。在最终确认之前，不会创建、写入、发布或启动任何东西。

- 新的 Git 检出会在确认前解析好的基准提交上执行一次 `git worktree add -b <name> -- <dest> <commit>`。新的 jj 检出执行 `jj workspace add --no-colocate --name <name> -r <parent>… -- <dest>`，父提交 id 会在确认时显示。
- 持久化回执（`$XDG_STATE_HOME/omp-jira-worktree/receipts/`）在原生命令执行前写为 Pending，仅在结果验证通过后写为 Ready。Pending/Failed 回执绝不会自动重试：请检查该路径，并在确认恢复后选择 **Existing**。
- 绝不会执行 reset、stash、强制操作、fetch、删除或 forget。终端启动失败不会回滚检出。
- 锁位于 `$XDG_STATE_HOME/omp-jira-worktree/locks/` 下。崩溃进程遗留的锁会被报告，但绝不会被抢占；确认后请手动删除。

### omp 交接

快照（有界的 issue 字段、描述、评论、部分获取的原因）和你的任务指令会写入 `$XDG_STATE_HOME/omp-jira-worktree/handoffs/<uuid>.md`（文件 0600，目录 0700）。该文件在启动后保留，也可能存储在子 omp 会话中。子进程按如下方式启动：

```
omp --cwd <checkout> --profile <profile> --extension <this extension.ts> @<handoff.md> -- <fixed message>
```

通过宿主终端启动器（tmux、Zellij、Herdr、CMUX、Orca）启动。Herdr/CMUX/Orca 会要求你确认目标 shell 是 POSIX。若没有受支持的多路复用器，或你拒绝，则会显示命令和 cwd 供你手动运行；不会向输入框中键入任何内容。“已请求终端启动”并不代表代理已开始运行。启动结果不确定时，重新启动是一个单独的操作，会先要求你检查现有面板。

交接中的 Jira 数据会发送给所选 omp profile 的模型提供商。

### 状态流转

`s` 列出该 issue 可用的流转。需要界面或必填字段的流转不会被提交；会显示 Jira 链接，便于你在网页上完成。最终确认后，会在锁保护下重新读取 issue 与流转。如有任何变化，批准将被作废。否则保存一条 Pending 记录，并恰好提交一次 `{transition:{id}}`。结果如下：

- **2xx**：已确认。通过新的 GET 检查状态。
- **4xx**：被拒绝。
- **5xx、超时或连接中断**：未知。在你确认已检查 Jira 历史之前，该 issue 的下一次流转将被阻止。

Jira 没有条件流转 API，因此无法完全排除在重新读取与 POST 之间与其他 Jira 用户发生竞争的可能。

## 开发

```sh
bun run check   # tsc --noEmit
bun test        # real git/jj temp repos + loopback Jira servers
bun run smoke   # end-to-end with the real modules against a loopback Jira fixture
bun scripts/smoke.ts --serve   # keep the fixture running for manual TUI testing
```

## 许可证

MIT
