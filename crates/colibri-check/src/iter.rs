//! Iteration (DESIGN §6.3, §9.2): turning anything iterable into an iterator, and the methods
//! every iterator has.
//!
//! `for x in it` works on anything implementing `Iter[T]` (`fn next(mut self) -> T?`) or
//! `Iterable[T]` (`fn iter(self) -> Iter[T]`), and on the built-in collections and ranges.
//! `Iterable.iter` returns the interface type `Iter[T]`, an interface value (dynamic dispatch),
//! exactly as the DESIGN signature reads. The iterators the built-in collections hand out are
//! interface values of `Iter[T]` too, implemented by the engine, so they mix freely with user
//! iterators.

use colibri_syntax::ast;
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::builtins::iter_of;
use crate::hir::{self, Builtin, Callee, Expr};
use crate::traits::{key_of, match_ty};
use crate::types::{CHANNEL_TYPE, ITER, ITERABLE, ParamId, Ty};

impl FnCtx<'_, '_> {
    /// The item type of a user type, type parameter or interface value that implements `Iter`
    /// or `Iterable`, or of a built-in collection or range.
    pub(crate) fn iter_item_ty(&mut self, ty: &Ty) -> Option<Ty> {
        let ty = self.infer.shallow(ty);
        match &ty {
            Ty::List(t) | Ty::Set(t) => Some((**t).clone()),
            Ty::Map(k, v) => Some(Ty::Tuple(vec![(**k).clone(), (**v).clone()])),
            Ty::Range(_) => Some(Ty::INT),
            Ty::Adt(CHANNEL_TYPE, args) if args.len() == 1 => Some(args[0].clone()),
            Ty::Bytes => Some(crate::builtins::BYTE),
            Ty::Dyn(id, args) if *id == ITER || *id == ITERABLE => args.first().cloned(),
            Ty::Param(p, _) => self
                .ck
                .param_bounds
                .get(p)?
                .iter()
                .find(|b| b.iface == ITER || b.iface == ITERABLE)
                .and_then(|b| b.args.first().cloned()),
            _ => {
                let key = key_of(&ty)?;
                [ITER, ITERABLE].into_iter().find_map(|iface| {
                    let imp = self.ck.impl_by_key.get(&(key, iface))?;
                    let info = &self.ck.impls[imp.0 as usize];
                    let params: Vec<ParamId> = info.generics.iter().map(|g| g.id).collect();
                    let mut subst = Vec::new();
                    match_ty(&info.target, &ty, &params, &mut subst)
                        .then(|| info.iface_args.first().map(|a| a.subst(&subst)))
                        .flatten()
                })
            }
        }
    }

    /// Whether the type implements `Iter` itself (not just `Iterable`).
    fn implements_iter(&mut self, ty: &Ty) -> bool {
        match self.infer.shallow(ty) {
            Ty::Dyn(id, _) => id == ITER,
            Ty::Param(p, _) => self
                .ck
                .param_bounds
                .get(&p)
                .is_some_and(|bs| bs.iter().any(|b| b.iface == ITER)),
            ty => key_of(&ty).is_some_and(|k| self.ck.impl_by_key.contains_key(&(k, ITER))),
        }
    }

    /// The iterator for `value` and its item type: the value itself when it is an `Iter`, the
    /// result of its `iter()` otherwise. `None` after reporting that it is not iterable.
    pub(crate) fn iterator_for(&mut self, value: Expr, span: Span) -> Option<(Expr, Ty)> {
        let ty = self.infer.shallow(&value.ty);
        if ty.is_error() {
            return None;
        }
        let Some(item) = self.iter_item_ty(&ty) else {
            let shown = self.show(&ty);
            self.ck.report(
                Diagnostic::error(format!("cannot loop over a value of type `{shown}`"), span)
                    .with_hint(if ty == Ty::Str {
                        "loop over its characters: `for c in s.chars() { ... }`".to_string()
                    } else {
                        "a `for` loop works on lists, maps, sets, ranges, and any type that implements `Iter[T]` or `Iterable[T]`".to_string()
                    }),
            );
            return None;
        };
        let iter = match &ty {
            Ty::List(_) | Ty::Set(_) | Ty::Map(..) | Ty::Range(_) | Ty::Bytes => {
                self.builtin_call(Builtin::Iter, vec![value], iter_of(item.clone()), span)
            }
            Ty::Adt(CHANNEL_TYPE, _) => self.builtin_call(
                Builtin::ChannelIter,
                vec![value],
                iter_of(item.clone()),
                span,
            ),
            Ty::Dyn(id, _) if *id == ITER => value,
            Ty::Dyn(id, _) if *id == ITERABLE => self.mk(
                hir::ExprKind::Call(Box::new(hir::Call {
                    callee: Callee::Dyn {
                        iface: ITERABLE,
                        method: 0,
                    },
                    type_args: vec![item.clone()],
                    args: vec![hir::Arg::Value(value)],
                })),
                iter_of(item.clone()),
                span,
            ),
            _ if self.implements_iter(&ty) => self.make_dyn(value, ITER, vec![item.clone()]),
            _ => {
                let name = ast::Ident {
                    name: "iter".to_string(),
                    span,
                };
                let mut found = self.method_candidates(&ty, "iter");
                match found.pop() {
                    Some((_, via)) => self.call_via(via, value, &name, span, None, &[], span),
                    None => {
                        self.ck.report(Diagnostic::error(
                            "this type implements `Iterable` but has no `iter` method",
                            span,
                        ));
                        return None;
                    }
                }
            }
        };
        Some((iter, item))
    }

    /// `it.enumerate()` on anything iterable: an iterator of `(index, item)`.
    pub(crate) fn check_enumerate(&mut self, recv: Expr, args: &[ast::Arg], span: Span) -> Expr {
        if !args.is_empty() {
            self.check_args_loosely(args);
            self.ck.report(Diagnostic::error(
                format!("`enumerate` takes no arguments, found {}", args.len()),
                span,
            ));
            return self.error_expr(span);
        }
        let Some((iter, item)) = self.iterator_for(recv, span) else {
            return self.error_expr(span);
        };
        let ty = iter_of(Ty::Tuple(vec![Ty::INT, item]));
        self.builtin_call(Builtin::IterEnumerate, vec![iter], ty, span)
    }
}
