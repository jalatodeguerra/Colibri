# Colibri

**A small, statically typed language with the feel of a scripting language.**

Colibri is a general-purpose programming language written in Rust. It reads like Go or Python
with braces, catches type errors before your program runs, and manages memory with plain
reference counting instead of a garbage collector. Use it for scripts and command-line tools,
for automation, or as a scripting language embedded in a Rust program.

> **Status: early and pre-release.** The language, standard library and tooling described below
> work today on a register bytecode VM. Expect breaking changes. Features that are planned but
> not built are marked **(planned)**.

```colibri
import fs

struct WordCount {
    word: str
    count: int
}

fn count_words(path: str) -> Result[List[WordCount]] {
    let text = fs.read(path).context("reading {path}")?
    let mut counts = Map[str, int]()
    for word in text.to_lower().split(" ") {
        let w = word.trim()
        if w != "" {
            counts[w] = counts.get_or(w, 0) + 1
        }
    }
    let mut words = counts.entries().map(fn(e) => WordCount { word: e.0, count: e.1 })
    words.sort_by_key(fn(w) => -w.count)
    return ok(words)
}

fn main() {
    let words = count_words("notes.txt") or |e| {
        print("error: {e}")
        return
    }
    for w in words.filter(fn(w) => w.count > 1) {
        print("{w.word}: {w.count}")
    }
}
```

Save it as `words.colibri` and run it:

```console
$ echo "the cat and the dog and the bird" > notes.txt
$ colibri run words.colibri
the: 3
and: 2
$ rm notes.txt && colibri run words.colibri
error: reading notes.txt: cannot read `notes.txt`: no such file or directory
```

## Learn Colibri

- [`guide/tutorial.md`](guide/tutorial.md): a hands-on tutorial, from installing to tests,
  coroutines and a small HTTP service.
- [`guide/tour.md`](guide/tour.md): the whole language in short sections, for programmers who
  know others.
- [`guide/builtins.md`](guide/builtins.md): the types and functions that need no `import`.
- [`guide/std`](guide/std/README.md): the standard library reference, one page for each module.

Every sample in the guide is checked, formatted and, where it shows output, run by the test suite.

## Highlights

- **Simple syntax.** Braces, no semicolons, string interpolation (`"{name}"`), a small set of
  keywords and one obvious way to do common things.
- **Static types with inference.** Function signatures are annotated; inside function bodies
  types are inferred. The checker reports several errors at once, with source spans and hints.
- **Errors are values.** `Result[T]`, `?` to propagate, `or` for a fallback or a handler block,
  `.context(...)` to add a message, `match` to inspect. No exceptions and no null (`T?` is an
  option type).
- **Values and references.** `struct`s are value types (assignment copies), `class`es are
  reference counted and shared, and `weak` references break cycles. Memory is freed as soon as
  the last reference goes away, in a predictable order.
- **Generics and interfaces:** `List[T]`, `fn largest[T: Ord](...)`, bounds on the parameters of
  types too (`struct Sorted[T: Ord]`), interfaces with default methods, enums with data and
  exhaustive `match`. Structs and enums with data can be compared and sorted without writing
  any code.
- **Coroutines without colored functions.** `go f()` starts a coroutine and `wait(secs)`,
  `join()`, groups and signals suspend it. There is no `async`/`await`, so any function can wait.
- **Batteries for scripting.** 22 standard library modules, including files (whole or streamed
  through a handle) and paths, JSON, TOML and CSV, regular expressions, subprocesses,
  command-line arguments, dates, an HTTP client, TCP sockets and an HTTP server, and `Bytes` for
  data that is not text.
- **Conditional code.** `@when(os == "windows")`, `@when(debug)` and feature flags from
  `colibri.toml` decide which items exist; `@deprecated` makes a use of an old item warn.
- **One binary for the whole workflow:** run, check, test, format, a REPL, a language server,
  hot reload and a leak detector.
- **Embeddable in Rust.** Load scripts, call their functions with typed arguments, and expose
  Rust functions and types to them through the `colibri-runtime` crate.
- **C FFI** with `@repr(C)` structs passed by value.

## A short tour

### Structs, classes and enums

```colibri
// Structs are values: assigning one makes a copy.
struct Point {
    x: float
    y: float
}

// Classes are reference counted and shared.
class Counter {
    n: int = 0
}

impl Counter {
    fn bump(self) {
        self.n += 1
    }
}

enum Shape {
    Circle(r: float)
    Rect(w: float, h: float)
}

fn area(s: Shape) -> float {
    return match s {
        Circle(r)  => 3.14159 * r * r
        Rect(w, h) => w * h
    }
}

fn main() {
    let mut a = Point { x: 1, y: 2 }
    let mut b = a
    b.x = 10
    print("{a.x} {b.x}") // 1.0 10.0

    let c = Counter {}
    let same = c
    same.bump()
    print(c.n) // 1

    print(area(.Rect(w: 2, h: 3))) // 6.0
}
```

### Generics and interfaces

```colibri
interface Describe {
    fn describe(self) -> str
}

struct User {
    name: str
    admin: bool = false
}

impl Describe for User {
    fn describe(self) -> str {
        let role = if self.admin { "admin" } else { "user" }
        return "{self.name} ({role})"
    }
}

fn largest[T: Ord](items: List[T]) -> T? {
    let mut best = items.first()?
    for x in items {
        if x > best {
            best = x
        }
    }
    return some(best)
}

fn print_all[T: Describe](items: List[T]) {
    for item in items {
        print(item.describe())
    }
}

fn main() {
    print(largest([3, 9, 4]) or 0) // 9
    print(largest(["pear", "apple"]) or "") // pear
    print_all([User { name: "ana", admin: true }, User { name: "bo" }])
}
```

### Operators on your own types

`+ - * /` and unary `-` on a type of your own are the interfaces `Add`, `Sub`, `Mul`, `Div` and
`Neg`. `Add[R, O]` has a right-hand type `R` and a result `O`, and both default to `Self`, so
`impl Add for Money` means `Add[Money, Money]`. A type can implement an operator once for each
right-hand type, told apart by the type's head only (`Add[Box[int]]` and `Add[Box[str]]` count as
one). A number can be the left operand of your type when the right-hand type is yours (comparison
stays on `Eq` and `Ord`). A type of the standard library (`vec.vec2`, `datetime.DateTime`) is the library's: you
can give it an operator only with a right-hand type of your own, and no other built-in interface.
A narrow integer type (`i8` to `u32`) cannot yet stand for a type
parameter that has an operator bound:

```colibri
struct Money { cents: int }

impl Add for Money {
    fn add(self, other: Money) -> Money => Money { cents: self.cents + other.cents }
}
impl Mul[int] for Money {
    fn mul(self, k: int) -> Money => Money { cents: self.cents * k }
}
impl Mul[Money, Money] for int {
    fn mul(self, m: Money) -> Money => m * self
}

fn total[T: Add](xs: List[T], zero: T) -> T {
    let mut sum = zero
    for x in xs {
        sum += x
    }
    return sum
}

fn main() {
    let price = Money { cents: 250 }
    print((price * 3 + 2 * price).cents) // 1250
    print(total([price, price], Money { cents: 0 }).cents) // 500
}
```

`a += b` is `a = a + b`. `%` and the bit operators are for numbers only. The `vec` module
(`import vec`) is built this way.

For code written against the vectors that used to be built in, three things change. The vector
types need `import vec` (`vec.vec3(..)`, or `import vec.{vec3}`). Swizzle fields are methods or
constructor calls, and the splat and conversion constructors have other names. And an operator
that has more than one impl for the left type (`v * k`, where `vec2` takes a `vec2` or a
`float`) must know the type of its right operand where it is written, so a list that is filled
only later needs its type spelled out:

```colibri
import vec

fn main() {
    let v = vec.vec3(1.0, 2.0, 3.0)
    print(v.xy()) // not v.xy: a method
    print(vec.vec3(v.z, v.y, v.x)) // not v.zyx
    print(vec.vec3(7.0, 7.0, 7.0)) // not vec3(7)
    print(vec.ivec3(1, 2, 3).to_vec()) // not vec3(iv); and v.to_ivec() the other way
    let mut ks: List[float] = [] // not `let mut ks = []`: its type decides which `*` is meant
    let mut w = v
    for i in 0..3 {
        if ks.len() > 0 {
            w = w * ks[0]
        }
        ks.push(2.0)
    }
    print(w)
}
```

### Ordering and bounds

A struct, or an enum whose variants carry data, is ordered by its parts when every part can be
ordered: structs field by field in the order they are declared, enums by variant (in declaration
order) and then by what the variant holds. Classes are not ordered unless they implement `Ord`.
A type can ask for bounds on its parameters, as a function does:

```colibri
struct Version {
    major: int
    minor: int
}

enum Level {
    Low
    Mid(n: int)
    High
}

class Sorted[T: Ord] {
    items: List[T] = []
}

impl[T] Sorted[T] {
    fn add(self, x: T) {
        self.items.push(x)
        self.items.sort()
    }
}

fn main() {
    let mut versions = [Version { major: 1, minor: 10 }, Version { major: 1, minor: 2 }]
    versions.sort()
    print(versions) // [Version { major: 1, minor: 2 }, Version { major: 1, minor: 10 }]
    print(Level.Mid(5) < Level.Mid(7)) // true
    print(Level.High > Level.Mid(100)) // true

    let numbers = Sorted[int] {}
    numbers.add(3)
    numbers.add(1)
    print(numbers.items) // [1, 3]
}
```

`Sorted[Version]` is fine and `Sorted[SomeClass]` is an error where it is written, unless
`SomeClass` implements `Ord`. A type alias takes the bounds of the type it names (`type S[T] =
Sorted[T]` needs `T: Ord` as well), so it does not write them.

### Coroutines

```colibri
fn fetch(name: str, secs: float) -> str {
    wait(secs) // suspends this coroutine, not the program
    return "{name} done"
}

fn main() {
    let slow = go fetch("slow", 0.2)
    let fast = go fetch("fast", 0.1)
    print(fast.join()) // fast done
    print(slow.join()) // slow done
}
```

Both coroutines wait at the same time, so the program takes 0.2 seconds, not 0.3.
`slow.join_result()` is `join` that returns a `Result`: a panic or a cancel of the coroutine arrives
as `err(e)` instead of panicking in the joiner, but only if `join_result` is already waiting for it
when it panics, or the coroutine panicked before its first pause and `join_result` is called before
the caller pauses again. A panic nobody has received ends the program. That is how a server survives
a handler's panic: it waits for each handler at once.

### Bytes and files

`Bytes` is an immutable string of bytes, the type for data that is not text. It needs no import.
A `Bytes` compares, hashes and can be a `Map` key like a `str`; printing one shows it in the Rust
form. `BytesBuilder` builds one a piece at a time.

```colibri
fn main() {
    let data = "héllo".to_bytes()
    print(data) // b"h\xc3\xa9llo"
    print(data.len()) // 6
    print(data[1..3]) // b"\xc3\xa9"
    print(data.to_str() or "not text") // héllo

    let packet = BytesBuilder()
    packet.push(0xCA)
    packet.push(0xFE)
    packet.push_str("hi")
    let bytes = packet.to_bytes()
    print(bytes) // b"\xca\xfehi"
    print(bytes.starts_with(Bytes([0xCA, 0xFE]))) // true
    print(bytes.to_str().is_err()) // true: not valid UTF-8
}
```

`fs.read`, `fs.write` and `fs.append` move a whole file as text, and `fs.read_bytes`,
`fs.write_bytes` and `fs.append_bytes` as `Bytes`. For a file too big to hold in memory, or a
log that grows a line at a time, `fs.open`, `fs.create` and `fs.open_append` give an `fs.File`
that reads or writes through a buffer:

```colibri
import fs

fn count_errors(path: str) -> Result[int] {
    let f = fs.open(path)?
    defer f.close()
    let mut errors = 0
    while true {
        match f.read_line()? {
            some(line) => {
                if line.contains("ERROR") {
                    errors += 1
                }
            }
            none => break
        }
    }
    return ok(errors)
}

fn main() {
    let log = fs.create("app.log") or |e| {
        print("error: {e}")
        return
    }
    log.write("ok\nERROR disk\nok\nERROR net\n") or |e| { print("error: {e}") }
    log.close() or |e| { print("error: {e}") }
    print(count_errors("app.log") or 0) // 2
}
```

A handle reads or writes, not both. `read_line`, `read(n)`, `read_all`, `write`, `write_bytes`,
`flush`, `seek`, `position` and `close` are its methods. A call on a handle waits for the disk on
a background thread, so other coroutines keep running (the whole-file functions above run to
their end first), and the operations on one handle happen in the order they were called, even
from different coroutines. A handle that goes out of scope is flushed and closed; call
`close()` (best with `defer`) to see an error from that. Reading or writing a folder as a file
is `fs.Error.IsADirectory` on every system.

### Sockets

`net` is TCP over IPv4 and IPv6. An address is always `"host:port"`; port 0 asks the system for a
free port, and `listener.addr()` says which one it picked. This script is a server and its client
in one program, over the loopback address:

```colibri
import net

// The server side: answer each line a client sends with a greeting.
fn greet(conn: net.Conn) {
    defer conn.close()
    while true {
        let line = conn.read_line() or |e| { return }
        let name = line or { return }
        conn.write("hello, {name}!\n") or |e| { return }
    }
}

fn serve(listener: net.Listener) {
    while true {
        let conn = listener.accept() or |e| { return }
        go greet(conn)
    }
}

fn main() -> Result[()] {
    let listener = net.listen("127.0.0.1:0")? // port 0: the system picks a free port
    let server = go serve(listener)

    let conn = net.connect(listener.addr())?
    conn.write("Colibri\n")?
    print(conn.read_line()?) // some("hello, Colibri!")
    conn.close()

    listener.close() // ends the accept loop
    server.join()
    return ok(())
}
```

```console
$ colibri run greet.colibri
some("hello, Colibri!")
```

`accept`, the reads, the writes and `connect` suspend the coroutine that calls them while they
wait, so the other coroutines keep running. One poller thread per program waits for every
connection and listener at once, so a connection or a listener that waits costs no thread and
no wake-ups, only its descriptor and its coroutine (and a read that waits with nothing buffered
holds no buffer; one that holds a half line or the start of a `read_exact` keeps those bytes and
the room made for them): measured by
hand in a release build on Linux with the peer in another process, 10,000 idle blocked reads are
3 threads in all, no CPU time over ten seconds and 117 MB of resident memory (a thousand idle
accepts and a thousand writes blocked on full buffers are tested to cost a few threads and no CPU
time; on macOS the test blocks fewer writes, 192 on the machine that runs it, because the kernel
there keeps unsent data in a pool of fixed size, 96 MiB on that machine, and a thousand full send
buffers of 128 KiB need more, after which a connect fails with "no buffer space available"). What
bounds how many a program can serve is the operating system's limit on open
descriptors (raise it for thousands) and the one thread that runs the program's coroutines, not a
thread per connection; counts beyond 10,000 have not been measured.
Cancelling a coroutine that waits in `read` or `accept` never loses data or a connection: what
had arrived stays for the next call. `close()` never waits and never fails, and it ends every
call that waits on that object with `net.Error.Closed`.

- A `net.Conn` has `read(n)` (up to `n` bytes, waiting for at least one; empty `Bytes` is the
  end of the stream), `read_exact(n)`, `read_line()` (`none` at the end; a line may be up to 1 MiB
  and must be UTF-8), `read_until(delim, max)`, `write(text)`, `write_bytes(data)`, `close_write()`
  (tells the peer that nothing more comes, and the connection still reads), `set_timeout(seconds)`
  for each later read and write (0 means none), `set_line_limit(bytes)`, `local_addr()`,
  `peer_addr()` and `close()`. `set_timeout` is a
  deadline for the whole call, counted from the call: a `read_line` that gets half a line and then
  nothing times out as a whole, so to bound the wait for each line, set it before each call.
  `close_write()` takes its turn behind the writes already called, so it never cuts one.
- `read_until(delim, max)` gives the `Bytes` before the first `delim` (one or more bytes, however
  the stream is cut into packets) and takes the delimiter off the stream, so a line that must end
  in `\r\n` and may hold a bare `\n` is `read_until("\r\n".to_bytes(), 8192)`. `max` counts the
  bytes before the delimiter, from 0 to 1 GiB, and the delimiter has 1 to 1,024 bytes. The result
  is `none` at the end of the stream with nothing pending, `UnexpectedEnd` at the end with bytes
  pending, and `LineTooLong` as soon as `max + 1` bytes have arrived without a delimiter (not at
  `max`: a delimiter right after `max` bytes is fine, also one whose first byte is the byte after
  them and whose rest has not come yet). A call that fails with `LineTooLong` reads at most what
  is left to the larger of 32 KiB and `max` plus the delimiter's length (`read_line`: its limit
  plus 2) beyond what an earlier call had already buffered, so it never reads a whole extra read
  past a large `max` or limit; the buffer's capacity may be up to twice that while it grows. The
  bytes of a `LineTooLong` or an `UnexpectedEnd` stay in the stream. It scans only the bytes that
  are new, takes its turn like the other reads, and a cancel or a timeout keeps what had arrived.
  `set_line_limit(bytes)` sets what `read_line` takes before its line end (1 to 1 GiB, 1 MiB when
  not set; `read_until` has its own `max`), and counts at the next look of a `read_line` that
  already waits.
- `net.connect_timeout(addr, seconds)` gives up after that long. A timed-out read keeps what it
  had read, and the connection stays usable.
- A read, an accept or a write that is called where nothing else can run (a `Drop` hook, a
  callback of a host function) while another coroutine already waits in a read of the same
  connection (or an accept of the same listener, or a write of the same connection) is a
  `net.Error.Other` at once: that coroutine cannot resume while the hook waits, so the two would
  wait for each other for ever. That is the one case `net` can see. `close_write()` from such a
  place is not refused: behind another coroutine's write it takes its place after that write and
  returns at once, and the writing end is shut when the write is done (the write is never cut).
  Its `ok` then means that the shutdown is scheduled, and an error of the shutdown itself has
  nowhere to go. A `close()` before the shutdown's turn comes skips it, and a write called after
  a scheduled `close_write` fails with `BrokenPipe`, as one after an ordinary `close_write`
  does. A write from such a place that finds no write ahead of it can still wait for room that
  only another coroutine of the same program would make by reading what was written (a write is
  not finished until the peer takes it), and that coroutine cannot run while the hook waits: a
  program must not do that.
- A port can be listened on again as soon as its listener is closed, also when connections of it
  are still closing. On Linux and Windows a second `listen` on a port that is in use fails with
  `AddrInUse` even when it names a specific address (`127.0.0.1:P` while `0.0.0.0:P` is held); on
  macOS it succeeds there, because the option that makes the restart possible lets a specific
  address be bound next to a wildcard one.
- `listen("[::]:0")` accepts IPv4 connections too, on every system, and connecting to an
  unspecified address (`0.0.0.0`) means the loopback address.
- A write to a peer that has gone is `net.Error.BrokenPipe` on every system. A write that
  stopped part way (cancelled, timed out) breaks the write side of that connection, so a later
  write cannot continue in the middle of a message; reading still works.
- Failures are a `net.Error` (`Refused`, `Timeout`, `Reset`, `BrokenPipe`, `AddrInUse`,
  `AddrNotAvailable`, `Unreachable`, `Dns`, `InvalidAddress`, `PermissionDenied`, `Closed`,
  `UnexpectedEnd`, `LineTooLong`, `InvalidData` and `Other`). The text of each is Colibri's own
  and is the same on Linux, macOS and Windows; only `Other` shows the system's words.

`net` needs the `net` capability, like `http`: `colibri run` grants it, and a program that embeds
Colibri decides whether its scripts have it (`Cap::Net`).
[`examples/scripts/kvserver`](examples/scripts/kvserver/kvserver.colibri) is a key-value server over
a line protocol, written with `net`.

### HTTP server

`httpd` is an HTTP/1.1 server written in Colibri on top of `net`. It is source that ships with the
toolchain, so it uses nothing a program could not write itself. Every connection is a coroutine,
so many slow clients cost memory and no threads. This server answers `GET /hello/<name>` and ends
on `POST /stop`:

```colibri
import httpd

fn hello(r: httpd.Request) -> Result[httpd.Response] {
    let name = r.param("name").expect("the route has a :name")
    return ok(httpd.text(200, "Hello, {name}!\n"))
}

fn main() -> Result[()] {
    let server = httpd.listen("127.0.0.1:0")? // port 0: the system picks a free port
    print("listening on {server.addr()}")

    let router = httpd.Router()
    router.get("/hello/:name", hello)
    router.post("/stop", fn(r) {
        server.stop() // never waits, so it is safe here; this answer is still sent
        return ok(httpd.text(200, "bye\n"))
    })
    return server.serve(router.handler())
}
```

`curl http://127.0.0.1:PORT/hello/Colibri` answers `Hello, Colibri!`. A handler is a function from
`httpd.Request` to `Result[httpd.Response]`:

- **Routing.** `Router` matches a method and a path with literals, `:name` parameters and a last
  `*rest`, and answers 404, 405 (with `Allow`), `HEAD` and `OPTIONS` itself. A handler can also be
  any function, with no router at all.
- **Requests.** Headers, the query (`query_value`), and a body that is read on demand with
  `text()`, `body()` or `read(n)`; bodies with a `Content-Length` and chunked ones both work.
  `httpd.parse_query` reads a URL-encoded form.
- **Responses.** `httpd.text`, `html`, `json`, `bytes`, `redirect` and `status`, with `.header(..)`
  to add a field, and `httpd.stream` for a body that a function writes while it is sent (chunked,
  or with a length).
- **Errors.** `err(httpd.fail(404, "no such note"))` answers with that status and message. Any
  other `err` is a 500 with a fixed body, and a handler that panics is a 500 too: the server keeps
  serving, the connection closes and the panic is logged to stderr.
- **Limits.** `httpd.Options` bounds the request line, the header bytes and fields, the body, the
  time for a head, for a body read, for a write, for an idle connection and for a handler (a
  handler that is out of time is cancelled and the client gets a 503), and the number of open
  connections. The defaults are listed on the type. A request that breaks the syntax or tries to
  smuggle one is refused with 400 and the connection closed.
- **Stopping.** `server.stop(grace)` closes the listener and the idle connections, lets requests
  in flight finish, and cancels what is left after `grace` seconds; `serve` then returns.
- **Testing.** `httpd.test_request(..)` builds a request without a socket, so a handler or a router
  is tested as a function.

What is not built: TLS (https), HTTP/2, `Upgrade` and WebSockets, and compression. `httpd` needs
the `net` and `os` capabilities (the clock of the `Date` header), which `colibri run` grants.
[`examples/scripts/notes`](examples/scripts/notes/notes.colibri) is a small JSON API with a
router, a body, `fail`, a streamed export and a graceful stop.

### Tests

Tests live next to the code they test and run with `colibri test`:

```colibri
fn clamp(x: int, lo: int, hi: int) -> int {
    if x < lo {
        return lo
    }
    if x > hi {
        return hi
    }
    return x
}

test "clamp keeps values in range" {
    assert_eq(clamp(5, 0, 10), 5)
    assert_eq(clamp(-3, 0, 10), 0)
    assert_eq(clamp(42, 0, 10), 10)
}
```

## Install

Colibri is built from source. You need a recent stable [Rust toolchain](https://rustup.rs) (the
workspace uses edition 2024) and a C compiler, which builds the bundled `libffi`.

```sh
git clone https://github.com/jalatodeguerra/Colibri colibri
cd colibri
cargo install --path crates/colibri-cli
colibri --version
```

This puts the `colibri` binary in `~/.cargo/bin`. To build without installing, run
`cargo build --release` and use `./target/release/colibri`.

Every change is tested on Linux, macOS and Windows.

## Quick start

```console
$ colibri new hello
Created the package in `hello`.
Try:  cd hello && colibri run && colibri test
$ cd hello
$ colibri run
Hello, world!
$ colibri test
running 1 test
test main::greeting names who it greets ... ok

1 passed, 0 failed
```

`colibri new` creates a package: a `colibri.toml` manifest and a `main.colibri` with a
`main` function and a test. A single file also works without a package: `colibri run
script.colibri`.

Each file is a module, and each folder is a package of modules. Items are private to their
package unless they are marked `pub`. For example, if `text/slug.colibri` declares
`pub fn slugify(s: str) -> str`, then `main.colibri` can `import text.slug` and call
`slug.slugify(...)`. Other projects on disk are dependencies in `colibri.toml`:

```toml
[package]
name = "hello"

[dependencies]
utils = { path = "../utils" }
greet = { git = "https://example.com/me/greet.git", tag = "v1.2.0" }
```

A git dependency names one of `rev` (a commit hash of 7 to 40 hexadecimal digits: the full
hash or the start of one that is long enough to be one commit), `tag` or `branch`, or none of
them for the repository's default branch. `colibri run`, `check` and `test` fetch it
into a cache folder and write the commit they used to `colibri.lock`, which belongs in version
control; later runs use that commit until `colibri update` moves it. The editor never uses the
network.

### Conditional code and deprecation

An item with `@when(condition)` exists only where the condition is true; otherwise it is parsed
and left out, so two items with the same name can be there for different builds. The condition
can test `os` (`"linux"`, `"macos"`, `"windows"`, ...), `arch` (`"x86_64"`, `"aarch64"`, ...),
`debug` (true unless the build is a release build) and feature flags, and combine them with `!`,
`&&`, `||` and parentheses. A misspelt name, or a value that no system has (`os == "win"`), is an
error, never a quiet `false`. Features are declared in `colibri.toml`:

```toml
[package]
name = "app"

[features]
default = []   # the features that are on unless --no-default-features
fast = []      # a feature; the list holds the features it turns on as well
```

```colibri
@deprecated("use scale instead")
fn double(x: int) -> int => x * 2

fn scale(x: int, by: int) -> int => x * by

@when(debug)
fn log(msg: str) {
    print("debug: {msg}")
}

@when(!debug)
fn log(msg: str) {}

@when(fast)
fn sum(n: int) -> int => n * (n + 1) / 2

@when(!fast)
fn sum(n: int) -> int {
    let mut total = 0
    for i in 1..=n {
        total += i
    }
    return total
}

fn main() {
    log("starting")
    print(sum(100)) // 5050
    print(double(4)) // 8, with a warning on this line
}
```

`colibri run` prints `debug: starting`; `colibri run --release --feature fast` does not, and
uses the closed form of `sum`. `run`, `check` and `test` take `--feature NAME` (as often as you
like; the name must be declared in `colibri.toml`), `--no-default-features` and `--release`.
A single script has no manifest, so any `--feature` flag exists for it. A dependency asks for
its own features in the entry that names it, and git dependencies take the same keys:

```toml
[dependencies]
audio = { path = "../audio", features = ["wav"], default-features = false }
```

The features of a package are the union of what everything that depends on it asks for, and the
command-line flags apply to the root package only. Features are not in `colibri.lock`, since
they change the program and not what is fetched.

`@deprecated("message")` goes on a function, method, type, constant or field. Every use of the
item is a warning with the message; the item's own code and the code of a dependency do not
warn, and the warning never fails `check` or `run`. Editors show deprecated names struck
through.

### The REPL

```console
$ colibri repl
colibri> let x = 21
colibri> x * 2
= 42 : int
colibri> fn sq(n: int) -> int => n * n
colibri> sq(7)
= 49 : int
```

`:help` lists the commands, such as `:type <expr>` and `:load <file>`. A `@deprecated` or
`@when` line waits for the declaration under it, a use of a deprecated item warns with its
entry, and two `@when` variants of one name are both kept. `colibri repl` takes `--feature NAME`, `--no-default-features` and `--release`, as `run` does.

## Commands

| Command | What it does |
|---------|--------------|
| `colibri run [file or folder]` | type-check, then run a script or the package in a folder |
| `colibri run --watch` | run and hot-reload on every save (see below) |
| `colibri run --leaks[=error]` | report reference cycles that were never freed |
| `colibri run --profile[=FILE]` | sample the program and write a flame graph and its folded stacks (see below) |
| `colibri run --profile-rc` | count the reference-count copies and drops of each line (see below) |
| `colibri check [file or folder]` | type-check without running; a folder checks every module |
| `colibri test [file or folder]` | run the `test "name" { ... }` blocks (`--filter`, `--nocapture`, `--leaks`, `--profile`, `--profile-rc`) |
| `colibri fmt [paths]` | format files in place (`--check` only reports) |
| `colibri new <name>` | create a package |
| `colibri update [names...]` | resolve git dependencies again (branches, tags) and rewrite `colibri.lock`: all of them, or only the ones named (transitive ones too); `--path DIR` is the package folder, `--locked` and `--offline` as for `run` |
| `colibri repl` | interactive session |
| `colibri lsp` | language server on stdin and stdout |
| `colibri build [file or folder] [-o FILE]` | write one executable that holds the program and runs it (see below) |
| `colibri build --web` | **(planned)** the same for the browser, through WebAssembly |

Arguments after the file go to the program (`os.args()`, or the `args` module for parsing);
put them after `--` if they could be mistaken for the tool's own flags, such as `--watch`.

`run`, `check`, `test`, `build` and `update` fetch git dependencies into the cache and write `colibri.lock`;
`--locked` fails instead of changing the lock and `--offline` never uses the network.

## Tooling

### Editor support

`colibri lsp` is a language server with diagnostics as you type (the whole package is checked,
including unsaved buffers, re-checked a moment after you stop typing, or at once when a request
needs the result), hover with types and signatures, completion, go to definition,
find references, rename, the outline of a file, and formatting. A rename is refused, with the
reason, where the editor cannot be sure to change every use. A VS Code extension with syntax
highlighting and the language client is in [`editors/vscode`](editors/vscode/README.md).

### Hot reload

`colibri run --watch` keeps a program running and applies your edits on save. A change that only
touches function bodies is patched into the running program; anything else (a new struct field,
a changed signature) restarts it, and the optional hooks `fn save_state() -> str` and
`fn restore_state(s: str)` carry state across the restart. Edits are applied while the program
is waiting, between event-loop ticks.

```colibri
fn message() -> str => "hello"

fn tick() {
    while true {
        print(message())
        wait(0.1)
    }
}

fn main() {
    go tick() // main returns; the event loop keeps the coroutine running
}
```

Run it with `colibri run --watch live.colibri`, change `"hello"` and save: the tool prints
`colibri: patched 1 function` and the loop prints the new text without restarting.

### Leak detector

Reference counting cannot free a cycle of objects that point at each other. `--leaks` finds
those cycles when the program ends and tells you where they were allocated:

```colibri
class Node {
    name: str
    next: Node? = none
}

fn main() {
    let a = Node { name: "a" }
    let b = Node { name: "b", next: some(a) }
    a.next = some(b) // a cycle: reference counting never frees it
}
```

```console
$ colibri run --leaks cycle.colibri
warning: 1 leaked reference cycle (reference counting never frees a cycle; make one link `weak`)
  leak: a cycle of 2 objects: Node(name: "a") -> next -> Node(name: "b") -> next -> Node(name: "a")
    allocated at Node at cycle.colibri:7, Node at cycle.colibri:8
```

`--leaks=error` also exits with status 1, which is useful in CI, and `colibri test --leaks`
checks each test.

### Profiler

`colibri run --profile` samples the running program about every 1,024 steps and, when it
ends, writes a flame graph (`profile.svg`, or the file you name with `--profile=FILE`), the
folded stacks it was drawn from (`profile.folded`, next to it, one `main;load;parse 1234` line
per stack, which other flame graph tools read too) and prints the heaviest functions and
lines to stderr:

```console
$ colibri run --profile sum.colibri
profile: 41.3 ms
written: profile.folded, profile.svg

  self   total  function
 61.0%   61.0%  fib
 21.9%   21.9%  go spin
 12.2%   78.1%  main
  4.9%    4.9%  <closure in main>

  self  line
 43.9%  sum.colibri:2
 21.9%  sum.colibri:8
```

Frames are functions; a coroutine's stack starts at `go name`, a built-in that is written in
Colibri shows as its own name (`List.map`) with your closure under it, and a call into a host
function shows as `[native: name]`. A stack deeper than 200 frames shows its first 40 and
last 160 with `[...]` between.

By default a sample weighs the real time since the previous one, so time spent in host
functions and long built-ins counts; because the sample is taken at the next step boundary,
that time is charged to the line that made the call. Time the program spends sleeping (`wait`,
the event loop between ticks) is not charged to the code that runs next: it is the root
`[idle]`, so a program that mostly waits shows mostly `[idle]`. Checking the program is left
out, and a function's first call includes the time to compile it. `--profile-weight=steps`
weighs by steps instead: the profile is then the same on every run. `colibri test --profile`
makes one profile of all the tests. `--profile=out` writes `out.svg` and `out.folded`. A
program shorter than 1,024 steps has no sample to draw from and shows up as one `[unsampled]`
frame, and a sample taken while no function runs (a host calling in) is `[host]`.
`--profile` does not combine with `--watch`. Profiling costs a few percent.

#### Reference-count traffic

Strings, lists, maps, structs and the like are shared by reference counting: passing one to a
function, reading it from a collection or letting it go out of scope takes or gives up a
reference. `colibri run --profile-rc` counts those operations by line and prints the lines
with the most, which are the places where a different shape of code (passing less around,
reading an item once instead of on every trip) pays off:

```console
$ colibri run --profile-rc names.colibri
111
rc traffic: 4003 operations

  share    copies     drops    clones  line
 50.0%      1001      1001         0  names.colibri:7
 50.0%      1000      1000         0  names.colibri:4
  0.0%         0         1         0  names.colibri:1
```

*Copies* are references taken: a value copied into a call, a string literal loaded, a global, a
field, an element or the item of a `for` read into a register, a value pushed onto a list or
kept by a constructor. *Drops* are references given up when a temporary or a local dies or a
variable is assigned over. *Clones* are the deep copies `x.clone()` makes. Lines are ranked by
the three added together. A local that dies when its function ends is charged to the line of the `fn` (or of the
`return`), and one that dies at the end of a loop trip to the line of the `for`.

It works on its own and does not need `--profile`; give both for both reports.
`colibri test --profile-rc` makes one report of all the tests. The program is compiled with a
counter next to each such instruction, so the run is slower (see `--help`) but prints and
computes exactly what it did without the flag; without the flag nothing is counted and
nothing is slower. What is not counted: the work that built-ins written in Rust do inside
(`sort` and `join` copying their items, map lookups), host functions, writes into fields and
elements, and the cleanup that runs while a panic unwinds. A built-in that is written in
Colibri (such as `map`) is charged to the line that calls it. Under `--profile-rc` some
closures that would otherwise run as native loops (`fn(p) => p.name`) go through that
Colibri loop instead, so their counts describe the loop's traffic. `--profile-rc` does not combine
with `--watch`.

### Single-file executables

`colibri build` writes one executable that holds a program: a script, or the package in a folder
(the current folder by default), with the modules it imports and the sources of its git
dependencies. The standard library is part of `colibri` itself. Copy the file to another machine
of the same kind and run it; nothing else is needed, not `colibri` and not the source files.

```console
$ colibri build tool.colibri -o tool
built tool (3 source files, 14293760 bytes)
$ ./tool one two
```

The executable is a copy of the `colibri` that built it, with the program appended. It runs
the way `colibri run` does: the same output, exit codes (`101` after a panic) and stack traces
(which name the files as they were named when it was built), and every capability granted. Every
argument given to it goes to the program (`os.args()`), including ones such as `--version` that
`colibri` itself would read.

- The program is checked first, and nothing is written if it does not check. Git dependencies are
  fetched and locked as for `run` (`--locked` and `--offline` work the same).
- `--feature`, `--no-default-features` and `--release` decide `@when` as they do for `run`, and the
  executable keeps the result: `--release` is how a program built for use is made.
- Without `-o` the file is named after the script or the package folder, in the current folder.
  A build never overwrites a source file.
- The executable runs on the platform of the `colibri` that was copied: this one, unless
  `--target` and `--runner` say otherwise (below).
- The files of the program keep the names they had when it was built, and so do the dependencies
  (which live in the cache folder of the machine that built it): a stack trace shows those paths,
  also on the machine that runs the program. An argument that is not valid UTF-8 reaches `os.args()`
  with replacement characters.
- The program is read when it starts and checked then, as `colibri run` does; the files of the
  program are held as text, so the file is about the size of `colibri` plus the source.

#### Another platform

`colibri build` copies the `colibri` that runs it, so by itself it writes programs for its own
platform. For another one, give the `colibri` of that platform and name the platform:

```console
$ colibri build tool.colibri --target windows-x86_64 --runner ./colibri-windows/colibri.exe
built tool.exe (3 source files, 14551040 bytes)
```

- The platforms are `linux-x86_64`, `linux-aarch64`, `macos-x86_64`, `macos-aarch64`,
  `windows-x86_64` and `windows-aarch64`. `--target` and `--runner` go together; `--target` alone
  is fine for the platform you are on. Nothing is ever downloaded: take the runner from the release
  of the same `colibri` version (an executable of another version is refused).
- The runner is looked at, never run. It must be an executable of the target's kind and
  processor (ELF, PE or Mach-O; a universal Mach-O file needs the target's processor among its
  slices), it must say that it is `colibri` of this version, and it must not be a built program.
- The target also decides `@when(os == "..")` and `@when(arch == "..")` while the program is
  checked and packed, so a cross build holds the files of the target and the executable behaves as
  it would there. Without `-o`, the name gets `.exe` for a Windows target and nothing else.
- **macOS.** A change to a signed executable invalidates its signature, and arm64 Macs refuse
  unsigned code. Built on a Mac, the program is signed ad hoc (`codesign -s -`) when it is
  written. Built anywhere else, it is left unsigned with a warning: run `codesign -s - PROGRAM`
  on a Mac, or sign it with your own identity (`codesign -s "Developer ID Application: ..."`),
  before it is used. To make that possible `colibri build` removes the runner's signature and
  extends the `__LINKEDIT` segment over the program. A universal runner cannot be prepared that way;
  take one slice with `lipo -thin`.
- **Windows.** A built executable is not signed, and the program is appended to the file, which
  some antivirus programs distrust: if one blocks it, ask it to allow the file. Signing it with
  Authenticode afterwards is not supported: a signature goes at the end of the file, where the
  program is looked for.

#### Assets

Files that a program reads at run time (a template, a table, an image) are *assets*. A package
lists them in `colibri.toml`, and the program reads them with `import assets`:

```toml
[build]
assets = ["greeting.txt", "data/", "img/*.png", "templates/**/*.html"]
```

```colibri
import assets

fn main() {
    print(assets.read("greeting.txt").expect("greeting"))
    let logo = assets.read_bytes("img/logo.png").expect("logo")  // Bytes
    print(assets.list("data").expect("data"))                  // ["deep/", "table.csv"]
}
```

- The list is on one line, as every array of `colibri.toml`. A pattern is a path relative to the
  package folder, written with `/`. `*` stands for any run of
  characters inside one name, `?` for one character, and `**` for any number of folders. A pattern
  selects a file when it matches the file or a folder above it, so `data/` (or `data`) is
  everything below `data`; a trailing `/` insists on a folder. Files whose names start with a dot
  are selected like any other.
- `assets.read(path)` gives text (`assets.Error.InvalidData` if the file is not UTF-8),
  `assets.read_bytes(path)` gives `Bytes`, as `fs.read` and `fs.read_bytes` do, and `assets.list(dir)`
  gives the sorted names in a folder, with a `/` after the name of a folder; `""` is the package
  folder. Each returns a `Result`, and the error is an `assets.Error`: `NotFound` (not listed in the
  manifest, not there, or not a file), `Invalid` (a path that is empty, absolute, has a `.` or `..`
  piece, a `\`, a `:`, a piece that ends with a `.` or a space or is a Windows device name such as
  `CON` or `COM1`, or a symbolic link on the way) or `Other`.
- The module needs no capability. A program reads only what the manifest lists: another file of the
  package, a file outside it and a path with `..` are errors, and a symbolic link is never followed.
- `colibri run` and `colibri test` read the files from the package folder, wherever the tool is
  started from; a built executable reads them from itself. The program cannot tell them apart,
  and the executable needs neither the source files nor the assets on disk.
- The package folder is the folder with the `colibri.toml`: the folder given to `colibri run`, or
  the folder of a script that has a manifest beside it. A script with no manifest has no assets.
- `colibri build` appends the files to the executable and lists them (`asset data/table.csv (4
  bytes)`, then `built tool (1 source file, 4 assets, ...)`). It stops if a pattern selects no file
  (a likely typo), if a selected file is a symbolic link, or if the output would overwrite an asset.
  Assets of dependencies are not packed: only the manifest of the program counts.
- A built program does not read its assets when it starts: it keeps them in its file and reads one
  when the program asks for it, so a large asset costs nothing to a program that does not use it
  (neither time nor memory). Each asset has a check sum that is verified when it is read; if the
  executable was damaged, that read is an `assets.Error.Other` and nothing else is affected.
  `colibri build` does not hold the assets in memory either.
- Names are looked up exactly as they are spelt in the folder, with the case of the file system
  that was walked: `assets.read("Logo.PNG")` does not find `logo.png`, also on Windows and macOS, so
  `colibri run` finds what the built program finds. `colibri build` and `assets.list` walk only
  the folders a pattern can reach (not `.git` or `target`, unless a pattern names them).

### Formatter

`colibri fmt` has no style settings: there is one canonical layout. `colibri fmt -` formats
standard input to standard output, for editors. `colibri fmt --check` lists files
that would change and exits with 1, for CI.

## Embedding in Rust

The `colibri-runtime` crate runs Colibri inside a Rust program. Register Rust functions, load a
script, and call its functions with typed arguments:

```rust
use colibri_runtime as colibri;

fn main() -> Result<(), colibri::Error> {
    let mut vm = colibri::Vm::builder().build();
    vm.register_fn("double", |x: i64| x * 2)?;
    vm.load_source(
        "main.colibri",
        r#"fn greet(name: str) -> str => "Hello, {name}! The answer is {double(21)}.""#,
    )?;
    let greet = vm.get_fn::<(&str,), String>("greet")?;
    println!("{}", greet.call(&mut vm, ("Rust",))?); // Hello, Rust! The answer is 42.
    Ok(())
}
```

`Vm` is the runtime handle; it runs the program on the bytecode VM. An instruction budget is
measured in steps that are approximate, so treat a budget as a bound, not an exact figure. The
runtime also offers:

- `#[colibri::export]` to expose Rust structs and methods as Colibri types
- generational handles to host objects, and callbacks the host can keep and call later
- capabilities that decide what a script may touch: files, the network, processes and the
  environment, and foreign functions
- an instruction budget per call, so a runaway script cannot hang the host
- hot reload of embedded scripts
- `Bytes` crossing as `colibri_runtime::Bytes`
- running a package's `test` blocks from the host (`Vm::load_tests`, `tests`, `run_test`), with
  `Vm::on_test_start` to put the host's own state back before each test
- a host class that defines `==` with `TypeBuilder::eq` gives `TypeBuilder::hash` next to it, so
  that `x.hash()` agrees with `==`
- feature flags for `@when` (`VmBuilder::feature`)
- `.cbi` interface files, so `colibri check` and the editor know about the host's functions

A script panic comes back to the host as an `Error` and leaves the `Vm` usable.
[`examples/host_jobs`](examples/host_jobs/README.md) is a complete example: a Rust job runner
whose logic comes from Colibri scripts (host classes, handles, callbacks and a scheduler). The API reference is in the crate's documentation:
`cargo doc -p colibri-runtime --open`.

## Standard library

| Module | Contents |
|--------|----------|
| `io` | standard input and output |
| `fs`, `path` | files, folders, walking trees, metadata; `fs.File` handles for reading and writing piece by piece; path manipulation |
| `os` | environment variables, arguments, exit |
| `process` | run subprocesses, with pipes |
| `args` | command-line argument parsing |
| `str` | string helpers and parsing |
| `json` | `json.encode`, typed `json.decode[T]`, and `json.Value` for a document of unknown shape |
| `toml` | `toml.parse` into a `json.Value` and typed `toml.decode[T]` |
| `csv` | CSV text to records or rows with a header, and back |
| `base64`, `hex` | encoding and decoding of text and of `Bytes` |
| `assets` | the files listed under `[build] assets` in `colibri.toml`, read from the package folder by `run` and `test` and from the executable after `colibri build` |
| `hash` | SHA-256 of text and of `Bytes` |
| `regex` | regular expressions |
| `http` | a blocking HTTP client that does not block other coroutines; `get_bytes` and `request_bytes` for bodies that are not text |
| `net` | TCP sockets: `net.listen`, `net.connect` and `net.connect_timeout` give a `net.Listener` or a `net.Conn` that read and write lines, bytes and text without blocking other coroutines |
| `httpd` | an HTTP/1.1 server written in Colibri on `net`: a router, request bodies (also chunked), streamed responses, keep-alive, limits and timeouts, and a graceful stop |
| `time`, `datetime` | clocks and sleeping; dates, times and formatting |
| `math`, `rand` | math functions; random numbers |
| `vec` | 2D, 3D and 4D vector types of `f32` and `i32`, written in Colibri (a library on top of operator interfaces, not part of the language) |

Every module has a page in the [standard library reference](guide/std/README.md).

Paths are `/`-separated text on every system, Windows included. `path.from_native` converts a path
that came from outside (a command-line argument, a file's content) from the system's own spelling,
and on Windows, and only there, `C:/` and `//host/share/` are roots.

`List`, `Map`, `Set`, `Bytes`, `StrBuilder` and `BytesBuilder` are built in. A Rust program that embeds Colibri decides
whether its scripts get the standard library (the `colibri-std` crate) and which capabilities
they have.

## Status and roadmap

**Done:**

- The language core: structs, classes, enums, `match`, generics with bounds, interfaces,
  closures, `Result`/`Option` error handling, modules and packages, `weak` references, derived
  ordering, `@deprecated` and conditional code with `@when` and features.
- A register bytecode VM over a typed intermediate representation, with a deterministic drop
  order. It runs programs for `colibri` and for the embedding API.
- The embedding API, the C FFI, coroutines, the 22 std modules above (TCP sockets and an HTTP/1.1 server included),
  `Bytes` and file handles.
- Operator interfaces (`Add`, `Sub`, `Mul`, `Div`, `Neg`) with default type parameters, which
  let a library such as `vec` define its own arithmetic.
- Tooling: `run`, `check`, `test`, `fmt`, `new`, `build` (single-file executables), `repl`, `lsp`, hot reload and the leak detector.

**Next:**

- Speed (in progress): the goal is to beat CPython 3.13 on every program in
  [`benchmarks/`](benchmarks/README.md); the VM is ahead of it on seven of the eight programs
  there and level on one.
- `colibri build --web` for the browser, and building for another platform than the one running.

**Later:**

- Thread tasks with channels, a debugger (DAP) and a profiler.
- TLS for `net` and `httpd` (https), HTTP/2, WebSockets, response compression, and UDP and Unix
  domain sockets in `net`: none of them is built yet.
- Using Rust crates from Colibri through generated bindings.

**Not planned:** a JIT, exceptions, inheritance, macros, `async`/`await` and a package registry
in v1.

## Examples

- [`examples/scripts`](examples/scripts/README.md): four real programs, a file organiser, a
  JSON/HTTP report generator, a TCP key-value server and a JSON API served with `httpd`.
- [`examples/host_jobs`](examples/host_jobs/README.md): a Rust job runner that takes its job
  logic from Colibri scripts (host classes, handles, callbacks and the scheduler).
- [`benchmarks`](benchmarks/README.md): eight small programs in Colibri and Python, with the
  current timings.

## Contributing

```sh
cargo test --workspace                                 # unit, golden and integration tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Most language behaviour is pinned by golden tests in `tests/golden`: each `.colibri` file lists
its expected output (`// expect:`), diagnostics (`// error:`) or drop order (`// drops:`).

The guide in `guide/` is tested too (`crates/colibri-cli/tests/guide.rs`): each ` ```colibri `
sample must pass `colibri check` and `colibri fmt --check`, and one followed by an ` ```output `
block must print exactly that. A fence word says more (`no-run`, `test`, `error`, `panics`, `pkg=NAME,file=PATH`; the
test's header lists them). The pages in `guide/std` are generated from the standard library's
interfaces and the doc comments of `vec` and `httpd`; after a change to a signature, write them
again with `colibri doc --std guide/std`, and the test fails until you do.

## License

Colibri is released under the [MIT license](LICENSE).
