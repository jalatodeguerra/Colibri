//! Places: locals and globals with an empty path (reads, assignments, `op=`).

use std::rc::Rc;

use colibri_check::hir::ArithOp;
use colibri_check::hir::{
    Anchor, BinOp, Expr, ExprKind, FnId, Lit, Num, Place, Storage, UserOp, ValueProj,
};
use colibri_check::types::IntTy;
use colibri_syntax::Span;

use super::expr::{dst_safe, writes_locals};
use super::{FnCompiler, Item, Kind, is_scalar, kind_of};
use crate::bytecode::{AnchorKind, Op, PlaceSite, Reg, Site};

/// A place whose anchor operands have been evaluated, once, into registers that live to the end
/// of the statement (ARCHITECTURE §4): `a` and `b` are what the instructions of
/// [`AnchorKind`] expect.
pub(super) struct Resolved {
    pub kind: AnchorKind,
    pub a: Reg,
    pub b: Reg,
    /// The collection's source text, for guard messages.
    pub name: Rc<str>,
}

impl Resolved {
    /// Whether the anchor is a collection element, which a place hold guards.
    pub(super) fn holds(&self) -> bool {
        matches!(self.kind, AnchorKind::Elem | AnchorKind::Entry)
    }
}

impl FnCompiler<'_> {
    /// A list or map operand of an anchor: a plain local's own register when nothing evaluated
    /// later can assign it, else a temporary holding a reference (the tree-walker keeps one on
    /// its temporaries stack).
    fn anchor_operand(&mut self, e: &Expr, later_writes: bool) -> Reg {
        if !later_writes
            && let ExprKind::Read(p) = &e.kind
            && p.path.is_empty()
            && let Anchor::Local(l) = p.anchor
            && self.storage(l) == Storage::Slot
        {
            return self.local(l);
        }
        let t = self.alloc_for(&e.ty);
        self.expr(e, t);
        self.own(t);
        t
    }

    /// Evaluates the operands of `place`'s anchor once, left to right. `rhs_writes`: whatever is
    /// evaluated after the anchor (a right-hand side) may assign a local.
    pub(super) fn resolve_anchor(
        &mut self,
        place: &Place,
        rhs_writes: bool,
        span: Span,
    ) -> Resolved {
        let name: Rc<str> = Rc::from("");
        match &place.anchor {
            Anchor::Local(l) => Resolved {
                kind: if self.storage(*l) == Storage::Cell {
                    AnchorKind::Cell
                } else {
                    AnchorKind::Local
                },
                a: self.local(*l),
                b: 0,
                name,
            },
            Anchor::Global(g) => {
                if self.env.checked_globals.get(g.0 as usize).copied() == Some(true) {
                    self.emit(Op::CheckGlobal { g: g.0 }, span);
                }
                Resolved {
                    kind: AnchorKind::Global(g.0),
                    a: 0,
                    b: 0,
                    name,
                }
            }
            Anchor::ClassField(obj, idx) => {
                let Ok(idx) = u16::try_from(idx.0) else {
                    self.fail("a class with too many fields for the bytecode VM");
                    return Resolved {
                        kind: AnchorKind::Local,
                        a: 0,
                        b: 0,
                        name,
                    };
                };
                let o = self.anchor_operand(obj, rhs_writes);
                Resolved {
                    kind: AnchorKind::Field(idx),
                    a: o,
                    b: 0,
                    name,
                }
            }
            Anchor::ListElem(list, index, text) => {
                let l = self.anchor_operand(list, writes_locals(index) || rhs_writes);
                let i = self.operand(index, rhs_writes);
                Resolved {
                    kind: AnchorKind::Elem,
                    a: l,
                    b: i,
                    name: Rc::from(text.as_str()),
                }
            }
            Anchor::MapEntry(map, key, text) => {
                let m = self.anchor_operand(map, writes_locals(key) || rhs_writes);
                let k = self.operand(key, rhs_writes);
                // A key type with a user `Eq` may be stored as another, equal key.
                let b = if kind_of(&key.ty) == Kind::Owned {
                    let c = self.alloc(Kind::Owned);
                    self.emit(Op::CanonKey { d: c, m, k }, key.span);
                    self.own(c);
                    c
                } else {
                    k
                };
                Resolved {
                    kind: AnchorKind::Entry,
                    a: m,
                    b,
                    name: Rc::from(text.as_str()),
                }
            }
        }
    }

    fn place_site(&mut self, res: &Resolved, path: &[ValueProj], guarded: bool) -> u16 {
        self.add_site(Site::Place(Box::new(PlaceSite {
            anchor: res.kind,
            path: path.into(),
            name: Rc::clone(&res.name),
            guarded,
        })))
    }

    /// `d = the part of the resolved place` (a clone).
    pub(super) fn emit_place_get(
        &mut self,
        res: &Resolved,
        path: &[ValueProj],
        d: Reg,
        span: Span,
    ) {
        match res.kind {
            AnchorKind::Local if path.is_empty() => {
                self.emit(Op::Copy { d, s: res.a }, span);
            }
            AnchorKind::Local => {
                let site = self.add_site(Site::Path(path.into()));
                self.emit(Op::GetPath { d, s: res.a, site }, span);
            }
            AnchorKind::Elem if path.is_empty() => {
                let op = Op::ListGetAt {
                    d,
                    l: res.a,
                    i: res.b,
                };
                self.emit(op, span);
            }
            AnchorKind::Entry if path.is_empty() => {
                let op = Op::MapGetAt {
                    d,
                    m: res.a,
                    k: res.b,
                };
                self.emit(op, span);
            }
            _ => {
                let site = self.place_site(res, path, false);
                self.emit(
                    Op::PlaceGet {
                        d,
                        a: res.a,
                        b: res.b,
                    },
                    span,
                );
                self.emit(Op::Site { site }, span);
            }
        }
    }

    /// Writes `v` (moved) into the resolved place, releasing the old part.
    pub(super) fn emit_place_set(
        &mut self,
        res: &Resolved,
        path: &[ValueProj],
        v: Reg,
        guarded: bool,
        span: Span,
    ) {
        if res.kind == AnchorKind::Local && !path.is_empty() {
            let site = self.add_site(Site::Path(path.into()));
            self.emit(
                Op::SetPath {
                    l: res.a,
                    s: v,
                    site,
                },
                span,
            );
        } else if guarded && path.is_empty() && res.kind == AnchorKind::Elem {
            let op = Op::ListSetAt {
                l: res.a,
                i: res.b,
                v,
            };
            self.emit(op, span);
        } else if guarded && path.is_empty() && res.kind == AnchorKind::Entry {
            let op = Op::MapSetAt {
                m: res.a,
                k: res.b,
                v,
            };
            self.emit(op, span);
        } else {
            let site = self.place_site(res, path, guarded);
            self.emit(
                Op::PlaceSet {
                    a: res.a,
                    b: res.b,
                    v,
                },
                span,
            );
            self.emit(Op::Site { site }, span);
        }
    }

    /// Takes the place hold of the collection `res` is an element of, if it is one.
    pub(super) fn emit_hold(&mut self, res: &Resolved, span: Span) {
        if !res.holds() {
            return;
        }
        let site = self.place_site(res, &[], true);
        self.emit(Op::PlaceHold { a: res.a }, span);
        self.emit(Op::Site { site }, span);
        self.push_item(Item::Hold);
    }

    /// Releases the hold taken by `emit_hold`, on the normal path.
    pub(super) fn emit_unhold(&mut self, res: &Resolved, span: Span) {
        if !res.holds() {
            return;
        }
        let depth = self.cleanup_depth();
        self.truncate_stack(depth - 1);
        self.emit(Op::Unhold, span);
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
            Anchor::ListElem(..) | Anchor::MapEntry(..) => {
                let res = self.resolve_anchor(place, false, span);
                self.emit_place_get(&res, &place.path, dst, span);
            }
        }
    }

    /// `place = rhs` and `place op= rhs`. The place's operands (there are none for a local or a
    /// global) come first, then the right-hand side, then the old value is read.
    pub(super) fn assign(&mut self, place: &Place, op: Option<BinOp>, rhs: &Expr, span: Span) {
        if let Some(BinOp::User(UserOp::Bound { .. })) = op {
            return self.unsupported("operators on type parameters", span);
        }
        if place.path.is_empty() && !matches!(op, Some(BinOp::User(_))) {
            match &place.anchor {
                Anchor::Local(l) if self.storage(*l) == Storage::Slot => {
                    return self.assign_slot(*l, place, op, rhs, span);
                }
                Anchor::Local(l) => return self.assign_cell(*l, place, op, rhs, span),
                Anchor::Global(g) => return self.assign_global(*g, place, op, rhs, span),
                _ => {}
            }
        }
        // Everything else: the anchor's operands once, then the right-hand side, then the old
        // value for `op=`, then a guarded write.
        let res = self.resolve_anchor(place, writes_locals(rhs), span);
        let new = match op {
            Some(BinOp::User(UserOp::Fn(f))) => self.user_op_value(&res, f, place, rhs, span),
            _ => {
                let path: &[ValueProj] = &place.path;
                let read = |c: &mut Self, d: Reg| c.emit_place_get(&res, path, d, span);
                self.new_value(&read, op, place, rhs, span)
            }
        };
        self.emit_place_set(&res, &place.path, new, true, span);
        self.free_moved(new);
        self.note_release(&place.ty);
    }

    /// `old op rhs` with a user operator `f`: the right-hand side first, then the old value,
    /// read and combined with the place held, so the operator cannot change the shape of the
    /// collection the place is an element of (DESIGN §11.6).
    fn user_op_value(
        &mut self,
        res: &Resolved,
        f: FnId,
        place: &Place,
        rhs: &Expr,
        span: Span,
    ) -> Reg {
        let t = self.alloc_for(&place.ty);
        let w = self.eval_window(1, &[rhs], true);
        self.emit_hold(res, span);
        // The old value is the callee's first parameter. Until the call it is owned by this
        // scope, like any other argument; after it, the callee is done with both.
        self.emit_place_get(res, &place.path, w.base, span);
        self.push_item(Item::Reg(w.base));
        let site = self.add_site(Site::Call(f));
        self.emit(
            Op::Call {
                d: t,
                base: w.base,
                site,
            },
            span,
        );
        // The callee has returned: its parameters die, last first, as the tree-walker's
        // callee releases them.
        self.drop_window(w);
        self.emit_unhold(res, span);
        self.own(t);
        t
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
