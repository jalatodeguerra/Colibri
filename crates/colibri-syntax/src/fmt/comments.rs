//! Comments and significant tokens of a source file.
//!
//! The lexer drops ordinary comments, so the formatter finds them itself: everything between
//! two significant tokens is whitespace and comments. Strings are single tokens, so a `//`
//! inside a string or an interpolation hole is never mistaken for a comment.

use crate::{FileId, TokenKind, lex};

/// A significant token (no newlines, no doc comments, no end of file).
pub(crate) struct Tok {
    pub start: u32,
    pub end: u32,
    pub kind: TokenKind,
}

#[derive(Clone, Debug)]
pub(crate) struct Comment {
    pub start: u32,
    pub end: u32,
    /// The comment as written, without trailing whitespace.
    pub text: String,
    /// Only whitespace comes before it on its line.
    pub own_line: bool,
    /// `/* .. */`
    pub block: bool,
    /// `/// ..`
    pub doc: bool,
}

/// The significant tokens of `src` and its comments, both in source order.
pub(crate) fn scan(src: &str) -> (Vec<Tok>, Vec<Comment>) {
    let (raw, _) = lex(FileId(0), src);
    let toks: Vec<Tok> = raw
        .into_iter()
        .filter(|t| {
            !matches!(
                t.kind,
                TokenKind::Newline | TokenKind::DocComment(_) | TokenKind::Eof
            )
        })
        .map(|t| Tok {
            start: t.span.start,
            end: t.span.end,
            kind: t.kind,
        })
        .collect();
    let mut comments = Vec::new();
    let mut prev_end = 0usize;
    for tok in &toks {
        scan_gap(src, prev_end, tok.start as usize, &mut comments);
        prev_end = tok.end as usize;
    }
    scan_gap(src, prev_end, src.len(), &mut comments);
    (toks, comments)
}

fn scan_gap(src: &str, from: usize, to: usize, out: &mut Vec<Comment>) {
    let first_gap = from == 0;
    let bytes = src.as_bytes();
    let mut i = from;
    while i < to {
        let rest = &src[i..to];
        let Some(c) = rest.chars().next() else { break };
        if c.is_whitespace() {
            i += c.len_utf8();
        } else if rest.starts_with("//") {
            let end = rest.find('\n').map_or(to, |n| i + n);
            let text = src[i..end].trim_end();
            out.push(comment(src, from, first_gap, i, end, text, false));
            i = end;
        } else if rest.starts_with("/*") {
            let mut depth = 0;
            let mut j = i;
            while j < to {
                if bytes[j] == b'/' && j + 1 < to && bytes[j + 1] == b'*' {
                    depth += 1;
                    j += 2;
                } else if bytes[j] == b'*' && j + 1 < to && bytes[j + 1] == b'/' {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            out.push(comment(src, from, first_gap, i, j, &src[i..j], true));
            i = j;
        } else {
            i += c.len_utf8();
        }
    }
}

/// `gap_start` is where the previous token ended: the comment is on its own line when no token
/// comes before it on its line (other comments may).
fn comment(
    src: &str,
    gap_start: usize,
    first_gap: bool,
    start: usize,
    end: usize,
    text: &str,
    block: bool,
) -> Comment {
    let own_line = (first_gap && gap_start == 0) || src[gap_start..start].contains('\n');
    Comment {
        start: start as u32,
        end: end as u32,
        text: text.to_string(),
        own_line,
        block,
        doc: text.starts_with("///") && !text.starts_with("////"),
    }
}

/// The texts of all comments of `src`, sorted: for checking that a formatter keeps them all.
pub(crate) fn comment_texts(src: &str) -> Vec<String> {
    let mut texts: Vec<String> = scan(src).1.into_iter().map(|c| c.text).collect();
    texts.sort();
    texts
}
