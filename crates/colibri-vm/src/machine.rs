//! The machine: loaded code, module-level values and the state shared with core.

use std::cell::OnceCell;
use std::io::Write;
use std::rc::Rc;

use colibri_check::hir::{Body, FnId, GlobalId, Program};
use colibri_core::Core;
use colibri_core::dispatch::Tables;
use colibri_core::value::Value;

use crate::bytecode::Chunk;
use crate::compile::{CompileEnv, compile_body};

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
        let _ = self.chunk.set(Rc::clone(&chunk));
        Ok(chunk)
    }
}

/// Test-only callback run at every `print`.
#[cfg(test)]
pub(crate) type PrintHook = Rc<dyn Fn(&mut Machine, &mut crate::fiber::VmFiber)>;

/// The bytecode virtual machine: the engine behind `colibri_core::engine::Engine`.
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
    pub(crate) global_names: Vec<Rc<str>>,
    pub(crate) released: bool,
    pub(crate) env: CompileEnv,
    pub(crate) max_depth: usize,
    pub(crate) native_stack_budget: usize,
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
            global_names: Vec::new(),
            released: false,
            env: CompileEnv::default(),
            max_depth: max_call_depth,
            native_stack_budget: DEFAULT_NATIVE_STACK_BUDGET,
            #[cfg(test)]
            print_hook: None,
        }
    }

    /// How many bytes of native stack nested calls from Rust into Colibri may use.
    pub fn set_native_stack_budget(&mut self, bytes: usize) {
        self.native_stack_budget = bytes;
    }

    /// Installs a checked program, replacing any earlier one and resetting module state (the
    /// old module-level values die first).
    pub fn load(&mut self, program: &Program) {
        self.shutdown();
        self.core.closure_maps.clear();
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
        self.env = CompileEnv {
            checked_globals: vec![false; program.globals.len()],
        };
        if program.script_from < program.init_order.len() {
            self.uninit = vec![None; program.globals.len()];
            for g in &program.init_order[program.script_from..] {
                let i = g.0 as usize;
                self.uninit[i] = Some(Rc::from(program.globals[i].name.as_str()));
                self.env.checked_globals[i] = true;
            }
        }
        self.script = Some(Slot::new(Rc::clone(&program.script)));
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
