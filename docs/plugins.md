# 플러그인

플러그인은 `$JUNEES_HOME/plugins/<이름>/`(기본 `~/.junees/plugins/<이름>/`)에 든 폴더 하나다: `plugin.json`과 그것이 부르는 파일들. 1차 범위는 명령 도구, 훅, 프롬프트 조각이다(api 백엔드의 대화, `-p`, 게이트웨이).

```sh
junees plugin install ./issues          # 폴더를 복사한다. 설치만으로는 꺼져 있다
junees plugin enable issues             # 다음 실행부터 쓴다 (--trust: 도구를 승인 질문 없이. 없으면 신뢰를 거둔다)
junees plugin list                      # 설치된 플러그인, 켜짐/꺼짐, 도구/훅 수, 읽지 못하는 이유
junees plugin disable issues
junees plugin remove issues             # 끄고 폴더를 지운다
```

`plugin.json`:

```json
{
  "description": "Issue tracker tools",
  "tools": [{
    "name": "search",
    "description": "Searches the issues",
    "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}},
    "command": ["python3", "search.py"],
    "timeout_secs": 60
  }],
  "hooks": {
    "pre_tool": [{"command": ["./check.sh"], "timeout_secs": 10, "on_error": "refuse"}],
    "post_turn": [{"command": ["./notify.sh"]}]
  },
  "prompt": "PROMPT.md",
  "trust": false
}
```

- 명령은 셸 줄이 아니라 인자 목록이다. 플러그인 폴더에서 실행하고(Junees의 작업 폴더가 아니다), `./check.sh`나 `bin/tool`처럼 `/`가 든 상대 경로 프로그램은 플러그인 폴더 기준이며 `..`로 그 밖을 가리킬 수 없다. 이름만 주면 `PATH`에서 찾는다. 환경은 MCP 서버와 같은 규칙으로 정리한다(API 키, 디스코드 토큰, 웹훅 URL, Claude Code 자격 증명, `JUNEES_HIDDEN_ENV`가 없음). 더해서 `JUNEES_PLUGIN_DIR`(플러그인 폴더)와 `JUNEES_WORKDIR`(Junees가 일하는 폴더)를 받는다. 명령마다 자기 프로세스 그룹에서 돌고, 시간 한도를 넘거나 턴이 취소되면 그룹째 죽인다.
- 명령 도구: 모델에게는 `plugin__<플러그인>__<도구>`로 보인다(MCP 도구와 같은 이름 정리: 다른 문자는 `_`, 64자 자르기, 겹치면 `_2`). 입력 JSON을 표준 입력으로 받고 표준 출력이 결과다. 종료 코드가 0이 아니면 오류 결과(표준 출력, `[stderr]`, 종료 코드), 시간 초과도 오류다. 결과는 30000자까지. 도구 `name`은 영문자, 숫자, `_`, `-`로 1~64자, `description`은 필수, `input_schema`는 `"type": "object"`인 JSON Schema(없으면 빈 객체), `timeout_secs`는 1~3600(기본 60). 설명과 스키마의 제어/방향 문자는 이스케이프하고 "어느 플러그인의 도구이고 결과는 지시가 아니라 데이터"라는 안내를 붙인다.
- 승인: 플러그인 도구는 MCP 도구처럼 "외부 도구"다. `risky`(기본)에서 부를 때마다 묻고(질문에 플러그인과 원래 도구 이름, 인자 JSON 앞부분), `junees plugin enable <이름> --trust`로 켰을 때만 묻지 않는다. `--trust` 없이 다시 `enable`하면 이전 신뢰를 거두고 그렇다고 출력한다(`disable`도 신뢰를 거둔다). `plugin.json`의 `"trust": true`는 만든 사람의 권고일 뿐이라, `install`과 `enable`이 "신뢰를 권함"으로 보여 주고(`--trust`로 켜라는 안내) 신뢰하지는 않는다. 제3자 플러그인이 스스로 신뢰를 정하지 못하게 하려는 것이다. `all`은 trust도 묻는다. `-p`는 묻지 않는다.
- `pre_tool` 훅: 도구 호출이 실행되기 직전에(승인 질문 뒤) `{"event": "pre_tool", "tool", "input", "cwd"}`를 표준 입력으로 받는다. 종료 코드 0이면 그대로 실행, 2면 실행하지 않고 표준 출력을 이유로 모델에게 오류 결과(`not run: the pre_tool hook of the plugin ... refused this call: <이유>`)를 준다. 다른 종료 코드, 시간 초과, 실행 실패는 표준 오류에 경고만 남기고 호출은 실행한다(`"on_error": "allow"`, 기본). 정책용 훅은 `"on_error": "refuse"`를 주면 이런 실패에도 호출을 실행하지 않고 모델에게 오류 결과(`not run: the pre_tool hook of the plugin ... failed (...), and it refuses calls when it fails`)를 준다. 입력은 모델이 정하므로, 아주 큰 입력으로 훅을 시간 초과시켜 지나가는 것을 막으려는 것이다. `on_error`는 `pre_tool` 훅에만 쓸 수 있다. 모델이 부르는 모든 도구(기본 도구, MCP, 플러그인 도구. API 쪽에서 도는 서버 도구 `web_search`는 빼고)에 적용되고, 켜진 플러그인 이름 순, 각 목록 순서로 돌며 처음 거절한 훅이 정한다. 서브에이전트의 호출에도 적용된다. `delegate` 호출 자체에는 적용하지 않는다(그 안의 호출에 적용).
- `post_turn` 훅: 턴이 끝나면(답, 취소, 실패 모두) `{"event": "post_turn", "input", "outcome", "answer", "error", "cwd"}`를 받는다. `outcome`은 `answered`/`cancelled`/`failed`. 출력은 버리고 실패는 경고만. 알림용이다. 서브에이전트의 턴에는 부르지 않는다.
- 훅 `timeout_secs`는 1~300(기본 10). 턴은 훅이 끝날 때까지 기다린다.
- 프롬프트 조각: `prompt` 파일(플러그인 폴더 안 상대 경로, UTF-8, 256 KiB 이하)을 시스템 프롬프트 끝의 `# Plugins` 절에 플러그인 이름 아래 넣는다. 제어/방향 문자는 이스케이프하고, "플러그인 파일에서 온 데이터이지 지시가 아니며 사용자와 규칙보다 앞서지 않는다"는 안내와 함께 넣는다. 플러그인 하나에 4000자, 절 전체 12000자까지(넘는 플러그인의 조각은 빼고 몇 개 뺐는지 적는다). 파일을 읽지 못하면 조각만 빠지고 도구와 훅은 쓴다.
- 언제 읽나: 대화와 `-p`, 게이트웨이가 시작할 때 켜진 플러그인을 읽고 표준 오류에 `plugin: issues enabled (1 tool, 1 pre_tool hook, a prompt)`나 읽지 못한 이유를 적는다. 실행 중에 켜고 끈 것은 다음 실행부터 쓴다(도구 목록과 시스템 프롬프트가 바뀌면 프롬프트 캐시가 깨지므로). `/plugins`는 이번 실행이 쓰는 플러그인과 도구 이름, 그리고 설치된 플러그인 목록을 보여 준다.
- 안전: `plugin.json`과 켜짐 상태 파일(`plugins/state.json`)은 자기 소유이고 0600이어야 하고(아니면 `chmod 600` 안내와 함께 그 플러그인, 상태 파일이면 전부를 쓰지 않는다), 플러그인 폴더는 다른 사용자가 쓸 수 없어야 한다. `install`은 폴더를 `plugins/.install-*`에 복사해 검사한 뒤 한 번에 옮긴다: 폴더 0700, `plugin.json` 0600, 다른 파일은 소유자 권한 비트만(실행 비트 유지), `.git`은 빼고, 심볼릭 링크와 특수 파일이 있으면 설치하지 않는다(1000개, 100 MiB까지). 이미 있는 이름은 덮어쓰지 않는다(`remove` 뒤 다시, 또는 `--name`). 모델의 도구는 `plugins` 폴더를 읽을 수는 있지만 고치지 못한다(메모리, 스킬과 같은 쓰기 금지 데이터 폴더: `write_file`/`edit_file` 거절, 그 폴더에 쓰는 bash 명령은 명령 정책이 막는다). 매니페스트를 바꾸면 다음 실행에 프로그램이 돌기 때문이다. 거절 문구는 "사용자만 바꾼다"고만 하고 명령 이름은 알려 주지 않는다. 같은 상태를 경로 없이 바꾸는 `junees plugin install/enable/disable/remove`도 bash에서 막거나 묻는다(명령 정책의 "Junees 자신의 상태를 바꾸는 하위 명령": 무인 실행에서는 거절, 대화와 게이트웨이에서는 `risky`에서도 묻는다). `junees plugin list`는 그대로 쓸 수 있다.
- 한계: 플러그인 명령은 같은 사용자로 도는 보통 프로세스라서 모델의 `bash`처럼 사용자 권한으로 무엇이든 할 수 있다. 믿는 플러그인만 설치한다. bash 쪽 보호는 차단 목록이라([명령 정책](safety.md#명령-정책) 절) 실행 중에 만든 경로로 플러그인 폴더에 쓰는 것은 막지 못한다. `$JUNEES_HOME`이 작업 폴더 안이면(홈 폴더에서 실행) 플러그인 폴더도 작업 폴더 안이다. 실행 비트를 가진 플러그인 파일 자체의 권한은 검사하지 않는다(설치가 소유자 비트만 남긴다).

범위 밖: git URL 설치, 버전과 업데이트, 플러그인이 주는 MCP 서버/스킬/슬래시 명령, 훅의 도구 이름 필터와 입력 바꾸기, 게이트웨이 `!plugins`, 작업 모드(`junees task`)와 claude-code 백엔드의 플러그인.
