# 구조

```
src/
  main.rs            CLI 진입점 (처음 실행 설정과 junees setup, 대화 모드와 슬래시 명령(api 에이전트와 Claude Code 대화를 Engine으로 전환), -p 실행, task 명령(계획 + 실행), 세션 열기/이어 가기(백엔드별), SIGINT/SIGTERM 처리)
  agent.rs           에이전트 루프 (기록 변경을 세션 기록에 남김, 도구 결과 비우기/압축/초과 시 재시도, 취소, 모델 호출 전 한도 확인, 도구 호출 전 승인 훅 ToolGate, 턴이 끝난 뒤 훅 AfterTurn, 스트리밍 이벤트 전달, pause_turn 이어 가기와 인용 출처 보고, delegate 호출 실행(서브에이전트와 provider, 도구, 취소, 승인, 한도 공유))
  approval.rs        실행 전 승인 (JUNEES_APPROVAL, 묻는 범위 판단(MCP 외부 도구, GitHub 쓰기 포함, merge는 매번, 도구가 스스로 정하는 질문 ToolQuestions(브라우저)), 터미널 질문, 세션 동안 항상 허용, 프롬프트와 공유하는 표준 입력, 서브에이전트 질문 표시(check_for), 묻지 않는 호출은 기다리지 않고 질문만 하나씩)
  cancel.rs          취소 플래그 (시그널 처리기와 에이전트 루프가 공유)
  session.rs         세션 기록(JSONL) 쓰기, 재생, 이어 가기, 세션 찾기/목록(백엔드별, 디스코드 채널 세션과 예약 작업 세션은 -c와 번호 목록에서 뺌), 비우기/압축 지점 계산, Claude Code 턴 기록, 서브에이전트 기록(create_sub)과 사용량
  context.rs         압축 요약 요청(시스템 프롬프트, 앞부분을 텍스트로 옮기기)
  claude_code/       claude-code 백엔드 공용 부분 (대화와 작업이 함께 씀)
    mod.rs             백엔드 설정, claude --version/auth status 확인, 환경 정리와 CLAUDE_CODE_SUBPROCESS_ENV_SCRUB, 세션 실행(Spawn)과 예산/취소, 프로세스 그룹과 자손 종료
    stream.rs          stream-json 읽기 (텍스트 조각 스트리밍, 도구 호출/결과, 거절된 호출, 응답별 사용량, 로그인 실패와 사용 한도 판단)
    rules.rs           명령 정책과 설정 파일 보호를 Claude Code 권한 규칙(--disallowedTools)으로
    chat.rs            구독으로 대화하기 (턴마다 claude -p, --resume으로 이어 가기, 턴 기록, 메모리와 스킬 절을 한 --append-system-prompt로)
  task/              장기 작업 모드
    runner.rs          task init/run 세션 루프, 체크포인트 커밋, 정지 조건, 크래시 복구, 실행 로그, Claude Code 세션 뒤 하네스 verify
    claude_code.rs     claude-code 백엔드의 작업 세션 인자(acceptEdits, 시스템 프롬프트)와 실행
    claude_tests.rs    가짜 claude로 claude-code 백엔드의 init/run 검증(단위 테스트)
    budget.rs          토큰/시간 예산, 기간 파싱(90s, 30m, 2h)
    lock.rs            동시 실행 방지(저장소 루트 flock)
    features.rs        features.json 검증, 하네스의 권위 있는 사본(Ledger)
    tools.rs           mark_feature_passing (verify 명령을 하네스가 실행)
    state.rs           state.json (세션 수, 무진전 횟수, 토큰, 정지 이유와 시각, 목표, 실행 중 표시, 기능 목록 사본)
    status.rs          task status (읽기 전용, 텍스트/JSON)
    prompt.rs          초기화/작업 세션 프롬프트
    git.rs             저장소 확인, 커밋, 로그
  message.rs         대화 메시지 타입 (Messages API 형식, 이미지 블록과 이미지가 든 도구 결과, 모르는 블록은 원본 JSON 그대로, 텍스트의 citations 보존, 서버 도구 사용량)
  images/            이미지 입력
    mod.rs             형식 판별(매직 바이트), 헤더에서 크기 읽기, 상한, base64, 토큰 추정, /image와 --image 파일 읽기
    store.rs           세션 기록의 이미지를 세션 폴더 파일로 저장하고 이어 갈 때 다시 읽기(sha256 확인)
    discord.rs         디스코드 첨부 중 이미지 고르기, CDN에서만 내려받기(리다이렉트 제한, 시간, 크기)
  web_search.rs      웹 검색 서버 도구 설정과 도구 정의, 인용 출처 목록, 압축용 검색 블록 요약
  notify.rs          디스코드 웹훅 알림 (정지, 세션마다)
  provider/          LLM 제공자 트레이트와 Anthropic 클라이언트 (프롬프트 캐싱 표시, 확장 사고 설정, 스트리밍 재시도와 대기 제한), 설정의 provider로 클라이언트 고르기(from_config), 공용 재시도(http.rs)
    openai/          OpenAI Chat Completions 호환 클라이언트 (대화 변환: tool_calls/tool 메시지/image_url, 응답과 사용량, 오류 매핑, 스트림 조각 합치기)
    stream.rs          SSE 이벤트 분리와 응답 조립 (스트리밍하지 않은 응답과 같은 값)
  sandbox/           bash 명령의 Landlock 실행 격리
    mod.rs             설정(sandbox, sandbox_write/read/network, 무인 실행은 require), 커널 지원(ABI) 확인, 명령마다 규칙 만들기와 auto/require 판단, 자식에 걸기, doctor용 시험 자식
    landlock.rs        Landlock 시스템 호출(libc 직접), 접근 권리와 ABI별 처리, 보호 경로를 뺀 하위 항목별 규칙(grant_except), no_new_privs + restrict_self
    tests.rs           실제 커널 Landlock으로 쓰기/읽기/비밀/네트워크 제한과 가짜 ABI의 auto/require
  tools/             도구 트레이트, 레지스트리, bash/read_file/write_file/edit_file, bash 명령 정책과 주의 등급, 작업 폴더 판단(policy.rs), 도구와 정책이 설정 파일(config.toml, mcp.json), 시크릿 폴더(secrets)와 쓰기 금지 데이터 폴더(memory, projects, skills, cron, plugins, webhooks, flows)를 알아보는 판단(settings_file.rs), 명령 안의 Junees 상태 변경 하위 명령 표와 판단(self_command.rs), 명령 안의 `gh` GitHub 쓰기 표와 판단(gh_command.rs), 웹훅 실행의 지침 파일 이름 쓰기 거절(instruction_files.rs), 도구 호출 전 훅 PreTool, 테스트 전용: 테스트 bash의 sandbox(bash.rs의 `testing::confine`, 환경 변수 수준이라 절대 경로와 프록시를 무시하는 프로그램은 막지 못함)와 테스트 코드가 홈 삭제나 origin push를 실제 셸로 돌리지 않는지, sandbox 밖(바이너리 테스트, 모델 응답으로 부르는 bash)에서는 원격과 상관없는 모든 `git push`, `.`/`..`/`/`/`*`의 재귀 삭제, `git reset --hard`, `git clean -f`도 돌리지 않는지, `Command::new("git")` 인자에 `push`가 있으면 임시 폴더의 bare 저장소로만 미는 헬퍼(SAFE_PUSH)를 거치는지, sandbox 밖 실제 bash 테스트가 허용 목록(파일과 모듈 경로까지 맞춘 테스트, 무해한 명령 또는 명령 없음)만 쓰는지 보는 소스 스캔(test_scan.rs)
  transcribe/        음성 메시지 받아쓰기: 설정(주소 검사, 주소와 한 쌍인 키, 모델, 언어, 크기 한도), OpenAI 호환 /audio/transcriptions 클라이언트(직접 만든 multipart, 리다이렉트 안 따름, 429/5xx 재시도, 401/403 안내), 모델에게 갈 글. tests.rs: 루프백 가짜 API
  mcp/               MCP 클라이언트 (stdio, Streamable HTTP)와 서버 (junees mcp serve)
    serve/             Junees를 MCP 서버로: mod.rs는 stdio JSON-RPC 줄 읽기(4 MiB 상한)와 한 곳에서만 쓰는 stdout, initialize 버전 협상, ping/tools/list/tools/call, 취소, 호출 하나씩 순서대로. tools.rs는 junees_run(Trigger::Mcp 무인 실행, 세션 새로/이어 가기, 진행 알림, 사용량과 비용, 비밀 값 가림), junees_sessions, junees_session(--dir의 세션만, 대시보드 session_view 재사용)
    config.rs          $JUNEES_HOME/mcp.json 읽기와 검사 (0600, 자기 소유, 값을 되풀이하지 않는 오류, command와 url 중 하나, 숨길 헤더 변수 목록)
    conn.rs            서버 프로세스(프로세스 그룹, 환경 정리)와 JSON-RPC 2.0 줄 메시지, 요청 id와 답 대기, 취소 알림, 서버 요청(ping) 답, stderr 꼬리, 종료(입력 닫기 -> SIGTERM -> SIGKILL)
    server.rs          server/discover 탐색과 initialize 대체(dual-era), tools/list 페이지, tools/call과 시간 한도, 상태와 다시 띄우기
    tool.rs            mcp__<서버>__<도구> 이름 정리와 충돌, 설명/스키마 이스케이프, 결과를 ToolOutput으로
    link.rs            Link: 실행 중인 서버에 닿는 전송(stdio Connection 또는 HttpConnection)
    http/              Streamable HTTP 전송
      mod.rs             HttpConnection: 요청마다 POST, JSON/SSE 응답, 두 세대 핸드셰이크(server/discover, 4xx면 initialize), 세션 id와 404 뒤 새 세션, 취소(스트림 닫기 / notifications/cancelled), 응답 크기 상한, 리다이렉트 거절, URL과 헤더 값 없는 오류, 종료 DELETE
      target.rs          url(https, http는 루프백만, 사용자 정보 금지)과 headers(env:NAME 참조만, 전송 헤더 금지), 값 읽기(민감 표시), origin 표시
      meta.rs            Mcp-Method/Mcp-Name, x-mcp-header 검사와 Mcp-Param-* 값, Base64 표시 인코딩
      sse.rs             응답 스트림의 SSE 이벤트 읽기(조각 경계, CRLF, 주석, message 이벤트만)
      tests.rs           루프백 가짜 HTTP MCP 서버(두 세대, JSON/SSE)로 핸드셰이크, 헤더, 세션, 404, 시간 초과와 취소, 크기 상한, 리다이렉트, 비노출 검증
    oauth/             url 서버의 OAuth 인가 (MCP 2026-07-28 Authorization)
      mod.rs             흐름 요약, 로그인 상태(/mcp와 doctor용, 토큰 없이)
      config.rs          mcp.json `oauth` (client_id, client_secret env 참조, scope, issuer)
      challenge.rs       WWW-Authenticate 파싱 (Bearer의 resource_metadata, scope, error)
      discovery.rs       토큰 없는 probe, 보호 자원 메타데이터(RFC 9728)와 resource 확인, 인가 서버 메타데이터(RFC 8414/OIDC, 발급자 일치, S256)
      fetch.rs           메타데이터/등록/토큰 요청 (https, 루프백 서버에만 루프백 http, 리다이렉트 거절, 1 MiB, URL 없는 오류, OAuth 오류 문구)
      pkce.rs            PKCE S256과 무작위 state
      token.rs           동적 클라이언트 등록(RFC 7591), 코드 교환과 갱신(resource 포함), 클라이언트 인증(none/basic/post), 토큰 응답 검사
      redirect.rs        127.0.0.1 한 번 수신기(state 검사), 붙여 넣은 주소 읽기
      login.rs           junees mcp login 흐름 (scope 선택과 step-up, iss 확인, 저장), 로그아웃
      store.rs           secrets/mcp-oauth/<서버>.json (0600/0700, 원자적 쓰기, 갱신/로그인/로그아웃 잠금)
      auth.rs            저장된 로그인 사용: Bearer 헤더, 만료 전과 401 뒤 한 번 갱신(로그아웃/다른 주소의 새 로그인이면 갱신 안 함), 다른 주소면 쓰지 않음, 403 scope 기억
      cli.rs             junees mcp login/logout 인자와 터미널 입출력
      tests.rs           루프백 가짜 인가 서버+MCP 서버로 전체 로그인(수신기, 붙여 넣기), PKCE, state/iss 거절, 갱신, 갱신과 로그아웃/재로그인 겹침, 잠금 대기, step-up, 무인 실행, 주소 바뀜, 헤더 우선, 비노출
    mod.rs             Mcp: 서버 묶음 시작/보고/종료, 도구 등록, 승인용 외부 도구 목록, /mcp 보기(전송 종류, 로그인 상태), 로그인 뒤 다시 띄우기
  plugins/           플러그인 ($JUNEES_HOME/plugins/<이름>/plugin.json)
    manifest.rs        plugin.json 읽기와 검사 (도구, 훅, 프롬프트, 명령 경로, 시간 한도, 0600/자기 소유, 값을 되풀이하지 않는 오류)
    process.rs         명령 실행 (플러그인 폴더, 프로세스 그룹, 환경 정리, 표준 입력 JSON, 시간 한도와 그룹 종료, 출력 머리와 꼬리)
    tool.rs            명령 도구를 Tool로 (설명/스키마 이스케이프, 결과와 오류)
    hooks.rs           pre_tool(종료 코드 2면 거절)과 post_turn 훅, 실패 경고
    state.rs           켜짐/trust 상태 파일 plugins/state.json (0600, 원자적 쓰기)
    manage.rs          junees plugin list/install/enable/disable/remove (복사, 링크 거절, 크기 제한)
    mod.rs             Plugins: 켜진 플러그인 읽기, 도구와 훅 등록, 승인용 외부 도구 목록, 프롬프트 절(상한), 시작 보고와 /plugins
    tests.rs           실제 플러그인 폴더와 sh/python3 명령으로 도구 실행/시간 초과/오류, 이름 정리, 승인 질문, 훅 거절/경고, post_turn, 프롬프트 상한, 켜짐 상태, 설치, 서브에이전트 검증
  config.rs          설정 값 읽기와 검사 (from_lookup 이음새, 도구 설정 ToolSettings 포함)
  config_file.rs     설정 파일 config.toml (단순 TOML 읽기/쓰기, 0600), Settings: 실행 중 변경 -> 환경 변수 -> 파일 순서로 찾기
  setup.rs           설정 마법사 (Prompter 트레이트, Claude Code 확인 ClaudeProbe, 키 입력 때 터미널 echo 끄기 EchoOff, 선택 단계 디스코드와 슬랙(토큰은 숨겨 입력))
  slash.rs           대화 안 슬래시 명령 파싱, /sessions 번호 고르기
  usage/             사용량과 비용 (/usage, junees usage)
    mod.rs             기록 종류(Kind)와 사용량 기록(Record), 합계와 비용 표시(Totals)
    prices.rs          모델 가격표(출처와 확인 날짜), prices 설정 덮어쓰기, 비용 계산(캐시 쓰기/읽기, 긴 프롬프트, 웹 검색)
    scan.rs            기록 파일에서 사용량 줄만 읽기(서브에이전트 기록은 부모 subagent_usage로), 기간과 폴더 범위
    report.rs          /usage의 세션 표(기록에 없는 사용량 포함)와 junees usage의 기간 표(날짜/모델/폴더/출처별), 안내 줄
    args.rs            junees usage 옵션과 도움말
    tests.rs           가격 계산, 덮어쓰기, 모르는 모델, 기록 종류별 합산, 깨진 줄, 날짜 경계(UTC와 시간대), 범위와 정렬 검증
  search/            세션 검색 (session_search 도구, /search, junees sessions search)
    mod.rs             검색 옵션과 결과, 상한(결과 수, 스니펫, 줄과 텍스트 크기, 시간), 출처 이름
    query.rs           질의 파싱(낱말 AND, 따옴표 구절), 대소문자 무시 매칭, 원래 글자로 스니펫
    scan.rs            기록 파일 목록(서브에이전트 포함, 새것부터), 큰 줄을 담지 않는 줄 읽기, 범위 판단, 항목에서 찾을 글 고르기
    format.rs          모델용 결과(무작위 태그, 데이터 안내)와 사람용 번호 목록, 읽지 못한 것 안내
    tool.rs            session_search 도구(입력 검사, 채널 세션 허용 여부)와 등록
    args.rs            /search와 junees sessions search의 옵션
    tests.rs           실제 기록 파일로 매칭, 정렬과 상한, 폴더/출처 범위, 깨진 줄과 큰 줄, 시간 상한, 도구 출력 검증
  skills/            스킬 ($JUNEES_HOME/skills/<이름>/SKILL.md)
    mod.rs             이름과 앞머리 검사, 목록(Index)과 상한, 모드별 시스템 프롬프트 절
    store.rs           읽기/만들기/패치/파일 쓰기와 지우기/휴지통, 경로 탈출과 심볼릭 링크 거절, 크기 제한, 원자적 쓰기(0600/0700)
    tools.rs           skill_view, skill_manage 도구와 등록
  subagent/          서브에이전트 (delegate 도구)
    mod.rs             설정(JUNEES_SUBAGENTS 등), 도구 정의와 입력 검사, 시스템 프롬프트 안내, 서브에이전트 이름을 붙여 부모 게이트로 묻는 ChildGate
    run.rs             과제 실행(동시 실행 상한, 과제 순서 보존), 자기 한도와 부모 예산, 진행 한 줄, 서브에이전트 기록, 결과 크기 제한
    tests.rs           Scripted provider로 병렬 상한과 순서, 재귀 금지, 예산 차감, 취소, 자식 기록, 승인 공유(한 자식이 답을 기다리는 동안 다른 자식의 묻지 않는 호출은 실행), 결과 크기 검증
  prompt.rs          시스템 프롬프트, 메모리와 스킬 절(data_sections), Claude Code에 넘기는 인자(claude_append, 지침 파일을 더한 claude_append_with_context, 128 KiB 한도)
  secrets.rs         비밀처럼 보이는 내용 찾기, 메모리와 스킬이 함께 씀 (키 접두사, 개인 키, AWS 키 id, 디스코드/텔레그램 봇 토큰, 긴 hex/토큰, 몇째 줄인지)
  prompt_text.rs     저장된 텍스트(메모리, 스킬)의 제어/방향 문자 판단: 쓸 때 거절(check_plain), 읽을 때 이스케이프(escape_hidden)
  context_files/     프로젝트 지침 파일 (AGENTS.md/JUNEES.md/CLAUDE.md를 저장소 맨 위나 HOME까지 찾기, 파일당/전체 상한, 링크 판단, 시스템 프롬프트 절, /context 목록, Claude Code의 AGENTS.md 플러그인 끄는 --settings, 웹훅 실행용 마지막 커밋 읽기와 커밋되지 않은 변경 비교(committed.rs))
  memory/            영구 메모리
    mod.rs             저장소 셋(user, notes, 프로젝트별 notes) 읽기/쓰기(0600, 원자적, flock), 항목 추가/교체/삭제와 상한, 프롬프트 절(대화, task, Claude Code 읽기 전용), /memory 보기
    tool.rs            memory 도구 (task 세션은 project만)
  cli.rs             인자 파싱(위치 인자 질문, task/status/setup/gateway/cron/webhook/doctor 하위 명령, 예전 플래그), 도움말, 출력 보조 함수
  gateway/           디스코드 게이트웨이 (junees gateway)와 게이트웨이 공통 엔진
    mod.rs             Gateway::run: Get Gateway Bot, 연결 태스크, 채널 LocalSet, 종료 처리, 로그(Log), 게이트웨이 잠금(lock_file)
    platform.rs        Platform 트레이트: 엔진이 플랫폼에 바라는 것(보내기, 타이핑, 이미지 받기, 오디오 받기(fetch_audio), 첨부 종류(Kind: Image/Audio), 명령/도구 줄/승인 질문 표기)
    channels.rs        공통 엔진: 채널별 작업자(대기열, 동시 실행 상한, 타이핑, 도구 줄 묶기, 취소), 명령 실행(new/stop/status/memory/...), 첨부 받기, 음성 받아쓰기(꺼져 있거나 실패하면 모델을 부르지 않음), 세션 열기
    config.rs          discord_* 설정 (토큰 Token은 Debug에서 숨김, 허용 사용자 필수, https/wss 확인)
    shard.rs           Gateway v10 WebSocket: Hello/Identify/Ready, 하트비트(지터, ACK 누락), Resume, Reconnect, Invalid Session, 닫힘 코드, 지수 대기
    rest.rs            REST: Get Gateway Bot, Create Message(allowed_mentions 없음), 타이핑, 경로별 레이트 리밋과 429 재시도
    bot.rs             디스코드 쪽: 메시지 고르기(허용 사용자, DM/멘션/지정 채널/스레드), ! 명령 읽기, Platform 구현(Discord)
    engine.rs          Conversation/Engines 트레이트, api 에이전트와 Claude Code 대화, 이벤트를 텍스트 블록과 도구 한 줄로(Collector)
    ask.rs             채널 승인 질문(다음 메시지로 답, 시간 초과 거절)
    sessions.rs        채널 -> 세션 색인(discord-sessions.json, 다른 게이트웨이는 자기 파일과 source), 세션 이어 가기/새로 만들기
    split.rs           2000자 나누기(줄 단위, 코드 블록 닫고 다시 열기, UTF-16 길이), 평문용 split_plain(울타리 없이)
  telegram/          텔레그램 게이트웨이 (junees gateway telegram)
    mod.rs             TelegramGateway::run: getMe, 롱 폴링 Poller(offset 다시 계산, 최근 id로 중복 거름, 409 몇 번 기다리기, 끝날 때 확인), 채팅 LocalSet, 종료 처리
    config.rs          telegram_* 설정 (토큰 모양 검사와 Debug 숨김, 숫자 사용자 id 필수, API 주소는 api.telegram.org 또는 루프백)
    api.rs             Bot API: getMe/getUpdates/sendMessage/sendChatAction/getFile/파일 받기, URL과 토큰 없는 오류, 429 retry_after, 중복 전송 없는 재시도, 응답과 파일 크기 상한
    bot.rs             메시지 고르기(허용 사용자, 개인/그룹 멘션/답장/승인 답, 포럼 토픽), /명령 읽기(@봇, 멘션 뒤 명령, 다른 봇, /start, 캡션과 전달은 명령 아님), 전달 표시, 앨범 묶기와 나뉘어 온 그룹 앨범
    platform.rs        Platform 구현(Telegram): 평문 4096자, 타이핑, 사진/이미지 문서와 voice/audio/오디오 문서 고르기와 받기, 평문 승인 질문, 도움말
  slack/             슬랙 게이트웨이 (junees gateway slack)
    mod.rs             SlackGateway::run: auth.test, Socket Mode 연결과 라우터를 LocalSet에서, 종료 처리
    config.rs          slack_* 설정 (xoxb-/xapp- 토큰 모양 검사와 Debug 숨김, member id 필수, API/파일 주소는 슬랙 또는 루프백)
    api.rs             Web API: auth.test/apps.connections.open/chat.postMessage(blocks와 ts)/chat.update/chat.postEphemeral, Bearer 헤더(민감), URL과 토큰 없는 오류, 429 Retry-After, 중복 전송 없는 재시도, files.slack.com에서만 토큰으로 받기(리다이렉트 직접 확인)
    socket.rs          Socket Mode: 연결 주소 받기와 wss://*.slack.com 확인, hello, 봉투마다 즉시 ack(events_api와 interactive를 라우터로), disconnect/끊김/무응답이면 다시 연결, ping, 지수 대기
    bot.rs             이벤트 고르기(허용 사용자, DM/채널 멘션/봇 스레드, subtype과 봇 메시지 거르기, event_id와 메시지 중복 제거), ! 명령, 승인 버튼 누름(block_actions: 허용 사용자, 대화 키와 메시지 ts, 받지 않은 누름은 chat.postEphemeral 안내)
    format.rs          Markdown -> mrkdwn 변환과 이스케이프(멘션 막기), 들어온 슬랙 표기를 평문으로
    platform.rs        Platform 구현(Slack): 3800자 나누기, 대화 키(DM 또는 채널:스레드), 이미지와 오디오 파일 고르기와 받기, 승인 질문(Block Kit section + actions 버튼, section 3000자에 맞게 나누기, 끝나면 chat.update로 버튼 없애고 결과), 도움말
  cron/              예약 작업 (junees cron)
    tz.rs              시스템 시간대 파일(TZif)과 POSIX TZ 규칙 읽기, 현지 시각 <-> UTC(건너뛴/두 번 오는 시각), 날짜 계산
    schedule.rs        일정 문법(every, in, daily, weekdays, cron 식, ISO 시각)과 다음 실행 시각
    store.rs           jobs.json (flock, 원자적 쓰기, 0600/0700, 소유자/권한 확인), 작업 찾기(id, 앞부분, 이름)
    run.rs             실행 하나: -p와 같은 무인 경로(api 에이전트 또는 Claude Code), 읽기 전용 메모리/스킬, 시간/토큰 한도, 마지막 답, 실행을 시작한 쪽(Trigger: 예약 작업 또는 웹훅 경로, GitHub 무인 쓰기를 정함)
    output.rs          실행 기록 파일 cron/output/<id>/<시각>.md (작업마다 100개)
    deliver.rs         전달: 디스코드 채널(봇 REST), 웹훅, 로컬
    daemon.rs          junees cron daemon: 매 분 틱, 따라잡기 한 번, 놓친 한 번짜리 기록, 작업마다 자식 프로세스, 결과 없이 끝난 실행 기록, 종료
    cli.rs, command.rs junees cron 인자 파싱과 명령(add/list/remove/pause/resume/allow-home/disallow-home/run/daemon)
    mod.rs             설정(timezone, cron_max_time, cron_max_tokens), 실행 마무리(finish), /cron과 !cron 목록
  github/            GitHub REST 도구 (github)
    mod.rs             설정(JUNEES_GITHUB_*, 토큰 Token은 Debug에서 숨김), repo 이름과 origin 원격 읽기, 허용 repo, 도구 등록(웹훅 실행은 register_webhook), /github 보기
    client.rs          REST v3 클라이언트 (헤더, Link 다음 페이지는 API 주소 아래만, 403/429 레이트 리밋 대기와 재시도, URL과 토큰 없는 오류)
    tool.rs            github 도구: 액션별 요청과 결과, 무인 허용 목록(웹훅 실행은 경로 목록과 둘 다), 보낼 글 검사, 승인 질문 내용(question)
    format.rs          GitHub가 준 글을 데이터로 표시 (첫 줄 안내, 인용 줄, 제어 문자 이스케이프, 상한)
    tests.rs           가짜 GitHub REST 서버로 요청 모양, 페이지, 레이트 리밋, 오류와 토큰 비노출, 허용 목록, 승인 판정 검증
  browser/           브라우저 도구 (browser, docs/browser.md)
    mod.rs             설정(JUNEES_BROWSER*), 실행 종류(Run: 묻는 대화, 묻지 않는 실행, 무인 실행)에 따른 등록
    process.rs         프로그램 찾기, --version, $JUNEES_HOME/browser 아래 프로필(0700, 지움, 소켓 경로가 길면 Chrome의 TMPDIR만 임시 폴더로), --remote-debugging-pipe로 띄우기(fd 3/4), 비밀 변수 제거, 프로세스 그룹 종료와 거두기
    cdp.rs             파이프 위 DevTools 프로토콜: NUL로 나눈 JSON, id로 답 찾기, 이벤트, 시간 한도, 닫힘 알림
    page.rs            브라우저 세션 하나: 페이지 붙기, 모든 요청 검사(Fetch), 다운로드 거절, 대화상자 닫기, 새 탭 옮기기, 로드 기다리기, 격리 world의 스냅샷과 요소 동작, 마우스/키 입력, 스크린샷
    snapshot.js        격리 world에서 도는 스냅샷 스크립트: 글과 번호 붙은 요소, 비밀번호/결제 칸과 폼 제출 판단, 클릭 위치와 덮인 요소
    address.rs         http/https만, 사설/루프백/링크 로컬 주소 판단(IPv4를 담은 IPv6 포함), 이름 해석 검사
    tool.rs            browser 도구: 액션, 승인 질문(ToolQuestions), 묻지 않는 실행의 거절, 취소 때 닫기, 유휴 닫기, 페이지 글 표시와 상한
    tests.rs           실제 Chromium(JUNEES_TEST_BROWSER, 없으면 건너뜀)과 루프백 테스트 서버로 탐색, 입력, 클릭, 새 탭, 대화상자, 다운로드 거절, 스크린샷, 거절 규칙, 승인, 프로세스와 프로필 정리 검증
  flow/              GitHub PR 흐름 (junees webhook add-github)
    mod.rs             흐름 설정 FlowConfig(저장소, 단계, 테스트 명령, 승인 수, 시간당 상한), 이벤트와 경로 쓰기, github_flow_merge
    state.rs           flows/state.json: head 커밋당 한 번 표시와 되돌리기, 시간당 모델 실행 상한, 흐름이 push한 커밋, 최근 처리
    prompts.rs         flows/review.md, address.md 기본값과 채우기(데이터 태그), 리뷰 형식(판정/필수/권장/참고) 읽기
    access.rs          세는 리뷰어: author_association과 collaborator 권한(실행마다 캐시), --approver
    merge.rs           merge 조건(열림, head 고정, 체크와 --require-check, 워크플로 변경 없음, 변경 요청, 자기 리뷰, head 커밋 승인, mergeable)과 REST merge
    address.rs         리뷰 반영의 git: 브랜치 이름 검사, origin 확인, fetch와 worktree, git 설정 스냅샷 비교, 커밋, 자격 증명 없는 테스트 명령, force 없는 push
    run.rs             이벤트 하나: 단계 계획, 루프 방지, 모델 실행(단계별로 좁힌 GitHub 쓰기), 코멘트, 기록
    setup.rs           add-github: 무작위 시크릿 파일, 기본 프롬프트, 경로, GitHub 설정 안내와 빠진 설정
    tests.rs           가짜 GitHub REST, 가짜 엔진, 로컬 bare 저장소로 이벤트별 시나리오
  webhook/           웹훅 (junees webhook)
    http.rs            작은 HTTP/1.1 수신기: 요청 줄, 헤더(16 KiB), Content-Length 본문(1 MiB), chunked 거절, 100-continue, 응답 뒤 연결 종료
    signature.rs       sha256=<hex> HMAC-SHA256 서명과 상수 시간 확인(ring)
    store.rs           경로 저장 webhooks/routes.json(0600, 잠금, 원자적 쓰기), 종류, 시크릿 위치, 이벤트 필터, GitHub 쓰기 허용(github_allow), GitHub 흐름(flow, 있으면 형식 2), 이름 검사
    template.rs        프롬프트 템플릿({{event}}, {{payload.a.b}} 등), 값 정리와 상한, 무작위 이름의 데이터 태그
    server.rs          junees webhook serve: 경로/메서드/서명/이벤트/중복 판단, 시크릿 없는 경로의 요청 검사(Host, 브라우저/프록시 헤더, JSON), 동시 연결 상한, 경로마다 줄 세우기(대기 20, 429), 실행마다 자식 프로세스, 결과 없이 끝난 실행 기록, 종료
    run.rs             실행 하나: running 기록, 프롬프트, cron 엔진(세션 source webhook), 출력 파일 webhooks/output/<이름>/, 전달, 마지막 실행 기록
    cli.rs, command.rs junees webhook 인자 파싱과 명령(serve/add/add-github/list/remove/pause/resume/allow-home/disallow-home/test/run), 흐름 경로의 실행 연결
    mod.rs             설정(webhook_max_time, webhook_max_tokens), /webhooks와 !webhooks 목록
  update/            junees update, /update: 최신 릴리스로 바이너리 바꾸기
    mod.rs             인자(--check, --version), Place(바꿀 파일, 지금 버전, 타깃, 상태 폴더, cargo 폴더: 테스트가 가짜 바이너리를 넣는 이음새), 비교 문구, 확인과 설치 순서
    version.rs         semver 버전 읽기와 비교(pre-release 포함), 태그
    fetch.rs           릴리스 주소(GitHub, 테스트용 루프백 JUNEES_DOWNLOAD_BASE), latest 리다이렉트 읽기, GitHub 파일 호스트로만 가는 리다이렉트, 크기 상한 내려받기
    install.rs         타깃 이름, SHA256SUMS 읽기와 확인, 시스템 tar로 바이너리 하나 꺼내기(일반 파일만), 같은 폴더 임시 파일 + rename 교체, cargo 설치와 돌고 있는 게이트웨이/cron/웹훅 안내
    tests.rs           루프백 가짜 릴리스 서버로 최신 확인, 교체(링크 따라가기, 권한 유지, 임시 파일 없음), 체크섬 불일치, 다른 호스트 리다이렉트 거절, 크기 상한, 쓸 수 없는 폴더, install.sh(테스트 샌드박스의 bash로: 최신/버전 지정 설치, 체크섬 불일치, 모르는 CPU, 루프백 아닌 주소)
  dashboard/         로컬 웹 대시보드: 보기와, 예약 작업과 웹훅의 멈춤/재개 (junees dashboard, docs/dashboard.md)
    mod.rs             인자(--listen 루프백만, --no-open), 실행마다 만드는 토큰, 브라우저에는 #code= 일회용 코드 주소, 브라우저를 띄우지 못했거나 --no-open이면 #token= 주소 출력
    code.rs            일회용 코드(60초, 한 번만, 틀린 코드 5번이면 무효, 상수 시간 비교), POST /api/token이 토큰과 바꿈
    server.rs          연결마다 요청 하나: Host(421), Origin(403), GET과 쓰기 경로의 POST만(405), 머리 8KB와 본문 1KB(POST만 읽음), 쓰기의 Origin 필수와 JSON, 상수 시간 Bearer 비교(401), 보안 헤더, 시간과 동시 연결 제한
    api.rs             /api/status, sessions, usage, cron, webhooks, doctor의 JSON(계약), 쿼리 검사, 미리보기의 비밀 값 가리기, doctor는 별도 런타임
    session_view.rs    /api/sessions/<id>: 기록된 대화를 페이지로(id 형식 검사, 링크 거절, 가린 뒤 자르기, 이미지와 thinking은 종류만)
    actions.rs         POST 멈춤과 재개(CLI와 같은 함수와 잠금, 정확한 id, 홈 폴더 때문에 멈춘 항목은 409, dashboard.log)
    chat/              --chat의 대화: mod.rs는 경로, 본문 검사, 대화(Room)마다 이벤트 1,000개와 질문과 취소, 한 번에 턴 하나, 열린 대화 16개, dashboard.log. runner.rs는 에이전트 전용 스레드(LocalSet, gateway ApiEngines::agent), 턴과 이벤트 변환(낱말 단위 text, 미리보기 가림), 승인 질문(ChatAsk, 5분, 취소). stream.rs는 text/event-stream(id/data, after, 15초 keepalive, 쓰기 30초)
    assets.rs          build.rs가 만든 web/dashboard 파일 목록(include_bytes!), import와 링크가 모두 서빙되는지 검사
    page_files.rs      서빙할 파일 판정(확장자별 Content-Type, 숨김 파일과 편집기 백업은 건너뜀, 모르는 확장자는 빌드 중단과 고치는 법), build.rs와 함께 씀
  doctor/            설치와 설정 점검 (junees doctor, /doctor)
    mod.rs             인자, 점검 결과(ok/warn/fail/skip, 고치는 방법), 텍스트와 JSON 출력, 비밀 값 지우기, 점검 순서
    files.rs           상태 폴더와 비공개 파일 권한, secrets/, 세션 폴더 크기
    settings.rs        설정 파일 읽기와 모든 값, 키가 있는지(값 없이), 디스코드, 텔레그램, 알림 웹훅
    claude.rs          claude --version, --online의 auth status, 환경 정리와 bubblewrap, user namespace 제한(/proc/sys)
    sandbox.rs         Landlock ABI와 sandbox 설정, bash와 같은 규칙으로 묶은 시험 sh(설정 파일 읽기 실패 확인)
    services.rs        git/gh, cron 데몬과 웹훅 서버(잠금으로 판단), 웹훅 시크릿, MCP 명령 찾기(실행 안 함), 켜진 플러그인
    online.rs          --online: 키 확인 요청 GET /v1/models(provider = openai면 GET <openai_base_url>/models와 모델 이름), 최신 릴리스 확인(update와 같은 요청)
    tests.rs           임시 상태 폴더, 가짜 PATH와 /proc/sys, 가짜 claude, 가짜 API로 각 점검의 ok/경고/실패
tests/
  common/              테스트용 가짜 Anthropic API 서버 (JSON 응답과 SSE 스트림), 테스트 환경의 JUNEES_*/ANTHROPIC_*/OPENAI_*/CLAUDE_CODE_* 변수와 GH_TOKEN/GITHUB_TOKEN/GH_ENTERPRISE_TOKEN/GITHUB_ENTERPRISE_TOKEN/GH_CONFIG_DIR을 지우고, 인자로 받은 임시 cwd와 HOME에서 도는 바이너리 명령(junees_binary(cwd, home), 크레이트 폴더나 그 안, 그 위의 폴더는 거부, 모든 바이너리 테스트가 이것으로 시작)
  binary_env.rs        junees_binary가 지우는 변수와 남기는 변수, 주어진 임시 폴더에서만 돌고 크레이트 쪽 폴더는 거부하는지, 모든 바이너리 테스트가 그 헬퍼를 쓰는지 검증(명령은 실행하지 않음)
  anthropic_client.rs  가짜 API 서버로 실제 클라이언트와 루프를 끝까지 검증 (thinking 블록이 signature까지 그대로 돌아가는지 포함)
  openai_client.rs     가짜 Chat Completions 서버로 OpenAI 호환 클라이언트 검증 (Bearer 키와 루프백에서 키 없이, 재시도와 재시도하지 않는 오류(스트림 중간의 한도 오류 포함), 조각난 스트림과 include_usage, reasoning_effort와 상한 필드 설정, 추론 토큰, 병렬 도구 호출과 tool 메시지·이미지 왕복, 바이너리 -p와 junees usage 비용)
  session_resume.rs    실제 바이너리로 -c, --resume <앞부분>, --no-session, HOME 없는 실행, 이어 간 세션의 thinking 블록 보존 검증
  secret_env.rs        기본 bash에서 API 키, 웹훅 URL, 디스코드 토큰, GitHub 토큰, Claude Code 토큰이 보이지 않는지 검증
  sandbox.rs           실제 바이너리와 가짜 API로 모델의 bash 명령이 Landlock 안에서 도는지(실행 중에 만든 경로로도 설정 파일, secrets 읽기와 memory 쓰기 실패, 작업 폴더 쓰기 성공)
  hidden_env.rs        JUNEES_HIDDEN_ENV로 지정한 변수가 bash에서 보이지 않는지 검증
  proc_environ.rs      실제 바이너리의 bash 손자 프로세스가 조상들의 /proc/<pid>/environ에서 API 키와 웹훅 URL을 읽지 못하는지 검증
  context_management.rs 가짜 API 서버로 도구 결과 비우기, 압축, 캐시 표시, 기록 재생 검증
  task_mode.rs         실제 바이너리로 임시 git 저장소에서 task init -> task run -> task status, task run 중 SIGTERM, 가짜 웹훅으로 알림 검증
  cancel.rs            실제 바이너리로 -p 중 SIGTERM(종료 코드 143), 대화 모드 Ctrl-C(턴 취소 후 다음 입력, 두 번이면 종료) 검증
  task_git_env.rs      GIT_DIR이나 GIT_CONFIG_PARAMETERS가 있어도 task 모드가 제 저장소와 설정으로만 git을 쓰는지 검증
  approval.rs          실제 바이너리로 파이프 입력 대화 모드의 위험 명령 거절과 안내, JUNEES_APPROVAL=off, -p에서는 묻지 않음, 가상 터미널에서 스트리밍 텍스트와 승인 질문이 섞이지 않는지 검증
  task_git_secret.rs   하네스 git이 심어진 훅을 실행하지 않고 git이 실행하는 필터에도 API 키를 넘기지 않는지 검증
  streaming.rs         조각내 보내는 가짜 SSE 서버로 조립 결과가 일반 응답과 같은지, 오류 이벤트/끊김/대기 초과 재시도, 취소 시 연결 종료, 바이너리가 텍스트를 한 번만 찍는지(stdout이 파이프일 때 재시도로 끊긴 조각을 버리는지) 검증
  web_search.rs        가짜 API 서버(JSON, SSE)로 웹 검색 도구 정의, pause_turn 이어 가기(멈춘 응답을 그대로, 뒤에 user 메시지 없이), pause 상한 뒤 검색을 끄고 이어 갈 때 결과 없는 검색 호출 제거, 검색 블록과 citations 재전송, 클라이언트 도구 미실행, 바이너리의 출처 목록과 조직 비활성 안내 검증
  claude_code.rs       실제 바이너리와 가짜 claude로 API 키 없이 task init/run, claude 환경에 API 변수 없음, 로그인 실패 안내와 알림, SIGTERM 검증
  claude_chat.rs       실제 바이너리와 가짜 claude로 구독 대화: 두 번째 턴이 같은 세션을 --resume, 스트리밍과 도구/거절 표시, /usage와 /compact, /context 안내, 거절 규칙 인자, claude 환경에 API 변수 없음, -p와 -c, Ctrl-C 후 다음 턴, 사용 한도 안내 검증
  context_files.rs     실제 바이너리와 가짜 API 서버로 저장소의 지침 파일이 시스템 프롬프트에 들어가는지(순서, 저장소 위 파일 제외, 숨은 문자 이스케이프), 세션 중 바뀐 파일은 /new부터 보이는지, /context 목록, JUNEES_CONTEXT_FILES로 끄기와 이름 바꾸기 검증
  memory.rs            실제 바이너리와 가짜 API 서버로 memory 도구가 저장한 항목이 지금 세션이 아니라 다음 세션의 시스템 프롬프트에 들어가는지, /memory 보기와 clear(확인 질문), /new가 메모리를 다시 읽는지 검증
  skills.rs            실제 바이너리와 가짜 API 서버로 시스템 프롬프트의 스킬 목록, skill_view/skill_manage(0600), 세션 중 목록 고정, /skills, /skill, /new 뒤 목록 다시 읽기, /skills 화면의 제어 문자 이스케이프 검증
  discord_gateway.rs   테스트 안에서 띄운 가짜 Gateway(WebSocket)와 가짜 REST로 IDENTIFY/READY, 하트비트와 ACK 누락, resume_gateway_url로 RESUME, Reconnect, INVALID_SESSION, 닫힘 코드, 거절된 토큰이 오류와 로그에 안 나옴, 허용 사용자와 멘션 규칙, 메시지 나누기, 채널별 대기열과 동시 실행, !stop/!new/!status, 종료 시 취소, 429 재시도, 실제 에이전트와 가짜 API로 재시작 뒤 채널 세션 이어 가기(meta의 source/channel, -c와 /sessions 번호 목록에서 빠짐)와 승인 답장/시간 초과 검증
  telegram_gateway.rs  테스트 안에서 띄운 가짜 Bot API(루프백, 롱 폴링과 offset 확인)로 허용 사용자와 그룹/답장/토픽 규칙, /명령과 다른 봇 명령, 그룹의 `@봇 /stop`, 전달된 메시지 표시, 캡션은 명령 아님, 409 몇 번 기다리기와 계속되면 끝내기, 업데이트 확인과 다시 온 업데이트, 재시작 뒤 중복 없음, 4096자 나누기(코드 블록 울타리 없이)와 429, /stop//status//new, 거절된 토큰이 오류와 로그에 안 나옴, 실제 에이전트와 가짜 API로 승인 질문과 답(파일 쓰기), 채팅 세션(meta source), 사진과 앨범, 두 응답에 나뉘어 온 그룹 앨범, 바이너리 시작/답/SIGTERM 종료 검증
  slack_gateway.rs     테스트 안에서 띄운 가짜 Web API와 가짜 Socket Mode WebSocket 서버(루프백)로 연결과 ack, disconnect와 끊김 뒤 다시 연결, 허용 사용자, DM/채널 멘션/봇 스레드 규칙, subtype과 봇 메시지 무시, 다시 온 이벤트와 두 번 온 멘션, mrkdwn 변환과 이스케이프, 긴 답 나누기와 429, !stop/!status/!new/!help, 거절된 토큰(봇, 앱)과 연결 실패에 토큰·URL 없음, 실제 에이전트와 가짜 API로 DM과 스레드의 승인 질문과 답, 승인 버튼(blocks 모양, 허용 목록 밖/짐작한 id/다른 메시지/다른 대화의 누름은 ephemeral 안내, chat.update로 버튼 없애고 이스케이프한 결과, 늦은 답, 시간 초과, 글 답, 버튼 거절 뒤 글 질문), 세션 meta, 이미지 첨부(files 호스트에서만 토큰으로, 원격 파일 안 받음), 바이너리 시작/답/SIGTERM 종료 검증
  mcp_oauth.rs         실제 바이너리 `junees mcp login`을 붙여 넣기 방식으로(가짜 인가 서버와 MCP 서버, PKCE와 resource 확인), 저장 파일 0600/0700, 출력에 토큰 없음, 저장된 로그인으로 `-p`의 도구 호출(토큰은 MCP 주소에만), `mcp logout`, 로그인 없는 실행은 401 한 번 뒤 그 서버만 실패하고 로그인을 시작하지 않음 검증
  mcp_http.rs          실제 바이너리 `-p`로 루프백 HTTP MCP 서버의 도구 호출(SSE 응답), 헤더 값이 환경 변수에서 오고 bash에는 그 변수가 없음, 변수가 없으면 그 서버만 빠짐 검증
  mcp.rs               실제 바이너리와 가짜 API, 가짜 MCP 서버(tests/fixtures/fake_mcp.py)로 모델의 MCP 도구 호출, 서버 환경에 API 키/디스코드 토큰/숨긴 변수가 없음, 종료 때 서버와 그 자식 정리, 0644 mcp.json 거절, /mcp, 파이프 입력 대화의 승인 거절과 trust, 게이트웨이 채널 질문, 작업 폴더의 모듈과 스크립트를 서버로 실행하지 않음(작업 폴더 안 상대 경로는 trust만) 검증
  plugins.rs           실제 바이너리로 junees plugin install/list/enable, -p에서 모델의 플러그인 도구 호출(환경에 비밀 없음, 플러그인 폴더에서 실행), pre_tool 훅의 bash 거절, post_turn 훅, 시스템 프롬프트의 조각 검증
  github.rs            실제 바이너리와 가짜 API, 가짜 GitHub REST로 -p에서 origin repo의 PR 보기 -> 리뷰(무인 허용 목록), 허용 목록 밖 쓰기 거절, 토큰이 모델/터미널/세션 기록에 없음 검증
  cron.rs              실제 바이너리와 가짜 API, 가짜 디스코드 REST로 cron add/list/run(전달, 출력 파일, 세션, 읽기 전용 도구)/pause/resume/rm, 데몬이 때가 된 작업을 자식 프로세스로 실행하고 실패를 전달하는지 검증
  github_flow.rs       실제 바이너리로 webhook add-github(시크릿 파일 0600, 안내, 형식 2), serve에 서명한 pull_request.opened -> 모델의 리뷰가 가짜 GitHub에 POST, 같은 커밋의 두 번째 전달은 아무것도 안 함, list의 최근 처리 검증
  browser.rs           실제 바이너리 -p와 가짜 API, 실제 Chromium(없으면 건너뜀)으로 루프백 페이지 열기(페이지 글 표시), evaluate 거절, 스크린샷 블록, 긴 JUNEES_HOME과 TMPDIR(프로필은 $JUNEES_HOME/browser, Chrome의 임시 폴더는 /tmp로), 끝난 뒤 브라우저 프로세스와 프로필, 임시 폴더 없음 검증
  webhook.rs           실제 바이너리와 가짜 API, 가짜 디스코드 REST로 webhook add/list/test(전달, 출력 파일, 웹훅 세션)/pause/resume/rm, serve에 reqwest로 서명한 요청(queued, duplicate, 틀린 서명, 405, 404), 자식 프로세스 실행의 bash에 시크릿 없음, SIGTERM 종료 검증
  doctor.rs            실제 바이너리로 junees doctor 출력과 종료 코드(권한 실패면 1), --json 형식, 깨진 설정 파일도 점검 결과로, --help와 잘못된 인자, 대화의 /doctor(대화 중에 깨진 설정 파일을 다시 읽음), 비밀 값이 출력되지 않음 검증
  dashboard.rs         실제 바이너리 junees dashboard --no-open --listen 127.0.0.1:0에 원시 HTTP로: 401, 421, 403, 405, 413, 431, 400, 404, 보안 헤더, 정적 파일 Content-Type, API마다 계약 필드, 키와 시크릿과 토큰과 대화에 붙인 키가 어떤 응답에도 없음, 루프백 아닌 주소 거절, 세션 상세와 멈춤/재개와 쓰기 로그, 가짜 xdg-open으로 일회용 코드 교환, 여는 프로그램이 없으면 토큰 주소 출력 검증
  dashboard_chat.rs    실제 바이너리 junees dashboard --chat과 루프백 가짜 API: 이벤트 스트림의 턴, 승인 y/n과 명령 실행 여부, 턴 중 메시지 409, 닫힌 질문 409, after로 다시 연결, 취소(질문 대기 중, 모델 대기 중), --chat 없으면 404, Origin/토큰/본문/크기 검사, 이벤트와 로그에 비밀 값 없음, claude-code 거절
  update.rs            실제 바이너리로 junees update --check(가짜 릴리스 서버), --help와 잘못된 인자, 루프백 아닌 JUNEES_DOWNLOAD_BASE 거절, 대화의 /update(바이너리는 바꾸지 않는다)
  usage.rs             실제 바이너리와 가짜 API로 대화의 /usage(호출, /model 전환 뒤 다른 가격, 합계), 기록 줄의 모델, junees usage(모델별, 날짜별, prices 덮어쓰기, 폴더 범위와 --all, 도움말과 오류) 검증
  session_search.rs    실제 바이너리와 가짜 API로 -p의 session_search 결과(지금 폴더, 채널 세션 제외, 데이터 표시), 대화의 /search와 junees sessions search 출력 검증
  simple_cli.rs        실제 바이너리로 위치 인자 질문과 설정 파일(환경 변수 우선, 잘못된 파일), 터미널 아닐 때 마법사 없음, 가상 터미널에서 처음 실행 마법사(키가 화면에 안 나옴, 0600), junees task "목표" -> junees task -> junees status, 대화의 /backend(대화도 전환), /task, /status, /sessions 고르기(다른 백엔드 세션 거절), /model, /new 검증
```
