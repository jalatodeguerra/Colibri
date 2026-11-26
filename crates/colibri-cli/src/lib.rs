//! Support code for the `colibri` command line tool.

pub mod assets;
pub mod buildcmd;
pub mod checkcmd;
pub mod debugcmd;
pub mod depscmd;
pub mod doccmd;
mod flame;
pub mod fmt;
pub mod golden;
pub mod inflate;
pub mod leakcmd;
#[doc(hidden)]
pub mod macho;
pub mod newcmd;
pub mod pack;
pub mod platform;
pub mod profilecmd;
pub mod repl;
mod repl_input;
pub mod runcmd;
pub mod testcmd;
pub mod watch;
pub mod webbuild;

use std::thread;

/// The stack of the thread that runs Colibri code. Colibri calls keep their frames on the heap,
/// but a host function that calls back into Colibri, and the deep operations on values (`show`,
/// `==`, `clone`), recurse on the Rust stack; this is enough for the default call-depth limit
/// even in an unoptimised build, so the limit (a deterministic "stack overflow" panic) always
/// fires before the Rust stack can overflow.
pub const STACK_SIZE: usize = 256 * 1024 * 1024;

/// What the engine may use of that stack: everything but a margin for the rest of the
/// program.
pub const NATIVE_STACK_BUDGET: usize = STACK_SIZE - 16 * 1024 * 1024;

static BUILD_CFG: std::sync::OnceLock<colibri_runtime::BuildCfg> = std::sync::OnceLock::new();

/// Sets what `@when` conditions are decided with for this process (`--feature`, `--release`,
/// `--no-default-features`); `main` calls it once, before anything is loaded.
pub fn set_build_cfg(cfg: colibri_runtime::BuildCfg) {
    let _ = BUILD_CFG.set(cfg);
}

/// What `@when` conditions are decided with: the command line's flags, or the defaults (this
/// platform, a debug build, the manifest's default features).
pub fn build_cfg() -> colibri_runtime::BuildCfg {
    BUILD_CFG.get().cloned().unwrap_or_default()
}

/// A `Vm` builder set up for a thread made by [`on_big_stack`].
pub fn vm_builder() -> colibri_runtime::VmBuilder {
    colibri_runtime::Vm::builder()
        .build_cfg(build_cfg())
        .max_call_depth(colibri_runtime::DEFAULT_MAX_CALL_DEPTH)
        .native_stack_budget(NATIVE_STACK_BUDGET)
        // What the main thread and the workers of thread tasks print goes through one writer,
        // whole lines at a time, so that their lines never tear each other. A command that
        // collects the output replaces it with `stdout_shared` of its own.
        .stdout_shared(Box::new(std::io::stdout()))
}

/// Builds the `Vm` with the standard library installed (`import math`, `import fs`, ...).
/// `args` is what the script's `os.args()` returns.
pub fn build_vm(builder: colibri_runtime::VmBuilder, args: Vec<String>) -> colibri_runtime::Vm {
    build_vm_with_assets(builder, args, None)
}

/// [`build_vm`] for a program whose `import assets` reads from what `assets` makes (see
/// [`assets`]); the workers of thread tasks call it too, each for a source of its own.
pub fn build_vm_with_assets(
    builder: colibri_runtime::VmBuilder,
    args: Vec<String>,
    assets: Option<colibri_std::Factory<Box<dyn colibri_std::AssetSource>>>,
) -> colibri_runtime::Vm {
    let config = colibri_std::StdConfig {
        args,
        assets: assets.as_ref().map(|make| make()),
        ..colibri_std::StdConfig::default()
    };
    let mut worker = colibri_std::WorkerStd::from_config(&config);
    if let Some(make) = assets {
        worker = worker.with_assets(move || make());
    }
    build_vm_with_worker_std(builder, config, worker)
}

/// [`build_vm`] with the whole standard library configuration given. The workers of thread
/// tasks get the plain settings of `config` and the machine's clock (see
/// [`build_vm_with_worker_std`] for more).
pub fn build_vm_with_std(
    builder: colibri_runtime::VmBuilder,
    config: colibri_std::StdConfig,
) -> colibri_runtime::Vm {
    let worker = colibri_std::WorkerStd::from_config(&config);
    build_vm_with_worker_std(builder, config, worker)
}

/// [`build_vm_with_std`] with what the workers of thread tasks see given too: a thread task's
/// isolate gets the library installed with `worker`'s settings and every capability the `Vm` was
/// given, so what it reads of the world is the main thread's.
pub fn build_vm_with_worker_std(
    builder: colibri_runtime::VmBuilder,
    config: colibri_std::StdConfig,
    worker: colibri_std::WorkerStd,
) -> colibri_runtime::Vm {
    let mut vm = builder
        .worker_setup(move |isolate| {
            colibri_std::install_worker(isolate, &worker)
                .expect("the standard library registers into a new Vm");
        })
        .build();
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
