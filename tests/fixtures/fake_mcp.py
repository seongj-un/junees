# A fake MCP server for the tests (src/mcp/tests.rs, tests/mcp.rs). It
# speaks the era FAKE_MCP_MODE names and logs every message it reads to
# FAKE_MCP_LOG, one JSON object per line.
import json, os, subprocess, sys, time

MODE = os.environ.get("FAKE_MCP_MODE", "modern")
LOG = os.environ.get("FAKE_MCP_LOG")
MODERN = MODE.startswith("modern") or MODE == "no_tools"

def log(message):
    if LOG:
        with open(LOG, "a") as f:
            f.write(json.dumps(message) + "\n")

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

def result(id, value):
    if MODERN:
        value = dict({"resultType": "complete"}, **value)
    send({"jsonrpc": "2.0", "id": id, "result": value})

def error(id, code, message, data=None):
    body = {"code": code, "message": message}
    if data is not None:
        body["data"] = data
    send({"jsonrpc": "2.0", "id": id, "error": body})

def schema(**properties):
    return {"type": "object", "properties": properties}

TOOLS = [
    {"name": "echo", "description": "Echoes\u001b[2K the text\u202e", "inputSchema": schema(text={"type": "string", "description": "what\u0007"})},
    {"name": "env", "description": "Shows variables", "inputSchema": schema(names={"type": "array"})},
    {"name": "fail", "description": "Fails", "inputSchema": schema()},
    {"name": "sleep", "description": "Sleeps", "inputSchema": schema(secs={"type": "number"})},
    {"name": "media", "description": "Mixed content", "inputSchema": schema()},
    {"name": "crash", "description": "Exits", "inputSchema": schema()},
    {"name": "spawn", "description": "Starts a background sleep", "inputSchema": schema()},
    {"name": "big", "description": "A long result", "inputSchema": schema()},
    {"name": "ask", "description": "Wants input", "inputSchema": schema()},
    {"name": "do.thing!", "description": "Odd name", "inputSchema": schema()},
    {"name": "do_thing_", "description": "Same after cleaning", "inputSchema": schema()},
    {"name": "x" * 80, "description": "Long name", "inputSchema": schema()},
    {"name": "echo", "description": "A second echo", "inputSchema": schema()},
    {"name": "no_schema", "description": "No schema"},
]
PAGE = 4
initialized = False
LINGER = False

for line in sys.stdin:
    message = json.loads(line)
    log(message)
    method = message.get("method")
    id = message.get("id")
    params = message.get("params") or {}
    if method == "notifications/initialized":
        initialized = True
        send({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})
        send({"jsonrpc": "2.0", "id": "srv-2", "method": "roots/list"})
        send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "hi"}})
        continue
    if id is None:
        continue
    if method is None:
        continue
    if MODE == "hang":
        continue
    if MODERN:
        version = (params.get("_meta") or {}).get("io.modelcontextprotocol/protocolVersion")
        if method == "initialize":
            error(id, -32601, "initialize is gone; supported: 2026-07-28")
            continue
        if MODE == "modern_other":
            error(id, -32022, "Unsupported protocol version", {"supported": ["2099-01-01"], "requested": version})
            continue
        if version != "2026-07-28":
            error(id, -32602, "missing _meta")
            continue
        if method == "server/discover":
            capabilities = {} if MODE == "no_tools" else {"tools": {}}
            result(id, {"supportedVersions": ["2026-07-28"], "capabilities": capabilities,
                        "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "fake", "version": "1"}}})
            continue
    else:
        if method == "server/discover":
            if MODE == "legacy_silent":
                continue
            if MODE == "legacy_dies":
                sys.exit(3)
            error(id, -32601, "Method not found")
            continue
        if method == "initialize":
            version = "1999-01-01" if MODE == "legacy_odd" else "2025-06-18"
            result(id, {"protocolVersion": version, "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fake", "version": "1"}})
            continue
        if not initialized:
            error(id, -32600, "not initialized")
            continue
    if method == "tools/list":
        start = int(params.get("cursor") or 0)
        page = {"tools": TOOLS[start:start + PAGE]}
        if start + PAGE < len(TOOLS):
            page["nextCursor"] = str(start + PAGE)
        result(id, page)
        continue
    if method != "tools/call":
        error(id, -32601, "Method not found")
        continue
    name = params.get("name")
    arguments = params.get("arguments") or {}
    if name == "echo":
        result(id, {"content": [{"type": "text", "text": "echo: " + arguments.get("text", "")}]})
    elif name == "env":
        lines = [n + "=" + os.environ.get(n, "<unset>") for n in arguments.get("names", [])]
        result(id, {"content": [{"type": "text", "text": "\n".join(lines)}]})
    elif name == "fail":
        result(id, {"content": [{"type": "text", "text": "bad input"}], "isError": True})
    elif name == "sleep":
        time.sleep(arguments.get("secs", 1))
        result(id, {"content": [{"type": "text", "text": "slept"}]})
    elif name == "media":
        result(id, {"content": [
            {"type": "text", "text": "first"},
            {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"},
            {"type": "resource_link", "uri": "file:///a.rs", "name": "a.rs"},
            {"type": "resource", "resource": {"uri": "file:///b.txt", "text": "inside b"}},
            {"type": "resource", "resource": {"uri": "file:///c.bin", "blob": "AAAA", "mimeType": "application/octet-stream"}},
        ]})
    elif name == "crash":
        os._exit(5)
    elif name == "spawn":
        # With `linger`, the server stays after its input closes.
        LINGER = arguments.get("linger", False)
        child = subprocess.Popen(["sleep", "300"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        result(id, {"content": [{"type": "text", "text": str(child.pid)}]})
    elif name == "big":
        result(id, {"content": [{"type": "text", "text": "가" * 40000}]})
    elif name == "ask":
        result(id, {"resultType": "input_required", "inputRequests": {}})
    else:
        error(id, -32602, "Unknown tool: " + str(name))

if LINGER:
    time.sleep(300)
