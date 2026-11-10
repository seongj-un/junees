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
- 컨텍스트 크기 = 마지막으로 잰 응답의 입력(캐시 포함) + 출력 + 그 뒤 더해진 블록의 추정치(JSON을 글자 단위로: ASCII 4글자당 1토큰, 한글/한자/가나 1글자당 1토큰, 그 밖의 비ASCII는 4바이트당 1토큰. #28). `JUNEES_CONTEXT_WINDOW`(기본 200000), `JUNEES_COMPACT_THRESHOLD`(기본 0.8). `/context`로 확인
- 모델 호출 직전에 크기가 한도 이상이면 최근 3개를 뺀 도구 결과 메시지의 출력을 비운다(context editing, 짝과 `is_error` 유지). 항목 `clear_tool_results { upto }`
- 비울 것이 없거나 비운 뒤 다음 응답도 한도 이상이면 압축(compaction): 최근 두 사용자 입력(안 되면 하나) 앞에서 자르고(도구 결과로 시작하거나 짝을 쪼개는 지점 금지), 앞부분을 도구 없는 요약 호출로 요약해 꼬리 첫 user 메시지 앞에 붙인다. 입력이 하나뿐인 긴 턴(L3 작업 세션 형태)은 도구 결과만 든 메시지 다음 assistant 메시지에서 자르고(최근 도구 왕복 두 번 유지), 요약과 원래 입력 원문을 담은 user 메시지를 앞에 넣는다. 요약 실패, 빈 요약, `max_tokens`로 잘린 요약이면 경고만
- 압축은 세션 파일을 새로 쓰지 않고 `compact { summary, keep_from }` 항목을 추가하는 방식으로 남긴다(`Transcript::apply`가 처리하므로 "재생 결과 = 실행 중 기록"이 유지됨). 요약 호출 사용량은 `summary_usage`(사용량에는 더하고 컨텍스트 크기에는 안 씀). 새 항목 때문에 `FORMAT`을 2로 올렸고 형식 1 세션도 읽는다
- `/compact`로 수동 압축, 압축하면 CLI에 한 줄 표시
- 컨텍스트 초과 400을 받으면 강제로 비우기+압축, 이어서 최근 것까지 모든 도구 결과를 비운 뒤 한 번만 재시도(줄어든 게 없으면 재시도 없이 오류)
- 형식 1 세션을 L2로 이어 쓰면 meta는 형식 1 그대로라 L1 바이너리는 `line N is corrupt`로 거절한다(파일은 안전). L2부터는 모르는 항목 종류를 "continued by a newer Junees"로 알린다
- 웹 검색(서버 도구, `JUNEES_WEB_SEARCH`): 검색 블록(`server_tool_use`, `web_search_tool_result`)은 원본 그대로 두고 다시 보낸다. 도구 결과 비우기는 이 블록을 건드리지 않는다(`encrypted_content`를 바꾸거나 빼면 400이고, 짝을 지우면 뒤 텍스트의 인용이 가리킬 곳이 없다). 압축 요약 텍스트에는 검색어와 앞 5개 결과의 제목/URL만 넣는다. 검색한 호출의 사용량은 API의 검색 반복마다 읽은 캐시가 모두 더해져 실제 크기보다 커서 컨텍스트 크기로 쓰지 않는다(검색 없는 다음 호출이 다시 잼). 그 사이 크기는 마지막 측정 + 이후 블록 추정치로 잡아 매 호출이 검색하는 세션도 제때 압축한다(#23, 재생으로 계산하므로 새 항목 없음). 턴이 결과 없는 `server_tool_use`를 남기고 끝나면(pause 상한, 취소, 오류) 새 입력 전에 그 호출을 `drop_server_calls` 항목으로 지우고 경고한다(#23, 새 항목이라 `FORMAT` 4). 그런 호출이 마지막 응답이 아닌 곳에 있으면 뒤 thinking 블록의 앞부분이 바뀌므로 `clear_tool_results { upto: 0 }`로 thinking 블록을 모두 지운다(#28, 수동 thinking 예산 모드 제외. 기존 항목이라 형식 그대로). 텍스트의 `citations`는 기록에 남는 필드라 이전 바이너리는 읽을 때 버리지만(인용 없는 텍스트로 보냄) 거절하지는 않으므로 `FORMAT`은 올리지 않았다
- 확장 사고 블록 보존(#3): 응답의 thinking, redacted_thinking, 서버 도구 블록을 `ContentBlock::Other`(원본 JSON)로 기록과 세션 파일에 그대로 남기고 다음 요청에 그대로 보낸다. 캐시 표시는 마지막 메시지의 마지막 일반 블록에, 요약 요청의 텍스트에서는 thinking을 빼고 다른 원본 블록은 종류만 적는다. 이런 블록을 이전 바이너리가 자리표시로 잘못 읽지 않도록 `FORMAT`을 3으로 올렸다. `JUNEES_THINKING_BUDGET`(기본 0, 켜면 1024 이상이고 `JUNEES_MAX_TOKENS` 미만)으로 확장 사고를 켜며, 요약 호출에는 쓰지 않는다. Claude 5.1 이후 모델의 thinking 앞부분 검사 때문에 비우기와 압축은 기록의 thinking 블록을 모두 지우고, 예산을 켠 수동 모드(검사 없는 4.6 이하 모델)에서는 지우지 않는 대신 한 턴 안의 압축을 턴 전체 요약으로 바꿔 턴이 thinking으로 시작하게 한다(`keep_thinking`). 형식 2 이하 세션을 이어 쓰면 `format` 항목을 덧붙여 이전 바이너리가 열지 않게 한다

### L3. 장기 작업 모드
- 작업 파일은 대상 저장소의 `.junees/`에 둔다. `features.json`(기능 목록: `id`, `description`, `verify` 셸 명령, `passes`), `progress.md`(사람이 읽는 진행 기록, 세션과 하네스가 함께 덧붙임), `state.json`(하네스 상태, `.junees/.gitignore`로 git에서 제외)
- `junees task init "<목표>"`: git 저장소가 아니면 오류(자동 `git init` 없음), 이미 `features.json`이 있거나 작업 트리가 더러우면(`.junees/` 제외) 오류. 초기화 세션(목표를 검증 가능한 기능 5~50개로 쪼개고 verify 명령을 정하고 `progress.md`를 쓰라, 필요하면 `init.sh`, 아직 구현하지 말라)이 끝나면 하네스가 목록을 검증(JSON 배열, 비어 있지 않음, id 유일, verify 존재, 전부 `passes: false`, verify를 한 번씩 실행해 이미 통과하는 것 거절, 문제를 모두 나열)하고, 틀리면 오류 내용을 같은 세션에 보내 한 번 더 고치게 한다. 그래도 틀리거나 세션이 실패하면 목록을 `features.rejected.json`으로 옮겨 init을 다시 할 수 있게 한다. 통과하면 하네스 서식으로 다시 쓰고 `junees: initialize task`로 커밋
- `junees task run [--max-sessions N]`(기본 20): 작업 트리가 더러우면 거절. 시작할 때 파일에서 `passes: true`인 기능의 verify를 다시 실행해 실패하는 것은 통과에서 뺀다(경고와 progress 한 줄). 재검증이 작업 트리를 바꿨으면 세션 전에 `junees: recheck passing features at run start`로 커밋해서, 세션 없이 정지해도 트리가 깨끗하다(이슈 #11). 세션마다 새 `Agent`(새 컨텍스트, L1 세션 기록). system 프롬프트에 규칙, 기능 목록과 통과 여부, `progress.md` 마지막 4000자, `git log --oneline -20`. 첫 입력은 "안 끝난 기능 하나를 골라 구현, 검증, 커밋하고 progress.md를 갱신하라"
- 하네스가 강제하는 규칙: `passes`는 전용 도구 `mark_feature_passing {id}`로만 바뀐다. 하네스가 verify 명령을 저장소 루트에서 bash 도구와 같은 방식(출력 제한, API 키 변수 제거, 타임아웃 600초)으로 `set -o pipefail`을 붙여 실행하고 exit 0일 때만 저장, 실패하면 출력과 함께 오류 결과. 하네스는 기능 목록의 권위 있는 사본을 들고 있다가 세션이 끝나면 파일을 그 사본으로 되돌려 쓴다(내용이 바뀌었으면 경고와 progress 한 줄, 서식만 다르면 조용히)
- 세션이 끝나면 progress에 하네스 한 줄(세션 번호, 통과 수/전체, 새로 통과한 기능, 토큰)을 붙이고, 작업 트리가 더러우면 `junees: checkpoint after session N`으로 커밋. 하네스의 커밋은 훅과 fsmonitor 없이(`core.hooksPath=/dev/null`, `--no-verify`), API 키 변수 없이 실행하고, 비밀 파일처럼 보이는 새 파일(`.env*`, `*.pem`, `*.key`, ssh 키 등. 비밀 판정은 rename 감지 없이 해서 `.env.example`을 옮겨 채운 `.env`도 잡는다)과 1 MiB가 넘는 새 파일(옮기기만 한 큰 파일은 제외)은 빼고 이유와 함께 경고. 그런 파일만 남아 있으면 다음 실행은 경고하고 시작하고(그 파일들은 이후 커밋에서도 빠지므로), 다른 변경 때문에 더러운 트리를 거절할 때는 파일 이름(세 개까지, 나머지는 개수)과 이유를 보여 준다. `task init`은 git이 아직 무시하지 않는 흔한 빌드 산출물 폴더(`/target/`, `node_modules/`, `/dist/`, `/build/`, `.venv/`, `__pycache__/`, 루트 산출물은 루트에만 걸리게 고정)를 루트 `.gitignore`에 더해 init 커밋에 넣는다. 커밋 전에 `.junees/`와 `.gitignore`를 다시 만든다
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
- 구독 사용 한도(#20): 끝나지 못한 세션이 `rate_limit_event`(`status: rejected`)로, 또는 429(마지막 모델 호출의 `rate_limit` 오류나 `api_error_status: 429`인 `result`)와 errors 문서의 한도 문구(`You've hit your ... limit`)로 끝나면 `limit`으로 정지(한도 문구 없는 429는 서버의 일시 제한일 수 있어 `error`)(종료 코드 0, 실패 아님). 리셋 시각은 거절 이벤트의 `resetsAt`(unix 초)이 있을 때만 `state.json`의 `stop.resets_at`에 남기고, Claude Code의 메시지는 `detail`. 정지 문장(리셋 시각과 "리셋 뒤 `junees task`를 다시 실행" 안내)이 `task status`(JSON `resets_at`), run.log, 알림에 나온다. 그 세션의 작업은 다른 세션처럼 verify, 체크포인트 커밋을 거친다. 자동으로 기다렸다 이어 가지는 않는다
- 설정 파일 보호: `--disallowedTools` 끝에 `$JUNEES_HOME/config.toml`(`..`, 링크를 푼 경로)의 `Read(//<경로>)`, `Edit(//<경로>)`(gitignore 특수 문자 이스케이프)와, 폴더가 저장소와 겹치지 않으면 `Read(//<폴더>/**)`, `Edit(//<폴더>/**)`, 폴더 이름을 쓰는 Bash 명령 규칙(`Bash(*<폴더>*)`, `~/`, `$HOME/`, `$JUNEES_HOME` 형태)을 붙인다. 파일을 직접 여는 스크립트, 실행 중에 만든 경로, 더 위 폴더를 다루는 명령은 막지 못한다
- 한계: Claude Code 권한 규칙은 쓰인 단어만 맞추므로 Junees bash 정책보다 약하다(`/usr/bin/sudo`, `git -C . push --force`, `bash -c`, fork bomb 등). 정지 전에 `claude`가 스스로 죽으면 그 명령은 남는다. 실제 `claude`로는 테스트하지 않았다(가짜 실행 파일로 검증)

### 사용법 단순화 (Hermes처럼)
- `junees` 하나만 외우면 되게: `junees`(대화), `junees "질문"`(한 번만, `-p`와 같음), `junees -c`, `junees task "목표"`(init + run을 한 프로세스에서), `junees task`(이어서 run), `junees status`, `junees setup`. 예전 플래그와 `task init/run/status`는 그대로 동작하고 `--help`의 `Advanced` 절로 옮겼다
- 설정 파일 `$JUNEES_HOME/config.toml`(0600, 폴더 0700): 평평한 TOML 부분집합을 직접 파싱(의존성 없음). 키마다 환경 변수 하나에 대응하고, `Settings`가 실행 중 변경(`/model`, `/backend`) -> 환경 변수 -> 파일 순서로 찾아 기존 `from_lookup` 이음새에 넘긴다. 설정 파일 키는 표(`KEYS`)에 있는 것만 받아서 Junees가 원래 읽지 않는 변수(`ANTHROPIC_TOKEN` 등)는 파일로도 생기지 않는다. 모델의 도구는 이 파일을 건드리지 못한다(`src/tools/settings_file.rs`): `read_file`/`write_file`/`edit_file`은 경로를 풀어서(`..`, 심볼릭 링크, 하드 링크) 거절하고, `bash` 정책은 파일을 이름(글롭, 중괄호 포함)으로 가리키는 명령과 `$JUNEES_HOME` 폴더를 `ls`/`cd`/`stat` 등 말고 통째로 다루는 명령을 `JUNEES_ALLOW_DANGEROUS=1`이어도 막는다. bash 쪽은 차단 목록이라 다른 변수, 실행 중에 만든 경로, 더 위 폴더(`grep -r api_key ~`)를 통한 접근은 통과한다
- 처음 실행 마법사(파일도 `ANTHROPIC_API_KEY`도 없고 stdin/stderr가 터미널일 때, 또는 `junees setup`): 구독(claude-code) 또는 API 키, 구독이면 task 백엔드의 `claude --version`/`auth status` 검사를 그대로 써서 로그인 방법(Linux면 bubblewrap)을 안내한다(대화도 구독으로 돌므로 API 키는 묻지 않는다, 아래 대화 모드 구독 백엔드). 키는 termios echo를 끄고 입력받고 다시 출력하지 않는다. 모델, 작업 기본 예산(2h, 토큰 상한 선택). 마법사 입력은 대화와 같은 `SharedLines`로 읽고 Ctrl-C는 echo를 되돌린 뒤 취소한다
- 대화 안 슬래시 명령: `/task [목표]`(같은 프로세스에서 init + run, 끝나면 원래 폴더로 돌아옴, Ctrl-C는 `junees task`처럼 세션을 마무리하고 멈춤), `/status`, `/sessions`(번호로 골라 이어 가기), `/new`(`/reset`), `/model [이름]`, `/backend [api|claude-code]`. `/model`과 `/backend`는 이 실행에만 적용(파일은 `junees setup`만 바꿈)
- 남은 것: 모델이 bash로 설정 파일의 키를 읽는 것을 막기 (대화 모드 구독은 아래 단계)

### 대화 모드 구독 백엔드 (claude-code로 대화)
- `backend = claude-code`(설정 파일, `JUNEES_BACKEND`, `/backend claude-code`)이면 대화, `junees "질문"`/`-p`, `-c`도 `claude`로 돌린다. API 키는 필요 없다
- task 전용이던 `src/task/claude_code.rs`의 공통 부분(백엔드 설정, `--version`/`auth status` 확인, 환경 정리와 `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`, 세션 실행과 예산/취소, 프로세스 그룹과 자손 종료, stream-json 읽기, 로그인 실패/사용 한도 판정, deny 규칙)을 `src/claude_code/`로 옮겼다. task 쪽은 인자(`acceptEdits`, 시스템 프롬프트)와 실행만 남았고 동작과 기존 테스트는 그대로다
- 한 턴 = 지금 폴더에서 `claude -p --output-format stream-json --verbose --include-partial-messages --permission-mode default --permission-prompts none --tools/--allowedTools Read,Edit,Write,Glob,Grep,Bash --setting-sources user --strict-mcp-config --max-turns N [--resume <id>] [--model M] --disallowedTools <규칙>`. 두 번째 턴부터 앞 턴이 알려 준 `session_id`를 `--resume`으로 넘긴다(headless 문서 Continue conversations, cli-reference `--resume`). 입력은 표준 입력으로 쓰고 닫는다(headless 문서 "Non-interactive mode reads stdin"): 인자 하나의 128 KiB 한도(E2BIG)와 `-`로 시작하는 입력이 옵션으로 읽히는 문제를 피한다
- 권한: `--permission-mode default`(cli-reference: `manual`은 별칭)에 task와 같은 허용/거절 규칙. 확인이 필요한 호출은 `--permission-prompts none`으로 거절되고, `permission_denied` 이벤트(도구 이름, 이유)와 `result.permission_denials`(이벤트로 못 본 것)를 화면에 보여 준다. `--permission-prompt-tool`(MCP)로 Junees 승인 질문을 붙이는 것은 문서상 가능하지만(stdio MCP 서버 구현, 대화 프로세스와의 통로, `--permission-prompts host`, 이때 거절 이벤트 없음, MCP 연결 대기) 후속으로 미뤘다
- 출력: `stream_event`의 텍스트 조각을 #18 스트리밍처럼 찍고 같은 응답의 완성 블록은 다시 찍지 않는다. `api_retry`면 `Retrying`으로 끊긴 조각을 버린다. 도구는 `-> Tool`/`<- Tool ok`. `/usage`는 턴들의 토큰, 턴 수, Claude Code 세션 id. 이어 간 턴은 `result` 합계에 이전 실행분이 들어 있어서(headless 문서) 응답별 합만 센다
- 세션 기록 형식 5: meta의 `backend: "claude-code"`와 턴마다 `claude_turn { claude_session, input, text, usage, end }`. 재생하면 마지막 Claude Code 세션 id와 턴 수, 사용량이 복원되고 메시지 기록은 비어 있다(Claude Code가 들고 있음). 형식 4 바이너리가 빈 API 대화로 잘못 열지 않도록 `FORMAT`을 5로 올렸다. `-c`는 지금 백엔드의 세션만 고르고, `--resume`/`/sessions`로 다른 백엔드 세션을 고르면 `/backend`로 바꾸라는 오류. Junees 쪽 압축(L2)은 이 백엔드에서 쓰지 않는다
- 슬래시 명령: `/compact`, `/context`는 Claude Code가 컨텍스트를 스스로 관리한다는 안내(문서의 `-p` 지원 명령에 없음). `/model`은 `--model`로 전달(`JUNEES_CLAUDE_MODEL` 덮어씀). `/backend`는 대화와 `/task`를 함께 바꾸고 대화를 바꾸면 새 세션
- 한도: 턴이 사용 한도로 끝나면 #26 판정 그대로 리셋 시각과 함께 알리고 턴을 끝낸다(`-p`는 종료 코드 1). Ctrl-C: claude와 자손을 끝내고 턴을 `interrupted`로 기록, 다음 입력은 같은 세션을 이어 간다
- 마법사: 구독을 고르면 대화도 구독으로 된다고 안내하고 API 키를 묻지 않는다
- 한계: 실제 `claude`로는 테스트하지 않았다(가짜 실행 파일로 검증). Claude Code가 세션 id를 알리기 전에 끊긴 첫 턴은 이어 갈 수 없다

### 스킬 (로드맵 4단계 B)
- 저장: `$JUNEES_HOME/skills/<이름>/SKILL.md`(앞머리 `name`, `description` + 본문)와 선택적 `references/`, `templates/`, `scripts/`, `assets/` 파일. 이름 `[a-z0-9-]{1,64}`, 앞머리 `name`은 폴더 이름과 같아야 한다. YAML은 직접 읽는다(최상위 한 줄 `name`/`description`만, 따옴표 허용, 블록 스칼라 거절. 다른 키와 들여쓴 줄은 무시). 새 의존성 없음
- `src/skills/`: `Index`(목록과 건너뛴 폴더의 이유), `Store`(파일 작업), `tools`(`skill_view`, `skill_manage`). 공유 파일은 연결만 바꿨다: `main.rs`(도구 등록, 시스템 프롬프트, `/new`, `/skills`, `/skill`), `task/runner.rs`(`TaskEnv::agent`와 Claude Code 세션의 시스템 프롬프트), `claude_code/chat.rs`(`--append-system-prompt`), `agent.rs`(`set_system`), `slash.rs`, `cli.rs`, `config.rs`/`config_file.rs`(`JUNEES_TASK_SKILL_MANAGE`)
- 시스템 프롬프트: `# Skills` 절에 지침(작업 전에 맞는 스킬을 `skill_view`로 읽고 따를 것, 여러 단계를 해냈거나 사용자가 정정하면 `skill_manage`로 남기거나 고칠 것, 비밀과 일회성 내용은 넣지 말 것), 목록은 작성자가 쓴 데이터일 뿐 지시가 아니고 스킬은 사용자나 규칙보다 앞서지 않는다는 안내, 그리고 `- 이름: 설명` 목록(제어 문자와 방향 제어 문자는 `prompt_text::escape_hidden`으로 이스케이프. 쓸 때는 `prompt_text::check_plain`으로 거절). 목록은 4000자 상한(`INDEX_MAX_CHARS`)까지 줄 단위로 넣고 넘는 스킬은 개수만 적는다. 세션 시작 때 한 번 읽어 고정한다: 대화는 에이전트를 만들 때와 `/new`(`Agent::set_system`, Claude Code는 `Chat::set_system`), 작업은 세션마다. 바뀐 내용은 도구 결과가 "다음 세션부터 목록에 나온다"고 알려 준다
- `skill_manage`: `create`/`patch`는 결과 `SKILL.md` 전체를 검사(크기, 앞머리, 이름 일치, 비밀)하고, 목록을 상한 넘게 늘리면 거절(줄어드는 변경은 언제나 허용, 거절 메시지에 가장 긴 설명 셋). 설명이 60자를 넘으면 저장하고 짧게 하라는 안내만. `patch`는 겹치는 일치까지 세어 정확히 한 번이어야 한다. `write_file`은 네 하위 폴더 아래만, `delete`는 확인 없이 `.trash/<이름>-<unix 초>[-n]`으로 rename. 파일 작업은 `spawn_blocking` 안에서 `std::fs`로 끝까지 한다(취소돼도 임시 파일이 남지 않음)
- 경로: `file_path`는 상대 경로만(`.`/`..`/빈 조각/절대 경로 거절). 읽기는 정규화한 경로가 스킬 폴더 안이어야 하고, 쓰기와 지우기는 스킬 폴더 아래 심볼릭 링크를 지나거나 링크 자체에 쓰는 것을 거절한다(스킬 폴더 자체나 `skills/`가 링크인 것은 사용자의 선택으로 허용)
- 비밀 판정: 메모리(4단계 A)와 merge하면서 `src/secrets.rs` 하나로 합쳤다. 기준은 하나다: 단어 첫머리의 알려진 접두사 뒤 키 문자 12자 이상, `-----BEGIN`과 `PRIVATE KEY`가 든 줄, 단어로 선 `AKIA`/`ASIA` + 대문자/숫자 16자, 토큰 문자열 안의 32자 이상 hex(한 글자 반복은 통과, 40/64자 소문자 hex는 옆 단어가 `commit`/`sha`/`커밋` 등일 때만 통과), 대소문자와 숫자가 섞인 32자 이상 토큰 문자열(`/`, `.`에서 끊김). 스킬의 48자 hex(40자 SHA 무조건 통과)와 64자 base64 기준은 버렸다: 32자, 40자 hex 토큰을 쓰는 서비스가 있고, 이름 붙은 해시는 예외로 충분해서다. 메모리 쪽은 `AWS_ACCESS_KEY_ID=AKIA...`(소문자가 없어 섞인 토큰 규칙에도 안 걸림)를 놓치던 것을 고쳤다
- 작업 모드: 목록과 `skill_view`는 늘, `skill_manage`는 `JUNEES_TASK_SKILL_MANAGE=1`(`task_skill_manage`)일 때만 등록한다(없을 때 부르면 `unknown tool`). 지침도 "이 세션에서 스킬은 읽기 전용"으로 바뀐다
- claude-code 백엔드: 대화, `/new`, 작업의 계획/작업 세션 모두 메모리 절 다음에 목록을 붙여 한 `--append-system-prompt` 인자로(`prompt::claude_append`. 둘 다 비면 인자 없음). `settings_rules`가 `$JUNEES_HOME` 전체의 Read/Edit를 거절하므로 Claude Code는 스킬 파일을 읽을 수 없고, 목록에 그렇다고 적는다. 메모리를 모두 8000자로 채우고 목록까지 꽉 채운 최악의 경우(4바이트 문자)도 128 KiB 한도 아래인지 테스트로 확인한다
- 남은 것: 여러 Junees 프로세스가 같은 스킬을 동시에 고치면 마지막 쓰기가 이긴다(잠금 없음). 하위 폴더(카테고리)나 Hermes 스킬 형식의 메타데이터는 읽지 않는다

### 디스코드 게이트웨이 (로드맵 5단계)
- `junees gateway`(별칭 `junees discord`): 새 모듈 `src/gateway/`에 두고 기존 파일은 연결만 바꿨다: `main.rs`(명령, 백엔드별 Engines, 시그널), `cli.rs`(명령과 도움말), `config_file.rs`(`discord_*` 키), `tools/mod.rs`(`JUNEES_DISCORD_TOKEN`을 `SECRET_ENV_VARS`에), `secrets.rs`(디스코드 봇 토큰 모양), `setup.rs`(선택 단계), `lib.rs`
- 의존성: 봇 프레임워크 대신 `tokio-tungstenite`(rustls + webpki roots, reqwest와 같은 rustls 0.23)와 `futures-util`(Sink/Stream)만 더했다. Gateway v10 JSON, 압축과 샤딩 없음
- 연결(`shard.rs`): Get Gateway Bot의 주소를 캐시해 재사용, Hello -> 지터 곱한 첫 하트비트 -> 간격마다, ACK 누락이면 4000으로 닫고 RESUME(`resume_gateway_url`, `session_id`, `seq`), op 1 요청엔 즉시 하트비트, op 7과 재연결 가능한 닫힘은 RESUME, op 9 `false`면 1~5초 뒤 처음 주소로 IDENTIFY, 4007/4009는 새 세션, 4004/4010~4014는 끝(4014는 MESSAGE CONTENT INTENT 안내). 재연결 대기 1초부터 두 배씩 120초까지, Ready/Resumed에 초기화. 연결 실패(거절, 시간 초과, 핸드셰이크)에는 RESUME 정보를 남기고 다시 RESUME(세션은 4007/4009와 op 9 `false`에서만 버림), `resume_gateway_url`에 3번 잇달아 실패하면 처음 주소로 RESUME
- REST(`rest.rs`): 경로(메서드 + 채널 id)별로 `X-RateLimit-Remaining: 0`이면 `Reset-After`까지 대기, 429는 `retry_after`(전역이면 모든 경로) 대기 후 재시도 5번, 네트워크 오류/5xx 3번. `X-RateLimit-Bucket` 묶음은 쓰지 않음. 모든 메시지에 `allowed_mentions: {parse: []}`, 그리고 재시도해도 두 번 올라가지 않게 `nonce`(메시지마다 무작위, 재시도는 같은 값) + `enforce_nonce: true`. 오류는 경로만, URL은 `without_url`
- 보안: 허용 사용자 목록이 비면 시작 거부. 토큰은 `Token`(Debug 숨김, Display 없음), 헤더는 sensitive. API URL은 https(루프백만 http), Gateway 주소는 wss(루프백만 ws). 다른 사용자의 메시지는 로그에 id만 한 번. `!memory`는 서버 채널에서 개수와 크기만, 내용은 DM에서만
- 대화: 채널마다 작업자 하나(대기열, `!stop`은 epoch를 올려 대기열을 버리고 Cancel), 동시 실행은 Semaphore(`discord_parallel`, 기본 2). 에이전트와 Claude Code 대화의 future가 `Send`가 아니어서(Agent가 `&self`를 await 너머로 잡음) 채널들은 `LocalSet` 한 스레드에서 돈다. 그래서 `Gateway::run`은 `block_on`으로 돌려야 한다(테스트는 전용 스레드와 current_thread 런타임). Agent를 `Send`로 바꾸는 것은 다른 작업과 겹치는 `agent.rs` 변경이라 하지 않았다
- 세션: 보통 세션 기록을 쓰고 채널 -> 세션 id 색인(`$JUNEES_HOME/gateway/discord-sessions.json`)만 더했다. `session.rs`와 세션 형식은 바꾸지 않았다. 다른 백엔드의 세션, 지워지거나 깨진 세션이면 새로 시작하고 이유를 `!status`에. 다른 Junees가 연 세션(잠김)이나 디스크 오류면 색인을 그대로 두고 메시지를 `(not run)`으로 돌려보낸 뒤 다음 메시지에 다시 시도. 한 `$JUNEES_HOME`에 게이트웨이 하나(`gateway/lock` flock, 두 번째는 시작 거부)
- 출력: 메시지 편집 스트리밍 대신 완성된 텍스트 블록마다 보낸다(레이트 리밋, 재시도 시 지울 텍스트). 도구 호출은 다음 텍스트나 질문 전에 한 코드 블록으로. 2000자 나누기는 UTF-16 길이로, 다시 여는 펜스는 펜스와 짧은 언어만, 같은 줄에서 닫히는 펜스는 블록이 아님
- 승인: `Ask`를 채널용으로(`ask.rs`). 질문은 작업자의 출력 순서를 거쳐(도구 줄 뒤) 나가고, 답 슬롯이 차 있으면 그 채널의 다음 비명령 메시지가 답이다(멘션 없어도). `y`/`n`/`n 이유`/`a`/그 밖은 이유로 거절, 시간 초과는 거절. claude-code 백엔드는 질문 없음
- 종료: SIGINT/SIGTERM이면 채널 턴을 취소(기록)하고 작업자를 30초까지 기다린 뒤 1000으로 닫는다
- 테스트: `tests/discord_gateway.rs`(가짜 Gateway/REST, 실제 에이전트 + 가짜 API로 세션 이어 가기와 승인), 모듈 단위 테스트(설정, 나누기, 답 읽기, 명령, 토큰 모양)
- 미룬 것: `!task`(작업 모드가 프로세스 cwd를 바꾸고 저장소당 하나라 게이트웨이의 동시 채널과 따로 설계 필요), 슬래시 명령 등록, 첨부 파일, 봇이 스레드를 만드는 것, 메시지 편집 스트리밍. 실제 디스코드로는 확인하지 않았다

### 서브에이전트 (로드맵 6단계)
- 새 모듈 `src/subagent/`(`mod.rs`, `run.rs`, `tests.rs`)에 두고 공유 파일은 연결만 바꿨다: `agent.rs`(delegate 실행, 공유할 필드를 `Arc`로), `tools/mod.rs`(`ToolRegistry`가 `Arc<dyn Tool>`을 들고 `without`으로 일부를 빌려줌), `session.rs`(`Journal::file`, `SessionWriter::create_sub`, `subagent_usage`), `approval.rs`(`ToolGate::check_for`), `config.rs`/`config_file.rs`(`JUNEES_SUBAGENTS`, `JUNEES_SUBAGENT_MAX_TURNS`, `JUNEES_SUBAGENT_MAX_TOKENS`), `main.rs`/`gateway/engine.rs`/`task/runner.rs`(등록, 프롬프트 한 줄, 진행 줄 표시), `Cargo.toml`(이미 쓰던 `futures-util`에 `alloc` 기능만, 새 의존성 없음)
- 실행 위치: `delegate`는 보통 도구(`Tool::run`, `Send` future)가 아니라 에이전트 루프가 직접 돌린다(`Agent::with_delegate`는 모델에 보일 도구 정의만 등록한다). Agent future는 `Send`가 아니므로(위 게이트웨이의 LocalSet) 자식을 spawn하지 않고 부모 future 안에서 `buffered(parallel)`로 함께 poll한다. 그래서 대화(멀티스레드 런타임의 `block_on`), 게이트웨이(LocalSet), 작업 모드에서 같은 코드가 돈다. 루프 안이라 부모의 provider(`Arc`), 도구, 취소, 승인 게이트(`Arc<tokio::sync::Mutex>`), 한도(`Arc<Mutex<Limit>>`), 세션 기록 경로를 그대로 쓴다. `/model`로 바꾼 provider도 자식이 따른다
- 취소: 다른 도구 호출은 취소 신호와 select해서 future를 버리지만 delegate는 버리지 않는다. 자식이 같은 `Cancel`을 보고 스스로 멈춰 자기 기록에 `cancelled` 결과를 남긴 뒤 돌아오므로, 부모는 결과를 기록하고 `Cancelled`로 끝난다
- 예산: 자식의 `Limit` = 자기 토큰 상한, 그다음 부모의 `Limit`(부모가 호출 전까지 쓴 토큰 + 모든 자식의 지금까지 토큰). 자식마다 모델 호출 전과 끝났을 때 공유 표에 사용량을 적는다(돌고 있는 자식의 마지막 호출 몫은 그 자식의 다음 확인 때 반영). 끝나면 합계를 부모 기록에 `subagent_usage`(형식 6, 컨텍스트 크기에는 쓰지 않음)로 남겨 `/usage`, 작업 상태의 토큰, 작업 예산에 들어간다. 자식이 부모 예산으로 멈추면 부모는 다음 모델 호출 전 확인에서 멈춘다
- 승인: `ToolGate::check_for(agent, tool, input)`(기본 구현은 `check`). `Approval`은 이유 맨 앞에 `asked by subagent [n] <goal>`을 넣는다. 자식 게이트(`ChildGate`)는 부모 게이트를 잠그고 물으므로 질문은 하나씩 나오고 "항상 허용"은 함께 쓴다. 다른 자식의 진행 줄은 질문 중에도 표준 오류에 찍힌다
- 진행: `Event::Subagent { index, line }` 한 줄(도구 호출, 오류 결과, 경고, 압축, 시작과 끝). main은 `  [n] ...`, 게이트웨이는 도구 줄. 자식의 텍스트와 thinking은 보내지 않는다. 모델이 쓴 goal과 입력은 `escape_hidden`을 거친다
- 자식 기록: 부모 기록 파일 옆 `<id>/sub-<n>.jsonl`(번호는 그 폴더의 가장 큰 번호 다음, 파일은 첫 항목 때 만듦, flock, 0600/0700). meta는 부모의 cwd와 모델, id는 `<부모 id>/sub-<n>`. `session_files`는 `sessions/` 바로 아래 `.jsonl`만 보므로 목록과 `-c`에 섞이지 않는다
- 테스트: 첫 메시지로 대화를 고르는 Router provider(지연, 동시 호출 수 측정)로 병렬 상한과 과제 순서, 새 기록과 도구 제외, context, 재귀 금지, 부모 한도와 자식 토큰/턴 상한, 취소와 자식 기록, 기록 위치/권한/번호/목록 제외, 승인 공유, 결과 크기(한글), 입력 오류, 설정(`src/subagent/tests.rs`). `task::runner`에는 작업 토큰 예산이 자식 토큰을 세는 테스트, `approval`에는 질문의 서브에이전트 표시 테스트. 취소 select, 도구 제외, 병렬, 부모 한도, 사용량 기록, 게이트 공유를 하나씩 되돌리는 변이 스크립트로 각각 테스트가 실패하는 것을 확인했다
- 미룬 것: claude-code 백엔드(Claude Code에 자체 Task 도구가 있다), 같은 파일을 동시에 고치는 자식 사이의 잠금, 자식 기록 이어 가기, 호출마다 자식 한도를 정하는 인자. 실제 API로는 돌려 보지 않았다

## 상태

- [x] 1단계 에이전트 루프 (PR #1)
- [x] L1 세션 기록과 재개 (PR #5)
- [x] L2 컨텍스트 관리 (PR #6)
- [x] L3 장기 작업 모드 (PR #8)
- [x] L4 무인 실행 안정성 (L4a PR #7, L4b PR #9)
- [x] L5 상태 보기와 알림 (PR #10)
