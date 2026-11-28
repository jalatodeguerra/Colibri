//! The virtual machine a host embeds.

use std::collections::HashMap;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use colibri_check::HostEnv;
use colibri_check::hir::{HostFnId, Program, TestDef};
use colibri_check::host::{DEFAULT_MODULE, float_literal, is_module_name, is_std_module_name};
use colibri_check::package::{BuildCfg, DiskFs, Entry, MemoryFs, RecordingFs, SourceFs};
use colibri_check::types::{RESULT_TYPE, Ty, VARIANT_ERR};
use colibri_core::clock::Clock;
use colibri_core::host::HostFault;
use colibri_core::value::Value as InterpValue;
use colibri_syntax::{Diagnostic, SourceMap};

use crate::cap::{Cap, CapSet};
use crate::convert::{Args, FromColibri, Value};
use crate::ctx::{Caller, Ctx, panic_error};
use crate::engine::{ClassHooks, HostCx, HostFnImpl, RuntimeEngine};
use crate::engine_vm;
use crate::error::{Error, Report};
use crate::func::Fn;
use crate::host::{HostFn, Registration, payload_message};
use crate::iface::{self, ConstEntry, ConstValue, HostFnEntry, Rendered};
use crate::reload::{Fingerprint, OwnedEntry, ReloadOptions, ReloadOutcome, ReloadState, reads_of};
use crate::tasks::{
    Launch, LineWriter, PendingTask, PoolStats, ProgramImage, SharedOut, Submission, TaskDone,
    TaskFailure, TaskPool, WorkerCfg, WorkerLaunch, WorkerOut, WorkerSetup,
};
use crate::types::{TypeDesc, TypeRegistry};
use colibri_core::leaks::Leak;
use colibri_core::taskrun::TaskSpawner;

/// The default limit on nested Colibri calls (DESIGN §10.4 leaves the number open).
pub const DEFAULT_MAX_CALL_DEPTH: usize = 10_000;

static NEXT_VM_ID: AtomicU64 = AtomicU64::new(0);

/// What `load_entry` does after checking.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadMode {
    /// Initialise the module and run the top-level statements.
    Run,
    /// Check every module of a package and run nothing ([`Vm::load_tests`]).
    Tests,
    /// Initialise the modules and run nothing else: the top-level statements and `main` are
    /// started by [`Vm::start_entry`] as coroutines ([`Vm::load_from_deferred`]).
    Deferred,
    /// Check a REPL session program, with every value in dependency order (it is not one file
    /// read top to bottom); the session runs it ([`Vm::session_commit`]).
    Session,
}

mod session;
mod worker;
pub use session::{SessionCheck, SessionError};

/// See [`Vm::on_warnings`].
type WarningHook = Box<dyn FnMut(&Report)>;
/// Called before each test of [`Vm::run_test`] ([`Vm::on_test_start`]).
type TestStartHook = Box<dyn FnMut(&mut Vm, &TestInfo)>;

/// A program that was read and checked, not yet loaded.
struct Checked {
    sources: SourceMap,
    program: Program,
    warnings: Vec<Diagnostic>,
    /// The interface text of each host module the program was checked against.
    iface: Vec<(String, String)>,
    /// What a thread task's worker builds its isolate from (not for sessions and test loads).
    image: Option<ProgramImage>,
}

/// A `test` block of the loaded program ([`Vm::tests`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestInfo {
    /// What [`Vm::run_test`] takes: the index in [`Vm::tests`].
    pub id: usize,
    /// The name written after `test`.
    pub name: String,
    /// The module it is in: the file name without `.colibri` for the entry file, the dotted
    /// module path otherwise (`util.strings`, a dependency's name first).
    pub module: String,
    /// In the program's own modules, not in a dependency's.
    pub root: bool,
    /// The file (as diagnostics name it) and the 1-based line of the block.
    pub file: String,
    pub line: u32,
}

/// Names a host function cannot take: they are built into the language.
const RESERVED_NAMES: &[&str] = &[
    "print",
    "panic",
    "assert",
    "assert_eq",
    "some",
    "ok",
    "err",
    "wait",
    "wait_until",
    "wait_real",
    "wait_for",
];

/// Configures a [`Vm`].
pub struct VmBuilder {
    stdout: Option<Box<dyn Write>>,
    max_call_depth: usize,
    native_stack_budget: usize,
    trace_drops: bool,
    track_leaks: bool,
    caps: CapSet,
    instruction_budget: Option<u64>,
    memory_limit: Option<u64>,
    coroutine_stack_size: Option<usize>,
    clock: Option<Box<dyn Clock>>,
    cfg: BuildCfg,
    compile_all: bool,
    safepoint_slice: Option<u32>,
    worker_threads: Option<usize>,
    worker_extra_cap: Option<usize>,
    worker_setup: Option<WorkerSetup>,
    shared_out: Option<SharedOut>,
}

impl VmBuilder {
    /// Turns the feature flag `name` on for the program's `@when(name)` items (DESIGN §3.6).
    /// With a `colibri.toml` the name must be declared in its `[features]` table.
    pub fn feature(mut self, name: &str) -> Self {
        self.cfg = self.cfg.with_feature(name);
        self
    }

    /// A release build: `@when(debug)` items are left out. The default is a debug build.
    pub fn release(mut self) -> Self {
        self.cfg = self.cfg.release();
        self
    }

    /// Sets every value `@when` conditions are decided with: the operating system, the
    /// architecture, `debug` and the feature flags ([`BuildCfg`]). The default is the platform
    /// the host runs on, a debug build and no flags beyond a manifest's `default` features.
    pub fn build_cfg(mut self, cfg: BuildCfg) -> Self {
        self.cfg = cfg;
        self
    }

    /// The size of the native stack of each coroutine (`go`), in bytes. The default is 16 MiB
    /// (DESIGN §14.3); the memory is reserved lazily and committed as the coroutine uses it.
    /// The evaluator panics with "stack overflow" before a coroutine uses more than three
    /// quarters of it, so deep recursion inside a coroutine needs a bigger size.
    ///
    /// On the bytecode VM a coroutine's own code lives on the heap, so this size is the one of the
    /// native stack that a host function runs on when a coroutine calls it (a function that is
    /// not marked [`Registration::no_await`]; the default is 1 MiB). A host function that needs
    /// more native stack than that, with deep recursion or large buffers on the stack, crashes
    /// the process with a stack overflow: raise the size, or call the function from the main
    /// fiber. The stacks are reserved lazily, a few are kept for reuse, and the engine measures
    /// callbacks into Colibri against three quarters of the size.
    pub fn coroutine_stack_size(mut self, bytes: usize) -> Self {
        self.coroutine_stack_size = Some(bytes);
        self
    }

    /// The clock behind `wait_real` and the event loop ([`Vm::run_event_loop`]). The default
    /// is the wall clock; tests inject a fake one to be deterministic.
    pub fn clock(mut self, clock: impl Clock + 'static) -> Self {
        self.clock = Some(Box::new(clock));
        self
    }

    /// Records the label of every class object as it is destroyed, for tests of drop order
    /// (see [`Vm::take_drop_trace`]). Off by default.
    #[doc(hidden)]
    pub fn trace_drops(mut self, on: bool) -> Self {
        self.trace_drops = on;
        self
    }

    /// Turns on the leak detector's registry: every class object and closure made from now on
    /// is recorded (weakly, with where it was made) so that [`Vm::find_leaks`] can find
    /// reference cycles that nothing reaches. Off by default; a `Vm` that does not track pays
    /// one `Option` test per object made. See ARCHITECTURE §18.
    pub fn track_leaks(mut self, on: bool) -> Self {
        self.track_leaks = on;
        self
    }

    /// Where `print` writes. The default is the process's standard output. On the web build
    /// (`wasm32-unknown-unknown`) standard output goes nowhere, so a host must set a writer.
    pub fn stdout(mut self, writer: Box<dyn Write>) -> Self {
        self.stdout = Some(writer);
        self
    }

    /// How many Colibri calls may be active at once before a "stack overflow" panic. Colibri
    /// calls live in a heap frame stack and use no Rust stack.
    pub fn max_call_depth(mut self, depth: usize) -> Self {
        self.max_call_depth = depth.max(1);
        self
    }

    /// How many bytes of Rust stack the engine may use below the point where the host calls
    /// in (mostly host calls that call back into Colibri), before a deep recursion panics with
    /// "stack overflow" instead of crashing the host. The default is 1 MiB, which fits a default test thread; on a thread with a
    /// bigger stack raise it to a little below that stack's size, together with
    /// [`VmBuilder::max_call_depth`].
    pub fn native_stack_budget(mut self, bytes: usize) -> Self {
        self.native_stack_budget = bytes;
        self
    }

    /// Lets scripts use host items that need this capability (and `extern` blocks, for
    /// [`Cap::Ffi`]). The default is none (EMBEDDING §5).
    pub fn allow(mut self, cap: Cap) -> Self {
        self.caps.insert(cap);
        self
    }

    /// Grants every capability: for trusted scripts, and the `colibri` tool.
    pub fn allow_all(mut self) -> Self {
        self.caps = CapSet::all();
        self
    }

    /// A limit on the work one entry from the host may do: `call`, `load` and `run_main` each
    /// get this many steps. A step is a statement, a call, a loop iteration, or an element
    /// visited by a looping built-in; the count is approximate and engine-specific, and the
    /// only guarantee is that unbounded work runs out. See [`Vm::set_instruction_budget`].
    pub fn instruction_budget(mut self, steps: u64) -> Self {
        self.instruction_budget = Some(steps);
        self
    }

    /// Makes the bytecode VM compile every function when a program is loaded, patched or
    /// extended, instead of at its first call, and panic on a function that does not compile
    /// (a bug: the checker accepted the program). For test suites, so that such a bug surfaces
    /// at once and not in the one run that reaches the function.
    #[doc(hidden)]
    pub fn compile_all_at_load(mut self, on: bool) -> Self {
        self.compile_all = on;
        self
    }

    /// Limits the memory this `Vm`'s entries may hold to `bytes` (approximately: see
    /// [`Vm::set_memory_limit`]). **The program must have installed the counting allocator**
    /// ([`crate::CountingAlloc`]) as its global allocator, or [`VmBuilder::build`] panics, with a
    /// message that says how.
    pub fn memory_limit(mut self, bytes: u64) -> Self {
        self.memory_limit = Some(bytes);
        self
    }

    /// Counts the budget in slices of `steps` and runs a (test-only) safepoint at each end, so
    /// that tests can check that nothing observable depends on the slice. See
    /// [`Vm::safepoints_run`].
    #[doc(hidden)]
    pub fn safepoint_slice(mut self, steps: u32) -> Self {
        self.safepoint_slice = Some(steps);
        self
    }

    /// How many threads run thread tasks (DESIGN §14.1): the default is the number of cores
    /// (`std::thread::available_parallelism`). The threads are not started until the first task
    /// runs, and each builds its own `Vm` of the program on its first task and keeps it.
    /// There are no thread tasks on the web.
    pub fn worker_threads(mut self, threads: usize) -> Self {
        self.worker_threads = Some(threads.max(1));
        self
    }

    /// Lowers the most temporary threads the pool lends to waiting tasks at a time (256 by
    /// default), so that a test reaches the limit without starting 256 threads.
    #[doc(hidden)]
    pub fn worker_extra_cap(mut self, threads: usize) -> Self {
        self.worker_extra_cap = Some(threads);
        self
    }

    /// Registers what thread tasks may call. `setup` runs on the fresh `Vm` of every worker, on
    /// the worker's thread, before the program is loaded into it: it registers host functions,
    /// types and state like the host does for its main `Vm` (the CLI passes
    /// `colibri_std::install` with every capability). A host function that the main `Vm` has and
    /// `setup` does not register is still declared in a worker, so programs check the same
    /// there, but calling it panics: `` `f` is not available in a thread task (the host did not
    /// register it for workers) ``. What `setup` registers must be declared identically on the
    /// main `Vm`, or the worker refuses to start. The capabilities of the main `Vm` apply.
    pub fn worker_setup(
        mut self,
        setup: impl std::ops::Fn(&mut Vm) + Send + Sync + 'static,
    ) -> Self {
        self.worker_setup = Some(Arc::new(setup));
        self
    }

    /// Like [`VmBuilder::stdout`] for a writer that threads may share: `print` in thread tasks
    /// writes to it too, whole lines at a time under a lock. [`VmBuilder::stdout`] takes any
    /// writer, but a writer that is not `Send` cannot be reached from a worker, so thread
    /// tasks write to the process's standard output instead.
    pub fn stdout_shared(mut self, writer: Box<dyn Write + Send>) -> Self {
        self.shared_out = Some(Arc::new(std::sync::Mutex::new(writer)));
        self
    }

    pub fn build(self) -> Vm {
        let (out, worker_out): (Box<dyn Write>, WorkerOut) = match (self.shared_out, self.stdout) {
            (Some(shared), _) => (
                Box::new(LineWriter::new(Arc::clone(&shared))),
                WorkerOut::Shared(shared),
            ),
            (None, Some(writer)) => (writer, WorkerOut::Process),
            (None, None) => (Box::new(std::io::stdout()), WorkerOut::Process),
        };
        let mut engine = engine_vm::new_engine(out, self.max_call_depth, self.native_stack_budget);
        engine.set_compile_all(self.compile_all);
        engine.core_mut().set_trace_drops(self.trace_drops);
        engine.core_mut().set_track_leaks(self.track_leaks);
        engine
            .core_mut()
            .set_instruction_budget(self.instruction_budget);
        if let Some(bytes) = self.memory_limit {
            colibri_core::mem::require_installed("a memory limit");
            engine.core_mut().set_memory_limit(Some(bytes));
        }
        if let Some(every) = self.safepoint_slice.and_then(std::num::NonZeroU32::new) {
            engine
                .core_mut()
                .want_safepoints(colibri_core::safepoint::SafepointKind::Debugger, every);
        }
        if let Some(size) = self.coroutine_stack_size {
            engine.core_mut().sched.set_stack_size(size);
            engine.set_host_stack_size(size);
        }
        if let Some(clock) = self.clock {
            engine.core_mut().sched.set_clock(clock);
        }
        let id = NEXT_VM_ID.fetch_add(1, Ordering::Relaxed);
        engine.core_mut().set_vm_id(id);
        let stats = Arc::new(PoolStats::default());
        let workers = WorkerCfg {
            threads: self.worker_threads,
            threads_supported: !cfg!(target_family = "wasm"),
            setup: self.worker_setup,
            out: worker_out,
            caps: self.caps,
            max_call_depth: self.max_call_depth,
            track_leaks: self.track_leaks,
            max_extra: self
                .worker_extra_cap
                .unwrap_or(crate::tasks::MAX_EXTRA_THREADS),
        };
        let launch = Rc::new(Launch::new(TaskPool::new(workers, Arc::clone(&stats))));
        // On the web there is no pool to hand tasks to: without the coercion to the trait object
        // the pool's code is unreachable, and the linker leaves it out.
        #[cfg(not(target_family = "wasm"))]
        engine
            .core_mut()
            .set_task_spawner(Some(Rc::clone(&launch) as Rc<dyn TaskSpawner>));
        Vm {
            sources: Rc::new(SourceMap::new()),
            engine,
            program: None,
            warnings: Vec::new(),
            warning_hook: None,
            test_start_hook: None,
            id,
            epoch: 0,
            native_stack_budget: self.native_stack_budget,
            caps: self.caps,
            registry: TypeRegistry::new(),
            host_fns: Vec::new(),
            no_await: Vec::new(),
            host_consts: Vec::new(),
            bound: Vec::new(),
            modules: Vec::new(),
            pending: Vec::new(),
            source_modules: Vec::new(),
            bulk: false,
            failed: Vec::new(),
            registering: Vec::new(),
            reload: None,
            entry: None,
            entry_pending: false,
            cfg: self.cfg,
            stats: Arc::clone(&stats),
            worker_launch: None,
            track_leaks: self.track_leaks,
            worker_leaks: Vec::new(),
            launch,
        }
    }
}

/// A Colibri virtual machine: one loaded program, its module-level state and an output writer.
pub struct Vm {
    sources: Rc<SourceMap>,
    engine: Box<dyn RuntimeEngine>,
    program: Option<Rc<Program>>,
    warnings: Vec<Diagnostic>,
    /// Called by a load with the warnings of a program that checked, before any of its code runs.
    warning_hook: Option<WarningHook>,
    /// Called before each test, so the host can reset what it keeps in the `Vm`.
    test_start_hook: Option<TestStartHook>,
    /// Distinguishes this `Vm` from every other, so a handle cannot be used on the wrong one.
    id: u64,
    /// Bumped whenever the loaded program changes, which invalidates function handles.
    epoch: u32,
    native_stack_budget: usize,
    /// What `@when` conditions are decided with.
    cfg: BuildCfg,
    caps: CapSet,
    /// The structs and enums the host's functions mention.
    registry: TypeRegistry,
    /// The registered host functions, in the order of their `HostFnId`s.
    host_fns: Vec<HostFnEntry>,
    /// Standard modules whose functions never wait on anything outside the program, with the
    /// names (`f`, or `Class.*`) of the ones that may (see `Vm::no_await_module`).
    no_await: Vec<(String, Vec<String>)>,
    /// The constants of named modules (`math.PI`).
    host_consts: Vec<ConstEntry>,
    /// The names that `#[colibri::module]` bindings registered under (`__private::bind_module`),
    /// so the same binding cannot register twice, whatever it registers.
    bound: Vec<String>,
    /// The named host modules (`vm.module("physics")`) that have something registered, in the
    /// order they got it. The default `host` module always exists and is not listed.
    modules: Vec<String>,
    /// Standard modules declared with [`Vm::lazy_std_module`] and not registered yet, by their
    /// interface name (`std.fs`). They are in `modules` already, so the order of the modules
    /// does not depend on what a program imports.
    pending: Vec<(String, LazyModule)>,
    /// Standard modules written in Colibri, by interface name (`std.vec`) and text.
    source_modules: Vec<(String, &'static str)>,
    /// A module is being registered in one go: each registration skips the interface check,
    /// and the whole module is checked once at the end.
    bulk: bool,
    /// Lazy modules whose registration failed (or is running), with the reason.
    failed: Vec<(String, String)>,
    /// The lazy modules being registered right now, outermost first: a module that asks for
    /// one of them (through `uses`) is a cycle.
    registering: Vec<String>,
    /// Where the program came from and which files it was built from, for
    /// [`Vm::reload_changed`]. Kept when a load fails, so that fixing the file loads it.
    reload: Option<ReloadState>,
    /// The program's entry, when a host runs it as coroutines ([`Vm::start_entry`]).
    entry: Option<EntryRun>,
    /// A program was loaded by [`Vm::load_from_deferred`] and its entry is not started yet.
    entry_pending: bool,
    /// What the pool counts. Kept here as well so that it can be read after the pool is gone.
    stats: Arc<PoolStats>,
    /// On a worker's `Vm`: where the tasks it starts are queued.
    worker_launch: Option<Rc<WorkerLaunch>>,
    /// Whether the leak detector is on (the builder's `track_leaks`).
    track_leaks: bool,
    /// What the workers' isolates leaked, reported when the pool shut down at [`Vm::shutdown`].
    worker_leaks: Vec<Leak>,
    /// The worker threads, which start with the first task, and what the workers build their
    /// isolates from. The engine holds the other end of it, to submit through. Last, so that
    /// it drops last: the threads are joined after everything else of the `Vm` is gone (see
    /// `Drop for Vm`).
    launch: Rc<Launch>,
}

/// Where the entry of a program started by [`Vm::start_entry`] is.
struct EntryRun {
    stage: EntryStage,
    /// The handle of the coroutine of this stage.
    handle: Option<InterpValue>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryStage {
    /// The top-level statements of a script are running.
    Script,
    /// `main` is running.
    Main,
    /// Both are over.
    Over,
}

/// Registers a module that was declared lazily.
type LazyModule = fn(&mut Vm) -> Result<(), Error>;

/// Marks a `Vm` as registering a module in one go for as long as it lives, also when the
/// registration panics.
struct BulkGuard<'a> {
    vm: &'a mut Vm,
    before: bool,
}

impl<'a> BulkGuard<'a> {
    fn new(vm: &'a mut Vm, module: &str) -> Self {
        let before = std::mem::replace(&mut vm.bulk, true);
        vm.registering.push(module.to_string());
        BulkGuard { vm, before }
    }
}

impl std::ops::Deref for BulkGuard<'_> {
    type Target = Vm;
    fn deref(&self) -> &Vm {
        self.vm
    }
}

impl std::ops::DerefMut for BulkGuard<'_> {
    fn deref_mut(&mut self) -> &mut Vm {
        self.vm
    }
}

impl Drop for BulkGuard<'_> {
    fn drop(&mut self) {
        self.vm.bulk = self.before;
        self.vm.registering.pop();
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        // The engine goes after this, and shuts the program down: the handle must be let go
        // of before.
        self.release_entry();
        // Tasks that have not started never will. The pool, the last field, joins the threads
        // once the rest of the `Vm` is gone.
        self.launch.pool().close();
    }
}

impl Default for Vm {
    fn default() -> Self {
        Vm::new()
    }
}

impl Caller for Vm {
    fn ctx(&mut self) -> Ctx<'_> {
        Ctx::new(&mut *self.engine)
    }
}

impl Vm {
    pub fn builder() -> VmBuilder {
        VmBuilder {
            stdout: None,
            max_call_depth: DEFAULT_MAX_CALL_DEPTH,
            native_stack_budget: colibri_vm::DEFAULT_NATIVE_STACK_BUDGET,
            trace_drops: false,
            track_leaks: false,
            caps: CapSet::none(),
            instruction_budget: None,
            memory_limit: None,
            coroutine_stack_size: None,
            clock: None,
            cfg: BuildCfg::default(),
            compile_all: false,
            safepoint_slice: None,
            worker_threads: None,
            worker_extra_cap: None,
            worker_setup: None,
            shared_out: None,
        }
    }

    /// A `Vm` printing to the process's standard output.
    pub fn new() -> Vm {
        Vm::builder().build()
    }

    // ---- host functions ----

    /// Makes a Rust function or closure callable from scripts as `name`, in the default `host`
    /// module, whose names are in scope without an import (EMBEDDING §1).
    ///
    /// The Colibri signature comes from the Rust one: parameters are [`FromColibri`] (up to
    /// eight, optionally after a leading `&mut Ctx`), the result [`crate::IntoColibri`];
    /// `Result<T, E>` becomes `Result[T]` with the message of `E`. A Rust panic becomes a
    /// Colibri panic. Parameters are named `a`, `b`, `c`, ...; scripts pass them by position.
    ///
    /// Registration is only possible before a program is loaded ([`Error::AlreadyLoaded`]).
    ///
    /// ```
    /// use colibri_runtime as colibri;
    ///
    /// # fn main() -> Result<(), colibri::Error> {
    /// let mut vm = colibri::Vm::new();
    /// vm.register_fn("double", |x: i64| x * 2)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn register_fn<Marker, F: HostFn<Marker>>(
        &mut self,
        name: &str,
        f: F,
    ) -> Result<Registration<'_>, Error> {
        self.register_inner(DEFAULT_MODULE, name, None, f)
    }

    /// Like [`Vm::register_fn`], with real parameter names, so a script may pass arguments by
    /// name (`submit(units: 1, priority: 2)`). One name per Rust parameter (not counting a leading
    /// `&mut Ctx`). `#[colibri::export] fn` generates a call to this.
    ///
    /// ```
    /// use colibri_runtime as colibri;
    ///
    /// # fn main() -> Result<(), colibri::Error> {
    /// let mut vm = colibri::Vm::new();
    /// vm.register_fn_named("area", ["width", "height"], |w: f64, h: f64| w * h)?;
    /// assert!(vm.interface_text().contains("fn area(width: float, height: float) -> float"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn register_fn_named<Marker, F: HostFn<Marker>, const N: usize>(
        &mut self,
        name: &str,
        params: [&str; N],
        f: F,
    ) -> Result<Registration<'_>, Error> {
        let names = params.iter().map(ToString::to_string).collect();
        self.register_inner(DEFAULT_MODULE, name, Some(names), f)
    }

    /// The host module `name`, to register functions and types into. A script uses them after
    /// `import name` (`physics.step()`, or `import physics.{step}`), while what goes into the
    /// default module ([`Vm::register_fn`]) is in scope everywhere without an import. The module
    /// exists once something is registered into it.
    ///
    /// ```
    /// use colibri_runtime as colibri;
    ///
    /// # fn main() -> Result<(), colibri::Error> {
    /// let out = colibri::SharedBuffer::new();
    /// let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    /// vm.module("physics").register_fn("gravity", || 9.81_f64)?;
    /// vm.load_source("a.colibri", "import physics\nprint(physics.gravity())\n")?;
    /// assert_eq!(out.contents(), "9.81\n");
    /// # Ok(())
    /// # }
    /// ```
    pub fn module(&mut self, name: &str) -> HostModule<'_> {
        let _ = self.materialize(name);
        HostModule {
            vm: self,
            name: name.to_string(),
            std: false,
        }
    }

    /// Gives back a name that `claim_binding` took, after a registration that failed.
    pub(crate) fn release_binding(&mut self, name: &str) {
        self.bound.retain(|b| b != name);
    }

    /// Claims `name` for one `#[colibri::module]` binding: a second claim of the same name is
    /// an error, even for a binding that registers only classes.
    pub(crate) fn claim_binding(&mut self, name: &str) -> Result<(), Error> {
        let fail = |message: String| Error::Registration {
            name: name.to_string(),
            message,
        };
        if let Some(e) = reserved(name) {
            return Err(e);
        }
        Self::check_module_name(name).map_err(fail)?;
        if name == DEFAULT_MODULE {
            return Err(fail(
                "a binding needs a module name of its own, not the default module".to_string(),
            ));
        }
        if self.bound.iter().any(|b| b == name) {
            return Err(fail(format!(
                "the module `{name}` is already registered: a binding registers once per `Vm`"
            )));
        }
        self.bound.push(name.to_string());
        Ok(())
    }

    /// The module `name` of the standard library: what `import name` and `import std.name`
    /// mean when no file or dependency of that name exists (ARCHITECTURE §13). It is a host
    /// module like any other whose interface name is `std.name`; this is how the `colibri-std`
    /// crate registers its modules, and it is the only way to get one: [`Vm::module`] refuses
    /// the reserved name `std`.
    pub fn std_module(&mut self, name: &str) -> HostModule<'_> {
        let name = format!("{}{name}", colibri_check::host::STD_PREFIX);
        // A failure is kept and reported by the load that imports the module.
        let _ = self.materialize(&name);
        HostModule {
            vm: self,
            name,
            std: true,
        }
    }

    /// Declares the standard module `name` without registering it: `register` runs the first
    /// time something needs the module (a program that imports it, [`Vm::std_module`] or
    /// [`Vm::module`] with its name, a session). Until then the module has a name, so
    /// `import` finds it and [`Vm::module_names`] lists it, and no interface text is
    /// generated, parsed or checked for it. `register` must register into `name` only (not
    /// enforced). A registration that fails (a host type of the embedder with the name of
    /// one of the module's) is an error of the load that imports the module.
    ///
    /// For the `colibri-std` crate; not part of the embedding API, hence hidden.
    #[doc(hidden)]
    pub fn lazy_std_module(&mut self, name: &str, register: fn(&mut Vm) -> Result<(), Error>) {
        let name = format!("{}{name}", colibri_check::host::STD_PREFIX);
        if self.pending.iter().any(|(n, _)| *n == name) || self.modules.contains(&name) {
            return;
        }
        self.note_module(&name);
        self.pending.push((name, register));
    }

    /// Declares the standard module `name` as Colibri source: `import name` and `import
    /// std.name` read `text` (once, when a program imports it) as the module `std.name`, with
    /// the file name `<std>/name.colibri`, unless a file or a dependency has that name. It is
    /// not a host module: nothing is registered and no interface is generated.
    ///
    /// For the `colibri-std` crate; not part of the embedding API, hence hidden.
    #[doc(hidden)]
    pub fn std_source_module(&mut self, name: &str, text: &'static str) {
        let name = format!("{}{name}", colibri_check::host::STD_PREFIX);
        if !self.source_modules.iter().any(|(n, _)| *n == name) {
            self.source_modules.push((name, text));
        }
    }

    /// Registers the module `name` if it was declared lazily and not registered yet. A module
    /// whose registration failed (now or earlier) reports that failure every time: what it
    /// had registered before failing stays, so it is not registered again.
    fn materialize(&mut self, name: &str) -> Result<(), Error> {
        if let Some(at) = self.registering.iter().position(|n| n == name) {
            let mut cycle: Vec<&str> = self.registering[at..].iter().map(String::as_str).collect();
            cycle.push(name);
            return Err(Error::Registration {
                name: name.to_string(),
                message: format!(
                    "the modules {} use each other in a cycle: a module can only use a module that does not use it back",
                    cycle.join(" -> ")
                ),
            });
        }
        if let Some((_, message)) = self.failed.iter().find(|(n, _)| n == name) {
            return Err(Error::Registration {
                name: name.to_string(),
                message: message.clone(),
            });
        }
        let Some(at) = self.pending.iter().position(|(n, _)| n == name) else {
            return Ok(());
        };
        let (_, register) = self.pending.remove(at);
        self.register_in_bulk(name, register)
    }

    /// Runs `register` for `module` with the interface check done once at its end. A failure
    /// (a host type of the embedder with the name of one of the module's, say) is an error
    /// of the load or call that needed the module, and is remembered. The module counts as
    /// failed while `register` runs, so that a panic in it leaves it failed too.
    fn register_in_bulk(&mut self, module: &str, register: LazyModule) -> Result<(), Error> {
        self.failed.push((
            module.to_string(),
            "its registration did not finish (it panicked)".to_string(),
        ));
        let at = self.failed.len() - 1;
        let result = {
            let mut guard = BulkGuard::new(self, module);
            register(&mut guard)
        };
        let outcome = result
            .map_err(|e| format!("registering the module failed: {e}"))
            .and_then(|()| {
                self.validate_interfaces(module)
                    .map_err(|m| format!("the interface of the module is not valid: {m}"))
            });
        match outcome {
            Ok(()) => {
                self.failed.remove(at);
                Ok(())
            }
            Err(message) => {
                self.failed[at].1 = message.clone();
                Err(Error::Registration {
                    name: module.to_string(),
                    message,
                })
            }
        }
    }

    /// Registers every module that was declared lazily and not registered yet; the first
    /// failure is returned (the others are still tried).
    #[doc(hidden)]
    pub fn materialize_all(&mut self) -> Result<(), Error> {
        let mut first = None;
        while let Some((name, register)) = self.pending.pop() {
            if let Err(e) = self.register_in_bulk(&name, register) {
                first.get_or_insert(e);
            }
        }
        // A module that failed on an earlier call is not tried again, but it is still failed.
        if first.is_none() {
            first = self
                .failed
                .first()
                .map(|(name, message)| Error::Registration {
                    name: name.clone(),
                    message: message.clone(),
                });
        }
        first.map_or(Ok(()), Err)
    }

    /// The error of a module the program imports whose registration failed earlier, the same
    /// one a load reports. Registers nothing.
    fn check_imports_not_failed(
        &self,
        loaded: &colibri_check::package::Loaded,
    ) -> Result<(), Error> {
        use colibri_check::package::ImportTarget;
        for meta in &loaded.metas {
            for target in meta.imports.values() {
                if let ImportTarget::Host(name) = target
                    && let Some((_, message)) = self.failed.iter().find(|(n, _)| n == name)
                {
                    return Err(Error::Registration {
                        name: name.clone(),
                        message: message.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Registers the lazy modules that the program imports.
    fn materialize_imports(
        &mut self,
        loaded: &colibri_check::package::Loaded,
    ) -> Result<(), Error> {
        if self.pending.is_empty() && self.failed.is_empty() {
            return Ok(());
        }
        use colibri_check::package::ImportTarget;
        for meta in &loaded.metas {
            for target in meta.imports.values() {
                if let ImportTarget::Host(name) = target {
                    self.materialize(name)?;
                }
            }
        }
        Ok(())
    }

    /// The names of the host modules that have an interface: `host` first, then the named
    /// ones in the order they were first registered into.
    pub fn module_names(&self) -> Vec<String> {
        let mut names = vec![DEFAULT_MODULE.to_string()];
        names.extend(self.modules.iter().cloned());
        names
    }

    /// The standard modules (`std.math`, ...) that have been registered: the lazy ones
    /// nothing has asked for yet are not among them. For tests of the lazy loading.
    #[doc(hidden)]
    pub fn registered_std_modules(&self) -> Vec<String> {
        self.modules
            .iter()
            .filter(|m| is_std_module_name(m) && !self.pending.iter().any(|(n, _)| n == *m))
            .cloned()
            .collect()
    }

    fn register_inner<Marker, F: HostFn<Marker>>(
        &mut self,
        module: &str,
        name: &str,
        names: Option<Vec<String>>,
        f: F,
    ) -> Result<Registration<'_>, Error> {
        let fail = |message: String| Error::Registration {
            name: name.to_string(),
            message,
        };
        if self.program.is_some() && !self.bulk {
            return Err(Error::AlreadyLoaded);
        }
        Self::check_module_name(module).map_err(fail)?;
        if RESERVED_NAMES.contains(&name) {
            return Err(fail("this name is built into the language".to_string()));
        }
        if self
            .host_fns
            .iter()
            .any(|e| e.module == module && e.name == name)
            || self
                .host_consts
                .iter()
                .any(|e| e.module == module && e.name == name)
        {
            return Err(fail(format!(
                "the module `{module}` already has an item with this name"
            )));
        }
        // Describe into a copy, so a failed registration leaves nothing behind.
        let mut registry = self.registry.clone();
        registry.current_module = (module != DEFAULT_MODULE).then(|| module.to_string());
        let sig = F::sig(&mut registry);
        registry.current_module = None;
        if let Some(conflict) = registry.conflicts.first() {
            return Err(fail(conflict.clone()));
        }
        if let Some(names) = &names
            && names.len() != sig.params.len()
        {
            return Err(fail(format!(
                "the function takes {} parameter(s) but {} name(s) were given",
                sig.params.len(),
                names.len()
            )));
        }
        self.host_fns.push(HostFnEntry {
            module: module.to_string(),
            name: name.to_string(),
            sig,
            raw: f.into_raw(),
            requires: None,
            positional: names.is_none(),
            names,
            typed: false,
            deprecated: None,
            no_await: false,
        });
        let before = std::mem::replace(&mut self.registry, registry);
        let added = self.note_module(module);
        // Whatever the names or types, what is printed must read back.
        if let Err(message) = self.validate_registration(module) {
            self.host_fns.pop();
            self.registry = before;
            self.forget_module(module, added);
            return Err(fail(message));
        }
        let index = self.host_fns.len() - 1;
        Ok(Registration { vm: self, index })
    }

    /// Describes a struct, enum or host class to the interface without a function that
    /// mentions it, so scripts can use it as a type. A [`crate::HostType`] brings its methods
    /// along; one that a registered signature mentions is registered automatically.
    pub fn register_type<T: crate::IntoColibri>(&mut self) -> Result<(), Error> {
        self.register_type_in::<T>(DEFAULT_MODULE)
    }

    fn register_type_in<T: crate::IntoColibri>(&mut self, module: &str) -> Result<(), Error> {
        self.describe_in(module, std::any::type_name::<T>(), |reg| {
            // A class belongs to the module that first described it; naming it from another
            // module's signatures is what `uses` is for, not registering it again. A value
            // type is shared by every module that registers it, in whatever order, so it is
            // not refused here: describing it goes through the sharing path.
            reg.last_class = None;
            if let TypeDesc::Host(_) = T::ty(reg)
                && let Some(at) = reg.last_class
            {
                let class = &reg.classes[at];
                let home = class.module.as_deref().unwrap_or(DEFAULT_MODULE);
                if home != module {
                    let name = &class.name;
                    let how = if home == DEFAULT_MODULE {
                        "the default module".to_string()
                    } else {
                        format!("the module `{home}`")
                    };
                    reg.conflicts.push(format!(
                        "the class `{name}` belongs to {how}, which described it first: it cannot be registered into the module `{module}` too (name it in this module's signatures{})",
                        if home == DEFAULT_MODULE {
                            String::new()
                        } else {
                            format!(" after `uses(\"{home}\")`")
                        }
                    ));
                }
            }
        })
    }

    /// Lets `describe` declare types into the registry as `module`'s, and checks the result.
    fn describe_in(
        &mut self,
        module: &str,
        type_name: &str,
        describe: impl FnOnce(&mut TypeRegistry),
    ) -> Result<(), Error> {
        let fail = |message: String| Error::Registration {
            name: type_name.to_string(),
            message,
        };
        if self.program.is_some() && !self.bulk {
            return Err(Error::AlreadyLoaded);
        }
        Self::check_module_name(module).map_err(fail)?;
        let mut registry = self.registry.clone();
        registry.current_module = (module != DEFAULT_MODULE).then(|| module.to_string());
        describe(&mut registry);
        registry.current_module = None;
        if let Some(conflict) = registry.conflicts.first() {
            return Err(fail(conflict.clone()));
        }
        let before = std::mem::replace(&mut self.registry, registry);
        let added = self.note_module(module);
        // Whatever the names, what is printed must read back.
        if let Err(message) = self.validate_registration(module) {
            self.registry = before;
            self.forget_module(module, added);
            return Err(fail(message));
        }
        Ok(())
    }

    /// Declares that `user`'s interface names types of the module `used`.
    fn declare_use(&mut self, user: &str, used: &str) -> Result<(), Error> {
        let fail = |message: String| Error::Registration {
            name: used.to_string(),
            message,
        };
        if self.program.is_some() && !self.bulk {
            return Err(Error::AlreadyLoaded);
        }
        Self::check_module_name(used).map_err(fail)?;
        if user == DEFAULT_MODULE {
            return Err(fail(
                "the default module cannot name another module's types: its interface is in the scope of every module, which has no way to say where `Name` comes from; put the function in a named module that uses it".to_string(),
            ));
        }
        if used == DEFAULT_MODULE {
            return Err(fail(
                "the types of the default module are in every module's scope and need no declaration".to_string(),
            ));
        }
        if used == user {
            return Err(fail("a module's own types need no declaration".to_string()));
        }
        // A standard module that was declared and not registered yet is registered now: what
        // a module uses exists before the module is described, and a program that imports
        // only the user gets both.
        self.materialize(used)?;
        if !self.modules.iter().any(|m| m == used) {
            return Err(fail(format!(
                "the module `{used}` has nothing registered: register its types before the module `{user}` that uses them"
            )));
        }
        let short = |m: &str| m.rsplit('.').next().unwrap_or(m).to_string();
        let already = self.registry.uses_of(user);
        if already.iter().any(|u| u == used) {
            return Ok(());
        }
        if let Some(other) = already.iter().find(|u| short(u) == short(used)) {
            return Err(fail(format!(
                "the module `{user}` already uses `{other}`, which is also written `{}` in an interface",
                short(used)
            )));
        }
        let before = self.registry.clone();
        match self.registry.uses.iter_mut().find(|(m, _)| m == user) {
            Some((_, list)) => list.push(used.to_string()),
            None => self
                .registry
                .uses
                .push((user.to_string(), vec![used.to_string()])),
        }
        let added = self.note_module(user);
        if let Err(message) = self.validate_registration(user) {
            self.registry = before;
            self.forget_module(user, added);
            return Err(fail(message));
        }
        Ok(())
    }

    fn check_module_name(module: &str) -> Result<(), String> {
        if module == DEFAULT_MODULE {
            return Ok(());
        }
        if is_std_module_name(module) {
            return Ok(());
        }
        if !is_module_name(module) || module == "std" {
            return Err(format!(
                "`{module}` is not a usable module name: use letters, digits and `_` (and not `std`, which is the standard library)"
            ));
        }
        Ok(())
    }

    /// Remembers that `module` has something registered; whether it was new.
    fn note_module(&mut self, module: &str) -> bool {
        if module == DEFAULT_MODULE || self.modules.iter().any(|m| m == module) {
            return false;
        }
        self.modules.push(module.to_string());
        true
    }

    fn forget_module(&mut self, module: &str, added: bool) {
        if added {
            self.modules.retain(|m| m != module);
        }
    }

    /// [`Vm::validate_interfaces`] after one registration, unless the module is being
    /// registered in one go.
    fn validate_registration(&self, touched: &str) -> Result<(), String> {
        if self.bulk {
            Ok(())
        } else {
            self.validate_interfaces(touched)
        }
    }

    /// Prints every module's interface and reads it back, and checks that every type a
    /// signature mentions is one the interface text can name: the module's own, the default
    /// module's, or one of a module it declared it uses (`HostModule::uses`). No signature can
    /// name a module enum.
    ///
    /// `touched` is the module that just got a registration: another module's text does not
    /// change with it, so only that one is printed and read back (all of them when it is the
    /// default module, whose types the others may mention). This keeps registering a library
    /// of many functions linear and not quadratic, which dominates the start of a script.
    fn validate_interfaces(&self, touched: &str) -> Result<(), String> {
        // A type that changed owner changes the text of every module that names it.
        let moved = self.registry.moved.replace(false);
        if touched == DEFAULT_MODULE || moved {
            for module in self.module_names() {
                HostEnv::from_text(&self.rendered_module(&module).text)?;
            }
        } else {
            HostEnv::from_text(&self.rendered_module(touched).text)?;
        }
        let reg = &self.registry;
        let check = |module: &str, sig: &crate::types::SigDesc, what: &str| -> Result<(), String> {
            let mut names = Vec::new();
            for p in &sig.params {
                p.mentions(&mut names);
            }
            sig.ret.mentions(&mut names);
            for n in names {
                if reg.is_module_enum_name(n) {
                    return Err(format!(
                        "{what} mentions the module enum `{n}`; a module's functions return the erased `Error` (a module enum is recovered with `is` and `as`), and no signature can name a module enum"
                    ));
                }
                reg.resolve(module, n)
                    .map_err(|why| format!("{what} mentions the type `{n}`: {why}"))?;
            }
            Ok(())
        };
        for f in &self.host_fns {
            check(&f.module, &f.sig, &format!("the function `{}`", f.name))?;
        }
        for c in &reg.classes {
            let module = c.module.as_deref().unwrap_or(DEFAULT_MODULE);
            for m in &c.methods {
                check(
                    module,
                    &m.sig,
                    &format!("the method `{}.{}`", c.name, m.name),
                )?;
            }
        }
        for (i, d) in reg.decls.iter().enumerate() {
            if let crate::types::TypeDecl::Struct { name, fields } = d {
                let module = reg.decl_scope[i].as_deref().unwrap_or(DEFAULT_MODULE);
                for (field, ty) in fields {
                    let sig = crate::types::SigDesc {
                        params: vec![ty.clone()],
                        ret: crate::types::TypeDesc::Unit,
                    };
                    check(module, &sig, &format!("the field `{name}.{field}`"))?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn eng(&self) -> &dyn HostCx {
        &*self.engine
    }

    pub(crate) fn eng_mut(&mut self) -> &mut dyn HostCx {
        &mut *self.engine
    }

    /// Whether the host function `name` of `module` never waits for anything outside the
    /// program (a pure one): the VM calls it from a coroutine without a native stack of its
    /// own, which any other function gets so that the coroutine can wait inside it.
    fn is_no_await(&self, module: &str, name: &str) -> bool {
        let class = name.split_once('.').map(|(c, _)| format!("{c}.*"));
        self.no_await.iter().any(|(m, waiting)| {
            m == module
                && !waiting
                    .iter()
                    .any(|w| w == name || class.as_deref() == Some(w.as_str()))
        })
    }

    /// Declares that the functions of the standard module `name` never wait for anything
    /// outside the program, except the ones in `waiting` (a function name, or `Class.*` for
    /// the methods of a class). A host function of any other module may wait. For the
    /// `colibri-std` crate; not part of the embedding API, hence hidden.
    #[doc(hidden)]
    pub fn no_await_module(&mut self, name: &str, waiting: &[&str]) {
        self.no_await.push((
            format!("{}{name}", colibri_check::host::STD_PREFIX),
            waiting.iter().map(|w| w.to_string()).collect(),
        ));
    }

    /// The raw function that implements the host function or method `name` (`World.step`), the
    /// capability it needs, and whether it never suspends its caller.
    fn find_raw(
        &self,
        module: &str,
        name: &str,
    ) -> Option<(crate::host::RawHostFn, Option<Cap>, bool)> {
        let pure = self.is_no_await(module, name);
        let marked = |e: &HostFnEntry| e.no_await || pure;
        if let Some(entry) = self
            .host_fns
            .iter()
            .find(|e| e.module == module && e.name == name)
        {
            return Some((entry.raw.clone(), entry.requires, marked(entry)));
        }
        let (class, method) = name.split_once('.')?;
        let class =
            self.registry.classes.iter().find(|c| {
                c.name == class && c.module.as_deref().unwrap_or(DEFAULT_MODULE) == module
            })?;
        let method = class.methods.iter().find(|m| m.name == method)?;
        Some((method.raw.clone(), None, pure || method.no_await))
    }

    pub(crate) fn set_typed(&mut self, index: usize) -> Result<(), Error> {
        let Some(entry) = self.host_fns.get_mut(index) else {
            return Ok(());
        };
        let mut generics = Vec::new();
        for t in entry.sig.params.iter().chain([&entry.sig.ret]) {
            t.params(&mut generics);
        }
        if generics.is_empty() {
            return Err(Error::Registration {
                name: entry.name.clone(),
                message: "`typed` is for a generic function: a parameter or the result must be a `Value`, which stands for the type `T`".to_string(),
            });
        }
        entry.typed = true;
        Ok(())
    }

    /// Registers the constant `name` in `module`.
    fn register_const_in(
        &mut self,
        module: &str,
        name: &str,
        value: ConstValue,
    ) -> Result<(), Error> {
        let fail = |message: String| Error::Registration {
            name: name.to_string(),
            message,
        };
        if self.program.is_some() && !self.bulk {
            return Err(Error::AlreadyLoaded);
        }
        Self::check_module_name(module).map_err(fail)?;
        if module == DEFAULT_MODULE {
            return Err(fail(
                "constants belong to a named module (`vm.module(..)`)".to_string(),
            ));
        }
        if let ConstValue::Float(v) = value
            && float_literal(v).is_none()
        {
            return Err(fail(format!(
                "{v:?} cannot be written as a constant: use a finite number that is not negative, between 1e-4 and 1e15"
            )));
        }
        let taken = self
            .host_fns
            .iter()
            .any(|e| e.module == module && e.name == name)
            || self
                .host_consts
                .iter()
                .any(|e| e.module == module && e.name == name);
        if taken {
            return Err(fail(format!(
                "the module `{module}` already has an item with this name"
            )));
        }
        self.host_consts.push(ConstEntry {
            module: module.to_string(),
            name: name.to_string(),
            value,
        });
        let added = self.note_module(module);
        if let Err(message) = self.validate_registration(module) {
            self.host_consts.pop();
            self.forget_module(module, added);
            return Err(fail(message));
        }
        Ok(())
    }

    pub(crate) fn set_deprecated(&mut self, index: usize, message: &str) {
        if let Some(entry) = self.host_fns.get_mut(index) {
            entry.deprecated = Some(message.to_string());
        }
    }

    pub(crate) fn set_no_await(&mut self, index: usize) {
        if let Some(entry) = self.host_fns.get_mut(index) {
            entry.no_await = true;
        }
    }

    pub(crate) fn set_requires(&mut self, index: usize, cap: Cap) {
        if let Some(entry) = self.host_fns.get_mut(index) {
            entry.requires = Some(cap);
        }
    }

    /// Sets what runs around every thread task of this isolate, so that the task sees the same
    /// world whichever thread runs it and whatever that thread ran before. `enter` is given the
    /// host state of type `T`, and the task's serial number (the same on every run of the
    /// program: the main program's tasks are numbered in the order they start, and the tasks a
    /// task starts from where they start), and returns what puts the state back when the task
    /// ends. The standard library seeds its random generator from it. Call it from
    /// [`VmBuilder::worker_setup`], on the worker's `Vm`.
    pub fn task_scope<T: 'static>(
        &mut self,
        enter: impl std::ops::Fn(&mut T, u64) -> Box<dyn FnOnce(&mut T)> + 'static,
    ) {
        self.engine
            .core_mut()
            .set_task_scope(Some(Rc::new(move |core, serial| {
                let Some(state) = core.state_mut::<T>() else {
                    return Box::new(|_| {});
                };
                let restore = enter(state, serial);
                Box::new(move |core| {
                    if let Some(state) = core.state_mut::<T>() {
                        restore(state);
                    }
                })
            })));
    }

    /// Stores host state of type `T`, which host functions reach through `cx.state_mut::<T>()`
    /// and the host between calls through [`Vm::state_mut`]. Returns the state it replaces.
    pub fn insert_state<T: 'static>(&mut self, value: T) -> Option<T> {
        self.engine.core_mut().insert_state(value)
    }

    /// The host state of type `T`.
    pub fn state_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.engine.core_mut().state_mut::<T>()
    }

    /// Takes host state of type `T` out of the `Vm`.
    pub fn remove_state<T: 'static>(&mut self) -> Option<T> {
        self.engine.core_mut().remove_state::<T>()
    }

    // ---- interface files ----

    fn rendered(&self) -> Rendered {
        self.rendered_module(DEFAULT_MODULE)
    }

    fn rendered_module(&self, module: &str) -> Rendered {
        iface::render(&self.registry, &self.host_fns, &self.host_consts, module)
    }

    /// The canonical interface (`.cbi`) text of what the host registered: the text the checker
    /// reads, and what [`Vm::write_interface`] writes.
    pub fn interface_text(&self) -> String {
        self.rendered().text
    }

    /// The hash of the interface: FNV-1a 64 over its canonical body, as sixteen hex digits. It
    /// changes whenever a registered signature or type does, and only then.
    pub fn interface_hash(&self) -> String {
        self.rendered().hash
    }

    /// Writes the interface text to `path` unless the file already holds exactly that, so a
    /// build that regenerates it does not touch an unchanged file. Whether it wrote.
    pub fn write_interface(&self, path: impl AsRef<Path>) -> Result<bool, Error> {
        iface::write_if_changed(path.as_ref(), &self.interface_text())
    }

    /// Checks that the file at `path` is the interface of what the host registered, for a CI
    /// check that a committed `.cbi` is current: [`Error::InterfaceMismatch`] when it is not.
    pub fn verify_interface(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        self.verify_text(path.as_ref(), &self.rendered())
    }

    fn verify_text(&self, path: &Path, rendered: &Rendered) -> Result<(), Error> {
        let found = std::fs::read_to_string(path).map_err(|e| Error::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        if found == rendered.text {
            return Ok(());
        }
        Err(Error::InterfaceMismatch {
            path: path.display().to_string(),
            expected: rendered.hash.clone(),
            found: iface::declared_hash(&found),
        })
    }

    /// The text of the interface of each host module, as the checker reads it.
    fn iface_texts(&self) -> Vec<(String, String)> {
        self.module_names()
            .into_iter()
            .map(|module| {
                let text = self.rendered_module(&module).text;
                (module, text)
            })
            .collect()
    }

    /// The interface as the checker reads it, with the host's grants.
    fn host_env_from(&self, iface: &[(String, String)]) -> Result<HostEnv, Error> {
        let mut env = HostEnv::new();
        for (_, text) in iface {
            let one = HostEnv::from_text(text).map_err(Error::Internal)?;
            env.merge(one);
        }
        for cap in self.caps.iter() {
            env.grant(cap.name());
        }
        env.set_web(self.cfg.os == colibri_check::package::WEB_OS);
        Ok(env)
    }

    /// Tells the interpreter which type of the loaded program each host class is, by the Rust
    /// type that implements it: a class is found by that and never by its bare name, which
    /// two modules may share.
    fn install_host_classes(&mut self) {
        // The structs and enums are found by their Rust type too (a type of one name can be
        // two Rust types in two modules).
        let values: Vec<_> = (0..self.registry.decls.len())
            .filter_map(|j| {
                let any = self.registry.decl_rust[j]?;
                let owner = self.registry.owner_of(j);
                Some((
                    any,
                    self.engine
                        .core()
                        .host_type_in(owner, self.registry.decls[j].name())?,
                ))
            })
            .collect();
        let classes = self
            .registry
            .classes
            .iter()
            .filter_map(|c| {
                let module = c.module.as_deref().unwrap_or(DEFAULT_MODULE);
                Some((c.any, self.engine.core().host_type_in(module, &c.name)?))
            })
            .chain(values)
            .collect();
        self.engine.core_mut().set_host_classes(classes);
        // A type of a module that another named module shares (or uses) is its bare name
        // there too: the type a host function of `render` means by `Pos` is `physics.Pos`.
        let mut aliases = HashMap::new();
        for (user, used) in &self.registry.uses {
            for home in used {
                for (decl, m) in self.registry.decls.iter().zip(&self.registry.decl_modules) {
                    let m = m.as_deref().unwrap_or(DEFAULT_MODULE);
                    if m != home || decl.is_module_local() {
                        continue;
                    }
                    let name = decl.name();
                    if self.registry.find_decl(user, name).is_some()
                        || self.registry.find_decl(DEFAULT_MODULE, name).is_some()
                    {
                        continue;
                    }
                    if let Some(id) = self.engine.core().host_type_in(home, name) {
                        aliases
                            .entry((user.clone(), name.to_string()))
                            .or_insert(id);
                    }
                }
            }
        }
        // The same Rust type registered by several modules: each names it by its own module.
        for (user, name, j, _) in &self.registry.aliases {
            let owner = self.registry.owner_of(*j);
            if let Some(id) = self.engine.core().host_type_in(owner, name) {
                aliases.insert((user.clone(), name.clone()), id);
            }
        }
        self.engine.core_mut().set_host_type_aliases(aliases);
        for c in &self.registry.classes {
            if c.hooks.show.is_none() && c.hooks.eq.is_none() && c.hooks.hash.is_none() {
                continue;
            }
            let module = c.module.as_deref().unwrap_or(DEFAULT_MODULE);
            let Some(ty) = self.engine.core().host_type_in(module, &c.name) else {
                continue;
            };
            let wrap = |what: &str, raw: &Option<crate::host::RawHostFn>| {
                raw.as_ref()
                    .map(|raw| trampoline(&format!("{}.{what}", c.name), module, raw.clone()))
            };
            self.engine.set_host_class_hooks(
                ty,
                ClassHooks {
                    show: wrap("show", &c.hooks.show),
                    eq: wrap("eq", &c.hooks.eq),
                    hash: wrap("hash", &c.hooks.hash),
                },
            );
        }
    }

    /// Installs the host functions the program uses. One whose capability was not granted
    /// stays a trap: the checker already refused every call of it.
    fn install_host_fns(&mut self, program: &Program) -> Result<(), Error> {
        for (i, def) in program.host_fns.iter().enumerate() {
            // A function of an `extern` block is bound to its C library now, so a missing
            // library or symbol is a load error and not a surprise at the first call.
            if let Some(ext) = &def.ext {
                if !self.caps.contains(Cap::Ffi) {
                    continue;
                }
                let name = program.fns[def.f.0 as usize].name.clone();
                self.engine
                    .bind_extern(HostFnId(i as u32), &name, ext)
                    .map_err(Error::Ffi)?;
                continue;
            }
            let Some((raw, requires, pure)) = self.find_raw(&def.module, &def.name) else {
                continue;
            };
            if requires.is_some_and(|cap| !self.caps.contains(cap)) {
                continue;
            }
            let imp = trampoline(&def.name, &def.module, raw);
            self.engine.set_host_fn(HostFnId(i as u32), imp, pure);
        }
        Ok(())
    }

    // ---- budget ----

    /// Sets the instruction budget: each outermost entry from the host (`call`, `load`,
    /// `run_main`) may take this many steps; `None` is unlimited. Calls a host function makes
    /// back into Colibri share the entry's budget. When it runs out, evaluation unwinds like a
    /// panic (so `defer` statements and `Drop` hooks run, for a grace of 10 000 more steps) and
    /// the call returns [`Error::BudgetExhausted`]; the `Vm` stays usable.
    pub fn set_instruction_budget(&mut self, steps: Option<u64>) {
        self.engine.core_mut().set_instruction_budget(steps);
    }

    /// How many safepoints ran (see [`VmBuilder::safepoint_slice`]).
    #[doc(hidden)]
    pub fn safepoints_run(&self) -> u64 {
        colibri_core::safepoint::SafepointKind::ALL
            .iter()
            .map(|k| self.engine.core().safepoint_hits(*k))
            .sum()
    }

    /// The steps the latest entry from the host used (including the grace after running out).
    pub fn steps_used(&self) -> u64 {
        self.engine.core().steps_used()
    }

    /// The bytes this `Vm`'s entries hold: what scripts and the host functions they call
    /// allocated during an entry and has not been freed yet, with a 16-byte prefix for each
    /// allocation. `None` unless the program installed [`crate::CountingAlloc`] as its global
    /// allocator, which is what makes the count possible.
    ///
    /// A value counts against the `Vm` that made it, wherever and whenever it is dropped, so the
    /// figure falls when the value dies even outside an entry. What the host allocated outside an
    /// entry is not counted, and neither is the `Vm`'s own bookkeeping made before its first entry.
    pub fn memory_used(&self) -> Option<u64> {
        self.engine.core().memory_used()
    }

    /// Sets the memory limit in bytes (`None` lifts it). Each isolate is limited separately. The
    /// main `Vm`'s entries may hold up to the limit themselves, not counting what its thread-pool
    /// workers hold, so a worker's memory never fails the main thread. A task in a worker fails
    /// with `TaskFailure::MemoryLimit` when the `Vm` and all its workers together
    /// ([`Vm::memory_used`]) hold more than the limit. So with workers, [`Vm::memory_used`] can
    /// reach about twice the limit: the workers up to the limit, then the main thread up to the
    /// limit on its own, each plus the overshoot described below; set the limit to about half
    /// of a hard cap. It is **approximate, with a bounded overshoot**, like
    /// the instruction budget: the program is checked every 4,096 steps, and the built-ins that
    /// allocate in proportion to an argument (`str.repeat`, `pad_left`, `join`, `fs.read`, an
    /// HTTP body, `json.decode`, concatenation) check before they allocate, so between two checks
    /// a program can pass the limit by what 4,096 steps allocate without such a built-in.
    ///
    /// When the limit is passed, evaluation unwinds like a panic: `defer` statements and `Drop`
    /// hooks run, with 1 MiB more than the failing isolate held when it was stopped (or than the limit, if
    /// that is more), measured the way its limit is, for
    /// them; past that they are skipped (plain releases still happen). The entry returns
    /// [`Error::MemoryLimit`] and the `Vm` stays usable. A coroutine that passes it ends the same
    /// way: [`Error::MemoryLimit`] reaches the host, a `join_result` never receives it and a
    /// joiner never takes it over. What a host function allocates during an entry counts.
    ///
    /// # Panics
    /// With `Some`, when the program has not installed the counting allocator
    /// ([`crate::CountingAlloc`]); the message says how. Call it between entries.
    pub fn set_memory_limit(&mut self, limit: Option<u64>) {
        if limit.is_some() {
            colibri_core::mem::require_installed("a memory limit");
        }
        self.engine.core_mut().set_memory_limit(limit);
    }

    // ---- loading and running ----

    /// Reads, checks and runs a script file: module-level values are initialised, then its
    /// top-level statements run. `fn main` is not called; use [`Vm::run_main`].
    ///
    /// The file's folder is its implicit package (DESIGN §13): `import helpers` finds
    /// `helpers.colibri` next to it, `import util.strings` finds `util/strings.colibri`, and
    /// every file reached that way is checked together with it. A `colibri.toml` in the folder
    /// supplies the dependencies `import` may name.
    pub fn load_file(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        self.load_entry(
            &DiskFs,
            Entry::File(path.as_ref()),
            LoadMode::Run,
            Some(OwnedEntry::File(path.as_ref().to_path_buf())),
        )
    }

    /// Reads, checks and runs the package in folder `dir`: its `colibri.toml` names it and lists
    /// its dependencies, its `main.colibri` is the entry (module values are initialised in
    /// dependency order, then the top-level statements of `main.colibri` run). `fn main` is not
    /// called; use [`Vm::run_main`].
    pub fn load_package(&mut self, dir: impl AsRef<Path>) -> Result<(), Error> {
        self.load_entry(
            &DiskFs,
            Entry::Package(dir.as_ref()),
            LoadMode::Run,
            Some(OwnedEntry::Package(dir.as_ref().to_path_buf())),
        )
    }

    /// Reads and checks a program for running its tests, and runs nothing: not the module
    /// initialisers, not the top-level statements (each test starts from freshly initialised
    /// module values, see [`Vm::run_test`]). `path` is a script file or a package folder; for a
    /// package every `.colibri` file under its root is loaded, also the ones `main.colibri` does
    /// not import, so that every `test` block is found (ARCHITECTURE §15). Then
    /// [`Vm::tests`] lists the tests.
    pub fn load_tests(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        let path = path.as_ref();
        let entry = if path.is_dir() {
            Entry::Package(path)
        } else {
            Entry::File(path)
        };
        self.load_entry(&DiskFs, entry, LoadMode::Tests, None)
    }

    /// [`Vm::load_package`] for a folder, [`Vm::load_file`] for anything else: what
    /// `colibri run <path>` does.
    pub fn load_path(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        let path = path.as_ref();
        if path.is_dir() {
            self.load_package(path)
        } else {
            self.load_file(path)
        }
    }

    /// Like [`Vm::load_file`] for source text; `name` is what diagnostics call the file.
    ///
    /// On a compile error nothing is loaded. If the top-level code panics, the program is
    /// discarded (its module state is half-built) and the `Vm` can load another. The source has
    /// no siblings, so `import` can only name a host module.
    pub fn load_source(&mut self, name: &str, source: &str) -> Result<(), Error> {
        self.load_sources(&[(name, source)])
    }

    /// Loads a program given as text: `(path, text)` pairs, as if the files were on disk. The
    /// first is the entry, and the folder it is in is the root that `import` paths start from,
    /// so `[("app/main.colibri", ..), ("app/jobs/worker.colibri", ..)]` lets `main.colibri`
    /// `import jobs.worker`. Paths are what diagnostics and stack traces call the files. For
    /// tests and hosts that keep their scripts in memory.
    pub fn load_sources(&mut self, files: &[(&str, &str)]) -> Result<(), Error> {
        let Some((entry, _)) = files.first() else {
            return Err(Error::Io {
                path: String::new(),
                message: "no source files were given".to_string(),
            });
        };
        let mut fs = MemoryFs::new();
        for (path, text) in files {
            fs.insert(path, *text);
        }
        self.load_entry(&fs, Entry::File(Path::new(entry)), LoadMode::Run, None)
    }

    /// Reads, checks and runs the program that starts at `entry` in the files of `fs`, as
    /// [`Vm::load_file`] and [`Vm::load_package`] do for the disk: a host that keeps a whole
    /// program somewhere else (packed into its own executable) gives it as a [`SourceFs`]. The
    /// program is not watched for changes.
    pub fn load_from(&mut self, fs: &dyn SourceFs, entry: Entry<'_>) -> Result<(), Error> {
        self.load_entry(fs, entry, LoadMode::Run, None)
    }

    /// [`Vm::load_from`] for a host that cannot block (the web): the modules are initialised, but
    /// the script's top-level statements and `main` do not run. [`Vm::start_entry`] starts them as
    /// a coroutine, so that a `wait` in them suspends to the host's own loop, and the host drives
    /// them with [`Vm::tick`] and [`Vm::entry_step`].
    pub fn load_from_deferred(&mut self, fs: &dyn SourceFs, entry: Entry<'_>) -> Result<(), Error> {
        self.load_entry(fs, entry, LoadMode::Deferred, None)
    }

    /// `watch` is set for programs read from disk by `load_file` and `load_package`: it makes
    /// the `Vm` remember the files, for [`Vm::reload_changed`].
    fn load_entry(
        &mut self,
        fs: &dyn SourceFs,
        entry: Entry<'_>,
        mode: LoadMode,
        watch: Option<OwnedEntry>,
    ) -> Result<(), Error> {
        if self.program.is_some() {
            return Err(Error::AlreadyLoaded);
        }
        let checked = match watch {
            Some(owned) => {
                let recorder = RecordingFs::new(fs);
                let checked = self.check_entry(&recorder, entry, mode);
                self.reload = Some(ReloadState::new(owned, reads_of(recorder)));
                checked
            }
            None => {
                self.reload = None;
                self.check_entry(fs, entry, mode)
            }
        }?;
        self.warn(&checked);
        self.install(checked, mode)?;
        self.entry_pending = mode == LoadMode::Deferred;
        Ok(())
    }

    /// Hands the warnings of a checked program to the [`Vm::on_warnings`] hook.
    fn warn(&mut self, checked: &Checked) {
        if !checked.warnings.is_empty()
            && let Some(mut hook) = self.warning_hook.take()
        {
            hook(&Report {
                sources: checked.sources.clone(),
                diagnostics: checked.warnings.clone(),
            });
            self.warning_hook = Some(hook);
        }
    }

    /// Reads and checks a program without touching the loaded one. The lazy standard modules
    /// the program imports are registered first.
    fn check_entry(
        &mut self,
        fs: &dyn SourceFs,
        entry: Entry<'_>,
        mode: LoadMode,
    ) -> Result<Checked, Error> {
        // Whatever the load reads is what a thread task's worker will read.
        let recorder = RecordingFs::new(fs);
        let (sources, loaded) = self.read_program(&recorder, entry, mode)?;
        // The loader only probes for a binding's `<key>.cbi`; a worker's load must find it in the
        // image too, or it would report the binding as having no interface.
        for binding in &loaded.rust_bindings {
            if recorder.is_file(&binding.interface) {
                let _ = recorder.read(&binding.interface);
            }
        }
        self.materialize_imports(&loaded)?;
        let mut checked = self.check_loaded(sources, loaded, mode)?;
        if matches!(mode, LoadMode::Run | LoadMode::Deferred) {
            let mut files = MemoryFs::new();
            for read in recorder.into_files() {
                files.insert_named(&read.path, read.name, read.text);
            }
            checked.image = Some(ProgramImage {
                files,
                entry: match entry {
                    Entry::File(p) => OwnedEntry::File(p.to_path_buf()),
                    Entry::Package(p) => OwnedEntry::Package(p.to_path_buf()),
                },
                cfg: self.cfg.clone(),
                iface: checked.iface.clone(),
            });
        }
        Ok(checked)
    }

    /// Reads the files of a program: the syntax trees and what each `import` names.
    fn read_program(
        &self,
        fs: &dyn SourceFs,
        entry: Entry<'_>,
        mode: LoadMode,
    ) -> Result<(SourceMap, colibri_check::package::Loaded), Error> {
        let mut sources = SourceMap::new();
        let options = colibri_check::package::LoadOptions {
            all_modules: mode == LoadMode::Tests,
            cfg: self.cfg.clone(),
            std_sources: self.source_modules.clone(),
        };
        let names: Vec<String> = self.modules.clone();
        let mut loaded =
            colibri_check::package::load_with(fs, &mut sources, entry, &names, options).map_err(
                |e| Error::Io {
                    path: e.path,
                    message: e.message,
                },
            )?;
        refuse_unregistered_bindings(&mut loaded, &names);
        Ok((sources, loaded))
    }

    /// Checks a program that was read, against the host as it is now.
    fn check_loaded(
        &self,
        sources: SourceMap,
        loaded: colibri_check::package::Loaded,
        mode: LoadMode,
    ) -> Result<Checked, Error> {
        let iface = self.iface_texts();
        let env = self.host_env_from(&iface)?;
        self.check_loaded_in(&env, iface, sources, loaded, mode)
    }

    /// [`Vm::check_loaded`] against the host interface `env` (the text `iface`).
    fn check_loaded_in(
        &self,
        env: &HostEnv,
        iface: Vec<(String, String)>,
        sources: SourceMap,
        loaded: colibri_check::package::Loaded,
        mode: LoadMode,
    ) -> Result<Checked, Error> {
        let mut diagnostics = loaded.diagnostics.clone();
        let mut program = None;
        // A tree with syntax errors would only produce follow-on errors.
        if !loaded.has_errors() {
            let checked = colibri_check::check_modules_with(
                &loaded.inputs(),
                &loaded.packages,
                env,
                self.native_stack_budget,
                colibri_check::CheckOptions {
                    dependency_order: mode == LoadMode::Session,
                },
            );
            // The loader's own warnings (an unknown `allow-ffi` name) stay.
            diagnostics.extend(checked.diagnostics);
            program = checked.program;
        }
        match program {
            Some(program) => Ok(Checked {
                sources,
                program,
                warnings: diagnostics,
                iface,
                image: None,
            }),
            None => Err(Error::Compile(Report {
                sources,
                diagnostics,
            })),
        }
    }

    /// Makes a checked program the loaded one: runs the module initialisers and top-level
    /// statements. The previous program must be gone already.
    fn install(&mut self, checked: Checked, mode: LoadMode) -> Result<(), Error> {
        let Checked {
            sources,
            program,
            warnings,
            image,
            ..
        } = checked;
        self.launch.set_image(image.map(Arc::new));
        self.sources = Rc::new(sources);
        self.sync_debug_locator();
        self.warnings = warnings;
        self.epoch += 1;
        self.engine.load(&program);
        // The initialisers and the script may start thread tasks, so the program is the pool's
        // from here on.
        let program = Rc::new(program);
        self.launch.set_program(Some(Rc::clone(&program)));
        self.install_host_classes();
        if let Err(error) = self.install_host_fns(&program) {
            self.launch.set_program(None);
            self.epoch += 1;
            self.engine.shutdown();
            self.engine.core_mut().set_owner(
                (self.id, self.epoch),
                false,
                Rc::clone(&self.sources),
            );
            return Err(error);
        }
        self.engine
            .core_mut()
            .set_owner((self.id, self.epoch), true, Rc::clone(&self.sources));
        let outcome = match mode {
            LoadMode::Run => self.engine.run_module(),
            LoadMode::Deferred => self.engine.run_module_inits(),
            LoadMode::Tests | LoadMode::Session => Ok(()),
        };
        self.engine.core_mut().flush();
        match outcome {
            Ok(()) => {
                self.program = Some(program);
                Ok(())
            }
            Err(info) => {
                self.launch.set_program(None);
                let error = panic_error(self.engine.core(), *info);
                self.epoch += 1;
                self.engine.shutdown();
                self.engine.core_mut().set_owner(
                    (self.id, self.epoch),
                    false,
                    Rc::clone(&self.sources),
                );
                Err(error)
            }
        }
    }

    // ---- hot reload ----

    /// Notices source files that changed since the program was loaded (or last reloaded),
    /// re-checks the whole program and puts the new code to work (DESIGN §16.3, ARCHITECTURE
    /// §17). Only programs loaded with [`Vm::load_file`], [`Vm::load_package`] or
    /// [`Vm::load_path`] have files to watch; for any other this returns
    /// [`ReloadOutcome::Unchanged`]. Call it at a safe point: between ticks or
    /// requests, never from inside a script call.
    ///
    /// * [`ReloadOutcome::Patched`]: only function bodies changed (same functions and
    ///   signatures, same types, same module-level declarations and initialisers), so the new
    ///   bodies replace the old ones and the next call uses them. Nothing is re-initialised,
    ///   function handles stay valid, and a coroutine suspended in an old body keeps running
    ///   it.
    /// * [`ReloadOutcome::Restarted`]: anything else changed. If the old program defines
    ///   `fn save_state() -> str` it is called first, and its text goes to the new program's
    ///   `fn restore_state(s: str)` once the new program is initialised. Coroutines are
    ///   cancelled, module values released, function handles and callbacks go stale (the epoch
    ///   moves), and `main` is not called again: the host decides.
    /// * [`ReloadOutcome::CheckFailed`]: the new sources do not check; the old program keeps
    ///   running unchanged.
    ///
    /// `Err` is an I/O problem (a file vanished mid-save, so the next call tries again), a
    /// panic in `save_state`, or a panic while the new program initialises or restores its
    /// state. A failed `save_state` leaves the old program running; a panic in the new
    /// program's initialisers leaves the `Vm` with no program, and the next edit loads again.
    pub fn reload_changed(&mut self) -> Result<ReloadOutcome, Error> {
        self.reload_changed_with(ReloadOptions::default())
    }

    /// [`Vm::reload_changed`] with options.
    pub fn reload_changed_with(&mut self, options: ReloadOptions) -> Result<ReloadOutcome, Error> {
        let Some(state) = self.reload.as_mut() else {
            return Ok(ReloadOutcome::Unchanged);
        };
        if !state.poll_changed() {
            return Ok(ReloadOutcome::Unchanged);
        }
        let owned = state.entry.clone();
        let recorder = RecordingFs::new(&DiskFs);
        let checked = match self.check_entry(&recorder, owned.as_entry(), LoadMode::Run) {
            Ok(checked) => checked,
            Err(Error::Compile(report)) => {
                self.accept_reads(recorder);
                return Ok(ReloadOutcome::CheckFailed(report));
            }
            // A file vanished mid-save, say: not accepted, so the next call tries again.
            Err(e) => return Err(e),
        };
        self.accept_reads(recorder);
        let fingerprint = Fingerprint::of(&checked.program);
        let Some(old) = self.program.clone() else {
            // The last load failed in its initialisers: nothing to patch.
            return self.restart(
                checked,
                fingerprint,
                "no program was running".to_string(),
                None,
            );
        };
        if self
            .reload
            .as_ref()
            .is_some_and(|r| r.fingerprint.is_none())
        {
            let current = Fingerprint::of(&old);
            if let Some(r) = self.reload.as_mut() {
                r.fingerprint = Some(current);
            }
        }
        let current = self
            .reload
            .as_ref()
            .and_then(|r| r.fingerprint.clone())
            .ok_or_else(|| Error::Internal("no fingerprint".to_string()))?;
        let difference = current.declaration_difference(&fingerprint);
        if options.force_restart || difference.is_some() {
            let reason = match difference {
                Some(what) if !options.force_restart => format!("{what} changed"),
                _ => "a restart was asked for".to_string(),
            };
            return self.restart(checked, fingerprint, reason, Some(&old));
        }
        let functions = current.changed_bodies(&fingerprint);
        // Swap in every body, also the unchanged ones: their spans point into the new sources.
        if let Err(why) = self.engine.patch(&checked.program) {
            // The engine cannot swap the bodies under what runs: start over instead.
            return self.restart(checked, fingerprint, why, Some(&old));
        }
        self.sources = Rc::new(checked.sources);
        self.sync_debug_locator();
        self.warnings = checked.warnings;
        self.engine
            .core_mut()
            .set_owner((self.id, self.epoch), true, Rc::clone(&self.sources));
        let program = Rc::new(checked.program);
        self.program = Some(Rc::clone(&program));
        self.launch.set_program(Some(program));
        self.launch.set_image(checked.image.map(Arc::new));
        if let Some(r) = self.reload.as_mut() {
            r.fingerprint = Some(fingerprint);
        }
        self.engine.core_mut().flush();
        Ok(if functions == 0 {
            ReloadOutcome::Unchanged
        } else {
            ReloadOutcome::Patched { functions }
        })
    }

    /// Records what a reload attempt read as the new baseline.
    fn accept_reads(&mut self, recorder: RecordingFs<'_>) {
        if let Some(r) = self.reload.as_mut() {
            r.set_files(reads_of(recorder));
        }
    }

    fn restart(
        &mut self,
        checked: Checked,
        fingerprint: Fingerprint,
        reason: String,
        old: Option<&Rc<Program>>,
    ) -> Result<ReloadOutcome, Error> {
        // The old program's state, before anything is torn down. A failure here keeps the old
        // program running.
        let saved = if old.is_some() {
            match self.find_fn::<(), String>("save_state")? {
                Some(save) => Some(save.call(self, ())?),
                None => None,
            }
        } else {
            None
        };
        self.teardown();
        if let Some(r) = self.reload.as_mut() {
            r.fingerprint = None;
        }
        self.warn(&checked);
        self.install(checked, LoadMode::Run)?;
        if let Some(r) = self.reload.as_mut() {
            r.fingerprint = Some(fingerprint);
        }
        let mut state_carried = false;
        if let Some(text) = saved
            && let Some(restore) = self.find_fn::<(String,), ()>("restore_state")?
        {
            restore.call(self, (text,))?;
            state_carried = true;
        }
        Ok(ReloadOutcome::Restarted {
            reason,
            state_carried,
        })
    }

    /// Calls `fn main()` if the program defines it. A `main` that returns `Result[()]` and
    /// ends in `err(e)` is [`Error::MainReturnedErr`] (DESIGN §10.3).
    pub fn run_main(&mut self) -> Result<(), Error> {
        let program = self.program.clone().ok_or(Error::NotLoaded)?;
        let Some(main) = program.main else {
            return Ok(());
        };
        let returns_result = matches!(
            &program.fns[main.0 as usize].sig.ret,
            Ty::Adt(RESULT_TYPE, _)
        );
        let result = self.engine.call(main, Vec::new());
        self.engine.core_mut().flush();
        let value = result.map_err(|info| panic_error(self.engine.core(), *info))?;
        if returns_result
            && let InterpValue::Enum(e) = &value
            && e.variant == VARIANT_ERR
            && let Some(payload) = e.payload.first()
        {
            let message = self.engine.error_report(payload);
            self.engine.release_in_entry(value);
            return Err(Error::MainReturnedErr { message });
        }
        self.engine.release_in_entry(value);
        Ok(())
    }

    // ---- the entry as coroutines (a host that cannot block) ----

    /// Starts the program of [`Vm::load_from_deferred`]: the top-level statements of a script
    /// as a coroutine, and `main` as one after them. A `wait` in either suspends it, as in any
    /// coroutine, so the host's loop goes on. Each runs until its first pause before this
    /// returns, and a panic before that pause is this call's error. A panic after a `wait` is
    /// reported by the [`Vm::tick`] that resumes it, like any coroutine's.
    pub fn start_entry(&mut self) -> Result<(), Error> {
        self.program.as_ref().ok_or(Error::NotLoaded)?;
        // Only the program of `load_from_deferred` has an entry that nothing has run, and only once.
        if !std::mem::take(&mut self.entry_pending) {
            return Err(Error::EntryNotPending);
        }
        let stage = if self.engine.has_script() {
            EntryStage::Script
        } else {
            EntryStage::Main
        };
        self.entry = Some(EntryRun {
            stage,
            handle: None,
        });
        self.begin_entry_stage(stage)?;
        // A part that ended before its first pause hands over to the next, and a panic of that
        // first run is this call's error.
        self.entry_step().map(|_| ())
    }

    fn begin_entry_stage(&mut self, stage: EntryStage) -> Result<(), Error> {
        let program = self.program.clone().ok_or(Error::NotLoaded)?;
        let (stage, handle) = match stage {
            EntryStage::Script => (EntryStage::Script, self.engine.start_entry(None)),
            EntryStage::Main => match program.main {
                Some(main) => (EntryStage::Main, self.engine.start_entry(Some(main))),
                None => {
                    self.set_entry(EntryStage::Over, None);
                    return Ok(());
                }
            },
            EntryStage::Over => return Ok(()),
        };
        self.engine.core_mut().flush();
        let handle = handle.map_err(|info| panic_error(self.engine.core(), *info))?;
        self.set_entry(stage, Some(handle));
        Ok(())
    }

    fn set_entry(&mut self, stage: EntryStage, handle: Option<InterpValue>) {
        self.entry = Some(EntryRun { stage, handle });
    }

    /// Moves the entry on when a part of it has ended: after the top-level statements `main`
    /// starts. `Ok(true)` once both are over; `Ok(false)` while one is still running. A `main`
    /// that returns `Result[()]` and ends in `err(e)` is [`Error::MainReturnedErr`]; a part that
    /// panicked gives that panic if no tick has reported it. Call it after each
    /// [`Vm::tick`], and after [`Vm::start_entry`].
    pub fn entry_step(&mut self) -> Result<bool, Error> {
        use colibri_core::sched::{DONE, H_RESULT, RUNNING, handle_of, handle_status};
        loop {
            let Some(run) = self.entry.as_mut() else {
                return Ok(true);
            };
            let stage = run.stage;
            if stage == EntryStage::Over {
                return Ok(true);
            }
            let Some(handle) = run.handle.as_ref() else {
                return Ok(true);
            };
            let Some(h) = handle_of(handle) else {
                return Ok(true);
            };
            let status = handle_status(h);
            if status == RUNNING {
                return Ok(false);
            }
            let result = h.field(H_RESULT);
            let handle = run.handle.take().expect("the handle was just read");
            self.engine.release_in_entry(handle);
            if status != DONE {
                // Panicked or cancelled: the panic is a failure no tick has returned yet.
                self.set_entry(EntryStage::Over, None);
                self.collect_failure()?;
                return Ok(true);
            }
            if stage == EntryStage::Main {
                self.set_entry(EntryStage::Over, None);
                let returns_result = self.program.as_ref().is_some_and(|p| {
                    p.main.is_some_and(|m| {
                        matches!(&p.fns[m.0 as usize].sig.ret, Ty::Adt(RESULT_TYPE, _))
                    })
                });
                if let Some(value) = result {
                    if returns_result
                        && let InterpValue::Enum(e) = &value
                        && e.variant == VARIANT_ERR
                        && let Some(payload) = e.payload.first()
                    {
                        let message = self.engine.error_report(payload);
                        self.engine.release_in_entry(value);
                        return Err(Error::MainReturnedErr { message });
                    }
                    self.engine.release_in_entry(value);
                }
                return Ok(true);
            }
            if let Some(value) = result {
                self.engine.release_in_entry(value);
            }
            self.begin_entry_stage(EntryStage::Main)?;
        }
    }

    /// How many seconds until a coroutine needs a tick: `Some(0.0)` for one that is due (or
    /// polls: `poll` is how often a coroutine that waits on a condition is looked at), `None`
    /// when nothing will be due until an event happens. What [`Vm::run_event_loop`] sleeps for.
    pub fn next_wake(&mut self, poll: f64) -> Option<f64> {
        self.engine.next_wake(poll)
    }

    /// A panic of a coroutine that no tick reported (it failed while starting or while being
    /// cancelled): `Err` with it, once.
    pub fn take_coroutine_failure(&mut self) -> Result<(), Error> {
        self.collect_failure()
    }

    // ---- coroutines ----

    /// Advances scheduler time by `dt` seconds and resumes every coroutine that is due, once,
    /// in the order the coroutines were started (DESIGN §14.2, ARCHITECTURE §16). A coroutine
    /// started during the tick waits for the next one. `tick(0.0)` pauses time: only coroutines
    /// waiting on `wait_until` or `wait(0)` can move.
    ///
    /// A panic in a coroutine ends that coroutine only; the others still run in this tick, and
    /// the first panic is returned as [`Error::Panic`] (the rest ride along as notes). The `Vm`
    /// stays usable. Each resumption gets a fresh instruction budget.
    pub fn tick(&mut self, dt: f64) -> Result<(), Error> {
        let result = self.engine.tick(dt);
        self.engine.core_mut().flush();
        result.map_err(|info| panic_error(self.engine.core(), *info))
    }

    /// How many coroutines are alive: started and neither finished nor cancelled.
    pub fn coroutine_count(&self) -> usize {
        self.engine.core().sched.coroutine_count()
    }

    /// How many times a coroutine has been resumed so far (starts, ticks and cancellations).
    /// A coroutine waiting for a `Signal` is not resumed until the signal is emitted.
    pub fn coroutine_resumptions(&self) -> u64 {
        self.engine.core().sched.resumptions()
    }

    /// How many coroutines are registered as waiting for a signal, a coroutine or a group
    /// (for tests).
    #[doc(hidden)]
    pub fn coroutine_waiters(&self) -> usize {
        self.engine.core().sched.waiter_count()
    }

    /// A flag that stops the program from another thread: once it is set, the program ends at its
    /// next safepoint, or in the `join` or channel wait it is in, with `Error::BudgetExhausted`
    /// (what an exhausted instruction budget gives). A `wait(seconds)`, `time.sleep` or other
    /// timer wait does not observe the flag: it ends when its time is up. For a host's watchdog;
    /// call it before running.
    #[doc(hidden)]
    pub fn stop_handle(&mut self) -> Arc<std::sync::atomic::AtomicBool> {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let core = self.engine.core_mut();
        core.set_cancel_flag(Some(Arc::clone(&flag)));
        if let Some(every) = std::num::NonZeroU32::new(4096) {
            core.want_safepoints(colibri_core::safepoint::SafepointKind::Cancel, every);
        }
        flag
    }

    /// Cancels every live coroutine, newest first, running their `defer`s and drops. Done
    /// automatically by [`Vm::shutdown`] and when the `Vm` is dropped.
    pub fn cancel_coroutines(&mut self) {
        self.engine.cancel_all_coroutines();
        self.engine.core_mut().flush();
    }

    /// Runs the event loop until no coroutine is alive, or until every live one waits for an
    /// event that nothing in the loop can cause: what `colibri run` does after `main`. In the
    /// second case (every live coroutine waits for a signal nobody emits, or in `join`,
    /// `join_result` or `join_all` for a coroutine or group that cannot end) it returns `Ok`
    /// with those coroutines still alive; they are cancelled at shutdown. Such waiters are
    /// never polled.
    /// It ticks the scheduler with the time elapsed on the [`VmBuilder::clock`], then sleeps on
    /// that clock until the next timed wake (`wait`, `wait_real`); coroutines that poll at
    /// every tick (`wait_until`) are checked every `poll` seconds (coroutines waiting for a
    /// signal, a `join` or a `join_all` are woken by the event and not polled),
    /// 1 ms by default ([`Vm::run_event_loop_polling`]). A panic in a coroutine ends the loop
    /// with that error; the remaining coroutines stay alive (and are cancelled at shutdown).
    pub fn run_event_loop(&mut self) -> Result<(), Error> {
        self.run_event_loop_polling(0.001)
    }

    /// [`Vm::run_event_loop`] with a chosen polling interval in seconds. Not for the web build:
    /// there the clock cannot sleep (see `colibri_core::clock::SourceClock`), so this would spin
    /// until the next timer is due; a web host calls [`Vm::tick`] from its own timers.
    pub fn run_event_loop_polling(&mut self, poll: f64) -> Result<(), Error> {
        let mut last = self.engine.core_mut().sched.clock_now();
        loop {
            if self.engine.core().sched.coroutine_count() == 0 {
                return self.collect_failure();
            }
            let now = self.engine.core_mut().sched.clock_now();
            let dt = (now - last).max(0.0);
            last = now;
            self.tick(dt)?;
            let Some(delay) = self.engine.next_wake(poll) else {
                return self.collect_failure();
            };
            self.engine.core_mut().sched.clock_sleep(delay);
        }
    }

    /// [`Vm::run_event_loop_polling`] for a program that is being reloaded: before every tick
    /// `between_ticks` runs (the one safe point for [`Vm::reload_changed`]; it may also call
    /// `main` again after a restart), and the loop never sleeps longer than `max_sleep`
    /// seconds, so the hook is called often enough to notice an edit. The loop ends when no
    /// coroutine is alive after the hook, or with the error of the hook or of a tick.
    pub fn run_event_loop_watching(
        &mut self,
        poll: f64,
        max_sleep: f64,
        mut between_ticks: impl FnMut(&mut Vm) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut last = self.engine.core_mut().sched.clock_now();
        loop {
            between_ticks(self)?;
            if self.engine.core().sched.coroutine_count() == 0 {
                return self.collect_failure();
            }
            let now = self.engine.core_mut().sched.clock_now();
            let dt = (now - last).max(0.0);
            last = now;
            self.tick(dt)?;
            let delay = match self.engine.next_wake(poll) {
                Some(delay) => delay.min(max_sleep),
                None if self.engine.core().sched.coroutine_count() == 0 => {
                    return self.collect_failure();
                }
                None => max_sleep,
            };
            self.engine.core_mut().sched.clock_sleep(delay);
        }
    }

    /// A panic of a coroutine that no tick reported (it failed while starting or while being
    /// cancelled).
    fn collect_failure(&mut self) -> Result<(), Error> {
        match self.engine.take_coroutine_failure() {
            Some(info) => Err(panic_error(self.engine.core(), *info)),
            None => Ok(()),
        }
    }

    // ---- tests ----

    /// The `test "name" { .. }` blocks of the loaded program, in source order (the entry
    /// module's first). They include the ones in dependencies (`TestInfo::root` is false for
    /// those). Empty before a program is loaded. The index in the list is the id
    /// [`Vm::run_test`] takes. The ids are valid only until the program is loaded again (by a
    /// reload or a hook that reloads): list the tests again after that, or go by name.
    pub fn tests(&self) -> Vec<TestInfo> {
        self.test_infos(false)
    }

    /// The `test` blocks of script modules (a module of the package that nothing imports and
    /// that has top-level statements): they are checked but cannot run in a `colibri test` of
    /// the folder, since that would run the script's initialisers. The ids are not valid for
    /// [`Vm::run_test`].
    pub fn skipped_tests(&self) -> Vec<TestInfo> {
        self.test_infos(true)
    }

    fn test_infos(&self, skipped: bool) -> Vec<TestInfo> {
        let Some(program) = &self.program else {
            return Vec::new();
        };
        let list = if skipped {
            &program.skipped_tests
        } else {
            &program.tests
        };
        list.iter()
            .enumerate()
            .map(|(id, t)| self.test_info(id, t))
            .collect()
    }

    fn test_info(&self, id: usize, t: &TestDef) -> TestInfo {
        let sources = &self.sources;
        let known = sources.contains(t.span.file);
        TestInfo {
            id,
            name: t.name.clone(),
            module: t.module.clone(),
            root: t.root,
            file: if known {
                sources.name(t.span.file).to_string()
            } else {
                String::new()
            },
            line: if known {
                sources.line_col(t.span.file, t.span.start).0
            } else {
                0
            },
        }
    }

    /// Sets a function that [`Vm::run_test`] calls before each test, after the previous test's
    /// module values are released and before the test's own are initialised. A test gets fresh
    /// module state, but what the host keeps in the `Vm` (handles from [`Vm::insert`], state from
    /// [`Vm::insert_state`], host-side counters) carries over from test to test; this is where
    /// the host puts that back the way a test expects it. The function gets the `Vm` and the
    /// test about to run. It replaces an earlier one. It runs for tests only, not for `load_*`,
    /// `run_main` or calls of ordinary functions.
    ///
    /// The script's module values do not exist while the hook runs (they were just released), so
    /// a script function that reads one panics with "used before it is initialised". A
    /// [`Vm::run_test`] called from inside the hook runs without the hook. A hook that panics
    /// stays registered. A hook that reloads the program ([`Vm::reload_changed`]) makes
    /// `run_test` run the test with the same name (and module) in the new program, wherever it is
    /// listed now, or fail with [`Error::FnNotFound`] when it has no such test. The ids you took
    /// from [`Vm::tests`] before that are stale (the new program may list the tests in another
    /// order): with a hook that reloads, go through the tests by name, or list them again after
    /// each one:
    ///
    /// ```ignore
    /// let names: Vec<String> = vm.tests().into_iter().map(|t| t.name).collect();
    /// for name in names {
    ///     let id = vm.tests().iter().find(|t| t.name == name).map(|t| t.id);
    ///     if let Some(id) = id { vm.run_test(id)?; }
    /// }
    /// ```
    pub fn on_test_start(&mut self, hook: impl FnMut(&mut Vm, &TestInfo) + 'static) {
        self.test_start_hook = Some(Box::new(hook));
    }

    /// Runs one test (an id from [`Vm::tests`]) from a fresh state: the module-level values are
    /// released (their `Drop` hooks run) and initialised again, the test runs, and they are
    /// released again, so no test sees what another one did and `Drop` output belongs to the
    /// test that caused it. The top-level statements of the entry file are not run. The
    /// instruction budget (if set) is a fresh allotment for the initialisers, the test body and
    /// the clean-up each.
    ///
    /// `Ok(())` if the test body finished or returned `ok(())`. A panic (a failed `assert`, an
    /// arithmetic fault, a `panic(..)`, a failing initialiser) is [`Error::Panic`], a test that
    /// returns `err(e)` is [`Error::TestReturnedErr`], and running out of budget is
    /// [`Error::BudgetExhausted`]. The `Vm` stays usable for the next test.
    ///
    /// Call it on a `Vm` that [`Vm::load_tests`] loaded. After a normal `load_*` it works too,
    /// but it throws away the module state the load built, so do not call `run_main` or other
    /// functions afterwards on that `Vm`: after any test the module-level values are released.
    pub fn run_test(&mut self, id: usize) -> Result<(), Error> {
        let program = self.program.clone().ok_or(Error::NotLoaded)?;
        let def = program.tests.get(id).ok_or_else(|| Error::FnNotFound {
            name: format!("test #{id}"),
        })?;
        let mut f = def.f;
        self.engine.release_globals();
        if let Some(mut hook) = self.test_start_hook.take() {
            let info = self.test_info(id, def);
            // The hook goes back even when it panics (the panic then goes on), so later tests
            // still run with it.
            let called =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(self, &info)));
            // The hook may have set a new one; that one wins.
            if self.test_start_hook.is_none() {
                self.test_start_hook = Some(hook);
            }
            if let Err(payload) = called {
                std::panic::resume_unwind(payload);
            }
            // The hook may have reloaded the program: look the test up again in the program that
            // is loaded now, by its name (an id counts positions, and the reload may have
            // added or removed a test before this one).
            let now = self.program.clone().ok_or(Error::NotLoaded)?;
            if !Rc::ptr_eq(&now, &program) {
                // A test name is unique within its module (a repeat is a load error).
                f = now
                    .tests
                    .iter()
                    .find(|t| t.name == info.name && t.module == info.module)
                    .ok_or_else(|| Error::FnNotFound {
                        name: format!(
                            "test #{id} `{}` (the `on_test_start` hook reloaded the program, which no longer has that test)",
                            info.name
                        ),
                    })?
                    .f;
                // A restart initialised the new program's module values; a test starts fresh.
                self.engine.release_globals();
            }
        }
        let outcome = match self.engine.run_initialisers() {
            Err(info) => Err(panic_error(self.engine.core(), *info)),
            Ok(()) => match self.engine.call(f, Vec::new()) {
                Err(info) => Err(panic_error(self.engine.core(), *info)),
                Ok(value) => {
                    let failed = match &value {
                        InterpValue::Enum(e) if e.variant == VARIANT_ERR => {
                            e.payload.first().map(|p| self.engine.error_report(p))
                        }
                        _ => None,
                    };
                    self.engine.release_in_entry(value);
                    match failed {
                        Some(message) => Err(Error::TestReturnedErr { message }),
                        None => Ok(()),
                    }
                }
            },
        };
        self.engine.release_globals();
        self.engine.core_mut().flush();
        outcome
    }

    /// A handle to the function `name`, if its Colibri signature matches `A -> R`.
    pub fn get_fn<A: Args, R: FromColibri>(&self, name: &str) -> Result<Fn<A, R>, Error> {
        let program = self.program.as_ref().ok_or(Error::NotLoaded)?;
        let id = program.fn_by_name(name).ok_or_else(|| Error::FnNotFound {
            name: name.to_string(),
        })?;
        let sig = &program.fns[id.0 as usize].sig;
        // The host's own types are known, so a class of a module is named as the program does.
        let mut registry = self.registry.clone();
        registry.host_side = true;
        let params: Vec<Option<TypeDesc>> = sig
            .params
            .iter()
            .map(|p| TypeDesc::of(p, program))
            .collect();
        let wanted: Vec<TypeDesc> = A::tys(&mut registry);
        let ret = TypeDesc::of(&sig.ret, program);
        let want_ret = R::ty(&mut registry);
        if let Some(conflict) = registry.conflicts.first() {
            return Err(Error::Registration {
                name: name.to_string(),
                message: conflict.clone(),
            });
        }
        let matches = params.len() == wanted.len()
            && params
                .iter()
                .zip(&wanted)
                .all(|(a, b)| a.as_ref() == Some(b))
            && ret.as_ref() == Some(&want_ret);
        if !matches {
            let list: Vec<String> = wanted.iter().map(TypeDesc::to_string).collect();
            let mut rust = format!("fn({})", list.join(", "));
            if want_ret != TypeDesc::Unit {
                rust.push_str(&format!(" -> {want_ret}"));
            }
            return Err(Error::SignatureMismatch {
                name: name.to_string(),
                colibri: sig.render(&program.types),
                rust,
            });
        }
        Ok(Fn::new(id, self.id, self.epoch))
    }

    /// Like [`Vm::get_fn`] for an optional hook: `Ok(None)` when the script does not define
    /// `name`. A function that exists with another signature is still an error.
    pub fn find_fn<A: Args, R: FromColibri>(&self, name: &str) -> Result<Option<Fn<A, R>>, Error> {
        match self.get_fn(name) {
            Ok(f) => Ok(Some(f)),
            Err(Error::FnNotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Sets a function that each load, and each reload that restarts the program (not one that
    /// only patches function bodies), calls with the warnings of a program that checked, when
    /// there are any: after the check and before any code of the program runs (initialisers,
    /// an entry script's statements, `main`).
    pub fn on_warnings(&mut self, hook: impl FnMut(&Report) + 'static) {
        self.warning_hook = Some(Box::new(hook));
    }

    /// Warnings from the last successful load.
    pub fn warnings(&self) -> &[Diagnostic] {
        &self.warnings
    }

    /// The last warnings rendered with source snippets.
    pub fn render_warnings(&self, color: bool) -> String {
        Report {
            sources: (*self.sources).clone(),
            diagnostics: self.warnings.clone(),
        }
        .render(color)
    }

    /// The sources of the loaded program.
    pub fn sources(&self) -> &SourceMap {
        &self.sources
    }

    /// Ends the program's life: callbacks go stale, module-level values are released in
    /// reverse initialisation order (DESIGN §11.2). Done automatically when the `Vm` is
    /// dropped; call it to see the effects (and the drop trace) at a chosen moment. The program
    /// is gone afterwards, and function handles with it.
    pub fn shutdown(&mut self) {
        self.reload = None;
        self.launch.set_image(None);
        if self.track_leaks {
            // The leak detector runs in every isolate, and a worker's report comes to this `Vm`
            // when its thread ends: the pool shuts down (what runs is cancelled), and what the
            // workers found is kept for `find_leaks`. The pool does not start again.
            self.worker_leaks
                .extend(self.launch.pool().shutdown_and_collect_leaks());
        }
        self.teardown();
    }

    /// Lets go of the handle of the entry's coroutine, if the entry has not ended.
    fn release_entry(&mut self) {
        if let Some(run) = self.entry.take()
            && let Some(handle) = run.handle
        {
            self.engine.release_in_entry(handle);
        }
    }

    fn teardown(&mut self) {
        self.release_entry();
        self.entry_pending = false;
        self.engine.shutdown();
        self.engine.core_mut().flush();
        if self.program.take().is_some() {
            self.epoch += 1;
        }
        self.launch.set_program(None);
        self.engine
            .core_mut()
            .set_owner((self.id, self.epoch), false, Rc::clone(&self.sources));
    }

    /// The reference cycles that nothing reaches: garbage that reference counting can never
    /// free. Works at any moment: mid-run it finds the cycles that no frame, temporary,
    /// global, suspended coroutine or host-held value reaches (a cycle that is still in use is
    /// not reported); after [`Vm::shutdown`] (or at the end of `colibri run`) every cycle
    /// that is left is a leak. `weak` references are not edges. It reports and frees nothing,
    /// and finds only objects made while [`VmBuilder::track_leaks`] was on; without it the
    /// result is empty. The cost is linear in the live objects. See ARCHITECTURE §18.
    pub fn find_leaks(&self) -> Vec<Leak> {
        let mut found = self.engine.find_leaks();
        found.extend(self.worker_leaks.iter().cloned());
        found
    }

    /// Starts the sampling profiler: about every 1,024 steps it records the stack that runs.
    /// Starting again begins a new profile. Not part of the stable interface yet: the format
    /// may change until it has settled.
    #[doc(hidden)]
    pub fn start_profile(&mut self, weight: crate::ProfileWeight) {
        self.engine.start_profile(weight);
    }

    /// Stops the profiler and returns what it found (`None` if none was started). The steps
    /// or time since the last sample are a final sample.
    #[doc(hidden)]
    pub fn finish_profile(&mut self) -> Option<crate::Profile> {
        let locate = self.locator();
        self.engine.finish_profile(&locate)
    }

    /// Makes the code that is loaded from now on count its reference-count traffic by line
    /// (copies, clones and releases of values that have a reference count). Call it before
    /// loading the program: starting after a load counts only the code compiled later, and
    /// [`Vm::finish_rc_profile`] does not take the `Count` instructions out of code that is
    /// already compiled (each stays a cheap call that does nothing). Not part of the
    /// stable interface yet. See [`Vm::finish_rc_profile`].
    #[doc(hidden)]
    pub fn start_rc_profile(&mut self) {
        self.engine.start_rc_profile();
    }

    /// Stops counting and returns the lines (`None` if counting was not started). The `Count`
    /// instructions that were compiled stay in the code after this call; they make a cheap call
    /// that does nothing. Counted: the
    /// register copies, clones and releases the program's own code makes, the values a
    /// constructor keeps and a list's `push`. Not counted: what built-ins written in Rust do
    /// inside (a `sort` copying its list, `join`, `map` lookups), host functions, field and
    /// element writes, the cleanup a panic runs, and values moved rather than copied.
    #[doc(hidden)]
    pub fn finish_rc_profile(&mut self) -> Option<crate::RcProfile> {
        let locate = self.locator();
        self.engine.finish_rc_profile(&locate)
    }

    /// Attaches a debugger (hidden: the interface is not settled; see `crate::debug`). Debug mode
    /// applies to the code loaded from now on, so call it before loading the program: the
    /// compiler then emits a step per statement and keeps what a debugger needs, and nothing
    /// runs as a leaf. The debugger is called when the program stops (a breakpoint, a step, a
    /// pause, an uncaught panic when [`Vm::debug_break_on_panic`] is on) and every 1,024
    /// steps while it runs; it must not run program code. Output and results are the same as
    /// without it.
    #[doc(hidden)]
    pub fn set_debugger(&mut self, debugger: Box<dyn crate::debug::Debugger>) {
        let locator = self.debug_locator();
        self.engine.set_debugger(debugger, locator);
    }

    /// Detaches the debugger; its breakpoints go out of the code.
    #[doc(hidden)]
    pub fn clear_debugger(&mut self) {
        self.engine.clear_debugger();
    }

    /// Replaces the breakpoints of the source file `file` (its name as `sources()` has it). One
    /// result per breakpoint; a breakpoint in a function that is not compiled yet is not
    /// verified until it is. `None` when the program has no such file.
    #[doc(hidden)]
    pub fn debug_set_breakpoints(
        &mut self,
        file: &str,
        specs: &[crate::debug::BreakpointSpec],
    ) -> Option<Vec<crate::debug::BreakpointInfo>> {
        let id = self.debug_file(file)?;
        Some(self.engine.debug_set_breakpoints(id, specs))
    }

    /// The id of the loaded source file called `name`.
    #[doc(hidden)]
    pub fn debug_file(&self, name: &str) -> Option<crate::debug::FileId> {
        (0..self.sources.next_file_id().0)
            .map(crate::debug::FileId)
            .find(|f| self.sources.name(*f) == name)
    }

    /// Stop at a panic that nothing handles, before anything unwinds.
    #[doc(hidden)]
    pub fn debug_break_on_panic(&mut self, on: bool) {
        self.engine.debug_break_on_panic(on);
    }

    /// Stops the program at its first statement, as a step (the stop on entry of a debug
    /// session). Call it before running the program.
    #[doc(hidden)]
    pub fn debug_stop_on_entry(&mut self) {
        self.engine.debug_stop_on_entry();
    }

    /// The handle that pauses the running program at its next safepoint, from any thread.
    #[doc(hidden)]
    pub fn debug_pause_handle(&self) -> crate::debug::PauseHandle {
        self.engine.debug_pause_handle()
    }

    fn debug_locator(&self) -> Rc<dyn colibri_vm::debug::Locator> {
        Rc::new(SourceLines(Rc::clone(&self.sources)))
    }

    /// A program was loaded or patched: the debugger reads its lines from the new sources.
    fn sync_debug_locator(&mut self) {
        let locator = self.debug_locator();
        self.engine.set_debug_locator(locator);
    }

    /// Turns a span into a file name and a 1-based line, for the profilers.
    fn locator(&self) -> impl std::ops::Fn(colibri_syntax::Span) -> Option<(String, u32)> + use<> {
        let sources = Rc::clone(&self.sources);
        move |span: colibri_syntax::Span| {
            let known = (span.file.0 < sources.next_file_id().0).then_some(span.file)?;
            // A span of the prelude or of a file the program no longer has has no line.
            if colibri_check::is_prelude_span(span) {
                return None;
            }
            let (line, _) = sources.line_col(known, span.start);
            Some((sources.name(known).to_string(), line))
        }
    }

    /// The labels of the class objects destroyed so far, in order, when the `Vm` was built
    /// with [`VmBuilder::trace_drops`]; the trace is emptied. Test support.
    #[doc(hidden)]
    pub fn take_drop_trace(&mut self) -> Vec<String> {
        self.engine.core_mut().take_trace()
    }
}

/// What the debugger reads lines and file names from.
struct SourceLines(Rc<SourceMap>);

impl colibri_vm::debug::Locator for SourceLines {
    fn line(&self, span: colibri_syntax::Span) -> Option<u32> {
        // A span of the prelude or of a file the program no longer has has no line.
        if !self.0.contains(span.file) || colibri_check::is_prelude_span(span) {
            return None;
        }
        Some(self.0.line_col(span.file, span.start).0)
    }

    fn file(&self, span: colibri_syntax::Span) -> Option<String> {
        if !self.0.contains(span.file) || colibri_check::is_prelude_span(span) {
            return None;
        }
        Some(self.0.name(span.file).to_string())
    }

    fn file_name(&self, id: crate::debug::FileId) -> Option<String> {
        self.0.contains(id).then(|| self.0.name(id).to_string())
    }

    fn file_id(&self, name: &str) -> Option<crate::debug::FileId> {
        (0..self.0.next_file_id().0)
            .map(crate::debug::FileId)
            .find(|f| self.0.name(*f) == name)
    }
}

/// Wraps a host function for the interpreter: arguments are handed over as a slice, panics
/// become Colibri panics, and the references the interpreter passed are released afterwards.
fn trampoline(name: &str, module: &str, raw: crate::host::RawHostFn) -> HostFnImpl {
    let name = name.to_string();
    let module: Rc<str> = Rc::from(module);
    Rc::new(move |eng: &mut dyn HostCx, args: Vec<InterpValue>| {
        let args: Vec<Value> = args.into_iter().map(Value).collect();
        let mut cx = Ctx::in_module(eng, Rc::clone(&module));
        let outcome = catch_unwind(AssertUnwindSafe(|| raw(&mut cx, &args)));
        for arg in args {
            cx.release(arg);
        }
        match outcome {
            Ok(Ok(value)) => Ok(value.0),
            Ok(Err(fault)) => Err(match fault {
                HostFault::Panic(message) => {
                    HostFault::Panic(format!("host function `{name}`: {message}"))
                }
                // Worded by the host layer, shown as is.
                HostFault::Message(message) => HostFault::Message(message),
                HostFault::Cancel => HostFault::Cancel,
                HostFault::MemoryLimit => HostFault::MemoryLimit,
                other => other,
            }),
            Err(payload) => Err(match payload.downcast::<crate::ctx::RaisedPanic>() {
                Ok(raised) => HostFault::Message(raised.0),
                Err(payload) => match payload.downcast::<crate::ctx::RaisedCancel>() {
                    Ok(_) => HostFault::Cancel,
                    Err(payload) => match payload.downcast::<crate::ctx::RaisedMemory>() {
                        Ok(_) => HostFault::MemoryLimit,
                        Err(payload) => HostFault::Panic(format!(
                            "host function `{name}` panicked: {}",
                            payload_message(payload.as_ref())
                        )),
                    },
                },
            }),
        }
    })
}

/// The name a binding crate gives the type of its registration function's parameter
/// (`fn physics(m: &mut colibri::Module<'_>)`, see `#[colibri::module]`).
pub type Module<'a> = HostModule<'a>;

/// A host module: a namespace of functions and types that scripts reach with `import`
/// ([`Vm::module`]).
pub struct HostModule<'vm> {
    vm: &'vm mut Vm,
    name: String,
    /// Made by [`Vm::std_module`].
    std: bool,
}

/// The error for registering into the reserved module name `std` (or `std.x`) through
/// [`Vm::module`]: only [`Vm::std_module`] makes those.
fn reserved(name: &str) -> Option<Error> {
    (name == "std" || name.starts_with("std.")).then(|| Error::Registration {
        name: name.to_string(),
        message: "the module name `std` is reserved for the standard library".to_string(),
    })
}

impl HostModule<'_> {
    /// The module's name, as `import` writes it (`std.math` for a standard library module,
    /// which `import math` also reaches).
    pub fn name(&self) -> &str {
        &self.name
    }

    fn guard(&self) -> Result<(), Error> {
        match reserved(&self.name) {
            Some(e) if !self.std => Err(e),
            _ => Ok(()),
        }
    }

    /// Registers the constant `NAME: float = value` in this module (`math.PI`). The value must
    /// be finite, not negative and writable as a plain decimal literal.
    pub fn register_const_float(&mut self, name: &str, value: f64) -> Result<(), Error> {
        self.guard()?;
        self.vm
            .register_const_in(&self.name, name, ConstValue::Float(value))
    }

    /// Registers the constant `NAME: int = value` in this module.
    pub fn register_const_int(&mut self, name: &str, value: i64) -> Result<(), Error> {
        self.guard()?;
        let Ok(value) = u64::try_from(value) else {
            return Err(Error::Registration {
                name: name.to_string(),
                message: "a constant cannot be negative".to_string(),
            });
        };
        self.vm
            .register_const_in(&self.name, name, ConstValue::Int(value))
    }

    /// Registers the host class `T` in this module: [`HostModule::register_type`] under the
    /// name a binding crate uses (`m.add_class::<World>()?`).
    pub fn add_class<T: crate::IntoColibri>(&mut self) -> Result<(), Error> {
        self.register_type::<T>()
    }

    /// Registers an exported function in this module: the `export` item that
    /// `#[colibri::export] fn name` writes next to the function (`m.add_fn(name::export)?`).
    pub fn add_fn(
        &mut self,
        export: impl FnOnce(&mut HostModule<'_>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.guard()?;
        export(self)
    }

    /// Like [`Vm::register_fn`], into this module.
    pub fn register_fn<Marker, F: HostFn<Marker>>(
        &mut self,
        name: &str,
        f: F,
    ) -> Result<Registration<'_>, Error> {
        self.guard()?;
        self.vm.register_inner(&self.name, name, None, f)
    }

    /// Like [`Vm::register_fn_named`], into this module.
    pub fn register_fn_named<Marker, F: HostFn<Marker>, const N: usize>(
        &mut self,
        name: &str,
        params: [&str; N],
        f: F,
    ) -> Result<Registration<'_>, Error> {
        self.guard()?;
        let names = params.iter().map(ToString::to_string).collect();
        self.vm.register_inner(&self.name, name, Some(names), f)
    }

    /// Like [`Vm::register_type`], into this module. A type belongs to the module that first
    /// describes it, and a module's signatures may mention only its own types and those of the
    /// default `host` module (anything else is [`Error::Registration`]).
    pub fn register_type<T: crate::IntoColibri>(&mut self) -> Result<(), Error> {
        self.guard()?;
        self.vm.register_type_in::<T>(&self.name)
    }

    /// Declares that this module's interface names types of the module `module` (`std.json`
    /// for the standard library's, as [`Vm::module`] and [`Vm::std_module`] name them): a
    /// function or method of this module may then take or return its classes (a class that
    /// the Rust type implements, found by [`TypeRegistry::define_host_type`] and written
    /// `json.Value` in the interface) and its structs ([`TypeRegistry::foreign_type`]).
    /// Without this declaration, registering a signature that mentions another module's type is
    /// an [`Error::Registration`] that names the missing declaration.
    ///
    /// The module `module` must have something registered: one of the standard library that
    /// was declared and not registered yet is registered now, so a program that imports only
    /// this module still gets both, and before this module's own items, so that the types
    /// exist when they are mentioned. The module's interface file lists what it uses
    /// (`uses = ["std.json"]` in the header), and `colibri check --interface` then needs the
    /// other file too. Not for the default module, whose names are in every module's scope.
    pub fn uses(&mut self, module: &str) -> Result<(), Error> {
        self.guard()?;
        self.vm.declare_use(&self.name, module)
    }

    /// Declares the payload-free enum `name` with these variants as this module's own type
    /// (`fs.Error`): scripts write `fs.Error` and `.NotFound`, and another module may declare
    /// an enum of the same name. Host functions return an error that carries one of its
    /// variants with [`TypedError`], and a script recovers it with `e.is[fs.Error]()` and
    /// `e.as[fs.Error]()`. The module's signatures cannot mention the enum.
    ///
    /// Refused with [`Error::Registration`]: the default module (`vm.module("host")`, whose
    /// names every script sees: it has no `fs.` to tell two `Error`s apart), a name the
    /// language or its prelude already declares (`Option`, `Result`, `Show`, ..; only `Error`
    /// is for modules to reuse), a name that another host type of any module already uses (or
    /// uses later: a module enum's bare name is its own), no variants, a variant twice, and a
    /// name or variant that is not an identifier.
    pub fn register_module_enum(&mut self, name: &str, variants: &[&str]) -> Result<(), Error> {
        self.guard()?;
        let fail = |message: String| Error::Registration {
            name: name.to_string(),
            message,
        };
        if self.name == DEFAULT_MODULE {
            return Err(fail(
                "a module enum belongs to a named module (`vm.module(\"disk\")`): the default module's names are in every script's scope without a module name to tell them apart".to_string(),
            ));
        }
        if !is_module_name(name) {
            return Err(fail("this is not a type name".to_string()));
        }
        if name != "Error" && colibri_check::is_reserved_type_name(name) {
            return Err(fail(format!(
                "`{name}` is a type of the language or its prelude and cannot be redefined"
            )));
        }
        if variants.is_empty() {
            return Err(fail("an enum needs at least one variant".to_string()));
        }
        for (i, v) in variants.iter().enumerate() {
            if !is_module_name(v) {
                return Err(fail(format!("`{v}` is not a variant name")));
            }
            if variants[..i].contains(v) {
                return Err(fail(format!("the variant `{v}` is declared twice")));
            }
        }
        self.vm.describe_in(&self.name, name, |reg| {
            let _ = reg.define_module_enum(name, variants);
        })
    }

    /// The canonical interface (`.cbi`) text of this module (see [`Vm::interface_text`]).
    pub fn interface_text(&self) -> String {
        self.vm.rendered_module(&self.name).text
    }

    /// The hash of this module's interface (see [`Vm::interface_hash`]).
    pub fn interface_hash(&self) -> String {
        self.vm.rendered_module(&self.name).hash
    }

    /// Writes this module's interface to `path` unless it already holds exactly that.
    pub fn write_interface(&self, path: impl AsRef<Path>) -> Result<bool, Error> {
        iface::write_if_changed(path.as_ref(), &self.interface_text())
    }

    /// Checks that the file at `path` is this module's interface (see
    /// [`Vm::verify_interface`]).
    pub fn verify_interface(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        self.vm
            .verify_text(path.as_ref(), &self.vm.rendered_module(&self.name))
    }
}

/// A package that declares a Rust binding can only run in the project runner, which registers
/// the binding module. A `Vm` that does not have the module says so at the import, instead of
/// leaving the checker to report an unknown name.
fn refuse_unregistered_bindings(
    loaded: &mut colibri_check::package::Loaded,
    registered: &[String],
) {
    use colibri_check::package::ImportTarget;
    use colibri_syntax::ast::{ItemKind, TopLevel};
    let mut problems = Vec::new();
    for (file, meta) in loaded.files.iter().zip(&loaded.metas) {
        for top in &file.items {
            let TopLevel::Item(item) = top else { continue };
            let ItemKind::Import(imp) = &item.kind else {
                continue;
            };
            if let Some(ImportTarget::Host(name)) = meta.imports.get(&item.id)
                && !registered.contains(name)
                && loaded.rust_bindings.iter().any(|b| b.key == *name)
            {
                problems.push(
                    colibri_syntax::Diagnostic::error(
                        format!("`{name}` is a Rust binding: a program that uses it runs in the project runner"),
                        match (imp.path.first(), imp.path.last()) {
                            (Some(a), Some(b)) => a.span.to(b.span),
                            _ => item.span,
                        },
                    )
                    .with_hint(
                        "the project runner (built from `[rust-dependencies]`) is not available yet; `colibri check` works without it",
                    ),
                );
            }
        }
    }
    loaded.diagnostics.extend(problems);
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    /// This test binary does not install the counting allocator, which is what the check of
    /// the memory-limit setter reports.
    #[test]
    fn the_limit_setter_refuses_without_the_allocator() {
        let mut vm = Vm::builder().build();
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            vm.set_memory_limit(Some(1 << 20));
        }))
        .expect_err("no allocator, so the setter panics");
        let text = err.downcast_ref::<String>().expect("a formatted message");
        assert!(
            text.starts_with("a memory limit needs the counting allocator"),
            "{text}"
        );
        assert_eq!(vm.memory_used(), None);
        // Lifting a limit that was never set needs nothing.
        vm.set_memory_limit(None);
    }
}
