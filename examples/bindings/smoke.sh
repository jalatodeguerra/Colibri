#!/usr/bin/env bash
# The phase 4 finish line, as one script: what the `bindings` job of CI runs on each platform, and
# what anyone can run locally. Needs a built `colibri` (the path is the first argument), Rust,
# and COLIBRI_SOURCE naming a checkout of Colibri (the project runner is built against it).
#
#   bash examples/bindings/smoke.sh target/release/colibri
#
# EXE is the suffix of executables on this system (`.exe` on Windows).
set -euo pipefail

COLIBRI=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
EXE=${EXE:-}
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
WORK=$(mktemp -d)
WATCH_PID=
# Ends the watching `colibri` and checks that its runner went with it. Bounded: a process that does
# not go is an error, not a wait for the job's time limit.
stop_watch() {
  [ -n "$WATCH_PID" ] || return 0
  local pid=$WATCH_PID i
  WATCH_PID=
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
      # Only the supervisor is ended (no `/T`): its job object is what must end the runner.
      taskkill //F //PID "$(cat "/proc/$pid/winpid")" > /dev/null || true ;;
    *) kill "$pid" || true ;;
  esac
  for i in $(seq 1 300); do
    ps -p "$pid" > /dev/null || break
    sleep 0.1
  done
  if ps -p "$pid" > /dev/null; then echo "the watching colibri ($pid) did not end" >&2; return 1; fi
  wait "$pid" || true
  for i in $(seq 1 300); do
    runner_running || return 0
    sleep 0.1
  done
  echo "a project runner outlived its supervisor" >&2
  return 1
}
runner_running() {
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) tasklist //FI "IMAGENAME eq runner-*" | grep -qi 'runner-' ;;
    *) pgrep -f '/runners/[0-9a-f]+/runner-' > /dev/null ;;
  esac
}
# What an interrupted run leaves is put back: the watched file, and the temporary file of an edit.
cleanup() {
  stop_watch || true
  if [ -f "$WORK/ticker.original" ]; then cp "$WORK/ticker.original" "$HERE/ticker.colibri"; fi
  rm -f "$HERE/ticker.colibri.saving"
  rm -rf "$WORK"
}
trap cleanup EXIT
LIMIT=15000000
cd "$HERE"

step() { printf '\n== %s\n' "$*"; }
# Waits (up to 180 s) until `grep -q PATTERN FILE` holds.
wait_for() {
  local pattern=$1 file=$2 i
  for i in $(seq 1 1800); do
    if grep -q -- "$pattern" "$file"; then return 0; fi
    sleep 0.1
  done
  echo "timed out waiting for '$pattern' in $file:" >&2
  cat "$file" >&2
  return 1
}

step "colibri bindings --check (builds the runner the first time)"
start=$(date +%s)
"$COLIBRI" bindings --check
echo "runner ready after $(( $(date +%s) - start )) s"

step "colibri fmt --check"
"$COLIBRI" fmt --check .

step "colibri run, against expected.txt"
"$COLIBRI" run . > "$WORK/run.txt"
diff -u --strip-trailing-cr expected.txt "$WORK/run.txt"

step "colibri test"
"$COLIBRI" test .

step "a Colibri edit reloads under run --watch"
"$COLIBRI" run --watch ticker.colibri > "$WORK/watch.txt" 2>&1 &
WATCH_PID=$!
wait_for '^one 4 ' "$WORK/watch.txt"
# An edit the way an editor saves: whole, by a rename in the same folder (one step on every
# system).
cp ticker.colibri "$WORK/ticker.original"
sed 's/=> "one"/=> "second"/' ticker.colibri > ticker.colibri.saving
mv ticker.colibri.saving ticker.colibri
wait_for '^second 4 ' "$WORK/watch.txt"
cp "$WORK/ticker.original" ticker.colibri
stop_watch

step "colibri build, and the built program"
start=$(date +%s)
"$COLIBRI" build . -o "$WORK/demo$EXE"
echo "built after $(( $(date +%s) - start )) s"
# Elsewhere, with no cargo at all: the built program needs nothing.
(cd "$WORK" && CARGO=colibri-no-such-cargo COLIBRI_HOME="$WORK/no-home" "./demo$EXE" > built.txt)
diff -u --strip-trailing-cr expected.txt "$WORK/built.txt"

step "size gate"
size=$(wc -c < "$WORK/demo$EXE" | tr -d ' ')
echo "the built demo: $size bytes (limit $LIMIT)"
test "$size" -le "$LIMIT"

step "a fresh clone checks without cargo"
rm -rf "$WORK/clone" && mkdir -p "$WORK/clone"
(cd "$HERE/../.." && git archive HEAD examples/bindings) | tar -x -C "$WORK/clone"
(cd "$WORK/clone/examples/bindings" && CARGO=colibri-no-such-cargo COLIBRI_HOME="$WORK/clone-home" "$COLIBRI" check .)
test ! -e "$WORK/clone-home/runners"

printf '\nall steps passed\n'
