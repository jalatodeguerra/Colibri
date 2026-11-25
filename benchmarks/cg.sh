#!/usr/bin/env bash
# Prints the instructions executed (cachegrind `I refs`) by each benchmark, a stable number
# where wall time is noisy. Needs valgrind.
#
#   benchmarks/cg.sh [path to the colibri binary] [benchmark names...]
#
# Build the binary first with `cargo build --release`.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# A binary given as a relative path is relative to where the script was run from.
COLIBRI="${1:-$here/../target/release/colibri}"
case "$COLIBRI" in /*) ;; *) COLIBRI="$PWD/$COLIBRI" ;; esac
shift || true
cd "$here"
NAMES=("$@")
if [ ${#NAMES[@]} -eq 0 ]; then NAMES=(fib loop); fi

if ! command -v valgrind >/dev/null; then
    echo "cg.sh needs valgrind" >&2
    exit 1
fi

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

# `I refs` of one run, as a plain number.
ir() {
    local file="$1"
    valgrind --tool=cachegrind --cache-sim=no \
        --cachegrind-out-file="$out/cg.out" "$COLIBRI" run "$file" 2>&1 >/dev/null |
        sed -n 's/.*I *refs: *//p' | tr -d ','
}

status=0
printf '%-10s %16s\n' benchmark Ir
for name in "${NAMES[@]}"; do
    src="$name.colibri"
    count=$(ir "$src") || count=""
    if [ -z "$count" ]; then
        printf '%-10s %16s\n' "$name" failed
        status=1
        continue
    fi
    printf '%-10s %16s\n' "$name" "$count"
done
exit $status
