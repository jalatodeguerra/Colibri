//! Expressions.

use colibri_check::hir::{
    Anchor, ArithOp, BinOp, CmpOp, Coercion, Expr, ExprKind, Lit, Num, Operand, UnOp, UserOp,
};
use colibri_check::types::IntTy;
use colibri_syntax::Span;

use super::{FnCompiler, Kind, kind_of};
use crate::bytecode::{Op, Reg, Site};

/// Whether evaluating `e` may assign a local of the running frame. Conservative: anything that
/// can contain a statement counts. A call cannot (a callee has its own registers), except that a
/// `var self` receiver is written back.
pub(super) fn writes_locals(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Lit(_) | ExprKind::Read(_) | ExprKind::FnRef { .. } | ExprKind::Error => false,
        ExprKind::Unary(_, x) | ExprKind::Coerce(x, _) => writes_locals(x),
        ExprKind::Binary(_, a, b) | ExprKind::And(a, b) | ExprKind::Or(a, b) => {
            writes_locals(a) || writes_locals(b)
        }
        ExprKind::StrConcat(parts) => parts.iter().any(writes_locals),
        ExprKind::Call(c) => c.args.iter().any(|a| match a {
            colibri_check::hir::Arg::Value(e) => writes_locals(e),
            colibri_check::hir::Arg::Place(_) => true,
        }),
        _ => true,
    }
}

/// Whether `e` writes its destination only as its last act, so the destination may be a live
/// local that `e` also reads.
pub(super) fn dst_safe(e: &Expr) -> bool {
    matches!(
        &e.kind,
        ExprKind::Lit(_)
            | ExprKind::Read(_)
            | ExprKind::Unary(..)
            | ExprKind::Binary(..)
            | ExprKind::Call(_)
            | ExprKind::StrConcat(_)
            | ExprKind::Coerce(..)
    )
}

fn node_name(e: &ExprKind) -> &'static str {
    match e {
        ExprKind::Go(_) | ExprKind::GoIn(..) => "`go`",
        ExprKind::Struct { .. } => "struct values",
        ExprKind::Object { .. } => "class objects",
        ExprKind::Variant { .. } => "enum values",
        ExprKind::Tuple(_) => "tuples",
        ExprKind::ListLit(_) => "lists",
        ExprKind::MapLit(_) => "maps",
        ExprKind::ToWeak(_) | ExprKind::FromWeak(_) => "weak references",
        ExprKind::Project(..) => "projections",
        ExprKind::Match { .. } => "`match`",
        ExprKind::Coerce(..) => "this coercion",
        ExprKind::FnRef { .. } => "function values",
        ExprKind::Closure(_) => "closures",
        ExprKind::Range { .. } => "range values",
        ExprKind::Error => "unchecked expressions",
        _ => "this expression",
    }
}

fn imm16(op: ArithOp, k: i64) -> Option<i16> {
    match op {
        ArithOp::Add => i16::try_from(k).ok(),
        ArithOp::Sub => i16::try_from(k.checked_neg()?).ok(),
        _ => None,
    }
}

impl FnCompiler<'_> {
    /// Evaluates `e` and puts its value, owned, in `dst`. `dst` must be a temporary or a local
    /// that holds no value (see [`dst_safe`] for writing into a live local).
    pub(super) fn expr(&mut self, e: &Expr, dst: Reg) {
        let span = e.span;
        match &e.kind {
            ExprKind::Lit(lit) => self.lit(lit, dst, span),
            ExprKind::Read(place) => self.read_place(place, dst, span),
            ExprKind::Unary(op, x) => self.unary(*op, x, dst, span),
            ExprKind::Binary(op, l, r) => self.binary(*op, l, r, dst, span),
            ExprKind::And(l, r) => self.logic(true, l, r, dst, span),
            ExprKind::Or(l, r) => self.logic(false, l, r, dst, span),
            ExprKind::Call(c) => self.call(e, c, Some(dst)),
            ExprKind::StrConcat(parts) => {
                let refs: Vec<&Expr> = parts.iter().collect();
                let w = self.eval_window(0, &refs, false);
                let Ok(n) = u16::try_from(parts.len()) else {
                    self.fail("a string with too many parts");
                    return;
                };
                self.emit(
                    Op::StrConcat {
                        d: dst,
                        base: w.base,
                        n,
                    },
                    span,
                );
                self.free_window(w, span);
            }
            ExprKind::If { cond, then, else_ } => {
                self.if_expr(cond, then, else_.as_deref(), Some(dst), span);
            }
            ExprKind::Block(b) => self.block(b, Some(dst)),
            ExprKind::Loop { id, body } => {
                self.loop_expr(*id, body, span);
                self.emit(Op::LoadUnit { d: dst }, span);
            }
            ExprKind::For(f) => {
                self.for_expr(f, span);
                self.emit(Op::LoadUnit { d: dst }, span);
            }
            ExprKind::Coerce(inner, Coercion::F32ToFloat) => {
                let s = self.operand(inner, false);
                let site = self.add_site(Site::F32ToFloat);
                self.emit(Op::Coerce { d: dst, s, site }, span);
                self.free(s, span);
            }
            other => self.unsupported(node_name(other), span),
        }
    }

    /// Evaluates `e` for its effect only.
    pub(super) fn expr_discard(&mut self, e: &Expr) {
        let span = e.span;
        match &e.kind {
            ExprKind::If { cond, then, else_ } => {
                self.if_expr(cond, then, else_.as_deref(), None, span);
            }
            ExprKind::Block(b) => self.block(b, None),
            ExprKind::Loop { id, body } => self.loop_expr(*id, body, span),
            ExprKind::For(f) => self.for_expr(f, span),
            ExprKind::Call(c) => self.call(e, c, None),
            _ => {
                let t = self.alloc_for(&e.ty);
                self.expr(e, t);
                self.free(t, span);
            }
        }
    }

    /// A register holding the value of `e` for reading: a local's own register when that is
    /// safe (`later_writes`: a later operand may assign it), else a fresh temporary.
    pub(super) fn operand(&mut self, e: &Expr, later_writes: bool) -> Reg {
        if !later_writes
            && let ExprKind::Read(p) = &e.kind
            && p.path.is_empty()
            && let Anchor::Local(l) = p.anchor
            && self.storage(l) == colibri_check::hir::Storage::Slot
            && kind_of(&e.ty) != Kind::Owned
        {
            return self.local(l);
        }
        let t = self.alloc_for(&e.ty);
        self.expr(e, t);
        t
    }

    pub(super) fn lit(&mut self, lit: &Lit, dst: Reg, span: Span) {
        match lit {
            Lit::Unit => {
                self.emit(Op::LoadUnit { d: dst }, span);
            }
            Lit::Bool(b) => {
                self.emit(Op::LoadBool { d: dst, b: *b }, span);
            }
            Lit::Int(i) if i32::try_from(*i).is_ok() => {
                self.emit(
                    Op::LoadInt {
                        d: dst,
                        i: *i as i32,
                    },
                    span,
                );
            }
            other => {
                let k = self.add_const(colibri_core::ops::literal(other));
                self.emit(Op::LoadK { d: dst, k }, span);
            }
        }
    }

    fn unary(&mut self, op: UnOp, x: &Expr, dst: Reg, span: Span) {
        match op {
            UnOp::User(UserOp::Fn(f)) => {
                self.call_fn(f, &[x], None, &x.ty, Some(dst), span);
            }
            UnOp::User(UserOp::Bound { .. }) => {
                self.unsupported("operators on type parameters", span);
            }
            UnOp::Neg(Num::Int(IntTy::I64)) => {
                let a = self.operand(x, false);
                self.emit(Op::NegI { d: dst, a }, span);
                self.free(a, span);
            }
            UnOp::Neg(Num::Float) => {
                let a = self.operand(x, false);
                self.emit(Op::NegF { d: dst, a }, span);
                self.free(a, span);
            }
            UnOp::Not => {
                let a = self.operand(x, false);
                self.emit(Op::Not { d: dst, a }, span);
                self.free(a, span);
            }
            _ => {
                let a = self.operand(x, false);
                let site = self.add_site(Site::Unary(op));
                self.emit(Op::Unary { d: dst, a, site }, span);
                self.free(a, span);
            }
        }
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr, dst: Reg, span: Span) {
        match op {
            BinOp::User(UserOp::Fn(f)) => {
                self.call_fn(f, &[l, r], None, &l.ty, Some(dst), span);
                return;
            }
            BinOp::User(UserOp::Bound { .. }) => {
                self.unsupported("operators on type parameters", span);
                return;
            }
            BinOp::Arith(ao, Num::Int(IntTy::I64))
                if matches!(ao, ArithOp::Add | ArithOp::Sub)
                    && matches!(&r.kind, ExprKind::Lit(Lit::Int(k)) if imm16(ao, *k).is_some()) =>
            {
                let ExprKind::Lit(Lit::Int(k)) = &r.kind else {
                    unreachable!()
                };
                let i = imm16(ao, *k).unwrap_or(0);
                let a = self.operand(l, false);
                self.emit(Op::AddIK { d: dst, a, i }, span);
                self.free(a, span);
                return;
            }
            _ => {}
        }
        let a = self.operand(l, writes_locals(r));
        let b = self.operand(r, false);
        self.binary_regs(op, a, b, dst, span);
        self.free(b, span);
        self.free(a, span);
    }

    /// `dst = a op b` on registers: a typed instruction when the operand types allow it, else
    /// core's operator.
    pub(super) fn binary_regs(&mut self, op: BinOp, a: Reg, b: Reg, d: Reg, span: Span) {
        let typed = match op {
            BinOp::Arith(ao, Num::Int(IntTy::I64)) => match ao {
                ArithOp::Add => Some(Op::AddI { d, a, b }),
                ArithOp::Sub => Some(Op::SubI { d, a, b }),
                ArithOp::Mul => Some(Op::MulI { d, a, b }),
                ArithOp::Div => Some(Op::DivI { d, a, b }),
                ArithOp::Rem => Some(Op::RemI { d, a, b }),
            },
            BinOp::Arith(ao, Num::Float) => match ao {
                ArithOp::Add => Some(Op::AddF { d, a, b }),
                ArithOp::Sub => Some(Op::SubF { d, a, b }),
                ArithOp::Mul => Some(Op::MulF { d, a, b }),
                ArithOp::Div => Some(Op::DivF { d, a, b }),
                ArithOp::Rem => None,
            },
            BinOp::Cmp(c, Operand::Int(ty)) if ty != IntTy::U64 => Some(match c {
                CmpOp::Lt => Op::LtI { d, a, b },
                CmpOp::Le => Op::LeI { d, a, b },
                CmpOp::Gt => Op::LtI { d, a: b, b: a },
                CmpOp::Ge => Op::LeI { d, a: b, b: a },
                CmpOp::Eq => Op::EqI { d, a, b },
                CmpOp::Ne => Op::NeI { d, a, b },
            }),
            BinOp::Cmp(c, Operand::Float) => Some(match c {
                CmpOp::Lt => Op::LtF { d, a, b },
                CmpOp::Le => Op::LeF { d, a, b },
                CmpOp::Gt => Op::LtF { d, a: b, b: a },
                CmpOp::Ge => Op::LeF { d, a: b, b: a },
                CmpOp::Eq => Op::EqF { d, a, b },
                CmpOp::Ne => Op::NeF { d, a, b },
            }),
            _ => None,
        };
        match typed {
            Some(op) => {
                self.emit(op, span);
            }
            None => {
                let site = self.add_site(Site::Binary(op));
                self.emit(Op::Binary { d, a, b }, span);
                self.emit(Op::Site { site }, span);
            }
        }
    }

    /// `l && r` (`and`) and `l || r`: the left value is the result when it decides.
    fn logic(&mut self, and: bool, l: &Expr, r: &Expr, dst: Reg, span: Span) {
        self.expr(l, dst);
        let end = self.new_label();
        let op = if and {
            Op::JmpIfNot { r: dst, off: 0 }
        } else {
            Op::JmpIf { r: dst, off: 0 }
        };
        self.emit_to(op, span, end);
        self.expr(r, dst);
        self.bind(end);
    }
}
