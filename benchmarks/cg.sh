#!/usr/bin/env bash
# Prints the instructions executed (cachegrind `I refs`) by each benchmark on both engines,
# a stable number where wall time is noisy. Needs valgrind.
#
#   benchmarks/cg.sh [path to the colibri binary] [benchmark names...]
#
# Build the binary first with `cargo build --release`. The default benchmarks are the ones
# the bytecode VM can run.
set -euo pipefail
cd "$(dirname "$0")"
COLIBRI="${1:-../target/release/colibri}"
shift || true
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
    local engine="$1" file="$2"
    COLIBRI_ENGINE="$engine" valgrind --tool=cachegrind --cache-sim=no \
        --cachegrind-out-file="$out/cg.out" "$COLIBRI" run "$file" 2>&1 >/dev/null |
        sed -n 's/.*I *refs: *//p' | tr -d ','
}

printf '%-10s %16s %16s %8s\n' benchmark "tree Ir" "vm Ir" "vm/tree"
for name in "${NAMES[@]}"; do
    src="$name.colibri"
    tree=$(ir tree "$src")
    vm=$(ir vm "$src")
    ratio=$(awk -v a="$vm" -v b="$tree" 'BEGIN { printf "%.2f", a / b }')
    printf '%-10s %16s %16s %8s\n' "$name" "$tree" "$vm" "$ratio"
done
