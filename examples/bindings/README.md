# Rust bindings example

A package whose Colibri program calls a Rust crate of its own (`native/`), which wraps
`unicode-width` and `unicode-segmentation` for display widths and grapheme splitting and keeps a
`Table` class on the Rust side.

| File | What it is |
|------|------------|
| `colibri.toml` | the package; `native = { path = "native/" }` under `[rust-dependencies]` |
| `native/` | the Rust crate (`#[colibri::export]` functions and the `Table` class) |
| `native.cbi` | the interface of `native`, written by `colibri bindings`; commit it, so that `colibri check` and editors need no Rust |
| `colibri.rust.lock` | the crate versions cargo chose for the runner; commit it |
| `main.colibri` | prints an aligned table of mixed-script text (`expected.txt` is its output) |
| `tests.colibri` | `colibri test`: a Rust `Err` as `err`, a Rust panic caught with `join_result` |
| `ticker.colibri` | runs until stopped; the watch smoke test edits it |
| `smoke.sh` | everything CI runs on each platform |

```console
$ colibri run examples/bindings        # builds the runner the first time (minutes), then instant
$ colibri test examples/bindings
$ colibri build examples/bindings -o demo   # one executable, no Rust needed where it runs
```

The first `run` compiles Colibri's engine with the crate (`COLIBRI_SOURCE` names a checkout when
`colibri` was not built from one). `bash examples/bindings/smoke.sh PATH-TO-COLIBRI` runs the
whole check, including the size gate (the built program must stay under 15,000,000 bytes).
