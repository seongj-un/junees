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
./target/release/junees task init "할 일 관리 웹 앱을 만들어"   # 장기 작업 계획 (git 저장소 안에서)
./target/release/junees task run --max-sessions 5             # 기능 하나씩 작업 세션 반복
```

대화 모드 명령: `/sessions` 최근 세션 10개(id, 마지막 기록 시각, 첫 입력), `/reset` 새 세션 시작, `/usage` 현재 세션의 토큰 사용량, `/exit` 종료.

### 세션 기록

대화는 바뀔 때마다 `$JUNEES_HOME/sessions/<id>.jsonl`에 바로 한 줄씩 추가된다(기본 `~/.junees`). 프로세스가 언제 죽어도 마지막으로 기록된 지점에서 `-c`나 `--resume`으로 이어 갈 수 있다.

- id는 `20261109-061704-3fa2`처럼 UTC 시각 + 랜덤 hex라서 이름순이 생성순이다.
- 파일은 첫 입력이 기록될 때 만들어진다. 시작하고 바로 나가면 빈 세션이 남지 않는다.
- `-c`는 시작한 폴더(기록의 `cwd`)가 지금 폴더와 같은 세션 중 마지막으로 기록된(파일 수정 시각) 것을 고른다. 없으면 새 세션을 시작한다.
- `--resume`으로 다른 폴더에서 시작한 세션을 열면 `resumed <id> (N messages, started in <폴더>)`로 알려 준다. 기록 속 상대 경로는 그 폴더 기준이다.
- 세션 파일은 그 세션을 쓰는 Junees가 `flock`으로 잠근다. 아직 실행 중인 세션을 다른 터미널에서 `-c`나 `--resume`으로 열면 "session <id> is in use by another Junees" 오류로 끝나고 파일은 건드리지 않는다. 프로세스가 죽으면 잠금은 커널이 풀어 준다.
- 이어 갈 때 마지막 줄이 줄바꿈 없이 끝나고 읽히지 않으면(쓰는 도중 죽은 흔적) 그 줄만 버린다. 줄바꿈까지 있는 줄이 읽히지 않으면 위치와 상관없이 오류로 멈추고 파일을 고치지 않는다. 도구 실행 중에 끊겨서 결과가 없는 도구 호출은 `interrupted` 오류 결과로 채우고 그것도 기록한다.
- 첫 줄(`meta`)에 기록 형식 버전 `format`(현재 1)이 있고, 각 줄에 기록 시각 `ts`(unix 초)가 붙는다. 이 버전보다 새 형식의 세션은 "written by a newer Junees" 오류로 열지 않는다.
- 세션 파일에는 도구 출력이 그대로 들어가므로 파일은 `0600`, 새로 만드는 `$JUNEES_HOME`과 `sessions` 폴더는 `0700`으로 만든다.
- `JUNEES_HOME`과 `HOME`이 둘 다 없으면(cron, 일부 서비스와 컨테이너) 기록 없이 실행하고 "session not saved" 한 줄로 알린다. 이때 `-c`와 `--resume`만 오류다.
- 토큰 사용량도 기록되고, 이어 가면 그 세션의 누적값부터 다시 센다.
- 기록 쓰기가 실패하면 경고를 한 번 띄우고 그 세션의 나머지는 기록하지 않는다(파일이 대화의 빈틈 없는 앞부분으로 남도록).

### 장기 작업 모드

컨텍스트 창 하나에 담기지 않는 큰 목표를, 매번 새 컨텍스트로 시작하는 세션 여러 개가 기능 하나씩 끝내며 이어 간다. 완료 판정은 모델이 아니라 하네스가 검증 명령의 종료 코드로 한다.

- `task init "<목표>"`: git 저장소 안에서만 동작한다(자동으로 `git init`하지 않음). 이미 `.junees/features.json`이 있으면 오류. 초기화 세션이 목표를 검증 가능한 기능 목록으로 쪼개 `.junees/features.json`과 `.junees/progress.md`를 쓰고(필요하면 `.junees/init.sh`), 하네스가 목록을 검사한다. 틀리면 오류 내용을 같은 세션에 보내 한 번 고치게 하고, 그래도 틀리면 커밋하지 않고 오류로 끝난다. 통과하면 `git add -A` 후 `junees: initialize task`로 커밋한다.
- `features.json`은 `[{"id": "F1", "description": "...", "verify": "<셸 명령>", "passes": false}]` 형식이다. 비어 있지 않은 배열, 유일한 id, 비어 있지 않은 verify가 조건이고 새 작업은 전부 `passes: false`여야 한다. 다른 필드(steps 등)는 그대로 둔다.
- `task run [--max-sessions N]`(기본 20): 세션마다 새 에이전트(새 컨텍스트, 세션 기록은 `$JUNEES_HOME/sessions/`에 따로 남음)가 기능 목록과 통과 여부, `progress.md` 끝부분(4000자), `git log --oneline -20`을 system 프롬프트로 받고, 안 끝난 기능 하나를 구현, 검증, 커밋하고 `progress.md`를 갱신한다.
- 기능 통과는 `mark_feature_passing {id}` 도구로만 바뀐다. 하네스가 그 기능의 verify 명령을 저장소 루트에서 bash 도구와 같은 방식(출력 제한, API 키 변수 제거, 타임아웃 600초)으로 실행하고 종료 코드 0일 때만 `passes: true`로 저장한다. 실패하면 출력과 함께 오류 결과를 돌려준다.
- 하네스는 기능 목록의 권위 있는 사본을 들고 있다. 세션이 `features.json`을 직접 고치면(지우거나 `passes`를 바꾸는 것 포함) 세션이 끝날 때 사본으로 되돌려 쓰고 경고와 `progress.md` 한 줄을 남긴다. 서식만 다른 것은 편집으로 치지 않는다.
- 세션이 끝나면 하네스가 `progress.md`에 `- [junees] session N: 통과 수/전체, 새로 통과한 기능, 토큰` 한 줄을 붙이고, 작업 트리가 더러우면 `junees: checkpoint after session N`으로 커밋한다(하네스 줄이 붙으므로 사실상 세션마다 하나).
- 정지: 모든 기능 통과, `--max-sessions`만큼 실행, 연속 3세션 동안 새로 통과한 기능 없음(stalled), 세션 하나가 오류로 끝남(오류를 `progress.md`에 남기고 체크포인트 커밋 후 정지). 정지 이유를 `task stopped: ...`로 출력하고 `state.json`에 기록한다. stalled와 오류는 종료 코드 1.
- `.junees/state.json`(실행한 세션 수, 연속 무진전 세션 수, 누적 토큰, 정지 이유)은 `.junees/.gitignore`로 git에서 뺀다. 다음 `task run`은 세션 번호와 토큰을 여기서 이어 간다. 이전 실행이 스스로 멈췄으면 무진전 횟수를 0부터 다시 세고, 정지 이유 없이 끝났으면(크래시) 이어서 센다.
- 하위 폴더에서 실행해도 저장소 루트로 옮겨서 작업한다. 하네스의 git 명령과 세션의 `bash`(verify 포함)에서는 `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` 같은 저장소 지정 변수를 지운다. git 훅이나 worktree의 `git rebase -x`처럼 이 변수가 설정된 곳에서 실행해도 다른 저장소에 커밋하지 않게 하기 위해서다. 세션당 모델 호출 상한은 `JUNEES_MAX_TURNS`이고, 넘으면 그 세션은 오류로 끝난다.
- `bash`에 아직 승인 절차나 명령 정책이 없으므로(L4 예정) 믿을 수 있는 저장소에서만 쓴다.

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
  task/              장기 작업 모드
    runner.rs          task init/run 세션 루프, 체크포인트 커밋, 정지 조건
    features.rs        features.json 검증, 하네스의 권위 있는 사본(Ledger)
    tools.rs           mark_feature_passing (verify 명령을 하네스가 실행)
    state.rs           state.json (세션 수, 무진전 횟수, 토큰, 정지 이유)
    prompt.rs          초기화/작업 세션 프롬프트
    git.rs             저장소 확인, 커밋, 로그
  message.rs         대화 메시지 타입 (Messages API 형식)
  provider/          LLM 제공자 트레이트와 Anthropic 클라이언트
  tools/             도구 트레이트, 레지스트리, bash/read_file/write_file
  config.rs          환경 변수 설정
  prompt.rs          시스템 프롬프트
  cli.rs             인자 파싱(task 서브커맨드 포함), 출력 보조 함수
tests/
  common/              테스트용 가짜 Anthropic API 서버
  anthropic_client.rs  가짜 API 서버로 실제 클라이언트와 루프를 끝까지 검증
  session_resume.rs    실제 바이너리로 -c, --resume <앞부분>, --no-session, HOME 없는 실행 검증
  task_mode.rs         실제 바이너리로 임시 git 저장소에서 task init -> task run 검증
  task_git_env.rs      GIT_DIR이 다른 저장소를 가리켜도 task 모드가 제 저장소에서만 git을 쓰는지 검증
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
