//! Checking calls: resolving the callee, matching arguments to parameters (named and default
//! arguments become positional), and keeping evaluation order stable.

use colibri_syntax::ast::{self, ExprKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::checker::{Dep, ParamInfo};
use crate::hir::{self, Arg, Block, Callee, Expr, FnId, Intrinsic, LocalId, Place, Stmt, StmtKind};
use crate::place::Action;
use crate::types::Ty;

/// An argument matched to a parameter.
pub(crate) struct Bound {
    pub expr: Expr,
    /// The position in the source for an explicit argument; `None` for a default.
    pub source: Option<usize>,
}

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
                return self.check_member_call(base, name, args, span);
            }
            ExprKind::Paren(inner) if matches!(inner.kind, ExprKind::Ident(_)) => {
                return self.check_call(inner, args, expected, span);
            }
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
        self.add_dep(Dep::Fn(f));
        let info = &self.ck.fns[f.0 as usize];
        let (name, params, ret, poisoned) = (
            info.name.clone(),
            info.params.clone(),
            info.ret.clone(),
            info.is_poisoned(),
        );
        if poisoned {
            self.check_args_loosely(args);
            return self.error_expr(span);
        }
        let Some(bound) = self.bind_args(&name, &params, args, span, 0) else {
            return self.error_expr_typed(span, ret);
        };
        self.finish_call(Callee::Fn(f), None, bound, ret, span)
    }

    /// `base.name(args)`: a call through a type (`Shape.Circle(1)`, `Particle.new()`) or a
    /// method call on a value.
    #[inline(never)]
    fn check_member_call(
        &mut self,
        base: &ast::Expr,
        name: &ast::Ident,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        if let ExprKind::Ident(id) = &base.kind
            && let Some(ty) = self.type_path(id)
        {
            return self.check_type_member(ty, name, Some(args), span);
        }
        let recv = self.check_expr(base, None);
        let recv_ty = self.infer.shallow(&recv.ty);
        let owner = match &recv_ty {
            Ty::Error => {
                self.check_args_loosely(args);
                return self.error_expr(span);
            }
            Ty::Adt(id, _) => *id,
            other => match self.check_builtin_method(recv, name, args, span) {
                Ok(call) => return call,
                Err(_) => {
                    self.check_args_loosely(args);
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
                    return self.error_expr(span);
                }
            },
        };
        let Some(&f) = self.ck.methods.get(&(owner, name.name.clone())) else {
            if name.name == "clone" {
                return self.check_clone(recv, args, span);
            }
            self.check_args_loosely(args);
            let tname = self.ck.types[owner.0 as usize].name.clone();
            let methods: Vec<String> = self
                .ck
                .methods
                .keys()
                .filter(|(t, _)| *t == owner)
                .map(|(_, n)| n.clone())
                .collect();
            let mut diag = Diagnostic::error(
                format!("`{tname}` has no method `{}`", name.name),
                name.span,
            );
            if let Some(hint) = crate::checker::Checker::did_you_mean(&name.name, &methods) {
                diag = diag.with_hint(hint);
            } else if self.ck.types[owner.0 as usize]
                .fields()
                .iter()
                .any(|f| f.name == name.name)
            {
                diag = diag.with_hint(format!("`{}` is a field, not a method", name.name));
            }
            self.ck.report(diag);
            return self.error_expr(span);
        };
        self.add_dep(Dep::Fn(f));
        let info = &self.ck.fns[f.0 as usize];
        let (fname, params, ret, receiver, poisoned) = (
            info.name.clone(),
            info.params.clone(),
            info.ret.clone(),
            info.receiver,
            info.is_poisoned(),
        );
        let Some(mutable) = receiver else {
            self.check_args_loosely(args);
            let tname = self.ck.types[owner.0 as usize].name.clone();
            self.ck.report(
                Diagnostic::error(
                    format!("`{}` is a function of `{tname}`, not a method", name.name),
                    name.span,
                )
                .with_hint(format!(
                    "call it through the type: `{tname}.{}(...)`",
                    name.name
                )),
            );
            return self.error_expr(span);
        };
        if poisoned {
            self.check_args_loosely(args);
            return self.error_expr(span);
        }
        let receiver = if mutable {
            match self.receiver_place(recv, &name.name, base.span) {
                Some(place) => Receiver::Place(place),
                None => {
                    self.check_args_loosely(args);
                    return self.error_expr_typed(span, ret);
                }
            }
        } else {
            Receiver::Value(recv)
        };
        let Some(bound) = self.bind_args(&fname, &params[1..], args, span, 1) else {
            return self.error_expr_typed(span, ret);
        };
        self.finish_call(Callee::Fn(f), Some(receiver), bound, ret, span)
    }

    /// The place a `var self` method changes: `recv` must be a variable or a part of one, and
    /// that variable must be mutable.
    fn receiver_place(&mut self, recv: Expr, method: &str, span: Span) -> Option<Place> {
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
                format!("parameter `{}` is declared `{}` here", param.name, param.ty),
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
            type_args: Vec::new(),
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
