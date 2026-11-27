#!/usr/bin/env bash
# Runs every benchmark with Colibri and with CPython, checks that both print the same
# result, and reports the best of three wall-clock times in milliseconds.
#
#   benchmarks/run.sh [path to the colibri binary]
#
# Build the binary first with `cargo build --release`.
set -euo pipefail
cd "$(dirname "$0")"
COLIBRI="${1:-../target/release/colibri}"
PYTHON="${PYTHON:-python3}"

best_of_three() {
    local best=""
    for _ in 1 2 3; do
        local start end ms
        start=$(date +%s%N)
        "$@" >/dev/null
        end=$(date +%s%N)
        ms=$(((end - start) / 1000000))
        if [ -z "$best" ] || [ "$ms" -lt "$best" ]; then best=$ms; fi
    done
    echo "$best"
}

printf '%-10s %12s %12s %8s\n' benchmark "colibri ms" "cpython ms" ratio
status=0
for src in *.colibri; do
    name="${src%.colibri}"
    # `tasks` has no Python version: `tasks.sh` measures it.
    [ -f "$name.py" ] || continue
    got=$("$COLIBRI" run "$src")
    want=$("$PYTHON" "$name.py")
    if [ "$got" != "$want" ]; then
        echo "$name: results differ (colibri: $got, cpython: $want)" >&2
        status=1
        continue
    fi
    c=$(best_of_three "$COLIBRI" run "$src")
    p=$(best_of_three "$PYTHON" "$name.py")
    ratio=$(awk -v c="$c" -v p="$p" 'BEGIN { if (p > 0) printf "%.1fx", c / p; else print "-" }')
    printf '%-10s %12s %12s %8s\n' "$name" "$c" "$p" "$ratio"
done
exit $status
