//! Calls: the window of consecutive argument registers, intrinsics.

use colibri_check::hir::{Anchor, Arg, Call, Callee, Expr, FnId, Intrinsic, Place, Storage};
use colibri_check::types::Ty;
use colibri_syntax::Span;

use super::{FnCompiler, Item, Kind};
use crate::bytecode::{Op, Reg, Site};

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
        // The values must sit side by side, and nothing may be alive above the last of them: a
        // callee's window starts at the first value and covers every register above it.
        let contiguous = self.next_reg() == first + reserve + args.len()
            && arg_regs
                .iter()
                .enumerate()
                .all(|(i, &r)| usize::from(r) == first + reserve + i);
        if contiguous {
            return Window {
                base: first as Reg,
                allocated,
            };
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
        Window { base, allocated }
    }

    /// Frees the registers of a window whose arguments the callee only borrowed.
    pub(super) fn free_window(&mut self, w: Window, span: Span) {
        for r in w.allocated.into_iter().rev() {
            self.free(r, span);
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
            Callee::Host(_) => self.unsupported("host functions", e.span),
            Callee::Value => self.unsupported("calls of function values", e.span),
            Callee::Bound { .. } | Callee::Dyn { .. } => {
                self.unsupported("calls through interfaces", e.span);
            }
        }
    }

    /// A call of the function `f`. The callee's window starts at the first argument register,
    /// so its parameters are the caller's argument registers: nothing is copied, and the callee
    /// never releases them (the caller does, as the tree-walker's twins do). The receiver of a
    /// `var self` call is moved into register 0 last, and moved back after the call.
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
                    self.unsupported("`var self` calls on this place", span);
                    return;
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

    fn intrinsic(&mut self, which: &Intrinsic, c: &Call, e: &Expr, dst: Option<Reg>) {
        let span = e.span;
        let mut args: Vec<&Expr> = Vec::new();
        for a in &c.args {
            match a {
                Arg::Value(v) => args.push(v),
                Arg::Place(_) => {
                    self.unsupported("`var self` intrinsics", span);
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
            other => {
                let what = match other {
                    Intrinsic::Builtin(b) => format!("the built-in `{b:?}`"),
                    other => format!("`{other:?}`"),
                };
                self.unsupported(&what, span);
            }
        }
    }
}
