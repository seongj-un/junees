# 변경 기록

## 다음 버전 (미출시)

### 새 기능

- 코드 탐색 도구 `grep`, `glob`, `list_dir`: Junees 안에서 도는 읽기 전용 도구로, bash의 `rg`, `find`, `ls` 대신 쓴다. ripgrep의 무시 규칙(`.gitignore`, `.ignore`, `.git/info/exclude`)을 따르고 `.git/`, `target/`, `node_modules/`, 점 파일(`hidden`이 아니면), 심볼릭 링크를 건너뛴다. 탐색 중에 만나는 파일과 폴더마다 `read_file`과 같은 읽기 규칙을 적용해 숨긴 경로는 보이지도, 내려가지도 않는다. 상한(파일 200, 줄 400, 출력 30,000자, glob 500, list_dir 400)과 10초 제한이 있고, `grep`의 `content` 출력은 비밀 값을 가린다. 작업 폴더 밖을 탐색하면 대화에서는 묻고, 무인 실행에서는 거절한다. 새 의존성: `ignore`, `globset`, `regex` ([코드 탐색](docs/tools.md#코드-탐색-grep-glob-list_dir))
- 파일 체크포인트와 되돌리기: `write_file`, `edit_file`이 파일을 바꾸기 직전의 내용과 권한(없던 파일은 "없었음")을 턴마다 `$JUNEES_HOME/checkpoints/<session>/<turn>/`(0700/0600)에 남긴다. `/checkpoints`, `/undo [N]`, `junees rewind --session <id> [--list] [--turn <n>] [--force] [--yes]`, 대시보드 `GET /api/chat/<id>/checkpoints`(되돌리기 기록이면 `undid`), `POST /api/chat/<id>/undo`(남긴 체크포인트 번호 `checkpoint`)로 최근 턴부터 되돌린다. 그 뒤 바뀐 파일은 `--force` 없이는 건너뛰고, 링크는 따라가지 않으며, 파일 도구의 쓰기 규칙을 그대로 거친다. 되돌린 내용은 세션의 다음 메시지 앞에 한 줄로 알리고, 되돌리기도 체크포인트로 남아 되돌릴 수 있다. 2MB가 넘는 파일은 기록하지 않고, 세션 200MB·전체 1GB·30일 상한이 있다. bash가 바꾼 파일은 기록하지 않는다. 체크포인트에는 원래 내용이 남으므로 `checkpoints = false`(`JUNEES_CHECKPOINTS=0`)로 끌 수 있다 ([파일 체크포인트](docs/checkpoints.md))
- 대시보드 대화(`junees dashboard --chat [--dir <폴더>]`): 페이지에서 Junees와 대화하고, 승인 질문을 페이지 버튼으로 답한다. 응답은 이벤트 스트림(`GET /api/chat/<id>/events`, `after`로 다시 연결)으로 오고, 세션은 일반 세션으로 저장된다. 기본은 꺼져 있고, 켜면 페이지가 터미널 대화와 같은 권한을 가진다. `--chat`이면 브라우저를 열지 않고 `#token=` 주소만 출력한다(`--open`으로 열 수 있고, 그때는 일회용 코드 경합을 경고한다). 일회용 코드 교환은 `dashboard.log`에 남는다. api 백엔드만 지원한다. `/api/status`에 `chat`이 더해졌다 ([대화](docs/dashboard.md#대화---chat))

### 바뀐 것

- 승인 질문의 문구를 한국어로 바꿨다. 묻는 이유(`` `rm`: 파일을 지웁니다 ``, `` `gh pr merge`: 내 GitHub 계정으로 pull request를 merge합니다 `` 등), 미리보기의 안내(`(새 파일, 3줄)`, `... 5줄 더`), 터미널 선택지, 게이트웨이(디스코드, 텔레그램, Slack)의 질문·버튼·결과 줄·안내, 브라우저 도구의 위험 설명이 한국어다. 터미널, 게이트웨이, 대시보드가 같은 이유 문구를 보여 준다. 무엇을 묻는지는 그대로이고, "항상 허용"이 기억하는 종류 이름(`bash: delete files` 등)과 모델이 읽는 거절 결과(`the user declined: ...`)는 바꾸지 않았다 ([승인](docs/safety.md))
- 승인 질문과 되돌리기 안내의 문구를 다듬었다. 시간은 `5분`, `1시간 30분`처럼 쓰고, `항상 허용하면:` 줄은 `항상 허용 범위:`로, 허용 목록 안내는 "Junees에게 답할 수 있습니다"로 바꿨다. 숫자나 커밋 뒤 조사는 읽는 소리에 맞춘다(`변경 기록 2는 건너뜀`, GitHub 흐름 코멘트의 `커밋 abc1234를`). 기본 규칙 설명은 원문임을 밝혀 따로 붙인다 ([승인](docs/safety.md))

## 0.4.0 (2026-11-13)

### 업그레이드할 때 확인할 것

- 기존 설정의 동작이 바뀌는 변경은 없습니다. 새 기능은 모두 켜야 동작합니다(`junees dashboard`, `junees mcp serve`, `transcribe_base_url`).
- 받아쓰기 키 `JUNEES_TRANSCRIBE_API_KEY`(`transcribe_api_key`)는 Junees가 읽는 자격 증명이라, 다른 키처럼 bash 도구, MCP 서버, 플러그인, `claude` 자식의 환경에서 지웁니다.
- `junees dashboard`는 세션 대화 전체를 보여 주고 예약 작업과 웹훅을 멈추거나 켤 수 있습니다. 웹훅을 받거나 예약 작업이 도는 컴퓨터에서 띄울 때는 [docs/dashboard.md](docs/dashboard.md) 첫머리의 "웹훅을 받거나 예약 작업이 도는 컴퓨터에서" 항목을 확인합니다. 무인 실행의 bash는 루프백에 접속할 수 있습니다.

### 새 기능

- 게이트웨이 음성 메시지 받아쓰기: 디스코드(음성 메시지, `audio/*` 첨부), 텔레그램(`voice`, `audio`, 오디오 문서), 슬랙(`audio/*` 파일)의 오디오를 OpenAI 호환 `POST <transcribe_base_url>/audio/transcriptions`(OpenAI, Groq, 로컬 whisper 서버)로 받아써서 `[음성 메시지 받아쓰기]`를 붙인 사용자 메시지로 처리한다. `transcribe_base_url`이 없으면 꺼져 있고, 이때 오디오는 이름만 안내하고 같은 메시지의 글과 이미지는 이전처럼 실행한다. 켜져 있는데 받아쓰지 못하면 그 메시지는 실행하지 않고 원인을 안내한다. 시간 초과(`transcribe_timeout`, 기본 120초)는 다시 보내지 않는다. 키는 주소와 같은 곳에서만 읽고, 오디오는 디스크에 쓰지 않는다. `junees doctor`에 받아쓰기 점검을 더했다 (#104) ([음성 메시지 받아쓰기](docs/gateways.md#음성-메시지-받아쓰기))
- `junees mcp serve`: Junees를 stdio MCP 서버(프로토콜 2025-06-18)로 띄워 다른 에이전트(Claude Desktop, Claude Code, Hermes, Cursor 등)가 일을 맡기게 한다. 도구는 `junees_run`(에이전트 한 턴, 세션 이어 가기, 진행 알림, 취소), `junees_sessions`, `junees_session`이다. 기본은 읽기 전용이고(`--allow-write`, `--allow-bash`로 넓힘), 웹훅과 같은 무인 실행 규칙을 적용한다. 홈 폴더와 claude-code 백엔드는 거절한다. 세션 도구는 `--dir` 폴더의 모든 세션(터미널 세션 포함)을 클라이언트에 보여 준다 (#105) ([Junees를 MCP 서버로](docs/mcp.md#junees를-mcp-서버로))
- 대시보드 2차: 세션 하나의 대화를 페이지 단위로 보는 `GET /api/sessions/<id>`(비밀 값을 가린 뒤 블록마다 8,000자/2,000자로 자름, 이미지 데이터와 thinking 내용은 보내지 않음)와 예약 작업·웹훅 경로의 멈춤과 재개 `POST /api/cron/<id>/pause|resume`, `POST /api/webhooks/<name>/pause|resume`. 쓰기는 같은 출처의 `Origin`이 반드시 있어야 하고 본문은 비었거나 `{}`만 받으며, CLI와 같은 함수와 저장 파일 잠금으로 바꾸고 `$JUNEES_HOME/dashboard.log`에 한 줄씩 남긴다. 홈 폴더 때문에 멈춘 항목은 대시보드에서 다시 켜지 않고(409) 터미널의 `allow-home` 명령을 안내한다 (#102, #100) ([대시보드](docs/dashboard.md))
- 대시보드가 브라우저를 열 때 주소에 토큰 대신 일회용 코드(`#code=`, 60초, 한 번만, 틀린 코드 5번이면 무효)를 넘기고, 페이지가 `POST /api/token`으로 토큰과 바꾼다. 브라우저 명령줄은 같은 컴퓨터의 다른 프로세스(무인 실행 포함)가 읽을 수 있기 때문이다. 이때 터미널에는 토큰을 출력하지 않고, `--no-open`이거나 `xdg-open`(`open`)을 실행하지 못했을 때만 지금처럼 `#token=` 주소를 출력한다 (#102, #100)
- `junees dashboard`: 이 컴퓨터의 브라우저에서 상태, 세션 목록과 대화, 사용량, 예약 작업, 웹훅, 점검을 보고, 예약 작업과 웹훅을 멈추고 다시 켜는 웹 대시보드(쓰기는 위 2차 항목). 루프백 주소에만 열고, 실행마다 새로 만든 토큰이 있어야 API가 답한다. `Host`와 `Origin` 검사, CSP 등 보안 헤더, 요청 크기와 시간 제한을 둔다. 키와 시크릿은 있는지만 보이고, 세션 목록에는 비밀 값을 가린 앞부분 200자만 보인다. `web/dashboard`의 편집기 백업 파일은 빌드에서 건너뛴다 (#97, #98, #101) ([대시보드](docs/dashboard.md))

## 0.3.0 (2026-11-13)

### 업그레이드할 때 확인할 것

- **Anthropic 키는 주소와 같은 곳에서만 읽음 (#88)**
  - `ANTHROPIC_API_KEY`(`api_key`)를 이제 주소 `ANTHROPIC_BASE_URL`(`base_url`)과 같은 곳에서만 읽습니다. 주소가 설정 파일에 있으면 `api_key`만, 환경 변수에 있으면 `ANTHROPIC_API_KEY`만 씁니다. 주소를 적지 않았거나 기본 주소(`https://api.anthropic.com`, 끝의 `/`는 상관없음)를 그대로 적었으면 지금처럼 환경 변수, 설정 파일 순서로 찾습니다. OpenAI 키도 같아서, `OPENAI_BASE_URL`이나 `openai_base_url`에 기본 주소 `https://api.openai.com/v1`을 적었으면 어느 쪽 키든 씁니다. 지금까지는 둘을 따로 읽어서, 셸에 export한 키가 설정 파일에 적은 프록시로 가거나 설정 파일의 키가 셸에 적은 주소로 갈 수 있었습니다.
  - 다음 두 경우는 올린 뒤 `no API key for <주소>` 오류로 시작하지 않습니다.
    - `base_url`은 설정 파일에, 키는 환경 변수(셸, systemd `EnvironmentFile`)에만 둔 경우: 키를 `api_key`로 옮기거나, 주소도 `ANTHROPIC_BASE_URL`로 옮깁니다.
    - `ANTHROPIC_BASE_URL`은 환경 변수에, 키는 설정 파일 `api_key`에만 둔 경우: 키도 `ANTHROPIC_API_KEY`로 주거나, 주소를 `base_url`로 옮깁니다.
  - 양쪽에 키가 다 있으면 오류 없이 주소 쪽 키를 씁니다. 그래서 예전에 환경 변수 키가 이기던 설정(파일에 `base_url`과 `api_key`, 셸에 `ANTHROPIC_API_KEY`)은 이제 파일의 키를 씁니다.
  - 올리기 전에 `junees doctor`를 돌려 `Anthropic API key` 줄을 확인합니다. "set, but not where the address is set"이면 위처럼 옮깁니다.
- **Anthropic 주소 검사와 리다이렉트 (#88)**
  - api 백엔드의 `ANTHROPIC_BASE_URL`(`base_url`)은 이제 OpenAI 주소처럼 `https://`이거나 이 컴퓨터의 `http://`(`localhost`, `127.0.0.1`, `::1`)여야 하고, 사용자 이름이나 비밀번호(`https://user:pass@proxy`)를 담을 수 없습니다. 아니면 시작할 때 `` ANTHROPIC_BASE_URL (`base_url`) must use https ... `` 오류로 끝나고 `junees doctor`의 `provider` 줄이 실패로 알립니다. 다른 컴퓨터의 프록시를 평문 `http://`로 쓰고 있었다면 https로 바꾸거나 이 컴퓨터에서 터널을 거쳐 씁니다. claude-code 백엔드는 이 주소를 쓰지 않으므로 검사하지 않습니다(`claude`가 이 변수를 따로 읽어도 그대로입니다).
  - Anthropic 요청은 이제 리다이렉트(3xx)를 따라가지 않습니다. reqwest는 다른 호스트로 넘어갈 때 `x-api-key`를 떼지 않으므로, 프록시가 3xx로 다른 곳을 가리키면 키가 그곳으로 갔습니다. 3xx를 돌려주는 프록시는 이제 `API error 307` 같은 오류가 되니, 최종 주소를 `base_url`에 직접 적습니다.
- **작업 폴더가 홈인 예약 작업과 웹훅 경로는 거절 (#84)**
  - 예약 작업, 웹훅 경로, GitHub 흐름(`add-github`)의 작업 폴더가 홈 자체이거나 홈을 품은 폴더(`/home`, `/`)면, 추가할 때 거절하고 이미 있는 것은 처음 실행할 때 실패로 기록하고 전달한 뒤 멈춥니다(`paused`. 일정이나 요청마다 실패가 다시 오지 않습니다). 막을 경로 목록(셸 시작 파일, systemd, `~/.ssh` 등)으로는 파이썬 `.pth`, 에디터 설정, 다른 저장소의 git 훅 같은 나중에 실행되는 파일을 다 막을 수 없기 때문입니다.
  - 올리기 전에 `junees doctor`를 돌려 `job <이름> folder`, `route <이름> folder` 경고가 있는지 봅니다. `junees cron list`와 `junees webhook list`의 폴더 줄로도 확인할 수 있습니다.
  - 고치는 법(권장): 작업을 지우고 프로젝트 폴더로 다시 추가합니다. `junees cron rm <id>` 뒤 `junees cron add "<일정>" "<프롬프트>" --dir ~/work/<프로젝트>`.
  - 홈에서 계속 돌려야 하면 지우지 말고 `junees cron allow-home <id>`, `junees webhook allow-home <이름>`으로 허용 표시를 붙입니다(#89). 홈 폴더 때문에 멈춘 작업과 경로는 이 명령이 다시 시작하고, id, 일정, 웹훅 시크릿이 그대로라 GitHub의 Secret도 바꾸지 않아도 됩니다. `junees cron pause`로 직접 멈춘 것은 멈춘 채 둡니다. 허용을 거두려면 `disallow-home`을 씁니다(작업 폴더가 홈이면 바로 멈춥니다).
  - `XDG_CONFIG_HOME`이 홈 아래의 경로로 있으면(홈 자체는 제외) 무인 실행이 `$XDG_CONFIG_HOME/systemd`, `environment.d`, `autostart`, `git`, `fish`에도 쓰지 못합니다(`~/.config` 아래와 같은 규칙).
- **비밀 값 검사가 OpenAI 키 모양도 찾음 (#81, #85)**
  - 스킬 저장, GitHub 도구와 flow가 바깥으로 올리는 글, doctor 출력의 비밀 값 검사가 이제 OpenAI와 OpenRouter 키 모양(`sk-proj-`, `sk-svcacct-`, `sk-admin-`, `sk-or-v1-`)과 접두 없는 긴 `sk-` 키(`sk-` 뒤에 영숫자 32자 이상)도 찾습니다. 그래서 0.2.0에서 통과하던 예시 키가 든 스킬이나, 키 모양 문자열이 든 PR 코멘트가 거절될 수 있습니다. 예시에는 `sk-...`처럼 짧게 줄인 값을 씁니다.
- **`OPENAI_API_KEY`를 bash에서 숨김 (OpenAI 호환 provider, #81)**
  - `OPENAI_API_KEY`는 이제 Junees가 읽는 자격 증명이라 `ANTHROPIC_API_KEY`처럼 bash 도구, MCP 서버, 플러그인, `claude` 자식의 환경에서 지웁니다. 모델이 실행하는 스크립트가 이 변수를 쓰고 있었다면 그 스크립트가 직접 읽도록 바꿔야 합니다.
- **웹훅 실행의 파일 도구와 `sandbox_read` (#77)**
  - 웹훅 실행(GitHub 흐름의 단계 포함)의 파일 도구(`read_file`)는 이제 `sandbox_read`를 받지 않습니다. 작업 폴더, 임시 폴더, `sandbox_write`, 그리고 새 설정 `webhook_sandbox_read`(`JUNEES_WEBHOOK_SANDBOX_READ`, 기본은 없음)의 폴더만 읽습니다.
  - 그래서 예약 작업용으로 설정 파일에 `sandbox_read = "/, ~/.ssh"`를 적어도, 웹훅 실행의 `read_file`은 디스크 전체를 읽지 않습니다. 0.2.0에서 안내한 "`JUNEES_SANDBOX_READ`는 예약 작업 데몬의 환경에만" 우회는 더 필요 없습니다.
  - 웹훅 실행의 파일 도구가 `sandbox_read`의 폴더를 읽고 있었다면 그 폴더를 `webhook_sandbox_read`에 옮겨 적습니다. 여기에 자격 증명(`~/.ssh` 등)을 적어도 웹훅 실행은 읽지 않습니다.
  - 예약 작업과 대화는 그대로입니다: 예약 작업의 파일 도구와 bash는 계속 `sandbox_read`를 따르고, 거기 적은 자격 증명을 예약 작업의 bash가 다시 읽습니다. 웹훅 실행의 bash 읽기도 계속 `sandbox_read`를 따릅니다(주면 좁아질 뿐이고, 자격 증명은 0.2.0처럼 모두 숨깁니다).
- **작업 폴더가 홈인 무인 실행의 bash (#82, #75 남은 위험)**
  - 예약 작업과 웹훅 실행의 bash는 이제 파일 도구와 같이 셸 시작 파일(`~/.bashrc`, `~/.profile` 등), systemd user 유닛(`~/.config/systemd`, `~/.local/share/systemd`), `~/.config/autostart`, `~/.gitconfig`, `~/.config/git`, cargo 설정(`~/.cargo/config.toml`, `~/.cargo/config`), `~/.ssh`의 `known_hosts*`를 뺀 모든 항목(`config`, `rc`, `environment`, `authorized_keys*`, `config`가 부르는 스크립트와 `Include` 파일), `~/.local/bin`, `~/bin`, 그리고 데몬 `PATH`에 든 홈 아래 폴더(`~/.cargo/bin` 등)에 쓰지 못합니다(읽기는 됩니다). 파일 도구의 쓰기 거절 목록에도 cargo 설정, `~/.local/bin`, `~/bin`, PATH 폴더가 더해졌습니다(`~/.ssh`는 파일 도구가 원래 통째로 거절합니다).
  - 작업 폴더가 그런 프로그램 폴더이거나 그 안에 있으면(`~/scripts`가 `PATH`에 있고 작업 폴더가 `~/scripts/job`) 그 폴더는 작업 폴더에 대해서만 보호하지 않으므로 작업 폴더에는 그대로 씁니다. 작업 폴더가 프로그램 폴더 자체면 실행을 시작할 때 경고합니다(그 실행이 쓴 프로그램은 다음 셸에서 돕니다).
  - 작업 폴더가 홈이면 무인 실행의 bash는 홈, `~/.config`, `~/.local/share` 바로 아래에 새 파일이나 폴더를 만들지 못합니다(이미 있는 하위 폴더 안은 됩니다). 예약 작업이 `cargo install`이나 `pip install --user`로 `~/.cargo/bin`, `~/.local/bin`에 설치하던 경우도 실패합니다. `ControlPath`를 `~/.ssh` 안에 둔 ssh 다중화도 소켓을 만들지 못합니다. 작업과 웹훅 경로에는 프로젝트 폴더를 주세요.
  - 대화, `-p`, `task` 모드는 그대로입니다.
- **게이트웨이 승인 버튼 (#79, #80)**
  - 디스코드, 텔레그램, Slack 게이트웨이의 승인 질문에 버튼(Allow, Always allow, Decline)이 붙습니다. 글 답장 `y`/`n`/`a`도 그대로 됩니다.
  - 기존 Slack 앱은 앱 설정의 Interactivity & Shortcuts에서 Interactivity를 켜야 버튼이 동작합니다(Socket Mode라 Request URL은 필요 없습니다). 켜지 않으면 버튼을 눌러도 Junees에 아무것도 오지 않으니 글로 답합니다.
  - 디스코드 앱 설정의 Interactions Endpoint URL은 비워 둡니다. 주소가 있으면 버튼 누름이 Gateway로 오지 않습니다.
  - 질문에 답한 사람이 30초 안에 답 낱말 하나만(`y`, `n` 등) 다시 보내면 새 요청이 되지 않고 "이미 답했다"는 안내만 갑니다.

### 새 기능

- OpenAI 호환 API provider (#81): `provider = "openai"`, `openai_base_url`, `openai_api_key`(`OPENAI_API_KEY`)로 api 백엔드가 OpenAI Chat Completions 형식의 서버(OpenAI, OpenRouter, Ollama, vLLM, LM Studio)를 부른다. 도구 호출(병렬 포함), 이미지, 스트리밍, 사용량과 `prices` 비용, 재시도와 컨텍스트 초과 압축이 그대로 동작하고, 확장 사고·서버 웹 검색·프롬프트 캐시 표시는 꺼진다. `junees doctor`가 provider와 주소를 보여 주고 `--online`이면 모델 목록을 확인한다 ([OpenAI 호환 API](docs/configuration.md#openai-호환-api))
- OpenAI 호환 provider 후속 (#83)
  - 출력 상한을 `max_completion_tokens`로 보내는 곳을 OpenAI의 지역 도메인(`eu.api.openai.com` 등 `*.api.openai.com`)과 Azure OpenAI v1 API(`*.openai.azure.com`, `*.services.ai.azure.com`, `*.cognitiveservices.azure.com`)로 넓혔다. 새 설정 `openai_max_tokens_field`(`JUNEES_OPENAI_MAX_TOKENS_FIELD`: `auto`, `max_tokens`, `max_completion_tokens`)로 직접 고를 수 있다.
  - 새 설정 `openai_reasoning_effort`(`JUNEES_OPENAI_REASONING_EFFORT`): 정했을 때만 `reasoning_effort`로 보낸다. 정해 두면 압축 요약 호출의 상한에 추론용 25,000 토큰을 더한다. 창이 작아 서버가 이 상한을 400으로 거절하면 더한 만큼 빼고 한 번 더 보낸다.
  - 추론 토큰(`completion_tokens_details.reasoning_tokens`)은 출력과 비용에 그대로 두고 기록에 따로 남기며, 다음 요청에 실리지 않으므로 컨텍스트 크기에서 뺀다.
  - 스트림 중간에 온 크레딧 소진·지출 한도 오류(`insufficient_quota` 등)를 더는 재시도하지 않는다(오류 응답, 200 본문 오류와 같은 기준). `type`만 `insufficient_quota`인 오류도 재시도하지 않는다.
  - 환경 변수와 설정 파일의 우선순위(키마다 환경 변수가 이김)를 문서에 적었다. 다만 OpenAI 키는 주소와 같은 곳에서만 읽는다: `openai_base_url`이 설정 파일에 있으면 `openai_api_key`만, `OPENAI_BASE_URL`이 환경 변수에 있으면 `OPENAI_API_KEY`만 쓴다. 다른 도구 때문에 셸에 둔 `OPENAI_API_KEY`가 설정 파일의 다른 서버(OpenRouter 등)로 가지 않는다.
- 디스코드와 텔레그램 승인 버튼 (#79), Slack Block Kit 승인 버튼 (#80)
- `junees setup`의 Slack 단계(봇 토큰, 앱 토큰, 허용할 member id) (#80)
- 웹훅 실행 전용 읽기 설정 `webhook_sandbox_read` (#78)
- 예약 작업과 웹훅 경로의 `--allow-home`: 홈(또는 홈을 품은 폴더)을 작업 폴더로 쓰는 것을 작업마다 명시적으로 허용 (#84)
- `junees cron allow-home`/`disallow-home`, `junees webhook allow-home`/`disallow-home`: 이미 있는 작업과 경로에 `--allow-home` 표시를 붙이거나 거둔다. 붙이면 홈 폴더 때문에 멈춘 것을 다시 시작하고, 거두면 작업 폴더가 홈일 때 바로 멈춘다. 모델이 bash로 `allow-home`을 실행하면 "항상 허용"과 상관없이 매번 승인을 묻는다 (#89)

### 고친 것

- Rust 1.100에서 `unused ExitCode` 경고로 빌드가 깨지던 것 (#87)
- 가끔 실패하던 claude-code 취소 테스트: 죽은 프로세스(`X` 상태)를 실행 중으로 보던 판정 (#90, #94)

## 0.2.0 (2026-11-12)

0.1.0 뒤로 들어간 기능과 보안 수정입니다. 아래 "업그레이드할 때 확인할 것"은 기존 설정이나 사용 방식이 다르게 동작하게 되는 변경이라, 올리기 전에 꼭 읽어 주세요.

### 업그레이드할 때 확인할 것

**예약 작업과 웹훅(무인 실행)**

- **claude-code 백엔드 (#74)**
  - `backend = "claude-code"`인 예약 작업과 웹훅 실행은 실패로 기록됩니다.
  - 계속 돌리려면 `claude_code_unattended = 1`을 켭니다. 다만 `sandbox = "require"`를 직접 적어 두었다면 이 설정을 켜도 거절됩니다.
  - 또는 `junees cron daemon`과 `junees webhook serve`의 환경에 `JUNEES_BACKEND=api`를 줘서 api 백엔드로 돌립니다.
- **웹훅 시크릿 (#74)**
  - `junees webhook add`는 시크릿을 기본으로 만듭니다.
  - 시크릿 없이 추가했던 기존 경로는 403으로 거절됩니다. `serve`, `list`, `doctor`가 알려 주니 다시 추가하세요.
  - 시크릿 없이 받으려면 `--no-secret`을 직접 줍니다(루프백 전용).
- **파일 도구와 bash 범위 (#75)**
  - 파일 도구는 작업 폴더, 임시 폴더, `sandbox_write`/`sandbox_read`로 지정한 곳만 다룹니다.
  - bash는 홈의 자격 증명을 읽지 못합니다(`~/.aws`, `~/.config/gh` 등). 예약 작업에서는 `~/.ssh`의 개인 키만 숨기고, 웹훅 실행에서는 `~/.ssh` 전체를 숨깁니다.
  - 예약 작업이 ssh 키 같은 자격 증명을 써야 하면, 예약 작업 데몬의 환경에만 그 경로를 줍니다: `JUNEES_SANDBOX_READ="/, ~/.ssh" junees cron daemon`.
  - 설정 파일의 `sandbox_read`는 예약 작업과 웹훅 모두에 걸리고, 파일 도구의 읽기 범위도 같이 넓힙니다. 예를 들어 `/`를 적으면 무인 실행의 `read_file`이 다시 디스크 전체를 읽습니다. 홈의 자격 증명, 보호 경로, 웹훅의 `sessions/`만 계속 막힙니다.
- **작업 폴더가 홈인 무인 실행 (#75)**
  - 셸 시작 파일(`~/.bashrc` 등), systemd user 유닛, `~/.gitconfig` 쓰기를 거절합니다.
  - 실행을 시작할 때 경고합니다.
- **신뢰 표시 없는 MCP 서버 (#74)**
  - `"trust": true`가 아닌 MCP 서버의 도구는 빠집니다.
  - 실행 결과 메모에 빠진 서버 이름이 남습니다.
- **웹훅의 PR 코멘트와 flow push (#74)**
  - 웹훅 실행에서는 쓰기 권한이 없는 사람의 코멘트 글이 빠집니다. `[bot]` 계정도 빠지고, 권한 조회에 실패하면 주인의 코멘트도 빠집니다.
  - flow는 `.github/` 아래를 바꾸는 push를 하지 않습니다. 워크플로나 `CODEOWNERS`를 고치던 flow는 실패합니다.
- **오래된 커널 (#71)**
  - Landlock ABI 1~2 커널에서는 예약 작업과 웹훅의 bash가 기본 설정(`sandbox = require`)으로는 모두 거절됩니다. Linux 6.2 전 커널로, Ubuntu 22.04와 Debian 12의 기본 커널이 여기에 해당합니다.
  - `junees doctor`의 `truncate` 줄을 확인하세요.

**모든 실행 (대화 포함)**

- **격리된 명령의 환경과 터미널 (#71)**
  - 격리된 bash에서는 `SSH_AUTH_SOCK`, `DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR`, `TMUX`, `TMUX_PANE`가 빠집니다.
  - 그래서 ssh 에이전트로 하는 `git push`, `systemctl --user`, rootless `podman`처럼 세션 버스나 런타임 폴더가 필요한 명령은 전처럼 되지 않습니다. https로 push하는 것은 그대로 됩니다.
  - 명령은 자기 세션에서 돌기 때문에 터미널(`/dev/tty`)을 열 수 없습니다.
- **별도 git 폴더 저장소의 작업 모드 (#71)**
  - `git init --separate-git-dir`로 만든 저장소에서는, 하네스 git이 `git config core.worktree <작업 폴더>`를 한 번 실행하라고 안내하며 거절합니다.
  - 서브모듈과 linked worktree는 그대로 됩니다.
- **Claude Code 파일 (#74)**
  - 파일 도구와 bash 정책이 `~/.claude/.credentials.json`, `settings.json`, `settings.local.json`, `backups/`, `~/.claude.json`을 거절합니다. 그래서 "내 Claude Code 설정을 고쳐 줘"는 이제 되지 않습니다.
  - 자동 메모리, `~/.claude/CLAUDE.md`, `rules/`는 대화에서 그대로 쓸 수 있습니다. 웹훅 실행에서는 `~/.claude` 전체를 막습니다.
- **파일 도구의 시스템 경로 (#75)**
  - `/proc`, `/sys`, `/dev`는 대화에서도 거절합니다. 예를 들어 `read_file /proc/cpuinfo`, `/dev/shm`의 파일 읽기, `/dev/null`에 쓰기가 막힙니다.
- **`write_file`과 `edit_file` (#75)**
  - `write_file`은 임시 파일에 쓴 뒤 rename으로 바꿔 넣습니다. 그래서 소유자와 xattr는 유지되지 않습니다. 그룹은 유지를 시도하고, bind mount된 파일은 제자리에 씁니다.
  - 하드 링크가 여럿인 파일은 대화에서는 제자리에 써서 링크를 유지합니다. 무인 실행에서는 링크가 갈라집니다.
  - 읽기 전용 파일은 `write_file`이 이전처럼 거절하고, 이제 `edit_file`도 거절합니다(0.1.0의 `edit_file`은 덮어썼습니다).
- **브라우저 (#70)**
  - `evaluate`는 2,000자를 넘는 식을 실행하지 않습니다.
  - PAC 스크립트나 데스크톱 설정(GNOME, KDE)으로 정한 사설 주소 프록시를 쓰면, 모든 페이지가 `not shown: a page came from a private or local address`로 막힙니다. 환경 변수나 `--proxy-server`로 준 프록시는 괜찮습니다([docs/browser.md](docs/browser.md)).

### 새 기능

- Slack 게이트웨이 `junees gateway slack`: Socket Mode로 받아서 공개 주소가 없어도 됩니다 (#66)
- 텔레그램 답장을 HTML 서식으로 보냅니다. `telegram_format = html|plain`, 기본은 html입니다 (#67)

### 보안 수정

- 브라우저 도구 (#70)
  - `evaluate` 승인 질문이 스크립트를 숨김 없이 보여 줍니다.
  - DNS rebinding으로 연 사설 주소 문서는 읽지 않습니다.
  - 닫힌 shadow root 호스트가 256개를 넘으면, 그 페이지의 모든 동작에 승인을 받습니다.
- 실행 격리 (#71)
  - 하네스가 돌리는 git도 세션과 같은 Landlock 안에서 실행합니다.
  - 격리된 자식 프로세스는 모두 자기 세션에서 돕니다.
  - D-Bus, SSH 에이전트, tmux 환경 변수를 뺍니다.
  - `junees update`는 확인한 압축 파일을 디스크에 다시 쓰지 않고 메모리에서 풉니다.
- 무인 실행 (#74)
  - Claude Code의 로그인 파일과 설정 파일을 보호합니다.
  - 웹훅 실행에서는 지난 세션 기록을 숨깁니다.
  - GitHub flow는 쓰기 권한이 없는 사람의 코멘트를 따르지 않습니다.
- 파일 도구 (#75)
  - 연 파일이 실제로 가리키는 위치로 검사합니다.
  - `/proc`, `/sys`, `/dev`를 거절합니다.
  - 무인 실행에서는 범위를 제한합니다.
- 디스코드 답장의 링크 미리보기를 끕니다 (#75)
- CI 액션을 SHA로 고정했습니다 (#75)
