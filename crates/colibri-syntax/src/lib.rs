//! Shared front-end types for Colibri: source files, spans, diagnostics, the
//! lexer, the AST and the parser.

pub mod ast;
pub mod ast_dump;
mod diagnostic;
mod lexer;
mod parser;
mod source;
mod terminate;
mod token;

pub use diagnostic::{Diagnostic, Severity};
pub use lexer::lex;
pub use parser::{parse_file, parse_interface_file};
pub use source::{FileId, SourceMap, Span};
pub use terminate::insert_terminators;
pub use token::{StrLit, StrPart, Token, TokenKind};

/// Lexes `src` and applies the statement termination rules (DESIGN §3.2):
/// the stream the parser consumes.
pub fn tokenize(file: FileId, src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let (tokens, diagnostics) = lex(file, src);
    (insert_terminators(tokens), diagnostics)
}

/// Lexes the part of `src` covered by `range` (an interpolation hole) and
/// applies the termination rules. Spans stay absolute: they point into `src`,
/// not into the slice.
pub fn tokenize_range(file: FileId, src: &str, range: Span) -> (Vec<Token>, Vec<Diagnostic>) {
    let base = range.start;
    let slice = &src[range.start as usize..range.end as usize];
    let (mut tokens, mut diagnostics) = tokenize(file, slice);
    let shift = |s: &mut Span| {
        s.start += base;
        s.end += base;
    };
    for token in &mut tokens {
        shift(&mut token.span);
        if let TokenKind::Str(lit) = &mut token.kind {
            for part in &mut lit.parts {
                if let StrPart::Interp { expr, .. } = part {
                    shift(expr);
                }
            }
        }
    }
    for diagnostic in &mut diagnostics {
        shift(&mut diagnostic.span);
        for (span, _) in &mut diagnostic.labels {
            shift(span);
        }
    }
    (tokens, diagnostics)
}
