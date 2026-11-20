# Automation scripts

Three real programs written in Colibri, run with `colibri run`.
They use the scripting std (`args`, `fs`, `path`, `json`, `regex`, `datetime`, `http`, `process`,
`io`, `os`, `net`), coroutines, `Result`/`?`/`.context`, typed JSON decoding and exit codes.

| Script | Lines | What it does |
|--------|-------|--------------|
| [`organize/organize.colibri`](organize/organize.colibri) | 238 | sorts a folder into subfolders by extension category or modification date |
| [`report/report.colibri`](report/report.colibri) | 329 | fetches a JSON index and its items over HTTP or from files, writes a Markdown report |
| [`kvserver/kvserver.colibri`](kvserver/kvserver.colibri) | 132 | a key-value store served over TCP with a line protocol, one coroutine per connection |

Tests: `cargo test -p colibri-lang --test scripts` (`crates/colibri-cli/tests/scripts.rs`) runs
all three through the real `colibri` binary on a temporary tree and a local `127.0.0.1` HTTP server,
starts the key-value server and talks to it over a socket, compares the report with [`report/expected.md`](report/expected.md) (`UPDATE_EXPECTED=1`
rewrites it) and runs `colibri check` on both. Nothing touches the network.

## organize

```sh
colibri run organize/organize.colibri -- <dir> [--dry-run] [--by ext|date]
                                         [--rules rules.json] [--match <regex>]
```

- `--by ext` (default): `images`, `documents`, `archives`, `code`, `other`, from the extension
  (case-insensitive, last extension only, so `a.tar.gz` is `gz`). `--rules` replaces the
  categories with a JSON object decoded as `Map[str, List[str]]`
  ([`rules.example.json`](organize/rules.example.json)); unlisted extensions go to `other`.
- `--by date`: `<dir>/<year>/<month>/` from the modification time (`fs.stat(path).modified`, UTC).
- The folder is walked recursively; hidden files are skipped; files already in their target
  folder stay (so a second run prints `nothing to do`). Emptied folders are left behind.
- A taken name becomes `name (1).ext`, `name (2).ext`; nothing is overwritten. The plan is made
  before anything moves, so collisions are decided in name order.
- `--match <regex>` only touches files whose name matches (`regex`).
- `--dry-run` prints the plan and the summary and changes nothing.
- Exit codes: 0 done, 1 an operation or the rules file failed (the error chain goes to stderr),
  2 bad command line (usage on stderr).

```
$ colibri run organize/organize.colibri -- ~/Downloads --dry-run
README -> other/README
SHOT.PNG -> images/SHOT.PNG
backup.tar.gz -> archives/backup.tar.gz
main.rs -> code/main.rs
notes.txt -> documents/notes.txt
old/photo (1).jpg -> images/photo (1).jpg
old/photo.jpg -> images/photo.jpg
paper.pdf -> documents/paper.pdf
photo.jpg -> images/photo (2).jpg
weird.xyz -> other/weird.xyz
dry run: nothing was moved
folder         files     bytes
archives           1        13
code               1         7
documents          2        18
images             4        28
other              2        15
total             10        81
```

## kvserver

```sh
colibri run kvserver/kvserver.colibri -- [--addr 127.0.0.1:0] [--idle SECONDS]
```

A key-value store that speaks a line protocol over TCP, written with `net`. The first line it
prints is `listening on HOST:PORT` (port 0 lets the system pick). One request per line, `\n` or
`\r\n`, one answer per line:

| Request | Answer |
|---------|--------|
| `SET key value` | `OK` (the value is the rest of the line) |
| `GET key` | `VALUE value` or `MISSING` |
| `DEL key` | `DELETED` or `MISSING` |
| `COUNT` | the number of keys |
| `KEYS` | `KEYS a b c` (sorted) |
| `QUIT` | `BYE`, and that connection is closed |
| `SHUTDOWN` | `BYE`, and the server stops, closing every connection |

`main` is an accept loop that starts one coroutine per connection (`go serve(..)`), so clients
are served at the same time; `net` waits on background threads and only the waiting coroutine
stops. `--idle` closes a connection that is silent for that long (`conn.set_timeout`). A connection
leaves the server's table when it ends, and an `accept` that fails for another reason than the
listener being closed (no descriptor left, say) is reported on stderr and retried after a short
wait. Exit codes: 0 stopped by `SHUTDOWN`, 1 the address cannot be used, 2 bad command line.

```
$ colibri run kvserver/kvserver.colibri -- --addr 127.0.0.1:7000 &
listening on 127.0.0.1:7000
$ printf 'SET a 1\nGET a\nSHUTDOWN\n' | nc 127.0.0.1 7000
OK
VALUE 1
BYE
```

## report

```sh
colibri run report/report.colibri -- <url-or-file> [--out report.md] [--concurrency N]
                                     [--top N] [--now <iso time>] [--git]
```

The index is `{"title": "...", "items": ["north.json", "https://host/other.json"]}`; relative
items are resolved against the index's URL or folder (see [`report/fixtures`](report/fixtures)).
Each item is `{"region": "north", "orders": [{"id": 1, "customer": "ann", "category": "books",
"total": 12.5}]}`, decoded with `json.decode[Batch]`. The report has a summary (count, revenue,
average, largest order), tables by region and by category (count, sum, average or share) and the
top-N customers.

- `http://` and `https://` locations go through `http.get` (a status other than 200 is an
  error), everything else through `fs.read`.
- `--concurrency N` (default 2) starts N worker coroutines in a `CoGroup`; each takes the next
  item from a shared queue, and `main` waits with `group.join_all()`. **The concurrency is
  real**: `http.get` runs the request on a background thread and suspends only the calling
  coroutine (ARCHITECTURE section 22), so `--concurrency 4` keeps four requests in flight at
  once (files are read in order, since `fs` does not wait).
- `--now` fixes the time stamp (ISO 8601); the default is `datetime.now()`. `--git` adds
  ``commit `abc1234` `` from `process.output("git", ["rev-parse", "--short", "HEAD"])` and does
  nothing outside a repository.
- A failing item does not stop the others: the report is still written, lists the failures,
  and the exit code is 1. An unreadable index is exit 1 with no report; bad flags are exit 2.
  Without `--out` the report goes to stdout.

```
$ colibri run report/report.colibri -- report/fixtures/index.json --now 2024-05-01T12:00:00Z
# Orders, spring

Generated 2024-05-01T12:00:00Z

Sources: 3 read, 0 failed.

## Summary

| Metric | Value |
|---|---|
| Orders | 11 |
| Revenue | 329.98 |
...
```
