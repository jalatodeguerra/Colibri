//! Ranges as values (`let r = 0..10`, DESIGN §5.2) and slicing (`xs[1..3]`, `s[0..4]`,
//! DESIGN §12).

use colibri_syntax::ast::{self, ExprKind, IndexArg};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::hir::{self, Builtin, Expr, Lit};
use crate::types::{IntTy, OPTION_TYPE, Ty, VARIANT_NONE, VARIANT_SOME};

impl FnCtx<'_, '_> {
    /// `a..b` and `a..=b` as a value of type `Range[int]`.
    pub fn check_range_value(
        &mut self,
        start: Option<&ast::Expr>,
        end: Option<&ast::Expr>,
        inclusive: bool,
        span: Span,
    ) -> Expr {
        let (Some(start), Some(end)) = (start, end) else {
            for e in start.into_iter().chain(end) {
                self.check_expr(e, None);
            }
            self.ck.report(
                Diagnostic::error("an open-ended range is not a value", span)
                    .with_hint("open ranges work only as a slice: `xs[2..]`, `xs[..n]`; a range value needs both ends: `2..10`"),
            );
            return self.error_expr(span);
        };
        let lo = self.check_expr(start, Some(&Ty::INT));
        let hi = self.check_expr(end, Some(&lo.ty));
        if self.infer.unify(&lo.ty, &hi.ty).is_err() {
            let (a, b) = (self.show(&lo.ty), self.show(&hi.ty));
            self.ck.report(
                Diagnostic::error(
                    format!("the two ends of this range have different types: `{a}` and `{b}`"),
                    span,
                )
                .with_label(lo.span, format!("this is `{a}`"))
                .with_label(hi.span, format!("this is `{b}`")),
            );
            return self.error_expr(span);
        }
        let elem = self.default_ty(&lo.ty);
        match elem {
            Ty::Int(IntTy::I64) => {}
            Ty::Error => return self.error_expr(span),
            Ty::Int(other) => {
                self.ck.report(
                    Diagnostic::error(
                        format!("a range value is a `Range[int]`, found ends of type `{}`", other.name()),
                        span,
                    )
                    .with_hint("convert the ends with `int(x)`; a `for` loop accepts the narrower integer types directly"),
                );
                return self.error_expr(span);
            }
            other => {
                let shown = self.show(&other);
                self.ck.report(
                    Diagnostic::error(format!("a range needs integer ends, found `{shown}`"), span)
                        .with_hint("only ranges of integers exist so far"),
                );
                return self.error_expr(span);
            }
        }
        self.mk(
            hir::ExprKind::Range {
                start: Box::new(lo),
                end: Box::new(hi),
                inclusive,
            },
            Ty::Range(Box::new(Ty::INT)),
            span,
        )
    }

    /// Whether the single index argument is a range, so the index is a slice.
    pub(crate) fn slice_range<'e>(&self, args: &'e [IndexArg]) -> Option<&'e ast::Expr> {
        let [IndexArg::Expr(ix)] = args else {
            return None;
        };
        let mut e = ix;
        while let ExprKind::Paren(inner) = &e.kind {
            e = inner;
        }
        matches!(e.kind, ExprKind::Range { .. }).then_some(e)
    }

    /// `base[a..b]`: a copy of part of a list or a string.
    pub(crate) fn check_slice(&mut self, base: &ast::Expr, range: &ast::Expr, span: Span) -> Expr {
        let ExprKind::Range {
            start,
            end,
            inclusive,
        } = &range.kind
        else {
            return self.error_expr(span);
        };
        let recv = self.check_expr(base, None);
        let bounds: Vec<Expr> = [start, end]
            .into_iter()
            .map(|b| match b {
                Some(e) => {
                    let v = self.check_expr(e, Some(&Ty::INT));
                    let reason = Some((
                        range.span,
                        "a slice is taken by `int` positions".to_string(),
                    ));
                    if self.expect(&v, &Ty::INT, reason) {
                        self.some_int(v)
                    } else {
                        self.error_expr(e.span)
                    }
                }
                None => self.none_int(range.span),
            })
            .collect();
        let (op, ty) = match self.infer.shallow(&recv.ty) {
            Ty::Error => return self.error_expr(span),
            Ty::List(t) => (Builtin::ListSlice, Ty::List(t)),
            Ty::Str => (Builtin::StrSlice, Ty::Str),
            other => {
                let shown = self.show(&other);
                self.ck.report(
                    Diagnostic::error(format!("cannot slice a value of type `{shown}`"), span)
                        .with_hint("only lists and strings can be sliced"),
                );
                return self.error_expr(span);
            }
        };
        let flag = self.mk(
            hir::ExprKind::Lit(Lit::Bool(*inclusive)),
            Ty::Bool,
            range.span,
        );
        let mut args = vec![recv];
        args.extend(bounds);
        args.push(flag);
        self.builtin_call(op, args, ty, span)
    }

    fn some_int(&self, v: Expr) -> Expr {
        let span = v.span;
        self.mk(
            hir::ExprKind::Variant {
                ty: OPTION_TYPE,
                variant: VARIANT_SOME,
                args: vec![v],
            },
            Ty::Option(Box::new(Ty::INT)),
            span,
        )
    }

    fn none_int(&self, span: Span) -> Expr {
        self.mk(
            hir::ExprKind::Variant {
                ty: OPTION_TYPE,
                variant: VARIANT_NONE,
                args: Vec::new(),
            },
            Ty::Option(Box::new(Ty::INT)),
            span,
        )
    }
}
