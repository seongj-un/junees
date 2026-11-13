#!/usr/bin/env python3
"""Development server for the dashboard: serves web/dashboard/ and answers
/api/* from dev/fixtures/*.json, following the API contract (token, Host
check, security headers). Not part of the binary.

    python3 web/dashboard/dev/serve.py [--port 8787] [--mode ok] [--token T]

Modes (also switchable while running: GET /__dev/mode/<mode>):
    ok            fixtures as they are
    empty         empty lists everywhere
    error         every API answers 500
    slow          answers after 3 seconds
    hang          answers after 30 seconds (the page gives up first)
    unauthorized  every API answers 401

Timestamps in the fixtures are shifted so that 2026-11-13 09:00 UTC is now.
"""

import argparse
import copy
import datetime as dt
import hmac
import json
import secrets
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = Path(__file__).resolve().parent / "fixtures"
MODES = {"ok", "empty", "error", "slow", "hang", "unauthorized"}
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


def api_body(name, query, mode, fixtures):
    """(status, body) for /api/<name>."""
    if name == "status":
        body = copy.deepcopy(fixtures["status"])
        body["uptime_secs"] = int(time.monotonic() - STARTED)
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


class Handler(BaseHTTPRequestHandler):
    server: DevServer
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

    def do_POST(self):
        self.send_json(405, {"error": "method_not_allowed", "message": "only GET"})

    do_PUT = do_DELETE = do_PATCH = do_OPTIONS = do_POST

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        port = self.server.server_address[1]
        if self.headers.get("Host") not in {f"127.0.0.1:{port}", f"localhost:{port}", f"[::1]:{port}"}:
            return self.send_json(421, {"error": "misdirected", "message": "unexpected Host"})
        url = urlsplit(self.path)

        if url.path.startswith("/__dev/mode/"):
            mode = url.path.rsplit("/", 1)[1]
            if mode not in MODES:
                return self.send_json(400, {"error": "bad_mode", "message": f"one of {sorted(MODES)}"})
            self.server.mode = mode
            return self.send_json(200, {"mode": mode})

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
        auth = self.headers.get("Authorization", "")
        expected = f"Bearer {self.server.token}"
        mode = self.server.mode
        if mode == "unauthorized" or not hmac.compare_digest(auth.encode(), expected.encode()):
            return self.send_json(401, {"error": "unauthorized", "message": "missing or wrong token"})
        if mode == "slow":
            time.sleep(3)
        elif mode == "hang":
            time.sleep(30)
        if mode == "error":
            return self.send_json(500, {"error": "internal", "message": "fixture error mode"})
        try:
            status, body = api_body(url.path[len("/api/"):], parse_qs(url.query), mode, self.server.fixtures)
        except ValueError as err:
            return self.send_json(400, {"error": "bad_query", "message": str(err)})
        self.send_json(status, body)

    def log_message(self, format, *args):  # noqa: A002 (the base class's name)
        print(f"{self.command} {self.path.split('?')[0]} {format % args}", flush=True)


STARTED = time.monotonic()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, default=8787)
    parser.add_argument("--mode", choices=sorted(MODES), default="ok")
    parser.add_argument("--token", help="fixed token instead of a random one")
    args = parser.parse_args()

    server = DevServer(("127.0.0.1", args.port), Handler)
    server.token = args.token or secrets.token_urlsafe(32)
    server.mode = args.mode
    server.fixtures = load_fixtures()
    print(f"http://127.0.0.1:{server.server_address[1]}/#token={server.token}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
