# 대시보드 개발용 파일

`junees dashboard`의 화면(`web/dashboard/*.html`, `*.css`, `*.js`)을 Rust 빌드 없이 고치고 확인할 때 쓰는 파일입니다. 이 폴더(`dev/`)는 바이너리에 넣지 않습니다.

## 가짜 서버

```sh
python3 web/dashboard/dev/serve.py            # http://127.0.0.1:8787/#token=<무작위 토큰> 을 출력
python3 web/dashboard/dev/serve.py --token dev --mode empty
```

- `web/dashboard/`의 정적 파일과, `fixtures/*.json`으로 만든 `/api/*` 응답을 내줍니다. 응답 모양은 로드맵 15단계 스펙의 API 계약과 같습니다.
- 실제 서버처럼 토큰(`Authorization: Bearer`)과 `Host`를 확인하고, CSP 등 보안 헤더를 붙입니다. 그래서 인라인 스크립트나 스타일을 쓰면 여기서도 막힙니다.
- fixtures의 시각은 2026-11-13 09:00 UTC를 기준으로 적었고, 서버가 지금 시각으로 옮겨서 보냅니다.
- 시작할 때 주소를 두 줄 출력합니다. `#token=` 주소는 `--no-open`처럼 사람이 붙여 넣는 경우이고, `#code=` 주소는 `junees dashboard`가 브라우저를 열 때 넘기는 일회용 주소입니다(60초, 한 번만, 틀린 코드 5번이면 무효). 화면은 이 코드를 `POST /api/token`으로 토큰과 바꿉니다. 새 일회용 주소가 필요하면 `GET /__dev/code`를 엽니다.
- 상태를 바꿔 보려면 실행 중에 `GET /__dev/mode/<모드>`를 엽니다(토큰 없이).
  - `ok`: fixtures 그대로
  - `empty`: 모든 목록이 빈 응답
  - `error`: 모든 API가 500
  - `slow`: 3초 뒤 응답
  - `hang`: 30초 뒤 응답(화면은 10초에 포기하고 오류를 보여야 합니다)
  - `unauthorized`: 모든 API가 401
  - `forbidden`: 모든 쓰기 요청(POST)이 403
  - `conflict`: 모든 쓰기 요청이 409
  - `busy`: 모든 쓰기 요청이 503 `busy`(저장 파일 잠금을 기다리다 포기한 경우)
- 쓰기 요청(`POST /api/cron/<id>/pause|resume`, `POST /api/webhooks/<이름>/pause|resume`)은 계약의 규칙(토큰, 같은 출처의 `Origin`, `application/json`이거나 없는 `Content-Type`, 비었거나 `{}`인 본문)을 그대로 검사하고, fixtures를 메모리에서만 바꿉니다. 서버를 다시 띄우면 처음 상태로 돌아갑니다.
  - 홈 폴더 때문에 멈춘 항목의 `resume`과, 시각이 지난 한 번만 도는 작업(`one-time-backup`)의 `resume`은 409를 돌려줍니다. 문구는 `fixtures/actions.json`에 있습니다.
- 세션 상세(`GET /api/sessions/<id>?offset=&limit=`)는 `fixtures/session_details.json`에서 잘라 보냅니다. `20261111-0920-1c4e`는 메시지가 230개라 "더 보기"를 두 번 눌러 볼 수 있습니다.

`sessions.json`과 `session_details.json`의 한 세션(`20261112-1530-77aa`)은 제목, 미리보기, 본문에 HTML 태그와 `javascript:` 링크를 넣어 두었습니다. 화면에 글자 그대로 보여야 하고, 이미지나 스크립트가 만들어지면 안 됩니다.

## 테스트

DOM을 쓰지 않는 함수(`format.js`, `usage.js`, `routes.js`, 오류 문구와 페이지 합치기)는 Node 내장 테스트로 확인합니다. 의존성은 없습니다.

```sh
node --test web/dashboard/dev/test/
```

## 대화 (19단계)

- `--chat`으로 띄우면 `/api/status`가 `"chat": true`를 돌려주고 `/api/chat` API가 켜집니다. 답은 `chat_sim.py`의 정해진 대본으로 만듭니다.
  - 메시지에 `정리`, `지워`, `승인`이 있으면 bash 명령 승인(허용, 항상 허용, 거절)을 묻습니다.
  - `파일`이 있으면 작업 폴더 밖 쓰기 승인(허용, 거절)을 묻습니다.
  - `오류`가 있으면 `error` 이벤트로 끝납니다.
  - `한도`가 있으면 단계 수 한도(`max_turns`)로 멈춥니다.
  - `긴`이 있으면 2,100자짜리 명령 끝에 `| sh`가 붙은 승인 질문을 띄웁니다.
- 이벤트 모양은 실제 서버(#109)와 같습니다. `turn_start`에 사용자 글이 없고, 질문에는 `target`, `reasons`, `preview`, `expires_at`이 있습니다. 턴을 취소하면 열린 질문은 `by: "cancel"`로 닫힙니다. 이벤트 번호는 서버 전체에서 하나씩 늘고, 스트림은 동시에 8개(넘으면 429), 열린 대화는 16개까지입니다.
- `--approval-seconds 30`처럼 주면 승인 질문이 그 시간 뒤 거절로 닫힙니다(기본 300초).
- `GET /__dev/drop-streams`는 열린 이벤트 스트림을 모두 끊습니다. 화면이 `after`로 다시 연결하는지 볼 때 씁니다.
- 세션 화면의 "이 세션 이어서 대화"는 fixtures에 있는 세션만 이어 갈 수 있습니다. 다른 id는 404입니다.


## 파일 되돌리기 (20단계)

- `--chat`의 대본에서 메시지에 `만들어`나 `고쳐`가 있으면 메시지에 적힌 파일(`a.txt`, `b.md` 등, 없으면 `notes.md`)을 `write_file`이나 `edit_file`로 씁니다. 파일은 메모리에만 있고(`/home/me/work/junees` 아래), 그 턴의 `turn_end`에 `checkpoint` 번호가 붙습니다.
- `GET /api/chat/<id>/checkpoints`와 `POST /api/chat/<id>/undo`는 실제 서버(#112)와 같은 모양으로 답합니다. 번호는 파일을 바꾼 턴만 세고, `restorable`은 그 기록부터 되돌릴 때 기준이며, 되돌리기도 기록으로 남고(목록의 `undid`, 응답의 `checkpoint`, #118), 다음 메시지 앞에 `[되돌림]` 줄이 `"notice": true` 블록으로 따로 붙습니다(#124). 턴이 돌고 있으면 409 `turn_running`입니다.
- `GET /__dev/edit?path=a.txt`는 bash처럼 밖에서 파일을 고칩니다(기록 없음). 건너뜀과 부분 되돌림을 볼 때 씁니다.
- `GET /__dev/undo-late/35`는 되돌리기를 한 뒤 35초 늦게 답합니다. 화면은 30초에서 기다림을 끝내므로, 답을 받지 못한 되돌리기를 볼 때 씁니다. `/__dev/undo-late/0`으로 되돌립니다.
- `GET /__dev/undo-slow/35`는 되돌리기 자체가 35초 걸리게 합니다(그동안 새 턴은 409). 화면이 30초에 기다림을 끝낸 뒤에야 되돌리기가 끝나므로, 화면이 목록을 몇 번 더 받아 확인하는지 볼 때 씁니다. 60초 이상이면 화면은 "확인하지 못했습니다"와 "다시 확인" 버튼을 보여 줍니다.
- `--no-checkpoints`로 띄우면 `checkpoints = false`처럼 목록은 빈 배열이고 되돌리기는 409 `checkpoints_off`입니다.
