# 장기 실행 하네스 계획

Junees가 컨텍스트 창 하나에 담기지 않는 큰 작업을, 여러 세션에 걸쳐 끊기지 않고 이어 가게 만드는 단계들이다.
설계는 Anthropic의 "Effective harnesses for long-running agents"(초기화 에이전트 + 작업 에이전트, 기능 목록, 진행 기록, git 체크포인트)를 따른다.

각 단계는 PR 하나이고, PR마다 Momus 리뷰를 받고 반영한 뒤 CI가 통과하면 main에 merge한다.
이 문서는 진행 상황을 기록하는 장부이기도 하다. 단계가 끝나면 아래 상태를 갱신한다.

## 핵심 원칙

- 기록은 디스크가 진실이다. 프로세스가 언제 죽어도 마지막으로 기록된 지점에서 다시 시작할 수 있어야 한다.
- 모델의 "다 했다"는 말을 믿지 않는다. 완료는 하네스가 확인한 검증(테스트, 명령 종료 코드)으로만 인정한다.
- 한 세션은 기능 하나만 끝낸다. 세션이 끝날 때 작업 트리는 커밋된 깨끗한 상태여야 한다.
- 무인 실행은 예산(토큰, 시간, 세션 수)과 정지 조건 안에서만 돈다.

## 단계

### L1. 세션 기록과 재개
- 대화의 모든 변경을 `$JUNEES_HOME/sessions/<id>.jsonl`에 바로 추가 기록(append-only, 기본 `~/.junees`). 항목은 `meta`(첫 줄, 형식 버전 `format: 1` 포함), `message`, `append_text`(실패한 턴 뒤 입력이 마지막 user 메시지에 합쳐짐), `usage`. 모든 줄에 기록 시각 `ts`(unix 초)
- 쓰는 동안 세션 파일을 `flock`으로 잠가서, 실행 중인 세션은 다른 Junees가 이어 가거나 고칠 수 없다. 파일 `0600`, 폴더 `0700`
- 지원하는 것보다 새 `format`의 세션은 오류로 거절(L2에서 새 항목 종류를 넣을 때 `FORMAT`을 올린다)
- 에이전트와 복원이 같은 `Transcript::apply`로 기록을 재생하므로 복원 결과가 실행 중 기록과 같다
- `--continue`/`-c`(현재 폴더에서 마지막으로 기록된 세션), `--resume <id>`(유일한 앞부분 허용), `--no-session`, 모두 `-p`와 함께 사용 가능. 대화 중 `/sessions`, `/reset`은 새 세션
- 재개할 때 기록 검증: 줄바꿈 없이 끝난 마지막 줄이 읽히지 않으면 잘린 쓰기로 보고 버리고 파일에서도 잘라 냄(줄바꿈까지 있는 줄이 읽히지 않으면 위치와 상관없이 오류), 결과 없는 tool_use는 "interrupted" 오류 결과로 채우고 그것도 기록
- 다른 폴더에서 시작한 세션을 이어 가면 시작 폴더를 한 줄로 알림. `JUNEES_HOME`과 `HOME`이 없으면 기록 없이 실행(`-c`/`--resume`만 오류)
- `/sessions`는 각 파일의 meta 줄과 첫 항목만 읽는다
- 토큰 사용량도 기록하고 복원(`/usage`는 현재 세션 기준)
- 기록 쓰기가 실패하면 경고 한 번, 그 세션의 나머지는 기록하지 않음
- 새 세션 파일은 첫 항목을 쓸 때 만든다(빈 세션이 `-c` 후보가 되지 않도록)

### L2. 컨텍스트 관리
- 프롬프트 캐싱: 직렬화 단계에서 마지막 도구, system(텍스트 블록 배열), 마지막 메시지의 마지막 블록에 `cache_control: ephemeral`(한 번만 보내는 요약 호출은 제외). `Usage`에 캐시 쓰기/읽기 토큰을 더하고 `/usage`에 표시
- 컨텍스트 크기 = 마지막 응답의 입력(캐시 포함) + 출력. `JUNEES_CONTEXT_WINDOW`(기본 200000), `JUNEES_COMPACT_THRESHOLD`(기본 0.8). `/context`로 확인
- 모델 호출 직전에 크기가 한도 이상이면 최근 3개를 뺀 도구 결과 메시지의 출력을 비운다(context editing, 짝과 `is_error` 유지). 항목 `clear_tool_results { upto }`
- 비울 것이 없거나 비운 뒤 다음 응답도 한도 이상이면 압축(compaction): 최근 두 사용자 입력(안 되면 하나) 앞에서 자르고(도구 결과로 시작하거나 짝을 쪼개는 지점 금지), 앞부분을 도구 없는 요약 호출로 요약해 꼬리 첫 user 메시지 앞에 붙인다. 입력이 하나뿐인 긴 턴(L3 작업 세션 형태)은 도구 결과만 든 메시지 다음 assistant 메시지에서 자르고(최근 도구 왕복 두 번 유지), 요약과 원래 입력 원문을 담은 user 메시지를 앞에 넣는다. 요약 실패, 빈 요약, `max_tokens`로 잘린 요약이면 경고만
- 압축은 세션 파일을 새로 쓰지 않고 `compact { summary, keep_from }` 항목을 추가하는 방식으로 남긴다(`Transcript::apply`가 처리하므로 "재생 결과 = 실행 중 기록"이 유지됨). 요약 호출 사용량은 `summary_usage`(사용량에는 더하고 컨텍스트 크기에는 안 씀). 새 항목 때문에 `FORMAT`을 2로 올렸고 형식 1 세션도 읽는다
- `/compact`로 수동 압축, 압축하면 CLI에 한 줄 표시
- 컨텍스트 초과 400을 받으면 강제로 비우기+압축, 이어서 최근 것까지 모든 도구 결과를 비운 뒤 한 번만 재시도(줄어든 게 없으면 재시도 없이 오류)
- 형식 1 세션을 L2로 이어 쓰면 meta는 형식 1 그대로라 L1 바이너리는 `line N is corrupt`로 거절한다(파일은 안전). L2부터는 모르는 항목 종류를 "continued by a newer Junees"로 알린다
- 웹 검색(서버 도구, `JUNEES_WEB_SEARCH`): 검색 블록(`server_tool_use`, `web_search_tool_result`)은 원본 그대로 두고 다시 보낸다. 도구 결과 비우기는 이 블록을 건드리지 않는다(`encrypted_content`를 바꾸거나 빼면 400이고, 짝을 지우면 뒤 텍스트의 인용이 가리킬 곳이 없다). 압축 요약 텍스트에는 검색어와 앞 5개 결과의 제목/URL만 넣는다. 검색한 호출의 사용량은 API의 검색 반복마다 읽은 캐시가 모두 더해져 실제 크기보다 커서 컨텍스트 크기로 쓰지 않는다(검색 없는 다음 호출이 다시 잼). 텍스트의 `citations`는 기록에 남는 필드라 이전 바이너리는 읽을 때 버리지만(인용 없는 텍스트로 보냄) 거절하지는 않으므로 `FORMAT`은 올리지 않았다
- 확장 사고 블록 보존(#3): 응답의 thinking, redacted_thinking, 서버 도구 블록을 `ContentBlock::Other`(원본 JSON)로 기록과 세션 파일에 그대로 남기고 다음 요청에 그대로 보낸다. 캐시 표시는 마지막 메시지의 마지막 일반 블록에, 요약 요청의 텍스트에서는 thinking을 빼고 다른 원본 블록은 종류만 적는다. 이런 블록을 이전 바이너리가 자리표시로 잘못 읽지 않도록 `FORMAT`을 3으로 올렸다. `JUNEES_THINKING_BUDGET`(기본 0, 켜면 1024 이상이고 `JUNEES_MAX_TOKENS` 미만)으로 확장 사고를 켜며, 요약 호출에는 쓰지 않는다. Claude 5.1 이후 모델의 thinking 앞부분 검사 때문에 비우기와 압축은 기록의 thinking 블록을 모두 지우고, 예산을 켠 수동 모드(검사 없는 4.6 이하 모델)에서는 지우지 않는 대신 한 턴 안의 압축을 턴 전체 요약으로 바꿔 턴이 thinking으로 시작하게 한다(`keep_thinking`). 형식 2 이하 세션을 이어 쓰면 `format` 항목을 덧붙여 이전 바이너리가 열지 않게 한다

### L3. 장기 작업 모드
- 작업 파일은 대상 저장소의 `.junees/`에 둔다. `features.json`(기능 목록: `id`, `description`, `verify` 셸 명령, `passes`), `progress.md`(사람이 읽는 진행 기록, 세션과 하네스가 함께 덧붙임), `state.json`(하네스 상태, `.junees/.gitignore`로 git에서 제외)
- `junees task init "<목표>"`: git 저장소가 아니면 오류(자동 `git init` 없음), 이미 `features.json`이 있거나 작업 트리가 더러우면(`.junees/` 제외) 오류. 초기화 세션(목표를 검증 가능한 기능 5~50개로 쪼개고 verify 명령을 정하고 `progress.md`를 쓰라, 필요하면 `init.sh`, 아직 구현하지 말라)이 끝나면 하네스가 목록을 검증(JSON 배열, 비어 있지 않음, id 유일, verify 존재, 전부 `passes: false`, verify를 한 번씩 실행해 이미 통과하는 것 거절, 문제를 모두 나열)하고, 틀리면 오류 내용을 같은 세션에 보내 한 번 더 고치게 한다. 그래도 틀리거나 세션이 실패하면 목록을 `features.rejected.json`으로 옮겨 init을 다시 할 수 있게 한다. 통과하면 하네스 서식으로 다시 쓰고 `junees: initialize task`로 커밋
- `junees task run [--max-sessions N]`(기본 20): 작업 트리가 더러우면 거절. 시작할 때 파일에서 `passes: true`인 기능의 verify를 다시 실행해 실패하는 것은 통과에서 뺀다(경고와 progress 한 줄). 재검증이 작업 트리를 바꿨으면 세션 전에 `junees: recheck passing features at run start`로 커밋해서, 세션 없이 정지해도 트리가 깨끗하다(이슈 #11). 세션마다 새 `Agent`(새 컨텍스트, L1 세션 기록). system 프롬프트에 규칙, 기능 목록과 통과 여부, `progress.md` 마지막 4000자, `git log --oneline -20`. 첫 입력은 "안 끝난 기능 하나를 골라 구현, 검증, 커밋하고 progress.md를 갱신하라"
- 하네스가 강제하는 규칙: `passes`는 전용 도구 `mark_feature_passing {id}`로만 바뀐다. 하네스가 verify 명령을 저장소 루트에서 bash 도구와 같은 방식(출력 제한, API 키 변수 제거, 타임아웃 600초)으로 `set -o pipefail`을 붙여 실행하고 exit 0일 때만 저장, 실패하면 출력과 함께 오류 결과. 하네스는 기능 목록의 권위 있는 사본을 들고 있다가 세션이 끝나면 파일을 그 사본으로 되돌려 쓴다(내용이 바뀌었으면 경고와 progress 한 줄, 서식만 다르면 조용히)
- 세션이 끝나면 progress에 하네스 한 줄(세션 번호, 통과 수/전체, 새로 통과한 기능, 토큰)을 붙이고, 작업 트리가 더러우면 `junees: checkpoint after session N`으로 커밋. 하네스의 커밋은 훅과 fsmonitor 없이(`core.hooksPath=/dev/null`, `--no-verify`), API 키 변수 없이 실행하고, 비밀 파일처럼 보이는 새 파일(`.env*`, `*.pem`, `*.key`, ssh 키 등)은 빼고 경고. 커밋 전에 `.junees/`와 `.gitignore`를 다시 만든다
- 정지 조건: 모든 기능 통과, 세션 수 상한, 연속 3세션 새 통과 없음(stalled), 세션 오류(오류를 progress에 남기고 체크포인트 후 정지), 세션 뒤 하네스 작업 실패. 턴 한도(`JUNEES_MAX_TURNS`)를 넘긴 세션은 오류가 아니라 세션 종료로 보고 다음 세션으로 넘어간다(stalled 규칙이 처리). 이유를 출력하고 `state.json`에 기록, stalled와 오류는 종료 코드 1
- `state.json`: 실행한 세션 수, 연속 무진전 세션 수, 누적 usage, 마지막 정지 이유. 다음 `task run`은 세션 번호를 이어 간다. 이전 실행이 스스로 멈췄으면(`--max-sessions` 포함) 무진전 횟수를 초기화, 정지 기록 없이 끝났으면(크래시) 이어서 센다. 그래서 `--max-sessions 1`을 반복하면 stalled로는 멈추지 않는다(의도한 동작)
- 예산(토큰/시간), 시그널 처리, 무인 명령 정책은 L4, `task status`와 알림은 L5

### L4. 무인 실행 안정성
- 예산: 작업 전체 토큰 상한, 시간 상한, 세션 수 상한
  - L4b: `task run --max-tokens N`/`JUNEES_TASK_MAX_TOKENS`(작업 전체 누적: init 세션, 요약 호출, 캐시 읽기/쓰기 포함, `state.json`에서 이어 셈), `--max-time`/`JUNEES_TASK_MAX_TIME`(이번 실행의 실제 경과 시간, `90s`/`30m`/`2h`/`1h30m`). 세션 사이와, 세션 안에서는 모델 호출 직전마다(`Agent::with_limit`, 압축 요약 호출 포함), 시작 시 재검증에서는 verify 명령마다 확인하고, 넘으면 그 세션을 마무리(체크포인트 커밋)한 뒤 `budget`으로 정지(종료 코드 0). 시계는 테스트에서 주입
- Ctrl-C/SIGTERM: 현재 도구를 끝내고 짝 없는 tool_use를 채움(#2)
  - L4b: 공유 취소 플래그(`Cancel`, tokio `watch`)를 `Agent::with_cancel`로 넘긴다. 설정되면 모델 호출은 버리고, 실행 중인 도구 호출은 drop되어 bash가 프로세스 그룹 전체를 SIGKILL로 끝내며(정상 종료한 명령의 백그라운드 프로세스는 그대로), 결과 없는 tool_use는 `cancelled by the user` 오류 결과로 채운 뒤 `Cancelled` 오류로 끝난다. 기록은 user 메시지로 끝나서 다음 입력이 붙는다
  - 대화 모드: Ctrl-C는 턴만 취소, 프롬프트에서 두 번(또는 Ctrl-D)이면 종료, SIGTERM은 취소 후 143. `-p`: 취소 후 130/143. `task run`: 세션을 마무리하고 체크포인트 커밋, `interrupted` 정지 이유, 130/143. 두 번째 시그널은 즉시 종료
- 크래시 복구와 실행 로그
  - L4b: `task run`이 저장소 루트 폴더에 `flock`을 잡고(`.junees/`는 세션이 지웠다 다시 만들어지면 잠금이 풀리므로) `state.json`에 `running {pid, started}`와 기능 목록의 하네스 사본을 남긴다. 잠금이 잡혀 있으면 거절, 표시만 남아 있으면 크래시로 보고 `features.json`의 기능 정의를 init 커밋에서 되돌림(사본과 파일에서는 `passes: true`만 가져와 init 커밋의 verify로 시작 시 재검증. 세션이 고칠 수 있는 `state.json` 사본의 정의는 믿지 않음), `progress.md` 한 줄, `junees: recover after crash` 커밋 후 이어 간다
  - `.junees/run.log`: 실행 시작/정지(이유, 토큰, 경과 시간), 세션 시작/끝, 크래시 복구를 UTC 시각과 함께 한 줄씩. git 제외(예전 작업은 첫 실행에서 `.gitignore`를 고쳐 따로 커밋)
  - 남은 것: 크래시 난 세션의 토큰은 누적에 들어가지 않음, 세션이 `.junees/`를 통째로 지우면 잠금과 실행 중 표시도 사라짐
- 무인 모드 명령 정책: 위험 명령 차단 목록, 작업 폴더 밖 쓰기 경고
  - L4a: `bash` 명령 정책(`src/tools/policy.rs`). 실행 전에 루트/홈/상위 폴더 재귀 삭제(그 폴더로 `cd` 한 뒤 `rm -rf *` 포함), 디스크 파괴, 시스템 종료, fork bomb, `curl | sh`, 강제 push, `sudo` 같은 권한 상승을 막고(셸의 표준 입력으로 넘긴 스크립트도 검사) `blocked by policy: <이유>` 오류 결과를 돌려준다. `JUNEES_DENY_COMMANDS`로 접두사 추가, `JUNEES_ALLOW_DANGEROUS=1`로 기본 규칙 끔. 숨길 환경 변수 `JUNEES_HIDDEN_ENV`(#4). 작업 폴더 밖 쓰기와 그 밖의 주의 신호(삭제, `git push`, 패키지 설치 등)는 정책 모듈의 주의 등급(`CommandPolicy::cautions`)으로 찾아 대화 모드에서만 실행 전에 묻는다(`JUNEES_APPROVAL`, `src/approval.rs`). 무인 모드(`-p`, `task`)는 묻지도 경고하지도 않는다

### L5. 상태 보기와 알림
- `junees task status [--json]`: `.junees/`를 읽기만 한다(실행 중인 run 옆에서도 안전, API 키 불필요). 목표, 통과 수/전체와 진행 막대, 기능별 id/통과 여부/설명 첫 줄, 세션 수, 연속 무진전 수, 누적 토큰(입력/출력/캐시), 마지막 정지 이유와 시각, `progress.md` 마지막 5줄. `--json` 필드는 README에 정리. `.junees/`가 없으면 `task init`을 먼저 하라는 오류
- 이를 위해 `state.json`에 `goal`(init 때 기록)과 `stopped_at`(정지 기록 시각, unix 초)을 추가했다. 둘 다 선택 필드라 예전 파일도 읽힌다. status는 L4b의 실행 중 표시(`running {pid, started}`)와 그 프로세스가 살아 있는지(`kill(pid, 0)`, 잠금은 잡지 않음)로 실행 중/크래시를 구분하고, 표시가 있는 동안에는 기능 목록을 `state.json`의 하네스 사본에서 읽는다(세션이 `features.json`을 직접 고친 통과가 verify 없이 보이지 않도록). `budget`/`interrupted` 정지도 텍스트, JSON, 알림에 그대로 나온다
- 알림: `JUNEES_NOTIFY_WEBHOOK`(디스코드 웹훅 URL)이 있으면 `task run`이 정지를 기록한 뒤 한 번 POST(저장소 이름, 정지 이유 키와 문장, 통과 수/전체, 세션 수, 토큰). 정지 이유는 `state.json`의 값을 그대로 쓰므로 L4b의 `budget`/`interrupted`도 따로 손대지 않고 알림에 나온다. `JUNEES_NOTIFY_EACH_SESSION=1`이면 세션마다 한 줄. `allowed_mentions: {"parse": []}`, 2000자 제한, 타임아웃 10초, 리다이렉트 안 따라감. 실패는 경고만 하고 종료 코드는 그대로. URL은 출력하지 않고 호스트만 보여 주며, 세션의 bash와 하네스 git에도 넘기지 않는다. 정지를 기록하기 전에 실패한 run(더러운 작업 트리 등)도 `run failed: <오류>` 한 번을 보낸다. 세션 알림은 상태(그 세션의 토큰, 마지막 세션이면 정지 기록)를 저장한 뒤에 보낸다. 웹훅은 https만 받고(평문 http는 루프백만), 바뀌는 텍스트는 인라인 코드로 감싸 마크다운이 적용되지 않게 한다
- 보안: junees는 시작할 때 `prctl(PR_SET_DUMPABLE, 0)`으로 dump 불가 프로세스가 되어, bash가 `/proc/$PPID/environ`(또는 더 위 조상)에서 숨긴 API 키와 웹훅 URL을 읽지 못한다(PR #1부터 있던 우회로)

### 구독 백엔드 (Claude Code CLI)
- `task init`/`task run`에 `--backend claude-code`(또는 `JUNEES_BACKEND`). 세션을 공식 `claude` CLI가 실행하고, 사용자가 자기 구독으로 직접 로그인한다(`claude` 실행 후 `/login`, 또는 `claude setup-token`). Junees는 Claude 자격 증명을 읽거나 저장하거나 보내지 않고 `claude`를 실행만 한다(약관상 허용되는 길). `ANTHROPIC_API_KEY`는 필요 없고, `claude`의 환경에서 인증 우선순위상 구독보다 앞서는 변수(API 키와 토큰, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_USE_*` 클라우드 제공자와 그 키, `ANTHROPIC_PROFILE`과 federation 변수)를 지운다. 설정 파일의 `apiKeyHelper`/`env`는 시작할 때와 세션마다 `claude auth status --json`의 `authMethod`가 `claude.ai`/`oauth_token`인지 확인해 막는다
- 토큰 보호: 세션마다 `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1`을 켜서 Claude Code가 Bash 명령의 환경에서 `CLAUDE_CODE_OAUTH_TOKEN` 등 자격 증명을 지우게 한다(Linux에서는 bubblewrap 필요, 없으면 시작 경고). 사용자가 끄면 그대로 넘기고 시작 경고
- 세션: `claude -p ... --output-format stream-json --verbose --append-system-prompt <규칙과 상태> --permission-mode acceptEdits --permission-prompts none --tools/--allowedTools Read,Edit,Write,Glob,Grep,Bash --setting-sources user --strict-mcp-config --max-turns N --disallowedTools <L4a 핵심 규칙>`. `--dangerously-skip-permissions`는 쓰지 않는다. 시작 전에 `claude --version`(2.1.259 이상) 확인, 로그인 실패(마지막 모델 호출의 인증 오류나 `auth status`)면 안내와 함께 `error` 정지와 알림
- `mark_feature_passing`이 없으므로 세션 뒤 하네스가 통과하지 않은 기능의 verify를 직접 실행해 기록한다(취소, 로그인 실패면 건너뜀, 시간 예산을 verify마다 확인). 나머지 하네스 규칙(목록 되돌리기, 체크포인트, 정지 조건, 재검증, 크래시 복구, run.log, 알림)은 그대로
- stream-json의 응답별 usage로 세션 중 예산을 확인하고(넘으면 SIGINT, 유예 뒤 SIGKILL), 끝나면 `result`의 `modelUsage`/`usage`를 누적. 취소도 같은 방식이고 `interrupted`. 정지할 때는 SIGINT 전에, 그리고 SIGKILL 전에 한 번 더 `claude`의 자손 프로세스(별도 세션으로 뜬 Bash 명령 포함)를 SIGSTOP으로 멈춘 뒤 SIGKILL한다. 원본 출력은 `.junees/sessions/<이름>.jsonl`(git 제외), 비용 추정과 Claude Code 세션 id는 run.log
- 한계: Claude Code 권한 규칙은 쓰인 단어만 맞추므로 Junees bash 정책보다 약하다(`/usr/bin/sudo`, `git -C . push --force`, `bash -c`, fork bomb 등). 구독 사용 한도는 알 수 없다(한도에 걸리면 `error` 정지, 별도 정지 이유는 다음 PR). 정지 전에 `claude`가 스스로 죽으면 그 명령은 남는다. 실제 `claude`로는 테스트하지 않았다(가짜 실행 파일로 검증)

## 상태

- [x] 1단계 에이전트 루프 (PR #1)
- [x] L1 세션 기록과 재개 (PR #5)
- [x] L2 컨텍스트 관리 (PR #6)
- [x] L3 장기 작업 모드 (PR #8)
- [x] L4 무인 실행 안정성 (L4a PR #7, L4b PR #9)
- [x] L5 상태 보기와 알림 (PR #10)
