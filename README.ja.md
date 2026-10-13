# omp-jira-worktree

[English](README.md) | [한국어](README.ko.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

omp 拡張機能：Jira 課題の閲覧（検索 / JQL / フィルター、説明 + コメント）、課題用の **Git worktree** または **ネイティブ jj workspace** の作成・再利用、Jira スナップショットを添付したそのチェックアウトでの **新しい omp プロセス** の起動、そして **明示的な** Jira ステータス遷移を行います。

omp 18.10.3、Bun 1.4.2、Git 2.54.0、jj 0.46.0 で検証済みです（jj を使うには jj ≥ 0.46.0 が必要。Git と閲覧機能は jj なしで動作します）。macOS/Linux のみ対応。

## インストール

```sh
git clone https://github.com/hanbong5938/omp-jira-worktree.git
cd omp-jira-worktree && bun install
omp plugin link "$PWD"   # or: omp --extension "$PWD"
```

## 設定

設定は `$OMP_JIRA_WORKTREE_CONFIG`（絶対パス）または `${XDG_CONFIG_HOME:-~/.config}/omp-jira-worktree/config.json` から読み込まれます。プロジェクト単位の設定や、環境変数による認証情報のフォールバックはありません。[`config.example.json`](./config.example.json) を元に作成してください。

| キー | 意味 |
| --- | --- |
| `jira.baseUrl` | サイト URL。HTTPS 必須（ローカルフィクスチャ用にループバック HTTP は許可）。コンテキストパスは保持されます。 |
| `jira.deployment` | `cloud`（REST v3、トークンページング）または `dc`（REST v2、オフセットページング）。自動フォールバックはありません。 |
| `jira.auth` / `jira.email` | `basic`（メール + API トークン）または `bearer`（PAT）。 |
| `jira.tokenCommand` | シェルを介さずに実行される argv。標準出力（末尾の LF を 1 つ除去）がトークンになります。ログ出力や書き込みは一切されません。 |
| `jira.maxResults` / `jira.timeoutMs` | クエリごとの合計結果上限（1–1000）、リクエストごとのタイムアウト（1000–120000 ms）。 |
| `filters` | `f` 用の名前付き JQL フィルター。最初のものが初期リストになります。 |
| `search.jql` | `/` 用のテンプレート。`{query}` をちょうど 1 回含む必要があります。 |
| `worktree.repos` | Jira プロジェクトキー → リポジトリパス（ウィザードでのデフォルトのリポジトリ選択）。 |
| `worktree.prefer` | リポジトリに `.jj` と `.git` の両方がある場合の `jj` または `git`。 |
| `worktree.root` | 新しいチェックアウトを作成する場所。 |
| `worktree.gitBase` / `worktree.jjBase` | 新しいチェックアウトのベース commit-ish / revset（デフォルトは `HEAD` / `parents(@)`）。 |
| `handoff.*` | スナップショットのコメント数および文字数の上限。 |
| `omp.binary` / `omp.profile` | 子 omp の実行ファイルと `--profile`。子は親のプロファイルを暗黙的に継承しません。 |

macOS キーチェーンのトークンコマンド例：`["security", "find-generic-password", "-s", "jira-api-token", "-a", "<you>", "-w"]`。

## 使い方

`/jira` でブラウザを開き、`/jira ABC-123` でその課題を開きます。対話型 TUI のみ対応。

| キー | 操作 |
| --- | --- |
| `j`/`k`, arrows | 移動 / スクロール |
| `Enter` | 課題の詳細（最新の課題 + コメント。`Tab` でペイン切り替え、`PgUp`/`PgDn`） |
| `/` | `search.jql` によるテキスト検索 |
| `J` | 生の JQL |
| `f` | 名前付きフィルター |
| `r` | 再取得 |
| `w` | チェックアウトウィザード（デフォルト：チェックアウトのみ） |
| `d` | omp 起動（ペイン）を事前選択したチェックアウトウィザード |
| `s` | ステータス遷移 |
| `Esc` | 戻る / 閉じる |

### チェックアウトウィザード

リポジトリ → 新しい名前または既存のチェックアウト → アクション（チェックアウトのみ / omp ペイン / omp タブ）→ スナップショットのプレビュー + タスク指示エディター → 最終確認（1 回のみ）。最終確認の前には、何も作成・書き込み・投稿・起動されません。

- 新しい Git チェックアウトでは、確認前に解決されたベースコミットで `git worktree add -b <name> -- <dest> <commit>` を 1 回実行します。新しい jj チェックアウトでは、確認時に表示された親コミット ID を使って `jj workspace add --no-colocate --name <name> -r <parent>… -- <dest>` を実行します。
- 永続的なレシート（`$XDG_STATE_HOME/omp-jira-worktree/receipts/`）は、ネイティブコマンドの前に Pending として書き込まれ、結果が検証された後にのみ Ready になります。Pending/Failed のレシートが自動的に再試行されることはありません。パスを確認し、復旧の確認とともに **Existing** を選択してください。
- reset、stash、force、fetch、remove、forget は一切行いません。ターミナルの起動に失敗してもチェックアウトはロールバックされません。
- ロックは `$XDG_STATE_HOME/omp-jira-worktree/locks/` 配下に置かれます。クラッシュしたプロセスが残したロックは報告されるだけで、奪取されることはありません。確認したうえで手動で削除してください。

### omp ハンドオフ

スナップショット（上限付きの課題フィールド、説明、コメント、部分取得の理由）とタスク指示は `$XDG_STATE_HOME/omp-jira-worktree/handoffs/<uuid>.md`（ファイル 0600、ディレクトリ 0700）に書き込まれます。このファイルは起動後も保持され、子 omp セッションにも保存される場合があります。子は次のように起動されます：

```
omp --cwd <checkout> --profile <profile> --extension <this extension.ts> @<handoff.md> -- <fixed message>
```

起動はホストのターミナルランチャー（tmux、Zellij、Herdr、CMUX、Orca）を通じて行われます。Herdr/CMUX/Orca では、対象シェルが POSIX であることの確認を求められます。対応するマルチプレクサーがない場合や確認を拒否した場合は、手動で実行できるようコマンドと cwd が表示されます。コンポーザーには何も入力されません。「Terminal launch requested」はエージェントが起動したことを意味しません。起動結果が不確かな場合、再起動は別のアクションとなり、まず既存のペインを確認するよう求められます。

ハンドオフ内の Jira データは、選択した omp プロファイルのモデルプロバイダーに送信されます。

### ステータス遷移

`s` は課題で利用可能な遷移を一覧表示します。画面や必須フィールドを必要とする遷移は投稿されず、Web 上で操作できるよう Jira のリンクが表示されます。最終確認の後、ロックを取得した状態で課題と遷移が再読み込みされます。何か変更があれば承認は破棄されます。変更がなければ Pending レコードが保存され、`{transition:{id}}` がちょうど 1 回投稿されます。結果は次のとおりです：

- **2xx**：受理。新たな GET でステータスを確認します。
- **4xx**：拒否。
- **5xx、タイムアウト、または接続切断**：不明。Jira の履歴を確認したことを確認するまで、その課題の次の遷移はブロックされます。

Jira には条件付き遷移 API がないため、再読み込みと POST の間に他の Jira ユーザーとの競合が発生する可能性を完全には排除できません。

## 開発

```sh
bun run check   # tsc --noEmit
bun test        # real git/jj temp repos + loopback Jira servers
bun run smoke   # end-to-end with the real modules against a loopback Jira fixture
bun scripts/smoke.ts --serve   # keep the fixture running for manual TUI testing
```

## ライセンス

MIT
