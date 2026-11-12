# 점검 (junees doctor)

```sh
junees doctor             # 항목마다 ok / warn / FAIL / --(해당 없음)과 고치는 방법 한 줄
junees doctor --json      # 같은 내용을 JSON으로
junees doctor --online    # API 키 확인 요청, 최신 릴리스 확인, claude auth status까지
```

대화 안에서는 `/doctor`(네트워크 없이, `junees doctor`처럼 설정을 그때 다시 읽으므로 대화 중에 고치거나 깨뜨린 `config.toml`도 보인다). 실패(`FAIL`)가 하나라도 있으면 종료 코드 1, 경고만 있으면 0이다.

점검하는 것:

- 상태 폴더: `$JUNEES_HOME`이 내 것이고 `0700`인지, `config.toml`, `mcp.json`, `cron/jobs.json`, `webhooks/routes.json`, `secrets/` 안 파일이 내 것이고 `0600`인지(`secrets/` 아래 하위 폴더는 `0700`인지 보고 그 안을 다시 확인한다). 다른 사용자가 바꿀 수 있으면 실패, 읽기만 할 수 있으면 비밀이 들 수 있는 파일(`config.toml`, `mcp.json`, `secrets/*`)은 실패, 나머지는 경고. 고치는 줄은 `chmod 600 <경로>`처럼 그대로 실행할 수 있는 명령이다.
- 설정: `config.toml`이 읽히는지(모르는 키, 따옴표 없는 값은 줄 번호와 함께), 모든 값이 읽히는지(`Config`, 백엔드, `timezone`, cron/웹훅 한도를 실제 실행과 같은 함수로 읽는다). 파일이 깨져 있어도 점검은 멈추지 않고 파일 없이 나머지를 본다.
- 키: API 키, GitHub 토큰, 알림 웹훅이 있는지와 어디서 왔는지(환경 변수 / config.toml)만. 값은 어떤 경우에도 출력하지 않는다. 오류 문장이 값을 인용하더라도(예: 토큰을 `discord_allowed_users`에 잘못 넣은 경우) 출력 전에 보고서에서 세 가지를 지운다: 설정 오류가 `` got `...` ``로 인용한 값은 길이만 남기고, 비밀 값(API 키, 디스코드 토큰은 `Bot `을 뗀 것도, GitHub과 `gh` 토큰, 알림 웹훅, `CLAUDE_CODE_OAUTH_TOKEN`·`ANTHROPIC_AUTH_TOKEN` 같은 Claude Code 자격 증명, 점검이 읽은 웹훅 시크릿)은 통째로든 12자 이상 조각으로든 `[hidden]`으로 바꾸고, 그래도 자격 증명처럼 보이는 단어(메모리와 같은 `secrets.rs` 규칙)도 `[hidden]`으로 바꾼다. api 백엔드에 키가 없으면 실패, claude-code 백엔드면 해당 없음.
- claude-code 백엔드: `claude --version`(2.1.259 이상, 이것과 브라우저의 `--version`, 아래 샌드박스 시험의 `sh` 말고는 프로그램을 실행하지 않는다), 로그인(`claude auth status`는 `--online`일 때만), `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`이 켜져 있는지, 켜져 있으면 `bwrap`이 PATH에 있는지와 비특권 user namespace를 막는 커널 설정(`kernel.apparmor_restrict_unprivileged_userns = 1`, `kernel.unprivileged_userns_clone = 0`, `user.max_user_namespaces = 0`). 백엔드가 claude-code면 문제는 실패, api면 경고이고, api인데 `claude`도 없으면 이 부분은 건너뛴다. setuid root인 `bwrap`은 user namespace가 필요 없으므로 제한이 있어도 ok.
- 프로그램: `git`(없으면 경고: 작업 모드와 GitHub 흐름이 쓴다), `gh`(선택).
- 샌드박스([실행 격리](safety.md#실행-격리-landlock)): 커널의 Landlock ABI와 `sandbox` 설정, 그리고 지금 폴더에서 bash 명령과 같은 규칙으로 묶은 `/bin/sh -c 'exit 0'` 하나와 설정 파일을 열어 보는 `sh` 하나(열리면 실패. 둘 다 빈 환경이고 아무것도 쓰지 않는다). Landlock이 없으면 경고(`sandbox = require`면 실패, 따로 정하지 않았으면 예약 작업과 웹훅 실행이 bash 명령을 거절한다는 경고), `sandbox_network = 0`인데 ABI가 4보다 낮으면 경고, `sandbox_read`가 bash의 시작을 막으면 실패, `sandbox = off`면 해당 없음.
- 브라우저: 브라우저 도구가 켜져 있거나 `browser_path`가 있으면 프로그램을 찾고 `--version`을 실행한다(네트워크 없이). 켜져 있는데 없거나 실행이 안 되면 실패, 꺼져 있으면 경고. `browser_no_sandbox`가 켜져 있으면 그것도 적는다([브라우저 도구](browser.md)).
- 디스코드: 토큰과 허용 사용자(게이트웨이가 시작할 때와 같은 검사). 둘 다 없으면 해당 없음.
- 텔레그램: 토큰 모양과 허용 사용자(게이트웨이가 시작할 때와 같은 검사, 토큰은 `[hidden]`). 둘 다 없으면 해당 없음. 토큰이 맞는지(`getMe`)는 `--online`에서도 묻지 않는다.
- 슬랙: 봇 토큰(`xoxb-`)과 앱 토큰(`xapp-`)의 모양, 허용 사용자(게이트웨이가 시작할 때와 같은 검사, 토큰은 `[hidden]`). 셋 다 없으면 해당 없음. `--online`이면 `auth.test`로 슬랙이 봇 토큰을 받는지와 봇 이름을 묻는다(거절이면 실패). 앱 토큰은 묻지 않는다(`apps.connections.open`이 연결 주소를 새로 만든다).
- 예약 작업과 웹훅: `jobs.json`/`routes.json`이 읽히는지, 기다리는 작업이나 멈추지 않은 경로가 있는데 `junees cron daemon`/`junees webhook serve`가 돌지 않으면 경고(잠금 파일이 잡혀 있는지로 판단한다. 비어 있으면 잠깐 잡았다 바로 놓는다). 경로마다 시크릿 파일이 있고 내 것이고 `0600`인지(값은 읽기만 하고 출력하지 않음), 환경 변수 시크릿은 지금 환경에 없으면 경고(서버의 환경은 다를 수 있다).
- 세션: `$JUNEES_HOME/sessions`의 파일 수와 크기, 1 GiB를 넘으면 경고.
- MCP: `mcp.json`이 읽히는지(`url`과 `headers` 형식 포함), 켜진 서버의 `command`가 PATH(서버 `env`에 PATH가 있으면 그것)나 그 경로에 실행 파일로 있는지, `cwd`가 폴더인지, `url` 서버의 헤더가 참조하는 환경 변수가 있는지(값은 출력하지 않는다), 저장된 OAuth 로그인(`login <서버>`: 있는지, 이 `url`의 것인지, 0600인지, 발급자와 만료, 갱신할 수 있는지. 만료됐는데 리프레시 토큰이 없으면 경고, 쓸 수 없으면 실패. 토큰 값은 `[hidden]`). 서버를 실행하거나 HTTP 서버에 연결하지는 않는다.
- 플러그인: 켜진 플러그인의 폴더와 `plugin.json`이 실제 실행 때와 같은 검사(소유자, 권한, manifest 형식, 프롬프트 파일)를 통과하는지.
- `--online`: 키가 있으면 `GET <base_url>/v1/models?limit=1`을 한 번 보낸다(토큰을 쓰지 않는다). 2xx면 ok, 401/403이면 실패, 그 밖이나 연결 실패는 경고(URL은 출력하지 않고 호스트만). GitHub에 최신 릴리스를 묻는다(`junees update --check`와 같은 요청, [업데이트](install.md#업데이트)): 새 버전이 있으면 경고(`junees update` 안내), 최신이면 ok, 릴리스가 없으면 해당 없음, 묻지 못하면 경고. 켜진 `url` MCP 서버마다 로그인 때와 같은 인가 서버 찾기만 한다(`login <서버> (online)`: 토큰 없는 `server/discover` 하나와 메타데이터 GET. 등록, 로그인, 토큰 요청은 하지 않는다). 로그인을 요구하지 않는 서버는 건너뛰고, PKCE가 없거나 클라이언트를 얻을 길이 없으면 경고.

점검은 읽기만 한다: 파일을 만들거나 쓰지 않고, 잠금을 붙잡고 있지 않으며, MCP 서버와 플러그인 명령을 실행하지 않으며(실행하는 프로그램은 `claude --version`, 브라우저의 `--version`, 샌드박스 시험용 `sh`뿐), 기본으로 네트워크를 쓰지 않는다.

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
