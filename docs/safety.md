# 명령 정책, 실행 격리, 실행 전 승인

모델의 도구 호출을 막거나 실행 전에 묻는 안전장치. 명령 정책은 차단 목록이므로 한계도 함께 적고, 그 빈틈의 일부는 커널의 실행 격리(Landlock)가 막는다.

## 명령 정책

`bash`는 명령을 실행하기 전에 정책으로 검사하고, 걸리면 아무것도 실행하지 않은 채 `blocked by policy: <이유>` 오류 결과를 모델에 돌려준다. 사람이 지켜보지 않는 장기 실행에서 치명적인 명령을 막는 1차 안전장치이고 샌드박스는 아니다. 통과한 명령은 [실행 격리](#실행-격리-landlock) 안에서 돈다.

기본으로 막는 것:

- 루트, 최상위 시스템 폴더(`/usr`, `/etc`, `/home` 등), 홈, 상위 폴더의 재귀 삭제: `rm -r`/`-rf`/`--recursive` 대상이 `/`, `/*`, `~`, `~/`, `$HOME`, `${HOME}`, 홈의 절대 경로나 그 위, `..`, `../..`처럼 현재 폴더 위로 올라가는 경로. `rm -rf ./build`, `rm -rf target`, `rm -rf ../other/target`은 허용
- 위험한 폴더로 `cd` 한 뒤 현재 폴더 전체 삭제: 같은 스크립트에서 앞서 `cd`/`pushd`로 홈(인자 없는 `cd` 포함), `/`, 최상위 시스템 폴더, 상위 폴더, `cd -`, `$`가 들어간 경로(`cd "$BUILD_DIR"`, 변수가 비면 홈으로 간다)에 들어갔으면 그 뒤의 `rm -r *`, `rm -r .`, `rm -r ./*`, `rm -r .*`를 막는다. 그 뒤 다른 폴더로 `cd` 하면 풀린다. `cd build && rm -rf *`는 허용
- 파일시스템/디스크 파괴: `mkfs*`, `wipefs`, `dd ... of=/dev/...`, `> /dev/sda` 같은 장치로의 리다이렉트 (`/dev/null`, `/dev/stdout`, `/dev/stderr`, `/dev/fd/*`, `/dev/tcp/*`, 메모리 파일시스템인 `/dev/shm/*`, `/dev/mqueue/*` 등은 허용)
- 시스템 종료: `shutdown`, `reboot`, `halt`, `poweroff`, `systemctl reboot|poweroff|halt|kexec`
- fork bomb (`:(){ :|:& };:`과 같은 모양: 함수 본문 안에서 자기 자신을 자기 자신에게 파이프로 넘기고 `&`로 띄우는 것. `f(){ ...; }; f | f` 같은 평범한 함수 파이프는 허용)
- 내려받은 스크립트 바로 실행: `curl`/`wget ... | sh`(bash, zsh 등 셸 포함), `bash <(curl ...)`, `sh -c "$(curl ...)"`
- 강제 push: `git push --force`, `-f`, `+refspec` (`--force-with-lease`는 허용)
- 권한 상승: `sudo`, `doas`, `pkexec`, `su`, `runuser`
- 설정 파일(`$JUNEES_HOME/config.toml`, API 키를 담는다)을 이름으로 가리키는 명령: 단어나 리다이렉트 대상(`<`, `>`, `>>`), `--file=경로`/`if=경로`처럼 `=` 뒤의 값이 그 파일이면 막는다. 절대 경로, `~/.junees/config.toml`, `$HOME`/`${HOME}`, `$JUNEES_HOME`/`${JUNEES_HOME}`으로 시작하는 경로, `..`와 심볼릭 링크를 거친 경로, 현재 폴더(같은 스크립트에서 앞서 `cd`/`pushd`한 곳)에서의 상대 경로(`cd ~/.junees && cat config.toml`)를 알아본다. 따옴표 안 단어도 경로로 본다(`echo "~/.junees/config.toml"`도 막힌다). `/proc/self/cwd/...`는 그 시점의 현재 폴더로 푼다. 글롭(`*`, `?`, `[...]`)과 중괄호(`{a,b}`)도 펼쳐서, 설정 파일과 맞을 수 있으면 막는다(`cat ~/.junees/*`, `cat ~/.junees/conf*`, `cat ~/.junees/{config.toml,x}`, `cat ~/.jun*/config.toml`). `mcp.json`과, `$JUNEES_HOME/secrets/`(웹훅 시크릿 파일과 환경 파일을 두는 곳) 폴더와 그 안의 모든 경로도 똑같이 막는다(`cat ~/.junees/secrets/x`, `grep -r . ~/.junees/secrets`, `cat ~/.junees/s*/x`). read_file, write_file, edit_file도 이 파일들을 거절한다. 안전한 쪽으로 기울게 `*`는 점으로 시작하는 이름에도 맞는다고 본다.
- 설정 파일이 든 폴더(`$JUNEES_HOME`, 기본 `~/.junees`) 자체를 가리키는 명령: 폴더를 통째로 복사하거나 묶거나 옮기면 파일 이름을 쓰지 않고도 키를 읽거나 설정을 바꿔치기할 수 있으므로(`grep -r api_key ~/.junees`, `cp -r ~/.junees /tmp/j`, `tar -C ~/.junees ...`, `mv ~/.junees /tmp/old && mv /tmp/evil ~/.junees`, `cd ~/.junees && tar -cf - .`) 막는다. 이름과 메타데이터만 보는 `cd`, `pushd`, `ls`, `stat`, `du`, `test`/`[`, `realpath`, `readlink`, `echo`는 허용한다. 폴더를 변수에 담는 대입(`d=~/.junees`)도 막힌다. 그 안의 세션 폴더와 파일(`~/.junees/sessions/...`)은 그대로 쓸 수 있다.
- Junees 데이터 폴더(`$JUNEES_HOME/memory/`, `$JUNEES_HOME/projects/`, `$JUNEES_HOME/skills/`)에 쓸 수 있는 명령: 이 폴더들은 Junees가 자기 도구(`memory`, 스킬은 `skill_manage`)로만 바꾼다. 그 도구들이 내용을 검사하기 때문이다(작업 세션은 project 메모리만, 비밀과 제어 문자 거절). 그래서 그 안을 가리키는 쓰기 리다이렉트(`>`, `>>`, `>|`, `&>`, `3>` 등)와, 읽기만 하는 것으로 알려진 프로그램(위 목록과 `cat`, `head`, `tail`, `less`, `more`, `grep`/`egrep`/`fgrep`, `rg`, `wc`, `file`, `diff`, `cmp`, `md5sum`/`sha1sum`/`sha256sum`, `nl`, `jq`) 밖의 명령이 그 안의 경로나 폴더 자체를 인자로 받으면 막는다(`echo ... >> ~/.junees/memory/user.md`, `sed -i ... ~/.junees/memory/user.md`, `cp`, `mv`, `rm`, `mkdir`, `ln`, `tee` 등). 오류는 해당 도구를 쓰라고 알려 준다. 경로를 알아보는 방식(`~`/`$HOME`/`$JUNEES_HOME` 형태, `..`, 심볼릭 링크, `cd` 추적, 글롭과 중괄호)은 설정 파일 규칙과 같다. 읽기(`cat ~/.junees/memory/user.md`, `grep -r ... ~/.junees/skills`, `< 파일`)와, 그 안의 파일을 프로그램으로 실행하는 것(`~/.junees/skills/x/scripts/run.sh`, `bash`/`sh`/`source`의 스크립트 인자)은 허용한다. 위 목록 밖의 프로그램은 읽기만 해도 막힌다(`cp ~/.junees/memory/user.md /tmp/x`. 이럴 때는 `cat`이나 `read_file`을 쓴다).
- Junees 자신의 상태를 바꾸는 하위 명령(`src/tools/self_command.rs`의 표, 지금은 `junees cron add`, `remove`/`rm`, `pause`, `resume`, `run`, `daemon`, `junees plugin install`, `enable`, `disable`, `remove`(`junees plugins ...`도), `junees webhook add`, `remove`/`rm`, `pause`, `resume`, `serve`, `test`, `run`, `junees mcp login`, `logout`): 모델이 bash로 작업을 예약하면 사람 없이 계속 돌고(봇이 볼 수 있는 아무 디스코드 채널로 보낼 수도 있다), 지우거나 멈추면 사용자의 작업이 망가진다. 모델이 임시 폴더에 쓴 플러그인을 설치하고 켜면 다음 세션부터 그 프로그램이 돌고(훅은 모든 도구 호출과 답을 본다), 사용자가 정책용으로 켠 훅 플러그인을 끌 수도 있다. 데이터 폴더 규칙은 경로를 쓰는 명령만 알아보므로 이 명령들을 따로 알아본다. 프로그램이 `junees`(어느 폴더든, `/usr/local/bin/junees`, `./target/debug/junees`), 지금 실행 중인 바이너리의 파일 이름, 또는 그 바이너리를 가리키는 경로(상대 경로는 현재 폴더 기준, 링크를 따라감)이고 그 뒤 단어가 표의 하위 명령이면 해당한다. 아래 검사 방식대로 `env junees ...`, `nohup`/`timeout` 같은 래퍼, `bash -c '...'`/`sh -c`, `eval`, `;`/`&&`/`||`/파이프, `$(...)`, 셸에 넘긴 here-document와 파이프 스크립트 안까지 본다. 사람에게 물을 수 없는 실행(`-p`, `task`, 예약 작업 실행, `JUNEES_APPROVAL=off`인 대화)에서는 막고(`... which changes Junees's own state: only the user does that, and no one is here to approve it`), 승인 게이트가 있는 대화와 디스코드 게이트웨이에서는 막지 않고 `risky`에서도 묻는다([실행 전 승인](#실행-전-승인)). 읽기만 하는 `junees cron list`, `junees cron`, `junees webhook list`, `junees webhook`, `junees plugin list`, `junees plugin`, `junees mcp`(도움말), `junees status`와 따옴표 안 문자열(`grep 'junees cron add' README.md`)은 허용한다. claude-code 백엔드에는 같은 표로 `Bash(junees cron add)`, `Bash(junees cron add *)` 같은 거절 규칙을 붙인다(Claude Code는 묻지 않으므로 늘 거절).
- `gh`(GitHub CLI)로 GitHub에 쓰는 명령(`src/tools/gh_command.rs`의 표): `gh pr merge`, `review`, `close`, `comment`, `create`, `edit`, `reopen`, `ready`, `lock`/`unlock`, `gh issue create`/`comment`/`close`/`reopen`/`edit`/`delete`/`transfer` 등, `gh release create`/`edit`/`delete`/`upload`, `gh repo create`/`delete`/`edit`/`rename`/`archive`/`fork`/`sync` 등, `gh workflow run`, `gh run rerun`/`cancel`/`delete`, `gh secret`/`gh variable`의 `set`/`delete`, `gh label`, `gh gist`의 쓰기, 그리고 `gh api` 중 `-X`/`--method`(`-XPUT`, `--method=put` 포함)가 POST, PUT, PATCH, DELETE이거나, 메서드 없이 `-f`/`-F`/`--field`/`--raw-field`/`--input`으로 본문을 보내는 것(gh가 POST로 보낸다. `gh api graphql`은 읽기도 POST라서 묻는다). `-R`/`--repo`가 하위 명령 앞에 와도(`gh pr -R o/n merge 7`) 알아본다. 서버에 `gh`가 로그인돼 있거나 `GH_TOKEN`/`GITHUB_TOKEN`이 있으면, bash로 `github` 도구의 승인 질문과 무인 허용 목록(`github_allow_unattended`)을 건너뛰고 merge나 approve를 할 수 있기 때문이다. 사람에게 물을 수 없는 실행에서는 막고(`` `gh pr merge` merges a pull request on GitHub as the user: no one is here to approve it ... ``), 승인 게이트가 있는 대화와 게이트웨이에서는 `risky`에서도 묻는다(종류 `write to GitHub through gh`). merge(`gh pr merge`, 그리고 인자에 `/merge`나 `mergePullRequest`가 든 `gh api` 쓰기)는 종류가 `merge on GitHub through gh (asked every time)`라서 `github` 도구의 merge처럼 "항상 허용"으로 답해도 다음 merge는 다시 묻는다. 읽기(`gh pr view`, `gh pr diff`, `gh pr checks`, `gh api repos/o/n/pulls`, `gh api -X GET ...`)는 그대로 된다. claude-code 백엔드의 거절 규칙에는 넣지 않았다(그 백엔드에는 `github` 도구도 없고, Claude Code는 자기 `gh`를 쓸 수 있다. [GitHub](automation.md#github) 절)
- git과 GitHub CLI의 자격 증명 파일: `~/.git-credentials`, `~/.config/git/credentials`, `~/.config/gh/`(`XDG_CONFIG_HOME`이 절대 경로로 있으면 `$XDG_CONFIG_HOME/git/credentials`와 `$XDG_CONFIG_HOME/gh/`도)는 설정 파일과 똑같이 막는다(이름이나 글롭으로 가리키는 명령, read_file/write_file/edit_file, claude-code 백엔드의 `Read`/`Edit` 거절 규칙). git의 자격 증명 저장소와 `gh` 로그인은 사용자로 push하고 merge할 수 있다.
- 웹훅으로 시작한 실행(GitHub 흐름의 단계 포함)에서만: `git push`(`git -C x push`, `--git-dir=...`, 래퍼, `sh -c`, `eval`까지), `git send-pack`과 `git-send-pack`, `git subtree push`, 읽기가 아닌 `git config`(`--get`, `--get-all`, `--list`, `git config get`/`list`, `git config <이름>`은 허용), `git credential`과 `git-credential-*`, `-c`/`--config-env`의 `alias.*`, `credential*`, `url.*` 키. 웹훅 요청은 누구나 쓸 수 있는 글이라, 주입된 모델이 `git push origin FETCH_HEAD:main`으로 merge 조건을 건너뛰거나 `url.<x>.pushInsteadOf`로 흐름의 push를 다른 곳으로 돌리지 못하게 한다(push는 흐름 코드가 직접 한다). 그 실행의 bash는 git 자격 증명 없이 돈다: `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_0`/`GIT_CONFIG_VALUE_0`으로 빈 `credential.helper`(모든 설정 파일의 helper 목록을 비운다), `GIT_TERMINAL_PROMPT=0`, 실행마다 새로 만들고 끝나면 지우는 빈 임시 폴더(0700)를 가리키는 `GH_CONFIG_DIR`(`gh`는 로그인을 `~/.config/gh/hosts.yml`에서 스스로 읽으므로, 그 폴더에서는 `gh auth token`도 `gh auth git-credential`도 토큰을 내주지 않는다), 그리고 `SSH_AUTH_SOCK`, `GIT_ASKPASS`, `SSH_ASKPASS`, `GIT_CONFIG_PARAMETERS`, `GH_TOKEN`/`GITHUB_TOKEN`/`GH_ENTERPRISE_TOKEN`/`GITHUB_ENTERPRISE_TOKEN`을 뺀다. 같은 실행의 bash는 `gh auth`(모든 하위 명령)와 `gh alias`, 그리고 `GH_CONFIG_DIR`, `XDG_CONFIG_HOME`, `HOME`, 토큰 변수를 바꾸는 명령(`NAME=...` 대입, `export`/`declare`/`unset`/`read`/`for`, `env -u`, `env -i`, `exec -c`)을 거절한다(`sh -c`, `$(...)` 안까지). 빈 폴더만으로는 `GH_CONFIG_DIR=$HOME/.config/gh gh ...`나 키링의 토큰을 막지 못하기 때문이다. claude-code 백엔드도 같은 환경과 `Bash(git push *)`, `Bash(git -C * push*)`, `Bash(git config *)`(읽기도 막힌다), `Bash(git -c *)`, `Bash(git credential*)`, `Bash(gh auth *)`, `Bash(gh alias *)`, `Bash(*GH_CONFIG_DIR*)` 같은 거절 규칙을 받는다. 막지 못하는 것: 암호 없는 ssh 키 파일(`~/.ssh/id_*`)을 직접 쓰는 ssh, 홈 폴더 git 설정에 미리 있던 별칭, 스크립트 파일이나 다른 언어로 부르는 git과 gh(같은 사용자로 도는 프로그램은 `GH_CONFIG_DIR`을 되돌리고 `~/.config/gh/hosts.yml`이나 키링을 읽을 수 있다. 그래서 흐름의 `gh`/git 로그인은 봇 전용 계정을 권한다).
- 위 일곱 규칙은 비밀과 도구 검사를 지키는 것이라 `JUNEES_ALLOW_DANGEROUS=1`이어도 적용된다. 차단 목록일 뿐 샌드박스가 아니다(설정 파일, `mcp.json`, `secrets/` 읽기와 데이터 폴더 쓰기는 [실행 격리](#실행-격리-landlock)가 켜져 있으면 아래 방법으로도 막힌다): 다른 변수에 담은 경로(`d=~; cat $d/.junees/config.toml`), 실행 중에 만든 경로(`cat $(echo ~/.junees)/config.toml`), 더 위 폴더를 통째로 다루는 명령(`grep -r api_key ~`, `cp -r ~ /tmp/h`. 막으면 정상 작업까지 너무 많이 막힌다), 경로 없이 현재 폴더를 쓰는 명령(`cd ~/.junees && grep -r api_key`), 스크립트 본문 안에 쓴 경로(`python3 -c "open('/home/u/.junees/memory/user.md', 'a')"`), 파일로 써 둔 스크립트, 판정과 실제 열기 사이에 링크를 바꿔치기하는 경쟁은 막지 못한다. Junees 하위 명령도 같은 이유로 다음은 막지 못한다: 변수에 담거나 실행 중에 만든 프로그램 이름(`j=junees; $j cron add ...`, `$(which junees) plugin enable p --trust`), 다른 이름으로 복사한 바이너리나 같은 명령 안에서 만든 링크, `find -exec junees ...`와 `xargs junees ...`(`echo p | xargs junees plugin enable`), `watch`처럼 모르는 래퍼, `cargo run -- cron add ...`, 셸 함수와 alias, 스크립트 파일이나 다른 언어(`python3 -c "subprocess.run(['junees', 'plugin', 'install', ...])"`)로 부르는 것. claude-code 백엔드의 규칙은 쓰인 단어 그대로만 맞추므로 경로를 붙인 호출(`/usr/local/bin/junees cron add`), `env junees`, `bash -c '...'`도 지나간다. `gh` 쓰기 규칙도 같은 이유로 다음은 막지 못한다: `gh alias set m 'pr merge'` 뒤의 `gh m 7`(별칭), 변수에 담거나 실행 중에 만든 프로그램 이름(`g=gh; $g pr merge 7`), 다른 이름의 복사본이나 링크, `xargs gh ...`와 `find -exec gh ...`, 스크립트 파일이나 다른 언어(`python3 -c "subprocess.run(['gh', 'pr', 'merge', '7'])"`), 토큰으로 API를 직접 부르는 `curl -X PUT -H "Authorization: ..." https://api.github.com/...`(`gh auth token`으로 토큰을 꺼내 쓰는 것 포함)과 `hub` 같은 다른 클라이언트, 표에 없는 `gh` 확장 명령(`gh <extension>`)

검사 방식:

- 명령을 셸처럼 대략 나눠서 `;`, `&&`, `||`, `|`, `&`, 줄바꿈, 괄호로 이어진 각 부분, `$(...)`과 백틱 안, `sh -c '...'`, `eval`과 `env -S` 인자까지 검사한다. 셸이 표준 입력으로 읽는 스크립트도 검사한다: `bash <<'EOF'`의 here-document 본문, `bash <<< '...'`, `echo`/`printf`/here-document를 받은 `cat`을 파이프로 넘긴 `... | sh`. 앞에 붙은 `FOO=1`, `env`, `nohup`, `timeout 5`, `nice`, `setsid`, `stdbuf`, `ionice`, `chrt`, `taskset`, `busybox`, `command`, `then`/`do`, `function 이름` 같은 키워드와 래퍼는 건너뛰고 실제 실행되는 프로그램을 본다. `$'...'` 문자열은 이스케이프를 풀어서 본다. 프로그램 이름은 경로를 뗀 이름으로 비교한다(`/sbin/reboot`도 차단).
- 따옴표 안 문자열, 주석, 셸이 아닌 명령(`cat > notes.md <<'EOF'`, `python3 - <<'EOF'`)이 받는 here-document 본문은 명령으로 보지 않는다. `echo "rm -rf /"`, `grep shutdown log.txt`, `git commit -m "git push --force"`는 허용된다.
- 실행 중에 만들어지는 단어(`rm -rf $(echo /)`, 변수에 담긴 경로)는 알 수 없으므로 막지 못한다. `$(...)`, `sh -c`, `eval`이 합쳐서 8단계보다 깊게 중첩되면 검사하지 않고 막는다.
- `JUNEES_DENY_COMMANDS`의 접두사는 단어 단위로 비교한다. `docker rm`은 `docker rm -f web`을 막지만 `docker rmi`는 막지 않는다. 이 규칙은 `JUNEES_ALLOW_DANGEROUS=1`이어도 적용된다.
- `JUNEES_ALLOW_DANGEROUS`는 `1`/`true`/`yes`/`on` 또는 `0`/`false`/`no`/`off`만 받고, 다른 값이면 시작할 때 오류로 멈춘다.

## 실행 격리 (Landlock)

api 백엔드의 `bash` 도구가 실행하는 명령(대화, `-p`, 작업 모드, 예약 작업, 웹훅 실행)과 GitHub 흐름의 테스트 명령(`--test-cmd`, 그 웹훅 실행의 bash와 같은 규칙)은 Linux Landlock으로 묶인 채 돈다. 명령 정책을 통과한 명령에만 그 위에 겹쳐 적용되고, 정책이 문자열만 보고는 놓치는 경로(변수나 실행 중에 만든 경로, 스크립트 파일, 다른 언어로 연 파일)도 커널이 막는다. 규칙은 명령마다 Junees가 만들어 두고, 자식 프로세스가 bash를 실행하기 직전에 `no_new_privs`와 함께 자기에게 건다. 묶인 명령과 그 명령이 띄운 모든 프로세스는:

- 쓰기: 작업 폴더(명령이 도는 폴더), 임시 폴더(`/tmp`, `/var/tmp`, `/dev/shm`, `$TMPDIR`과 Junees 프로세스의 임시 폴더), `sandbox_write`의 폴더 아래에서만 파일을 만들고 바꾸고 지우고 옮길 수 있다. `/dev` 아래 이미 있는 파일(`/dev/null`, 터미널)에는 쓸 수 있다. 그 밖에서는 `Permission denied`.
- 읽기: 기본은 어디든 읽고 실행할 수 있다. `sandbox_read`를 주면 그 폴더들과 쓸 수 있는 폴더만 읽는다(bash가 시작하려면 `/usr`, `/lib`, `/lib64`, `/bin`, `/etc` 같은 시스템 폴더가 들어 있어야 한다. `junees doctor`가 확인한다).
- 언제나: 설정 파일(`$JUNEES_HOME/config.toml`), `mcp.json`, `$JUNEES_HOME/secrets/`(웹훅 시크릿, MCP OAuth 토큰), 실행 중인 브라우저의 프로필 폴더 `$JUNEES_HOME/browser/`(그 세션의 쿠키와 로그인)는 읽지도 바꾸지도 못하고(`/proc/self/root/...`로 돌아가도, 하드 링크를 새로 걸어도), 데이터 폴더(`memory`, `projects`, `skills`, `cron`, `plugins`, `webhooks`, `flows`)는 읽기만 된다. 작업 폴더가 이것들을 품어도(홈에서 실행) 마찬가지다. 웹훅 실행에서는 git과 GitHub CLI 자격 증명(`~/.git-credentials`, `~/.config/git/credentials`, `~/.config/gh/`)도 읽지 못한다. 다른 실행에서는 `gh`와 git push가 그 파일을 읽어야 하므로 명령 정책만 막는다.
- 네트워크: `sandbox_network = 0`이면 TCP 연결과 bind를 모두 막는다(Landlock ABI 4 이상, Linux 6.7부터). 기본은 허용이다. UDP(DNS 포함)와 유닉스 소켓은 막지 않는다.
- `no_new_privs`: setuid 프로그램(`sudo` 등)과 파일 capability가 권한을 주지 않는다.
- 다른 프로세스: Landlock의 ptrace 규칙으로, 격리된 명령은 격리되지 않은 프로세스(Junees, MCP 서버, 같은 사용자의 다른 프로그램)를 추적하거나 그 `/proc/<pid>/environ`, `mem`을 읽지 못한다.

`sandbox` 설정(`JUNEES_SANDBOX`):

- `auto`(대화, `-p`, 작업 모드의 기본): 커널이 Landlock을 지원하면 묶는다. 지원하지 않으면 묶지 않고 실행하되 그 bash 도구의 첫 결과 끝에 `[sandbox: <이유>; commands run without it (sandbox = auto)]` 한 줄을 붙인다. `sandbox_network = 0`인데 ABI가 4보다 낮으면 파일만 묶고 `[sandbox: ...; TCP is not restricted]`를 붙인다.
- `require`: 설정대로 묶을 수 없으면 실행하지 않고 `not run: the sandbox is required (sandbox = require) but <이유>` 오류 결과를 돌려준다. 예약 작업과 웹훅 실행은 `sandbox`를 따로 주지 않으면 `require`다(지켜보는 사람이 없다). 그런 서버에서 Landlock 없이도 돌리려면 `sandbox = "auto"`를 직접 준다.
- `off`: 묶지 않는다(명령 정책은 그대로).
- 지원하는 커널에서 규칙을 만들지 못하면(작업 폴더가 사라졌을 때 등) 모드와 상관없이 실행하지 않는다. 보호 경로로 가는 길의 폴더를 나열할 수 없으면(`/home`이 0711, 끊긴 마운트) 그 폴더에서 길 위의 항목만 따라가고 나머지 항목에는 규칙을 주지 않는다. 명령은 묶인 채 돌고, 그 항목들만 읽고 쓸 수 없다.

```toml
sandbox = "auto"
sandbox_write = "~/.cargo, ~/.cache"   # 쉼표 구분, 절대 경로나 ~/...
sandbox_network = "1"
```

작업 폴더 밖에 쓰는 흔한 명령은 기본 설정에서 실패한다: `cargo build`의 레지스트리 내려받기(`~/.cargo`), pip와 npm의 캐시(`~/.cache`, `~/.npm`), `git config --global`. 필요한 폴더를 `sandbox_write`에 더한다.

정한 것:

- 플러그인 명령과 stdio MCP 서버는 묶지 않는다. 사용자가 설치하고 켠 프로그램이고(모델은 입력만 준다), 자기 상태 폴더나 네트워크가 필요한 경우가 많아 bash와 같은 규칙으로는 깨진다. claude-code 백엔드도 대상이 아니다(Claude Code가 자기 도구를 자기 방식으로 돌린다). `read_file`/`write_file`/`edit_file`은 Junees 프로세스 안에서 돌고 지금처럼 경로 검사로 지킨다.
- 브라우저 도구([브라우저 도구](browser.md))도 묶지 않는다. Chrome은 bash가 아니라 Junees가 직접 띄우는 프로그램이고, 자기 샌드박스(`browser_no_sandbox`면 그것도 없이)와 주소 검사(사설, 루프백, 링크 로컬 주소는 `browser_allow_private`일 때만)로만 지켜진다. 그래서 `sandbox_network = 0`과 예약 작업, 웹훅 실행의 `require`는 bash 명령만 묶는다: `browser_unattended`를 켜면 무인 실행의 모델이 `sandbox_network = 0`이어도 브라우저로 공개 웹에 나갈 수 있다(페이지를 열고 폼을 채워 보내는 것 포함, 비밀번호와 결제 칸, `evaluate`는 묻는 사람이 없으니 거절된다). 무인 실행에서 네트워크를 막으려면 `browser_unattended`를 끈 채 둔다.
- 폴더 목록(`LANDLOCK_ACCESS_FS_READ_DIR`)은 다루지 않는다. Landlock 규칙은 허용만 하고 아래로 상속되므로, 비밀 폴더 하나만 빼고 그 위 폴더들을 나열할 수 있게 하는 방법이 없다. 그래서 `ls ~/.junees/secrets`로 파일 이름은 보인다(내용은 못 읽는다).
- `landlock` 크레이트 대신 세 시스템 호출을 libc로 직접 부른다(구조체 두 개와 상수 몇 개라 의존성이 필요 없다). Linux가 아니면 지원하지 않는 커널과 같이 다룬다.

한계:

- Landlock은 경로 기준이다. 같은 파일을 다른 경로로 보여 주는 bind mount와 미리 있던 하드 링크는 막지 못한다.
- 보호 경로를 품은 폴더(`/`, `/home`, 홈, `$JUNEES_HOME`, 그리고 그런 폴더가 작업 폴더일 때)에는 폴더 자체가 아니라 그 안의 항목마다 규칙을 준다. 그래서 명령이 시작된 뒤 그 폴더 바로 아래에 생긴 항목은 읽을 수 없고, 묶인 명령은 그 폴더 바로 아래에서 항목을 만들거나 지우지 못한다(홈에서 실행하면 `~/새파일`은 안 되고 `~/하위폴더/새파일`은 된다). 상태 폴더가 아직 없으면(보호할 파일이 없으면) 이 제한도 없다. 작업 폴더가 그런 폴더면 그 bash 도구의 첫 결과 끝에 `[sandbox: the working folder holds Junees's state folder, ...; work in a subfolder]` 한 줄로 알린다. 그 폴더에 쓰기를 통째로 허용하지 않는 것은, 그러면 같은 폴더 안에서 이름만 바꾸는 것(`mv config.toml x`, 폴더를 옮기지 않으므로 Landlock의 `REFER` 검사도 없다)을 막을 수 없어 다음 명령에서 새 이름으로 읽히기 때문이다. `~/.junees`가 다른 곳(`/data/state`)을 가리키는 심볼릭 링크여도 같다: 보호 경로를 링크를 푼 경로와 쓰인 경로 둘 다로 계산하고, 가는 길의 링크에는 규칙을 주지 않으므로 홈에서 실행해도 링크 자체를 옮기거나 바꿔 치지 못한다(Junees는 다음 실행에서 링크를 거쳐 읽는다).
- 묶이지 않은 같은 사용자의 프로세스에 신호를 보내는 것과 이미 떠 있던 프로세스(예: 명령 전에 띄운 서버, Junees 자신)를 통한 우회는 막지 않는다. 묶인 명령이 백그라운드로 띄운 서버는 묶인 채로 남는다.
- 커널이 지원하지 않으면 `auto`에서는 이전처럼 명령 정책만 남는다.

## 실행 전 승인

대화 모드에서는 위험할 수 있는 도구 호출을 실행하기 전에 터미널에서 묻는다. 묻는 범위는 `JUNEES_APPROVAL`로 정한다.

- `risky`(기본): 명령 정책이 막지는 않지만 주의할 신호가 있는 `bash` 명령, 작업 폴더 밖 경로를 고치는 `write_file`/`edit_file`, 그리고 `trust`가 없는 MCP 서버와 플러그인의 도구([MCP 서버](mcp.md#mcp-서버), [플러그인](plugins.md#플러그인) 절).
- `all`: 모든 `bash`, `write_file`, `edit_file`, MCP 도구, 플러그인 도구 호출. `read_file`은 묻지 않는다.
- `off`: 묻지 않는다(이전 동작).

`bash`의 주의 신호는 명령 정책과 같은 파서로 찾는다(`$(...)`, `sh -c`, `eval`, 셸에 파이프로 넘긴 스크립트, `timeout`/`env` 같은 래퍼, `find -exec` 안까지).

- 파일 삭제: `rm`, `rmdir`, `unlink`, `shred`, `find -delete`
- git: `git push`(모든 push), 작업을 버리는 명령(종류 `discard git work`: `git reset --hard`, `--`/`.`/`-f`가 붙은 `git checkout`, 작업 트리를 되돌리는 `git restore`(`--staged`만이면 제외), `git stash drop`/`clear`, `git branch -D`), `git clean`(`-n`/`--dry-run` 제외)
- 패키지 설치/제거: `apt`/`apt-get`/`dnf`/`yum`/`brew`/`pacman`/`apk` 등의 install/remove/upgrade, `pip install`, `python -m pip install`, `npm`/`pnpm`/`yarn`/`bun`의 install/add/ci/update, `cargo install`, `go install`, `gem install`, `uv pip install`/`uv add`, 그리고 레지스트리에서 받아 바로 실행하는 `npx`/`pnpx`/`bunx`/`uvx`
- 파일로 내려받기: `curl -o`/`-O`, `wget`(`-O -`처럼 표준 출력으로 받거나 `--spider`면 제외). 내려받은 스크립트를 셸에 바로 넘기는 것은 명령 정책이 막는다
- 권한 변경: `chmod`, `chown`, `chgrp`, `chattr`, `setfacl`
- 작업 폴더 밖에 쓰기: 리다이렉트(`>`, `>>`, `2>`, `&>`; `2>&1`과 `/dev/null` 같은 장치는 제외), `cp`/`ln`/`install`의 대상(`-t` 포함), `mv`/`tee`/`touch`/`mkdir`/`truncate`의 인자, `sed -i`의 파일, `dd of=`, 삭제 명령의 대상
- Junees 자신의 상태를 바꾸는 하위 명령(`junees cron add` 등, [명령 정책](#명령-정책)): 종류 `change Junees's own state`. 승인 게이트가 있을 때만 명령 정책이 통과시키고, 이 신호로 `risky`에서도 묻는다
- `gh`로 GitHub에 쓰는 명령(`gh pr merge`, `gh pr review`, `gh api -X POST` 등, [명령 정책](#명령-정책)): 종류 `write to GitHub through gh`, merge는 `merge on GitHub through gh (asked every time)`("항상 허용"이 덮지 않는다). 승인 게이트가 있을 때만 명령 정책이 통과시키고, 이 신호로 `risky`에서도 묻는다
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
