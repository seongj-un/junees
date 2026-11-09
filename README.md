# Junees

Rust로 만드는 AI 에이전트 하네스. LLM이 도구를 호출하며 일을 처리하는 루프를 바닥부터 직접 만든다.
[Hermes Agent](https://github.com/NousResearch/hermes-agent)에서 영감을 받았고, 단일 바이너리와 적은 메모리 사용을 목표로 한다.

## 사용법

```sh
cargo build --release
export ANTHROPIC_API_KEY=sk-ant-...
./target/release/junees              # 대화 모드
./target/release/junees -p "이 폴더에 뭐가 있어?"   # 한 번만 실행
./target/release/junees -c            # 이 폴더의 마지막 세션 이어서
./target/release/junees -c -p "계속"   # 이어서 한 번만 실행
./target/release/junees --resume 20261109-0617   # 지정한 세션 (id 앞부분만 써도 유일하면 됨)
./target/release/junees --no-session  # 기록하지 않음
```

대화 모드 명령: `/sessions` 최근 세션 10개, `/reset` 새 세션 시작, `/usage` 현재 세션의 토큰 사용량, `/exit` 종료.

### 세션 기록

대화는 바뀔 때마다 `$JUNEES_HOME/sessions/<id>.jsonl`에 바로 한 줄씩 추가된다(기본 `~/.junees`). 프로세스가 언제 죽어도 마지막으로 기록된 지점에서 `-c`나 `--resume`으로 이어 갈 수 있다.

- id는 `20261109-061704-3fa2`처럼 UTC 시각 + 랜덤 hex라서 이름순이 생성순이다.
- 파일은 첫 입력이 기록될 때 만들어진다. 시작하고 바로 나가면 빈 세션이 남지 않는다.
- `-c`는 시작한 폴더(기록의 `cwd`)가 지금 폴더와 같은 세션 중 마지막으로 기록된(파일 수정 시각) 것을 고른다. 없으면 새 세션을 시작한다.
- 이어 갈 때 마지막 줄이 잘려 있으면 그 줄만 버린다. 도구 실행 중에 끊겨서 결과가 없는 도구 호출은 `interrupted` 오류 결과로 채우고 그것도 기록한다.
- 토큰 사용량도 기록되고, 이어 가면 그 세션의 누적값부터 다시 센다.
- 기록 쓰기가 실패하면 경고를 한 번 띄우고 그 세션의 나머지는 기록하지 않는다(파일이 대화의 빈틈 없는 앞부분으로 남도록).

| 환경 변수 | 기본값 | 설명 |
|---|---|---|
| `ANTHROPIC_API_KEY` | (필수) | Anthropic API 키 |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | API 주소 |
| `JUNEES_MODEL` | `claude-sonnet-4-6` | 모델 |
| `JUNEES_MAX_TOKENS` | `8192` | 모델 호출 1회의 최대 출력 토큰 |
| `JUNEES_MAX_TURNS` | `50` | 입력 하나당 최대 모델 호출 횟수 |
| `JUNEES_HOME` | `~/.junees` | 세션 기록 등 상태를 두는 폴더 |

기본 도구는 `bash`, `read_file`, `write_file` 세 가지다. `bash`는 아직 승인 절차 없이 바로 실행되니(2단계에서 추가 예정) 믿을 수 있는 폴더에서만 쓴다.

## 구조

```
src/
  main.rs            CLI 진입점 (대화 모드, -p 실행, 세션 열기/이어 가기)
  agent.rs           에이전트 루프 (기록 변경을 세션 기록에 남김)
  session.rs         세션 기록(JSONL) 쓰기, 재생, 이어 가기, 세션 찾기/목록
  message.rs         대화 메시지 타입 (Messages API 형식)
  provider/          LLM 제공자 트레이트와 Anthropic 클라이언트
  tools/             도구 트레이트, 레지스트리, bash/read_file/write_file
  config.rs          환경 변수 설정
  prompt.rs          시스템 프롬프트
  cli.rs             인자 파싱, 출력 보조 함수
tests/
  common/              테스트용 가짜 Anthropic API 서버
  anthropic_client.rs  가짜 API 서버로 실제 클라이언트와 루프를 끝까지 검증
  session_resume.rs    실제 바이너리를 두 번 실행해 -c 이어 가기와 --no-session 검증
```

## 로드맵

1. 에이전트 루프: Anthropic Messages API, 도구 호출 반복, 기본 도구(bash, 파일 읽기/쓰기), CLI (완료)
2. 장기 실행 하네스: 세션 기록과 재개, 컨텍스트 압축, 여러 세션에 걸친 장기 작업 모드, 무인 실행 안정성. 자세한 계획과 진행 상황은 [docs/long-running.md](docs/long-running.md)
3. 도구 체계: 스트리밍 출력, 위험 명령 승인, 파일 패치, 웹 검색
4. 메모리와 스킬: 세션을 넘어 남는 기억, 절차 문서화와 재사용
5. 게이트웨이: 디스코드 봇
6. 확장: 서브에이전트, 예약 작업, 플러그인, MCP 클라이언트

## 개발

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 라이선스

MIT
