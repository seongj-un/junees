# 게이트웨이

Junees를 채팅 봇으로 띄운다. 디스코드, 텔레그램, 슬랙은 같은 엔진(채널마다 대화 하나, 대기열, 승인 질문, 세션 기록)을 쓴다.

## 디스코드 게이트웨이

`junees gateway`(또는 `junees discord`)는 Junees를 디스코드 봇으로 띄운다. 터미널 대화처럼 도구, 메모리, 스킬, 세션 기록, 실행 전 승인을 그대로 쓰고, 채널(스레드, DM)마다 대화 하나를 따로 둔다. 봇 프레임워크(serenity, twilight) 없이 Gateway v10 WebSocket과 REST를 직접 구현했다(의존성은 `tokio-tungstenite`와 `futures-util`만 늘었다).

### 봇 만들기

1. [개발자 포털](https://discord.com/developers/applications)에서 New Application을 만들고, Bot 페이지에서 Reset Token으로 토큰을 받는다. 토큰은 비밀번호와 같다([설정](#설정)의 설정 파일이나 환경 변수에만 둔다).
2. 같은 Bot 페이지의 Privileged Gateway Intents에서 **MESSAGE CONTENT INTENT를 켠다.** 메시지 내용은 특권 인텐트라서 꺼져 있으면 디스코드가 연결을 `4014 Disallowed intent(s)`로 닫고, Junees는 이 스위치를 켜라는 안내와 함께 끝난다([Gateway: Privileged Intents](https://docs.discord.com/developers/events/gateway#privileged-intents)). 봇이 쓰는 인텐트는 `GUILDS`(스레드 이벤트), `GUILD_MESSAGES`, `DIRECT_MESSAGES`, `MESSAGE_CONTENT`다.
3. 서버에 초대한다. OAuth2 페이지의 URL Generator에서 scope `bot`, 권한 View Channels, Send Messages, Send Messages in Threads, Read Message History를 고르거나, 아래 URL의 `client_id`를 자기 애플리케이션 id로 바꿔 연다([OAuth2: Bot Authorization Flow](https://docs.discord.com/developers/topics/oauth2#bot-authorization-flow)). 권한 값 274877975552는 `VIEW_CHANNEL`(1<<10), `SEND_MESSAGES`(1<<11), `READ_MESSAGE_HISTORY`(1<<16), `SEND_MESSAGES_IN_THREADS`(1<<38)의 합이다.
   ```
   https://discord.com/oauth2/authorize?client_id=<애플리케이션 id>&scope=bot&permissions=274877975552

   승인 버튼에는 따로 켤 인텐트나 권한이 없다(`INTERACTION_CREATE`는 인텐트 없이 오고, 버튼이 달린 메시지도 Send Messages로 보낸다). 다만 General Information 페이지의 **Interactions Endpoint URL은 비워 둔다.** 이 주소를 넣으면 디스코드가 버튼 누름을 Gateway가 아니라 그 HTTP 주소로 보내서("you can *only* receive Interactions one of the two ways", [Receiving and Responding](https://docs.discord.com/developers/interactions/receiving-and-responding)), 버튼이 "상호작용 실패"로 끝나고 글 답장만 된다.
   ```
4. 디스코드 설정의 고급에서 개발자 모드를 켜면 사용자와 채널을 우클릭해 id를 복사할 수 있다(Copy User ID, Copy Channel ID).

### 설정

`junees setup`의 마지막 질문(선택)이나 설정 파일, 환경 변수로 둔다. 환경 변수가 파일보다 우선한다.

```toml
discord_token = "..."                 # JUNEES_DISCORD_TOKEN (필수)
discord_allowed_users = "123456789012345678"   # JUNEES_DISCORD_ALLOWED_USERS (필수, 쉼표 목록)
discord_channels = "234567890123456789"        # JUNEES_DISCORD_CHANNELS (선택, 쉼표 목록)
discord_parallel = 2                  # JUNEES_DISCORD_PARALLEL (동시에 턴을 돌릴 채널 수, 기본 2)
discord_approval_timeout = "5m"       # JUNEES_DISCORD_APPROVAL_TIMEOUT (승인 버튼/답장 대기, 기본 5m)
```

- `discord_allowed_users`가 비어 있으면 시작하지 않는다(`no allowed Discord users ...`). 목록이 없으면 봇에게 말을 걸 수 있는 누구나 이 컴퓨터에서 명령을 실행할 수 있기 때문이다. id는 숫자만 받는다.
- 토큰은 화면, 로그, 오류에 나오지 않는다: `Debug`는 `Token(hidden)`이고, REST의 `Authorization: Bot <토큰>` 헤더(민감 표시)와 Gateway의 IDENTIFY/RESUME 페이로드에만 들어간다. 오류는 경로(`Discord GET /gateway/bot: 401 Unauthorized ...`)와 닫힘 코드만 말한다. bash 도구, 작업 git, Claude Code의 환경에서 `JUNEES_DISCORD_TOKEN`을 지우고(`SECRET_ENV_VARS`), 설정 파일 보호(모델의 파일 도구와 bash 정책이 `config.toml`을 막음)도 그대로다. 메모리와 스킬은 디스코드 봇 토큰 모양(점으로 이은 세 base64 조각, 첫 조각이 숫자 id)의 내용을 저장하지 않는다(`src/secrets.rs`).
- `JUNEES_DISCORD_API_URL`(환경 변수만, 기본 `https://discord.com/api/v10`)은 테스트와 프록시용이다. 토큰이 평문으로 나가지 않도록 `https://`만 받고 `http://`는 이 컴퓨터(`127.0.0.1`, `localhost`, `::1`)일 때만 받는다. Gateway 주소(`Get Gateway Bot`의 `url`, Ready의 `resume_gateway_url`)도 `wss://`(이 컴퓨터면 `ws://`)만 쓴다.
- 백엔드는 설정의 `backend` 그대로다. api면 `ANTHROPIC_API_KEY`가, claude-code면 로그인된 `claude`가 필요하다(시작할 때 확인).

### 실행

```sh
cd ~/work/some-project      # 도구가 도는 폴더 (세션 기록의 cwd)
junees gateway
```

연결, 재연결, 채널별 세션 같은 진행 상황은 표준 오류에 시각과 함께 한 줄씩 나온다(메시지 내용과 토큰은 찍지 않는다). SIGINT/SIGTERM이면 진행 중인 턴을 취소해 세션에 기록하고(채널에 `(stopped: Junees is shutting down)`), Gateway를 1000으로 닫고 종료 코드 0으로 끝난다. 두 번째 시그널은 바로 끝낸다. 게이트웨이는 처음 실행 마법사를 띄우지 않는다.

systemd 사용자 서비스 예시(`~/.config/systemd/user/junees-gateway.service`):

```ini
[Unit]
Description=Junees Discord gateway
After=network-online.target

[Service]
WorkingDirectory=%h/work/some-project
ExecStart=%h/.cargo/bin/junees gateway
Restart=on-failure
RestartSec=10
# 설정은 ~/.junees/config.toml. 환경 변수로 줄 때는 0600 파일에:
# EnvironmentFile=%h/.config/junees/gateway.env

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now junees-gateway
journalctl --user -u junees-gateway -f     # 로그
loginctl enable-linger "$USER"             # 로그아웃해도 계속 돌게
```

`systemctl --user stop`은 SIGTERM이므로 위처럼 턴을 정리하고 끝난다. claude-code 백엔드를 서비스로 돌리면 서비스 환경에서도 `claude`가 로그인돼 있어야 한다(`CLAUDE_CODE_OAUTH_TOKEN`이라면 `EnvironmentFile`로).

### 누구의 어떤 메시지에 답하나

- `discord_allowed_users`의 사용자만. 다른 사용자의 메시지는 무시하고 로그에 사용자 id를 한 번만 남긴다. 봇(자기 자신 포함)의 메시지와 일반/답장이 아닌 시스템 메시지도 무시한다.
- DM은 늘 답한다. 서버 채널에서는 봇을 멘션했을 때(`@Junees 질문`, 멘션 글자는 빼고 넘긴다), `discord_channels`에 있는 채널과 스레드, 그 채널 아래 스레드, 봇이 만든 스레드에서만 답한다. 그 밖의 채널에서는 명령도 멘션과 함께 쓴다(`@Junees !stop`). 승인 질문이 기다리는 채널에서는 멘션 없는 답장도 받는다.
- 모델 출력이 누구도 호출하지 않도록 모든 메시지를 `allowed_mentions: {"parse": []}`로 보낸다(`@everyone`을 써도 알림이 가지 않는다). 링크 미리보기(embed)도 끈다(`flags: 4`, SUPPRESS_EMBEDS): 주입된 모델이 답에 `https://공격자/?d=<비밀>`을 쓰면 디스코드의 미리보기 수집기가 그 주소를 열어, 도구 호출이나 승인 없이(`sandbox_network = 0`이어도) 비밀이 나갈 수 있기 때문이다. 텔레그램과 슬랙도 미리보기를 끈다. 예약 작업과 웹훅의 디스코드 전달도 같은 요청을 쓴다.

### 대화

- 채널(스레드, DM)마다 Junees 세션 하나. 세션은 보통 세션 기록(`$JUNEES_HOME/sessions/<id>.jsonl`, cwd는 게이트웨이를 띄운 폴더)이고, 어느 채널이 어느 세션인지는 `$JUNEES_HOME/gateway/discord-sessions.json`(채널 id -> 세션 id, 0600)에 둔다. 다시 띄우면 채널의 첫 메시지 때 그 세션을 이어 간다. 세션은 flock으로 잠그므로, 다른 Junees(`junees -c`, `--resume`)가 그 세션을 열고 있거나 디스크 오류로 읽지 못하면 채널을 새 세션으로 옮기지 않는다. 그 메시지는 실행하지 않고 `(not run) cannot continue session <id> for now (...)`로 알리며, 다음 메시지 때 다시 시도한다(`!new`면 새로 시작). 지워졌거나 깨진 세션이면 새로 시작하고 이유가 `!status`에 나온다. 백엔드를 바꾸면 이전 백엔드의 세션은 이어 가지 않고 새로 시작한다. 채널 세션의 meta 줄에는 `"source": "discord"`와 `"channel": "<채널 id>"`가 들어간다. 그래서 게이트웨이를 띄운 폴더에서 `junees -c`를 해도 채널 세션을 고르지 않고(그동안 채널이 잠금 때문에 `(not run)`이 되지 않게), `/sessions`는 채널 세션을 `[discord <채널 id>]`를 붙여 번호 없이 보여 주기만 한다. 꼭 열어야 하면 `junees --resume <id>`로 연다(게이트웨이가 그 세션을 쥐고 있지 않을 때). 이 필드가 생기기 전에 만든 채널 세션에는 표시가 없어 예전처럼 `-c`와 `/sessions` 후보에 나온다.
- 한 `$JUNEES_HOME`에는 게이트웨이 하나만 돈다. 시작할 때 `$JUNEES_HOME/gateway/lock`에 flock을 잡고, 이미 잡혀 있으면 `another Junees gateway is running ...` 오류로 끝난다(두 프로세스가 같은 메시지에 두 번 답하고 도구도 두 번 돌리지 않도록). 프로세스가 죽으면 커널이 잠금을 푼다.
- 같은 채널의 메시지는 받은 순서대로 하나씩 처리한다(턴 중에 온 메시지는 대기열). 다른 채널은 동시에 돌고, 동시에 턴을 돌리는 채널은 `discord_parallel`(기본 2)까지다. 자리가 없으면 `(waiting for a free slot: ...)`을 알리고 기다린다(이때 `!stop`이면 시작하지 않는다). 모든 채널의 대화는 한 스레드에서 비동기로 돈다(에이전트의 future가 `Send`가 아니라서 `LocalSet`).
- 출력: 턴 중에는 타이핑 표시(8초마다 갱신, 승인 질문을 기다리는 동안은 멈춤). 응답은 메시지 편집으로 스트리밍하지 않고 **완성된 텍스트 블록마다** 보낸다(편집은 레이트 리밋을 빨리 쓰고, 끊긴 응답의 재시도 처리가 복잡해진다). 다음 텍스트 전까지의 도구 호출은 `-> bash ls -la` 같은 한 줄 요약(명령, 경로, 검색어 120자)을 코드 블록 하나로 묶어 보낸다. 2000자가 넘는 블록은 줄 단위로 나누고, 코드 블록이 잘리면 앞 메시지에서 닫고 다음 메시지에서 같은 언어로 다시 연다(다시 여는 줄은 펜스와 32자 이하의 언어만이고, ```` ```rm -rf build``` ````처럼 같은 줄에서 닫히는 펜스는 코드 블록으로 보지 않는다. 길이는 UTF-16 단위로 세어 어느 방식으로 세도 2000자를 넘지 않게 한다). 잘리거나 압축된 응답 같은 안내도 채널에 나온다. 오류는 `error: ...`와 터미널과 같은 힌트로 보낸다.
- 시스템 프롬프트에 "디스코드로 대화 중이니 짧게, 표 없는 Markdown"이라는 한 문단을 더한다.
- 이미지 첨부: 허용된 사용자의 메시지에 붙은 첨부 중 `content_type`이 `image/*`인 것(없으면 확장자)을 그 턴이 시작할 때 내려받아 메시지 글 앞에 이미지 블록으로 넣는다(api 백엔드, [이미지 입력](usage.md#이미지-입력)). 글 없이 이미지만 보내도 된다. 디스코드 CDN(`https://cdn.discordapp.com`, `https://media.discordapp.net`)에서만, 리다이렉트는 그 호스트로 3번까지, 30초 안에, 5 MB까지 받고, 받은 바이트가 PNG/JPEG/GIF/WebP가 아니면 버린다. 한 메시지에 10개까지. 이미지가 아닌 파일, 큰 파일, CDN 밖 주소, 받지 못한 것은 `(attachments left out: ...)`로 채널에 알리고 같은 줄을 모델에게도 붙인다(못 받은 그림에 대해 지어내지 않게). 오류 문구와 로그에는 서명된 첨부 URL을 쓰지 않고, 로그에는 형식과 크기만 남긴다. 테스트용 다른 주소는 환경 변수 `JUNEES_DISCORD_CDN_URL`(https, http는 이 컴퓨터만)로만 준다.
- 명령(슬래시 명령 등록 없이 텍스트로):
  - `!new`: 새 대화(새 세션, 메모리와 스킬 다시 읽음). 대기열 순서대로 처리된다.
  - `!stop`: 진행 중인 턴을 취소하고(도구의 프로세스 그룹까지, 기록은 터미널의 Ctrl-C와 같다) 대기열의 메시지를 버린다. 자리를 기다리던 턴과 첨부 이미지를 내려받던 턴도 시작하지 않고 멈춘다(`!status`에는 "a turn downloads its attachments").
  - `!status`: 백엔드와 모델, 동시 실행 중인 턴 수, 승인 방식, 이 채널의 세션과 상태(실행 중/대기열/승인 대기), 토큰 사용량.
  - `!memory`: 영구 메모리 보기. DM에서는 터미널의 `/memory`와 같다. 서버 채널에서는 답이 그 채널을 보는 모두에게 보이므로 저장소마다 항목 수와 글자 수만 보여 주고, 내용은 DM에서 보라고 안내한다. `!skills`: 스킬 목록. `!cron`: 예약 작업 목록(서버 채널에서는 프롬프트와 폴더 없이, [예약 작업](automation.md#예약-작업) 절). `!help`: 명령 목록.
  - `!task`: 아직 지원하지 않는다는 안내([범위 밖](#범위-밖-한계)).
- 승인(`JUNEES_APPROVAL`, 기본 `risky`): 위험한 도구 호출이면 채널에 `Approve this tool call?`과 명령(코드 블록, 터미널 질문과 같은 내용)을 버튼과 함께 보내고, 허용 사용자가 버튼을 누르거나 그 채널에 다음 메시지를 쓰면 정한다. 버튼은 Allow(실행), Always allow(이 채널의 대화가 끝날 때까지 같은 종류 허용. 매번 묻는 종류뿐이면 없다), Decline(거절)이다. 글 답장도 그대로 된다: `y`는 실행, `n`은 거절, `n <이유>`는 이유와 함께 거절(모델이 읽는다), `a`는 같은 종류 허용, 그 밖의 말은 그 말을 이유로 거절한다. `discord_approval_timeout`(기본 5분) 안에 답이 없으면 `(no answer within 5m: declined)`와 함께 거절한다. `!stop`은 질문을 취소하고 턴을 끝낸다. 질문에 답한(버튼이나 글 답장) 사용자가 30초 안에 답 낱말 하나만(`y`, `yes`, `a`, `always`, `n`, `no`, 대소문자 무관) 다시 보내면, 버튼을 누르고 확인 삼아 한 번 더 친 답으로 보고 새 턴의 입력으로 넘기지 않는다. 대신 `(that approval question was already answered; this reply was not passed on. Send it again to pass it on.)`로 알린다. 이렇게 걸러지는 것은 답한 뒤 그 사용자의 첫 메시지뿐이라, 같은 말을 다시 보내거나 다른 말을 보내면 평소처럼 입력이 된다(슬랙과 텔레그램도 같다). 질문이 끝나면(버튼, 글 답장, 시간 초과, `!stop`) 질문 메시지를 고쳐 버튼을 없애고 아래에 결과를 굵게 적는다(`Allowed by @사용자.`, `Declined.`, `No answer within 5m: declined.`, `Not answered: the turn stopped.`). claude-code 백엔드는 터미널처럼 승인 질문 없이 Claude Code의 거절 규칙만 적용된다(`!status`에 나온다).
- 승인 버튼의 동작([Message Components](https://docs.discord.com/developers/components/reference), [Receiving and Responding](https://docs.discord.com/developers/interactions/receiving-and-responding)): 버튼은 질문 메시지(길어서 나뉘면 마지막 메시지)의 액션 행에 달리고, 각 버튼의 `custom_id`는 `junees:<질문 id>:<y|a|n>`이다. 질문 id는 질문마다 새로 뽑은 128비트 난수(시스템 난수, 16진 32자리)라 짐작할 수 없다. 누르면 디스코드가 Gateway로 `INTERACTION_CREATE`(type 3, MESSAGE_COMPONENT)를 보내고, 3초 안에 HTTP로 응답해야 한다. Junees는 받는 즉시 판단해 응답을 따로 보낸다(전역 레이트 리밋 대기 없이: 문서상 상호작용 응답은 전역 한도에 묶이지 않는다). 누름이 받아들여지는 것은 누른 사람이 `discord_allowed_users`에 있고, 그 채널에서 지금 기다리는 질문의 id이며, 그 질문 메시지에서 눌렸고, 그 질문에 있는 버튼일 때뿐이다. 그때는 type 6(DEFERRED_UPDATE_MESSAGE)으로 응답하고 메시지 편집(`PATCH /channels/{id}/messages/{id}`)으로 버튼을 없앤다. 그 밖의 누름(허용 목록 밖 사람, 끝난 질문이나 시간 초과·`!stop` 뒤의 버튼, 다른 채널, 재시작 전의 버튼)은 아무것도 바꾸지 않고, 누른 사람에게만 보이는 type 4 `EPHEMERAL` 메시지로 알린다(`This question was already answered, timed out or stopped; ...`, 허용 목록 밖이면 `Only the users in discord_allowed_users can answer Junees.`, 허용 목록 밖 사용자는 로그에 한 번 남긴다). 상호작용 토큰은 로그와 오류에 나오지 않는다.

### 연결과 레이트 리밋

[Gateway 문서](https://docs.discord.com/developers/events/gateway)를 따른다. `Get Gateway Bot`으로 받은 주소에 `?v=10&encoding=json`으로 연결하고(그 주소를 재연결에 다시 쓴다), Hello의 `heartbeat_interval`에 0~1 사이 무작위 값을 곱한 뒤 첫 하트비트를, 그 뒤로는 간격마다 마지막 시퀀스 번호와 함께 보낸다. 디스코드가 하트비트(op 1)를 요청하면 바로 보낸다. 다음 하트비트 때까지 ACK(op 11)가 없으면 좀비 연결로 보고 1000/1001이 아닌 코드(4000)로 닫은 뒤 Ready의 `resume_gateway_url`로 RESUME(op 6, `session_id`, `seq`)한다. Reconnect(op 7), 재연결 가능한 닫힘 코드, 코드 없는 끊김도 RESUME한다. Invalid Session(op 9)이 `d: false`면 1~5초 기다린 뒤 처음 주소로 새로 IDENTIFY한다. 연결 자체가 실패해도(거절, 시간 초과, 핸드셰이크 실패) 디스코드 쪽 세션은 그대로이므로 세션 정보를 남기고 다시 RESUME한다(세션이 없어졌으면 디스코드가 op 9로 알려 준다). `resume_gateway_url`에 세 번 잇달아 붙지 못하면 처음 주소로 RESUME한다(문서: 그 주소를 쓰지 않으면 끊김이 잦아질 뿐). 4007, 4009는 새 세션, 4004(토큰 거절), 4010~4014는 다시 시도하지 않고 이유와 함께 끝난다([닫힘 코드](https://docs.discord.com/developers/topics/opcodes-and-status-codes#gateway-gateway-close-event-codes)). 재연결 사이 대기는 1초에서 두 배씩 120초까지 늘고 Ready/Resumed를 받으면 처음으로 돌아간다(하루 1000번의 IDENTIFY 한도를 아끼려고). 압축(zlib/zstd)과 샤딩은 쓰지 않는다(봇 하나, 서버 몇 개 기준).

REST는 [레이트 리밋 문서](https://docs.discord.com/developers/topics/rate-limits)대로 하드코딩 없이 응답 헤더를 따른다: 경로(메서드 + 채널 id)마다 `X-RateLimit-Remaining`이 0이면 `X-RateLimit-Reset-After`까지 다음 요청을 미룬다. 429면 본문의 `retry_after`(없으면 `Retry-After` 헤더)만큼 기다렸다 다시 보내고, `global: true`면 모든 경로를 멈춘다. 429는 다섯 번, 네트워크 오류와 5xx는 세 번까지 시도한다. 보낸 뒤에 실패한 메시지를 다시 보내도 두 번 올라가지 않도록, 메시지마다 무작위 `nonce`를 정해 `enforce_nonce: true`와 함께 보내고 재시도에도 같은 값을 쓴다([Create Message](https://docs.discord.com/developers/resources/message#create-message): 같은 작성자가 몇 분 안에 같은 nonce로 만든 메시지가 있으면 새로 만들지 않고 그 메시지를 돌려준다). 10분 넘게 기다리라면 그 메시지는 실패로 로그에 남긴다. `X-RateLimit-Bucket`으로 경로를 묶지는 않는다(묶인 경로가 먼저 429를 받으면 그때 기다린다). User-Agent는 문서가 요구하는 `DiscordBot (https://github.com/seongj-un/junees, <버전>)`이다.

### 범위 밖, 한계

- `!task <목표>`(게이트웨이에서 장기 작업 시작)는 후속으로 미뤘다. 작업 모드는 프로세스의 현재 폴더를 저장소 루트로 옮기고 한 저장소에 하나만 돌기 때문에, 여러 채널이 동시에 도는 게이트웨이 안에서는 따로 설계해야 한다. 지금은 컴퓨터에서 `junees task`를 돌리고 진행 알림은 웹훅(`notify_webhook`)으로 받는다.
- 디스코드 애플리케이션(슬래시) 명령 등록, 이미지가 아닌 첨부 파일 읽기, 메시지 편집 스트리밍, 반응 이모지, 봇이 스스로 스레드를 만드는 것은 하지 않는다.
- 재시작하면 기다리던 질문은 사라지므로(턴도 끝난다) 그 전 질문 메시지에는 버튼이 남는다. 누르면 "already answered" 안내만 받는다. 질문 메시지를 올리다 실패하면 버튼 없이 글로 다시 올리고 글 답장으로 받는다.
- 테스트는 테스트 안에서 띄운 가짜 Gateway(WebSocket)와 가짜 REST 서버로만 했다(`tests/discord_gateway.rs`). 실제 디스코드에는 연결해 보지 않았다.

## 텔레그램 게이트웨이

`junees gateway telegram`(또는 `junees telegram`)은 Junees를 텔레그램 봇으로 띄운다. 디스코드 게이트웨이와 같은 엔진(`src/gateway/channels.rs`)을 쓰므로 채팅마다 대화 하나, 대기열과 동시 실행 상한, 세션 기록과 재시작 뒤 이어 가기, 승인 질문, 이미지 첨부, `stop`/`status`/`new` 명령이 디스코드와 같다. 다른 점만 적는다. [Bot API](https://core.telegram.org/bots/api)를 직접 부르고(라이브러리 없음, 새 의존성 없음) 업데이트는 `getUpdates` 롱 폴링으로 받는다(웹훅 아님: 공개 주소가 필요 없다).

### 봇 만들기와 설정

1. 텔레그램에서 @BotFather에게 `/newbot`을 보내 봇을 만들고 토큰(`123456789:AA...`)을 받는다.
2. 내 사용자 id(숫자)를 알아 둔다. 예: @userinfobot에게 아무 메시지나 보내면 알려 준다. 사용자 이름(`@name`)은 받지 않는다(바꿀 수 있어서).
3. 그룹에서 쓰려면 봇을 그룹에 넣는다. 기본 개인 정보 모드(privacy mode)에서는 봇이 명령, 봇 멘션, 봇 메시지에 대한 답장만 받는데, Junees도 그룹에서는 그것에만 답하므로 그대로 두면 된다.

`junees setup`은 텔레그램을 묻지 않는다. 설정 파일이나 환경 변수로 둔다(환경 변수가 우선).

```toml
telegram_token = "..."                  # JUNEES_TELEGRAM_TOKEN (필수)
telegram_allowed_users = "123456789"    # JUNEES_TELEGRAM_ALLOWED_USERS (필수, 숫자 id 쉼표 목록)
telegram_parallel = 2                   # JUNEES_TELEGRAM_PARALLEL (동시에 턴을 돌릴 채팅 수, 기본 2)
telegram_approval_timeout = "5m"        # JUNEES_TELEGRAM_APPROVAL_TIMEOUT (승인 버튼/답장 대기, 기본 5m)
telegram_format = "html"                # JUNEES_TELEGRAM_FORMAT (답의 서식: html 기본, plain은 평문)
```

- `telegram_allowed_users`가 비어 있으면 시작하지 않는다(`no allowed Telegram users ...`). 텔레그램 봇은 이름만 알면 누구나 말을 걸 수 있기 때문이다.
- 토큰: Bot API는 토큰을 요청 URL 경로(`https://api.telegram.org/bot<토큰>/<메서드>`, 파일은 `/file/bot<토큰>/<경로>`)에 받는다. 그래서 요청 URL을 어디에도 출력하지 않는다: 연결 오류는 종류와 원인만(`reqwest`의 `without_url`), 서버가 보낸 오류 설명은 토큰(과 콜론 뒤 비밀 부분)을 `[hidden]`으로 바꾼 뒤 쓴다(`Telegram getMe: error 401 (Unauthorized); check the bot token (telegram_token)`). `Debug`는 `Token(hidden)`. 토큰은 `숫자:영문/숫자/_/-` 모양만 받아 경로를 바꿀 수 없고, 모양이 틀려도 값을 되풀이하지 않는다. bash, MCP 서버, 작업 git의 환경에서 `JUNEES_TELEGRAM_TOKEN`을 지우고(`SECRET_ENV_VARS`), `junees doctor`는 값을 `[hidden]`으로 가리며, 메모리와 스킬과 GitHub로 나가는 글은 텔레그램 봇 토큰 모양을 거절한다(`src/secrets.rs`).
- `JUNEES_TELEGRAM_API_URL`(환경 변수만)은 테스트용이다. 기본 `https://api.telegram.org` 말고는 이 컴퓨터 주소(`127.0.0.1` 등 루프백, `::1`, `localhost`, http/https)만 받는다. 파일도 이 주소에서만 받으므로 실제로는 api.telegram.org의 https에서만 내려받는다. 리다이렉트는 따르지 않는다. 그래서 로컬 Bot API 서버(`telegram-bot-api`)는 이 컴퓨터에서 돌 때만 쓸 수 있고, 그 서버가 주는 절대 경로 `file_path`는 받지 않는다.

```sh
cd ~/work/some-project      # 도구가 도는 폴더 (세션 기록의 cwd)
junees gateway telegram
```

진행 상황은 디스코드 게이트웨이처럼 표준 오류에 한 줄씩 나오고(메시지 내용과 토큰은 찍지 않는다), SIGINT/SIGTERM이면 턴을 취소해 기록한 뒤 끝난다. systemd 예시는 디스코드 절의 `ExecStart`를 `junees gateway telegram`으로 바꾸면 된다. 한 `$JUNEES_HOME`에는 텔레그램 게이트웨이도 하나만 돈다(`$JUNEES_HOME/gateway/telegram.lock`). 디스코드 게이트웨이와는 잠금이 달라 함께 돌릴 수 있다.

### 누구의 어떤 메시지에 답하나

- `telegram_allowed_users`의 사용자만. 다른 사용자의 메시지는 무시하고 로그에 사용자 id를 한 번만 남긴다(사진도 내려받지 않는다). 봇의 메시지와 채널 게시물, 수정된 메시지는 받지 않는다(`allowed_updates: ["message"]`).
- 개인 채팅은 늘 답한다. 그룹과 슈퍼그룹에서는 봇을 멘션했을 때(`@봇이름`, mention 엔티티나 text_mention. 멘션 글자는 빼고 넘긴다), 봇 메시지에 답장했을 때, 그 채팅에서 승인 질문이 답을 기다릴 때만 답한다. 그렇게 받아들인 앨범의 나머지 사진은 캡션(멘션)이 없어도 받는다([대화와 출력](#대화와-출력)의 이미지).
- 개인 정보 모드(privacy mode, 그룹에 넣은 봇의 기본값): Bot API 문서(https://core.telegram.org/bots/features#privacy-mode, 2026-11-12 `curl`)에 따르면 이 모드의 봇은 그룹에서 자기를 가리킨 명령(`/command@봇이름`), 마지막으로 말한 봇일 때의 일반 명령(`/start`), 자기 메시지에 대한 답장, 서비스 메시지만 받는다(개인 채팅은 모두 받는다). 이 목록에 `@봇이름` 멘션 글과 캡션 없는 앨범 사진은 없으므로, 이 모드에서는 그룹의 멘션 글이나 앨범의 나머지 사진이 봇에 오지 않을 수 있다. 실제 텔레그램으로는 확인하지 못했다(#58: Junees의 시험은 실제 봇 토큰 없이 가짜 Bot API로만 한다). 그룹에서 멘션과 앨범을 확실히 받으려면 BotFather의 `/setprivacy`로 개인 정보 모드를 끄고 봇을 그룹에 다시 넣거나, 봇을 그룹 관리자로 만든다(관리자인 봇은 모든 메시지를 받는다). 끄면 봇이 그룹의 모든 메시지를 받지만, Junees는 위 조건(멘션, 봇 메시지에 대한 답장, 승인 질문 대기, 받아들인 앨범의 나머지)이 아닌 메시지와 허용 사용자가 아닌 사람의 메시지를 여전히 무시한다. 확인하는 법: 개인 정보 모드를 켠 채 그룹에서 첫 사진에 `@봇이름 둘을 비교해 줘` 캡션을 단 두 장짜리 앨범을 보내고, 답이 두 장을 다 봤는지 본다. 한 장만 봤다면 나머지가 오지 않은 것이다.
- 전달(forward)된 메시지는 허용 사용자가 보냈어도 다른 사람이 쓴 글이다. 앞에 `(forwarded message from <이름>; they wrote it, not the user)` 한 줄을 붙여 모델에 넘기고(이름은 `forward_origin`의 사용자 이름, 채팅/채널 제목, 숨긴 사용자 이름. 숨은 문자는 이스케이프하고 64자에서 자른다), 명령으로 읽지 않는다. 승인 질문이 기다리는 동안 전달된 글은 답으로 보지 않는다(#58): 질문은 계속 사용자의 답을 기다리고, 전달된 글은 그 턴이 끝난 뒤 다음 입력으로 돈다. 그룹에서도 질문이 기다린다는 이유만으로 전달된 글을 받지는 않는다(멘션이나 봇 메시지에 대한 답장이어야 한다).
- 포럼 슈퍼그룹의 토픽은 토픽마다 대화 하나(채널 키 `<chat id>:<topic id>`, 답도 그 토픽에). 일반 그룹의 답장 줄기는 따로 나누지 않는다.

### 명령

- 텔레그램 관례대로 `/new`, `/stop`, `/status`, `/memory`, `/skills`, `/cron`, `/webhooks`, `/help`(텔레그램이 봇을 처음 열 때 보내는 `/start`도 도움말). 그룹에서는 `/stop@봇이름`처럼 봇을 붙이거나 `@봇이름 /stop`처럼 멘션 뒤에 써도 되고(멘션을 빼고 명령을 다시 읽는다), 다른 봇 이름이 붙은 명령은 무시한다. 사진이나 파일에 붙은 캡션과 전달된 메시지는 명령이 아니다(캡션 `/new`는 사진과 함께 모델에 간다). 디스코드처럼 `!stop`도 된다. 뜻은 디스코드의 `!` 명령과 같고, 그룹에서 `/memory`는 개수만, 개인 채팅에서 내용을 보여 준다.
- Junees 터미널의 슬래시 명령(`/model`, `/sessions`, `/image` 등)과의 충돌: 게이트웨이에서는 위 명령만 명령이다. 모르는 `/단어`는 개인 채팅에서 `unknown command /model; /help lists them`으로 답하고, 그룹에서는 다른 봇의 명령일 수 있어 조용히 무시한다(봇을 부른 `/model@봇이름`이나 `@봇이름 /model`은 개인 채팅처럼 답한다). `/etc/hosts 보여 줘`처럼 `/` 뒤가 한 단어의 영문자가 아니면 명령이 아니라 보통 메시지다. 모르는 `/단어` 뒤에 다른 글이 이어지면(`/tmp is full`, `!important 이것 좀 봐`) 그 단어가 Junees 터미널 명령(`/model`, `/search` 등)이 아닌 한 보통 메시지로 모델에 간다(#58). 모르는 단어 하나뿐이거나(`/tmp`), 봇 이름을 붙였거나(`/tmp@봇이름 is full`), 터미널 명령이면(`/model opus`) 지금처럼 모르는 명령이다. 터미널 명령을 게이트웨이에서 쓰려던 사람에게 그런 명령이 없다고 알려 주기 위해서다. 그룹에서 승인 질문이 답을 기다리는 동안에도 `/단어`로 시작하는 글(`/roll 2d6`)은 다른 봇의 명령일 수 있어 답으로 보지 않고 무시한다(멘션하거나 봇 메시지에 답장하면 받는다).

### 대화와 출력

- 세션 색인은 `$JUNEES_HOME/gateway/telegram-sessions.json`(채팅 키 -> 세션 id, 0600), 세션 meta는 `"source": "telegram"`, `"channel": "<채팅 키>"`. `junees -c`와 `/sessions` 번호 목록에서 빠지는 것, 다른 Junees가 세션을 쥐고 있을 때 `(not run)`으로 미루는 것은 디스코드와 같다. 세션 검색의 출처 이름은 `telegram`.
- 답의 서식(`telegram_format`): 기본 `html`은 모델의 Markdown을 텔레그램이 받는 HTML로 바꿔 `parse_mode: "HTML"`로 보낸다(`src/telegram/format.rs`). MarkdownV2 대신 HTML을 고른 이유: Bot API 문서(https://core.telegram.org/bots/api#formatting-options, "HTML style", 2026-11-12 `curl`)에 따르면 HTML에서 이스케이프할 글자는 태그나 엔티티가 아닌 `<`, `>`, `&` 셋뿐이고, MarkdownV2는 코드 밖의 열여덟 글자(`_`, `*`, `[`, `]`, `(`, `)`, `~`, 백틱, `>`, `#`, `+`, `-`, `=`, `|`, `{`, `}`, `.`, `!`)를 모두 이스케이프해야 해서 하나만 놓쳐도 전송이 거절된다.
  - 바꾸는 것: 울타리 코드 블록(```` ```rust ````, `~~~`, 언어는 영문/숫자/`+#._-` 32자까지 → `<pre><code class="language-rust">`), 인라인 코드(`<code>`), 굵게(`**`, `__` → `<b>`), 기울임(`*`, `_` → `<i>`), 취소선(`~~` → `<s>`), 링크(`[글](주소)` → `<a href>`), 인용(`>` 줄 → `<blockquote>`). 목록, 제목(`#`), 표, 가로줄은 글 그대로.
  - 짝이 없는 표시는 글자 그대로다: 닫는 울타리 없는 코드 블록, 닫히지 않은 `**`나 백틱. 강조 표시는 CommonMark처럼 앞뒤의 공백과 문장 부호로 판정한다(left/right-flanking): 뒤가 공백이 아니어야 열리고 앞이 공백이 아니어야 닫힌다. 그래서 `*`와 `~~`는 글자에 붙어도 되어 `**설정 파일**을`, `**PR**s`가 굵게가 된다. `_`는 단어 안에서 열고 닫지 않고(`snake_case`), 경로 글자(`/ . : -` 등)에 붙은 `_`도 표시가 아니다(`src/pkg/__init__.py`, `__main__.py`). 두 숫자 사이의 `*`는 곱하기다(`2*3*4`). `* 목록`도 그대로. `\*`처럼 역슬래시를 붙인 문장 부호는 그 글자 그대로. 코드 안의 표시는 코드다.
  - 맨 주소(`http://`, `https://`로 시작해 공백까지, 끝의 `.`, `,`, `)` 같은 문장 부호는 빼고)는 쓴 그대로 둔다(GFM autolink). 주소 안의 `__init__`나 `*`가 서식으로 바뀌지 않고, 텔레그램이 주소를 그대로 링크로 만든다.
  - 링크는 http/https 주소만(공백·제어 문자 없음, 호스트 있음, `href`에 넣을 때 이스케이프한 길이로 1024자까지: `&`가 많은 주소는 `&amp;`로 다섯 배가 되므로). `javascript:`, `data:`, `tg://user?id=`(그 사용자에게 알림이 간다), `mailto:`, 상대 경로는 링크로 만들지 않고 글자 그대로 둔다. 주소의 `"`, `&`, `<`, `>`는 엔티티로 바꾼다.
  - 텔레그램의 중첩 규칙(같은 문서: 굵게·기울임·취소선은 pre와 code를 빼고 무엇이든 담을 수 있고, 그 밖의 엔티티끼리는 서로 담지 못한다)에 맞춰, 굵게 안의 인라인 코드는 굵게를 닫았다가 코드 뒤에 다시 열고, 링크 글자 안의 인라인 코드는 코드 없이 글로 둔다.
  - 4096자 나누기는 바꾼 뒤의 HTML 길이(태그와 엔티티 포함, UTF-16 단위)로 센다. 텔레그램이 세는 "엔티티 해석 뒤" 길이보다 짧을 수 없으므로 늘 한도 안이다. 줄바꿈, 없으면 공백, 그것도 없으면 글자 사이에서 자르고(엔티티 `&amp;` 같은 것은 자르지 않는다), 자르는 곳에서 열려 있던 태그는 그 메시지 끝에서 닫고 다음 메시지 앞에서 같은 태그(코드 블록이면 언어까지, 링크면 주소까지)로 다시 연다. 보이는 글자가 없는 메시지는 보내지 않는다.
  - 텔레그램이 HTML을 읽지 못해 400 `can't parse entities`로 거절하면 그 메시지만 서식 없는 글(태그를 뺀 같은 글. 링크는 `글 (주소)`로, 글이 주소와 같으면 주소만)로 다시 보내고 로그에 한 줄 남긴다. 400은 보내지지 않은 것이라 다시 보내도 두 번 가지 않는다(중복 전송 규칙은 그대로: 보내졌을 수 있는 실패는 다시 보내지 않는다).
  - 도구 줄은 코드 블록(`<pre>`)으로, 승인 질문의 내용(명령, 이유, 미리보기)도 코드 블록으로 보낸다. 명령 속 `*`나 백틱이 서식으로 바뀌지 않고 실행될 그대로 보이게 하려는 것이고, 명령 안의 ```` ``` ````는 사이에 폭 없는 공백을 넣어 블록을 닫지 못하게 한다(디스코드와 같다).
  - `plain`이면 예전처럼 `parse_mode` 없이 글 그대로 보낸다(Markdown이 글자 그대로 보이고, 4096자가 넘으면 줄 단위로 나누며 잘린 코드 블록에 울타리를 덧붙이지 않는다). 도구 줄은 코드 블록 없이 `-> bash ls` 한 줄씩, 승인 질문의 내용은 울타리(```` ``` ````) 사이에 글자 그대로 보인다.
  - 링크 미리보기는 두 방식 모두 끈다. 시스템 프롬프트에는 "텔레그램이니 짧게" 문단이 들어가고, `html`이면 어떤 Markdown이 서식으로 보이는지, `plain`이면 Markdown이 보이지 않는다는 것을 알린다.
- 그룹에 보내는 답의 `@사용자이름`은 `@` 바로 뒤에 보이지 않는 문자(U+2060 WORD JOINER)를 넣어 멘션이 되지 않게 한다(#58). 텔레그램에는 디스코드의 `allowed_mentions` 같은 끄는 방법이 없어서, 그대로 두면 모델 출력에 섞여 들어온 글(웹 페이지, 파일 내용)로도 그룹 구성원에게 알림이 간다. 글자, 숫자, `_` 바로 뒤의 `@`(메일 주소)와 영문자로 시작하지 않는 것, 그리고 웹 주소의 경로 안(`https://medium.com/@alice/글`, 텔레그램은 링크의 일부로 본다)은 그대로 둔다. `html`에서는 코드(`<code>`, `<pre>`) 안의 `@이름`도 그대로 둔다(#63): Bot API 문서의 중첩 규칙상 멘션 엔티티는 code나 pre 엔티티 안에 있을 수 없어 알림이 가지 않는다. 링크 글자 안의 `@이름`은 바꾼다. 텔레그램이 HTML을 거절해 서식 없이 다시 보내는 메시지와 `plain`에서는 백틱이 코드가 아니므로 코드였던 자리의 `@이름`도 바꾼다. 화면에는 똑같이 보이지만 복사하면 그 문자가 함께 복사된다. 개인 채팅의 답은 바꾸지 않는다(다른 사람에게 갈 알림이 없다). 그룹 여부는 채팅 id의 부호로 판단한다(텔레그램의 그룹과 슈퍼그룹 id는 음수).
- 타이핑: 턴 중 `sendChatAction typing`을 4초마다(텔레그램은 5초 동안 보여 준다). 승인 질문을 기다리는 동안은 멈춘다.
- 승인 질문은 `Approve this tool call?` + 코드 블록 안의 터미널과 같은 내용 + 답하는 법이고, 질문 메시지에 인라인 키보드 버튼(Allow, Always allow, Decline. Always allow는 매번 묻는 종류뿐이면 없다)이 달린다. 글 답장 `y`/`n`/`n <이유>`/`a`도 그대로 된다. 답한 직후(30초 안) 같은 사용자의 답 낱말 하나는 디스코드처럼 새 입력으로 넘기지 않고 이미 답했다고 알린다. 그룹에서 글로 답할 때는 질문 메시지에 답장한다(개인 정보 모드에서도 봇에 오도록). 버튼 누름은 개인 정보 모드와 상관없이 봇에 온다.
- 승인 버튼의 동작([Bot API](https://core.telegram.org/bots/api): InlineKeyboardButton, CallbackQuery, answerCallbackQuery, editMessageText): 버튼의 `callback_data`는 `junees:<질문 id>:<y|a|n>`(41바이트, 한도 64바이트)이고, 질문 id는 질문마다 새로 뽑은 128비트 난수다. `getUpdates`의 `allowed_updates`에 `callback_query`를 더해 누름을 받는다. 누름이 받아들여지는 것은 누른 사람이 `telegram_allowed_users`에 있고, 그 채팅(포럼이면 같은 토픽)에서 지금 기다리는 질문의 id이며, 그 질문 메시지에서 눌렸고, 그 질문에 있는 버튼일 때뿐이다. 모든 누름에 `answerCallbackQuery`로 답한다(답하지 않으면 텔레그램 앱이 진행 표시를 계속 보여 준다): 받아들인 누름은 알림 없이, 그 밖의 누름(허용 목록 밖, 끝났거나 시간이 지난 질문, 다른 채팅이나 토픽)은 아무것도 바꾸지 않고 `This question was already answered, timed out or stopped.`(허용 목록 밖이면 `Only the users in telegram_allowed_users can answer Junees.`) 알림으로. 질문이 끝나면(버튼, 글 답장, 시간 초과, `/stop`) `editMessageText`로 질문 메시지에 결과 한 줄(`Allowed by <이름>.` 등, HTML이면 굵게)을 더하고 빈 인라인 키보드로 버튼을 없앤다. 더한 글이 4096자를 넘으면 버튼만 없애고 결과는 따로 보낸다. 재시작 전 질문의 버튼은 남지만 눌러도 안내만 받는다.
- 이미지: 사진(`photo`, 크기 목록 중 5 MB 이하에서 가장 큰 것)과 이미지 문서(`document`, MIME `image/png|jpeg|gif|webp`, 없으면 확장자)를 그 턴이 시작할 때 `getFile`과 파일 URL로 30초 안에 5 MB까지 스트리밍으로 받는다(봇이 받을 수 있는 최대는 20 MB). 받은 바이트가 네 형식이 아니면 버린다. 앨범(같은 채팅, 보낸 사람, `media_group_id`)은 한 입력으로 묶는다(10장까지). 텔레그램은 앨범 사진을 하나씩 보내고 롱 폴링은 첫 업데이트가 오자마자 돌아오므로, 나머지는 다음 응답들로 올 수 있다. 그래서 앨범 사진은 받는 대로 모아 두었다가 1초 동안 다음 사진이 오지 않으면 한 입력으로 넘긴다(#58). 그 사이 같은 채팅에 다른 메시지가 오면 모아 둔 앨범을 먼저 넘겨 순서를 지킨다. 그보다 늦게 온 사진은 따로 입력이 되고, 그룹에서는 받아들인 앨범(최근 64개)을 기억해 그런 늦은 사진도 캡션 없이 받는다. 개인 정보 모드에서는 그룹의 캡션 없는 앨범 사진이 아예 오지 않을 수 있다(아래 개인 정보 모드). 이미지가 아닌 문서, 동영상, 음성, 스티커는 `(attachments left out: ...)`로 알린다. claude-code 백엔드는 디스코드처럼 이미지를 거절한다.

### 폴링, 오프셋, 레이트 리밋

- `getUpdates`(`timeout` 30초, `limit` 100)를 이어서 부른다. 응답을 받을 때마다 `offset`을 받은 업데이트 중 가장 큰 `update_id` + 1로 다시 계산하고(문서: "recalculate offset after each server response"), 업데이트가 확인되는 것은 다음 `getUpdates`가 그 offset으로 갈 때다. 배치는 채팅들에 넘긴 뒤에야 다음 폴링을 하므로, 넘기기 전에 죽으면 텔레그램이 같은 업데이트를 다시 준다. 최근에 넘긴 id(4096개)는 기억해서 다시 와도 두 번 돌리지 않는다(id 비교가 아니라 기억으로: 일주일 넘게 업데이트가 없으면 다음 id가 무작위라서). 끝날 때는 `timeout: 0`, `limit: 1`로 한 번 더 불러 마지막 배치를 확인하므로 다시 띄워도 같은 메시지를 다시 돌리지 않는다. 게이트웨이가 꺼져 있는 동안 온 메시지(텔레그램이 24시간 보관)는 다음에 띄울 때 처리된다.
- 폴링 오류(연결, 5xx, 시간 초과)는 1초부터 두 배씩 60초까지 기다렸다 다시 한다. 401/404(토큰 거절)는 다시 하지 않고 이유와 함께 끝난다. 409(다른 프로그램이 같은 봇의 `getUpdates`를 하거나 웹훅이 설정됨)는 한 번 들여다본 `getUpdates`처럼 금방 지나갈 수 있어 같은 방식으로 기다렸다 다시 하고, 10분 안에 여섯 번째 409가 오면(같은 토큰으로 다른 게이트웨이가 돌 때) `deleteWebhook`으로 지우라는 안내와 함께 끝난다.
- 429는 `parameters.retry_after`초만큼 기다렸다 다시 보낸다(다섯 번, 10분 넘게 기다리라면 실패). `sendMessage`에는 디스코드의 nonce 같은 중복 방지가 없으므로 보내졌을 수 있는 실패(응답 전 끊김, 시간 초과, 5xx)는 다시 보내지 않고 로그에 남긴다. 연결 자체가 안 된 경우만 다시 보낸다. 다른 메서드는 연결 오류, 시간 초과, 5xx를 세 번까지 다시 한다.

### 범위 밖, 한계

- 웹훅 방식 수신, 메시지 편집 스트리밍, MarkdownV2 서식, 밑줄·스포일러·펼치는 인용(Markdown에 표기가 없다), 음성과 문서 내용 읽기, 그룹 관리 명령, `/task`, setup 마법사의 텔레그램 질문, `junees doctor --online`의 `getMe`(토큰을 외부로 보내야 해서)는 하지 않는다.
- 여러 응답에 나뉘어 온 앨범은 따로따로 입력이 된다(그룹에서도 받기는 한다, [대화와 출력](#대화와-출력)의 이미지). 그룹이 슈퍼그룹으로 바뀌면(`migrate_to_chat_id`) 새 채팅으로 보고 새 대화를 시작한다.
- 테스트는 테스트 안에서 띄운 가짜 Bot API(루프백)로만 했다(`tests/telegram_gateway.rs`). 실제 텔레그램에는 연결해 보지 않았다.

## 슬랙 게이트웨이

`junees gateway slack`(또는 `junees slack`)은 Junees를 슬랙 앱으로 띄운다. 디스코드, 텔레그램과 같은 엔진(`src/gateway/channels.rs`)을 쓰므로 대기열과 동시 실행 상한, 세션 기록과 재시작 뒤 이어 가기, 승인 질문, 이미지 첨부, `!stop`/`!status`/`!new` 명령이 같다. 다른 점만 적는다. 이벤트는 [Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode)로 받는다: 이 컴퓨터가 슬랙으로 WebSocket을 여는 방식이라 공개 주소, TLS 인증서, 열린 포트가 필요 없다. 보내기는 [Web API](https://docs.slack.dev/reference/methods/chat.postMessage)(`chat.postMessage`)를 직접 부른다(SDK 없음, 새 의존성 없음: WebSocket은 디스코드 게이트웨이가 이미 쓰는 `tokio-tungstenite`).

### 앱 만들기

1. [api.slack.com/apps](https://api.slack.com/apps)에서 Create New App -> From a manifest를 고르고 워크스페이스를 정한 뒤 아래 manifest를 붙여 넣는다(YAML). 손으로 만들 때도 같은 항목을 켜면 된다.
   ```yaml
   display_information:
     name: Junees
   features:
     bot_user:
       display_name: Junees
       always_online: false
     app_home:
       messages_tab_enabled: true
       messages_tab_read_only_enabled: false
   oauth_config:
     scopes:
       bot:
         - app_mentions:read
         - chat:write
         - im:history
         - files:read
         - channels:history
         - groups:history
   settings:
     event_subscriptions:
       bot_events:
         - app_mention
         - message.im
         - message.channels
         - message.groups
     socket_mode_enabled: true
     org_deploy_enabled: false
     token_rotation_enabled: false
   ```
   - 필수 bot scope: `app_mentions:read`(채널 멘션 `app_mention` 이벤트), `chat:write`(`chat.postMessage`, App Home의 Messages 탭), `im:history`(DM의 `message.im` 이벤트), `files:read`(첨부 이미지 받기). 이벤트는 `app_mention`, `message.im`.
   - 선택: `channels:history` + `message.channels`(공개 채널), `groups:history` + `message.groups`(비공개 채널). 이것이 있으면 봇이 답하는 스레드 안의 답글을 멘션 없이 받는다. 없으면 스레드에서도 매번 봇을 멘션해야 한다. 봇은 들어가 있는 채널의 메시지만 받고, 그 가운데 아래 조건에 맞는 것만 처리한다.
   - `im:read`, `users:read`는 쓰지 않는다(채널 정보나 사용자 정보를 묻지 않는다). `chat:write.public`도 없다: 봇은 초대된 채널에만 쓴다.
   - App Home의 Messages Tab을 켜야(`messages_tab_enabled`, 읽기 전용 끔) 사용자가 봇에게 DM을 보낼 수 있다.
2. Install App으로 워크스페이스에 설치하고 OAuth & Permissions 페이지의 **Bot User OAuth Token**(`xoxb-...`)을 `slack_bot_token`에 둔다. scope를 바꾸면 다시 설치해야 한다.
3. Basic Information -> App-Level Tokens에서 scope `connections:write`로 토큰을 만들어(`xapp-...`) `slack_app_token`에 둔다. Socket Mode 연결 주소를 받는 `apps.connections.open`에만 쓴다.
4. 내 member id(`U...`)를 알아 둔다: 슬랙에서 내 프로필 -> 더보기(⋮) -> Copy member ID. 이름이나 이메일은 받지 않는다.
5. 채널에서 쓰려면 봇을 채널에 초대한다(`/invite @Junees`).

`junees setup`은 슬랙을 묻지 않는다. 설정 파일이나 환경 변수로 둔다(환경 변수가 우선).

```toml
slack_bot_token = "xoxb-..."           # JUNEES_SLACK_BOT_TOKEN (필수)
slack_app_token = "xapp-..."           # JUNEES_SLACK_APP_TOKEN (필수, scope connections:write)
slack_allowed_users = "U012AB3CD"      # JUNEES_SLACK_ALLOWED_USERS (필수, member id 쉼표 목록)
slack_parallel = 2                     # JUNEES_SLACK_PARALLEL (동시에 턴을 돌릴 대화 수, 기본 2)
slack_approval_timeout = "5m"          # JUNEES_SLACK_APPROVAL_TIMEOUT (승인 답 대기, 기본 5m)
```

- `slack_allowed_users`가 비어 있으면 시작하지 않는다(`no allowed Slack users ...`). 워크스페이스의 누구나 봇에게 말을 걸 수 있기 때문이다.
- 토큰: 둘 다 `Authorization: Bearer` 헤더(민감 표시)로만 보내고 URL에는 넣지 않는다(문서: "Tokens should be passed as an HTTP Authorization header"). `xoxb-`/`xapp-`로 시작하고 영문/숫자/`-`/`_`만 받는다. 두 토큰을 바꿔 넣으면 그렇다고 알려 주고, 모양이 틀려도 값을 되풀이하지 않는다. 오류는 연결 오류의 종류와 원인만(URL 없음), 슬랙이 보낸 `error` 코드는 토큰을 `[hidden]`으로 바꾼 뒤 쓴다(`Slack auth.test: invalid_auth; check the bot token (slack_bot_token) and its scopes`). `Debug`는 `Token(hidden)`. bash, MCP 서버, 작업 git의 환경에서 `JUNEES_SLACK_BOT_TOKEN`, `JUNEES_SLACK_APP_TOKEN`을 지우고(`SECRET_ENV_VARS`), `junees doctor`는 값을 `[hidden]`으로 가리며, 메모리와 스킬과 GitHub로 나가는 글은 `xoxb-`/`xapp-` 토큰을 거절한다(`src/secrets.rs`).
- `JUNEES_SLACK_API_URL`, `JUNEES_SLACK_FILES_URL`(환경 변수만)은 테스트용이다. 기본(`https://slack.com/api`, 파일은 `https://files.slack.com`) 말고는 이 컴퓨터 주소(루프백, `localhost`)만 받는다. API 주소를 바꾸면 Socket Mode 주소도 루프백을 받는다. 기본일 때 WebSocket 주소는 `wss://` + `slack.com`이나 그 하위 호스트만 연다.

```sh
cd ~/work/some-project      # 도구가 도는 폴더 (세션 기록의 cwd)
junees gateway slack
```

시작하면 `auth.test`로 봇 사용자 id(멘션 판단)와 봇 id(자기 메시지 거르기)를 받고 `slack gateway: junees (U...) in <워크스페이스>; ...`를 표준 오류에 찍는다. 진행 줄에는 메시지 내용과 토큰을 찍지 않는다. SIGINT/SIGTERM이면 턴을 취소해 기록한 뒤 끝난다. systemd 예시는 디스코드 절의 `ExecStart`를 `junees gateway slack`으로 바꾸면 된다. 한 `$JUNEES_HOME`에는 슬랙 게이트웨이도 하나만 돈다(`$JUNEES_HOME/gateway/slack.lock`). 다른 게이트웨이와는 잠금이 달라 함께 돌릴 수 있다.

### 누구의 어떤 메시지에 답하나

- `slack_allowed_users`의 사용자만. 다른 사용자의 메시지는 무시하고 로그에 member id를 한 번만 남긴다(첨부도 받지 않는다).
- DM(`message`, `channel_type: im`)은 늘 답한다. DM 채널 하나가 대화 하나(채널 키 `D...`), 답은 DM에 바로(스레드 없이).
- 채널(공개, 비공개, 여러 명 DM)에서는 봇을 멘션한 메시지(`app_mention`, 또는 글에 `<@봇 id>`가 든 `message`)에 답하고, 그 뒤로는 그 스레드의 답글에도 멘션 없이 답한다(`message.channels`/`message.groups` 이벤트를 받을 때). 스레드마다 대화 하나(채널 키 `<채널 id>:<thread_ts>`)이고 답은 그 스레드에 단다. 채널 본문의 멘션이면 그 메시지가 스레드의 시작이 된다. 다른 사람의 스레드에서 멘션해도 그 스레드에 붙는다. 재시작 뒤에도 세션 색인에 있는 스레드는 봇의 스레드로 본다. 그 스레드에서 승인 질문이 답을 기다리면 다음 답글이 답이다.
- 허용 목록은 누가 봇을 움직이는지만 정한다. 채널 스레드의 답과 도구 줄(실행한 명령, 파일 내용 일부)은 그 채널에 있는 모든 사람이 본다. 허용 목록에 없는 멤버도, Slack Connect로 공유한 채널이면 다른 회사 사람도 포함이다. 비밀(토큰, 설정 파일, 개인 정보)이 나올 수 있는 작업은 봇과의 DM에서 한다. `!memory`의 내용을 DM에서만 보여 주는 것도 같은 이유다.
- `subtype`이 있는 메시지(편집 `message_changed`, 삭제, 입장, 다른 봇의 `bot_message` 등)는 사용자가 쓴 글이 아니므로 무시한다. 파일이 붙은 `file_share`와 스레드 답글을 채널에도 보낸 `thread_broadcast`만 받는다. `bot_id`가 있거나 보낸 사람이 봇 자신인 메시지도 무시한다.
- 중복: 슬랙은 확인(ack)을 제때 못 받은 이벤트를 다시 보내고(같은 `event_id`, 문서: 세 번까지), 봇이 들어간 채널의 멘션은 `message`와 `app_mention` 두 이벤트로 온다. 최근 `event_id`와 메시지(채널 + `ts`) 4096개씩을 기억해 한 번만 처리한다.
- 들어온 글의 슬랙 표기는 평문으로 바꿔 모델에 넘긴다: 봇 멘션은 빼고, `<https://x|글>` -> `글 (https://x)`, `<@U123>` -> `@U123`, `<#C123|general>` -> `#general`, `<!here>` -> `@here`, `&lt;` `&gt;` `&amp;` -> `<` `>` `&`.

### 명령

- 텍스트 명령 `!new`, `!stop`, `!status`, `!memory`, `!skills`, `!cron`, `!webhooks`, `!help`(뜻은 디스코드와 같다). 채널에서는 `@Junees !stop`처럼 멘션 뒤에 쓰거나 봇의 스레드에서 그냥 쓴다. `!memory`는 채널에서 개수만, DM에서 내용을 보여 준다. 파일이 붙은 메시지의 글은 명령이 아니다.
- 슬랙 슬래시 명령(`/junees ...`)은 앱 설정에 명령을 등록해야 해서 쓰지 않는다. `/`로 시작하는 글은 슬랙이 먼저 가로채므로 `!`를 쓴다.

### 대화와 출력

- 스레드마다 대화 하나라서 대화(워커와 열린 기록)가 계속 생긴다. 30분 동안 쓰지 않은 대화는 닫는다: 턴이 돌거나 차례를 기다리거나 승인 답을 기다리거나 대기열이 있으면 닫지 않는다. 세션은 저장되어 있으므로 그 스레드나 DM의 다음 메시지가 새 워커에서 세션을 다시 열어 이어 간다(`$JUNEES_HOME`도 `HOME`도 없어 저장하지 않는 경우에는 새 대화로 시작한다). 닫을 때 로그에 `slack: closed N idle conversation(s), M open`을 남긴다. 멘션 없이 답할 봇 스레드 목록은 최근에 쓴 4096개까지 기억하고, 그보다 오래된 스레드는 세션 색인에 있으면 여전히 봇의 스레드로 본다.
- 세션 색인은 `$JUNEES_HOME/gateway/slack-sessions.json`(대화 키 -> 세션 id, 0600), 세션 meta는 `"source": "slack"`, `"channel": "<대화 키>"`. `junees -c`와 `/sessions` 번호 목록에서 빠지는 것, 다른 Junees가 세션을 쥐고 있을 때 `(not run)`으로 미루는 것은 디스코드와 같다. 세션 검색과 `junees usage --by source`의 출처 이름은 `slack`.
- 서식: 모델의 Markdown을 슬랙 mrkdwn으로 바꿔 `chat.postMessage`의 `text`로 보낸다(`src/slack/format.rs`). `**굵게**` -> `*굵게*`, `*기울임*` -> `_기울임_`, `~~취소~~` -> `~취소~`, `[글](https://...)` -> `<https://...|글>`(http, https, mailto만. `javascript:` 같은 주소는 `글 (주소)` 글자로), `# 제목` -> 굵은 줄, `- 항목` -> `• 항목`, 인용 `>`는 그대로, 코드 블록은 언어 이름을 빼고 그대로(안은 바꾸지 않음), 인라인 코드는 백틱 하나로. 울타리(```` ``` ````) 뒤가 비었거나 백틱 없는 한 단어(언어 이름)일 때만 코드 블록을 연다: 한 줄 안에서 닫히는 ```` ```ls -la``` ````는 인라인 코드이고, 울타리 뒤에 여러 단어가 오면 블록이 아니다. 링크 주소 안의 괄호는 짝을 세어 주소에 넣는다(`https://en.wikipedia.org/wiki/Rust_(programming_language)`). 짝이 없는 표시는 글자 그대로 두고, `__x__`는 파이썬 `__init__` 같은 이름이 많아 바꾸지 않는다. 그 밖의 모든 글은 `&`, `<`, `>`를 `&amp;`, `&lt;`, `&gt;`로 이스케이프한다(문서 "Escaping text"). 이 이스케이프 때문에 모델 출력의 `<!channel>`, `<@U...>`, `<!subteam^...>` 같은 멘션은 알림을 보내지 못하고 글자로 보인다. `@here` 같은 평문은 `link_names`를 보내지 않으므로 멘션이 되지 않는다(문서: "as long as you exclude the `link_names` argument ... this will be disabled by default"). 링크와 미디어 미리보기(`unfurl_links`, `unfurl_media`)는 끈다. 시스템 프롬프트에는 "슬랙 메시지로 가니 짧게, 표는 서식이 안 된다" 문단이 들어간다.
- 길이: `chat.postMessage` 문서는 `text`를 4,000자 안으로 권하고 40,000자부터 자른다. 답을 3800 UTF-16 단위로 줄 단위로 나누고(디스코드처럼 잘린 코드 블록은 닫고 다음 메시지에서 다시 연다), 이스케이프로 늘어나는 몇 글자는 그 여유 안에 든다.
- 도구 줄은 코드 블록 하나로 모아 보낸다(디스코드와 같다).
- 타이핑 표시는 없다: 봇의 타이핑 표시는 은퇴한 RTM API에만 있었다.
- 승인 질문은 굵은 제목 + 코드 블록(터미널과 같은 내용) + 답하는 법이고, `y`/`n`/`n <이유>`/`a`로 답한다. 채널에서는 그 스레드에 답글로 답한다(버튼, Block Kit 상호작용은 쓰지 않는다).
- 이미지: 메시지의 `files` 가운데 MIME `image/png|jpeg|gif|webp`이고 5 MB 이하인 것(10개까지)을 그 턴이 시작할 때 `url_private_download`(없으면 `url_private`)에서 봇 토큰으로 30초 안에 스트리밍으로 받는다. 문서 "Downloading files"대로 토큰은 `https://files.slack.com`(파싱한 호스트가 정확히 같고 기본 포트, 사용자 정보 없음)에만 보내고, 리다이렉트는 자동으로 따르지 않고 세 번까지 같은 규칙으로 확인한 뒤 따른다. 원격 파일(`mode: external`, `is_external`, `filetype: remote`: 주소를 다른 앱이 정한다), 봇에게 열리지 않은 파일(`file_access`), 이미지가 아닌 파일, 큰 파일은 `(attachments left out: ...)`로 알린다. 받은 바이트가 네 형식이 아니면 버린다(토큰이 틀리면 슬랙은 로그인 HTML을 준다). claude-code 백엔드는 디스코드처럼 이미지를 거절한다.

### 연결, 재연결, 레이트 리밋

- `apps.connections.open`(앱 토큰)으로 받은 주소에 연결하면 슬랙이 `hello`를 보낸다. 그 뒤 오는 봉투(envelope)마다 처리하기 전에 바로 `{"envelope_id": ...}`로 확인한다(문서: "Your app still needs to acknowledge receiving each event so that Slack knows whether to retry"). `events_api` 봉투만 처리하고 슬래시 명령과 상호작용 봉투는 확인만 한다.
- 슬랙은 몇 시간마다 연결을 바꾼다: `disconnect`(`warning`, `refresh_requested`)가 오면 닫고 바로 새 주소를 받아 다시 연결한다(`link_disabled`, 즉 Socket Mode를 끈 경우는 기다렸다가). 연결이 끊기거나 닫히면, 그리고 30초마다 보내는 ping에도 90초 동안 아무것도 오지 않으면(연결 직후 `hello`는 30초) 다시 연결한다. 기준은 마지막으로 받은 프레임(pong 포함)의 시각이라, 보낸 ping은 이 시간을 늘리지 않는다. 실패하면 1초부터 두 배씩 60초까지 기다린다(`hello`를 받으면 초기화). `invalid_auth`, `not_authed`, `token_revoked`, `missing_scope` 같은 토큰 거절은 다시 하지 않고 이유와 함께 끝난다.
- 429는 `Retry-After` 헤더의 초만큼 기다렸다 다시 보낸다(문서 "Rate limits", 다섯 번까지, 10분 넘게 기다리라면 실패). `chat.postMessage`에는 중복 방지 키가 없으므로 보내졌을 수 있는 실패(응답 전 끊김, 시간 초과, 5xx)는 다시 보내지 않고 로그에 남긴다. 연결 자체가 안 된 경우와 429만 다시 보낸다. 다른 메서드는 연결 오류, 시간 초과, 5xx를 세 번까지 다시 한다.

### 범위 밖, 한계

- 슬래시 명령, 버튼과 Block Kit 승인, 메시지 편집 스트리밍, 반응 이모지, 타이핑 표시, Agent/Assistant 스레드(`assistant.threads.*`), 이미지가 아닌 파일 읽기, 여러 워크스페이스 설치(OAuth 배포), HTTP Events API(요청 URL) 방식, `!task`, setup 마법사의 슬랙 질문은 하지 않는다.
- `junees doctor`는 토큰 모양과 허용 사용자를 오프라인으로 보고, `--online`이면 `auth.test`로 봇 토큰만 확인한다(앱 토큰은 `apps.connections.open`이 연결 주소를 새로 만들어서 묻지 않는다).
- 확인(ack)한 뒤 처리 전에 프로세스가 죽은 이벤트는 다시 오지 않는다. 게이트웨이가 꺼진 동안의 메시지는 받지 못한다(Socket Mode는 연결된 동안만 보낸다).
- 테스트는 테스트 안에서 띄운 가짜 Web API와 가짜 Socket Mode WebSocket 서버(루프백)로만 했다(`tests/slack_gateway.rs`). 실제 슬랙에는 연결해 보지 않았다.
