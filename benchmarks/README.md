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
| `structs` | value-type structs updated in place through `var self` methods |
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

## Baseline

The tree-walking interpreter on 2026-11-19 (Linux x86-64, CPython 3.13), in milliseconds:

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

### Map hashing

`map` after the keys of `Map` and `Set` got a cheap hash (an FxHash-style fold with a full
finalizer, and an inner table that uses it as it is instead of hashing it again with SipHash;
`colibri-interp/src/ordered.rs`): about 490 to 510 ms against 520 to 530 ms for the same
program before the change, best of three, three runs each, in one session. Both binaries were
built with the release profile but `lto = off` and 16 codegen units (to keep the build short on
a shared machine), so they are comparable with each other and not with the table above, whose
`map` row (572 ms) is the fat-LTO build on another day. The program spends most of its time in
the interpreter loop and in building the `"key{i}"` strings, so the hash is a small share of it.

The profile of the tree-walker is flat, so these numbers are not worth tuning: the VM replaces it.
The programs are micro-benchmarks, so read the ratios as a rough guide.
