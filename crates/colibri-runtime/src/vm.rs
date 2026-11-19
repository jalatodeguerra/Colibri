//! The virtual machine a host embeds.

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use colibri_check::HostEnv;
use colibri_check::hir::{HostFnId, Program};
use colibri_check::types::{RESULT_TYPE, Ty, VARIANT_ERR};
use colibri_interp::{
    DEFAULT_NATIVE_STACK_BUDGET, Fiber, HostFault, HostImpl, Interp, Value as InterpValue,
};
use colibri_syntax::{Diagnostic, SourceMap, parse_file};

use crate::cap::{Cap, CapSet};
use crate::convert::{Args, FromColibri, Value};
use crate::ctx::{Caller, Ctx, panic_error};
use crate::error::{Error, Report};
use crate::func::Fn;
use crate::host::{HostFn, Registration, payload_message};
use crate::iface::{self, HostFnEntry, Rendered};
use crate::types::{TypeDesc, TypeRegistry};

/// The default limit on nested Colibri calls (DESIGN §10.4 leaves the number open).
pub const DEFAULT_MAX_CALL_DEPTH: usize = 10_000;

static NEXT_VM_ID: AtomicU64 = AtomicU64::new(0);

/// Names a host function cannot take: they are built into the language.
const RESERVED_NAMES: &[&str] = &["print", "panic", "some", "ok", "err"];

/// Configures a [`Vm`].
pub struct VmBuilder {
    stdout: Option<Box<dyn Write>>,
    max_call_depth: usize,
    native_stack_budget: usize,
    trace_drops: bool,
    caps: CapSet,
    instruction_budget: Option<u64>,
}

impl VmBuilder {
    /// Records the label of every class object as it is destroyed, for tests of drop order
    /// (see [`Vm::take_drop_trace`]). Off by default.
    #[doc(hidden)]
    pub fn trace_drops(mut self, on: bool) -> Self {
        self.trace_drops = on;
        self
    }

    /// Where `print` writes. The default is the process's standard output.
    pub fn stdout(mut self, writer: Box<dyn Write>) -> Self {
        self.stdout = Some(writer);
        self
    }

    /// How many Colibri calls may be active at once before a "stack overflow" panic. Each
    /// call also uses Rust stack (see [`VmBuilder::native_stack_budget`]): the `colibri` tool
    /// runs on a 256 MB stack for the default of 10 000.
    pub fn max_call_depth(mut self, depth: usize) -> Self {
        self.max_call_depth = depth.max(1);
        self
    }

    /// How many bytes of Rust stack the interpreter may use below the point where the host
    /// calls in, before a deep recursion panics with "stack overflow" instead of crashing the
    /// host. The default is 1 MiB, which fits a default test thread; on a thread with a
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

    pub fn build(self) -> Vm {
        let out = self.stdout.unwrap_or_else(|| Box::new(std::io::stdout()));
        let mut interp = Interp::new(out, self.max_call_depth);
        interp.set_native_stack_budget(self.native_stack_budget);
        interp.set_trace_drops(self.trace_drops);
        interp.set_instruction_budget(self.instruction_budget);
        let id = NEXT_VM_ID.fetch_add(1, Ordering::Relaxed);
        interp.set_vm_id(id);
        Vm {
            sources: Rc::new(SourceMap::new()),
            interp,
            fiber: Fiber::new(),
            program: None,
            warnings: Vec::new(),
            id,
            epoch: 0,
            native_stack_budget: self.native_stack_budget,
            caps: self.caps,
            registry: TypeRegistry::new(),
            host_fns: Vec::new(),
        }
    }
}

/// A Colibri virtual machine: one loaded program, its module-level state and an output writer.
pub struct Vm {
    sources: Rc<SourceMap>,
    interp: Interp,
    fiber: Fiber,
    program: Option<Rc<Program>>,
    warnings: Vec<Diagnostic>,
    /// Distinguishes this `Vm` from every other, so a handle cannot be used on the wrong one.
    id: u64,
    /// Bumped whenever the loaded program changes, which invalidates function handles.
    epoch: u32,
    native_stack_budget: usize,
    caps: CapSet,
    /// The structs and enums the host's functions mention.
    registry: TypeRegistry,
    /// The registered host functions, in the order of their `HostFnId`s.
    host_fns: Vec<HostFnEntry>,
}

impl Default for Vm {
    fn default() -> Self {
        Vm::new()
    }
}

impl Caller for Vm {
    fn ctx(&mut self) -> Ctx<'_> {
        Ctx::new(&mut self.interp, &mut self.fiber)
    }
}

impl Vm {
    pub fn builder() -> VmBuilder {
        VmBuilder {
            stdout: None,
            max_call_depth: DEFAULT_MAX_CALL_DEPTH,
            native_stack_budget: DEFAULT_NATIVE_STACK_BUDGET,
            trace_drops: false,
            caps: CapSet::none(),
            instruction_budget: None,
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
        self.register_inner(name, None, f)
    }

    /// Like [`Vm::register_fn`], with real parameter names, so a script may pass arguments by
    /// name (`spawn_enemy(x: 1.0, y: 2.0)`). One name per Rust parameter (not counting a leading
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
        self.register_inner(name, Some(names), f)
    }

    fn register_inner<Marker, F: HostFn<Marker>>(
        &mut self,
        name: &str,
        names: Option<Vec<String>>,
        f: F,
    ) -> Result<Registration<'_>, Error> {
        let fail = |message: String| Error::Registration {
            name: name.to_string(),
            message,
        };
        if self.program.is_some() {
            return Err(Error::AlreadyLoaded);
        }
        if RESERVED_NAMES.contains(&name) {
            return Err(fail("this name is built into the language".to_string()));
        }
        if self.host_fns.iter().any(|e| e.name == name) {
            return Err(fail(
                "a host function with this name is registered".to_string(),
            ));
        }
        // Describe into a copy, so a failed registration leaves nothing behind.
        let mut registry = self.registry.clone();
        let sig = F::sig(&mut registry);
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
            name: name.to_string(),
            sig,
            raw: f.into_raw(),
            requires: None,
            positional: names.is_none(),
            names,
        });
        let before = std::mem::replace(&mut self.registry, registry);
        // Whatever the names or types, what is printed must read back.
        if let Err(message) = HostEnv::from_text(&self.rendered().text) {
            self.host_fns.pop();
            self.registry = before;
            return Err(fail(message));
        }
        let index = self.host_fns.len() - 1;
        Ok(Registration { vm: self, index })
    }

    /// Describes a struct, enum or host class to the interface without a function that
    /// mentions it, so scripts can use it as a type. A [`crate::HostType`] brings its methods
    /// along; one that a registered signature mentions is registered automatically.
    pub fn register_type<T: crate::IntoColibri>(&mut self) -> Result<(), Error> {
        if self.program.is_some() {
            return Err(Error::AlreadyLoaded);
        }
        let mut registry = self.registry.clone();
        let _ = T::ty(&mut registry);
        if let Some(conflict) = registry.conflicts.first() {
            return Err(Error::Registration {
                name: std::any::type_name::<T>().to_string(),
                message: conflict.clone(),
            });
        }
        let before = std::mem::replace(&mut self.registry, registry);
        // Whatever the names, what is printed must read back.
        if let Err(message) = HostEnv::from_text(&self.rendered().text) {
            self.registry = before;
            return Err(Error::Registration {
                name: std::any::type_name::<T>().to_string(),
                message,
            });
        }
        Ok(())
    }

    pub(crate) fn interp(&self) -> &Interp {
        &self.interp
    }

    pub(crate) fn interp_mut(&mut self) -> &mut Interp {
        &mut self.interp
    }

    /// The raw function that implements the host function or method `name` (`World.step`),
    /// and the capability it needs.
    fn find_raw(&self, name: &str) -> Option<(crate::host::RawHostFn, Option<Cap>)> {
        if let Some(entry) = self.host_fns.iter().find(|e| e.name == name) {
            return Some((entry.raw.clone(), entry.requires));
        }
        let (class, method) = name.split_once('.')?;
        let class = self.registry.classes.iter().find(|c| c.name == class)?;
        let method = class.methods.iter().find(|m| m.name == method)?;
        Some((method.raw.clone(), None))
    }

    pub(crate) fn set_requires(&mut self, index: usize, cap: Cap) {
        if let Some(entry) = self.host_fns.get_mut(index) {
            entry.requires = Some(cap);
        }
    }

    /// Stores host state of type `T`, which host functions reach through `cx.state_mut::<T>()`
    /// and the host between calls through [`Vm::state_mut`]. Returns the state it replaces.
    pub fn insert_state<T: 'static>(&mut self, value: T) -> Option<T> {
        self.interp.insert_state(value)
    }

    /// The host state of type `T`.
    pub fn state_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.interp.state_mut::<T>()
    }

    /// Takes host state of type `T` out of the `Vm`.
    pub fn remove_state<T: 'static>(&mut self) -> Option<T> {
        self.interp.remove_state::<T>()
    }

    // ---- interface files ----

    fn rendered(&self) -> Rendered {
        iface::render(&self.registry, &self.host_fns)
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
        let path = path.as_ref();
        let found = std::fs::read_to_string(path).map_err(|e| Error::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        let rendered = self.rendered();
        if found == rendered.text {
            return Ok(());
        }
        Err(Error::InterfaceMismatch {
            path: path.display().to_string(),
            expected: rendered.hash,
            found: iface::declared_hash(&found),
        })
    }

    /// The interface as the checker reads it, with the host's grants.
    fn host_env(&self) -> Result<HostEnv, Error> {
        let mut env = HostEnv::from_text(&self.rendered().text).map_err(Error::Internal)?;
        for cap in self.caps.iter() {
            env.grant(cap.name());
        }
        Ok(env)
    }

    /// Installs the host functions the program uses. One whose capability was not granted
    /// stays a trap: the checker already refused every call of it.
    fn install_host_fns(&mut self, program: &Program) {
        for (i, def) in program.host_fns.iter().enumerate() {
            let Some((raw, requires)) = self.find_raw(&def.name) else {
                continue;
            };
            if requires.is_some_and(|cap| !self.caps.contains(cap)) {
                continue;
            }
            let imp = trampoline(&def.name, raw);
            self.interp.set_host_fn(HostFnId(i as u32), imp);
        }
    }

    // ---- budget ----

    /// Sets the instruction budget: each outermost entry from the host (`call`, `load`,
    /// `run_main`) may take this many steps; `None` is unlimited. Calls a host function makes
    /// back into Colibri share the entry's budget. When it runs out, evaluation unwinds like a
    /// panic (so `defer` statements and `Drop` hooks run, for a grace of 10 000 more steps) and
    /// the call returns [`Error::BudgetExhausted`]; the `Vm` stays usable.
    pub fn set_instruction_budget(&mut self, steps: Option<u64>) {
        self.interp.set_instruction_budget(steps);
    }

    /// The steps the latest entry from the host used (including the grace after running out).
    pub fn steps_used(&self) -> u64 {
        self.interp.steps_used()
    }

    // ---- loading and running ----

    /// Reads, checks and runs a script file: module-level values are initialised, then its
    /// top-level statements run. `fn main` is not called; use [`Vm::run_main`].
    pub fn load_file(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        self.load_source(&path.display().to_string(), &text)
    }

    /// Like [`Vm::load_file`] for source text; `name` is what diagnostics call the file.
    ///
    /// On a compile error nothing is loaded. If the top-level code panics, the program is
    /// discarded (its module state is half-built) and the `Vm` can load another.
    pub fn load_source(&mut self, name: &str, source: &str) -> Result<(), Error> {
        if self.program.is_some() {
            return Err(Error::AlreadyLoaded);
        }
        let env = self.host_env()?;
        let mut sources = SourceMap::new();
        let file = sources.add_file(name, source);
        let (ast, mut diagnostics) = parse_file(file, source);
        let mut program = None;
        // A tree with syntax errors would only produce follow-on errors.
        if !diagnostics.iter().any(Diagnostic::is_error) {
            let checked = colibri_check::check_with_host(&ast, &env, self.native_stack_budget);
            diagnostics = checked.diagnostics;
            program = checked.program;
        }
        let Some(program) = program else {
            return Err(Error::Compile(Report {
                sources,
                diagnostics,
            }));
        };
        self.sources = Rc::new(sources);
        self.warnings = diagnostics;
        self.epoch += 1;
        self.interp.load(&program);
        self.install_host_fns(&program);
        self.interp
            .set_owner((self.id, self.epoch), true, Rc::clone(&self.sources));
        let outcome = self.interp.run_module(&mut self.fiber);
        self.interp.flush();
        match outcome {
            Ok(()) => {
                self.program = Some(Rc::new(program));
                Ok(())
            }
            Err(info) => {
                let error = panic_error(&self.interp, *info);
                self.epoch += 1;
                self.interp.shutdown();
                self.interp
                    .set_owner((self.id, self.epoch), false, Rc::clone(&self.sources));
                Err(error)
            }
        }
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
        let result = self.interp.call(&mut self.fiber, main, Vec::new());
        self.interp.flush();
        let value = result.map_err(|info| panic_error(&self.interp, *info))?;
        if returns_result
            && let InterpValue::Enum(e) = &value
            && e.variant == VARIANT_ERR
            && let Some(payload) = e.payload.first()
        {
            let message = self.interp.error_report(&mut self.fiber, payload);
            self.interp.release_in_entry(value);
            return Err(Error::MainReturnedErr { message });
        }
        self.interp.release_in_entry(value);
        Ok(())
    }

    /// A handle to the function `name`, if its Colibri signature matches `A -> R`.
    pub fn get_fn<A: Args, R: FromColibri>(&self, name: &str) -> Result<Fn<A, R>, Error> {
        let program = self.program.as_ref().ok_or(Error::NotLoaded)?;
        let id = program.fn_by_name(name).ok_or_else(|| Error::FnNotFound {
            name: name.to_string(),
        })?;
        let sig = &program.fns[id.0 as usize].sig;
        let mut registry = TypeRegistry::new();
        let params: Vec<Option<TypeDesc>> = sig
            .params
            .iter()
            .map(|p| TypeDesc::of(p, program))
            .collect();
        let wanted: Vec<TypeDesc> = A::tys(&mut registry);
        let ret = TypeDesc::of(&sig.ret, program);
        let want_ret = R::ty(&mut registry);
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
        self.interp.shutdown();
        self.interp.flush();
        if self.program.take().is_some() {
            self.epoch += 1;
        }
        self.interp
            .set_owner((self.id, self.epoch), false, Rc::clone(&self.sources));
    }

    /// The labels of the class objects destroyed so far, in order, when the `Vm` was built
    /// with [`VmBuilder::trace_drops`]; the trace is emptied. Test support.
    #[doc(hidden)]
    pub fn take_drop_trace(&mut self) -> Vec<String> {
        self.interp.take_trace()
    }
}

/// Wraps a host function for the interpreter: arguments are handed over as a slice, panics
/// become Colibri panics, and the references the interpreter passed are released afterwards.
fn trampoline(name: &str, raw: crate::host::RawHostFn) -> HostImpl {
    let name = name.to_string();
    Rc::new(
        move |interp: &mut Interp, fiber: &mut Fiber, args: Vec<InterpValue>| {
            let args: Vec<Value> = args.into_iter().map(Value).collect();
            let mut cx = Ctx::new(interp, fiber);
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
                }),
                Err(payload) => Err(HostFault::Panic(format!(
                    "host function `{name}` panicked: {}",
                    payload_message(payload.as_ref())
                ))),
            }
        },
    )
}
