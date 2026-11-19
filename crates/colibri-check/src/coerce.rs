//! Interface values (DESIGN §9.2): the implicit conversion from an implementing type to an
//! interface type where one is expected, and the explicit `value as Interface`.
//!
//! Both produce `Coerce(ToDyn(impl))`: the value is boxed together with the implementation
//! that dispatches its methods, found here, statically. A struct stays a value inside the box
//! (copying the box is lazy, copy-on-write); a class is shared, as always.

use colibri_syntax::ast;
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::hir::{self, Coercion, Expr};
use crate::infer::VarKind;
use crate::traits::Source;
use crate::types::{Ty, TypeId};

impl FnCtx<'_, '_> {
    /// Converts `value` to an interface value when the context expects an interface type and
    /// the value is of an implementing type. Anything else is returned as it is: a mismatch is
    /// the caller's to report.
    pub fn coerce_to_expected(&mut self, value: Expr, expected: Option<&Ty>) -> Expr {
        let Some(expected) = expected else {
            return value;
        };
        let expected = self.infer.shallow(expected);
        if expected == Ty::Float && self.infer.shallow(&value.ty) == Ty::F32 {
            return self.widen_f32(value);
        }
        let Ty::Dyn(iface, args) = expected else {
            return value;
        };
        match self.infer.shallow(&value.ty) {
            Ty::Dyn(..) | Ty::Error | Ty::Never => return value,
            Ty::Infer(_) => match self.infer.unbound_kind(&value.ty) {
                Some(VarKind::IntLit | VarKind::FloatLit) => {
                    self.default_ty(&value.ty);
                }
                // Nothing is known about the value yet; unification decides later.
                _ => return value,
            },
            _ => {}
        }
        self.make_dyn(value, iface, args)
    }

    /// `f32` to `float`, the one implicit numeric conversion besides literals (DESIGN §5.1).
    pub fn widen_f32(&mut self, value: Expr) -> Expr {
        let span = value.span;
        self.mk(
            hir::ExprKind::Coerce(Box::new(value), Coercion::F32ToFloat),
            Ty::Float,
            span,
        )
    }

    /// Boxes `value` as the interface value `iface[args]`, reporting a type that does not
    /// implement the interface.
    pub fn make_dyn(&mut self, value: Expr, iface: TypeId, args: Vec<Ty>) -> Expr {
        let ty = self.infer.resolve(&value.ty);
        let result = Ty::Dyn(iface, args.clone());
        let span = value.span;
        let source = self.ck.env().implements(&ty, iface, &args);
        let coercion = match source {
            Err(blame) => {
                let (shown, name) = (self.show(&ty), self.show(&result));
                let blamed = self.show(&blame);
                let message = if blame == ty {
                    format!("`{shown}` does not implement `{name}`")
                } else {
                    format!("`{shown}` does not implement `{name}` because `{blamed}` does not")
                };
                let mut diag = Diagnostic::error(message, span);
                diag = diag.with_hint(if matches!(ty, Ty::Dyn(..)) {
                    "an interface value cannot be converted to another interface".to_string()
                } else {
                    format!("add `impl {name} for {shown} {{ ... }}` to use it as a `{name}`")
                });
                self.ck.report(diag);
                return self.mk(hir::ExprKind::Error, result, span);
            }
            Ok(Source::Bound) => Coercion::ToDynBound(iface),
            Ok(Source::Dyn) => return value,
            Ok(Source::User(id)) => Coercion::ToDyn(id),
            Ok(Source::Derived) => match self.ck.impl_of(&ty, iface) {
                Some(id) => Coercion::ToDyn(id),
                None => Coercion::ToDynBound(iface),
            },
        };
        self.mk(
            hir::ExprKind::Coerce(Box::new(value), coercion),
            result,
            span,
        )
    }

    /// `value as Type`: converts to an interface type.
    pub fn check_cast(&mut self, expr: &ast::Expr, ty: &ast::TypeExpr, span: Span) -> Expr {
        let target = self.resolve_type_infer(ty);
        let value = self.check_expr(expr, None);
        match &target {
            Ty::Error => value,
            Ty::Dyn(iface, args) => {
                if self.infer.shallow(&value.ty).is_error() {
                    return value;
                }
                self.default_ty(&value.ty);
                let mut out = self.make_dyn(value, *iface, args.clone());
                out.span = span;
                out
            }
            other => {
                let shown = self.show(other);
                let mut diag =
                    Diagnostic::error(format!("cannot convert to `{shown}` with `as`"), span);
                diag = if other.is_number() {
                    diag.with_hint(format!(
                        "`as` converts a value to an interface type (`circle as Shape`); convert numbers with a call: `{shown}(x)`"
                    ))
                } else {
                    diag.with_hint("`as` converts a value to an interface type (`circle as Shape`)")
                };
                self.ck.report(diag);
                self.error_expr(span)
            }
        }
    }
}
