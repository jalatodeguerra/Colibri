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
//! | `host_random(ptr, len)` | fills the memory with random bytes (`crypto.getRandomValues`) |
//!
//! A panic of the engine itself (a bug, not a Colibri panic) is written to the error stream and
//! ends the instance: nothing can be called after it.

pub mod runner;

#[cfg(target_family = "wasm")]
mod ffi;

pub use runner::{Runner, Sink, State};
