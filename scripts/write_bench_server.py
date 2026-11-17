#!/usr/bin/env python3
"""A loopback fake of the Anthropic Messages API for timing `junees write`.

Every reply takes TTFT seconds plus PER_TOKEN seconds per output token, as a
model that streams at a fixed rate would. The replies follow the prompts of
`junees write` (plan, gist, section text, paragraph outline, paragraph, seam);
any other request (a one-shot `junees -p`) gets one text as long as all
sections together. Only for local measurements: it never calls a real API.

Environment: PORT (18123), SECTIONS (8), BODY_TOKENS (400), GIST_TOKENS (80),
PER_TOKEN (0.005), TTFT (0.3), LOG (a file that gets one JSON line per
request: kind, start and end time, output tokens).
"""

import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("PORT", "18123"))
SECTIONS = int(os.environ.get("SECTIONS", "8"))
BODY_TOKENS = int(os.environ.get("BODY_TOKENS", "400"))
GIST_TOKENS = int(os.environ.get("GIST_TOKENS", "80"))
PER_TOKEN = float(os.environ.get("PER_TOKEN", "0.005"))
TTFT = float(os.environ.get("TTFT", "0.3"))
LOG = os.environ.get("LOG")
LOCK = threading.Lock()


def words(count, label):
    return " ".join(f"{label}{i}" for i in range(count))


def last_user_text(body):
    for message in reversed(body.get("messages", [])):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, str):
            return content
        return "".join(b.get("text", "") for b in content if b.get("type") == "text")
    return ""


def system_text(body):
    system = body.get("system", "")
    if isinstance(system, list):
        return "".join(b.get("text", "") for b in system)
    return system


def reply_for(body):
    system = system_text(body)
    user = last_user_text(body)
    if "Your part is the plan" in system:
        plan = {
            "title": "Bench",
            "sections": [
                {"id": f"s{i}", "concept": f"concept {i}"} for i in range(1, SECTIONS + 1)
            ],
        }
        return "plan", json.dumps(plan), 12 * SECTIONS
    if "edit the seam" in system:
        return "seam", json.dumps({"end": words(60, "e"), "start": words(60, "s")}), 130
    if "the gist of" in user:
        return "gist", words(GIST_TOKENS, "g"), GIST_TOKENS
    if "paragraph outline" in user:
        return "outline", "\n".join(f"{i}. point" for i in range(1, 5)), 20
    if "outline line" in user:
        tokens = BODY_TOKENS // 4
        return "paragraph", words(tokens, "p"), tokens
    if "the full text of" in user:
        return "body", words(BODY_TOKENS, "b"), BODY_TOKENS
    tokens = SECTIONS * BODY_TOKENS
    return "one-shot", words(tokens, "w"), tokens


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        started = time.time()
        kind, text, tokens = reply_for(body)
        time.sleep(TTFT + tokens * PER_TOKEN)
        payload = json.dumps(
            {
                "id": "msg_bench",
                "type": "message",
                "role": "assistant",
                "model": body.get("model", "fake"),
                "content": [{"type": "text", "text": text}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": len(json.dumps(body)) // 4,
                    "output_tokens": tokens,
                },
            }
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
        if LOG:
            with LOCK, open(LOG, "a") as log:
                log.write(
                    json.dumps(
                        {
                            "kind": kind,
                            "start": started,
                            "end": time.time(),
                            "tokens": tokens,
                            "cache": "cache_control" in json.dumps(body.get("system", "")),
                        }
                    )
                    + "\n"
                )


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
