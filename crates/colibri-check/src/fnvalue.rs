//! Function values (DESIGN §5.2, §6.2): named functions, static methods and instantiated
//! generic functions used as values, and calls of values of function type.
//!
//! A call `callee(args)` where `callee` is not the name of a function goes through
//! [`Callee::Value`]: the function value is the first argument. `obj.name(args)` is a method
//! call when `obj`'s type has a method called `name`; only when it has none, and a field
//! called `name` has a function type, is it a call of that field's value (ARCHITECTURE §6).

use colibri_syntax::ast;
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::checker::Dep;
use crate::hir::{self, Arg, Callee, Expr, FnId};
use crate::types::Ty;

impl FnCtx<'_, '_> {
    /// The function `f` as a value: `explicit` are the type arguments written (`max[int]`),
    /// `owner` the type before a static function (`Particle.new`). The type parameters that
    /// are not given are inferred from where the value is used.
    pub(crate) fn fn_value(
        &mut self,
        f: FnId,
        explicit: Option<Vec<Ty>>,
        owner: Option<Ty>,
        span: Span,
    ) -> Expr {
        self.add_dep(Dep::Fn(f));
        let info = &self.ck.fns[f.0 as usize];
        let (name, params, ret, poisoned, generics, self_ty, receiver) = (
            info.name.clone(),
            info.params.clone(),
            info.ret.clone(),
            info.is_poisoned(),
            info.generics.clone(),
            info.self_ty.clone(),
            info.receiver,
        );
        if poisoned {
            return self.error_expr(span);
        }
        if self.ck.fns[f.0 as usize].host.is_some() {
            self.ck.report(
                Diagnostic::error(
                    format!("the host function `{name}` cannot be used as a value"),
                    span,
                )
                .with_hint(format!("wrap it in a closure: `fn(..) => {name}(..)`")),
            );
            return self.error_expr(span);
        }
        if receiver.is_some() {
            let owner_name = name.rsplit_once('.').map_or(name.as_str(), |(o, _)| o);
            self.ck.report(
                Diagnostic::error(
                    format!("`{name}` is a method, so it needs a value to act on"),
                    span,
                )
                .with_hint(format!(
                    "call it on a value, or wrap it in a closure: `fn(x: {owner_name}) => x.method()`"
                )),
            );
            return self.error_expr(span);
        }
        let outer = generics.len() - self.own_generic_count(f);
        let Some((type_args, map)) = self.instantiate(&generics, outer, explicit, &name, span)
        else {
            return self.error_expr(span);
        };
        if let (Some(owner), Some(self_ty)) = (&owner, &self_ty) {
            let _ = self.infer.unify(&self_ty.subst(&map), owner);
        }
        let params: Vec<Ty> = params.iter().map(|p| p.ty.subst(&map)).collect();
        let ty = Ty::Fn(params, Box::new(ret.subst(&map)));
        self.mk(hir::ExprKind::FnRef { f, type_args }, ty, span)
    }

    /// A call whose callee is an expression other than a plain function name: a variable or
    /// field of function type, a parenthesised expression, the result of another call.
    pub(crate) fn check_value_call(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        let f = self.check_expr(callee, None);
        self.call_value(f, callee, args, span)
    }

    /// Calls the already-checked function value `f`.
    pub(crate) fn call_value(
        &mut self,
        f: Expr,
        callee: &ast::Expr,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        let ty = self.infer.shallow(&f.ty);
        let Ty::Fn(params, ret) = ty else {
            self.check_args_loosely(args);
            if !ty.is_error() {
                let shown = self.show(&ty);
                let name = crate::place::expr_text(callee);
                self.ck.report(
                    Diagnostic::error(
                        format!("`{name}` is a `{shown}`, not a function"),
                        callee.span,
                    )
                    .with_hint("only functions and closures can be called"),
                );
            }
            return self.error_expr(span);
        };
        if let Some(named) = args.iter().find(|a| a.name.is_some()) {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    "a function value takes positional arguments",
                    named.value.span,
                )
                .with_hint("named arguments work only when calling a declared function"),
            );
            return self.error_expr_typed(span, *ret);
        }
        if args.len() != params.len() {
            self.check_args_loosely(args);
            let (want, got) = (params.len(), args.len());
            let plural = if want == 1 { "" } else { "s" };
            self.ck.report(
                Diagnostic::error(
                    format!("this function takes {want} argument{plural}, found {got}"),
                    span,
                )
                .with_label(
                    callee.span,
                    format!("this is `{}`", self.show(&Ty::Fn(params, ret.clone()))),
                ),
            );
            return self.error_expr_typed(span, *ret);
        }
        let mut values = vec![Arg::Value(f)];
        for (arg, param) in args.iter().zip(&params) {
            let e = self.check_expr(&arg.value, Some(param));
            self.expect(&e, param, None);
            values.push(Arg::Value(e));
        }
        self.mk(
            hir::ExprKind::Call(Box::new(hir::Call {
                callee: Callee::Value,
                type_args: Vec::new(),
                args: values,
            })),
            *ret,
            span,
        )
    }

    /// `recv.name(args)` where the receiver has no method `name` but a field of that name has
    /// a function type. Gives the receiver back when that is not the case.
    pub(crate) fn field_call(
        &mut self,
        recv: Expr,
        base: &ast::Expr,
        name: &ast::Ident,
        args: &[ast::Arg],
        span: Span,
    ) -> Result<Expr, Expr> {
        let Ty::Adt(id, targs) = self.infer.shallow(&recv.ty) else {
            return Err(recv);
        };
        let def = &self.ck.types[id.0 as usize];
        let Some(field) = def.fields().iter().find(|f| f.name == name.name) else {
            return Err(recv);
        };
        let map = crate::checker::Checker::param_map_of(def, &targs);
        if !matches!(field.ty.subst(&map), Ty::Fn(..)) || field.weak {
            return Err(recv);
        }
        let field_expr = self.field_value(recv, base, name, name.span);
        let callee = ast::Expr {
            id: base.id,
            span: name.span,
            kind: ast::ExprKind::Field {
                base: Box::new(base.clone()),
                name: name.clone(),
            },
        };
        Ok(self.call_value(field_expr, &callee, args, span))
    }
}
