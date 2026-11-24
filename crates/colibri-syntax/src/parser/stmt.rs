//! Blocks and statements (DESIGN §4, §6, §10).

use super::{Failed, PResult, Parser};
use crate::ast::*;
use crate::{Diagnostic, TokenKind};

fn assign_op(kind: &TokenKind) -> Option<AssignOp> {
    Some(match kind {
        TokenKind::Eq => AssignOp::Assign,
        TokenKind::PlusEq => AssignOp::Add,
        TokenKind::MinusEq => AssignOp::Sub,
        TokenKind::StarEq => AssignOp::Mul,
        TokenKind::SlashEq => AssignOp::Div,
        TokenKind::PercentEq => AssignOp::Rem,
        TokenKind::AmpEq => AssignOp::BitAnd,
        TokenKind::PipeEq => AssignOp::BitOr,
        TokenKind::CaretEq => AssignOp::BitXor,
        TokenKind::ShlEq => AssignOp::Shl,
        TokenKind::ShrEq => AssignOp::Shr,
        _ => return None,
    })
}

/// Whether an expression ends in a `{ }` block, so that no terminator is
/// needed after it.
fn is_blocklike(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::If { .. }
            | ExprKind::IfLet { .. }
            | ExprKind::Match { .. }
            | ExprKind::Unsafe(_)
            | ExprKind::Block(_)
    )
}

/// Whether `expr` can be assigned to.
fn is_place(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Ident(_)
        | ExprKind::SelfValue
        | ExprKind::Field { .. }
        | ExprKind::TupleIndex { .. }
        | ExprKind::Index { .. } => true,
        ExprKind::Paren(inner) => is_place(inner),
        _ => false,
    }
}

impl Parser<'_> {
    pub(crate) fn make_stmt(&mut self, start: u32, kind: StmtKind) -> Stmt {
        Stmt {
            id: self.id(),
            span: self.span_from(start),
            kind,
        }
    }

    /// Parses `{ stmts }` (the caller knows a block is expected).
    pub(crate) fn parse_block(&mut self) -> PResult<Block> {
        self.expect(&TokenKind::LBrace)?;
        self.parse_block_body()
    }

    /// Like [`Parser::parse_block`], for the body of `construct` (for
    /// example "`if` condition"), with a hint when the `{` is missing.
    pub(crate) fn parse_block_for(&mut self, construct: &str, hint: &str) -> PResult<Block> {
        if !self.at(&TokenKind::LBrace) {
            let message = format!(
                "expected `{{` after the {construct}, found {}",
                self.describe()
            );
            return self.fail(Diagnostic::error(message, self.found_span()).with_hint(hint));
        }
        self.bump();
        self.parse_block_body()
    }

    /// The statements of a block whose `{` was just consumed, through `}`.
    fn parse_block_body(&mut self) -> PResult<Block> {
        self.nested(|p| p.parse_block_body_inner())
    }

    fn parse_block_body_inner(&mut self) -> PResult<Block> {
        let open = self.toks[self.pos - 1].span;
        let saved = std::mem::replace(&mut self.no_struct, false);
        // Each statement with whether an explicit `;` ended it.
        let mut stmts: Vec<(Stmt, bool)> = Vec::new();
        loop {
            self.skip_semis();
            if self.at(&TokenKind::RBrace) {
                break;
            }
            if self.at_eof() {
                let diag = Diagnostic::error("unclosed `{`", open)
                    .with_hint("add a matching `}` at the end");
                self.report(diag);
                break;
            }
            let before = self.pos;
            let start = self.start();
            let parsed = self
                .parse_stmt()
                .and_then(|(stmt, blocklike)| Ok((stmt, self.finish_stmt(blocklike)?)));
            match parsed {
                Ok((stmt, explicit)) => stmts.push((stmt, explicit)),
                Err(Failed) => {
                    self.sync_stmt(false);
                    if self.pos == before {
                        self.bump();
                    }
                    let stmt = self.make_stmt(start, StmtKind::Error);
                    stmts.push((stmt, true));
                }
            }
        }
        self.no_struct = saved;
        self.eat(&TokenKind::RBrace);
        let span = self.span_from(open.start);
        let mut tail = None;
        if let Some((last, false)) = stmts.last()
            && matches!(last.kind, StmtKind::Expr(_))
            && let Some((stmt, _)) = stmts.pop()
            && let StmtKind::Expr(expr) = stmt.kind
        {
            tail = Some(Box::new(expr));
        }
        Ok(Block {
            id: self.id(),
            span,
            stmts: stmts.into_iter().map(|(s, _)| s).collect(),
            tail,
        })
    }

    /// Consumes the end of a statement. Returns whether an explicit `;` was
    /// used. A statement may also end right before `}` or the end of file,
    /// and a statement that ends in a block needs no terminator.
    pub(crate) fn finish_stmt(&mut self, blocklike: bool) -> PResult<bool> {
        match self.kind() {
            TokenKind::Semi => {
                let explicit = !self.tok().span.is_empty();
                self.bump();
                Ok(explicit)
            }
            TokenKind::RBrace | TokenKind::Eof => Ok(false),
            _ if blocklike || self.toks[self.pos.saturating_sub(1)].kind == TokenKind::RBrace => {
                Ok(false)
            }
            kind => {
                let message = format!(
                    "expected the end of the statement, found {}",
                    self.describe()
                );
                let hint = if *kind == TokenKind::Eq {
                    "assignment is a statement, not an expression: it cannot be used as a value or a condition"
                } else if *kind == TokenKind::Colon {
                    "if this was meant to be a map, wrap it in parentheses: `=> ({\"a\": 1})`; a bare `{` here starts a block"
                } else {
                    "separate statements with a new line or `;`"
                };
                self.fail(Diagnostic::error(message, self.found_span()).with_hint(hint))
            }
        }
    }

    /// Parses one statement; the flag says it ended with a `{ }` block.
    pub(crate) fn parse_stmt(&mut self) -> PResult<(Stmt, bool)> {
        let start = self.start();
        let (kind, blocklike) = match self.kind() {
            TokenKind::Let | TokenKind::Var | TokenKind::Const => {
                (StmtKind::Let(self.parse_let(true)?), false)
            }
            TokenKind::Return => {
                self.bump();
                let value = if self.can_start_expr() {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                (StmtKind::Return(value), false)
            }
            TokenKind::Break => (self.parse_break()?, false),
            TokenKind::Continue => {
                self.bump();
                let label = self.parse_label_opt();
                (StmtKind::Continue { label }, false)
            }
            TokenKind::Defer => {
                self.bump();
                let inner_start = self.start();
                let (kind, blocklike) = if self.at(&TokenKind::LBrace) {
                    let block = self.parse_block()?;
                    (StmtKind::Expr(self.block_expr(inner_start, block)), true)
                } else {
                    self.parse_simple_stmt()?
                };
                let inner = self.make_stmt(inner_start, kind);
                (StmtKind::Defer(Box::new(inner)), blocklike)
            }
            TokenKind::For | TokenKind::While => (self.parse_loop(None)?, true),
            TokenKind::Label(_) => (self.parse_labelled_loop()?, true),
            TokenKind::LBrace => {
                let block = self.parse_block()?;
                (StmtKind::Expr(self.block_expr(start, block)), true)
            }
            _ if self.starts_item() => {
                self.skip_local_item()?;
                (StmtKind::Error, true)
            }
            _ => self.parse_simple_stmt()?,
        };
        Ok((self.make_stmt(start, kind), blocklike))
    }

    pub(crate) fn block_expr(&mut self, start: u32, block: Block) -> Expr {
        self.make_expr(start, ExprKind::Block(block))
    }

    /// An assignment or an expression statement.
    pub(crate) fn parse_simple_stmt(&mut self) -> PResult<(StmtKind, bool)> {
        let target = self.parse_expr()?;
        let Some(op) = assign_op(self.kind()) else {
            let blocklike = is_blocklike(&target);
            return Ok((StmtKind::Expr(target), blocklike));
        };
        if !is_place(&target) {
            let diag = Diagnostic::error("cannot assign to this expression", target.span)
                .with_hint("the left side must be a variable, a field or an index");
            self.report(diag);
        }
        self.bump();
        let value = self.parse_expr()?;
        Ok((StmtKind::Assign { op, target, value }, false))
    }

    /// `let`, `let mut` or `const` with a pattern, an optional type and `= init`. `local` is
    /// true inside a block: `const` is only for the top level of a file, so there it is
    /// reported and read as `let`. The old `var` is reported and read as `let mut`.
    pub(crate) fn parse_let(&mut self, local: bool) -> PResult<LetStmt> {
        let kind = match self.bump().kind {
            TokenKind::Let => {
                if self.eat(&TokenKind::Mut) {
                    BindKind::Var
                } else {
                    BindKind::Let
                }
            }
            TokenKind::Var => {
                // `var` was already consumed: the name is the current token.
                let hint = match self.name_here() {
                    Some(name) => format!("write `let mut {name} = ...`"),
                    None => "write `let mut` instead of `var`".to_string(),
                };
                self.report_old_var_at(self.toks[self.pos - 1].span, &hint);
                BindKind::Var
            }
            _ if local => {
                let diag = Diagnostic::error(
                    "`const` is only allowed at the top level of a file",
                    self.toks[self.pos - 1].span,
                )
                .with_hint("use `let` for a local value");
                self.report(diag);
                BindKind::Let
            }
            _ => BindKind::Const,
        };
        let pat = self.parse_pat()?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        if !self.at(&TokenKind::Eq) {
            let message = format!(
                "expected `=` and an initial value, found {}",
                self.describe()
            );
            let diag = Diagnostic::error(message, self.found_span())
                .with_hint("bindings must be initialised: `let name = value`");
            return self.fail(diag);
        }
        self.bump();
        let init = self.with_struct_mode(false, |p| p.parse_expr())?;
        Ok(LetStmt {
            kind,
            pat,
            ty,
            init,
        })
    }

    fn parse_label_opt(&mut self) -> Option<Label> {
        if let TokenKind::Label(name) = self.kind().clone() {
            let span = self.bump().span;
            Some(Label { name, span })
        } else {
            None
        }
    }

    fn parse_break(&mut self) -> PResult<StmtKind> {
        self.bump();
        let label = self.parse_label_opt();
        let value = if self.can_start_expr() {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(StmtKind::Break { label, value })
    }

    /// `'outer: for ...` or `'outer: while ...`.
    ///
    /// TODO(design): DESIGN only shows `break 'outer`; the label syntax on
    /// the loop is `'outer: for ...`.
    fn parse_labelled_loop(&mut self) -> PResult<StmtKind> {
        let label = self.parse_label_opt();
        if !self.eat(&TokenKind::Colon) {
            return self.unexpected("`:` after a loop label");
        }
        if !matches!(self.kind(), TokenKind::For | TokenKind::While) {
            let diag = Diagnostic::error(
                format!(
                    "expected `for` or `while` after the label, found {}",
                    self.describe()
                ),
                self.found_span(),
            )
            .with_hint("labels name loops: `'outer: for x in xs { ... }`");
            return self.fail(diag);
        }
        self.parse_loop(label)
    }

    fn parse_loop(&mut self, label: Option<Label>) -> PResult<StmtKind> {
        if self.bump().kind == TokenKind::For {
            let mut mutable = self.eat(&TokenKind::Mut);
            if !mutable && self.at(&TokenKind::Var) {
                let hint = match self.name_after_keyword() {
                    Some(name) => format!("write `for mut {name} in ...`"),
                    None => "write `for mut` instead of `for var`".to_string(),
                };
                self.report_old_var(&hint);
                self.bump();
                mutable = true;
            }
            let pat = self.parse_pat()?;
            if !self.at(&TokenKind::In) {
                let message = format!(
                    "expected `in` after the loop pattern, found {}",
                    self.describe()
                );
                let diag = Diagnostic::error(message, self.found_span())
                    .with_hint("loops are written `for x in items { ... }`");
                return self.fail(diag);
            }
            self.bump();
            let iter = self.with_struct_mode(true, |p| p.parse_expr())?;
            let body = self.parse_block_for(
                "`for` header",
                "the body of a `for` loop must be a block: `for x in xs { ... }`",
            )?;
            return Ok(StmtKind::For {
                label,
                mutable,
                pat,
                iter,
                body,
            });
        }
        if self.eat(&TokenKind::Let) {
            let pat = self.parse_pat()?;
            self.expect(&TokenKind::Eq)?;
            let scrutinee = self.with_struct_mode(true, |p| p.parse_expr())?;
            let body = self.parse_block_for(
                "`while let` header",
                "the body of `while` must be a block: `while let some(x) = it.next() { ... }`",
            )?;
            return Ok(StmtKind::WhileLet {
                label,
                pat,
                scrutinee,
                body,
            });
        }
        let cond = self.parse_condition("while")?;
        let body = self.parse_block_for(
            "`while` condition",
            "the body of `while` must be a block: `while cond { ... }`",
        )?;
        Ok(StmtKind::While { label, cond, body })
    }

    /// The condition of an `if` or `while`, parsed without struct literals.
    pub(crate) fn parse_condition(&mut self, construct: &str) -> PResult<Expr> {
        let cond = self.with_struct_mode(true, |p| p.parse_expr())?;
        if self.at(&TokenKind::Eq) {
            let message = format!("expected a condition after `{construct}`, found assignment `=`");
            let diag = Diagnostic::error(message, self.found_span())
                .with_label(cond.span, "this is assigned to")
                .with_hint("to compare, use `==`; assignment is a statement and cannot be used as a condition");
            return self.fail(diag);
        }
        Ok(cond)
    }
}
