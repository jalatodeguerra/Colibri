//! Calls: the window of consecutive argument registers, intrinsics.

use std::rc::Rc;

use colibri_check::hir::{Anchor, Arg, Call, Callee, Expr, FnId, Intrinsic, Place, Storage};
use colibri_check::types::Ty;
use colibri_syntax::Span;

use super::{FnCompiler, Item, Kind};
use crate::bytecode::{AnchorKind, IfaceSite, Op, PlaceSite, Reg, Site};

/// Registers `base..` that a call's callee sees as its first locals.
pub(super) struct Window {
    pub base: Reg,
    /// Every register allocated for the window, in allocation order (freed newest first).
    allocated: Vec<Reg>,
}

impl FnCompiler<'_> {
    /// Evaluates `args` left to right into consecutive temporaries after `reserve` placeholder
    /// registers. The values normally land contiguously on their own; when an argument leaves a
    /// value alive that must live to the statement end, they do not, and the values are copied
    /// (moved, for `consume`) into a fresh window above everything: the originals stay where
    /// they are and die in the order the tree-walker's temporaries stack would drop them.
    pub(super) fn eval_window(&mut self, reserve: usize, args: &[&Expr], consume: bool) -> Window {
        self.eval_window_then(reserve, args, consume, |_| ()).0
    }

    /// [`FnCompiler::eval_window`] that runs `after` once the arguments are evaluated and before
    /// the window's layout is settled: what it leaves alive above them (the anchor operands of a
    /// `mut self` receiver, evaluated after the arguments) pushes the window up.
    pub(super) fn eval_window_then<T>(
        &mut self,
        reserve: usize,
        args: &[&Expr],
        consume: bool,
        after: impl FnOnce(&mut Self) -> T,
    ) -> (Window, T) {
        let first = self.next_reg();
        let mut allocated = Vec::new();
        for _ in 0..reserve {
            allocated.push(self.alloc(Kind::Scalar));
        }
        let mut arg_regs = Vec::new();
        for a in args {
            let t = self.alloc_for(&a.ty);
            self.expr(a, t);
            self.own(t);
            allocated.push(t);
            arg_regs.push(t);
        }
        let extra = after(self);
        // The values must sit side by side, and nothing may be alive above the last of them: a
        // callee's window starts at the first value and covers every register above it.
        let contiguous = self.next_reg() == first + reserve + args.len()
            && arg_regs
                .iter()
                .enumerate()
                .all(|(i, &r)| usize::from(r) == first + reserve + i);
        if contiguous {
            return (
                Window {
                    base: first as Reg,
                    allocated,
                },
                extra,
            );
        }
        let base = self.next_reg() as Reg;
        for _ in 0..reserve {
            allocated.push(self.alloc(Kind::Scalar));
        }
        for (a, &s) in args.iter().zip(&arg_regs) {
            let d = self.alloc_for(&a.ty);
            let op = if consume {
                Op::Move { d, s }
            } else {
                Op::Copy { d, s }
            };
            self.emit(op, a.span);
            self.own(d);
            allocated.push(d);
        }
        (Window { base, allocated }, extra)
    }

    /// Frees the registers of a window whose arguments the callee only borrowed.
    pub(super) fn free_window(&mut self, w: Window, span: Span) {
        for r in w.allocated.into_iter().rev() {
            self.free(r, span);
        }
    }

    /// Drops the values of a window now, last first, because the callee that borrowed them is
    /// done (the tree-walker's callee releases its parameters when it returns).
    pub(super) fn drop_window(&mut self, w: Window) {
        for r in w.allocated.into_iter().rev() {
            self.drop_now(r);
            if self.temp_kind(r).is_some() && usize::from(r) + 1 == self.next_reg() {
                self.temps.pop();
            }
        }
    }

    /// Frees the registers of a window whose arguments an instruction took.
    pub(super) fn free_window_moved(&mut self, w: Window) {
        for r in w.allocated.into_iter().rev() {
            self.free_moved(r);
        }
    }

    pub(super) fn call(&mut self, e: &Expr, c: &Call, dst: Option<Reg>) {
        match &c.callee {
            Callee::Fn(f) => {
                let mut values = Vec::new();
                let mut receiver = None;
                for a in &c.args {
                    match a {
                        Arg::Value(v) => values.push(v),
                        Arg::Place(p) => receiver = Some(p),
                    }
                }
                self.call_fn(*f, &values, receiver, &e.ty, dst, e.span);
            }
            Callee::Intrinsic(which) => self.intrinsic(which, c, e, dst),
            Callee::Host(id) => self.call_host_fn(*id, c, e, dst),
            Callee::Value => self.call_value(e, c, dst),
            Callee::Bound { iface, method } | Callee::Dyn { iface, method } => {
                let is_dyn = matches!(c.callee, Callee::Dyn { .. });
                let mut values = Vec::new();
                let mut receiver = None;
                for a in &c.args {
                    match a {
                        Arg::Value(v) => values.push(v),
                        Arg::Place(p) => receiver = Some(p),
                    }
                }
                self.call_iface(*iface, *method, is_dyn, &values, receiver, e, dst);
            }
        }
    }

    /// A call of a function value: the function is the first value of the window, the arguments
    /// follow, and the callee's window starts at the first argument.
    fn call_value(&mut self, e: &Expr, c: &Call, dst: Option<Reg>) {
        let span = e.span;
        let mut args: Vec<&Expr> = Vec::new();
        for a in &c.args {
            match a {
                Arg::Value(v) => args.push(v),
                Arg::Place(_) => {
                    return self.unsupported("a place argument of a function value", span);
                }
            }
        }
        let d = match dst {
            Some(d) => d,
            None => self.alloc_for(&e.ty),
        };
        let w = self.eval_window(0, &args, false);
        self.emit(Op::CallValue { d, base: w.base }, span);
        self.free_window(w, span);
        if dst.is_none() {
            self.own(d);
            self.free(d, span);
        }
    }

    /// A call of a method of an interface. The receiver is the first value, or, for a `mut self`
    /// method, the place it lives in.
    #[allow(clippy::too_many_arguments)]
    fn call_iface(
        &mut self,
        iface: colibri_check::types::TypeId,
        method: u32,
        is_dyn: bool,
        args: &[&Expr],
        receiver: Option<&Place>,
        e: &Expr,
        dst: Option<Reg>,
    ) {
        let span = e.span;
        let Ok(argc) = u16::try_from(args.len() + usize::from(receiver.is_some())) else {
            return self.fail("a call with too many arguments");
        };
        let site = self.add_site(Site::Iface(Box::new(IfaceSite {
            iface,
            method,
            argc,
            cache: std::cell::Cell::new(None),
        })));
        if let Some(place) = receiver {
            return self.call_iface_on_place(site, args, place, &e.ty, dst, span);
        }
        let d = match dst {
            Some(d) => d,
            None => self.alloc_for(&e.ty),
        };
        let w = self.eval_window(0, args, false);
        let base = w.base;
        self.emit(
            if is_dyn {
                Op::CallDyn { d, base, site }
            } else {
                Op::CallBound { d, base, site }
            },
            span,
        );
        self.free_window(w, span);
        if dst.is_none() {
            self.own(d);
            self.free(d, span);
        }
    }

    /// A `mut self` call through an interface: like [`FnCompiler::call_fn_on_place`], with the
    /// receiver unwrapped from its interface value for the call and boxed again before it is
    /// written back (also when the call panics).
    fn call_iface_on_place(
        &mut self,
        site: u16,
        args: &[&Expr],
        place: &Place,
        ret: &Ty,
        dst: Option<Reg>,
        span: Span,
    ) {
        let via_temp = matches!(dst, Some(d) if usize::from(d) < self.nlocals);
        let d = match dst {
            Some(d) if !via_temp => d,
            _ => self.alloc_for(ret),
        };
        let (w, res) =
            self.eval_window_then(2, args, false, |c| c.resolve_anchor(place, false, span));
        self.emit_hold(&res, span);
        let place_site = |c: &mut Self, guarded: bool| {
            c.add_site(Site::Place(Box::new(PlaceSite {
                anchor: res.kind,
                path: place.path.clone().into_boxed_slice(),
                name: Rc::clone(&res.name),
                guarded,
            })))
        };
        if res.kind == AnchorKind::Local {
            let take = place_site(self, false);
            self.emit(
                Op::PlaceTake {
                    d: w.base,
                    a: res.a,
                    b: res.b,
                },
                span,
            );
            self.emit(Op::Site { site: take }, span);
        } else {
            self.emit_place_get(&res, &place.path, w.base, span);
        }
        let back = place_site(self, false);
        self.push_item(Item::Place {
            site: back,
            a: res.a,
            b: res.b,
            r: w.base,
        });
        self.push_item(Item::Rewrap {
            shell: w.base,
            recv: w.base + 1,
        });
        self.emit(
            Op::CallIfaceInout {
                d,
                base: w.base,
                site,
            },
            span,
        );
        let depth = self.cleanup_depth();
        self.truncate_stack(depth - 2);
        self.emit(
            Op::Rewrap {
                shell: w.base,
                recv: w.base + 1,
            },
            span,
        );
        self.emit(
            Op::PlaceSet {
                a: res.a,
                b: res.b,
                v: w.base,
            },
            span,
        );
        self.emit(Op::Site { site: back }, span);
        self.note_release(&place.ty);
        self.emit_unhold(&res, span);
        self.free_window(w, span);
        if let (true, Some(local)) = (via_temp, dst) {
            self.own(d);
            self.emit(Op::Move { d: local, s: d }, span);
            self.free_moved(d);
        } else if dst.is_none() {
            self.own(d);
            self.free(d, span);
        }
    }

    /// A call of the function `f`. The callee's window starts at the first argument register,
    /// so its parameters are the caller's argument registers: nothing is copied, and the callee
    /// never releases them (the caller does, as the tree-walker's twins do). The receiver of a
    /// `mut self` call is moved into register 0 last, and moved back after the call.
    pub(super) fn call_fn(
        &mut self,
        f: FnId,
        args: &[&Expr],
        receiver: Option<&Place>,
        ret: &Ty,
        dst: Option<Reg>,
        span: Span,
    ) {
        let recv_local = match receiver {
            None => None,
            Some(p) => match (&p.anchor, p.path.is_empty()) {
                (Anchor::Local(l), true) if self.storage(*l) == Storage::Slot => Some(*l),
                _ => {
                    return self.call_fn_on_place(f, args, p, ret, dst, span);
                }
            },
        };
        let d = match dst {
            Some(d) => d,
            None => self.alloc_for(ret),
        };
        let w = self.eval_window(usize::from(recv_local.is_some()), args, false);
        if let Some(l) = recv_local {
            let l = self.local(l);
            self.emit(Op::Move { d: w.base, s: l }, span);
            // If the call panics, the receiver goes back to its local before anything dies.
            self.push_item(Item::Recv { l, r: w.base });
        }
        let site = self.add_site(Site::Call(f));
        self.emit(
            Op::Call {
                d,
                base: w.base,
                site,
            },
            span,
        );
        if let Some(l) = recv_local {
            let l = self.local(l);
            self.emit(Op::Move { d: l, s: w.base }, span);
            let depth = self.cleanup_depth();
            self.truncate_stack(depth - 1);
        }
        self.free_window(w, span);
        if dst.is_none() {
            self.own(d);
            self.free(d, span);
        }
    }

    /// A `mut self` call on any place but a whole local: the arguments first, then the place's
    /// operands, once. The receiver is copied in (a plain local's field is moved out), the
    /// collection a place is an element of stays guarded for the whole call, and the callee's
    /// final value is written back, unguarded, on every way out, a panic included
    /// (ARCHITECTURE §4, DESIGN §11.6).
    fn call_fn_on_place(
        &mut self,
        f: FnId,
        args: &[&Expr],
        place: &Place,
        ret: &Ty,
        dst: Option<Reg>,
        span: Span,
    ) {
        // A result stored straight into a local could overwrite an index or key register that the
        // write-back below still reads (`i = xs[i].bump()`): it goes through a temporary.
        let via_temp = matches!(dst, Some(d) if usize::from(d) < self.nlocals);
        let d = match dst {
            Some(d) if !via_temp => d,
            _ => self.alloc_for(ret),
        };
        let (w, res) =
            self.eval_window_then(1, args, false, |c| c.resolve_anchor(place, false, span));
        self.emit_hold(&res, span);
        // A field of a plain local is moved out, so the callee owns it uniquely and changes it in
        // place; everything else is copied.
        if res.kind == AnchorKind::Local {
            let site = self.add_site(Site::Place(Box::new(PlaceSite {
                anchor: res.kind,
                path: place.path.clone().into_boxed_slice(),
                name: Rc::clone(&res.name),
                guarded: false,
            })));
            self.emit(
                Op::PlaceTake {
                    d: w.base,
                    a: res.a,
                    b: res.b,
                },
                span,
            );
            self.emit(Op::Site { site }, span);
        } else {
            self.emit_place_get(&res, &place.path, w.base, span);
        }
        let back = self.add_site(Site::Place(Box::new(PlaceSite {
            anchor: res.kind,
            path: place.path.clone().into_boxed_slice(),
            name: Rc::clone(&res.name),
            guarded: false,
        })));
        self.push_item(Item::Place {
            site: back,
            a: res.a,
            b: res.b,
            r: w.base,
        });
        let site = self.add_site(Site::Call(f));
        self.emit(
            Op::Call {
                d,
                base: w.base,
                site,
            },
            span,
        );
        // The write-back is the last thing the call does; a failure in it is one of the
        // statement, with the receiver already handed over.
        let depth = self.cleanup_depth();
        self.truncate_stack(depth - 1);
        self.emit(
            Op::PlaceSet {
                a: res.a,
                b: res.b,
                v: w.base,
            },
            span,
        );
        self.emit(Op::Site { site: back }, span);
        self.note_release(&place.ty);
        self.emit_unhold(&res, span);
        self.free_window(w, span);
        if let (true, Some(local)) = (via_temp, dst) {
            self.own(d);
            self.emit(Op::Move { d: local, s: d }, span);
            self.free_moved(d);
        } else if dst.is_none() {
            self.own(d);
            self.free(d, span);
        }
    }

    /// A type as a refusal names it: a user type by its name, not its table position.
    fn type_key_name(&self, key: colibri_check::hir::TypeKey) -> String {
        match key {
            colibri_check::hir::TypeKey::Adt(t) => match self.env.type_names.get(t.0 as usize) {
                Some(name) => format!("`{name}`"),
                None => format!("`{key:?}`"),
            },
            other => format!("`{other:?}`"),
        }
    }

    fn intrinsic(&mut self, which: &Intrinsic, c: &Call, e: &Expr, dst: Option<Reg>) {
        let span = e.span;
        let mut args: Vec<&Expr> = Vec::new();
        for a in &c.args {
            match a {
                Arg::Value(v) => args.push(v),
                Arg::Place(_) => {
                    self.unsupported("`mut self` intrinsics", span);
                    return;
                }
            }
        }
        match which {
            Intrinsic::Print if args.len() == 1 => {
                let r = self.operand(args[0], false);
                self.emit(Op::Print { r }, span);
                self.free(r, span);
                if let Some(d) = dst {
                    self.emit(Op::LoadUnit { d }, span);
                }
            }
            Intrinsic::Show if args.len() == 1 => {
                let d = match dst {
                    Some(d) => d,
                    None => self.alloc_for(&Ty::Str),
                };
                let r = self.operand(args[0], false);
                self.emit(Op::Show { d, r }, span);
                self.free(r, span);
                if dst.is_none() {
                    self.own(d);
                    self.free(d, span);
                }
            }
            Intrinsic::Format(spec) if args.len() == 1 => {
                let d = match dst {
                    Some(d) => d,
                    None => self.alloc_for(&Ty::Str),
                };
                let r = self.operand(args[0], false);
                let site = self.add_site(Site::Format(spec.clone()));
                self.emit(Op::Format { d, r, site }, span);
                self.free(r, span);
                if dst.is_none() {
                    self.own(d);
                    self.free(d, span);
                }
            }
            Intrinsic::Panic
            | Intrinsic::Assert
            | Intrinsic::AssertEq
            | Intrinsic::Convert { .. }
            | Intrinsic::Ptr(_)
            | Intrinsic::NumMethod(..) => {
                let d = match dst {
                    Some(d) => d,
                    None => self.alloc_for(&e.ty),
                };
                let w = self.eval_window(0, &args, true);
                let Ok(argc) = u16::try_from(args.len()) else {
                    self.fail("a call with too many arguments");
                    return;
                };
                let site = self.add_site(Site::Intrinsic {
                    which: which.clone(),
                    argc,
                });
                self.emit(
                    Op::Intrinsic {
                        d,
                        base: w.base,
                        site,
                    },
                    span,
                );
                // The intrinsic consumes (and releases) its arguments.
                for a in &args {
                    self.note_release(&a.ty);
                }
                self.free_window_moved(w);
                if dst.is_none() {
                    self.own(d);
                    self.free(d, span);
                }
            }
            // The receiver is only looked at: it dies with the caller's statement.
            Intrinsic::HostAlive if args.len() == 1 => {
                let d = match dst {
                    Some(d) => d,
                    None => self.alloc_for(&Ty::Bool),
                };
                let w = self.eval_window(0, &args, false);
                let site = self.add_site(Site::Intrinsic {
                    which: which.clone(),
                    argc: 1,
                });
                self.emit(
                    Op::Intrinsic {
                        d,
                        base: w.base,
                        site,
                    },
                    span,
                );
                self.free_window(w, span);
                if dst.is_none() {
                    self.own(d);
                    self.free(d, span);
                }
            }
            other => {
                let what = match other {
                    Intrinsic::Builtin(b) => format!("the built-in `{b:?}`"),
                    Intrinsic::ErrorIs(k) => format!("`ErrorIs` of {}", self.type_key_name(*k)),
                    Intrinsic::ErrorAs(k) => format!("`ErrorAs` of {}", self.type_key_name(*k)),
                    other => format!("`{other:?}`"),
                };
                self.unsupported(&what, span);
            }
        }
    }
}
