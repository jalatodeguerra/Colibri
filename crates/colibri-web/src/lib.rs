//! The web runner: `colibri.wasm`, which runs a pack (a program written by `colibri build --web`)
//! in a page, driven by the page's own timers (`colibri.js`).
//!
//! There is no `wasm-bindgen`: the interface is a few C functions, so the only tool a build needs
//! is `rustup target add wasm32-unknown-unknown`.
//!
//! **Exports** (see [`ffi`]), all called from the page's one thread:
//!
//! | function | what it does |
//! |---|---|
//! | `colibri_alloc(len) -> ptr`, `colibri_free(ptr, len)` | memory for the host to write the pack into |
//! | `colibri_start(ptr, len) -> state` | loads the pack and starts the program, up to its first pause |
//! | `colibri_tick(dt) -> state` | `dt` seconds have passed: resumes the coroutines that are due |
//! | `colibri_next_deadline() -> f64` | milliseconds until the next tick is needed; `-1` for never |
//! | `colibri_exit_code() -> i32` | `0`, `101` after a panic, `1` after another failure |
//!
//! A state is `0` running, `1` done, `2` failed.
//!
//! **Imports** (module `env`), which the host page defines:
//!
//! | function | what it gives |
//! |---|---|
//! | `host_write(fd, ptr, len)` | text for standard output (`fd` 1) or the error stream (`fd` 2), UTF-8, in pieces |
//! | `host_now_real() -> f64` | milliseconds on a monotonic clock, any origin (`performance.now()`) |
//! | `host_now_unix() -> f64` | seconds since 1970 (`Date.now() / 1000`) |
//! | `host_utc_offset(unix) -> f64` | the page's time zone at that instant, in seconds east of UTC (`-new Date(unix * 1000).getTimezoneOffset() * 60`); anything that is not a number of seconds within a day is read as UTC |
//! | `host_random(ptr, len)` | fills the memory with random bytes (`crypto.getRandomValues`); called by `colibri_start` (the seed of the hashing of maps and sets) and by the program's `rand` |
//!
//! **Imports must not call back into the module.** Every import runs while the runner is borrowed
//! (`host_write` and `host_random` inside `colibri_tick`, the clocks inside `tick` and
//! `colibri_next_deadline`; `host_random` is also called once at the top of `colibri_start`, to
//! seed the hashing of maps and sets, before the runner exists): an import that calls an export makes the module panic, which traps. A
//! host that wants to react to output does so after the export returns. The exports are not
//! re-entrant either, and the module is single-threaded.
//!
//! A panic of the engine itself (a bug, not a Colibri panic) is written to the error stream and
//! ends the instance: nothing can be called after it.

pub mod runner;

#[cfg(target_family = "wasm")]
mod ffi;

pub use runner::{Runner, Sink, State};
