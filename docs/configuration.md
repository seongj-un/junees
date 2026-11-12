# 설정

설정 파일, 처음 설정 마법사, 환경 변수 전체 목록.

## 설정 파일과 처음 설정

설정은 `$JUNEES_HOME/config.toml`(기본 `~/.junees/config.toml`)에 있다. 처음 실행할 때 설정 파일도 `ANTHROPIC_API_KEY`도 없고 표준 입력과 표준 오류가 모두 터미널이면 설정 마법사가 먼저 뜬다(`junees setup`으로 언제든 다시).

1. Claude에 닿는 방법: `1) Claude subscription`(Claude Code CLI 로그인 필요) 또는 `2) Anthropic API key`.
2. 구독이면 `claude --version`과 `claude auth status --json`으로 확인한다(작업 모드가 쓰는 것과 같은 검사). 로그인이 안 돼 있으면 방법(`claude` 실행 후 `/login`, 브라우저가 없으면 `claude setup-token`, Linux면 bubblewrap 설치)을 보여 주고 설정은 저장한다. 구독은 대화(`junees`, `junees "질문"`)와 장기 작업(`junees task`, `/task`)을 모두 Claude Code로 돌리므로 API 키는 묻지 않는다(파일에 있던 키는 api 백엔드로 돌아갈 때를 위해 그대로 둔다).
3. API 키를 고르면 화면에 보이지 않게 입력한다(터미널 echo를 끔). `sk-ant-`로 시작하지 않으면 다시 묻고, 잘못 넣은 값도 화면에 다시 찍지 않는다.
4. API 키를 골랐으면 대화 모델(기본 `claude-sonnet-4-6`)을 묻고, 작업 기본 예산(시간 기본 `2h`, 토큰 상한은 선택)을 묻고 저장한다.
5. 마지막으로 디스코드 봇을 설정할지 묻는다(선택, 기본 아니오. 이 질문에서 입력이 끝나면 건너뛴다). `y`면 봇 토큰(화면에 보이지 않게, 디스코드 토큰 모양이 아니면 값을 되풀이하지 않고 다시 묻는다), 허용할 사용자 id(하나 이상), 멘션 없이 답할 채널 id(선택, `none`이면 비움)를 묻는다. [디스코드 게이트웨이](gateways.md#디스코드-게이트웨이) 참고.

Ctrl-C나 입력 끝(Ctrl-D)이면 아무것도 저장하지 않고 끝난다. 터미널이 아니면(cron, 파이프) 마법사를 띄우지 않고 `no API key: run `junees setup`, or set ANTHROPIC_API_KEY` 오류로 끝난다. 백엔드가 `claude-code`면(설정, `JUNEES_BACKEND`, 작업의 `--backend`) 키가 필요 없으므로 대화든 작업이든 마법사 없이 실행한다.

파일은 TOML의 단순한 부분만 쓴다: 한 줄에 `키 = 값`, 값은 따옴표 문자열이나 숫자, `true`/`false`, `#` 주석. 표(`[...]`)와 배열은 받지 않는다. 키마다 환경 변수 하나에 대응하고, **환경 변수가 있으면 파일보다 우선한다**(`/model`, `/backend`로 바꾼 값은 그보다도 우선하지만 이 실행에만 적용되고 파일은 바꾸지 않는다). 모르는 키, 따옴표 없는 문자열(`model = claude-...`), 같은 키 두 번은 `<경로> is not a valid settings file ... line N: ...` 오류로 알려 준다.

```toml
backend = "claude-code"          # JUNEES_BACKEND
api_key = "sk-ant-..."           # ANTHROPIC_API_KEY
model = "claude-sonnet-4-6"      # JUNEES_MODEL
task_max_time = "2h"             # JUNEES_TASK_MAX_TIME
task_max_tokens = 2000000        # JUNEES_TASK_MAX_TOKENS
```

그 밖의 키: `base_url`, `prices`([사용량과 비용](usage.md#사용량과-비용)), `max_tokens`, `max_turns`, `context_window`, `compact_threshold`, `thinking_budget`, `task_skill_manage`, `subagents`, `subagent_max_turns`, `subagent_max_tokens`, `approval`, `stream`, `deny_commands`, `allow_dangerous`, `hidden_env`(쉼표로 구분한 문자열), `sandbox`, `sandbox_write`, `sandbox_read`(쉼표로 구분한 문자열), `sandbox_network`, `notify_webhook`, `notify_each_session`, `claude_bin`, `claude_model`, `web_search`, `web_search_max_uses`, `web_search_allowed_domains`, `web_search_blocked_domains`(쉼표로 구분한 문자열), `memory_user_chars`, `memory_notes_chars`, `memory_project_chars`, `context_files`(쉼표로 구분한 문자열, [프로젝트 지침 파일](memory-and-skills.md#프로젝트-지침-파일)), `discord_token`, `discord_allowed_users`, `discord_channels`, `discord_parallel`, `discord_approval_timeout`, `telegram_token`, `telegram_allowed_users`, `telegram_parallel`, `telegram_approval_timeout`, `telegram_format`(`html` 또는 `plain`, [텔레그램 게이트웨이](gateways.md#대화와-출력)), `timezone`, `cron_max_time`, `cron_max_tokens`, `webhook_max_time`, `webhook_max_tokens`, `github_token`, `github_api_url`, `github_repos`, `github_allow_unattended`, `github_flow_merge`. 각각 `JUNEES_` 접두 환경 변수(`base_url`은 `ANTHROPIC_BASE_URL`, `discord_*`는 `JUNEES_DISCORD_*`, `telegram_*`는 `JUNEES_TELEGRAM_*`, `cron_*`는 `JUNEES_CRON_*`, `webhook_*`는 `JUNEES_WEBHOOK_*`, `github_*`는 `JUNEES_GITHUB_*`)와 같다. `JUNEES_HOME`과 `HOME`은 파일 위치를 정하므로 파일에 둘 수 없고, Junees가 읽지 않는 변수(`ANTHROPIC_TOKEN` 등)는 파일로도 설정되지 않는다.

파일은 API 키를 담을 수 있으므로 `0600`, 새로 만드는 폴더는 `0700`으로 만들고, 임시 파일에 쓴 뒤 이름을 바꿔서 중간에 죽어도 반쯤 쓴 파일이 남지 않는다. 마법사는 저장한 뒤 `ANTHROPIC_API_KEY`나 `JUNEES_BACKEND`가 환경에 있으면 그쪽이 이긴다고 알려 준다. 파일의 키는 환경 변수가 아니므로 bash 도구의 환경에는 처음부터 없다. 모델의 도구도 이 파일에 손대지 못한다: `read_file`/`write_file`/`edit_file`은 이 파일을 가리키는 경로(`..`, 심볼릭 링크, 아직 없는 파일을 가리키는 링크, 하드 링크 포함)를 "사용자의 비공개 설정 파일"이라는 오류로 거절하고, `bash` 명령 정책은 이 파일을 인자나 리다이렉트 대상으로 쓰는 명령과, 이 파일이 든 폴더(`$JUNEES_HOME`)를 통째로 다루는 명령을 막는다([명령 정책](safety.md#명령-정책) 참고). 쓰기까지 막는 것은 파일에 `allow_dangerous = "1"`을 넣어 다음 실행의 명령 규칙을 끄는 일도 막기 위해서다. `$JUNEES_HOME`의 나머지(세션 기록 등)는 그대로 읽을 수 있다. 다만 bash 쪽 명령 정책은 차단 목록일 뿐이어서 다른 변수나 실행 중에 만든 경로, 더 위 폴더를 통째로 다루는 명령은 놓친다. 그런 읽기는 [실행 격리](safety.md#실행-격리-landlock)(Landlock, 지원하는 커널에서 기본으로 켜짐)가 커널에서 막는다. 꺼져 있거나 지원하지 않는 커널에서는 여전히 읽을 수 있다(`~/.ssh`처럼 같은 사용자의 파일이다).

## 환경 변수

`JUNEES_HOME` 말고는 모두 설정 파일 키로도 줄 수 있고([설정 파일](#설정-파일과-처음-설정) 절), 둘 다 있으면 환경 변수가 이긴다.

| 환경 변수 | 기본값 | 설명 |
|---|---|---|
| `ANTHROPIC_API_KEY` | (api 백엔드에서 필수, 또는 설정 파일 `api_key`) | Anthropic API 키 |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | API 주소 |
| `JUNEES_BACKEND` | `api` | 대화와 작업을 돌릴 것: `api`(Junees의 에이전트) 또는 `claude-code`([구독 모드](subscription.md#구독-모드-claude-code-백엔드)) |
| `JUNEES_MODEL` | `claude-sonnet-4-6` | 모델 |
| `JUNEES_PRICES` | (없음) | 비용 추정에 쓸 모델 가격, `<모델>=<입력>/<출력>` 쉼표 목록 ([사용량과 비용](usage.md#사용량과-비용)) |
| `JUNEES_MAX_TOKENS` | `8192` | 모델 호출 1회의 최대 출력 토큰 |
| `JUNEES_MAX_TURNS` | `50` | 입력 하나당 최대 모델 호출 횟수 |
| `JUNEES_SUBAGENTS` | `3` | `delegate`가 동시에 돌리는 서브에이전트 수(1~10). `0`이면 `delegate`를 끈다 ([서브에이전트](memory-and-skills.md#서브에이전트-delegate)) |
| `JUNEES_SUBAGENT_MAX_TURNS` | `30` | 서브에이전트 하나의 최대 모델 호출 횟수 |
| `JUNEES_SUBAGENT_MAX_TOKENS` | `2000000` | 서브에이전트 하나의 최대 토큰(입력, 캐시 쓰기/읽기, 출력 합계) |
| `JUNEES_CONTEXT_WINDOW` | `200000` | 모델의 컨텍스트 창 크기(토큰) |
| `JUNEES_COMPACT_THRESHOLD` | `0.8` | 컨텍스트 창 중 이 비율부터 도구 결과 비우기/압축 (0 초과 1 이하) |
| `JUNEES_THINKING_BUDGET` | `0` | 확장 사고(extended thinking) 토큰 예산. `0`이면 끔. 켜려면 1024 이상이고 `JUNEES_MAX_TOKENS`보다 작아야 한다(아니면 시작할 때 오류) |
| `JUNEES_HOME` | `~/.junees` | 세션 기록 등 상태를 두는 폴더 |
| `JUNEES_DENY_COMMANDS` | (없음) | `bash`에서 추가로 막을 명령 접두사, 쉼표 구분 (예: `docker rm,kubectl delete`) |
| `JUNEES_ALLOW_DANGEROUS` | `0` | `1`이면 기본 명령 차단 규칙을 끈다 (사용자 책임) |
| `JUNEES_APPROVAL` | `risky` | 대화 모드에서 실행 전에 물을 도구 호출: `off`, `risky`(위험 신호가 있는 명령, 작업 폴더 밖 쓰기), `all`(모든 `bash`/`write_file`/`edit_file`) |
| `JUNEES_SANDBOX` | `auto` (예약 작업, 웹훅 실행은 `require`) | `bash` 명령의 Landlock 격리: `auto`(가능하면), `require`(못 하면 실행 거절), `off` ([실행 격리](safety.md#실행-격리-landlock)) |
| `JUNEES_SANDBOX_WRITE` | (없음) | 격리된 명령이 작업 폴더와 임시 폴더 밖에서 더 쓸 수 있는 폴더, 쉼표 구분 절대 경로(`~/...` 가능) |
| `JUNEES_SANDBOX_READ` | (없음, 전체) | 주면 격리된 명령이 이 폴더들과 쓸 수 있는 폴더만 읽는다(시스템 폴더를 넣어야 bash가 시작한다) |
| `JUNEES_SANDBOX_NETWORK` | `1` | `0`이면 격리된 명령의 TCP 연결과 bind를 막는다 (Landlock ABI 4 이상) |
| `JUNEES_HIDDEN_ENV` | (없음) | `bash`에서 추가로 숨길 환경 변수 이름, 쉼표 구분 (예: `DB_PASSWORD,GITHUB_TOKEN`) |
| `JUNEES_NOTIFY_WEBHOOK` | (없음) | `task run`이 멈출 때 알림을 보낼 디스코드 웹훅 URL (https만; 평문 http는 localhost와 루프백 주소에만 허용) |
| `JUNEES_NOTIFY_EACH_SESSION` | `0` | `1`이면 작업 세션마다 한 줄 알림도 보낸다 |
| `JUNEES_WEBHOOK_MAX_TIME` | `30m` | 웹훅 실행 하나의 시간 한도 ([웹훅](automation.md#웹훅)) |
| `JUNEES_WEBHOOK_MAX_TOKENS` | `1000000` | 웹훅 실행 하나의 토큰 한도 (`0`이면 없음) |
| `JUNEES_TASK_MAX_TOKENS` | (없음) | `task run --max-tokens`의 기본값 (작업 전체 누적 토큰) |
| `JUNEES_TASK_MAX_TIME` | (없음) | `task run --max-time`의 기본값 (이번 실행 시간, 예: `2h`, `1h30m`) |
| `JUNEES_TASK_SKILL_MANAGE` | `0` | `1`이면 작업 세션도 `skill_manage`로 스킬을 바꿀 수 있다 (기본은 읽기만, [스킬](memory-and-skills.md#스킬)) |
| `JUNEES_STREAM` | `1` | `0`이면 스트리밍을 끄고 응답을 한 번에 받아 찍는다 |
| `JUNEES_WEB_SEARCH` | `0` | `on`/`1`이면 API의 웹 검색 도구를 켠다 (별도 과금, [웹 검색](usage.md#웹-검색)) |
| `JUNEES_WEB_SEARCH_MAX_USES` | `5` | 한 턴의 웹 검색 횟수 상한 (`max_uses`) |
| `JUNEES_WEB_SEARCH_ALLOWED_DOMAINS` | (없음) | 이 도메인에서만 검색, 쉼표 구분 (예: `docs.rs,example.com/blog`) |
| `JUNEES_WEB_SEARCH_BLOCKED_DOMAINS` | (없음) | 이 도메인은 검색하지 않음, 쉼표 구분. `ALLOWED`와 함께 쓸 수 없다 |
| `JUNEES_CLAUDE_BIN` | `PATH`의 `claude` | claude-code 백엔드가 실행할 `claude` 프로그램 |
| `JUNEES_CLAUDE_MODEL` | (없음) | claude-code 백엔드에서 `claude`에 `--model`로 넘길 모델 |
| `JUNEES_MEMORY_USER_CHARS` | `1500` | 메모리 `user` 저장소의 문자 수 상한(1~8000, [메모리](memory-and-skills.md#메모리)) |
| `JUNEES_MEMORY_NOTES_CHARS` | `2500` | 메모리 `notes` 저장소의 문자 수 상한(1~8000) |
| `JUNEES_MEMORY_PROJECT_CHARS` | `2500` | 메모리 `project` 저장소의 문자 수 상한(1~8000) |
| `JUNEES_CONTEXT_FILES` | `AGENTS.md,JUNEES.md,CLAUDE.md` | 읽을 프로젝트 지침 파일 이름, 쉼표 구분. `off`면 끈다 ([프로젝트 지침 파일](memory-and-skills.md#프로젝트-지침-파일)) |
| `JUNEES_TIMEZONE` | 시스템 시간대 | 예약 작업의 일정과 `junees usage`의 날짜 경계 |
| `JUNEES_CRON_MAX_TIME` | `30m` | 예약 작업 실행 하나의 시간 한도 ([예약 작업](automation.md#예약-작업)) |
| `JUNEES_CRON_MAX_TOKENS` | `1000000` | 예약 작업 실행 하나의 토큰 한도 |

디스코드(`JUNEES_DISCORD_*`)와 텔레그램(`JUNEES_TELEGRAM_*`) 설정은 [게이트웨이](gateways.md), GitHub 설정(`JUNEES_GITHUB_*`)은 [GitHub](automation.md#github)의 설정 예시에 있다.

브라우저 도구의 키 `browser`, `browser_path`, `browser_allow_private`, `browser_unattended`, `browser_no_sandbox`(환경 변수 `JUNEES_BROWSER`, `JUNEES_BROWSER_PATH`, `JUNEES_BROWSER_ALLOW_PRIVATE`, `JUNEES_BROWSER_UNATTENDED`, `JUNEES_BROWSER_NO_SANDBOX`)는 [브라우저 도구](browser.md#켜기)에 있다.
