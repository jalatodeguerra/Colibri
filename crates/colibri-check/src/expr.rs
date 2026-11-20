//! Checking expressions (bidirectional: an expected type flows in, the type of the result
//! flows out).

use std::rc::Rc;

use colibri_syntax::ast::{self, BinOp as AstBin, ExprKind, StrSeg, UnOp as AstUn};
use colibri_syntax::{Diagnostic, Span};

use crate::body::{BlockMode, FnCtx};
use crate::checker::{Dep, ValueRef};
use crate::deprecated::DeprKey;
use crate::format;
use crate::hir::{
    self, Anchor, ArithOp, BinOp, BitOp, Block, CmpOp, Expr, Intrinsic, Lit, Num, Operand, Place,
    Stmt, UnOp,
};
use crate::infer::VarKind;
use crate::operators::operator_iface;
use crate::types::{IntTy, NEG, Ty};

impl FnCtx<'_, '_> {
    /// Checks `e`. `expected` is a hint that lets literals pick their type; the caller still
    /// verifies the result with [`FnCtx::expect`].
    pub fn check_expr(&mut self, e: &ast::Expr, expected: Option<&Ty>) -> Expr {
        if !self.enter() {
            self.ck.report(
                Diagnostic::error("this expression is nested too deeply", e.span)
                    .with_hint("split it into smaller expressions with `let`"),
            );
            return self.error_expr(e.span);
        }
        let out = self.check_expr_inner(e, expected);
        let out = self.coerce_to_expected(out, expected);
        self.leave();
        out
    }

    #[inline(never)]
    fn check_expr_inner(&mut self, e: &ast::Expr, expected: Option<&Ty>) -> Expr {
        let span = e.span;
        match &e.kind {
            ExprKind::Int(n) => self.check_int(*n, expected, span),
            ExprKind::Float(f) => self.literal(Lit::Float(*f), VarKind::FloatLit, expected, span),
            ExprKind::Char(c) => self.mk(hir::ExprKind::Lit(Lit::Char(*c)), Ty::Char, span),
            ExprKind::Bool(b) => self.mk(hir::ExprKind::Lit(Lit::Bool(*b)), Ty::Bool, span),
            ExprKind::Str(segs) => self.check_str(segs, span),
            ExprKind::Tuple(items) if items.is_empty() => {
                self.mk(hir::ExprKind::Lit(Lit::Unit), Ty::Unit, span)
            }
            ExprKind::Tuple(items) => self.check_tuple(items, expected, span),
            ExprKind::Paren(inner) => self.check_expr(inner, expected),
            ExprKind::Ident(id) => self.check_ident(id, span),
            ExprKind::Unary { op, expr } => self.check_unary(*op, expr, span, expected),
            ExprKind::Binary { op, lhs, rhs } => self.check_binary(*op, lhs, rhs, span, expected),
            ExprKind::Call { callee, args } => self.check_call(callee, args, expected, span),
            ExprKind::If { cond, then, else_ } => self.check_if(
                span,
                cond,
                then,
                else_.as_deref(),
                BlockMode::Value(expected),
            ),
            ExprKind::Block(b) => {
                let block = self.check_block(b, BlockMode::Value(expected));
                let ty = block.ty.clone();
                self.mk(hir::ExprKind::Block(Box::new(block)), ty, span)
            }
            ExprKind::None => self.check_none(expected, span),
            ExprKind::List(items) => self.check_list_lit(items, expected, span),
            ExprKind::Map(entries) => self.check_map_lit(entries, expected, span),
            ExprKind::SelfValue => self.check_self(span),
            ExprKind::DotVariant { name, args } => {
                self.check_dot_variant(name, args.as_deref(), expected, span)
            }
            ExprKind::StructLit { ty, fields } => self.check_struct_lit(ty, fields, expected, span),
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => self.check_range_value(start.as_deref(), end.as_deref(), *inclusive, span),
            ExprKind::Cast { expr, ty } => self.check_cast(expr, ty, span),
            ExprKind::Or { lhs, rhs } => self.check_or(lhs, rhs, span),
            ExprKind::OrHandler {
                lhs,
                binding,
                handler,
            } => self.check_or_handler(lhs, binding.as_ref(), handler, span, expected),
            ExprKind::Try(inner) => self.check_try(inner, span),
            ExprKind::Go(call) => self.check_go(call, span),
            ExprKind::Field { base, name } => self.check_field_expr(base, name, span),
            ExprKind::TupleIndex { base, index } => self.check_tuple_index_expr(base, *index, span),
            ExprKind::Index { base, args } => self.check_index_expr(base, args, e),
            ExprKind::IfLet {
                pat,
                scrutinee,
                then,
                else_,
            } => self.check_if_let(
                span,
                pat,
                scrutinee,
                then,
                else_.as_deref(),
                BlockMode::Value(expected),
            ),
            ExprKind::Match { scrutinee, arms } => {
                self.check_match(span, scrutinee, arms, BlockMode::Value(expected))
            }
            ExprKind::Closure(c) => self.check_closure(c, expected, span),
            ExprKind::Unsafe(b) => self.check_unsafe_block(b, expected, span),
            ExprKind::Error => self.error_expr(span),
        }
    }

    #[inline(never)]
    fn check_int(&mut self, n: u64, expected: Option<&Ty>, span: Span) -> Expr {
        // Whether the literal fits its final type is decided when that type is known
        // (`zonk`); a value above `i64::MAX` can only ever be a `u64`.
        let lit = match i64::try_from(n) {
            Ok(v) => Lit::Int(v),
            Err(_) => Lit::U64(n),
        };
        self.literal(lit, VarKind::IntLit, expected, span)
    }

    /// An untyped numeric literal: a fresh literal variable, bound at once to the expected type
    /// when that is a number (DESIGN §3.4).
    fn literal(&mut self, lit: Lit, kind: VarKind, expected: Option<&Ty>, span: Span) -> Expr {
        let ty = self.fresh_var(kind);
        if let Some(t) = expected {
            let _ = self.infer.unify(&ty, t);
        }
        self.mk(hir::ExprKind::Lit(lit), ty, span)
    }

    #[inline(never)]
    pub(crate) fn check_ident(&mut self, id: &ast::Ident, span: Span) -> Expr {
        let name = id.name.as_str();
        if let Some(local) = self.lookup_local(name) {
            self.flags[local.0 as usize].read = true;
            let ty = self.local_ty(local);
            let place = Place {
                anchor: Anchor::Local(local),
                path: Vec::new(),
                ty: ty.clone(),
                display: name.to_string(),
            };
            return self.mk(hir::ExprKind::Read(Box::new(place)), ty, span);
        }
        match self.ck.values.get(name).copied() {
            Some(ValueRef::Global(g)) => {
                let ty = self.ck.global_ty(g);
                self.add_dep(Dep::Global(g), span);
                self.ck.note_deprecated_use(DeprKey::Global(g), span);
                let place = Place {
                    anchor: Anchor::Global(g),
                    path: Vec::new(),
                    ty: ty.clone(),
                    display: name.to_string(),
                };
                self.mk(hir::ExprKind::Read(Box::new(place)), ty, span)
            }
            Some(ValueRef::Fn(f)) => {
                self.ck.note_deprecated_use(DeprKey::Fn(f), span);
                self.fn_value(f, None, None, span)
            }
            None => match name {
                "print" => {
                    self.ck.report(
                        Diagnostic::error("`print` is a function, not a value", span)
                            .with_hint("call it: `print(x)`"),
                    );
                    self.error_expr(span)
                }
                "some" => {
                    self.ck.report(
                        Diagnostic::error("`some` is a constructor, not a value", span)
                            .with_hint("call it: `some(x)`"),
                    );
                    self.error_expr(span)
                }
                "ok" | "err" => {
                    self.ck.report(
                        Diagnostic::error(format!("`{name}` is a constructor, not a value"), span)
                            .with_hint(format!("call it: `{name}(x)`")),
                    );
                    self.error_expr(span)
                }
                _ if Ty::numeric_by_name(name).is_some() => {
                    self.ck.report(
                        Diagnostic::error(format!("`{name}` is a type, not a value"), span)
                            .with_hint(format!(
                                "convert with `{name}(x)`, or use a constant such as `{name}.MAX`"
                            )),
                    );
                    self.error_expr(span)
                }
                _ if self.ck.type_by_name(name).is_some() => {
                    self.ck.report(
                        Diagnostic::error(format!("`{name}` is a type, not a value"), span)
                            .with_hint(format!(
                                "build one with `{name} {{ ... }}` (a struct) or `{name}.Variant` (an enum)"
                            )),
                    );
                    self.error_expr(span)
                }
                _ => {
                    self.report_unknown_name(name, span);
                    self.error_expr(span)
                }
            },
        }
    }

    // ---- strings ----

    #[inline(never)]
    fn check_str(&mut self, segs: &[StrSeg], span: Span) -> Expr {
        let mut parts: Vec<Expr> = Vec::new();
        let mut text = String::new();
        let flush = |text: &mut String, parts: &mut Vec<Expr>, this: &Self| {
            if !text.is_empty() {
                let lit = Lit::Str(Rc::from(std::mem::take(text).as_str()));
                parts.push(this.mk(hir::ExprKind::Lit(lit), Ty::Str, span));
            }
        };
        for seg in segs {
            match seg {
                StrSeg::Lit(s) => text.push_str(s),
                StrSeg::Interp { expr, spec } => {
                    flush(&mut text, &mut parts, self);
                    let part = self.check_hole(expr, spec.as_deref());
                    parts.push(part);
                }
            }
        }
        flush(&mut text, &mut parts, self);
        match parts.len() {
            0 => self.mk(hir::ExprKind::Lit(Lit::Str(Rc::from(""))), Ty::Str, span),
            1 if matches!(parts[0].kind, hir::ExprKind::Lit(_)) => {
                let mut only = parts.remove(0);
                only.span = span;
                only
            }
            _ => self.mk(hir::ExprKind::StrConcat(parts), Ty::Str, span),
        }
    }

    /// One `{expr:spec}` hole, as a resolved `Show` or format operation.
    pub(crate) fn check_hole(&mut self, expr: &ast::Expr, spec: Option<&str>) -> Expr {
        let value = self.check_expr(expr, None);
        let ty = self.default_ty(&value.ty);
        let span = value.span;
        if ty.is_error() {
            return value;
        }
        let spec = spec.filter(|s| !s.is_empty());
        if !matches!(
            ty,
            Ty::Int(_) | Ty::Float | Ty::F32 | Ty::Bool | Ty::Char | Ty::Str | Ty::Unit
        ) {
            let shown = self.show(&ty);
            if !matches!(
                ty,
                Ty::Tuple(_)
                    | Ty::Adt(..)
                    | Ty::Dyn(..)
                    | Ty::Param(..)
                    | Ty::List(_)
                    | Ty::Map(..)
                    | Ty::Set(_)
                    | Ty::Option(_)
                    | Ty::Fn(..)
                    | Ty::Range(_)
                    | Ty::Ptr(_)
                    | Ty::Bytes
            ) {
                self.ck.report(Diagnostic::error(
                    format!("a value of type `{shown}` cannot be shown in a string"),
                    span,
                ));
                return self.error_expr(span);
            }
            if spec.is_some() {
                self.ck.report(
                    Diagnostic::error(
                        format!("a value of type `{shown}` cannot take a format spec"),
                        span,
                    )
                    .with_hint("a spec applies to numbers, strings, chars and bools; format the fields one by one"),
                );
                return self.error_expr(span);
            }
        }
        let intrinsic = match spec {
            None if ty == Ty::Str => return value,
            None => Intrinsic::Show,
            Some(text) => {
                let checked = format::parse_spec(text).and_then(|s| {
                    format::validate(&s, &ty)?;
                    Ok(s)
                });
                match checked {
                    Ok(s) => Intrinsic::Format(Box::new(s)),
                    Err(message) => {
                        self.ck.report(
                            Diagnostic::error(
                                format!("invalid format spec `{text}`: {message}"),
                                span,
                            )
                            .with_hint("a spec looks like `.2`, `>8`, `*^10`, `05` or `x`"),
                        );
                        return self.error_expr(span);
                    }
                }
            }
        };
        self.mk(
            hir::ExprKind::Call(Box::new(hir::Call {
                callee: hir::Callee::Intrinsic(intrinsic),
                type_args: Vec::new(),
                args: vec![hir::Arg::Value(value)],
            })),
            Ty::Str,
            span,
        )
    }

    // ---- operators ----

    #[inline(never)]
    fn check_unary(
        &mut self,
        op: AstUn,
        operand: &ast::Expr,
        span: Span,
        expected: Option<&Ty>,
    ) -> Expr {
        match op {
            AstUn::Neg => {
                // `-9223372036854775808` has no positive counterpart in `int`, so a negated
                // literal is folded into one.
                if let ExprKind::Int(n) = operand.kind {
                    let Some(value) = i64::try_from(-(n as i128)).ok() else {
                        self.ck.report(
                            Diagnostic::error(
                                format!("literal -{n} does not fit in any integer type"),
                                span,
                            )
                            .with_hint(
                                "`int` is a 64-bit signed integer: at least -9223372036854775808",
                            ),
                        );
                        return self.error_expr(span);
                    };
                    return self.literal(Lit::Int(value), VarKind::IntLit, expected, span);
                }
                let value = self.check_expr(operand, expected);
                let operand_ty = self.infer.shallow(&value.ty);
                if self.left_uses_operator(&operand_ty, NEG) {
                    return self.check_user_neg(value, span);
                }
                if !self.is_numeric(&value.ty) && !self.infer.shallow(&value.ty).is_error() {
                    if matches!(self.infer.shallow(&value.ty), Ty::Infer(_)) {
                        self.report_unknown_operand("-", value.span, span);
                        return self.error_expr(span);
                    }
                    let shown = self.show(&value.ty);
                    self.ck.report(Diagnostic::error(
                        format!("cannot negate a value of type `{shown}`"),
                        span,
                    ));
                    return self.error_expr(span);
                }
                if let Ty::Int(t) = self.infer.shallow(&value.ty)
                    && !t.is_signed()
                {
                    self.report_unsigned_neg(t, span);
                    return self.error_expr(span);
                }
                let ty = value.ty.clone();
                self.mk(
                    hir::ExprKind::Unary(UnOp::Neg(Num::Int(IntTy::I64)), Box::new(value)),
                    ty,
                    span,
                )
            }
            AstUn::Not => {
                let value = self.check_expr(operand, Some(&Ty::Bool));
                if !self.expect(&value, &Ty::Bool, None) {
                    return self.error_expr(span);
                }
                self.mk(
                    hir::ExprKind::Unary(UnOp::Not, Box::new(value)),
                    Ty::Bool,
                    span,
                )
            }
            AstUn::BitNot => {
                let hint = expected.filter(|t| matches!(t, Ty::Int(_)));
                let value = self.check_expr(operand, hint);
                if !self.require_integer("~", &value, span) {
                    return self.error_expr(span);
                }
                let ty = value.ty.clone();
                self.mk(
                    hir::ExprKind::Unary(UnOp::BitNot(IntTy::I64), Box::new(value)),
                    ty,
                    span,
                )
            }
        }
    }

    /// `-x` on an unsigned integer has no value for most `x`; it is an error, not a wrap.
    pub(crate) fn report_unsigned_neg(&mut self, ty: IntTy, span: Span) {
        let name = ty.name();
        self.ck.report(
            Diagnostic::error(format!("cannot negate a value of type `{name}`"), span)
                .with_hint(format!(
                    "`{name}` is unsigned; subtract from zero with `wrapping_sub`, or convert first with `int(x)`"
                )),
        );
    }

    /// Whether `value` is (or may still become) an integer, for the bitwise operators;
    /// otherwise reports why not.
    pub(crate) fn require_integer(&mut self, symbol: &str, value: &Expr, span: Span) -> bool {
        self.require_integer_ty(symbol, &value.ty, value.span, span)
    }

    /// [`FnCtx::require_integer`] for a type whose value is at `value_span`.
    pub(crate) fn require_integer_ty(
        &mut self,
        symbol: &str,
        ty: &Ty,
        value_span: Span,
        span: Span,
    ) -> bool {
        let ty = self.infer.shallow(ty);
        let ok = match &ty {
            Ty::Int(_) | Ty::Error => true,
            _ => self.infer.unbound_kind(&ty) == Some(VarKind::IntLit),
        };
        if ok {
            return true;
        }
        let shown = self.show(&ty);
        let hint = match symbol {
            "&" | "|" if ty == Ty::Bool => {
                let logic = if symbol == "&" { "&&" } else { "||" };
                format!("`{symbol}` works on integers; use `{logic}` for booleans")
            }
            "^" if ty == Ty::Bool => {
                "`^` works on integers; compare booleans with `!=`".to_string()
            }
            "~" if ty == Ty::Bool => "`~` works on integers; use `!` for booleans".to_string(),
            _ => format!("`{symbol}` works on integer types (`int`, `u8`, `u32`, ...)"),
        };
        self.ck.report(
            Diagnostic::error(
                format!("`{symbol}` cannot be applied to a value of type `{shown}`"),
                span,
            )
            .with_label(value_span, format!("this is `{shown}`"))
            .with_hint(hint),
        );
        false
    }

    /// `& | ^ << >>` on integers (DESIGN §5.1). The operands of `& | ^` have one type; a shift
    /// takes its count in any integer type, and the result has the type of the left operand.
    #[inline(never)]
    fn check_bitwise(
        &mut self,
        op: AstBin,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        span: Span,
        expected: Option<&Ty>,
    ) -> Expr {
        let (bit, symbol) = match op {
            AstBin::BitAnd => (BitOp::And, "&"),
            AstBin::BitOr => (BitOp::Or, "|"),
            AstBin::BitXor => (BitOp::Xor, "^"),
            AstBin::Shl => (BitOp::Shl, "<<"),
            _ => (BitOp::Shr, ">>"),
        };
        let hint = expected.filter(|t| matches!(t, Ty::Int(_)));
        let mut l = self.check_expr(lhs, hint);
        let shift = matches!(bit, BitOp::Shl | BitOp::Shr);
        let mut r = if shift {
            let r = self.check_expr(rhs, None);
            self.default_ty(&r.ty);
            r
        } else {
            self.check_expr(rhs, Some(&l.ty))
        };
        let (lok, rok) = (
            self.require_integer(symbol, &l, span),
            self.require_integer(symbol, &r, span),
        );
        if !(lok && rok) {
            return self.error_expr(span);
        }
        if !shift && !self.unify_operands(op, &mut l, &mut r, span) {
            return self.error_expr(span);
        }
        let ty = l.ty.clone();
        self.mk(
            hir::ExprKind::Binary(BinOp::Bit(bit, IntTy::I64), Box::new(l), Box::new(r)),
            ty,
            span,
        )
    }

    #[inline(never)]
    fn check_binary(
        &mut self,
        op: AstBin,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        span: Span,
        expected: Option<&Ty>,
    ) -> Expr {
        let arith = match op {
            AstBin::Add => Some(ArithOp::Add),
            AstBin::Sub => Some(ArithOp::Sub),
            AstBin::Mul => Some(ArithOp::Mul),
            AstBin::Div => Some(ArithOp::Div),
            AstBin::Rem => Some(ArithOp::Rem),
            _ => None,
        };
        if let Some(arith) = arith {
            return self.check_arith(op, arith, lhs, rhs, span, expected);
        }
        let cmp = match op {
            AstBin::Eq => CmpOp::Eq,
            AstBin::Ne => CmpOp::Ne,
            AstBin::Lt => CmpOp::Lt,
            AstBin::Le => CmpOp::Le,
            AstBin::Gt => CmpOp::Gt,
            AstBin::Ge => CmpOp::Ge,
            AstBin::And | AstBin::Or => {
                let l = self.check_expr(lhs, Some(&Ty::Bool));
                let r = self.check_expr(rhs, Some(&Ty::Bool));
                let (lok, rok) = (
                    self.expect(&l, &Ty::Bool, None),
                    self.expect(&r, &Ty::Bool, None),
                );
                if !(lok && rok) {
                    return self.error_expr(span);
                }
                let kind = if op == AstBin::And {
                    hir::ExprKind::And(Box::new(l), Box::new(r))
                } else {
                    hir::ExprKind::Or(Box::new(l), Box::new(r))
                };
                return self.mk(kind, Ty::Bool, span);
            }
            AstBin::Is => return self.check_is(lhs, rhs, span),
            AstBin::BitOr | AstBin::BitXor | AstBin::BitAnd | AstBin::Shl | AstBin::Shr => {
                return self.check_bitwise(op, lhs, rhs, span, expected);
            }
            _ => return self.error_expr(span),
        };
        let mut l = self.check_expr(lhs, None);
        let mut r = self.check_expr(rhs, Some(&l.ty));
        if !self.unify_operands(op, &mut l, &mut r, span) {
            return self.error_expr(span);
        }
        let equality = matches!(cmp, CmpOp::Eq | CmpOp::Ne);
        let ok = if equality {
            self.supports_eq(&l.ty)
        } else {
            self.supports_ord(&l.ty)
        };
        if !ok {
            let shown = self.show(&l.ty);
            let mut diag = Diagnostic::error(
                format!(
                    "`{}` cannot compare values of type `{shown}`",
                    op_symbol(op)
                ),
                span,
            );
            if let Some(hint) = self.comparison_hint(&l.ty, equality) {
                diag = diag.with_hint(hint);
            }
            self.ck.report(diag);
            return self.error_expr(span);
        }
        self.mk(
            hir::ExprKind::Binary(
                BinOp::Cmp(cmp, Operand::Int(IntTy::I64)),
                Box::new(l),
                Box::new(r),
            ),
            Ty::Bool,
            span,
        )
    }

    /// `a is b`: whether two references are the same object (DESIGN §5.3).
    #[inline(never)]
    fn check_is(&mut self, lhs: &ast::Expr, rhs: &ast::Expr, span: Span) -> Expr {
        let mut l = self.check_expr(lhs, None);
        let mut r = self.check_expr(rhs, Some(&l.ty));
        if !self.unify_operands(AstBin::Is, &mut l, &mut r, span) {
            return self.error_expr(span);
        }
        let ty = self.infer.shallow(&l.ty);
        let reference = match &ty {
            Ty::List(_) | Ty::Map(..) | Ty::Set(_) => true,
            Ty::Adt(id, _) => self.ck.types[id.0 as usize].is_class(),
            Ty::Error => return self.error_expr(span),
            _ => false,
        };
        if !reference {
            let shown = self.show(&ty);
            let hint = if ty == Ty::Str {
                "strings are compared by content: use `==`".to_string()
            } else {
                "`is` tests whether two references are the same object (classes and collections); values are compared with `==`".to_string()
            };
            self.ck.report(
                Diagnostic::error(format!("`is` cannot be used on `{shown}`"), span)
                    .with_label(l.span, format!("this is `{shown}`"))
                    .with_hint(hint),
            );
            return self.error_expr(span);
        }
        self.mk(
            hir::ExprKind::Binary(BinOp::Is, Box::new(l), Box::new(r)),
            Ty::Bool,
            span,
        )
    }

    #[inline(never)]
    fn check_arith(
        &mut self,
        op: AstBin,
        arith: ArithOp,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        span: Span,
        expected: Option<&Ty>,
    ) -> Expr {
        // A `float` hint is left out on purpose: it would widen an `f32` operand, while
        // `a * b` on two `f32` is an `f32` product that widens afterwards. A literal adapts
        // later anyway.
        let hint = expected.filter(|t| matches!(t, Ty::Int(_) | Ty::F32 | Ty::Str));
        let mut l = self.check_expr(lhs, hint);
        let iface = operator_iface(arith);
        // A user type on the left chooses its operator impl by the right operand, which is
        // checked with what that impl takes as a hint.
        if let Some(iface) = iface {
            let left = self.infer.shallow(&l.ty);
            if self.left_uses_operator(&left, iface) {
                return self.check_user_arith(op, iface, l, rhs, span);
            }
        }
        let left_unknown = matches!(self.infer.shallow(&l.ty), Ty::Infer(_));
        let mut r = self.check_expr(rhs, Some(&l.ty));
        // A number on the left and a user type on the right: `2 * v`.
        if let Some(iface) = iface
            && let Some(done) = self.check_scalar_left(op, iface, &mut l, &mut r, span)
        {
            return done;
        }
        if !self.unify_operands(op, &mut l, &mut r, span) {
            return self.error_expr(span);
        }
        let ok = match self.infer.shallow(&l.ty) {
            Ty::Error => true,
            Ty::Str | Ty::Bytes => arith == ArithOp::Add,
            t => self.is_numeric(&t),
        };
        if !ok {
            if left_unknown && iface.is_some() {
                // The right operand fixed the type, but the left one had none when the
                // operator had to be chosen, so a type of the program could not be asked.
                self.report_unknown_operand(op_symbol(op), l.span, span);
                return self.error_expr(span);
            }
            let shown = self.show(&l.ty);
            let mut diag = Diagnostic::error(
                format!(
                    "`{}` cannot be applied to values of type `{shown}`",
                    op_symbol(op)
                ),
                span,
            );
            if matches!(self.infer.shallow(&l.ty), Ty::Adt(id, _) if id.0 >= crate::types::FIRST_USER_TYPE)
                && iface.is_none()
            {
                diag = diag.with_hint(format!(
                    "`{}` is defined for numbers only; there is no interface for it, so a type of your own cannot implement it (write a method such as `rem` instead)",
                    op_symbol(op)
                ));
            } else if shown == "str" {
                diag = diag.with_hint("`+` joins strings; there is no other arithmetic on `str`");
            } else if shown == "Bytes" {
                diag =
                    diag.with_hint("`+` joins two `Bytes`; there is no other arithmetic on them");
            }
            self.ck.report(diag);
            return self.error_expr(span);
        }
        let ty = l.ty.clone();
        let op = if matches!(self.infer.shallow(&ty), Ty::Str | Ty::Bytes) {
            BinOp::Concat
        } else {
            BinOp::Arith(arith, Num::Int(IntTy::I64))
        };
        self.mk(
            hir::ExprKind::Binary(op, Box::new(l), Box::new(r)),
            ty,
            span,
        )
    }

    /// The left operand of an operator has no type yet, so the operator of a type of the program
    /// cannot be chosen (it is chosen when the operator is checked, not at the end of the body).
    pub(crate) fn report_unknown_operand(&mut self, symbol: &str, operand: Span, span: Span) {
        self.ck.report(
            Diagnostic::error(
                format!("the type of the operand of `{symbol}` is not known yet"),
                span,
            )
            .with_label(operand, "its type is decided later in the function")
            .with_hint("give it a type where it is declared (for example `let xs: List[Money] = []`) or convert it, so that the operator can be chosen here"),
        );
    }

    /// Makes both operands one type, or reports why that cannot be. The one exception is an
    /// `f32` meeting a `float`: the `f32` operand widens and the operation is a `float` one
    /// (DESIGN §5.1).
    pub(crate) fn unify_operands(
        &mut self,
        op: AstBin,
        l: &mut Expr,
        r: &mut Expr,
        span: Span,
    ) -> bool {
        if self.infer.unify(&l.ty, &r.ty).is_ok() {
            return true;
        }
        match (self.infer.shallow(&l.ty), self.infer.shallow(&r.ty)) {
            (Ty::F32, Ty::Float) => {
                let narrow = std::mem::replace(l, self.error_expr(span));
                *l = self.widen_f32(narrow);
                return true;
            }
            (Ty::Float, Ty::F32) => {
                let narrow = std::mem::replace(r, self.error_expr(span));
                *r = self.widen_f32(narrow);
                return true;
            }
            _ => {}
        }
        let (lt, rt) = (self.show(&l.ty), self.show(&r.ty));
        let mut diag = Diagnostic::error(
            format!("cannot apply `{}` to `{lt}` and `{rt}`", op_symbol(op)),
            span,
        )
        .with_label(l.span, format!("this is `{lt}`"))
        .with_label(r.span, format!("this is `{rt}`"));
        if crate::body::numeric_hint(&lt, &rt).is_some() {
            diag = diag.with_hint(
                "numbers of different types never mix (only `f32` widens to `float`); convert one side explicitly, for example `int(x)` or `f32(x)`, or write the literal with the right type",
            );
        } else if matches!(op, AstBin::Rem)
            && [&l.ty, &r.ty].iter().any(|t| {
                matches!(self.infer.shallow(t), Ty::Adt(id, _) if id.0 >= crate::types::FIRST_USER_TYPE)
            })
        {
            diag = diag.with_hint(
                "`%` is defined for numbers only; there is no interface for it, so a type of your own cannot implement it (write a method such as `rem` instead)",
            );
        } else if let Some(hint) = self.hint_number_on_the_left(op, &l.ty, &r.ty) {
            diag = diag.with_hint(hint);
        } else if lt == "str" || rt == "str" {
            diag = diag.with_hint("both sides must have the same type");
        }
        self.ck.report(diag);
        false
    }

    // ---- if ----

    /// `if` as an expression ([`BlockMode::Value`]) or as a statement ([`BlockMode::Stmts`]).
    #[inline(never)]
    pub fn check_if(
        &mut self,
        span: Span,
        cond: &ast::Expr,
        then: &ast::Block,
        else_: Option<&ast::Expr>,
        mode: BlockMode<'_>,
    ) -> Expr {
        let cond = self.check_cond(cond);
        // Without `else` the value is `()`, so the branch's own value is dropped.
        let then_mode = if else_.is_some() {
            mode
        } else {
            BlockMode::Stmts
        };
        let then_block = self.check_block(then, then_mode);
        let Some(else_expr) = else_ else {
            return self.mk(
                hir::ExprKind::If {
                    cond: Box::new(cond),
                    then: Box::new(then_block),
                    else_: None,
                },
                Ty::Unit,
                span,
            );
        };
        let Some(else_block) = self.check_else(else_expr, mode) else {
            return self.error_expr(span);
        };
        let ty = self.join_branches(mode, &then_block, &else_block);
        self.mk(
            hir::ExprKind::If {
                cond: Box::new(cond),
                then: Box::new(then_block),
                else_: Some(Box::new(else_block)),
            },
            ty,
            span,
        )
    }

    /// The block of an `else` (a plain block, or another `if` / `if let`).
    pub fn check_else(&mut self, else_expr: &ast::Expr, mode: BlockMode<'_>) -> Option<Block> {
        match &else_expr.kind {
            ExprKind::Block(b) => Some(self.check_block(b, mode)),
            ExprKind::If { .. } | ExprKind::IfLet { .. } => {
                self.push_scope();
                let espan = else_expr.span;
                let inner = match &else_expr.kind {
                    ExprKind::If { cond, then, else_ } => {
                        self.check_if(espan, cond, then, else_.as_deref(), mode)
                    }
                    ExprKind::IfLet {
                        pat,
                        scrutinee,
                        then,
                        else_,
                    } => self.check_if_let(espan, pat, scrutinee, then, else_.as_deref(), mode),
                    _ => return None,
                };
                let locals = self.pop_scope();
                let ty = inner.ty.clone();
                Some(match mode {
                    BlockMode::Value(_) => Block {
                        locals,
                        stmts: Vec::new(),
                        tail: Some(Box::new(inner)),
                        ty,
                        span: espan,
                    },
                    BlockMode::Stmts => Block {
                        locals,
                        stmts: vec![Stmt {
                            span: espan,
                            kind: hir::StmtKind::Expr(inner),
                        }],
                        tail: None,
                        ty,
                        span: espan,
                    },
                })
            }
            _ => None,
        }
    }

    /// The type of an `if` / `else` pair: both branches must agree when it is a value.
    pub fn join_branches(
        &mut self,
        mode: BlockMode<'_>,
        then_block: &Block,
        else_block: &Block,
    ) -> Ty {
        let (then_ty, else_ty) = (
            self.infer.shallow(&then_block.ty),
            self.infer.shallow(&else_block.ty),
        );
        match mode {
            BlockMode::Stmts => {
                if then_ty == Ty::Never && else_ty == Ty::Never {
                    Ty::Never
                } else {
                    Ty::Unit
                }
            }
            BlockMode::Value(_) => {
                if then_ty == Ty::Never {
                    else_ty
                } else if else_ty == Ty::Never || self.infer.unify(&then_ty, &else_ty).is_ok() {
                    then_ty
                } else {
                    let (a, b) = (self.show(&then_block.ty), self.show(&else_block.ty));
                    let at = then_block.tail.as_ref().map_or(then_block.span, |t| t.span);
                    let bt = else_block.tail.as_ref().map_or(else_block.span, |t| t.span);
                    self.ck.report(
                        Diagnostic::error(
                            format!("`if` and `else` give different types: `{a}` and `{b}`"),
                            bt,
                        )
                        .with_label(at, format!("this branch is `{a}`"))
                        .with_hint(
                            "both branches of an `if` used as a value must have the same type",
                        ),
                    );
                    Ty::Error
                }
            }
        }
    }
}

pub(crate) fn op_symbol(op: AstBin) -> &'static str {
    match op {
        AstBin::Add => "+",
        AstBin::Sub => "-",
        AstBin::Mul => "*",
        AstBin::Div => "/",
        AstBin::Rem => "%",
        AstBin::Eq => "==",
        AstBin::Ne => "!=",
        AstBin::Lt => "<",
        AstBin::Le => "<=",
        AstBin::Gt => ">",
        AstBin::Ge => ">=",
        AstBin::Is => "is",
        AstBin::BitAnd => "&",
        AstBin::BitOr => "|",
        AstBin::BitXor => "^",
        AstBin::Shl => "<<",
        AstBin::Shr => ">>",
        _ => "?",
    }
}
