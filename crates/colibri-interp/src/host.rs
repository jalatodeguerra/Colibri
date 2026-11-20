//! What the embedding program attaches to the interpreter: host functions and the calls
//! between them and Colibri code (EMBEDDING §1, §6). The state that is not tied to this engine
//! (host types, host state, the root slab) is in [`colibri_core::host`].

use std::collections::HashMap;
use std::rc::Rc;

use colibri_check::hir::{HostFnId, Program};
use colibri_check::types::{Ty, TypeId};
use colibri_syntax::Span;

pub use colibri_core::host::HostFault;

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

    /// How many parameters function `f` takes; `None` if there is no such function in the
    /// loaded program.
    pub fn fn_param_count(&self, f: colibri_check::hir::FnId) -> Option<u32> {
        self.fns.get(f.0 as usize).map(|body| body.param_count)
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
        fiber.state.host_depth += 1;
        // The type arguments of the call, for a function that reads them (`json.decode[T]`).
        let outer_type_args = std::mem::replace(&mut self.core.host.type_args, type_args.to_vec());
        let result = imp(self, fiber, args);
        self.core.host.type_args = outer_type_args;
        fiber.state.host_depth -= 1;
        let outcome = match result {
            Ok(v) => Ok(v),
            Err(HostFault::Panic(message) | HostFault::Message(message)) => {
                Err(self.panic(fiber, span, message))
            }
            Err(HostFault::Cancel) => {
                fiber.state.cancelling = true;
                Err(crate::ctl::Ctl::Cancel)
            }
        };
        fiber.frames.pop();
        outcome
    }
}
