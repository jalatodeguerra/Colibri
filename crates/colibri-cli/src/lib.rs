//! Support code for the `colibri` command line tool.

pub mod fmt;
pub mod golden;
pub mod leakcmd;
pub mod repl;
mod repl_input;
pub mod testcmd;
pub mod watch;

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

/// Builds the `Vm` with the standard library installed (`import math`, `import fs`, ...).
/// `args` is what the script's `os.args()` returns.
pub fn build_vm(builder: colibri_runtime::VmBuilder, args: Vec<String>) -> colibri_runtime::Vm {
    let mut vm = builder.build();
    let config = colibri_std::StdConfig {
        args,
        ..colibri_std::StdConfig::default()
    };
    colibri_std::install_with(&mut vm, config)
        .expect("the standard library registers into a new Vm");
    vm
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
