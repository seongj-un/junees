# Junees

Rust로 만드는 AI 에이전트 하네스. LLM이 도구를 호출하며 일을 처리하는 루프를 바닥부터 직접 만든다.
단일 바이너리와 적은 메모리 사용을 목표로 한다.

- 단일 바이너리: `junees` 하나로 대화, 한 번 실행, 장기 작업, 채팅 봇, 예약 작업, 웹훅 서버를 모두 돌린다.
- 두 가지 백엔드: Anthropic API 키로 Junees 자신의 에이전트를 돌리거나(`api`), Claude Pro/Max 구독으로 로그인한 공식 Claude Code CLI에 맡긴다(`claude-code`).
- OpenAI 호환 API: `api` 백엔드는 Anthropic 대신 OpenAI Chat Completions 형식의 서버(OpenAI, OpenRouter, Ollama, vLLM, LM Studio)도 부른다(`provider = "openai"`, [OpenAI 호환 API](docs/configuration.md#openai-호환-api)).
- 장기 작업: 큰 목표를 검증 가능한 기능 목록으로 쪼개고, 매번 새 컨텍스트로 시작하는 세션들이 기능 하나씩 구현, 검증, 커밋한다. 완료 판정은 모델이 아니라 하네스가 검증 명령의 종료 코드로 한다.
- 어디서나: 터미널, 디스코드·텔레그램 봇과 슬랙 앱, 일정에 맞춘 예약 작업, GitHub 웹훅으로 시작하는 PR 리뷰와 merge 흐름.
- 기억과 확장: 세션 기록과 검색, 영구 메모리, 스킬, 프로젝트 지침 파일, 서브에이전트, MCP 서버, 플러그인.
- 안전장치: 명령 정책, Linux Landlock 실행 격리(작업 폴더 밖 쓰기와 설정, 비밀 파일 읽기를 커널이 막는다), 대화 모드의 실행 전 승인, 도구 환경에서 비밀 값 숨기기([명령 정책, 실행 격리, 실행 전 승인](docs/safety.md)).

## 설치

Linux(x86_64, aarch64)와 macOS(x86_64, Apple silicon)용 릴리스 빌드를 받아 `~/.local/bin`에 둔다(sudo 없이, SHA-256 확인):

```sh
curl -fsSL https://raw.githubusercontent.com/seongj-un/junees/main/install.sh | sh
```

폴더와 버전은 `JUNEES_INSTALL_DIR`, `JUNEES_VERSION`(예 `v0.1.0`)으로 정한다. 새 릴리스로 바꾸기는 `junees update`(확인만은 `--check`, 대화에서 `/update`). 직접 받기, 출처 증명 확인, 릴리스 만드는 법은 [설치, 업데이트, 릴리스](docs/install.md)에 있다.

소스에서 빌드하려면 Rust 1.88 이상(edition 2024)이 필요하다. Linux에서 개발하고 CI(ubuntu, macOS는 컴파일 확인)로 확인한다.

```sh
git clone https://github.com/seongj-un/junees
cd junees
cargo install --path .
```

`cargo install` 없이 `cargo build --release` 후 `./target/release/junees`로 써도 된다. 구독으로 쓰려면 Claude Code CLI(`claude`, 2.1.259 이상)를 따로 설치하고 직접 로그인해 둔다([구독 모드](docs/subscription.md)). 설치와 설정은 `junees doctor`로 점검한다.

## 빠른 시작

```sh
cargo install --path .              # 한 번 설치하면 어디서나 junees (~/.cargo/bin)
junees                              # 대화 (처음이면 설정 마법사가 먼저 뜸)
junees "이 폴더에 뭐가 있어?"        # 한 번만 묻기
junees -c                           # 이 폴더의 마지막 대화 이어서
junees task "할 일 관리 웹 앱을 만들어"   # 장기 작업: 계획하고 바로 실행 (git 저장소 안에서)
junees setup                        # 설정 다시
junees gateway                      # 디스코드 봇으로 띄우기
junees gateway telegram             # 텔레그램 봇으로 띄우기
junees gateway slack                # 슬랙 앱으로 띄우기 (Socket Mode)
junees cron add "daily 09:00" "오늘 할 일을 정리해 줘"   # 예약 작업
junees doctor                       # 설치와 설정 점검
```

봇은 [게이트웨이](docs/gateways.md), 예약 작업은 [예약 작업](docs/automation.md#예약-작업), 점검은 [점검](docs/doctor.md)에 자세히 있다.

### 설정 파일과 처음 설정

설정은 `$JUNEES_HOME/config.toml`(기본 `~/.junees/config.toml`)에 있다. 처음 실행할 때 설정 파일도 `ANTHROPIC_API_KEY`도 없고 터미널이면 설정 마법사가 먼저 뜬다(`junees setup`으로 언제든 다시). 마법사는 Claude에 닿는 방법(`1) Claude subscription` 또는 `2) Anthropic API key`)을 묻고, API 키면 대화 모델과 작업 기본 예산을, 마지막으로 디스코드 봇과 슬랙 앱 설정(선택)을 묻는다. 터미널이 아니면(cron, 파이프) 마법사를 띄우지 않는다.

파일은 TOML의 단순한 부분(한 줄에 `키 = 값`, 표와 배열은 받지 않음)만 쓴다. 키마다 환경 변수 하나에 대응하고 환경 변수가 있으면 파일보다 우선한다. 파일은 `0600`으로 만들고, 모델의 도구가 이 파일을 읽고 쓰는 것을 막는다(차단 목록이라 한계가 있다).

```toml
backend = "claude-code"          # JUNEES_BACKEND
api_key = "sk-ant-..."           # ANTHROPIC_API_KEY
model = "claude-sonnet-4-6"      # JUNEES_MODEL
task_max_time = "2h"             # JUNEES_TASK_MAX_TIME
task_max_tokens = 2000000        # JUNEES_TASK_MAX_TOKENS
```

마법사의 단계, 모든 키와 환경 변수 표는 [설정](docs/configuration.md)에 있다.

## 대화 안 명령

각 명령의 자세한 설명은 [대화 안 명령](docs/usage.md#대화-안-명령)에 있다.

- `/task <목표>`, `/task`: 장기 작업을 계획하고 실행, 멈춘 작업 이어서 하기
- `/status`: 작업 상태
- `/sessions`: 최근 세션을 골라 이어 가기
- `/search <낱말>`: 저장된 세션 검색
- `/new`: 새 대화(`/reset`도 됨)
- `/image <경로> [질문]`: 이미지와 함께 묻기(api 백엔드)
- `/model [이름]`: 모델 보기, 이 실행 동안 바꾸기
- `/backend [api|claude-code]`: 백엔드 보기, 이 실행 동안 바꾸기
- `/memory`, `/memory clear user|notes|project`: 영구 메모리 보기, 한 저장소 비우기
- `/skills`, `/skill <이름>`: 스킬 목록과 본문
- `/mcp`, `/mcp login <서버>`, `/mcp logout <서버>`: MCP 서버 상태와 OAuth 로그인
- `/plugins`: 플러그인
- `/cron`: 예약 작업 목록
- `/github`: GitHub 토큰, 저장소, 요청 한도
- `/webhooks`: 웹훅 경로 목록
- `/context`: 컨텍스트 크기와 이 세션이 읽은 지침 파일
- `/doctor`: 설치와 설정 점검
- `/update`: 새 릴리스가 있는지(설치는 `junees update`)
- `/compact`: 앞 대화를 요약으로 바꾸기(api 백엔드)
- `/usage`: 이 세션의 토큰과 추정 비용
- `/help`, `/exit`(`/quit`, Ctrl-D)

턴 중 Ctrl-C는 그 턴만 취소한다([중단](docs/usage.md#중단-ctrl-c-sigterm)).

## 명령줄

하위 명령마다 `--help`가 있다(`junees cron --help` 등).

- `junees [-p] "<질문>"`, `-c`, `--resume <id>`, `--no-session`, `--image <경로>`: 한 번 실행과 세션([고급 사용법](docs/usage.md))
- `junees task ["<목표>"]`, `junees task init|run|status`, `junees status`: 장기 작업([장기 작업 모드](docs/usage.md#장기-작업-모드))
- `junees setup`: 설정 마법사
- `junees gateway`(`junees discord`), `junees gateway telegram`(`junees telegram`), `junees gateway slack`(`junees slack`): 채팅 봇([게이트웨이](docs/gateways.md))
- `junees cron add|list|remove|pause|resume|run|daemon`: 예약 작업([예약 작업](docs/automation.md#예약-작업))
- `junees webhook serve|add|add-github|list|remove|pause|resume|test`: 웹훅과 GitHub PR 흐름([웹훅](docs/automation.md#웹훅))
- `junees sessions search <낱말>`: 세션 검색([세션 검색](docs/usage.md#세션-검색))
- `junees usage`: 지난 며칠의 토큰과 비용([사용량과 비용](docs/usage.md#사용량과-비용))
- `junees doctor`: 설치와 설정 점검([점검](docs/doctor.md))
- `junees dashboard [--listen 127.0.0.1:7878] [--no-open] [--chat [--dir <폴더>]]`: 브라우저로 보는 대시보드. `--chat`이면 페이지에서 대화하고 승인 질문에 답한다([대시보드](docs/dashboard.md))
- `junees plugin list|install|enable|disable|remove`: 플러그인([플러그인](docs/plugins.md))
- `junees mcp login|logout <서버>`: MCP 서버 OAuth 로그인([OAuth 로그인](docs/mcp.md#oauth-로그인))
- `junees mcp serve [--dir <폴더>]`: Junees를 stdio MCP 서버로 띄워 다른 에이전트가 일을 맡기게 한다([Junees를 MCP 서버로](docs/mcp.md#junees를-mcp-서버로))
- `junees update [--check] [--version vX.Y.Z]`: 최신 릴리스로 바꾸기([업데이트](docs/install.md#업데이트))

명령줄에서도 `junees task "목표"`(계획 + 실행), `junees task`(이어서 실행), `junees status`(상태). 하위 명령 이름과 같은 한 단어 질문(`status`, `task`, `setup`, `gateway`, `discord`, `telegram`, `cron`, `webhook`, `sessions`, `doctor`, `dashboard`, `usage`, `plugin`, `plugins`, `mcp`, `update`)은 `junees -p status`처럼 `-p`로 묻는다. `-`로 시작하는 질문은 `junees -- "-5도면 추워?"`.

## 기능

- [세션 기록](docs/usage.md#세션-기록): 대화가 바뀔 때마다 JSONL로 남아, 언제 죽어도 `-c`나 `--resume`으로 이어 간다.
- [중단](docs/usage.md#중단-ctrl-c-sigterm): Ctrl-C와 SIGTERM은 턴을 취소하고 실행 중인 도구의 프로세스 그룹을 끝낸다.
- [세션 검색](docs/usage.md#세션-검색): 모델 도구 `session_search`, `/search`, `junees sessions search`로 지난 세션을 찾는다.
- [사용량과 비용](docs/usage.md#사용량과-비용): 세션 기록에서 토큰과 추정 비용을 센다(`/usage`, `junees usage`).
- [장기 작업 모드](docs/usage.md#장기-작업-모드): 기능 목록, 세션마다 새 컨텍스트, 하네스의 검증과 체크포인트 커밋, 예산, 크래시 복구, 알림.
- [스트리밍 출력](docs/usage.md#스트리밍-출력): 응답을 SSE로 받아 도착하는 대로 찍는다.
- [확장 사고](docs/usage.md#확장-사고): thinking 예산을 켜고, thinking 블록을 고치지 않고 돌려보낸다.
- [웹 검색](docs/usage.md#웹-검색): Anthropic의 서버 도구 `web_search`(기본 꺼짐, 별도 과금).
- [이미지 입력](docs/usage.md#이미지-입력): `read_file`, `/image`, `--image`, 채팅 첨부로 그림을 보여 준다(api 백엔드).
- [메모리](docs/memory-and-skills.md#메모리): 세션을 넘어 남는 짧은 메모(`user`, `notes`, `project`).
- [프로젝트 지침 파일](docs/memory-and-skills.md#프로젝트-지침-파일): 저장소의 `AGENTS.md`, `JUNEES.md`, `CLAUDE.md`를 시스템 프롬프트에 넣는다.
- [스킬](docs/memory-and-skills.md#스킬): 반복 작업의 절차를 적은 문서를 모델이 읽고 고친다.
- [서브에이전트](docs/memory-and-skills.md#서브에이전트-delegate): `delegate`로 독립적인 하위 작업을 동시에 맡긴다.
- [구독 모드](docs/subscription.md): 대화와 장기 작업을 Claude 구독으로 로그인한 Claude Code가 실행한다.
- [디스코드 게이트웨이](docs/gateways.md#디스코드-게이트웨이): 채널(스레드, DM)마다 대화 하나를 두는 디스코드 봇. 승인 질문은 버튼이나 글 답장으로 답한다.
- [텔레그램 게이트웨이](docs/gateways.md#텔레그램-게이트웨이): 같은 엔진으로 도는 텔레그램 봇(승인 버튼은 인라인 키보드).
- [슬랙 게이트웨이](docs/gateways.md#슬랙-게이트웨이): Socket Mode로 받는 슬랙 앱(DM과 채널 스레드마다 대화 하나, mrkdwn 서식).
- [음성 메시지 받아쓰기](docs/gateways.md#음성-메시지-받아쓰기): 디스코드, 텔레그램, 슬랙의 음성 메시지와 오디오 파일을 OpenAI 호환 받아쓰기 API(OpenAI, Groq, 로컬 whisper 서버)로 글로 바꿔 처리한다.
- [MCP 서버](docs/mcp.md): stdio와 Streamable HTTP MCP 서버의 도구, OAuth 로그인, Junees 자신을 MCP 서버로 쓰기(`junees mcp serve`).
- [예약 작업](docs/automation.md#예약-작업): 프롬프트를 일정에 맞춰 무인 실행하고 결과를 파일, 디스코드, 웹훅으로 보낸다.
- [웹훅](docs/automation.md#웹훅): HTTP 요청(GitHub 웹훅 등)이 무인 실행을 시작하고, `add-github`는 PR 리뷰, 리뷰 반영, merge를 맡는다.
- [GitHub](docs/automation.md#github): 모델의 `github` 도구로 PR 읽기, 코멘트, 리뷰, PR 열기, merge.
- [브라우저](docs/browser.md): 모델의 `browser` 도구로 헤드리스 Chromium을 열고 읽고 클릭하고 입력한다(기본 꺼짐, http/https만, 사설 주소와 위험한 입력은 설정과 승인으로만).
- [플러그인](docs/plugins.md): 명령 도구, 훅, 프롬프트 조각을 폴더 하나로 더한다.
- [컨텍스트 관리](docs/context.md): 프롬프트 캐싱, 오래된 도구 결과 비우기, 압축.
- [기본 도구](docs/tools.md): `bash`, `read_file`, `write_file`, `edit_file`.
- [명령 정책](docs/safety.md#명령-정책): 치명적인 명령과 비밀 파일을 다루는 명령을 실행 전에 막는다.
- [실행 격리](docs/safety.md#실행-격리-landlock): `bash` 명령을 Linux Landlock으로 묶어 작업 폴더 밖 쓰기, 설정과 비밀 파일 읽기, (설정하면) TCP를 커널이 막는다.
- [실행 전 승인](docs/safety.md#실행-전-승인): 대화 모드에서 위험할 수 있는 도구 호출을 먼저 묻는다.
- [점검](docs/doctor.md): `junees doctor`로 설치와 설정을 점검한다(기본은 네트워크 없이).
- [대시보드](docs/dashboard.md): `junees dashboard`가 이 컴퓨터의 브라우저에 상태, 세션, 사용량, 예약 작업, 웹훅, 점검 결과를 보여 준다(루프백 주소와 실행마다 새 토큰만). `--chat`이면 페이지에서 Junees와 대화한다.
- [설치와 업데이트](docs/install.md): `install.sh`, `junees update`(체크섬 확인, 한 번의 rename으로 교체), 태그로 만드는 릴리스.

## 문서

- [docs/install.md](docs/install.md): 설치 스크립트, 업데이트(`junees update`), macOS에서 꺼지는 기능, 릴리스 만들기
- [docs/configuration.md](docs/configuration.md): 설정 파일, 처음 설정 마법사, 환경 변수 표
- [docs/usage.md](docs/usage.md): 대화 안 명령, 고급 사용법, 세션 기록과 검색, 사용량과 비용, 장기 작업 모드, 스트리밍, 확장 사고, 웹 검색, 이미지 입력
- [docs/memory-and-skills.md](docs/memory-and-skills.md): 메모리, 프로젝트 지침 파일, 스킬, 서브에이전트
- [docs/subscription.md](docs/subscription.md): 구독 모드(claude-code 백엔드)
- [docs/gateways.md](docs/gateways.md): 디스코드, 텔레그램, 슬랙 게이트웨이
- [docs/mcp.md](docs/mcp.md): MCP 서버(stdio, Streamable HTTP, OAuth)
- [docs/automation.md](docs/automation.md): 예약 작업, 웹훅(GitHub PR 흐름 포함), GitHub 도구
- [docs/plugins.md](docs/plugins.md): 플러그인
- [docs/context.md](docs/context.md): 컨텍스트 관리
- [docs/tools.md](docs/tools.md): 기본 도구와 `edit_file`
- [docs/browser.md](docs/browser.md): 브라우저 도구
- [docs/safety.md](docs/safety.md): 명령 정책, 실행 격리(Landlock), 실행 전 승인
- [docs/doctor.md](docs/doctor.md): 점검(`junees doctor`)
- [docs/dashboard.md](docs/dashboard.md): 웹 대시보드(`junees dashboard`)와 그 API 계약
- [docs/architecture.md](docs/architecture.md): 구조(모듈 설명과 테스트 목록)
- [docs/long-running.md](docs/long-running.md): 단계별 계획과 진행 장부(각 단계에서 무엇을 왜 그렇게 만들었는지, 리뷰 반영 기록)

## 로드맵

1. 에이전트 루프: Anthropic Messages API, 도구 호출 반복, 기본 도구(bash, 파일 읽기/쓰기), CLI (완료)
2. 장기 실행 하네스: 세션 기록과 재개, 컨텍스트 압축, 여러 세션에 걸친 장기 작업 모드, 무인 실행 안정성. 자세한 계획과 진행 상황은 [docs/long-running.md](docs/long-running.md) (완료)
3. 도구 체계: 스트리밍 출력, 위험 명령 승인, 파일 패치(edit_file), 웹 검색, 확장 사고 블록 보존 (완료)
4. 메모리와 스킬: 세션을 넘어 남는 기억, 절차 문서화와 재사용 (완료)
5. 게이트웨이: 디스코드 봇 (`junees gateway`, [디스코드 게이트웨이](docs/gateways.md#디스코드-게이트웨이)) (완료)
6. 확장: 서브에이전트(`delegate`), 예약 작업(`junees cron`), 플러그인(`junees plugin`), MCP 클라이언트(stdio) (완료)
7. 웹훅과 GitHub: 웹훅 수신 서버(`junees webhook`), GitHub 도구(PR, 리뷰, merge), 웹훅으로 PR 리뷰·리뷰 반영·merge를 맡기는 흐름(`junees webhook add-github`) (완료)
8. 일상 사용 다듬기: 프로젝트 지침 파일(AGENTS.md, JUNEES.md, CLAUDE.md), 세션 검색(`session_search`, `/search`), 사용량과 비용(`/usage`, `junees usage`), 점검(`junees doctor`) (완료)
9. 연결과 입력 넓히기: 원격 MCP 서버(Streamable HTTP, OAuth 로그인 `junees mcp login`), 이미지 입력(`read_file`, `/image`, `--image`, 디스코드·텔레그램 첨부), 텔레그램 게이트웨이(`junees gateway telegram`) (완료)
10. 도구와 안전: 브라우저 도구(헤드리스 Chromium, [docs/browser.md](docs/browser.md)), bash 실행 격리(Landlock, [docs/safety.md](docs/safety.md)) (완료)
11. 릴리스와 설치: `v*` 태그의 릴리스 워크플로(Linux musl, macOS, SHA256SUMS, 출처 증명), `install.sh`, `junees update` ([docs/install.md](docs/install.md)), 첫 릴리스 v0.1.0 (완료)
12. 게이트웨이 확장: Slack(Socket Mode), 텔레그램 HTML 서식 ([docs/gateways.md](docs/gateways.md)) (완료)
13. 승인 버튼: 디스코드, 텔레그램, Slack 게이트웨이의 승인 질문을 버튼으로 답하기, `junees setup`의 Slack 단계 ([docs/gateways.md](docs/gateways.md)) (완료)
14. 모델과 무인 실행 보강: OpenAI 호환 Chat Completions provider(OpenAI, OpenRouter, Ollama, vLLM, [docs/configuration.md](docs/configuration.md)), 무인 실행의 홈 폴더 지속 실행 경로 보호 ([docs/safety.md](docs/safety.md)) (완료)
15. 로컬 웹 대시보드: `junees dashboard`(루프백, 토큰, 읽기 전용 상태·세션·사용량·예약 작업·웹훅·점검 화면, [docs/dashboard.md](docs/dashboard.md)) (완료)
16. 대시보드 2차: 세션 대화 보기, 예약 작업과 웹훅 멈춤·재개(같은 출처 확인, 쓰기 로그), 브라우저에는 일회용 코드만 넘기기 ([docs/dashboard.md](docs/dashboard.md)) (완료)
17. 음성 메시지: 디스코드, 텔레그램, Slack 게이트웨이의 음성 메시지와 오디오 첨부를 OpenAI 호환 받아쓰기 API(OpenAI, Groq, 로컬 whisper 서버)로 글로 바꿔 처리 ([docs/gateways.md](docs/gateways.md)) (완료)
18. MCP 서버: `junees mcp serve`(stdio)로 다른 에이전트(Claude Desktop, Claude Code, Hermes, Cursor)가 Junees에 일을 맡기기, 웹훅과 같은 무인 실행 규칙 ([docs/mcp.md](docs/mcp.md#junees를-mcp-서버로))
19. 대시보드 대화: `junees dashboard --chat`으로 페이지에서 Junees와 대화하고, 승인 질문을 버튼으로 답하기(이벤트 스트림, 터미널과 같은 신뢰 수준, [docs/dashboard.md](docs/dashboard.md#대화---chat))

## 개발

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 라이선스

MIT
