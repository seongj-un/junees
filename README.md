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
```

대화 안에서 쓰는 명령:

- `/task <목표>`: 지금 폴더의 git 저장소에서 장기 작업을 계획하고 바로 실행한다(`junees task "목표"`와 같음). `/task`만 쓰면 멈춘 작업을 이어서 한다(`junees task`).
- `/status`: 작업 상태(`junees status`와 같음).
- `/sessions`: 최근 세션 10개를 번호와 함께 보여 주고, 번호를 넣으면 그 세션으로 이어 간다(Enter는 그대로).
- `/new`: 새 대화(예전 이름 `/reset`도 됨).
- `/model [이름]`: 지금 모델을 보여 주거나, 이 실행 동안만 다른 모델로 바꾼다.
- `/backend [api|claude-code]`: `/task`를 무엇으로 돌릴지 보여 주거나, 이 실행 동안만 바꾼다. 대화 자체는 늘 API를 쓴다.
- `/compact`, `/context`, `/usage`, `/help`, `/exit`.

명령줄에서도 `junees task "목표"`(계획 + 실행), `junees task`(이어서 실행), `junees status`(상태). 하위 명령 이름과 같은 한 단어 질문(`status`, `task`, `setup`)은 `junees -p status`처럼 `-p`로 묻는다. `-`로 시작하는 질문은 `junees -- "-5도면 추워?"`.

### 설정 파일과 처음 설정

설정은 `$JUNEES_HOME/config.toml`(기본 `~/.junees/config.toml`)에 있다. 처음 실행할 때 설정 파일도 `ANTHROPIC_API_KEY`도 없고 표준 입력과 표준 오류가 모두 터미널이면 설정 마법사가 먼저 뜬다(`junees setup`으로 언제든 다시).

1. Claude에 닿는 방법: `1) Claude subscription`(Claude Code CLI 로그인 필요) 또는 `2) Anthropic API key`.
2. 구독이면 `claude --version`과 `claude auth status --json`으로 확인한다(작업 모드가 쓰는 것과 같은 검사). 로그인이 안 돼 있으면 방법(`claude` 실행 후 `/login`, 브라우저가 없으면 `claude setup-token`, Linux면 bubblewrap 설치)을 보여 주고 설정은 저장한다. 구독은 지금 장기 작업(`junees task`, `/task`)만 돌린다. 대화(`junees`, `junees "질문"`)는 API를 직접 부르므로 API 키가 따로 필요하고, 마법사가 그 자리에서 키를 넣을지 묻는다.
3. API 키는 화면에 보이지 않게 입력한다(터미널 echo를 끔). `sk-ant-`로 시작하지 않으면 다시 묻고, 잘못 넣은 값도 화면에 다시 찍지 않는다.
4. 대화 모델(기본 `claude-sonnet-4-6`)과 작업 기본 예산(시간 기본 `2h`, 토큰 상한은 선택)을 묻고 저장한다.

Ctrl-C나 입력 끝(Ctrl-D)이면 아무것도 저장하지 않고 끝난다. 터미널이 아니면(cron, 파이프) 마법사를 띄우지 않고 `no API key: run `junees setup`, or set ANTHROPIC_API_KEY` 오류로 끝난다. `claude-code` 백엔드로 고른 작업 명령은 키가 필요 없으므로 마법사 없이 실행한다.

파일은 TOML의 단순한 부분만 쓴다: 한 줄에 `키 = 값`, 값은 따옴표 문자열이나 숫자, `true`/`false`, `#` 주석. 표(`[...]`)와 배열은 받지 않는다. 키마다 환경 변수 하나에 대응하고, **환경 변수가 있으면 파일보다 우선한다**(`/model`, `/backend`로 바꾼 값은 그보다도 우선하지만 이 실행에만 적용되고 파일은 바꾸지 않는다). 모르는 키, 따옴표 없는 문자열(`model = claude-...`), 같은 키 두 번은 `<경로> is not a valid settings file ... line N: ...` 오류로 알려 준다.

```toml
backend = "claude-code"          # JUNEES_BACKEND
api_key = "sk-ant-..."           # ANTHROPIC_API_KEY
model = "claude-sonnet-4-6"      # JUNEES_MODEL
task_max_time = "2h"             # JUNEES_TASK_MAX_TIME
task_max_tokens = 2000000        # JUNEES_TASK_MAX_TOKENS
```

그 밖의 키: `base_url`, `max_tokens`, `max_turns`, `context_window`, `compact_threshold`, `thinking_budget`, `approval`, `stream`, `deny_commands`, `allow_dangerous`, `hidden_env`(쉼표로 구분한 문자열), `notify_webhook`, `notify_each_session`, `claude_bin`, `claude_model`, `web_search`, `web_search_max_uses`, `web_search_allowed_domains`, `web_search_blocked_domains`(쉼표로 구분한 문자열). 각각 `JUNEES_` 접두 환경 변수(`base_url`은 `ANTHROPIC_BASE_URL`)와 같다. `JUNEES_HOME`과 `HOME`은 파일 위치를 정하므로 파일에 둘 수 없고, Junees가 읽지 않는 변수(`ANTHROPIC_TOKEN` 등)는 파일로도 설정되지 않는다.

파일은 API 키를 담을 수 있으므로 `0600`, 새로 만드는 폴더는 `0700`으로 만들고, 임시 파일에 쓴 뒤 이름을 바꿔서 중간에 죽어도 반쯤 쓴 파일이 남지 않는다. 마법사는 저장한 뒤 `ANTHROPIC_API_KEY`나 `JUNEES_BACKEND`가 환경에 있으면 그쪽이 이긴다고 알려 준다. 파일의 키는 환경 변수가 아니므로 bash 도구의 환경에는 처음부터 없다. 모델의 도구도 이 파일에 손대지 못한다: `read_file`/`write_file`/`edit_file`은 이 파일을 가리키는 경로(`..`, 심볼릭 링크, 아직 없는 파일을 가리키는 링크, 하드 링크 포함)를 "사용자의 비공개 설정 파일"이라는 오류로 거절하고, `bash` 명령 정책은 이 파일을 인자나 리다이렉트 대상으로 쓰는 명령과, 이 파일이 든 폴더(`$JUNEES_HOME`)를 통째로 다루는 명령을 막는다(아래 명령 정책 참고). 쓰기까지 막는 것은 파일에 `allow_dangerous = "1"`을 넣어 다음 실행의 명령 규칙을 끄는 일도 막기 위해서다. `$JUNEES_HOME`의 나머지(세션 기록 등)는 그대로 읽을 수 있다. 다만 bash 쪽은 차단 목록일 뿐 샌드박스가 아니어서 다른 변수나 실행 중에 만든 경로, 더 위 폴더를 통째로 다루는 명령으로는 여전히 읽을 수 있다(`~/.ssh`처럼 같은 사용자의 파일이다).

## 고급 사용법

예전 명령과 플래그는 그대로 동작한다. `junees --help`도 첫 부분은 위 사용법, 나머지는 `Advanced` 절이다.

```sh
junees -p "이 폴더에 뭐가 있어?"        # 한 번만 실행 (위치 인자와 같음)
junees -c -p "계속"                     # 이어서 한 번만 실행 (junees -c "계속"도 됨)
junees --resume 20261109-0617           # 지정한 세션 (id 앞부분만 써도 유일하면 됨)
junees --no-session                     # 기록하지 않음
junees task init "할 일 관리 웹 앱을 만들어"   # 계획만 (git 저장소 안에서)
junees task run --max-sessions 5        # 기능 하나씩 작업 세션 반복 (junees task 와 같음)
junees task --max-tokens 2000000 --max-time 2h   # 토큰/시간 예산 안에서만 (task run 도 같은 옵션)
junees task --max-time 30m "목표"        # 계획 + 실행에 옵션
junees task status --json               # 작업 상태 JSON (junees status --json)
junees task --backend claude-code "할 일 관리 웹 앱을 만들어"   # 이번만 Claude 구독으로 (아래)
JUNEES_BACKEND=claude-code junees task --max-time 2h
```

`cargo install` 없이 `cargo build --release` 후 `./target/release/junees`로 써도 된다. 설정 파일 없이 환경 변수만으로도 예전처럼 동작한다(`export ANTHROPIC_API_KEY=sk-ant-...`).

### 중단 (Ctrl-C, SIGTERM)

- 대화 모드: 턴 실행 중 Ctrl-C는 그 턴만 취소하고 프롬프트로 돌아온다. 모델 호출 중이면 응답을 버리고, 도구 실행 중이면 그 도구의 프로세스 그룹 전체(백그라운드로 띄운 자식 포함)를 끝낸다. 결과가 없는 도구 호출은 `cancelled by the user` 오류 결과로 채워 기록하므로(세션 파일에도) 다음 입력이 그대로 이어진다. 프롬프트에서 Ctrl-C를 연달아 두 번 누르거나 Ctrl-D를 누르면 종료한다. `/compact` 중 Ctrl-C는 압축만 취소한다(기록은 그대로). SIGTERM은 턴을 같은 방식으로 취소하고 종료 코드 143으로 끝난다. 승인 질문을 기다리는 중 Ctrl-C는 그 호출을 실행하지 않고 턴을 취소한다.
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
- 첫 줄(`meta`)에 기록 형식 버전 `format`(현재 3)이 있고, 각 줄에 기록 시각 `ts`(unix 초)가 붙는다. 이 버전보다 새 형식의 세션은 "written by a newer Junees" 오류로 열지 않는다. 형식 1(L1) 세션도 그대로 읽고 이어 쓴다.
- 이어 쓸 때 첫 줄은 바꾸지 않으므로(덧붙이기만 함), 형식 1 세션을 이 버전으로 이어 쓴 뒤 L1 바이너리로 열면 "newer Junees"가 아니라 `line N is corrupt` 오류가 난다(파일은 그대로). 이 버전부터는 모르는 종류의 항목을 만나면 "continued by a newer Junees" 오류로 원인을 알려 준다.
- 항목 종류: `meta`, `message`, `append_text`, `usage`, 그리고 형식 2에서 추가된 `clear_tool_results`, `compact`, `summary_usage`(아래 컨텍스트 관리).
- 형식 3: 메시지 안의 thinking, redacted_thinking, 서버 도구 블록처럼 Junees가 직접 다루지 않는 블록을 API가 보낸 JSON 그대로 저장한다(아래 확장 사고). 이전 버전은 이런 블록을 API가 거절하는 자리표시로 읽기 때문에 형식을 올렸고, 이전 바이너리는 형식 3 세션을 "newer Junees" 오류로 열지 않는다. 형식 1, 2 세션도 그대로 읽는다. 이런 세션을 이 버전으로 이어 쓰면 첫 새 줄 앞에 `{"type":"format","format":3}` 항목을 한 번 덧붙인다. 이전 바이너리는 이 모르는 항목에서 "continued by a newer Junees" 오류로 멈추므로, thinking 블록을 자리표시로 잘못 읽어 400을 받는 일이 없다. 기록을 바꾸는 일도 항목으로 덧붙이기만 하고 파일을 새로 쓰지 않으므로, 재생 결과가 실행 중 기록과 같다.
- 세션 파일에는 도구 출력이 그대로 들어가므로 파일은 `0600`, 새로 만드는 `$JUNEES_HOME`과 `sessions` 폴더는 `0700`으로 만든다.
- `JUNEES_HOME`과 `HOME`이 둘 다 없으면(cron, 일부 서비스와 컨테이너) 기록 없이 실행하고 "session not saved" 한 줄로 알린다. 이때 `-c`와 `--resume`만 오류다.
- 토큰 사용량도 기록되고, 이어 가면 그 세션의 누적값부터 다시 센다.
- 기록 쓰기가 실패하면 경고를 한 번 띄우고 그 세션의 나머지는 기록하지 않는다(파일이 대화의 빈틈 없는 앞부분으로 남도록).

### 장기 작업 모드

컨텍스트 창 하나에 담기지 않는 큰 목표를, 매번 새 컨텍스트로 시작하는 세션 여러 개가 기능 하나씩 끝내며 이어 간다. 완료 판정은 모델이 아니라 하네스가 검증 명령의 종료 코드로 한다.

- `task init "<목표>"`: git 저장소 안에서만 동작한다(자동으로 `git init`하지 않음). 이미 `.junees/features.json`이 있거나, 작업 트리에 커밋하지 않은 변경이나 추적하지 않는 파일이 있으면(`.junees/`는 제외) 오류. 초기화 세션이 목표를 검증 가능한 기능 목록으로 쪼개 `.junees/features.json`과 `.junees/progress.md`를 쓰고(필요하면 `.junees/init.sh`), 하네스가 목록을 검사한다. 이때 verify 명령을 하나씩 실행해서 아직 아무것도 만들지 않았는데 이미 통과하는 것(`true`, 테스트가 하나도 걸리지 않는 필터 등)도 거절한다. 틀리면 오류 내용을 같은 세션에 보내 한 번 고치게 하고, 그래도 틀리면(또는 세션이 실패하면) 커밋하지 않고 오류로 끝나며, 거절된 목록은 `.junees/features.rejected.json`(git 제외)으로 옮겨서 `task init`을 다시 할 수 있게 한다. 통과하면 커밋한다(커밋 방식은 아래 체크포인트와 같음). 커밋 직전에 흔한 빌드 산출물 폴더(`/target/`, `node_modules/`, `/dist/`, `/build/`, `.venv/`, `__pycache__/`) 중 저장소 루트의 그 폴더를 git이 아직 무시하지 않고(어느 `.gitignore`, `.git/info/exclude`, 전역 제외 목록이든) 추적 중인 파일도 없는 것을 저장소 루트 `.gitignore`에 더해 init 커밋에 함께 넣는다. 빌드 도구가 프로젝트 루트에 만드는 `target`, `dist`, `build`는 앞에 `/`를 붙여 루트에만 적용하므로, 더 깊은 곳의 같은 이름 소스 폴더(Java 패키지 `build`, `web/dist/` 등)는 무시되지 않는다. 계획이 실패하면 `.gitignore`는 건드리지 않는다(init 커밋 자체가 실패하면, 예를 들어 git `user.name`이 없으면, 더한 줄이 남으므로 init 세션이 남긴 다른 파일과 함께 치운 뒤 다시 실행한다). `dist/`처럼 일부러 커밋하는 산출물 폴더를 쓰는 프로젝트(JavaScript GitHub Action 등)는 init 출력의 `build output kept out of git` 줄을 보고 그 줄을 `.gitignore`에서 빼면 된다.
- `features.json`은 `[{"id": "F1", "description": "...", "verify": "<셸 명령>", "passes": false}]` 형식이다. 비어 있지 않은 배열, 유일한 id, 비어 있지 않은 verify가 조건이고 새 작업은 전부 `passes: false`여야 한다. 다른 필드(steps 등)는 그대로 둔다.
- `task run [--max-sessions N] [--max-tokens N] [--max-time 2h]`(세션 수 기본 20, 예산은 아래): 이전 실행이 크래시로 끝났으면 먼저 복구하고(아래), 작업 트리가 깨끗해야 시작한다(아니면 커밋하거나 `git stash -u` 하라는 오류. 하네스 커밋이 빼는 새 파일만 있으면 경고하고 시작한다, 아래 체크포인트 참고). 시작할 때 `features.json`에서 `passes: true`인 기능의 verify를 모두 다시 실행해서, 실패하는 것은 통과로 치지 않는다(경고와 `progress.md` 한 줄). 실패한 init이나 하네스가 되돌리기 전에 죽은 세션이 남긴 목록, 나중 변경으로 깨진 기능을 걸러 내기 위해서다. 재검증이 바꾼 것(되돌린 통과, 하네스 형식으로 다시 쓴 `features.json`, verify 명령이 남긴 파일)은 세션을 시작하기 전에 `junees: recheck passing features at run start`로 커밋한다(체크포인트와 같은 방식). 그래서 세션 없이 정지해도(이미 넘은 예산, 재검증 직후 시그널, 할 일 없음) 작업 트리가 깨끗하게 남고 다음 실행이 시작된다. 세션마다 새 에이전트(새 컨텍스트, 세션 기록은 `$JUNEES_HOME/sessions/`에 따로 남음)가 기능 목록과 통과 여부, `progress.md` 끝부분(4000자), `git log --oneline -20`을 system 프롬프트로 받고, 안 끝난 기능 하나를 구현, 검증, 커밋하고 `progress.md`를 갱신한다.
- 기능 통과는 `mark_feature_passing {id}` 도구로만 바뀐다. 하네스가 그 기능의 verify 명령을 저장소 루트에서 bash 도구와 같은 방식(출력 제한, API 키 변수 제거, 타임아웃 600초)으로, `set -o pipefail`을 앞에 붙여 실행하고(`cargo test 2>&1 | tail`도 테스트가 실패하면 실패) 종료 코드 0일 때만 `passes: true`로 저장한다. 실패하면 출력과 함께 오류 결과를 돌려준다.
- 하네스는 기능 목록의 권위 있는 사본을 들고 있다. 세션이 `features.json`을 직접 고치면(지우거나 `passes`를 바꾸는 것 포함) 세션이 끝날 때 사본으로 되돌려 쓰고 경고와 `progress.md` 한 줄을 남긴다. 서식만 다른 것은 편집으로 치지 않는다.
- 세션이 끝나면 하네스가 `progress.md`에 `- [junees] session N: 통과 수/전체, 새로 통과한 기능, 토큰` 한 줄을 붙이고, 작업 트리가 더러우면 `junees: checkpoint after session N`으로 커밋한다(하네스 줄이 붙으므로 사실상 세션마다 하나). 하네스의 커밋(init, 재검증, 크래시 복구, 체크포인트)은 `git add -A`에서 이름이 비밀 파일 같은 새 파일(git이 추적 중인 파일을 옮기거나 복사한 것으로 보는 파일도 포함. 예: `.env.example`을 `.env`로 옮겨 값을 채운 경우)(`.env*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa`/`id_ed25519` 등 ssh 키, `.netrc`)과 1 MiB가 넘는 새 파일(verify가 남긴 빌드 결과물 등. 추적 중인 큰 파일을 옮기기만 한 것은 새 파일로 치지 않는다)을 빼고, 파일마다 이유를 붙여 경고한다. 이미 추적 중인 파일은 크기와 이름에 상관없이 그대로 커밋한다. 이렇게 빠지는 파일(추적하지 않는 비밀 같은 파일, 1 MiB가 넘는 파일)만 남아 있으면 다음 실행은 그 파일들을 `not committed: ...` 경고로 알리고 그대로 시작한다. 하네스 커밋이 그 파일들을 계속 빼므로 사용자의 작업이 커밋에 쓸려 들어갈 일이 없고, 커밋해야 할 큰 파일(lock 파일, 픽스처 등) 하나 때문에 무인 실행이 멈추지 않게 하기 위해서다(직접 커밋하거나 `.gitignore`에 넣거나 치우면 경고가 사라진다). 다른 변경이 함께 있어 실행을 거절할 때는 거절 메시지에 그런 파일을 세 개까지 이유와 함께 보여 주고 나머지는 개수로 알린다. 세션이 `.junees/`나 `.junees/.gitignore`를 지워도 커밋 전에 다시 만든다.
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

### 스트리밍 출력

모델 응답은 SSE 스트리밍(`stream: true`)으로 받는다. stdout이 터미널이면 텍스트를 도착하는 대로 찍는다(대화 모드, `-p`, `task` 세션 모두). 응답이 끝난 뒤 다시 찍지 않으므로 텍스트는 한 번만 나온다. stdout이 터미널이 아니면(`junees -p ... > answer.txt`, 파이프) stdout이 곧 결과이므로, 모델 호출 하나의 텍스트를 호출이 끝날 때까지 모아 두었다가 한 번에 찍는다. 그래서 재시도로 버려진 시도의 조각은 결과에 남지 않고, 출력은 `JUNEES_STREAM=0`일 때와 같다(스트리밍의 대기 제한 이점은 그대로). 취소나 오류로 호출이 중간에 끝나면 받은 데까지 찍고 줄을 끝낸다. thinking 블록은 지금처럼 내용 없이 `(thinking...)`만, 도구 호출과 결과도 지금처럼 stderr에 나온다. `JUNEES_STREAM=0`이면 응답을 한 번에 받아 끝난 뒤 찍는다. 압축 요약 호출은 늘 한 번에 받는다.

- 조립: 이벤트(`message_start`, `content_block_start/delta/stop`, `message_delta`, `message_stop`)로 스트리밍하지 않은 응답과 같은 값을 만든다. 블록은 index 순서, 도구 입력은 `input_json_delta` 조각을 이어 블록이 끝난 뒤 JSON으로 읽고, thinking 블록은 `thinking_delta`와 `signature_delta`로 signature까지 채운다. 모르는 블록 종류(서버 도구 결과 등)는 시작 이벤트의 원본 JSON 그대로 남는다. `message_delta`의 usage는 누적값이라 더하지 않고 덮어쓴다(입력 쪽 값이 없으면 `message_start` 값 유지). `ping`과 모르는 이벤트, 모르는 delta 종류는 무시한다(API 버전 정책상 새 종류가 생길 수 있다). 출력 한도(`max_tokens`)에 걸려 도구 입력 JSON이 잘렸으면 입력을 `{}`로 둔다(어차피 실행하지 않는다).
- 재시도: 스트림이 끊기거나(연결 끊김, `message_stop` 전에 끝남), 재시도할 만한 `error` 이벤트(`overloaded_error` 등, 같은 오류의 HTTP 상태로 판단: 429, 5xx, 529)가 오거나, 다음 이벤트가 300초 동안 오지 않으면 기존 정책(최대 4번, 0.5초부터 두 배 대기)대로 처음부터 다시 요청한다. 이미 텍스트나 `(thinking...)`을 보여 줬으면 줄을 끝내고 stderr에 `(retrying)`을 찍은 뒤 다시 받은 응답 전체를 찍는다(터미널에서는 끊긴 조각이 화면에 남고, 터미널이 아니면 조각은 버린다). 부분 응답은 기록하지 않는다. 끊긴 시도의 usage도 버리고 성공한 시도의 usage만 센다. 끊기기 전까지 만든 출력이 과금되는지는 문서에 나와 있지 않아서, 재시도가 있었던 호출은 대화의 `/usage`와 task 예산이 실제보다 조금 적게 셀 수 있다(호출당 재시도는 많아야 3번). `invalid_request_error` 같은 오류 이벤트는 같은 상태의 HTTP 오류처럼 바로 실패한다(컨텍스트 초과도 같은 방식으로 감지).
- 시간 제한: 스트리밍 호출에는 전체 시간 제한이 없고, 응답 시작과 다음 이벤트(`ping` 포함)까지의 대기만 300초로 제한한다. 문서는 오래 걸리는 요청에 스트리밍을 권하고, 도구 입력은 값 하나를 다 만든 뒤에 보내므로 큰 파일을 쓸 때 이벤트 사이가 길 수 있다. 300초는 예전 전체 제한과 같아서, 예전에 끝나던 호출은 여기서 시간 초과되지 않는다. 이벤트가 오는 한 길게 이어지는 것은 대화와 task 세션의 모델 호출뿐이다. 스트리밍하지 않는 호출(컨텍스트 압축의 요약 호출, `JUNEES_STREAM=0`)은 지금처럼 요청 전체가 300초로 제한된다.
- 취소: Ctrl-C/SIGTERM으로 턴을 취소하면 스트림 연결을 바로 닫는다. 찍던 줄은 끝내고, 응답은 기록하지 않는다.
- 스트리밍을 요청했는데 서버(프록시 등)가 `text/event-stream`이 아닌 JSON 응답 전체를 주면 그대로 읽는다.

### 확장 사고

`JUNEES_THINKING_BUDGET`을 1024 이상으로 주면 모델 호출마다 `thinking: {"type": "enabled", "budget_tokens": N}`을 보낸다(`task` 세션 포함, 압축 요약 호출 제외). 사고 토큰도 출력 한도에 들어가므로 예산은 `JUNEES_MAX_TOKENS`보다 작아야 한다.

- 응답의 `thinking`, `redacted_thinking` 블록은 내용 대신 `(thinking...)` 한 줄로만 알린다.
- 이 블록들(그리고 서버 도구 블록 같은, Junees가 직접 다루지 않는 모든 블록)은 받은 JSON 그대로 기록과 세션 파일에 남고, 다음 요청에 그대로 다시 보낸다. 도구 호출 턴의 thinking 블록은 `signature`까지 고치지 않고 돌려보내야 API가 받아 준다.
- 도구 결과 비우기와 압축은 기록 전체의 `thinking`, `redacted_thinking` 블록을 지운다(텍스트와 도구 호출은 남긴다). Claude 5.1 이후 모델은 돌려받은 thinking 블록 앞(시스템 프롬프트, 도구, 앞선 메시지)이 그 블록을 만들 때와 같은지 확인하고, 달라졌으면 그 블록과 뒤의 모든 thinking 블록을 거절한다(400). 비우기와 압축은 앞부분을 고치므로, 남길 수 있는 thinking 블록이 없다. 그 뒤에 나온 thinking 블록은 다음 변경 때까지 그대로 보낸다. Claude 5 모델은 `JUNEES_THINKING_BUDGET=0`이어도 기본으로 생각하므로 이 규칙이 늘 적용된다.
- 예산을 켠 수동 모드는 예외다. 이 방식을 받는 모델(Claude 4.6 이하)은 앞부분 검사를 하지 않는 대신, 진행 중인 턴이 thinking 블록으로 시작해야 하고 thinking은 턴의 첫 응답에만 나온다. 그래서 비우기와 압축이 thinking 블록을 지우지 않고(기록 항목에 `keep_thinking: true`), 입력이 하나뿐인 턴 안에서 압축할 때는 꼬리를 남기지 않고 마지막 도구 결과까지 턴 전체를 요약해 `user(요약 + 원래 입력)` 하나로 만든다. 모델은 그 턴을 thinking부터 다시 시작한다. 사용자 입력 앞에서 자르는 경우는 같다.
- 응답이 사고 중에 출력 한도(`max_tokens`)에 걸려 thinking 블록만 남으면 그 응답은 기록하지 않고, `JUNEES_MAX_TOKENS`를 올리거나 `JUNEES_THINKING_BUDGET`을 낮추라는 경고를 띄운다(기록은 입력으로 끝나 다음 입력이 거기에 붙는다). 한도에 걸리지 않았는데 thinking만 온 응답도 기록하지 않고, 모델이 생각만 하고 답하지 않았다는 경고를 띄운다.
- `type: "enabled"` 방식은 Claude 4.6 모델(기본 `claude-sonnet-4-6`)에서는 동작하지만 폐기 예정이고, 4.7 이후 모델은 400으로 거절한다. 그런 모델에서는 켜지 않는다. 켜면 오류 메시지 끝에 `JUNEES_THINKING_BUDGET`을 0으로 하라는 안내가 붙는다(adaptive thinking은 아직 지원하지 않음. Claude 5 모델은 설정 없이도 adaptive로 생각한다).

### 웹 검색

`JUNEES_WEB_SEARCH=on`이면 모델이 Anthropic의 서버 도구 `web_search`로 웹을 검색할 수 있다(대화 모드, `-p`, `task` 세션, `api` 백엔드). 검색은 API가 실행하고 Junees는 실행하지 않는다. 기본은 꺼짐이다. 검색은 토큰과 별도로 1000회당 10달러가 청구되고, 검색 결과도 입력 토큰으로 계속 쌓이며, 조직 관리자가 Console에서 웹 검색을 꺼 두었으면 요청 전체가 400으로 거절되기 때문이다.

- 요청: 도구 목록 끝(Junees 도구 다음)에 `{"type": "web_search_20250305", "name": "web_search", "max_uses": N}`을 넣고, 시스템 프롬프트 뒤에 "최신 정보가 필요하면 web_search로 찾고 출처를 밝히라"는 한 줄을 덧붙인다. 압축 요약 호출에는 넣지 않는다. 기본 버전을 쓰는 이유: `web_search_20260209` 이후 버전은 기본으로 코드 실행으로 결과를 거르므로(dynamic filtering) `pause_turn` 때 컨테이너 id를 돌려보내야 하고 블록 종류가 늘며, ZDR 대상이 아니고 일부 플랫폼에 없다. `allowed_callers: ["direct"]`로 쓰면 기본 버전과 같은 검색이다.
- `max_uses`(`JUNEES_WEB_SEARCH_MAX_USES`, 기본 5)는 한 턴의 검색 횟수 상한이다. 넘으면 API가 `max_uses_exceeded` 오류 결과를 주고 모델이 처리한다. `JUNEES_WEB_SEARCH_ALLOWED_DOMAINS` 또는 `JUNEES_WEB_SEARCH_BLOCKED_DOMAINS`(쉼표 구분, 둘 다는 안 됨)로 도메인을 제한한다. `https://` 같은 스킴, ASCII가 아닌 글자(비슷한 모양의 키릴 문자 등), 도메인 부분의 `*`는 시작할 때 오류다(경로의 `*`는 된다: `example.com/blog/*`).
- 응답의 `server_tool_use`, `web_search_tool_result`(암호화된 `encrypted_content` 포함)는 원본 그대로 기록과 세션 파일에 남기고 다음 요청에 그대로 보낸다(문서상 빠지거나 바뀌면 400). 클라이언트 도구로 실행하지 않고, 검색 결과의 오류 코드(`max_uses_exceeded`, `too_many_requests` 등)도 모델이 처리하도록 그대로 둔다. 텍스트 블록의 `citations`도 원본 그대로 보존해 다시 보낸다(`encrypted_index`를 돌려보내야 한다).
- `stop_reason: "pause_turn"`(API의 서버 쪽 도구 반복이 한도에 걸려 턴을 멈춤)이면 그 응답을 기록하고, 사용자 메시지를 넣지 않고 같은 대화로 다시 요청해 이어 가게 한다. 연속 5번(`MAX_PAUSES`)까지이고, 넘으면 경고를 띄우고 멈춘다. 다음 입력은 멈춘 응답 뒤에 새 user 메시지로 붙는다. 이어 가는 요청도 `JUNEES_MAX_TURNS`에 센다.
- 출력: 검색은 stderr에 `-> web_search {"query": ...} (run by the API)`로 응답이 오는 대로 나온다. 답이 끝나면 인용된 URL을 중복 없이 처음 나온 순서로 stdout에 `Sources:` 목록으로 찍는다(10개까지, 나머지는 개수). 출처도 답의 일부로 보므로 `-p` 결과를 파일이나 파이프로 받으면 이 목록이 답 뒤에 붙는다.
- 사용량: `usage.server_tool_use.web_search_requests`를 세션마다 더해 `/usage`에 `web searches N`으로, `task run`의 실행 로그(run.log) 정지 줄에 `web searches N`으로 보여 준다. 토큰 예산(`--max-tokens`)에는 들어가지 않는다.
- 컨텍스트: 검색한 호출의 사용량은 컨텍스트 크기로 쓰지 않는다. 그 호출의 캐시 읽기에는 API의 검색 반복 단계마다 읽은 양이 모두 들어 있어 실제보다 훨씬 커서, 압축이 너무 일찍 일어난다(context editing 문서의 서버 도구 주의). 검색하지 않은 다음 호출이 다시 잰다. 도구 결과 비우기는 검색 결과 블록을 건드리지 않는다. `encrypted_content`는 바꾸면 400이고, Junees가 대신 넣을 수 있는 유효한 자리표시가 없으며, 블록 짝을 지우면 뒤 텍스트의 인용이 가리킬 곳이 사라진다. 검색 결과는 압축 때 요약으로 정리된다(요약 텍스트에는 검색어와 앞 5개 결과의 제목/URL만 넣는다). 검색을 클라이언트 도구와 같은 응답에서 부르면 검색은 그 도구 결과를 보낸 다음 요청에서 실행되어 결과 블록이 다음 응답에 오는데, 턴 안 압축은 이 둘 사이를 자르지 않는다(검색 호출 없이 결과만 남지 않도록).
- 조직에서 웹 검색이 꺼져 있다는 400이 오면 오류 끝에 `JUNEES_WEB_SEARCH=off`로 하거나 관리자에게 켜 달라는 안내가 붙는다.

### 구독 모드 (claude-code 백엔드)

`task init`과 `task run`(그리고 둘을 합친 `junees task "목표"`, 대화의 `/task`)의 세션을 Junees 자신의 에이전트 대신 공식 Claude Code CLI(`claude`)가 실행하게 할 수 있다. Claude Pro/Max 구독으로 Junees를 쓰는 방법이다. 설정 마법사에서 `1) Claude subscription`을 고르면 설정 파일에 `backend = "claude-code"`가 들어간다. 한 번만 쓰려면 `--backend claude-code`, 환경 변수는 `JUNEES_BACKEND=claude-code`, 대화 안에서는 `/backend claude-code`(기본은 `api`). 이때는 `ANTHROPIC_API_KEY`가 필요 없고 읽지도 않는다. 대화 모드는 아직 구독으로 돌지 않는다(API 키 필요).

로그인은 직접 한다. Junees는 로그인을 대신하지 않고, 구독으로 로그인돼 있는지 `claude auth status`로 확인만 한다(아래).

```sh
claude            # 처음 한 번: Claude Code를 열고
/login            # 그 안에서 구독 계정으로 로그인한 뒤 /exit
# 브라우저 없는 서버라면: claude setup-token 으로 토큰을 만들어
# export CLAUDE_CODE_OAUTH_TOKEN=... 을 자기 셸에 둔다 (Junees는 claude에게 물려줄 뿐 읽지 않는다)
junees setup      # 1) Claude subscription 을 고르면 기본 백엔드가 claude-code
junees task "목표"     # 계획 + 실행 (설정의 백엔드로)
junees task --backend claude-code --max-time 2h   # 설정과 상관없이 이번만
```

왜 이 방식만 지원하나: Anthropic 약관([Legal and compliance](https://code.claude.com/docs/en/legal-and-compliance))은 구독(OAuth) 자격 증명을 다른 도구가 직접 쓰거나 대신 저장, 중계하는 것을 금지하고, 사용자가 **수정하지 않은 Claude Code**에 자기 구독으로 로그인해 쓰는 것은 허용한다. 그래서 Junees는 `claude` 프로그램을 자식 프로세스로 실행하기만 한다. `~/.claude`의 자격 증명이나 `CLAUDE_CODE_OAUTH_TOKEN`을 읽거나 저장하거나 복사하지 않고, Junees의 API 클라이언트가 Claude Code인 척하지도 않는다(헤더, 시스템 프롬프트). 환경에 `CLAUDE_CODE_OAUTH_TOKEN`이 있으면 다른 변수처럼 `claude`에 상속될 뿐이다.

- 시작할 때 `claude --version`이 실행되고 2.1.259 이상인지 확인한다(`--permission-prompts`가 이 버전부터).
- 구독 확인: 시작할 때와 세션마다 그 직전에, 세션과 같은 환경으로 `claude auth status --json`을 `/`에서 실행한다. `authMethod`가 `claude.ai`(`/login`)나 `oauth_token`(`CLAUDE_CODE_OAUTH_TOKEN`)일 때만 진행한다([CLI reference](https://code.claude.com/docs/en/cli-reference)). `none`이거나 명령이 실패하면(로그인 안 됨은 종료 코드 1) 로그인 안내와 함께, `api_key`, `api_key_helper`, `third_party` 등이면 그 방식과 지울 곳을 보여 주며 `error`로 정지하고, 웹훅이 있으면 알린다. 환경 변수는 아래처럼 지우지만 `~/.claude/settings.json`의 `apiKeyHelper`나 `env` 블록, 활성 federation 프로필은 Junees가 지울 수 없는 과금 경로라서 이렇게 막는다. 세션의 명령이 사용자 설정을 바꿀 수 있으므로 세션마다 다시 묻는다. `/`에서 묻는 것은 세션이 읽지 않는 저장소의 `.claude/settings.json`이 결과를 바꾸지 않게 하기 위해서다.
- 세션 중 로그인 실패: Claude Code가 "Not logged in", "Failed to authenticate" 같은 결과를 내거나, 마지막 모델 호출이 `authentication_failed`, `oauth_org_not_allowed`, `account_on_hold` 오류였으면([headless](https://code.claude.com/docs/en/headless)의 `api_retry` 분류) "Claude Code is not signed in ... run `claude` and then /login (or create a token with `claude setup-token`)" 안내와 함께 `error`로 정지한다. 인증 오류 뒤에 다른 오류나 정상 응답이 왔으면 로그인 실패로 보지 않는다.
- 세션 하나는 저장소 루트에서 `claude -p <입력> --output-format stream-json --verbose --append-system-prompt <하네스 규칙과 작업 상태> --permission-mode acceptEdits --permission-prompts none --tools Read,Edit,Write,Glob,Grep,Bash --allowedTools Read,Edit,Write,Glob,Grep,Bash --setting-sources user --strict-mcp-config --max-turns <JUNEES_MAX_TURNS> [--model <JUNEES_CLAUDE_MODEL>] --disallowedTools <규칙...>`이다. 무인 실행이라 확인을 기다리면 안 되므로, 편집과 위 도구는 묻지 않고 허용하고, 그 밖에 확인이 필요한 것은 거절된다(`--permission-prompts none`). `--dangerously-skip-permissions`는 쓰지 않는다. 저장소의 `.claude/settings*.json`과 `.mcp.json`은 읽지 않는다(세션이 써 둔 훅이나 MCP 서버가 다음 세션에서 규칙 밖으로 실행되지 않도록). 사용자 설정(`~/.claude/settings.json`)은 그대로 적용된다. `--bare`는 OAuth 로그인을 읽지 않으므로 쓰지 않는다. 아래의 `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`가 켜져 있으면 Claude Code 2.1.319는 `acceptEdits` 대신 `default` 모드로 돈다(바이너리의 "Permission mode forced to default" 경고). 이때도 `--allowedTools`가 편집을 묻지 않고 허용하고, `.git`, `.claude` 같은 보호 경로 쓰기는 어느 모드에서든 자동 허용되지 않으므로 거절된다([permission modes](https://code.claude.com/docs/en/permission-modes)). 다만 작업 폴더 밖의 파일 편집을 `acceptEdits`처럼 막아 주지는 않는다(Bash가 전부 허용이라 원래도 막히지 않던 것).
- 명령 정책: L4a 정책의 핵심을 Claude Code 권한 규칙으로 옮긴 `--disallowedTools` 목록을 넘긴다. `sudo`/`su`/`doas`/`pkexec`/`runuser`, `shutdown`/`reboot`/`halt`/`poweroff`/`systemctl poweroff` 등, `mkfs*`, `wipefs`, `dd ... of=/dev/...`, `rm -rf`(와 `-fr`, `-r`, `-R` 등) 뒤에 `/`, `~`, `$HOME`, `..`, 강제 push(`--force`, `-f`, `+refspec`. `--force-with-lease`는 허용), 표준 입력에서 스크립트를 읽는 셸(`curl ... | sh`). `JUNEES_DENY_COMMANDS`의 접두사는 `Bash(<접두사> *)`로 더하고, `JUNEES_ALLOW_DANGEROUS=1`이면 기본 규칙은 빠진다. 한계: Claude Code 규칙은 쓰인 그대로의 단어만 맞춘다. `/usr/bin/sudo`, `git -C . push --force`, `bash -c '...'`, 실행 중에 만든 명령, fork bomb, 루트 바로 아래 글롭(`/*`. 규칙의 `*`는 무엇이든 맞으므로 `/tmp/x`까지 막게 되어 넣지 않음)은 막지 못한다. Junees의 bash 정책보다 약하다.
- 환경: [인증 우선순위](https://code.claude.com/docs/en/authentication#authentication-precedence)에서 `/login` 구독보다 앞서는 변수는 `claude`에 넘기지 않는다. 클라우드 제공자 전환(`CLAUDE_CODE_USE_BEDROCK`, `_VERTEX`, `_FOUNDRY`, `_ANTHROPIC_AWS`, `_MANTLE`)과 그 키(`ANTHROPIC_AWS_API_KEY`, `ANTHROPIC_FOUNDRY_API_KEY`, `ANTHROPIC_FOUNDRY_AUTH_TOKEN`), `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, 이름 붙인 프로필과 federation(`ANTHROPIC_PROFILE`, `ANTHROPIC_FEDERATION_RULE_ID`, `ANTHROPIC_ORGANIZATION_ID`)이다([env vars](https://code.claude.com/docs/en/env-vars)). 웹훅 URL, `JUNEES_HIDDEN_ENV`, 저장소 지정 git 변수도 넘기지 않는다.
- 명령에서 토큰 숨기기: `claude`에는 `CLAUDE_CODE_OAUTH_TOKEN`이 필요하지만, Claude Code는 기본으로 자기 환경을 Bash 명령에 그대로 물려준다. 그래서 Junees는 세션마다 Claude Code의 공식 설정 `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1`을 켠다([What the subprocess environment scrub removes](https://code.claude.com/docs/en/env-vars#what-the-subprocess-environment-scrub-removes)). Bash 명령, 훅, stdio MCP 서버의 환경에서 자격 증명을 지우고(Claude Code 자신은 계속 씀), Linux에서는 명령을 별도 PID 네임스페이스에서 돌려 `/proc`로 `claude`의 환경을 읽지도 못하게 한다. Junees는 토큰을 읽지 않고 이 스위치만 넘긴다. 조건과 한계:
  - Linux에서는 bubblewrap(`bwrap`)이 있어야 한다. 없으면 Claude Code 2.1.319가 세션을 시작하지 않는다("bubblewrap is required for subprocess env scrubbing"). Junees는 `PATH`에 `bwrap`이 없으면 시작할 때 경고한다(`sudo apt-get install -y bubblewrap`).
  - 사용자가 `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=0`(또는 `false`, `no`, `off`)을 두면 그대로 넘기고, 명령이 토큰을 볼 수 있다는 경고(`CLAUDE_CODE_SUBPROCESS_ENV_SCRUB is off: ...`)로 시작한다. bubblewrap을 설치할 수 없거나 샌드박스 때문에 명령이 실패할 때만 쓴다.
  - 문서대로 이름이나 값의 모양으로 자격 증명을 알아보므로 완전하지 않다. `GITHUB_TOKEN` 등은 남는다. `/login`이 저장한 `~/.claude/.credentials.json`을 명령에서 숨긴다는 약속은 문서에 없다. 끄면 같은 사용자의 명령이 그 파일과 `/proc/<claude pid>/environ`을 읽을 수 있다.
- 기능 통과: Claude Code에는 `mark_feature_passing`이 없다. 세션 프롬프트는 "기능 하나를 끝내고 verify 명령을 직접 실행해 확인하라, 통과 기록은 하네스가 한다"고 알려 주고, 세션이 끝나면 하네스가 아직 통과하지 않은 기능의 verify를 모두(목록 순서로) 실행해서 exit 0인 것을 기록한다(`verified by the harness: F1`). 세션이 취소됐거나 로그인 실패면 건너뛰고, 시간 예산이 다 되면 남은 verify는 다음 실행으로 미룬다. 기능 목록 되돌리기, 체크포인트 커밋, 정지 조건, 재검증, 크래시 복구, 실행 로그, 알림은 api 백엔드와 같다. init이 거절된 목록을 고치는 것은 같은 세션이 아니라 목표와 문제를 받은 새 init 세션이 한다.
- 토큰과 예산: stream-json의 응답별 usage(같은 응답 id는 한 번만)를 세션 중에 세어 예산을 확인하고(응답마다, 그리고 시간 때문에 1초마다), 넘으면 세션의 명령을 끝내고 Claude Code의 프로세스 그룹에 SIGINT를 보내 턴을 끝내게 한 뒤, 10초 안에 끝나지 않으면 SIGKILL한다(아래 취소와 같은 방식). 응답의 usage를 읽지 못하면(값이 `null` 등) 처음 한 번 경고한다. 그 응답은 예산에 세지 않는다. 세션이 끝나면 `result` 이벤트의 `modelUsage`(모든 모델 합계, 없으면 `usage`)가 더 크면 그것을 누적에 더한다. Claude Code가 추정한 비용(`total_cost_usd`)과 턴 수, Claude Code 세션 id는 `run.log`에 `session N claude-code: ...` 한 줄로 남는다(구독은 이 금액으로 과금되지 않는다). 구독의 사용 한도는 Junees가 알 수 없다.
- 취소: SIGINT/SIGTERM이면 먼저 Claude Code의 자손 프로세스를 모두 끝낸다. Claude Code의 Bash 도구는 명령을 별도 세션(`detached`)으로 띄우므로 프로세스 그룹 신호가 닿지 않는다. 그래서 `/proc`의 부모 관계로 자손을 찾아, 새 프로세스가 더 나오지 않을 때까지 SIGSTOP으로 멈춘 뒤 모두 SIGKILL한다. 그다음 Claude Code의 프로세스 그룹에 SIGINT를 보내고, 유예 뒤에는 자손을 한 번 더 정리하고 SIGKILL한다. 세션을 마무리한 뒤 `interrupted`로 정지한다. 세션이 스스로 끝나면 그 세션이 띄운 백그라운드 프로세스는 bash 도구처럼 그대로 둔다. 한계: 정지 신호 전에 `claude`가 스스로 죽은 경우(크래시)에는 그 명령이 init의 자식이 되어 찾을 수 없으므로 남는다.
- 출력: 세션 중에 stream-json 이벤트를 다른 백엔드와 같은 형식(텍스트, `-> Read {...}`, `<- Read ok: ...`)으로 보여 준다. 원본은 `.junees/sessions/init.jsonl`, `init-fix.jsonl`, `session-N.jsonl`에 저장한다(0600, git 제외). Junees의 세션 기록(`$JUNEES_HOME/sessions/`)은 남지 않고, Claude Code가 자기 대화를 따로 저장하므로 `claude --resume <id>`로 열어 볼 수 있다.
- 설정: `JUNEES_CLAUDE_BIN`(설정 파일 `claude_bin`, 기본 `PATH`의 `claude`), `JUNEES_CLAUDE_MODEL`(`--model`로 전달, 없으면 Claude Code의 기본), `JUNEES_MAX_TURNS`(`--max-turns`). `JUNEES_MODEL`, `JUNEES_MAX_TOKENS`, 컨텍스트 관리 설정은 이 백엔드에서 쓰지 않는다(압축은 Claude Code가 한다).

### 컨텍스트 관리

긴 작업에서도 대화가 모델의 컨텍스트 창을 넘지 않게 하고, 반복되는 앞부분은 캐시한다.

- 프롬프트 캐싱: 매 요청에서 도구 목록의 마지막 도구(웹 검색이 켜졌으면 `web_search`), 시스템 프롬프트의 마지막 블록, 마지막 메시지의 마지막 일반 블록(텍스트, 도구 호출, 도구 결과)에 `cache_control: ephemeral`을 붙인다. thinking 블록에는 붙일 수 없어서, 그런 블록은 건너뛰고 그 앞의 일반 블록에 붙인다. 한 번만 보내는 요약 호출에는 붙이지 않는다(캐시 쓰기는 일반 입력보다 비싸다). 응답의 캐시 쓰기/읽기 토큰은 `/usage`에 따로 나온다.
- 컨텍스트 크기: 마지막 응답의 `input_tokens + cache_creation_input_tokens + cache_read_input_tokens + output_tokens`(웹 검색을 한 응답은 제외, 위 웹 검색 참고). 세션 기록의 `usage` 항목에서 복원되므로 이어 가도 유지된다. 한도는 `JUNEES_CONTEXT_WINDOW` x `JUNEES_COMPACT_THRESHOLD`(기본 200000 x 0.8 = 160000).
- 1차 대응(오래된 도구 결과 비우기): 모델 호출 직전에 크기가 한도 이상이면, 도구 결과가 든 메시지 중 최근 3개를 뺀 이전 것들의 출력을 `[tool output cleared to save context]`로 바꾼다. `tool_use_id`와 `is_error`는 그대로라 호출/결과 짝이 유지된다. 기록 항목은 `clear_tool_results { upto }`(수동 thinking 모드면 `keep_thinking: true`, 위 확장 사고 참고).
- 2차 대응(압축): 비울 것이 없거나, 비운 뒤 다음 응답에서도 한도 이상이면 앞부분을 같은 모델로 요약한다. 꼬리는 최근 두 사용자 입력(안 되면 마지막 입력)부터 끝까지이고, 도구 결과가 든 메시지나 도구 호출 바로 뒤에서는 자르지 않는다. 입력이 첫 메시지 하나뿐인 긴 턴(`-p` 한 번으로 끝까지 가는 작업 등)은 그 턴 안에서 자른다: 도구 결과만 든 user 메시지 바로 다음 assistant 메시지(앞의 호출이 모두 결과를 받은 지점)에서, 최근 도구 왕복 두 번(안 되면 한 번)을 남긴다. 이때는 요약과 그 턴의 원래 입력(원문 그대로)을 담은 user 메시지를 꼬리 앞에 새로 넣는다. 사용자 입력에서 자를 수 있으면 그쪽을 먼저 쓴다. 요약 호출은 도구 없이, 사고(thinking) 없이, 요약 전용 시스템 프롬프트로, 앞부분을 텍스트로 옮긴 user 메시지 하나를 보낸다(`max_tokens` 4096). 텍스트로 옮길 때 thinking 블록은 빼고, 웹 검색은 `[assistant: web search: <검색어>]`, 검색 결과는 앞 5개의 제목과 URL(`[assistant: web search results: 제목 (URL); ...; 15 more]`, 내용은 넣지 않음), 다른 원본 블록은 `[assistant: code_execution_tool_result block]`처럼 종류만 적는다. 요약은 꼬리 첫 user 메시지 맨 앞에 `[Summary of the earlier conversation]` 블록으로 들어가 역할 교대가 유지된다. 기록 항목은 `compact { summary, keep_from }`이고, 요약 호출의 사용량은 `summary_usage`로 남아 `/usage`에는 더해지지만 컨텍스트 크기에는 쓰이지 않는다. 압축하면 `(compacted the conversation: N messages -> M)` 한 줄이 나온다.
- 요약 호출이 실패하거나, 빈 요약이 오거나, 요약이 출력 한도(`max_tokens`)에 걸려 잘리면 경고만 띄우고 기록은 그대로 둔다(요약 호출 사용량은 더한다).
- `/compact`는 한도와 상관없이 지금 압축한다.
- API가 컨텍스트 초과(400 "prompt is too long")로 거절하면, 크기 추적과 상관없이 비우기와 압축을 하고, 이어서 최근 3개를 포함한 모든 도구 결과도 비운다(한 응답의 병렬 `read_file` 결과처럼 최근 결과가 원인일 수 있어서). 줄어든 게 있으면 한 번만 다시 보내고, 그래도 실패하면 오류로 끝난다.

`JUNEES_HOME` 말고는 모두 설정 파일 키로도 줄 수 있고(위 설정 파일 절), 둘 다 있으면 환경 변수가 이긴다.

| 환경 변수 | 기본값 | 설명 |
|---|---|---|
| `ANTHROPIC_API_KEY` | (필수, 또는 설정 파일 `api_key`) | Anthropic API 키 |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | API 주소 |
| `JUNEES_MODEL` | `claude-sonnet-4-6` | 모델 |
| `JUNEES_MAX_TOKENS` | `8192` | 모델 호출 1회의 최대 출력 토큰 |
| `JUNEES_MAX_TURNS` | `50` | 입력 하나당 최대 모델 호출 횟수 |
| `JUNEES_CONTEXT_WINDOW` | `200000` | 모델의 컨텍스트 창 크기(토큰) |
| `JUNEES_COMPACT_THRESHOLD` | `0.8` | 컨텍스트 창 중 이 비율부터 도구 결과 비우기/압축 (0 초과 1 이하) |
| `JUNEES_THINKING_BUDGET` | `0` | 확장 사고(extended thinking) 토큰 예산. `0`이면 끔. 켜려면 1024 이상이고 `JUNEES_MAX_TOKENS`보다 작아야 한다(아니면 시작할 때 오류) |
| `JUNEES_HOME` | `~/.junees` | 세션 기록 등 상태를 두는 폴더 |
| `JUNEES_DENY_COMMANDS` | (없음) | `bash`에서 추가로 막을 명령 접두사, 쉼표 구분 (예: `docker rm,kubectl delete`) |
| `JUNEES_ALLOW_DANGEROUS` | `0` | `1`이면 기본 명령 차단 규칙을 끈다 (사용자 책임) |
| `JUNEES_APPROVAL` | `risky` | 대화 모드에서 실행 전에 물을 도구 호출: `off`, `risky`(위험 신호가 있는 명령, 작업 폴더 밖 쓰기), `all`(모든 `bash`/`write_file`/`edit_file`) |
| `JUNEES_HIDDEN_ENV` | (없음) | `bash`에서 추가로 숨길 환경 변수 이름, 쉼표 구분 (예: `DB_PASSWORD,GITHUB_TOKEN`) |
| `JUNEES_NOTIFY_WEBHOOK` | (없음) | `task run`이 멈출 때 알림을 보낼 디스코드 웹훅 URL (https만; 평문 http는 localhost와 루프백 주소에만 허용) |
| `JUNEES_NOTIFY_EACH_SESSION` | `0` | `1`이면 작업 세션마다 한 줄 알림도 보낸다 |
| `JUNEES_TASK_MAX_TOKENS` | (없음) | `task run --max-tokens`의 기본값 (작업 전체 누적 토큰) |
| `JUNEES_TASK_MAX_TIME` | (없음) | `task run --max-time`의 기본값 (이번 실행 시간, 예: `2h`, `1h30m`) |
| `JUNEES_STREAM` | `1` | `0`이면 스트리밍을 끄고 응답을 한 번에 받아 찍는다 |
| `JUNEES_WEB_SEARCH` | `0` | `on`/`1`이면 API의 웹 검색 도구를 켠다 (별도 과금, 위 "웹 검색") |
| `JUNEES_WEB_SEARCH_MAX_USES` | `5` | 한 턴의 웹 검색 횟수 상한 (`max_uses`) |
| `JUNEES_WEB_SEARCH_ALLOWED_DOMAINS` | (없음) | 이 도메인에서만 검색, 쉼표 구분 (예: `docs.rs,example.com/blog`) |
| `JUNEES_WEB_SEARCH_BLOCKED_DOMAINS` | (없음) | 이 도메인은 검색하지 않음, 쉼표 구분. `ALLOWED`와 함께 쓸 수 없다 |

기본 도구는 `bash`, `read_file`, `write_file`, `edit_file` 네 가지다(`task` 작업 세션도 같은 네 가지에 `mark_feature_passing`을 더해 쓴다). 대화 모드는 위험할 수 있는 호출을 실행하기 전에 묻는다(아래 "실행 전 승인"). `-p`와 `task` 모드는 묻지 않고 명령 정책만 적용하므로 믿을 수 있는 폴더에서만 쓴다. `bash`에는 `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `JUNEES_NOTIFY_WEBHOOK`과 `JUNEES_HIDDEN_ENV`에 적은 변수가 보이지 않는다. 리눅스에서는 junees가 시작할 때 자기 프로세스를 dump 불가(`PR_SET_DUMPABLE` 0)로 만들어, 같은 사용자의 프로세스도 `/proc/<junees pid>/environ`과 `mem`을 읽지 못한다. 그래서 `bash`가 `/proc/$PPID/environ`처럼 부모를 거슬러 올라가도 이 변수들을 읽을 수 없다. 자식 프로세스는 exec할 때 다시 보통 프로세스가 되므로 도구 동작은 그대로이고, junees 자신의 코어 덤프만 남지 않는다.

`edit_file {path, old_string, new_string, replace_all?}`은 파일의 일부만 고치는 도구다. 큰 파일을 고칠 때 `write_file`로 전체를 다시 쓰지 않아도 되므로 출력 토큰이 줄고, 긴 출력이 `max_tokens`에 잘리는 일도 줄어든다. 시스템 프롬프트에도 "부분 수정은 `edit_file`, 새 파일이나 전체 교체는 `write_file`"이라고 적혀 있다.

- `old_string`은 공백과 들여쓰기까지 정확히 같아야 하고, 파일에서 정확히 한 번 나와야 한다(겹치는 위치도 따로 센다. `aaa`에서 `aa`는 두 번). 여러 번 나오면 횟수와 줄 번호(최대 20개)를 오류로 돌려주고 아무것도 바꾸지 않는다. 모두 바꾸려면 `replace_all: true`. 다만 겹치는 위치는 앞에서부터 바꾸므로 이미 바꾼 곳과 겹치는 위치는 건너뛴다(`aaaa`에서 `aa`는 세 번 나오지만 두 번 바뀐다). 겹치는 위치가 있으면 여러 번 나온다는 오류에 실제로 바뀔 개수를 적는다.
- 찾지 못하면 `old_string`의 첫 줄(공백 제외)과 가장 비슷한 줄을 최대 3개, 줄 번호와 함께 힌트로 보여 준다(문자 2-gram 유사도 0.5 이상).
- 빈 `old_string`, `old_string`과 같은 `new_string`은 오류다. 대상은 이미 있는 일반 파일이고 10 MB 이하(`read_file`과 같은 한도), UTF-8이어야 한다. 심볼릭 링크는 가리키는 파일을 고친다.
- 줄바꿈 보존: `read_file`이 보여 준 대로 `\n`으로 쓴 `old_string`이 그대로는 없으면 `\r\n`으로 바꿔 다시 찾는다(대부분 LF인 파일의 CRLF 줄도). 두 종류가 섞인 부분에 걸친 `old_string`은 찾지 못하며, 파일에 두 종류가 섞여 있으면 찾지 못했다는 오류에 그 사실을 적는다. `new_string`의 줄바꿈은 파일에서 일치한 부분을 따른다. 일치한 부분의 줄바꿈이 모두 `\r\n`이면(`\n`으로 시작하는 `old_string`이 `\r\n`의 `\n`에서 일치한 경우 포함) `new_string`의 `\n`을 `\r\n`으로 쓰고, 일치한 부분에 줄바꿈이 없으면 파일 대부분의 줄바꿈을 따른다. `replace_all`이면 일치한 곳마다 따로 정한다. 원래 파일이 개행으로 끝났는데 고친 결과가 개행 없이 끝나면 개행을 다시 붙인다(결과에 `kept the final newline` 표시).
- 같은 폴더의 임시 파일(`.<이름>.junees-edit-<pid>-<번호>.tmp`)에 원래 권한으로 쓰고 `fsync`한 뒤 `rename`으로 바꿔 넣으므로 중간에 죽어도 반쯤 쓴 파일이 남지 않는다. 쓰기, 권한 설정, `fsync`, `rename` 중 하나라도 실패하면(디스크가 가득 찬 경우 등) 임시 파일을 지우고 오류를 돌려주며 원본은 그대로다. 이 과정은 한 blocking 스레드에서 끝까지 실행되므로, 편집 중에 턴이 취소돼도 임시 파일이 작업 트리에 남지 않는다. 소유자, 하드 링크, 확장 속성(xattr)과 ACL은 유지되지 않는다(새 inode). 임시 파일 이름이 255바이트를 넘지 않도록 긴 파일 이름은 임시 이름에서 줄인다. 그래서 파일에 쓸 수 있어도 폴더에 쓸 수 없으면 고칠 수 없고, 이때 오류에 폴더를 밝히고 `write_file`(제자리 쓰기)을 안내한다.
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
- 설정 파일(`$JUNEES_HOME/config.toml`, API 키를 담는다)을 이름으로 가리키는 명령: 단어나 리다이렉트 대상(`<`, `>`, `>>`), `--file=경로`/`if=경로`처럼 `=` 뒤의 값이 그 파일이면 막는다. 절대 경로, `~/.junees/config.toml`, `$HOME`/`${HOME}`, `$JUNEES_HOME`/`${JUNEES_HOME}`으로 시작하는 경로, `..`와 심볼릭 링크를 거친 경로, 현재 폴더(같은 스크립트에서 앞서 `cd`/`pushd`한 곳)에서의 상대 경로(`cd ~/.junees && cat config.toml`)를 알아본다. 따옴표 안 단어도 경로로 본다(`echo "~/.junees/config.toml"`도 막힌다). `/proc/self/cwd/...`는 그 시점의 현재 폴더로 푼다. 글롭(`*`, `?`, `[...]`)과 중괄호(`{a,b}`)도 펼쳐서, 설정 파일과 맞을 수 있으면 막는다(`cat ~/.junees/*`, `cat ~/.junees/conf*`, `cat ~/.junees/{config.toml,x}`, `cat ~/.jun*/config.toml`). 안전한 쪽으로 기울게 `*`는 점으로 시작하는 이름에도 맞는다고 본다.
- 설정 파일이 든 폴더(`$JUNEES_HOME`, 기본 `~/.junees`) 자체를 가리키는 명령: 폴더를 통째로 복사하거나 묶거나 옮기면 파일 이름을 쓰지 않고도 키를 읽거나 설정을 바꿔치기할 수 있으므로(`grep -r api_key ~/.junees`, `cp -r ~/.junees /tmp/j`, `tar -C ~/.junees ...`, `mv ~/.junees /tmp/old && mv /tmp/evil ~/.junees`, `cd ~/.junees && tar -cf - .`) 막는다. 이름과 메타데이터만 보는 `cd`, `pushd`, `ls`, `stat`, `du`, `test`/`[`, `realpath`, `readlink`, `echo`는 허용한다. 폴더를 변수에 담는 대입(`d=~/.junees`)도 막힌다. 그 안의 세션 폴더와 파일(`~/.junees/sessions/...`)은 그대로 쓸 수 있다.
- 위 두 규칙은 비밀을 지키는 것이라 `JUNEES_ALLOW_DANGEROUS=1`이어도 적용된다. 차단 목록일 뿐 샌드박스가 아니다: 다른 변수에 담은 경로(`d=~; cat $d/.junees/config.toml`), 실행 중에 만든 경로(`cat $(echo ~/.junees)/config.toml`), 더 위 폴더를 통째로 다루는 명령(`grep -r api_key ~`, `cp -r ~ /tmp/h`. 막으면 정상 작업까지 너무 많이 막힌다), 경로 없이 현재 폴더를 쓰는 명령(`cd ~/.junees && grep -r api_key`), 파일로 써 둔 스크립트, 판정과 실제 열기 사이에 링크를 바꿔치기하는 경쟁은 막지 못한다

검사 방식:

- 명령을 셸처럼 대략 나눠서 `;`, `&&`, `||`, `|`, `&`, 줄바꿈, 괄호로 이어진 각 부분, `$(...)`과 백틱 안, `sh -c '...'`, `eval`과 `env -S` 인자까지 검사한다. 셸이 표준 입력으로 읽는 스크립트도 검사한다: `bash <<'EOF'`의 here-document 본문, `bash <<< '...'`, `echo`/`printf`/here-document를 받은 `cat`을 파이프로 넘긴 `... | sh`. 앞에 붙은 `FOO=1`, `env`, `nohup`, `timeout 5`, `nice`, `setsid`, `stdbuf`, `ionice`, `chrt`, `taskset`, `busybox`, `command`, `then`/`do`, `function 이름` 같은 키워드와 래퍼는 건너뛰고 실제 실행되는 프로그램을 본다. `$'...'` 문자열은 이스케이프를 풀어서 본다. 프로그램 이름은 경로를 뗀 이름으로 비교한다(`/sbin/reboot`도 차단).
- 따옴표 안 문자열, 주석, 셸이 아닌 명령(`cat > notes.md <<'EOF'`, `python3 - <<'EOF'`)이 받는 here-document 본문은 명령으로 보지 않는다. `echo "rm -rf /"`, `grep shutdown log.txt`, `git commit -m "git push --force"`는 허용된다.
- 실행 중에 만들어지는 단어(`rm -rf $(echo /)`, 변수에 담긴 경로)는 알 수 없으므로 막지 못한다. `$(...)`, `sh -c`, `eval`이 합쳐서 8단계보다 깊게 중첩되면 검사하지 않고 막는다.
- `JUNEES_DENY_COMMANDS`의 접두사는 단어 단위로 비교한다. `docker rm`은 `docker rm -f web`을 막지만 `docker rmi`는 막지 않는다. 이 규칙은 `JUNEES_ALLOW_DANGEROUS=1`이어도 적용된다.
- `JUNEES_ALLOW_DANGEROUS`는 `1`/`true`/`yes`/`on` 또는 `0`/`false`/`no`/`off`만 받고, 다른 값이면 시작할 때 오류로 멈춘다.

### 실행 전 승인

대화 모드에서는 위험할 수 있는 도구 호출을 실행하기 전에 터미널에서 묻는다. 묻는 범위는 `JUNEES_APPROVAL`로 정한다.

- `risky`(기본): 명령 정책이 막지는 않지만 주의할 신호가 있는 `bash` 명령, 그리고 작업 폴더 밖 경로를 고치는 `write_file`/`edit_file`.
- `all`: 모든 `bash`, `write_file`, `edit_file` 호출. `read_file`은 묻지 않는다.
- `off`: 묻지 않는다(이전 동작).

`bash`의 주의 신호는 명령 정책과 같은 파서로 찾는다(`$(...)`, `sh -c`, `eval`, 셸에 파이프로 넘긴 스크립트, `timeout`/`env` 같은 래퍼, `find -exec` 안까지).

- 파일 삭제: `rm`, `rmdir`, `unlink`, `shred`, `find -delete`
- git: `git push`(모든 push), 작업을 버리는 명령(종류 `discard git work`: `git reset --hard`, `--`/`.`/`-f`가 붙은 `git checkout`, 작업 트리를 되돌리는 `git restore`(`--staged`만이면 제외), `git stash drop`/`clear`, `git branch -D`), `git clean`(`-n`/`--dry-run` 제외)
- 패키지 설치/제거: `apt`/`apt-get`/`dnf`/`yum`/`brew`/`pacman`/`apk` 등의 install/remove/upgrade, `pip install`, `python -m pip install`, `npm`/`pnpm`/`yarn`/`bun`의 install/add/ci/update, `cargo install`, `go install`, `gem install`, `uv pip install`/`uv add`, 그리고 레지스트리에서 받아 바로 실행하는 `npx`/`pnpx`/`bunx`/`uvx`
- 파일로 내려받기: `curl -o`/`-O`, `wget`(`-O -`처럼 표준 출력으로 받거나 `--spider`면 제외). 내려받은 스크립트를 셸에 바로 넘기는 것은 명령 정책이 막는다
- 권한 변경: `chmod`, `chown`, `chgrp`, `chattr`, `setfacl`
- 작업 폴더 밖에 쓰기: 리다이렉트(`>`, `>>`, `2>`, `&>`; `2>&1`과 `/dev/null` 같은 장치는 제외), `cp`/`ln`/`install`의 대상(`-t` 포함), `mv`/`tee`/`touch`/`mkdir`/`truncate`의 인자, `sed -i`의 파일, `dd of=`, 삭제 명령의 대상
- `JUNEES_ALLOW_DANGEROUS=1`일 때는 기본 규칙이 막았을 명령(`sudo` 등)도 주의 신호로 묻는다

작업 폴더는 junees를 실행한 폴더다. 경로는 그 폴더 기준으로 풀고 이미 있는 부분은 심볼릭 링크를 따라가서 판단한다(폴더 안의 링크가 밖을 가리키면 밖). 커널처럼 링크를 먼저 따라간 뒤 `..`를 적용하므로, `out`이 밖의 `~/elsewhere/sub`를 가리키면 `out/../x`는 `~/elsewhere/x`(밖)다. 한 명령 안의 `cd`/`pushd`/`popd`는 따라간다. `cd sub && touch a`는 안, `cd ~ && echo x >> .bashrc`는 밖이다. bash처럼 `-P`가 없으면 `cd`의 `..`는 글자대로 지운다. `cd -`나 `cd "$DIR"`처럼 갈 곳을 알 수 없으면 그 뒤의 상대 경로는 모두 밖으로 친다. 파이프라인이나 `( ... )` 안의 `cd`도 뒤 명령에 이어지는 것으로 보고, `cd`가 있는 명령의 `$(...)` 안은 위치를 모르는 것으로 본다(더 묻는 쪽으로만 틀린다). `/tmp`와 `TMPDIR`은 안으로 친다. `~`, `$HOME`은 홈으로 풀고, 그 밖의 변수나 `` ` ``가 들어간 경로, 다른 사용자의 홈(`~user`), 대상이 없는 심볼릭 링크는 알 수 없으므로 밖으로 친다. 명령 정책이 막는 명령은 묻지 않고 그대로 막힌다.

질문은 표준 오류에 도구 이름, 명령이나 경로, 이유를 보여 준다. 모델이 쓴 글자 가운데 제어 문자(탭 제외)와 양방향 서식 문자(U+202E 등)는 `\r`, `\u{1b}`처럼 글자로 바꿔 보여 주므로, `\r`이나 ESC로 질문 줄을 지우고 다른 명령처럼 보이게 할 수 없다. `edit_file`이면 바꿀 부분(`- 원래`, `+ 새 내용`, 각 8줄까지)을, `write_file`이면 새 파일인지 덮어쓰는지와 내용 앞부분을 함께 보여 준다.

```
  ?? approve bash: rm marker
     * `rm` deletes files
     always allows: bash: delete files
  [y] run  [n] decline  [a] always allow this kind this session  [e] decline with a reason >
```

- `y` 실행, `n` 거절, `a` 이번 실행 동안 같은 종류(도구별: 예 `bash: delete files`, `edit_file: write outside the working folder`)는 묻지 않고 허용, `e` 거절 이유 입력(`e 이유`처럼 한 줄로도 된다). 다른 입력이면 다시 묻는다.
- 거절하면 도구를 실행하지 않고 `the user declined: <이유>`(이유가 없으면 `no reason given`) 오류 결과를 모델에 돌려줘서 모델이 계획을 바꾸게 한다. Ctrl-D(입력 끝)도 거절이다.
- 질문을 기다리는 중 Ctrl-C는 그 호출을 실행하지 않고 턴을 취소한다(실행 중인 도구를 Ctrl-C로 취소할 때와 같이 그 호출과 남은 호출을 `cancelled by the user` 결과로 채운다). 질문과 프롬프트가 표준 입력을 함께 쓰므로 그 뒤에 입력한 줄은 프롬프트로 간다.
- "항상 허용"은 junees를 끝낼 때까지 유지된다(`/new`로 새 세션을 시작해도, `/sessions`로 다른 세션으로 옮겨도 남는다).
- 표준 입력이나 표준 오류가 터미널이 아니면(파이프, `2>log`) 대화 모드라도 물을 수 없으므로(질문이 보이지 않는다) 시작할 때 안내를 띄우고, 승인이 필요한 호출은 실행하지 않고 `not run: this call needs the user's approval ...` 오류 결과를 돌려준다. 그런 호출까지 실행하려면 `JUNEES_APPROVAL=off`.
- 질문은 모델 응답이 끝난 뒤, 도구 호출 줄(`-> bash ...`) 다음에 나온다. 스트리밍으로 찍던 텍스트 줄은 그 전에 끝나므로 질문이 텍스트 줄 중간에 붙지 않는다. stdout이 터미널이 아니면(`junees | tee log`) 그 응답의 텍스트는 도구 호출 줄 직전에 한 번에 찍힌다.
- `-p`와 `task` 모드는 사람이 없으므로 묻지 않는다(명령 정책만 적용).
- `risky`는 흔한 실수를 막는 장치이지 보안 경계가 아니다. 명령 문자열만 보고 판단하므로 `rsync`, `tar -C`, `unzip -d`, `perl -pi`, `python3 -c "open(...)"`, `crontab -r`처럼 따로 알지 못하는 명령의 쓰기는 묻지 않고, 작업 폴더 안의 `write_file`은 묻지 않으므로 스크립트를 써 두고 `bash run.sh`나 `make`로 실행하면 어떤 검사든 피할 수 있다. 그 경계가 필요하면 `JUNEES_APPROVAL=all`을 쓴다.

## 구조

```
src/
  main.rs            CLI 진입점 (처음 실행 설정과 junees setup, 대화 모드와 슬래시 명령, -p 실행, task 명령(계획 + 실행), 세션 열기/이어 가기, SIGINT/SIGTERM 처리)
  agent.rs           에이전트 루프 (기록 변경을 세션 기록에 남김, 도구 결과 비우기/압축/초과 시 재시도, 취소, 모델 호출 전 한도 확인, 도구 호출 전 승인 훅 ToolGate, 스트리밍 이벤트 전달, pause_turn 이어 가기와 인용 출처 보고)
  approval.rs        실행 전 승인 (JUNEES_APPROVAL, 묻는 범위 판단, 터미널 질문, 세션 동안 항상 허용, 프롬프트와 공유하는 표준 입력)
  cancel.rs          취소 플래그 (시그널 처리기와 에이전트 루프가 공유)
  session.rs         세션 기록(JSONL) 쓰기, 재생, 이어 가기, 세션 찾기/목록, 비우기/압축 지점 계산
  context.rs         압축 요약 요청(시스템 프롬프트, 앞부분을 텍스트로 옮기기)
  task/              장기 작업 모드
    runner.rs          task init/run 세션 루프, 체크포인트 커밋, 정지 조건, 크래시 복구, 실행 로그, Claude Code 세션 뒤 하네스 verify
    claude_code.rs     claude-code 백엔드: claude 실행 인자와 권한 규칙, 환경 정리, stream-json 읽기, 예산/취소, 로그인 실패 판단
    claude_tests.rs    가짜 claude로 claude-code 백엔드의 init/run 검증(단위 테스트)
    budget.rs          토큰/시간 예산, 기간 파싱(90s, 30m, 2h)
    lock.rs            동시 실행 방지(저장소 루트 flock)
    features.rs        features.json 검증, 하네스의 권위 있는 사본(Ledger)
    tools.rs           mark_feature_passing (verify 명령을 하네스가 실행)
    state.rs           state.json (세션 수, 무진전 횟수, 토큰, 정지 이유와 시각, 목표, 실행 중 표시, 기능 목록 사본)
    status.rs          task status (읽기 전용, 텍스트/JSON)
    prompt.rs          초기화/작업 세션 프롬프트
    git.rs             저장소 확인, 커밋, 로그
  message.rs         대화 메시지 타입 (Messages API 형식, 모르는 블록은 원본 JSON 그대로, 텍스트의 citations 보존, 서버 도구 사용량)
  web_search.rs      웹 검색 서버 도구 설정과 도구 정의, 인용 출처 목록, 압축용 검색 블록 요약
  notify.rs          디스코드 웹훅 알림 (정지, 세션마다)
  provider/          LLM 제공자 트레이트와 Anthropic 클라이언트 (프롬프트 캐싱 표시, 확장 사고 설정, 스트리밍 재시도와 대기 제한)
    stream.rs          SSE 이벤트 분리와 응답 조립 (스트리밍하지 않은 응답과 같은 값)
  tools/             도구 트레이트, 레지스트리, bash/read_file/write_file/edit_file, bash 명령 정책과 주의 등급, 작업 폴더 판단(policy.rs), 도구와 정책이 설정 파일을 알아보는 판단(settings_file.rs)
  config.rs          설정 값 읽기와 검사 (from_lookup 이음새, 도구 설정 ToolSettings 포함)
  config_file.rs     설정 파일 config.toml (단순 TOML 읽기/쓰기, 0600), Settings: 실행 중 변경 -> 환경 변수 -> 파일 순서로 찾기
  setup.rs           설정 마법사 (Prompter 트레이트, Claude Code 확인 ClaudeProbe, 키 입력 때 터미널 echo 끄기 EchoOff)
  slash.rs           대화 안 슬래시 명령 파싱, /sessions 번호 고르기
  prompt.rs          시스템 프롬프트
  cli.rs             인자 파싱(위치 인자 질문, task/status/setup 하위 명령, 예전 플래그), 도움말, 출력 보조 함수
tests/
  common/              테스트용 가짜 Anthropic API 서버 (JSON 응답과 SSE 스트림)
  anthropic_client.rs  가짜 API 서버로 실제 클라이언트와 루프를 끝까지 검증 (thinking 블록이 signature까지 그대로 돌아가는지 포함)
  session_resume.rs    실제 바이너리로 -c, --resume <앞부분>, --no-session, HOME 없는 실행, 이어 간 세션의 thinking 블록 보존 검증
  secret_env.rs        기본 bash에서 API 키와 웹훅 URL이 보이지 않는지 검증
  hidden_env.rs        JUNEES_HIDDEN_ENV로 지정한 변수가 bash에서 보이지 않는지 검증
  proc_environ.rs      실제 바이너리의 bash 손자 프로세스가 조상들의 /proc/<pid>/environ에서 API 키와 웹훅 URL을 읽지 못하는지 검증
  context_management.rs 가짜 API 서버로 도구 결과 비우기, 압축, 캐시 표시, 기록 재생 검증
  task_mode.rs         실제 바이너리로 임시 git 저장소에서 task init -> task run -> task status, task run 중 SIGTERM, 가짜 웹훅으로 알림 검증
  cancel.rs            실제 바이너리로 -p 중 SIGTERM(종료 코드 143), 대화 모드 Ctrl-C(턴 취소 후 다음 입력, 두 번이면 종료) 검증
  task_git_env.rs      GIT_DIR이나 GIT_CONFIG_PARAMETERS가 있어도 task 모드가 제 저장소와 설정으로만 git을 쓰는지 검증
  approval.rs          실제 바이너리로 파이프 입력 대화 모드의 위험 명령 거절과 안내, JUNEES_APPROVAL=off, -p에서는 묻지 않음, 가상 터미널에서 스트리밍 텍스트와 승인 질문이 섞이지 않는지 검증
  task_git_secret.rs   하네스 git이 심어진 훅을 실행하지 않고 git이 실행하는 필터에도 API 키를 넘기지 않는지 검증
  streaming.rs         조각내 보내는 가짜 SSE 서버로 조립 결과가 일반 응답과 같은지, 오류 이벤트/끊김/대기 초과 재시도, 취소 시 연결 종료, 바이너리가 텍스트를 한 번만 찍는지(stdout이 파이프일 때 재시도로 끊긴 조각을 버리는지) 검증
  web_search.rs        가짜 API 서버(JSON, SSE)로 웹 검색 도구 정의, pause_turn 이어 가기(멈춘 응답을 그대로, 뒤에 user 메시지 없이), 검색 블록과 citations 재전송, 클라이언트 도구 미실행, 바이너리의 출처 목록과 조직 비활성 안내 검증
  claude_code.rs       실제 바이너리와 가짜 claude로 API 키 없이 task init/run, claude 환경에 API 변수 없음, 로그인 실패 안내와 알림, SIGTERM 검증
  simple_cli.rs        실제 바이너리로 위치 인자 질문과 설정 파일(환경 변수 우선, 잘못된 파일), 터미널 아닐 때 마법사 없음, 가상 터미널에서 처음 실행 마법사(키가 화면에 안 나옴, 0600), junees task "목표" -> junees task -> junees status, 대화의 /backend, /task, /status, /sessions 고르기, /model, /new 검증
```

## 로드맵

1. 에이전트 루프: Anthropic Messages API, 도구 호출 반복, 기본 도구(bash, 파일 읽기/쓰기), CLI (완료)
2. 장기 실행 하네스: 세션 기록과 재개, 컨텍스트 압축, 여러 세션에 걸친 장기 작업 모드, 무인 실행 안정성. 자세한 계획과 진행 상황은 [docs/long-running.md](docs/long-running.md)
3. 도구 체계: 스트리밍 출력, 위험 명령 승인, 파일 패치(edit_file), 웹 검색, 확장 사고 블록 보존 (완료)
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
