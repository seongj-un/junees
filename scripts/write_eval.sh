#!/bin/sh
# Compares writing in one call with `junees write` on the real API, for the
# same three requests: time, usage (on standard error) and the texts, side
# by side, to read and judge by hand. It spends API credit: about three
# one-shot answers and three drafts of 12 to 30 calls each.
#
#   ANTHROPIC_API_KEY=... scripts/write_eval.sh [path/to/junees] [out-dir]
#
# Not run by the tests or CI.
set -eu
BIN=$(realpath "${1:-target/debug/junees}")
OUT=${2:-write-eval-$(date +%Y%m%d-%H%M%S)}
if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
    echo "set ANTHROPIC_API_KEY first: this calls the real API" >&2
    exit 2
fi
mkdir -p "$OUT"
i=0
while IFS= read -r request; do
    i=$((i + 1))
    for mode in one-shot write; do
        start=$(date +%s)
        if [ "$mode" = one-shot ]; then
            "$BIN" "$request Answer with the whole document only." \
                >"$OUT/$i-$mode.md" 2>"$OUT/$i-$mode.err" </dev/null
        else
            "$BIN" write "$request" >"$OUT/$i-$mode.md" 2>"$OUT/$i-$mode.err" </dev/null
        fi
        echo "$i $mode $(($(date +%s) - start))s $(wc -w <"$OUT/$i-$mode.md") words"
    done
done <<'REQUESTS'
에이전트 하네스에 프롬프트 캐시를 도입하는 설계 문서를 써 줘. 배경, 목표, 설계, 비용, 위험, 대안을 다뤄 줘.
Write a README for a small Rust CLI that syncs a folder to S3: install, usage, configuration, troubleshooting.
신입 백엔드 개발자를 위한 PostgreSQL 인덱스 입문 보고서를 써 줘. 예시 쿼리와 흔한 실수를 포함해 줘.
REQUESTS
echo "texts and usage lines: $OUT"
