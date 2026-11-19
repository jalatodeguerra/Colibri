//! The virtual machine a host embeds.

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use colibri_check::HostEnv;
use colibri_check::hir::{HostFnId, Program};
use colibri_check::host::{DEFAULT_MODULE, float_literal, is_module_name, is_std_module_name};
use colibri_check::package::{DiskFs, Entry, MemoryFs, SourceFs};
use colibri_check::types::{RESULT_TYPE, Ty, VARIANT_ERR};
use colibri_interp::{
    DEFAULT_NATIVE_STACK_BUDGET, Fiber, HostFault, HostImpl, Interp, Value as InterpValue,
};
use colibri_syntax::{Diagnostic, SourceMap};

use crate::cap::{Cap, CapSet};
use crate::convert::{Args, FromColibri, Value};
use crate::ctx::{Caller, Ctx, panic_error};
use crate::error::{Error, Report};
use crate::func::Fn;
use crate::host::{HostFn, Registration, payload_message};
use crate::iface::{self, ConstEntry, ConstValue, HostFnEntry, Rendered};
use crate::types::{TypeDesc, TypeRegistry};

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
const RESERVED_NAMES: &[&str] = &["print", "panic", "assert", "assert_eq", "some", "ok", "err"];

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
            host_consts: Vec::new(),
            modules: Vec::new(),
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
    /// The constants of named modules (`math.PI`).
    host_consts: Vec<ConstEntry>,
    /// The named host modules (`vm.module("physics")`) that have something registered, in the
    /// order they got it. The default `host` module always exists and is not listed.
    modules: Vec<String>,
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
        self.register_inner(DEFAULT_MODULE, name, None, f)
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
        HostModule {
            vm: self,
            name: name.to_string(),
            std: false,
        }
    }

    /// The module `name` of the standard library: what `import name` and `import std.name`
    /// mean when no file or dependency of that name exists (ARCHITECTURE §13). It is a host
    /// module like any other whose interface name is `std.name`; this is how the `colibri-std`
    /// crate registers its modules, and it is the only way to get one: [`Vm::module`] refuses
    /// the reserved name `std`.
    pub fn std_module(&mut self, name: &str) -> HostModule<'_> {
        HostModule {
            vm: self,
            name: format!("{}{name}", colibri_check::host::STD_PREFIX),
            std: true,
        }
    }

    /// The names of the host modules that have an interface: `host` first, then the named
    /// ones in the order they were first registered into.
    pub fn module_names(&self) -> Vec<String> {
        let mut names = vec![DEFAULT_MODULE.to_string()];
        names.extend(self.modules.iter().cloned());
        names
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
        if self.program.is_some() {
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
        });
        let before = std::mem::replace(&mut self.registry, registry);
        let added = self.note_module(module);
        // Whatever the names or types, what is printed must read back.
        if let Err(message) = self.validate_interfaces() {
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
        let type_name = std::any::type_name::<T>();
        let fail = |message: String| Error::Registration {
            name: type_name.to_string(),
            message,
        };
        if self.program.is_some() {
            return Err(Error::AlreadyLoaded);
        }
        Self::check_module_name(module).map_err(fail)?;
        let mut registry = self.registry.clone();
        registry.current_module = (module != DEFAULT_MODULE).then(|| module.to_string());
        let _ = T::ty(&mut registry);
        registry.current_module = None;
        if let Some(conflict) = registry.conflicts.first() {
            return Err(fail(conflict.clone()));
        }
        let before = std::mem::replace(&mut self.registry, registry);
        let added = self.note_module(module);
        // Whatever the names, what is printed must read back.
        if let Err(message) = self.validate_interfaces() {
            self.registry = before;
            self.forget_module(module, added);
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

    /// Prints every module's interface and reads it back, and checks that no module mentions
    /// the types of another named module (its interface text could not express that).
    fn validate_interfaces(&self) -> Result<(), String> {
        for module in self.module_names() {
            HostEnv::from_text(&self.rendered_module(&module).text)?;
        }
        let reg = &self.registry;
        let check = |module: &str, sig: &crate::types::SigDesc, what: &str| -> Result<(), String> {
            let mut names = Vec::new();
            for p in &sig.params {
                p.mentions(&mut names);
            }
            sig.ret.mentions(&mut names);
            for n in names {
                let owner = reg.module_of(n);
                if owner != module && owner != DEFAULT_MODULE {
                    return Err(format!(
                        "{what} is in the module `{module}` but mentions the type `{n}` of the module `{owner}`; a module can only mention its own types and those of the default `host` module"
                    ));
                }
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
                let module = reg.decl_modules[i].as_deref().unwrap_or(DEFAULT_MODULE);
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

    pub(crate) fn interp(&self) -> &Interp {
        &self.interp
    }

    pub(crate) fn interp_mut(&mut self) -> &mut Interp {
        &mut self.interp
    }

    /// The raw function that implements the host function or method `name` (`World.step`),
    /// and the capability it needs.
    fn find_raw(&self, module: &str, name: &str) -> Option<(crate::host::RawHostFn, Option<Cap>)> {
        if let Some(entry) = self
            .host_fns
            .iter()
            .find(|e| e.module == module && e.name == name)
        {
            return Some((entry.raw.clone(), entry.requires));
        }
        let (class, method) = name.split_once('.')?;
        let class =
            self.registry.classes.iter().find(|c| {
                c.name == class && c.module.as_deref().unwrap_or(DEFAULT_MODULE) == module
            })?;
        let method = class.methods.iter().find(|m| m.name == method)?;
        Some((method.raw.clone(), None))
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
        if self.program.is_some() {
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
        if let Err(message) = self.validate_interfaces() {
            self.host_consts.pop();
            self.forget_module(module, added);
            return Err(fail(message));
        }
        Ok(())
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

    /// The interface as the checker reads it, with the host's grants.
    fn host_env(&self) -> Result<HostEnv, Error> {
        let mut env = HostEnv::new();
        for module in self.module_names() {
            let one =
                HostEnv::from_text(&self.rendered_module(&module).text).map_err(Error::Internal)?;
            env.merge(one);
        }
        for cap in self.caps.iter() {
            env.grant(cap.name());
        }
        Ok(env)
    }

    /// Installs the host functions the program uses. One whose capability was not granted
    /// stays a trap: the checker already refused every call of it.
    fn install_host_fns(&mut self, program: &Program) {
        for (i, def) in program.host_fns.iter().enumerate() {
            let Some((raw, requires)) = self.find_raw(&def.module, &def.name) else {
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
    ///
    /// The file's folder is its implicit package (DESIGN §13): `import helpers` finds
    /// `helpers.colibri` next to it, `import util.strings` finds `util/strings.colibri`, and
    /// every file reached that way is checked together with it. A `colibri.toml` in the folder
    /// supplies the dependencies `import` may name.
    pub fn load_file(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        self.load_entry(&DiskFs, Entry::File(path.as_ref()), LoadMode::Run)
    }

    /// Reads, checks and runs the package in folder `dir`: its `colibri.toml` names it and lists
    /// its dependencies, its `main.colibri` is the entry (module values are initialised in
    /// dependency order, then the top-level statements of `main.colibri` run). `fn main` is not
    /// called; use [`Vm::run_main`].
    pub fn load_package(&mut self, dir: impl AsRef<Path>) -> Result<(), Error> {
        self.load_entry(&DiskFs, Entry::Package(dir.as_ref()), LoadMode::Run)
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
        self.load_entry(&DiskFs, entry, LoadMode::Tests)
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
    /// so `[("app/main.colibri", ..), ("app/game/player.colibri", ..)]` lets `main.colibri`
    /// `import game.player`. Paths are what diagnostics and stack traces call the files. For
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
        self.load_entry(&fs, Entry::File(Path::new(entry)), LoadMode::Run)
    }

    fn load_entry(
        &mut self,
        fs: &dyn SourceFs,
        entry: Entry<'_>,
        mode: LoadMode,
    ) -> Result<(), Error> {
        if self.program.is_some() {
            return Err(Error::AlreadyLoaded);
        }
        let env = self.host_env()?;
        let mut sources = SourceMap::new();
        let options = colibri_check::package::LoadOptions {
            all_modules: mode == LoadMode::Tests,
        };
        let loaded = colibri_check::package::load_with(
            fs,
            &mut sources,
            entry,
            &env.named_modules(),
            options,
        )
        .map_err(|e| Error::Io {
            path: e.path,
            message: e.message,
        })?;
        let mut diagnostics = loaded.diagnostics.clone();
        let mut program = None;
        // A tree with syntax errors would only produce follow-on errors.
        if !loaded.has_errors() {
            let checked = colibri_check::check_modules(
                &loaded.inputs(),
                &loaded.packages,
                &env,
                self.native_stack_budget,
            );
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
        let outcome = match mode {
            LoadMode::Run => self.interp.run_module(&mut self.fiber),
            LoadMode::Tests => Ok(()),
        };
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

    // ---- tests ----

    /// The `test "name" { .. }` blocks of the loaded program, in source order (the entry
    /// module's first). They include the ones in dependencies (`TestInfo::root` is false for
    /// those). Empty before a program is loaded. The index in the list is the id
    /// [`Vm::run_test`] takes.
    pub fn tests(&self) -> Vec<TestInfo> {
        let Some(program) = &self.program else {
            return Vec::new();
        };
        program
            .tests
            .iter()
            .enumerate()
            .map(|(id, t)| {
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
            })
            .collect()
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
        self.interp.release_globals();
        let outcome = match self.interp.run_initialisers(&mut self.fiber) {
            Err(info) => Err(panic_error(&self.interp, *info)),
            Ok(()) => match self.interp.call(&mut self.fiber, def.f, Vec::new()) {
                Err(info) => Err(panic_error(&self.interp, *info)),
                Ok(value) => {
                    let failed = match &value {
                        InterpValue::Enum(e) if e.variant == VARIANT_ERR => e
                            .payload
                            .first()
                            .map(|p| self.interp.error_report(&mut self.fiber, p)),
                        _ => None,
                    };
                    self.interp.release_in_entry(value);
                    match failed {
                        Some(message) => Err(Error::TestReturnedErr { message }),
                        None => Ok(()),
                    }
                }
            },
        };
        self.interp.release_globals();
        self.interp.flush();
        outcome
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
                Err(payload) => Err(match payload.downcast::<crate::ctx::RaisedPanic>() {
                    Ok(raised) => HostFault::Message(raised.0),
                    Err(payload) => HostFault::Panic(format!(
                        "host function `{name}` panicked: {}",
                        payload_message(payload.as_ref())
                    )),
                }),
            }
        },
    )
}

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
