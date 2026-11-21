//! Places: locals and globals with an empty path (reads, assignments, `op=`).

use colibri_check::hir::ArithOp;
use colibri_check::hir::{Anchor, BinOp, Expr, ExprKind, Lit, Num, Place, Storage, UserOp};
use colibri_check::types::IntTy;
use colibri_syntax::Span;

use super::expr::dst_safe;
use super::{FnCompiler, Kind, is_scalar};
use crate::bytecode::{Op, Reg, Site};

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
        let path = |c: &mut Self| c.add_site(Site::Path(place.path.clone().into_boxed_slice()));
        match &place.anchor {
            Anchor::Local(l) => {
                let r = self.local(*l);
                let cell = self.storage(*l) == Storage::Cell;
                if place.path.is_empty() {
                    if cell {
                        self.emit(Op::CellGet { d: dst, l: r }, span);
                    } else if r != dst {
                        self.emit(Op::Copy { d: dst, s: r }, span);
                    }
                } else if cell {
                    let t = self.alloc(Kind::Owned);
                    self.emit(Op::CellGet { d: t, l: r }, span);
                    let site = path(self);
                    self.emit(Op::GetPath { d: dst, s: t, site }, span);
                    self.emit(Op::Drop { r: t }, span);
                    self.free_moved(t);
                } else {
                    let site = path(self);
                    self.emit(Op::GetPath { d: dst, s: r, site }, span);
                }
            }
            Anchor::Global(g) => {
                let checked = self.env.checked_globals.get(g.0 as usize).copied() == Some(true);
                let op = |d| {
                    if checked {
                        Op::GetGlobalChecked { d, g: g.0 }
                    } else {
                        Op::GetGlobal { d, g: g.0 }
                    }
                };
                if place.path.is_empty() {
                    self.emit(op(dst), span);
                } else {
                    let t = self.alloc(Kind::Owned);
                    self.emit(op(t), span);
                    let site = path(self);
                    self.emit(Op::GetPath { d: dst, s: t, site }, span);
                    self.emit(Op::Drop { r: t }, span);
                    self.free_moved(t);
                }
            }
            Anchor::ClassField(obj, idx) => {
                let Ok(idx) = u16::try_from(idx.0) else {
                    return self.fail("a class with too many fields for the bytecode VM");
                };
                // The object is a temporary: it lives to the end of the statement.
                let o = self.alloc_for(&obj.ty);
                self.expr(obj, o);
                self.own(o);
                if place.path.is_empty() {
                    self.emit(Op::ObjGet { d: dst, o, idx }, span);
                } else {
                    let t = self.alloc(Kind::Owned);
                    self.emit(Op::ObjGet { d: t, o, idx }, span);
                    let site = path(self);
                    self.emit(Op::GetPath { d: dst, s: t, site }, span);
                    self.emit(Op::Drop { r: t }, span);
                    self.free_moved(t);
                }
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

    /// The value a store writes. For `op=` the right-hand side is evaluated first and the old
    /// value is read afterwards (`read` emits that read into a register), as the tree-walker
    /// does: the right-hand side may change the place. Returns the register with the result.
    fn new_value(
        &mut self,
        read: &dyn Fn(&mut Self, Reg),
        op: Option<BinOp>,
        place: &Place,
        rhs: &Expr,
        span: Span,
    ) -> Reg {
        let t = self.alloc_for(&place.ty);
        match op {
            Some(op) => {
                let b = self.operand(rhs, false);
                let old = self.alloc_for(&place.ty);
                read(self, old);
                self.own(old);
                self.binary_regs(op, old, b, t, span);
                self.free(old, span);
                self.free(b, span);
            }
            None => self.expr(rhs, t),
        }
        self.own(t);
        t
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
                self.store_local(l, lr, t, scalar, span);
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
                self.store_local(l, lr, t, false, span);
            }
        }
    }

    fn store_local(
        &mut self,
        l: colibri_check::hir::LocalId,
        lr: Reg,
        t: Reg,
        scalar: bool,
        span: Span,
    ) {
        if scalar {
            self.emit(Op::Move { d: lr, s: t }, span);
            self.free(t, span);
        } else {
            self.emit(Op::StoreLocal { l: lr, s: t }, span);
            self.free_moved(t);
            self.note_release(self.local_ty(l));
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
        let read = move |c: &mut Self, d: Reg| {
            c.emit(Op::CellGet { d, l: lr }, span);
        };
        let new = self.new_value(&read, op, place, rhs, span);
        self.emit(Op::CellSet { l: lr, s: new }, span);
        self.free_moved(new);
        self.note_release(&place.ty);
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
        let read = move |c: &mut Self, d: Reg| {
            c.emit(Op::GetGlobal { d, g: g.0 }, span);
        };
        let new = self.new_value(&read, op, place, rhs, span);
        self.emit(Op::SetGlobal { g: g.0, s: new }, span);
        self.free_moved(new);
        self.note_release(&place.ty);
    }
}

fn small(op: BinOp, k: i64) -> Option<i16> {
    match op {
        BinOp::Arith(ArithOp::Add, _) => i16::try_from(k).ok(),
        BinOp::Arith(ArithOp::Sub, _) => i16::try_from(k.checked_neg()?).ok(),
        _ => None,
    }
}
