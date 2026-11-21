# Automation scripts

Four real programs written in Colibri, run with `colibri run`.
They use the scripting std (`args`, `fs`, `path`, `json`, `regex`, `datetime`, `http`, `process`,
`io`, `os`, `net`, `httpd`), coroutines, `Result`/`?`/`.context`, typed JSON decoding and exit codes.

| Script | Lines | What it does |
|--------|-------|--------------|
| [`organize/organize.colibri`](organize/organize.colibri) | 270 | sorts a folder into subfolders by extension category or modification date |
| [`report/report.colibri`](report/report.colibri) | 327 | fetches a JSON index and its items over HTTP or from files, writes a Markdown report |
| [`kvserver/kvserver.colibri`](kvserver/kvserver.colibri) | 131 | a key-value store served over TCP with a line protocol, one coroutine per connection |
| [`kvserver/kvclient.colibri`](kvserver/kvclient.colibri) | 31 | sends request lines to the key-value server and prints the answers (no `nc` needed) |
| [`notes/notes.colibri`](notes/notes.colibri) | 112 | a small JSON API served with `httpd`: a router, a path parameter, a typed JSON body, `fail`, a streamed export and a graceful stop |
| [`notes/notes_client.colibri`](notes/notes_client.colibri) | 55 | adds notes to the notes server with the `http` client, lists them and reads the streamed export |

Tests: `cargo test -p colibri-lang --test scripts` (`crates/colibri-cli/tests/scripts.rs`) runs
all three through the real `colibri` binary on a temporary tree and a local `127.0.0.1` HTTP server,
starts the key-value server and talks to it over a socket (with a Rust client and with `kvclient`), compares the report with [`report/expected.md`](report/expected.md) (`UPDATE_EXPECTED=1`
rewrites it) and runs `colibri check` on both. `notes` is started and called over a socket from
Rust, and checked and formatted too. Nothing touches the network.

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
                                         [--max-connections N]
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
are served at the same time; a coroutine that waits on `net` stops alone, and the waits of all
of them share one poller thread. `--idle SECONDS` is a deadline for each request line (`conn.set_timeout` counts from the
call): a client whose next line has not arrived whole within that time is closed, also when it
sent part of the line meanwhile. A connection waiting for a request holds a descriptor and no thread;
the server serves at most `--max-connections N` at once (default 256, to be raised together with
the process's descriptor limit); a connection over that is told `ERR too many connections` and
closed. A connection
leaves the server's table when it ends, and an `accept` that fails for another reason than the
listener being closed (no descriptor left, say) is reported on stderr and retried after a short
wait. Exit codes: 0 stopped by `SHUTDOWN`, 1 the address cannot be used, 2 bad command line.

It runs the same on Linux, macOS and Windows. To try it, start it in one terminal and talk to it
from another with the Colibri client, which needs nothing else installed:

```
$ colibri run kvserver/kvserver.colibri -- --addr 127.0.0.1:7000
listening on 127.0.0.1:7000
```

```
$ colibri run kvserver/kvclient.colibri -- 127.0.0.1:7000 "SET a 1" "GET a" SHUTDOWN
OK
VALUE 1
BYE
```

`kvclient.colibri -- HOST:PORT REQUEST...` sends each request as a line and prints the answer to
it; it exits 1 when the server cannot be reached or goes away. Any other TCP client speaks the
protocol too: `nc 127.0.0.1 7000` on Linux and macOS (type the requests, one per line), or
`telnet 127.0.0.1 7000` on a system that has it.

## notes

```sh
colibri run notes/notes.colibri -- [--addr 127.0.0.1:0]
```

A notes service over HTTP, written with the `httpd` module. The first line it prints is
`listening on HOST:PORT` (port 0 lets the system pick). The notes live in memory.

| Request | Answer |
|---------|--------|
| `GET /notes` | every note, as a JSON array |
| `POST /notes` with `{"text": "..."}` | 201, the note, and a `Location` header; 400 for a body that is not that, 422 for empty text |
| `GET /notes/export` | every note, one JSON object per line, streamed in chunks as it is written |
| `GET /notes/:id` | one note, or 404 |
| `DELETE /notes/:id` | 204, or 404 |
| `POST /shutdown` | `stopping`, then the server stops: busy connections finish, then the program exits 0 |

What it shows: `httpd.Router` with a path parameter (`:id`, where the literal `export` wins over
it), the body decoded with `json.decode[NewNote]`, `httpd.fail(404, ..)` as the way a handler
says "this is an error with a status", `httpd.stream` for a response written while it is sent,
and `server.stop`, which never waits and so can be called from a handler. The router answers
405 with an `Allow` header for a method the path does not have. Handlers run in coroutines of one
thread and stop only where they wait, so the store needs no lock. Exit codes: 0 stopped
by `/shutdown`, 1 the address cannot be used.

`notes/notes_client.colibri -- HOST:PORT TEXT...` is a client that uses the `http` module: it adds a
note for each TEXT, prints the count the server lists and the lines of the streamed export (exit 1
when the server cannot be reached or answers an error, 2 for a bad command line).

```
$ colibri run notes/notes.colibri -- --addr 127.0.0.1:7001
listening on 127.0.0.1:7001
```

```
$ curl -i -d '{"text":"buy milk"}' http://127.0.0.1:7001/notes
HTTP/1.1 201 Created
Content-Type: application/json
Location: /notes/1
Content-Length: 26
Date: Sat, 21 Nov 2026 00:42:09 GMT

{"id":1,"text":"buy milk"}
$ curl http://127.0.0.1:7001/notes/export
{"id":1,"text":"buy milk"}
$ curl -X POST http://127.0.0.1:7001/shutdown
stopping
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
