//! Places: locals and globals with an empty path (reads, assignments, `op=`).

use colibri_check::hir::ArithOp;
use colibri_check::hir::{Anchor, BinOp, Expr, ExprKind, Lit, Num, Place, Storage, UserOp};
use colibri_check::types::IntTy;
use colibri_syntax::Span;

use super::expr::dst_safe;
use super::{FnCompiler, is_scalar};
use crate::bytecode::{Op, Reg};

impl FnCompiler<'_> {
    fn unsupported_place(&mut self, place: &Place, span: Span) {
        let what = match (&place.anchor, place.path.is_empty()) {
            (Anchor::ClassField(..), _) => "class fields",
            (Anchor::ListElem(..), _) => "list elements",
            (Anchor::MapEntry(..), _) => "map entries",
            (_, false) => "struct and tuple fields",
            _ => "this place",
        };
        self.unsupported(what, span);
    }

    pub(super) fn read_place(&mut self, place: &Place, dst: Reg, span: Span) {
        if !place.path.is_empty() {
            return self.unsupported_place(place, span);
        }
        match &place.anchor {
            Anchor::Local(l) => {
                let r = self.local(*l);
                if self.storage(*l) == Storage::Cell {
                    self.emit(Op::CellGet { d: dst, l: r }, span);
                } else if r != dst {
                    self.emit(Op::Copy { d: dst, s: r }, span);
                }
            }
            Anchor::Global(g) => {
                let checked = self.env.checked_globals.get(g.0 as usize).copied() == Some(true);
                let op = if checked {
                    Op::GetGlobalChecked { d: dst, g: g.0 }
                } else {
                    Op::GetGlobal { d: dst, g: g.0 }
                };
                self.emit(op, span);
            }
            _ => self.unsupported_place(place, span),
        }
    }

    /// `place = rhs` and `place op= rhs`. The place's operands (there are none for a local or a
    /// global) come first, then the right-hand side, then the old value is read.
    pub(super) fn assign(&mut self, place: &Place, op: Option<BinOp>, rhs: &Expr, span: Span) {
        if !place.path.is_empty() {
            return self.unsupported_place(place, span);
        }
        if let Some(BinOp::User(user)) = op {
            let what = match user {
                UserOp::Fn(_) => "`op=` with a user operator",
                UserOp::Bound { .. } => "operators on type parameters",
            };
            return self.unsupported(what, span);
        }
        match &place.anchor {
            Anchor::Local(l) if self.storage(*l) == Storage::Slot => {
                self.assign_slot(*l, place, op, rhs, span);
            }
            Anchor::Local(l) => self.assign_cell(*l, place, op, rhs, span),
            Anchor::Global(g) => self.assign_global(*g, place, op, rhs, span),
            _ => self.unsupported_place(place, span),
        }
    }

    /// The value a store writes: `rhs`, or the old value combined with it (`old` is the register
    /// that already holds the place's value for `op=`). Returns the register with the result.
    fn new_value(
        &mut self,
        old: Option<Reg>,
        op: Option<BinOp>,
        place: &Place,
        rhs: &Expr,
        span: Span,
    ) -> Reg {
        match (op, old) {
            (Some(op), Some(old)) => {
                let t = self.alloc_for(&place.ty);
                let b = self.operand(rhs, false);
                self.binary_regs(op, old, b, t, span);
                self.free(b, span);
                t
            }
            _ => {
                let t = self.alloc_for(&place.ty);
                self.expr(rhs, t);
                t
            }
        }
    }

    fn assign_slot(
        &mut self,
        l: colibri_check::hir::LocalId,
        place: &Place,
        op: Option<BinOp>,
        rhs: &Expr,
        span: Span,
    ) {
        let lr = self.local(l);
        let scalar = is_scalar(&place.ty);
        match op {
            None if scalar && dst_safe(rhs) => self.expr(rhs, lr),
            None => {
                let t = self.alloc_for(&place.ty);
                self.expr(rhs, t);
                self.store_local(lr, t, scalar, span);
            }
            Some(op) if scalar => {
                // Typed in place: `sum += x` is one instruction on the local's register.
                if let BinOp::Arith(ArithOp::Add | ArithOp::Sub, Num::Int(IntTy::I64)) = op
                    && let ExprKind::Lit(Lit::Int(k)) = &rhs.kind
                    && let Some(i) = small(op, *k)
                {
                    self.emit(Op::AddIK { d: lr, a: lr, i }, span);
                    return;
                }
                let b = self.operand(rhs, false);
                self.binary_regs(op, lr, b, lr, span);
                self.free(b, span);
            }
            Some(op) => {
                let t = self.alloc_for(&place.ty);
                let b = self.operand(rhs, false);
                self.binary_regs(op, lr, b, t, span);
                self.free(b, span);
                self.store_local(lr, t, false, span);
            }
        }
    }

    fn store_local(&mut self, lr: Reg, t: Reg, scalar: bool, span: Span) {
        if scalar {
            self.emit(Op::Move { d: lr, s: t }, span);
            self.free(t, span);
        } else {
            self.emit(Op::StoreLocal { l: lr, s: t }, span);
            self.free_moved(t);
        }
    }

    fn assign_cell(
        &mut self,
        l: colibri_check::hir::LocalId,
        place: &Place,
        op: Option<BinOp>,
        rhs: &Expr,
        span: Span,
    ) {
        let lr = self.local(l);
        let old = op.map(|_| {
            let t = self.alloc_for(&place.ty);
            self.emit(Op::CellGet { d: t, l: lr }, span);
            t
        });
        let new = self.new_value(old, op, place, rhs, span);
        self.emit(Op::CellSet { l: lr, s: new }, span);
        self.free_moved(new);
        if let Some(old) = old {
            self.free(old, span);
        }
    }

    fn assign_global(
        &mut self,
        g: colibri_check::hir::GlobalId,
        place: &Place,
        op: Option<BinOp>,
        rhs: &Expr,
        span: Span,
    ) {
        if self.env.checked_globals.get(g.0 as usize).copied() == Some(true) {
            self.emit(Op::CheckGlobal { g: g.0 }, span);
        }
        let old = op.map(|_| {
            let t = self.alloc_for(&place.ty);
            self.emit(Op::GetGlobal { d: t, g: g.0 }, span);
            t
        });
        let new = self.new_value(old, op, place, rhs, span);
        self.emit(Op::SetGlobal { g: g.0, s: new }, span);
        self.free_moved(new);
        if let Some(old) = old {
            self.free(old, span);
        }
    }
}

fn small(op: BinOp, k: i64) -> Option<i16> {
    match op {
        BinOp::Arith(ArithOp::Add, _) => i16::try_from(k).ok(),
        BinOp::Arith(ArithOp::Sub, _) => i16::try_from(k.checked_neg()?).ok(),
        _ => None,
    }
}
