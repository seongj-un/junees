# 대시보드 (`junees dashboard`)

이 컴퓨터의 브라우저에서 Junees의 상태를 보는 페이지입니다. 설정, 최근 세션과 그 대화, 사용량, 예약 작업, 웹훅 경로, 점검 결과를 보여 줍니다. 바꾸는 것은 예약 작업과 웹훅 경로의 멈춤과 재개뿐입니다([쓰기](#쓰기-멈춤과-재개)). `--chat`으로 띄우면 페이지에서 Junees와 대화할 수도 있습니다([대화](#대화---chat)).

```sh
junees dashboard                      # 127.0.0.1:7878에서 열고 브라우저를 띄운다
junees dashboard --no-open            # 주소만 출력한다
junees dashboard --listen 127.0.0.1:0 # 빈 포트를 고른다
junees dashboard --chat --dir ~/work/x # 페이지에서 ~/work/x의 Junees와 대화한다(주소만 출력)
```

실행할 때마다 32바이트 무작위 토큰을 새로 만들고, 페이지는 API 요청마다 이 토큰을 보냅니다. 페이지가 토큰을 받는 주소는 두 가지입니다. `#` 뒤는 브라우저가 서버에 보내지 않고 `Referer`에도 넣지 않으며, 페이지는 읽자마자 주소에서 지웁니다. Ctrl-C로 끝냅니다.

- **브라우저를 열 때(기본)**: `xdg-open`(macOS는 `open`)에 `http://127.0.0.1:7878/#code=<일회용 코드>`를 넘깁니다. 페이지는 이 코드를 `POST /api/token`으로 토큰과 바꿉니다. 코드는 32바이트 무작위 값이고, 60초 동안 한 번만 쓸 수 있으며, 틀린 코드가 5번 오면 무효가 됩니다. 터미널에는 연다는 사실과 `http://127.0.0.1:7878/`만 출력하고, 토큰은 어디에도 출력하지 않습니다. 브라우저가 열리지 않으면 끄고 `--no-open`으로 다시 띄웁니다.
  - 받은 토큰은 그 탭에만 남습니다. 새 탭이나 다른 브라우저에서 열면 들어갈 수 없으니, 여러 탭이나 다른 브라우저에서도 쓰려면 `--no-open`으로 띄워 출력된 주소를 씁니다.
  - `xdg-open`(`open`)을 아예 실행하지 못하면(SSH로 들어간 서버, 데스크톱이 없는 리눅스) 명령줄에 코드가 실린 적이 없으므로, 코드를 무효로 하고 `--no-open`처럼 `#token=` 주소를 출력합니다.
  - 코드를 토큰과 바꿀 때마다 표준 오류와 `dashboard.log`에 `<시각> code exchanged` 한 줄을 남깁니다(코드와 토큰은 남기지 않음). 내가 바꾸지 않은 교환이 보이면 대시보드를 끄고 다시 띄웁니다.
  - 일회용 코드를 쓰는 이유: 브라우저를 띄울 때 주소가 `xdg-open`과 브라우저의 명령줄에 실리고, 이 컴퓨터의 다른 프로세스(다른 사용자, 그리고 같은 사용자로 도는 예약 작업과 웹훅 실행)가 `/proc/<pid>/cmdline`에서 읽을 수 있습니다. 거기 남은 코드는 페이지가 쓴 뒤나 60초 뒤에는 쓸모가 없습니다.
- **`--no-open`**: `Junees dashboard: http://127.0.0.1:7878/#token=<토큰>`을 출력합니다. 이 주소를 브라우저에 직접 붙여 넣습니다. 주소를 읽을 수 있는 사람은 대시보드도 쓸 수 있으니, 터미널 기록이나 화면 공유에 남지 않게 합니다.
- **웹훅을 받거나 예약 작업이 도는 컴퓨터에서**: 무인 실행의 bash는 기본 설정에서 루프백에 접속할 수 있습니다. 토큰을 알면 대시보드로 세션 대화를 읽고 작업을 멈추거나 켤 수 있으므로, 토큰이 든 주소(`--no-open`의 출력)를 무인 실행이 읽을 수 있는 파일에 남기지 않습니다. 쓰지 않을 때는 대시보드를 꺼 둡니다.
- 표준 출력이 닫혀 있으면(`| head -1`, 닫힌 터미널) 주소를 표준 오류에 경고로 남기고 계속 서빙합니다.
- 설정과 상태 폴더는 요청마다 새로 읽습니다. 설정을 바꾼 뒤 페이지를 새로 고치면 바로 보입니다. 세션마다 메시지 수와 토큰은 기록 파일의 수정 시각과 크기가 바뀔 때만 다시 셉니다.

## 보안

- **주소**: 루프백 주소(`127.0.0.1`, `::1` 등)에만 엽니다. `0.0.0.0`이나 다른 주소는 시작할 때 거절합니다.
- **토큰**: `/api/*`는 `Authorization: Bearer <토큰>`이 있어야 답합니다. 없거나 틀리면 401입니다. 비교는 상수 시간으로 합니다. `POST /api/token`만 토큰 없이 받습니다(아래).
- **`Host`**: `127.0.0.1:<포트>`, `localhost:<포트>`, `[::1]:<포트>`, 그리고 `--listen`에 준 주소(`127.0.0.2:<포트>` 등)만 받고, 아니면 421입니다. 포트가 80이면 브라우저가 포트를 빼고 보내므로 포트 없는 `Host`도 받습니다. 다른 사이트의 이름을 127.0.0.1로 돌리는 공격(DNS rebinding)을 막습니다. 이 검사는 정적 파일에도 적용됩니다.
- **`Origin`**: 보내 왔으면 이 서버(`http://<Host>`)여야 하고, 아니면 403입니다. CORS 헤더는 보내지 않습니다.
- **메서드와 크기**: `GET`을 받고, `POST`는 쓰기 경로에서만 받습니다(그 밖은 405와 `Allow`). 머리는 8KB(431), 본문은 1KB(413, 대화 메시지는 512KB)까지이고, 본문은 `POST`일 때만 읽습니다. 머리는 10초 안에 와야 하고, 연결 하나는 60초까지이며(대화의 이벤트 스트림만 예외), 동시에 16개까지 받습니다.
- **헤더**: 모든 응답에 다음을 붙입니다.
  - `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'`
  - `X-Content-Type-Options: nosniff`
  - `Referrer-Policy: no-referrer`
  - `Cache-Control: no-store`
- **쓰기 요청**: `POST`는 위 검사에 더해 `Origin`이 반드시 있어야 하고(없으면 403, 다른 출처면 위에서 이미 403), `Content-Type`이 있으면 `application/json`이어야 하며(415), 본문은 비었거나 `{}`여야 합니다(400). 그래서 이 페이지의 스크립트만 쓰기를 보낼 수 있습니다. 다른 사이트의 폼이나 `fetch`는 `Origin`을 다르게 붙이고, `Origin` 없는 요청은 브라우저 밖의 프로그램뿐인데 그런 프로그램은 토큰도 따로 있어야 합니다. 쓰기는 모두 `$JUNEES_HOME/dashboard.log`(권한 600)에 한 줄씩 남습니다.
- **비밀 값**: 키, 토큰, 웹훅 시크릿은 응답에 넣지 않습니다. 키는 있는지와 출처(`env`, `config.toml`)만, 웹훅 시크릿은 있는지만 보냅니다. 세션 목록은 첫 사용자 메시지의 앞부분만 보내고, 세션 상세는 글을 블록마다 정해진 길이까지만 보냅니다. 이미지 데이터와 thinking 내용은 보내지 않습니다. 이 글들, 제목, 실행 결과 메모, 점검 결과에서는 `junees doctor`와 같은 방식으로 비밀 값을 가립니다. 알려진 설정과 변수의 값, 그리고 자격 증명처럼 보이는 낱말(`secrets` 모듈)이 대상입니다. 가린 뒤에 자르므로 비밀 값의 조각이 남지 않습니다.

## API 계약

모두 `GET`이고 JSON(UTF-8, `application/json; charset=utf-8`)으로 답합니다. 시각은 RFC 3339 UTC 문자열(`2026-11-13T01:02:03Z`)이고, 없는 값은 `null`입니다. 비용(`cost_usd`)은 `prices`로 값을 매긴 토큰의 추정 비용(달러)입니다. 값을 매긴 토큰이 하나도 없으면 `null`입니다. 가격을 모르는 모델과 구독으로 도는 Claude Code 턴이 그렇습니다.

### `GET /api/status`

```json
{"version":"0.3.0","home":"/home/u/.junees","backend":"api","provider":"anthropic","model":"claude-sonnet-4-6","uptime_secs":12,
 "keys":{"anthropic":{"set":true,"source":"env"},"openai":{"set":false,"source":null}},
 "sandbox":{"landlock":true,"abi":4}}
```

- `backend`: `api` 또는 `claude-code`입니다. 설정을 읽지 못하면 `null`입니다.
- `provider`: `anthropic` 또는 `openai`입니다.
- `model`: api 백엔드는 `model`(`JUNEES_MODEL`) 값이고, 정하지 않았으면 Anthropic 기본 모델입니다. OpenAI 호환 provider에서 정하지 않았으면 `null`입니다. claude-code 백엔드는 `JUNEES_CLAUDE_MODEL` 값이고, 정하지 않았으면 Claude Code가 고르므로 `null`입니다.
- `keys.*.source`: 실제로 쓰이는 키의 출처입니다. 키는 주소와 같은 곳에서만 읽으므로, 다른 곳에 적어 둔 키는 `set: false`로 나옵니다.
- `sandbox.abi`: 커널의 Landlock ABI입니다. Landlock이 없으면 `landlock: false`, `abi: null`입니다.
- `chat`: `--chat`으로 띄워 [대화 API](#대화---chat)를 서빙하면 `true`입니다.

### `GET /api/sessions?limit=50`

최근에 기록된 세션부터 보냅니다. 터미널 세션과 채널(디스코드 등) 세션이 모두 들어갑니다. `limit`은 1부터 200까지이고, 주지 않으면 50입니다.

```json
{"sessions":[{"id":"20261113-010203-abcd","title":"Fix the login bug","cwd":"/home/u/work/x","updated_at":"2026-11-13T01:05:00Z","messages":12,
  "input_tokens":1000,"output_tokens":200,"cost_usd":0.012,"preview":"Fix the login bug\n..."}]}
```

- `title`: 세션에는 제목이 따로 없으므로, 첫 사용자 메시지에서 처음으로 비어 있지 않은 줄의 앞 80자를 씁니다. 비밀 값은 가립니다. 첫 메시지가 없으면 `null`입니다.
- `preview`: 첫 사용자 메시지의 앞 200자입니다. 비밀 값은 가립니다.
- `messages`: 기록된 사용자와 어시스턴트 메시지 수입니다. Claude Code 턴 하나는 2로 셉니다.
- `input_tokens`: 캐시를 거치지 않은 입력 토큰입니다. `/api/usage`의 `input_tokens`와 같은 기준입니다.

세션 화면의 정렬(최근 순, 비용 큰 순, 메시지 많은 순)은 이미 받은 이 목록 안에서만 브라우저가 하고, 요청을 새로 보내지 않습니다. `cost_usd`가 `null`인 세션은 비용 큰 순에서 맨 뒤에 두며, 선택한 정렬은 주소나 저장소에 남기지 않아 화면을 떠나면 최근 순으로 돌아갑니다. 목록 위의 합계 줄(메시지 수, 입력·출력 토큰, 비용)도 이 목록에서 지금 보이는 세션만 더한 값이고 `cost_usd`가 `null`인 세션은 0으로 치지 않고 제외한 수를 함께 적습니다. 이 목록에 없는 세션까지 더한 사용량은 사용량 화면에서 보는데, 그 화면도 전체 기록이 아니라 오늘을 포함한 최근 7일, 30일, 90일 중 고른 기간(기본 30일)만 보여 줍니다.

### `GET /api/usage?days=30`

오늘을 포함한 최근 `days`일(1부터 365, 기본 30)의 사용량을 날짜별, 모델별로 묶어 보냅니다. 모든 폴더가 대상입니다. 날짜는 `timezone` 설정(없으면 시스템 시간대)을 따르고, 오래된 날부터 보냅니다. 모델 이름은 `junees usage --by model`과 같습니다(Claude Code 턴은 `claude-code (<모델>)`).

```json
{"days":[{"date":"2026-11-13","model":"claude-sonnet-4-6","input_tokens":1,"output_tokens":1,"cache_read_tokens":0,"cache_write_tokens":0,"cost_usd":0.0}],
 "total":{"input_tokens":1,"output_tokens":1,"cost_usd":0.0}}
```

### `GET /api/cron`

```json
{"jobs":[{"id":"a1b2c3d4","name":"daily","schedule":"every 2h","dir":"/home/u/work/x","paused":false,"paused_for_home":false,"allow_home":false,
  "next_run":"2026-11-13T03:00:00Z","last_run":{"at":"2026-11-13T01:00:00Z","status":"ok","note":null}}]}
```

- `schedule`: 추가할 때 적은 일정 그대로입니다(`every 2h`, `0 9 * * *`).
- `last_run.status`: `running`, `ok`, `failed`, `missed` 중 하나입니다. `at`은 실행을 시작한 시각이고, `note`는 실패 이유 같은 메모입니다(없으면 `null`). 한 번도 돌지 않았으면 `last_run`이 `null`입니다.

### `GET /api/webhooks`

```json
{"routes":[{"name":"gh","kind":"github","dir":"/home/u/work/x","paused":false,"paused_for_home":false,"allow_home":false,"has_secret":true,
  "last_run":{"at":"2026-11-13T01:00:00Z","status":"ok","note":null}}]}
```

- `kind`: `github` 또는 `generic`입니다. `junees webhook add --kind`와 같은 이름입니다.
- `has_secret`: 서명 시크릿이 있는지만 알려 줍니다(`--no-secret` 경로는 `false`).

### `GET /api/doctor`

`junees doctor`(네트워크 없이)의 결과를 영역별로, 검사가 돈 순서대로 보냅니다. 해당하지 않는 검사(`skip`)는 빠집니다.

```json
{"sections":[{"name":"settings","checks":[{"name":"provider","status":"ok","detail":"anthropic (https://api.anthropic.com)","fix":null}]}]}
```

- `status`: `ok`, `warn`, `fail` 중 하나입니다.
- `fix`: 경고와 실패를 고치는 방법입니다. 없으면 `null`입니다.

### `GET /api/sessions/<id>?offset=0&limit=100`

세션 하나의 대화를 기록된 그대로, 앞에서부터 `offset`번째 메시지부터 `limit`개(1부터 200, 기본 100)씩 보냅니다. 압축이나 도구 결과 비우기로 모델이 보는 기록이 줄어도, 여기서는 쓰인 메시지를 모두 보냅니다. 이어 붙인 입력은 앞의 사용자 메시지에 합치고, Claude Code 턴 하나는 메시지 두 개(입력, 답)입니다. `/api/sessions`의 `messages`도 같은 수입니다.

- `id`는 `/api/sessions`가 준 값입니다. `[A-Za-z0-9_-]` 1~64자가 아니면 400 `bad_id`이고(`..`, `/`, `%`도 여기서 막힙니다), 그런 세션이 없으면 404입니다. 세션 폴더의 심볼릭 링크는 따라가지 않고 404로 봅니다.
- `offset`이 전체보다 크면 `messages`가 빈 배열입니다.

```json
{"id":"20261113-010203-abcd","title":"Fix the login bug","cwd":"/home/u/work/x","updated_at":"2026-11-13T01:05:00Z","total":42,"offset":0,
 "messages":[{"index":0,"role":"user","at":"2026-11-13T01:02:03Z","blocks":[
    {"type":"text","text":"...","truncated":true},
    {"type":"text","text":"[되돌림] 변경 기록 2의 파일 변경을 되돌렸습니다: ...","notice":true},
    {"type":"tool_use","tool_use_id":"toolu_01","name":"bash","input_preview":"ls -la"},
    {"type":"tool_result","tool_use_id":"toolu_01","is_error":false,"text_preview":"..."},
    {"type":"tool_result","tool_use_id":"toolu_02","is_error":false,"text_preview":"...","truncated":true,
     "text_tail":"...","omitted_chars":1820,"draft":{"stop":"done","error":null,"levels":3,"levels_done":3,
     "polished":false,"sections":8,"failed_sections":[],"calls":17,"input_tokens":11207,"output_tokens":3936,
     "cache_read_tokens":0,"elapsed_ms":7000,"sequential_ms":25200,"by_level":null}},
    {"type":"image","media_type":"image/png"},
    {"type":"thinking"},
    {"type":"other","kind":"server_tool_use"}]}]}
```

- `index`는 대화 전체에서의 순서(0부터)이고, `at`은 그 줄이 기록된 시각입니다. 시각을 남기기 전의 기록은 `null`입니다.
- `text`는 8,000자, `input_preview`와 `text_preview`는 2,000자까지이고, 잘렸으면 `"truncated": true`가 붙습니다(안 잘렸으면 이 필드가 없습니다). 비밀 값을 먼저 가린 뒤에 자릅니다.
- `input_preview`는 `bash`처럼 `command`가 있으면 그 명령, 문자열 필드 하나뿐이면 그 값, 그 밖에는 입력 JSON입니다.
- `tool_use_id`: `tool_use`와 그 결과(`tool_result`)가 같은 값을 가집니다. 호출과 결과는 이 값으로 짝짓습니다(같은 메시지 안의 순서가 아니라). 모델 API가 준 id를 비밀 값을 가린 뒤 200자까지 보냅니다.
- `text_tail`, `omitted_chars`: 잘린 `tool_result`(`"truncated": true`)에만 붙습니다. `text_tail`은 결과의 끝 500자(넘친 부분이 그보다 짧으면 넘친 부분 전체)이고, `text_preview`와 겹치지 않습니다. `omitted_chars`는 둘 사이에 빠진 글자 수입니다(0이면 이어집니다). 도구가 상태를 맨 끝에 붙이므로(`draft`의 상태 줄 등) 잘린 결과에서도 끝은 보입니다. 비밀 값은 끝 앞 512자까지 함께 놓고 가린 뒤에 자릅니다.
- `draft`: `draft` 도구 호출(같은 `tool_use_id`의 `tool_use` 이름이 `draft`)의 결과에만 붙습니다. 그 밖의 결과에는 이 필드가 없습니다. 값은 그 호출이 어떻게 끝났는지이고, 필드는 대화 이벤트 `draft_done`([NCM식 글 쓰기](write.md#대시보드-이벤트))과 같습니다.
  - 값은 Junees가 쓴 것에서만 읽습니다. 결과의 첫머리(`draft failed: `, `draft stopped: `, 취소 문구), 글과 상태 줄 사이의 `---`, Junees가 결과 맨 끝에 붙인 상태 줄(`[draft] levels ...`), 그리고 세션 기록의 `draft` 토큰 줄(`subagent_usage`, `"source": "draft"`)입니다. 모델이 쓴 글은 읽지 않으므로, 글 안에 상태 줄과 같은 모양의 줄이 있어도 값이 바뀌지 않습니다. 상태 줄은 잘리지 않은 결과 전체에서 읽습니다.
  - `null`: 결과가 `draft` 결과 모양이 아닐 때입니다. 잘못된 입력, 시작 전에 이미 넘은 토큰 상한, 실행되기 전에 끝난 호출(답이 잘림, 취소), `draft`가 없는 대화에서 부른 호출이 여기에 듭니다.
  - `stop`: `done`, `cancelled`, `failed`, `limit`. `error`: `failed`, `limit`의 이유(비밀 값을 가리고 2,000자까지), 그 밖에는 `null`.
  - `levels`, `levels_done`, `polished`, `sections`(절 수, `draft_done`에는 없는 필드), `failed_sections`, `calls`, `input_tokens`(캐시 쓰기와 읽기 포함), `output_tokens`: 상태 줄의 값 그대로입니다.
  - `elapsed_ms`, `sequential_ms`: 상태 줄이 0.1초 단위로 적으므로 100밀리초 단위입니다(실시간 `draft_done`은 밀리초). `sequential_ms`는 `stop`이 `done`이고 걸린 시간보다 (0.1초 단위로) 길 때만 값이 있고, 그 밖에는 `null`입니다. 이 규칙 전의 상태 줄(늘 추정을 적음)도 같은 규칙으로 읽습니다.
  - `cache_read_tokens`: 토큰 줄에서 더합니다. 한 답에 `draft` 호출이 여럿이면, 호출 순서대로 상태 줄의 입력과 출력 토큰에 정확히 맞을 때까지 줄을 나눠 가집니다. 맞지 않거나 줄이 남으면(답한 모델 이름이 없는 provider는 표시 없는 줄을 남깁니다) 그 답의 모든 호출에서 `null`입니다. 다른 호출의 토큰을 붙이지 않기 위해서입니다.
  - `by_level`: 세션 기록에 단계별 값이 없어 늘 `null`입니다.
  - 기록에 없는 것: 예비 모델 전환(`model_switch`)은 세션 기록에 이벤트로 남지 않아 다시 그릴 수 없습니다. 세션 기록 형식은 9 그대로이고, 새 항목을 쓰지 않습니다.
- `tool_result`에 이미지(브라우저 스크린샷 등)가 붙어 있으면 `"images":["image/png"]`처럼 형식만 보냅니다. 없으면 이 필드가 없습니다.
- `thinking`은 내용 없이 종류만 보냅니다. 서버 도구 호출과 결과(웹 검색 등)는 `"type":"other"`와 그 종류(`kind`)만 보냅니다.
- `"notice": true`는 사람이 쓴 글이 아니라 Junees가 입력 앞에 붙인 안내라는 표시입니다(되돌리기의 `[되돌림]` 줄, [파일 체크포인트](checkpoints.md)). 그 user 메시지의 `text` 블록으로 사람이 쓴 입력보다 앞에 오고, 여러 줄이면 줄바꿈으로 이어집니다. 표시가 없는 블록은 사람이 쓴 글입니다. 사람이 `[되돌림] `으로 시작하는 글을 써도 표시가 붙지 않습니다. 기록 형식 9 전의 세션(안내 줄이 입력 글 앞부분에 들어 있음)에는 표시가 없습니다. 제목(`title`)은 표시가 없는 글에서 정합니다. MCP의 `junees_session`도 같은 모양입니다.
- 화면의 "불러온 메시지 안에서 찾기"는 이미 불러온 메시지의 글(`text`, 도구 호출의 `name`과 `input_preview`, 도구 결과의 `text_preview`와 `text_tail`)에서만 찾고, 영문 대소문자를 가리지 않습니다. 서버에 새 요청을 보내지 않으며, 찾는 낱말은 주소나 브라우저 저장소에 남기지 않습니다. 아직 불러오지 않은 메시지는 "더 보기"로 불러온 뒤에 찾을 수 있습니다.
- 화면의 "도구 호출과 결과 숨기기"를 켜면 `tool_use`, `tool_result`, 서버 도구 블록(`other` 중 `server_tool_use`, `web_search_tool_result`, `web_fetch_tool_result`와 이름이 `_tool_use`, `_tool_result`로 끝나는 종류)을 그리지 않고, 그것을 빼면 읽을 글이 남지 않는 메시지(내용 없는 생각 과정 표시만 남는 경우 포함)는 통째로 숨깁니다. `draft` 호출의 결과는 사용자가 받은 글이라 카드로 남기고 그 호출만 숨깁니다. 켠 동안 찾기는 화면에 남은 글에서만 찾습니다. 체크 상태는 서버, 주소, 브라우저 저장소 어디에도 남기지 않아 화면을 떠나면 꺼진 상태로 돌아갑니다.

### `POST /api/token`

브라우저를 열 때 넘긴 일회용 코드를 토큰과 바꿉니다. 토큰 없이 받는 유일한 경로이고, 쓰기 요청 규칙(같은 출처의 `Origin` 필수, `Content-Type`은 있으면 `application/json`)을 따릅니다.

- 본문: `{"code":"<코드>"}`. 다른 필드가 있거나 모양이 다르면 400 `bad_body`입니다.
- 200 `{"token":"<토큰>"}`
- 401 `bad_code`: 코드가 틀렸거나, 이미 썼거나, 60초가 지났거나, 틀린 코드가 5번 와서 무효가 됐습니다. 비교는 상수 시간입니다. 터미널에서 `junees dashboard`를 다시 실행해야 합니다.
- 이 요청은 쓰기 로그에 남지 않습니다.

### 쓰기: 멈춤과 재개

- `POST /api/cron/<id>/pause`, `POST /api/cron/<id>/resume`
- `POST /api/webhooks/<name>/pause`, `POST /api/webhooks/<name>/resume`

[쓰기 요청 규칙](#보안)을 지켜야 합니다. 바꾼 뒤의 항목 하나를 200으로 돌려주고, 모양은 `/api/cron`의 `jobs[]` 원소, `/api/webhooks`의 `routes[]` 원소와 같습니다.

- `<id>`는 작업의 `id` 그대로, `<name>`은 경로 이름 그대로여야 합니다. CLI처럼 이름이나 id 앞부분으로는 찾지 않습니다. 없으면 404, 형식(`[A-Za-z0-9_-]` 1~64자)이 다르면 400 `bad_id`입니다.
- 이미 멈춘 항목의 `pause`는 아무것도 바꾸지 않고 그 항목을 그대로 돌려줍니다. 그래서 홈 폴더 때문에 멈춘 표시(`paused_for_home`)가 지워지지 않습니다.
- 바꾸는 방법은 `junees cron pause|resume`, `junees webhook pause|resume`과 같은 함수입니다. 저장 파일의 잠금도 CLI와 같아서, 데몬이나 서버가 돌고 있어도 됩니다. 멈추면 "사용자가 멈춘 것"이 되고(`paused_for_home`은 `false`), 예약 작업을 다시 켜면 다음 실행 시각을 지금부터 다시 셉니다.
- 409 `conflict`
  - 홈 폴더 때문에 멈춘 항목(`paused_for_home`)의 `resume`: `message`에 터미널에서 실행할 `junees cron allow-home <id>`(웹훅은 `junees webhook allow-home <name>`)를 담습니다. 대시보드는 `allow-home`을 하지 않습니다. CLI의 `resume`은 이런 항목도 다시 켜지만, 다음 실행이 같은 이유로 다시 멈추므로 대시보드에서는 막습니다.
  - 시각이 지난 일회성 작업의 `resume`: CLI와 같은 문구("was to run once, at a time that has passed; add it again")입니다.
- 쓰기 로그: 대상까지 찾아간 쓰기(200, 404, 409 등)는 `$JUNEES_HOME/dashboard.log`에 `<시각> <pause|resume> <cron|webhook> <id 또는 이름> <ok 또는 상태 코드와 오류 코드>` 한 줄을 남깁니다. 토큰과 본문은 남기지 않습니다. 401, 403처럼 대상에 닿기 전에 거절된 요청은 남기지 않습니다. 로그를 쓰지 못해도 바꾼 것은 그대로 두고, 표준 오류에 알립니다.

### 오류

모든 오류는 `{"error":"<짧은 영어 코드>","message":"<사람이 읽을 문장>"}` 모양입니다.

- 400 `bad_query`: `limit`, `days`가 범위 밖이거나 숫자가 아님
- 400 `bad_id`: 세션 id, 작업 id, 경로 이름의 형식이 다름
- 400 `bad_body`: 쓰기의 본문이 비었거나 `{}`가 아님, `POST /api/token`의 본문이 `{"code":"..."}`가 아님
- 401 `bad_code`: `POST /api/token`의 코드가 틀렸거나, 썼거나, 만료됨
- 400 `bad_request`: 요청 머리를 읽을 수 없음, `Transfer-Encoding` 본문
- 401 `unauthorized`: 토큰이 없거나 틀림(`WWW-Authenticate: Bearer`)
- 403 `forbidden_origin`: 다른 출처의 `Origin`, 또는 `Origin`이 없는 쓰기
- 409 `conflict`: 다시 켤 수 없는 작업이나 경로(홈 폴더 때문에 멈춤, 시각이 지난 일회성 작업)
- 415 `unsupported_media_type`: 쓰기의 `Content-Type`이 `application/json`이 아님
- 404 `not_found`: 없는 경로
- 405 `method_not_allowed`: 쓰기 경로가 아닌데 `GET`이 아니거나(`Allow: GET`), 쓰기 경로인데 `POST`가 아님(`Allow: POST`)
- 413 `body_too_large`: 본문이 1KB(대화 메시지는 512KB)보다 큼
- 421 `misdirected_request`: `Host`가 이 서버가 아님
- 431 `head_too_large`: 머리가 8KB보다 큼
- 503 `busy`: 쓰기 중 다른 junees가 저장 파일 잠금을 10초 넘게 쥐고 있음. 다시 시도하면 됩니다.
- 500 `internal`, `bad_settings`: 상태 폴더나 설정(`prices`, `timezone`)을 읽지 못함. 메시지의 비밀 값은 가립니다.
- 503 `doctor_busy`: `/api/doctor`는 한 번에 하나만 돕니다. 앞 요청의 doctor를 20초 기다려도 차례가 오지 않거나, doctor가 20초 안에 끝나지 않으면 이 오류입니다.

## 대화 (`--chat`)

```sh
junees dashboard --chat [--dir <폴더>]
```

페이지에서 Junees와 대화합니다. 위험한 명령의 승인 질문도 페이지의 버튼으로 답합니다. 기본으로는 꺼져 있고, `--chat`이 없으면 아래 경로는 모두 404입니다.

**보안 주의: 대화를 켜면 페이지가 그 폴더의 터미널 대화(`junees`)와 같은 권한을 가집니다.** 대화는 무인 실행이 아닙니다. bash, 파일 쓰기, MCP 서버, 플러그인을 터미널처럼 쓰고, 승인이 필요한 호출은 페이지에 질문을 띄워 사람의 답을 기다립니다. 그래서 토큰이 든 주소를 읽을 수 있는 사람이나 프로그램은 이 컴퓨터에서 명령을 실행할 수 있습니다. [위의 토큰 주의](#대시보드-junees-dashboard)를 더 엄격히 지키고, 무인 실행(웹훅, 예약 작업)이 도는 컴퓨터에서는 쓸 때만 켭니다.

- **브라우저를 열지 않음**: `--chat`이면 기본으로 브라우저를 열지 않고 `--no-open`처럼 `#token=` 주소를 터미널에만 출력합니다. 브라우저를 띄우면 일회용 코드가 브라우저 명령줄에 실리는데, 브라우저가 코드를 쓰기 전 수백 ms 동안 이 컴퓨터의 다른 프로세스(프롬프트 주입을 받은 웹훅 실행 등)가 `/proc`에서 코드를 읽어 먼저 바꿀 수 있습니다. 대화가 켜져 있으면 그 토큰으로 명령을 시키고 승인에도 스스로 답할 수 있어서, 무인 실행의 샌드박스가 막던 것(SSH 키, gh 설정 등)까지 닿습니다. 켜는 바로 그 순간의 경합이라 "쓸 때만 켠다"로는 막을 수 없습니다. 터미널 출력은 `/proc`으로 읽을 수 없으므로 주소는 터미널에만 둡니다. 그래도 열고 싶으면 `--open`을 주고, 그때는 이 위험을 표준 오류에 한 줄 경고합니다. 코드 교환은 위처럼 로그에 남습니다.
- **작업 폴더**: `--dir`(기본: 현재 폴더)입니다. 터미널 대화처럼 홈 폴더도 받습니다. 대시보드는 이 폴더로 옮겨 가서 돕니다.
- **백엔드**: api 백엔드(Junees 자신의 에이전트)만 됩니다. claude-code 백엔드는 Junees의 승인 질문 없이 명령을 실행하므로 페이지가 답할 수 없어서, 시작할 때 거절합니다.
- **승인**: 질문 규칙은 터미널과 같습니다(`JUNEES_APPROVAL`, "항상 허용"은 그 대화에서만, 매번 묻는 종류는 매번). 5분 동안 답이 없으면 거절로 닫고, 턴을 취소하면 열린 질문도 거절로 닫습니다.
- **턴**: 대시보드 전체에서 한 번에 하나만 돕니다(터미널 하나처럼). 턴이 도는 동안 보낸 메시지는 409 `turn_running`입니다.
- **세션**: 터미널 세션과 같은 일반 세션으로 저장됩니다. 대시보드의 세션 화면과 `junees --resume`으로 볼 수 있습니다. 기존 세션을 이어 갈 수도 있는데, 작업 폴더가 `--dir`이고 claude-code 세션이 아니어야 합니다. 다른 Junees가 쓰고 있는 세션은 409 `session_busy`입니다.
- **열린 대화**: 에이전트를 16개까지 둡니다. 더 열면 가장 오래 쓰지 않은 대화를 닫고, 그 대화의 이벤트 스트림을 끝냅니다. 닫힌 대화(그리고 대시보드를 다시 띄운 뒤의 대화)는 `session_id`로 다시 엽니다.
- **턴이 비정상으로 끝나면**: 에이전트 안에서 예기치 않은 오류(panic)가 나도 대화는 409에 묶이지 않습니다. `error`와 `turn_end {stop_reason: "error"}`를 보내고, 다음 메시지에서 세션을 다시 열라고 안내합니다.
- **화면: 글 생성**: 모델이 `draft`를 부르면 그 도구 블록 안에 "글 생성" 카드가 생깁니다. 절마다 한 칸이고, `draft_level`이 올 때마다 칸의 글이 다음 단계의 글로 바뀝니다(개념, 요지, 본문처럼 점점 자세해지는 모습). 카드 위에는 단계(`1/3 개념`, `2/3 요지`, `3/3 본문`, 이음)와 제목이 있고, 한 단계가 끝날 때마다 진행 줄 하나만 스크린 리더에 알립니다(`role="status"`). 칸의 글은 모델이 쓴 것이므로 텍스트로만 넣고, 칸마다 높이를 제한하고 펼치기를 둡니다. `truncated`인 칸에는 "잘림" 표시를 합니다. `draft_done`이 오면 카드를 접고 한 줄 요약(호출 수, 입력과 출력 토큰, 캐시 읽기, 걸린 시간, 순차였다면 걸렸을 시간)을 보여 줍니다. `stop`이 `cancelled`, `limit`, `failed`면 그 이유를 따로 안내하고, `failed_sections`의 칸에는 "대신함" 표시를 합니다. 펼치면 칸별 글과 `by_level` 표가 보입니다. 화면에는 단계 수(`levels`)가 이벤트로 오지 않으므로 `draft` 호출의 입력(`levels`)에서 읽고, 입력이 잘려 읽을 수 없으면 3으로 두었다가 단계의 `kind`(`outline`, `paragraph`, 2단계의 `body`)로 바로잡습니다.
- **화면: 새로 고침 뒤**: 이벤트는 대시보드 프로세스가 메모리에 둔 1,000개까지만 다시 오므로, 오래된 대화나 다시 띄운 뒤의 대화는 세션 기록의 `draft` 도구 결과로 같은 카드를 그립니다. 호출과 결과는 `tool_use_id`로만 짝짓습니다. 호출이 받은 목록 밖에 있어도(대화 화면은 마지막 20개 메시지만 받으므로 창의 첫 메시지가 `draft` 결과일 수 있습니다) 결과에 `draft` 필드가 있으면 카드로 그립니다. 서버가 이 필드를 `draft` 호출의 결과에만 붙이기 때문입니다. `draft`가 없는 결과는 id가 없는 옛 응답이든 짝이 없는 결과든 일반 도구 블록입니다., 단계 수, 호출 수, 토큰, 시간, 실패한 절은 결과의 `draft` 필드에서만 읽습니다. 모델이 쓴 글에서는 상태를 읽지 않으므로 글 안에 상태 줄 모양의 줄이 있어도 숫자가 바뀌지 않고, 글에서 찾은 `> [s2]` 실패 표시도 `failed_sections`에 있는 절에만 붙입니다. `draft`가 `null`이면 "상태 모름"이라고 하고 숫자를 보이지 않으며, `sequential_ms`가 `null`이면 "순차였다면"을, `by_level`이 `null`이면 표를 보이지 않습니다. 글은 `text_preview`(2,000자)에서 절을 나누어 보여 주고, 잘렸으면 "가운데 N자 생략"(`omitted_chars`) 뒤에 `text_tail`(끝 500자)을 "글의 끝부분"으로 보여 줍니다. 두 부분이 이어지면(`omitted_chars`가 0) 한 글로 봅니다.
- **화면: 모델 전환**: `model_switch`가 오면 대화 흐름에 "사용 한도로 막혀 예비 모델로 이어 갑니다"(`reason`별 문구) 줄을 넣고, 주 모델과 예비 모델 이름, `retry_at`으로 센 "다시 시도: 15분 뒤"를 보여 줍니다. 상대 시간은 1초마다 새로 셉니다. `model_switch`는 세션 기록에 남지 않고 끝난 턴은 새로 고친 뒤 세션 기록으로만 그려지므로, 새로 고친 뒤에는 이 줄이 다시 그려지지 않습니다.
- **쓰기 로그**에는 아래 대화 줄 말고도 코드 교환(`code exchanged`)이 남습니다.
- **쓰기 로그**: `dashboard.log`에 `chat started in <폴더>`, `chat <id> opened`, `chat <id> message <번호>`, `chat <id> approval <질문 id> <y|a|n>`, `chat <id> cancel`을 남깁니다. 메시지 내용과 토큰은 남기지 않습니다.

### 경로

쓰기 요청 규칙(Bearer, `Host`, `Origin` 필수와 같은 출처, `Content-Type: application/json`)은 [보안](#보안)과 같고, 본문은 JSON 객체입니다. 모르는 필드가 있으면 400 `bad_body`입니다.

- `POST /api/chat` `{"session_id"?: string}` → 200 `{"session_id": "..."}`
  - `session_id`가 없거나 `{}`면 새 대화입니다. 세션 파일은 첫 메시지 때 만들어집니다.
  - 있으면 그 세션을 이어 갑니다. 이미 열린 대화면 그대로 돌려줍니다. `--dir`의 세션이 아니면 404, 형식이 다르면 400입니다.
- `POST /api/chat/<id>/messages` `{"text": string}` → 202 `{"turn": n}`
  - `text`는 1~32,000자이고, 공백만이면 안 됩니다(400 `bad_body`). `n`은 이 대화에서 몇 번째 턴인지입니다.
- `GET /api/chat/<id>/events?after=<seq>` → 200 `text/event-stream`
  - Bearer 헤더가 있어야 하므로 `EventSource`가 아니라 `fetch` 스트림으로 읽습니다.
  - 이벤트마다 `id: <seq>` 줄과 `data: <JSON 한 줄>` 줄, 빈 줄을 씁니다. `seq`는 대시보드 프로세스 전체에서 하나씩 늘어나는 정수입니다. 대화마다 이어지지는 않지만 늘 커지고, 닫혔다 다시 연 대화도 이전 번호 다음부터 받습니다.
  - `after`를 주면 그다음부터, 없으면 남아 있는 것 전부부터 보냅니다. `after`가 그 대화의 마지막 번호보다 크면(대시보드를 다시 띄운 경우) 남아 있는 것 처음부터 보냅니다. 대화마다 최근 1,000개를 메모리에 둡니다. 그보다 오래된 것은 다시 받을 수 없습니다.
  - 15초마다 `: keepalive` 주석 줄을 보냅니다. 스트림은 동시에 8개까지이고, 넘으면 429 `too_many_streams`입니다. 페이지가 연결을 닫으면 그 자리는 바로 풀립니다.
  - 대화가 닫히면(열린 대화 상한, 대시보드 종료) 스트림을 끝냅니다. 다시 열 때는 `POST /api/chat {session_id}` 뒤에 같은 `after`로 다시 연결합니다.
- `POST /api/chat/<id>/approvals/<question_id>` `{"answer": "y"|"a"|"n"}` → 200 `{"question_id", "answer"}`
  - 없는 질문은 404, 이미 닫힌 질문은 409 `conflict`입니다. `a`를 내놓지 않은 질문에 `a`를 보내면 400 `bad_body`입니다.
- `POST /api/chat/<id>/cancel` → 200 `{"cancelled": true|false}`
  - 이 대화의 턴이 돌고 있으면 취소하고(Ctrl-C와 같음) `true`, 아니면 아무것도 하지 않고 `false`입니다.
- `GET /api/chat/<id>/checkpoints` → 200 `[{"turn": n, "created_at": "...", "undid": [n, ...], "files": [{"path", "change": "created"|"modified"|"deleted", "restorable": bool}]}]`
  - **`turn`은 체크포인트 번호입니다.** 대화의 턴 번호(`turn_start`, `turn_end`의 `turn`)와 다릅니다. 체크포인트 번호는 세션에서 파일을 바꾼 턴만 1부터 세고, 대화를 다시 열어도 이어집니다. 대화의 턴이 체크포인트를 남겼으면 그 턴의 `turn_end`에 `checkpoint`로 옵니다. `created_at`은 그 턴이 처음 파일을 바꾼 시각(RFC 3339 UTC)입니다. `undid`는 그 체크포인트가 되돌리기일 때 되돌린 체크포인트 번호들(최근 것부터)이고, 되돌리기가 아니면 `[]`입니다. 새로 고친 페이지나 다른 탭도 이 값으로 이미 되돌린 체크포인트를 알 수 있습니다.
  - 이 대화(세션)에서 `write_file`, `edit_file`로 파일을 바꾼 턴들이고, 오래된 턴이 먼저입니다. 체크포인트가 꺼져 있으면 `[]`입니다.
  - `restorable`은 그 턴부터 되돌리면(뒤의 턴을 먼저 되돌린 뒤) 그 파일이 그대로 되돌아가는지입니다. 그 뒤에 사람이나 bash가 파일을 바꿨거나(턴 사이에 바뀐 것 포함), 너무 커서 기록하지 않은 파일은 `false`입니다([파일 체크포인트](checkpoints.md)).
- `POST /api/chat/<id>/undo` `{"turn": n, "force"?: bool}` → 200 `{"restored": [path, ...], "skipped": [{"path", "reason"}], "partial": [{"path", "until_turn", "skipped_turn", "reason"}], "checkpoint"?: n}`
  - 체크포인트 `n`과 그 뒤의 모든 체크포인트를 최근 것부터 되돌립니다(`n`은 대화의 턴 번호가 아니라 `turn_end.checkpoint`, `GET .../checkpoints`의 `turn`입니다). 규칙은 `junees rewind --turn n`과 같습니다.
  - `checkpoint`는 이 되돌리기가 남긴 체크포인트 번호입니다. 되돌리기도 체크포인트로 남아(목록에서 `undid`가 채워진 항목) 다시 되돌릴 수 있습니다. 아무 파일도 바꾸지 않았으면 체크포인트를 남기지 않고 이 필드도 없습니다.
  - `reason`은 `바뀐 뒤 수정됨`, `너무 커서 기록하지 않음`, `심볼릭 링크로 바뀜`, `일반 파일이 아님`, `쓰기 정책이 막음`, `턴이 만든 파일이 그 뒤 수정되어 지우지 않음`, `쓰지 못함`(쓰거나 지우다 실패), `기록이 올바르지 않음`(manifest의 경로나 내용 파일 이름이 기록할 때의 모양이 아님) 중 하나입니다.
  - `restored`는 내용이 바뀐 파일 전부이고, `skipped`는 그대로 둔 파일입니다. 파일 하나는 둘 중 한 곳에만 한 번 나옵니다.
  - 여러 턴을 되돌리다 뒤 턴은 되돌리고 앞 턴에서 건너뛴 파일은 `restored`에 들어가고, `partial`에 어디까지 되돌렸는지가 함께 옵니다. `until_turn`은 되돌린 가장 오래된 턴(파일이 그 턴 전의 내용입니다), `skipped_turn`은 건너뛴 가장 최근 턴, `reason`은 그 이유입니다. 예: 턴 3이 만들고 밖에서 고친 뒤 턴 4가 고친 파일을 턴 3부터 되돌리면 `{"until_turn": 4, "skipped_turn": 3, "reason": "턴이 만든 파일이 그 뒤 수정되어 지우지 않음"}`입니다. 대화에 남는 줄에도 `c.txt (변경 기록 4까지 되돌림, 변경 기록 3은 건너뜀: …)`로 적힙니다. 이 줄은 체크포인트 번호를 대화의 턴과 헷갈리지 않게 "변경 기록 N"이라 부릅니다(`[되돌림] 변경 기록 1, 2의 파일 변경을 되돌렸습니다: …`).
  - 어느 대화든 턴이 돌고 있으면 409 `turn_running`이고, 되돌리는 동안에는 새 턴을 받지 않습니다(409 `turn_running`).
  - `n`이 1보다 작거나 숫자가 아니면 400 `bad_body`, `n` 이후에 체크포인트가 없으면 404 `not_found`, 체크포인트가 꺼져 있으면 409 `checkpoints_off`입니다.
  - 되돌린 뒤 이 대화의 다음 메시지 앞에 `[되돌림] …` 한 줄이 붙습니다. `dashboard.log`에 `chat <id> undo from turn <n>: <k> restored, <m> skipped`를 남깁니다(경로는 남기지 않습니다).
- 없는 대화 id는 404입니다.

### 이벤트

`data`는 `type`과 아래 필드를 가진 JSON입니다. 도구 미리보기와 오류는 세션 상세처럼 비밀 값을 가린 뒤 2,000자로 자릅니다. 승인 질문은 예외로, 자르지 않습니다(아래).

- `turn_start` `{turn}`
- `text` `{delta}`: 모델 답의 다음 조각입니다. 비밀 값을 가리려면 낱말이 온전해야 하므로, 토큰 단위가 아니라 공백이나 줄 끝까지 모아 보냅니다. 이어 붙이면 답이 됩니다. Junees가 덧붙이는 줄도 `text`로 옵니다. 경고, 대화 요약(compaction), 인용한 웹 페이지, 그리고 응답이 끊겨 다시 받을 때의 "(the reply broke off; asking for it again)"가 그렇습니다.
- `tool_use` `{name, input_preview}`: 도구 호출입니다. bash는 명령, 입력이 문자열 하나인 도구는 그 문자열, 나머지는 입력 JSON입니다. API가 직접 도는 도구(웹 검색)와 서브에이전트의 한 줄(`name`: `subagent <번호>`)도 이 이벤트로 옵니다.
- `tool_result` `{name, is_error, text_preview}`
- `hook` `{event, command, origin, result, took_ms, message?, reason?, error?, blocked?}`: 훅 하나가 돌았습니다([훅](hooks.md#세션-기록과-모델이-보는-것)). `result`는 `passed`, `blocked`, `timed_out`, `failed`입니다. 세션 기록에는 같은 내용이 Junees의 안내 줄(`notes`)로 남으므로, 새로 고친 뒤에는 이 이벤트가 아니라 그 안내 줄로 보입니다. 이 이벤트를 그리는 화면은 아직 없습니다.
- `model_switch` `{from, to, reason, retry_at}`: 주 모델(`from`)이 막혀 예비 모델(`to`)로 넘어갔습니다([예비 모델](configuration.md#예비-모델)). `reason`은 `rate_limit`(429), `overloaded`(529), `unavailable`(5xx나 응답 없음) 중 하나입니다. `retry_at`은 주 모델을 다시 시도할 수 있는 시각(RFC 3339 UTC, 초 단위)이고, 그 뒤 처음 시작하는 턴의 첫 호출이 주 모델로 갑니다. 넘어가기 전에 보낸 `text`가 있으면 그 앞에 응답이 끊겼다는 안내가 `text`로 옵니다. 무인 실행의 기다리기는 대화에 쓰지 않으므로 이 화면에는 오지 않습니다.
- `approval` `{question_id, kind, summary, target, reasons, preview, options, expires_in, expires_at}`
  - `kind`는 도구 이름입니다. `summary`는 질문을 평범한 줄로 쓴 글입니다. 명령이나 경로, `- 이유` 줄들, 바뀌는 내용 미리보기 순서이고, 터미널의 `??` 표시나 들여쓰기는 없습니다.
  - 화면이 직접 꾸밀 수 있도록 부분도 따로 줍니다. `target`(명령이나 경로), `reasons`(이유 문자열 배열, 영어), `preview`(미리보기, 없으면 `null`)입니다.
  - **`summary`, `target`, `reasons`, `preview`는 자르지 않습니다.** 터미널과 게이트웨이의 승인 질문처럼 실행될 내용을 전부 보여 줍니다. 잘리면 2,000자 뒤에 붙인 `; curl … | sh`가 보이지 않은 채 허용될 수 있기 때문입니다. 크기는 도구 입력 상한으로 묶여 있습니다.
  - 비밀 값은 설정과 변수에 있는 알려진 값(그리고 그 12자 이상 조각)만, 그 자리만 `[hidden]`으로 바꿉니다. 낱말 통째로 가리지 않으므로 `<키>;curl …|sh`의 뒷부분은 그대로 보입니다. 자격 증명처럼 보이기만 하는 낱말도 가리지 않습니다(터미널도 보여 줍니다). 제어 문자(ANSI 등)는 `\u{1b}`처럼 글자로 바꿉니다.
  - `expires_in`은 질문이 닫히기까지의 초(300)이고, `expires_at`은 그 시각(RFC 3339 UTC)입니다. 다시 연결해 같은 이벤트를 받아도 `expires_at`으로 남은 시간을 셀 수 있습니다.
  - `options`는 `["y","a","n"]`이고, "항상 허용"이 의미 없는 질문(매번 묻는 종류만 있음)이면 `["y","n"]`입니다.
- `approval_closed` `{question_id, answer, by}`: `by`는 `browser`, `timeout`(5분, `answer: "n"`), `cancel`(턴 취소, `answer: "n"`) 중 하나입니다.
- `usage` `{input_tokens, output_tokens, cost_usd}`: 이번 턴의 사용량이고, `turn_end` 바로 앞에 옵니다.
- `error` `{message}`: 턴이 실패한 이유이고, `turn_end` 바로 앞에 옵니다.
- `turn_end` `{turn, stop_reason, checkpoint?}`: `checkpoint`는 이 턴이 파일을 바꿔 체크포인트를 남겼을 때만 있고, `POST .../undo`의 `turn`에 넣는 번호입니다. `stop_reason`은 `end_turn`, `max_turns`, `cancelled`, `error` 중 하나입니다. 이 이벤트가 나갈 때는 이미 다음 메시지를 받을 수 있습니다.

### 대화 오류

- 400 `bad_body`, `bad_query`(`after`가 0 이상의 정수가 아님)
- 404 `not_found`: `--chat`이 아님, 없는 대화·질문·세션
- 405: `GET /api/chat` 등 메서드가 다름
- 409 `turn_running`, `session_busy`, `conflict`(닫힌 질문, claude-code 세션), `checkpoints_off`
- 500 `checkpoints_unreadable`, `undo_failed`: 체크포인트 폴더를 읽지 못함
- 413 `body_too_large`: 메시지 본문이 512KB보다 큼
- 429 `too_many_streams`
- 503 `chat_unavailable`(에이전트를 만들지 못함), `chat_stopped`(대화 스레드가 멈춤. 대시보드를 다시 띄웁니다)

## 정적 파일

`GET /`는 `index.html`이고, `web/dashboard/`의 다른 파일(`style.css`, `app.js`, 모듈 `*.js` 등)도 같은 경로로 줍니다(`/app.js`, 하위 폴더는 `/a/b.js`). `build.rs`가 빌드할 때 `web/dashboard/`를 훑어 목록을 만들고, 파일은 `include_bytes!`로 바이너리에 들어가므로 설치한 바이너리 하나로 동작합니다. 프론트엔드가 파일을 더해도 Rust 코드는 고치지 않습니다.

- `dev/` 폴더(fixtures, 개발 서버)와 `.`으로 시작하는 파일은 넣지 않고, 그 경로는 404입니다.
- Content-Type은 확장자를 따릅니다: `html`, `css`, `js`(`mjs`)는 `charset=utf-8`이 붙은 `text/html`, `text/css`, `text/javascript`이고, `json`, `svg`, `png`, `ico`, `webp`, `woff2`, `txt`도 받습니다. 그 밖의 확장자는 빌드가 실패하므로, 새 형식은 `src/dashboard/page_files.rs`의 `TYPES`에 더하거나 `dev/`에 둡니다. 실패 문구가 이 방법을 알려 줍니다.
- 편집기와 merge가 남기는 파일(`~`, `.orig`, `.rej`, `.bak`, `.swp`로 끝나는 이름)은 건너뜁니다.
- 파일 이름은 영문자, 숫자, `-`, `_`, `.`만 씁니다.
- 테스트가 서빙하는 스크립트의 `import`와 `index.html`의 `src`, `href`가 모두 서빙되는지 확인합니다.
