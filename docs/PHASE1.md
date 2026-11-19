# Phase 1 implementation plan

This file tracks how phase 1 of [DESIGN.md §17.5](DESIGN.md#175-roadmap-locked-revised-2026-11-19)
is being built. DESIGN.md is the source of truth for the language; this file records the
implementation architecture and the PR sequence. When the two disagree, DESIGN.md wins and this
file is fixed.

## Crates (phase 1)

Only the crates phase 1 needs exist. The others in DESIGN §17.1 (`colibri-ir`, `colibri-vm`,
`colibri-std`, `colibri-game`, `colibri-lsp`) are created when their phase starts.

| Crate | Directory | Role |
|-------|-----------|------|
| `colibri-syntax` | `crates/colibri-syntax` | Source map, spans, diagnostics type, lexer, parser, AST, the formatter (`fmt`) |
| `colibri-check` | `crates/colibri-check` | Name resolution, type checking, typed HIR |
| `colibri-interp` | `crates/colibri-interp` | Tree-walking interpreter over the HIR (the reference engine) |
| `colibri-runtime` | `crates/colibri-runtime` | Embedding API: `Vm`, `VmBuilder`, `get_fn`, `call`, `register_fn`, host classes and handles, capabilities |
| `colibri-bind` | `crates/colibri-bind` | Proc macros behind `colibri::export` (struct, enum, `impl` and function exports) |
| `colibri-lang` | `crates/colibri-cli` | The `colibri` binary (DESIGN §17.2: published as `colibri-lang`, binary `colibri`) |

Dependency direction: `syntax → check → interp → runtime → cli`; `bind` has no dependencies
but `syn`/`quote` and is re-exported by `runtime`. The runtime is the facade
hosts use (`use colibri_runtime as colibri;`). When the bytecode VM lands in phase 3 it slots in
behind the same `Vm` API.

## Architecture decisions

- **Diagnostics** live in `colibri-syntax` (`Diagnostic` with severity, message, primary span,
  secondary labels and an optional hint) so every stage reports the same way. The CLI renders
  them with `ariadne` (the renderer lives in `colibri-runtime`, so hosts and the CLI share it).
  The checker keeps going after the first error (DESIGN §16.4).
- **Lexer** produces raw tokens including newlines; a separate pass applies the statement
  termination rules of DESIGN §3.2 (Go's rule, bracket stack, leading `.`/`or`/`&&`/`||`/`else`
  continuation). String literals are lexed whole, with interpolation holes recorded as spans that
  the parser re-lexes and parses as expressions.
- **Parser** is hand-written recursive descent with Pratt parsing (DESIGN §17.3), following the
  brace rules of §3.5. Every expression node carries a `NodeId` and a `Span`.
- **HIR** is the contract between the checker and every engine (DESIGN §16.2): names resolved
  (locals as frame slot indices, globals and functions as ids), every expression typed, sugar
  removed. The interpreter never re-resolves names.
- **Types** are bidirectional with local unification (DESIGN §5.4, §17.3), inference variables
  scoped to one function body.
- **Interpreter values** are a Rust `enum Value` with `Rc` heap objects. Scope-end drops are
  explicit and in reverse declaration order (DESIGN §11.2); places are copy-in/copy-out with a
  mutation guard (§11.6). Semantics never rely on `RefCell` panics.
- **Output** from `print` goes through a writer configured on the `Vm`, so golden tests and hosts
  capture it in-process.

## Golden tests

`tests/golden/**/*.colibri` run through the runtime in-process, one test per file
(DESIGN §17.3). Directives are comments:

| Directive | Meaning |
|-----------|---------|
| `// expect: text` | The next line of stdout must equal `text`. All `expect` lines in file order form the whole expected stdout. |
| `// error: text` | At the end of a source line: a compile diagnostic on that line whose message contains `text`. The program is not run. |
| `// warning: text` | At the end of a source line: a compile warning on that line whose message contains `text`. Warnings are compared strictly: an unlisted warning fails the file, as does a listed one that does not appear. |
| `// panic: text` | The run must end in a panic whose message contains `text` (stdout before it is still checked). Several lines must all be contained; notes of later panics (`defer`) are appended to the message as `note: ..`. |
| `// main-err: text` | `main` returns `err(e)`: the run ends with `Error::MainReturnedErr` whose message (the whole error chain) contains `text`. The CLI prints `error: ..` to stderr and exits 1 (a panic exits 101). |
| `// drops: a, b, c` | The class objects destroyed, in order, up to and including the module-level values released after `main` (reverse initialisation order). An object is labelled by its `name: str` field if it has one, else its class name. Compared only when the directive is present; `// drops:` alone expects none. |

### Folders as tests

A folder under `tests/golden/` that holds a `main.colibri` is one test (named `dir/`), and the
files in it, at any depth, are one program: the harness runs `Vm::load_file(dir/main.colibri)`, so
the folder is the implicit package and its subfolders are packages (ARCHITECTURE §11). The folder
is not searched for further tests. Directives are read from every `.colibri` file in it:

- `// error:` and `// warning:` apply to the line of the file that carries them, and the
  diagnostic must be reported in that same file (the harness compares file and line).
- `// expect:` lines form the expected stdout: those of `main.colibri` first, then those of the
  other files in path order.
- `// drops:`, `// panic:`, `// main-err:` and `// skip:` are read from `main.colibri` only.

Package tests live in `tests/golden/modules/`.

## PR sequence

1. Workspace skeleton, CI (fmt, clippy, test), golden-test harness, syntax highlighting grammar.
2. Lexer: the complete lexical grammar of DESIGN §3 with statement termination.
3. Slice 1 (hello world end to end): `print`, functions, `let`/`var`, ints, strings with
   interpolation; parser, checker, interpreter, CLI `run`/`check`, and a Rust host using
   `Vm::load_file`, `get_fn`, `call`.
4. Structs, enums, `match` with exhaustiveness, `if`/`for`/`while`, ranges, tuples.
5. Classes, `Option`, `weak`, `List`/`Map`/`Set`, places with the mutation guard, drop order and `defer` (slice 3, built).
6. `Result`/`Option`, `?`, `or` with handler blocks, `Error` with context (slice 5, built).
7. Generics, interfaces, `Drop` hooks (slice 4a, built), then closures and iteration (slice 4b, built).
   The numeric tower: sized integers, `f32`, conversions, bitwise operators (slice 6a, built).
8. Embedding API: `register_fn`, exported host types, generational handles, `.cbi`, capabilities.
   Part A (host functions, `.cbi`, capabilities, instruction budget, callbacks) is built (slice 7a);
   part B (exported host types, generational handles, `colibri-bind`) is built (slice 7b).
9. Packages and `import` (built, slice 8: ARCHITECTURE §11); `colibri check`, minimal LSP,
   `colibri test`, `colibri fmt` (built: ARCHITECTURE §12); core std.
10. Milestone: a small Rust program with its own game loop running gameplay logic from Colibri.

Large steps may split into several PRs.
