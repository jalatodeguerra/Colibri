//! The deep operations: equality, ordering, `Show` and `clone` of values that honour a
//! user-written `impl Eq`, `Ord`, `Show` or `Clone` at every level (DESIGN §7.4).
//!
//! The structural functions ([`equal_with`], [`cmp_with`], [`show_with`], [`Cloner`]) know
//! nothing of an engine; they ask an [`Overrides`] first. [`Hooks`] answers by running the
//! user's method (or the host class's hook) through the [`Engine`].

use std::cmp::Ordering;
use std::rc::Rc;

use colibri_check::hir::{CmpOp, FnId};
use colibri_check::types::TypeId;
use colibri_syntax::{FileId, Span};

use crate::clone::Cloner;
use crate::compare::{Guard, cmp_with, equal_with};
use crate::ctl::{Ctl, Eval};
use crate::engine::Engine;
use crate::hooks::{Answer, Overrides, Plain, Stop};
use crate::host::HostHook;
use crate::protocol::Protocol;
use crate::release::Release;
use crate::show::show_with;
use crate::value::Value;

/// The type a user-defined value belongs to.
fn adt_of(v: &Value) -> Option<TypeId> {
    match v {
        Value::Struct(s) => Some(s.ty),
        Value::Enum(e) => Some(e.ty),
        Value::Obj(o) => Some(o.ty()),
        Value::Host(r) => Some(r.ty),
        _ => None,
    }
}

/// An engine and a fiber, answering [`Overrides`] by running user methods.
#[doc(hidden)]
pub struct Hooks<'a, E: Engine> {
    pub engine: &'a mut E,
    pub fiber: &'a mut E::Fiber,
    /// Where the operation that runs the hooks stands, for the steps they cost.
    pub span: Span,
}

impl<E: Engine> Hooks<'_, E> {
    /// Runs the `show` or `eq` hook of the host class of `v`, if it has one.
    fn host_hook(
        &mut self,
        v: &Value,
        hook: HostHook,
        args: Vec<Value>,
    ) -> Option<Result<Value, Stop>> {
        let Value::Host(r) = v else {
            return None;
        };
        if !self.engine.core().host_hook_set(r.ty)?.has(hook) {
            return None;
        }
        // A host hook is a call like any other: a panic in it shows the line it was called
        // from, and it costs one step (EMBEDDING §5).
        if self.span != Span::new(FileId(0), 0, 0) {
            E::note_position(self.fiber, self.span);
        }
        if let Err(ctl) = self.engine.step(self.fiber, self.span) {
            return Some(Err(Stop::Ctl(Box::new(ctl))));
        }
        Some(
            match self.engine.call_host_hook(self.fiber, r.ty, hook, args) {
                Ok(value) => Ok(value),
                Err(fault) => Err(Stop::Ctl(Box::new(
                    self.engine.host_fault_ctl(self.fiber, fault),
                ))),
            },
        )
    }

    /// Runs the user method `f` on `args`; its result as a value.
    fn run(&mut self, f: FnId, args: Vec<Value>) -> Result<Value, Stop> {
        if self.engine.fn_span(f).is_none() {
            return Err(Stop::TooDeep);
        }
        // A call like any other: a panic in the method shows the line it was called from.
        // (A hook run for the host, with no source position, leaves the frame's alone.)
        if self.span != Span::new(FileId(0), 0, 0) {
            E::note_position(self.fiber, self.span);
        }
        self.engine
            .call_fn(self.fiber, f, args)
            .map_err(|c| Stop::Ctl(Box::new(c)))
    }
}

impl<E: Engine> Overrides for Hooks<'_, E> {
    fn eq(&mut self, a: &Value, b: &Value) -> Answer<bool> {
        if let Some(answer) = self.host_hook(a, HostHook::Eq, vec![a.clone(), b.clone()]) {
            return Some(answer.map(|v| matches!(v, Value::Bool(true))));
        }
        let f = *self.engine.core().dispatch.eq.get(&adt_of(a)?)?;
        Some(
            self.run(f, vec![a.clone(), b.clone()])
                .map(|v| matches!(v, Value::Bool(true))),
        )
    }

    fn cmp(&mut self, a: &Value, b: &Value) -> Answer<Option<Ordering>> {
        let f = *self.engine.core().dispatch.ord.get(&adt_of(a)?)?;
        Some(self.run(f, vec![a.clone(), b.clone()]).map(|v| match v {
            Value::Int(i) => Some(i.cmp(&0)),
            _ => None,
        }))
    }

    fn show(&mut self, v: &Value) -> Answer<String> {
        if let Some(answer) = self.host_hook(v, HostHook::Show, vec![v.clone()]) {
            return Some(answer.map(|s| match s {
                Value::Str(s) => s.to_string(),
                _ => String::new(),
            }));
        }
        let f = *self.engine.core().dispatch.show.get(&adt_of(v)?)?;
        Some(self.run(f, vec![v.clone()]).map(|s| match s {
            Value::Str(s) => s.to_string(),
            _ => String::new(),
        }))
    }

    fn clone_value(&mut self, v: &Value) -> Answer<Value> {
        let f = *self.engine.core().dispatch.clone.get(&adt_of(v)?)?;
        Some(self.run(f, vec![v.clone()]))
    }

    fn error_message(&mut self, value: &Value, impl_id: i64) -> Answer<String> {
        let f = self.engine.error_message_fn(impl_id)?;
        Some(self.run(f, vec![value.clone()]).map(|s| match s {
            Value::Str(s) => s.to_string(),
            _ => String::new(),
        }))
    }
}

/// Equality, ordering, `Show` and `clone` through the user's implementations.
#[doc(hidden)]
pub trait Deep: Engine {
    /// Whether any user-written method replaces a derived one (else the plain functions run).
    fn has_overrides(&self) -> bool {
        let core = self.core();
        let t = &core.dispatch;
        !(t.eq.is_empty()
            && t.ord.is_empty()
            && t.show.is_empty()
            && t.clone.is_empty()
            && !t.error_messages
            && !core.host.any_show_hook
            && !core.host.any_eq_hook)
    }

    /// The function the `IntoError` implementation `impl_id` runs for `message`, if it has a
    /// user-written one.
    fn error_message_fn(&self, impl_id: i64) -> Option<FnId> {
        let imp = self
            .core()
            .dispatch
            .impls
            .get(usize::try_from(impl_id).ok()?)?;
        if imp.derived {
            return None;
        }
        imp.methods.first().copied().flatten()
    }

    /// Whether a key like `v` may be compared by an `Eq` that structure and identity do not
    /// give: it holds (at any depth, also inside a list, an interface value, ...) a value
    /// whose type has a user-written `impl Eq`, or an object of a host class that customises
    /// `==`. Such keys are found by scanning, see `canonicalise_key`; every other key takes
    /// the plain hashed path.
    ///
    /// The answer comes from the key's own values, so generics are exact (`Pair { a: 1 }` is
    /// plain even if a `Pair[Q]` with `impl Eq for Q` exists) and a later `impl Eq` (hot
    /// reload, the REPL) is seen at once: the tables it reads are rebuilt on load.
    fn key_needs_scan(&self, v: &Value) -> bool {
        let core = self.core();
        (!core.dispatch.eq.is_empty() || core.host.any_eq_hook) && self.reaches_eq(v, 0)
    }

    fn reaches_eq(&self, v: &Value, depth: u32) -> bool {
        if depth > 64 {
            return true;
        }
        let d = depth + 1;
        let core = self.core();
        let user_eq = |ty: &TypeId| core.dispatch.eq.contains_key(ty);
        match v {
            Value::Host(r) => {
                user_eq(&r.ty) || core.host_hook_set(r.ty).is_some_and(|hooks| hooks.eq)
            }
            Value::Obj(o) => user_eq(&o.ty()),
            Value::Struct(s) => user_eq(&s.ty) || s.fields.iter().any(|x| self.reaches_eq(x, d)),
            Value::Enum(e) => user_eq(&e.ty) || e.payload.iter().any(|x| self.reaches_eq(x, d)),
            Value::Tuple(items) => items.iter().any(|x| self.reaches_eq(x, d)),
            Value::Dyn(dv) => self.reaches_eq(&dv.val, d),
            Value::List(l) => l.snapshot().iter().any(|x| self.reaches_eq(x, d)),
            Value::Set(set) => set.snapshot().iter().any(|x| self.reaches_eq(x, d)),
            Value::Map(m) => m
                .snapshot()
                .iter()
                .any(|(k, x)| self.reaches_eq(k, d) || self.reaches_eq(x, d)),
            _ => false,
        }
    }

    /// Turns a stop of a structural operation into a panic.
    fn stop_to_ctl(&self, fiber: &mut Self::Fiber, stop: Stop, span: Span, what: &str) -> Ctl {
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
    fn structural_cmp(
        &mut self,
        fiber: &mut Self::Fiber,
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
    fn eq_deep(&mut self, fiber: &mut Self::Fiber, a: &Value, b: &Value, span: Span) -> Eval<bool> {
        let guard = Guard::new(Self::stack_base(fiber), self.native_stack_budget());
        let result = if self.has_overrides() {
            equal_with(
                a,
                b,
                &guard,
                &mut Hooks {
                    engine: self,
                    fiber,
                    span,
                },
            )
        } else {
            equal_with(a, b, &guard, &mut Plain)
        };
        result.map_err(|stop| self.stop_to_ctl(fiber, stop, span, "compare"))
    }

    /// Ordering that honours a user-written `impl Ord` at every level.
    fn cmp_deep(
        &mut self,
        fiber: &mut Self::Fiber,
        a: &Value,
        b: &Value,
        span: Span,
    ) -> Eval<Option<Ordering>> {
        let guard = Guard::new(Self::stack_base(fiber), self.native_stack_budget());
        let result = if self.has_overrides() {
            cmp_with(
                a,
                b,
                &guard,
                &mut Hooks {
                    engine: self,
                    fiber,
                    span,
                },
            )
        } else {
            cmp_with(a, b, &guard, &mut Plain)
        };
        result.map_err(|stop| self.stop_to_ctl(fiber, stop, span, "compare"))
    }

    /// The `Show` text of a value, honouring a user-written `impl Show` at every level.
    fn show_deep(&mut self, fiber: &mut Self::Fiber, v: &Value, span: Span) -> Eval<String> {
        if !self.has_overrides() {
            return Ok(crate::show::show(&self.core().types, v).into_owned());
        }
        let types = Rc::clone(&self.core().types);
        let result = show_with(
            &types,
            v,
            &mut Hooks {
                engine: self,
                fiber,
                span,
            },
        );
        result.map_err(|stop| self.stop_to_ctl(fiber, stop, span, "show"))
    }

    /// Appends the `Show` text of a value to `out`: strings and integers are written straight
    /// in, so building a string from parts makes no temporary `String` for them.
    fn show_deep_into(
        &mut self,
        fiber: &mut Self::Fiber,
        v: &Value,
        span: Span,
        out: &mut String,
    ) -> Eval<()> {
        match v {
            Value::Str(s) => out.push_str(s),
            Value::Int(i) => crate::show::push_int(out, *i),
            _ => out.push_str(&self.show_deep(fiber, v, span)?),
        }
        Ok(())
    }

    /// The `Show` text of a value as a `str` value: a string is shared, an integer is formatted
    /// on the stack and copied once into its final allocation.
    fn show_deep_rc(&mut self, fiber: &mut Self::Fiber, v: &Value, span: Span) -> Eval<Rc<str>> {
        Ok(match v {
            Value::Str(s) => Rc::clone(s),
            Value::Int(i) => Rc::from(crate::show::int_text(&mut [0; 20], *i)),
            _ => Rc::from(self.show_deep(fiber, v, span)?),
        })
    }

    /// A deep copy honouring a user-written `impl Clone` at every level.
    fn clone_deep(&mut self, fiber: &mut Self::Fiber, v: &Value, span: Span) -> Eval {
        let mut cloner = Cloner::new(Self::stack_base(fiber), self.native_stack_budget());
        let result = if self.has_overrides() {
            cloner.clone_value(
                v,
                &mut Hooks {
                    engine: self,
                    fiber,
                    span,
                },
            )
        } else {
            cloner.clone_value(v, &mut Plain)
        };
        match result {
            Ok(copy) => {
                if self.core().leaks.is_some() {
                    for made in cloner.into_copies() {
                        if let Value::Obj(obj) = &made {
                            self.core_mut().track_object(obj, Some(span));
                        }
                    }
                }
                Ok(copy)
            }
            Err(stop) => {
                self.abandon_copies(cloner.into_copies());
                Err(self.stop_to_ctl(fiber, stop, span, "clone"))
            }
        }
    }
}

impl<E: Engine> Deep for E {}
