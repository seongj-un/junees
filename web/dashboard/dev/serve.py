#!/usr/bin/env python3
"""Development server for the dashboard: serves web/dashboard/ and answers
/api/* from dev/fixtures/*.json, following the API contract (token, Host
check, security headers). Not part of the binary.

    python3 web/dashboard/dev/serve.py [--port 8787] [--mode ok] [--token T]

One-time codes: the second line it prints opens with #code=, which the page
trades at POST /api/token (60 seconds, once, 5 wrong tries end it), as the
browser `junees dashboard` opens does. GET /__dev/code makes another.

Chat (stage 19): with --chat, /api/status says "chat": true and the
/api/chat endpoints answer from chat_sim.py, a scripted agent (see there for
what each message does). --approval-seconds shortens the 5-minute question
timeout. GET /__dev/drop-streams ends every open event stream, to try the
page's reconnection.

Writes: POST /api/cron/<id>/pause|resume and /api/webhooks/<name>/pause|resume
change the fixtures in memory (restart to reset). They need the token, an
Origin from this server and an empty or {} body, as the contract says.

Modes (also switchable while running: GET /__dev/mode/<mode>):
    ok            fixtures as they are
    empty         empty lists everywhere
    error         every API answers 500
    slow          answers after 3 seconds
    hang          answers after 30 seconds (the page gives up first)
    unauthorized  every API answers 401
    forbidden     every write answers 403
    conflict      every write answers 409
    busy          every write answers 503 busy (the jobs file lock)

Timestamps in the fixtures are shifted so that 2026-11-13 09:00 UTC is now.
"""

import argparse
import sys
import copy
import datetime as dt
import hmac
import json
import re
import secrets
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlsplit

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = Path(__file__).resolve().parent / "fixtures"
MODES = {"ok", "empty", "error", "slow", "hang", "unauthorized", "forbidden", "conflict", "busy"}
SESSION_ID = re.compile(r"[A-Za-z0-9_-]{1,64}")
ACTION = re.compile(r"/api/(cron|webhooks)/([^/]+)/(pause|resume)")
CHAT = re.compile(r"/api/chat(?:/([^/]+)/(messages|cancel|approvals/([^/]+)))?")
CHAT_EVENTS = re.compile(r"/api/chat/([^/]+)/events")

sys.path.insert(0, str(Path(__file__).resolve().parent))
import chat_sim  # noqa: E402  (next to this file)
STATIC = {
    "/": ("index.html", "text/html; charset=utf-8"),
}
TYPES = {
    ".html": "text/html; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".js": "text/javascript; charset=utf-8",
}
SECURITY_HEADERS = {
    "Content-Security-Policy": "default-src 'self'; script-src 'self'; style-src 'self'; "
    "img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'",
    "X-Content-Type-Options": "nosniff",
    "Referrer-Policy": "no-referrer",
    "Cache-Control": "no-store",
}
TIME_KEYS = {"updated_at", "next_run", "at"}
ANCHOR = dt.datetime(2026, 11, 13, 9, 0, tzinfo=dt.timezone.utc)


def load_fixtures():
    data = {p.stem: json.loads(p.read_text(encoding="utf-8")) for p in FIXTURES.glob("*.json")}
    # The fixtures were written as if it were ANCHOR; move them to now so
    # "3분 전" and "내일" read the same on any day.
    shift = dt.datetime.now(dt.timezone.utc) - ANCHOR
    day_shift = dt.date.today() - ANCHOR.date()

    def walk(value):
        if isinstance(value, dict):
            for key, item in value.items():
                if key in TIME_KEYS and isinstance(item, str):
                    at = dt.datetime.fromisoformat(item.replace("Z", "+00:00")) + shift
                    value[key] = at.strftime("%Y-%m-%dT%H:%M:%SZ")
                elif key == "date" and isinstance(item, str):
                    value[key] = (dt.date.fromisoformat(item) + day_shift).isoformat()
                else:
                    walk(item)
        elif isinstance(value, list):
            for item in value:
                walk(item)

    walk(data)
    return data


def bounded(query, name, default, low, high):
    raw = query.get(name, [str(default)])[0]
    if not raw.isdigit() or not low <= int(raw) <= high:
        raise ValueError(f"{name} must be a whole number from {low} to {high}")
    return int(raw)


def api_body(name, query, mode, fixtures, chat=False):
    """(status, body) for /api/<name>."""
    if name == "status":
        body = copy.deepcopy(fixtures["status"])
        body["uptime_secs"] = int(time.monotonic() - STARTED)
        body["chat"] = chat
        return 200, body
    if name.startswith("sessions/"):
        session_id = unquote(name[len("sessions/"):])
        if not SESSION_ID.fullmatch(session_id):
            return 400, {"error": "bad_id", "message": "a session id is [A-Za-z0-9_-]{1,64}"}
        detail = fixtures["session_details"].get(session_id)
        if detail is None or mode == "empty":
            return 404, {"error": "not_found", "message": "no such session"}
        offset = bounded(query, "offset", 0, 0, 10**9)
        limit = bounded(query, "limit", 100, 1, 200)
        body = {key: value for key, value in detail.items() if key != "messages"}
        body["offset"] = offset
        body["messages"] = detail["messages"][offset : offset + limit]
        return 200, body
    if name == "sessions":
        limit = bounded(query, "limit", 50, 1, 200)
        sessions = [] if mode == "empty" else fixtures["sessions"]["sessions"][:limit]
        return 200, {"sessions": sessions}
    if name == "usage":
        days = bounded(query, "days", 30, 1, 365)
        start = (dt.date.today() - dt.timedelta(days=days - 1)).isoformat()
        rows = [] if mode == "empty" else [r for r in fixtures["usage"]["days"] if r["date"] >= start]
        priced = [r for r in rows if r["cost_usd"] is not None]
        total = {
            "input_tokens": sum(r["input_tokens"] for r in rows),
            "output_tokens": sum(r["output_tokens"] for r in rows),
            # null when nothing in the range could be priced, as the server does
            "cost_usd": round(sum(r["cost_usd"] for r in priced), 4) if priced else None,
        }
        return 200, {"days": rows, "total": total}
    if name == "cron":
        return 200, {"jobs": [] if mode == "empty" else fixtures["cron"]["jobs"]}
    if name == "webhooks":
        return 200, {"routes": [] if mode == "empty" else fixtures["webhooks"]["routes"]}
    if name == "doctor":
        return 200, {"sections": [] if mode == "empty" else fixtures["doctor"]["sections"]}
    return 404, {"error": "not_found", "message": "no such API"}


class DevServer(ThreadingHTTPServer):
    token = ""
    mode = "ok"
    fixtures: dict = {}
    chats = None  # chat_sim.Chats with --chat
    # One-time codes for the address: code -> [expires (monotonic), wrong tries]
    codes: dict = {}

    def new_code(self):
        code = secrets.token_urlsafe(32)
        self.codes[code] = [time.monotonic() + CODE_SECONDS, 0]
        return code

    def trade(self, given):
        """Uses up `given` if it is a live code; a wrong one counts against every live code."""
        now = time.monotonic()
        for code, (expires, _) in list(self.codes.items()):
            if expires < now:
                del self.codes[code]
        for code in list(self.codes):
            if hmac.compare_digest(code.encode(), given.encode()):
                del self.codes[code]
                return True
        for code in list(self.codes):
            self.codes[code][1] += 1
            if self.codes[code][1] >= 5:
                del self.codes[code]
        return False


class Handler(BaseHTTPRequestHandler):
    server: DevServer
    SECURITY_HEADERS = SECURITY_HEADERS
    server_version = "junees-dashboard-dev"

    def send(self, status, body, content_type):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        for key, value in SECURITY_HEADERS.items():
            self.send_header(key, value)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def send_json(self, status, obj):
        self.send(status, json.dumps(obj, ensure_ascii=False).encode(), "application/json; charset=utf-8")

    def do_PUT(self):
        self.send_json(405, {"error": "method_not_allowed", "message": "only GET and the POST actions"})

    do_DELETE = do_PATCH = do_OPTIONS = do_PUT

    def do_POST(self):
        """Pause or resume a fixture job or route, with the write rules of the contract."""
        port = self.server.server_address[1]
        host = self.headers.get("Host")
        if host not in {f"127.0.0.1:{port}", f"localhost:{port}", f"[::1]:{port}"}:
            return self.send_json(421, {"error": "misdirected", "message": "unexpected Host"})
        if urlsplit(self.path).path == "/api/token":
            return self.trade_code(host)
        if CHAT.fullmatch(urlsplit(self.path).path):
            return self.chat_write(host)
        match = ACTION.fullmatch(urlsplit(self.path).path)
        if not match:
            return self.send_json(405 if self.path.startswith("/api/") else 404, {"error": "method_not_allowed", "message": "only GET here"})
        length = int(self.headers.get("Content-Length") or 0)
        if length > 1024:
            return self.send_json(413, {"error": "too_large", "message": "the body may be 1KB"})
        body = self.rfile.read(length).decode("utf-8", "replace").strip() if length else ""
        # The real server checks Origin before the token.
        mode = self.server.mode
        if mode == "forbidden" or self.headers.get("Origin") != f"http://{host}":
            return self.send_json(403, {"error": "forbidden_origin", "message": "a write needs an Origin from this server"})
        if not self.authorized():
            return self.send_json(401, {"error": "unauthorized", "message": "missing or wrong token"})
        content_type = self.headers.get("Content-Type")
        if content_type is not None and content_type.split(";")[0].strip() != "application/json":
            return self.send_json(415, {"error": "unsupported_media_type", "message": "application/json only"})
        if body not in ("", "{}"):
            return self.send_json(400, {"error": "bad_body", "message": "the body must be empty or {}"})
        if mode == "slow":
            time.sleep(3)
        elif mode == "hang":
            time.sleep(30)
        if mode == "error":
            return self.send_json(500, {"error": "internal", "message": "fixture error mode"})
        kind, key, action = match.group(1), unquote(match.group(2)), match.group(3)
        if mode == "busy":
            return self.send_json(503, {"error": "busy", "message": "the jobs file is busy (another junees holds its lock); nothing was changed"})
        if mode == "conflict":
            return self.send_json(409, {"error": "conflict", "message": f"fixture conflict mode: cannot {action} {key}"})
        items = self.server.fixtures[kind]["jobs" if kind == "cron" else "routes"]
        field = "id" if kind == "cron" else "name"
        item = next((it for it in items if it[field] == key), None)
        if item is None:
            return self.send_json(404, {"error": "not_found", "message": f"no such {'job' if kind == 'cron' else 'route'}"})
        examples = self.server.fixtures["actions"]
        if action == "pause":
            item["paused"] = True
            item["paused_for_home"] = False
        elif item.get("paused_for_home"):
            example = examples["resume_home_conflict" if kind == "cron" else "resume_home_conflict_webhook"]["body"]
            return self.send_json(409, {**example, "message": example["message"].format(id=key, name=key)})
        elif kind == "cron" and len(item["schedule"].split()) != 5:
            # A one-time job whose time has passed cannot resume, as in the CLI.
            example = examples["resume_once_conflict"]["body"]
            return self.send_json(409, {**example, "message": example["message"].format(id=key)})
        else:
            item["paused"] = False
            if kind == "cron":
                soon = dt.datetime.now(dt.timezone.utc) + dt.timedelta(hours=1)
                item["next_run"] = soon.replace(minute=0, second=0).strftime("%Y-%m-%dT%H:%M:%SZ")
        self.send_json(200, item)

    def chat_write(self, host):
        """POST /api/chat, .../messages, .../approvals/<q>, .../cancel."""
        chats = self.server.chats
        if chats is None:
            return self.send_json(404, {"error": "not_found", "message": "chat is off (start with --chat)"})
        if self.server.mode == "forbidden" or self.headers.get("Origin") != f"http://{host}":
            return self.send_json(403, {"error": "forbidden_origin", "message": "a write needs an Origin from this server"})
        if not self.authorized():
            return self.send_json(401, {"error": "unauthorized", "message": "missing or wrong token"})
        content_type = self.headers.get("Content-Type")
        if content_type is not None and content_type.split(";")[0].strip() != "application/json":
            return self.send_json(415, {"error": "unsupported_media_type", "message": "application/json only"})
        length = int(self.headers.get("Content-Length") or 0)
        if length > 256 * 1024:
            return self.send_json(413, {"error": "too_large", "message": "the body may be 256KB"})
        try:
            body = json.loads(self.rfile.read(length) or b"{}")
            if not isinstance(body, dict):
                raise ValueError
        except ValueError:
            return self.send_json(400, {"error": "bad_body", "message": "send a JSON object"})
        if self.server.mode == "error":
            return self.send_json(500, {"error": "internal", "message": "fixture error mode"})
        match = CHAT.fullmatch(urlsplit(self.path).path)
        session_id, what, question_id = match.group(1), match.group(2), match.group(3)
        if session_id is None:
            given = body.get("session_id")
            if given is not None and (not isinstance(given, str) or not SESSION_ID.fullmatch(given)):
                return self.send_json(400, {"error": "bad_id", "message": "a session id is [A-Za-z0-9_-]{1,64}"})
            return self.send_json(*chats.start(given))
        chat = chats.get(unquote(session_id))
        if chat is None:
            return self.send_json(404, {"error": "not_found", "message": "no such chat; POST /api/chat first"})
        if what == "messages":
            text = body.get("text")
            if not isinstance(text, str) or not 1 <= len(text) <= 32000:
                return self.send_json(400, {"error": "bad_text", "message": "text is 1 to 32,000 characters"})
            return self.send_json(*chats.send(chat, text))
        if what == "cancel":
            return self.send_json(*chats.cancel(chat))
        return self.send_json(*chats.answer(chat, unquote(question_id), body.get("answer")))

    def trade_code(self, host):
        """POST /api/token: a one-time code from the address for the token."""
        if self.headers.get("Origin") != f"http://{host}":
            return self.send_json(403, {"error": "forbidden_origin", "message": "a write needs an Origin from this server"})
        content_type = self.headers.get("Content-Type")
        if content_type is not None and content_type.split(";")[0].strip() != "application/json":
            return self.send_json(415, {"error": "unsupported_media_type", "message": "application/json only"})
        length = int(self.headers.get("Content-Length") or 0)
        if length > 1024:
            return self.send_json(413, {"error": "too_large", "message": "the body may be 1KB"})
        try:
            code = json.loads(self.rfile.read(length) or b"{}").get("code")
        except (ValueError, AttributeError):
            return self.send_json(400, {"error": "bad_body", "message": "send {\"code\": \"...\"}"})
        if self.server.trade(code if isinstance(code, str) else ""):
            return self.send_json(200, {"token": self.server.token})
        self.send_json(401, {"error": "bad_code", "message": "the code is wrong, used or expired"})

    def authorized(self):
        auth = self.headers.get("Authorization", "")
        expected = f"Bearer {self.server.token}"
        return self.server.mode != "unauthorized" and hmac.compare_digest(auth.encode(), expected.encode())

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        port = self.server.server_address[1]
        if self.headers.get("Host") not in {f"127.0.0.1:{port}", f"localhost:{port}", f"[::1]:{port}"}:
            return self.send_json(421, {"error": "misdirected", "message": "unexpected Host"})
        url = urlsplit(self.path)

        if url.path == "/__dev/code":
            # A fresh one-time address, as `junees dashboard` gives the browser it opens.
            return self.send_json(200, {"url": f"http://{self.headers.get('Host')}/#code={self.server.new_code()}"})

        if url.path == "/__dev/drop-streams":
            chats = self.server.chats
            if chats is not None:
                chats.generation += 1
                for chat in list(chats.chats.values()):
                    with chat.cond:
                        chat.cond.notify_all()
            return self.send_json(200, {"dropped": True})

        if url.path.startswith("/__dev/mode/"):
            mode = url.path.rsplit("/", 1)[1]
            if mode not in MODES:
                return self.send_json(400, {"error": "bad_mode", "message": f"one of {sorted(MODES)}"})
            self.server.mode = mode
            return self.send_json(200, {"mode": mode})

        origin = self.headers.get("Origin")
        if origin is not None and origin != f"http://{self.headers.get('Host')}":
            return self.send_json(403, {"error": "forbidden_origin", "message": "another site's page"})

        if url.path.startswith("/api/"):
            return self.api(url)

        if url.path in STATIC:
            name, content_type = STATIC[url.path]
        else:
            name = url.path.lstrip("/")
            content_type = TYPES.get(Path(name).suffix)
            if "/" in name or content_type is None:
                return self.send_json(404, {"error": "not_found", "message": "no such file"})
        path = ROOT / name
        if not path.is_file():
            return self.send_json(404, {"error": "not_found", "message": "no such file"})
        self.send(200, path.read_bytes(), content_type)

    def api(self, url):
        mode = self.server.mode
        if not self.authorized():
            return self.send_json(401, {"error": "unauthorized", "message": "missing or wrong token"})
        if mode == "slow":
            time.sleep(3)
        elif mode == "hang":
            time.sleep(30)
        if mode == "error":
            return self.send_json(500, {"error": "internal", "message": "fixture error mode"})
        events = CHAT_EVENTS.fullmatch(url.path)
        if events:
            chat = self.server.chats.get(unquote(events.group(1))) if self.server.chats else None
            if chat is None:
                return self.send_json(404, {"error": "not_found", "message": "no such chat"})
            try:
                after = bounded(parse_qs(url.query), "after", None, 0, 10**12) if "after" in parse_qs(url.query) else None
            except ValueError as err:
                return self.send_json(400, {"error": "bad_query", "message": str(err)})
            return chat_sim.stream(self, chat, after, self.server.chats)
        try:
            status, body = api_body(
                url.path[len("/api/"):], parse_qs(url.query), mode, self.server.fixtures, self.server.chats is not None
            )
        except ValueError as err:
            return self.send_json(400, {"error": "bad_query", "message": str(err)})
        self.send_json(status, body)

    def log_message(self, format, *args):  # noqa: A002 (the base class's name)
        print(f"{self.command} {self.path.split('?')[0]} {format % args}", flush=True)


STARTED = time.monotonic()
CODE_SECONDS = 60


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, default=8787)
    parser.add_argument("--mode", choices=sorted(MODES), default="ok")
    parser.add_argument("--token", help="fixed token instead of a random one")
    parser.add_argument("--chat", action="store_true", help="turn chat on, as junees dashboard --chat")
    parser.add_argument("--approval-seconds", type=int, default=chat_sim.APPROVAL_SECONDS)
    args = parser.parse_args()

    server = DevServer(("127.0.0.1", args.port), Handler)
    server.token = args.token or secrets.token_urlsafe(32)
    server.mode = args.mode
    server.codes = {}
    server.fixtures = load_fixtures()
    if args.chat:
        server.chats = chat_sim.Chats(server.fixtures["session_details"].keys(), args.approval_seconds)
    base = f"http://127.0.0.1:{server.server_address[1]}/"
    print(f"{base}#token={server.token}", flush=True)
    print(f"{base}#code={server.new_code()}  (one-time, 60 s; GET /__dev/code for another)", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
