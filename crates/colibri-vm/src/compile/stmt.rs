//! Functions, blocks, statements, `if`, `loop` and `for` over ranges.

use colibri_check::hir::{
    Block, Expr, ForBinding, ForLoop, ForSource, LoopId, Stmt, StmtKind, Storage,
};
use std::rc::Rc;

use colibri_check::types::{IntTy, Ty};
use colibri_syntax::Span;

use super::{FnCompiler, Item, Kind, Label, LoopCtx, is_scalar, kind_of};
use crate::bytecode::{Op, Reg, Site};

/// What a `for` loop that is not over a range steps through.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Each {
    List,
    Map,
    Set,
    Iter,
}

impl FnCompiler<'_> {
    /// The whole body.
    pub(super) fn function(&mut self) {
        let body = self.body;
        let span = body.span;
        // A parameter that is captured and assigned lives in a cell, made on entry.
        for (i, local) in body
            .locals
            .iter()
            .enumerate()
            .take(body.param_count as usize)
        {
            if local.storage == Storage::Cell {
                let r = i as Reg;
                self.emit(Op::NewCell { l: r, s: r }, span);
            }
        }
        if matches!(body.ret, Ty::Unit | Ty::Never) || body.block.tail.is_none() {
            // Without a tail the value, if any, is returned by a `return` statement; falling
            // off the end is only possible for `()`.
            self.block(&body.block, None);
            self.emit_return(None, span);
        } else {
            let t = self.alloc_for(&body.ret);
            self.block(&body.block, Some(t));
            self.own(t);
            self.emit_return(Some(t), span);
        }
    }

    /// Leaves the function: every live cleanup runs, newest first, except the register being
    /// returned.
    fn emit_return(&mut self, value: Option<Reg>, span: Span) {
        self.emit_exit(0, value, span);
        let body = self.body;
        if body.mutates_self
            && body
                .locals
                .first()
                .is_some_and(|l| l.storage == Storage::Cell)
        {
            self.emit(Op::UnwrapCell { r: 0 }, span);
        }
        match value {
            Some(s) => self.emit(Op::Return { s }, span),
            None => self.emit(Op::ReturnUnit, span),
        };
    }

    /// A block: its statements and tail, then its locals die in reverse order. The value of
    /// the tail goes to `dst`; its temporaries die once it has its value.
    pub(super) fn block(&mut self, b: &Block, dst: Option<Reg>) {
        let depth = self.cleanup_depth();
        self.push_fold();
        for s in &b.stmts {
            self.stmt(s);
        }
        match &b.tail {
            Some(tail) => {
                let m = self.mark();
                match dst {
                    Some(d) => self.expr(tail, d),
                    None => self.expr_discard(tail),
                }
                self.end_group(m, tail.span);
            }
            None => {
                // A block that cannot end normally (it returns or breaks) has no value.
                if let Some(d) = dst
                    && b.ty != Ty::Never
                {
                    self.emit(Op::LoadUnit { d }, b.span);
                }
            }
        }
        if b.ty == Ty::Never {
            // The block cannot end normally: its exits (`return`, `break`) already dropped.
            self.truncate_stack(depth);
        } else {
            // The value of the block, if it has one, survives the scope's cleanups.
            let keep = dst.filter(|_| !is_scalar(&b.ty));
            self.end_scope(depth, keep, b.span);
        }
    }

    pub(super) fn stmt(&mut self, s: &Stmt) {
        self.count_statement(s.span);
        let span = s.span;
        if let StmtKind::Defer(deferred) = &s.kind {
            self.register_defer(deferred);
            // A statement like any other: a hook panic that is pending (inside a `Drop` hook)
            // surfaces here, with the defer already registered.
            let m = self.begin_group();
            self.end_group(m, span);
            return;
        }
        let m = self.begin_group();
        match &s.kind {
            StmtKind::Let { local, init } => {
                let l = self.local(*local);
                let ty = self.local_ty(*local);
                if self.storage(*local) == Storage::Cell {
                    let t = self.alloc_for(&init.ty);
                    self.expr(init, t);
                    self.emit(Op::NewCell { l, s: t }, span);
                    self.free_moved(t);
                    // The local exists when the statement's hook check runs: it dies first.
                    self.end_group_keeping(m, l, span);
                    self.push_item(Item::Reg(l));
                } else {
                    self.expr(init, l);
                    if kind_of(ty) == Kind::Scalar {
                        self.end_group(m, span);
                    } else {
                        self.end_group_keeping(m, l, span);
                    }
                    self.bind_local_cleanup(l, ty);
                }
            }
            StmtKind::Expr(e) => {
                self.expr_discard(e);
                self.end_group(m, span);
            }
            StmtKind::Assign { place, op, rhs } => {
                self.assign(place, *op, rhs, span);
                self.end_group(m, span);
            }
            StmtKind::Return(value) => {
                self.return_stmt(value.as_ref(), span);
                self.abandon(m);
            }
            StmtKind::Break(id) | StmtKind::Continue(id) => {
                let is_break = matches!(s.kind, StmtKind::Break(_));
                self.jump_out(*id, is_break, span);
                self.abandon(m);
            }
            StmtKind::InitGlobal { global, init } => {
                let Ok(g16) = u16::try_from(global.0) else {
                    self.fail("too many module-level values for the bytecode VM");
                    return;
                };
                let t = self.alloc_for(&init.ret);
                let base = self.next_reg();
                self.emit(
                    Op::CallInit {
                        d: t,
                        base: base as Reg,
                        g: g16,
                    },
                    span,
                );
                self.emit(Op::InitGlobal { g: global.0, s: t }, span);
                self.free_moved(t);
                self.note_release(&init.ret);
                self.end_group(m, span);
            }
            StmtKind::LetPat { pat, init } => {
                // The value is matched, then released at once; the bindings are the block's, not
                // the statement's, so they go below the group on the cleanup stack.
                let t = self.alloc_for(&init.ty);
                self.expr(init, t);
                self.own(t);
                let (cx, held) = self.pat_tests_must_match(pat, t, span);
                let (items, moved) = self.emit_binds(&cx, t, span);
                self.drop_unmoved(&held, &moved, span);
                self.drop_now(t);
                let n = items.len();
                self.insert_items(m.cleanups, &items);
                self.end_group(
                    super::Mark {
                        cleanups: m.cleanups + n,
                        ..m
                    },
                    span,
                );
            }
            StmtKind::Defer(_) => unreachable!("handled above"),
        }
    }

    fn return_stmt(&mut self, value: Option<&Expr>, span: Span) {
        let Some(e) = value else {
            return self.emit_return(None, span);
        };
        if matches!(e.ty, Ty::Unit | Ty::Never) {
            self.expr_discard(e);
            return self.emit_return(None, span);
        }
        // Return moves the register out, so a parameter (the caller's) is copied first, and so
        // is any local when a `defer` is still to run: it may read or change the variable, and
        // the value returned is the one computed here.
        let nparams = self.body.param_count as usize;
        let s = self.operand(e, false);
        let deferred = self.has_defer();
        let s = if usize::from(s) < nparams || (deferred && usize::from(s) < self.nlocals) {
            let t = self.alloc_for(&e.ty);
            self.emit(Op::Copy { d: t, s }, span);
            self.own(t);
            t
        } else {
            s
        };
        self.emit_return(Some(s), span);
    }

    /// `break` and `continue`: the cleanups of every scope left run, then the jump.
    fn jump_out(&mut self, id: LoopId, is_break: bool, span: Span) {
        let Some(ctx) = self.loops.iter().rev().find(|c| c.id == id) else {
            self.fail("a `break` or `continue` outside its loop");
            return;
        };
        let (depth, target) = (ctx.cleanups, if is_break { ctx.brk } else { ctx.cont });
        self.emit_exit(depth, None, span);
        self.jmp(target, span);
    }

    pub(super) fn if_expr(
        &mut self,
        cond: &Expr,
        then: &Block,
        else_: Option<&Block>,
        dst: Option<Reg>,
        span: Span,
    ) {
        let else_l = self.new_label();
        self.jump_if(cond, false, else_l);
        self.block(then, dst);
        if else_.is_none() && dst.is_none() {
            self.bind(else_l);
            return;
        }
        let end = self.new_label();
        self.jmp(end, span);
        self.bind(else_l);
        match else_ {
            Some(b) => self.block(b, dst),
            None => {
                if let Some(d) = dst {
                    self.emit(Op::LoadUnit { d }, span);
                }
            }
        }
        self.bind(end);
    }

    pub(super) fn loop_expr(&mut self, id: LoopId, body: &Block, span: Span) {
        let head = self.new_label();
        let cont = self.new_label();
        let brk = self.new_label();
        self.bind(head);
        let cleanups = self.cleanup_depth();
        self.loops.push(LoopCtx {
            id,
            cleanups,
            brk,
            cont,
        });
        self.block(body, None);
        self.bind(cont);
        self.emit_to(Op::Loop { off: 0 }, span, head);
        self.loops.pop();
        self.bind(brk);
    }

    /// `for` loops: over an integer range, a list, a map, a set or an iterator.
    pub(super) fn for_expr(&mut self, f: &ForLoop, span: Span) {
        match &f.source {
            ForSource::Range { .. } => self.for_range(f, span),
            ForSource::List { list, name } => {
                self.for_each(f, list, Each::List, Some(name.as_str()), span);
            }
            ForSource::Map(e) => self.for_each(f, e, Each::Map, None, span),
            ForSource::Set(e) => self.for_each(f, e, Each::Set, None, span),
            ForSource::Iter(e) => self.for_each(f, e, Each::Iter, None, span),
        }
    }

    /// A loop over a collection or an iterator. The source lives in the first of a few
    /// consecutive registers `a..`: with it a position (`a + 1`) and, for `for mut`, a copy of
    /// the element in flight (`a + 2`). The loop holds the collection's guard for its whole
    /// duration (the place guard for `for mut`); each trip binds a fresh item, and `for mut`
    /// writes the binding's final value back on every way out of the trip (DESIGN §11.6).
    fn for_each(&mut self, f: &ForLoop, source: &Expr, each: Each, name: Option<&str>, span: Span) {
        let m = self.mark();
        let places = f.places && each == Each::List;
        let a = self.alloc(Kind::Owned);
        if each != Each::Iter {
            self.alloc(Kind::Scalar);
        }
        if places {
            self.alloc(Kind::Scalar);
        }
        self.expr(source, a);
        self.own(a);
        if each != Each::Iter {
            self.emit(Op::LoadInt { d: a + 1, i: 0 }, span);
            if places {
                let n = Rc::<str>::from(name.unwrap_or(""));
                let site = self.add_site(Site::Name(n));
                self.emit(Op::HoldListPlace { r: a, site }, span);
            } else {
                self.emit(Op::HoldIter { r: a }, span);
            }
            self.push_item(Item::Hold);
        }
        let head = self.new_label();
        let cont = self.new_label();
        let exit = self.new_label();
        self.bind(head);
        // The trip's scope starts after the step: a failing step (a budget, an iterator's `next`)
        // is outside it, as in the tree-walker's `run_iteration`.
        let cleanups = self.cleanup_depth();
        // `for mut` writes back through its binding; `for mut _` has none and only holds the guard.
        let write_back = places && matches!(f.binding, ForBinding::Local(_));
        // Where the item goes: straight into a plain binding, else into a temporary.
        let (item, cell) = match &f.binding {
            ForBinding::Local(l) if self.storage(*l) == Storage::Slot => (self.local(*l), None),
            ForBinding::Local(l) => (self.alloc(Kind::Scalar), Some(self.local(*l))),
            ForBinding::Pat(pat) => (self.alloc_for(&pat.ty), None),
            ForBinding::None => (self.alloc(Kind::Scalar), None),
        };
        let next = match (each, write_back) {
            (Each::List, false) => Op::ForListNext { d: item, a },
            (Each::List, true) => Op::ForListNextVar { d: item, a },
            (Each::Map, _) => Op::ForMapNext { d: item, a },
            (Each::Set, _) => Op::ForSetNext { d: item, a },
            (Each::Iter, _) => Op::ForIterNext { d: item, a },
        };
        self.emit(next, span);
        self.emit_to(Op::Jmp { off: 0 }, span, exit);
        match &f.binding {
            ForBinding::Local(l) => {
                let l = *l;
                if let Some(lr) = cell {
                    self.emit(Op::NewCell { l: lr, s: item }, span);
                }
                if write_back {
                    // The write-back runs after the trip's hook check, as `run_iteration` writes
                    // back after `exit_scope` has folded the hook panics.
                    self.push_item(Item::ForBack {
                        l: self.local(l),
                        a,
                    });
                    self.push_fold();
                } else {
                    self.push_fold();
                    if cell.is_some() {
                        // A cell holds a value whatever the type of the item is.
                        self.push_item(Item::Reg(self.local(l)));
                    } else {
                        let ty = self.local_ty(l);
                        self.bind_local_cleanup(self.local(l), ty);
                    }
                }
            }
            ForBinding::Pat(pat) => {
                // The item is matched and released before the body runs; the bindings are the
                // trip's.
                self.push_fold();
                self.own(item);
                let (cx, held) = self.pat_tests_must_match(pat, item, pat.span);
                let (items, moved) = self.emit_binds(&cx, item, pat.span);
                for it in items {
                    self.push_item(it);
                }
                self.drop_unmoved(&held, &moved, pat.span);
                self.drop_now(item);
            }
            ForBinding::None => {
                self.push_fold();
                // `for _ in ..` binds nothing: the item dies at once.
                self.releases += 1;
                self.emit(Op::Drop { r: item }, span);
            }
        }
        self.loops.push(LoopCtx {
            id: f.id,
            cleanups,
            brk: exit,
            cont,
        });
        self.block(&f.body, None);
        self.end_scope(cleanups, None, span);
        self.bind(cont);
        self.jmp(head, span);
        self.loops.pop();
        self.bind(exit);
        // The guard is let go of, then the source.
        self.end_group(m, span);
    }

    /// `for i in a..b` and `a..=b`.
    fn for_range(&mut self, f: &ForLoop, span: Span) {
        let ForSource::Range {
            start,
            end,
            inclusive,
        } = &f.source
        else {
            return self.unsupported("`for` over this kind of value", span);
        };
        let counted = matches!(&start.ty, Ty::Int(t) if *t != IntTy::U64);
        if matches!(f.binding, ForBinding::Pat(_)) || !counted {
            return self.unsupported("`for` with this binding or range", span);
        }
        let m = self.mark();
        // The loop variable is the counter itself when nothing can assign it.
        let direct = match f.binding {
            ForBinding::Local(l) => {
                !f.places
                    && self.storage(l) == Storage::Slot
                    && !self.body.locals[l.0 as usize].mutable
            }
            _ => false,
        };
        let cur = match f.binding {
            ForBinding::Local(l) if direct => self.local(l),
            _ => self.alloc(Kind::Scalar),
        };
        self.expr(start, cur);
        let end_reg = self.alloc(Kind::Scalar);
        self.expr(end, end_reg);

        let exit = self.new_label();
        let head = self.new_label();
        let cont = self.new_label();
        self.jump_unless_in_range(cur, end_reg, *inclusive, exit, span);
        self.bind(head);
        // The per-iteration scope: a cell binding is dropped at every exit from the iteration.
        let cleanups = self.cleanup_depth();
        self.push_fold();
        if let ForBinding::Local(l) = f.binding
            && !direct
        {
            let lr = self.local(l);
            if self.storage(l) == Storage::Cell {
                let t = self.alloc(Kind::Scalar);
                self.emit(Op::Copy { d: t, s: cur }, span);
                self.emit(Op::NewCell { l: lr, s: t }, span);
                self.free_moved(t);
                self.push_item(Item::Reg(lr));
            } else {
                self.emit(Op::Copy { d: lr, s: cur }, span);
            }
        }
        self.loops.push(LoopCtx {
            id: f.id,
            cleanups,
            brk: exit,
            cont,
        });
        self.block(&f.body, None);
        self.end_scope(cleanups, None, span);
        self.bind(cont);
        self.range_next(cur, end_reg, *inclusive, head, exit, span);
        self.loops.pop();
        self.bind(exit);
        // The temporaries of the bounds live for the whole loop, like the source of the
        // tree-walker's iteration.
        self.emit_exit(m.cleanups, None, span);
        self.abandon(m);
    }

    fn jump_unless_in_range(
        &mut self,
        cur: Reg,
        end: Reg,
        inclusive: bool,
        exit: Label,
        span: Span,
    ) {
        if self.fuse {
            let op = if inclusive {
                Op::JmpNotLeI {
                    a: cur,
                    b: end,
                    off: 0,
                }
            } else {
                Op::JmpNotLtI {
                    a: cur,
                    b: end,
                    off: 0,
                }
            };
            self.emit_to(op, span, exit);
        } else {
            let t = self.alloc(Kind::Scalar);
            let op = if inclusive {
                Op::LeI {
                    d: t,
                    a: cur,
                    b: end,
                }
            } else {
                Op::LtI {
                    d: t,
                    a: cur,
                    b: end,
                }
            };
            self.emit(op, span);
            self.emit_to(Op::JmpIfNot { r: t, off: 0 }, span, exit);
            self.free(t, span);
        }
    }

    /// The end of an iteration: step the counter and go round again, or fall through.
    fn range_next(
        &mut self,
        cur: Reg,
        end: Reg,
        inclusive: bool,
        head: Label,
        exit: Label,
        span: Span,
    ) {
        if self.fuse {
            let op = if inclusive {
                Op::ForRangeNextIncl {
                    i: cur,
                    end,
                    off: 0,
                }
            } else {
                Op::ForRangeNext {
                    i: cur,
                    end,
                    off: 0,
                }
            };
            self.emit_to(op, span, head);
            return;
        }
        let t = self.alloc(Kind::Scalar);
        if inclusive {
            self.emit(
                Op::EqI {
                    d: t,
                    a: cur,
                    b: end,
                },
                span,
            );
            self.emit_to(Op::JmpIf { r: t, off: 0 }, span, exit);
            self.emit(
                Op::AddIK {
                    d: cur,
                    a: cur,
                    i: 1,
                },
                span,
            );
        } else {
            self.emit(
                Op::AddIK {
                    d: cur,
                    a: cur,
                    i: 1,
                },
                span,
            );
            self.emit(
                Op::LtI {
                    d: t,
                    a: cur,
                    b: end,
                },
                span,
            );
            self.emit_to(Op::JmpIfNot { r: t, off: 0 }, span, exit);
        }
        self.free(t, span);
        self.emit_to(Op::Loop { off: 0 }, span, head);
    }
}
