//! `for` loops over integer ranges and `while let` (DESIGN §6.3).

use colibri_syntax::ast::{self, ExprKind, Label, PatKind as AstPat};
use colibri_syntax::{Diagnostic, Span};

use crate::body::{BlockMode, FnCtx, LocalKind, LoopFrame};
use crate::hir::{self, Arm, Block, Expr, ForLoop, LoopId, Pat, PatKind, Stmt};
use crate::types::Ty;

impl FnCtx<'_, '_> {
    fn new_loop(&mut self, label: Option<&Label>) -> LoopId {
        let id = LoopId(self.next_loop);
        self.next_loop += 1;
        self.loops.push(LoopFrame {
            id,
            label: label.map(|l| l.name.clone()),
        });
        id
    }

    /// `for i in a..b { body }` and `for i in a..=b { body }`.
    #[inline(never)]
    pub fn check_for(
        &mut self,
        span: Span,
        label: Option<&Label>,
        mutable: bool,
        pat: &ast::Pat,
        iter: &ast::Expr,
        body: &ast::Block,
    ) -> Expr {
        let mut head = iter;
        while let ExprKind::Paren(inner) = &head.kind {
            head = inner;
        }
        let ExprKind::Range {
            start,
            end,
            inclusive,
        } = &head.kind
        else {
            return self.for_over_other(span, label, mutable, pat, iter, body);
        };
        let (Some(start), Some(end)) = (start, end) else {
            self.ck
                .unsupported("an open-ended range in a `for` loop", head.span);
            return self.for_body_only(label, mutable, pat, &Ty::Error, body, span);
        };
        let lo = self.check_expr(start, None);
        let hi = self.check_expr(end, Some(&lo.ty));
        let mut elem = Ty::Error;
        if self.infer.unify(&lo.ty, &hi.ty).is_err() {
            let (a, b) = (self.show(&lo.ty), self.show(&hi.ty));
            self.ck.report(
                Diagnostic::error(
                    format!("the two ends of this range have different types: `{a}` and `{b}`"),
                    head.span,
                )
                .with_label(lo.span, format!("this is `{a}`"))
                .with_label(hi.span, format!("this is `{b}`")),
            );
        } else {
            let ty = self.default_ty(&lo.ty);
            match ty {
                Ty::Int(_) => elem = ty,
                Ty::Error => {}
                other => {
                    let shown = self.show(&other);
                    self.ck.report(
                        Diagnostic::error(
                            format!("a `for` range needs integer ends, found `{shown}`"),
                            head.span,
                        )
                        .with_hint("only ranges of integers can be looped over so far"),
                    );
                }
            }
        }
        let id = self.new_loop(label);
        let (binding, block) = self.loop_body(mutable, pat, &elem, body);
        self.loops.pop();
        self.mk(
            hir::ExprKind::For(Box::new(ForLoop {
                id,
                binding,
                start: lo,
                end: hi,
                inclusive: *inclusive,
                body: block,
            })),
            Ty::Unit,
            span,
        )
    }

    /// A `for` over something that is not an integer range: not supported yet, but its parts
    /// are still checked.
    fn for_over_other(
        &mut self,
        span: Span,
        label: Option<&Label>,
        mutable: bool,
        pat: &ast::Pat,
        iter: &ast::Expr,
        body: &ast::Block,
    ) -> Expr {
        let value = self.check_expr(iter, None);
        if !value.ty.is_error() {
            let shown = self.show(&value.ty);
            self.ck
                .unsupported(&format!("`for` over a `{shown}`"), iter.span);
        }
        self.for_body_only(label, mutable, pat, &Ty::Error, body, span)
    }

    /// Checks a loop body whose loop cannot be built, so its own errors still show.
    fn for_body_only(
        &mut self,
        label: Option<&Label>,
        mutable: bool,
        pat: &ast::Pat,
        elem: &Ty,
        body: &ast::Block,
        span: Span,
    ) -> Expr {
        self.new_loop(label);
        self.loop_body(mutable, pat, elem, body);
        self.loops.pop();
        self.error_expr(span)
    }

    /// The body block of a `for`, with the loop variable as its first local.
    fn loop_body(
        &mut self,
        mutable: bool,
        pat: &ast::Pat,
        elem: &Ty,
        body: &ast::Block,
    ) -> (Option<hir::LocalId>, Block) {
        self.push_scope();
        let binding = match &pat.kind {
            AstPat::Ident(id) => {
                let kind = if mutable {
                    LocalKind::Var
                } else {
                    LocalKind::Let
                };
                Some(self.declare(&id.name, id.span, elem.clone(), kind))
            }
            AstPat::Wild | AstPat::Error => None,
            _ => {
                self.ck.unsupported("a pattern in a `for` loop", pat.span);
                None
            }
        };
        let (stmts, tail, ty) = self.block_in_scope(body, BlockMode::Stmts);
        let locals = self.pop_scope();
        (
            binding,
            Block {
                locals,
                stmts,
                tail,
                ty,
                span: body.span,
            },
        )
    }

    /// `while let pat = scrutinee { body }`: `loop { match scrutinee { pat => body, _ => break } }`.
    #[inline(never)]
    pub fn check_while_let(
        &mut self,
        span: Span,
        label: Option<&Label>,
        pat: &ast::Pat,
        scrutinee: &ast::Expr,
        body: &ast::Block,
    ) -> Expr {
        let id = LoopId(self.next_loop);
        self.next_loop += 1;
        let scrut = self.check_expr(scrutinee, None);
        let sty = self.default_ty(&scrut.ty);
        self.loops.push(LoopFrame {
            id,
            label: label.map(|l| l.name.clone()),
        });
        let (checked, body_expr, locals) = self.pattern_scope(pat, &sty, |this| {
            let block = this.check_block(body, BlockMode::Stmts);
            let ty = block.ty.clone();
            this.mk(hir::ExprKind::Block(Box::new(block)), ty, body.span)
        });
        self.loops.pop();
        let run = Block {
            locals,
            stmts: Vec::new(),
            tail: Some(Box::new(body_expr)),
            ty: Ty::Unit,
            span: body.span,
        };
        let exit = Block {
            locals: Vec::new(),
            stmts: vec![Stmt {
                span,
                kind: hir::StmtKind::Break(id),
            }],
            tail: None,
            ty: Ty::Never,
            span,
        };
        let arms = vec![
            Arm {
                pat: checked,
                guard: None,
                body: run,
            },
            Arm {
                pat: Pat {
                    kind: PatKind::Wild,
                    ty: sty,
                    span,
                },
                guard: None,
                body: exit,
            },
        ];
        let test = self.mk(
            hir::ExprKind::Match {
                scrutinee: Box::new(scrut),
                arms,
            },
            Ty::Unit,
            span,
        );
        let wrapper = Block {
            locals: Vec::new(),
            stmts: vec![Stmt {
                span,
                kind: hir::StmtKind::Expr(test),
            }],
            tail: None,
            ty: Ty::Unit,
            span,
        };
        self.mk(
            hir::ExprKind::Loop {
                id,
                body: Box::new(wrapper),
            },
            Ty::Unit,
            span,
        )
    }
}
