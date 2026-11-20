# Colibri

**A small, statically typed language with the feel of a scripting language.**

Colibri is a general-purpose programming language written in Rust. It reads like Go or Python
with braces, catches type errors before your program runs, and manages memory with plain
reference counting instead of a garbage collector. Use it for scripts and command-line tools,
for automation, or as a scripting language embedded in a Rust program.

> **Status: early and pre-release.** The language, standard library and tooling described below
> work today on a tree-walking interpreter. A bytecode VM will replace it; that work is not
> started. Expect breaking changes. Features that are planned but not built are marked
> **(planned)**.

```colibri
import fs

struct WordCount {
    word: str
    count: int
}

fn count_words(path: str) -> Result[List[WordCount]] {
    let text = fs.read(path).context("reading {path}")?
    var counts = Map[str, int]()
    for word in text.to_lower().split(" ") {
        let w = word.trim()
        if w != "" {
            counts[w] = counts.get_or(w, 0) + 1
        }
    }
    var words = counts.entries().map(fn(e) => WordCount { word: e.0, count: e.1 })
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
- **Batteries for scripting.** 21 standard library modules, including files (whole or streamed
  through a handle) and paths, JSON, TOML and CSV, regular expressions, subprocesses,
  command-line arguments, dates, an HTTP client and TCP sockets, and `Bytes` for data that is not
  text.
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
    var a = Point { x: 1, y: 2 }
    var b = a
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
    var best = items.first()?
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
    var sum = zero
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
    var ks: List[float] = [] // not `var ks = []`: its type decides which `*` is meant
    var w = v
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
    var versions = [Version { major: 1, minor: 10 }, Version { major: 1, minor: 2 }]
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
    var errors = 0
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

`accept`, the reads, the writes and `connect` wait on background threads, so while one coroutine
waits for a peer the others keep running. Cancelling a coroutine that waits in `read` or `accept`
never loses data or a connection: what had arrived stays for the next call. `close()` never
waits and never fails, and it ends every call that waits on that object with `net.Error.Closed`.
Each connection or listener that waits in a read or an accept holds one operating system thread
while it waits, so `net` suits tens or hundreds of connections at once, not tens of thousands.

- A `net.Conn` has `read(n)` (up to `n` bytes, waiting for at least one; empty `Bytes` is the
  end of the stream), `read_exact(n)`, `read_line()` (`none` at the end; a line may be up to 1 MiB
  and must be UTF-8), `write(text)`, `write_bytes(data)`, `close_write()` (tells the peer that
  nothing more comes, and the connection still reads), `set_timeout(seconds)` for each later read
  and write (0 means none), `local_addr()`, `peer_addr()` and `close()`. `set_timeout` is a
  deadline for the whole call, counted from the call: a `read_line` that gets half a line and then
  nothing times out as a whole, so to bound the wait for each line, set it before each call.
  `close_write()` takes its turn behind the writes already called, so it never cuts one.
- `net.connect_timeout(addr, seconds)` gives up after that long. A timed-out read keeps what it
  had read, and the connection stays usable.
- A read or an accept that is called where nothing else can run (a `Drop` hook, a callback of a
  host function) while another coroutine already waits in a read of the same connection (or an
  accept of the same listener) is a `net.Error.Other` at once: that coroutine cannot resume while
  the hook waits, so the two would wait for each other for ever. That is the one case `net` can
  see. A `write` or `close_write` from such a place can still wait for room that only another
  coroutine of the same program would make by reading what was written (a write is not
  finished until the peer takes it, and `close_write` waits for the writes before it), and that
  coroutine cannot run while the hook waits: a program must not do that.
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
Colibri decides whether its scripts have it (`Cap::Net`). There is no HTTP server in the
standard library yet; [`examples/scripts/kvserver`](examples/scripts/kvserver/kvserver.colibri)
is a key-value server over a line protocol, written with `net`.

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
    var total = 0
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
| `colibri check [file or folder]` | type-check without running; a folder checks every module |
| `colibri test [file or folder]` | run the `test "name" { ... }` blocks (`--filter`, `--nocapture`, `--leaks`) |
| `colibri fmt [paths]` | format files in place (`--check` only reports) |
| `colibri new <name>` | create a package |
| `colibri update [names...]` | resolve git dependencies again (branches, tags) and rewrite `colibri.lock`: all of them, or only the ones named (transitive ones too); `--path DIR` is the package folder, `--locked` and `--offline` as for `run` |
| `colibri repl` | interactive session |
| `colibri lsp` | language server on stdin and stdout |
| `colibri build` | **(planned)** one executable with the VM and the program's bytecode |
| `colibri build --web` | **(planned)** the same for the browser, through WebAssembly |

Arguments after the file go to the program (`os.args()`, or the `args` module for parsing);
put them after `--` if they could be mistaken for the tool's own flags, such as `--watch`.

`run`, `check`, `test` and `update` fetch git dependencies into the cache and write `colibri.lock`;
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

`Vm` is the runtime handle; today it runs the tree-walking interpreter, and the bytecode VM will
take its place behind the same API. The runtime also offers:

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
| `hash` | SHA-256 of text and of `Bytes` |
| `regex` | regular expressions |
| `http` | a blocking HTTP client that does not block other coroutines; `get_bytes` and `request_bytes` for bodies that are not text |
| `net` | TCP sockets: `net.listen`, `net.connect` and `net.connect_timeout` give a `net.Listener` or a `net.Conn` that read and write lines, bytes and text without blocking other coroutines |
| `time`, `datetime` | clocks and sleeping; dates, times and formatting |
| `math`, `rand` | math functions; random numbers |
| `vec` | 2D, 3D and 4D vector types of `f32` and `i32`, written in Colibri (a library on top of operator interfaces, not part of the language) |

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
- A tree-walking interpreter over a typed intermediate representation, with a deterministic
  drop order.
- The embedding API, the C FFI, coroutines, the 21 std modules above (TCP sockets included),
  `Bytes` and file handles.
- Operator interfaces (`Add`, `Sub`, `Mul`, `Div`, `Neg`) with default type parameters, which
  let a library such as `vec` define its own arithmetic.
- Tooling: `run`, `check`, `test`, `fmt`, `new`, `repl`, `lsp`, hot reload and the leak detector.

**Next** (not started):

- A shared runtime core, then the **bytecode VM**, which becomes the engine behind `colibri run`,
  embedding and hot reload. The interpreter is retired once the VM passes every test. The first
  speed goal is to beat CPython 3.13 on every program in [`benchmarks/`](benchmarks/README.md);
  the interpreter is about 4 to 10 times slower than CPython today.
- `colibri build` for single-file executables and `colibri build --web` for the browser.

**Later:**

- Thread tasks with channels, a debugger (DAP) and a profiler.
- Using Rust crates from Colibri through generated bindings.

**Not planned:** a JIT, exceptions, inheritance, macros, `async`/`await` and a package registry
in v1.

## Examples

- [`examples/scripts`](examples/scripts/README.md): three real automation programs, a file
  organiser, a JSON/HTTP report generator and a TCP key-value server.
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

## License

Colibri is released under the [MIT license](LICENSE).
