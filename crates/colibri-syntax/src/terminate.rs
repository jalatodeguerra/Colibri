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

/// Whether a line starting with `kind` (followed by `next`) continues the
/// previous statement. Only a single `.`, not `..` or `..=`, and not a `.`
/// followed by a capitalised name: `.Circle(r) =>` starts a new `match` arm
/// with a variant shorthand (DESIGN §8.2), while method chains use lower-case
/// names.
fn continues_line(kind: &TokenKind, next: Option<&TokenKind>) -> bool {
    use TokenKind::*;
    match kind {
        Dot => !matches!(next, Option::Some(Ident(name)) if name.starts_with(char::is_uppercase)),
        Or | AndAnd | OrOr | Else => true,
        _ => false,
    }
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

    let mut tokens = tokens.into_iter().peekable();
    while let Some(tok) = tokens.next() {
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
                let next = tokens.peek().map(|t| &t.kind);
                if continues_line(&tok.kind, next)
                    && let Some(i) = inserted
                {
                    out.remove(i);
                }
                inserted = None;
                match tok.kind {
                    TokenKind::LParen | TokenKind::LBracket => frames.push(Frame::Inert),
                    TokenKind::LBrace => frames.push(Frame::Active),
                    // A `}` closes the innermost `{` even if a `(` or `[` inside it was
                    // never closed, and a stray `)` or `]` never closes a `{`. This keeps
                    // one unbalanced bracket from silencing terminators for the rest of
                    // the file; the parser reports the bracket itself.
                    TokenKind::RParen | TokenKind::RBracket => {
                        if frames.last() == Some(&Frame::Inert) {
                            frames.pop();
                        }
                    }
                    TokenKind::RBrace => while frames.pop() == Some(Frame::Inert) {},
                    _ => {}
                }
                can_end = ends_statement(&tok.kind);
                out.push(tok);
            }
        }
    }
    out
}
