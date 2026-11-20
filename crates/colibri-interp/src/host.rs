//! What the embedding program attaches to the interpreter: host functions and the calls
//! between them and Colibri code (EMBEDDING §1, §6). The state that is not tied to this engine
//! (host types, host state, the root slab) is in [`colibri_core::host`].

use std::any::TypeId as AnyTypeId;
use std::collections::HashMap;
use std::rc::Rc;

use colibri_check::hir::{HostFnId, Program, TypeDef};
use colibri_check::types::{Ty, TypeId};
use colibri_syntax::{SourceMap, Span};

pub use colibri_core::host::{HostFault, RootDropQueue, RootHandle, map_value};

use crate::ctl::{Eval, PanicInfo};
use crate::eval::Interp;
use crate::fiber::{Fiber, Frame};
use crate::value::Value;
use colibri_core::prelude::*;

/// A host function as the interpreter calls it: the arguments are the caller's references,
/// which the function must release (or hand on).
pub type HostImpl = Rc<dyn Fn(&mut Interp, &mut Fiber, Vec<Value>) -> Result<Value, HostFault>>;

/// What a host class customises: how its objects are shown and compared (`print`, `"{x}"`,
/// `==`, `Map` and `Set` lookups). The implementations take the objects as ordinary
/// arguments (one for `show`, two for `eq`) and return a `str` or a `bool`.
#[derive(Clone, Default)]
pub struct HostClassHooks {
    pub show: Option<HostImpl>,
    pub eq: Option<HostImpl>,
    /// The hash of one object, as an `int`, consistent with `eq` (what `hash()` gives).
    pub hash: Option<HostImpl>,
}

/// The host functions and class hooks that were installed. They are closures over
/// [`Interp`] and [`Fiber`], so unlike the rest of what the host attached they stay with the
/// engine.
#[derive(Default)]
pub(crate) struct HostImpls {
    /// Indexed by [`HostFnId`]; `None` for a function the host did not install (a trap slot).
    pub fns: Vec<Option<HostImpl>>,
    /// The classes that customise `Show` or `==`, by type.
    pub class_hooks: HashMap<TypeId, HostClassHooks>,
}

impl Interp {
    /// Records the host functions of a freshly loaded program (their empty bodies).
    pub(crate) fn load_host(&mut self, program: &Program) {
        self.core.load_host(program);
        self.host_impls.fns = vec![None; program.host_fns.len()];
        self.host_impls.class_hooks.clear();
    }

    /// Installs the implementation of host function `id`.
    pub fn set_host_fn(&mut self, id: HostFnId, f: HostImpl) {
        if let Some(slot) = self.host_impls.fns.get_mut(id.0 as usize) {
            *slot = Some(f);
        }
    }

    /// Installs how the objects of the host class `ty` are shown and compared.
    pub fn set_host_class_hooks(&mut self, ty: TypeId, hooks: HostClassHooks) {
        self.core.set_host_hook_set(
            ty,
            colibri_core::host::HookSet {
                show: hooks.show.is_some(),
                eq: hooks.eq.is_some(),
                hash: hooks.hash.is_some(),
            },
        );
        self.host_impls.class_hooks.insert(ty, hooks);
    }

    // ---- the host side, which lives in `Core` ----

    /// The type id of a struct, enum or class declared by the default `host` module of the
    /// host interface. Another module's types are found with [`Interp::host_type_in`].
    pub fn host_type(&self, name: &str) -> Option<TypeId> {
        self.core.host_type(name)
    }

    /// The type id of a struct, enum or class that the host module `module` declares (`host`
    /// for the default module).
    pub fn host_type_in(&self, module: &str, name: &str) -> Option<TypeId> {
        self.core.host_type_in(module, name)
    }

    /// Records the types a module's bare names mean that another module declares (see
    /// [`Interp::host_type_here`]).
    pub fn set_host_type_aliases(&mut self, aliases: HashMap<(String, String), TypeId>) {
        self.core.set_host_type_aliases(aliases);
    }

    /// The type a host function of `module` means by the bare `name`, or the reason there is
    /// none (no such type, or several modules declare one).
    pub fn host_type_here(&self, module: Option<&str>, name: &str) -> Result<TypeId, String> {
        self.core.host_type_here(module, name)
    }

    /// Records which Rust type implements which host class of the loaded program.
    pub fn set_host_classes(&mut self, classes: HashMap<AnyTypeId, TypeId>) {
        self.core.set_host_classes(classes);
    }

    /// The type id of the host class that the Rust type `any` implements.
    pub fn host_class(&self, any: AnyTypeId) -> Option<TypeId> {
        self.core.host_class(any)
    }

    /// The type id and the index of the variant `variant` of the enum `enum_name` that the host
    /// module `module` declared as its own, or the reason there is none.
    pub fn module_enum_variant(
        &self,
        module: &str,
        enum_name: &str,
        variant: &str,
    ) -> Result<(TypeId, u32), String> {
        self.core.module_enum_variant(module, enum_name, variant)
    }

    /// The name of a type of the loaded program.
    pub fn type_name(&self, id: TypeId) -> Option<&str> {
        self.core.type_name(id)
    }

    /// Says which program of which `Vm` this is, whether it is loaded, and what shows the
    /// locations of its panics.
    pub fn set_owner(&mut self, owner: (u64, u32), loaded: bool, sources: Rc<SourceMap>) {
        self.core.set_owner(owner, loaded, sources);
    }

    pub fn is_loaded(&self) -> bool {
        self.core.is_loaded()
    }

    /// Tells the interpreter which `Vm` it belongs to (for handles).
    pub fn set_vm_id(&mut self, id: u64) {
        self.core.set_vm_id(id);
    }

    pub fn vm_id(&self) -> u64 {
        self.core.vm_id()
    }

    /// Where dropped callbacks leave the roots they held.
    pub fn root_drop_queue(&self) -> RootDropQueue {
        self.core.root_drop_queue()
    }

    pub fn owner(&self) -> (u64, u32) {
        self.core.owner()
    }

    pub fn sources(&self) -> &Rc<SourceMap> {
        self.core.sources()
    }

    /// The type arguments written (or inferred) at the host function call that is running, in
    /// the order of the function's type parameters. Empty outside a host function.
    pub fn host_type_args(&self) -> &[Ty] {
        self.core.host_type_args()
    }

    /// Every struct, enum and class of the loaded program, indexed by type id.
    pub fn type_defs(&self) -> &[TypeDef] {
        self.core.type_defs()
    }

    /// The same descriptors, shared: a host function can keep them while it calls back into
    /// the program.
    pub fn type_defs_shared(&self) -> Rc<[TypeDef]> {
        self.core.type_defs_shared()
    }

    /// How many parameters function `f` takes; `None` if there is no such function in the
    /// loaded program.
    pub fn fn_param_count(&self, f: colibri_check::hir::FnId) -> Option<u32> {
        self.fns.get(f.0 as usize).map(|body| body.param_count)
    }

    /// Stores a value of type `T` for the host functions (replacing any earlier one).
    pub fn insert_state<T: 'static>(&mut self, value: T) -> Option<T> {
        self.core.insert_state(value)
    }

    pub fn state_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.core.state_mut::<T>()
    }

    pub fn remove_state<T: 'static>(&mut self) -> Option<T> {
        self.core.remove_state::<T>()
    }

    // ---- roots ----

    /// Keeps `value` alive; the handle is `(index, generation, epoch)`.
    pub fn root(&mut self, value: Value) -> RootHandle {
        self.core.root(value)
    }

    /// The rooted value, if the handle is still current.
    pub fn rooted(&self, handle: RootHandle) -> Option<&Value> {
        self.core.rooted(handle)
    }

    /// The current root epoch (for the handles of a program not yet running).
    pub fn root_epoch(&self) -> u32 {
        self.core.root_epoch()
    }

    // ---- values ----

    /// An `Error` made from a message, as `ok`/`err` payloads need it.
    pub fn error_value(message: &str) -> Value {
        colibri_core::host::error_value(message)
    }

    /// An `Error` that keeps the typed `value` (an enum of a host module, say) and shows as
    /// `message`: `e.is[T]()` and `e.as[T]()` find the value, `{e}` prints the message.
    pub fn typed_error_value(value: Value, message: &str) -> Value {
        colibri_core::host::typed_error_value(value, message)
    }

    /// `err(e)` of the prelude `Result`.
    pub fn err_variant(error: Value) -> Value {
        colibri_core::host::err_variant(error)
    }

    /// `ok(v)` of the prelude `Result`.
    pub fn ok_variant(v: Value) -> Value {
        colibri_core::host::ok_variant(v)
    }

    // ---- calls ----

    /// Calls a function or closure value from the host.
    pub fn call_function(
        &mut self,
        fiber: &mut Fiber,
        f: &Value,
        args: Vec<Value>,
    ) -> Result<Value, Box<PanicInfo>> {
        let f = f.clone();
        self.enter_from_host(fiber, true, move |this, fiber| {
            let span = fiber
                .frames
                .last()
                .map_or(Span::new(colibri_syntax::FileId(0), 0, 0), |fr| fr.span);
            let result = this.call_function_value(fiber, &f, args, span);
            this.release(f);
            result
        })
    }

    /// A call of the host function `id` from Colibri code.
    pub(crate) fn call_host(
        &mut self,
        fiber: &mut Fiber,
        id: HostFnId,
        type_args: &[Ty],
        args: Vec<Value>,
        span: Span,
    ) -> Eval {
        let installed = self
            .host_impls
            .fns
            .get(id.0 as usize)
            .cloned()
            .flatten()
            .zip(self.core.host.bodies.get(id.0 as usize).cloned());
        let Some((imp, body)) = installed else {
            self.release_all(args);
            return Err(self.internal(fiber, span, "a host function is not installed"));
        };
        if fiber.frames.len() >= self.max_depth() {
            self.release_all(args);
            let message = format!(
                "stack overflow: more than {} nested calls",
                self.max_depth()
            );
            return Err(self.panic(fiber, span, message));
        }
        if let Err(ctl) = self.step(fiber, span) {
            self.release_all(args);
            return Err(ctl);
        }
        if let Some(frame) = fiber.frames.last_mut() {
            frame.span = span;
        }
        // The host function gets a frame of its own, so a trace names it and the call depth
        // counts it; it has no slots and nothing to clean up.
        fiber.frames.push(Frame {
            body,
            slots: Vec::new(),
            span,
            cleanups: Vec::new(),
        });
        fiber.host_depth += 1;
        // The type arguments of the call, for a function that reads them (`json.decode[T]`).
        let outer_type_args = std::mem::replace(&mut self.core.host.type_args, type_args.to_vec());
        let result = imp(self, fiber, args);
        self.core.host.type_args = outer_type_args;
        fiber.host_depth -= 1;
        let outcome = match result {
            Ok(v) => Ok(v),
            Err(HostFault::Panic(message) | HostFault::Message(message)) => {
                Err(self.panic(fiber, span, message))
            }
            Err(HostFault::Cancel) => {
                fiber.cancelling = true;
                Err(crate::ctl::Ctl::Cancel)
            }
        };
        fiber.frames.pop();
        outcome
    }
}
