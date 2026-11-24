//! The interface implementations of the loaded program, and the user-written methods that take
//! over from the derived `Eq`, `Ord`, `Show`, `Clone` and `Drop` of one type (DESIGN §7.4,
//! §9.2; ARCHITECTURE §6).

use std::collections::HashMap;
use std::rc::Rc;

use colibri_check::hir::{FnId, ImplDef, Program, TypeKey};
use colibri_check::types::{
    CLONE, DROP, EQ, HASH, INTO_ERROR, ITERABLE, IntTy, ORD, SHOW, Ty, TypeId, is_operator,
};
use colibri_syntax::{FileId, Span};

use crate::ctl::{Ctl, Eval};
use crate::deep::Deep;
use crate::engine::Engine;
use crate::host::HostHook;
use crate::iter::IterOps;
use crate::ordered::{hash_value, hash_value_with};
use crate::protocol::Protocol;
use crate::release::Release;
use crate::value::Value;

/// The implementations a program has, and the user-written methods that take over from the
/// derived `Eq`, `Ord`, `Show`, `Clone` and `Drop` of one type.
#[derive(Default)]
#[doc(hidden)]
pub struct Tables {
    pub(crate) impls: Vec<ImplDef>,
    pub(crate) index: HashMap<(TypeKey, TypeId), u32>,
    /// The impls of `Add`, `Sub`, `Mul`, `Div` and `Neg`, by left type, interface and the head
    /// of the right-hand type (`None` when it is a type parameter, or for `Neg`).
    operators: HashMap<(TypeKey, TypeId, Option<TypeKey>), u32>,
    pub(crate) eq: HashMap<TypeId, FnId>,
    pub(crate) ord: HashMap<TypeId, FnId>,
    pub(crate) show: HashMap<TypeId, FnId>,
    pub(crate) clone: HashMap<TypeId, FnId>,
    pub(crate) drop: HashMap<TypeId, FnId>,
    /// Some `IntoError` implementation has a `message` method that must run to show an error.
    pub(crate) error_messages: bool,
}

impl Tables {
    pub fn new(program: &Program) -> Self {
        let mut t = Tables {
            impls: program.impls.clone(),
            ..Tables::default()
        };
        for (i, imp) in program.impls.iter().enumerate() {
            t.index.insert((imp.key, imp.iface), i as u32);
            if is_operator(imp.iface) && !imp.derived {
                t.operators.insert((imp.key, imp.iface, imp.rhs), i as u32);
            }
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

    /// The index of the user impl of operator interface `iface` for the left type `left` and
    /// the right-hand type head `right` (`None` for a type parameter, or for `Neg`).
    #[inline]
    pub(crate) fn operator_impl(
        &self,
        left: TypeKey,
        iface: TypeId,
        right: Option<TypeKey>,
    ) -> Option<u32> {
        self.operators.get(&(left, iface, right)).copied()
    }

    /// The implementation with index `id`.
    #[inline]
    pub fn impl_def(&self, id: u32) -> Option<&ImplDef> {
        self.impls.get(id as usize)
    }

    /// The user-written `Drop` hook of a class.
    #[inline]
    pub(crate) fn drop_hook(&self, class: TypeId) -> Option<FnId> {
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

/// Interface dispatch that needs no engine internals: finding implementations, the lookup of
/// map and set keys by a user-written `Eq`, and the derived methods of the built-in
/// interfaces. Running a found method (`call_interface`, `call_operator`) stays with the
/// engine, which owns the calling convention and the places a `mut self` receiver lives in.
#[doc(hidden)]
pub trait Dispatch: Engine {
    /// A `Map` key or `Set` element is looked up by the user-written `Eq` of its type: the
    /// key in `args[1]` is replaced by the stored key that `eq` says is equal to it, so the
    /// structural lookup finds it. This scans the keys; a user `Hash` is not consulted (it
    /// cannot be observed, ARCHITECTURE §6).
    fn canonicalise_key(
        &mut self,
        fiber: &mut Self::Fiber,
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
        self.core_mut().key_scans += 1;
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
    fn canonical_key(
        &mut self,
        fiber: &mut Self::Fiber,
        stored: impl FnOnce() -> Vec<Value>,
        key: Value,
        span: Span,
    ) -> Eval<Value> {
        if !self.key_needs_scan(&key) {
            return Ok(key);
        }
        self.core_mut().key_scans += 1;
        for candidate in stored() {
            self.step(fiber, span)?;
            if self.eq_deep(fiber, &candidate, &key, span)? {
                return Ok(candidate);
            }
        }
        Ok(key)
    }

    /// The implementation of `iface` for the runtime type of `v`, made up (as a derived one)
    /// when the program has none.
    fn impl_for(&mut self, v: &Value, iface: TypeId) -> Option<u32> {
        let key = runtime_key(v)?;
        let core = self.core_mut();
        if let Some(&i) = core.dispatch.index.get(&(key, iface)) {
            return Some(i);
        }
        let methods = core
            .types
            .get(iface.0 as usize)
            .map_or(0, |t| t.iface_methods().len());
        let id = core.dispatch.impls.len() as u32;
        core.dispatch.impls.push(ImplDef {
            iface,
            iface_args: Vec::new(),
            target: Ty::Unit,
            key,
            rhs: None,
            methods: vec![None; methods],
            derived: true,
            span: Span::new(FileId(0), 0, 0),
        });
        core.dispatch.index.insert((key, iface), id);
        Some(id)
    }

    /// The function an implementation runs for `method`, if it has one of its own.
    fn target_of(&self, impl_id: Option<u32>, method: u32) -> Option<FnId> {
        self.core()
            .dispatch
            .impls
            .get(impl_id? as usize)?
            .methods
            .get(method as usize)
            .copied()
            .flatten()
    }

    /// The function that implements operator `iface` for these operands, if the program has a
    /// user impl: found from the runtime types, `rhs` being the right-hand head when the bound
    /// fixes it. Two primitives of one kind never have one, since no impl can replace the
    /// built-in operation (the orphan rule), and none is looked up.
    fn operator_target(
        &self,
        iface: TypeId,
        rhs: Option<TypeKey>,
        values: &[Value],
    ) -> Option<FnId> {
        let left = values.first().and_then(runtime_key);
        let right = rhs.or_else(|| values.get(1).and_then(runtime_key));
        let primitives = matches!(
            (values.first(), values.get(1)),
            (Some(Value::Int(_)), Some(Value::Int(_)))
                | (Some(Value::U64(_)), Some(Value::U64(_)))
                | (Some(Value::Float(_)), Some(Value::Float(_)))
                | (Some(Value::F32(_)), Some(Value::F32(_)))
                | (Some(Value::Str(_)), Some(Value::Str(_)))
        );
        let found = left.filter(|_| !primitives).and_then(|k| {
            let tables = &self.core().dispatch;
            tables
                .operator_impl(k, iface, right)
                .or_else(|| tables.operator_impl(k, iface, None))
        });
        self.target_of(found, 0)
    }

    /// The `hash()` of a value. Equal values hash equally, which for an object of a host class
    /// that customises `==` means its own hash: without one there is no hash that agrees with
    /// `==`, so asking for it is a panic, not a wrong answer. (Map and set lookups of such
    /// keys scan with `==` and never use this.)
    fn hash_deep(&mut self, fiber: &mut Self::Fiber, v: &Value, span: Span) -> Eval<u64> {
        if !self.core().host.any_eq_hook || !self.key_needs_scan(v) {
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
            let hooks = self.core().host_hook_set(r.ty)?;
            if !hooks.eq {
                return None;
            }
            if !hooks.hash {
                let name = self.core().types[r.ty.0 as usize].name.clone();
                failed = Some(self.panic(
                    fiber,
                    span,
                    format!(
                        "`{name}` compares with its own `==` but gives no hash, so `hash()` cannot agree with `==`"
                    ),
                ));
                return None;
            }
            let outcome = self.step(fiber, span).and_then(|()| {
                self.call_host_hook(fiber, r.ty, HostHook::Hash, vec![object.clone()])
                    .map_err(|f| self.host_fault_ctl(fiber, f))
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
        fiber: &mut Self::Fiber,
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

impl<E: Engine> Dispatch for E {}
