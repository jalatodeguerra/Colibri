//! Conditions: jumping on a boolean expression, with compare-and-branch for integers.

use colibri_check::hir::{Arg, BinOp, CmpOp, Expr, ExprKind, Lit, Operand, UnOp};
use colibri_check::types::IntTy;

use super::expr::writes_locals;
use super::{FnCompiler, Kind, Label, kind_of};
use crate::bytecode::Op;

/// Whether evaluating `e` leaves nothing alive that must die at the statement end: no node has
/// an owned (non-inert) type and every node is a literal, a read, an operator or a call.
/// Such a condition may be split into jumps without changing when temporaries die.
fn clean(e: &Expr) -> bool {
    if kind_of(&e.ty) == Kind::Owned {
        return false;
    }
    match &e.kind {
        ExprKind::Lit(_) | ExprKind::Read(_) => true,
        ExprKind::Unary(_, x) | ExprKind::Coerce(x, _) => clean(x),
        ExprKind::Binary(_, a, b) | ExprKind::And(a, b) | ExprKind::Or(a, b) => {
            clean(a) && clean(b)
        }
        ExprKind::StrConcat(parts) => parts.iter().all(clean),
        ExprKind::Call(c) => c.args.iter().all(|a| match a {
            Arg::Value(v) => clean(v),
            Arg::Place(_) => false,
        }),
        _ => false,
    }
}

impl FnCompiler<'_> {
    /// Jumps to `target` when `e` is `sense`, falls through otherwise. Nothing the condition
    /// evaluated is alive afterwards (its temporaries die before either branch runs).
    pub(super) fn jump_if(&mut self, e: &Expr, sense: bool, target: Label) {
        let span = e.span;
        match &e.kind {
            ExprKind::Lit(Lit::Bool(b)) => {
                if *b == sense {
                    self.jmp(target, span);
                }
            }
            ExprKind::Unary(UnOp::Not, x) => self.jump_if(x, !sense, target),
            ExprKind::And(l, r) if clean(l) && clean(r) => {
                if sense {
                    let skip = self.new_label();
                    self.jump_if(l, false, skip);
                    self.jump_if(r, true, target);
                    self.bind(skip);
                } else {
                    self.jump_if(l, false, target);
                    self.jump_if(r, false, target);
                }
            }
            ExprKind::Or(l, r) if clean(l) && clean(r) => {
                if sense {
                    self.jump_if(l, true, target);
                    self.jump_if(r, true, target);
                } else {
                    let skip = self.new_label();
                    self.jump_if(l, true, skip);
                    self.jump_if(r, false, target);
                    self.bind(skip);
                }
            }
            ExprKind::Binary(BinOp::Cmp(c, Operand::Int(ty)), l, r)
                if self.fuse && *ty != IntTy::U64 && clean(l) && clean(r) =>
            {
                self.jump_int_cmp(*c, l, r, sense, target, span);
            }
            _ => {
                let m = self.mark();
                let t = self.alloc_for(&e.ty);
                self.expr(e, t);
                // The condition's temporaries die before the branch.
                self.drop_above_keeping(m, t, span);
                let op = if sense {
                    Op::JmpIf { r: t, off: 0 }
                } else {
                    Op::JmpIfNot { r: t, off: 0 }
                };
                self.emit_to(op, span, target);
                self.abandon(m);
            }
        }
    }

    /// Fused integer compare-and-branch. When an operand left something alive to drop, the
    /// comparison is computed into a register, the temporaries dropped, then the branch taken.
    fn jump_int_cmp(
        &mut self,
        c: CmpOp,
        l: &Expr,
        r: &Expr,
        sense: bool,
        target: Label,
        span: colibri_syntax::Span,
    ) {
        let m = self.mark();
        // Jump when `sense` holds: that is "jump unless !sense", so negate the comparison.
        let c = if sense { negate(c) } else { c };
        let imm = match &r.kind {
            ExprKind::Lit(Lit::Int(k)) => i16::try_from(*k).ok(),
            _ => None,
        };
        let a = self.operand(l, writes_locals(r));
        if let Some(k) = imm {
            let op = match c {
                CmpOp::Lt => Op::JmpNotLtIK { a, k, off: 0 },
                CmpOp::Le => Op::JmpNotLeIK { a, k, off: 0 },
                CmpOp::Gt => Op::JmpNotGtIK { a, k, off: 0 },
                CmpOp::Ge => Op::JmpNotGeIK { a, k, off: 0 },
                CmpOp::Eq => Op::JmpNotEqIK { a, k, off: 0 },
                CmpOp::Ne => Op::JmpNotNeIK { a, k, off: 0 },
            };
            self.emit_to(op, span, target);
            self.abandon(m);
            return;
        }
        let b = self.operand(r, false);
        let op = match c {
            CmpOp::Lt => Op::JmpNotLtI { a, b, off: 0 },
            CmpOp::Le => Op::JmpNotLeI { a, b, off: 0 },
            CmpOp::Gt => Op::JmpNotLtI { a: b, b: a, off: 0 },
            CmpOp::Ge => Op::JmpNotLeI { a: b, b: a, off: 0 },
            CmpOp::Eq => Op::JmpNotEqI { a, b, off: 0 },
            CmpOp::Ne => Op::JmpNotNeI { a, b, off: 0 },
        };
        self.emit_to(op, span, target);
        self.abandon(m);
    }

    /// Emits the drops of everything above `m` except the register `keep`, and forgets it.
    fn drop_above_keeping(&mut self, m: super::Mark, keep: u16, span: colibri_syntax::Span) {
        self.emit_drops_above(m.cleanups, Some(keep), span);
    }
}

fn negate(c: CmpOp) -> CmpOp {
    match c {
        CmpOp::Lt => CmpOp::Ge,
        CmpOp::Le => CmpOp::Gt,
        CmpOp::Gt => CmpOp::Le,
        CmpOp::Ge => CmpOp::Lt,
        CmpOp::Eq => CmpOp::Ne,
        CmpOp::Ne => CmpOp::Eq,
    }
}
