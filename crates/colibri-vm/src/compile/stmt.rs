//! Functions, blocks, statements, `if`, `loop` and `for` over ranges.

use colibri_check::hir::{
    Block, Expr, ForBinding, ForLoop, ForSource, LoopId, Stmt, StmtKind, Storage,
};
use colibri_check::types::{IntTy, Ty};
use colibri_syntax::Span;

use super::{FnCompiler, Kind, Label, LoopCtx};
use crate::bytecode::{Op, Reg};

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
        self.emit_drops_above(0, value, span);
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
                self.end_stmt(m, tail.span);
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
            self.cleanups.truncate(depth);
        } else {
            self.end_scope(depth, b.span);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        self.count_statement(s.span);
        let m = self.mark();
        let span = s.span;
        match &s.kind {
            StmtKind::Let { local, init } => {
                let l = self.local(*local);
                let ty = self.local_ty(*local);
                if self.storage(*local) == Storage::Cell {
                    let t = self.alloc_for(&init.ty);
                    self.expr(init, t);
                    self.emit(Op::NewCell { l, s: t }, span);
                    self.free_moved(t);
                    self.end_stmt(m, span);
                    self.cleanups.push(l);
                } else {
                    self.expr(init, l);
                    self.end_stmt(m, span);
                    self.bind_local_cleanup(l, ty);
                }
            }
            StmtKind::Expr(e) => {
                self.expr_discard(e);
                self.end_stmt(m, span);
            }
            StmtKind::Assign { place, op, rhs } => {
                self.assign(place, *op, rhs, span);
                self.end_stmt(m, span);
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
                self.end_stmt(m, span);
            }
            StmtKind::LetPat { .. } => {
                self.unsupported("`let` with a pattern", span);
                self.end_stmt(m, span);
            }
            StmtKind::Defer(_) => {
                self.unsupported("`defer`", span);
                self.end_stmt(m, span);
            }
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
        // Return moves the register out, so a parameter (the caller's) is copied first.
        let nparams = self.body.param_count as usize;
        let s = self.operand(e, false);
        let s = if usize::from(s) < nparams {
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
        self.emit_drops_above(depth, None, span);
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

    /// `for i in a..b` and `a..=b`. Other sources are not compiled yet.
    pub(super) fn for_expr(&mut self, f: &ForLoop, span: Span) {
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
        if let ForBinding::Local(l) = f.binding
            && !direct
        {
            let lr = self.local(l);
            if self.storage(l) == Storage::Cell {
                let t = self.alloc(Kind::Scalar);
                self.emit(Op::Copy { d: t, s: cur }, span);
                self.emit(Op::NewCell { l: lr, s: t }, span);
                self.free_moved(t);
                self.cleanups.push(lr);
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
        self.end_scope(cleanups, span);
        self.bind(cont);
        self.range_next(cur, end_reg, *inclusive, head, exit, span);
        self.loops.pop();
        self.bind(exit);
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
