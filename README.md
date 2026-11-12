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

설정 파일과 환경 변수: [docs/configuration.md](docs/configuration.md)

고급 사용법(세션 기록, 세션 검색, 사용량과 비용, 장기 작업 모드, 스트리밍, 확장 사고, 웹 검색, 이미지 입력): [docs/usage.md](docs/usage.md)

메모리, 프로젝트 지침 파일, 스킬, 서브에이전트: [docs/memory-and-skills.md](docs/memory-and-skills.md)

구독 모드(claude-code 백엔드): [docs/subscription.md](docs/subscription.md)

디스코드, 텔레그램 게이트웨이: [docs/gateways.md](docs/gateways.md)

MCP 서버: [docs/mcp.md](docs/mcp.md)

예약 작업, 웹훅, GitHub: [docs/automation.md](docs/automation.md)

플러그인: [docs/plugins.md](docs/plugins.md)

컨텍스트 관리: [docs/context.md](docs/context.md)

기본 도구: [docs/tools.md](docs/tools.md)

명령 정책과 실행 전 승인: [docs/safety.md](docs/safety.md)

점검(junees doctor): [docs/doctor.md](docs/doctor.md)

구조(모듈과 테스트 목록): [docs/architecture.md](docs/architecture.md)

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
