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
- **Generics and interfaces:** `List[T]`, `fn largest[T: Ord](...)`, interfaces
  with default methods, enums with data and exhaustive `match`.
- **Coroutines without colored functions.** `go f()` starts a coroutine and `wait(secs)`,
  `join()`, groups and signals suspend it. There is no `async`/`await`, so any function can wait.
- **Batteries for scripting.** 20 standard library modules, including files and paths, JSON,
  TOML and CSV, regular expressions, subprocesses, command-line arguments, dates and an HTTP
  client.
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

`:help` lists the commands, such as `:type <expr>` and `:load <file>`.

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
including unsaved buffers), hover with types and signatures, completion, go to definition,
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
- `.cbi` interface files, so `colibri check` and the editor know about the host's functions

A script panic comes back to the host as an `Error` and leaves the `Vm` usable.
[`examples/host_game`](examples/host_game/README.md) is a complete example: a Rust program that
runs its logic from Colibri scripts. The API reference is in the crate's documentation:
`cargo doc -p colibri-runtime --open`.

## Standard library

| Module | Contents |
|--------|----------|
| `io` | standard input and output |
| `fs`, `path` | files, folders, walking trees, metadata; path manipulation |
| `os` | environment variables, arguments, exit |
| `process` | run subprocesses, with pipes |
| `args` | command-line argument parsing |
| `str` | string helpers and parsing |
| `json` | `json.encode`, typed `json.decode[T]`, and `json.Value` for a document of unknown shape |
| `toml` | `toml.parse` into a `json.Value` and typed `toml.decode[T]` |
| `csv` | CSV text to records or rows with a header, and back |
| `base64`, `hex` | encoding and decoding of text and of bytes |
| `hash` | SHA-256 |
| `regex` | regular expressions |
| `http` | a blocking HTTP client that does not block other coroutines |
| `time`, `datetime` | clocks and sleeping; dates, times and formatting |
| `math`, `rand` | math functions; random numbers |
| `vec` | 2D, 3D and 4D vector types |

Paths are `/`-separated text on every system, Windows included. `path.from_native` converts a path
that came from outside (a command-line argument, a file's content) from the system's own spelling,
and on Windows, and only there, `C:/` and `//host/share/` are roots.

`List`, `Map`, `Set` and `StrBuilder` are built in. A Rust program that embeds Colibri decides
whether its scripts get the standard library (the `colibri-std` crate) and which capabilities
they have.

## Status and roadmap

**Done:**

- The language core: structs, classes, enums, `match`, generics, interfaces, closures,
  `Result`/`Option` error handling, modules and packages, `weak` references.
- A tree-walking interpreter over a typed intermediate representation, with a deterministic
  drop order.
- The embedding API, the C FFI, coroutines, the 20 std modules above.
- Tooling: `run`, `check`, `test`, `fmt`, `new`, `repl`, `lsp`, hot reload and the leak detector.

**Next** (not started):

- Arithmetic operator interfaces (`Add`, `Sub`, `Mul`, `Neg`), so that vector types can move out
  of the core into a library.
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

- [`examples/scripts`](examples/scripts/README.md): two real automation programs, a file
  organiser and a JSON/HTTP report generator.
- [`examples/host_game`](examples/host_game/README.md): a Rust host program driven by Colibri
  scripts.
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
