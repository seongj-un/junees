# 고급 사용법

예전 명령과 플래그는 그대로 동작한다. `junees --help`도 첫 부분은 위 사용법, 나머지는 `Advanced` 절이다.

```sh
junees -p "이 폴더에 뭐가 있어?"        # 한 번만 실행 (위치 인자와 같음)
junees -c -p "계속"                     # 이어서 한 번만 실행 (junees -c "계속"도 됨)
junees --resume 20261109-0617           # 지정한 세션 (id 앞부분만 써도 유일하면 됨)
junees --no-session                     # 기록하지 않음
junees --image shot.png "이 오류는 뭐야?"   # 이미지와 함께 한 번만 실행 (--image 여러 번 가능)
junees task init "할 일 관리 웹 앱을 만들어"   # 계획만 (git 저장소 안에서)
junees task run --max-sessions 5        # 기능 하나씩 작업 세션 반복 (junees task 와 같음)
junees task --max-tokens 2000000 --max-time 2h   # 토큰/시간 예산 안에서만 (task run 도 같은 옵션)
junees task --max-time 30m "목표"        # 계획 + 실행에 옵션
junees task status --json               # 작업 상태 JSON (junees status --json)
junees task --backend claude-code "할 일 관리 웹 앱을 만들어"   # 이번만 Claude 구독으로 (아래)
JUNEES_BACKEND=claude-code junees task --max-time 2h
```

`cargo install` 없이 `cargo build --release` 후 `./target/release/junees`로 써도 된다. 설정 파일 없이 환경 변수만으로도 예전처럼 동작한다(`export ANTHROPIC_API_KEY=sk-ant-...`).

## 중단 (Ctrl-C, SIGTERM)

- 대화 모드: 턴 실행 중 Ctrl-C는 그 턴만 취소하고 프롬프트로 돌아온다. 모델 호출 중이면 응답을 버리고, 도구 실행 중이면 그 도구의 프로세스 그룹 전체(백그라운드로 띄운 자식 포함)를 끝낸다. 결과가 없는 도구 호출은 `cancelled by the user` 오류 결과로 채워 기록하므로(세션 파일에도) 다음 입력이 그대로 이어진다. 프롬프트에서 Ctrl-C를 연달아 두 번 누르거나 Ctrl-D를 누르면 종료한다. `/compact` 중 Ctrl-C는 압축만 취소한다(기록은 그대로). SIGTERM은 턴을 같은 방식으로 취소하고 종료 코드 143으로 끝난다. 승인 질문을 기다리는 중 Ctrl-C는 그 호출을 실행하지 않고 턴을 취소한다.
- `-p`: SIGINT/SIGTERM을 받으면 같은 방식으로 턴을 취소하고(도구 종료, 짝 채움, 세션 파일에 기록) `interrupted by SIGINT|SIGTERM`을 출력한 뒤 종료 코드 130/143으로 끝난다.
- `task init`/`task run`: 아래 장기 작업 모드 참고. 어느 모드든 두 번째 시그널은 정리 없이 바로 끝낸다(`task run`이면 다음 실행이 크래시로 보고 복구한다).
- 정상적으로 끝난 도구가 남긴 백그라운드 프로세스(`server > log 2>&1 &`)는 취소와 상관없이 그대로 둔다. 취소는 아직 실행 중인 도구에만 적용된다.

## 세션 기록

대화는 바뀔 때마다 `$JUNEES_HOME/sessions/<id>.jsonl`에 바로 한 줄씩 추가된다(기본 `~/.junees`). 프로세스가 언제 죽어도 마지막으로 기록된 지점에서 `-c`나 `--resume`으로 이어 갈 수 있다.

- id는 `20261109-061704-3fa2`처럼 UTC 시각 + 랜덤 hex라서 이름순이 생성순이다.
- 파일은 첫 입력이 기록될 때 만들어진다. 시작하고 바로 나가면 빈 세션이 남지 않는다.
- `-c`는 시작한 폴더(기록의 `cwd`)가 지금 폴더와 같고 지금 백엔드가 쓴 세션 중 마지막으로 기록된(파일 수정 시각) 것을 고른다. 없으면 새 세션을 시작한다. api 백엔드의 대화와 claude-code 백엔드의 대화는 서로 이어 갈 수 없어서(Claude Code는 Junees가 들고 있는 기록을 못 보고, 반대도 같다), `--resume`이나 `/sessions`로 다른 백엔드의 세션을 고르면 `session <id> was run by claude-code, ... continue it with `/backend claude-code``처럼 바꿀 방법을 알려 주고 열지 않는다. `/sessions` 목록에는 claude-code 세션 앞에 `[claude-code]`가 붙는다. 디스코드 채널 세션과 예약 작업 실행 세션(meta의 `source`가 있는 세션)은 `-c`가 고르지 않는다.
- `--resume`으로 다른 폴더에서 시작한 세션을 열면 `resumed <id> (N messages, started in <폴더>)`로 알려 준다(claude-code 세션은 `N Claude Code turns`). 기록 속 상대 경로는 그 폴더 기준이다.
- 세션 파일은 그 세션을 쓰는 Junees가 `flock`으로 잠근다. 아직 실행 중인 세션을 다른 터미널에서 `-c`나 `--resume`으로 열면 "session <id> is in use by another Junees" 오류로 끝나고 파일은 건드리지 않는다. 프로세스가 죽으면 잠금은 커널이 풀어 준다.
- 이어 갈 때 마지막 줄이 줄바꿈 없이 끝나고 읽히지 않으면(쓰는 도중 죽은 흔적) 그 줄만 버린다. 줄바꿈까지 있는 줄이 읽히지 않으면 위치와 상관없이 오류로 멈추고 파일을 고치지 않는다. 도구 실행 중에 끊겨서 결과가 없는 도구 호출은 `interrupted` 오류 결과로 채우고 그것도 기록한다.
- 첫 줄(`meta`)에 기록 형식 버전 `format`(현재 8)이 있고, 각 줄에 기록 시각 `ts`(unix 초)가 붙는다. 이 버전보다 새 형식의 세션은 "written by a newer Junees" 오류로 열지 않는다. 형식 1(L1) 세션도 그대로 읽고 이어 쓴다.
- 이어 쓸 때 첫 줄은 바꾸지 않으므로(덧붙이기만 함), 형식 1 세션을 이 버전으로 이어 쓴 뒤 L1 바이너리로 열면 "newer Junees"가 아니라 `line N is corrupt` 오류가 난다(파일은 그대로). 이 버전부터는 모르는 종류의 항목을 만나면 "continued by a newer Junees" 오류로 원인을 알려 준다.
- 항목 종류: `meta`, `message`, `append_text`, `usage`, 그리고 형식 2에서 추가된 `clear_tool_results`, `compact`, `summary_usage`(아래 컨텍스트 관리).
- 형식 3: 메시지 안의 thinking, redacted_thinking, 서버 도구 블록처럼 Junees가 직접 다루지 않는 블록을 API가 보낸 JSON 그대로 저장한다(아래 확장 사고). 이전 버전은 이런 블록을 API가 거절하는 자리표시로 읽기 때문에 형식을 올렸고, 이전 바이너리는 형식 3 세션을 "newer Junees" 오류로 열지 않는다. 형식 1, 2 세션도 그대로 읽는다. 이런 세션을 이 버전으로 이어 쓰면 첫 새 줄 앞에 `{"type":"format","format":4}` 항목을 한 번 덧붙인다. 이전 바이너리는 이 모르는 항목에서 "continued by a newer Junees" 오류로 멈추므로, thinking 블록을 자리표시로 잘못 읽어 400을 받는 일이 없다. 기록을 바꾸는 일도 항목으로 덧붙이기만 하고 파일을 새로 쓰지 않으므로, 재생 결과가 실행 중 기록과 같다.
- 형식 4: 결과 없이 남은 서버 도구 호출을 지우는 `drop_server_calls { ids }` 항목(아래 웹 검색). 이전 바이너리는 이 항목을 모르고 지운 호출을 다시 보내 400을 받으므로 형식을 올렸다.
- 형식 5: Claude Code가 돌린 대화(아래 구독 모드). meta 줄에 `backend: "claude-code"`(api 대화는 이 필드가 없다)가 있고, 턴마다 `claude_turn { claude_session, input, text, usage, end }` 항목이 붙는다(Claude Code 세션 id, 입력, 최종 답, 사용량, 끝난 방식). 메시지 기록은 Claude Code가 자기 세션에 들고 있으므로 Junees 기록에는 메시지가 없다. 형식 4 바이너리는 이런 파일을 빈 API 대화로 잘못 열기 때문에 형식을 올렸다(이제 "newer Junees" 오류).
- 형식 6: 서브에이전트가 쓴 토큰을 세는 `subagent_usage` 항목(아래 서브에이전트). `summary_usage`처럼 사용량에만 더하고 컨텍스트 크기에는 쓰지 않는다. 서브에이전트의 기록은 `sessions/<id>/sub-<n>.jsonl`에 따로 있다.
- 형식 7: 이미지(위 이미지 입력). `image` 블록과 이미지가 든 도구 결과(`content`가 목록)를 쓰고, 이미지는 세션 폴더의 파일을 가리킨다(`junees_file` 소스). 이미지가 든 입력을 실패한 턴 뒤 user 메시지에 붙이는 `append_content { content }` 항목도 생겼다. 형식 6 바이너리는 파일을 가리키는 소스를 API에 그대로 보내거나 목록인 `content`를 깨진 줄로 보므로 형식을 올렸다.
- 형식 8: API가 거절한 이미지를 마지막 user 메시지에서 빼는 `clear_images { note }` 항목(위 이미지 입력). 형식 7 바이너리는 이 항목을 깨진 줄로 보므로 형식을 올렸다.
- 세션 파일에는 도구 출력이 그대로 들어가므로 파일은 `0600`, 새로 만드는 `$JUNEES_HOME`과 `sessions` 폴더는 `0700`으로 만든다.
- `JUNEES_HOME`과 `HOME`이 둘 다 없으면(cron, 일부 서비스와 컨테이너) 기록 없이 실행하고 "session not saved" 한 줄로 알린다. 이때 `-c`와 `--resume`만 오류다.
- 토큰 사용량도 기록되고, 이어 가면 그 세션의 누적값부터 다시 센다.
- 기록 쓰기가 실패하면 경고를 한 번 띄우고 그 세션의 나머지는 기록하지 않는다(파일이 대화의 빈틈 없는 앞부분으로 남도록).

## 세션 검색

지난 세션에서 한 일을 찾는다. 모델 도구 `session_search`, 대화의 `/search <낱말>`, 명령줄 `junees sessions search <낱말>`이 같은 검색을 쓴다.

```bash
junees sessions search "빌드 실패" clippy        # 지금 폴더의 세션, 새것부터 5개
junees sessions search --all --limit 10 배포     # 모든 폴더, 10개까지
junees sessions search --source discord 회의록   # 디스코드 채널 세션만
```

대화 안에서는 `/search "빌드 실패" clippy`처럼 쓴다(옵션도 같다. 큰따옴표 안의 낱말은 `/search "run --all tests"`처럼 옵션 모양이어도 구절의 낱말로 읽는다). 결과는 번호, 세션 id, 마지막 기록 시각(UTC), 작업 폴더, 출처 표시(`[discord <채널>]`, `[cron <작업>]`), 맞는 곳의 한 줄 스니펫(`user:`, `assistant:`, `summary:`, `tool bash:`)이고, 끝에 `junees --resume <id>`로 이어 가라는 안내가 붙는다.

- 찾는 곳: `$JUNEES_HOME/sessions/*.jsonl`과 서브에이전트 기록(`sessions/<id>/*.jsonl`)의 사용자 입력, 모델 텍스트, 압축 요약, Claude Code 턴의 입력과 답, 도구 결과의 앞부분. 입력과 텍스트는 메시지마다 앞 64 KiB, 도구 결과는 앞 4 KiB만 본다(명령이 찍은 모든 줄이나 읽은 파일 전체가 아니라 무엇에 대한 결과인지). 비워진 도구 결과와 이전 `session_search`의 결과는 빼서, 검색이 자기 결과를 다시 찾지 않는다.
- 맞추기: 공백으로 나눈 낱말이 모두 한 메시지(텍스트 블록이나 도구 결과 하나) 안에 있어야 한다. 대소문자는 무시하고 한글은 그대로 찾는다. `"빌드 실패"`처럼 큰따옴표로 묶으면 구절로 찾는다(공백과 줄바꿈 차이는 무시). 낱말은 16개, 질의는 500자까지.
- 결과: 마지막으로 기록된(파일 수정 시각) 세션부터 기본 5개(최대 20). 세션마다 맞는 곳 3개를 240자 안팎의 한 줄로 보여 주고 나머지는 개수만 센다.
- 범위: 기본은 지금 폴더에서 시작한 세션이다(기록의 `cwd`가 같은 것, `-c`와 같은 기준이라 하위 폴더는 다른 폴더). 모델은 `all: true`, 사람은 `--all`이면 모든 폴더. `session`(사람은 `--session`)에 id나 id 앞부분을 주면 그 세션과 서브에이전트 기록만, 폴더와 상관없이 찾는다. 다만 게이트웨이, 예약 작업, task 실행의 모델 도구는 자기 작업 폴더의 세션만 찾는다: `all: true`는 거절하고, `session`도 그 폴더 안의 세션 중에서만 고른다(디스코드 채널의 답은 여러 사람이 읽고, 예약 작업의 프롬프트는 사람이 지켜보지 않으므로).
- 출처: 모델 도구는 기본으로 터미널에서 시작한 세션(대화, `-p`, task 세션)만 찾는다. 디스코드 채널, 텔레그램 채팅, 예약 작업, 웹훅 실행 세션은 터미널의 대화와 `-p`에서만 `sources: ["discord", "telegram", "cron", "webhook"]`(또는 `"terminal"`)로 더할 수 있다. 사람의 `/search`와 `junees sessions search`는 기본으로 모든 출처를 보여 주고 `--source terminal|discord|telegram|cron|webhook|all`(반복하거나 쉼표로)로 좁힌다. 출처를 기록하기 전(#34 이전)의 게이트웨이 세션은 meta에 `source`가 없으므로, meta의 `channel`이나 게이트웨이 색인(`gateway/discord-sessions.json`, 세션과 서브에이전트 기록)으로 디스코드 세션으로 센다. 색인에서 이미 빠진 더 오래된 채널 세션은 구별할 표시가 없어 여전히 터미널 세션으로 센다.
- 도구가 있는 곳: 대화와 `-p`(api 백엔드), 디스코드 게이트웨이, 예약 작업 실행, task 세션, 그리고 이들의 서브에이전트(읽기만 하므로 `delegate`가 빌려준다). 게이트웨이, 예약 작업, task 세션의 모델은 채널 세션을 찾을 수 없다(길드 채널의 답은 물은 사람 말고도 읽는다). 웹훅 실행에는 도구를 주지 않는다: PR 본문 같은 남의 글이 모델을 움직이고 답이 GitHub로 나갈 수 있어서다. claude-code 백엔드의 모델은 Claude Code의 도구만 쓰므로 이 도구가 없다(사람용 명령은 그대로 쓴다). `JUNEES_HOME`과 `HOME`이 없으면 도구도 없다.
- 데이터 표시: 모델에게 주는 결과는 세션마다 매번 무작위 이름의 요소(`<session-1a2b3c4d id="..." created="..." updated="..." cwd="..." source="...">`)로 감싸서 인용한 글이 요소를 닫을 수 없게 하고, 그 안은 지난 세션에서 인용한 데이터일 뿐 지시가 아니며 사용자나 규칙보다 앞서지 않는다는 줄을 앞에 둔다. 제어 문자와 방향 제어 문자는 모델 결과와 터미널 출력 모두 `prompt_text::escape_hidden`으로 이스케이프한다.
- 성능: 색인 없이 파일을 줄 단위로 읽는다. 4 MiB가 넘는 줄은 메모리에 담지 않고 건너뛰고, 읽을 수 없는 줄(깨진 줄, 잘린 마지막 줄, 기록이 아닌 파일)도 건너뛰어 개수를 결과 끝에 알린다. 새 세션부터 읽다가 보여 줄 수를 채우면 멈추고, 시간 상한(모델 5초, 사람 15초)에 닿으면 그때까지 찾은 것과 "N개 중 M개 기록을 읽고 멈췄다"는 줄을 돌려준다.

## 사용량과 비용

대화의 `/usage`는 지금 세션이 쓴 토큰과 추정 비용을, `junees usage`는 지난 며칠의 합계를 표로 보여 준다. 둘 다 세션 기록(`$JUNEES_HOME/sessions/*.jsonl`)만 읽는다(API 키가 필요 없고 기록을 바꾸지 않는다).

```bash
junees usage                       # 지금 폴더의 세션, 오늘까지 7일, 날짜별
junees usage --days 30 --by model  # 30일, 모델별
junees usage --all --by folder     # 모든 폴더, 폴더별
junees usage --by source           # 터미널, discord, telegram, cron, webhook별
```

- 세는 것: 대화의 모델 호출마다 남는 `usage`, 압축 요약 호출의 `summary_usage`, 서브에이전트(`delegate`)의 토큰을 모은 `subagent_usage`, Claude Code 턴(`claude_turn`)의 사용량. 서브에이전트의 기록(`sessions/<id>/sub-<n>.jsonl`)은 부모의 `subagent_usage`가 이미 세므로 보통 읽지 않는다. 호출마다 프롬프트 길이로 가격이 달라지는 모델(Haiku 5.5)의 `subagent_usage`만 예외다: 항목에는 어느 기록의 합인지 적혀 있지 않지만 `delegate`는 서브에이전트 번호를 그때 있는 가장 큰 번호 다음부터 매기므로, 기록을 번호 순으로 읽어 부모의 `subagent_usage` 항목(모든 모델, 차례를 맞추려고)마다 합이 그 항목과 정확히 같은 다음 기록들을 찾고, 찾으면 그 항목 대신 그 기록의 호출들을 센다. 그래서 같은 호출을 두 번 세지 않는다. 부모가 항목을 남기기 전에 끝난 실행의 서브에이전트 기록은 어느 항목에도 들지 않으므로 전처럼 세지 않는다. 열은 입력, 캐시 쓰기, 캐시 읽기, 출력, 웹 검색 횟수, 캐시 적중률(캐시 읽기 / (캐시 읽기 + 캐시 쓰기 + 입력)), 비용이다.
- `/usage`: 지금 세션을 종류(model calls, summaries, subagents, Claude Code turns)와 모델별로 나누고 합계를 붙인다. 기록에 없는 사용량(`--no-session`, 기록 쓰기 실패 뒤)은 `not saved`/`not in the journal` 줄로 지금 모델의 가격으로 더한다. claude-code 백엔드에서는 턴 수와 Claude Code 세션 id도 함께 나온다.
- `junees usage`: `--days N`(기본 7, 1이면 오늘만)은 오늘과 그 앞 N-1일이고, 날짜 경계는 `timezone` 설정(`JUNEES_TIMEZONE`, 없으면 시스템 시간대, 예약 작업과 같은 설정)의 자정이다. `--by day|model|folder|source`(기본 day), `--all`이면 모든 작업 폴더(기본은 지금 폴더에서 시작한 세션, `-c`와 같은 기준). 날짜별은 날짜 순, 나머지는 토큰이 많은 순.
- 비용: Claude API 공식 가격표(https://platform.claude.com/docs/en/about-claude/pricing, 2026-11-11 확인)를 코드에 두고 추정한다. 표준 가격(직접 API, 전역 라우팅. 배치, fast mode, 미국 내 추론 배율은 반영하지 않음)이고, 캐시 쓰기는 5분 캐시 가격(Junees는 `ttl` 없이 캐시 지점을 표시하므로 5분 캐시), 캐시 읽기는 모델마다 표의 값(대부분 입력의 0.1배, Opus/Sonnet 5.5는 0.05배, Fable/Mythos 5.1과 5.2는 0.025배), 웹 검색은 1000회당 10달러다. Haiku 5.5는 프롬프트가 10만 토큰을 넘는 호출마다 높은 가격으로 계산한다. 서브에이전트의 호출도 위처럼 자기 기록에서 호출마다 계산한다. 서브에이전트 기록이 없거나(만들지 못했거나 지워짐) 중간에 끊겨 합이 맞지 않는 `subagent_usage`, 그리고 기록에 없는 사용량(`not saved`/`not in the journal` 줄)은 여러 호출의 합이라 기본 가격으로 계산하고, 표 아래에 그 모델 이름과 함께 비용이 적게 잡혔을 수 있다고 알린다(`Some tokens of <모델> are only known as sums of several calls ...`). 날짜가 붙은 이전 모델 id(`claude-haiku-4-5-20251001`)는 날짜를 뗀 이름으로 찾는다.
- 모델 이름: 각 `usage` 기록 줄에 그 호출의 모델(`model`)이 함께 남는다. `/model`로 바꾸거나 다른 모델로 `-c` 이어 가도 호출마다 제 가격으로 계산한다. 이 필드가 생기기 전의 줄은 세션 첫 줄의 모델로 센다.
- 가격 덮어쓰기: `prices` 설정(`JUNEES_PRICES`)에 쉼표로 `<모델>=<입력>/<출력>` 또는 `<모델>=<입력>/<출력>/<캐시 쓰기>/<캐시 읽기>`(100만 토큰당 달러, 소수점 셋째 자리까지)를 적는다. 캐시 가격을 빼면 표에 있는 모델은 표의 배율(캐시 쓰기/입력, 캐시 읽기/입력)을 그대로 쓰고(예: `claude-opus-5-5=4/20`은 읽기가 0.05배라 표와 같다), 표에 없는 모델만 쓰기는 입력의 1.25배, 읽기는 0.1배다. 표에 없는 모델을 더하거나 표의 가격을 바꾼다. 예: `prices = "claude-opus-4-7=5/25, my-proxy-model=1/4"`.
- 모르는 모델: 가격을 모르는 모델의 토큰은 세지만 비용에서 빼고, 비용 칸에 `no price`, 표 아래에 모델 이름과 `prices`로 정하라는 안내를 붙인다.
- claude-code 백엔드: 구독으로 도는 Claude Code 턴은 비용 대신 `subscription`으로 표시하고 토큰만 센다. claude-code 백엔드의 task 세션은 세션 기록을 남기지 않으므로 `junees usage`에 들어가지 않는다.
- 큰 기록: 세션 검색과 같은 줄 읽기로 파일을 스트리밍하고, 사용량이 아닌 줄(메시지 등)은 Junees가 쓴 줄의 앞부분(`{"type":"message"`)만 보고 파싱하지 않고 넘긴다. 4 MiB가 넘는 줄은 담지 않고 건너뛰며, 읽을 수 없는 사용량 줄(깨진 줄, 잘린 마지막 줄)과 함께 개수를 표 아래에 알린다. 마지막 수정 시각이 기간 시작보다 이른 기록 파일은 열지 않는다.

## 장기 작업 모드

컨텍스트 창 하나에 담기지 않는 큰 목표를, 매번 새 컨텍스트로 시작하는 세션 여러 개가 기능 하나씩 끝내며 이어 간다. 완료 판정은 모델이 아니라 하네스가 검증 명령의 종료 코드로 한다.

- `task init "<목표>"`: git 저장소 안에서만 동작한다(자동으로 `git init`하지 않음). 이미 `.junees/features.json`이 있거나, 작업 트리에 커밋하지 않은 변경이나 추적하지 않는 파일이 있으면(`.junees/`는 제외) 오류. 초기화 세션이 목표를 검증 가능한 기능 목록으로 쪼개 `.junees/features.json`과 `.junees/progress.md`를 쓰고(필요하면 `.junees/init.sh`), 하네스가 목록을 검사한다. 이때 verify 명령을 하나씩 실행해서 아직 아무것도 만들지 않았는데 이미 통과하는 것(`true`, 테스트가 하나도 걸리지 않는 필터 등)도 거절한다. 틀리면 오류 내용을 같은 세션에 보내 한 번 고치게 하고, 그래도 틀리면(또는 세션이 실패하면) 커밋하지 않고 오류로 끝나며, 거절된 목록은 `.junees/features.rejected.json`(git 제외)으로 옮겨서 `task init`을 다시 할 수 있게 한다. 통과하면 커밋한다(커밋 방식은 아래 체크포인트와 같음). 커밋 직전에 흔한 빌드 산출물 폴더(`/target/`, `node_modules/`, `/dist/`, `/build/`, `.venv/`, `__pycache__/`) 중 저장소 루트의 그 폴더를 git이 아직 무시하지 않고(어느 `.gitignore`, `.git/info/exclude`, 전역 제외 목록이든) 추적 중인 파일도 없는 것을 저장소 루트 `.gitignore`에 더해 init 커밋에 함께 넣는다. 빌드 도구가 프로젝트 루트에 만드는 `target`, `dist`, `build`는 앞에 `/`를 붙여 루트에만 적용하므로, 더 깊은 곳의 같은 이름 소스 폴더(Java 패키지 `build`, `web/dist/` 등)는 무시되지 않는다. 계획이 실패하면 `.gitignore`는 건드리지 않는다(init 커밋 자체가 실패하면, 예를 들어 git `user.name`이 없으면, 더한 줄이 남으므로 init 세션이 남긴 다른 파일과 함께 치운 뒤 다시 실행한다). `dist/`처럼 일부러 커밋하는 산출물 폴더를 쓰는 프로젝트(JavaScript GitHub Action 등)는 init 출력의 `build output kept out of git` 줄을 보고 그 줄을 `.gitignore`에서 빼면 된다.
- `features.json`은 `[{"id": "F1", "description": "...", "verify": "<셸 명령>", "passes": false}]` 형식이다. 비어 있지 않은 배열, 유일한 id, 비어 있지 않은 verify가 조건이고 새 작업은 전부 `passes: false`여야 한다. 다른 필드(steps 등)는 그대로 둔다.
- `task run [--max-sessions N] [--max-tokens N] [--max-time 2h]`(세션 수 기본 20, 예산은 아래): 이전 실행이 크래시로 끝났으면 먼저 복구하고(아래), 작업 트리가 깨끗해야 시작한다(아니면 커밋하거나 `git stash -u` 하라는 오류. 하네스 커밋이 빼는 새 파일만 있으면 경고하고 시작한다, 아래 체크포인트 참고). 시작할 때 `features.json`에서 `passes: true`인 기능의 verify를 모두 다시 실행해서, 실패하는 것은 통과로 치지 않는다(경고와 `progress.md` 한 줄). 실패한 init이나 하네스가 되돌리기 전에 죽은 세션이 남긴 목록, 나중 변경으로 깨진 기능을 걸러 내기 위해서다. 재검증이 바꾼 것(되돌린 통과, 하네스 형식으로 다시 쓴 `features.json`, verify 명령이 남긴 파일)은 세션을 시작하기 전에 `junees: recheck passing features at run start`로 커밋한다(체크포인트와 같은 방식). 그래서 세션 없이 정지해도(이미 넘은 예산, 재검증 직후 시그널, 할 일 없음) 작업 트리가 깨끗하게 남고 다음 실행이 시작된다. 세션마다 새 에이전트(새 컨텍스트, 세션 기록은 `$JUNEES_HOME/sessions/`에 따로 남음)가 기능 목록과 통과 여부, `progress.md` 끝부분(4000자), `git log --oneline -20`을 system 프롬프트로 받고, 안 끝난 기능 하나를 구현, 검증, 커밋하고 `progress.md`를 갱신한다.
- 기능 통과는 `mark_feature_passing {id}` 도구로만 바뀐다. 하네스가 그 기능의 verify 명령을 저장소 루트에서 bash 도구와 같은 방식(출력 제한, API 키 변수 제거, 타임아웃 600초)으로, `set -o pipefail`을 앞에 붙여 실행하고(`cargo test 2>&1 | tail`도 테스트가 실패하면 실패) 종료 코드 0일 때만 `passes: true`로 저장한다. 실패하면 출력과 함께 오류 결과를 돌려준다.
- 하네스는 기능 목록의 권위 있는 사본을 들고 있다. 세션이 `features.json`을 직접 고치면(지우거나 `passes`를 바꾸는 것 포함) 세션이 끝날 때 사본으로 되돌려 쓰고 경고와 `progress.md` 한 줄을 남긴다. 서식만 다른 것은 편집으로 치지 않는다.
- 세션이 끝나면 하네스가 `progress.md`에 `- [junees] session N: 통과 수/전체, 새로 통과한 기능, 토큰` 한 줄을 붙이고, 작업 트리가 더러우면 `junees: checkpoint after session N`으로 커밋한다(하네스 줄이 붙으므로 사실상 세션마다 하나). 하네스의 커밋(init, 재검증, 크래시 복구, 체크포인트)은 `git add -A`에서 이름이 비밀 파일 같은 새 파일(git이 추적 중인 파일을 옮기거나 복사한 것으로 보는 파일도 포함. 예: `.env.example`을 `.env`로 옮겨 값을 채운 경우)(`.env*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa`/`id_ed25519` 등 ssh 키, `.netrc`)과 1 MiB가 넘는 새 파일(verify가 남긴 빌드 결과물 등. 추적 중인 큰 파일을 옮기기만 한 것은 새 파일로 치지 않는다)을 빼고, 파일마다 이유를 붙여 경고한다. 이미 추적 중인 파일은 크기와 이름에 상관없이 그대로 커밋한다. 이렇게 빠지는 파일(추적하지 않는 비밀 같은 파일, 1 MiB가 넘는 파일)만 남아 있으면 다음 실행은 그 파일들을 `not committed: ...` 경고로 알리고 그대로 시작한다. 하네스 커밋이 그 파일들을 계속 빼므로 사용자의 작업이 커밋에 쓸려 들어갈 일이 없고, 커밋해야 할 큰 파일(lock 파일, 픽스처 등) 하나 때문에 무인 실행이 멈추지 않게 하기 위해서다(직접 커밋하거나 `.gitignore`에 넣거나 치우면 경고가 사라진다). 다른 변경이 함께 있어 실행을 거절할 때는 거절 메시지에 그런 파일을 세 개까지 이유와 함께 보여 주고 나머지는 개수로 알린다. 세션이 `.junees/`나 `.junees/.gitignore`를 지워도 커밋 전에 다시 만든다.
- 정지: 모든 기능 통과, `--max-sessions`만큼 실행, 연속 3세션 동안 새로 통과한 기능 없음(stalled), 세션 하나가 오류로 끝남(공급자 오류 등. 오류를 `progress.md`에 남기고 체크포인트 커밋 후 정지), 세션 뒤 하네스 작업(되돌리기, progress 기록, 커밋)이 실패함, 예산 소진(`budget`), 시그널(`interrupted`), claude-code 백엔드의 구독 사용 한도(`limit`, 아래 구독 모드). 정지 이유를 `task stopped: ...`로 출력하고 `state.json`에 기록한다. stalled와 오류는 종료 코드 1, 예산 소진과 사용 한도는 `--max-sessions`처럼 0, 시그널은 130(SIGINT)/143(SIGTERM).
- 예산: `--max-tokens N`(기본값 `JUNEES_TASK_MAX_TOKENS`)은 작업의 모든 세션(init 세션과 요약 호출 포함)이 쓴 토큰의 합이다. 입력, 캐시 쓰기, 캐시 읽기, 출력 토큰을 모두 센다(캐시 읽기도 1토큰으로 센다. 비용이 아니라 처리한 양 기준). `state.json`의 누적값에서 이어 세므로 다시 실행해도 줄지 않는다(늘리려면 더 큰 값을 주고 실행). `--max-time`(기본값 `JUNEES_TASK_MAX_TIME`, `90s`, `30m`, `2h`, `1h30m` 형식)은 이번 `task run`이 시작한 뒤 흐른 실제 시간이다. 예산은 세션을 시작하기 전과, 세션 안에서는 모델 호출마다 그 직전에 확인한다(컨텍스트 압축의 요약 호출도 모델 호출로 따로 확인하므로, 요약 직후 예산이 다 되면 다음 호출 없이 끝난다). 시작할 때의 재검증도 verify 명령마다 그 직전에 예산을 확인해서(명령 하나가 최대 600초), 넘으면 남은 재검증 없이 목록을 그대로 두고 `budget`으로 정지한다. 확인하지 못한 통과는 확정되지 않았으므로 작업 완료로 치지 않는다. 세션 중에 넘으면 그 세션은 다음 모델 호출을 하지 않고 끝나며(진행 중인 도구는 끝까지 실행), 다른 세션과 똑같이 기능 목록 복원, `progress.md` 한 줄(`ended early: token budget used up: ...`), 체크포인트 커밋을 거친 뒤 `budget`으로 정지한다. 한 번의 모델 호출이 쓰는 양만큼은 넘을 수 있다.
- 시그널: `task run` 중 SIGINT(Ctrl-C)나 SIGTERM을 받으면 실행 중인 도구의 프로세스 그룹을 끝내고, 짝 없는 tool_use를 `cancelled by the user` 오류 결과로 채운 뒤, 그 세션을 보통 세션처럼 마무리(기능 목록 복원, `progress.md`에 `interrupted` 한 줄, 체크포인트 커밋)하고 `interrupted`로 정지한다. 시작할 때 verify를 다시 실행하는 중이었으면 아무것도 바꾸지 않고 정지한다. `task init` 중이면 init이 실패한 것과 같이 처리하고 130/143으로 끝난다.
- 동시 실행 방지와 크래시 복구: `task run`은 실행하는 동안 저장소 루트 폴더에 `flock`을 잡고(close-on-exec라서 세션이 남긴 백그라운드 프로세스는 잠금을 물려받지 않음. 잠금은 폴더의 inode에 걸리므로, 세션이 지웠다가 하네스가 다시 만들 수 있는 `.junees/`가 아니라 루트에 건다), `state.json`에 실행 중 표시(`running`: pid, 시작 시각)를 남긴다. 다른 실행이 잠금을 잡고 있으면 그 pid와 시작 시각을 알려 주며 거절한다. 정상 정지(위 모든 정지 이유)에서는 표시를 지운다. 시작할 때 표시가 남아 있는데 잠금은 비어 있으면 이전 실행이 죽은 것(kill -9, 전원 등)으로 보고 먼저 복구한다: `features.json`의 기능 정의(id, 설명, verify)를 작업의 init 커밋(가장 오래된 `junees: initialize task` 커밋, `git replace`는 무시)에서 가져와 되돌리고, `state.json`의 하네스 사본과 파일에서는 `passes: true`인 기능만 가져와서 시작 시 재검증이 init 커밋의 verify로 확인한다. `state.json`도 세션이 고칠 수 있는 파일이라 그 사본의 정의는 믿지 않는다(사본의 정의가 init 커밋과 다르면 경고와 `progress.md` 기록). 그래서 init 뒤에 손으로 고친 기능 정의는 크래시 복구 때 init 커밋 기준으로 돌아가니 다시 적용해야 하고, 이력에 init 커밋이 없으면(squash 등) 복구를 거절한다. 이어서 `progress.md`에 한 줄, 작업 트리가 더러우면 `junees: recover after crash`로 커밋(체크포인트와 같은 방식). 그다음 보통 실행처럼 이어 간다.
- 실행 로그: `.junees/run.log`에 실행 시작(pid, 통과 수, 예산 사용량), 세션 시작/끝(요약), 크래시 복구, 정지 이유가 UTC 시각과 함께 한 줄씩 쌓인다(`tail -f .junees/run.log`). git에서는 뺀다. 이 로그가 생기기 전에 만든 작업은 첫 실행 때 `.junees/.gitignore`에 `run.log`를 더하고 `junees: keep the run log out of git`로 따로 커밋한다.
- `.junees/state.json`(실행한 세션 수, 연속 무진전 세션 수, 누적 토큰, 정지 이유와 시각 `stopped_at`, init 때의 목표 `goal`, 실행 중 표시, 기능 목록의 하네스 사본)은 `.junees/.gitignore`로 git에서 뺀다. 다음 `task run`은 세션 번호와 토큰을 여기서 이어 간다. 이전 실행이 스스로 멈췄으면(`--max-sessions`, 예산, 시그널 포함) 무진전 횟수를 0부터 다시 세고, 정지 이유 없이 끝났으면(크래시) 이어서 센다. 그래서 `--max-sessions 1`로 여러 번 나눠 실행하면 stalled로는 멈추지 않는다. 크래시 난 세션이 쓴 토큰은 세션이 끝나야 더해지므로 누적값에 들어가지 않는다.
- 하위 폴더에서 실행해도 저장소 루트로 옮겨서 작업한다. 하네스의 git 명령과 세션의 `bash`(verify 포함)에서는 `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE` 같은 저장소 지정 변수와 상위 git이 넘기는 설정(`GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`)을 지운다. git 훅이나 worktree의 `git rebase -x`처럼 이 변수가 설정된 곳에서 실행해도 다른 저장소에 커밋하지 않게 하기 위해서다. 하네스의 git은 세션이 심었을 수 있는 훅과 fsmonitor를 실행하지 않고(`-c core.hooksPath=/dev/null -c core.fsmonitor=false`, 커밋은 `--no-verify`), API 키 변수와 Claude Code 자격 증명(`CLAUDE_CODE_OAUTH_TOKEN` 등, 아래 기본 도구 절의 목록)도 받지 않는다. 세션당 모델 호출 상한은 `JUNEES_MAX_TURNS`이고, 넘으면 그 세션만 끝난다. 작업은 체크포인트로 남고 다음 세션으로 넘어가며, 진전이 없으면 stalled 규칙이 멈춘다.
- `task status [--json]`: 저장소의 `.junees/`를 읽기만 하고(아무것도 쓰지 않으므로 실행 중인 `task run` 옆에서 써도 된다) 목표, 통과 수/전체와 진행 막대, 기능별 통과 여부와 설명 첫 줄(72자에서 자름), 세션 수와 연속 무진전 수, 누적 토큰(입력/출력/캐시 쓰기/캐시 읽기), 마지막 정지 이유와 시각, `progress.md`의 비어 있지 않은 마지막 5줄을 보여 준다. API 키가 필요 없다. `.junees/`가 없으면 `task init`을 먼저 하라는 오류. 실행 중 여부도 보여 준다: `state.json`의 실행 중 표시(pid, 시작 시각)와 그 pid의 프로세스가 살아 있는지(`kill(pid, 0)`)로 판단하고, 실행 잠금은 잡지 않는다(잡으면 그 순간 시작하는 `task run`이 거절되므로). 표시는 있는데 프로세스가 없으면 크래시로 보고 다음 `task run`이 복구한다고 알려 준다(드물게 pid가 재사용되면 크래시가 실행 중으로 보일 수 있다. `task run` 자신은 잠금으로 판단하므로 영향 없음). 실행 표시가 있는 동안에는 세션이 `features.json`을 직접 고쳤을 수 있으므로 기능 목록과 통과 수를 `state.json`의 하네스 사본(하네스가 마지막으로 저장한 것)에서 가져온다. 그래서 진행 중인 세션이 새로 통과시킨 기능은 세션이 끝나야 보인다.
- `task status --json` 필드: `repo`(저장소 폴더 이름), `root`(저장소 경로), `goal`(문자열 또는 `null`: 목표가 기록되기 전에 만든 작업), `passing`, `total`, `features`(`[{id, passes, description}]`, 설명 전체), `sessions`, `stalled`, `tokens`(`{input, output, cache_write, cache_read}`), `stop`(`null` 또는 `{reason, session, detail, resets_at, message, at}`: `reason`은 `complete`/`max_sessions`/`stalled`/`error`/`budget`/`interrupted`/`limit`로 `state.json`과 같은 값, `detail`은 오류 내용이나 다 쓴 예산 설명, `limit`이면 Claude Code의 메시지, 또는 `null`, `resets_at`은 `limit` 정지에서 사용 한도가 풀리는 시각(unix 초) 또는 `null`, `message`는 사람이 읽는 한 문장, `at`은 unix 초 또는 `null`), `features_from`(`features.json` 또는 실행 표시가 있을 때 `harness`), `running`(`true`/`false`, 지금 실행 중인지), `run`(`null` 또는 `{pid, started, alive}`: `state.json`의 실행 중 표시, `started`는 unix 초, `alive`가 `false`면 크래시로 남은 표시), `progress_tail`(문자열 배열).
- 알림: `JUNEES_NOTIFY_WEBHOOK`에 디스코드 웹훅 URL을 넣으면 `task run`이 정지 이유를 기록한 뒤 웹훅에 한 번 POST한다. 내용은 저장소 이름, 정지 이유(키와 문장), 통과 수/전체, 세션 수, 누적 토큰. `JUNEES_NOTIFY_EACH_SESSION=1`이면 세션이 끝날 때마다 `session N done: ...` 한 줄도 보낸다 세션 알림도 그 세션의 토큰과 (마지막 세션이면) 정지 기록을 `state.json`에 저장한 뒤에 보내므로, 알림을 기다리는 동안 프로세스가 죽어도 기록은 남는다. 예산 소진(`budget`)과 시그널(`interrupted`) 정지도 같은 형식으로 알린다. 시그널을 받은 뒤에는 세션 한 줄 알림은 건너뛰고 정지 알림만 보낸다(두 번째 시그널은 알림을 기다리지 않고 바로 끝낸다). 본문은 `{"content": "...", "allowed_mentions": {"parse": []}}`(멘션 알림 없음)이고, 저장소 이름, 정지 문장, 세션 요약, 오류처럼 바뀌는 부분은 인라인 코드(`` ` ``)로 감싸 마크다운 링크나 서식이 적용되지 않게 한다(백틱은 `'`로, 줄바꿈은 공백으로 바꾸고 한 부분은 1500자까지). `content`는 2000자에서 자른다(UTF-16 단위로 세어 이모지도 안전). 요청 하나의 제한 시간은 10초, 리다이렉트는 따라가지 않는다. 실패(연결 실패, 시간 초과, 2xx가 아닌 응답)는 `!! notification to <호스트> failed: ...` 경고만 띄우고 종료 코드는 바꾸지 않는다. URL에는 웹훅 토큰이 들어 있어서 경고와 오류에는 호스트만 나오고, 세션의 `bash`와 하네스 git에도 이 변수를 넘기지 않는다. 정지를 기록하기 전에 실패하면(작업 트리가 더러움, 기능 목록을 읽지 못함, 상태 파일을 쓰지 못함 등) `task in <저장소> run failed: <오류>` 한 줄을 대신 보낸다. 설정 오류(API 키 없음 등)처럼 `task run`이 작업을 열기도 전에 끝나는 경우는 알림이 없다.
- 세션의 `bash`와 verify 명령에는 대화 모드와 같은 명령 정책과 숨김 변수 설정(`JUNEES_DENY_COMMANDS`, `JUNEES_ALLOW_DANGEROUS`, `JUNEES_HIDDEN_ENV`)이 걸리고, 세션의 컨텍스트 관리도 같은 설정(`JUNEES_CONTEXT_WINDOW`, `JUNEES_COMPACT_THRESHOLD`)을 쓴다. 정책에 막힌 verify 명령은 실패로 친다.

## 스트리밍 출력

모델 응답은 SSE 스트리밍(`stream: true`)으로 받는다. stdout이 터미널이면 텍스트를 도착하는 대로 찍는다(대화 모드, `-p`, `task` 세션 모두). 응답이 끝난 뒤 다시 찍지 않으므로 텍스트는 한 번만 나온다. stdout이 터미널이 아니면(`junees -p ... > answer.txt`, 파이프) stdout이 곧 결과이므로, 모델 호출 하나의 텍스트를 호출이 끝날 때까지 모아 두었다가 한 번에 찍는다. 그래서 재시도로 버려진 시도의 조각은 결과에 남지 않고, 출력은 `JUNEES_STREAM=0`일 때와 같다(스트리밍의 대기 제한 이점은 그대로). 취소나 오류로 호출이 중간에 끝나면 받은 데까지 찍고 줄을 끝낸다. thinking 블록은 지금처럼 내용 없이 `(thinking...)`만, 도구 호출과 결과도 지금처럼 stderr에 나온다. `JUNEES_STREAM=0`이면 응답을 한 번에 받아 끝난 뒤 찍는다. 압축 요약 호출은 늘 한 번에 받는다.

- 조립: 이벤트(`message_start`, `content_block_start/delta/stop`, `message_delta`, `message_stop`)로 스트리밍하지 않은 응답과 같은 값을 만든다. 블록은 index 순서, 도구 입력은 `input_json_delta` 조각을 이어 블록이 끝난 뒤 JSON으로 읽고, thinking 블록은 `thinking_delta`와 `signature_delta`로 signature까지 채운다. 모르는 블록 종류(서버 도구 결과 등)는 시작 이벤트의 원본 JSON 그대로 남는다. `message_delta`의 usage는 누적값이라 더하지 않고 덮어쓴다(입력 쪽 값이 없으면 `message_start` 값 유지). `ping`과 모르는 이벤트, 모르는 delta 종류는 무시한다(API 버전 정책상 새 종류가 생길 수 있다). 출력 한도(`max_tokens`)에 걸려 도구 입력 JSON이 잘렸으면 입력을 `{}`로 둔다(어차피 실행하지 않는다).
- 재시도: 스트림이 끊기거나(연결 끊김, `message_stop` 전에 끝남), 재시도할 만한 `error` 이벤트(`overloaded_error` 등, 같은 오류의 HTTP 상태로 판단: 429, 5xx, 529)가 오거나, 다음 이벤트가 300초 동안 오지 않으면 기존 정책(최대 4번, 0.5초부터 두 배 대기)대로 처음부터 다시 요청한다. 이미 텍스트나 `(thinking...)`을 보여 줬으면 줄을 끝내고 stderr에 `(retrying)`을 찍은 뒤 다시 받은 응답 전체를 찍는다(터미널에서는 끊긴 조각이 화면에 남고, 터미널이 아니면 조각은 버린다). 부분 응답은 기록하지 않는다. 끊긴 시도의 usage도 버리고 성공한 시도의 usage만 센다. 끊기기 전까지 만든 출력이 과금되는지는 문서에 나와 있지 않아서, 재시도가 있었던 호출은 대화의 `/usage`와 task 예산이 실제보다 조금 적게 셀 수 있다(호출당 재시도는 많아야 3번). `invalid_request_error` 같은 오류 이벤트는 같은 상태의 HTTP 오류처럼 바로 실패한다(컨텍스트 초과도 같은 방식으로 감지).
- 시간 제한: 스트리밍 호출에는 전체 시간 제한이 없고, 응답 시작과 다음 이벤트(`ping` 포함)까지의 대기만 300초로 제한한다. 문서는 오래 걸리는 요청에 스트리밍을 권하고, 도구 입력은 값 하나를 다 만든 뒤에 보내므로 큰 파일을 쓸 때 이벤트 사이가 길 수 있다. 300초는 예전 전체 제한과 같아서, 예전에 끝나던 호출은 여기서 시간 초과되지 않는다. 이벤트가 오는 한 길게 이어지는 것은 대화와 task 세션의 모델 호출뿐이다. 스트리밍하지 않는 호출(컨텍스트 압축의 요약 호출, `JUNEES_STREAM=0`)은 지금처럼 요청 전체가 300초로 제한된다.
- 취소: Ctrl-C/SIGTERM으로 턴을 취소하면 스트림 연결을 바로 닫는다. 찍던 줄은 끝내고, 응답은 기록하지 않는다.
- 스트리밍을 요청했는데 서버(프록시 등)가 `text/event-stream`이 아닌 JSON 응답 전체를 주면 그대로 읽는다.

## 확장 사고

`JUNEES_THINKING_BUDGET`을 1024 이상으로 주면 모델 호출마다 `thinking: {"type": "enabled", "budget_tokens": N}`을 보낸다(`task` 세션 포함, 압축 요약 호출 제외). 사고 토큰도 출력 한도에 들어가므로 예산은 `JUNEES_MAX_TOKENS`보다 작아야 한다.

- 응답의 `thinking`, `redacted_thinking` 블록은 내용 대신 `(thinking...)` 한 줄로만 알린다.
- 이 블록들(그리고 서버 도구 블록 같은, Junees가 직접 다루지 않는 모든 블록)은 받은 JSON 그대로 기록과 세션 파일에 남고, 다음 요청에 그대로 다시 보낸다. 도구 호출 턴의 thinking 블록은 `signature`까지 고치지 않고 돌려보내야 API가 받아 준다.
- 도구 결과 비우기와 압축은 기록 전체의 `thinking`, `redacted_thinking` 블록을 지운다(텍스트와 도구 호출은 남긴다). Claude 5.1 이후 모델은 돌려받은 thinking 블록 앞(시스템 프롬프트, 도구, 앞선 메시지)이 그 블록을 만들 때와 같은지 확인하고, 달라졌으면 그 블록과 뒤의 모든 thinking 블록을 거절한다(400). 비우기와 압축은 앞부분을 고치므로, 남길 수 있는 thinking 블록이 없다. 그 뒤에 나온 thinking 블록은 다음 변경 때까지 그대로 보낸다. Claude 5 모델은 `JUNEES_THINKING_BUDGET=0`이어도 기본으로 생각하므로 이 규칙이 늘 적용된다.
- 예산을 켠 수동 모드는 예외다. 이 방식을 받는 모델(Claude 4.6 이하)은 앞부분 검사를 하지 않는 대신, 진행 중인 턴이 thinking 블록으로 시작해야 하고 thinking은 턴의 첫 응답에만 나온다. 그래서 비우기와 압축이 thinking 블록을 지우지 않고(기록 항목에 `keep_thinking: true`), 입력이 하나뿐인 턴 안에서 압축할 때는 꼬리를 남기지 않고 마지막 도구 결과까지 턴 전체를 요약해 `user(요약 + 원래 입력)` 하나로 만든다. 모델은 그 턴을 thinking부터 다시 시작한다. 사용자 입력 앞에서 자르는 경우는 같다.
- 응답이 사고 중에 출력 한도(`max_tokens`)에 걸려 thinking 블록만 남으면 그 응답은 기록하지 않고, `JUNEES_MAX_TOKENS`를 올리거나 `JUNEES_THINKING_BUDGET`을 낮추라는 경고를 띄운다(기록은 입력으로 끝나 다음 입력이 거기에 붙는다). 한도에 걸리지 않았는데 thinking만 온 응답도 기록하지 않고, 모델이 생각만 하고 답하지 않았다는 경고를 띄운다.
- `type: "enabled"` 방식은 Claude 4.6 모델(기본 `claude-sonnet-4-6`)에서는 동작하지만 폐기 예정이고, 4.7 이후 모델은 400으로 거절한다. 그런 모델에서는 켜지 않는다. 켜면 오류 메시지 끝에 `JUNEES_THINKING_BUDGET`을 0으로 하라는 안내가 붙는다(adaptive thinking은 아직 지원하지 않음. Claude 5 모델은 설정 없이도 adaptive로 생각한다).

## 웹 검색

`JUNEES_WEB_SEARCH=on`이면 모델이 Anthropic의 서버 도구 `web_search`로 웹을 검색할 수 있다(대화 모드, `-p`, `task` 세션, `api` 백엔드). 검색은 API가 실행하고 Junees는 실행하지 않는다. 기본은 꺼짐이다. 검색은 토큰과 별도로 1000회당 10달러가 청구되고, 검색 결과도 입력 토큰으로 계속 쌓이며, 조직 관리자가 Console에서 웹 검색을 꺼 두었으면 요청 전체가 400으로 거절되기 때문이다.

- 요청: 도구 목록 끝(Junees 도구 다음)에 `{"type": "web_search_20250305", "name": "web_search", "max_uses": N}`을 넣고, 시스템 프롬프트 뒤에 "최신 정보가 필요하면 web_search로 찾고 출처를 밝히라"는 한 줄을 덧붙인다. 압축 요약 호출에는 넣지 않는다. 기본 버전을 쓰는 이유: `web_search_20260209` 이후 버전은 기본으로 코드 실행으로 결과를 거르므로(dynamic filtering) `pause_turn` 때 컨테이너 id를 돌려보내야 하고 블록 종류가 늘며, ZDR 대상이 아니고 일부 플랫폼에 없다. `allowed_callers: ["direct"]`로 쓰면 기본 버전과 같은 검색이다.
- `max_uses`(`JUNEES_WEB_SEARCH_MAX_USES`, 기본 5)는 한 턴의 검색 횟수 상한이다. 넘으면 API가 `max_uses_exceeded` 오류 결과를 주고 모델이 처리한다. `JUNEES_WEB_SEARCH_ALLOWED_DOMAINS` 또는 `JUNEES_WEB_SEARCH_BLOCKED_DOMAINS`(쉼표 구분, 둘 다는 안 됨)로 도메인을 제한한다. `https://` 같은 스킴, ASCII가 아닌 글자(비슷한 모양의 키릴 문자 등), 도메인 부분의 `*`는 시작할 때 오류다(경로의 `*`는 된다: `example.com/blog/*`).
- 응답의 `server_tool_use`, `web_search_tool_result`(암호화된 `encrypted_content` 포함)는 원본 그대로 기록과 세션 파일에 남기고 다음 요청에 그대로 보낸다(문서상 빠지거나 바뀌면 400). 클라이언트 도구로 실행하지 않고, 검색 결과의 오류 코드(`max_uses_exceeded`, `too_many_requests` 등)도 모델이 처리하도록 그대로 둔다. 텍스트 블록의 `citations`도 원본 그대로 보존해 다시 보낸다(`encrypted_index`를 돌려보내야 한다).
- `stop_reason: "pause_turn"`(API의 서버 쪽 도구 반복이 한도에 걸려 턴을 멈춤)이면 그 응답을 기록하고, 사용자 메시지를 넣지 않고 같은 대화로 다시 요청해 이어 가게 한다. 연속 5번(`MAX_PAUSES`)까지이고, 넘으면 경고를 띄우고 멈춘다. 다음 입력은 멈춘 응답 뒤에 새 user 메시지로 붙는다.
- 결과 없이 남은 검색: 멈춘 응답은 결과 블록 없는 `server_tool_use`로 끝날 수 있고(이어 가면 다음 응답이 그 결과로 시작한다), 검색과 클라이언트 도구를 함께 부른 응답의 검색은 도구 결과만 든 다음 요청에서 실행된다. 그 전에 턴이 끝나면(pause 상한, 취소, 오류, 턴 한도) 새 입력이 그 뒤에 붙는 순간 API가 400(``web_search` tool use with id ... was found without a corresponding `web_search_tool_result` block``, handling stop reasons 문서의 tool_use 절)으로 거절한다. 그래서 새 입력을 붙이기 전에 기록 어디에도 결과가 없는 `server_tool_use`(와 `mcp_tool_use`)를 지우고 `drop_server_calls` 항목으로 남긴 뒤 `dropped N web search call(s) ...` 경고를 띄운다. 호출을 지워 thinking만 남은 응답은 통째로 지우고, 그 앞뒤 user 메시지는 하나로 합친다. 열린 호출은 보통 마지막 응답의 thinking 블록 뒤에 있어 지워도 thinking 블록 앞부분이 바뀌지 않는다. 마지막 응답이 아닌 메시지에 열린 호출이 있으면(그 뒤로 대화가 이어진 이전 Junees의 기록) 그 뒤 응답의 thinking 블록 앞부분이 바뀌어 5.1+ 모델이 거절하므로, `clear_tool_results { upto: 0 }` 항목을 하나 더 남겨 도구 출력은 그대로 두고 thinking 블록을 모두 지운다(형식 2부터 있는 항목이라 형식은 그대로. 수동 thinking 예산 모드는 이 검사를 하지 않으므로 지우지 않는다). 닫는 결과 블록을 지어 넣지 않는 이유: 검색 결과는 API가 만든 값(`encrypted_content` 등)이라 Junees가 만들 수 없다. 검색을 끄고 `-c`로 이어 가도 같다(검색 도구가 없는 요청에 열린 호출이 남으면 ``but no `web_search` tool was provided``로 거절된다). 이어 가는 요청도 `JUNEES_MAX_TURNS`에 센다.
- 출력: 검색은 stderr에 `-> web_search {"query": ...} (run by the API)`로 응답이 오는 대로 나온다. 답이 끝나면 인용된 URL을 중복 없이 처음 나온 순서로 stdout에 `Sources:` 목록으로 찍는다(10개까지, 나머지는 개수). 출처도 답의 일부로 보므로 `-p` 결과를 파일이나 파이프로 받으면 이 목록이 답 뒤에 붙는다.
- 사용량: `usage.server_tool_use.web_search_requests`를 세션마다 더해 `/usage`에 `web searches N`으로, `task run`의 실행 로그(run.log) 정지 줄에 `web searches N`으로 보여 준다. 토큰 예산(`--max-tokens`)에는 들어가지 않는다.
- 컨텍스트: 검색한 호출의 사용량은 컨텍스트 크기로 쓰지 않는다. 그 호출의 캐시 읽기에는 API의 검색 반복 단계마다 읽은 양이 모두 들어 있어 실제보다 훨씬 커서, 압축이 너무 일찍 일어난다(context editing 문서의 서버 도구 주의). 대신 그 응답(검색 결과 포함)은 아래 추정치로 크기에 더해지므로, 매 호출이 검색하는 세션도 제때 압축된다. 검색하지 않은 다음 호출이 다시 잰다. 도구 결과 비우기는 검색 결과 블록을 건드리지 않는다. `encrypted_content`는 바꾸면 400이고, Junees가 대신 넣을 수 있는 유효한 자리표시가 없으며, 블록 짝을 지우면 뒤 텍스트의 인용이 가리킬 곳이 사라진다. 검색 결과는 압축 때 요약으로 정리된다(요약 텍스트에는 검색어와 앞 5개 결과의 제목/URL만 넣는다). 검색을 클라이언트 도구와 같은 응답에서 부르면 검색은 그 도구 결과를 보낸 다음 요청에서 실행되어 결과 블록이 다음 응답에 오는데, 턴 안 압축은 이 둘 사이를 자르지 않는다(검색 호출 없이 결과만 남지 않도록).
- 조직에서 웹 검색이 꺼져 있다는 400이 오면 오류 끝에 `JUNEES_WEB_SEARCH=off`로 하거나 관리자에게 켜 달라는 안내가 붙는다.

## 이미지 입력

모델에게 그림을 보여 준다. 방법은 넷이고 모두 api 백엔드에서만 된다.

- 모델이 `read_file`로 PNG, JPEG, GIF, WebP 파일을 읽으면 내용 대신 이미지 자체가 도구 결과로 간다(결과 `content`가 텍스트 블록 + `image` 블록 목록, [도구 결과 문서](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls)). 형식은 파일 이름이 아니라 앞부분 바이트(매직 바이트)로 알아본다. `offset`/`limit`은 이미지에 쓰이지 않는다.
- 대화에서 `/image <경로> [질문]`: 이미지와 질문을 한 입력으로 보낸다. 공백이 든 경로는 `/image "내 사진.png" 이건 뭐야?`처럼 큰따옴표로, `~/`는 홈. 질문이 없으면 `(sent with no text)`가 텍스트로 간다.
- `-p`에 `--image <경로>`(여러 번): `junees --image a.png --image b.png "둘의 차이는?"`. 프롬프트 없는 `--image`는 오류. 잘못된 이미지는 서버를 띄우거나 세션을 만들기 전에 오류로 끝난다.
- 디스코드: 허용된 사용자의 메시지에 붙은 이미지(아래 디스코드 게이트웨이). 텔레그램도 같다(사진과 이미지 문서, 아래 텔레그램 게이트웨이).
- 입력의 이미지는 텍스트 앞에 둔다([vision 문서](https://platform.claude.com/docs/en/build-with-claude/vision): "Claude works best when images come before text"). API에는 base64 소스(`{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": ...}}`)로 보낸다.
- 상한: 파일 5 MB(base64로 약 7 MB. API는 이미지당 base64 10 MB, Bedrock/Vertex는 5 MB)와 가로세로 8000 px(문서의 최대). 넘으면 이유와 줄이는 방법(ImageMagick 예)을 알려 주고 보내지 않는다. 크기는 헤더(PNG IHDR, GIF 화면 크기, WebP VP8/VP8L/VP8X, JPEG SOF)에서 읽고 픽셀은 디코딩하지 않는다. 리사이즈는 하지 않는다(이미지 라이브러리가 필요해서).
- 세션 기록: 이미지는 세션 폴더의 파일 `$JUNEES_HOME/sessions/<id>/images/<sha256>.<확장자>`(폴더 0700, 파일 0600, 같은 이미지는 한 번만)에 두고 기록 줄에는 그 파일을 가리키는 블록(`"source": {"type": "junees_file", "media_type": ..., "sha256": ...}`)만 남긴다. 이어 갈 때 파일을 다시 읽어 붙이고, 파일이 없거나 내용이 바뀌었으면(해시가 다르면) 그 자리에 `[image ... could not be read ...]` 같은 한 줄을 넣고 이어 간다. 기록 형식 7(위 세션 기록, 아래 거절된 이미지는 형식 8).
- 컨텍스트: 이미지 크기는 base64 길이가 아니라 픽셀로 추정한다(`ceil(가로/28) x ceil(세로/28)`, 긴 변 2576 px로 줄인 뒤, 최대 4784토큰: 문서의 고해상도 등급 기준이라 다른 모델에서는 크게 잡힌다). 도구 결과를 비우면 그 결과의 이미지도 지운다. 압축 요약 호출에는 이미지 대신 `[user: image, image/png, 640x480 px]` 같은 줄이 가고, 한 턴 안에서 자를 때 그 턴의 입력에 붙은 이미지는 입력 글과 함께 남긴다.
- 세션 검색은 이미지 블록을 건너뛰고 글만 찾고, `junees usage`는 이미지가 든 기록도 그대로 센다.
- claude-code 백엔드: `/image`, `--image`, 디스코드 이미지는 "images are sent on the api backend only"로 거절한다. Claude Code에는 텍스트만 넘기므로, 파일을 읽으라고 하면 Claude Code의 Read 도구가 이미지를 연다.
- API가 거절한 이미지: 헤더는 맞지만 데이터가 깨진 파일처럼 Junees가 미리 걸러 내지 못한 이미지 때문에 호출이 400 `invalid_request_error`로 끝나고 오류 문구에 image가 들어 있으면, 그 턴이 끝난 마지막 user 메시지(이번 입력과, 실패한 앞 턴에서 붙은 입력, 또는 도구 결과)의 이미지를 `[image removed: the API rejected the request that carried it, so it is not sent again]` 한 줄로 바꾸고(입력 이미지는 몇 개든 한 줄, 도구 결과는 출력 뒤에 그 줄) `!! the API rejected an image in this message; ...` 경고로 알린다. 기록에는 `clear_images { note }` 항목으로 남아 `-c`/`--resume`으로 이어 가도 그 이미지는 다시 가지 않고, 글은 남아 다음 입력과 함께 간다. image를 말하지 않는 400(도구 스키마, thinking 설정 등)과 컨텍스트 초과는 이미지를 그대로 둔다. 앞 턴에서 API가 이미 받아 준 이미지와 마지막 메시지가 아닌 곳의 이미지는 건드리지 않는다(앞 메시지를 바꾸면 thinking 블록이 거절되므로).
- 한계: 한 요청에 이미지가 20개를 넘으면 API가 이미지마다 더 작은 크기 제한을 둔다(문서: 각 변 3000 px 이하 권장). Junees는 이를 세지 않으므로, 큰 스크린샷을 많이 읽은 긴 세션은 400을 받을 수 있다. 이때 걸린 이미지가 앞 턴들에 있으면 아래처럼 빠지지 않으므로 `/new`(디스코드는 `!new`)로 새로 시작한다. `/compact`는 최근 두 입력과 그 이미지를 남기므로 도움이 되지 않을 수 있다. MCP 서버가 돌려준 이미지는 지금처럼 한 줄 설명으로만 간다.
