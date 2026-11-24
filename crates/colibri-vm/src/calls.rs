//! The calls that are not a plain `Call`: function values and closures, interface methods with
//! their inline caches, operators reached through a bound, user iterators, and building
//! closures (vm-plan step 7). Out of line, like the other cold arms of the dispatch loop.

use std::rc::Rc;

use colibri_check::hir::{CaptureMode, FnId};
use colibri_core::closure::{Captured, ClosureObj};
use colibri_core::ctl::Ctl;
use colibri_core::dispatch::runtime_key;
use colibri_core::item_of;
use colibri_core::prelude::*;
use colibri_core::value::{DynVal, Value};

use crate::bytecode::{Chunk, IcEntry, IcKey, Site};
use crate::fiber::VmFiber;
use crate::machine::Machine;

impl Machine {
    /// The compiled body of `f`, or the internal error the call reports.
    pub(crate) fn callee_chunk(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        pc: usize,
        f: FnId,
    ) -> Result<Rc<Chunk>, Ctl> {
        match self.fns.get(f.0 as usize).and_then(|s| s.chunk.get()) {
            Some(c) => Ok(Rc::clone(c)),
            None => self.callee_slow(fiber, chunk, pc, f),
        }
    }

    /// Fills the capture registers of a closure's frame, whose window starts at `new_base`: a
    /// weak capture is a strong `T?` for the length of the call.
    pub(crate) fn install_captures(
        &mut self,
        fiber: &mut VmFiber,
        new_base: usize,
        c: &ClosureObj,
    ) {
        for cap in &c.captures {
            let value = match (&cap.mode, &cap.value) {
                (CaptureMode::Weak, Value::Weak(w)) => {
                    self.core.option(w.upgrade().map(Value::Obj))
                }
                (_, v) => v.clone(),
            };
            Self::put_reg(fiber, new_base + cap.inner.0 as usize, value);
        }
    }

    /// The chunk of a closure's lifted body, checked to expect the closure's captures.
    pub(crate) fn closure_chunk(&mut self, c: &ClosureObj) -> Result<Rc<Chunk>, String> {
        let Some(f) = self.core.closure_fn(c) else {
            return Err("unknown closure body".to_string());
        };
        let chunk = match self.chunk_of(f) {
            Some(Ok(chunk)) => chunk,
            Some(Err(msg)) => return Err(format!("cannot compile a closure: {msg}")),
            None => return Err("unknown closure body".to_string()),
        };
        let same = chunk.body.captures.len() == c.captures.len()
            && chunk
                .body
                .captures
                .iter()
                .zip(&c.captures)
                .all(|(a, b)| *a == b.inner);
        if same {
            Ok(chunk)
        } else {
            Err("a closure does not match its body".to_string())
        }
    }

    /// `CallValue`: the function value is in register `a`, its arguments follow.
    #[inline(never)]
    pub(crate) fn op_call_value(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        d: u16,
        a: u16,
    ) -> Result<(), Ctl> {
        let at = base + usize::from(a);
        self.call_value_at_reg(fiber, chunk, base, pc, d, at, at + 1)
    }

    /// `CallValueIn`: like `CallValue`, with the function value in register `f` and the
    /// arguments in the window that starts at `a`: no copy of the function value is made.
    #[inline(never)]
    pub(crate) fn op_call_value_in(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [d, f, a]: [u16; 3],
    ) -> Result<(), Ctl> {
        let (at, window) = (base + usize::from(f), base + usize::from(a));
        self.call_value_at_reg(fiber, chunk, base, pc, d, at, window)
    }

    /// Calls the function value in the absolute register `at`, whose arguments start at
    /// `window`; the result goes to register `d` of the caller whose window starts at `base`.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn call_value_at_reg(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        d: u16,
        at: usize,
        window: usize,
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let (callee, closure) = match &fiber.regs[at] {
            Value::Fn(id, _) => {
                let id = *id;
                (self.callee_chunk(fiber, chunk, pc, id)?, None)
            }
            Value::Closure(c) => {
                let c = Rc::clone(c);
                match self.closure_chunk(&c) {
                    Ok(callee) => (callee, Some(c)),
                    Err(msg) => return Err(self.internal(fiber, span, &msg)),
                }
            }
            _ => {
                return Err(self.internal(
                    fiber,
                    span,
                    "a call of something that is not a function",
                ));
            }
        };
        self.push_call(fiber, callee, window, base + usize::from(d), chunk, pc)?;
        if let Some(c) = closure {
            self.install_captures(fiber, window, &c);
        }
        Ok(())
    }

    /// `MakeClosure`: each capture reads its variable in the running frame.
    #[inline(never)]
    pub(crate) fn op_make_closure(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        d: u16,
        site: u16,
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let Site::Closure { f, captures } = &chunk.sites[usize::from(site)] else {
            return Err(self.internal(fiber, span, "a closure without its site"));
        };
        let mut made = Vec::with_capacity(captures.len());
        for cap in captures.iter() {
            let slot = &fiber.regs[base + cap.outer.0 as usize];
            let value = match (cap.mode, slot) {
                (CaptureMode::ByValue, Value::Cell(cell)) => Some(cell.get()),
                (CaptureMode::ByValue, v) => Some(v.clone()),
                (CaptureMode::Cell, v @ Value::Cell(_)) => Some(v.clone()),
                (CaptureMode::Weak, v) => match colibri_core::value::unwrap_cell_ref(v) {
                    Value::Obj(o) => Some(Value::Weak(Rc::downgrade(&o))),
                    _ => None,
                },
                _ => None,
            };
            match value {
                Some(value) => made.push(Captured {
                    inner: cap.inner,
                    mode: cap.mode,
                    value,
                }),
                None => {
                    let released = made.into_iter().map(|c: Captured| c.value).collect();
                    self.release_all(released);
                    return Err(self.internal(
                        fiber,
                        span,
                        "a closure captured a missing variable",
                    ));
                }
            }
        }
        let closure = Rc::new(ClosureObj {
            f: *f,
            generation: self.core.generation(),
            captures: made,
        });
        self.core.track_closure(&closure, Some(span));
        Self::put_reg(fiber, base + usize::from(d), Value::Closure(closure));
        Ok(())
    }

    /// `CallBound` and `CallDyn`: the implementation is found from the receiver in register `a`
    /// (through the site's cache), the receiver is unwrapped from its interface value, and the
    /// method runs with the window starting at `a`.
    #[inline(never)]
    pub(crate) fn op_call_iface(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [d, a, site]: [u16; 3],
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let Site::Iface(s) = &chunk.sites[usize::from(site)] else {
            return Err(self.internal(fiber, span, "an interface call without its site"));
        };
        let at = base + usize::from(a);
        let key = match &fiber.regs[at] {
            Value::Dyn(dv) => Some(IcKey::Impl(dv.impl_id)),
            other => runtime_key(other).map(IcKey::Type),
        };
        let cached = s
            .cache
            .get()
            .filter(|e| e.epoch == self.epoch && Some(e.key) == key);
        let target = match cached {
            Some(e) => e.target,
            None => {
                let impl_id = match &fiber.regs[at] {
                    Value::Dyn(dv) => Some(dv.impl_id),
                    other => {
                        let v = other.clone();
                        self.impl_for(&v, s.iface)
                    }
                };
                let target = self.target_of(impl_id, s.method);
                if let Some(key) = key {
                    s.cache.set(Some(IcEntry {
                        epoch: self.epoch,
                        key,
                        target,
                    }));
                }
                target
            }
        };
        // The receiver is replaced by what the interface value holds.
        if let Value::Dyn(dv) = &fiber.regs[at] {
            let inner = dv.val.clone();
            let wrapper = std::mem::replace(&mut fiber.regs[at], inner);
            self.release(wrapper);
        }
        match target {
            Some(f) => {
                let callee = self.callee_chunk(fiber, chunk, pc, f)?;
                self.push_call(fiber, callee, at, base + usize::from(d), chunk, pc)
            }
            None => {
                let values: Vec<Value> = fiber.regs[at..at + usize::from(s.argc)].to_vec();
                let result = self.derived_call(fiber, s.iface, values, span)?;
                Self::put_reg(fiber, base + usize::from(d), result);
                Ok(())
            }
        }
    }

    /// `CallIfaceInout`: a `mut self` method through an interface. The place's value is in
    /// register `a`; the receiver proper goes to `a + 1`, which is where the callee's window
    /// starts, and `a` keeps the interface value's shell (an empty one for a plain value).
    #[inline(never)]
    pub(crate) fn op_call_iface_inout(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [d, a, site]: [u16; 3],
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let Site::Iface(s) = &chunk.sites[usize::from(site)] else {
            return Err(self.internal(fiber, span, "an interface call without its site"));
        };
        let at = base + usize::from(a);
        let current = std::mem::replace(&mut fiber.regs[at], Value::Unit);
        let (impl_id, inner) = match current {
            Value::Dyn(rc) => {
                let id = rc.impl_id;
                let inner = match Rc::try_unwrap(rc) {
                    Ok(dv) => dv.val,
                    Err(rc) => rc.val.clone(),
                };
                fiber.regs[at] = Value::Dyn(Rc::new(DynVal {
                    impl_id: id,
                    val: Value::Unit,
                }));
                (Some(id), inner)
            }
            other => {
                let id = self.impl_for(&other, s.iface);
                (id, other)
            }
        };
        if s.iface == colibri_check::types::ITER
            && s.method == 0
            && let Value::Iter(obj) = &inner
        {
            // A built-in iterator is an object: advancing it changes nothing in its place.
            let obj = Rc::clone(obj);
            Self::put_reg(fiber, at + 1, inner);
            let stepped = self.native_next(fiber, &obj, span)?;
            Self::put_reg(fiber, base + usize::from(d), Value::option(stepped));
            return Ok(());
        }
        Self::put_reg(fiber, at + 1, inner);
        let Some(f) = self.target_of(impl_id, s.method) else {
            return Err(self.internal(fiber, span, "a derived method that changes its receiver"));
        };
        let callee = self.callee_chunk(fiber, chunk, pc, f)?;
        self.push_call(fiber, callee, at + 1, base + usize::from(d), chunk, pc)
    }

    /// `Rewrap`: the receiver in `recv` goes back into the interface value whose shell is in
    /// `shell`, or into `shell` itself when the receiver was a plain value.
    #[inline(never)]
    pub(crate) fn op_rewrap(&mut self, fiber: &mut VmFiber, base: usize, shell: u16, recv: u16) {
        let s = base + usize::from(shell);
        let v = std::mem::replace(&mut fiber.regs[base + usize::from(recv)], Value::Unit);
        match &fiber.regs[s] {
            Value::Dyn(shell_rc) => {
                let impl_id = shell_rc.impl_id;
                fiber.regs[s] = Value::Dyn(Rc::new(DynVal { impl_id, val: v }));
            }
            _ => Self::put_reg(fiber, s, v),
        }
    }

    /// `CallOperator`.
    #[inline(never)]
    pub(crate) fn op_call_operator(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [d, a, site]: [u16; 3],
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let Site::Operator { iface, rhs, argc } = &chunk.sites[usize::from(site)] else {
            return Err(self.internal(fiber, span, "an operator without its site"));
        };
        let at = base + usize::from(a);
        let end = at + usize::from(*argc);
        match self.operator_target(*iface, *rhs, &fiber.regs[at..end]) {
            Some(f) => {
                let callee = self.callee_chunk(fiber, chunk, pc, f)?;
                self.push_call(fiber, callee, at, base + usize::from(d), chunk, pc)
            }
            None => {
                let values: Vec<Value> = fiber.regs[at..end].to_vec();
                let result = self.builtin_operator(fiber, *iface, values, span)?;
                Self::put_reg(fiber, base + usize::from(d), result);
                Ok(())
            }
        }
    }

    /// Whether the iterator in a register is a boxed user iterator that has a `next` of Colibri
    /// code (a built-in iterator, boxed or not, is stepped natively).
    pub(crate) fn iter_is_user(&self, v: &Value) -> bool {
        let Value::Dyn(dv) = v else {
            return false;
        };
        !matches!(dv.val, Value::Iter(_))
            && self
                .target_of(Some(dv.impl_id), 0)
                .is_some_and(|f| self.fn_span(f).is_some())
    }

    /// `IterNextCall`.
    #[inline(never)]
    pub(crate) fn op_iter_next_call(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        a: u16,
        w: u16,
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let at = base + usize::from(a);
        let Value::Dyn(rc) = std::mem::replace(&mut fiber.regs[at], Value::Unit) else {
            return Err(self.internal(fiber, span, "not an interface value"));
        };
        let impl_id = rc.impl_id;
        let inner = match Rc::try_unwrap(rc) {
            Ok(dv) => dv.val,
            Err(rc) => rc.val.clone(),
        };
        fiber.regs[at] = Value::Dyn(Rc::new(DynVal {
            impl_id,
            val: Value::Unit,
        }));
        let wr = base + usize::from(w);
        Self::put_reg(fiber, wr, inner);
        let Some(f) = self.target_of(Some(impl_id), 0) else {
            return Err(self.internal(fiber, span, "an iterator without a `next`"));
        };
        let callee = self.callee_chunk(fiber, chunk, pc, f)?;
        self.push_call(fiber, callee, wr, wr - 1, chunk, pc)
    }

    /// `IterNextEnd`: `Ok(true)` when there is an item (in `d`, one budget step charged).
    #[inline(never)]
    pub(crate) fn op_iter_next_end(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [d, a, w]: [u16; 3],
    ) -> Result<bool, Ctl> {
        let span = chunk.spans[pc - 1];
        self.op_rewrap(fiber, base, a, w);
        let next = std::mem::replace(&mut fiber.regs[base + usize::from(w) - 1], Value::Unit);
        let item = item_of(&next);
        self.release(next);
        match item {
            Some(Some(item)) => {
                if let Err(ctl) = self.step(fiber, span) {
                    self.release(item);
                    return Err(ctl);
                }
                Self::put_reg(fiber, base + usize::from(d), item);
                Ok(true)
            }
            Some(None) => Ok(false),
            None => Err(self.internal(fiber, span, "`next` did not return an `Option`")),
        }
    }
}
