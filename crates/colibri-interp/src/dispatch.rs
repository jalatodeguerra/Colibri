//! Interface dispatch and the user-written implementations that replace derived behaviour
//! (DESIGN §7.4, §9.2; ARCHITECTURE §6).
//!
//! `Callee::Bound` calls find their implementation from the runtime type of the receiver, in
//! `impls[(runtime type, interface)]`; `Callee::Dyn` calls (and `Bound` calls on an interface
//! value) read it from the value. A type that has no explicit implementation of a built-in
//! interface gets the derived one, which `colibri_core::dispatch` runs structurally. This module
//! runs the implementations that are found: the calling convention and the places a `var self`
//! receiver lives in are the engine's.

use std::rc::Rc;

use colibri_check::hir::{ArithOp, BinOp, Coercion, Num, Place, TypeKey, UnOp};
use colibri_check::types::{ADD, DIV, ITER, IntTy, MUL, NEG, SUB, TypeId};
use colibri_syntax::Span;

pub(crate) use colibri_core::dispatch::Tables;

use crate::ctl::Eval;
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::value::{DynVal, Value};
use colibri_core::prelude::*;

impl Interp {
    // ---- interface values ----

    /// A value converted to an interface value.
    pub(crate) fn coerce(&mut self, value: Value, how: &Coercion) -> Value {
        let impl_id = match how {
            Coercion::IntoError(conv) => return self.convert_to_error(value, *conv),
            Coercion::ResultIntoError(conv) => return self.result_into_error(value, *conv),
            Coercion::F32ToFloat => {
                return match value {
                    Value::F32(x) => Value::Float(f64::from(x)),
                    other => other,
                };
            }
            Coercion::ToDyn(id) => id.0,
            Coercion::ToDynBound(iface) => {
                if let Value::Dyn(d) = &value
                    && self
                        .core
                        .dispatch
                        .impl_def(d.impl_id)
                        .is_some_and(|i| i.iface == *iface)
                {
                    return value;
                }
                match self.impl_for(&value, *iface) {
                    Some(i) => i,
                    None => return value,
                }
            }
        };
        Value::Dyn(Rc::new(DynVal {
            impl_id,
            val: value,
        }))
    }

    /// Calls method `method` of `iface` on the receiver, which is the first of `values` or,
    /// for a `var self` method, the value in `receiver`.
    pub(crate) fn call_interface(
        &mut self,
        fiber: &mut Fiber,
        iface: TypeId,
        method: u32,
        mut values: Vec<Value>,
        receiver: Option<&Place>,
        span: Span,
    ) -> Eval {
        if let Some(place) = receiver {
            return self.call_interface_inout(fiber, iface, method, values, place, span);
        }
        let Some(recv) = values.first().cloned() else {
            return Err(self.internal(fiber, span, "an interface call without a receiver"));
        };
        let (impl_id, inner) = match &recv {
            Value::Dyn(d) => (Some(d.impl_id), d.val.clone()),
            other => (self.impl_for(other, iface), other.clone()),
        };
        values[0] = inner;
        drop(recv);
        match self.target_of(impl_id, method) {
            Some(f) => {
                let Some(body) = self.fns.get(f.0 as usize).cloned() else {
                    return Err(self.internal(fiber, span, "unknown function"));
                };
                if let Some(frame) = fiber.frames.last_mut() {
                    frame.span = span;
                }
                self.call_body(fiber, body, values)
            }
            None => self.derived_call(fiber, iface, values, span),
        }
    }

    /// `a + b`, `-a`, ... where the left operand has the type of a bounded type parameter: the
    /// impl is found from the runtime types, `rhs` being the right-hand head when the bound
    /// fixes it. Numbers have no impl; they run the built-in operation, with the width of the
    /// value (`int` and `u64`: the narrow integers cannot satisfy the bound).
    pub(crate) fn call_operator(
        &mut self,
        fiber: &mut Fiber,
        iface: TypeId,
        rhs: Option<TypeKey>,
        values: Vec<Value>,
        span: Span,
    ) -> Eval {
        if let Some(f) = self.operator_target(iface, rhs, &values) {
            let Some(body) = self.fns.get(f.0 as usize).cloned() else {
                return Err(self.internal(fiber, span, "unknown function"));
            };
            if let Some(frame) = fiber.frames.last_mut() {
                frame.span = span;
            }
            return self.call_body(fiber, body, values);
        }
        self.builtin_operator(fiber, iface, values, span)
    }

    /// The operator of a number or a string reached through a bound.
    fn builtin_operator(
        &mut self,
        fiber: &mut Fiber,
        iface: TypeId,
        mut values: Vec<Value>,
        span: Span,
    ) -> Eval {
        let num = match values.first() {
            Some(Value::Int(_)) => Some(Num::Int(IntTy::I64)),
            Some(Value::U64(_)) => Some(Num::Int(IntTy::U64)),
            Some(Value::Float(_)) => Some(Num::Float),
            Some(Value::F32(_)) => Some(Num::F32),
            _ => None,
        };
        if iface == NEG {
            return match (values.pop(), num) {
                (Some(v), Some(n)) => self.unary(fiber, UnOp::Neg(n), v, span),
                _ => Err(self.internal(fiber, span, "an operator without an implementation")),
            };
        }
        let arith = match iface {
            ADD => ArithOp::Add,
            SUB => ArithOp::Sub,
            MUL => ArithOp::Mul,
            DIV => ArithOp::Div,
            _ => return Err(self.internal(fiber, span, "not an operator interface")),
        };
        let op = match (&values.first(), num) {
            (Some(Value::Str(_) | Value::Bytes(_)), _) if iface == ADD => BinOp::Concat,
            (_, Some(n)) => BinOp::Arith(arith, n),
            _ => return Err(self.internal(fiber, span, "an operator without an implementation")),
        };
        let (Some(r), Some(l)) = (values.pop(), values.pop()) else {
            return Err(self.internal(fiber, span, "an operator without operands"));
        };
        self.binary(fiber, op, l, r, span)
    }

    /// A `var self` interface call: the receiver is taken out of its place, changed by the
    /// callee, and written back (re-boxed when it is an interface value).
    fn call_interface_inout(
        &mut self,
        fiber: &mut Fiber,
        iface: TypeId,
        method: u32,
        values: Vec<Value>,
        place: &Place,
        span: Span,
    ) -> Eval {
        let anchor = self.resolve_anchor(fiber, place, span)?;
        let _hold = self.hold_place(&anchor, place);
        let current = self.take_receiver(fiber, &anchor, &place.path, span)?;
        let (impl_id, inner, was_dyn) = match current {
            Value::Dyn(rc) => {
                let id = rc.impl_id;
                match Rc::try_unwrap(rc) {
                    Ok(d) => (Some(id), d.val, true),
                    Err(rc) => (Some(id), rc.val.clone(), true),
                }
            }
            other => (self.impl_for(&other, iface), other, false),
        };
        if iface == ITER
            && method == 0
            && let Value::Iter(obj) = &inner
        {
            // A built-in iterator is an object: advancing it changes nothing in its place.
            let obj = Rc::clone(obj);
            let stepped = self.native_next(fiber, &obj, span);
            let restored = rewrap(inner, impl_id, was_dyn);
            self.release_all(values);
            return match self.write_resolved(fiber, &anchor, &place.path, restored, false, span) {
                Ok(old) => {
                    self.release(old);
                    stepped.map(Value::option)
                }
                Err(ctl) => {
                    if let Ok(v) = stepped {
                        self.release(v.unwrap_or(Value::Unit));
                    }
                    Err(ctl)
                }
            };
        }
        let Some(f) = self.target_of(impl_id, method) else {
            // Derived interfaces have no `var self` methods; put the value back.
            let restored = rewrap(inner, impl_id, was_dyn);
            let back = self.write_resolved(fiber, &anchor, &place.path, restored, false, span);
            self.release_all(values);
            return match back {
                Ok(old) => {
                    self.release(old);
                    Err(self.internal(fiber, span, "a derived method that changes its receiver"))
                }
                Err(ctl) => Err(ctl),
            };
        };
        let Some(body) = self.fns.get(f.0 as usize).cloned() else {
            return Err(self.internal(fiber, span, "unknown function"));
        };
        let mut args = values;
        // An implementation that does not change its receiver (a class's `self`) hands nothing
        // back; the value goes back where it was.
        let unchanged = (!body.mutates_self).then(|| inner.clone());
        args.insert(0, inner);
        let (result, out) = self.call_body_inout(fiber, body, args);
        if let Some(updated) = out.or(unchanged) {
            let new = rewrap(updated, impl_id, was_dyn);
            match self.write_resolved(fiber, &anchor, &place.path, new, false, span) {
                Ok(old) => self.release(old),
                Err(ctl) => {
                    self.release_outcome(result);
                    return Err(ctl);
                }
            }
        }
        result
    }
}

/// Boxes `val` again as the interface value it was taken from.
fn rewrap(val: Value, impl_id: Option<u32>, was_dyn: bool) -> Value {
    match impl_id {
        Some(impl_id) if was_dyn => Value::Dyn(Rc::new(DynVal { impl_id, val })),
        _ => val,
    }
}
