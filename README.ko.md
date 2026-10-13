# omp-jira-worktree

[English](README.md) | [한국어](README.ko.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

omp 확장: Jira 이슈를 탐색하고(검색 / JQL / 필터, 설명 + 댓글), 이슈별로 **Git worktree** 또는 **네이티브 jj workspace**를 생성하거나 재사용하며, Jira 스냅샷을 첨부한 채 해당 체크아웃에서 **새 omp 프로세스**를 시작하고, **명시적인** Jira 상태 전환을 실행합니다.

omp 18.10.3, Bun 1.4.2, Git 2.54.0, jj 0.46.0에서 검증했습니다(jj 사용 시 jj ≥ 0.46.0 필요. Git과 탐색 기능은 jj 없이도 동작합니다). macOS/Linux 전용입니다.

## 설치

```sh
git clone https://github.com/hanbong5938/omp-jira-worktree.git
cd omp-jira-worktree && bun install
omp plugin link "$PWD"   # or: omp --extension "$PWD"
```

## 설정

설정은 `$OMP_JIRA_WORKTREE_CONFIG`(절대 경로) 또는 `${XDG_CONFIG_HOME:-~/.config}/omp-jira-worktree/config.json`에서 읽습니다. 프로젝트 단위 설정은 없으며, 환경 변수로 자격 증명을 대신하는 폴백도 없습니다. [`config.example.json`](./config.example.json)을 바탕으로 시작하세요.

| 키 | 의미 |
| --- | --- |
| `jira.baseUrl` | 사이트 URL. HTTPS 필수(로컬 픽스처용 루프백 HTTP는 허용). 컨텍스트 경로는 유지됩니다. |
| `jira.deployment` | `cloud`(REST v3, 토큰 페이징) 또는 `dc`(REST v2, 오프셋 페이징). 자동 폴백은 없습니다. |
| `jira.auth` / `jira.email` | `basic`(이메일 + API 토큰) 또는 `bearer`(PAT). |
| `jira.tokenCommand` | 셸 없이 실행되는 argv. stdout(끝의 LF 하나 제거)이 토큰이 됩니다. 로그에 남기거나 파일에 기록하지 않습니다. |
| `jira.maxResults` / `jira.timeoutMs` | 쿼리당 전체 결과 상한(1–1000), 요청당 타임아웃(1000–120000 ms). |
| `filters` | `f`에서 쓰는 이름 있는 JQL 필터. 첫 번째 항목이 초기 목록입니다. |
| `search.jql` | `/`용 템플릿. `{query}`를 정확히 한 번 포함해야 합니다. |
| `worktree.repos` | Jira 프로젝트 키 → 저장소 경로(위저드의 기본 저장소 선택). |
| `worktree.prefer` | 저장소에 `.jj`와 `.git`이 모두 있을 때 `jj` 또는 `git`. |
| `worktree.root` | 새 체크아웃이 생성될 위치. |
| `worktree.gitBase` / `worktree.jjBase` | 새 체크아웃의 기준 commit-ish / revset(기본값 `HEAD` / `parents(@)`). |
| `handoff.*` | 스냅샷의 댓글 수 및 문자 수 상한. |
| `omp.binary` / `omp.profile` | 자식 omp 실행 파일과 `--profile`. 자식은 부모 프로필을 암묵적으로 상속하지 않습니다. |

macOS Keychain 토큰 명령 예시: `["security", "find-generic-password", "-s", "jira-api-token", "-a", "<you>", "-w"]`.

## 사용법

`/jira`는 브라우저를 열고, `/jira ABC-123`은 해당 이슈를 엽니다. 대화형 TUI에서만 동작합니다.

| 키 | 동작 |
| --- | --- |
| `j`/`k`, 방향키 | 이동 / 스크롤 |
| `Enter` | 이슈 상세(이슈 + 댓글을 새로 조회. `Tab`으로 창 전환, `PgUp`/`PgDn`) |
| `/` | `search.jql`을 이용한 텍스트 검색 |
| `J` | JQL 직접 입력 |
| `f` | 이름 있는 필터 |
| `r` | 다시 불러오기 |
| `w` | 체크아웃 위저드(기본: 체크아웃만) |
| `d` | omp 실행(pane)이 미리 선택된 체크아웃 위저드 |
| `s` | 상태 전환 |
| `Esc` | 뒤로 / 닫기 |

### 체크아웃 위저드

저장소 → 새 이름 또는 기존 체크아웃 → 동작(체크아웃만 / omp pane / omp tab) → 스냅샷 미리보기 + 작업 지시 편집기 → 단일 최종 확인 순으로 진행됩니다. 최종 확인 전에는 아무것도 생성, 기록, 전송, 실행되지 않습니다.

- 새 Git 체크아웃은 확인 전에 결정된 기준 커밋에서 `git worktree add -b <name> -- <dest> <commit>`을 한 번 실행합니다. 새 jj 체크아웃은 확인 화면에 표시된 부모 커밋 id로 `jj workspace add --no-colocate --name <name> -r <parent>… -- <dest>`를 실행합니다.
- 영속 영수증(`$XDG_STATE_HOME/omp-jira-worktree/receipts/`)은 네이티브 명령 실행 전에 Pending으로 기록되고, 결과가 검증된 후에만 Ready로 기록됩니다. Pending/Failed 영수증은 절대 자동으로 재시도되지 않습니다. 경로를 확인한 뒤 복구 확인과 함께 **Existing**을 선택하세요.
- reset, stash, 강제 실행, fetch, 삭제, forget은 절대 하지 않습니다. 터미널 실행이 실패해도 체크아웃은 롤백되지 않습니다.
- 락은 `$XDG_STATE_HOME/omp-jira-worktree/locks/` 아래에 있습니다. 비정상 종료된 프로세스가 남긴 락은 보고만 하고 절대 빼앗지 않습니다. 확인 후 수동으로 삭제하세요.

### omp 핸드오프

스냅샷(제한된 이슈 필드, 설명, 댓글, 일부 누락 사유)과 작업 지시는 `$XDG_STATE_HOME/omp-jira-worktree/handoffs/<uuid>.md`(파일 0600, 디렉터리 0700)에 기록됩니다. 이 파일은 실행 후에도 유지되며 자식 omp 세션에 저장될 수도 있습니다. 자식은 다음과 같이 시작됩니다.

```
omp --cwd <checkout> --profile <profile> --extension <this extension.ts> @<handoff.md> -- <fixed message>
```

호스트 터미널 런처(tmux, Zellij, Herdr, CMUX, Orca)를 통해 실행됩니다. Herdr/CMUX/Orca는 대상 셸이 POSIX인지 확인을 요청합니다. 지원되는 멀티플렉서가 없거나 거절하면 명령과 cwd를 보여주므로 직접 실행하면 됩니다. 컴포저에는 아무것도 입력되지 않습니다. "Terminal launch requested"는 에이전트가 시작되었다는 뜻이 아닙니다. 실행 결과가 불확실한 경우, 재실행은 별도의 동작이며 먼저 기존 pane을 확인하라고 요청합니다.

핸드오프에 포함된 Jira 데이터는 선택한 omp 프로필의 모델 제공자에게 전송됩니다.

### 상태 전환

`s`는 이슈에서 사용 가능한 전환 목록을 보여줍니다. 화면이나 필수 필드가 필요한 전환은 전송하지 않고, 웹에서 처리할 수 있도록 Jira 링크를 보여줍니다. 최종 확인 후 락을 잡은 상태에서 이슈와 전환 목록을 다시 읽습니다. 무언가 바뀌었다면 승인은 폐기됩니다. 그렇지 않으면 Pending 기록을 저장하고 `{transition:{id}}`를 정확히 한 번 전송합니다. 결과는 다음과 같습니다.

- **2xx**: 수락됨. 새 GET으로 상태를 확인합니다.
- **4xx**: 거부됨.
- **5xx, 타임아웃, 연결 끊김**: 알 수 없음. Jira 이력을 확인했다고 확인할 때까지 해당 이슈의 다음 전환은 차단됩니다.

Jira에는 조건부 전환 API가 없으므로, 재조회와 POST 사이에 다른 Jira 사용자와 경합이 발생할 가능성을 완전히 배제할 수는 없습니다.

## 개발

```sh
bun run check   # tsc --noEmit
bun test        # real git/jj temp repos + loopback Jira servers
bun run smoke   # end-to-end with the real modules against a loopback Jira fixture
bun scripts/smoke.ts --serve   # keep the fixture running for manual TUI testing
```

## 라이선스

MIT
