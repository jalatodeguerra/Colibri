# Findings from writing the scripts

What got in the way while writing `organize` and `report`. Severity: high blocks a natural
program, medium forces a workaround, low is rough edges. "Fixed" ones have a golden test.

1. **`fs.modified` did not exist** (medium, fixed). The date mode needs a modification time and
   `fs` only had `size`. Added `fs.modified(path) -> Result[float]`, unix seconds, with the
   golden `std/fs_modified`. `fs.stat` and `fs.set_modified` were added later (fixed).
2. **No way to write a diverging function** (medium). `fn die(msg: str) -> !` is a syntax error
   (`expected a type, found !`), and `os.exit(2)` in a helper leaves its caller's flow
   unchanged, so every call site needs a dead `return ok(())` after `usage_error(..)`, and
   `let mode = match x { .. other => usage_error(..) }` is not possible. A generic
   `fn f[T]() -> T { os.exit(2) }` fails too ("must return `T`"). Suggest: a `!` return type in
   source (the checker already has `Ty::Never`), and give `os.exit` that type.
3. **`join_all` and `join` do not work from `main`** (medium). `main` cannot wait for a
   `CoGroup`, so `fetch_all` hand-rolls `while group.count() > 0 { task.tick(0.0) }`. That is
   a correct idiom but not an obvious one (the error says "can only pause a coroutine").
   Suggest `group.run()` / `task.run_until(c)`: drive the event loop until the group or
   coroutine finishes, allowed on the main fiber.
4. **`http` blocks the Vm, so the report's coroutines never overlap** (medium, known:
   ARCHITECTURE §13). The worker pool is structural only; documented in the README. A
   non-blocking `http.get` that a coroutine can suspend on (or a `http.spawn` returning a
   handle that `try_wait`s, like `process`) would make `--concurrency` real.
5. **No safe "remove an empty folder"** (medium, fixed: `fs.remove_empty_dir`, `fs.walk`). `fs.remove_dir` is recursive
   (`remove_dir_all`), so the organiser cannot prune the folders it emptied without risking
   data. Suggest `fs.remove_empty_dir` (or a non-recursive `remove_dir` plus
   `remove_dir_all`), and `fs.walk(path)` returning relative paths (the script recurses by hand).
6. **`n.to_float()` is a natural guess and the error did not say what to write** (low, fixed).
   It listed the methods of `int`; now it says to write `float(x)` / `int(x)`.
7. **`str.pad_right(s, width, fill)` has no default and wants a `char`** (low, fixed: `s.pad_right(w)`, `rfind`, `split_once`, `rsplit_once`). Host
   functions have no default arguments, so padding needs `' '` every time; the format spec
   (`{name:<14}`) is what the scripts use instead. A `str` method (`s.pad_right(14)`) would be
   the idiom. Likewise `str` lacks `rfind`/`rsplit`/`last index`: the report resolves item URLs
   with `split("/")`, `pop`, `join("/")`.
8. **Empty collection literals** (low). `{}` is an empty block (the error explains and
   suggests `{:}`), `Set()` needs `Set[str]()` (also explained). Both messages are good; the
   forms are still a trap for people coming from Python. No change suggested.
9. **Errors are built with `Error.from("text")`** (low). `err(Error.from(".."))` is
   verbose for the most common "this failed" return in a script; suggest `error("..")`
   as a prelude function or `err("..")` coercing a `str` for `Result[T]`.
10. **`sort_by` wants an `int` comparator** (low). Sorting by a float descending, with a name
    tie-break, is a six-line closure with `-1`/`1`; `sort_by_key` with a negated key does not
    give the tie-break. Suggest an `Ordering` result or `cmp(a, b)` on ordered types.
11. **`{x:.2}` rounds ties to even** (low, informational). `54.125` prints `54.12`, as
    in Rust and C. Worth one line in the format-spec docs because report scripts hit it.
12. **Things that worked without friction**: typed `json.decode` of maps and structs with
    path-carrying errors (`$.images`), `.context(..)` chains from `main`, `args.positional_with`,
    `regex`, `datetime.format(from_unix(..))`, `process.output` and its failure as a `Result`,
    mutation through `totals[i].files += 1`, `if let .. else` as an expression, shared class
    objects between coroutines, `CoGroup.go`, and the `colibri check` messages.
