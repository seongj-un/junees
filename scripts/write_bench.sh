#!/bin/sh
# Times writing one text in one call against `junees write` (levels 3,
# parallel 4 and 8) on a loopback fake API whose replies take a fixed time
# per output token (scripts/write_bench_server.py). No real API is called.
#
#   scripts/write_bench.sh [path/to/junees]
#
# PORT (18123), SECTIONS, BODY_TOKENS, GIST_TOKENS, PER_TOKEN and TTFT are
# passed to the fake. Results go to standard output; the run's files stay
# in the folder printed at the end.
set -eu
BIN=$(realpath "${1:-target/debug/junees}")
HERE=$(dirname "$(realpath "$0")")
PORT=${PORT:-18123}
DIR=$(mktemp -d "${TMPDIR:-/tmp}/write-bench.XXXXXX")
LOG="$DIR/requests.jsonl"
export PORT LOG
python3 "$HERE/write_bench_server.py" &
SERVER=$!
trap 'kill "$SERVER" 2>/dev/null' EXIT INT TERM
mkdir -p "$DIR/home/work"
python3 - "$PORT" <<'EOF'
import socket, sys, time
for _ in range(50):
    try:
        socket.create_connection(("127.0.0.1", int(sys.argv[1])), 0.2).close()
        break
    except OSError:
        time.sleep(0.1)
EOF

now() { python3 -c 'import time; print(time.time())'; }

run() {
    label=$1
    shift
    : > "$LOG"
    start=$(now)
    (cd "$DIR/home/work" && env -i PATH="$PATH" HOME="$DIR/home" \
        JUNEES_HOME="$DIR/home/.junees" ANTHROPIC_API_KEY=sk-fake \
        ANTHROPIC_BASE_URL="http://127.0.0.1:$PORT" JUNEES_STREAM=0 \
        "$BIN" "$@") >"$DIR/$label.out" 2>"$DIR/$label.err" </dev/null
    end=$(now)
    cp "$LOG" "$DIR/$label.requests.jsonl"
    python3 - "$label" "$start" "$end" "$DIR/$label.requests.jsonl" "$DIR/$label.out" <<'EOF'
import json, sys
label, start, end, log, out = sys.argv[1], float(sys.argv[2]), float(sys.argv[3]), sys.argv[4], sys.argv[5]
calls = [json.loads(line) for line in open(log)]
busy = sum(c["end"] - c["start"] for c in calls)
peak, events = 0, sorted([(c["start"], 1) for c in calls] + [(c["end"], -1) for c in calls])
now = 0
for _, step in events:
    now += step
    peak = max(peak, now)
words = len(open(out).read().split())
print(f"{label:<10} {end - start:6.1f}s  calls {len(calls):3d}  peak {peak:2d} at once  "
      f"output tokens {sum(c['tokens'] for c in calls):6d}  call time {busy:6.1f}s  "
      f"words out {words}")
EOF
}

REQUEST="Write a design document about prompt caching for an agent harness."
run one-shot "$REQUEST"
run write-p4 write --parallel 4 "$REQUEST"
run write-p8 write --parallel 8 "$REQUEST"
echo "files: $DIR"
