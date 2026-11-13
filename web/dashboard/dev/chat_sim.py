"""Chat for the dev server (`serve.py --chat`): the /api/chat endpoints of
the stage 19 contract, with a scripted agent instead of a model.

What the scripted turn does depends on the message:
    contains 정리, 지워 or 승인   asks to approve a bash command (y/a/n)
    contains 파일                 asks to approve a write_file (y/n, no "always")
    contains 오류                 ends the turn with an error event
    anything else                reads a file, then answers
Every turn streams its answer in small pieces so the page shows it growing.
"""

import json
import secrets
import threading
import time

APPROVAL_SECONDS = 300
KEEP_EVENTS = 1000


class Chat:
    def __init__(self, session_id):
        self.id = session_id
        self.events = []  # (seq, dict)
        self.seq = 0
        self.turn = 0
        self.running = False
        self.cancelled = False
        self.questions = {}  # id -> {"options", "answer", "closed"}
        self.cond = threading.Condition()

    def emit(self, event):
        with self.cond:
            self.seq += 1
            self.events.append((self.seq, event))
            del self.events[:-KEEP_EVENTS]
            self.cond.notify_all()

    def since(self, after):
        with self.cond:
            return [(seq, ev) for seq, ev in self.events if after is None or seq > after]


class Chats:
    """Every chat of one dev server, and what its scripted turns do."""

    def __init__(self, known_sessions, approval_seconds=APPROVAL_SECONDS):
        self.known = set(known_sessions)
        self.chats = {}
        self.lock = threading.Lock()
        self.approval_seconds = approval_seconds
        # Raised by GET /__dev/drop-streams: open streams end, as a network drop.
        self.generation = 0

    def get(self, session_id):
        with self.lock:
            return self.chats.get(session_id)

    def start(self, session_id=None):
        """(status, body) for POST /api/chat."""
        with self.lock:
            if session_id is None:
                session_id = time.strftime("%Y%m%d-%H%M%S", time.gmtime()) + "-" + secrets.token_hex(2)
            elif session_id not in self.known and session_id not in self.chats:
                return 404, {"error": "not_found", "message": f"no session {session_id} in this folder"}
            self.chats.setdefault(session_id, Chat(session_id))
            self.known.add(session_id)
        return 200, {"session_id": session_id}

    def send(self, chat, text):
        """(status, body) for POST /api/chat/<id>/messages."""
        with chat.cond:
            if chat.running:
                return 409, {"error": "turn_running", "message": "a turn is still running; wait for turn_end or cancel it"}
            chat.running = True
            chat.cancelled = False
            chat.turn += 1
            turn = chat.turn
        threading.Thread(target=self.script, args=(chat, turn, text), daemon=True).start()
        return 202, {"turn": turn}

    def answer(self, chat, question_id, answer):
        with chat.cond:
            question = chat.questions.get(question_id)
            if question is None:
                return 404, {"error": "not_found", "message": "no such question"}
            if question["closed"]:
                return 409, {"error": "closed", "message": "the question is closed already"}
            if answer not in question["options"]:
                return 400, {"error": "bad_answer", "message": f"answer one of {question['options']}"}
            question["answer"] = answer
            chat.cond.notify_all()
        return 200, {}

    def cancel(self, chat):
        with chat.cond:
            chat.cancelled = True
            chat.cond.notify_all()
        return 200, {}

    # ---- the scripted agent ----

    def script(self, chat, turn, text):
        try:
            self.run_turn(chat, turn, text)
        finally:
            with chat.cond:
                chat.running = False

    def stream_text(self, chat, words):
        for word in words.split(" "):
            if chat.cancelled:
                return False
            chat.emit({"type": "text", "delta": word + " "})
            time.sleep(0.06)
        return True

    def ask(self, chat, kind, summary, options):
        """Asks a question and waits for its answer, the timeout or a cancel."""
        question_id = "q" + secrets.token_hex(4)
        question = {"options": options, "answer": None, "closed": False}
        with chat.cond:
            chat.questions[question_id] = question
        expires = time.time() + self.approval_seconds
        chat.emit({
            "type": "approval",
            "question_id": question_id,
            "kind": kind,
            "summary": summary,
            "options": options,
            "expires_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(expires)),
        })
        with chat.cond:
            while question["answer"] is None and not chat.cancelled and time.time() < expires:
                chat.cond.wait(timeout=max(0.1, expires - time.time()))
            answer, by = (question["answer"], "browser") if question["answer"] else ("n", "timeout")
            question["closed"] = True
        # The contract names no `by` for a cancelled turn; the turn_end that
        # follows closes the card on the page.
        if not (chat.cancelled and question["answer"] is None):
            chat.emit({"type": "approval_closed", "question_id": question_id, "answer": answer, "by": by})
        return answer

    def run_turn(self, chat, turn, text):
        chat.emit({"type": "turn_start", "turn": turn, "text": text})
        time.sleep(0.3)
        ok = self.stream_text(chat, "요청을 확인했습니다. 먼저 작업 폴더를 살펴보겠습니다.")
        if ok and "오류" in text:
            chat.emit({"type": "error", "message": "the API answered 529 overloaded three times; try again later"})
            return
        if ok:
            chat.emit({"type": "tool_use", "name": "read_file", "input_preview": '{"path": "README.md"}'})
            time.sleep(0.4)
            chat.emit({
                "type": "tool_result",
                "name": "read_file",
                "is_error": False,
                "text_preview": "# Junees\n\nJunees는 Rust로 만든 AI 에이전트 하네스입니다.\n\n## 설치\n...",
            })
        if ok and any(word in text for word in ("정리", "지워", "승인")):
            command = "rm -rf target/debug/incremental"
            ok = self.stream_text(chat, "빌드 캐시를 지우려면 명령을 실행해야 합니다.")
            if ok:
                chat.emit({"type": "tool_use", "name": "bash", "input_preview": command})
                answer = self.ask(chat, "bash", f"{command}\n\n이유: 파일을 지우는 명령(rm -rf)입니다.", ["y", "a", "n"])
                if answer in ("y", "a") and not chat.cancelled:
                    time.sleep(0.5)
                    chat.emit({"type": "tool_result", "name": "bash", "is_error": False, "text_preview": "(no output)\nexit status 0"})
                    ok = self.stream_text(chat, "빌드 캐시를 지웠습니다. 다음 빌드는 처음부터 다시 컴파일합니다.")
                else:
                    chat.emit({"type": "tool_result", "name": "bash", "is_error": True, "text_preview": "the person did not approve this command"})
                    ok = self.stream_text(chat, "명령을 실행하지 않았습니다. 다른 방법이 필요하면 알려 주세요.")
        elif ok and "파일" in text:
            ok = self.stream_text(chat, "작업 폴더 밖에 파일을 써야 해서 허락이 필요합니다.")
            if ok:
                chat.emit({"type": "tool_use", "name": "write_file", "input_preview": '{"path": "/tmp/notes.md", "content": "..."}'})
                answer = self.ask(chat, "write_file", "/tmp/notes.md\n\n이유: 작업 폴더 밖에 씁니다.", ["y", "n"])
                chat.emit({
                    "type": "tool_result",
                    "name": "write_file",
                    "is_error": answer != "y",
                    "text_preview": "wrote 12 lines" if answer == "y" else "the person did not approve this write",
                })
        elif ok:
            ok = self.stream_text(chat, "README에 설치 방법과 사용법이 정리되어 있습니다. 더 알고 싶은 부분이 있으면 말씀해 주세요.")
        chat.emit({"type": "usage", "input_tokens": 1830 + 400 * turn, "output_tokens": 212, "cost_usd": 0.0087})
        chat.emit({"type": "turn_end", "stop_reason": "cancelled" if chat.cancelled else "end_turn"})


def stream(handler, chat, after, chats):
    """Writes GET /api/chat/<id>/events until the client goes or a drop is asked."""
    handler.send_response(200)
    handler.send_header("Content-Type", "text/event-stream; charset=utf-8")
    for key, value in handler.SECURITY_HEADERS.items():
        handler.send_header(key, value)
    handler.end_headers()
    generation = chats.generation
    last = after
    try:
        while chats.generation == generation:
            events = chat.since(last)
            for seq, event in events:
                handler.wfile.write(f"id: {seq}\ndata: {json.dumps(event, ensure_ascii=False)}\n\n".encode())
                last = seq
            handler.wfile.flush()
            with chat.cond:
                if not chat.since(last):
                    if not chat.cond.wait(timeout=15) and chats.generation == generation:
                        handler.wfile.write(b": keepalive\n\n")
                        handler.wfile.flush()
    except (BrokenPipeError, ConnectionResetError):
        pass
