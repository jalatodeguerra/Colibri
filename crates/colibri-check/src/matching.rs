//! `match` and `if let` (DESIGN §8.2). Patterns stay patterns in HIR; the checker proves the
//! `match` exhaustive and warns about arms that can never run.

use colibri_syntax::ast::{self, ArmBody};
use colibri_syntax::{Diagnostic, Span};

use crate::body::{BlockMode, FnCtx, LocalKind};
use crate::exhaust::{self, ArmInfo};
use crate::hir::{self, Arm, Block, Expr, LocalId, Pat, PatKind, Stmt};
use crate::pat::PatCx;
use crate::types::Ty;

impl FnCtx<'_, '_> {
    /// `match scrutinee { arms }`, as a value or as a statement.
    #[inline(never)]
    pub fn check_match(
        &mut self,
        span: Span,
        scrutinee: &ast::Expr,
        arms: &[ast::MatchArm],
        mode: BlockMode<'_>,
    ) -> Expr {
        let scrut = self.check_expr(scrutinee, None);
        let sty = self.default_ty(&scrut.ty);
        let mut hint: Option<Ty> = match mode {
            BlockMode::Value(expected) => expected.cloned(),
            BlockMode::Stmts => None,
        };
        let mut result: Option<(Ty, Span)> = None;
        let mut out: Vec<Arm> = Vec::new();
        let mut pattern_errors = false;
        for arm in arms {
            self.push_scope();
            let mut cx = PatCx::new(LocalKind::Let);
            let before = self.ck.error_count();
            let pat = self.check_pat(&arm.pat, &sty, &mut cx);
            pattern_errors |= self.ck.error_count() > before;
            let guard = arm.guard.as_ref().map(|g| self.check_cond(g));
            let (stmts, tail, ty) = self.arm_body(&arm.body, mode, hint.as_ref());
            let locals = self.pop_scope();
            if matches!(mode, BlockMode::Value(_)) {
                let aty = self.infer.shallow(&ty);
                if aty != Ty::Never {
                    let at = tail.as_ref().map_or(arm.span, |t| t.span);
                    match &result {
                        None => {
                            hint = Some(aty.clone());
                            result = Some((aty, at));
                        }
                        Some((first, first_span)) => {
                            let (first, first_span) = (first.clone(), *first_span);
                            if self.infer.unify(&first, &aty).is_err() {
                                self.report_arm_mismatch(&first, first_span, &aty, at);
                            }
                        }
                    }
                }
            }
            out.push(Arm {
                pat,
                guard,
                body: Block {
                    locals,
                    stmts,
                    tail,
                    ty,
                    span: arm.span,
                },
            });
        }
        let all_never = out
            .iter()
            .all(|a| self.infer.shallow(&a.body.ty) == Ty::Never);
        let ty = match mode {
            BlockMode::Value(_) => match result {
                Some((ty, _)) => ty,
                None => Ty::Never,
            },
            BlockMode::Stmts if all_never => Ty::Never,
            BlockMode::Stmts => Ty::Unit,
        };
        // A pattern that was already reported as wrong stands in as `_`; analysing the rest
        // would only add noise.
        if !sty.is_error() && !pattern_errors {
            self.check_exhaustive(&sty, &out, scrutinee.span);
        }
        self.mk(
            hir::ExprKind::Match {
                scrutinee: Box::new(scrut),
                arms: out,
            },
            ty,
            span,
        )
    }

    fn report_arm_mismatch(&mut self, first: &Ty, first_span: Span, found: &Ty, at: Span) {
        let (a, b) = (self.show(first), self.show(found));
        self.ck.report(
            Diagnostic::error(
                format!("the arms of this `match` give different types: `{a}` and `{b}`"),
                at,
            )
            .with_label(first_span, format!("an earlier arm is `{a}`"))
            .with_hint("every arm of a `match` used as a value must have the same type"),
        );
    }

    /// The statements, value and type of one arm body.
    fn arm_body(
        &mut self,
        body: &ArmBody,
        mode: BlockMode<'_>,
        hint: Option<&Ty>,
    ) -> (Vec<Stmt>, Option<Box<Expr>>, Ty) {
        match (body, mode) {
            (ArmBody::Expr(e), BlockMode::Value(_)) => {
                let value = self.check_expr(e, hint);
                let ty = value.ty.clone();
                (Vec::new(), Some(Box::new(value)), ty)
            }
            (ArmBody::Expr(e), BlockMode::Stmts) => {
                let (stmt, never) = self.expr_stmt(e);
                (vec![stmt], None, if never { Ty::Never } else { Ty::Unit })
            }
            (ArmBody::Stmt(s), _) => {
                let mut stmts = Vec::new();
                let never = self.check_stmt(s, &mut stmts);
                (stmts, None, if never { Ty::Never } else { Ty::Unit })
            }
        }
    }

    /// Reports the cases no arm covers, and arms that can never run.
    fn check_exhaustive(&mut self, scrutinee_ty: &Ty, arms: &[Arm], scrutinee_span: Span) {
        let ty = self.infer.resolve(scrutinee_ty);
        let infos: Vec<ArmInfo<'_>> = arms
            .iter()
            .map(|a| ArmInfo {
                pat: &a.pat,
                guarded: a.guard.is_some(),
            })
            .collect();
        let analysis = exhaust::analyze(&self.ck.types, &ty, &infos);
        let spans: Vec<Span> = analysis
            .unreachable
            .iter()
            .map(|&i| arms[i].pat.span)
            .collect();
        for span in spans {
            self.ck.report(
                Diagnostic::warning("this arm can never run", span)
                    .with_hint("the arms above already match every value it could match; remove it or reorder the arms"),
            );
        }
        if analysis.gave_up {
            self.report_gave_up(scrutinee_span, "`match`");
        } else if !analysis.missing.is_empty() {
            self.report_non_exhaustive(
                &analysis.missing,
                analysis.more_missing,
                scrutinee_span,
                "`match`",
            );
        }
    }

    /// The exhaustiveness analysis ran out of budget: reject rather than risk a `match` that
    /// no arm matches at runtime.
    pub fn report_gave_up(&mut self, span: Span, what: &str) {
        self.ck.report(
            Diagnostic::error(
                format!("this {what} is too complex to check for missing cases"),
                span,
            )
            .with_hint("add a final `_ => ...` arm"),
        );
    }

    pub fn report_non_exhaustive(
        &mut self,
        missing: &[String],
        more: bool,
        span: Span,
        what: &str,
    ) {
        let plural = if missing.len() == 1 { "" } else { "s" };
        let mut list = missing.join(", ");
        if more {
            list.push_str(", ...");
        }
        let covers_rest = missing.iter().any(|m| m == "_");
        let hint = if covers_rest {
            "add a `_ =>` arm to cover the remaining values".to_string()
        } else {
            format!("add an arm for the missing case{plural}, or a `_ =>` arm to cover the rest")
        };
        self.ck.report(
            Diagnostic::error(
                format!("non-exhaustive {what}: missing case{plural}: {list}"),
                span,
            )
            .with_hint(hint),
        );
    }

    // ---- if let ----

    /// `if let pat = scrutinee { then } else { else }`: a `match` with the `then` block as the
    /// first arm and the `else` block (or nothing) as the catch-all.
    #[inline(never)]
    pub fn check_if_let(
        &mut self,
        span: Span,
        pat: &ast::Pat,
        scrutinee: &ast::Expr,
        then: &ast::Block,
        else_: Option<&ast::Expr>,
        mode: BlockMode<'_>,
    ) -> Expr {
        let scrut = self.check_expr(scrutinee, None);
        let sty = self.default_ty(&scrut.ty);
        let then_mode = if else_.is_some() {
            mode
        } else {
            BlockMode::Stmts
        };
        let (pat, then_arm_body, locals) = self.pattern_scope(pat, &sty, |this| {
            let block = this.check_block(then, then_mode);
            let ty = block.ty.clone();
            this.mk(hir::ExprKind::Block(Box::new(block)), ty, then.span)
        });
        let then_ty = then_arm_body.ty.clone();
        let then_block = Block {
            locals,
            stmts: Vec::new(),
            tail: Some(Box::new(then_arm_body)),
            ty: then_ty,
            span: then.span,
        };
        let (else_block, ty) = match else_ {
            None => {
                let empty = Block {
                    locals: Vec::new(),
                    stmts: Vec::new(),
                    tail: None,
                    ty: Ty::Unit,
                    span,
                };
                (empty, Ty::Unit)
            }
            Some(e) => {
                let Some(block) = self.check_else(e, mode) else {
                    return self.error_expr(span);
                };
                let ty = self.join_branches(mode, &then_block, &block);
                (block, ty)
            }
        };
        let wild = Pat {
            kind: PatKind::Wild,
            ty: sty,
            span,
        };
        let arms = vec![
            Arm {
                pat,
                guard: None,
                body: then_block,
            },
            Arm {
                pat: wild,
                guard: None,
                body: else_block,
            },
        ];
        self.mk(
            hir::ExprKind::Match {
                scrutinee: Box::new(scrut),
                arms,
            },
            ty,
            span,
        )
    }

    /// Checks a pattern in a fresh scope, then `body` inside it; returns the pattern, the
    /// body's value and the locals the scope declared (the pattern's bindings first).
    pub fn pattern_scope(
        &mut self,
        pat: &ast::Pat,
        ty: &Ty,
        body: impl FnOnce(&mut Self) -> Expr,
    ) -> (Pat, Expr, Vec<LocalId>) {
        self.push_scope();
        let mut cx = PatCx::new(LocalKind::Let);
        let checked = self.check_pat(pat, ty, &mut cx);
        let value = body(self);
        let locals = self.pop_scope();
        (checked, value, locals)
    }
}
