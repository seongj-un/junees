# 훅

훅은 사용자가 정한 명령을 세션의 정해진 시점에 실행하는 기능입니다. 파일을 고친 뒤 포매터를 돌리거나, 특정 명령을 막거나, 턴이 끝나면 알림을 보내거나, 세션을 시작할 때 맥락을 더할 수 있습니다. 훅은 도구 호출이나 입력을 막을 수 있고 모델에게 안내를 더할 수 있지만, 도구의 입력을 바꾸지는 못하고 승인 질문과 명령 정책을 건너뛰게 하지도 못합니다.

훅은 api 백엔드의 대화(터미널, `-p`), 게이트웨이, 대시보드 대화, 예약 작업, 웹훅, `junees mcp serve`, `task`에서 돕니다. claude-code 백엔드는 Claude Code가 도구를 직접 실행하므로 Junees의 훅을 쓰지 않습니다.

## 설정

훅은 `[[hooks]]` 표 하나에 하나씩 적습니다. 세 곳에서 읽습니다.

- `$JUNEES_HOME/config.toml`: 평평한 설정들 뒤에 `[[hooks]]` 표를 둡니다. `junees setup`이 설정 파일을 다시 써도 훅 표는 그대로 남습니다.
- `$JUNEES_HOME/hooks.toml`: 훅 표만 둔 파일입니다. 설정 파일의 훅 다음에 읽습니다.
- 프로젝트의 `.junees/hooks.toml`: 프로젝트 지침 파일을 읽는 범위의 맨 위 폴더(저장소의 맨 위, 저장소가 아니면 HOME 또는 작업 폴더 자신)에 둔 파일입니다. 남이 쓴 파일일 수 있으므로 승인한 뒤에만 실행합니다([보안](#보안)).

```toml
[[hooks]]
event = "pre_tool"          # pre_tool | post_tool | turn_end | session_start | user_prompt
match = "bash|write_file"   # 도구 이름 정규식 (pre_tool, post_tool만). 없으면 모든 도구
command = ["./scripts/check.sh"]   # 프로그램과 인자 배열. 셸 문자열은 받지 않음
timeout = "10s"             # 기본 10s, 최대 60s (ms, s, m 단위)
on_error = "allow"          # allow | block (pre_tool, user_prompt만)
```

- `match`는 도구 이름 전체와 맞춰 봅니다. `bash`는 `bash`에만 맞고 `plugin__x__bash_helper`에는 맞지 않습니다.
- `command`의 첫 낱말이 `/`를 담은 상대 경로(`./scripts/check.sh`)이면 세션의 작업 폴더에서 찾고, `/`가 없으면 PATH에서 찾습니다.
- 사용자 훅(설정 파일과 `hooks.toml`)에 잘못된 값이 있으면(모르는 이벤트, 깨진 정규식, 문자열로 쓴 `command`, 60초를 넘는 `timeout`, 막을 수 없는 이벤트의 `on_error = "block"` 등) Junees가 시작하지 않고 파일과 줄을 알립니다. 정책 훅이 조용히 빠지면 막아야 할 호출이 통과하기 때문입니다.

## 이벤트

| 이벤트 | 언제 | 막을 수 있나 |
|---|---|---|
| `session_start` | 새 대화의 첫 입력 전(`/new` 뒤 포함). 이어 가는 세션(`-c`, `--resume`)에서는 돌지 않습니다 | 아니요 |
| `user_prompt` | 사람의 입력이 모델에게 가기 전 | 예 |
| `pre_tool` | 도구 호출 전, 승인 질문 전 | 예 |
| `post_tool` | 도구 호출 뒤(막히거나 거절된 호출 포함) | 아니요 |
| `turn_end` | 턴이 끝난 뒤(답, 오류, 취소 모두) | 아니요 |

서브에이전트(`delegate`)의 도구 호출에도 위임한 세션의 `pre_tool`, `post_tool` 훅이 돕니다. `user_prompt` 훅에 막힌 입력에는 `turn_end` 훅이 돌지 않습니다.

## 실행 계약

### 입력

표준 입력으로 JSON 객체 하나를 받습니다.

```json
{
  "event": "pre_tool",
  "session_id": "20261117-101500-ab12",
  "cwd": "/home/me/project",
  "tool": {"name": "bash", "input": {"command": "cargo test"}},
  "result": {"is_error": false, "text": "..."},
  "prompt": "..."
}
```

- `session_id`: 세션 기록의 id. 기록하지 않는 세션이면 `null`입니다.
- `tool`: `pre_tool`, `post_tool`에만 있습니다.
- `result`: `post_tool`에서는 도구 결과, `turn_end`에서는 모델의 답(오류면 오류 글, 취소면 `cancelled`)입니다. `text`는 앞 16KB(문자 경계에서 자름)입니다.
- `prompt`: `user_prompt`에만 있습니다.
- 모든 문자열에서 알려진 비밀 값(설정과 환경의 키, 토큰)을 가린 뒤 넘깁니다. 가린 뒤에 자르므로 잘린 키 조각이 남지 않습니다.

### 환경과 작업 폴더

- 작업 폴더는 세션의 작업 폴더입니다.
- 환경 변수는 bash 도구와 같은 규칙으로 지웁니다: 비밀 변수 목록(`ANTHROPIC_API_KEY` 등), Claude Code 로그인 변수, `JUNEES_HIDDEN_ENV`, 그리고 이름에 `KEY`, `TOKEN`, `SECRET`, `PASSWORD`가 든 `JUNEES_*` 변수. 더하는 변수는 `JUNEES_HOOK_EVENT`(이벤트 이름)와 `JUNEES_SESSION_ID`(세션 id, 없으면 지움) 둘뿐입니다.
- 훅은 자기 세션(setsid)에서 돌아 Junees의 터미널을 열 수 없습니다. bash 도구의 샌드박스(Landlock, [실행 격리](safety.md#실행-격리-landlock))가 켜져 있으면 훅에도 같은 규칙을 씁니다. 샌드박스가 실행을 거절하면 훅이 실패한 것으로 봅니다.
- 시간이 넘으면 훅의 프로세스 그룹을 통째로 끝냅니다. 턴이 취소되면 도는 `pre_tool`, `post_tool`, `session_start`, `user_prompt` 훅도 끝냅니다.

### 결과

| 결과 | 뜻 |
|---|---|
| 종료 코드 0 | 계속합니다. 표준 출력(앞 16KB)이 `{"message": "..."}`이면 그 글(최대 4000자)을 모델에게 Junees의 안내로 더합니다 |
| 종료 코드 2 (`pre_tool`, `user_prompt`) | 막습니다. 표준 오류의 앞 2KB가 막은 이유입니다 |
| 그 밖의 코드, 신호, 시간 초과, 시작 실패 | `on_error`를 따릅니다. `allow`(기본)이면 경고 한 줄을 남기고 계속하고, `block`이면 막습니다 |

- `pre_tool` 훅이 막으면 그 호출은 실행되지 않고, 승인 질문도 나오지 않습니다. 모델은 `not run: the pre_tool hook \`<명령>\` blocked this: <이유>` 오류 결과를 받습니다.
- `pre_tool` 훅이 통과시켜도 승인 질문과 명령 정책은 그대로 적용됩니다. 훅은 입력을 바꾸지 못합니다. 바꿀 수 있다면 승인 화면에서 본 것과 실제 실행이 달라지기 때문입니다.
- `user_prompt` 훅이 막으면 입력이 모델에게 가지 않고, 터미널에는 `the message was not sent: ...` 오류가 나옵니다. 세션 기록에는 막았다는 안내 줄만 남고 입력은 남지 않습니다.
- 종료 코드 2는 `post_tool`, `turn_end`, `session_start`에서는 막지 않고 실패로 봅니다(경고).
- 같은 이벤트의 훅은 설정 순서(설정 파일, `hooks.toml`, 프로젝트 파일)대로 하나씩 돌고, 하나가 막으면 뒤의 훅은 돌지 않습니다. `post_tool`, `turn_end`는 끝날 때까지 기다리지만 실패해도 턴을 멈추지 않습니다.

### 세션 기록과 모델이 보는 것

훅이 돌 때마다 세션 기록에 Junees의 안내 줄(`notes` 항목, 모델에게는 `<junees_notice>` 블록)이 한 줄 남습니다. 세션 기록 형식은 9 그대로입니다.

```
[훅] pre_tool `./scripts/check.sh`: 막음 (0.05초)
[훅 post_tool `cargo fmt`] formatted 2 files
```

- 결과는 `통과`, `막음`, `실패`, `시간 초과(N초)`이고, `on_error = "block"`으로 막았으면 `실패 시 막는 설정이라 막음`이 붙습니다.
- `message`는 두 번째 줄처럼 따로 남습니다. 제어 문자와 방향 바꾸기 문자는 이스케이프하고, 알려진 비밀 값은 가리고, `<junees_notice>` 태그는 깨뜨려서(#117/#125 규칙) 훅 출력이 Junees의 안내 블록을 열거나 닫지 못하게 합니다.
- 도구 훅의 줄은 그 도구 라운드의 결과 뒤에, `session_start`와 `user_prompt` 훅의 줄은 입력 앞에, `turn_end` 훅의 줄은 턴이 끝난 뒤(다음 입력과 같은 메시지)에 들어갑니다.
- 터미널과 게이트웨이는 같은 줄을 진행 표시로 보여 줍니다. 대시보드 대화에는 `hook` 이벤트를 보냅니다.

```json
{"type": "hook", "event": "pre_tool", "command": "./scripts/check.sh", "origin": "user",
 "result": "blocked", "took_ms": 52, "reason": "rm is not allowed here"}
```

`result`는 `passed`, `blocked`, `timed_out`, `failed`이고, `message`(통과하며 남긴 글), `reason`(막은 이유), `error`(실패 이유), `blocked: true`(실패로 막음)는 있을 때만 붙습니다. `origin`은 `user` 또는 `project`입니다.

## 예제

### 파일을 고친 뒤 포매터 돌리기

```toml
[[hooks]]
event = "post_tool"
match = "write_file|edit_file"
command = ["./scripts/fmt-hook.sh"]
timeout = "30s"
```

```sh
#!/bin/sh
# scripts/fmt-hook.sh: 고친 파일이 .rs이면 rustfmt를 돌리고 모델에게 알린다.
path=$(jq -r '.tool.input.path // empty')
case "$path" in
  *.rs) rustfmt --edition 2024 "$path" && echo '{"message": "rustfmt로 정리했습니다. 다시 읽고 이어 가세요."}' ;;
esac
```

### 명령 막기

```toml
[[hooks]]
event = "pre_tool"
match = "bash"
command = ["./scripts/no-force-push.sh"]
on_error = "block"   # 훅이 실패해도 막는다
```

```sh
#!/bin/sh
# scripts/no-force-push.sh: force push를 막는다.
if jq -r '.tool.input.command' | grep -Eq 'git push.*(--force|-f)'; then
  echo "force push는 사람이 직접 합니다" >&2
  exit 2
fi
```

### 턴이 끝나면 알림 보내기

```toml
[[hooks]]
event = "turn_end"
command = ["notify-send", "Junees", "턴이 끝났습니다"]
```

## 명령

- `junees hooks` 또는 `junees hooks list`: 이 폴더의 세션이 읽는 사용자 훅과 프로젝트 훅, 프로젝트 파일의 승인 상태를 보여 줍니다.
- `junees hooks trust`: 이 폴더의 프로젝트 훅 파일을 지금 내용 그대로 승인하고, 승인한 내용을 보여 줍니다.
- `junees hooks untrust`: 그 파일의 승인을 거둡니다.
- `junees hooks test <event> [--tool <name>]`: 그 이벤트의 훅(사용자 훅과 승인된 프로젝트 훅)을 예시 입력(`session_id`는 `hooks-test`, 도구 입력은 `{}`, 결과와 프롬프트는 `test`)으로 한 번 돌리고 결과를 보여 줍니다.
- `junees doctor`: 훅마다 프로그램이 있는지, 프로젝트 파일이 읽히는지와 승인 상태를 점검합니다. 사용자 훅이 깨져 있으면 실패로 표시합니다.

## 보안

- 사용자 훅(`$JUNEES_HOME`)은 사용자가 쓴 것이므로 묻지 않고 실행합니다. 모델의 도구는 `$JUNEES_HOME/hooks.toml`과 승인 기록 `$JUNEES_HOME/trusted-hooks`를 읽지도 쓰지도 못합니다(설정 파일과 같은 보호, [명령 정책](safety.md#명령-정책)). 쓸 수 있다면 모델이 묻지 않고 실행될 명령을 심거나 프로젝트 훅을 스스로 승인할 수 있기 때문입니다.
- 프로젝트 훅(`.junees/hooks.toml`)은 저장소에 들어 있는 남의 코드이므로 사람이 승인해야 실행합니다. 승인은 파일 경로와 내용의 SHA-256으로 기록하므로(`$JUNEES_HOME/trusted-hooks`, 0600) 한 글자라도 바뀌면 다시 묻습니다. 링크인 파일, 일반 파일이 아닌 것, 256KB가 넘는 파일은 읽지 않습니다.
- 터미널 대화(표준 입력과 표준 오류가 모두 터미널)는 시작할 때 파일 내용 전체를 보여 주고 실행할지 묻습니다. 답이 `y`가 아니면 이번 세션에서는 실행하지 않습니다.
- 물을 사람이 없는 실행(`-p`, 게이트웨이, 대시보드 대화, 예약 작업, 웹훅, `junees mcp serve`, `task`)은 승인되지 않은 프로젝트 훅을 실행하지 않고 경고만 냅니다. 웹훅 실행은 지침 파일처럼 경로 폴더의 프로젝트 파일을 봅니다.
- 모델이 bash로 `junees hooks trust`/`untrust`를 실행하려 하면 Junees 자신의 상태를 바꾸는 명령으로 보고 승인을 묻거나(대화) 막습니다(무인 실행).
- 훅 명령은 bash 도구와 같은 비밀 변수 제거와 샌드박스 아래에서 돕니다. 다만 샌드박스가 꺼져 있으면 훅은 사용자 권한 그대로 돕니다. 승인한 프로젝트 훅은 그 저장소의 다른 스크립트(`./scripts/check.sh`)를 부를 수 있고, 승인은 그 스크립트의 내용까지 고정하지는 않습니다. 승인하기 전에 훅이 부르는 스크립트도 읽어 보세요.
- 훅에 넘기는 입력은 알려진 비밀 값을 가리지만, 파일 내용이나 명령 출력에 든 모르는 비밀까지 가리지는 못합니다. 훅이 입력을 밖으로 보낸다면 그 점을 생각하고 씁니다.
