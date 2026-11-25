# Benchmarks

Eight small programs, each written in Colibri and in Python, that exercise one thing each. They
are the benchmark set behind the VM speed target: phase 3 is
finished when the bytecode VM is faster than CPython 3.13 on every one of them (stage 1); within
1.5× of Lua 5.4 is a later stage, and needs Lua versions of the same programs.

| Program | What it measures |
|---------|------------------|
| `fib` | recursive function calls |
| `loop` | integer arithmetic in a counted loop |
| `list` | a list of a million integers: push, index, iterate |
| `map` | maps with integer keys and with string keys |
| `structs` | value-type structs updated in place through `mut self` methods |
| `objs` | class objects: building and walking a linked list |
| `closures` | closures through `map` and `filter` |
| `strings` | format, join, split and prefix tests |

Run them with:

```sh
cargo build --release
benchmarks/run.sh
```

The script checks that Colibri and Python print the same result, then reports the best of three
wall-clock times. Times include start-up and type checking.

The two programs in `coroutines/` (`many`: many short coroutines; `pingpong`: two coroutines
that hand control back and forth) have no Python version and are not run by `run.sh`. Count their
instructions with `valgrind --tool=cachegrind --cache-sim=no target/release/colibri run benchmarks/coroutines/many.colibri`.

`benchmarks/cg.sh` prints the instructions (cachegrind `I refs`) the VM executes for `fib` and
`loop` (or the benchmarks named on the command line), where wall time is too noisy to compare.

## Baseline

The first engine, which the bytecode VM replaced, on 2026-11-19 (Linux x86-64, CPython 3.13),
in milliseconds:

| Program | Colibri | CPython | Colibri is slower by |
|---------|--------:|--------:|---------------------:|
| `fib` | 1,000 | 105 | 9.5× |
| `loop` | 1,057 | 181 | 5.8× |
| `list` | 701 | 107 | 6.6× |
| `map` | 572 | 106 | 5.4× |
| `structs` | 245 | 35 | 7.0× |
| `objs` | 1,413 | 220 | 6.4× |
| `closures` | 917 | 169 | 5.4× |
| `strings` | 238 | 63 | 3.8× |

## Bytecode VM

The bytecode VM on 2026-11-25 (Linux x86-64, fat LTO, CPython 3.13, the
lowest of three runs of best-of-five, machine load below 1.5), in milliseconds:

| Program | VM | CPython | VM is |
|---------|---:|--------:|------:|
| `fib` | 65 | 83 | faster |
| `loop` | 34 | 161 | faster |
| `list` | 50 | 79 | faster |
| `map` | 61 | 78 | faster |
| `structs` | 26 | 27 | level |
| `objs` | 138 | 187 | faster |
| `closures` | 70 | 133 | faster |
| `strings` | 27 | 43 | faster |

`objs` got its margin from an unboxed `T?` for a class reference: `some(obj)` is the object
itself in the VM (`Value::SomeObj`), with no `Option` box allocated per link of the list.

Simple lambdas given to the list built-ins (`map`, `filter`, `any`, `all`, `find`, `find_index`,
`fold`), those that only compute over their arguments, run as native loops. Closure code that
captures, calls or prints runs in the bytecode loop like any other code and is slower than
CPython there; the `closures` program above is mostly the first kind. `for mut` over a list of
structs moves each element into the binding instead of copying it.

### Map hashing

The keys of `Map` and `Set` get a cheap hash (an FxHash-style fold with a full finalizer, and an
inner table that uses it as it is instead of hashing it again with SipHash;
`crates/colibri-core/src/ordered.rs`).

The programs are micro-benchmarks, so read the ratios as a rough guide.
