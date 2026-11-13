# 설정

설정 파일, 처음 설정 마법사, 환경 변수 전체 목록.

## 설정 파일과 처음 설정

설정은 `$JUNEES_HOME/config.toml`(기본 `~/.junees/config.toml`)에 있다. 처음 실행할 때 설정 파일도 `ANTHROPIC_API_KEY`도 없고 표준 입력과 표준 오류가 모두 터미널이면 설정 마법사가 먼저 뜬다(`junees setup`으로 언제든 다시).

1. Claude에 닿는 방법: `1) Claude subscription`(Claude Code CLI 로그인 필요) 또는 `2) Anthropic API key`.
2. 구독이면 `claude --version`과 `claude auth status --json`으로 확인한다(작업 모드가 쓰는 것과 같은 검사). 로그인이 안 돼 있으면 방법(`claude` 실행 후 `/login`, 브라우저가 없으면 `claude setup-token`, Linux면 bubblewrap 설치)을 보여 주고 설정은 저장한다. 구독은 대화(`junees`, `junees "질문"`)와 장기 작업(`junees task`, `/task`)을 모두 Claude Code로 돌리므로 API 키는 묻지 않는다(파일에 있던 키는 api 백엔드로 돌아갈 때를 위해 그대로 둔다).
3. API 키를 고르면 화면에 보이지 않게 입력한다(터미널 echo를 끔). `sk-ant-`로 시작하지 않으면 다시 묻고, 잘못 넣은 값도 화면에 다시 찍지 않는다.
4. API 키를 골랐으면 대화 모델(기본 `claude-sonnet-4-6`)을 묻고, 작업 기본 예산(시간 기본 `2h`, 토큰 상한은 선택)을 묻고 저장한다.
5. 마지막으로 디스코드 봇을 설정할지 묻는다(선택, 기본 아니오. 이 질문에서 입력이 끝나면 건너뛴다). `y`면 봇 토큰(화면에 보이지 않게, 디스코드 토큰 모양이 아니면 값을 되풀이하지 않고 다시 묻는다), 허용할 사용자 id(하나 이상), 멘션 없이 답할 채널 id(선택, `none`이면 비움)를 묻는다. [디스코드 게이트웨이](gateways.md#디스코드-게이트웨이) 참고.

Ctrl-C나 입력 끝(Ctrl-D)이면 아무것도 저장하지 않고 끝난다. 터미널이 아니면(cron, 파이프) 마법사를 띄우지 않고 `no API key: run `junees setup`, or set ANTHROPIC_API_KEY` 오류로 끝난다. 백엔드가 `claude-code`면(설정, `JUNEES_BACKEND`, 작업의 `--backend`) 키가 필요 없으므로 대화든 작업이든 마법사 없이 실행한다. `JUNEES_PROVIDER=openai`일 때도 마법사를 띄우지 않는다(질문이 Claude에 관한 것뿐이라, [OpenAI 호환 API](#openai-호환-api)는 설정 파일이나 환경 변수로 정한다).

파일은 TOML의 단순한 부분만 쓴다: 한 줄에 `키 = 값`, 값은 따옴표 문자열이나 숫자, `true`/`false`, `#` 주석. 표(`[...]`)와 배열은 받지 않는다. 키마다 환경 변수 하나에 대응하고, **환경 변수가 있으면 파일보다 우선한다**(`/model`, `/backend`로 바꾼 값은 그보다도 우선하지만 이 실행에만 적용되고 파일은 바꾸지 않는다). 모르는 키, 따옴표 없는 문자열(`model = claude-...`), 같은 키 두 번은 `<경로> is not a valid settings file ... line N: ...` 오류로 알려 준다.

```toml
backend = "claude-code"          # JUNEES_BACKEND
api_key = "sk-ant-..."           # ANTHROPIC_API_KEY
model = "claude-sonnet-4-6"      # JUNEES_MODEL
task_max_time = "2h"             # JUNEES_TASK_MAX_TIME
task_max_tokens = 2000000        # JUNEES_TASK_MAX_TOKENS
```

그 밖의 키: `base_url`, `provider`, `openai_base_url`, `openai_api_key`, `openai_reasoning_effort`, `openai_max_tokens_field`([OpenAI 호환 API](#openai-호환-api)), `prices`([사용량과 비용](usage.md#사용량과-비용)), `max_tokens`, `max_turns`, `context_window`, `compact_threshold`, `thinking_budget`, `task_skill_manage`, `subagents`, `subagent_max_turns`, `subagent_max_tokens`, `approval`, `stream`, `deny_commands`, `allow_dangerous`, `hidden_env`(쉼표로 구분한 문자열), `sandbox`, `sandbox_write`, `sandbox_read`, `webhook_sandbox_read`(쉼표로 구분한 문자열), `sandbox_network`, `notify_webhook`, `notify_each_session`, `claude_bin`, `claude_model`, `claude_code_unattended`(예약 작업과 웹훅에서 claude-code 백엔드 허용, [실행 격리](safety.md#실행-격리-landlock)), `web_search`, `web_search_max_uses`, `web_search_allowed_domains`, `web_search_blocked_domains`(쉼표로 구분한 문자열), `memory_user_chars`, `memory_notes_chars`, `memory_project_chars`, `context_files`(쉼표로 구분한 문자열, [프로젝트 지침 파일](memory-and-skills.md#프로젝트-지침-파일)), `discord_token`, `discord_allowed_users`, `discord_channels`, `discord_parallel`, `discord_approval_timeout`, `telegram_token`, `telegram_allowed_users`, `telegram_parallel`, `telegram_approval_timeout`, `telegram_format`(`html` 또는 `plain`, [텔레그램 게이트웨이](gateways.md#대화와-출력)), `timezone`, `cron_max_time`, `cron_max_tokens`, `webhook_max_time`, `webhook_max_tokens`, `github_token`, `github_api_url`, `github_repos`, `github_allow_unattended`, `github_flow_merge`. 각각 `JUNEES_` 접두 환경 변수(`base_url`은 `ANTHROPIC_BASE_URL`, `openai_base_url`은 `OPENAI_BASE_URL`, `openai_api_key`는 `OPENAI_API_KEY`, `discord_*`는 `JUNEES_DISCORD_*`, `telegram_*`는 `JUNEES_TELEGRAM_*`, `cron_*`는 `JUNEES_CRON_*`, `webhook_*`는 `JUNEES_WEBHOOK_*`, `github_*`는 `JUNEES_GITHUB_*`)와 같다. `JUNEES_HOME`과 `HOME`은 파일 위치를 정하므로 파일에 둘 수 없고, Junees가 읽지 않는 변수(`ANTHROPIC_TOKEN` 등)는 파일로도 설정되지 않는다.

파일은 API 키를 담을 수 있으므로 `0600`, 새로 만드는 폴더는 `0700`으로 만들고, 임시 파일에 쓴 뒤 이름을 바꿔서 중간에 죽어도 반쯤 쓴 파일이 남지 않는다. 마법사는 저장한 뒤 `ANTHROPIC_API_KEY`나 `JUNEES_BACKEND`가 환경에 있으면 그쪽이 이긴다고 알려 준다. 파일의 키는 환경 변수가 아니므로 bash 도구의 환경에는 처음부터 없다. 모델의 도구도 이 파일에 손대지 못한다: `read_file`/`write_file`/`edit_file`은 이 파일을 가리키는 경로(`..`, 심볼릭 링크, 아직 없는 파일을 가리키는 링크, 하드 링크 포함)를 "사용자의 비공개 설정 파일"이라는 오류로 거절하고, `bash` 명령 정책은 이 파일을 인자나 리다이렉트 대상으로 쓰는 명령과, 이 파일이 든 폴더(`$JUNEES_HOME`)를 통째로 다루는 명령을 막는다([명령 정책](safety.md#명령-정책) 참고). 쓰기까지 막는 것은 파일에 `allow_dangerous = "1"`을 넣어 다음 실행의 명령 규칙을 끄는 일도 막기 위해서다. `$JUNEES_HOME`의 나머지(세션 기록 등)는 그대로 읽을 수 있다. 다만 bash 쪽 명령 정책은 차단 목록일 뿐이어서 다른 변수나 실행 중에 만든 경로, 더 위 폴더를 통째로 다루는 명령은 놓친다. 그런 읽기는 [실행 격리](safety.md#실행-격리-landlock)(Landlock, 지원하는 커널에서 기본으로 켜짐)가 커널에서 막는다. 꺼져 있거나 지원하지 않는 커널에서는 여전히 읽을 수 있다(`~/.ssh`처럼 같은 사용자의 파일이다).

## OpenAI 호환 API

`provider = "openai"`(`JUNEES_PROVIDER=openai`)이면 api 백엔드가 Anthropic Messages API 대신 OpenAI Chat Completions API(`POST <openai_base_url>/chat/completions`)를 부른다. 같은 형식을 쓰는 서버면 된다: OpenAI, OpenRouter, Ollama(`/v1`), vLLM, LM Studio 등. claude-code 백엔드와는 따로다(`backend = "api"`일 때만 의미가 있다).

```toml
provider = "openai"
openai_base_url = "https://openrouter.ai/api/v1"   # OPENAI_BASE_URL, 기본 https://api.openai.com/v1
openai_api_key = "sk-or-v1-..."                    # OPENAI_API_KEY
model = "openai/gpt-4o-mini"                       # JUNEES_MODEL, 이 provider에서는 꼭 적는다
# openai_reasoning_effort = "low"                  # JUNEES_OPENAI_REASONING_EFFORT, 추론 모델만
# openai_max_tokens_field = "auto"                 # JUNEES_OPENAI_MAX_TOKENS_FIELD
```

- 우선순위: 키마다 환경 변수(`OPENAI_BASE_URL`, `JUNEES_OPENAI_*`)가 있으면 설정 파일 값(`openai_base_url`, `openai_*`)보다 우선하고, 빈 값(공백뿐인 값 포함)인 환경 변수는 없는 것으로 본다. 키만은 주소와 한 쌍으로 읽는다: 주소를 설정 파일(`openai_base_url`)에서 읽었으면 키도 설정 파일(`openai_api_key`)에서만, 주소를 환경 변수(`OPENAI_BASE_URL`)에서 읽었으면 키도 환경 변수(`OPENAI_API_KEY`)에서만 읽는다. 다른 도구 때문에 셸에 `OPENAI_API_KEY`를 둔 채 설정 파일에 OpenRouter 같은 다른 서버를 적어도 OpenAI 키가 그 서버로 가지 않고, 반대로 셸의 `OPENAI_BASE_URL`로 파일의 키가 가지도 않는다. 주소를 어디에도 적지 않았으면(기본 주소) 키는 다른 설정처럼 환경 변수, 설정 파일 순서로 찾는다. 짝이 맞는 키가 없고 서버가 이 컴퓨터에 있지 않으면 시작할 때 `no API key for <주소>: ... in the same place as the address` 오류로 알리고, `junees doctor`는 "set, but not where the address is set"로 어느 쪽에 키를 옮길지 알려 준다. 키가 어디서 왔는지(환경 변수 / config.toml)와 실제로 쓸 주소도 `junees doctor`가 보여 준다.

- 주소: https만 받는다. 평문 http는 키와 대화 전체가 네트워크에 그대로 실리므로 서버가 이 컴퓨터에 있을 때(`localhost`, `127.0.0.0/8`, `::1`)만 받는다. 리다이렉트는 따라가지 않는다. 주소는 오류와 `junees doctor` 출력에 나오므로 사용자 이름이나 비밀번호가 든 주소(`https://user:pass@proxy/v1`)는 거절한다. 키는 `openai_api_key`에 둔다.
- 키: `Authorization: Bearer <키>`로 보낸다. 루프백 주소의 서버는 키 없이 부를 수 있고, 그 밖의 주소에서 키가 없으면 시작할 때 오류다. `OPENAI_API_KEY`는 `ANTHROPIC_API_KEY`처럼 bash, MCP 서버, 플러그인, `claude` 자식 환경에서 지우고, `junees doctor` 출력에서 가린다. 메모리와 스킬에 저장하는 글의 비밀 값 검사는 OpenAI 프로젝트 키(`sk-proj-` 등), OpenRouter 키(`sk-or-v1-`), 접두 없는 `sk-` 키(`sk-` 뒤 영문자와 숫자 32자 이상: OpenAI 옛 키, DeepSeek 같은 호환 서버 키)도 찾는다.
- 모델: 기본값(`claude-sonnet-4-6`)은 이 provider에서 쓰지 않으므로 `model`을 꼭 적는다(없으면 시작할 때 오류). 서버가 아는 모델 이름은 `junees doctor --online`이 `GET <openai_base_url>/models`로 보여 준다. `/model`로 바꿀 때 `claude-`로 시작하지 않아도 경고하지 않는다. `context_window`는 모델에 맞게 따로 정한다(기본 200000).
- 출력 상한: `openai_max_tokens_field`(`JUNEES_OPENAI_MAX_TOKENS_FIELD`, 기본 `auto`)가 정한다. `auto`면 OpenAI 자체 API(`api.openai.com`과 데이터 상주용 지역 도메인 `eu.api.openai.com`, `us.api.openai.com` 등 `*.api.openai.com`)와 Azure OpenAI의 v1 API(`*.openai.azure.com`, `*.services.ai.azure.com`)에는 `max_completion_tokens`로, 그 밖의 서버에는 호환 서버들이 받는 `max_tokens`로 보낸다. OpenAI는 `max_tokens`를 deprecated로 두고 추론 모델(o 시리즈)에서 받지 않으며, Azure OpenAI의 추론 모델도 Chat Completions에서는 `max_completion_tokens`만 받는다. 주소로 알 수 없는 프록시(OpenAI 앞에 둔 사내 게이트웨이 등)는 `max_completion_tokens`, 반대로 `max_completion_tokens`를 모르는 서버는 `max_tokens`로 직접 고른다. 그 밖의 값은 시작할 때 오류다.
- Azure OpenAI: v1 API 주소를 쓴다(`openai_base_url = "https://<리소스>.openai.azure.com/openai/v1"`). `model`에는 배포 이름을 적고, 키는 다른 서버처럼 `Authorization: Bearer`로 간다. `api-version` 쿼리를 붙이는 옛 주소(`/openai/deployments/<이름>/...`)는 지원하지 않는다.
- 추론 모델: `openai_reasoning_effort`(`JUNEES_OPENAI_REASONING_EFFORT`)를 정하면 모든 호출에 `reasoning_effort`로 보낸다. 정하지 않으면 보내지 않으므로 서버의 기본값을 따른다(OpenAI 문서 기준 대개 `medium`, 모델마다 다르다). 값은 `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` 등인데 모델마다 받는 값이 다르고 계속 늘어서, Junees는 영문 소문자 한 낱말인지만 보고(대문자는 소문자로 바꾼다) 모델이 받지 않는 값은 서버의 400 오류로 알린다. 추론 토큰은 출력 상한(`max_tokens`, 위의 `max_completion_tokens`)을 같이 쓴다. 그래서 대화 호출의 상한은 추론까지 담을 만큼 `max_tokens`를 올려 둔다(OpenAI는 처음에 추론과 출력용으로 25,000 토큰 이상을 권한다). 상한을 설정할 수 없는 압축 요약 호출(상한 4096)은 `none`이 아닌 effort가 정해져 있으면 상한에 25,000을 더해 보낸다. effort를 정하지 않은 추론 모델은 요약이 추론에 상한을 다 써서 "the summary hit the output token limit" 경고와 함께 압축하지 못할 수 있으므로, 추론 모델이면 effort를 적어 둔다. 창이 작은 로컬 추론 모델은 이 25,000이 창을 넘을 수 있다(그 서버의 오류로 알린다).
- 도구 호출: 모델이 JSON이 아닌 인자를 쓰면 도구가 입력 오류로 거절하고, 다음 요청에는 그 호출의 인자를 `{}`로 보낸다(인자를 검사하는 서버(Ollama)가 깨진 인자 때문에 세션의 모든 요청을 400으로 거절하지 않도록). 스트리밍에서 `index`와 `id`가 없는 조각은, 앞 호출의 인자가 완성된 JSON이고 새 조각이 함수 이름을 가지면 새 호출로 본다.
- 꺼지는 Anthropic 전용 기능: 프롬프트 캐시 표시(`cache_control`, OpenAI 쪽은 서버가 알아서 캐시한다), 확장 사고(`thinking_budget`), 서버 웹 검색(`web_search`)은 보내지 않는다. 설정해 두면 무시하고 `junees doctor`가 경고한다.
- 사용량: `usage.prompt_tokens`에서 캐시된 토큰(`prompt_tokens_details.cached_tokens`, 서버가 주면 `cache_write_tokens`)을 빼 입력으로, 그 둘을 캐시 읽기/쓰기로 센다. 추론 토큰(`completion_tokens_details.reasoning_tokens`)은 `completion_tokens`에 들어 있고 출력 가격으로 청구되므로 출력에 그대로 두어 비용에 들어간다. 다만 Chat Completions는 추론 내용을 돌려주지 않아 다음 요청에 실리지 않으므로, 기록에는 `reasoning_tokens`로 따로 남기고 컨텍스트 크기(자동 압축 기준)에서는 뺀다. 스트리밍은 `stream_options.include_usage`로 마지막 조각의 사용량을 받는다. 사용량을 주지 않는 서버(또는 사용량 조각 전에 끊긴 스트림)면 비용은 0으로 남지만, 컨텍스트 크기는 0으로 두지 않고 마지막으로 잰 크기에 그 뒤 늘어난 대화의 추정치를 더해 자동 압축이 계속 동작한다. 가격표에는 Claude 모델만 있으므로 비용을 보려면 `prices`에 모델 가격을 적는다([사용량과 비용](usage.md#사용량과-비용)).
- 오류: 401/403은 키 문제로, 429와 5xx는 재시도(`retry-after`를 따름)하되 크레딧 소진·지출 한도 429(`code`가 `insufficient_quota`, `credit_balance_exhausted`, `organization_spend_limit_exceeded`, `project_spend_limit_exceeded`, `organization_usage_limit_exceeded`이거나 `type`이 `insufficient_quota`)는 재시도하지 않는다. 오류 응답, 상태 200 본문의 오류, 스트림 중간의 오류 조각 모두 같은 기준이다. 400 `context_length_exceeded`(또는 "maximum context length" 문구)는 컨텍스트 초과로 보고 도구 결과 비우기와 압축을 한다. 다만 vLLM처럼 출력 상한(`max_tokens`)만으로 모델의 창 이상이라고 말하는 오류("8192 in the completion", "'max_tokens' ... is too large")는 압축해도 소용이 없으므로 초과로 보지 않고, 창이 작은 모델이면 `max_tokens`를 낮추라고 안내한다. OpenRouter처럼 상태 200 응답 본문에 든 `{"error": ...}`도 그 `code`(없으면 `type`)의 상태로 보고 같은 기준으로 재시도한다. JSON이 아닌 오류 본문(프록시의 HTML 페이지 등)은 앞 300자만 남긴다. 이미지를 가리키는 400은 Anthropic과 같이 다음 요청부터 그 이미지를 안내 한 줄로 바꾼다.

## 환경 변수

`JUNEES_HOME` 말고는 모두 설정 파일 키로도 줄 수 있고([설정 파일](#설정-파일과-처음-설정) 절), 둘 다 있으면 환경 변수가 이긴다.

| 환경 변수 | 기본값 | 설명 |
|---|---|---|
| `ANTHROPIC_API_KEY` | (api 백엔드에서 필수, 또는 설정 파일 `api_key`) | Anthropic API 키 |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | API 주소 |
| `JUNEES_PROVIDER` | `anthropic` | api 백엔드가 부를 API: `anthropic`(Messages API) 또는 `openai`(Chat Completions와 그 호환 서버, [OpenAI 호환 API](#openai-호환-api)) |
| `OPENAI_BASE_URL` | `https://api.openai.com/v1` | `provider = openai`의 API 주소. https만, 평문 http는 localhost와 루프백 주소에만 |
| `OPENAI_API_KEY` | (없음, 또는 설정 파일 `openai_api_key`) | `provider = openai`의 키. 주소와 같은 곳(둘 다 환경 변수 또는 둘 다 설정 파일)에서만 읽는다. 루프백 주소의 서버(Ollama 등)는 없어도 된다 |
| `JUNEES_OPENAI_REASONING_EFFORT` | (없음: 보내지 않음) | `provider = openai`의 `reasoning_effort`(`low`, `medium`, `high` 등). [OpenAI 호환 API](#openai-호환-api)의 추론 모델 참고 |
| `JUNEES_OPENAI_MAX_TOKENS_FIELD` | `auto` | 출력 상한을 보낼 필드: `auto`(OpenAI와 Azure OpenAI는 `max_completion_tokens`, 그 밖은 `max_tokens`), `max_tokens`, `max_completion_tokens` |
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
| `JUNEES_SANDBOX_READ` | (없음, 전체) | 주면 격리된 명령이 이 폴더들과 쓸 수 있는 폴더만 읽는다(시스템 폴더를 넣어야 bash가 시작한다). 예약 작업의 파일 도구는 작업 폴더, 임시 폴더, `sandbox_write`에 더해 여기를 읽는다. 웹훅 실행의 파일 도구는 이 값을 받지 않는다 |
| `JUNEES_WEBHOOK_SANDBOX_READ` | (없음) | 웹훅 실행의 파일 도구가 작업 폴더, 임시 폴더, `sandbox_write` 밖에서 더 읽을 수 있는 폴더, 쉼표 구분 절대 경로(`~/...` 가능). 자격 증명은 적어도 읽지 않고, bash의 격리에는 쓰이지 않는다 ([실행 격리](safety.md#실행-격리-landlock)) |
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

디스코드(`JUNEES_DISCORD_*`), 텔레그램(`JUNEES_TELEGRAM_*`), 슬랙(`JUNEES_SLACK_*`: `slack_bot_token`, `slack_app_token`, `slack_allowed_users`, `slack_parallel`, `slack_approval_timeout`) 설정은 [게이트웨이](gateways.md), GitHub 설정(`JUNEES_GITHUB_*`)은 [GitHub](automation.md#github)의 설정 예시에 있다.

브라우저 도구의 키 `browser`, `browser_path`, `browser_allow_private`, `browser_unattended`, `browser_no_sandbox`(환경 변수 `JUNEES_BROWSER`, `JUNEES_BROWSER_PATH`, `JUNEES_BROWSER_ALLOW_PRIVATE`, `JUNEES_BROWSER_UNATTENDED`, `JUNEES_BROWSER_NO_SANDBOX`)는 [브라우저 도구](browser.md#켜기)에 있다.
