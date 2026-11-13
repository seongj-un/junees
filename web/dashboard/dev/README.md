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
- 상태를 바꿔 보려면 실행 중에 `GET /__dev/mode/<모드>`를 엽니다(토큰 없이).
  - `ok`: fixtures 그대로
  - `empty`: 모든 목록이 빈 응답
  - `error`: 모든 API가 500
  - `slow`: 3초 뒤 응답
  - `hang`: 30초 뒤 응답(화면은 10초에 포기하고 오류를 보여야 합니다)
  - `unauthorized`: 모든 API가 401
  - `forbidden`: 모든 쓰기 요청(POST)이 403
  - `conflict`: 모든 쓰기 요청이 409
- 쓰기 요청(`POST /api/cron/<id>/pause|resume`, `POST /api/webhooks/<이름>/pause|resume`)은 계약의 규칙(토큰, 같은 출처의 `Origin`, `application/json`이거나 없는 `Content-Type`, 비었거나 `{}`인 본문)을 그대로 검사하고, fixtures를 메모리에서만 바꿉니다. 서버를 다시 띄우면 처음 상태로 돌아갑니다.
  - 홈 폴더 때문에 멈춘 항목의 `resume`과, 시각이 지난 한 번만 도는 작업(`one-time-backup`)의 `resume`은 409를 돌려줍니다. 문구는 `fixtures/actions.json`에 있습니다.
- 세션 상세(`GET /api/sessions/<id>?offset=&limit=`)는 `fixtures/session_details.json`에서 잘라 보냅니다. `20261111-0920-1c4e`는 메시지가 230개라 "더 보기"를 두 번 눌러 볼 수 있습니다.

`sessions.json`과 `session_details.json`의 한 세션(`20261112-1530-77aa`)은 제목, 미리보기, 본문에 HTML 태그와 `javascript:` 링크를 넣어 두었습니다. 화면에 글자 그대로 보여야 하고, 이미지나 스크립트가 만들어지면 안 됩니다.

## 테스트

DOM을 쓰지 않는 함수(`format.js`, `usage.js`, `routes.js`, 오류 문구와 페이지 합치기)는 Node 내장 테스트로 확인합니다. 의존성은 없습니다.

```sh
node --test web/dashboard/dev/test/
```
