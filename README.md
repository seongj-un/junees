# Junees

Rust로 만드는 AI 에이전트 하네스. LLM이 도구를 호출하며 일을 처리하는 루프를 바닥부터 직접 만든다.
[Hermes Agent](https://github.com/NousResearch/hermes-agent)에서 영감을 받았고, 단일 바이너리와 적은 메모리 사용을 목표로 한다.

## 빠른 시작

```sh
cargo install --path .              # 한 번 설치하면 어디서나 junees (~/.cargo/bin)
junees                              # 대화 (처음이면 설정 마법사가 먼저 뜸)
junees "이 폴더에 뭐가 있어?"        # 한 번만 묻기
junees -c                           # 이 폴더의 마지막 대화 이어서
junees task "할 일 관리 웹 앱을 만들어"   # 장기 작업: 계획하고 바로 실행 (git 저장소 안에서)
junees setup                        # 설정 다시
junees gateway                      # 디스코드 봇으로 띄우기 (아래 디스코드 게이트웨이)
junees gateway telegram             # 텔레그램 봇으로 띄우기 (아래 텔레그램 게이트웨이)
junees cron add "daily 09:00" "오늘 할 일을 정리해 줘"   # 예약 작업 (아래 예약 작업)
junees doctor                       # 설치와 설정 점검 (아래 점검)
```

대화 안에서 쓰는 명령:

- `/task <목표>`: 지금 폴더의 git 저장소에서 장기 작업을 계획하고 바로 실행한다(`junees task "목표"`와 같음). `/task`만 쓰면 멈춘 작업을 이어서 한다(`junees task`).
- `/status`: 작업 상태(`junees status`와 같음).
- `/sessions`: 최근 세션 10개를 번호와 함께 보여 주고, 번호를 넣으면 그 세션으로 이어 간다(Enter는 그대로). 디스코드 게이트웨이의 채널 세션과 예약 작업 실행 세션은 번호 없이 따로 보여 주기만 한다(아래 디스코드 게이트웨이, 예약 작업).
- `/search <낱말>`: 저장된 세션에서 낱말을 찾아 번호와 함께 보여 준다(아래 세션 검색).
- `/new`: 새 대화(예전 이름 `/reset`도 됨).
- `/model [이름]`: 지금 모델을 보여 주거나, 이 실행 동안만 다른 모델로 바꾼다(claude-code 백엔드에서는 Claude Code에 `--model`로 넘긴다).
- `/backend [api|claude-code]`: 대화와 `/task`를 무엇으로 돌릴지 보여 주거나, 이 실행 동안만 둘 다 바꾼다. 대화를 바꾸면 새 세션으로 시작한다(아래 구독 모드).
- `/memory`: 영구 메모리 세 저장소의 파일, 사용량, 항목을 보여 준다. `/memory clear user|notes|project`는 한 저장소를 비운다(`y`로 확인한 뒤. 아래 메모리).
- `/skills`: 스킬 목록(`$JUNEES_HOME/skills`, 아래 스킬), `/skill <이름>`: 스킬 본문과 파일 목록.
- `/cron`: 예약 작업 목록(아래 예약 작업).
- `/context`: 컨텍스트 크기와 한도(api 백엔드만. claude-code에서는 Claude Code가 컨텍스트를 스스로 관리한다는 안내), 그리고 이 세션이 읽은 프로젝트 지침 파일과 바이트 수(두 백엔드 모두, 아래 프로젝트 지침 파일).
- `/doctor`: 설치와 설정 점검(`junees doctor`와 같은 목록, 네트워크 없이. 아래 점검).
- `/compact`(api 백엔드만), `/usage`, `/help`, `/exit`.

명령줄에서도 `junees task "목표"`(계획 + 실행), `junees task`(이어서 실행), `junees status`(상태). 하위 명령 이름과 같은 한 단어 질문(`status`, `task`, `setup`, `gateway`, `discord`, `telegram`, `cron`, `sessions`, `doctor`)은 `junees -p status`처럼 `-p`로 묻는다. `-`로 시작하는 질문은 `junees -- "-5도면 추워?"`.

### 설정 파일과 처음 설정

설정은 `$JUNEES_HOME/config.toml`(기본 `~/.junees/config.toml`)에 있다. 처음 실행할 때 설정 파일도 `ANTHROPIC_API_KEY`도 없고 표준 입력과 표준 오류가 모두 터미널이면 설정 마법사가 먼저 뜬다(`junees setup`으로 언제든 다시).

1. Claude에 닿는 방법: `1) Claude subscription`(Claude Code CLI 로그인 필요) 또는 `2) Anthropic API key`.
2. 구독이면 `claude --version`과 `claude auth status --json`으로 확인한다(작업 모드가 쓰는 것과 같은 검사). 로그인이 안 돼 있으면 방법(`claude` 실행 후 `/login`, 브라우저가 없으면 `claude setup-token`, Linux면 bubblewrap 설치)을 보여 주고 설정은 저장한다. 구독은 대화(`junees`, `junees "질문"`)와 장기 작업(`junees task`, `/task`)을 모두 Claude Code로 돌리므로 API 키는 묻지 않는다(파일에 있던 키는 api 백엔드로 돌아갈 때를 위해 그대로 둔다).
3. API 키를 고르면 화면에 보이지 않게 입력한다(터미널 echo를 끔). `sk-ant-`로 시작하지 않으면 다시 묻고, 잘못 넣은 값도 화면에 다시 찍지 않는다.
4. API 키를 골랐으면 대화 모델(기본 `claude-sonnet-4-6`)을 묻고, 작업 기본 예산(시간 기본 `2h`, 토큰 상한은 선택)을 묻고 저장한다.
5. 마지막으로 디스코드 봇을 설정할지 묻는다(선택, 기본 아니오. 이 질문에서 입력이 끝나면 건너뛴다). `y`면 봇 토큰(화면에 보이지 않게, 디스코드 토큰 모양이 아니면 값을 되풀이하지 않고 다시 묻는다), 허용할 사용자 id(하나 이상), 멘션 없이 답할 채널 id(선택, `none`이면 비움)를 묻는다. 아래 디스코드 게이트웨이 참고.

Ctrl-C나 입력 끝(Ctrl-D)이면 아무것도 저장하지 않고 끝난다. 터미널이 아니면(cron, 파이프) 마법사를 띄우지 않고 `no API key: run `junees setup`, or set ANTHROPIC_API_KEY` 오류로 끝난다. 백엔드가 `claude-code`면(설정, `JUNEES_BACKEND`, 작업의 `--backend`) 키가 필요 없으므로 대화든 작업이든 마법사 없이 실행한다.

파일은 TOML의 단순한 부분만 쓴다: 한 줄에 `키 = 값`, 값은 따옴표 문자열이나 숫자, `true`/`false`, `#` 주석. 표(`[...]`)와 배열은 받지 않는다. 키마다 환경 변수 하나에 대응하고, **환경 변수가 있으면 파일보다 우선한다**(`/model`, `/backend`로 바꾼 값은 그보다도 우선하지만 이 실행에만 적용되고 파일은 바꾸지 않는다). 모르는 키, 따옴표 없는 문자열(`model = claude-...`), 같은 키 두 번은 `<경로> is not a valid settings file ... line N: ...` 오류로 알려 준다.

```toml
backend = "claude-code"          # JUNEES_BACKEND
api_key = "sk-ant-..."           # ANTHROPIC_API_KEY
model = "claude-sonnet-4-6"      # JUNEES_MODEL
task_max_time = "2h"             # JUNEES_TASK_MAX_TIME
task_max_tokens = 2000000        # JUNEES_TASK_MAX_TOKENS
```

그 밖의 키: `base_url`, `prices`(아래 사용량과 비용), `max_tokens`, `max_turns`, `context_window`, `compact_threshold`, `thinking_budget`, `approval`, `stream`, `deny_commands`, `allow_dangerous`, `hidden_env`(쉼표로 구분한 문자열), `notify_webhook`, `notify_each_session`, `claude_bin`, `claude_model`, `web_search`, `web_search_max_uses`, `web_search_allowed_domains`, `web_search_blocked_domains`(쉼표로 구분한 문자열), `memory_user_chars`, `memory_notes_chars`, `memory_project_chars`, `context_files`(쉼표로 구분한 문자열, 아래 프로젝트 지침 파일), `discord_token`, `discord_allowed_users`, `discord_channels`, `discord_parallel`, `discord_approval_timeout`, `timezone`, `cron_max_time`, `cron_max_tokens`, `webhook_max_time`, `webhook_max_tokens`, `github_token`, `github_api_url`, `github_repos`, `github_allow_unattended`. 각각 `JUNEES_` 접두 환경 변수(`base_url`은 `ANTHROPIC_BASE_URL`, `discord_*`는 `JUNEES_DISCORD_*`, `cron_*`는 `JUNEES_CRON_*`, `webhook_*`는 `JUNEES_WEBHOOK_*`, `github_*`는 `JUNEES_GITHUB_*`)와 같다. `JUNEES_HOME`과 `HOME`은 파일 위치를 정하므로 파일에 둘 수 없고, Junees가 읽지 않는 변수(`ANTHROPIC_TOKEN` 등)는 파일로도 설정되지 않는다.

파일은 API 키를 담을 수 있으므로 `0600`, 새로 만드는 폴더는 `0700`으로 만들고, 임시 파일에 쓴 뒤 이름을 바꿔서 중간에 죽어도 반쯤 쓴 파일이 남지 않는다. 마법사는 저장한 뒤 `ANTHROPIC_API_KEY`나 `JUNEES_BACKEND`가 환경에 있으면 그쪽이 이긴다고 알려 준다. 파일의 키는 환경 변수가 아니므로 bash 도구의 환경에는 처음부터 없다. 모델의 도구도 이 파일에 손대지 못한다: `read_file`/`write_file`/`edit_file`은 이 파일을 가리키는 경로(`..`, 심볼릭 링크, 아직 없는 파일을 가리키는 링크, 하드 링크 포함)를 "사용자의 비공개 설정 파일"이라는 오류로 거절하고, `bash` 명령 정책은 이 파일을 인자나 리다이렉트 대상으로 쓰는 명령과, 이 파일이 든 폴더(`$JUNEES_HOME`)를 통째로 다루는 명령을 막는다(아래 명령 정책 참고). 쓰기까지 막는 것은 파일에 `allow_dangerous = "1"`을 넣어 다음 실행의 명령 규칙을 끄는 일도 막기 위해서다. `$JUNEES_HOME`의 나머지(세션 기록 등)는 그대로 읽을 수 있다. 다만 bash 쪽은 차단 목록일 뿐 샌드박스가 아니어서 다른 변수나 실행 중에 만든 경로, 더 위 폴더를 통째로 다루는 명령으로는 여전히 읽을 수 있다(`~/.ssh`처럼 같은 사용자의 파일이다).

고급 사용법(세션 기록, 세션 검색, 사용량과 비용, 장기 작업 모드, 스트리밍, 확장 사고, 웹 검색, 이미지 입력): [docs/usage.md](docs/usage.md)

메모리, 프로젝트 지침 파일, 스킬, 서브에이전트: [docs/memory-and-skills.md](docs/memory-and-skills.md)

구독 모드(claude-code 백엔드): [docs/subscription.md](docs/subscription.md)

디스코드, 텔레그램 게이트웨이: [docs/gateways.md](docs/gateways.md)

MCP 서버: [docs/mcp.md](docs/mcp.md)

예약 작업, 웹훅, GitHub: [docs/automation.md](docs/automation.md)

플러그인: [docs/plugins.md](docs/plugins.md)

### 컨텍스트 관리

긴 작업에서도 대화가 모델의 컨텍스트 창을 넘지 않게 하고, 반복되는 앞부분은 캐시한다.

- 프롬프트 캐싱: 매 요청에서 도구 목록의 마지막 도구(웹 검색이 켜졌으면 `web_search`), 시스템 프롬프트의 마지막 블록, 마지막 메시지의 마지막 일반 블록(텍스트, 도구 호출, 도구 결과)에 `cache_control: ephemeral`을 붙인다. thinking 블록에는 붙일 수 없어서, 그런 블록은 건너뛰고 그 앞의 일반 블록에 붙인다. 한 번만 보내는 요약 호출에는 붙이지 않는다(캐시 쓰기는 일반 입력보다 비싸다). 응답의 캐시 쓰기/읽기 토큰은 `/usage`에 따로 나온다.
- 컨텍스트 크기: 마지막으로 잰 크기(웹 검색을 하지 않은 마지막 응답의 `input_tokens + cache_creation_input_tokens + cache_read_input_tokens + output_tokens`, 위 웹 검색 참고) + 그 뒤 기록에 더해진 블록의 추정치(도구 결과, 입력, 검색한 호출의 응답). 잰 크기가 없으면(첫 호출 전, 압축 직후) 기록 전체와 시스템 프롬프트, 도구 정의를 추정한다. 추정은 블록 JSON을 글자 단위로 센다(`session::estimate_text_tokens`): ASCII는 4글자당 1토큰, 한글/한자/가나(전각 문자 포함)는 1글자당 1토큰, 그 밖의 비ASCII는 UTF-8 4바이트당 1토큰(라틴 악센트, 그리스, 키릴 문자는 2글자당 1토큰). 한글 한 글자는 3바이트지만 대개 1토큰 이상이라 4바이트당 1토큰으로는 낮게 잡혔다. 영어 등 ASCII는 예전(4바이트당 1토큰)과 같고, 어떤 텍스트도 예전보다 작게 잡히지 않는다. JSON 키와 base64 검색 결과 때문에 대개 실제보다 크게 잡히고, 그만큼 조금 일찍 압축될 뿐이다. 둘 다 세션 기록의 항목에서 재생으로 다시 계산되므로 새 항목 없이 이전 세션에도 그대로 적용되고, 이어 가도 유지된다. 한도는 `JUNEES_CONTEXT_WINDOW` x `JUNEES_COMPACT_THRESHOLD`(기본 200000 x 0.8 = 160000).
- 1차 대응(오래된 도구 결과 비우기): 모델 호출 직전에 크기가 한도 이상이면, 도구 결과가 든 메시지 중 최근 3개를 뺀 이전 것들의 출력을 `[tool output cleared to save context]`로 바꾼다(결과에 든 이미지도 지운다). `tool_use_id`와 `is_error`는 그대로라 호출/결과 짝이 유지된다. 기록 항목은 `clear_tool_results { upto }`(수동 thinking 모드면 `keep_thinking: true`, 위 확장 사고 참고).
- 2차 대응(압축): 비울 것이 없거나, 비운 뒤 다음 응답에서도 한도 이상이면 앞부분을 같은 모델로 요약한다. 꼬리는 최근 두 사용자 입력(안 되면 마지막 입력)부터 끝까지이고, 도구 결과가 든 메시지나 도구 호출 바로 뒤에서는 자르지 않는다. 입력이 첫 메시지 하나뿐인 긴 턴(`-p` 한 번으로 끝까지 가는 작업 등)은 그 턴 안에서 자른다: 도구 결과만 든 user 메시지 바로 다음 assistant 메시지(앞의 호출이 모두 결과를 받은 지점)에서, 최근 도구 왕복 두 번(안 되면 한 번)을 남긴다. 이때는 요약과 그 턴의 원래 입력(원문 그대로)을 담은 user 메시지를 꼬리 앞에 새로 넣는다. 사용자 입력에서 자를 수 있으면 그쪽을 먼저 쓴다. 요약 호출은 도구 없이, 사고(thinking) 없이, 요약 전용 시스템 프롬프트로, 앞부분을 텍스트로 옮긴 user 메시지 하나를 보낸다(`max_tokens` 4096). 텍스트로 옮길 때 thinking 블록은 빼고, 웹 검색은 `[assistant: web search: <검색어>]`, 검색 결과는 앞 5개의 제목과 URL(`[assistant: web search results: 제목 (URL); ...; 15 more]`, 내용은 넣지 않음), 다른 원본 블록은 `[assistant: code_execution_tool_result block]`처럼 종류만 적는다. 요약은 꼬리 첫 user 메시지 맨 앞에 `[Summary of the earlier conversation]` 블록으로 들어가 역할 교대가 유지된다. 기록 항목은 `compact { summary, keep_from }`이고, 요약 호출의 사용량은 `summary_usage`로 남아 `/usage`에는 더해지지만 컨텍스트 크기에는 쓰이지 않는다. 압축하면 `(compacted the conversation: N messages -> M)` 한 줄이 나온다.
- 요약 호출이 실패하거나, 빈 요약이 오거나, 요약이 출력 한도(`max_tokens`)에 걸려 잘리면 경고만 띄우고 기록은 그대로 둔다(요약 호출 사용량은 더한다).
- `/compact`는 한도와 상관없이 지금 압축한다.
- API가 컨텍스트 초과(400 "prompt is too long")로 거절하면, 크기 추적과 상관없이 비우기와 압축을 하고, 이어서 최근 3개를 포함한 모든 도구 결과도 비운다(한 응답의 병렬 `read_file` 결과처럼 최근 결과가 원인일 수 있어서). 줄어든 게 있으면 한 번만 다시 보내고, 그래도 실패하면 오류로 끝난다.

`JUNEES_HOME` 말고는 모두 설정 파일 키로도 줄 수 있고(위 설정 파일 절), 둘 다 있으면 환경 변수가 이긴다.

| 환경 변수 | 기본값 | 설명 |
|---|---|---|
| `ANTHROPIC_API_KEY` | (api 백엔드에서 필수, 또는 설정 파일 `api_key`) | Anthropic API 키 |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | API 주소 |
| `JUNEES_MODEL` | `claude-sonnet-4-6` | 모델 |
| `JUNEES_MAX_TOKENS` | `8192` | 모델 호출 1회의 최대 출력 토큰 |
| `JUNEES_MAX_TURNS` | `50` | 입력 하나당 최대 모델 호출 횟수 |
| `JUNEES_SUBAGENTS` | `3` | `delegate`가 동시에 돌리는 서브에이전트 수(1~10). `0`이면 `delegate`를 끈다 (위 서브에이전트) |
| `JUNEES_SUBAGENT_MAX_TURNS` | `30` | 서브에이전트 하나의 최대 모델 호출 횟수 |
| `JUNEES_SUBAGENT_MAX_TOKENS` | `2000000` | 서브에이전트 하나의 최대 토큰(입력, 캐시 쓰기/읽기, 출력 합계) |
| `JUNEES_CONTEXT_WINDOW` | `200000` | 모델의 컨텍스트 창 크기(토큰) |
| `JUNEES_COMPACT_THRESHOLD` | `0.8` | 컨텍스트 창 중 이 비율부터 도구 결과 비우기/압축 (0 초과 1 이하) |
| `JUNEES_THINKING_BUDGET` | `0` | 확장 사고(extended thinking) 토큰 예산. `0`이면 끔. 켜려면 1024 이상이고 `JUNEES_MAX_TOKENS`보다 작아야 한다(아니면 시작할 때 오류) |
| `JUNEES_HOME` | `~/.junees` | 세션 기록 등 상태를 두는 폴더 |
| `JUNEES_DENY_COMMANDS` | (없음) | `bash`에서 추가로 막을 명령 접두사, 쉼표 구분 (예: `docker rm,kubectl delete`) |
| `JUNEES_ALLOW_DANGEROUS` | `0` | `1`이면 기본 명령 차단 규칙을 끈다 (사용자 책임) |
| `JUNEES_APPROVAL` | `risky` | 대화 모드에서 실행 전에 물을 도구 호출: `off`, `risky`(위험 신호가 있는 명령, 작업 폴더 밖 쓰기), `all`(모든 `bash`/`write_file`/`edit_file`) |
| `JUNEES_HIDDEN_ENV` | (없음) | `bash`에서 추가로 숨길 환경 변수 이름, 쉼표 구분 (예: `DB_PASSWORD,GITHUB_TOKEN`) |
| `JUNEES_NOTIFY_WEBHOOK` | (없음) | `task run`이 멈출 때 알림을 보낼 디스코드 웹훅 URL (https만; 평문 http는 localhost와 루프백 주소에만 허용) |
| `JUNEES_NOTIFY_EACH_SESSION` | `0` | `1`이면 작업 세션마다 한 줄 알림도 보낸다 |
| `JUNEES_WEBHOOK_MAX_TIME` | `30m` | 웹훅 실행 하나의 시간 한도 (위 "웹훅") |
| `JUNEES_WEBHOOK_MAX_TOKENS` | `1000000` | 웹훅 실행 하나의 토큰 한도 (`0`이면 없음) |
| `JUNEES_TASK_MAX_TOKENS` | (없음) | `task run --max-tokens`의 기본값 (작업 전체 누적 토큰) |
| `JUNEES_TASK_MAX_TIME` | (없음) | `task run --max-time`의 기본값 (이번 실행 시간, 예: `2h`, `1h30m`) |
| `JUNEES_TASK_SKILL_MANAGE` | `0` | `1`이면 작업 세션도 `skill_manage`로 스킬을 바꿀 수 있다 (기본은 읽기만, 아래 스킬) |
| `JUNEES_STREAM` | `1` | `0`이면 스트리밍을 끄고 응답을 한 번에 받아 찍는다 |
| `JUNEES_WEB_SEARCH` | `0` | `on`/`1`이면 API의 웹 검색 도구를 켠다 (별도 과금, 위 "웹 검색") |
| `JUNEES_WEB_SEARCH_MAX_USES` | `5` | 한 턴의 웹 검색 횟수 상한 (`max_uses`) |
| `JUNEES_WEB_SEARCH_ALLOWED_DOMAINS` | (없음) | 이 도메인에서만 검색, 쉼표 구분 (예: `docs.rs,example.com/blog`) |
| `JUNEES_WEB_SEARCH_BLOCKED_DOMAINS` | (없음) | 이 도메인은 검색하지 않음, 쉼표 구분. `ALLOWED`와 함께 쓸 수 없다 |

기본 도구는 `bash`, `read_file`, `write_file`, `edit_file` 네 가지다(`task` 작업 세션도 같은 네 가지에 `mark_feature_passing`을 더해 쓴다). api 백엔드의 대화와 작업 세션에는 서브에이전트를 띄우는 `delegate`도 있다(위 서브에이전트). 대화 모드는 위험할 수 있는 호출을 실행하기 전에 묻는다(아래 "실행 전 승인"). `-p`와 `task` 모드는 묻지 않고 명령 정책만 적용하므로 믿을 수 있는 폴더에서만 쓴다. `bash`에는 `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `JUNEES_NOTIFY_WEBHOOK`, `JUNEES_DISCORD_TOKEN`, Claude Code의 자격 증명(`CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_OAUTH_REFRESH_TOKEN`, `CLAUDE_CODE_CLIENT_KEY_PASSPHRASE`, `ANTHROPIC_AWS_API_KEY`, `ANTHROPIC_FOUNDRY_API_KEY`, `ANTHROPIC_FOUNDRY_AUTH_TOKEN`, `AWS_BEARER_TOKEN_BEDROCK`, `MCP_CLIENT_SECRET`. [Claude Code 환경 변수 문서](https://code.claude.com/docs/en/env-vars) 기준이고, claude-code 백엔드의 `claude`에는 그대로 넘긴다)과 `JUNEES_HIDDEN_ENV`에 적은 변수가 보이지 않는다. 리눅스에서는 junees가 시작할 때 자기 프로세스를 dump 불가(`PR_SET_DUMPABLE` 0)로 만들어, 같은 사용자의 프로세스도 `/proc/<junees pid>/environ`과 `mem`을 읽지 못한다. 그래서 `bash`가 `/proc/$PPID/environ`처럼 부모를 거슬러 올라가도 이 변수들을 읽을 수 없다. 자식 프로세스는 exec할 때 다시 보통 프로세스가 되므로 도구 동작은 그대로이고, junees 자신의 코어 덤프만 남지 않는다.

`edit_file {path, old_string, new_string, replace_all?}`은 파일의 일부만 고치는 도구다. 큰 파일을 고칠 때 `write_file`로 전체를 다시 쓰지 않아도 되므로 출력 토큰이 줄고, 긴 출력이 `max_tokens`에 잘리는 일도 줄어든다. 시스템 프롬프트에도 "부분 수정은 `edit_file`, 새 파일이나 전체 교체는 `write_file`"이라고 적혀 있다.

- `old_string`은 공백과 들여쓰기까지 정확히 같아야 하고, 파일에서 정확히 한 번 나와야 한다(겹치는 위치도 따로 센다. `aaa`에서 `aa`는 두 번). 여러 번 나오면 횟수와 줄 번호(최대 20개)를 오류로 돌려주고 아무것도 바꾸지 않는다. 모두 바꾸려면 `replace_all: true`. 다만 겹치는 위치는 앞에서부터 바꾸므로 이미 바꾼 곳과 겹치는 위치는 건너뛴다(`aaaa`에서 `aa`는 세 번 나오지만 두 번 바뀐다). 겹치는 위치가 있으면 여러 번 나온다는 오류에 실제로 바뀔 개수를 적는다.
- 찾지 못하면 `old_string`의 첫 줄(공백 제외)과 가장 비슷한 줄을 최대 3개, 줄 번호와 함께 힌트로 보여 준다(문자 2-gram 유사도 0.5 이상).
- 빈 `old_string`, `old_string`과 같은 `new_string`은 오류다. 대상은 이미 있는 일반 파일이고 10 MB 이하(`read_file`과 같은 한도), UTF-8이어야 한다. 심볼릭 링크는 가리키는 파일을 고친다.
- 줄바꿈 보존: `read_file`이 보여 준 대로 `\n`으로 쓴 `old_string`이 그대로는 없으면 `\r\n`으로 바꿔 다시 찾는다(대부분 LF인 파일의 CRLF 줄도). 두 종류가 섞인 부분에 걸친 `old_string`은 찾지 못하며, 파일에 두 종류가 섞여 있으면 찾지 못했다는 오류에 그 사실을 적는다. `new_string`의 줄바꿈은 파일에서 일치한 부분을 따른다. 일치한 부분의 줄바꿈이 모두 `\r\n`이면(`\n`으로 시작하는 `old_string`이 `\r\n`의 `\n`에서 일치한 경우 포함) `new_string`의 `\n`을 `\r\n`으로 쓰고, 일치한 부분에 줄바꿈이 없으면 파일 대부분의 줄바꿈을 따른다. `replace_all`이면 일치한 곳마다 따로 정한다. 원래 파일이 개행으로 끝났는데 고친 결과가 개행 없이 끝나면 개행을 다시 붙인다(결과에 `kept the final newline` 표시).
- 같은 폴더의 임시 파일(`.<이름>.junees-edit-<pid>-<번호>.tmp`)에 원래 권한으로 쓰고 `fsync`한 뒤 `rename`으로 바꿔 넣으므로 중간에 죽어도 반쯤 쓴 파일이 남지 않는다. 쓰기, 권한 설정, `fsync`, `rename` 중 하나라도 실패하면(디스크가 가득 찬 경우 등) 임시 파일을 지우고 오류를 돌려주며 원본은 그대로다. 이 과정은 한 blocking 스레드에서 끝까지 실행되므로, 편집 중에 턴이 취소돼도 임시 파일이 작업 트리에 남지 않는다. 소유자, 하드 링크, 확장 속성(xattr)과 ACL은 유지되지 않는다(새 inode). 임시 파일 이름이 255바이트를 넘지 않도록 긴 파일 이름은 임시 이름에서 줄인다. 그래서 파일에 쓸 수 있어도 폴더에 쓸 수 없으면 고칠 수 없고, 이때 오류에 폴더를 밝히고 `write_file`(제자리 쓰기)을 안내한다.
- 성공하면 바꾼 부분과 위아래 3줄을 `read_file`과 같은 줄 번호 형식으로 보여 준다(`replace_all`이면 처음 3곳까지, 나머지는 개수만).

### 명령 정책

`bash`는 명령을 실행하기 전에 정책으로 검사하고, 걸리면 아무것도 실행하지 않은 채 `blocked by policy: <이유>` 오류 결과를 모델에 돌려준다. 사람이 지켜보지 않는 장기 실행에서 치명적인 명령을 막는 1차 안전장치이고 샌드박스는 아니다.

기본으로 막는 것:

- 루트, 최상위 시스템 폴더(`/usr`, `/etc`, `/home` 등), 홈, 상위 폴더의 재귀 삭제: `rm -r`/`-rf`/`--recursive` 대상이 `/`, `/*`, `~`, `~/`, `$HOME`, `${HOME}`, 홈의 절대 경로나 그 위, `..`, `../..`처럼 현재 폴더 위로 올라가는 경로. `rm -rf ./build`, `rm -rf target`, `rm -rf ../other/target`은 허용
- 위험한 폴더로 `cd` 한 뒤 현재 폴더 전체 삭제: 같은 스크립트에서 앞서 `cd`/`pushd`로 홈(인자 없는 `cd` 포함), `/`, 최상위 시스템 폴더, 상위 폴더, `cd -`, `$`가 들어간 경로(`cd "$BUILD_DIR"`, 변수가 비면 홈으로 간다)에 들어갔으면 그 뒤의 `rm -r *`, `rm -r .`, `rm -r ./*`, `rm -r .*`를 막는다. 그 뒤 다른 폴더로 `cd` 하면 풀린다. `cd build && rm -rf *`는 허용
- 파일시스템/디스크 파괴: `mkfs*`, `wipefs`, `dd ... of=/dev/...`, `> /dev/sda` 같은 장치로의 리다이렉트 (`/dev/null`, `/dev/stdout`, `/dev/stderr`, `/dev/fd/*`, `/dev/tcp/*`, 메모리 파일시스템인 `/dev/shm/*`, `/dev/mqueue/*` 등은 허용)
- 시스템 종료: `shutdown`, `reboot`, `halt`, `poweroff`, `systemctl reboot|poweroff|halt|kexec`
- fork bomb (`:(){ :|:& };:`과 같은 모양: 함수 본문 안에서 자기 자신을 자기 자신에게 파이프로 넘기고 `&`로 띄우는 것. `f(){ ...; }; f | f` 같은 평범한 함수 파이프는 허용)
- 내려받은 스크립트 바로 실행: `curl`/`wget ... | sh`(bash, zsh 등 셸 포함), `bash <(curl ...)`, `sh -c "$(curl ...)"`
- 강제 push: `git push --force`, `-f`, `+refspec` (`--force-with-lease`는 허용)
- 권한 상승: `sudo`, `doas`, `pkexec`, `su`, `runuser`
- 설정 파일(`$JUNEES_HOME/config.toml`, API 키를 담는다)을 이름으로 가리키는 명령: 단어나 리다이렉트 대상(`<`, `>`, `>>`), `--file=경로`/`if=경로`처럼 `=` 뒤의 값이 그 파일이면 막는다. 절대 경로, `~/.junees/config.toml`, `$HOME`/`${HOME}`, `$JUNEES_HOME`/`${JUNEES_HOME}`으로 시작하는 경로, `..`와 심볼릭 링크를 거친 경로, 현재 폴더(같은 스크립트에서 앞서 `cd`/`pushd`한 곳)에서의 상대 경로(`cd ~/.junees && cat config.toml`)를 알아본다. 따옴표 안 단어도 경로로 본다(`echo "~/.junees/config.toml"`도 막힌다). `/proc/self/cwd/...`는 그 시점의 현재 폴더로 푼다. 글롭(`*`, `?`, `[...]`)과 중괄호(`{a,b}`)도 펼쳐서, 설정 파일과 맞을 수 있으면 막는다(`cat ~/.junees/*`, `cat ~/.junees/conf*`, `cat ~/.junees/{config.toml,x}`, `cat ~/.jun*/config.toml`). `mcp.json`과, `$JUNEES_HOME/secrets/`(웹훅 시크릿 파일과 환경 파일을 두는 곳) 폴더와 그 안의 모든 경로도 똑같이 막는다(`cat ~/.junees/secrets/x`, `grep -r . ~/.junees/secrets`, `cat ~/.junees/s*/x`). read_file, write_file, edit_file도 이 파일들을 거절한다. 안전한 쪽으로 기울게 `*`는 점으로 시작하는 이름에도 맞는다고 본다.
- 설정 파일이 든 폴더(`$JUNEES_HOME`, 기본 `~/.junees`) 자체를 가리키는 명령: 폴더를 통째로 복사하거나 묶거나 옮기면 파일 이름을 쓰지 않고도 키를 읽거나 설정을 바꿔치기할 수 있으므로(`grep -r api_key ~/.junees`, `cp -r ~/.junees /tmp/j`, `tar -C ~/.junees ...`, `mv ~/.junees /tmp/old && mv /tmp/evil ~/.junees`, `cd ~/.junees && tar -cf - .`) 막는다. 이름과 메타데이터만 보는 `cd`, `pushd`, `ls`, `stat`, `du`, `test`/`[`, `realpath`, `readlink`, `echo`는 허용한다. 폴더를 변수에 담는 대입(`d=~/.junees`)도 막힌다. 그 안의 세션 폴더와 파일(`~/.junees/sessions/...`)은 그대로 쓸 수 있다.
- Junees 데이터 폴더(`$JUNEES_HOME/memory/`, `$JUNEES_HOME/projects/`, `$JUNEES_HOME/skills/`)에 쓸 수 있는 명령: 이 폴더들은 Junees가 자기 도구(`memory`, 스킬은 `skill_manage`)로만 바꾼다. 그 도구들이 내용을 검사하기 때문이다(작업 세션은 project 메모리만, 비밀과 제어 문자 거절). 그래서 그 안을 가리키는 쓰기 리다이렉트(`>`, `>>`, `>|`, `&>`, `3>` 등)와, 읽기만 하는 것으로 알려진 프로그램(위 목록과 `cat`, `head`, `tail`, `less`, `more`, `grep`/`egrep`/`fgrep`, `rg`, `wc`, `file`, `diff`, `cmp`, `md5sum`/`sha1sum`/`sha256sum`, `nl`, `jq`) 밖의 명령이 그 안의 경로나 폴더 자체를 인자로 받으면 막는다(`echo ... >> ~/.junees/memory/user.md`, `sed -i ... ~/.junees/memory/user.md`, `cp`, `mv`, `rm`, `mkdir`, `ln`, `tee` 등). 오류는 해당 도구를 쓰라고 알려 준다. 경로를 알아보는 방식(`~`/`$HOME`/`$JUNEES_HOME` 형태, `..`, 심볼릭 링크, `cd` 추적, 글롭과 중괄호)은 설정 파일 규칙과 같다. 읽기(`cat ~/.junees/memory/user.md`, `grep -r ... ~/.junees/skills`, `< 파일`)와, 그 안의 파일을 프로그램으로 실행하는 것(`~/.junees/skills/x/scripts/run.sh`, `bash`/`sh`/`source`의 스크립트 인자)은 허용한다. 위 목록 밖의 프로그램은 읽기만 해도 막힌다(`cp ~/.junees/memory/user.md /tmp/x`. 이럴 때는 `cat`이나 `read_file`을 쓴다).
- Junees 자신의 상태를 바꾸는 하위 명령(`src/tools/self_command.rs`의 표, 지금은 `junees cron add`, `remove`/`rm`, `pause`, `resume`, `run`, `daemon`, `junees plugin install`, `enable`, `disable`, `remove`(`junees plugins ...`도), `junees webhook add`, `remove`/`rm`, `pause`, `resume`, `serve`, `test`, `run`, `junees mcp login`, `logout`): 모델이 bash로 작업을 예약하면 사람 없이 계속 돌고(봇이 볼 수 있는 아무 디스코드 채널로 보낼 수도 있다), 지우거나 멈추면 사용자의 작업이 망가진다. 모델이 임시 폴더에 쓴 플러그인을 설치하고 켜면 다음 세션부터 그 프로그램이 돌고(훅은 모든 도구 호출과 답을 본다), 사용자가 정책용으로 켠 훅 플러그인을 끌 수도 있다. 데이터 폴더 규칙은 경로를 쓰는 명령만 알아보므로 이 명령들을 따로 알아본다. 프로그램이 `junees`(어느 폴더든, `/usr/local/bin/junees`, `./target/debug/junees`), 지금 실행 중인 바이너리의 파일 이름, 또는 그 바이너리를 가리키는 경로(상대 경로는 현재 폴더 기준, 링크를 따라감)이고 그 뒤 단어가 표의 하위 명령이면 해당한다. 아래 검사 방식대로 `env junees ...`, `nohup`/`timeout` 같은 래퍼, `bash -c '...'`/`sh -c`, `eval`, `;`/`&&`/`||`/파이프, `$(...)`, 셸에 넘긴 here-document와 파이프 스크립트 안까지 본다. 사람에게 물을 수 없는 실행(`-p`, `task`, 예약 작업 실행, `JUNEES_APPROVAL=off`인 대화)에서는 막고(`... which changes Junees's own state: only the user does that, and no one is here to approve it`), 승인 게이트가 있는 대화와 디스코드 게이트웨이에서는 막지 않고 `risky`에서도 묻는다(아래 실행 전 승인). 읽기만 하는 `junees cron list`, `junees cron`, `junees webhook list`, `junees webhook`, `junees plugin list`, `junees plugin`, `junees mcp`(도움말), `junees status`와 따옴표 안 문자열(`grep 'junees cron add' README.md`)은 허용한다. claude-code 백엔드에는 같은 표로 `Bash(junees cron add)`, `Bash(junees cron add *)` 같은 거절 규칙을 붙인다(Claude Code는 묻지 않으므로 늘 거절).
- `gh`(GitHub CLI)로 GitHub에 쓰는 명령(`src/tools/gh_command.rs`의 표): `gh pr merge`, `review`, `close`, `comment`, `create`, `edit`, `reopen`, `ready`, `lock`/`unlock`, `gh issue create`/`comment`/`close`/`reopen`/`edit`/`delete`/`transfer` 등, `gh release create`/`edit`/`delete`/`upload`, `gh repo create`/`delete`/`edit`/`rename`/`archive`/`fork`/`sync` 등, `gh workflow run`, `gh run rerun`/`cancel`/`delete`, `gh secret`/`gh variable`의 `set`/`delete`, `gh label`, `gh gist`의 쓰기, 그리고 `gh api` 중 `-X`/`--method`(`-XPUT`, `--method=put` 포함)가 POST, PUT, PATCH, DELETE이거나, 메서드 없이 `-f`/`-F`/`--field`/`--raw-field`/`--input`으로 본문을 보내는 것(gh가 POST로 보낸다. `gh api graphql`은 읽기도 POST라서 묻는다). `-R`/`--repo`가 하위 명령 앞에 와도(`gh pr -R o/n merge 7`) 알아본다. 서버에 `gh`가 로그인돼 있거나 `GH_TOKEN`/`GITHUB_TOKEN`이 있으면, bash로 `github` 도구의 승인 질문과 무인 허용 목록(`github_allow_unattended`)을 건너뛰고 merge나 approve를 할 수 있기 때문이다. 사람에게 물을 수 없는 실행에서는 막고(`` `gh pr merge` merges a pull request on GitHub as the user: no one is here to approve it ... ``), 승인 게이트가 있는 대화와 게이트웨이에서는 `risky`에서도 묻는다(종류 `write to GitHub through gh`). merge(`gh pr merge`, 그리고 인자에 `/merge`나 `mergePullRequest`가 든 `gh api` 쓰기)는 종류가 `merge on GitHub through gh (asked every time)`라서 `github` 도구의 merge처럼 "항상 허용"으로 답해도 다음 merge는 다시 묻는다. 읽기(`gh pr view`, `gh pr diff`, `gh pr checks`, `gh api repos/o/n/pulls`, `gh api -X GET ...`)는 그대로 된다. claude-code 백엔드의 거절 규칙에는 넣지 않았다(그 백엔드에는 `github` 도구도 없고, Claude Code는 자기 `gh`를 쓸 수 있다. 위 GitHub 절)
- git과 GitHub CLI의 자격 증명 파일: `~/.git-credentials`, `~/.config/git/credentials`, `~/.config/gh/`(`XDG_CONFIG_HOME`이 절대 경로로 있으면 `$XDG_CONFIG_HOME/git/credentials`와 `$XDG_CONFIG_HOME/gh/`도)는 설정 파일과 똑같이 막는다(이름이나 글롭으로 가리키는 명령, read_file/write_file/edit_file, claude-code 백엔드의 `Read`/`Edit` 거절 규칙). git의 자격 증명 저장소와 `gh` 로그인은 사용자로 push하고 merge할 수 있다.
- 웹훅으로 시작한 실행(GitHub 흐름의 단계 포함)에서만: `git push`(`git -C x push`, `--git-dir=...`, 래퍼, `sh -c`, `eval`까지), `git send-pack`과 `git-send-pack`, `git subtree push`, 읽기가 아닌 `git config`(`--get`, `--get-all`, `--list`, `git config get`/`list`, `git config <이름>`은 허용), `git credential`과 `git-credential-*`, `-c`/`--config-env`의 `alias.*`, `credential*`, `url.*` 키. 웹훅 요청은 누구나 쓸 수 있는 글이라, 주입된 모델이 `git push origin FETCH_HEAD:main`으로 merge 조건을 건너뛰거나 `url.<x>.pushInsteadOf`로 흐름의 push를 다른 곳으로 돌리지 못하게 한다(push는 흐름 코드가 직접 한다). 그 실행의 bash는 git 자격 증명 없이 돈다: `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_0`/`GIT_CONFIG_VALUE_0`으로 빈 `credential.helper`(모든 설정 파일의 helper 목록을 비운다), `GIT_TERMINAL_PROMPT=0`, 실행마다 새로 만들고 끝나면 지우는 빈 임시 폴더(0700)를 가리키는 `GH_CONFIG_DIR`(`gh`는 로그인을 `~/.config/gh/hosts.yml`에서 스스로 읽으므로, 그 폴더에서는 `gh auth token`도 `gh auth git-credential`도 토큰을 내주지 않는다), 그리고 `SSH_AUTH_SOCK`, `GIT_ASKPASS`, `SSH_ASKPASS`, `GIT_CONFIG_PARAMETERS`, `GH_TOKEN`/`GITHUB_TOKEN`/`GH_ENTERPRISE_TOKEN`/`GITHUB_ENTERPRISE_TOKEN`을 뺀다. 같은 실행의 bash는 `gh auth`(모든 하위 명령)와 `gh alias`, 그리고 `GH_CONFIG_DIR`, `XDG_CONFIG_HOME`, `HOME`, 토큰 변수를 바꾸는 명령(`NAME=...` 대입, `export`/`declare`/`unset`/`read`/`for`, `env -u`, `env -i`, `exec -c`)을 거절한다(`sh -c`, `$(...)` 안까지). 빈 폴더만으로는 `GH_CONFIG_DIR=$HOME/.config/gh gh ...`나 키링의 토큰을 막지 못하기 때문이다. claude-code 백엔드도 같은 환경과 `Bash(git push *)`, `Bash(git -C * push*)`, `Bash(git config *)`(읽기도 막힌다), `Bash(git -c *)`, `Bash(git credential*)`, `Bash(gh auth *)`, `Bash(gh alias *)`, `Bash(*GH_CONFIG_DIR*)` 같은 거절 규칙을 받는다. 막지 못하는 것: 암호 없는 ssh 키 파일(`~/.ssh/id_*`)을 직접 쓰는 ssh, 홈 폴더 git 설정에 미리 있던 별칭, 스크립트 파일이나 다른 언어로 부르는 git과 gh(같은 사용자로 도는 프로그램은 `GH_CONFIG_DIR`을 되돌리고 `~/.config/gh/hosts.yml`이나 키링을 읽을 수 있다. 그래서 흐름의 `gh`/git 로그인은 봇 전용 계정을 권한다).
- 위 일곱 규칙은 비밀과 도구 검사를 지키는 것이라 `JUNEES_ALLOW_DANGEROUS=1`이어도 적용된다. 차단 목록일 뿐 샌드박스가 아니다: 다른 변수에 담은 경로(`d=~; cat $d/.junees/config.toml`), 실행 중에 만든 경로(`cat $(echo ~/.junees)/config.toml`), 더 위 폴더를 통째로 다루는 명령(`grep -r api_key ~`, `cp -r ~ /tmp/h`. 막으면 정상 작업까지 너무 많이 막힌다), 경로 없이 현재 폴더를 쓰는 명령(`cd ~/.junees && grep -r api_key`), 스크립트 본문 안에 쓴 경로(`python3 -c "open('/home/u/.junees/memory/user.md', 'a')"`), 파일로 써 둔 스크립트, 판정과 실제 열기 사이에 링크를 바꿔치기하는 경쟁은 막지 못한다. Junees 하위 명령도 같은 이유로 다음은 막지 못한다: 변수에 담거나 실행 중에 만든 프로그램 이름(`j=junees; $j cron add ...`, `$(which junees) plugin enable p --trust`), 다른 이름으로 복사한 바이너리나 같은 명령 안에서 만든 링크, `find -exec junees ...`와 `xargs junees ...`(`echo p | xargs junees plugin enable`), `watch`처럼 모르는 래퍼, `cargo run -- cron add ...`, 셸 함수와 alias, 스크립트 파일이나 다른 언어(`python3 -c "subprocess.run(['junees', 'plugin', 'install', ...])"`)로 부르는 것. claude-code 백엔드의 규칙은 쓰인 단어 그대로만 맞추므로 경로를 붙인 호출(`/usr/local/bin/junees cron add`), `env junees`, `bash -c '...'`도 지나간다. `gh` 쓰기 규칙도 같은 이유로 다음은 막지 못한다: `gh alias set m 'pr merge'` 뒤의 `gh m 7`(별칭), 변수에 담거나 실행 중에 만든 프로그램 이름(`g=gh; $g pr merge 7`), 다른 이름의 복사본이나 링크, `xargs gh ...`와 `find -exec gh ...`, 스크립트 파일이나 다른 언어(`python3 -c "subprocess.run(['gh', 'pr', 'merge', '7'])"`), 토큰으로 API를 직접 부르는 `curl -X PUT -H "Authorization: ..." https://api.github.com/...`(`gh auth token`으로 토큰을 꺼내 쓰는 것 포함)과 `hub` 같은 다른 클라이언트, 표에 없는 `gh` 확장 명령(`gh <extension>`)

검사 방식:

- 명령을 셸처럼 대략 나눠서 `;`, `&&`, `||`, `|`, `&`, 줄바꿈, 괄호로 이어진 각 부분, `$(...)`과 백틱 안, `sh -c '...'`, `eval`과 `env -S` 인자까지 검사한다. 셸이 표준 입력으로 읽는 스크립트도 검사한다: `bash <<'EOF'`의 here-document 본문, `bash <<< '...'`, `echo`/`printf`/here-document를 받은 `cat`을 파이프로 넘긴 `... | sh`. 앞에 붙은 `FOO=1`, `env`, `nohup`, `timeout 5`, `nice`, `setsid`, `stdbuf`, `ionice`, `chrt`, `taskset`, `busybox`, `command`, `then`/`do`, `function 이름` 같은 키워드와 래퍼는 건너뛰고 실제 실행되는 프로그램을 본다. `$'...'` 문자열은 이스케이프를 풀어서 본다. 프로그램 이름은 경로를 뗀 이름으로 비교한다(`/sbin/reboot`도 차단).
- 따옴표 안 문자열, 주석, 셸이 아닌 명령(`cat > notes.md <<'EOF'`, `python3 - <<'EOF'`)이 받는 here-document 본문은 명령으로 보지 않는다. `echo "rm -rf /"`, `grep shutdown log.txt`, `git commit -m "git push --force"`는 허용된다.
- 실행 중에 만들어지는 단어(`rm -rf $(echo /)`, 변수에 담긴 경로)는 알 수 없으므로 막지 못한다. `$(...)`, `sh -c`, `eval`이 합쳐서 8단계보다 깊게 중첩되면 검사하지 않고 막는다.
- `JUNEES_DENY_COMMANDS`의 접두사는 단어 단위로 비교한다. `docker rm`은 `docker rm -f web`을 막지만 `docker rmi`는 막지 않는다. 이 규칙은 `JUNEES_ALLOW_DANGEROUS=1`이어도 적용된다.
- `JUNEES_ALLOW_DANGEROUS`는 `1`/`true`/`yes`/`on` 또는 `0`/`false`/`no`/`off`만 받고, 다른 값이면 시작할 때 오류로 멈춘다.

### 실행 전 승인

대화 모드에서는 위험할 수 있는 도구 호출을 실행하기 전에 터미널에서 묻는다. 묻는 범위는 `JUNEES_APPROVAL`로 정한다.

- `risky`(기본): 명령 정책이 막지는 않지만 주의할 신호가 있는 `bash` 명령, 작업 폴더 밖 경로를 고치는 `write_file`/`edit_file`, 그리고 `trust`가 없는 MCP 서버와 플러그인의 도구(위 MCP 서버, 플러그인 절).
- `all`: 모든 `bash`, `write_file`, `edit_file`, MCP 도구, 플러그인 도구 호출. `read_file`은 묻지 않는다.
- `off`: 묻지 않는다(이전 동작).

`bash`의 주의 신호는 명령 정책과 같은 파서로 찾는다(`$(...)`, `sh -c`, `eval`, 셸에 파이프로 넘긴 스크립트, `timeout`/`env` 같은 래퍼, `find -exec` 안까지).

- 파일 삭제: `rm`, `rmdir`, `unlink`, `shred`, `find -delete`
- git: `git push`(모든 push), 작업을 버리는 명령(종류 `discard git work`: `git reset --hard`, `--`/`.`/`-f`가 붙은 `git checkout`, 작업 트리를 되돌리는 `git restore`(`--staged`만이면 제외), `git stash drop`/`clear`, `git branch -D`), `git clean`(`-n`/`--dry-run` 제외)
- 패키지 설치/제거: `apt`/`apt-get`/`dnf`/`yum`/`brew`/`pacman`/`apk` 등의 install/remove/upgrade, `pip install`, `python -m pip install`, `npm`/`pnpm`/`yarn`/`bun`의 install/add/ci/update, `cargo install`, `go install`, `gem install`, `uv pip install`/`uv add`, 그리고 레지스트리에서 받아 바로 실행하는 `npx`/`pnpx`/`bunx`/`uvx`
- 파일로 내려받기: `curl -o`/`-O`, `wget`(`-O -`처럼 표준 출력으로 받거나 `--spider`면 제외). 내려받은 스크립트를 셸에 바로 넘기는 것은 명령 정책이 막는다
- 권한 변경: `chmod`, `chown`, `chgrp`, `chattr`, `setfacl`
- 작업 폴더 밖에 쓰기: 리다이렉트(`>`, `>>`, `2>`, `&>`; `2>&1`과 `/dev/null` 같은 장치는 제외), `cp`/`ln`/`install`의 대상(`-t` 포함), `mv`/`tee`/`touch`/`mkdir`/`truncate`의 인자, `sed -i`의 파일, `dd of=`, 삭제 명령의 대상
- Junees 자신의 상태를 바꾸는 하위 명령(`junees cron add` 등, 위 명령 정책): 종류 `change Junees's own state`. 승인 게이트가 있을 때만 명령 정책이 통과시키고, 이 신호로 `risky`에서도 묻는다
- `gh`로 GitHub에 쓰는 명령(`gh pr merge`, `gh pr review`, `gh api -X POST` 등, 위 명령 정책): 종류 `write to GitHub through gh`, merge는 `merge on GitHub through gh (asked every time)`("항상 허용"이 덮지 않는다). 승인 게이트가 있을 때만 명령 정책이 통과시키고, 이 신호로 `risky`에서도 묻는다
- `JUNEES_ALLOW_DANGEROUS=1`일 때는 기본 규칙이 막았을 명령(`sudo` 등)도 주의 신호로 묻는다

작업 폴더는 junees를 실행한 폴더다. 경로는 그 폴더 기준으로 풀고 이미 있는 부분은 심볼릭 링크를 따라가서 판단한다(폴더 안의 링크가 밖을 가리키면 밖). 커널처럼 링크를 먼저 따라간 뒤 `..`를 적용하므로, `out`이 밖의 `~/elsewhere/sub`를 가리키면 `out/../x`는 `~/elsewhere/x`(밖)다. 한 명령 안의 `cd`/`pushd`/`popd`는 따라간다. `cd sub && touch a`는 안, `cd ~ && echo x >> .bashrc`는 밖이다. bash처럼 `-P`가 없으면 `cd`의 `..`는 글자대로 지운다. `cd -`나 `cd "$DIR"`처럼 갈 곳을 알 수 없으면 그 뒤의 상대 경로는 모두 밖으로 친다. 파이프라인이나 `( ... )` 안의 `cd`도 뒤 명령에 이어지는 것으로 보고, `cd`가 있는 명령의 `$(...)` 안은 위치를 모르는 것으로 본다(더 묻는 쪽으로만 틀린다). `/tmp`와 `TMPDIR`은 안으로 친다. `~`, `$HOME`은 홈으로 풀고, 그 밖의 변수나 `` ` ``가 들어간 경로, 다른 사용자의 홈(`~user`), 대상이 없는 심볼릭 링크는 알 수 없으므로 밖으로 친다. 명령 정책이 막는 명령은 묻지 않고 그대로 막힌다.

질문은 표준 오류에 도구 이름, 명령이나 경로, 이유를 보여 준다. 모델이 쓴 글자 가운데 제어 문자(탭 제외)와 양방향 서식 문자(U+202E 등)는 `\r`, `\u{1b}`처럼 글자로 바꿔 보여 주므로, `\r`이나 ESC로 질문 줄을 지우고 다른 명령처럼 보이게 할 수 없다. `edit_file`이면 바꿀 부분(`- 원래`, `+ 새 내용`, 각 8줄까지)을, `write_file`이면 새 파일인지 덮어쓰는지와 내용 앞부분을 함께 보여 준다.

```
  ?? approve bash: rm marker
     * `rm` deletes files
     always allows: bash: delete files
  [y] run  [n] decline  [a] always allow this kind this session  [e] decline with a reason >
```

- `y` 실행, `n` 거절, `a` 이번 실행 동안 같은 종류(도구별: 예 `bash: delete files`, `edit_file: write outside the working folder`)는 묻지 않고 허용, `e` 거절 이유 입력(`e 이유`처럼 한 줄로도 된다). 다른 입력이면 다시 묻는다.
- 거절하면 도구를 실행하지 않고 `the user declined: <이유>`(이유가 없으면 `no reason given`) 오류 결과를 모델에 돌려줘서 모델이 계획을 바꾸게 한다. Ctrl-D(입력 끝)도 거절이다.
- 질문을 기다리는 중 Ctrl-C는 그 호출을 실행하지 않고 턴을 취소한다(실행 중인 도구를 Ctrl-C로 취소할 때와 같이 그 호출과 남은 호출을 `cancelled by the user` 결과로 채운다). 질문과 프롬프트가 표준 입력을 함께 쓰므로 그 뒤에 입력한 줄은 프롬프트로 간다.
- "항상 허용"은 junees를 끝낼 때까지 유지된다(`/new`로 새 세션을 시작해도, `/sessions`로 다른 세션으로 옮겨도 남는다).
- 표준 입력이나 표준 오류가 터미널이 아니면(파이프, `2>log`) 대화 모드라도 물을 수 없으므로(질문이 보이지 않는다) 시작할 때 안내를 띄우고, 승인이 필요한 호출은 실행하지 않고 `not run: this call needs the user's approval ...` 오류 결과를 돌려준다. 그런 호출까지 실행하려면 `JUNEES_APPROVAL=off`.
- 질문은 모델 응답이 끝난 뒤, 도구 호출 줄(`-> bash ...`) 다음에 나온다. 스트리밍으로 찍던 텍스트 줄은 그 전에 끝나므로 질문이 텍스트 줄 중간에 붙지 않는다. stdout이 터미널이 아니면(`junees | tee log`) 그 응답의 텍스트는 도구 호출 줄 직전에 한 번에 찍힌다.
- `-p`와 `task` 모드는 사람이 없으므로 묻지 않는다(명령 정책만 적용).
- `risky`는 흔한 실수를 막는 장치이지 보안 경계가 아니다. 명령 문자열만 보고 판단하므로 `rsync`, `tar -C`, `unzip -d`, `perl -pi`, `python3 -c "open(...)"`, `crontab -r`처럼 따로 알지 못하는 명령의 쓰기는 묻지 않고, 작업 폴더 안의 `write_file`은 묻지 않으므로 스크립트를 써 두고 `bash run.sh`나 `make`로 실행하면 어떤 검사든 피할 수 있다. 그 경계가 필요하면 `JUNEES_APPROVAL=all`을 쓴다.

### 점검 (junees doctor)

```sh
junees doctor             # 항목마다 ok / warn / FAIL / --(해당 없음)과 고치는 방법 한 줄
junees doctor --json      # 같은 내용을 JSON으로
junees doctor --online    # API 키 확인 요청과 claude auth status까지
```

대화 안에서는 `/doctor`(네트워크 없이, `junees doctor`처럼 설정을 그때 다시 읽으므로 대화 중에 고치거나 깨뜨린 `config.toml`도 보인다). 실패(`FAIL`)가 하나라도 있으면 종료 코드 1, 경고만 있으면 0이다.

점검하는 것:

- 상태 폴더: `$JUNEES_HOME`이 내 것이고 `0700`인지, `config.toml`, `mcp.json`, `cron/jobs.json`, `webhooks/routes.json`, `secrets/` 안 파일이 내 것이고 `0600`인지(`secrets/` 아래 하위 폴더는 `0700`인지 보고 그 안을 다시 확인한다). 다른 사용자가 바꿀 수 있으면 실패, 읽기만 할 수 있으면 비밀이 들 수 있는 파일(`config.toml`, `mcp.json`, `secrets/*`)은 실패, 나머지는 경고. 고치는 줄은 `chmod 600 <경로>`처럼 그대로 실행할 수 있는 명령이다.
- 설정: `config.toml`이 읽히는지(모르는 키, 따옴표 없는 값은 줄 번호와 함께), 모든 값이 읽히는지(`Config`, 백엔드, `timezone`, cron/웹훅 한도를 실제 실행과 같은 함수로 읽는다). 파일이 깨져 있어도 점검은 멈추지 않고 파일 없이 나머지를 본다.
- 키: API 키, GitHub 토큰, 알림 웹훅이 있는지와 어디서 왔는지(환경 변수 / config.toml)만. 값은 어떤 경우에도 출력하지 않는다. 오류 문장이 값을 인용하더라도(예: 토큰을 `discord_allowed_users`에 잘못 넣은 경우) 출력 전에 보고서에서 세 가지를 지운다: 설정 오류가 `` got `...` ``로 인용한 값은 길이만 남기고, 비밀 값(API 키, 디스코드 토큰은 `Bot `을 뗀 것도, GitHub과 `gh` 토큰, 알림 웹훅, `CLAUDE_CODE_OAUTH_TOKEN`·`ANTHROPIC_AUTH_TOKEN` 같은 Claude Code 자격 증명, 점검이 읽은 웹훅 시크릿)은 통째로든 12자 이상 조각으로든 `[hidden]`으로 바꾸고, 그래도 자격 증명처럼 보이는 단어(메모리와 같은 `secrets.rs` 규칙)도 `[hidden]`으로 바꾼다. api 백엔드에 키가 없으면 실패, claude-code 백엔드면 해당 없음.
- claude-code 백엔드: `claude --version`(2.1.259 이상, 실행하는 프로그램은 이것 하나), 로그인(`claude auth status`는 `--online`일 때만), `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`이 켜져 있는지, 켜져 있으면 `bwrap`이 PATH에 있는지와 비특권 user namespace를 막는 커널 설정(`kernel.apparmor_restrict_unprivileged_userns = 1`, `kernel.unprivileged_userns_clone = 0`, `user.max_user_namespaces = 0`). 백엔드가 claude-code면 문제는 실패, api면 경고이고, api인데 `claude`도 없으면 이 부분은 건너뛴다. setuid root인 `bwrap`은 user namespace가 필요 없으므로 제한이 있어도 ok.
- 프로그램: `git`(없으면 경고: 작업 모드와 GitHub 흐름이 쓴다), `gh`(선택).
- 디스코드: 토큰과 허용 사용자(게이트웨이가 시작할 때와 같은 검사). 둘 다 없으면 해당 없음.
- 텔레그램: 토큰 모양과 허용 사용자(게이트웨이가 시작할 때와 같은 검사, 토큰은 `[hidden]`). 둘 다 없으면 해당 없음. 토큰이 맞는지(`getMe`)는 `--online`에서도 묻지 않는다.
- 예약 작업과 웹훅: `jobs.json`/`routes.json`이 읽히는지, 기다리는 작업이나 멈추지 않은 경로가 있는데 `junees cron daemon`/`junees webhook serve`가 돌지 않으면 경고(잠금 파일이 잡혀 있는지로 판단한다. 비어 있으면 잠깐 잡았다 바로 놓는다). 경로마다 시크릿 파일이 있고 내 것이고 `0600`인지(값은 읽기만 하고 출력하지 않음), 환경 변수 시크릿은 지금 환경에 없으면 경고(서버의 환경은 다를 수 있다).
- 세션: `$JUNEES_HOME/sessions`의 파일 수와 크기, 1 GiB를 넘으면 경고.
- MCP: `mcp.json`이 읽히는지(`url`과 `headers` 형식 포함), 켜진 서버의 `command`가 PATH(서버 `env`에 PATH가 있으면 그것)나 그 경로에 실행 파일로 있는지, `cwd`가 폴더인지, `url` 서버의 헤더가 참조하는 환경 변수가 있는지(값은 출력하지 않는다), 저장된 OAuth 로그인(`login <서버>`: 있는지, 이 `url`의 것인지, 0600인지, 발급자와 만료, 갱신할 수 있는지. 만료됐는데 리프레시 토큰이 없으면 경고, 쓸 수 없으면 실패. 토큰 값은 `[hidden]`). 서버를 실행하거나 HTTP 서버에 연결하지는 않는다.
- 플러그인: 켜진 플러그인의 폴더와 `plugin.json`이 실제 실행 때와 같은 검사(소유자, 권한, manifest 형식, 프롬프트 파일)를 통과하는지.
- `--online`: 키가 있으면 `GET <base_url>/v1/models?limit=1`을 한 번 보낸다(토큰을 쓰지 않는다). 2xx면 ok, 401/403이면 실패, 그 밖이나 연결 실패는 경고(URL은 출력하지 않고 호스트만). 켜진 `url` MCP 서버마다 로그인 때와 같은 인가 서버 찾기만 한다(`login <서버> (online)`: 토큰 없는 `server/discover` 하나와 메타데이터 GET. 등록, 로그인, 토큰 요청은 하지 않는다). 로그인을 요구하지 않는 서버는 건너뛰고, PKCE가 없거나 클라이언트를 얻을 길이 없으면 경고.

점검은 읽기만 한다: 파일을 만들거나 쓰지 않고, 잠금을 붙잡고 있지 않으며, MCP 서버와 플러그인 명령을 실행하지 않으며, 기본으로 네트워크를 쓰지 않는다.

Linux에서 Claude Code의 환경 정리(`CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1`)는 bubblewrap이 있어야 시작하고, bubblewrap은 비특권 user namespace를 만든다. Ubuntu 23.10 이후처럼 `kernel.apparmor_restrict_unprivileged_userns = 1`이면 bubblewrap을 설치해도 AppArmor가 막으므로, `bwrap`에 `userns`를 허용하는 프로필을 더한다:

```sh
sudo apt install bubblewrap
sudo tee /etc/apparmor.d/bwrap >/dev/null <<'EOF'
abi <abi/4.0>,
include <tunables/global>
profile bwrap /usr/bin/bwrap flags=(unconfined) {
  userns,
  include if exists <local/bwrap>
}
EOF
sudo apparmor_parser -r /etc/apparmor.d/bwrap
bwrap --ro-bind / / true && echo ok    # 확인
```

그럴 수 없으면 `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=0`으로 정리 없이 돌릴 수 있다(Claude Code가 실행하는 명령이 그 자격 증명을 볼 수 있다. 시작 때 경고).

## 구조

```
src/
  main.rs            CLI 진입점 (처음 실행 설정과 junees setup, 대화 모드와 슬래시 명령(api 에이전트와 Claude Code 대화를 Engine으로 전환), -p 실행, task 명령(계획 + 실행), 세션 열기/이어 가기(백엔드별), SIGINT/SIGTERM 처리)
  agent.rs           에이전트 루프 (기록 변경을 세션 기록에 남김, 도구 결과 비우기/압축/초과 시 재시도, 취소, 모델 호출 전 한도 확인, 도구 호출 전 승인 훅 ToolGate, 턴이 끝난 뒤 훅 AfterTurn, 스트리밍 이벤트 전달, pause_turn 이어 가기와 인용 출처 보고, delegate 호출 실행(서브에이전트와 provider, 도구, 취소, 승인, 한도 공유))
  approval.rs        실행 전 승인 (JUNEES_APPROVAL, 묻는 범위 판단(MCP 외부 도구, GitHub 쓰기 포함, merge는 매번), 터미널 질문, 세션 동안 항상 허용, 프롬프트와 공유하는 표준 입력, 서브에이전트 질문 표시(check_for), 묻지 않는 호출은 기다리지 않고 질문만 하나씩)
  cancel.rs          취소 플래그 (시그널 처리기와 에이전트 루프가 공유)
  session.rs         세션 기록(JSONL) 쓰기, 재생, 이어 가기, 세션 찾기/목록(백엔드별, 디스코드 채널 세션과 예약 작업 세션은 -c와 번호 목록에서 뺌), 비우기/압축 지점 계산, Claude Code 턴 기록, 서브에이전트 기록(create_sub)과 사용량
  context.rs         압축 요약 요청(시스템 프롬프트, 앞부분을 텍스트로 옮기기)
  claude_code/       claude-code 백엔드 공용 부분 (대화와 작업이 함께 씀)
    mod.rs             백엔드 설정, claude --version/auth status 확인, 환경 정리와 CLAUDE_CODE_SUBPROCESS_ENV_SCRUB, 세션 실행(Spawn)과 예산/취소, 프로세스 그룹과 자손 종료
    stream.rs          stream-json 읽기 (텍스트 조각 스트리밍, 도구 호출/결과, 거절된 호출, 응답별 사용량, 로그인 실패와 사용 한도 판단)
    rules.rs           명령 정책과 설정 파일 보호를 Claude Code 권한 규칙(--disallowedTools)으로
    chat.rs            구독으로 대화하기 (턴마다 claude -p, --resume으로 이어 가기, 턴 기록, 메모리와 스킬 절을 한 --append-system-prompt로)
  task/              장기 작업 모드
    runner.rs          task init/run 세션 루프, 체크포인트 커밋, 정지 조건, 크래시 복구, 실행 로그, Claude Code 세션 뒤 하네스 verify
    claude_code.rs     claude-code 백엔드의 작업 세션 인자(acceptEdits, 시스템 프롬프트)와 실행
    claude_tests.rs    가짜 claude로 claude-code 백엔드의 init/run 검증(단위 테스트)
    budget.rs          토큰/시간 예산, 기간 파싱(90s, 30m, 2h)
    lock.rs            동시 실행 방지(저장소 루트 flock)
    features.rs        features.json 검증, 하네스의 권위 있는 사본(Ledger)
    tools.rs           mark_feature_passing (verify 명령을 하네스가 실행)
    state.rs           state.json (세션 수, 무진전 횟수, 토큰, 정지 이유와 시각, 목표, 실행 중 표시, 기능 목록 사본)
    status.rs          task status (읽기 전용, 텍스트/JSON)
    prompt.rs          초기화/작업 세션 프롬프트
    git.rs             저장소 확인, 커밋, 로그
  message.rs         대화 메시지 타입 (Messages API 형식, 이미지 블록과 이미지가 든 도구 결과, 모르는 블록은 원본 JSON 그대로, 텍스트의 citations 보존, 서버 도구 사용량)
  images/            이미지 입력
    mod.rs             형식 판별(매직 바이트), 헤더에서 크기 읽기, 상한, base64, 토큰 추정, /image와 --image 파일 읽기
    store.rs           세션 기록의 이미지를 세션 폴더 파일로 저장하고 이어 갈 때 다시 읽기(sha256 확인)
    discord.rs         디스코드 첨부 중 이미지 고르기, CDN에서만 내려받기(리다이렉트 제한, 시간, 크기)
  web_search.rs      웹 검색 서버 도구 설정과 도구 정의, 인용 출처 목록, 압축용 검색 블록 요약
  notify.rs          디스코드 웹훅 알림 (정지, 세션마다)
  provider/          LLM 제공자 트레이트와 Anthropic 클라이언트 (프롬프트 캐싱 표시, 확장 사고 설정, 스트리밍 재시도와 대기 제한)
    stream.rs          SSE 이벤트 분리와 응답 조립 (스트리밍하지 않은 응답과 같은 값)
  tools/             도구 트레이트, 레지스트리, bash/read_file/write_file/edit_file, bash 명령 정책과 주의 등급, 작업 폴더 판단(policy.rs), 도구와 정책이 설정 파일(config.toml, mcp.json), 시크릿 폴더(secrets)와 쓰기 금지 데이터 폴더(memory, projects, skills, cron, plugins, webhooks, flows)를 알아보는 판단(settings_file.rs), 명령 안의 Junees 상태 변경 하위 명령 표와 판단(self_command.rs), 명령 안의 `gh` GitHub 쓰기 표와 판단(gh_command.rs), 웹훅 실행의 지침 파일 이름 쓰기 거절(instruction_files.rs), 도구 호출 전 훅 PreTool, 테스트 전용: 테스트 bash의 sandbox(bash.rs의 `testing::confine`, 환경 변수 수준이라 절대 경로와 프록시를 무시하는 프로그램은 막지 못함)와 테스트 코드가 홈 삭제나 origin push를 실제 셸로 돌리지 않는지, sandbox 밖(바이너리 테스트, 모델 응답으로 부르는 bash)에서는 원격과 상관없는 모든 `git push`, `.`/`..`/`/`/`*`의 재귀 삭제, `git reset --hard`, `git clean -f`도 돌리지 않는지, `Command::new("git")` 인자에 `push`가 있으면 임시 폴더의 bare 저장소로만 미는 헬퍼(SAFE_PUSH)를 거치는지, sandbox 밖 실제 bash 테스트가 허용 목록(파일과 모듈 경로까지 맞춘 테스트, 무해한 명령 또는 명령 없음)만 쓰는지 보는 소스 스캔(test_scan.rs)
  mcp/               MCP 클라이언트 (stdio, Streamable HTTP)
    config.rs          $JUNEES_HOME/mcp.json 읽기와 검사 (0600, 자기 소유, 값을 되풀이하지 않는 오류, command와 url 중 하나, 숨길 헤더 변수 목록)
    conn.rs            서버 프로세스(프로세스 그룹, 환경 정리)와 JSON-RPC 2.0 줄 메시지, 요청 id와 답 대기, 취소 알림, 서버 요청(ping) 답, stderr 꼬리, 종료(입력 닫기 -> SIGTERM -> SIGKILL)
    server.rs          server/discover 탐색과 initialize 대체(dual-era), tools/list 페이지, tools/call과 시간 한도, 상태와 다시 띄우기
    tool.rs            mcp__<서버>__<도구> 이름 정리와 충돌, 설명/스키마 이스케이프, 결과를 ToolOutput으로
    link.rs            Link: 실행 중인 서버에 닿는 전송(stdio Connection 또는 HttpConnection)
    http/              Streamable HTTP 전송
      mod.rs             HttpConnection: 요청마다 POST, JSON/SSE 응답, 두 세대 핸드셰이크(server/discover, 4xx면 initialize), 세션 id와 404 뒤 새 세션, 취소(스트림 닫기 / notifications/cancelled), 응답 크기 상한, 리다이렉트 거절, URL과 헤더 값 없는 오류, 종료 DELETE
      target.rs          url(https, http는 루프백만, 사용자 정보 금지)과 headers(env:NAME 참조만, 전송 헤더 금지), 값 읽기(민감 표시), origin 표시
      meta.rs            Mcp-Method/Mcp-Name, x-mcp-header 검사와 Mcp-Param-* 값, Base64 표시 인코딩
      sse.rs             응답 스트림의 SSE 이벤트 읽기(조각 경계, CRLF, 주석, message 이벤트만)
      tests.rs           루프백 가짜 HTTP MCP 서버(두 세대, JSON/SSE)로 핸드셰이크, 헤더, 세션, 404, 시간 초과와 취소, 크기 상한, 리다이렉트, 비노출 검증
    oauth/             url 서버의 OAuth 인가 (MCP 2026-07-28 Authorization)
      mod.rs             흐름 요약, 로그인 상태(/mcp와 doctor용, 토큰 없이)
      config.rs          mcp.json `oauth` (client_id, client_secret env 참조, scope, issuer)
      challenge.rs       WWW-Authenticate 파싱 (Bearer의 resource_metadata, scope, error)
      discovery.rs       토큰 없는 probe, 보호 자원 메타데이터(RFC 9728)와 resource 확인, 인가 서버 메타데이터(RFC 8414/OIDC, 발급자 일치, S256)
      fetch.rs           메타데이터/등록/토큰 요청 (https, 루프백 서버에만 루프백 http, 리다이렉트 거절, 1 MiB, URL 없는 오류, OAuth 오류 문구)
      pkce.rs            PKCE S256과 무작위 state
      token.rs           동적 클라이언트 등록(RFC 7591), 코드 교환과 갱신(resource 포함), 클라이언트 인증(none/basic/post), 토큰 응답 검사
      redirect.rs        127.0.0.1 한 번 수신기(state 검사), 붙여 넣은 주소 읽기
      login.rs           junees mcp login 흐름 (scope 선택과 step-up, iss 확인, 저장), 로그아웃
      store.rs           secrets/mcp-oauth/<서버>.json (0600/0700, 원자적 쓰기, 갱신/로그인/로그아웃 잠금)
      auth.rs            저장된 로그인 사용: Bearer 헤더, 만료 전과 401 뒤 한 번 갱신(로그아웃/다른 주소의 새 로그인이면 갱신 안 함), 다른 주소면 쓰지 않음, 403 scope 기억
      cli.rs             junees mcp login/logout 인자와 터미널 입출력
      tests.rs           루프백 가짜 인가 서버+MCP 서버로 전체 로그인(수신기, 붙여 넣기), PKCE, state/iss 거절, 갱신, 갱신과 로그아웃/재로그인 겹침, 잠금 대기, step-up, 무인 실행, 주소 바뀜, 헤더 우선, 비노출
    mod.rs             Mcp: 서버 묶음 시작/보고/종료, 도구 등록, 승인용 외부 도구 목록, /mcp 보기(전송 종류, 로그인 상태), 로그인 뒤 다시 띄우기
  plugins/           플러그인 ($JUNEES_HOME/plugins/<이름>/plugin.json)
    manifest.rs        plugin.json 읽기와 검사 (도구, 훅, 프롬프트, 명령 경로, 시간 한도, 0600/자기 소유, 값을 되풀이하지 않는 오류)
    process.rs         명령 실행 (플러그인 폴더, 프로세스 그룹, 환경 정리, 표준 입력 JSON, 시간 한도와 그룹 종료, 출력 머리와 꼬리)
    tool.rs            명령 도구를 Tool로 (설명/스키마 이스케이프, 결과와 오류)
    hooks.rs           pre_tool(종료 코드 2면 거절)과 post_turn 훅, 실패 경고
    state.rs           켜짐/trust 상태 파일 plugins/state.json (0600, 원자적 쓰기)
    manage.rs          junees plugin list/install/enable/disable/remove (복사, 링크 거절, 크기 제한)
    mod.rs             Plugins: 켜진 플러그인 읽기, 도구와 훅 등록, 승인용 외부 도구 목록, 프롬프트 절(상한), 시작 보고와 /plugins
    tests.rs           실제 플러그인 폴더와 sh/python3 명령으로 도구 실행/시간 초과/오류, 이름 정리, 승인 질문, 훅 거절/경고, post_turn, 프롬프트 상한, 켜짐 상태, 설치, 서브에이전트 검증
  config.rs          설정 값 읽기와 검사 (from_lookup 이음새, 도구 설정 ToolSettings 포함)
  config_file.rs     설정 파일 config.toml (단순 TOML 읽기/쓰기, 0600), Settings: 실행 중 변경 -> 환경 변수 -> 파일 순서로 찾기
  setup.rs           설정 마법사 (Prompter 트레이트, Claude Code 확인 ClaudeProbe, 키 입력 때 터미널 echo 끄기 EchoOff)
  slash.rs           대화 안 슬래시 명령 파싱, /sessions 번호 고르기
  usage/             사용량과 비용 (/usage, junees usage)
    mod.rs             기록 종류(Kind)와 사용량 기록(Record), 합계와 비용 표시(Totals)
    prices.rs          모델 가격표(출처와 확인 날짜), prices 설정 덮어쓰기, 비용 계산(캐시 쓰기/읽기, 긴 프롬프트, 웹 검색)
    scan.rs            기록 파일에서 사용량 줄만 읽기(서브에이전트 기록은 부모 subagent_usage로), 기간과 폴더 범위
    report.rs          /usage의 세션 표(기록에 없는 사용량 포함)와 junees usage의 기간 표(날짜/모델/폴더/출처별), 안내 줄
    args.rs            junees usage 옵션과 도움말
    tests.rs           가격 계산, 덮어쓰기, 모르는 모델, 기록 종류별 합산, 깨진 줄, 날짜 경계(UTC와 시간대), 범위와 정렬 검증
  search/            세션 검색 (session_search 도구, /search, junees sessions search)
    mod.rs             검색 옵션과 결과, 상한(결과 수, 스니펫, 줄과 텍스트 크기, 시간), 출처 이름
    query.rs           질의 파싱(낱말 AND, 따옴표 구절), 대소문자 무시 매칭, 원래 글자로 스니펫
    scan.rs            기록 파일 목록(서브에이전트 포함, 새것부터), 큰 줄을 담지 않는 줄 읽기, 범위 판단, 항목에서 찾을 글 고르기
    format.rs          모델용 결과(무작위 태그, 데이터 안내)와 사람용 번호 목록, 읽지 못한 것 안내
    tool.rs            session_search 도구(입력 검사, 채널 세션 허용 여부)와 등록
    args.rs            /search와 junees sessions search의 옵션
    tests.rs           실제 기록 파일로 매칭, 정렬과 상한, 폴더/출처 범위, 깨진 줄과 큰 줄, 시간 상한, 도구 출력 검증
  skills/            스킬 ($JUNEES_HOME/skills/<이름>/SKILL.md)
    mod.rs             이름과 앞머리 검사, 목록(Index)과 상한, 모드별 시스템 프롬프트 절
    store.rs           읽기/만들기/패치/파일 쓰기와 지우기/휴지통, 경로 탈출과 심볼릭 링크 거절, 크기 제한, 원자적 쓰기(0600/0700)
    tools.rs           skill_view, skill_manage 도구와 등록
  subagent/          서브에이전트 (delegate 도구)
    mod.rs             설정(JUNEES_SUBAGENTS 등), 도구 정의와 입력 검사, 시스템 프롬프트 안내, 서브에이전트 이름을 붙여 부모 게이트로 묻는 ChildGate
    run.rs             과제 실행(동시 실행 상한, 과제 순서 보존), 자기 한도와 부모 예산, 진행 한 줄, 서브에이전트 기록, 결과 크기 제한
    tests.rs           Scripted provider로 병렬 상한과 순서, 재귀 금지, 예산 차감, 취소, 자식 기록, 승인 공유(한 자식이 답을 기다리는 동안 다른 자식의 묻지 않는 호출은 실행), 결과 크기 검증
  prompt.rs          시스템 프롬프트, 메모리와 스킬 절(data_sections), Claude Code에 넘기는 인자(claude_append, 지침 파일을 더한 claude_append_with_context, 128 KiB 한도)
  secrets.rs         비밀처럼 보이는 내용 찾기, 메모리와 스킬이 함께 씀 (키 접두사, 개인 키, AWS 키 id, 디스코드/텔레그램 봇 토큰, 긴 hex/토큰, 몇째 줄인지)
  prompt_text.rs     저장된 텍스트(메모리, 스킬)의 제어/방향 문자 판단: 쓸 때 거절(check_plain), 읽을 때 이스케이프(escape_hidden)
  context_files/     프로젝트 지침 파일 (AGENTS.md/JUNEES.md/CLAUDE.md를 저장소 맨 위나 HOME까지 찾기, 파일당/전체 상한, 링크 판단, 시스템 프롬프트 절, /context 목록, Claude Code의 AGENTS.md 플러그인 끄는 --settings, 웹훅 실행용 마지막 커밋 읽기와 커밋되지 않은 변경 비교(committed.rs))
  memory/            영구 메모리
    mod.rs             저장소 셋(user, notes, 프로젝트별 notes) 읽기/쓰기(0600, 원자적, flock), 항목 추가/교체/삭제와 상한, 프롬프트 절(대화, task, Claude Code 읽기 전용), /memory 보기
    tool.rs            memory 도구 (task 세션은 project만)
  cli.rs             인자 파싱(위치 인자 질문, task/status/setup/gateway/cron/webhook/doctor 하위 명령, 예전 플래그), 도움말, 출력 보조 함수
  gateway/           디스코드 게이트웨이 (junees gateway)와 게이트웨이 공통 엔진
    mod.rs             Gateway::run: Get Gateway Bot, 연결 태스크, 채널 LocalSet, 종료 처리, 로그(Log), 게이트웨이 잠금(lock_file)
    platform.rs        Platform 트레이트: 엔진이 플랫폼에 바라는 것(보내기, 타이핑, 이미지 받기, 명령/도구 줄/승인 질문 표기)
    channels.rs        공통 엔진: 채널별 작업자(대기열, 동시 실행 상한, 타이핑, 도구 줄 묶기, 취소), 명령 실행(new/stop/status/memory/...), 첨부 받기, 세션 열기
    config.rs          discord_* 설정 (토큰 Token은 Debug에서 숨김, 허용 사용자 필수, https/wss 확인)
    shard.rs           Gateway v10 WebSocket: Hello/Identify/Ready, 하트비트(지터, ACK 누락), Resume, Reconnect, Invalid Session, 닫힘 코드, 지수 대기
    rest.rs            REST: Get Gateway Bot, Create Message(allowed_mentions 없음), 타이핑, 경로별 레이트 리밋과 429 재시도
    bot.rs             디스코드 쪽: 메시지 고르기(허용 사용자, DM/멘션/지정 채널/스레드), ! 명령 읽기, Platform 구현(Discord)
    engine.rs          Conversation/Engines 트레이트, api 에이전트와 Claude Code 대화, 이벤트를 텍스트 블록과 도구 한 줄로(Collector)
    ask.rs             채널 승인 질문(다음 메시지로 답, 시간 초과 거절)
    sessions.rs        채널 -> 세션 색인(discord-sessions.json, 다른 게이트웨이는 자기 파일과 source), 세션 이어 가기/새로 만들기
    split.rs           2000자 나누기(줄 단위, 코드 블록 닫고 다시 열기, UTF-16 길이), 평문용 split_plain(울타리 없이)
  telegram/          텔레그램 게이트웨이 (junees gateway telegram)
    mod.rs             TelegramGateway::run: getMe, 롱 폴링 Poller(offset 다시 계산, 최근 id로 중복 거름, 409 몇 번 기다리기, 끝날 때 확인), 채팅 LocalSet, 종료 처리
    config.rs          telegram_* 설정 (토큰 모양 검사와 Debug 숨김, 숫자 사용자 id 필수, API 주소는 api.telegram.org 또는 루프백)
    api.rs             Bot API: getMe/getUpdates/sendMessage/sendChatAction/getFile/파일 받기, URL과 토큰 없는 오류, 429 retry_after, 중복 전송 없는 재시도, 응답과 파일 크기 상한
    bot.rs             메시지 고르기(허용 사용자, 개인/그룹 멘션/답장/승인 답, 포럼 토픽), /명령 읽기(@봇, 멘션 뒤 명령, 다른 봇, /start, 캡션과 전달은 명령 아님), 전달 표시, 앨범 묶기와 나뉘어 온 그룹 앨범
    platform.rs        Platform 구현(Telegram): 평문 4096자, 타이핑, 사진/이미지 문서 고르기와 받기, 평문 승인 질문, 도움말
  cron/              예약 작업 (junees cron)
    tz.rs              시스템 시간대 파일(TZif)과 POSIX TZ 규칙 읽기, 현지 시각 <-> UTC(건너뛴/두 번 오는 시각), 날짜 계산
    schedule.rs        일정 문법(every, in, daily, weekdays, cron 식, ISO 시각)과 다음 실행 시각
    store.rs           jobs.json (flock, 원자적 쓰기, 0600/0700, 소유자/권한 확인), 작업 찾기(id, 앞부분, 이름)
    run.rs             실행 하나: -p와 같은 무인 경로(api 에이전트 또는 Claude Code), 읽기 전용 메모리/스킬, 시간/토큰 한도, 마지막 답, 실행을 시작한 쪽(Trigger: 예약 작업 또는 웹훅 경로, GitHub 무인 쓰기를 정함)
    output.rs          실행 기록 파일 cron/output/<id>/<시각>.md (작업마다 100개)
    deliver.rs         전달: 디스코드 채널(봇 REST), 웹훅, 로컬
    daemon.rs          junees cron daemon: 매 분 틱, 따라잡기 한 번, 놓친 한 번짜리 기록, 작업마다 자식 프로세스, 결과 없이 끝난 실행 기록, 종료
    cli.rs, command.rs junees cron 인자 파싱과 명령(add/list/remove/pause/resume/run/daemon)
    mod.rs             설정(timezone, cron_max_time, cron_max_tokens), 실행 마무리(finish), /cron과 !cron 목록
  github/            GitHub REST 도구 (github)
    mod.rs             설정(JUNEES_GITHUB_*, 토큰 Token은 Debug에서 숨김), repo 이름과 origin 원격 읽기, 허용 repo, 도구 등록(웹훅 실행은 register_webhook), /github 보기
    client.rs          REST v3 클라이언트 (헤더, Link 다음 페이지는 API 주소 아래만, 403/429 레이트 리밋 대기와 재시도, URL과 토큰 없는 오류)
    tool.rs            github 도구: 액션별 요청과 결과, 무인 허용 목록(웹훅 실행은 경로 목록과 둘 다), 보낼 글 검사, 승인 질문 내용(question)
    format.rs          GitHub가 준 글을 데이터로 표시 (첫 줄 안내, 인용 줄, 제어 문자 이스케이프, 상한)
    tests.rs           가짜 GitHub REST 서버로 요청 모양, 페이지, 레이트 리밋, 오류와 토큰 비노출, 허용 목록, 승인 판정 검증
  flow/              GitHub PR 흐름 (junees webhook add-github)
    mod.rs             흐름 설정 FlowConfig(저장소, 단계, 테스트 명령, 승인 수, 시간당 상한), 이벤트와 경로 쓰기, github_flow_merge
    state.rs           flows/state.json: head 커밋당 한 번 표시와 되돌리기, 시간당 모델 실행 상한, 흐름이 push한 커밋, 최근 처리
    prompts.rs         flows/review.md, address.md 기본값과 채우기(데이터 태그), 리뷰 형식(판정/필수/권장/참고) 읽기
    access.rs          세는 리뷰어: author_association과 collaborator 권한(실행마다 캐시), --approver
    merge.rs           merge 조건(열림, head 고정, 체크와 --require-check, 워크플로 변경 없음, 변경 요청, 자기 리뷰, head 커밋 승인, mergeable)과 REST merge
    address.rs         리뷰 반영의 git: 브랜치 이름 검사, origin 확인, fetch와 worktree, git 설정 스냅샷 비교, 커밋, 자격 증명 없는 테스트 명령, force 없는 push
    run.rs             이벤트 하나: 단계 계획, 루프 방지, 모델 실행(단계별로 좁힌 GitHub 쓰기), 코멘트, 기록
    setup.rs           add-github: 무작위 시크릿 파일, 기본 프롬프트, 경로, GitHub 설정 안내와 빠진 설정
    tests.rs           가짜 GitHub REST, 가짜 엔진, 로컬 bare 저장소로 이벤트별 시나리오
  webhook/           웹훅 (junees webhook)
    http.rs            작은 HTTP/1.1 수신기: 요청 줄, 헤더(16 KiB), Content-Length 본문(1 MiB), chunked 거절, 100-continue, 응답 뒤 연결 종료
    signature.rs       sha256=<hex> HMAC-SHA256 서명과 상수 시간 확인(ring)
    store.rs           경로 저장 webhooks/routes.json(0600, 잠금, 원자적 쓰기), 종류, 시크릿 위치, 이벤트 필터, GitHub 쓰기 허용(github_allow), GitHub 흐름(flow, 있으면 형식 2), 이름 검사
    template.rs        프롬프트 템플릿({{event}}, {{payload.a.b}} 등), 값 정리와 상한, 무작위 이름의 데이터 태그
    server.rs          junees webhook serve: 경로/메서드/서명/이벤트/중복 판단, 시크릿 없는 경로의 요청 검사(Host, 브라우저/프록시 헤더, JSON), 동시 연결 상한, 경로마다 줄 세우기(대기 20, 429), 실행마다 자식 프로세스, 결과 없이 끝난 실행 기록, 종료
    run.rs             실행 하나: running 기록, 프롬프트, cron 엔진(세션 source webhook), 출력 파일 webhooks/output/<이름>/, 전달, 마지막 실행 기록
    cli.rs, command.rs junees webhook 인자 파싱과 명령(serve/add/add-github/list/remove/pause/resume/test/run), 흐름 경로의 실행 연결
    mod.rs             설정(webhook_max_time, webhook_max_tokens), /webhooks와 !webhooks 목록
  doctor/            설치와 설정 점검 (junees doctor, /doctor)
    mod.rs             인자, 점검 결과(ok/warn/fail/skip, 고치는 방법), 텍스트와 JSON 출력, 비밀 값 지우기, 점검 순서
    files.rs           상태 폴더와 비공개 파일 권한, secrets/, 세션 폴더 크기
    settings.rs        설정 파일 읽기와 모든 값, 키가 있는지(값 없이), 디스코드, 텔레그램, 알림 웹훅
    claude.rs          claude --version, --online의 auth status, 환경 정리와 bubblewrap, user namespace 제한(/proc/sys)
    services.rs        git/gh, cron 데몬과 웹훅 서버(잠금으로 판단), 웹훅 시크릿, MCP 명령 찾기(실행 안 함), 켜진 플러그인
    online.rs          --online: 키 확인 요청 GET /v1/models
    tests.rs           임시 상태 폴더, 가짜 PATH와 /proc/sys, 가짜 claude, 가짜 API로 각 점검의 ok/경고/실패
tests/
  common/              테스트용 가짜 Anthropic API 서버 (JSON 응답과 SSE 스트림), 테스트 환경의 JUNEES_*/ANTHROPIC_*/CLAUDE_CODE_* 변수와 GH_TOKEN/GITHUB_TOKEN/GH_ENTERPRISE_TOKEN/GITHUB_ENTERPRISE_TOKEN/GH_CONFIG_DIR을 지우고, 인자로 받은 임시 cwd와 HOME에서 도는 바이너리 명령(junees_binary(cwd, home), 크레이트 폴더나 그 안, 그 위의 폴더는 거부, 모든 바이너리 테스트가 이것으로 시작)
  binary_env.rs        junees_binary가 지우는 변수와 남기는 변수, 주어진 임시 폴더에서만 돌고 크레이트 쪽 폴더는 거부하는지, 모든 바이너리 테스트가 그 헬퍼를 쓰는지 검증(명령은 실행하지 않음)
  anthropic_client.rs  가짜 API 서버로 실제 클라이언트와 루프를 끝까지 검증 (thinking 블록이 signature까지 그대로 돌아가는지 포함)
  session_resume.rs    실제 바이너리로 -c, --resume <앞부분>, --no-session, HOME 없는 실행, 이어 간 세션의 thinking 블록 보존 검증
  secret_env.rs        기본 bash에서 API 키, 웹훅 URL, 디스코드 토큰, GitHub 토큰, Claude Code 토큰이 보이지 않는지 검증
  hidden_env.rs        JUNEES_HIDDEN_ENV로 지정한 변수가 bash에서 보이지 않는지 검증
  proc_environ.rs      실제 바이너리의 bash 손자 프로세스가 조상들의 /proc/<pid>/environ에서 API 키와 웹훅 URL을 읽지 못하는지 검증
  context_management.rs 가짜 API 서버로 도구 결과 비우기, 압축, 캐시 표시, 기록 재생 검증
  task_mode.rs         실제 바이너리로 임시 git 저장소에서 task init -> task run -> task status, task run 중 SIGTERM, 가짜 웹훅으로 알림 검증
  cancel.rs            실제 바이너리로 -p 중 SIGTERM(종료 코드 143), 대화 모드 Ctrl-C(턴 취소 후 다음 입력, 두 번이면 종료) 검증
  task_git_env.rs      GIT_DIR이나 GIT_CONFIG_PARAMETERS가 있어도 task 모드가 제 저장소와 설정으로만 git을 쓰는지 검증
  approval.rs          실제 바이너리로 파이프 입력 대화 모드의 위험 명령 거절과 안내, JUNEES_APPROVAL=off, -p에서는 묻지 않음, 가상 터미널에서 스트리밍 텍스트와 승인 질문이 섞이지 않는지 검증
  task_git_secret.rs   하네스 git이 심어진 훅을 실행하지 않고 git이 실행하는 필터에도 API 키를 넘기지 않는지 검증
  streaming.rs         조각내 보내는 가짜 SSE 서버로 조립 결과가 일반 응답과 같은지, 오류 이벤트/끊김/대기 초과 재시도, 취소 시 연결 종료, 바이너리가 텍스트를 한 번만 찍는지(stdout이 파이프일 때 재시도로 끊긴 조각을 버리는지) 검증
  web_search.rs        가짜 API 서버(JSON, SSE)로 웹 검색 도구 정의, pause_turn 이어 가기(멈춘 응답을 그대로, 뒤에 user 메시지 없이), pause 상한 뒤 검색을 끄고 이어 갈 때 결과 없는 검색 호출 제거, 검색 블록과 citations 재전송, 클라이언트 도구 미실행, 바이너리의 출처 목록과 조직 비활성 안내 검증
  claude_code.rs       실제 바이너리와 가짜 claude로 API 키 없이 task init/run, claude 환경에 API 변수 없음, 로그인 실패 안내와 알림, SIGTERM 검증
  claude_chat.rs       실제 바이너리와 가짜 claude로 구독 대화: 두 번째 턴이 같은 세션을 --resume, 스트리밍과 도구/거절 표시, /usage와 /compact, /context 안내, 거절 규칙 인자, claude 환경에 API 변수 없음, -p와 -c, Ctrl-C 후 다음 턴, 사용 한도 안내 검증
  context_files.rs     실제 바이너리와 가짜 API 서버로 저장소의 지침 파일이 시스템 프롬프트에 들어가는지(순서, 저장소 위 파일 제외, 숨은 문자 이스케이프), 세션 중 바뀐 파일은 /new부터 보이는지, /context 목록, JUNEES_CONTEXT_FILES로 끄기와 이름 바꾸기 검증
  memory.rs            실제 바이너리와 가짜 API 서버로 memory 도구가 저장한 항목이 지금 세션이 아니라 다음 세션의 시스템 프롬프트에 들어가는지, /memory 보기와 clear(확인 질문), /new가 메모리를 다시 읽는지 검증
  skills.rs            실제 바이너리와 가짜 API 서버로 시스템 프롬프트의 스킬 목록, skill_view/skill_manage(0600), 세션 중 목록 고정, /skills, /skill, /new 뒤 목록 다시 읽기, /skills 화면의 제어 문자 이스케이프 검증
  discord_gateway.rs   테스트 안에서 띄운 가짜 Gateway(WebSocket)와 가짜 REST로 IDENTIFY/READY, 하트비트와 ACK 누락, resume_gateway_url로 RESUME, Reconnect, INVALID_SESSION, 닫힘 코드, 거절된 토큰이 오류와 로그에 안 나옴, 허용 사용자와 멘션 규칙, 메시지 나누기, 채널별 대기열과 동시 실행, !stop/!new/!status, 종료 시 취소, 429 재시도, 실제 에이전트와 가짜 API로 재시작 뒤 채널 세션 이어 가기(meta의 source/channel, -c와 /sessions 번호 목록에서 빠짐)와 승인 답장/시간 초과 검증
  telegram_gateway.rs  테스트 안에서 띄운 가짜 Bot API(루프백, 롱 폴링과 offset 확인)로 허용 사용자와 그룹/답장/토픽 규칙, /명령과 다른 봇 명령, 그룹의 `@봇 /stop`, 전달된 메시지 표시, 캡션은 명령 아님, 409 몇 번 기다리기와 계속되면 끝내기, 업데이트 확인과 다시 온 업데이트, 재시작 뒤 중복 없음, 4096자 나누기(코드 블록 울타리 없이)와 429, /stop//status//new, 거절된 토큰이 오류와 로그에 안 나옴, 실제 에이전트와 가짜 API로 승인 질문과 답(파일 쓰기), 채팅 세션(meta source), 사진과 앨범, 두 응답에 나뉘어 온 그룹 앨범, 바이너리 시작/답/SIGTERM 종료 검증
  mcp_oauth.rs         실제 바이너리 `junees mcp login`을 붙여 넣기 방식으로(가짜 인가 서버와 MCP 서버, PKCE와 resource 확인), 저장 파일 0600/0700, 출력에 토큰 없음, 저장된 로그인으로 `-p`의 도구 호출(토큰은 MCP 주소에만), `mcp logout`, 로그인 없는 실행은 401 한 번 뒤 그 서버만 실패하고 로그인을 시작하지 않음 검증
  mcp_http.rs          실제 바이너리 `-p`로 루프백 HTTP MCP 서버의 도구 호출(SSE 응답), 헤더 값이 환경 변수에서 오고 bash에는 그 변수가 없음, 변수가 없으면 그 서버만 빠짐 검증
  mcp.rs               실제 바이너리와 가짜 API, 가짜 MCP 서버(tests/fixtures/fake_mcp.py)로 모델의 MCP 도구 호출, 서버 환경에 API 키/디스코드 토큰/숨긴 변수가 없음, 종료 때 서버와 그 자식 정리, 0644 mcp.json 거절, /mcp, 파이프 입력 대화의 승인 거절과 trust, 게이트웨이 채널 질문, 작업 폴더의 모듈과 스크립트를 서버로 실행하지 않음(작업 폴더 안 상대 경로는 trust만) 검증
  plugins.rs           실제 바이너리로 junees plugin install/list/enable, -p에서 모델의 플러그인 도구 호출(환경에 비밀 없음, 플러그인 폴더에서 실행), pre_tool 훅의 bash 거절, post_turn 훅, 시스템 프롬프트의 조각 검증
  github.rs            실제 바이너리와 가짜 API, 가짜 GitHub REST로 -p에서 origin repo의 PR 보기 -> 리뷰(무인 허용 목록), 허용 목록 밖 쓰기 거절, 토큰이 모델/터미널/세션 기록에 없음 검증
  cron.rs              실제 바이너리와 가짜 API, 가짜 디스코드 REST로 cron add/list/run(전달, 출력 파일, 세션, 읽기 전용 도구)/pause/resume/rm, 데몬이 때가 된 작업을 자식 프로세스로 실행하고 실패를 전달하는지 검증
  github_flow.rs       실제 바이너리로 webhook add-github(시크릿 파일 0600, 안내, 형식 2), serve에 서명한 pull_request.opened -> 모델의 리뷰가 가짜 GitHub에 POST, 같은 커밋의 두 번째 전달은 아무것도 안 함, list의 최근 처리 검증
  webhook.rs           실제 바이너리와 가짜 API, 가짜 디스코드 REST로 webhook add/list/test(전달, 출력 파일, 웹훅 세션)/pause/resume/rm, serve에 reqwest로 서명한 요청(queued, duplicate, 틀린 서명, 405, 404), 자식 프로세스 실행의 bash에 시크릿 없음, SIGTERM 종료 검증
  doctor.rs            실제 바이너리로 junees doctor 출력과 종료 코드(권한 실패면 1), --json 형식, 깨진 설정 파일도 점검 결과로, --help와 잘못된 인자, 대화의 /doctor(대화 중에 깨진 설정 파일을 다시 읽음), 비밀 값이 출력되지 않음 검증
  usage.rs             실제 바이너리와 가짜 API로 대화의 /usage(호출, /model 전환 뒤 다른 가격, 합계), 기록 줄의 모델, junees usage(모델별, 날짜별, prices 덮어쓰기, 폴더 범위와 --all, 도움말과 오류) 검증
  session_search.rs    실제 바이너리와 가짜 API로 -p의 session_search 결과(지금 폴더, 채널 세션 제외, 데이터 표시), 대화의 /search와 junees sessions search 출력 검증
  simple_cli.rs        실제 바이너리로 위치 인자 질문과 설정 파일(환경 변수 우선, 잘못된 파일), 터미널 아닐 때 마법사 없음, 가상 터미널에서 처음 실행 마법사(키가 화면에 안 나옴, 0600), junees task "목표" -> junees task -> junees status, 대화의 /backend(대화도 전환), /task, /status, /sessions 고르기(다른 백엔드 세션 거절), /model, /new 검증
```

## 로드맵

1. 에이전트 루프: Anthropic Messages API, 도구 호출 반복, 기본 도구(bash, 파일 읽기/쓰기), CLI (완료)
2. 장기 실행 하네스: 세션 기록과 재개, 컨텍스트 압축, 여러 세션에 걸친 장기 작업 모드, 무인 실행 안정성. 자세한 계획과 진행 상황은 [docs/long-running.md](docs/long-running.md) (완료)
3. 도구 체계: 스트리밍 출력, 위험 명령 승인, 파일 패치(edit_file), 웹 검색, 확장 사고 블록 보존 (완료)
4. 메모리와 스킬: 세션을 넘어 남는 기억, 절차 문서화와 재사용 (완료)
5. 게이트웨이: 디스코드 봇 (`junees gateway`, 위 디스코드 게이트웨이) (완료)
6. 확장: 서브에이전트(`delegate`), 예약 작업(`junees cron`), 플러그인(`junees plugin`), MCP 클라이언트(stdio) (완료)
7. 웹훅과 GitHub: 웹훅 수신 서버(`junees webhook`), GitHub 도구(PR, 리뷰, merge), 웹훅으로 PR 리뷰·리뷰 반영·merge를 맡기는 흐름(`junees webhook add-github`) (완료)
8. 일상 사용 다듬기: 프로젝트 지침 파일(AGENTS.md, JUNEES.md, CLAUDE.md), 세션 검색(`session_search`, `/search`), 사용량과 비용(`/usage`, `junees usage`), 점검(`junees doctor`) (완료)
9. 연결과 입력 넓히기: 원격 MCP 서버(Streamable HTTP, OAuth 로그인 `junees mcp login`), 이미지 입력(`read_file`, `/image`, `--image`, 디스코드·텔레그램 첨부), 텔레그램 게이트웨이(`junees gateway telegram`) (완료)

## 개발

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 라이선스

MIT
