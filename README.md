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
./target/release/junees task run --max-tokens 2000000 --max-time 2h   # 토큰/시간 예산 안에서만
./target/release/junees task status            # 작업 상태 보기 (--json이면 JSON)
```

대화 모드 명령: `/sessions` 최근 세션 10개(id, 마지막 기록 시각, 첫 입력), `/reset` 새 세션 시작, `/usage` 현재 세션의 토큰 사용량(캐시 쓰기/읽기 포함), `/context` 현재 컨텍스트 크기와 한도, `/compact` 앞부분을 요약으로 압축, `/exit` 종료.

### 중단 (Ctrl-C, SIGTERM)

- 대화 모드: 턴 실행 중 Ctrl-C는 그 턴만 취소하고 프롬프트로 돌아온다. 모델 호출 중이면 응답을 버리고, 도구 실행 중이면 그 도구의 프로세스 그룹 전체(백그라운드로 띄운 자식 포함)를 끝낸다. 결과가 없는 도구 호출은 `cancelled by the user` 오류 결과로 채워 기록하므로(세션 파일에도) 다음 입력이 그대로 이어진다. 프롬프트에서 Ctrl-C를 연달아 두 번 누르거나 Ctrl-D를 누르면 종료한다. `/compact` 중 Ctrl-C는 압축만 취소한다(기록은 그대로). SIGTERM은 턴을 같은 방식으로 취소하고 종료 코드 143으로 끝난다.
- `-p`: SIGINT/SIGTERM을 받으면 같은 방식으로 턴을 취소하고(도구 종료, 짝 채움, 세션 파일에 기록) `interrupted by SIGINT|SIGTERM`을 출력한 뒤 종료 코드 130/143으로 끝난다.
- `task init`/`task run`: 아래 장기 작업 모드 참고. 어느 모드든 두 번째 시그널은 정리 없이 바로 끝낸다(`task run`이면 다음 실행이 크래시로 보고 복구한다).
- 정상적으로 끝난 도구가 남긴 백그라운드 프로세스(`server > log 2>&1 &`)는 취소와 상관없이 그대로 둔다. 취소는 아직 실행 중인 도구에만 적용된다.

### 세션 기록

대화는 바뀔 때마다 `$JUNEES_HOME/sessions/<id>.jsonl`에 바로 한 줄씩 추가된다(기본 `~/.junees`). 프로세스가 언제 죽어도 마지막으로 기록된 지점에서 `-c`나 `--resume`으로 이어 갈 수 있다.

- id는 `20261109-061704-3fa2`처럼 UTC 시각 + 랜덤 hex라서 이름순이 생성순이다.
- 파일은 첫 입력이 기록될 때 만들어진다. 시작하고 바로 나가면 빈 세션이 남지 않는다.
- `-c`는 시작한 폴더(기록의 `cwd`)가 지금 폴더와 같은 세션 중 마지막으로 기록된(파일 수정 시각) 것을 고른다. 없으면 새 세션을 시작한다.
- `--resume`으로 다른 폴더에서 시작한 세션을 열면 `resumed <id> (N messages, started in <폴더>)`로 알려 준다. 기록 속 상대 경로는 그 폴더 기준이다.
- 세션 파일은 그 세션을 쓰는 Junees가 `flock`으로 잠근다. 아직 실행 중인 세션을 다른 터미널에서 `-c`나 `--resume`으로 열면 "session <id> is in use by another Junees" 오류로 끝나고 파일은 건드리지 않는다. 프로세스가 죽으면 잠금은 커널이 풀어 준다.
- 이어 갈 때 마지막 줄이 줄바꿈 없이 끝나고 읽히지 않으면(쓰는 도중 죽은 흔적) 그 줄만 버린다. 줄바꿈까지 있는 줄이 읽히지 않으면 위치와 상관없이 오류로 멈추고 파일을 고치지 않는다. 도구 실행 중에 끊겨서 결과가 없는 도구 호출은 `interrupted` 오류 결과로 채우고 그것도 기록한다.
- 첫 줄(`meta`)에 기록 형식 버전 `format`(현재 2)이 있고, 각 줄에 기록 시각 `ts`(unix 초)가 붙는다. 이 버전보다 새 형식의 세션은 "written by a newer Junees" 오류로 열지 않는다. 형식 1(L1) 세션도 그대로 읽고 이어 쓴다.
- 이어 쓸 때 첫 줄은 바꾸지 않으므로(덧붙이기만 함), 형식 1 세션을 이 버전으로 이어 쓴 뒤 L1 바이너리로 열면 "newer Junees"가 아니라 `line N is corrupt` 오류가 난다(파일은 그대로). 이 버전부터는 모르는 종류의 항목을 만나면 "continued by a newer Junees" 오류로 원인을 알려 준다.
- 항목 종류: `meta`, `message`, `append_text`, `usage`, 그리고 형식 2에서 추가된 `clear_tool_results`, `compact`, `summary_usage`(아래 컨텍스트 관리). 기록을 바꾸는 일도 항목으로 덧붙이기만 하고 파일을 새로 쓰지 않으므로, 재생 결과가 실행 중 기록과 같다.
- 세션 파일에는 도구 출력이 그대로 들어가므로 파일은 `0600`, 새로 만드는 `$JUNEES_HOME`과 `sessions` 폴더는 `0700`으로 만든다.
- `JUNEES_HOME`과 `HOME`이 둘 다 없으면(cron, 일부 서비스와 컨테이너) 기록 없이 실행하고 "session not saved" 한 줄로 알린다. 이때 `-c`와 `--resume`만 오류다.
- 토큰 사용량도 기록되고, 이어 가면 그 세션의 누적값부터 다시 센다.
- 기록 쓰기가 실패하면 경고를 한 번 띄우고 그 세션의 나머지는 기록하지 않는다(파일이 대화의 빈틈 없는 앞부분으로 남도록).

### 장기 작업 모드

컨텍스트 창 하나에 담기지 않는 큰 목표를, 매번 새 컨텍스트로 시작하는 세션 여러 개가 기능 하나씩 끝내며 이어 간다. 완료 판정은 모델이 아니라 하네스가 검증 명령의 종료 코드로 한다.

- `task init "<목표>"`: git 저장소 안에서만 동작한다(자동으로 `git init`하지 않음). 이미 `.junees/features.json`이 있거나, 작업 트리에 커밋하지 않은 변경이나 추적하지 않는 파일이 있으면(`.junees/`는 제외) 오류. 초기화 세션이 목표를 검증 가능한 기능 목록으로 쪼개 `.junees/features.json`과 `.junees/progress.md`를 쓰고(필요하면 `.junees/init.sh`), 하네스가 목록을 검사한다. 이때 verify 명령을 하나씩 실행해서 아직 아무것도 만들지 않았는데 이미 통과하는 것(`true`, 테스트가 하나도 걸리지 않는 필터 등)도 거절한다. 틀리면 오류 내용을 같은 세션에 보내 한 번 고치게 하고, 그래도 틀리면(또는 세션이 실패하면) 커밋하지 않고 오류로 끝나며, 거절된 목록은 `.junees/features.rejected.json`(git 제외)으로 옮겨서 `task init`을 다시 할 수 있게 한다. 통과하면 커밋한다(커밋 방식은 아래 체크포인트와 같음).
- `features.json`은 `[{"id": "F1", "description": "...", "verify": "<셸 명령>", "passes": false}]` 형식이다. 비어 있지 않은 배열, 유일한 id, 비어 있지 않은 verify가 조건이고 새 작업은 전부 `passes: false`여야 한다. 다른 필드(steps 등)는 그대로 둔다.
- `task run [--max-sessions N] [--max-tokens N] [--max-time 2h]`(세션 수 기본 20, 예산은 아래): 이전 실행이 크래시로 끝났으면 먼저 복구하고(아래), 작업 트리가 깨끗해야 시작한다(아니면 커밋하거나 `git stash -u` 하라는 오류). 시작할 때 `features.json`에서 `passes: true`인 기능의 verify를 모두 다시 실행해서, 실패하는 것은 통과로 치지 않는다(경고와 `progress.md` 한 줄). 실패한 init이나 하네스가 되돌리기 전에 죽은 세션이 남긴 목록, 나중 변경으로 깨진 기능을 걸러 내기 위해서다. 재검증이 바꾼 것(되돌린 통과, 하네스 형식으로 다시 쓴 `features.json`, verify 명령이 남긴 파일)은 세션을 시작하기 전에 `junees: recheck passing features at run start`로 커밋한다(체크포인트와 같은 방식). 그래서 세션 없이 정지해도(이미 넘은 예산, 재검증 직후 시그널, 할 일 없음) 작업 트리가 깨끗하게 남고 다음 실행이 시작된다. 세션마다 새 에이전트(새 컨텍스트, 세션 기록은 `$JUNEES_HOME/sessions/`에 따로 남음)가 기능 목록과 통과 여부, `progress.md` 끝부분(4000자), `git log --oneline -20`을 system 프롬프트로 받고, 안 끝난 기능 하나를 구현, 검증, 커밋하고 `progress.md`를 갱신한다.
- 기능 통과는 `mark_feature_passing {id}` 도구로만 바뀐다. 하네스가 그 기능의 verify 명령을 저장소 루트에서 bash 도구와 같은 방식(출력 제한, API 키 변수 제거, 타임아웃 600초)으로, `set -o pipefail`을 앞에 붙여 실행하고(`cargo test 2>&1 | tail`도 테스트가 실패하면 실패) 종료 코드 0일 때만 `passes: true`로 저장한다. 실패하면 출력과 함께 오류 결과를 돌려준다.
- 하네스는 기능 목록의 권위 있는 사본을 들고 있다. 세션이 `features.json`을 직접 고치면(지우거나 `passes`를 바꾸는 것 포함) 세션이 끝날 때 사본으로 되돌려 쓰고 경고와 `progress.md` 한 줄을 남긴다. 서식만 다른 것은 편집으로 치지 않는다.
- 세션이 끝나면 하네스가 `progress.md`에 `- [junees] session N: 통과 수/전체, 새로 통과한 기능, 토큰` 한 줄을 붙이고, 작업 트리가 더러우면 `junees: checkpoint after session N`으로 커밋한다(하네스 줄이 붙으므로 사실상 세션마다 하나). 하네스의 커밋은 `git add -A`에서 이름이 비밀 파일 같은 새 파일(`.env*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa`/`id_ed25519` 등 ssh 키, `.netrc`)을 빼고 경고한다. 이미 추적 중인 파일은 그대로 커밋한다. 세션이 `.junees/`나 `.junees/.gitignore`를 지워도 커밋 전에 다시 만든다.
- 정지: 모든 기능 통과, `--max-sessions`만큼 실행, 연속 3세션 동안 새로 통과한 기능 없음(stalled), 세션 하나가 오류로 끝남(공급자 오류 등. 오류를 `progress.md`에 남기고 체크포인트 커밋 후 정지), 세션 뒤 하네스 작업(되돌리기, progress 기록, 커밋)이 실패함, 예산 소진(`budget`), 시그널(`interrupted`). 정지 이유를 `task stopped: ...`로 출력하고 `state.json`에 기록한다. stalled와 오류는 종료 코드 1, 예산 소진은 `--max-sessions`처럼 0, 시그널은 130(SIGINT)/143(SIGTERM).
- 예산: `--max-tokens N`(기본값 `JUNEES_TASK_MAX_TOKENS`)은 작업의 모든 세션(init 세션과 요약 호출 포함)이 쓴 토큰의 합이다. 입력, 캐시 쓰기, 캐시 읽기, 출력 토큰을 모두 센다(캐시 읽기도 1토큰으로 센다. 비용이 아니라 처리한 양 기준). `state.json`의 누적값에서 이어 세므로 다시 실행해도 줄지 않는다(늘리려면 더 큰 값을 주고 실행). `--max-time`(기본값 `JUNEES_TASK_MAX_TIME`, `90s`, `30m`, `2h`, `1h30m` 형식)은 이번 `task run`이 시작한 뒤 흐른 실제 시간이다. 예산은 세션을 시작하기 전과, 세션 안에서는 모델 호출마다 그 직전에 확인한다(컨텍스트 압축의 요약 호출도 모델 호출로 따로 확인하므로, 요약 직후 예산이 다 되면 다음 호출 없이 끝난다). 시작할 때의 재검증도 verify 명령마다 그 직전에 예산을 확인해서(명령 하나가 최대 600초), 넘으면 남은 재검증 없이 목록을 그대로 두고 `budget`으로 정지한다. 확인하지 못한 통과는 확정되지 않았으므로 작업 완료로 치지 않는다. 세션 중에 넘으면 그 세션은 다음 모델 호출을 하지 않고 끝나며(진행 중인 도구는 끝까지 실행), 다른 세션과 똑같이 기능 목록 복원, `progress.md` 한 줄(`ended early: token budget used up: ...`), 체크포인트 커밋을 거친 뒤 `budget`으로 정지한다. 한 번의 모델 호출이 쓰는 양만큼은 넘을 수 있다.
- 시그널: `task run` 중 SIGINT(Ctrl-C)나 SIGTERM을 받으면 실행 중인 도구의 프로세스 그룹을 끝내고, 짝 없는 tool_use를 `cancelled by the user` 오류 결과로 채운 뒤, 그 세션을 보통 세션처럼 마무리(기능 목록 복원, `progress.md`에 `interrupted` 한 줄, 체크포인트 커밋)하고 `interrupted`로 정지한다. 시작할 때 verify를 다시 실행하는 중이었으면 아무것도 바꾸지 않고 정지한다. `task init` 중이면 init이 실패한 것과 같이 처리하고 130/143으로 끝난다.
- 동시 실행 방지와 크래시 복구: `task run`은 실행하는 동안 저장소 루트 폴더에 `flock`을 잡고(close-on-exec라서 세션이 남긴 백그라운드 프로세스는 잠금을 물려받지 않음. 잠금은 폴더의 inode에 걸리므로, 세션이 지웠다가 하네스가 다시 만들 수 있는 `.junees/`가 아니라 루트에 건다), `state.json`에 실행 중 표시(`running`: pid, 시작 시각)를 남긴다. 다른 실행이 잠금을 잡고 있으면 그 pid와 시작 시각을 알려 주며 거절한다. 정상 정지(위 모든 정지 이유)에서는 표시를 지운다. 시작할 때 표시가 남아 있는데 잠금은 비어 있으면 이전 실행이 죽은 것(kill -9, 전원 등)으로 보고 먼저 복구한다: `features.json`의 기능 정의(id, 설명, verify)를 작업의 init 커밋(가장 오래된 `junees: initialize task` 커밋, `git replace`는 무시)에서 가져와 되돌리고, `state.json`의 하네스 사본과 파일에서는 `passes: true`인 기능만 가져와서 시작 시 재검증이 init 커밋의 verify로 확인한다. `state.json`도 세션이 고칠 수 있는 파일이라 그 사본의 정의는 믿지 않는다(사본의 정의가 init 커밋과 다르면 경고와 `progress.md` 기록). 그래서 init 뒤에 손으로 고친 기능 정의는 크래시 복구 때 init 커밋 기준으로 돌아가니 다시 적용해야 하고, 이력에 init 커밋이 없으면(squash 등) 복구를 거절한다. 이어서 `progress.md`에 한 줄, 작업 트리가 더러우면 `junees: recover after crash`로 커밋(체크포인트와 같은 방식). 그다음 보통 실행처럼 이어 간다.
- 실행 로그: `.junees/run.log`에 실행 시작(pid, 통과 수, 예산 사용량), 세션 시작/끝(요약), 크래시 복구, 정지 이유가 UTC 시각과 함께 한 줄씩 쌓인다(`tail -f .junees/run.log`). git에서는 뺀다. 이 로그가 생기기 전에 만든 작업은 첫 실행 때 `.junees/.gitignore`에 `run.log`를 더하고 `junees: keep the run log out of git`로 따로 커밋한다.
- `.junees/state.json`(실행한 세션 수, 연속 무진전 세션 수, 누적 토큰, 정지 이유와 시각 `stopped_at`, init 때의 목표 `goal`, 실행 중 표시, 기능 목록의 하네스 사본)은 `.junees/.gitignore`로 git에서 뺀다. 다음 `task run`은 세션 번호와 토큰을 여기서 이어 간다. 이전 실행이 스스로 멈췄으면(`--max-sessions`, 예산, 시그널 포함) 무진전 횟수를 0부터 다시 세고, 정지 이유 없이 끝났으면(크래시) 이어서 센다. 그래서 `--max-sessions 1`로 여러 번 나눠 실행하면 stalled로는 멈추지 않는다. 크래시 난 세션이 쓴 토큰은 세션이 끝나야 더해지므로 누적값에 들어가지 않는다.
- 하위 폴더에서 실행해도 저장소 루트로 옮겨서 작업한다. 하네스의 git 명령과 세션의 `bash`(verify 포함)에서는 `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` 같은 저장소 지정 변수와 상위 git이 넘기는 설정(`GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`)을 지운다. git 훅이나 worktree의 `git rebase -x`처럼 이 변수가 설정된 곳에서 실행해도 다른 저장소에 커밋하지 않게 하기 위해서다. 하네스의 git은 세션이 심었을 수 있는 훅과 fsmonitor를 실행하지 않고(`-c core.hooksPath=/dev/null -c core.fsmonitor=false`, 커밋은 `--no-verify`), API 키 변수도 받지 않는다. 세션당 모델 호출 상한은 `JUNEES_MAX_TURNS`이고, 넘으면 그 세션만 끝난다. 작업은 체크포인트로 남고 다음 세션으로 넘어가며, 진전이 없으면 stalled 규칙이 멈춘다.
- `task status [--json]`: 저장소의 `.junees/`를 읽기만 하고(아무것도 쓰지 않으므로 실행 중인 `task run` 옆에서 써도 된다) 목표, 통과 수/전체와 진행 막대, 기능별 통과 여부와 설명 첫 줄(72자에서 자름), 세션 수와 연속 무진전 수, 누적 토큰(입력/출력/캐시 쓰기/캐시 읽기), 마지막 정지 이유와 시각, `progress.md`의 비어 있지 않은 마지막 5줄을 보여 준다. API 키가 필요 없다. `.junees/`가 없으면 `task init`을 먼저 하라는 오류. 실행 중 여부도 보여 준다: `state.json`의 실행 중 표시(pid, 시작 시각)와 그 pid의 프로세스가 살아 있는지(`kill(pid, 0)`)로 판단하고, 실행 잠금은 잡지 않는다(잡으면 그 순간 시작하는 `task run`이 거절되므로). 표시는 있는데 프로세스가 없으면 크래시로 보고 다음 `task run`이 복구한다고 알려 준다(드물게 pid가 재사용되면 크래시가 실행 중으로 보일 수 있다. `task run` 자신은 잠금으로 판단하므로 영향 없음). 실행 표시가 있는 동안에는 세션이 `features.json`을 직접 고쳤을 수 있으므로 기능 목록과 통과 수를 `state.json`의 하네스 사본(하네스가 마지막으로 저장한 것)에서 가져온다. 그래서 진행 중인 세션이 새로 통과시킨 기능은 세션이 끝나야 보인다.
- `task status --json` 필드: `repo`(저장소 폴더 이름), `root`(저장소 경로), `goal`(문자열 또는 `null`: 목표가 기록되기 전에 만든 작업), `passing`, `total`, `features`(`[{id, passes, description}]`, 설명 전체), `sessions`, `stalled`, `tokens`(`{input, output, cache_write, cache_read}`), `stop`(`null` 또는 `{reason, session, detail, message, at}`: `reason`은 `complete`/`max_sessions`/`stalled`/`error`/`budget`/`interrupted`로 `state.json`과 같은 값, `detail`은 오류 내용이나 다 쓴 예산 설명 또는 `null`, `message`는 사람이 읽는 한 문장, `at`은 unix 초 또는 `null`), `features_from`(`features.json` 또는 실행 표시가 있을 때 `harness`), `running`(`true`/`false`, 지금 실행 중인지), `run`(`null` 또는 `{pid, started, alive}`: `state.json`의 실행 중 표시, `started`는 unix 초, `alive`가 `false`면 크래시로 남은 표시), `progress_tail`(문자열 배열).
- 알림: `JUNEES_NOTIFY_WEBHOOK`에 디스코드 웹훅 URL을 넣으면 `task run`이 정지 이유를 기록한 뒤 웹훅에 한 번 POST한다. 내용은 저장소 이름, 정지 이유(키와 문장), 통과 수/전체, 세션 수, 누적 토큰. `JUNEES_NOTIFY_EACH_SESSION=1`이면 세션이 끝날 때마다 `session N done: ...` 한 줄도 보낸다 세션 알림도 그 세션의 토큰과 (마지막 세션이면) 정지 기록을 `state.json`에 저장한 뒤에 보내므로, 알림을 기다리는 동안 프로세스가 죽어도 기록은 남는다. 예산 소진(`budget`)과 시그널(`interrupted`) 정지도 같은 형식으로 알린다. 시그널을 받은 뒤에는 세션 한 줄 알림은 건너뛰고 정지 알림만 보낸다(두 번째 시그널은 알림을 기다리지 않고 바로 끝낸다). 본문은 `{"content": "...", "allowed_mentions": {"parse": []}}`(멘션 알림 없음)이고, 저장소 이름, 정지 문장, 세션 요약, 오류처럼 바뀌는 부분은 인라인 코드(`` ` ``)로 감싸 마크다운 링크나 서식이 적용되지 않게 한다(백틱은 `'`로, 줄바꿈은 공백으로 바꾸고 한 부분은 1500자까지). `content`는 2000자에서 자른다(UTF-16 단위로 세어 이모지도 안전). 요청 하나의 제한 시간은 10초, 리다이렉트는 따라가지 않는다. 실패(연결 실패, 시간 초과, 2xx가 아닌 응답)는 `!! notification to <호스트> failed: ...` 경고만 띄우고 종료 코드는 바꾸지 않는다. URL에는 웹훅 토큰이 들어 있어서 경고와 오류에는 호스트만 나오고, 세션의 `bash`와 하네스 git에도 이 변수를 넘기지 않는다. 정지를 기록하기 전에 실패하면(작업 트리가 더러움, 기능 목록을 읽지 못함, 상태 파일을 쓰지 못함 등) `task in <저장소> run failed: <오류>` 한 줄을 대신 보낸다. 설정 오류(API 키 없음 등)처럼 `task run`이 작업을 열기도 전에 끝나는 경우는 알림이 없다.
- 세션의 `bash`와 verify 명령에는 대화 모드와 같은 명령 정책과 숨김 변수 설정(`JUNEES_DENY_COMMANDS`, `JUNEES_ALLOW_DANGEROUS`, `JUNEES_HIDDEN_ENV`)이 걸리고, 세션의 컨텍스트 관리도 같은 설정(`JUNEES_CONTEXT_WINDOW`, `JUNEES_COMPACT_THRESHOLD`)을 쓴다. 정책에 막힌 verify 명령은 실패로 친다.

### 컨텍스트 관리

긴 작업에서도 대화가 모델의 컨텍스트 창을 넘지 않게 하고, 반복되는 앞부분은 캐시한다.

- 프롬프트 캐싱: 매 요청에서 도구 목록의 마지막 도구, 시스템 프롬프트, 마지막 메시지의 마지막 블록에 `cache_control: ephemeral`을 붙인다. 한 번만 보내는 요약 호출에는 붙이지 않는다(캐시 쓰기는 일반 입력보다 비싸다). 응답의 캐시 쓰기/읽기 토큰은 `/usage`에 따로 나온다.
- 컨텍스트 크기: 마지막 응답의 `input_tokens + cache_creation_input_tokens + cache_read_input_tokens + output_tokens`. 세션 기록의 `usage` 항목에서 복원되므로 이어 가도 유지된다. 한도는 `JUNEES_CONTEXT_WINDOW` x `JUNEES_COMPACT_THRESHOLD`(기본 200000 x 0.8 = 160000).
- 1차 대응(오래된 도구 결과 비우기): 모델 호출 직전에 크기가 한도 이상이면, 도구 결과가 든 메시지 중 최근 3개를 뺀 이전 것들의 출력을 `[tool output cleared to save context]`로 바꾼다. `tool_use_id`와 `is_error`는 그대로라 호출/결과 짝이 유지된다. 기록 항목은 `clear_tool_results { upto }`.
- 2차 대응(압축): 비울 것이 없거나, 비운 뒤 다음 응답에서도 한도 이상이면 앞부분을 같은 모델로 요약한다. 꼬리는 최근 두 사용자 입력(안 되면 마지막 입력)부터 끝까지이고, 도구 결과가 든 메시지나 도구 호출 바로 뒤에서는 자르지 않는다. 입력이 첫 메시지 하나뿐인 긴 턴(`-p` 한 번으로 끝까지 가는 작업 등)은 그 턴 안에서 자른다: 도구 결과만 든 user 메시지 바로 다음 assistant 메시지(앞의 호출이 모두 결과를 받은 지점)에서, 최근 도구 왕복 두 번(안 되면 한 번)을 남긴다. 이때는 요약과 그 턴의 원래 입력(원문 그대로)을 담은 user 메시지를 꼬리 앞에 새로 넣는다. 사용자 입력에서 자를 수 있으면 그쪽을 먼저 쓴다. 요약 호출은 도구 없이, 요약 전용 시스템 프롬프트로, 앞부분을 텍스트로 옮긴 user 메시지 하나를 보낸다(`max_tokens` 4096). 요약은 꼬리 첫 user 메시지 맨 앞에 `[Summary of the earlier conversation]` 블록으로 들어가 역할 교대가 유지된다. 기록 항목은 `compact { summary, keep_from }`이고, 요약 호출의 사용량은 `summary_usage`로 남아 `/usage`에는 더해지지만 컨텍스트 크기에는 쓰이지 않는다. 압축하면 `(compacted the conversation: N messages -> M)` 한 줄이 나온다.
- 요약 호출이 실패하거나, 빈 요약이 오거나, 요약이 출력 한도(`max_tokens`)에 걸려 잘리면 경고만 띄우고 기록은 그대로 둔다(요약 호출 사용량은 더한다).
- `/compact`는 한도와 상관없이 지금 압축한다.
- API가 컨텍스트 초과(400 "prompt is too long")로 거절하면, 크기 추적과 상관없이 비우기와 압축을 하고, 이어서 최근 3개를 포함한 모든 도구 결과도 비운다(한 응답의 병렬 `read_file` 결과처럼 최근 결과가 원인일 수 있어서). 줄어든 게 있으면 한 번만 다시 보내고, 그래도 실패하면 오류로 끝난다.

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
| `JUNEES_DENY_COMMANDS` | (없음) | `bash`에서 추가로 막을 명령 접두사, 쉼표 구분 (예: `docker rm,kubectl delete`) |
| `JUNEES_ALLOW_DANGEROUS` | `0` | `1`이면 기본 명령 차단 규칙을 끈다 (사용자 책임) |
| `JUNEES_HIDDEN_ENV` | (없음) | `bash`에서 추가로 숨길 환경 변수 이름, 쉼표 구분 (예: `DB_PASSWORD,GITHUB_TOKEN`) |
| `JUNEES_NOTIFY_WEBHOOK` | (없음) | `task run`이 멈출 때 알림을 보낼 디스코드 웹훅 URL (https만; 평문 http는 localhost와 루프백 주소에만 허용) |
| `JUNEES_NOTIFY_EACH_SESSION` | `0` | `1`이면 작업 세션마다 한 줄 알림도 보낸다 |
| `JUNEES_TASK_MAX_TOKENS` | (없음) | `task run --max-tokens`의 기본값 (작업 전체 누적 토큰) |
| `JUNEES_TASK_MAX_TIME` | (없음) | `task run --max-time`의 기본값 (이번 실행 시간, 예: `2h`, `1h30m`) |

기본 도구는 `bash`, `read_file`, `write_file`, `edit_file` 네 가지다(`task` 작업 세션도 같은 네 가지에 `mark_feature_passing`을 더해 쓴다). `bash`는 승인 절차 없이 바로 실행되니(로드맵 3단계에서 추가 예정) 믿을 수 있는 폴더에서만 쓴다. `bash`에는 `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `JUNEES_NOTIFY_WEBHOOK`과 `JUNEES_HIDDEN_ENV`에 적은 변수가 보이지 않는다. 리눅스에서는 junees가 시작할 때 자기 프로세스를 dump 불가(`PR_SET_DUMPABLE` 0)로 만들어, 같은 사용자의 프로세스도 `/proc/<junees pid>/environ`과 `mem`을 읽지 못한다. 그래서 `bash`가 `/proc/$PPID/environ`처럼 부모를 거슬러 올라가도 이 변수들을 읽을 수 없다. 자식 프로세스는 exec할 때 다시 보통 프로세스가 되므로 도구 동작은 그대로이고, junees 자신의 코어 덤프만 남지 않는다.

`edit_file {path, old_string, new_string, replace_all?}`은 파일의 일부만 고치는 도구다. 큰 파일을 고칠 때 `write_file`로 전체를 다시 쓰지 않아도 되므로 출력 토큰이 줄고, 긴 출력이 `max_tokens`에 잘리는 일도 줄어든다. 시스템 프롬프트에도 "부분 수정은 `edit_file`, 새 파일이나 전체 교체는 `write_file`"이라고 적혀 있다.

- `old_string`은 공백과 들여쓰기까지 정확히 같아야 하고, 파일에서 정확히 한 번 나와야 한다(겹치는 위치도 따로 센다. `aaa`에서 `aa`는 두 번). 여러 번 나오면 횟수와 줄 번호(최대 20개)를 오류로 돌려주고 아무것도 바꾸지 않는다. 모두 바꾸려면 `replace_all: true`.
- 찾지 못하면 `old_string`의 첫 줄(공백 제외)과 가장 비슷한 줄을 최대 3개, 줄 번호와 함께 힌트로 보여 준다(문자 2-gram 유사도 0.5 이상).
- 빈 `old_string`, `old_string`과 같은 `new_string`은 오류다. 대상은 이미 있는 일반 파일이고 10 MB 이하(`read_file`과 같은 한도), UTF-8이어야 한다. 심볼릭 링크는 가리키는 파일을 고친다.
- 줄바꿈 보존: `read_file`이 보여 준 대로 `\n`으로 쓴 `old_string`이 그대로는 없으면 `\r\n`으로 바꿔 다시 찾는다(대부분 LF인 파일의 CRLF 줄도). 두 종류가 섞인 부분에 걸친 `old_string`은 찾지 못하며, 파일에 두 종류가 섞여 있으면 찾지 못했다는 오류에 그 사실을 적는다. `new_string`의 줄바꿈은 파일에서 일치한 부분을 따른다. 일치한 부분의 줄바꿈이 모두 `\r\n`이면(`\n`으로 시작하는 `old_string`이 `\r\n`의 `\n`에서 일치한 경우 포함) `new_string`의 `\n`을 `\r\n`으로 쓰고, 일치한 부분에 줄바꿈이 없으면 파일 대부분의 줄바꿈을 따른다. `replace_all`이면 일치한 곳마다 따로 정한다. 원래 파일이 개행으로 끝났는데 고친 결과가 개행 없이 끝나면 개행을 다시 붙인다(결과에 `kept the final newline` 표시).
- 같은 폴더의 임시 파일(`.<이름>.junees-edit-<pid>-<번호>.tmp`)에 원래 권한으로 쓰고 `fsync`한 뒤 `rename`으로 바꿔 넣으므로 중간에 죽어도 반쯤 쓴 파일이 남지 않는다. 쓰기, 권한 설정, `fsync`, `rename` 중 하나라도 실패하면(디스크가 가득 찬 경우 등) 임시 파일을 지우고 오류를 돌려주며 원본은 그대로다. 이 과정은 한 blocking 스레드에서 끝까지 실행되므로, 편집 중에 턴이 취소돼도 임시 파일이 작업 트리에 남지 않는다. 소유자와 하드 링크는 유지되지 않는다(새 inode).
- 성공하면 바꾼 부분과 위아래 3줄을 `read_file`과 같은 줄 번호 형식으로 보여 준다(`replace_all`이면 처음 3곳까지, 나머지는 개수만).

### 명령 정책

`bash`는 명령을 실행하기 전에 정책으로 검사하고, 걸리면 아무것도 실행하지 않은 채 `blocked by policy: <이유>` 오류 결과를 모델에 돌려준다. 사람이 지켜보지 않는 장기 실행에서 치명적인 명령을 막는 1차 안전장치이고 샌드박스는 아니다.

기본으로 막는 것:

- 루트, 최상위 시스템 폴더(`/usr`, `/etc`, `/home` 등), 홈, 상위 폴더의 재귀 삭제: `rm -r`/`-rf`/`--recursive` 대상이 `/`, `/*`, `~`, `~/`, `$HOME`, `${HOME}`, 홈의 절대 경로나 그 위, `..`, `../..`처럼 현재 폴더 위로 올라가는 경로. `rm -rf ./build`, `rm -rf target`, `rm -rf ../other/target`은 허용
- 위험한 폴더로 `cd` 한 뒤 현재 폴더 전체 삭제: 같은 스크립트에서 앞서 `cd`/`pushd`로 홈(인자 없는 `cd` 포함), `/`, 최상위 시스템 폴더, 상위 폴더, `cd -`, `$`가 들어간 경로(`cd "$BUILD_DIR"`, 변수가 비면 홈으로 간다)에 들어갔으면 그 뒤의 `rm -r *`, `rm -r .`, `rm -r ./*`, `rm -r .*`를 막는다. 그 뒤 다른 폴더로 `cd` 하면 풀린다. `cd build && rm -rf *`는 허용
- 파일시스템/디스크 파괴: `mkfs*`, `wipefs`, `dd ... of=/dev/...`, `> /dev/sda` 같은 장치로의 리다이렉트 (`/dev/null`, `/dev/stdout`, `/dev/stderr`, `/dev/fd/*`, `/dev/tcp/*`, 메모리 파일시스템인 `/dev/shm/*`, `/dev/mqueue/*` 등은 허용)
- 시스템 종료: `shutdown`, `reboot`, `halt`, `poweroff`, `systemctl reboot|poweroff|halt|kexec`
- fork bomb (`:(){ :|:& };:`과 같은 모양: 함수 본문 안에서 자기 자신을 자기 자신에게 파이프로 넘기고 `&`로 띄우는 것. `f(){ ...; }; f | f` 같은 평범한 함수 파이프는 허용)
- 내려받은 스크립트 바로 실행: `curl`/`wget ... | sh`(bash, zsh 등 셸 포함), `bash <(curl ...)`, `sh -c "$(curl ...)"`
- 강제 push: `git push --force`, `-f`, `+refspec` (`--force-with-lease`는 허용)
- 권한 상승: `sudo`, `doas`, `pkexec`, `su`, `runuser`

검사 방식:

- 명령을 셸처럼 대략 나눠서 `;`, `&&`, `||`, `|`, `&`, 줄바꿈, 괄호로 이어진 각 부분, `$(...)`과 백틱 안, `sh -c '...'`, `eval`과 `env -S` 인자까지 검사한다. 셸이 표준 입력으로 읽는 스크립트도 검사한다: `bash <<'EOF'`의 here-document 본문, `bash <<< '...'`, `echo`/`printf`/here-document를 받은 `cat`을 파이프로 넘긴 `... | sh`. 앞에 붙은 `FOO=1`, `env`, `nohup`, `timeout 5`, `nice`, `setsid`, `stdbuf`, `ionice`, `chrt`, `taskset`, `busybox`, `command`, `then`/`do`, `function 이름` 같은 키워드와 래퍼는 건너뛰고 실제 실행되는 프로그램을 본다. `$'...'` 문자열은 이스케이프를 풀어서 본다. 프로그램 이름은 경로를 뗀 이름으로 비교한다(`/sbin/reboot`도 차단).
- 따옴표 안 문자열, 주석, 셸이 아닌 명령(`cat > notes.md <<'EOF'`, `python3 - <<'EOF'`)이 받는 here-document 본문은 명령으로 보지 않는다. `echo "rm -rf /"`, `grep shutdown log.txt`, `git commit -m "git push --force"`는 허용된다.
- 실행 중에 만들어지는 단어(`rm -rf $(echo /)`, 변수에 담긴 경로)는 알 수 없으므로 막지 못한다. `$(...)`, `sh -c`, `eval`이 합쳐서 8단계보다 깊게 중첩되면 검사하지 않고 막는다.
- `JUNEES_DENY_COMMANDS`의 접두사는 단어 단위로 비교한다. `docker rm`은 `docker rm -f web`을 막지만 `docker rmi`는 막지 않는다. 이 규칙은 `JUNEES_ALLOW_DANGEROUS=1`이어도 적용된다.
- `JUNEES_ALLOW_DANGEROUS`는 `1`/`true`/`yes`/`on` 또는 `0`/`false`/`no`/`off`만 받고, 다른 값이면 시작할 때 오류로 멈춘다.

## 구조

```
src/
  main.rs            CLI 진입점 (대화 모드, -p 실행, 세션 열기/이어 가기, SIGINT/SIGTERM 처리)
  agent.rs           에이전트 루프 (기록 변경을 세션 기록에 남김, 도구 결과 비우기/압축/초과 시 재시도, 취소, 모델 호출 전 한도 확인)
  cancel.rs          취소 플래그 (시그널 처리기와 에이전트 루프가 공유)
  session.rs         세션 기록(JSONL) 쓰기, 재생, 이어 가기, 세션 찾기/목록, 비우기/압축 지점 계산
  context.rs         압축 요약 요청(시스템 프롬프트, 앞부분을 텍스트로 옮기기)
  task/              장기 작업 모드
    runner.rs          task init/run 세션 루프, 체크포인트 커밋, 정지 조건, 크래시 복구, 실행 로그
    budget.rs          토큰/시간 예산, 기간 파싱(90s, 30m, 2h)
    lock.rs            동시 실행 방지(저장소 루트 flock)
    features.rs        features.json 검증, 하네스의 권위 있는 사본(Ledger)
    tools.rs           mark_feature_passing (verify 명령을 하네스가 실행)
    state.rs           state.json (세션 수, 무진전 횟수, 토큰, 정지 이유와 시각, 목표, 실행 중 표시, 기능 목록 사본)
    status.rs          task status (읽기 전용, 텍스트/JSON)
    prompt.rs          초기화/작업 세션 프롬프트
    git.rs             저장소 확인, 커밋, 로그
  message.rs         대화 메시지 타입 (Messages API 형식)
  notify.rs          디스코드 웹훅 알림 (정지, 세션마다)
  provider/          LLM 제공자 트레이트와 Anthropic 클라이언트 (프롬프트 캐싱 표시)
  tools/             도구 트레이트, 레지스트리, bash/read_file/write_file/edit_file, bash 명령 정책(policy.rs)
  config.rs          환경 변수 설정 (도구 설정 ToolSettings 포함)
  prompt.rs          시스템 프롬프트
  cli.rs             인자 파싱(task 서브커맨드 포함), 출력 보조 함수
tests/
  common/              테스트용 가짜 Anthropic API 서버
  anthropic_client.rs  가짜 API 서버로 실제 클라이언트와 루프를 끝까지 검증
  session_resume.rs    실제 바이너리로 -c, --resume <앞부분>, --no-session, HOME 없는 실행 검증
  secret_env.rs        기본 bash에서 API 키와 웹훅 URL이 보이지 않는지 검증
  hidden_env.rs        JUNEES_HIDDEN_ENV로 지정한 변수가 bash에서 보이지 않는지 검증
  proc_environ.rs      실제 바이너리의 bash 손자 프로세스가 조상들의 /proc/<pid>/environ에서 API 키와 웹훅 URL을 읽지 못하는지 검증
  context_management.rs 가짜 API 서버로 도구 결과 비우기, 압축, 캐시 표시, 기록 재생 검증
  task_mode.rs         실제 바이너리로 임시 git 저장소에서 task init -> task run -> task status, task run 중 SIGTERM, 가짜 웹훅으로 알림 검증
  cancel.rs            실제 바이너리로 -p 중 SIGTERM(종료 코드 143), 대화 모드 Ctrl-C(턴 취소 후 다음 입력, 두 번이면 종료) 검증
  task_git_env.rs      GIT_DIR이나 GIT_CONFIG_PARAMETERS가 있어도 task 모드가 제 저장소와 설정으로만 git을 쓰는지 검증
  task_git_secret.rs   하네스 git이 심어진 훅을 실행하지 않고 git이 실행하는 필터에도 API 키를 넘기지 않는지 검증
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
