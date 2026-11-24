//! `for` loops over integer ranges and `while let` (DESIGN §6.3).

use colibri_syntax::ast::{self, ExprKind, Label, PatKind as AstPat};
use colibri_syntax::{Diagnostic, Span};

use crate::body::{BlockMode, FnCtx, LocalKind, LoopFrame};
use crate::hir::{
    self, Arm, Block, Expr, ForBinding, ForLoop, ForSource, LoopId, Pat, PatKind, Stmt,
};
use crate::pat::PatCx;
use crate::types::{IntTy, Ty};

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

    /// `for` over an integer range, a list, a map, a set, `list.enumerate()` or `str.chars()`.
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
            return self.for_over_other(span, label, mutable, pat, head, body);
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
                Ty::Int(IntTy::U64) => {
                    self.ck.unsupported("a `for` range of `u64`", head.span);
                }
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
        let source = ForSource::Range {
            start: lo,
            end: hi,
            inclusive: *inclusive,
        };
        self.finish_for(span, label, mutable, false, pat, &elem, source, body)
    }

    /// Builds the loop once its source and element type are known.
    #[allow(clippy::too_many_arguments)]
    fn finish_for(
        &mut self,
        span: Span,
        label: Option<&Label>,
        mutable: bool,
        places: bool,
        pat: &ast::Pat,
        elem: &Ty,
        source: ForSource,
        body: &ast::Block,
    ) -> Expr {
        let id = self.new_loop(label);
        // A `for mut` over a plain local lends the element's slot when the body never names the
        // local (the VM adds the conditions only it can see).
        let source_local = match &source {
            ForSource::List { list, .. } if places => match &list.kind {
                hir::ExprKind::Read(place) if place.path.is_empty() => match place.anchor {
                    hir::Anchor::Local(l) => Some(l),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        let named_before = source_local.map(|l| self.flags[l.0 as usize].names);
        let (binding, block) = self.loop_body(mutable, pat, elem, body);
        let lend = match (source_local, named_before) {
            (Some(l), Some(before)) => self.flags[l.0 as usize].names == before,
            _ => false,
        };
        self.loops.pop();
        self.mk(
            hir::ExprKind::For(Box::new(ForLoop {
                id,
                binding,
                source,
                places,
                lend,
                body: block,
            })),
            Ty::Unit,
            span,
        )
    }

    /// A `for` over a collection, or over anything else iterable (`Iter`, `Iterable`, a
    /// stored range, `xs.enumerate()`, `s.chars()`).
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
        let ty = self.infer.shallow(&value.ty);
        let name = crate::place::expr_text(iter);
        let (source, elem, plain_list) = match ty {
            Ty::Error => {
                return self.for_body_only(label, mutable, pat, &Ty::Error, body, span);
            }
            Ty::List(t) => (ForSource::List { list: value, name }, *t, true),
            Ty::Map(k, v) => (ForSource::Map(value), Ty::Tuple(vec![*k, *v]), false),
            Ty::Set(t) => (ForSource::Set(value), *t, false),
            _ => match self.iterator_for(value, iter.span) {
                Some((it, item)) => (ForSource::Iter(it), item, false),
                None => return self.for_body_only(label, mutable, pat, &Ty::Error, body, span),
            },
        };
        if mutable && !plain_list {
            self.ck
                .unsupported("`for mut` over anything but a list", pat.span);
            return self.for_body_only(label, false, pat, &elem, body, span);
        }
        self.finish_for(span, label, mutable, mutable, pat, &elem, source, body)
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

    /// The body block of a `for`, with the loop variables as its first locals.
    fn loop_body(
        &mut self,
        mutable: bool,
        pat: &ast::Pat,
        elem: &Ty,
        body: &ast::Block,
    ) -> (ForBinding, Block) {
        self.push_scope();
        let binding = match &pat.kind {
            AstPat::Ident(id) => {
                let kind = if mutable {
                    LocalKind::Var
                } else {
                    LocalKind::Let
                };
                let local = self.declare(&id.name, id.span, elem.clone(), kind);
                if let Some(f) = self.flags.get_mut(local.0 as usize) {
                    f.for_var = true;
                }
                ForBinding::Local(local)
            }
            AstPat::Wild | AstPat::Error => ForBinding::None,
            AstPat::Tuple(_) if !mutable => {
                let mut cx = PatCx::new(LocalKind::Let);
                let checked = self.check_pat(pat, elem, &mut cx);
                self.require_irrefutable(&checked, elem, pat.span);
                ForBinding::Pat(checked)
            }
            _ => {
                self.ck.unsupported("a pattern in a `for` loop", pat.span);
                ForBinding::None
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
