# jira-tern

[English](README.md) | [한국어](README.ko.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

Rust で書かれた [Tern](https://docs.stencil.so/tern/) アプリ：Jira 課題の閲覧（検索 / JQL / フィルター、説明 + コメント）、コメントの作成、課題用の **Git worktree** または **ネイティブ jj workspace** の作成・再利用、Jira スナップショットを添付したそのチェックアウトでの **新しい omp プロセス** の起動、そして **明示的な** Jira ステータス遷移を行います。UI は Tern ネイティブ（Tern Surface Protocol）です。Tern 専用、macOS/Linux。

## 必要なもの

- Tern
- Rust ツールチェーン ≥ 1.89（`cargo`）、初回起動時のアプリビルドに必要
- Git
- jj ≥ 0.46（任意。jj リポジトリでのみ必要）
- omp（エージェントへのハンドオフ用）

## インストール

```sh
tern plugin install github.com/hanbong5938/jira-tern
# またはクローンしたディレクトリで:
tern plugin link .
```

プラグインはパレットコマンド **Jira: open (tab)** と **Jira: open (split right)** を追加します。どちらもフォーカス中の pane のディレクトリ（不明ならホーム）で `scripts/jira-tern.sh` を実行します。スクリプトはバイナリが無いかソースより古い場合に `cargo build --release --locked` で `<plugin>/target` にビルドしてから実行します。

## 設定

設定は `$JIRA_TERN_CONFIG`（絶対パス）または `${XDG_CONFIG_HOME:-~/.config}/jira-tern/config.json` から読み込みます。プロジェクト単位の設定や環境変数による認証情報のフォールバックはありません。[`config.example.json`](./config.example.json) から始めてください。

| キー | 意味 |
| --- | --- |
| `jira.baseUrl` | サイト URL。HTTPS 必須（ローカルフィクスチャ用の loopback HTTP は可）。コンテキストパスは保持。 |
| `jira.deployment` | `cloud`（REST v3、トークンページング）または `dc`（REST v2、オフセットページング）。自動フォールバックなし。 |
| `jira.auth` / `jira.email` | `basic`（メール + API トークン）または `bearer`（PAT）。 |
| `jira.tokenCommand` | シェルを介さず実行される argv。stdout（末尾の LF 1 つを除去）がトークン。ログやファイルには残りません。 |
| `jira.maxResults` / `jira.timeoutMs` | クエリごとの総結果上限（1–1000）、リクエストごとのタイムアウト（1000–120000 ms）。 |
| `filters` | `f` 用の名前付き JQL フィルター。最初のものが初期リスト。 |
| `search.jql` | `/` 用テンプレート。`{query}` をちょうど 1 回含む必要があります。 |
| `worktree.repos` | Jira プロジェクトキー → リポジトリパス（ウィザードの既定リポジトリ）。 |
| `worktree.prefer` | リポジトリに `.jj` と `.git` の両方がある場合の `jj` または `git`。 |
| `worktree.root` | 新しいチェックアウトの作成先（既定 `~/.local/share/jira-tern/checkouts`）。 |
| `worktree.gitBase` / `worktree.jjBase` | 新しいチェックアウトの基点 commit-ish / revset（既定 `HEAD` / `parents(@)`）。 |
| `handoff.*` | スナップショットのコメント数と文字数の上限。 |
| `omp.binary` / `omp.profile` | 子 omp の実行ファイルと `--profile`。 |

macOS キーチェーンのトークンコマンド例：`["security", "find-generic-password", "-s", "jira-api-token", "-a", "<you>", "-w"]`。

### omp-jira-worktree からのアップグレード

自動移行はありません。以下の順序で手動で切り替えてください。以下の手順はデータを移動・削除しません。

1. 旧 omp extension とすべての jira-tern インスタンスを終了し、実行中の native add/トランジション/launch がないことを確認します。2 つの namespace の lock は互いを保護しないため、両方を動かしたまま移行しないでください。
2. 旧設定: `$OMP_JIRA_WORKTREE_CONFIG` または `${XDG_CONFIG_HOME:-~/.config}/omp-jira-worktree/config.json`。新設定: `$JIRA_TERN_CONFIG` または `${XDG_CONFIG_HOME:-~/.config}/jira-tern/config.json`(`XDG_CONFIG_HOME` は絶対パスの場合のみ使用)。元ファイルは残し、新しいパスにファイルがない場合のみコピーします。新設定が既にある場合は上書きせず手動で比較してください。`JIRA_TERN_CONFIG` に絶対パスを指定しても構いません。jira-tern は `OMP_JIRA_WORKTREE_CONFIG` を**読みません**。
3. トップレベルのキーは同じです(`jira`、`filters`、`search`、`worktree`、`handoff`、`omp`)。コピーを `config.example.json` と比較し、URL・パスの誤りを修正します。`worktree.root` を省略していて旧い場所を使い続けたい場合は `"root": "~/.local/share/omp-jira-worktree/checkouts"` を明示します。省略すると新しいデフォルト `~/.local/share/jira-tern/checkouts` が使われます。明示された root はそのままです。`jira.baseUrl` は標準 URL パーサで正規化されるようになり(IDN → Punycode、デフォルトポート削除、非正規 IPv4・context path の正規化)、この正規 URL がトランジション記録の site identity です。旧記録の `site` と新しい正規 URL を比較し、異なる場合は先に手順 6 の確認を行ってください。
4. **既存の Git worktree/jj workspace を `mv`/`cp` しないでください。** native の登録と identity を保つ必要があります。`git -C <source> worktree list --porcelain` または `jj --ignore-working-copy -R <source> workspace list` と各 checkout の状態を確認し、jira-tern で **Existing** として選択します。既存の branch/workspace に対して **New** を再実行しないでください。
5. 旧 state root: `${XDG_STATE_HOME:-~/.local/state}/omp-jira-worktree`。新 root: `${XDG_STATE_HOME:-~/.local/state}/jira-tern`。旧ディレクトリと handoff ファイルは残します。state/locks/receipts/transitions を丸ごとコピー・削除しないでください。receipt の構造が似ていても state の互換性を意味しません。
6. 旧トランジション記録は `transitions/<SHA-256(JSON([site,key]))>.json`、新しい記録は `records/transitions/<独自の 128-bit digest>.json` です。旧 `pending`/`unknown` 記録ごとに `site`、`key`、`attempt`、`startedAt`、`transitionId` を読み、jira-tern からトランジションを送る**前に** Jira で課題の実際の履歴を確認してください。確認できない記録があれば切り替えを中止します。現在のステータスが一致するだけで未実行と判断したり再送したりしないでください。**警告:** jira-tern は旧記録を読まないため、重複トランジションは自動ではブロックされません。
7. 旧 pending/failed receipt は、destination のファイル、branch/workspace、working tree の状態を直接確認します。確認前に不確かな checkout を開いたり再作成したりしないでください。receipt がない場合、jira-tern は復旧の確認を**表示しないことがあり**、確認が表示されても安全が保証されるわけではありません。破損している場合は native の Git/jj ツールで修復するまで中止してください。
8. stale lock を自動で削除しないでください。生きている owner がいないことを確認せずに lock を削除しないでください。強制 reset、stash、`worktree prune`、`workspace forget` などデータを消すコマンドを切り替えに使わないでください。handoff は jira-tern で再生成し、旧 handoff は残します。

## 使い方

| キー | 動作 |
| --- | --- |
| `j`/`k`、矢印 | 移動 / スクロール |
| `Enter` | 課題詳細（課題 + コメントを再取得。`Tab` で pane 切替、`PgUp`/`PgDn`） |
| `/` | `search.jql` によるテキスト検索 |
| `J` | 生の JQL |
| `f` | 名前付きフィルター |
| `r` | 再取得 |
| `c` | コメントを書く |
| `w` | チェックアウトウィザード（既定：チェックアウトのみ） |
| `d` | omp 起動を事前選択したチェックアウトウィザード（split） |
| `s` | ステータス遷移 |
| `Esc` | 戻る / 閉じる |

### コメント

`c` で課題のコメントエディターを開きます。`Ctrl+Enter` で確認後に送信します。コメントはちょうど 1 回だけ投稿され、再試行されません：

- **2xx**：作成済み。コメントを再読み込みします。
- **4xx**：拒否。サーバーメッセージを表示し、入力したテキストは保持されます。
- **5xx、タイムアウト、接続切断**：不明。コメントが保存されている可能性があるため、再投稿する前に Jira で課題を確認してください。

### チェックアウトウィザード

リポジトリ → 新しい名前または既存のチェックアウト → アクション（チェックアウトのみ / omp split / omp tab）→ スナップショットのプレビュー + タスク指示エディター → 一度だけの最終確認。最終確認の前には何も作成・書き込み・投稿・起動されません。

- 新しい Git チェックアウトは、確認前に解決した基点コミットで `git worktree add -b <name> -- <dest> <commit>` を 1 回実行します。新しい jj チェックアウトは、確認時に表示された親コミット id で `jj workspace add --no-colocate --name <name> -r <parent>… -- <dest>` を実行します。
- 永続的なレシート（`${XDG_STATE_HOME:-~/.local/state}/jira-tern/receipts/`）はネイティブコマンドの前に Pending として、結果を検証した後にのみ Ready として書き込まれます。Pending/Failed のレシートは自動では再試行されません。パスを確認し、復旧の承認とともに **Existing** を選んでください。
- reset、stash、force、fetch、remove、forget は一切行いません。ターミナル起動に失敗してもチェックアウトはロールバックされません。
- ロックは `${XDG_STATE_HOME:-~/.local/state}/jira-tern/locks/` 以下にあります。クラッシュしたプロセスが残したロックは報告のみで奪いません。確認後に手動で削除してください。

### omp ハンドオフ

スナップショット（上限付きの課題フィールド、説明、コメント、部分取得の理由）とタスク指示は `${XDG_STATE_HOME:-~/.local/state}/jira-tern/handoffs/<id>.md`（ファイル 0600、ディレクトリ 0700）に書き込まれます。ファイルは起動後も保持されます。子は `tern new tab` または `tern split`（フォーカス中の pane の右）で開かれ、次を実行します：

```
omp --cwd <checkout> --profile <profile> @<handoff.md> -- <fixed message>
```

`tern` CLI を起動できない場合は、手動で実行できるようコマンドと cwd を表示します。「起動を要求済み」はエージェントが開始したことを意味しません。起動結果が不確かな場合、再起動は別のアクションで、先に既存の pane を確認するよう求めます。

ハンドオフ内の Jira データは、選択した omp プロファイルのモデルプロバイダーに送信されます。

### ステータス遷移

`s` は課題で利用可能な遷移を一覧表示します。画面や必須フィールドが必要な遷移は投稿せず、Web で行えるよう Jira のリンクを表示します。最終確認の後、ロック下で課題と遷移を再取得します。何か変わっていれば承認は破棄されます。そうでなければ Pending 記録を保存し、`{transition:{id}}` をちょうど 1 回投稿します。結果は次のとおりです：

- **2xx**：受理。新しい GET でステータスを確認します。
- **4xx**：拒否。
- **5xx、タイムアウト、接続切断**：不明。Jira の履歴を確認したと承認するまで、その課題の次の遷移はブロックされます。

Jira には条件付き遷移 API がないため、再取得と POST の間に他の Jira ユーザーと競合する可能性を完全には排除できません。

## 開発

```sh
cargo test     # 実際の git/jj 一時リポジトリ + loopback Jira サーバー
cargo clippy
```

## ライセンス

MIT
