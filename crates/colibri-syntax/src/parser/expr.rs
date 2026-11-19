//! Expressions: a Pratt parser over the precedence table of DESIGN §3.5 and
//! §10.3, plus postfix forms and literals.
//!
//! From loosest to tightest:
//! `or` < `||` < `&&` < comparisons (non-associative) < `is` < `|` < `^` < `&`
//! < `<< >>` < ranges (non-associative) < `+ -` < `* / %` < `as` < unary
//! < postfix (call, index, field, `?`).

use super::{Failed, MAX_CHAIN, PResult, Parser};
use crate::ast::*;
use crate::{Diagnostic, Span, StrPart, Token, TokenKind};

const BP_OR: u8 = 2;
const BP_OROR: u8 = 4;
const BP_ANDAND: u8 = 6;
const BP_CMP: u8 = 8;
const BP_IS: u8 = 10;
// The bitwise operators (DESIGN §5.1) bind tighter than comparisons, so `x & 1 == 0` is
// `(x & 1) == 0`; they sit between `is` and ranges, in Rust's relative order.
const BP_BITOR: u8 = 12;
const BP_BITXOR: u8 = 14;
const BP_BITAND: u8 = 16;
const BP_SHIFT: u8 = 18;
const BP_RANGE: u8 = 20;
const BP_ADD: u8 = 22;
const BP_MUL: u8 = 24;
const BP_AS: u8 = 26;

/// The binary operator spelled by `kind`, with its binding power.
fn binary_op(kind: &TokenKind) -> Option<(BinOp, u8)> {
    Some(match kind {
        TokenKind::OrOr => (BinOp::Or, BP_OROR),
        TokenKind::AndAnd => (BinOp::And, BP_ANDAND),
        TokenKind::EqEq => (BinOp::Eq, BP_CMP),
        TokenKind::NotEq => (BinOp::Ne, BP_CMP),
        TokenKind::Lt => (BinOp::Lt, BP_CMP),
        TokenKind::LtEq => (BinOp::Le, BP_CMP),
        TokenKind::Gt => (BinOp::Gt, BP_CMP),
        TokenKind::GtEq => (BinOp::Ge, BP_CMP),
        TokenKind::Is => (BinOp::Is, BP_IS),
        TokenKind::Pipe => (BinOp::BitOr, BP_BITOR),
        TokenKind::Caret => (BinOp::BitXor, BP_BITXOR),
        TokenKind::Amp => (BinOp::BitAnd, BP_BITAND),
        TokenKind::Shl => (BinOp::Shl, BP_SHIFT),
        TokenKind::Shr => (BinOp::Shr, BP_SHIFT),
        TokenKind::Plus => (BinOp::Add, BP_ADD),
        TokenKind::Minus => (BinOp::Sub, BP_ADD),
        TokenKind::Star => (BinOp::Mul, BP_MUL),
        TokenKind::Slash => (BinOp::Div, BP_MUL),
        TokenKind::Percent => (BinOp::Rem, BP_MUL),
        _ => return None,
    })
}

fn is_comparison(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::EqEq
            | TokenKind::NotEq
            | TokenKind::Lt
            | TokenKind::LtEq
            | TokenKind::Gt
            | TokenKind::GtEq
    )
}

/// Whether `expr` looks like a type name, so `expr {` can be a struct
/// literal: `Player`, `game.Thing`, `Pair[int, str]`.
fn is_path_like(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Ident(_) => true,
        ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => is_path_like(base),
        _ => false,
    }
}

impl Parser<'_> {
    pub(crate) fn make_expr(&mut self, start: u32, kind: ExprKind) -> Expr {
        Expr {
            id: self.id(),
            span: self.span_from(start),
            kind,
        }
    }

    pub(crate) fn error_expr(&mut self, span: Span) -> Expr {
        Expr {
            id: self.id(),
            span,
            kind: ExprKind::Error,
        }
    }

    /// Whether the current token can begin an expression.
    pub(crate) fn can_start_expr(&self) -> bool {
        match self.kind() {
            TokenKind::LBrace => !self.no_struct,
            TokenKind::Int(_)
            | TokenKind::Float(_)
            | TokenKind::Char(_)
            | TokenKind::Str(_)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::None
            | TokenKind::Some
            | TokenKind::Ok
            | TokenKind::Err
            | TokenKind::Ident(_)
            | TokenKind::SelfKw
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::Minus
            | TokenKind::Bang
            | TokenKind::Tilde
            | TokenKind::Not
            | TokenKind::Go
            | TokenKind::Fn
            | TokenKind::If
            | TokenKind::Match
            | TokenKind::Unsafe
            | TokenKind::Dot
            | TokenKind::DotDot
            | TokenKind::DotDotEq => true,
            _ => false,
        }
    }

    pub(crate) fn parse_expr(&mut self) -> PResult<Expr> {
        self.expr_bp(0)
    }

    fn expr_bp(&mut self, min_bp: u8) -> PResult<Expr> {
        self.nested(|p| p.expr_bp_inner(min_bp))
    }

    fn expr_bp_inner(&mut self, min_bp: u8) -> PResult<Expr> {
        let mut lhs = self.parse_unary()?;
        let mut chained = 0;
        loop {
            chained += 1;
            if chained > MAX_CHAIN {
                return self.fail(Diagnostic::error(
                    "this expression is too long",
                    self.found_span(),
                ));
            }
            let kind = self.kind().clone();
            let start = lhs.span.start;
            match kind {
                TokenKind::Or => {
                    if BP_OR < min_bp {
                        break;
                    }
                    self.bump();
                    lhs = self.parse_or_rest(start, lhs)?;
                    continue;
                }
                TokenKind::As => {
                    if BP_AS < min_bp {
                        break;
                    }
                    self.bump();
                    let ty = self.parse_type()?;
                    lhs = self.make_expr(
                        start,
                        ExprKind::Cast {
                            expr: Box::new(lhs),
                            ty,
                        },
                    );
                    continue;
                }
                TokenKind::DotDot | TokenKind::DotDotEq => {
                    if BP_RANGE < min_bp {
                        break;
                    }
                    let inclusive = self.bump().kind == TokenKind::DotDotEq;
                    let end = self.parse_range_end(inclusive)?;
                    lhs = self.make_expr(
                        start,
                        ExprKind::Range {
                            start: Some(Box::new(lhs)),
                            end,
                            inclusive,
                        },
                    );
                    if matches!(self.kind(), TokenKind::DotDot | TokenKind::DotDotEq) {
                        let diag = Diagnostic::error("ranges cannot be chained", self.found_span())
                            .with_hint("a range has two ends: `a..b`; parenthesise to combine");
                        self.report(diag);
                    }
                    continue;
                }
                _ => {}
            }
            let (op, bp) = match binary_op(&kind) {
                Some(found) => found,
                None if kind == TokenKind::And => (BinOp::And, BP_ANDAND),
                None => break,
            };
            if bp < min_bp {
                break;
            }
            if kind == TokenKind::And {
                let diag =
                    Diagnostic::error("`and` is not an operator in Colibri", self.found_span())
                        .with_hint("use `&&` instead of `and`");
                self.report(diag);
            }
            self.bump();
            let rhs = self.expr_bp(bp + 1)?;
            lhs = self.make_expr(
                start,
                ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
            );
            if bp == BP_CMP && is_comparison(self.kind()) {
                let diag =
                    Diagnostic::error("comparison operators cannot be chained", self.found_span())
                        .with_hint("compare each pair separately: `a < b && b < c`");
                self.report(diag);
            }
        }
        Ok(lhs)
    }

    /// The end of a range after `..`/`..=`, if there is one.
    fn parse_range_end(&mut self, inclusive: bool) -> PResult<Option<Box<Expr>>> {
        if self.can_start_expr() {
            return Ok(Some(Box::new(self.expr_bp(BP_RANGE + 1)?)));
        }
        if inclusive {
            let diag = Diagnostic::error("`..=` needs an end value", self.found_span())
                .with_hint("write `a..=b`, or use `a..` for an open range");
            return self.fail(diag);
        }
        Ok(None)
    }

    /// After `lhs or`: a handler block, `|e| { ... }` or a fallback value.
    fn parse_or_rest(&mut self, start: u32, lhs: Expr) -> PResult<Expr> {
        if self.at(&TokenKind::LBrace) {
            let handler = self.parse_block()?;
            return Ok(self.make_expr(
                start,
                ExprKind::OrHandler {
                    lhs: Box::new(lhs),
                    binding: None,
                    handler,
                },
            ));
        }
        if self.eat(&TokenKind::Pipe) {
            let binding = self.expect_ident("an error name between `|` bars")?;
            self.expect(&TokenKind::Pipe)?;
            let handler = self.parse_block_for(
                "`or |name|` binding",
                "the handler is a block: `or |e| { ... }`",
            )?;
            return Ok(self.make_expr(
                start,
                ExprKind::OrHandler {
                    lhs: Box::new(lhs),
                    binding: Some(binding),
                    handler,
                },
            ));
        }
        let rhs = self.expr_bp(BP_OR + 1)?;
        Ok(self.make_expr(
            start,
            ExprKind::Or {
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        ))
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        self.nested(|p| p.parse_unary_inner())
    }

    fn parse_unary_inner(&mut self) -> PResult<Expr> {
        let start = self.start();
        let op = match self.kind() {
            TokenKind::Minus => UnOp::Neg,
            TokenKind::Bang => UnOp::Not,
            TokenKind::Tilde => UnOp::BitNot,
            TokenKind::Not => {
                let diag =
                    Diagnostic::error("`not` is not an operator in Colibri", self.found_span())
                        .with_hint("use `!` instead of `not`");
                self.report(diag);
                UnOp::Not
            }
            TokenKind::Go => {
                self.bump();
                let call = self.parse_unary()?;
                if !matches!(call.kind, ExprKind::Call { .. }) {
                    let diag =
                        Diagnostic::error("`go` must be followed by a function call", call.span)
                            .with_hint("write `go work(args)` to start a coroutine");
                    self.report(diag);
                }
                return Ok(self.make_expr(start, ExprKind::Go(Box::new(call))));
            }
            TokenKind::DotDot | TokenKind::DotDotEq => {
                let inclusive = self.bump().kind == TokenKind::DotDotEq;
                let end = self.parse_range_end(inclusive)?;
                return Ok(self.make_expr(
                    start,
                    ExprKind::Range {
                        start: None,
                        end,
                        inclusive,
                    },
                ));
            }
            _ => {
                let primary = self.parse_primary()?;
                return self.parse_postfix(primary);
            }
        };
        self.bump();
        let operand = self.parse_unary()?;
        Ok(self.make_expr(
            start,
            ExprKind::Unary {
                op,
                expr: Box::new(operand),
            },
        ))
    }

    // ---- postfix ----

    fn parse_postfix(&mut self, mut expr: Expr) -> PResult<Expr> {
        let mut chained = 0;
        loop {
            chained += 1;
            if chained > MAX_CHAIN {
                return self.fail(Diagnostic::error(
                    "this expression is too long",
                    self.found_span(),
                ));
            }
            let start = expr.span.start;
            let kind = match self.kind().clone() {
                TokenKind::LParen => {
                    self.bump();
                    let args = self.parse_args()?;
                    ExprKind::Call {
                        callee: Box::new(expr),
                        args,
                    }
                }
                TokenKind::LBracket => {
                    self.bump();
                    let args = self.parse_index_args()?;
                    ExprKind::Index {
                        base: Box::new(expr),
                        args,
                    }
                }
                TokenKind::Dot => {
                    self.bump();
                    self.parse_member(expr)?
                }
                TokenKind::Question => {
                    self.bump();
                    ExprKind::Try(Box::new(expr))
                }
                TokenKind::LBrace if is_path_like(&expr) => {
                    if self.no_struct && !self.looks_like_struct_body() {
                        break;
                    }
                    if self.no_struct {
                        let diag = Diagnostic::error(
                            "struct literals are not allowed here",
                            self.found_span(),
                        )
                        .with_label(expr.span, "this looks like a struct literal")
                        .with_hint("wrap the struct literal in parentheses: `if p == (Player { ... }) { ... }`");
                        self.report(diag);
                    }
                    self.parse_struct_lit(expr)?
                }
                _ => break,
            };
            expr = self.make_expr(start, kind);
        }
        Ok(expr)
    }

    /// After `.`: a field name, a tuple index or a method name. Keywords are
    /// fine here (`e.as[T]()`, `door.co.go(...)`).
    fn parse_member(&mut self, base: Expr) -> PResult<ExprKind> {
        let base = Box::new(base);
        match self.kind().clone() {
            TokenKind::Ident(name) => {
                let span = self.bump().span;
                Ok(ExprKind::Field {
                    base,
                    name: Ident { name, span },
                })
            }
            TokenKind::Int(n) => {
                let span = self.bump().span;
                let Ok(index) = u32::try_from(n) else {
                    return self.fail(Diagnostic::error("tuple index is too large", span));
                };
                Ok(ExprKind::TupleIndex { base, index })
            }
            ref kind if super::is_keyword(kind) => {
                let name = kind.fixed_text().unwrap_or_default().to_string();
                let span = self.bump().span;
                Ok(ExprKind::Field {
                    base,
                    name: Ident { name, span },
                })
            }
            _ => self.unexpected("a field or method name after `.`"),
        }
    }

    /// Arguments after `(`, through `)`.
    fn parse_args(&mut self) -> PResult<Vec<Arg>> {
        self.with_struct_mode(false, |p| {
            p.parse_comma_list(&TokenKind::RParen, |p| {
                if matches!(p.kind(), TokenKind::Ident(_)) && *p.nth(1) == TokenKind::Colon {
                    let name = p.expect_ident("an argument name")?;
                    p.bump();
                    let value = p.parse_expr()?;
                    Ok(Arg {
                        name: Some(name),
                        value,
                    })
                } else {
                    Ok(Arg {
                        name: None,
                        value: p.parse_expr()?,
                    })
                }
            })
        })
    }

    /// Arguments after `[`, through `]`: expressions, or types for the ones
    /// that are not expressions (DESIGN §3.5).
    fn parse_index_args(&mut self) -> PResult<Vec<IndexArg>> {
        self.with_struct_mode(false, |p| {
            p.parse_comma_list(&TokenKind::RBracket, |p| p.parse_index_arg())
        })
    }

    fn parse_index_arg(&mut self) -> PResult<IndexArg> {
        let ends_arg = |p: &Self| matches!(p.kind(), TokenKind::Comma | TokenKind::RBracket);
        let before = self.snapshot();
        if let Ok(expr) = self.parse_expr()
            && ends_arg(self)
        {
            return Ok(IndexArg::Expr(expr));
        }
        self.restore(before);
        let before = self.snapshot();
        if let Ok(ty) = self.parse_type()
            && ends_arg(self)
        {
            return Ok(IndexArg::Type(ty));
        }
        // Neither reading works: report the expression's problem.
        self.restore(before);
        Ok(IndexArg::Expr(self.parse_expr()?))
    }

    /// Whether the `{` at the cursor starts `{ name: ...` or `{ name, ...`,
    /// which a block cannot.
    fn looks_like_struct_body(&self) -> bool {
        matches!(self.nth(1), TokenKind::Ident(_))
            && matches!(self.nth(2), TokenKind::Colon | TokenKind::Comma)
    }

    /// `Type { name: value, shorthand }` after the type name `path`.
    fn parse_struct_lit(&mut self, path: Expr) -> PResult<ExprKind> {
        let span = path.span;
        let Some(ty) = self.expr_to_type(path) else {
            let diag = Diagnostic::error("expected a type name before `{`", span);
            return self.fail(diag);
        };
        let fields = self.parse_braced("fields", |p| {
            let name = p.expect_ident("a field name")?;
            if p.eat(&TokenKind::Colon) {
                let value = p.parse_expr()?;
                return Ok(FieldInit {
                    name,
                    shorthand: false,
                    value,
                });
            }
            if p.at(&TokenKind::Eq) {
                let diag = Diagnostic::error(
                    format!("expected `:` after field `{}`, found `=`", name.name),
                    p.found_span(),
                )
                .with_hint(format!("field values are written `{}: value`", name.name));
                return p.fail(diag);
            }
            let value = Expr {
                id: p.id(),
                span: name.span,
                kind: ExprKind::Ident(name.clone()),
            };
            Ok(FieldInit {
                name,
                shorthand: true,
                value,
            })
        })?;
        Ok(ExprKind::StructLit { ty, fields })
    }

    /// Reinterprets a path-like expression as a type (for struct literals).
    fn expr_to_type(&mut self, expr: Expr) -> Option<TypeExpr> {
        let span = expr.span;
        let kind = match expr.kind {
            ExprKind::Ident(name) => TypeKind::Named {
                path: vec![name],
                args: Vec::new(),
            },
            ExprKind::Field { base, name } => match self.expr_to_type(*base)?.kind {
                TypeKind::Named { mut path, args } if args.is_empty() => {
                    path.push(name);
                    TypeKind::Named { path, args }
                }
                _ => return None,
            },
            ExprKind::Index { base, args } => match self.expr_to_type(*base)?.kind {
                TypeKind::Named { path, args: none } if none.is_empty() => {
                    let args = args
                        .into_iter()
                        .map(|arg| match arg {
                            IndexArg::Type(ty) => Some(ty),
                            IndexArg::Expr(e) => self.expr_to_type(e),
                        })
                        .collect::<Option<Vec<_>>>()?;
                    TypeKind::Named { path, args }
                }
                _ => return None,
            },
            ExprKind::Tuple(items) => TypeKind::Tuple(
                items
                    .into_iter()
                    .map(|e| self.expr_to_type(e))
                    .collect::<Option<Vec<_>>>()?,
            ),
            ExprKind::Try(inner) => TypeKind::Optional(Box::new(self.expr_to_type(*inner)?)),
            ExprKind::Paren(inner) => return self.expr_to_type(*inner),
            _ => return None,
        };
        Some(TypeExpr {
            id: self.id(),
            span,
            kind,
        })
    }

    // ---- primary expressions ----

    fn parse_primary(&mut self) -> PResult<Expr> {
        let start = self.start();
        let token = self.tok().clone();
        let kind = match token.kind {
            TokenKind::Int(n) => {
                self.bump();
                ExprKind::Int(n)
            }
            TokenKind::Float(x) => {
                self.bump();
                ExprKind::Float(x)
            }
            TokenKind::Char(c) => {
                self.bump();
                ExprKind::Char(c)
            }
            TokenKind::True | TokenKind::False => {
                self.bump();
                ExprKind::Bool(token.kind == TokenKind::True)
            }
            TokenKind::None => {
                self.bump();
                ExprKind::None
            }
            TokenKind::Str(ref lit) => {
                self.bump();
                ExprKind::Str(self.parse_str_segments(lit))
            }
            TokenKind::Ident(name) => {
                let span = self.bump().span;
                ExprKind::Ident(Ident { name, span })
            }
            // Constructors of `Option` and `Result` are reserved words used
            // as ordinary callees: `ok(level)`.
            TokenKind::Some | TokenKind::Ok | TokenKind::Err => {
                let span = self.bump().span;
                let name = token.kind.fixed_text().unwrap_or_default().to_string();
                ExprKind::Ident(Ident { name, span })
            }
            TokenKind::SelfKw => {
                self.bump();
                ExprKind::SelfValue
            }
            TokenKind::LParen => return self.parse_paren(),
            TokenKind::LBracket => {
                self.bump();
                let items = self.with_struct_mode(false, |p| {
                    p.parse_comma_list(&TokenKind::RBracket, |p| p.parse_expr())
                })?;
                ExprKind::List(items)
            }
            TokenKind::LBrace => return self.parse_map(),
            TokenKind::Dot => return self.parse_dot_variant(),
            TokenKind::Fn => return self.parse_closure(),
            TokenKind::If => return self.parse_if(),
            TokenKind::Match => return self.parse_match(),
            TokenKind::Unsafe => {
                self.bump();
                let block = self.parse_block_for(
                    "`unsafe` keyword",
                    "unsafe code goes in a block: `unsafe { ... }`",
                )?;
                ExprKind::Unsafe(block)
            }
            TokenKind::Underscore => {
                let diag = Diagnostic::error("`_` cannot be used as a value", token.span)
                    .with_hint("`_` only works in patterns, where it matches anything");
                return self.fail(diag);
            }
            _ => return self.unexpected_expr(),
        };
        Ok(self.make_expr(start, kind))
    }

    fn unexpected_expr<T>(&mut self) -> PResult<T> {
        let message = format!("expected an expression, found {}", self.describe());
        let mut diag = Diagnostic::error(message, self.found_span());
        match self.kind() {
            TokenKind::Eq => {
                diag = diag.with_hint(
                    "assignment is a statement, not an expression: it cannot be used as a value",
                );
            }
            TokenKind::Else => {
                diag = diag.with_hint("`else` must follow the `}` of an `if` block");
            }
            _ => {}
        }
        self.fail(diag)
    }

    /// `()`, `(e)` and `(a, b)`.
    fn parse_paren(&mut self) -> PResult<Expr> {
        let start = self.start();
        self.bump();
        self.with_struct_mode(false, |p| {
            if p.eat(&TokenKind::RParen) {
                return Ok(p.make_expr(start, ExprKind::Tuple(Vec::new())));
            }
            let first = p.parse_expr()?;
            if p.eat(&TokenKind::RParen) {
                return Ok(p.make_expr(start, ExprKind::Paren(Box::new(first))));
            }
            if !p.at(&TokenKind::Comma) {
                return p.unexpected("`,` or `)`");
            }
            p.bump();
            let mut items = vec![first];
            while !p.at(&TokenKind::RParen) {
                items.push(p.parse_expr()?);
                if !p.eat(&TokenKind::Comma) {
                    break;
                }
            }
            p.expect(&TokenKind::RParen)?;
            Ok(p.make_expr(start, ExprKind::Tuple(items)))
        })
    }

    /// A map literal: `{k: v}` or `{:}`. This is where a `{` lands when no
    /// block is allowed (DESIGN §3.5).
    fn parse_map(&mut self) -> PResult<Expr> {
        let start = self.start();
        if *self.nth(1) == TokenKind::Colon && *self.nth(2) == TokenKind::RBrace {
            self.bump();
            self.bump();
            self.bump();
            return Ok(self.make_expr(start, ExprKind::Map(Vec::new())));
        }
        if *self.nth(1) == TokenKind::RBrace {
            let open = self.found_span();
            self.bump();
            self.bump();
            let span = open.to(self.span_from(start));
            let diag = Diagnostic::error("`{}` is not an empty map in an expression", span)
                .with_hint("write `{:}` for an empty map; `{}` is an empty block");
            self.report(diag);
            return Ok(self.make_expr(start, ExprKind::Map(Vec::new())));
        }
        let entries = self.parse_braced("map entries", |p| {
            let key = p.parse_expr()?;
            if !p.at(&TokenKind::Colon) {
                let message = format!("expected `:` in a map entry, found {}", p.describe());
                let diag = Diagnostic::error(message, p.found_span()).with_hint(
                    "here `{` starts a map literal `{key: value}`; a block is only allowed at the start of a statement or after `=>`, `else`, `or` and a condition",
                );
                return p.fail(diag);
            }
            p.bump();
            let value = p.parse_expr()?;
            Ok((key, value))
        })?;
        Ok(self.make_expr(start, ExprKind::Map(entries)))
    }

    /// `.Variant` or `.Variant(args)` (DESIGN §8.2).
    fn parse_dot_variant(&mut self) -> PResult<Expr> {
        let start = self.start();
        self.bump();
        let name = self.expect_ident("a variant name after `.`")?;
        let args = if self.eat(&TokenKind::LParen) {
            Some(self.parse_args()?)
        } else {
            None
        };
        Ok(self.make_expr(start, ExprKind::DotVariant { name, args }))
    }

    // ---- closures ----

    /// `fn(x) => e`, `fn(x: T) -> R => e`, `fn() { ... }`,
    /// `fn() use(weak self) { ... }`.
    fn parse_closure(&mut self) -> PResult<Expr> {
        let start = self.start();
        self.bump();
        if super::is_keyword(self.kind()) {
            // `fn match() {}`: someone meant a function declaration.
            self.expect_ident("a function name")?;
        }
        self.expect(&TokenKind::LParen)?;
        let (_, params) = self.with_struct_mode(false, |p| p.parse_params(true))?;
        let ret = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let captures = self.parse_captures()?;
        let body = if self.at(&TokenKind::LBrace) {
            FnBody::Block(self.parse_block()?)
        } else if self.eat(&TokenKind::FatArrow) {
            FnBody::Expr(self.parse_expr()?)
        } else {
            let message = format!(
                "expected `=>` or `{{` to start the closure body, found {}",
                self.describe()
            );
            let diag = Diagnostic::error(message, self.found_span())
                .with_hint("closures are `fn(x) => expr` or `fn(x) { ... }`");
            return self.fail(diag);
        };
        let closure = Closure {
            params,
            ret,
            captures,
            body: Box::new(body),
        };
        Ok(self.make_expr(start, ExprKind::Closure(closure)))
    }

    /// An optional capture list `use(weak self, other)`; `use` is an
    /// ordinary identifier outside this position.
    fn parse_captures(&mut self) -> PResult<Vec<Capture>> {
        let is_use = matches!(self.kind(), TokenKind::Ident(w) if w == "use")
            && *self.nth(1) == TokenKind::LParen;
        if !is_use {
            return Ok(Vec::new());
        }
        self.bump();
        self.bump();
        self.parse_comma_list(&TokenKind::RParen, |p| {
            let weak = p.eat(&TokenKind::Weak);
            let name = if p.at(&TokenKind::SelfKw) {
                let span = p.bump().span;
                Ident {
                    name: "self".to_string(),
                    span,
                }
            } else {
                p.expect_ident("a variable to capture")?
            };
            Ok(Capture { weak, name })
        })
    }

    // ---- if and match ----

    /// `if cond { } else if ... else { }` and `if let pat = e { }`.
    pub(crate) fn parse_if(&mut self) -> PResult<Expr> {
        self.nested(|p| p.parse_if_inner())
    }

    fn parse_if_inner(&mut self) -> PResult<Expr> {
        let start = self.start();
        self.bump();
        if self.eat(&TokenKind::Let) {
            let pat = self.parse_pat()?;
            self.expect(&TokenKind::Eq)?;
            let scrutinee = self.with_struct_mode(true, |p| p.parse_expr())?;
            let then = self.parse_block_for(
                "`if let` header",
                "the body of `if` must be a block: `if let some(x) = e { ... }`",
            )?;
            let else_ = self.parse_else()?;
            return Ok(self.make_expr(
                start,
                ExprKind::IfLet {
                    pat: Box::new(pat),
                    scrutinee: Box::new(scrutinee),
                    then,
                    else_,
                },
            ));
        }
        let cond = self.parse_condition("if")?;
        let then = self.parse_block_for(
            "`if` condition",
            "the body of `if` must be a block: `if cond { ... }`",
        )?;
        let else_ = self.parse_else()?;
        Ok(self.make_expr(
            start,
            ExprKind::If {
                cond: Box::new(cond),
                then,
                else_,
            },
        ))
    }

    fn parse_else(&mut self) -> PResult<Option<Box<Expr>>> {
        if !self.eat(&TokenKind::Else) {
            return Ok(None);
        }
        let start = self.start();
        if self.at(&TokenKind::If) {
            return Ok(Some(Box::new(self.parse_if()?)));
        }
        if self.at(&TokenKind::LBrace) {
            let block = self.parse_block()?;
            return Ok(Some(Box::new(self.block_expr(start, block))));
        }
        let message = format!(
            "expected `{{` or `if` after `else`, found {}",
            self.describe()
        );
        let diag = Diagnostic::error(message, self.found_span())
            .with_hint("write `else { ... }` or `else if cond { ... }`");
        self.fail(diag)
    }

    pub(crate) fn parse_match(&mut self) -> PResult<Expr> {
        let start = self.start();
        self.bump();
        let scrutinee = self.with_struct_mode(true, |p| p.parse_expr())?;
        if !self.at(&TokenKind::LBrace) {
            let message = format!(
                "expected `{{` after the `match` value, found {}",
                self.describe()
            );
            let diag = Diagnostic::error(message, self.found_span())
                .with_hint("match arms go in braces: `match x { pat => expr }`");
            return self.fail(diag);
        }
        let arms = self.parse_braced("match arms", |p| p.parse_arm())?;
        Ok(self.make_expr(
            start,
            ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
        ))
    }

    /// `pat (if guard)? => body`
    fn parse_arm(&mut self) -> PResult<MatchArm> {
        let start = self.start();
        let pat = self.parse_pat()?;
        let guard = if self.eat(&TokenKind::If) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        if !self.at(&TokenKind::FatArrow) {
            let message = format!(
                "expected `=>` after the match pattern, found {}",
                self.describe()
            );
            let mut diag = Diagnostic::error(message, self.found_span());
            if matches!(
                self.kind(),
                TokenKind::Arrow | TokenKind::Eq | TokenKind::Colon
            ) {
                diag = diag.with_hint("match arms are written `pattern => expression`");
            }
            return self.fail(diag);
        }
        self.bump();
        let body = self.parse_arm_body()?;
        Ok(MatchArm {
            id: self.id(),
            span: self.span_from(start),
            pat,
            guard,
            body,
        })
    }

    /// An arm body: a block, an expression, or a statement such as an
    /// assignment, `return`, `break` or `continue`.
    fn parse_arm_body(&mut self) -> PResult<ArmBody> {
        let start = self.start();
        if self.at(&TokenKind::LBrace) {
            let block = self.parse_block()?;
            return Ok(ArmBody::Expr(self.block_expr(start, block)));
        }
        if matches!(
            self.kind(),
            TokenKind::Return | TokenKind::Break | TokenKind::Continue
        ) {
            let (stmt, _) = self.parse_stmt()?;
            return Ok(ArmBody::Stmt(Box::new(stmt)));
        }
        let (kind, _) = self.parse_simple_stmt()?;
        match kind {
            StmtKind::Expr(expr) => Ok(ArmBody::Expr(expr)),
            other => Ok(ArmBody::Stmt(Box::new(self.make_stmt(start, other)))),
        }
    }

    // ---- strings ----

    /// Converts the parts of a string token, parsing each interpolation hole
    /// as an expression.
    fn parse_str_segments(&mut self, lit: &crate::StrLit) -> Vec<StrSeg> {
        lit.parts
            .iter()
            .map(|part| match part {
                StrPart::Lit(text) => StrSeg::Lit(text.clone()),
                StrPart::Interp { expr, spec } => StrSeg::Interp {
                    expr: self.parse_hole(*expr),
                    spec: spec.clone(),
                },
            })
            .collect()
    }

    /// Re-lexes and parses the text of an interpolation hole. Newlines inside
    /// a hole never end it, so terminators are dropped.
    fn parse_hole(&mut self, span: Span) -> Expr {
        let (tokens, diags) = crate::tokenize_range(self.file, self.src, span);
        self.diags.extend(diags);
        let tokens: Vec<Token> = tokens
            .into_iter()
            .filter(|t| t.kind != TokenKind::Semi)
            .collect();
        let mut sub = Parser::new(self.file, self.src, tokens);
        sub.next_id = self.next_id;
        sub.depth = self.depth;
        let result = sub.parse_expr();
        if result.is_ok() && !sub.at_eof() {
            let message = format!("unexpected {} in string interpolation", sub.describe());
            let diag = Diagnostic::error(message, sub.found_span())
                .with_hint("a hole holds one expression: `{a + b}`; write `{{` for a literal `{`");
            sub.report(diag);
        }
        self.next_id = sub.next_id;
        self.diags.append(&mut sub.diags);
        match result {
            Ok(expr) => expr,
            Err(Failed) => self.error_expr(span),
        }
    }
}
