# host_game

A headless Rust game loop that runs its gameplay from Colibri scripts: the phase 1 milestone
(DESIGN §17.5, EMBEDDING §7).

The host (`src/lib.rs`) owns the world and the clock: a fixed 60 ticks per second of simulated
time, no wall clock, no randomness, so the output is identical on every run. The script
(`scripts/game.colibri`) decides what happens. It shows:

- `#[colibri::export] struct Enemy`, a host class the host owns (`vm.insert` handles; `despawn`
  destroys it, and `alive()` tells a script whether its reference is still good), and
  `#[colibri::export(value)] struct Vec2`, a copy type that crosses as a Colibri struct.
- Host functions with host state reached through `Ctx`: `spawn_enemy`, `enemies`, `despawn`,
  `player`, `move_toward`, `distance`, `spawn_point` (a `Result`), `log`, and `on_timer(secs, f)`,
  whose callbacks the host keeps (`Callback`) and fires from its loop.
- Hooks found with `vm.find_fn`: `init()`, `update(dt: f32)` (required) and `on_hit(e, dmg)`
  (optional; a script without it is simply not told about hits).
- A script `class`, `enum`, `match`, a closure passed to `on_timer`, a `List`, `or` for a
  fallback, and a module-level `var` that holds the score.
- An instruction budget for every entry into the script (`STEP_BUDGET`). A script that panics or
  runs out of budget is logged and the game goes on. The summary line reports the largest number
  of steps one entry used (`max_steps`).

## Run

```sh
cargo run -p colibri-example-host-game        # 600 ticks (10 s)
cargo run -p colibri-example-host-game -- 120 # fewer ticks
cargo test -p colibri-example-host-game
```

Running it also rewrites `scripts/host.cbi` if the registered host API changed. That file is
committed, and a test fails if it goes stale. It is what lets the editor and the checker know
the host API without the host running:

```sh
colibri check --interface examples/host_game/scripts/host.cbi examples/host_game/scripts/game.colibri
colibri fmt --check examples/host_game/scripts
```

## Tests

`tests/host_game.rs` runs 600 ticks in-process and compares the log with `tests/expected.txt`
(`UPDATE_EXPECTED=1 cargo test` rewrites it after an intended change), checks `host.cbi` against
`vm.interface_text()`, and covers a script that panics on tick 5, a script that never returns, and
a script without `on_hit`.
