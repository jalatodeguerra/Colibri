//! Interface dispatch and the user-written implementations that replace derived behaviour
//! (DESIGN §7.4, §9.2; ARCHITECTURE §6).
//!
//! `Callee::Bound` calls find their implementation from the runtime type of the receiver, in
//! `impls[(runtime type, interface)]`; `Callee::Dyn` calls (and `Bound` calls on an interface
//! value) read it from the value. A type that has no explicit implementation of a built-in
//! interface gets the derived one, which this module runs structurally.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

use colibri_check::hir::{CmpOp, Coercion, FnId, ImplDef, Place, Program, TypeKey};
use colibri_check::types::{
    CLONE, DROP, EQ, HASH, INTO_ERROR, ITER, ITERABLE, IntTy, ORD, SHOW, Ty, TypeId,
};
use colibri_syntax::{FileId, Span};

use crate::clone::Cloner;
use crate::compare::{cmp_with, equal_with};
use crate::ctl::{Ctl, Eval};
use crate::fiber::Fiber;
use crate::hooks::{Answer, Overrides, Stop};
use crate::ordered::hash_value;
use crate::show::show_with;
use crate::value::{DynVal, Value};

/// Hooks may nest this deep (a hook that drops an object whose hook drops another, ...).
pub(crate) const MAX_HOOK_DEPTH: u32 = 64;

/// The implementations a program has, and the user-written methods that take over from the
/// derived `Eq`, `Ord`, `Show`, `Clone` and `Drop` of one type.
#[derive(Default)]
pub(crate) struct Tables {
    pub impls: Vec<ImplDef>,
    index: HashMap<(TypeKey, TypeId), u32>,
    eq: HashMap<TypeId, FnId>,
    ord: HashMap<TypeId, FnId>,
    show: HashMap<TypeId, FnId>,
    clone: HashMap<TypeId, FnId>,
    drop: HashMap<TypeId, FnId>,
    /// Some `IntoError` implementation has a `message` method that must run to show an error.
    error_messages: bool,
}

impl Tables {
    pub fn new(program: &Program) -> Self {
        let mut t = Tables {
            impls: program.impls.clone(),
            ..Tables::default()
        };
        for (i, imp) in program.impls.iter().enumerate() {
            t.index.insert((imp.key, imp.iface), i as u32);
            if imp.iface == INTO_ERROR
                && !imp.derived
                && matches!(imp.methods.first(), Some(Some(_)))
            {
                t.error_messages = true;
            }
            let (TypeKey::Adt(ty), Some(Some(f)), false) =
                (imp.key, imp.methods.first(), imp.derived)
            else {
                continue;
            };
            let table = match imp.iface {
                EQ => &mut t.eq,
                ORD => &mut t.ord,
                SHOW => &mut t.show,
                CLONE => &mut t.clone,
                DROP => &mut t.drop,
                _ => continue,
            };
            table.insert(ty, *f);
        }
        t
    }

    /// The user-written `Drop` hook of a class.
    pub fn drop_hook(&self, class: TypeId) -> Option<FnId> {
        self.drop.get(&class).copied()
    }
}

/// The head of a value's type, which interface dispatch works on.
pub(crate) fn runtime_key(v: &Value) -> Option<TypeKey> {
    Some(match v {
        Value::Unit => TypeKey::Unit,
        Value::Bool(_) => TypeKey::Bool,
        // Narrow integers carry no width; `impl` for them is not accepted (check/ifaces.rs).
        Value::Int(_) => TypeKey::Int(IntTy::I64),
        Value::U64(_) => TypeKey::Int(IntTy::U64),
        Value::Float(_) => TypeKey::Float,
        Value::F32(_) => TypeKey::F32,
        Value::Char(_) => TypeKey::Char,
        Value::Str(_) => TypeKey::Str,
        Value::Struct(s) => TypeKey::Adt(s.ty),
        Value::Tuple(_) => TypeKey::Tuple,
        Value::Enum(e) => TypeKey::Adt(e.ty),
        Value::List(_) => TypeKey::List,
        Value::Map(_) => TypeKey::Map,
        Value::Set(_) => TypeKey::Set,
        Value::Obj(o) => TypeKey::Adt(o.ty()),
        Value::Range(_) => TypeKey::Range,
        Value::Weak(_)
        | Value::Dyn(_)
        | Value::Fn(..)
        | Value::Closure(_)
        | Value::Cell(_)
        | Value::Iter(_) => return None,
    })
}

/// The type a user-defined value belongs to.
fn adt_of(v: &Value) -> Option<TypeId> {
    match v {
        Value::Struct(s) => Some(s.ty),
        Value::Enum(e) => Some(e.ty),
        Value::Obj(o) => Some(o.ty()),
        _ => None,
    }
}

/// The interpreter and a fiber, answering [`Overrides`] by running user methods.
pub(crate) struct Hooks<'a> {
    pub interp: &'a mut Interp,
    pub fiber: &'a mut Fiber,
}

use crate::eval::Interp;

impl Hooks<'_> {
    /// Runs the user method `f` on `args`; its result as a value.
    fn run(&mut self, f: FnId, args: Vec<Value>) -> Result<Value, Stop> {
        let Some(body) = self.interp.fns.get(f.0 as usize).cloned() else {
            return Err(Stop::TooDeep);
        };
        self.interp
            .call_body(self.fiber, body, args)
            .map_err(|c| Stop::Ctl(Box::new(c)))
    }
}

impl Overrides for Hooks<'_> {
    fn eq(&mut self, a: &Value, b: &Value) -> Answer<bool> {
        let f = *self.interp.dispatch.eq.get(&adt_of(a)?)?;
        Some(
            self.run(f, vec![a.clone(), b.clone()])
                .map(|v| matches!(v, Value::Bool(true))),
        )
    }

    fn cmp(&mut self, a: &Value, b: &Value) -> Answer<Option<Ordering>> {
        let f = *self.interp.dispatch.ord.get(&adt_of(a)?)?;
        Some(self.run(f, vec![a.clone(), b.clone()]).map(|v| match v {
            Value::Int(i) => Some(i.cmp(&0)),
            _ => None,
        }))
    }

    fn show(&mut self, v: &Value) -> Answer<String> {
        let f = *self.interp.dispatch.show.get(&adt_of(v)?)?;
        Some(self.run(f, vec![v.clone()]).map(|s| match s {
            Value::Str(s) => s.to_string(),
            _ => String::new(),
        }))
    }

    fn clone_value(&mut self, v: &Value) -> Answer<Value> {
        let f = *self.interp.dispatch.clone.get(&adt_of(v)?)?;
        Some(self.run(f, vec![v.clone()]))
    }

    fn error_message(&mut self, value: &Value, impl_id: i64) -> Answer<String> {
        let f = self.interp.error_message_fn(impl_id)?;
        Some(self.run(f, vec![value.clone()]).map(|s| match s {
            Value::Str(s) => s.to_string(),
            _ => String::new(),
        }))
    }
}

impl Interp {
    /// Whether any user-written method replaces a derived one (else the plain functions run).
    fn has_overrides(&self) -> bool {
        let t = &self.dispatch;
        !(t.eq.is_empty()
            && t.ord.is_empty()
            && t.show.is_empty()
            && t.clone.is_empty()
            && !t.error_messages)
    }

    /// Whether any type has a user-written `Eq`.
    pub(crate) fn has_custom_eq(&self) -> bool {
        !self.dispatch.eq.is_empty()
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
        for stored in keys {
            if self.eq_deep(fiber, stored, &key, span)? {
                args[1] = stored.clone();
                break;
            }
        }
        Ok(())
    }

    /// Turns a stop of a structural operation into a panic.
    fn stop_to_ctl(&self, fiber: &mut Fiber, stop: Stop, span: Span, what: &str) -> Ctl {
        match stop {
            Stop::Ctl(ctl) => *ctl,
            Stop::TooDeep => self.panic(
                fiber,
                span,
                format!("these values nest too deeply to {what}"),
            ),
        }
    }

    /// `==` and the ordering operators on values of user-defined or generic type.
    pub(crate) fn structural_cmp(
        &mut self,
        fiber: &mut Fiber,
        op: CmpOp,
        l: &Value,
        r: &Value,
        span: Span,
    ) -> Eval<bool> {
        use Ordering::{Equal, Greater, Less};
        match op {
            CmpOp::Eq | CmpOp::Ne => {
                let eq = self.eq_deep(fiber, l, r, span)?;
                Ok(eq == (op == CmpOp::Eq))
            }
            _ => {
                let ord = self.cmp_deep(fiber, l, r, span)?;
                Ok(match op {
                    CmpOp::Lt => ord == Some(Less),
                    CmpOp::Le => matches!(ord, Some(Less | Equal)),
                    CmpOp::Gt => ord == Some(Greater),
                    _ => matches!(ord, Some(Greater | Equal)),
                })
            }
        }
    }

    /// Equality that honours a user-written `impl Eq` at every level.
    pub(crate) fn eq_deep(
        &mut self,
        fiber: &mut Fiber,
        a: &Value,
        b: &Value,
        span: Span,
    ) -> Eval<bool> {
        let result = if self.has_overrides() {
            equal_with(
                a,
                b,
                &mut Hooks {
                    interp: self,
                    fiber,
                },
            )
        } else {
            equal_with(a, b, &mut crate::hooks::Plain)
        };
        result.map_err(|stop| self.stop_to_ctl(fiber, stop, span, "compare"))
    }

    /// Ordering that honours a user-written `impl Ord` at every level.
    pub(crate) fn cmp_deep(
        &mut self,
        fiber: &mut Fiber,
        a: &Value,
        b: &Value,
        span: Span,
    ) -> Eval<Option<Ordering>> {
        let result = if self.has_overrides() {
            cmp_with(
                a,
                b,
                &mut Hooks {
                    interp: self,
                    fiber,
                },
            )
        } else {
            cmp_with(a, b, &mut crate::hooks::Plain)
        };
        result.map_err(|stop| self.stop_to_ctl(fiber, stop, span, "compare"))
    }

    /// The `Show` text of a value, honouring a user-written `impl Show` at every level.
    pub(crate) fn show_deep(&mut self, fiber: &mut Fiber, v: &Value, span: Span) -> Eval<String> {
        if !self.has_overrides() {
            return Ok(crate::show::show(&self.types, v).into_owned());
        }
        let types = Rc::clone(&self.types);
        let result = show_with(
            &types,
            v,
            &mut Hooks {
                interp: self,
                fiber,
            },
        );
        result.map_err(|stop| self.stop_to_ctl(fiber, stop, span, "show"))
    }

    /// A deep copy honouring a user-written `impl Clone` at every level.
    pub(crate) fn clone_deep(&mut self, fiber: &mut Fiber, v: &Value, span: Span) -> Eval {
        let mut cloner = Cloner::new(fiber.stack_base, self.native_stack_budget());
        let result = if self.has_overrides() {
            cloner.clone_value(
                v,
                &mut Hooks {
                    interp: self,
                    fiber,
                },
            )
        } else {
            cloner.clone_value(v, &mut crate::hooks::Plain)
        };
        match result {
            Ok(copy) => Ok(copy),
            Err(stop) => {
                self.abandon_copies(cloner.into_copies());
                Err(self.stop_to_ctl(fiber, stop, span, "clone"))
            }
        }
    }

    // ---- interface values ----

    /// The implementation of `iface` for the runtime type of `v`, made up (as a derived one)
    /// when the program has none.
    pub(crate) fn impl_for(&mut self, v: &Value, iface: TypeId) -> Option<u32> {
        let key = runtime_key(v)?;
        if let Some(&i) = self.dispatch.index.get(&(key, iface)) {
            return Some(i);
        }
        let methods = self
            .types
            .get(iface.0 as usize)
            .map_or(0, |t| t.iface_methods().len());
        let id = self.dispatch.impls.len() as u32;
        self.dispatch.impls.push(ImplDef {
            iface,
            iface_args: Vec::new(),
            target: Ty::Unit,
            key,
            methods: vec![None; methods],
            derived: true,
            span: Span::new(FileId(0), 0, 0),
        });
        self.dispatch.index.insert((key, iface), id);
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

    /// The function an implementation runs for `method`, if it has one of its own.
    pub(crate) fn target_of(&self, impl_id: Option<u32>, method: u32) -> Option<FnId> {
        self.dispatch
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
                        self.release(v.map_or(Value::Unit, |x| x));
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
            (HASH, [a]) => Ok(Value::Int(hash_value(a) as i64)),
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

    // ---- Drop hooks ----

    /// Runs the `Drop` hook of a class object that is about to die. Returns whether the hook
    /// kept the object alive (resurrection): then it is not destroyed, and its hook never runs
    /// again (ARCHITECTURE §6).
    pub(crate) fn run_drop_hook(&mut self, obj: &Rc<crate::heap::ClassObj>) -> bool {
        let Some(f) = self.dispatch.drop_hook(obj.ty()) else {
            return false;
        };
        if obj.mark_hooked() {
            return false;
        }
        let Some(body) = self.fns.get(f.0 as usize).cloned() else {
            return false;
        };
        if self.hook_depth >= MAX_HOOK_DEPTH {
            self.hook_panics.push(crate::ctl::PanicInfo {
                message: "Drop hooks are nested too deeply".to_string(),
                span: body.span,
                stack: Vec::new(),
                notes: Vec::new(),
                budget: false,
            });
            return false;
        }
        // Past the grace of an exhausted budget, hooks no longer run (the object is released
        // plainly).
        if self.budget_is_dead() {
            return false;
        }
        self.hook_depth += 1;
        let mut hook_fiber = Fiber::new();
        hook_fiber.stack_base = crate::eval::stack_position();
        let result = self.call_body(&mut hook_fiber, body, vec![Value::Obj(Rc::clone(obj))]);
        self.hook_depth -= 1;
        match result {
            Ok(v) | Err(Ctl::Return(v)) => self.release(v),
            Err(Ctl::Panic(info)) => self.hook_panics.push(*info),
            Err(_) => {}
        }
        Rc::strong_count(obj) > 1
    }
}

/// Boxes `val` again as the interface value it was taken from.
fn rewrap(val: Value, impl_id: Option<u32>, was_dyn: bool) -> Value {
    match impl_id {
        Some(impl_id) if was_dyn => Value::Dyn(Rc::new(DynVal { impl_id, val })),
        _ => val,
    }
}
