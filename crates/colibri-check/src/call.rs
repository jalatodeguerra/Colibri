//! Checking calls: resolving the callee, matching arguments to parameters (named and default
//! arguments become positional), and keeping evaluation order stable.

use colibri_syntax::ast::{self, ExprKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::body::GenericCall;
use crate::checker::{Checker, Dep, ParamInfo};
use crate::hir::{self, Arg, Block, Callee, Expr, FnId, Intrinsic, LocalId, Place, Stmt, StmtKind};
use crate::infer::VarKind;
use crate::obligations::{Obligation, Origin};
use crate::place::Action;
use crate::traits::GenericParamInfo;
use crate::types::{ParamId, Ty};

/// An argument matched to a parameter.
pub(crate) struct Bound {
    pub expr: Expr,
    /// The position in the source for an explicit argument; `None` for a default.
    pub source: Option<usize>,
}

/// The fresh type arguments of a call and the substitution that maps parameters to them.
pub(crate) type Instance = (Vec<Ty>, Vec<(ParamId, Ty)>);

impl FnCtx<'_, '_> {
    #[inline(never)]
    pub fn check_call(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::Arg],
        expected: Option<&Ty>,
        span: Span,
    ) -> Expr {
        if let Some(built) = self.check_constructor(callee, args, expected, span) {
            return built;
        }
        let id = match &callee.kind {
            ExprKind::Ident(id) => id,
            ExprKind::Field { base, name } => {
                return self.check_member_call(base, name, None, args, span);
            }
            ExprKind::Paren(inner) if matches!(inner.kind, ExprKind::Ident(_)) => {
                return self.check_call(inner, args, expected, span);
            }
            ExprKind::Index {
                base,
                args: type_args,
            } => return self.check_instantiated_call(base, type_args, args, span),
            _ => {
                self.check_args_loosely(args);
                return self.unsupported("calling anything but a named function", callee.span);
            }
        };
        let name = id.name.as_str();
        if let Some(local) = self.lookup_local(name) {
            let shown = self.show(&self.local_ty(local));
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!("`{name}` is a `{shown}`, not a function"),
                    callee.span,
                )
                .with_hint("only declared functions can be called so far"),
            );
            return self.error_expr(span);
        }
        if let Some(f) = self.lookup_fn(name) {
            return self.check_fn_call(f, args, span);
        }
        match name {
            "print" if !self.ck.values.contains_key(name) => self.check_print(args, span),
            "some" => self.check_some(args, expected, span),
            "ok" | "err" => {
                self.check_args_loosely(args);
                self.unsupported(&format!("`{name}`"), callee.span)
            }
            _ => {
                self.check_args_loosely(args);
                if self.ck.values.contains_key(name) {
                    self.ck.report(Diagnostic::error(
                        format!("`{name}` is a value, not a function"),
                        callee.span,
                    ));
                } else {
                    self.report_unknown_name(name, callee.span);
                }
                self.error_expr(span)
            }
        }
    }

    /// Checks arguments of a call that cannot be resolved, so their own errors still show.
    pub(crate) fn check_args_loosely(&mut self, args: &[ast::Arg]) {
        for a in args {
            self.check_expr(&a.value, None);
        }
    }

    fn check_print(&mut self, args: &[ast::Arg], span: Span) -> Expr {
        if args.len() != 1 || args[0].name.is_some() {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!("`print` takes exactly one argument, found {}", args.len()),
                    span,
                )
                .with_hint(
                    "to print several values, use string interpolation: `print(\"{a} {b}\")`",
                ),
            );
            return self.error_expr(span);
        }
        let value = self.check_expr(&args[0].value, None);
        let ty = self.default_ty(&value.ty);
        if !matches!(
            ty,
            Ty::Int(_)
                | Ty::Float
                | Ty::Bool
                | Ty::Char
                | Ty::Str
                | Ty::Unit
                | Ty::Error
                | Ty::Tuple(_)
                | Ty::Adt(..)
                | Ty::Dyn(..)
                | Ty::Param(..)
                | Ty::List(_)
                | Ty::Map(..)
                | Ty::Set(_)
                | Ty::Option(_)
        ) {
            let shown = self.show(&ty);
            self.ck.report(Diagnostic::error(
                format!("a value of type `{shown}` cannot be printed"),
                value.span,
            ));
            return self.error_expr(span);
        }
        self.mk(
            hir::ExprKind::Call(Box::new(hir::Call {
                callee: Callee::Intrinsic(Intrinsic::Print),
                type_args: Vec::new(),
                args: vec![Arg::Value(value)],
            })),
            Ty::Unit,
            span,
        )
    }

    pub(crate) fn check_fn_call(&mut self, f: FnId, args: &[ast::Arg], span: Span) -> Expr {
        self.check_fn_call_with(f, None, None, args, span)
    }

    /// A call of a function. `explicit` gives the type arguments written at the call
    /// (`max[f32](a, b)`), `owner` the type written before a static function
    /// (`Pair[int, str].new(..)`); anything not given is inferred (DESIGN §5.4).
    pub(crate) fn check_fn_call_with(
        &mut self,
        f: FnId,
        explicit: Option<Vec<Ty>>,
        owner: Option<Ty>,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        self.add_dep(Dep::Fn(f));
        let info = &self.ck.fns[f.0 as usize];
        let (name, params, ret, poisoned, generics, self_ty) = (
            info.name.clone(),
            info.params.clone(),
            info.ret.clone(),
            info.is_poisoned(),
            info.generics.clone(),
            info.self_ty.clone(),
        );
        if poisoned {
            self.check_args_loosely(args);
            return self.error_expr(span);
        }
        let outer = generics.len() - self.own_generic_count(f);
        let Some((type_args, map)) = self.instantiate(&generics, outer, explicit, &name, span)
        else {
            self.check_args_loosely(args);
            return self.error_expr(span);
        };
        if let (Some(owner), Some(self_ty)) = (&owner, &self_ty) {
            let _ = self.infer.unify(&self_ty.subst(&map), owner);
        }
        let params: Vec<ParamInfo<'_>> = params.into_iter().map(|p| subst_param(p, &map)).collect();
        let ret = ret.subst(&map);
        let Some(bound) = self.bind_args(&name, &params, args, span, 0) else {
            return self.error_expr_typed(span, ret);
        };
        self.finish_call(Callee::Fn(f), None, bound, ret, type_args, span)
    }

    /// How many type parameters function `f` declares itself (not counting its `impl`'s).
    pub(crate) fn own_generic_count(&self, f: FnId) -> usize {
        let info = &self.ck.fns[f.0 as usize];
        match info.decl {
            Some(d) => d.generics.len().min(info.generics.len()),
            None => 0,
        }
    }

    /// Fresh inference variables for `generics`, the substitution that maps each parameter to
    /// its variable, and the requirements the parameters' bounds make. The first `skip`
    /// parameters belong to the enclosing `impl` and are never given explicitly. `None` after
    /// reporting a wrong number of explicit type arguments.
    pub(crate) fn instantiate(
        &mut self,
        generics: &[GenericParamInfo],
        skip: usize,
        explicit: Option<Vec<Ty>>,
        func: &str,
        span: Span,
    ) -> Option<Instance> {
        let skip = skip.min(generics.len());
        let vars: Vec<Ty> = generics
            .iter()
            .map(|_| self.fresh_var(VarKind::General))
            .collect();
        if let Some(given) = explicit {
            let own = generics.len() - skip;
            if given.len() != own {
                let plural = if own == 1 { "" } else { "s" };
                self.ck.report(
                    Diagnostic::error(
                        format!(
                            "`{func}` takes {own} type argument{plural}, found {}",
                            given.len()
                        ),
                        span,
                    )
                    .with_hint(if own == 0 {
                        format!("`{func}` is not generic")
                    } else {
                        let names: Vec<&str> =
                            generics[skip..].iter().map(|g| g.name.as_str()).collect();
                        format!("its type parameters are: {}", names.join(", "))
                    }),
                );
                return None;
            }
            for (var, ty) in vars[skip..].iter().zip(&given) {
                let _ = self.infer.unify(var, ty);
            }
        }
        let map = Checker::param_map(generics, &vars);
        for g in generics {
            for b in &g.bounds {
                self.obligations.push(Obligation {
                    ty: g.ty().subst(&map),
                    iface: b.iface,
                    args: b.args.iter().map(|a| a.subst(&map)).collect(),
                    span,
                    origin: Origin::Bound {
                        param: g.name.clone(),
                        func: func.to_string(),
                        bound_span: b.span,
                    },
                });
            }
        }
        if !generics.is_empty() {
            self.generic_calls.push(GenericCall {
                args: vars.clone(),
                names: generics.iter().map(|g| g.name.clone()).collect(),
                func: func.to_string(),
                span,
            });
        }
        Some((vars, map))
    }

    /// The type arguments written in `f[int, str]`.
    pub(crate) fn explicit_type_args(&mut self, args: &[ast::IndexArg]) -> Option<Vec<Ty>> {
        let mut out = Vec::new();
        let mut ok = true;
        for a in args {
            let texpr = match a {
                ast::IndexArg::Type(t) => Some(t.clone()),
                ast::IndexArg::Expr(e) => crate::collections::expr_as_type(e),
            };
            match texpr {
                Some(t) => {
                    let ty = self.resolve_type_infer(&t);
                    ok &= !ty.is_error();
                    out.push(ty);
                }
                None => {
                    if let ast::IndexArg::Expr(e) = a {
                        self.ck
                            .report(Diagnostic::error("expected a type here", e.span));
                    }
                    ok = false;
                }
            }
        }
        ok.then_some(out)
    }

    /// `max[f32](a, b)` and `x.map[int](..)`: a call with explicit type arguments. The `[..]`
    /// parses like an index; it is instantiation when `base` names a generic function or
    /// method (DESIGN §3.5).
    fn check_instantiated_call(
        &mut self,
        base: &ast::Expr,
        type_args: &[ast::IndexArg],
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        match &base.kind {
            ExprKind::Ident(id) if self.lookup_local(&id.name).is_none() => {
                if let Some(f) = self.lookup_fn(&id.name) {
                    let Some(explicit) = self.explicit_type_args(type_args) else {
                        self.check_args_loosely(args);
                        return self.error_expr(span);
                    };
                    return self.check_fn_call_with(f, Some(explicit), None, args, span);
                }
                self.check_args_loosely(args);
                let hint = if self.ck.values.contains_key(&id.name)
                    || self.ck.type_by_name(&id.name).is_some()
                {
                    format!("`{}` is not a generic function", id.name)
                } else {
                    format!("unknown function `{}`", id.name)
                };
                self.ck.report(
                    Diagnostic::error(format!("`{}[..]` cannot be called", id.name), base.span)
                        .with_hint(hint),
                );
                self.error_expr(span)
            }
            ExprKind::Field { base: recv, name } => {
                let Some(explicit) = self.explicit_type_args(type_args) else {
                    self.check_args_loosely(args);
                    return self.error_expr(span);
                };
                self.check_member_call(recv, name, Some(explicit), args, span)
            }
            _ => {
                self.check_args_loosely(args);
                self.unsupported("calling anything but a named function", base.span)
            }
        }
    }

    /// The place a `var self` method changes: `recv` must be a variable or a part of one, and
    /// that variable must be mutable.
    pub(crate) fn receiver_place(&mut self, recv: Expr, method: &str, span: Span) -> Option<Place> {
        match recv.kind {
            hir::ExprKind::Error => None,
            hir::ExprKind::Read(place) => self
                .require_mutable(&place, span, Action::Call(method))
                .then_some(*place),
            _ => {
                self.ck.report(
                    Diagnostic::error(
                        format!("`{method}` changes its receiver, so it needs a variable, not a temporary value"),
                        span,
                    )
                    .with_hint("store the value in a `var` first, then call the method on it"),
                );
                None
            }
        }
    }

    pub(crate) fn error_expr_typed(&self, span: Span, ty: Ty) -> Expr {
        self.mk(hir::ExprKind::Error, ty, span)
    }

    /// Matches arguments to parameters; `None` after reporting a problem.
    pub(crate) fn bind_args(
        &mut self,
        fname: &str,
        params: &[ParamInfo<'_>],
        args: &[ast::Arg],
        span: Span,
        first_source: usize,
    ) -> Option<Vec<Bound>> {
        let mut slots: Vec<Option<Bound>> = params.iter().map(|_| None).collect();
        let mut ok = true;
        let mut next_positional = 0;
        let mut seen_named = false;
        let mut too_many_reported = false;
        for (source, arg) in args.iter().enumerate() {
            let target = match &arg.name {
                Some(n) => {
                    seen_named = true;
                    match params.iter().position(|p| p.name == n.name) {
                        Some(i) => Some(i),
                        None => {
                            let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
                            let mut diag = Diagnostic::error(
                                format!("`{fname}` has no parameter named `{}`", n.name),
                                n.span,
                            );
                            diag = diag.with_hint(if names.is_empty() {
                                format!("`{fname}` takes no arguments")
                            } else {
                                format!("its parameters are: {}", names.join(", "))
                            });
                            self.ck.report(diag);
                            ok = false;
                            None
                        }
                    }
                }
                None => {
                    if seen_named {
                        self.ck.report(
                            Diagnostic::error(
                                "a positional argument cannot follow a named argument",
                                arg.value.span,
                            )
                            .with_hint("put positional arguments first"),
                        );
                        ok = false;
                        self.check_expr(&arg.value, None);
                        continue;
                    }
                    let i = next_positional;
                    next_positional += 1;
                    if i < params.len() {
                        Some(i)
                    } else {
                        if !too_many_reported {
                            let plural = if params.len() == 1 { "" } else { "s" };
                            self.ck.report(
                                Diagnostic::error(
                                    format!(
                                        "too many arguments: `{fname}` takes {} argument{plural}, found {}",
                                        params.len(),
                                        args.iter().filter(|a| a.name.is_none()).count()
                                    ),
                                    arg.value.span,
                                )
                                .with_label(span, format!("in this call to `{fname}`")),
                            );
                            too_many_reported = true;
                        }
                        ok = false;
                        None
                    }
                }
            };
            let Some(i) = target else {
                self.check_expr(&arg.value, None);
                continue;
            };
            let param = &params[i];
            let expr = self.check_expr(&arg.value, Some(&param.ty));
            let reason = Some((
                param.span,
                format!(
                    "parameter `{}` is declared `{}` here",
                    param.name,
                    self.show(&param.ty)
                ),
            ));
            self.expect(&expr, &param.ty, reason);
            if slots[i].is_some() {
                self.ck.report(Diagnostic::error(
                    format!("the parameter `{}` is given more than once", param.name),
                    arg.value.span,
                ));
                ok = false;
            } else {
                slots[i] = Some(Bound {
                    expr,
                    source: Some(first_source + source),
                });
            }
        }
        let mut bound = Vec::new();
        let mut missing: Vec<&ParamInfo<'_>> = Vec::new();
        for (param, slot) in params.iter().zip(slots) {
            match (slot, param.default) {
                (Some(b), _) => bound.push(b),
                (None, Some(default)) => {
                    let expr = self.check_default_at_call(default, &param.ty, span);
                    bound.push(Bound { expr, source: None });
                }
                (None, None) => missing.push(param),
            }
        }
        if !missing.is_empty() && ok {
            let names: Vec<String> = missing.iter().map(|p| format!("`{}`", p.name)).collect();
            let plural = if missing.len() == 1 { "" } else { "s" };
            let mut diag = Diagnostic::error(
                format!(
                    "missing argument{plural} for {} in the call to `{fname}`",
                    names.join(", ")
                ),
                span,
            );
            for p in &missing {
                diag = diag.with_label(p.span, format!("parameter `{}` is declared here", p.name));
            }
            self.ck.report(diag);
            ok = false;
        }
        ok.then_some(bound)
    }

    /// Builds the call from matched arguments (and the receiver of a method call).
    pub(crate) fn finish_call(
        &mut self,
        callee: Callee,
        receiver: Option<Receiver>,
        mut bound: Vec<Bound>,
        ret: Ty,
        type_args: Vec<Ty>,
        span: Span,
    ) -> Expr {
        let mut place = None;
        match receiver {
            Some(Receiver::Value(expr)) => bound.insert(
                0,
                Bound {
                    expr,
                    source: Some(0),
                },
            ),
            Some(Receiver::Place(p)) => place = Some(p),
            None => {}
        }
        let (values, lets, temps) = self.order_args(bound);
        let mut args: Vec<Arg> = place.into_iter().map(Arg::Place).collect();
        args.extend(values.into_iter().map(Arg::Value));
        let call = hir::ExprKind::Call(Box::new(hir::Call {
            callee,
            type_args,
            args,
        }));
        let call = self.mk(call, ret, span);
        self.wrap_hoisted(call, lets, temps, span)
    }

    /// Puts matched arguments in evaluation order. Arguments run in source order, then
    /// defaults in parameter order (ARCHITECTURE §6); when that differs from parameter order
    /// and more than one of them has an effect, they are evaluated into temporaries first.
    /// Returns the argument expressions in parameter order and the statements that fill the
    /// temporaries (with the locals they declare).
    pub(crate) fn order_args(&mut self, bound: Vec<Bound>) -> (Vec<Expr>, Vec<Stmt>, Vec<LocalId>) {
        // The evaluation rank each argument should have.
        let explicit = bound.iter().filter(|b| b.source.is_some()).count();
        let mut default_rank = explicit;
        let ranks: Vec<usize> = bound
            .iter()
            .map(|b| match b.source {
                Some(_) => bound
                    .iter()
                    .filter(|o| matches!((o.source, b.source), (Some(x), Some(y)) if x < y))
                    .count(),
                None => {
                    default_rank += 1;
                    default_rank - 1
                }
            })
            .collect();
        let effectful: Vec<usize> = (0..bound.len())
            .filter(|&i| !is_pure(&bound[i].expr))
            .collect();
        let needs_hoist = effectful.windows(2).any(|w| ranks[w[0]] > ranks[w[1]]);

        let mut lets = Vec::new();
        let mut temps = Vec::new();
        let mut order: Vec<usize> = (0..bound.len()).collect();
        order.sort_by_key(|&i| ranks[i]);
        let mut slots: Vec<Option<Expr>> = bound.into_iter().map(|b| Some(b.expr)).collect();
        if needs_hoist {
            for i in order {
                let Some(expr) = slots[i].take() else {
                    continue;
                };
                if is_pure(&expr) {
                    slots[i] = Some(expr);
                    continue;
                }
                let (ty, espan) = (expr.ty.clone(), expr.span);
                let local = self.declare_temp(ty.clone(), espan);
                temps.push(local);
                lets.push(Stmt {
                    span: espan,
                    kind: StmtKind::Let { local, init: expr },
                });
                let place = hir::Place {
                    anchor: hir::Anchor::Local(local),
                    path: Vec::new(),
                    ty: ty.clone(),
                    display: "<argument>".to_string(),
                };
                slots[i] = Some(self.mk(hir::ExprKind::Read(Box::new(place)), ty, espan));
            }
        }
        (slots.into_iter().flatten().collect(), lets, temps)
    }

    /// Wraps `value` in a block that first runs the hoisting statements, if there are any.
    pub(crate) fn wrap_hoisted(
        &mut self,
        value: Expr,
        lets: Vec<Stmt>,
        temps: Vec<LocalId>,
        span: Span,
    ) -> Expr {
        if lets.is_empty() {
            return value;
        }
        let ty = value.ty.clone();
        let block = Block {
            locals: temps,
            stmts: lets,
            tail: Some(Box::new(value)),
            ty: ty.clone(),
            span,
        };
        self.mk(hir::ExprKind::Block(Box::new(block)), ty, span)
    }
}

/// A parameter with its type's type parameters replaced.
pub(crate) fn subst_param<'a>(mut p: ParamInfo<'a>, map: &[(ParamId, Ty)]) -> ParamInfo<'a> {
    p.ty = p.ty.subst(map);
    p
}

/// The receiver of a method call.
pub(crate) enum Receiver {
    /// `self`: read-only, passed by value.
    Value(Expr),
    /// `var self`: a place the method changes in place.
    Place(Place),
}

/// Whether `e` is a literal, which can be evaluated at any time. Everything else (even a
/// plain variable read, which an earlier argument could change) keeps its place.
fn is_pure(e: &Expr) -> bool {
    matches!(e.kind, hir::ExprKind::Lit(_))
}
