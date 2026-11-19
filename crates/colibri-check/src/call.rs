//! Checking calls: resolving the callee, matching arguments to parameters (named and default
//! arguments become positional), and keeping evaluation order stable.

use colibri_syntax::ast::{self, ExprKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::body::GenericCall;
use crate::checker::{Checker, Dep, ParamInfo, ValueRef};
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
        // `module.f(..)` calls the function `module.f` of the scope (ARCHITECTURE §11.3).
        let qualified;
        let id = match &callee.kind {
            ExprKind::Ident(id) => id,
            ExprKind::Field { base, name } => match self.qualified_parts(base, name) {
                Some(q) => {
                    qualified = q;
                    &qualified
                }
                None => return self.check_member_call(base, name, None, args, span),
            },
            ExprKind::Paren(inner) if matches!(inner.kind, ExprKind::Ident(_)) => {
                return self.check_call(inner, args, expected, span);
            }
            ExprKind::Index {
                base,
                args: type_args,
            } if self.is_generic_fn_name(base) => {
                return self.check_instantiated_call(base, type_args, args, span);
            }
            _ => return self.check_value_call(callee, args, span),
        };
        let name = id.name.as_str();
        if self.lookup_local(name).is_some() {
            return self.check_value_call(callee, args, span);
        }
        if let Some(f) = self.lookup_fn(name) {
            return self.check_fn_call(f, args, span);
        }
        if matches!(self.ck.values.get(name), Some(ValueRef::Global(_))) {
            return self.check_value_call(callee, args, span);
        }
        match name {
            "print" if !self.ck.values.contains_key(name) => self.check_print(args, span),
            "some" => self.check_some(args, expected, span),
            "ok" | "err" => self.check_ok_err(name == "ok", args, expected, span),
            "panic" if !self.ck.values.contains_key(name) => self.check_panic(args, span),
            "assert" if !self.ck.values.contains_key(name) => self.check_assert(args, span),
            "assert_eq" if !self.ck.values.contains_key(name) => self.check_assert_eq(args, span),
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

    /// Whether `base` of `base[..](..)` names a function (so the brackets are type arguments)
    /// or a method after a dot, rather than a variable being indexed.
    fn is_generic_fn_name(&mut self, base: &ast::Expr) -> bool {
        match &base.kind {
            ExprKind::Ident(id) => {
                !self.is_local(&id.name)
                    && !matches!(self.ck.values.get(&id.name), Some(ValueRef::Global(_)))
            }
            ExprKind::Field { base, name } if self.qualified_parts(base, name).is_some() => self
                .qualified_parts(base, name)
                .is_some_and(|q| !matches!(self.ck.values.get(&q.name), Some(ValueRef::Global(_)))),
            // `x.map[int](..)` names a generic method; `self.handlers[0](..)` indexes a field.
            // (`is` and `as` are keywords, so no field has those names: `e.is[T]()` is the
            // downcast of an `Error`.)
            ExprKind::Field { name, .. } => {
                matches!(name.name.as_str(), "is" | "as")
                    || self.ck.methods.iter().any(|((_, n), f)| {
                        n == &name.name && !self.ck.fns[f.0 as usize].generics.is_empty()
                    })
            }
            _ => false,
        }
    }

    /// Checks arguments of a call that cannot be resolved, so their own errors still show.
    pub(crate) fn check_args_loosely(&mut self, args: &[ast::Arg]) {
        for a in args {
            self.check_expr(&a.value, None);
        }
    }

    /// `panic(msg)`: stops the program with a message. It has type `!`, so it can end a
    /// handler block or a branch.
    fn check_panic(&mut self, args: &[ast::Arg], span: Span) -> Expr {
        let [arg] = args else {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!("`panic` takes exactly one message, found {}", args.len()),
                    span,
                )
                .with_hint("write `panic(\"what went wrong\")`"),
            );
            return self.error_expr(span);
        };
        if let Some(name) = &arg.name {
            self.check_expr(&arg.value, None);
            self.ck.report(Diagnostic::error(
                "`panic` does not take a named argument",
                name.span,
            ));
            return self.error_expr(span);
        }
        let msg = self.check_expr(&arg.value, Some(&Ty::Str));
        self.expect(&msg, &Ty::Str, None);
        let call = hir::ExprKind::Call(Box::new(hir::Call {
            callee: Callee::Intrinsic(Intrinsic::Panic),
            type_args: Vec::new(),
            args: vec![Arg::Value(msg)],
        }));
        self.mk(call, Ty::Never, span)
    }

    /// `assert(cond)` or `assert(cond, "message")`: panics when `cond` is false. The message
    /// may be named (`msg: ..`). Type `()` (ARCHITECTURE §15).
    fn check_assert(&mut self, args: &[ast::Arg], span: Span) -> Expr {
        let shape_ok = (1..=2).contains(&args.len())
            && args[0].name.is_none()
            && args
                .get(1)
                .is_none_or(|a| a.name.as_ref().is_none_or(|n| n.name == "msg"));
        if !shape_ok {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!(
                        "`assert` takes a condition and an optional message, found {} arguments",
                        args.len()
                    ),
                    span,
                )
                .with_hint("write `assert(x > 0)` or `assert(x > 0, \"x must be positive\")`"),
            );
            return self.error_expr(span);
        }
        let cond = self.check_expr(&args[0].value, Some(&Ty::Bool));
        let cond_ok = self.expect(&cond, &Ty::Bool, None);
        let msg = match args.get(1) {
            Some(a) => {
                let m = self.check_expr(&a.value, Some(&Ty::Str));
                let ok = self.expect(&m, &Ty::Str, None);
                ok.then_some(m)
            }
            None => Some(self.mk(
                hir::ExprKind::Lit(hir::Lit::Str(std::rc::Rc::from("assertion failed"))),
                Ty::Str,
                span,
            )),
        };
        let (true, Some(msg)) = (cond_ok, msg) else {
            return self.error_expr(span);
        };
        self.mk(
            hir::ExprKind::Call(Box::new(hir::Call {
                callee: Callee::Intrinsic(Intrinsic::Assert),
                type_args: Vec::new(),
                args: vec![Arg::Value(cond), Arg::Value(msg)],
            })),
            Ty::Unit,
            span,
        )
    }

    /// `assert_eq(a, b)`: panics unless `a == b`, showing both values. The two sides must have
    /// the same type, which must support `==` (`Eq`); every type can be shown.
    fn check_assert_eq(&mut self, args: &[ast::Arg], span: Span) -> Expr {
        if args.len() != 2 || args.iter().any(|a| a.name.is_some()) {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!(
                        "`assert_eq` takes exactly two values, found {} arguments",
                        args.len()
                    ),
                    span,
                )
                .with_hint("write `assert_eq(actual, expected)`"),
            );
            return self.error_expr(span);
        }
        let mut l = self.check_expr(&args[0].value, None);
        let mut r = self.check_expr(&args[1].value, Some(&l.ty));
        if !self.unify_operands(ast::BinOp::Eq, &mut l, &mut r, span) {
            return self.error_expr(span);
        }
        if !self.supports_eq(&l.ty) {
            let shown = self.show(&l.ty);
            let mut diag = Diagnostic::error(
                format!("`assert_eq` cannot compare values of type `{shown}`"),
                span,
            );
            if let Some(hint) = self.comparison_hint(&l.ty, true) {
                diag = diag.with_hint(hint);
            }
            self.ck.report(diag);
            return self.error_expr(span);
        }
        self.mk(
            hir::ExprKind::Call(Box::new(hir::Call {
                callee: Callee::Intrinsic(Intrinsic::AssertEq),
                type_args: Vec::new(),
                args: vec![Arg::Value(l), Arg::Value(r)],
            })),
            Ty::Unit,
            span,
        )
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
                | Ty::F32
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
                | Ty::Fn(..)
                | Ty::Range(_)
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
        let (name, params, ret, poisoned, generics, self_ty, host) = (
            info.name.clone(),
            info.params.clone(),
            info.ret.clone(),
            info.is_poisoned(),
            info.generics.clone(),
            info.self_ty.clone(),
            info.host.clone(),
        );
        if poisoned {
            self.check_args_loosely(args);
            return self.error_expr(span);
        }
        if let Some(host) = &host {
            let module = &self.ck.mods[self.ck.fns[f.0 as usize].module.0 as usize];
            // A function of a named module reads as it is written: `fs.read`.
            let shown = if module.kind == crate::modules::ModKind::Host {
                let short = module.name.rsplit('.').next().unwrap_or(&module.name);
                format!("{short}.{name}")
            } else {
                name.clone()
            };
            self.check_host_capability(&shown, host, span);
            if host.positional
                && let Some(named) = args.iter().find(|a| a.name.is_some())
            {
                self.check_args_loosely(args);
                self.ck.report(
                    Diagnostic::error(
                        format!("`{name}` takes positional arguments only"),
                        named.value.span,
                    )
                    .with_hint("a host function registered as a closure has unnamed parameters"),
                );
                return self.error_expr_typed(span, ret);
            }
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
        let callee = match host {
            Some(host) => Callee::Host(host.id),
            None => Callee::Fn(f),
        };
        self.finish_call(callee, None, bound, ret, type_args, span)
    }

    /// Reports a use of a host item whose capability the host did not grant.
    pub(crate) fn check_host_capability(
        &mut self,
        name: &str,
        host: &crate::checker::HostFnInfo,
        span: Span,
    ) {
        if let Some(cap) = &host.requires
            && !self.ck.host.is_granted(cap)
        {
            self.ck.report(
                Diagnostic::error(
                    format!("`{name}` needs the `{cap}` capability, which this host did not grant"),
                    span,
                )
                .with_hint(format!(
                    "the host grants it with `VmBuilder::allow(Cap::{})`",
                    capitalised(cap)
                )),
            );
        }
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
        let qualified = self.qualified(base);
        let ident = match &base.kind {
            ExprKind::Ident(id) => Some(id),
            _ => qualified.as_ref(),
        };
        match &base.kind {
            _ if ident.is_some_and(|id| self.lookup_local(&id.name).is_none()) => {
                let id = ident.expect("checked above");
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
        let mut pending: Vec<(usize, usize)> = Vec::new();
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
            pending.push((source, i));
        }
        // Arguments are checked left to right, except that closures wait for the others, so
        // the types the other arguments fix (`xs` before `fn(x) => ..`) reach the closure's
        // parameters whatever the order they are written in (DESIGN §5.4).
        let is_closure = |source: usize| matches!(&args[source].value.kind, ExprKind::Closure(_));
        pending.sort_by_key(|&(source, _)| (is_closure(source), source));
        for (source, i) in pending {
            let arg = &args[source];
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
                    let expr = self.check_default_at_call(default, &param.ty, span, param.module);
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

/// `fs` as `Fs`, for the name of a capability in a hint.
fn capitalised(cap: &str) -> String {
    let mut chars = cap.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
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
