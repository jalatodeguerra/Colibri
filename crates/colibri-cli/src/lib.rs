//! Support code for the `colibri` command line tool.

pub mod fmt;
pub mod golden;

use std::thread;

/// The stack of the thread that runs Colibri code. Each active Colibri call uses Rust stack
/// in the tree-walking interpreter; this is enough for the default call-depth limit even in
/// an unoptimised build, so the limit (a deterministic "stack overflow" panic) always fires
/// before the Rust stack can overflow.
pub const STACK_SIZE: usize = 256 * 1024 * 1024;

/// What the interpreter may use of that stack: everything but a margin for the rest of the
/// program.
pub const NATIVE_STACK_BUDGET: usize = STACK_SIZE - 16 * 1024 * 1024;

/// A `Vm` builder set up for a thread made by [`on_big_stack`].
pub fn vm_builder() -> colibri_runtime::VmBuilder {
    colibri_runtime::Vm::builder()
        .max_call_depth(colibri_runtime::DEFAULT_MAX_CALL_DEPTH)
        .native_stack_budget(NATIVE_STACK_BUDGET)
}

/// Runs `f` on a thread with a [`STACK_SIZE`] stack and returns its result.
pub fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let handle = thread::Builder::new()
        .name("colibri".to_string())
        .stack_size(STACK_SIZE)
        .spawn(f)
        .expect("the operating system can start a thread");
    match handle.join() {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
