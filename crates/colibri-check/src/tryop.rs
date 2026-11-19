//! `?`, `or` and `or { .. }` (DESIGN §10.3).
//!
//! All three lower to a `Match` over the `Result` or `Option` (ARCHITECTURE §1):
//!
//! - `e?` is `match e { ok(v) => v, err(x) => return err(into_error(x)) }` (and the same with
//!   `some`/`none` for an `Option`); the `return` is an ordinary one, so drops and `defer`s
//!   run exactly as for a written `return`.
//! - `a or b` is `match a { ok(v) => v, err(_) => b }`; `b` is the arm body, so it runs only
//!   when needed. When `b` is itself a `Result` or `Option`, the arms rebuild one.
//! - `a or { .. }` and `a or |e| { .. }` put the handler block in the `err`/`none` arm. The
//!   handler is checked in the enclosing function's context, not as a closure, so `return`,
//!   `break` and `continue` in it act on the enclosing function and loop.

use colibri_syntax::ast::{self, Ident};
use colibri_syntax::{Diagnostic, Span};

use crate::body::{BlockMode, FnCtx, LocalKind};
use crate::hir::{self, Anchor, Arm, Block, Expr, LocalId, Pat, PatKind, Place, Stmt, StmtKind};
use crate::infer::VarKind;
use crate::optres::{is_error_ty, result_ty};
use crate::types::{
    OPTION_TYPE, RESULT_TYPE, Ty, VARIANT_ERR, VARIANT_NONE, VARIANT_OK, VARIANT_SOME,
};

/// What a `?` or `or` operand is: the two shapes of a fallible value.
#[derive(Clone)]
enum Fallible {
    /// `Result[T, E]`
    Result { ok: Ty, err: Ty },
    /// `T?`
    Option { some: Ty },
}

/// Whether and how the failure pattern binds the error.
#[derive(Clone, Copy)]
enum ErrorBinding<'a> {
    No,
    /// A hidden local (for `?`).
    Temp,
    /// The name of `or |e| { .. }`.
    Named(&'a Ident),
}

impl Fallible {
    fn value_ty(&self) -> &Ty {
        match self {
            Fallible::Result { ok, .. } => ok,
            Fallible::Option { some } => some,
        }
    }
}

impl FnCtx<'_, '_> {
    /// Reads the operand type of `?` or `or`; `None` after reporting (or when the type was
    /// already an error).
    fn fallible(&mut self, operand: &Expr, what: &str, hint: &str) -> Option<Fallible> {
        match self.infer.shallow(&operand.ty) {
            Ty::Adt(RESULT_TYPE, a) if a.len() == 2 => Some(Fallible::Result {
                ok: a[0].clone(),
                err: a[1].clone(),
            }),
            Ty::Option(t) => Some(Fallible::Option { some: *t }),
            Ty::Error | Ty::Never => None,
            Ty::Infer(_) => {
                self.ck.report(
                    Diagnostic::error(
                        format!("the type of the value before {what} is not known yet"),
                        operand.span,
                    )
                    .with_hint("give it a type, for example by annotating the `let` it comes from"),
                );
                None
            }
            other => {
                let shown = self.show(&other);
                self.ck.report(
                    Diagnostic::error(
                        format!("{what} needs a `Result` or an `Option`, found `{shown}`"),
                        operand.span,
                    )
                    .with_hint(hint),
                );
                None
            }
        }
    }

    /// A read of a local.
    fn read_local(&self, local: LocalId, ty: &Ty, span: Span) -> Expr {
        let place = Place {
            anchor: Anchor::Local(local),
            path: Vec::new(),
            ty: ty.clone(),
            display: "<value>".to_string(),
        };
        self.mk(hir::ExprKind::Read(Box::new(place)), ty.clone(), span)
    }

    /// The pattern that matches the success case (`ok(local)` / `some(local)`), binding a
    /// fresh local (declared in the open scope), and that local.
    fn success_pat(&mut self, kind: &Fallible, scrutinee: &Ty, span: Span) -> (Pat, LocalId) {
        let value = kind.value_ty().clone();
        let local = self.declare_temp(value.clone(), span);
        let bind = Pat {
            kind: PatKind::Bind(local, None),
            ty: value,
            span,
        };
        let (ty, variant) = match kind {
            Fallible::Result { .. } => (RESULT_TYPE, VARIANT_OK),
            Fallible::Option { .. } => (OPTION_TYPE, VARIANT_SOME),
        };
        let pat = Pat {
            kind: PatKind::Variant {
                ty,
                variant,
                args: vec![bind],
            },
            ty: scrutinee.clone(),
            span,
        };
        (pat, local)
    }

    /// The pattern that matches the failure case (`err(local)` or `err(_)` / `none`), binding
    /// a fresh local when `bind` is set and the operand is a `Result`.
    fn failure_pat(
        &mut self,
        kind: &Fallible,
        scrutinee: &Ty,
        bind: ErrorBinding<'_>,
        span: Span,
    ) -> (Pat, Option<LocalId>) {
        let mut local = None;
        let (ty, variant, args) = match kind {
            Fallible::Result { err, .. } => {
                let sub = match bind {
                    ErrorBinding::No => PatKind::Wild,
                    ErrorBinding::Temp => {
                        let l = self.declare_temp(err.clone(), span);
                        local = Some(l);
                        PatKind::Bind(l, None)
                    }
                    ErrorBinding::Named(id) => {
                        let l = self.declare(&id.name, id.span, err.clone(), LocalKind::Let);
                        local = Some(l);
                        PatKind::Bind(l, None)
                    }
                };
                let sub = Pat {
                    kind: sub,
                    ty: err.clone(),
                    span,
                };
                (RESULT_TYPE, VARIANT_ERR, vec![sub])
            }
            Fallible::Option { .. } => (OPTION_TYPE, VARIANT_NONE, Vec::new()),
        };
        let pat = Pat {
            kind: PatKind::Variant { ty, variant, args },
            ty: scrutinee.clone(),
            span,
        };
        (pat, local)
    }

    /// An arm whose body is `tail` (with the locals its pattern declared).
    fn arm(pat: Pat, locals: Vec<LocalId>, tail: Expr, span: Span) -> Arm {
        let ty = tail.ty.clone();
        Arm {
            pat,
            guard: None,
            body: Block {
                locals,
                stmts: Vec::new(),
                tail: Some(Box::new(tail)),
                ty,
                span,
            },
        }
    }

    // ---- ? ----

    /// `inner?`: the value of a success, or an early `return` of the failure.
    #[inline(never)]
    pub fn check_try(&mut self, inner: &ast::Expr, span: Span) -> Expr {
        let operand = self.check_expr(inner, None);
        let Some(kind) = self.fallible(
            &operand,
            "`?`",
            "`?` unwraps a `Result` or an `Option`; a plain value needs no `?`",
        ) else {
            return self.error_expr(span);
        };
        let value_ty = kind.value_ty().clone();
        let fail = |this: &Self| this.error_expr_typed(span, value_ty.clone());
        if self.defer_depth > 0 {
            self.ck.report(
                Diagnostic::error("`?` cannot be used inside `defer`", span)
                    .with_hint("a deferred statement runs while the block is already exiting, so it cannot return early"),
            );
            return fail(self);
        }
        let Some((ret, ret_span)) = self.ret.clone() else {
            self.ck.report(
                Diagnostic::error("`?` can only be used inside a function", span).with_hint(
                    "module-level code cannot return early: handle the failure with `or`, `match` or `.expect(..)`, or move the code into a function that returns `Result`",
                ),
            );
            return fail(self);
        };
        let Some(early) = self.try_return(&kind, &ret, ret_span, span) else {
            return fail(self);
        };
        let scrutinee_ty = operand.ty.clone();

        self.push_scope();
        let (ok_pat, v) = self.success_pat(&kind, &scrutinee_ty, span);
        let ok_value = self.read_local(v, &value_ty, span);
        let ok_locals = self.pop_scope();
        let ok_arm = Self::arm(ok_pat, ok_locals, ok_value, span);

        self.push_scope();
        let (fail_pat, bound) = self.failure_pat(&kind, &scrutinee_ty, ErrorBinding::Temp, span);
        let returned = self.early_value(&early, bound, span);
        let ret_stmt = Stmt {
            span,
            kind: StmtKind::Return(Some(returned)),
        };
        let fail_locals = self.pop_scope();
        let fail_arm = Arm {
            pat: fail_pat,
            guard: None,
            body: Block {
                locals: fail_locals,
                stmts: vec![ret_stmt],
                tail: None,
                ty: Ty::Never,
                span,
            },
        };
        self.mk(
            hir::ExprKind::Match {
                scrutinee: Box::new(operand),
                arms: vec![ok_arm, fail_arm],
            },
            value_ty,
            span,
        )
    }
}

/// What a failing `?` returns from the enclosing function.
enum Early {
    /// `err(error)` of the function's `Result` type, converting the error to `Error` first
    /// when the types differ.
    Result { ty: Ty, convert: bool },
    /// `none` of the function's `Option` type; the payload is the element type.
    Option { some: Ty },
}

impl FnCtx<'_, '_> {
    /// Checks that the enclosing function can take a failure of this kind and prepares the
    /// value to return. `None` after reporting.
    fn try_return(
        &mut self,
        kind: &Fallible,
        ret: &Ty,
        ret_span: Option<Span>,
        span: Span,
    ) -> Option<Early> {
        let shown_ret = self.show(ret);
        let declared = ret_span.map(|s| (s, "the return type is declared here".to_string()));
        let ret = self.infer.shallow(ret);
        match kind {
            Fallible::Result { ok: _, err } => {
                let (ret_ok, ret_err) = match &ret {
                    Ty::Adt(RESULT_TYPE, a) if a.len() == 2 => (a[0].clone(), a[1].clone()),
                    // A closure whose result is not known yet: it returns the same kind of
                    // failure that it propagates.
                    Ty::Infer(_) => {
                        let ok = self.fresh_var(VarKind::General);
                        let wanted = result_ty(ok.clone(), err.clone());
                        let _ = self.infer.unify(&ret, &wanted);
                        (ok, err.clone())
                    }
                    Ty::Error => return None,
                    _ => {
                        let mut diag = Diagnostic::error(
                            format!(
                                "`?` on a `Result` needs a function that returns `Result`, but this one returns `{shown_ret}`"
                            ),
                            span,
                        )
                        .with_hint(
                            "make the function return `Result[T]`, or handle the failure here with `or`, `match` or `.expect(..)`",
                        );
                        if let Some((s, text)) = declared {
                            diag = diag.with_label(s, text);
                        }
                        self.ck.report(diag);
                        return None;
                    }
                };
                let converts = !self.infer.unify(err, &ret_err).is_ok();
                if converts {
                    let target = self.infer.shallow(&ret_err);
                    if !is_error_ty(&target) {
                        let (from, to) = (self.show(err), self.show(&ret_err));
                        self.ck.report(
                            Diagnostic::error(
                                format!("`?` cannot turn a `{from}` error into `{to}`"),
                                span,
                            )
                            .with_hint(format!(
                                "only conversion to `Error` is automatic (through `IntoError`); build the `{to}` yourself: `... or |e| {{ return err(...) }}`"
                            )),
                        );
                        return None;
                    }
                    if !self.converts_to_error(err) {
                        let from = self.show(err);
                        self.ck.report(
                            Diagnostic::error(
                                format!("`?` cannot convert `{from}` to `Error`: it does not implement `IntoError`"),
                                span,
                            )
                            .with_hint(format!(
                                "add `impl IntoError for {from} {{}}`; the message then comes from its `Show`"
                            )),
                        );
                        return None;
                    }
                }
                Some(Early::Result {
                    ty: result_ty(ret_ok, ret_err),
                    convert: converts,
                })
            }
            Fallible::Option { .. } => {
                let ret_some = match &ret {
                    Ty::Option(t) => (**t).clone(),
                    Ty::Infer(_) => {
                        let some = self.fresh_var(VarKind::General);
                        let _ = self.infer.unify(&ret, &Ty::Option(Box::new(some.clone())));
                        some
                    }
                    Ty::Error => return None,
                    other => {
                        let result_hint = matches!(other, Ty::Adt(RESULT_TYPE, _));
                        let mut diag = Diagnostic::error(
                            format!(
                                "`?` on an `Option` needs a function that returns an `Option`, but this one returns `{shown_ret}`"
                            ),
                            span,
                        )
                        .with_hint(if result_hint {
                            "an `Option` cannot become a `Result` implicitly: `match` on it, or give the failure a value with `or { return err(...) }`"
                        } else {
                            "make the function return `T?`, or handle the `none` here with `or`, `match` or `.expect(..)`"
                        });
                        if let Some((s, text)) = declared {
                            diag = diag.with_label(s, text);
                        }
                        self.ck.report(diag);
                        return None;
                    }
                };
                Some(Early::Option { some: ret_some })
            }
        }
    }
}

impl FnCtx<'_, '_> {
    /// The value a failing `?` returns; `bound` is the local holding the error.
    fn early_value(&mut self, early: &Early, bound: Option<LocalId>, span: Span) -> Expr {
        match early {
            Early::Option { some } => self.none_of(some, span),
            Early::Result { ty, convert } => {
                let Some(local) = bound else {
                    return self.error_expr(span);
                };
                let err_ty = self.local_ty(local);
                let mut value = self.read_local(local, &err_ty, span);
                if *convert {
                    value = self.convert_error(value);
                }
                self.mk(
                    hir::ExprKind::Variant {
                        ty: RESULT_TYPE,
                        variant: VARIANT_ERR,
                        args: vec![value],
                    },
                    ty.clone(),
                    span,
                )
            }
        }
    }
}

// ---- or ----

impl FnCtx<'_, '_> {
    /// The arm `ok(v) => v` (or `some(v) => v`), or, when `wrap` is the type of a `Result` or
    /// `Option` that `or` produces, the arm that rebuilds a success of that type.
    fn success_arm(
        &mut self,
        kind: &Fallible,
        scrutinee: &Ty,
        wrap: Option<&Ty>,
        span: Span,
    ) -> Arm {
        self.push_scope();
        let (pat, v) = self.success_pat(kind, scrutinee, span);
        let value_ty = kind.value_ty().clone();
        let mut tail = self.read_local(v, &value_ty, span);
        if let Some(target) = wrap {
            let (ty, variant) = match target {
                Ty::Option(_) => (OPTION_TYPE, VARIANT_SOME),
                _ => (RESULT_TYPE, VARIANT_OK),
            };
            tail = self.mk(
                hir::ExprKind::Variant {
                    ty,
                    variant,
                    args: vec![tail],
                },
                target.clone(),
                span,
            );
        }
        let locals = self.pop_scope();
        Self::arm(pat, locals, tail, span)
    }

    /// `lhs or rhs`: `rhs` is a plain value of the success type, or itself a `Result` or
    /// `Option` of it (so fallbacks chain). It is the body of the failure arm, so it runs only
    /// when needed.
    #[inline(never)]
    pub fn check_or(&mut self, lhs: &ast::Expr, rhs: &ast::Expr, span: Span) -> Expr {
        let a = self.check_expr(lhs, None);
        let Some(kind) = self.fallible(
            &a,
            "`or`",
            "`or` gives a fallback for a `Result` or an `Option`; for `bool` use `||`",
        ) else {
            self.check_expr(rhs, None);
            return self.error_expr(span);
        };
        let t = kind.value_ty().clone();
        let b = self.check_expr(rhs, Some(&t));
        let scrutinee_ty = a.ty.clone();
        // A plain fallback first; only when the right side is not a `T`, look for a
        // `Result[T, _]` or `T?`.
        let (result_ty, wrap) = if self.infer.coerce(&b.ty, &t).is_ok() {
            (t.clone(), None)
        } else {
            let chained = match self.infer.shallow(&b.ty) {
                Ty::Adt(RESULT_TYPE, args) if args.len() == 2 => Some(args[0].clone()),
                Ty::Option(inner) => Some(*inner),
                _ => None,
            };
            match chained {
                Some(inner) if self.infer.unify(&inner, &t).is_ok() => {
                    (b.ty.clone(), Some(self.infer.shallow(&b.ty)))
                }
                _ => {
                    let (want, got) = (self.show(&t), self.show(&b.ty));
                    self.ck.report(
                        Diagnostic::error(
                            format!(
                                "the right side of `or` is a `{got}`, but the left side gives `{want}`"
                            ),
                            b.span,
                        )
                        .with_hint(format!(
                            "write a plain `{want}` as the fallback, or another `Result[{want}]` / `{want}?` to chain"
                        )),
                    );
                    return self.error_expr_typed(span, t);
                }
            }
        };
        let ok_arm = self.success_arm(&kind, &scrutinee_ty, wrap.as_ref(), span);
        self.push_scope();
        let (fail_pat, _) = self.failure_pat(&kind, &scrutinee_ty, ErrorBinding::No, span);
        let locals = self.pop_scope();
        let fail_arm = Self::arm(fail_pat, locals, b, span);
        self.mk(
            hir::ExprKind::Match {
                scrutinee: Box::new(a),
                arms: vec![ok_arm, fail_arm],
            },
            result_ty,
            span,
        )
    }

    /// `lhs or { .. }` and `lhs or |e| { .. }`: the block is checked in the enclosing
    /// function and must produce the success type or leave (`return`, `break`, `continue`,
    /// `panic`).
    #[inline(never)]
    pub fn check_or_handler(
        &mut self,
        lhs: &ast::Expr,
        binding: Option<&Ident>,
        handler: &ast::Block,
        span: Span,
        expected: Option<&Ty>,
    ) -> Expr {
        let _ = expected;
        let a = self.check_expr(lhs, None);
        let Some(kind) = self.fallible(
            &a,
            "`or`",
            "`or` handles the failure of a `Result` or an `Option`",
        ) else {
            self.push_scope();
            if let Some(id) = binding {
                self.declare(&id.name, id.span, Ty::Error, LocalKind::Let);
            }
            self.check_block(handler, BlockMode::Stmts);
            self.pop_scope();
            return self.error_expr(span);
        };
        let t = kind.value_ty().clone();
        let scrutinee_ty = a.ty.clone();
        let mut binding = binding;
        if let (Fallible::Option { .. }, Some(id)) = (&kind, binding) {
            self.ck.report(
                Diagnostic::error(
                    format!(
                        "`or |{}|` needs a `Result`: an `Option` has no error to bind",
                        id.name
                    ),
                    id.span,
                )
                .with_hint("write `or { ... }` for an `Option`"),
            );
            binding = None;
        }
        let ok_arm = self.success_arm(&kind, &scrutinee_ty, None, span);

        self.push_scope();
        let named = binding.map_or(ErrorBinding::No, ErrorBinding::Named);
        let (fail_pat, _) = self.failure_pat(&kind, &scrutinee_ty, named, span);
        let block = self.check_block(handler, BlockMode::Value(Some(&t)));
        self.require_handler_value(&block, &t);
        let block_ty = block.ty.clone();
        let tail = self.mk(
            hir::ExprKind::Block(Box::new(block)),
            block_ty,
            handler.span,
        );
        let locals = self.pop_scope();
        let fail_arm = Self::arm(fail_pat, locals, tail, span);
        self.mk(
            hir::ExprKind::Match {
                scrutinee: Box::new(a),
                arms: vec![ok_arm, fail_arm],
            },
            t,
            span,
        )
    }

    /// The handler block must give the success type, or leave the function or loop.
    fn require_handler_value(&mut self, block: &Block, t: &Ty) {
        if self.infer.shallow(&block.ty) == Ty::Never || self.infer.coerce(&block.ty, t).is_ok() {
            return;
        }
        let (want, got) = (self.show(t), self.show(&block.ty));
        let diag = if block.tail.is_none() || got == "()" {
            Diagnostic::error(
                format!("this `or` block ends without a value: it must produce a `{want}` or leave with `return`, `break`, `continue` or `panic`"),
                block.span,
            )
            .with_hint(format!("end the block with a `{want}`, for example `or {{ log(\"failed\"); default }}`"))
        } else {
            Diagnostic::error(
                format!("this `or` block produces `{got}`, but it must produce `{want}`"),
                block.span,
            )
            .with_hint("the block takes the place of the value that failed: it must give the same type, or leave the function or loop")
        };
        self.ck.report(diag);
    }
}
