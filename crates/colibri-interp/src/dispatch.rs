//! Interface dispatch and the user-written implementations that replace derived behaviour
//! (DESIGN §7.4, §9.2; ARCHITECTURE §6).
//!
//! `Callee::Bound` calls find their implementation from the runtime type of the receiver, in
//! `impls[(runtime type, interface)]`; `Callee::Dyn` calls (and `Bound` calls on an interface
//! value) read it from the value. A type that has no explicit implementation of a built-in
//! interface gets the derived one, which this module runs structurally.

use std::rc::Rc;

use colibri_check::hir::{ArithOp, BinOp, Coercion, FnId, ImplDef, Num, Place, TypeKey, UnOp};
use colibri_check::types::{
    ADD, CLONE, DIV, DROP, EQ, HASH, ITER, ITERABLE, IntTy, MUL, NEG, ORD, SHOW, SUB, Ty, TypeId,
};
use colibri_syntax::{FileId, Span};

pub(crate) use colibri_core::dispatch::Tables;

use crate::ctl::{Ctl, Eval};
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::ordered::{hash_value, hash_value_with};
use crate::value::{DynVal, Value};
use colibri_core::prelude::*;

/// The head of a value's type, which interface dispatch works on.
pub(crate) fn runtime_key(v: &Value) -> Option<TypeKey> {
    Some(match v {
        Value::Unit => TypeKey::Unit,
        Value::Bool(_) => TypeKey::Bool,
        // Narrow integers carry no width; `impl` for them is not accepted (check/ifaces.rs).
        Value::Int(_) => TypeKey::Int(IntTy::I64),
        Value::U64(_) => TypeKey::Int(IntTy::U64),
        Value::Ptr(_) => TypeKey::Ptr,
        Value::Float(_) => TypeKey::Float,
        Value::F32(_) => TypeKey::F32,
        Value::Char(_) => TypeKey::Char,
        Value::Str(_) => TypeKey::Str,
        Value::Bytes(_) => TypeKey::Bytes,
        Value::Struct(s) => TypeKey::Adt(s.ty),
        Value::Tuple(_) => TypeKey::Tuple,
        Value::Enum(e) => TypeKey::Adt(e.ty),
        Value::List(_) => TypeKey::List,
        Value::Map(_) => TypeKey::Map,
        Value::Set(_) => TypeKey::Set,
        Value::Obj(o) => TypeKey::Adt(o.ty()),
        Value::Host(r) => TypeKey::Adt(r.ty),
        Value::Range(_) => TypeKey::Range,
        Value::Weak(_)
        | Value::Dyn(_)
        | Value::Fn(..)
        | Value::Closure(_)
        | Value::Cell(_)
        | Value::ByteBuf(_)
        | Value::Iter(_) => return None,
    })
}

impl Interp {
    /// How many map or set key lookups scanned the stored keys for a user-written `Eq` (a
    /// test hook: keys that cannot reach one never scan).
    pub fn key_scans(&self) -> u64 {
        self.core.key_scans
    }

    /// A `Map` key or `Set` element is looked up by the user-written `Eq` of its type: the
    /// key in `args[1]` is replaced by the stored key that `eq` says is equal to it, so the
    /// structural lookup finds it. This scans the keys; a user `Hash` is not consulted (it
    /// cannot be observed, ARCHITECTURE §6).
    pub(crate) fn canonicalise_key(
        &mut self,
        fiber: &mut Fiber,
        keys: &[Value],
        args: &mut [Value],
        span: Span,
    ) -> Eval<()> {
        let Some(key) = args.get(1).cloned() else {
            return Ok(());
        };
        if keys.is_empty() {
            return Ok(());
        }
        self.core.key_scans += 1;
        for stored in keys {
            // Each comparison costs a step, so a budget stops a quadratic scan.
            self.step(fiber, span)?;
            if self.eq_deep(fiber, stored, &key, span)? {
                args[1] = stored.clone();
                break;
            }
        }
        Ok(())
    }

    /// `key` as a `Map` or `Set` stores it: the stored key that a user-written (or host
    /// class) `Eq` says is equal to it, else `key` itself. For the places that add keys
    /// without a method call (`m[k] = v`, a map literal, `Set(list)`, set algebra).
    pub(crate) fn canonical_key(
        &mut self,
        fiber: &mut Fiber,
        stored: impl FnOnce() -> Vec<Value>,
        key: Value,
        span: Span,
    ) -> Eval<Value> {
        if !self.key_needs_scan(&key) {
            return Ok(key);
        }
        self.core.key_scans += 1;
        for candidate in stored() {
            self.step(fiber, span)?;
            if self.eq_deep(fiber, &candidate, &key, span)? {
                return Ok(candidate);
            }
        }
        Ok(key)
    }

    // ---- interface values ----

    /// The implementation of `iface` for the runtime type of `v`, made up (as a derived one)
    /// when the program has none.
    pub(crate) fn impl_for(&mut self, v: &Value, iface: TypeId) -> Option<u32> {
        let key = runtime_key(v)?;
        if let Some(&i) = self.core.dispatch.index.get(&(key, iface)) {
            return Some(i);
        }
        let methods = self
            .core
            .types
            .get(iface.0 as usize)
            .map_or(0, |t| t.iface_methods().len());
        let id = self.core.dispatch.impls.len() as u32;
        self.core.dispatch.impls.push(ImplDef {
            iface,
            iface_args: Vec::new(),
            target: Ty::Unit,
            key,
            rhs: None,
            methods: vec![None; methods],
            derived: true,
            span: Span::new(FileId(0), 0, 0),
        });
        self.core.dispatch.index.insert((key, iface), id);
        Some(id)
    }

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
                        .impls
                        .get(d.impl_id as usize)
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
        let left = values.first().and_then(runtime_key);
        let right = rhs.or_else(|| values.get(1).and_then(runtime_key));
        // Two primitives of one kind always run the built-in operation: no impl can replace it
        // (the orphan rule), and none is looked up.
        let primitives = matches!(
            (values.first(), values.get(1)),
            (Some(Value::Int(_)), Some(Value::Int(_)))
                | (Some(Value::U64(_)), Some(Value::U64(_)))
                | (Some(Value::Float(_)), Some(Value::Float(_)))
                | (Some(Value::F32(_)), Some(Value::F32(_)))
                | (Some(Value::Str(_)), Some(Value::Str(_)))
        );
        let found = left.filter(|_| !primitives).and_then(|k| {
            let tables = &self.core.dispatch;
            tables
                .operator_impl(k, iface, right)
                .or_else(|| tables.operator_impl(k, iface, None))
        });
        if let Some(f) = self.target_of(found, 0) {
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

    /// The function an implementation runs for `method`, if it has one of its own.
    pub(crate) fn target_of(&self, impl_id: Option<u32>, method: u32) -> Option<FnId> {
        self.core
            .dispatch
            .impls
            .get(impl_id? as usize)?
            .methods
            .get(method as usize)
            .copied()
            .flatten()
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

    /// The `hash()` of a value. Equal values hash equally, which for an object of a host class
    /// that customises `==` means its own hash: without one there is no hash that agrees with
    /// `==`, so asking for it is a panic, not a wrong answer. (Map and set lookups of such
    /// keys scan with `==` and never use this.)
    fn hash_deep(&mut self, fiber: &mut Fiber, v: &Value, span: Span) -> Eval<u64> {
        if !self.core.host.any_eq_hook || !self.key_needs_scan(v) {
            return Ok(hash_value(v));
        }
        let mut failed: Option<Ctl> = None;
        let h = hash_value_with(v, &mut |object| {
            if failed.is_some() {
                return None;
            }
            let Value::Host(r) = object else {
                return None;
            };
            let hooks = self.host_impls.class_hooks.get(&r.ty)?;
            hooks.eq.as_ref()?;
            let Some(hook) = hooks.hash.clone() else {
                let name = self.core.types[r.ty.0 as usize].name.clone();
                failed = Some(self.panic(
                    fiber,
                    span,
                    format!(
                        "`{name}` compares with its own `==` but gives no hash, so `hash()` cannot agree with `==`"
                    ),
                ));
                return None;
            };
            let outcome = self.step(fiber, span).and_then(|()| {
                hook(self, fiber, vec![object.clone()]).map_err(|f| self.host_fault_ctl(fiber, f))
            });
            match outcome {
                Ok(Value::Int(n)) => Some(n as u64),
                Ok(_) => None,
                Err(ctl) => {
                    failed = Some(ctl);
                    None
                }
            }
        });
        match failed {
            Some(ctl) => Err(ctl),
            None => Ok(h),
        }
    }

    /// A method of a built-in interface on a type that has no implementation of its own.
    fn derived_call(
        &mut self,
        fiber: &mut Fiber,
        iface: TypeId,
        values: Vec<Value>,
        span: Span,
    ) -> Eval {
        let result = match (iface, values.as_slice()) {
            (EQ, [a, b]) => self.eq_deep(fiber, a, b, span).map(Value::Bool),
            (ORD, [a, b]) => match self.cmp_deep(fiber, a, b, span) {
                Ok(Some(o)) => Ok(Value::Int(o as i64)),
                Ok(None) => Err(self.panic(fiber, span, "these values are not ordered (a `nan`)")),
                Err(ctl) => Err(ctl),
            },
            (HASH, [a]) => self.hash_deep(fiber, a, span).map(|h| Value::Int(h as i64)),
            (SHOW, [a]) => self
                .show_deep(fiber, a, span)
                .map(|s| Value::Str(Rc::from(s))),
            (CLONE, [a]) => self.clone_deep(fiber, a, span),
            (DROP, [_]) => Ok(Value::Unit),
            (ITERABLE, [v]) => match self.iterator_of(v) {
                Some(it) => Ok(it),
                None => Err(self.internal(fiber, span, "`iter` of something not iterable")),
            },
            _ => Err(self.internal(fiber, span, "a method without an implementation")),
        };
        self.release_all(values);
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
