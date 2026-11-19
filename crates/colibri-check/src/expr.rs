//! Checking expressions (bidirectional: an expected type flows in, the type of the result
//! flows out).

use std::rc::Rc;

use colibri_syntax::ast::{self, BinOp as AstBin, ExprKind, StrSeg, UnOp as AstUn};
use colibri_syntax::{Diagnostic, Span};

use crate::body::{BlockMode, FnCtx};
use crate::checker::{Dep, ValueRef};
use crate::format;
use crate::hir::{
    self, Anchor, ArithOp, BinOp, Block, CmpOp, Expr, Intrinsic, Lit, Num, Operand, Place, Stmt,
    UnOp,
};
use crate::infer::VarKind;
use crate::types::{IntTy, Ty};

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
            ExprKind::Unsafe(_) => self.unsupported("`unsafe` block", span),
            ExprKind::Error => self.error_expr(span),
        }
    }

    #[inline(never)]
    fn check_int(&mut self, n: u64, expected: Option<&Ty>, span: Span) -> Expr {
        match i64::try_from(n) {
            Ok(v) => self.literal(Lit::Int(v), VarKind::IntLit, expected, span),
            Err(_) => {
                self.ck.report(
                    Diagnostic::error("this integer literal does not fit in `int`", span)
                        .with_hint("`int` is a 64-bit signed integer: at most 9223372036854775807"),
                );
                self.error_expr(span)
            }
        }
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
    fn check_ident(&mut self, id: &ast::Ident, span: Span) -> Expr {
        let name = id.name.as_str();
        if let Some(local) = self.lookup_local(name) {
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
                self.add_dep(Dep::Global(g));
                let place = Place {
                    anchor: Anchor::Global(g),
                    path: Vec::new(),
                    ty: ty.clone(),
                    display: name.to_string(),
                };
                self.mk(hir::ExprKind::Read(Box::new(place)), ty, span)
            }
            Some(ValueRef::Fn(f)) => self.fn_value(f, None, None, span),
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
    fn check_hole(&mut self, expr: &ast::Expr, spec: Option<&str>) -> Expr {
        let value = self.check_expr(expr, None);
        let ty = self.default_ty(&value.ty);
        let span = value.span;
        if ty.is_error() {
            return value;
        }
        let spec = spec.filter(|s| !s.is_empty());
        if !matches!(
            ty,
            Ty::Int(_) | Ty::Float | Ty::Bool | Ty::Char | Ty::Str | Ty::Unit
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
                if let ExprKind::Int(n) = operand.kind
                    && n <= i64::MAX as u64 + 1
                {
                    let value = (n as i128).wrapping_neg() as i64;
                    return self.literal(Lit::Int(value), VarKind::IntLit, expected, span);
                }
                let value = self.check_expr(operand, expected);
                if !self.is_numeric(&value.ty) && !self.infer.shallow(&value.ty).is_error() {
                    let shown = self.show(&value.ty);
                    self.ck.report(Diagnostic::error(
                        format!("cannot negate a value of type `{shown}`"),
                        span,
                    ));
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
            AstUn::BitNot => self.unsupported("the `~` operator", span),
        }
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
                return self.unsupported("bitwise operator", span);
            }
            _ => return self.error_expr(span),
        };
        let l = self.check_expr(lhs, None);
        let r = self.check_expr(rhs, Some(&l.ty));
        if !self.unify_operands(op, &l, &r, span) {
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
        let l = self.check_expr(lhs, None);
        let r = self.check_expr(rhs, Some(&l.ty));
        if !self.unify_operands(AstBin::Is, &l, &r, span) {
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
        let hint = expected.filter(|t| matches!(t, Ty::Int(_) | Ty::Float | Ty::Str));
        let l = self.check_expr(lhs, hint);
        let r = self.check_expr(rhs, Some(&l.ty));
        if !self.unify_operands(op, &l, &r, span) {
            return self.error_expr(span);
        }
        let ok = match self.infer.shallow(&l.ty) {
            Ty::Error => true,
            Ty::Str => arith == ArithOp::Add,
            t => self.is_numeric(&t),
        };
        if !ok {
            let shown = self.show(&l.ty);
            let mut diag = Diagnostic::error(
                format!(
                    "`{}` cannot be applied to values of type `{shown}`",
                    op_symbol(op)
                ),
                span,
            );
            if shown == "str" {
                diag = diag.with_hint("`+` joins strings; there is no other arithmetic on `str`");
            }
            self.ck.report(diag);
            return self.error_expr(span);
        }
        let ty = l.ty.clone();
        let op = if self.infer.shallow(&ty) == Ty::Str {
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

    /// Makes both operands one type, or reports why that cannot be.
    fn unify_operands(&mut self, op: AstBin, l: &Expr, r: &Expr, span: Span) -> bool {
        if self.infer.unify(&l.ty, &r.ty).is_ok() {
            return true;
        }
        let (lt, rt) = (self.show(&l.ty), self.show(&r.ty));
        let mut diag = Diagnostic::error(
            format!("cannot apply `{}` to `{lt}` and `{rt}`", op_symbol(op)),
            span,
        )
        .with_label(l.span, format!("this is `{lt}`"))
        .with_label(r.span, format!("this is `{rt}`"));
        let numeric = |t: &str| matches!(t, "int" | "float" | "{integer}" | "{float}");
        if numeric(&lt) && numeric(&rt) {
            diag = diag.with_hint(
                "`int` and `float` never mix implicitly; write the literal as a float (`2.0`) or convert the value explicitly",
            );
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

fn op_symbol(op: AstBin) -> &'static str {
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
        _ => "?",
    }
}
