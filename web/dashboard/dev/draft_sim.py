"""The `draft` tool of the dev chat (`chat_sim.py`, stage 23): a scripted turn
in which the model asks for a draft and the dashboard gets `draft_level`
and `draft_done` events in the shape the real server sends
(`src/dashboard/chat/runner.rs`), then the tool result the model reads.

What the message says picks the run (it must contain `초안`):
    (nothing else)   3 levels (개념, 요지, 본문), 4 sections
    길게             long section texts, so the cells need their 펼치기 button
    잘림             one section text cut by the server (`truncated: true`)
    실패             section s3 fails at its last level (an earlier level's text stands in)
    이음             3 levels and the seam level
    4단계 / 2단계    4 levels (outlines, paragraphs) / 2 levels
    상한             stops at level 2 on the run's token limit
    중단             no plan: `draft_done` with stop `failed` and no level
    예비             a `model_switch` after the draft (the fallback model took over)
Cancel the turn while it runs for stop `cancelled`. Each level takes 1.5 s.
"""

import json
import time

TITLE = "배포 가이드"
CONCEPTS = ["준비물", "설치", "설정", "운영"]
FAILED_NOTE = "이 절은 {levels}단계 생성에 실패해 앞 단계의 글을 대신 넣었습니다."
LEVEL_SECONDS = 1.5
RESULT_CHARS = 2000

GISTS = [
    "배포 전에 필요한 계정, 서버 사양, 방화벽 설정을 먼저 갖춥니다. 빠뜨리기 쉬운 것은 도메인과 인증서입니다. 준비가 끝났는지 확인하는 점검 목록도 함께 둡니다.",
    "설치 파일을 받아 서버에 올리고 서비스로 등록하는 순서를 설명합니다. 처음 설치와 업그레이드의 차이를 짚습니다. 실패했을 때 되돌리는 방법을 덧붙입니다.",
    "설정 파일의 필수 항목과 환경 변수를 정리합니다. 비밀 값을 파일에 적지 않고 다루는 법을 보여 줍니다. 설정을 바꾼 뒤 적용 여부를 확인하는 방법을 둡니다.",
    "배포한 뒤 상태를 지켜보는 방법을 설명합니다. 로그를 보는 위치와 알림 기준을 정합니다. 장애가 났을 때 처음 확인할 세 가지를 적습니다.",
]


def body_for(i, long):
    paras = [
        f"{CONCEPTS[i]} 단계에서는 먼저 현재 상태를 확인합니다. 서버에 접속해 버전과 남은 디스크 공간을 보고, 문제가 있으면 여기서 멈춥니다.",
        "각 명령은 한 번에 하나씩 실행하고, 결과가 기대와 다르면 다음으로 넘어가지 않습니다. 이 규칙이 나중에 원인을 찾는 시간을 크게 줄여 줍니다.",
        "끝나면 확인 명령을 한 번 더 실행해 같은 결과가 나오는지 봅니다. 결과는 팀 채널에 남겨 두면 다음 사람이 이어받기 쉽습니다.",
    ]
    if long:
        paras += [
            "자주 생기는 실수는 세 가지입니다. 첫째, 권한이 맞지 않는 사용자로 실행하는 것. 둘째, 이전 버전의 설정 파일을 그대로 쓰는 것. 셋째, 확인 없이 다음 단계로 넘어가는 것입니다. " * 2,
            "문제가 계속되면 로그의 마지막 쉰 줄과 실행한 명령을 함께 붙여 도움을 요청하세요. 그래야 같은 질문을 두 번 하지 않습니다.",
        ]
    return "\n\n".join(paras)


def outline_for(i):
    return "\n".join(f"{n}. {CONCEPTS[i]}의 {point}" for n, point in enumerate(["목적", "순서", "확인", "되돌리기"], 1))


def status_line(done, levels, polished, failed, calls, tokens_in, tokens_out, elapsed, sequential, sections):
    failed_text = "none" if not failed else f"{', '.join(failed)} (an earlier level's text stands in, marked in the text)"
    return (
        f"[draft] levels {done}/{levels}{' + polish' if polished else ''}, {sections} sections, "
        f"failed sections: {failed_text}, {calls} calls, {tokens_in} input and {tokens_out} output tokens, "
        f"{elapsed:.1f}s (about {sequential:.1f}s one call at a time)"
    )


def run(chats, chat, text):
    """Runs the draft part of a turn; returns False when the turn was cancelled."""
    long = "길게" in text
    cut = "잘림" in text
    fail = "실패" in text
    polish = "이음" in text
    limit = "상한" in text
    nothing = "중단" in text
    levels = 4 if "4단계" in text else 2 if "2단계" in text else 3
    count = len(CONCEPTS)
    request = "배포 가이드를 써 줘. 준비물, 설치, 설정, 운영 순서로."
    arguments: dict = {"request": request}
    if levels != 3:
        arguments["levels"] = levels
    if polish:
        arguments["polish"] = True
    preview = json.dumps(arguments, ensure_ascii=False)
    chat.emit({"type": "tool_use", "name": "draft", "input_preview": preview})
    blocks = [{"type": "tool_use", "name": "draft", "input_preview": preview}]
    chats.record(chat, "assistant", blocks)

    kinds = {2: ["concept", "body"], 3: ["concept", "gist", "body"], 4: ["concept", "gist", "outline", "paragraph"]}[levels]
    if polish:
        kinds.append("polish")
    sections = {c: {"id": f"s{i + 1}", "concept": CONCEPTS[i], "gist": None, "outline": None, "body": None} for i, c in enumerate(CONCEPTS)}
    rows = []
    done_levels = 0
    calls = 0
    total_in = 0
    total_out = 0
    sequential = 0.0
    started = time.monotonic()
    stop = "done"
    error = None

    def sleep_level():
        end = time.monotonic() + LEVEL_SECONDS
        while time.monotonic() < end:
            if chat.cancelled:
                return False
            time.sleep(0.05)
        return not chat.cancelled

    if nothing:
        sleep_level()
        stop, error = "failed", "the plan was not valid JSON (asked twice)"
        kinds = []
    for number, kind in enumerate(kinds, 1):
        if not sleep_level():
            stop = "cancelled"
            break
        if limit and number == 3:
            stop, error = "limit", "token limit reached: 31200 of 30000 tokens"
            break
        n = 1 if kind == "concept" else count if kind != "polish" else count - 1
        if kind == "paragraph":
            n = count * 4
        tin = 270 if kind == "concept" else 780 * n + 120
        tout = 96 if kind == "concept" else {"gist": 80, "outline": 60, "body": 400, "paragraph": 100, "polish": 60}[kind] * n
        calls += n
        total_in += tin
        total_out += tout
        sequential += 1.1 * n
        failed_calls = 1 if (fail and kind in ("body", "paragraph")) else 0
        rows.append({"level": number, "kind": kind, "calls": n, "failed_calls": failed_calls, "input_tokens": tin, "output_tokens": tout})
        for i, s in enumerate(sections.values()):
            if kind == "gist":
                s["gist"] = GISTS[i]
            elif kind == "outline":
                s["outline"] = outline_for(i)
            elif kind in ("body", "paragraph", "polish"):
                if fail and i == 2 and kind != "polish":
                    s["body"] = s["gist"] or s["concept"]
                    s["failed_level"] = number
                elif kind != "polish":
                    s["body"] = body_for(i, long)
        done_levels = number
        payload = []
        for i, s in enumerate(sections.values()):
            body = {
                "concept": s["concept"],
                "gist": s["gist"] or "",
                "outline": s["outline"] or "",
                "body": s["body"] or "",
                "paragraph": s["body"] or "",
                "polish": s["body"] or "",
            }[kind]
            item = {"id": s["id"], "text": body, "truncated": False}
            if cut and i == 1 and kind in ("body", "paragraph", "polish"):
                item["text"] = (body + "\n\n") * 40
                item["text"] = item["text"][:32768]
                item["truncated"] = True
            payload.append(item)
        chat.emit({"type": "draft_level", "level": number, "kind": kind, "title": TITLE, "sections": payload})

    elapsed = time.monotonic() - started
    failed = [s["id"] for s in sections.values() if s.get("failed_level")]
    polished = polish and done_levels == len(kinds) and stop == "done"
    chat.emit({
        "type": "draft_done",
        "stop": stop,
        "error": error,
        "levels": levels,
        "levels_done": min(done_levels, levels),
        "polished": polished,
        "failed_sections": failed,
        "calls": calls,
        "input_tokens": total_in,
        "output_tokens": total_out,
        "cache_read_tokens": 2048 if total_in > 4000 else 0,
        "elapsed_ms": int(elapsed * 1000),
        "sequential_ms": int(sequential * 1000),
        "by_level": rows,
    })
    if "예비" in text:
        retry = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + 15 * 60))
        chat.emit({
            "type": "model_switch",
            "from": "claude-sonnet-4-6",
            "to": "claude-haiku-4-5",
            "reason": "rate_limit",
            "retry_at": retry,
        })

    article = f"# {TITLE}\n"
    if min(done_levels, levels) > 0:
        for s in sections.values():
            best = s["body"] or s["gist"] or s["outline"]
            article += f"\n## {s['concept']}\n"
            if best:
                article += f"\n{best}\n"
            if s.get("failed_level"):
                article += "\n> [" + s["id"] + "] " + FAILED_NOTE.format(levels=s["failed_level"]) + "\n"
    else:
        article = ""
    line = status_line(min(done_levels, levels), levels, polished, failed, calls, total_in, total_out, elapsed, sequential, count)
    if stop == "done":
        result, is_error = f"{article}\n---\n{line}", False
    elif stop == "cancelled":
        result, is_error = f"cancelled by the user; the levels finished before the cancel:\n\n{article}\n---\n{line}", True
    elif stop == "limit":
        result, is_error = f"draft stopped: {error}; the levels finished before:\n\n{article}\n---\n{line}", True
    else:
        result, is_error = f"draft failed: {error}\n{line}", True
    preview_text = result[:RESULT_CHARS]
    chat.emit({"type": "tool_result", "name": "draft", "is_error": is_error, "text_preview": preview_text})
    block = {"type": "tool_result", "is_error": is_error, "text_preview": preview_text}
    if len(result) > RESULT_CHARS:
        block["truncated"] = True
    chats.record(chat, "user", [block])
    return stop != "cancelled"
