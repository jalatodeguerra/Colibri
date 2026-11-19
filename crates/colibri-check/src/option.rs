//! The built-in `Option[T]` (`T?`): `some(x)`, `none`, and the weak references that read as
//! `T?` (DESIGN §5.2, §11.5). `Option` is the prelude enum at [`OPTION_TYPE`], so matching,
//! equality and `Show` use the same machinery as user enums.

use colibri_syntax::ast;
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::hir::{self, Expr};
use crate::infer::VarKind;
use crate::types::{OPTION_TYPE, Ty, VARIANT_NONE, VARIANT_SOME};

impl FnCtx<'_, '_> {
    /// The value `none` of type `elem?`.
    pub fn none_of(&self, elem: &Ty, span: Span) -> Expr {
        self.mk(
            hir::ExprKind::Variant {
                ty: OPTION_TYPE,
                variant: VARIANT_NONE,
                args: Vec::new(),
            },
            Ty::Option(Box::new(elem.clone())),
            span,
        )
    }

    /// `none`: its element type comes from what is expected, or is inferred later.
    pub fn check_none(&mut self, expected: Option<&Ty>, span: Span) -> Expr {
        let elem = match expected.map(|t| self.infer.shallow(t)) {
            Some(Ty::Option(inner)) => *inner,
            _ => self.fresh_var(VarKind::General),
        };
        self.none_of(&elem, span)
    }

    /// `some(x)`.
    pub fn check_some(&mut self, args: &[ast::Arg], expected: Option<&Ty>, span: Span) -> Expr {
        let [arg] = args else {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!("`some` takes exactly one value, found {}", args.len()),
                    span,
                )
                .with_hint("write `some(x)`; use `none` for no value"),
            );
            return self.error_expr(span);
        };
        if let Some(name) = &arg.name {
            self.check_expr(&arg.value, None);
            self.ck.report(Diagnostic::error(
                "`some` does not take a named argument",
                name.span,
            ));
            return self.error_expr(span);
        }
        let hint = match expected.map(|t| self.infer.shallow(t)) {
            Some(Ty::Option(inner)) => Some(*inner),
            _ => None,
        };
        let value = self.check_expr(&arg.value, hint.as_ref());
        let ty = Ty::Option(Box::new(value.ty.clone()));
        self.mk(
            hir::ExprKind::Variant {
                ty: OPTION_TYPE,
                variant: VARIANT_SOME,
                args: vec![value],
            },
            ty,
            span,
        )
    }

    /// Converts a value stored into a `weak` field of class type `target`: a `target`, or a
    /// `target?` (so `none` clears it). `None` after reporting a mismatch.
    pub fn coerce_to_weak(
        &mut self,
        value: Expr,
        target: &Ty,
        reason: Option<(Span, String)>,
    ) -> Option<Expr> {
        let span = value.span;
        if self.infer.coerce(&value.ty, target).is_err() {
            let optional = Ty::Option(Box::new(target.clone()));
            if self.infer.unify(&value.ty, &optional).is_err() {
                self.report_mismatch(span, target, &value.ty, reason);
                return None;
            }
        }
        Some(self.mk(hir::ExprKind::ToWeak(Box::new(value)), target.clone(), span))
    }
}
