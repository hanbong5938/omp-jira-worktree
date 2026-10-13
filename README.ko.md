# jira-tern

[English](README.md) | [한국어](README.ko.md) | [日本語](README.ja.md) | [简体中文](README.zh-CN.md)

Rust로 작성한 [Tern](https://docs.stencil.so/tern/) 앱: Jira 이슈를 탐색하고(검색 / JQL / 필터, 설명 + 댓글), 댓글을 작성하며, 이슈별로 **Git worktree** 또는 **네이티브 jj workspace**를 생성하거나 재사용하고, Jira 스냅샷을 첨부한 채 해당 체크아웃에서 **새 omp 프로세스**를 시작하며, **명시적인** Jira 상태 전환을 실행합니다. UI는 Tern 네이티브(Tern Surface Protocol)입니다. Tern 전용, macOS/Linux.

## 요구 사항

- Tern
- Rust 툴체인 ≥ 1.89 (`cargo`), 첫 실행 시 앱 빌드에 필요
- Git
- jj ≥ 0.46 (선택, jj 저장소에만 필요)
- omp (에이전트 핸드오프용)

## 설치

```sh
tern plugin install github.com/hanbong5938/jira-tern
# 또는 클론한 디렉터리에서:
tern plugin link .
```

플러그인은 팔레트 명령 **Jira: open (tab)** 과 **Jira: open (split right)** 두 개를 추가합니다. 둘 다 포커스된 pane의 디렉터리(알 수 없으면 홈)에서 `scripts/jira-tern.sh`를 실행합니다. 스크립트는 바이너리가 없거나 소스보다 오래되었으면 `cargo build --release --locked`로 `<plugin>/target`에 빌드한 뒤 실행합니다.

## 설정

설정은 `$JIRA_TERN_CONFIG`(절대 경로) 또는 `${XDG_CONFIG_HOME:-~/.config}/jira-tern/config.json`에서 읽습니다. 프로젝트 단위 설정이나 환경 변수 자격 증명 대체는 없습니다. [`config.example.json`](./config.example.json)에서 시작하세요.

| 키 | 의미 |
| --- | --- |
| `jira.baseUrl` | 사이트 URL. HTTPS 필수(로컬 픽스처용 loopback HTTP 허용). 컨텍스트 경로 유지. |
| `jira.deployment` | `cloud`(REST v3, 토큰 페이징) 또는 `dc`(REST v2, 오프셋 페이징). 자동 대체 없음. |
| `jira.auth` / `jira.email` | `basic`(이메일 + API 토큰) 또는 `bearer`(PAT). |
| `jira.tokenCommand` | 셸 없이 실행되는 argv. stdout(끝의 LF 하나 제거)이 토큰. 로그나 파일에 남지 않음. |
| `jira.maxResults` / `jira.timeoutMs` | 쿼리당 전체 결과 상한(1–1000), 요청당 타임아웃(1000–120000 ms). |
| `filters` | `f`용 이름 붙은 JQL 필터. 첫 번째가 초기 목록. |
| `search.jql` | `/`용 템플릿. `{query}`를 정확히 한 번 포함해야 함. |
| `worktree.repos` | Jira 프로젝트 키 → 저장소 경로(마법사의 기본 저장소). |
| `worktree.prefer` | 저장소에 `.jj`와 `.git`이 모두 있을 때 `jj` 또는 `git`. |
| `worktree.root` | 새 체크아웃이 생성되는 위치(기본 `~/.local/share/jira-tern/checkouts`). |
| `worktree.gitBase` / `worktree.jjBase` | 새 체크아웃의 기준 commit-ish / revset(기본 `HEAD` / `parents(@)`). |
| `handoff.*` | 스냅샷의 댓글 수와 문자 수 상한. |
| `omp.binary` / `omp.profile` | 자식 omp 실행 파일과 `--profile`. |

macOS 키체인 토큰 명령 예: `["security", "find-generic-password", "-s", "jira-api-token", "-a", "<you>", "-w"]`.

## 사용법

| 키 | 동작 |
| --- | --- |
| `j`/`k`, 화살표 | 이동 / 스크롤 |
| `Enter` | 이슈 상세(이슈 + 댓글 새로 조회, `Tab`으로 pane 전환, `PgUp`/`PgDn`) |
| `/` | `search.jql`을 통한 텍스트 검색 |
| `J` | 원시 JQL |
| `f` | 이름 붙은 필터 |
| `r` | 다시 조회 |
| `c` | 댓글 작성 |
| `w` | 체크아웃 마법사(기본: 체크아웃만) |
| `d` | omp 실행이 미리 선택된 체크아웃 마법사(split) |
| `s` | 상태 전환 |
| `Esc` | 뒤로 / 닫기 |

### 댓글

`c`는 이슈의 댓글 편집기를 엽니다. `Ctrl+Enter`는 확인 후 전송합니다. 댓글은 정확히 한 번 게시되며 재시도하지 않습니다:

- **2xx**: 생성됨. 댓글을 다시 불러옵니다.
- **4xx**: 거부됨. 서버 메시지를 보여 주고 입력한 텍스트는 유지됩니다.
- **5xx, 타임아웃, 연결 끊김**: 알 수 없음. 댓글이 저장되었을 수 있으므로 다시 게시하기 전에 Jira에서 이슈를 확인하세요.

### 체크아웃 마법사

저장소 → 새 이름 또는 기존 체크아웃 → 동작(체크아웃만 / omp split / omp tab) → 스냅샷 미리보기 + 작업 지시 편집기 → 단 한 번의 최종 확인. 최종 확인 전에는 아무것도 생성·기록·게시·실행되지 않습니다.

- 새 Git 체크아웃은 확인 전에 결정된 기준 커밋에서 `git worktree add -b <name> -- <dest> <commit>`을 한 번 실행합니다. 새 jj 체크아웃은 확인 화면에 표시된 부모 커밋 id로 `jj workspace add --no-colocate --name <name> -r <parent>… -- <dest>`를 실행합니다.
- 영속 영수증(`${XDG_STATE_HOME:-~/.local/state}/jira-tern/receipts/`)은 네이티브 명령 전에 Pending으로, 결과가 검증된 뒤에만 Ready로 기록됩니다. Pending/Failed 영수증은 자동으로 재시도되지 않습니다. 경로를 확인한 뒤 복구 확인과 함께 **Existing**을 선택하세요.
- reset, stash, force, fetch, remove, forget은 절대 하지 않습니다. 터미널 실행이 실패해도 체크아웃은 롤백되지 않습니다.
- 잠금은 `${XDG_STATE_HOME:-~/.local/state}/jira-tern/locks/` 아래에 있습니다. 비정상 종료한 프로세스가 남긴 잠금은 보고만 하고 빼앗지 않습니다. 확인 후 직접 삭제하세요.

### omp 핸드오프

스냅샷(제한된 이슈 필드, 설명, 댓글, 부분 결과 사유)과 작업 지시는 `${XDG_STATE_HOME:-~/.local/state}/jira-tern/handoffs/<id>.md`(파일 0600, 디렉터리 0700)에 기록됩니다. 파일은 실행 후에도 유지됩니다. 자식은 `tern new tab` 또는 `tern split`(포커스된 pane 오른쪽)으로 열리며 다음을 실행합니다:

```
omp --cwd <checkout> --profile <profile> @<handoff.md> -- <fixed message>
```

`tern` CLI를 시작할 수 없으면 직접 실행할 수 있도록 명령과 cwd를 보여 줍니다. "실행 요청됨"은 에이전트가 시작되었다는 뜻이 아닙니다. 실행 결과가 불확실하면 재실행은 별도의 동작이며 먼저 기존 pane을 확인하라고 묻습니다.

핸드오프의 Jira 데이터는 선택한 omp 프로필의 모델 제공자에게 전송됩니다.

### 상태 전환

`s`는 이슈에서 가능한 전환 목록을 보여 줍니다. 화면이나 필수 필드가 필요한 전환은 게시하지 않고 웹에서 처리할 수 있도록 Jira 링크를 보여 줍니다. 최종 확인 후 잠금 아래에서 이슈와 전환을 다시 읽습니다. 무엇이든 바뀌었으면 승인은 폐기됩니다. 그렇지 않으면 Pending 기록을 저장하고 `{transition:{id}}`를 정확히 한 번 게시합니다. 결과는 다음과 같습니다:

- **2xx**: 수락됨. 새 GET으로 상태를 확인합니다.
- **4xx**: 거부됨.
- **5xx, 타임아웃, 연결 끊김**: 알 수 없음. Jira 이력을 확인했다고 확인할 때까지 해당 이슈의 다음 전환은 차단됩니다.

Jira에는 조건부 전환 API가 없으므로, 재조회와 POST 사이에 다른 Jira 사용자와의 경합을 완전히 배제할 수는 없습니다.

## 개발

```sh
cargo test     # 실제 git/jj 임시 저장소 + loopback Jira 서버
cargo clippy
```

## 라이선스

MIT
