# host_jobs

A headless Rust job runner that takes its job logic from Colibri scripts: an example of embedding
Colibri in a Rust program.

The host (`src/lib.rs`) owns the job queue, a worker and the clock. The scheduler advances in
fixed ticks (60 per second of simulated time), with no wall clock and no randomness, so the
output is identical on every run. The script (`scripts/jobs.colibri`) decides what happens. It
shows:

- `#[colibri::export] struct Job`, a host class the host owns (`vm.insert` handles; `finish`
  destroys it, and `alive()` tells a script whether its reference is still good), and
  `#[colibri::export(value)] struct Load`, a copy type that crosses as a Colibri struct.
- Host functions with host state reached through `Ctx`: `submit`, `jobs`, `finish`, `baseline`,
  `ease`, `gap`, `template` (a `Result`), `log`, and `on_timer(secs, f)`, whose callbacks the host
  keeps (`Callback`) and fires when it advances the scheduler.
- Hooks found with `vm.find_fn`: `init()`, `update(dt: f32)` (required) and
  `on_progress(job, units)` (optional; a script without it is simply not told about the worker's
  progress).
- A script `class`, `enum`, `match`, a closure passed to `on_timer`, a `List`, `or` for a
  fallback, and a module-level `let mut` that holds the balance.
- An instruction budget for every entry into the script (`STEP_BUDGET`). A script that panics or
  runs out of budget is logged and the host goes on. The summary line reports the largest number
  of steps one entry used (`max_steps`).

## Run

```sh
cargo run -p colibri-example-host-jobs        # 600 ticks (10 s of simulated time)
cargo run -p colibri-example-host-jobs -- 120 # fewer ticks
cargo test -p colibri-example-host-jobs
```

Running it also rewrites `scripts/host.cbi` if the registered host API changed. That file is
committed, and a test fails if it goes stale. It is what lets the editor and the checker know
the host API without the host running:

```sh
colibri check --interface examples/host_jobs/scripts/host.cbi examples/host_jobs/scripts/jobs.colibri
colibri fmt --check examples/host_jobs/scripts
```

## Tests

`tests/host_jobs.rs` runs 600 ticks in-process and compares the log with `tests/expected.txt`
(`UPDATE_EXPECTED=1 cargo test` rewrites it after an intended change), checks `host.cbi` against
`vm.interface_text()`, and covers a script that panics on tick 5, a script that never returns, and
a script without `on_progress`.
