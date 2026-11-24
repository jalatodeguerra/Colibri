//! What the embedding program attaches to the machine: host functions and host class hooks,
//! and the calls of them from Colibri code (EMBEDDING §1, §6; vm-plan §1.10). The state that is
//! not tied to this engine (host types, host state, the root slab) is in `colibri_core::host`.
//!
//! A host function runs on the native stack of whoever called it, with a frame of its own on the
//! fiber (a `HostShim`: no code, no registers) so that a trace names it, the call depth counts it
//! and a host object it borrows is held under its body. Everything it does to Colibri code in
//! turn (a callback, `Ctx::call`) is a nested dispatch loop on the same fiber.

use std::collections::HashMap;
use std::rc::Rc;

use colibri_check::hir::{Body, HostFnId, Program};
use colibri_check::types::{Ty, TypeId};
use colibri_core::ctl::{Ctl, Eval};
use colibri_core::host::{HookSet, HostHook};
use colibri_core::prelude::*;
use colibri_core::value::Value;
use colibri_syntax::Span;

pub use colibri_core::host::HostFault;

use crate::bytecode::{Chunk, Op, Site};
use crate::fiber::{Frame, FrameKind, VmFiber};
use crate::machine::Machine;

/// A host function as the machine calls it: the arguments are the caller's references, which
/// the function must release (or hand on).
pub type HostImpl = Rc<dyn Fn(&mut Machine, &mut VmFiber, Vec<Value>) -> Result<Value, HostFault>>;

/// What a host class customises: how its objects are shown and compared (`print`, `"{x}"`,
/// `==`, `Map` and `Set` lookups). The implementations take the objects as ordinary arguments
/// (one for `show`, two for `eq`) and return a `str` or a `bool`.
#[derive(Clone, Default)]
pub struct HostClassHooks {
    pub show: Option<HostImpl>,
    pub eq: Option<HostImpl>,
    /// The hash of one object, as an `int`, consistent with `eq` (what `hash()` gives).
    pub hash: Option<HostImpl>,
}

/// The host functions and class hooks that were installed. They are closures over [`Machine`]
/// and [`VmFiber`], so unlike the rest of what the host attached they stay with the engine.
#[derive(Default)]
pub(crate) struct HostImpls {
    /// Indexed by [`HostFnId`]; `None` for a function the host did not install (a trap slot).
    pub fns: Vec<Option<HostImpl>>,
    /// The classes that customise `Show`, `==` or `hash`, by type.
    pub class_hooks: HashMap<TypeId, HostClassHooks>,
    /// The frame chunk of each host function, made at its first call (see [`Machine::shim`]).
    shims: Vec<Option<Rc<Chunk>>>,
}

impl Machine {
    /// Forgets the host functions of an earlier program; `program`'s start out as trap slots.
    pub(crate) fn load_host_impls(&mut self, program: &Program) {
        self.host_impls.fns = vec![None; program.host_fns.len()];
        self.host_impls.class_hooks.clear();
        self.host_impls.shims = vec![None; program.host_fns.len()];
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
            HookSet {
                show: hooks.show.is_some(),
                eq: hooks.eq.is_some(),
                hash: hooks.hash.is_some(),
            },
        );
        self.host_impls.class_hooks.insert(ty, hooks);
    }

    /// The chunk of the frame a call of host function `id` gets: it has no code, and its body is
    /// the host function's empty one, which names it in a trace and holds what it borrows.
    fn shim(&mut self, id: HostFnId, body: &Rc<Body>) -> Rc<Chunk> {
        let slot = id.0 as usize;
        if self.host_impls.shims.len() <= slot {
            self.host_impls.shims.resize(slot + 1, None);
        }
        // A hot reload swaps the bodies; the chunk follows.
        if let Some(chunk) = &self.host_impls.shims[slot]
            && Rc::ptr_eq(&chunk.body, body)
        {
            return Rc::clone(chunk);
        }
        let chunk = Rc::new(Chunk {
            fn_id: None,
            body: Rc::clone(body),
            code: Box::new([]),
            spans: Box::new([]),
            consts: Box::new([]),
            sites: Box::new([]),
            unwind: Box::new([]),
            nregs: 0,
            nparams: 0,
            hidden: false,
        });
        self.host_impls.shims[slot] = Some(Rc::clone(&chunk));
        chunk
    }

    /// `CallHost`: the host function of the site gets references of its own to the arguments in
    /// the window, which stay for the caller to drop.
    #[inline(never)]
    pub(crate) fn op_call_host(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let Op::CallHost { d, base: a, site } = op else {
            unreachable!("op_call_host is called for its own instruction")
        };
        let span = chunk.spans[pc - 1];
        let Site::Host(id, argc, type_args) = &chunk.sites[usize::from(site)] else {
            return Err(self.internal(fiber, span, "a host call without its function"));
        };
        let from = base + usize::from(a);
        let args: Vec<Value> = fiber.regs[from..from + usize::from(*argc)].to_vec();
        let result = self.call_host(fiber, *id, type_args, args, span)?;
        Self::put_reg(fiber, base + usize::from(d), result);
        Ok(())
    }

    /// A call of the host function `id` from Colibri code: the depth check, one budget step, a
    /// frame of its own, the type arguments of the call for a function that reads them
    /// (`json.decode[T]`), and the fault mapped to a panic or a cancellation.
    pub(crate) fn call_host(
        &mut self,
        fiber: &mut VmFiber,
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
        if fiber.visible_depth() >= self.max_depth {
            self.release_all(args);
            return Err(self.overflow(fiber, span));
        }
        if let Err(ctl) = self.step(fiber, span) {
            self.release_all(args);
            return Err(ctl);
        }
        let shim = self.shim(id, &body);
        let base = fiber.top() as u32;
        fiber.frames.push(Frame {
            chunk: shim,
            pc: 0,
            base,
            ret: u32::MAX,
            kind: FrameKind::HostShim,
            noted: Some((0, span)),
            temps_mark: fiber.temps.len() as u32,
        });
        fiber.state.host_depth += 1;
        let outer_type_args = std::mem::replace(&mut self.core.host.type_args, type_args.to_vec());
        let result = imp(self, fiber, args);
        self.core.host.type_args = outer_type_args;
        fiber.state.host_depth -= 1;
        // The trace of a panic includes the host function's frame, so it is popped after.
        let outcome = match result {
            Ok(v) => match self
                .core
                .host_result_mismatch(id.0 as usize, &body, type_args, &v)
            {
                None => Ok(v),
                Some(message) => {
                    self.release(v);
                    Err(self.panic(fiber, span, message))
                }
            },
            Err(HostFault::Panic(message) | HostFault::Message(message)) => {
                Err(self.panic(fiber, span, message))
            }
            Err(HostFault::Cancel) => {
                fiber.state.cancelling = true;
                Err(Ctl::Cancel)
            }
        };
        let shim = fiber.frames.pop();
        debug_assert!(
            shim.is_some_and(|f| f.kind == FrameKind::HostShim),
            "the host call's frame is not on top"
        );
        outcome
    }

    /// A hook of a host class (`Show`, `==`, `hash`), called by core on behalf of `print`, `==`,
    /// map and set lookups. It runs without a frame of its own, as in the tree-walker.
    pub(crate) fn run_host_hook(
        &mut self,
        fiber: &mut VmFiber,
        ty: TypeId,
        hook: HostHook,
        args: Vec<Value>,
    ) -> Result<Value, HostFault> {
        let hooks = self.host_impls.class_hooks.get(&ty);
        let hook = hooks.and_then(|h| match hook {
            HostHook::Show => h.show.clone(),
            HostHook::Eq => h.eq.clone(),
            HostHook::Hash => h.hash.clone(),
        });
        match hook {
            Some(hook) => hook(self, fiber, args),
            None => {
                self.release_all(args);
                Err(HostFault::Message(
                    "internal error: the host class has no such hook".to_string(),
                ))
            }
        }
    }
}
