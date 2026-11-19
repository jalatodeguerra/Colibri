# Colibri

Colibri is a statically typed, general-purpose programming language implemented in Rust. It aims
for syntax as simple as Go or Python (with braces), static types with local inference, errors
as values, and memory management by plain reference counting. It is meant for tools and
automation, servers and embedding in Rust programs, among other uses.

```colibri
struct Task {
    title: str
    done: bool = false
}

fn main() {
    var tasks = [Task { title: "write docs" }, Task { title: "ship it" }]
    tasks[0].done = true

    for t in tasks {
        let mark = if t.done { "x" } else { " " }
        print("[{mark}] {t.title}")
    }

    let open = tasks.filter(fn(t) => !t.done).len()
    print("{open} left")
}
```

## Status

Phases 1 and 2 are done and on `main`: the language core on a tree-walking interpreter over a
typed HIR, an embedding API for Rust hosts (`colibri-runtime`), a formatter, a language server
(diagnostics, hover, go to definition, formatting), 15 standard library modules, a test runner,
packages with path dependencies, coroutines (`go`, `wait`, groups, signals) and C FFI.

Next is phase 3: a bytecode VM that replaces the tree-walker, `colibri build`, a web target and
thread tasks. None of it is started. Features that are designed but not built are marked in
[docs/DESIGN.md](docs/DESIGN.md).

## Building and running

```sh
cargo build --release
./target/release/colibri run hello.colibri
```

While developing, `cargo run -p colibri-lang -- run hello.colibri` does the same. Commands:

| Command | What it does |
|---------|--------------|
| `colibri run [file or folder]` | run a script, or the package in a folder |
| `colibri check [file or folder]` | type-check without running; a folder checks every module of the package |
| `colibri fmt <paths>` | format source files in place |
| `colibri test [file or folder]` | run the `test "name" { .. }` blocks |
| `colibri repl` | interactive session |
| `colibri lsp` | language server on stdin and stdout |
| `colibri new <name>` | create a new package folder with a manifest, a `main.colibri` and a test |

Run the test suite with `cargo test --workspace`.

## Documentation

- [docs/DESIGN.md](docs/DESIGN.md): the language design, with the status of each decision and the
  open questions
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how the implementation is built
- [docs/EMBEDDING.md](docs/EMBEDDING.md): embedding Colibri in a Rust program
- [docs/history](docs/history): the finished phase 1 and 2 implementation plans

## Examples

- [examples/scripts](examples/scripts/README.md): a file organiser and a JSON/HTTP report
  generator, written in Colibri
- [examples/host_game](examples/host_game/README.md): a Rust program that drives its logic from
  Colibri scripts, as an embedding example
- [benchmarks](benchmarks/README.md): eight small programs in Colibri and Python, and the
  baseline the bytecode VM has to beat
