# 대시보드 (`junees dashboard`)

이 컴퓨터의 브라우저에서 Junees의 상태를 보는 읽기 전용 페이지입니다. 설정, 최근 세션, 사용량, 예약 작업, 웹훅 경로, 점검 결과를 보여 주고, 아무것도 바꾸지 않습니다.

```sh
junees dashboard                      # 127.0.0.1:7878에서 열고 브라우저를 띄운다
junees dashboard --no-open            # 주소만 출력한다
junees dashboard --listen 127.0.0.1:0 # 빈 포트를 고른다
```

시작하면 `Junees dashboard: http://127.0.0.1:7878/#token=<토큰>`을 출력하고, `--no-open`이 아니면 `xdg-open`(macOS는 `open`)으로 그 주소를 엽니다. Ctrl-C로 끝냅니다.

- 매번 실행할 때마다 32바이트 무작위 토큰을 새로 만듭니다. 페이지는 주소의 `#token=` 뒤에서 토큰을 읽어 API 요청마다 보냅니다. `#` 뒤는 브라우저가 서버에 보내지 않고 `Referer`에도 넣지 않습니다.
- 주소를 읽을 수 있는 사람은 대시보드도 읽을 수 있습니다. 터미널 기록이나 화면 공유에 주소가 남지 않게 합니다. 브라우저를 띄울 때 주소가 `xdg-open`의 인자로 잠깐 넘어가므로, 같은 컴퓨터의 다른 사용자가 프로세스 목록에서 볼 수 있는 환경이면 `--no-open`으로 열고 주소를 직접 붙여 넣습니다.
- 설정과 상태 폴더는 요청마다 새로 읽습니다. 설정을 바꾼 뒤 페이지를 새로 고치면 바로 보입니다.

## 보안

- **주소**: 루프백 주소(`127.0.0.1`, `::1` 등)에만 엽니다. `0.0.0.0`이나 다른 주소는 시작할 때 거절합니다.
- **토큰**: `/api/*`는 `Authorization: Bearer <토큰>`이 있어야 답합니다. 없거나 틀리면 401입니다. 비교는 상수 시간으로 합니다.
- **`Host`**: `127.0.0.1:<포트>`, `localhost:<포트>`, `[::1]:<포트>`만 받고, 아니면 421입니다. 다른 사이트의 이름을 127.0.0.1로 돌리는 공격(DNS rebinding)을 막습니다. 이 검사는 정적 파일에도 적용됩니다.
- **`Origin`**: 보내 왔으면 이 서버(`http://<Host>`)여야 하고, 아니면 403입니다. CORS 헤더는 보내지 않습니다.
- **메서드와 크기**: `GET`만 받고(405), 머리는 8KB(431), 본문은 1KB(413)까지입니다. 본문은 읽지 않습니다. 머리는 10초 안에 와야 하고, 연결 하나는 60초까지이며, 동시에 16개까지 받습니다.
- **헤더**: 모든 응답에 다음을 붙입니다.
  - `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'`
  - `X-Content-Type-Options: nosniff`
  - `Referrer-Policy: no-referrer`
  - `Cache-Control: no-store`
- **비밀 값**: 키, 토큰, 웹훅 시크릿은 응답에 넣지 않습니다. 키는 있는지와 출처(`env`, `config.toml`)만, 웹훅 시크릿은 있는지만 보냅니다. 세션 본문은 보내지 않고, 첫 사용자 메시지의 앞부분만 보냅니다. 이 미리보기, 제목, 실행 결과 메모, 점검 결과에서는 `junees doctor`와 같은 방식으로 비밀 값을 가립니다. 알려진 설정과 변수의 값, 그리고 자격 증명처럼 보이는 낱말(`secrets` 모듈)이 대상입니다. 가린 뒤에 자르므로 비밀 값의 조각이 남지 않습니다.

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

### 오류

모든 오류는 `{"error":"<짧은 영어 코드>","message":"<사람이 읽을 문장>"}` 모양입니다.

- 400 `bad_query`: `limit`, `days`가 범위 밖이거나 숫자가 아님
- 400 `bad_request`: 요청 머리를 읽을 수 없음, `Transfer-Encoding` 본문
- 401 `unauthorized`: 토큰이 없거나 틀림(`WWW-Authenticate: Bearer`)
- 403 `forbidden_origin`: 다른 출처의 `Origin`
- 404 `not_found`: 없는 경로
- 405 `method_not_allowed`: `GET`이 아님(`Allow: GET`)
- 413 `body_too_large`: 본문이 1KB보다 큼
- 421 `misdirected_request`: `Host`가 이 서버가 아님
- 431 `head_too_large`: 머리가 8KB보다 큼
- 500 `internal`, `bad_settings`: 상태 폴더나 설정(`prices`, `timezone`)을 읽지 못함. 메시지의 비밀 값은 가립니다.

## 정적 파일

`GET /`는 `index.html`이고, `web/dashboard/`의 `style.css`, `app.js`도 같은 경로로 줍니다. 파일은 `include_bytes!`로 바이너리에 들어가므로, 설치한 바이너리 하나로 동작합니다. 파일을 더하면 `src/dashboard/assets.rs`의 목록에도 넣습니다. 목록에 없는 경로(`web/dashboard/dev/` 등)는 404입니다. Content-Type은 확장자를 따릅니다(`text/html`, `text/css`, `text/javascript`, 모두 `charset=utf-8`).
