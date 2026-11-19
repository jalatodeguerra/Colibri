//! Statement termination (DESIGN §3.2).
//!
//! The lexer produces raw `Newline` tokens. This pass removes them and inserts
//! [`TokenKind::Semi`] where a newline ends a statement.

use crate::{Span, Token, TokenKind};

/// What a newline does inside the innermost open bracket.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    /// `( )` and `[ ]`: newlines never end a statement.
    Inert,
    /// `{ }`: normal statement endings (even when nested in a call).
    Active,
}

/// Whether a newline after this token ends a statement (Go's rule).
fn ends_statement(kind: &TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        kind,
        Ident(_)
            | Underscore
            | Int(_)
            | Float(_)
            | Char(_)
            | Str(_)
            | Label(_)
            | True
            | False
            | None
            | SelfKw
            | RParen
            | RBracket
            | RBrace
            | Return
            | Break
            | Continue
            | Question
    )
}

/// Whether a line starting with this token continues the previous statement.
/// Only a single `.`, not `..` or `..=`.
fn continues_line(kind: &TokenKind) -> bool {
    use TokenKind::*;
    matches!(kind, Dot | Or | AndAnd | OrOr | Else)
}

fn implicit_semi(at: u32, like: Span) -> Token {
    Token {
        kind: TokenKind::Semi,
        span: Span::new(like.file, at, at),
    }
}

/// Turns the raw token stream into the parser's stream: no `Newline` tokens,
/// with implicit `Semi` tokens (empty span at the newline) where a statement
/// ends. Doc comments pass through and never affect the decisions.
pub fn insert_terminators(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut frames: Vec<Frame> = Vec::new();
    // The last significant token could end a statement and no `Semi` follows it.
    let mut can_end = false;
    // Index in `out` of a `Semi` inserted for a newline that no significant
    // token has followed yet; withdrawn if the next line continues this one.
    let mut inserted: Option<usize> = None;

    for tok in tokens {
        let active = frames.last() != Some(&Frame::Inert);
        match tok.kind {
            TokenKind::Newline => {
                if can_end && active {
                    inserted = Some(out.len());
                    out.push(implicit_semi(tok.span.start, tok.span));
                    can_end = false;
                }
            }
            TokenKind::DocComment(_) => out.push(tok),
            TokenKind::Eof => {
                if can_end && active {
                    out.push(implicit_semi(tok.span.start, tok.span));
                }
                out.push(tok);
            }
            _ => {
                if continues_line(&tok.kind)
                    && let Some(i) = inserted
                {
                    out.remove(i);
                }
                inserted = None;
                match tok.kind {
                    TokenKind::LParen | TokenKind::LBracket => frames.push(Frame::Inert),
                    TokenKind::LBrace => frames.push(Frame::Active),
                    TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                        frames.pop();
                    }
                    _ => {}
                }
                can_end = ends_statement(&tok.kind);
                out.push(tok);
            }
        }
    }
    out
}
