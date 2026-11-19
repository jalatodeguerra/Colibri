//! Shared front-end types for Colibri: source files, spans, diagnostics and
//! the lexer.

mod diagnostic;
mod lexer;
mod source;
mod terminate;
mod token;

pub use diagnostic::{Diagnostic, Severity};
pub use lexer::lex;
pub use source::{FileId, SourceMap, Span};
pub use terminate::insert_terminators;
pub use token::{StrLit, StrPart, Token, TokenKind};

/// Lexes `src` and applies the statement termination rules (DESIGN §3.2):
/// the stream the parser consumes.
pub fn tokenize(file: FileId, src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let (tokens, diagnostics) = lex(file, src);
    (insert_terminators(tokens), diagnostics)
}
