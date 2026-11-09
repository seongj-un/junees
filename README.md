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

대화 모드 명령: `/sessions` 최근 세션 10개(id, 마지막 기록 시각, 첫 입력), `/reset` 새 세션 시작, `/usage` 현재 세션의 토큰 사용량(캐시 쓰기/읽기 포함), `/context` 현재 컨텍스트 크기와 한도, `/compact` 앞부분을 요약으로 압축, `/exit` 종료.

### 세션 기록

대화는 바뀔 때마다 `$JUNEES_HOME/sessions/<id>.jsonl`에 바로 한 줄씩 추가된다(기본 `~/.junees`). 프로세스가 언제 죽어도 마지막으로 기록된 지점에서 `-c`나 `--resume`으로 이어 갈 수 있다.

- id는 `20261109-061704-3fa2`처럼 UTC 시각 + 랜덤 hex라서 이름순이 생성순이다.
- 파일은 첫 입력이 기록될 때 만들어진다. 시작하고 바로 나가면 빈 세션이 남지 않는다.
- `-c`는 시작한 폴더(기록의 `cwd`)가 지금 폴더와 같은 세션 중 마지막으로 기록된(파일 수정 시각) 것을 고른다. 없으면 새 세션을 시작한다.
- `--resume`으로 다른 폴더에서 시작한 세션을 열면 `resumed <id> (N messages, started in <폴더>)`로 알려 준다. 기록 속 상대 경로는 그 폴더 기준이다.
- 세션 파일은 그 세션을 쓰는 Junees가 `flock`으로 잠근다. 아직 실행 중인 세션을 다른 터미널에서 `-c`나 `--resume`으로 열면 "session <id> is in use by another Junees" 오류로 끝나고 파일은 건드리지 않는다. 프로세스가 죽으면 잠금은 커널이 풀어 준다.
- 이어 갈 때 마지막 줄이 줄바꿈 없이 끝나고 읽히지 않으면(쓰는 도중 죽은 흔적) 그 줄만 버린다. 줄바꿈까지 있는 줄이 읽히지 않으면 위치와 상관없이 오류로 멈추고 파일을 고치지 않는다. 도구 실행 중에 끊겨서 결과가 없는 도구 호출은 `interrupted` 오류 결과로 채우고 그것도 기록한다.
- 첫 줄(`meta`)에 기록 형식 버전 `format`(현재 2)이 있고, 각 줄에 기록 시각 `ts`(unix 초)가 붙는다. 이 버전보다 새 형식의 세션은 "written by a newer Junees" 오류로 열지 않는다. 형식 1(L1) 세션도 그대로 읽고 이어 쓴다.
- 항목 종류: `meta`, `message`, `append_text`, `usage`, 그리고 형식 2에서 추가된 `clear_tool_results`, `compact`, `summary_usage`(아래 컨텍스트 관리). 기록을 바꾸는 일도 항목으로 덧붙이기만 하고 파일을 새로 쓰지 않으므로, 재생 결과가 실행 중 기록과 같다.
- 세션 파일에는 도구 출력이 그대로 들어가므로 파일은 `0600`, 새로 만드는 `$JUNEES_HOME`과 `sessions` 폴더는 `0700`으로 만든다.
- `JUNEES_HOME`과 `HOME`이 둘 다 없으면(cron, 일부 서비스와 컨테이너) 기록 없이 실행하고 "session not saved" 한 줄로 알린다. 이때 `-c`와 `--resume`만 오류다.
- 토큰 사용량도 기록되고, 이어 가면 그 세션의 누적값부터 다시 센다.
- 기록 쓰기가 실패하면 경고를 한 번 띄우고 그 세션의 나머지는 기록하지 않는다(파일이 대화의 빈틈 없는 앞부분으로 남도록).

### 컨텍스트 관리

긴 작업에서도 대화가 모델의 컨텍스트 창을 넘지 않게 하고, 반복되는 앞부분은 캐시한다.

- 프롬프트 캐싱: 매 요청에서 도구 목록의 마지막 도구, 시스템 프롬프트, 마지막 메시지의 마지막 블록에 `cache_control: ephemeral`을 붙인다. 응답의 캐시 쓰기/읽기 토큰은 `/usage`에 따로 나온다.
- 컨텍스트 크기: 마지막 응답의 `input_tokens + cache_creation_input_tokens + cache_read_input_tokens + output_tokens`. 세션 기록의 `usage` 항목에서 복원되므로 이어 가도 유지된다. 한도는 `JUNEES_CONTEXT_WINDOW` x `JUNEES_COMPACT_THRESHOLD`(기본 200000 x 0.8 = 160000).
- 1차 대응(오래된 도구 결과 비우기): 모델 호출 직전에 크기가 한도 이상이면, 도구 결과가 든 메시지 중 최근 3개를 뺀 이전 것들의 출력을 `[tool output cleared to save context]`로 바꾼다. `tool_use_id`와 `is_error`는 그대로라 호출/결과 짝이 유지된다. 기록 항목은 `clear_tool_results { upto }`.
- 2차 대응(압축): 비울 것이 없거나, 비운 뒤 다음 응답에서도 한도 이상이면 앞부분을 같은 모델로 요약한다. 꼬리는 최근 두 사용자 입력(안 되면 마지막 입력)부터 끝까지이고, 도구 결과가 든 메시지나 도구 호출 바로 뒤에서는 자르지 않는다. 요약 호출은 도구 없이, 요약 전용 시스템 프롬프트로, 앞부분을 텍스트로 옮긴 user 메시지 하나를 보낸다(`max_tokens` 4096). 요약은 꼬리 첫 user 메시지 맨 앞에 `[Summary of the earlier conversation]` 블록으로 들어가 역할 교대가 유지된다. 기록 항목은 `compact { summary, keep_from }`이고, 요약 호출의 사용량은 `summary_usage`로 남아 `/usage`에는 더해지지만 컨텍스트 크기에는 쓰이지 않는다. 압축하면 `(compacted the conversation: N messages -> M)` 한 줄이 나온다.
- 입력이 하나뿐인 대화(한 턴)는 압축할 지점이 없어서 1차 대응만 한다. 요약 호출이 실패하거나 빈 요약이 오면 경고만 띄우고 기록은 그대로 둔다.
- `/compact`는 한도와 상관없이 지금 압축한다.
- API가 컨텍스트 초과(400 "prompt is too long")로 거절하면, 크기 추적과 상관없이 비우기와 압축을 해 보고 줄어든 게 있으면 한 번만 다시 보낸다. 그래도 실패하면 오류로 끝난다.

| 환경 변수 | 기본값 | 설명 |
|---|---|---|
| `ANTHROPIC_API_KEY` | (필수) | Anthropic API 키 |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | API 주소 |
| `JUNEES_MODEL` | `claude-sonnet-4-6` | 모델 |
| `JUNEES_MAX_TOKENS` | `8192` | 모델 호출 1회의 최대 출력 토큰 |
| `JUNEES_MAX_TURNS` | `50` | 입력 하나당 최대 모델 호출 횟수 |
| `JUNEES_CONTEXT_WINDOW` | `200000` | 모델의 컨텍스트 창 크기(토큰) |
| `JUNEES_COMPACT_THRESHOLD` | `0.8` | 컨텍스트 창 중 이 비율부터 도구 결과 비우기/압축 (0 초과 1 이하) |
| `JUNEES_HOME` | `~/.junees` | 세션 기록 등 상태를 두는 폴더 |

기본 도구는 `bash`, `read_file`, `write_file` 세 가지다. `bash`는 아직 승인 절차 없이 바로 실행되니(2단계에서 추가 예정) 믿을 수 있는 폴더에서만 쓴다.

## 구조

```
src/
  main.rs            CLI 진입점 (대화 모드, -p 실행, 세션 열기/이어 가기)
  agent.rs           에이전트 루프 (기록 변경을 세션 기록에 남김, 도구 결과 비우기/압축/초과 시 재시도)
  session.rs         세션 기록(JSONL) 쓰기, 재생, 이어 가기, 세션 찾기/목록, 비우기/압축 지점 계산
  context.rs         압축 요약 요청(시스템 프롬프트, 앞부분을 텍스트로 옮기기)
  message.rs         대화 메시지 타입 (Messages API 형식)
  provider/          LLM 제공자 트레이트와 Anthropic 클라이언트 (프롬프트 캐싱 표시)
  tools/             도구 트레이트, 레지스트리, bash/read_file/write_file
  config.rs          환경 변수 설정
  prompt.rs          시스템 프롬프트
  cli.rs             인자 파싱, 출력 보조 함수
tests/
  common/              테스트용 가짜 Anthropic API 서버
  anthropic_client.rs  가짜 API 서버로 실제 클라이언트와 루프를 끝까지 검증
  session_resume.rs    실제 바이너리로 -c, --resume <앞부분>, --no-session, HOME 없는 실행 검증
  context_management.rs 가짜 API 서버로 도구 결과 비우기, 압축, 캐시 표시, 기록 재생 검증
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
