# 변경 기록

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
