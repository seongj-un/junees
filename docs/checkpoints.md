# 파일 체크포인트와 되돌리기

Junees가 `write_file`과 `edit_file`로 바꾼 파일을 턴 단위로 되돌립니다. 모델이 파일을 잘못 고쳤을 때 git 없이도 한 번에 되돌릴 수 있게 하기 위해서입니다.

- 터미널 대화: `/checkpoints`, `/undo`, `/undo N`
- 명령줄: `junees rewind --session <id> [--list] [--turn <n>] [--force] [--yes]`
- 대시보드 대화: `GET /api/chat/<id>/checkpoints`, `POST /api/chat/<id>/undo` ([대시보드](dashboard.md#경로))

## 무엇을 기록하나

- `write_file`과 `edit_file`이 파일을 바꾸기 직전의 내용과 권한 비트를 남깁니다. 파일이 없었다면 "없었음"으로 남깁니다.
- 한 턴에서 같은 파일을 여러 번 바꾸면 첫 번째 원본만 남깁니다. 바꿀 때마다 바뀐 뒤 내용의 SHA-256을 함께 남깁니다.
- 서브에이전트가 바꾼 파일은 그 서브에이전트를 부른 턴에 들어갑니다.
- 대화, `-p` 한 번 실행, 게이트웨이, 예약 작업, 웹훅, `junees mcp serve`, 대시보드 대화가 모두 기록합니다. 세션을 저장하지 않는 실행(`--no-session`)은 기록하지 않습니다.
- **`bash`가 바꾼 파일은 기록하지 않습니다.** `sed -i`, `git checkout`, 빌드 결과물, 스크립트가 쓴 파일은 되돌리지 못합니다. `/undo`와 `junees rewind`의 출력, 대화에 남는 줄에도 이 점을 적습니다.
- 파일을 하나도 바꾸지 않은 턴은 아무것도 남기지 않습니다. 턴 번호는 파일을 바꾼 턴마다 1부터 매기고, 세션을 이어 가도 이어집니다.

## 저장 위치와 상한

- `$JUNEES_HOME/checkpoints/<session id>/<turn>/`
  - `manifest.json`: 파일마다 경로(기록 당시의 절대 경로), 원래 상태(`content`, `absent`, `too_large`), 내용 파일 이름, 원래 권한 비트, 바뀐 뒤 해시, 시각
  - `<n>.bin`: 원래 내용
- 폴더는 0700, 파일은 0600입니다.
- 파일 하나가 2MB를 넘으면 내용을 남기지 않고 "너무 커서 기록하지 않음"(`too_large`)으로만 남깁니다. 이 파일은 되돌릴 수 없고, 목록에 그렇게 보입니다.
- 세션 하나가 200MB, 전체가 1GB를 넘으면 오래된 턴부터 지웁니다. 30일이 지난 턴은 Junees가 시작할 때 지웁니다.
- `checkpoints = false`(`JUNEES_CHECKPOINTS=0`)로 끕니다. 끄면 새로 기록하지 않고, 이미 있는 체크포인트는 `junees rewind`로 계속 되돌릴 수 있습니다.

**비밀 값 주의:** 체크포인트에는 파일의 원래 내용이 그대로 남습니다. 모델이 `.env` 같은 파일을 고쳤다면 원래 내용이 30일 동안(또는 상한에 밀려 지워질 때까지) `$JUNEES_HOME/checkpoints`에 남습니다. 지우려면 그 세션 폴더를 지웁니다. 세션 기록(`sessions/`)과 같은 권한(0700/0600)입니다.

## 되돌리기 규칙

- 여러 턴을 되돌릴 때는 최근 턴부터 차례로 되돌립니다. `--turn n`(대시보드는 `{"turn": n}`)은 n번째 턴과 그 뒤의 모든 턴입니다.
- 지금 파일의 해시가 그 턴이 남긴 해시와 다르면 건너뜁니다(`바뀐 뒤 수정됨`). 그 뒤에 사람이나 bash가 바꾼 것을 덮어쓰지 않기 위해서입니다. `--force`(대시보드는 `"force": true`)를 주면 건너뛰지 않고 원래 내용으로 되돌립니다.
- 원래 없던 파일은 지웁니다. 단, 해시가 그 턴이 남긴 것과 같을 때만 지웁니다. `--force`여도 그 뒤에 바뀐 파일은 지우지 않습니다(`turn이 만든 파일이 그 뒤 수정되어 지우지 않음`). 남의 작업을 지우는 일은 덮어쓰기보다 되돌리기 어렵기 때문입니다.
- 원래 내용을 되돌릴 때는 원래 권한 비트도 되돌립니다.
- 되돌린 뒤 그 세션의 다음 메시지 앞에 한 줄을 붙입니다. 예: `[되돌림] 턴 7의 파일 변경을 되돌렸습니다: /work/a.rs, /work/b.rs (건너뜀: /work/c.rs, 바뀐 뒤 수정됨). bash로 바꾼 파일은 기록되지 않아 되돌리지 않았습니다.` 모델이 파일 상태가 바뀐 것을 알게 하기 위해서입니다. `junees rewind`처럼 다른 프로세스에서 되돌려도, 그 세션을 이어 간 첫 메시지에 붙습니다.
- 되돌리기 자체도 새 턴으로 기록합니다. 그래서 `/undo`를 한 번 더 하면 되돌리기를 되돌립니다.

## 경로 안전

- 기록 당시의 절대 경로에만 씁니다.
- 심볼릭 링크를 따라가지 않습니다. 대상이 링크로 바뀌었으면 건너뜁니다(`심볼릭 링크로 바뀜`). 일반 파일이 아니게 됐으면 건너뜁니다(`일반 파일이 아님`).
- 파일 도구와 같은 쓰기 규칙을 통과해야 씁니다. 시스템 폴더, 설정 파일과 시크릿 폴더, Junees 데이터 폴더(메모리, 스킬, 예약 작업 등)는 거절하고 건너뜁니다(`쓰기 정책이 막음`). 쓰기는 파일 도구처럼 임시 파일에 쓴 뒤 rename합니다.

## 사용

터미널 대화:

```
> /checkpoints
turn 1 (2026-11-13 12:00:00 UTC)
  modified /work/src/main.rs
  created  /work/src/util.rs
> /undo
undo these changes, newest turn first:
turn 1 (2026-11-13 12:00:00 UTC)
  modified /work/src/main.rs
  created  /work/src/util.rs
undo? [y/N] y
  undone   /work/src/util.rs
  undone   /work/src/main.rs
recorded as turn 2: undo it with `/undo` or `junees rewind --turn 2`
files bash changed are not recorded, so they were not undone
```

- `/undo N`는 파일을 바꾼 최근 N개 턴을 되돌립니다. 터미널의 `/undo`에는 `--force`가 없습니다. 바뀐 뒤 수정된 파일까지 되돌리려면 `junees rewind --force`를 씁니다.
- 목록에서 `(changed since: skipped unless forced)`는 지금 되돌리면 건너뛸 파일이고, `(too large to record: cannot be undone)`는 되돌릴 수 없는 파일입니다.

명령줄:

- `junees rewind --session <id> --list`: 턴별 목록만 보여 줍니다.
- `junees rewind --session <id>`: 마지막 턴을 보여 주고 `y/N`을 물은 뒤 되돌립니다. `--yes`면 묻지 않습니다.
- 세션 id는 `junees sessions search`, 대시보드, `$JUNEES_HOME/sessions`의 파일 이름에서 찾습니다.
- 무인 실행(예약 작업, 웹훅, MCP serve)도 체크포인트를 남기지만, 되돌리기는 사람이 이 명령이나 대시보드로만 합니다.

## 게이트웨이

디스코드, 텔레그램, Slack 게이트웨이에는 아직 `/undo`가 없습니다. 게이트웨이 대화도 체크포인트를 남기므로, 그 세션 id로 `junees rewind`를 씁니다.
