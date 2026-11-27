#!/usr/bin/env bash
# Measures the speed-up of thread tasks: runs `tasks.colibri` as one task and as one task for each
# core, checks that both print the same, and reports the best of three wall-clock times in
# milliseconds. The target is 3x or more on 4 cores (release build). Not part of CI.
#
#   benchmarks/tasks.sh [path to the colibri binary] [parts]
#
# Build the binary first with `cargo build --release`. Other load on the machine hides the
# speed-up: run it on a quiet one.
set -euo pipefail
cd "$(dirname "$0")"
COLIBRI="${1:-../target/release/colibri}"
CORES="$(nproc 2>/dev/null || sysctl -n hw.ncpu)"
PARTS="${2:-$CORES}"

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

one=$("$COLIBRI" run tasks.colibri 1)
many=$("$COLIBRI" run tasks.colibri "$PARTS")
if [ "$one" != "$many" ]; then
    echo "results differ (1 task: $one, $PARTS tasks: $many)" >&2
    exit 1
fi
echo "$one"
t1=$(best_of_three "$COLIBRI" run tasks.colibri 1)
tn=$(best_of_three "$COLIBRI" run tasks.colibri "$PARTS")
awk -v t1="$t1" -v tn="$tn" -v n="$PARTS" 'BEGIN { printf "1 task: %d ms, %d tasks: %d ms, speed-up %.2fx\n", t1, n, tn, t1 / tn }'
