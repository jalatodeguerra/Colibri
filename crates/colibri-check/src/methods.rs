//! Method calls `receiver.name(args)` (DESIGN §9.1, §9.2): which function runs, how it is
//! instantiated and how it is dispatched.
//!
//! A name is looked up in this order (ARCHITECTURE §6): a method of the receiver's own type
//! (an inherent `impl`, which always wins), then a method of a built-in type, then the methods
//! of the interfaces the receiver implements. Two interfaces offering the same name is an
//! ambiguity the programmer resolves by converting the value to one of them with `as`.

use colibri_syntax::ast::{self, TypeKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::builtins::builtin_method_exists;
use crate::call::Receiver;
use crate::checker::Dep;
use crate::checker::ParamInfo;
use crate::hir::{self, Callee, Expr, FnId, Intrinsic};
use crate::infer::VarKind;
use crate::traits::{Source, key_of, match_ty};
use crate::types::{EQ, HASH, ORD, ParamId, Ty, TypeId};

/// One way a method name can be resolved for a receiver.
pub(crate) enum Via {
    /// A function with a body: an inherent method, an impl's method, or an interface's default
    /// (then with the interface arguments the implementation has).
    Fn {
        f: FnId,
        iface_args: Option<Vec<Ty>>,
    },
    /// A method of an interface the receiver is bound by (a type parameter).
    Bound {
        iface: TypeId,
        index: usize,
        args: Vec<Ty>,
    },
    /// A method of the interface the receiver is a value of.
    Dyn {
        iface: TypeId,
        index: usize,
        args: Vec<Ty>,
    },
    /// `eq`, `cmp` or `hash` derived from the structure of the receiver.
    Derived { iface: TypeId },
}

impl FnCtx<'_, '_> {
    /// `base.name(args)`: a call through a type (`Shape.Circle(1)`, `Pair[int, str].new(..)`)
    /// or a method call on a value. `explicit` are the type arguments of the method
    /// (`x.map[int](..)`).
    #[inline(never)]
    pub(crate) fn check_member_call(
        &mut self,
        base: &ast::Expr,
        name: &ast::Ident,
        explicit: Option<Vec<Ty>>,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        if let Some((ty, owner_args)) = self.type_path_expr(base) {
            return self.check_type_member_with(ty, owner_args, explicit, name, args, span);
        }
        let recv = self.check_expr(base, None);
        let mut recv_ty = self.infer.shallow(&recv.ty);
        match &recv_ty {
            Ty::Error => {
                self.check_args_loosely(args);
                return self.error_expr(span);
            }
            Ty::Infer(_) => match self.infer.unbound_kind(&recv_ty) {
                Some(VarKind::IntLit | VarKind::FloatLit) => {
                    recv_ty = self.default_ty(&recv_ty);
                }
                _ => {
                    self.check_args_loosely(args);
                    self.ck.report(
                        Diagnostic::error(
                            format!(
                                "the type of the receiver of `.{}` is not known yet",
                                name.name
                            ),
                            base.span,
                        )
                        .with_hint("give the value a type, for example `let x: List[int] = []`"),
                    );
                    return self.error_expr(span);
                }
            },
            _ => {}
        }
        // `e.is[T]()` and `e.as[T]()` on an `Error` (DESIGN §10.2)
        if matches!(&recv_ty, Ty::Adt(id, _) if *id == crate::types::ERROR_TYPE)
            && matches!(name.name.as_str(), "is" | "as")
        {
            return self.check_error_downcast(recv, name, explicit, args, span);
        }
        // `group.go(call(..))` on a `CoGroup` (DESIGN §14.2)
        if matches!(&recv_ty, Ty::Adt(id, _) if *id == crate::types::COGROUP_TYPE)
            && name.name == "go"
        {
            return self.check_group_go(recv, args, span);
        }
        // 1. an inherent method of a user-defined type
        if let Ty::Adt(id, _) = &recv_ty
            && let Some(&f) = self.ck.methods.get(&(*id, name.name.clone()))
        {
            if !self.ck.fn_visible(f) {
                self.check_args_loosely(args);
                self.ck.report_private("method", &name.name, *id, name.span);
                return self.error_expr(span);
            }
            return self.call_fn_method(f, None, recv, name, base.span, explicit, args, span);
        }
        // 1b. `alive()` of a class of the host
        if name.name == "alive"
            && let Ty::Adt(id, _) = &recv_ty
            && self.ck.types[id.0 as usize].is_host_class()
        {
            return self.check_host_alive(recv, args, span);
        }
        // 2. a method of a number type, or of another built-in type
        if recv_ty.is_number() && crate::numeric::num_method_exists(&recv_ty, &name.name) {
            return self.check_num_method(recv, &recv_ty, name, args, span);
        }
        if name.name != "clone" && builtin_method_exists(&recv_ty, &name.name) {
            return match self.check_builtin_method(recv, name, args, span) {
                Ok(call) | Err(call) => call,
            };
        }
        // 3. a method of an interface
        let mut found = self.method_candidates(&recv_ty, &name.name);
        match found.len() {
            0 => match self.field_call(recv, base, name, args, span) {
                Ok(call) => call,
                Err(recv) => self.no_such_method(recv, &recv_ty, name, args, span),
            },
            1 => {
                let (_, via) = found.remove(0);
                self.call_via(via, recv, name, base.span, explicit, args, span)
            }
            _ => {
                self.check_args_loosely(args);
                let shown = self.show(&recv_ty);
                let names: Vec<String> = found
                    .iter()
                    .map(|(i, _)| format!("`{}`", self.ck.types[i.0 as usize].name))
                    .collect();
                let first = self.ck.types[found[0].0.0 as usize].name.clone();
                self.ck.report(
                    Diagnostic::error(
                        format!(
                            "`{}` is a method of several interfaces that `{shown}` implements: {}",
                            name.name,
                            names.join(", ")
                        ),
                        name.span,
                    )
                    .with_hint(format!(
                        "say which one you mean by converting the value first: `(value as {first}).{}(...)`",
                        name.name
                    )),
                );
                self.error_expr(span)
            }
        }
    }

    /// `Type.name(args)` with the type arguments of the type and of the function.
    fn check_type_member_with(
        &mut self,
        ty: TypeId,
        owner_args: Option<Vec<Ty>>,
        explicit: Option<Vec<Ty>>,
        name: &ast::Ident,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        if let Some(explicit) = explicit
            && let Some(&f) = self.ck.methods.get(&(ty, name.name.clone()))
            && self.ck.fns[f.0 as usize].receiver.is_none()
        {
            let owner_args = match owner_args {
                Some(a) => a,
                None => (0..self.ck.types[ty.0 as usize].params.len())
                    .map(|_| self.fresh_var(VarKind::General))
                    .collect(),
            };
            let owner = Ty::Adt(ty, owner_args);
            return self.check_fn_call_with(f, Some(explicit), Some(owner), args, span);
        }
        self.check_type_member(ty, owner_args, name, Some(args), span)
    }

    /// The interfaces through which `ty` has a method called `name`.
    pub(crate) fn method_candidates(&mut self, ty: &Ty, name: &str) -> Vec<(TypeId, Via)> {
        let mut out: Vec<(TypeId, Via)> = Vec::new();
        let mut ids: Vec<TypeId> = self.ck.ifaces.keys().copied().collect();
        ids.sort();
        let index_of = |this: &Self, iface: TypeId| {
            this.ck
                .ifaces
                .get(&iface)
                .and_then(|i| i.methods.iter().position(|m| m.name == name))
        };
        match ty {
            Ty::Param(p, _) => {
                let bounds = self.ck.param_bounds.get(p).cloned().unwrap_or_default();
                for b in bounds {
                    if out.iter().any(|(i, _)| *i == b.iface) {
                        continue;
                    }
                    if let Some(index) = index_of(self, b.iface) {
                        out.push((
                            b.iface,
                            Via::Bound {
                                iface: b.iface,
                                index,
                                args: b.args,
                            },
                        ));
                    }
                }
                // Being ordered implies being comparable for equality.
                if name == "eq"
                    && !out.iter().any(|(i, _)| *i == EQ)
                    && out.iter().any(|(i, _)| *i == ORD)
                {
                    out.push((
                        EQ,
                        Via::Bound {
                            iface: EQ,
                            index: 0,
                            args: Vec::new(),
                        },
                    ));
                }
            }
            Ty::Dyn(iface, args) => {
                if let Some(index) = index_of(self, *iface) {
                    out.push((
                        *iface,
                        Via::Dyn {
                            iface: *iface,
                            index,
                            args: args.clone(),
                        },
                    ));
                }
            }
            Ty::Error | Ty::Infer(_) | Ty::Never => {}
            _ => {
                for iface in ids {
                    let Some(index) = index_of(self, iface) else {
                        continue;
                    };
                    if let Some(key) = key_of(ty)
                        && let Some(&imp) = self.ck.impl_by_key.get(&(key, iface))
                    {
                        let info = &self.ck.impls[imp.0 as usize];
                        let params: Vec<ParamId> = info.generics.iter().map(|g| g.id).collect();
                        let mut subst = Vec::new();
                        if match_ty(&info.target, ty, &params, &mut subst) {
                            let provided = info.methods.get(index).copied().flatten();
                            let iface_args: Vec<Ty> =
                                info.iface_args.iter().map(|a| a.subst(&subst)).collect();
                            match provided {
                                Some(f) => out.push((
                                    iface,
                                    Via::Fn {
                                        f,
                                        iface_args: None,
                                    },
                                )),
                                None => {
                                    let default = self
                                        .ck
                                        .ifaces
                                        .get(&iface)
                                        .and_then(|i| i.methods.get(index))
                                        .and_then(|m| m.default);
                                    if let Some(f) = default {
                                        out.push((
                                            iface,
                                            Via::Fn {
                                                f,
                                                iface_args: Some(iface_args),
                                            },
                                        ));
                                    }
                                }
                            }
                            continue;
                        }
                    }
                    if matches!(iface, EQ | ORD | HASH)
                        && matches!(
                            self.ck.env().implements(ty, iface, &[]),
                            Ok(Source::Derived)
                        )
                    {
                        out.push((iface, Via::Derived { iface }));
                    }
                }
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn call_via(
        &mut self,
        via: Via,
        recv: Expr,
        name: &ast::Ident,
        base_span: Span,
        explicit: Option<Vec<Ty>>,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        match via {
            Via::Fn { f, iface_args } => {
                self.call_fn_method(f, iface_args, recv, name, base_span, explicit, args, span)
            }
            Via::Derived { iface } => {
                // `a.eq(b)`, `a.cmp(b)`, `a.hash()`: typed from the interface, run by the engine.
                self.call_iface_method(
                    iface,
                    0,
                    Vec::new(),
                    false,
                    recv,
                    name,
                    base_span,
                    explicit,
                    args,
                    span,
                )
            }
            Via::Bound {
                iface,
                index,
                args: iargs,
            } => self.call_iface_method(
                iface, index, iargs, false, recv, name, base_span, explicit, args, span,
            ),
            Via::Dyn {
                iface,
                index,
                args: iargs,
            } => {
                if let Some(why) = self.ck.object_unsafety(iface, index) {
                    self.check_args_loosely(args);
                    let iname = self.ck.types[iface.0 as usize].name.clone();
                    self.ck.report(
                        Diagnostic::error(
                            format!(
                                "`{}` cannot be called on an interface value of `{iname}`",
                                name.name
                            ),
                            name.span,
                        )
                        .with_hint(why),
                    );
                    return self.error_expr(span);
                }
                self.call_iface_method(
                    iface, index, iargs, true, recv, name, base_span, explicit, args, span,
                )
            }
        }
    }

    /// A call of a function with a body: the receiver is its first parameter.
    #[allow(clippy::too_many_arguments)]
    fn call_fn_method(
        &mut self,
        f: FnId,
        iface_args: Option<Vec<Ty>>,
        recv: Expr,
        name: &ast::Ident,
        base_span: Span,
        explicit: Option<Vec<Ty>>,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        self.add_dep(Dep::Fn(f));
        let info = &self.ck.fns[f.0 as usize];
        let (fname, params, ret, receiver, poisoned, generics, host) = (
            info.name.clone(),
            info.params.clone(),
            info.ret.clone(),
            info.receiver,
            info.is_poisoned(),
            info.generics.clone(),
            info.host.clone(),
        );
        if poisoned {
            self.check_args_loosely(args);
            return self.error_expr(span);
        }
        if let Some(host) = &host {
            self.check_host_capability(&fname, host, span);
        }
        let callee = match host {
            Some(host) => Callee::Host(host.id),
            None => Callee::Fn(f),
        };
        let outer = generics.len() - self.own_generic_count(f);
        let Some((vars, map)) = self.instantiate(&generics, outer, explicit, &fname, span) else {
            self.check_args_loosely(args);
            return self.error_expr(span);
        };
        if let Some(iargs) = iface_args {
            for (var, ty) in vars.iter().skip(1).zip(&iargs) {
                let _ = self.infer.unify(var, ty);
            }
        }
        let params: Vec<ParamInfo<'_>> = params
            .into_iter()
            .map(|p| crate::call::subst_param(p, &map))
            .collect();
        let ret = ret.subst(&map);
        if receiver.is_some()
            && let Some(first) = params.first()
            && self.infer.unify(&first.ty, &recv.ty).is_err()
        {
            self.check_args_loosely(args);
            let (want, got) = (self.show(&first.ty), self.show(&recv.ty));
            self.ck.report(
                Diagnostic::error(
                    format!("`{}` is not available for `{got}`", name.name),
                    name.span,
                )
                .with_hint(format!("it is defined only for `{want}`")),
            );
            return self.error_expr(span);
        }
        self.finish_method(
            callee, &fname, receiver, params, ret, vars, recv, name, base_span, args, span,
        )
    }

    /// A call of a method as the interface declares it: through a bound, an interface value
    /// (`dynamic`), or a derived implementation.
    #[allow(clippy::too_many_arguments)]
    fn call_iface_method(
        &mut self,
        iface: TypeId,
        index: usize,
        iface_args: Vec<Ty>,
        dynamic: bool,
        recv: Expr,
        name: &ast::Ident,
        base_span: Span,
        explicit: Option<Vec<Ty>>,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        let Some(info) = self.ck.ifaces.get(&iface) else {
            self.check_args_loosely(args);
            return self.error_expr(span);
        };
        let Some(m) = info.methods.get(index).cloned() else {
            self.check_args_loosely(args);
            return self.error_expr(span);
        };
        let fname = format!("{}.{}", info.name, m.name);
        let skip = 1 + info.generics.len();
        let Some((vars, map)) = self.instantiate(&m.generics, skip, explicit, &fname, span) else {
            self.check_args_loosely(args);
            return self.error_expr(span);
        };
        for (var, ty) in vars.iter().skip(1).zip(&iface_args) {
            let _ = self.infer.unify(var, ty);
        }
        let params: Vec<ParamInfo<'_>> = m
            .params
            .iter()
            .cloned()
            .map(|p| crate::call::subst_param(p, &map))
            .collect();
        let ret = m.ret.subst(&map);
        if let Some(first) = params.first() {
            let _ = self.infer.unify(&first.ty, &recv.ty);
        }
        let method = index as u32;
        let callee = if dynamic {
            Callee::Dyn { iface, method }
        } else {
            Callee::Bound { iface, method }
        };
        self.finish_method(
            callee, &fname, m.receiver, params, ret, vars, recv, name, base_span, args, span,
        )
    }

    /// Matches the arguments of a method call and builds it.
    #[allow(clippy::too_many_arguments)]
    fn finish_method(
        &mut self,
        callee: Callee,
        fname: &str,
        receiver: Option<bool>,
        params: Vec<ParamInfo<'_>>,
        ret: Ty,
        type_args: Vec<Ty>,
        recv: Expr,
        name: &ast::Ident,
        base_span: Span,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        let Some(mutable) = receiver else {
            self.check_args_loosely(args);
            let owner = fname.rsplit_once('.').map_or(fname, |(o, _)| o).to_string();
            self.ck.report(
                Diagnostic::error(
                    format!("`{}` is a function of `{owner}`, not a method", name.name),
                    name.span,
                )
                .with_hint(format!(
                    "call it through the type: `{owner}.{}(...)`",
                    name.name
                )),
            );
            return self.error_expr(span);
        };
        let receiver = if mutable {
            match self.receiver_place(recv, &name.name, base_span) {
                Some(place) => Receiver::Place(place),
                None => {
                    self.check_args_loosely(args);
                    return self.error_expr_typed(span, ret);
                }
            }
        } else {
            Receiver::Value(recv)
        };
        let rest = params.get(1..).unwrap_or_default();
        let Some(bound) = self.bind_args(fname, rest, args, span, 1) else {
            return self.error_expr_typed(span, ret);
        };
        self.finish_call(callee, Some(receiver), bound, ret, type_args, span)
    }

    /// `object.alive()`: whether the host still has the object.
    fn check_host_alive(&mut self, recv: Expr, args: &[ast::Arg], span: Span) -> Expr {
        if !args.is_empty() {
            self.check_args_loosely(args);
            self.ck.report(Diagnostic::error(
                format!("`alive` takes no arguments, found {}", args.len()),
                span,
            ));
            return self.error_expr(span);
        }
        let call = hir::ExprKind::Call(Box::new(hir::Call {
            callee: Callee::Intrinsic(Intrinsic::HostAlive),
            type_args: Vec::new(),
            args: vec![hir::Arg::Value(recv)],
        }));
        self.mk(call, Ty::Bool, span)
    }

    /// No method of that name was found: `clone` and `show` every type has, anything else is
    /// an error.
    fn no_such_method(
        &mut self,
        recv: Expr,
        recv_ty: &Ty,
        name: &ast::Ident,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        match name.name.as_str() {
            "clone" if self.is_host_class_ty(recv_ty) => {
                self.check_args_loosely(args);
                let shown = self.show(recv_ty);
                self.ck.report(
                    Diagnostic::error(format!("`{shown}` is a host class and cannot be cloned"), name.span)
                        .with_hint("the object lives in the host program and there is only one of it; keep the reference, or ask the host for another object"),
                );
                return self.error_expr(span);
            }
            "clone" => return self.check_clone(recv, args, span),
            "show" if args.is_empty() => {
                let call = hir::ExprKind::Call(Box::new(hir::Call {
                    callee: Callee::Intrinsic(Intrinsic::Show),
                    type_args: Vec::new(),
                    args: vec![hir::Arg::Value(recv)],
                }));
                return self.mk(call, Ty::Str, span);
            }
            "enumerate" if self.iter_item_ty(recv_ty).is_some() => {
                return self.check_enumerate(recv, args, span);
            }
            _ => {}
        }
        if recv_ty.is_number() {
            // Reports the missing method together with what the type does have.
            return self.check_num_method(recv, recv_ty, name, args, span);
        }
        let recv = match self.check_builtin_method(recv, name, args, span) {
            Ok(call) => return call,
            Err(recv) => recv,
        };
        drop(recv);
        self.check_args_loosely(args);
        let shown = self.show(recv_ty);
        match recv_ty {
            Ty::Adt(id, _) => {
                let tname = self.ck.types[id.0 as usize].name.clone();
                let mut methods: Vec<String> = self
                    .ck
                    .methods
                    .keys()
                    .filter(|(t, _)| t == id)
                    .map(|(_, n)| n.clone())
                    .collect();
                methods.extend(self.interface_method_names(recv_ty));
                let mut diag = Diagnostic::error(
                    format!("`{tname}` has no method `{}`", name.name),
                    name.span,
                );
                if let Some(hint) = crate::checker::Checker::did_you_mean(&name.name, &methods) {
                    diag = diag.with_hint(hint);
                } else if self.ck.types[id.0 as usize]
                    .fields()
                    .iter()
                    .any(|f| f.name == name.name)
                {
                    diag = diag.with_hint(format!("`{}` is a field, not a method", name.name));
                }
                self.ck.report(diag);
            }
            Ty::Param(..) => {
                let mut diag = Diagnostic::error(
                    format!("`{shown}` has no method `{}`", name.name),
                    name.span,
                );
                diag = diag.with_hint(format!(
                    "inside a generic body only the methods of the bounds are known; add a bound that has `{}`: `{shown}: Interface`",
                    name.name
                ));
                self.ck.report(diag);
            }
            Ty::Dyn(iface, _) => {
                let iname = self.ck.types[iface.0 as usize].name.clone();
                let names: Vec<String> = self
                    .ck
                    .ifaces
                    .get(iface)
                    .map(|i| i.methods.iter().map(|m| m.name.clone()).collect())
                    .unwrap_or_default();
                let hint = crate::checker::Checker::did_you_mean(&name.name, &names)
                    .unwrap_or_else(|| format!("`{iname}` has the methods: {}", names.join(", ")));
                self.ck.report(
                    Diagnostic::error(
                        format!("the interface `{iname}` has no method `{}`", name.name),
                        name.span,
                    )
                    .with_hint(hint),
                );
            }
            other => {
                let shown = self.show(other);
                self.ck.report(
                    Diagnostic::error(
                        format!("methods on the built-in type `{shown}` are not supported yet"),
                        name.span,
                    )
                    .with_hint(
                        "Colibri is being built in slices; see docs/PHASE1.md for what comes next",
                    ),
                );
            }
        }
        self.error_expr(span)
    }

    /// The names of the interface methods a type has, for "did you mean".
    fn interface_method_names(&self, ty: &Ty) -> Vec<String> {
        let Some(key) = key_of(ty) else {
            return Vec::new();
        };
        self.ck
            .impl_by_key
            .iter()
            .filter(|((k, _), _)| *k == key)
            .filter_map(|((_, iface), _)| self.ck.ifaces.get(iface))
            .flat_map(|i| i.methods.iter().map(|m| m.name.clone()))
            .collect()
    }

    /// A type written in an expression (`value as Shape`, `Pair { .. }`, `f[T]`): a generic
    /// type named without arguments gets a fresh variable for each.
    pub(crate) fn resolve_type_infer(&mut self, t: &ast::TypeExpr) -> Ty {
        if let TypeKind::Named { path, args } = &t.kind
            && args.is_empty()
            && (path.len() == 1 || self.ck.mod_aliases.contains_key(&path[0].name))
            && self.ck.scoped_param(&path[0].name).is_none()
            && let Some(&id) = self.ck.type_ids.get(
                &path
                    .iter()
                    .map(|i| i.name.as_str())
                    .collect::<Vec<_>>()
                    .join("."),
            )
        {
            let def = &self.ck.types[id.0 as usize];
            if !def.is_interface() && !def.params.is_empty() {
                let n = def.params.len();
                let vars = (0..n).map(|_| self.fresh_var(VarKind::General)).collect();
                return Ty::Adt(id, vars);
            }
        }
        self.ck.resolve_type(t)
    }
}
