# jira-tern

[English](README.md) | [한국어](README.ko.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

用 Rust 编写的 [Tern](https://docs.stencil.so/tern/) 应用：浏览 Jira issue（搜索 / JQL / 过滤器，描述 + 评论），撰写评论，为 issue 创建或复用 **Git worktree** 或 **原生 jj workspace**，在该检出中附带 Jira 快照启动**新的 omp 进程**，并执行**显式的** Jira 状态流转。界面为 Tern 原生（Tern Surface Protocol）。仅支持 Tern，macOS/Linux。

## 环境要求

- Tern
- Rust 工具链 ≥ 1.89（`cargo`），首次启动时用于构建应用
- Git
- jj ≥ 0.46（可选，仅 jj 仓库需要）
- omp（用于移交给代理）

## 安装

```sh
tern plugin install github.com/hanbong5938/jira-tern
# 或在克隆目录中：
tern plugin link .
```

插件添加两个命令面板命令：**Jira: open (tab)** 和 **Jira: open (split right)**。两者都在当前聚焦 pane 的目录（未知时为主目录）中运行 `scripts/jira-tern.sh`。当二进制不存在或比源码旧时，脚本会用 `cargo build --release --locked` 构建到 `<plugin>/target`，然后运行它。

## 配置

配置从 `$JIRA_TERN_CONFIG`（绝对路径）或 `${XDG_CONFIG_HOME:-~/.config}/jira-tern/config.json` 读取。没有项目级配置，也没有环境变量凭据回退。请从 [`config.example.json`](./config.example.json) 开始。

| 键 | 含义 |
| --- | --- |
| `jira.baseUrl` | 站点 URL；必须为 HTTPS（本地 fixture 可用 loopback HTTP）。保留上下文路径。 |
| `jira.deployment` | `cloud`（REST v3，token 分页）或 `dc`（REST v2，offset 分页）。无自动回退。 |
| `jira.auth` / `jira.email` | `basic`（邮箱 + API token）或 `bearer`（PAT）。 |
| `jira.tokenCommand` | 不经 shell 执行的 argv；stdout（去掉一个结尾 LF）即 token。不会被记录或写入。 |
| `jira.maxResults` / `jira.timeoutMs` | 每次查询的总结果上限（1–1000）；每个请求的超时（1000–120000 ms）。 |
| `filters` | `f` 使用的命名 JQL 过滤器；第一个为初始列表。 |
| `search.jql` | `/` 的模板；必须恰好包含一次 `{query}`。 |
| `worktree.repos` | Jira 项目键 → 仓库路径（向导中的默认仓库）。 |
| `worktree.prefer` | 仓库同时有 `.jj` 和 `.git` 时选 `jj` 或 `git`。 |
| `worktree.root` | 新检出的创建位置（默认 `~/.local/share/jira-tern/checkouts`）。 |
| `worktree.gitBase` / `worktree.jjBase` | 新检出的基准 commit-ish / revset（默认 `HEAD` / `parents(@)`）。 |
| `handoff.*` | 快照的评论数量与字符数上限。 |
| `omp.binary` / `omp.profile` | 子 omp 可执行文件与 `--profile`。 |

macOS 钥匙串 token 命令示例：`["security", "find-generic-password", "-s", "jira-api-token", "-a", "<you>", "-w"]`。

## 使用

| 键 | 操作 |
| --- | --- |
| `j`/`k`、方向键 | 移动 / 滚动 |
| `Enter` | issue 详情（重新获取 issue + 评论；`Tab` 切换 pane，`PgUp`/`PgDn`） |
| `/` | 通过 `search.jql` 文本搜索 |
| `J` | 原始 JQL |
| `f` | 命名过滤器 |
| `r` | 重新获取 |
| `c` | 撰写评论 |
| `w` | 检出向导（默认：仅检出） |
| `d` | 预选 omp 启动的检出向导（split） |
| `s` | 状态流转 |
| `Esc` | 返回 / 关闭 |

### 评论

`c` 打开该 issue 的评论编辑器。`Ctrl+Enter` 在确认后发送。评论只提交一次，绝不重试：

- **2xx**：已创建；重新加载评论。
- **4xx**：被拒绝；显示服务器消息并保留你的文本。
- **5xx、超时或连接中断**：未知。评论可能已保存；再次提交前请先在 Jira 中检查该 issue。

### 检出向导

仓库 → 新名称或已有检出 → 操作（仅检出 / omp split / omp tab）→ 快照预览 + 任务指令编辑器 → 唯一一次最终确认。最终确认之前不会创建、写入、提交或启动任何东西。

- 新的 Git 检出在确认前解析出的基准提交上执行一次 `git worktree add -b <name> -- <dest> <commit>`。新的 jj 检出使用确认时显示的父提交 id 执行 `jj workspace add --no-colocate --name <name> -r <parent>… -- <dest>`。
- 持久回执（`${XDG_STATE_HOME:-~/.local/state}/jira-tern/receipts/`）在原生命令之前写为 Pending，仅在结果验证后写为 Ready。Pending/Failed 回执不会自动重试：请检查路径，并在确认恢复后选择 **Existing**。
- 绝不执行 reset、stash、force、fetch、remove 或 forget。终端启动失败不会回滚检出。
- 锁位于 `${XDG_STATE_HOME:-~/.local/state}/jira-tern/locks/`。崩溃进程遗留的锁只会被报告，绝不会被抢占；确认后请手动删除。

### omp 移交

快照（有上限的 issue 字段、描述、评论、部分结果原因）和任务指令写入 `${XDG_STATE_HOME:-~/.local/state}/jira-tern/handoffs/<id>.md`（文件 0600，目录 0700）。启动后文件会保留。子进程通过 `tern new tab` 或 `tern split`（聚焦 pane 的右侧）打开，并运行：

```
omp --cwd <checkout> --profile <profile> @<handoff.md> -- <fixed message>
```

如果无法启动 `tern` CLI，会显示命令和 cwd 供你手动运行。“已请求启动”并不表示代理已开始运行。启动结果不确定时，重新启动是单独的操作，会先要求你检查已有的 pane。

移交中的 Jira 数据会发送给所选 omp profile 的模型提供方。

### 状态流转

`s` 列出该 issue 可用的流转。需要界面或必填字段的流转不会提交，而是显示 Jira 链接以便在网页上完成。最终确认后，在锁内重新读取 issue 和流转。若有任何变化，则作废此次批准。否则保存 Pending 记录，并恰好提交一次 `{transition:{id}}`。结果如下：

- **2xx**：已接受。通过新的 GET 检查状态。
- **4xx**：被拒绝。
- **5xx、超时或连接中断**：未知。在你确认已检查 Jira 历史之前，该 issue 的下一次流转将被阻止。

Jira 没有条件流转 API，因此无法完全排除在重新读取与 POST 之间与其他 Jira 用户发生竞争的可能。

## 开发

```sh
cargo test     # 真实的 git/jj 临时仓库 + loopback Jira 服务器
cargo clippy
```

## 许可证

MIT
