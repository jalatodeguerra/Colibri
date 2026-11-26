//! The machine: loaded code, module-level values and the state shared with core.

use std::cell::OnceCell;
use std::io::Write;
use std::rc::Rc;

use colibri_check::hir::{Body, FnId, GlobalId, Program};
use colibri_core::Core;
use colibri_core::ctl::PanicInfo;
use colibri_core::dispatch::Tables;
use colibri_core::value::Value;

use crate::bytecode::Chunk;
use crate::cffi::CffiState;
use crate::compile::{CompileEnv, compile_body};
use crate::coro::FiberSlab;
use crate::host::HostImpls;
use crate::stacks::HostStacks;

/// A body and its chunk, compiled the first time it is called.
pub(crate) struct Slot {
    pub body: Rc<Body>,
    pub chunk: OnceCell<Rc<Chunk>>,
}

impl Slot {
    pub fn new(body: Rc<Body>) -> Slot {
        Slot {
            body,
            chunk: OnceCell::new(),
        }
    }

    /// The compiled chunk, compiling it on first use.
    pub fn chunk(&self, id: Option<FnId>, env: &CompileEnv) -> Result<Rc<Chunk>, String> {
        if let Some(c) = self.chunk.get() {
            return Ok(Rc::clone(c));
        }
        let chunk = Rc::new(compile_body(&self.body, id, env)?);
        if let Some(debug) = &env.debug {
            debug.adopt(&chunk);
        }
        let _ = self.chunk.set(Rc::clone(&chunk));
        Ok(chunk)
    }
}

/// Test-only callback run at every `print`.
#[cfg(test)]
pub(crate) type PrintHook = Rc<dyn Fn(&mut Machine, &mut crate::fiber::VmFiber)>;

/// The bytecode virtual machine: the engine behind `colibri_core::engine::Engine`.
///
/// **A machine must not move while a coroutine is parked in a host function.** A host function
/// that a coroutine called runs on a native stack of its own (see `stacks`) and keeps references
/// into the machine across its waits; a machine that is moved (or dropped from another place after
/// a move) while one is parked makes the process abort at the next resume, because going on would
/// be undefined behaviour. Keep the machine in a `Box` (the runtime does) or do not move it after
/// the first `go`. Moving it before anything runs, or when no coroutine is alive, is fine. The
/// fibers of coroutines are boxed and are not moved by the machine.
pub struct Machine {
    /// Looked up at call time, so a patched function takes effect at its next call.
    pub(crate) fns: Vec<Slot>,
    pub(crate) globals: Vec<Value>,
    /// The state shared with the engine-neutral runtime code.
    pub(crate) core: Core,
    /// Global ids in initialisation order, and the initialiser body of every global by id.
    pub(crate) inits: Vec<GlobalId>,
    pub(crate) global_inits: Vec<Slot>,
    pub(crate) script_from: usize,
    pub(crate) script: Option<Slot>,
    /// By global id, the name of each script value whose line has not run yet.
    pub(crate) uninit: Vec<Option<Rc<str>>>,
    /// This machine is a thread task's isolate (`Machine::load_worker`): the entries of `uninit`
    /// are the module values of the main thread, not script values whose line has not run.
    pub(crate) worker: bool,
    pub(crate) global_names: Vec<Rc<str>>,
    pub(crate) released: bool,
    /// A feature the VM lacks stopped a run: no program code (a `Drop` hook) runs after it.
    pub(crate) refused: bool,
    pub(crate) env: CompileEnv,
    pub(crate) max_depth: usize,
    /// Closure built-ins never run in a native loop (tests that compare the two paths).
    pub(crate) leaf_off: bool,
    /// How many closure built-in calls ran in a native loop.
    pub(crate) leaf_runs: u64,
    pub(crate) native_stack_budget: usize,
    /// The host functions and class hooks the runtime installed.
    pub(crate) host_impls: HostImpls,
    /// The native stacks that a coroutine's host functions run on (vm-plan §3.2).
    pub(crate) host_stacks: HostStacks,
    /// The layouts of the C structs of the program and the libraries it uses.
    pub(crate) cffi: CffiState,
    /// Counts the loads and patches of the program: an inline cache entry made under another
    /// epoch is stale (its chunk may be one a running frame kept across a patch).
    pub(crate) epoch: u32,
    /// Compile everything when a program is installed (see `set_compile_all`).
    pub(crate) compile_all: bool,
    /// The fibers of the coroutines that have not ended (vm-plan §2.8).
    pub(crate) fibers: FiberSlab,
    /// A coroutine that ended with a refusal during the resumption in progress: what resumed it
    /// raises the refusal on its own fiber (`settle_refusal`), so a refusal stops the program where
    /// it is met. The id of the coroutine and its failure. A copy rides in `hook_panics` too, so a
    /// resumption that no resumer settles (the death of a `CoGroup`) raises it where drop hooks fold.
    pub(crate) pending_refusal: Option<(u64, PanicInfo)>,
    /// Where the native stack stood when the host (or the event loop) last called in with no
    /// coroutine running: a resumed coroutine measures its native recursion from it.
    pub(crate) stack_root: usize,
    /// Text the string instructions build in before they make the string: kept between
    /// instructions so its capacity is reused.
    pub(crate) scratch: String,
    /// Tests only (`Machine::set_safepoint_probe`): told of each safepoint that runs.
    pub(crate) safepoint_probe: Option<crate::safepoint::Probe>,
    /// Tests only (`Machine::set_safepoint_failure`): makes a consumer fail.
    pub(crate) safepoint_failure: Option<crate::safepoint::Failure>,
    /// The sampling profiler while one runs (`Machine::start_profile`).
    pub(crate) profiler: Option<Box<crate::profile::Profiler>>,
    /// The reference-count counters while a run counts (`Machine::start_rc_profile`).
    pub(crate) rc: Option<Box<crate::rc::RcCounter>>,
    /// The debugger's state while debug mode is on (`Machine::set_debug`).
    pub(crate) debug: Option<Box<crate::debug::DebugState>>,
    /// Tests only: runs at every `print`, so a test can call back into Colibri from inside the
    /// dispatch loop (a native re-entry, like a user `Show` or a `Drop` hook would).
    #[cfg(test)]
    pub(crate) print_hook: Option<PrintHook>,
}

/// The default for [`Machine::set_native_stack_budget`].
pub const DEFAULT_NATIVE_STACK_BUDGET: usize = 1 << 20;

impl Machine {
    pub fn new(out: Box<dyn Write>, max_call_depth: usize) -> Machine {
        Machine {
            fns: Vec::new(),
            globals: Vec::new(),
            core: Core::new(out),
            inits: Vec::new(),
            global_inits: Vec::new(),
            script_from: 0,
            script: None,
            uninit: Vec::new(),
            worker: false,
            global_names: Vec::new(),
            released: false,
            refused: false,
            env: CompileEnv::default(),
            max_depth: max_call_depth,
            leaf_off: false,
            leaf_runs: 0,
            native_stack_budget: DEFAULT_NATIVE_STACK_BUDGET,
            host_impls: HostImpls::default(),
            host_stacks: HostStacks::default(),
            cffi: CffiState::default(),
            epoch: 0,
            compile_all: false,
            fibers: FiberSlab::default(),
            pending_refusal: None,
            stack_root: 0,
            safepoint_probe: None,
            safepoint_failure: None,
            profiler: None,
            rc: None,
            debug: None,
            scratch: String::new(),
            #[cfg(test)]
            print_hook: None,
        }
    }

    /// Sets the size of the native stacks that host functions called from coroutines run on
    /// (default 1 MiB). Stacks pooled at another size are dropped at the next call.
    pub fn set_host_stack_size(&mut self, bytes: usize) {
        self.host_stacks.set_size(bytes);
    }

    /// How many host stacks were mapped from the system, as opposed to taken from the pool (for
    /// tests of the pool).
    #[doc(hidden)]
    pub fn host_stacks_allocated(&self) -> usize {
        self.host_stacks.allocated()
    }

    /// How many finished host stacks wait in the pool (for tests of the pool).
    #[doc(hidden)]
    pub fn host_stacks_pooled(&self) -> usize {
        self.host_stacks.pooled()
    }

    /// How many bytes of native stack nested calls from Rust into Colibri may use.
    pub fn set_native_stack_budget(&mut self, bytes: usize) {
        self.native_stack_budget = bytes;
    }

    /// Whether installing a program (load, patch, session) compiles every function, global
    /// initialiser and the script at once, and panics on one that does not compile. Chunks
    /// are compiled lazily otherwise, so a function that cannot be compiled (an internal error:
    /// the checker accepted it) shows only in a run that calls it. For test suites.
    pub fn set_compile_all(&mut self, on: bool) {
        self.compile_all = on;
    }

    /// With `compile_all` on: compiles every body now.
    pub(crate) fn compile_everything(&self) {
        if !self.compile_all {
            return;
        }
        let bodies = self
            .fns
            .iter()
            .enumerate()
            .map(|(i, s)| (Some(FnId(i as u32)), s))
            .chain(self.global_inits.iter().map(|s| (None, s)))
            .chain(self.script.iter().map(|s| (None, s)));
        for (id, slot) in bodies {
            if let Err(why) = slot.chunk(id, &self.env) {
                panic!(
                    "internal error: the bytecode VM cannot compile `{}`: {why}",
                    slot.body.name
                );
            }
        }
    }

    /// Installs a checked program, replacing any earlier one and resetting module state (the
    /// old module-level values die first).
    pub fn load(&mut self, program: &Program) {
        self.load_as(program, false);
    }

    /// Installs a checked program as a thread task's isolate (DESIGN §4.2, §14.1): only the
    /// `const`s are module state here. Their initialisers are the only ones
    /// [`Machine::run_module_inits`] runs, and a read or write of any other module value panics
    /// because it belongs to the main thread. The script does not run.
    pub fn load_worker(&mut self, program: &Program) {
        self.load_as(program, true);
    }

    fn load_as(&mut self, program: &Program, worker: bool) {
        self.shutdown();
        self.epoch = self.epoch.wrapping_add(1);
        self.core.closure_maps.clear();
        self.core.load_host(program);
        self.load_host_impls(program);
        self.cffi.load(program);
        self.fns = program
            .fns
            .iter()
            .map(|f| Slot::new(Rc::clone(&f.body)))
            .collect();
        self.globals = vec![Value::Unit; program.globals.len()];
        self.core.types = Rc::from(program.types.clone());
        self.core.dispatch = Tables::new(program);
        self.inits = program.init_order.clone();
        self.global_inits = program
            .globals
            .iter()
            .map(|g| Slot::new(Rc::clone(&g.init)))
            .collect();
        self.script_from = program.script_from;
        self.released = false;
        self.global_names = program
            .globals
            .iter()
            .map(|g| Rc::from(g.name.as_str()))
            .collect();
        self.uninit = Vec::new();
        self.worker = worker;
        self.env = CompileEnv::for_program(program);
        self.env.count_rc = self.rc.is_some();
        self.env.debug = self.debug.as_ref().map(|d| Rc::clone(&d.shared));
        if worker {
            // Only the constants are initialised; every other value is checked on each use.
            self.inits = program
                .init_order
                .iter()
                .copied()
                .filter(|g| program.globals[g.0 as usize].constant)
                .collect();
            self.script_from = self.inits.len();
            self.uninit = vec![None; program.globals.len()];
            for (i, g) in program.globals.iter().enumerate() {
                if !g.constant {
                    self.uninit[i] = Some(Rc::from(g.name.as_str()));
                    self.env.checked_globals[i] = true;
                }
            }
        } else if program.script_from < program.init_order.len() {
            self.uninit = vec![None; program.globals.len()];
            for g in &program.init_order[program.script_from..] {
                let i = g.0 as usize;
                self.uninit[i] = Some(Rc::from(program.globals[i].name.as_str()));
            }
        }
        self.script = Some(Slot::new(Rc::clone(&program.script)));
        self.compile_everything();
    }

    /// The module-level value `g` (for hosts and tests).
    pub fn global(&self, g: GlobalId) -> Option<&Value> {
        self.globals.get(g.0 as usize)
    }

    /// The chunk of function `f`, compiled on first use.
    pub(crate) fn chunk_of(&self, f: FnId) -> Option<Result<Rc<Chunk>, String>> {
        let slot = self.fns.get(f.0 as usize)?;
        Some(slot.chunk(Some(f), &self.env))
    }
}
