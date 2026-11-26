//! The seam between the runtime and the engine that runs Colibri code.
//!
//! [`RuntimeEngine`] is what a [`crate::Vm`] asks of an engine, and [`HostCx`] is the subset a
//! host function (a [`crate::Ctx`]) needs while it runs. Both are object-safe and
//! runtime-internal: nothing here is public, so the embedding API does not depend on which
//! engine is behind it. The bytecode VM implements
//! them, in `engine_vm`.
//!
//! An engine owns its main fiber, so the runtime never names a fiber type; a host function
//! gets the fiber it runs on through its `&mut dyn HostCx`.

use std::any::{Any, TypeId as AnyTypeId};
use std::rc::Rc;

use colibri_check::hir::{ExternDef, FnId, GlobalId, HostFnId, Program};
use colibri_check::types::TypeId;
use colibri_core::Core;
use colibri_core::ctl::PanicInfo;
use colibri_core::external::{CancelToken, ExternalError, Waited};
use colibri_core::host::HostFault;
use colibri_core::hostheap::{HostBorrowError, HostGuard};
use colibri_core::leaks::Leak;
use colibri_core::value::Value as CoreValue;

/// A host function as an engine calls it: the context of the call, and the arguments, which
/// are the caller's references: the function must release them (or hand them on).
pub(crate) type HostFnImpl =
    Rc<dyn Fn(&mut dyn HostCx, Vec<CoreValue>) -> Result<CoreValue, HostFault>>;

/// What a host class customises: how its objects are shown and compared.
#[derive(Clone, Default)]
pub(crate) struct ClassHooks {
    pub show: Option<HostFnImpl>,
    pub eq: Option<HostFnImpl>,
    pub hash: Option<HostFnImpl>,
}

/// What a host function may use of the engine that is running it: the shared state, the fiber
/// it runs on, calls back into Colibri code and the wait for a background operation.
pub(crate) trait HostCx {
    fn core(&self) -> &Core;
    fn core_mut(&mut self) -> &mut Core;

    /// How many host functions are running, one inside another, on this fiber.
    fn host_depth(&self) -> u32;

    /// Lets go of a value the host owns, so a `Drop` hook it was the last owner of can run.
    fn release(&mut self, value: CoreValue);

    /// What `main` returning `err(e)` reports: the whole chain of `e`.
    fn error_report(&mut self, error: &CoreValue) -> String;

    /// How many parameters function `f` takes; `None` if there is no such function.
    fn fn_param_count(&self, f: FnId) -> Option<u32>;

    /// Calls a function of the loaded program.
    fn call(&mut self, f: FnId, args: Vec<CoreValue>) -> Result<CoreValue, Box<PanicInfo>>;

    /// Calls a function or closure value.
    fn call_function(
        &mut self,
        f: &CoreValue,
        args: Vec<CoreValue>,
    ) -> Result<CoreValue, Box<PanicInfo>>;

    /// Borrows a host object, shared or exclusive. `in_call`: on behalf of the host function
    /// this fiber is running; otherwise host code between calls.
    fn host_borrow(
        &self,
        in_call: bool,
        index: u32,
        generation: u32,
        any: AnyTypeId,
        exclusive: bool,
    ) -> Result<HostGuard, HostBorrowError>;

    /// Whether a wait for a background operation lets the rest of the program run meanwhile.
    fn external_wait_runs_others(&self) -> bool;

    /// The engine's half of waiting for background operation `op`: suspend the coroutine, or
    /// drive the event loop, until it is completed. See [`Waited`].
    fn wait_for_completion(
        &mut self,
        op: u64,
        token: &CancelToken,
    ) -> Result<Waited, ExternalError>;
}

impl dyn HostCx + '_ {
    /// Runs `work` on a background thread and waits for its result. The result type is
    /// generic, so this is not a trait method: the type-independent halves are in the
    /// scheduler, and the wait is the engine's.
    pub(crate) fn await_external_with<R: Send + 'static>(
        &mut self,
        work: impl FnOnce(&CancelToken) -> R + Send + 'static,
    ) -> Result<R, ExternalError> {
        let pending = self.core_mut().sched.submit_work(work)?;
        let waited = self.wait_for_completion(pending.op, &pending.token)?;
        self.core_mut().sched.collect(pending, waited)
    }

    /// Waits for an operation that some other thread completes (see
    /// `colibri_core::external::Completer`).
    pub(crate) fn await_completion<R: Send + 'static>(
        &mut self,
        start: impl FnOnce(colibri_core::external::Completer<R>) -> Result<(), String>,
    ) -> Result<R, ExternalError> {
        let pending = self.core_mut().sched.start_completion(start)?;
        let waited = self.wait_for_completion(pending.op, &pending.token)?;
        self.core_mut().sched.collect(pending, waited)
    }
}

/// What a [`crate::Vm`] asks of an engine. Each engine owns its main fiber.
pub(crate) trait RuntimeEngine: HostCx {
    /// Compile every function when a program is loaded, patched or extended (see
    /// [`crate::VmBuilder::compile_all_at_load`]); the default is to compile lazily.
    fn set_compile_all(&mut self, on: bool);

    /// The size of the native stack a coroutine's call of a host function runs on.
    fn set_host_stack_size(&mut self, bytes: usize);

    /// Puts a checked program to work: the code tables, the host function slots. Module
    /// initialisers do not run yet.
    fn load(&mut self, program: &Program);

    /// [`RuntimeEngine::load`] for a thread task's isolate: only the `const`s are module state
    /// (`run_module_inits` initialises just those), and any other module value belongs to the
    /// main thread.
    fn load_worker(&mut self, program: &Program);

    /// Copies a value into its portable form on this thread.
    fn to_portable(
        &self,
        value: &CoreValue,
        stack: colibri_core::portable::StackGuard,
    ) -> Result<colibri_core::portable::Portable, colibri_core::portable::NotSendable>;

    /// Replaces the bodies of the functions of the loaded program with those of `program`
    /// (hot reload; the declarations are unchanged). An engine that cannot swap them without
    /// harming what runs (a coroutine parked in a host function whose declaration changed)
    /// refuses with the reason and changes nothing; the caller then restarts.
    fn patch(&mut self, program: &Program) -> Result<(), String>;

    /// Runs the module initialisers and the top-level statements.
    fn run_module(&mut self) -> Result<(), Box<PanicInfo>>;

    /// The first half of [`RuntimeEngine::run_module`]: the module initialisers, and not the
    /// script's top-level statements.
    fn run_module_inits(&mut self) -> Result<(), Box<PanicInfo>>;

    /// Whether the program has top-level statements.
    fn has_script(&self) -> bool;

    /// Starts `main` (`Some`) or the top-level statements (`None`) as a coroutine, and gives its
    /// handle. It runs to its first pause before this returns.
    fn start_entry(&mut self, main: Option<FnId>) -> Result<CoreValue, Box<PanicInfo>>;

    /// Runs the module-level value initialisers again, after `release_globals`.
    fn run_initialisers(&mut self) -> Result<(), Box<PanicInfo>>;

    /// `no_await`: the function never waits for anything outside the program.
    fn set_host_fn(&mut self, id: HostFnId, imp: HostFnImpl, no_await: bool);
    fn set_host_class_hooks(&mut self, ty: TypeId, hooks: ClassHooks);

    /// Binds an `extern` function to its C library; the error is what a load failure shows.
    fn bind_extern(&mut self, id: HostFnId, name: &str, def: &ExternDef) -> Result<(), String>;

    // ---- REPL sessions (ARCHITECTURE §20) ----

    /// Puts the code of `program`, which extends the loaded one, to work. `closure_map` says
    /// where each old function went. What it returns is handed back to `session_commit` or
    /// `session_rollback`.
    fn session_extend(
        &mut self,
        program: &Program,
        closure_map: Vec<u32>,
        reinit: &[GlobalId],
    ) -> Box<dyn Any>;
    fn session_run(
        &mut self,
        program: &Program,
        new_from: usize,
        reinit: &[GlobalId],
    ) -> Result<(), Box<PanicInfo>>;
    fn session_commit(&mut self, undo: Box<dyn Any>);
    fn session_rollback(&mut self, undo: Box<dyn Any>, old: &Program);
    fn session_show(&mut self, g: GlobalId) -> Result<String, Box<PanicInfo>>;

    // ---- the end of the program ----

    /// Releases the module-level values but keeps the host's roots.
    fn release_globals(&mut self);
    /// Ends the program's life. Safe to call twice.
    fn shutdown(&mut self);

    /// Releases a value outside any call, with a budget of its own for the `Drop` hooks.
    fn release_in_entry(&mut self, value: CoreValue);

    // ---- coroutines ----

    fn cancel_all_coroutines(&mut self);
    /// Resumes every due coroutine once.
    fn tick(&mut self, dt: f64) -> Result<(), Box<PanicInfo>>;
    /// How long until the next coroutine needs a tick (see `Machine::next_wake`).
    fn next_wake(&mut self, poll: f64) -> Option<f64>;
    /// Panics that no `tick` has reported yet.
    fn take_coroutine_failure(&mut self) -> Option<Box<PanicInfo>>;

    /// The cycles that nothing reaches.
    fn find_leaks(&self) -> Vec<Leak>;

    /// Starts the sampling profiler.
    fn start_profile(&mut self, weight: colibri_vm::ProfileWeight);
    /// Stops it; `locate` turns a span into a file name and a line.
    fn finish_profile(
        &mut self,
        locate: &dyn Fn(colibri_syntax::Span) -> Option<(String, u32)>,
    ) -> Option<colibri_vm::Profile>;

    /// Attaches a debugger: debug mode for the code loaded from now on.
    fn set_debugger(
        &mut self,
        debugger: Box<dyn colibri_vm::debug::Debugger>,
        locator: std::rc::Rc<dyn colibri_vm::debug::Locator>,
    );
    /// Detaches it and takes its breakpoints out of the code.
    fn clear_debugger(&mut self);
    /// Gives the debugger the lines of the program that is loaded now.
    fn set_debug_locator(&mut self, locator: std::rc::Rc<dyn colibri_vm::debug::Locator>);
    fn debug_set_breakpoints(
        &mut self,
        file: colibri_syntax::FileId,
        specs: &[colibri_vm::debug::BreakpointSpec],
    ) -> Vec<colibri_vm::debug::BreakpointInfo>;
    fn debug_break_on_panic(&mut self, on: bool);
    fn debug_pause_handle(&self) -> colibri_vm::debug::PauseHandle;
    fn debug_stop_on_entry(&mut self);

    /// Makes the code compiled from now on count reference-count traffic by line.
    fn start_rc_profile(&mut self);
    /// Stops counting; `locate` turns a span into a file name and a line.
    fn finish_rc_profile(
        &mut self,
        locate: &dyn Fn(colibri_syntax::Span) -> Option<(String, u32)>,
    ) -> Option<colibri_vm::RcProfile>;
}
