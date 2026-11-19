//! `colibri fmt`: the canonical formatter (DESIGN §16.1). See ARCHITECTURE §12.

mod comments;
mod doc;
mod print;

use crate::ast_dump::dump_file;
use crate::{Diagnostic, FileId, Span, parse_file};

/// Formats Colibri source text.
///
/// Refuses (returns the diagnostics) a file with syntax errors. `name` is only used to say
/// which file broke an internal check. The result parses to the same syntax tree as `text`
/// and keeps every comment; formatting it again changes nothing.
pub fn format_source(name: &str, text: &str) -> Result<String, Vec<Diagnostic>> {
    let file = FileId(0);
    let (ast, diagnostics) = parse_file(file, text);
    if diagnostics.iter().any(Diagnostic::is_error) {
        return Err(diagnostics);
    }
    let out = print::format_file(text, &ast);
    // The formatter must never change a program or lose a comment: check, and refuse to
    // return text that does.
    let (again, again_diags) = parse_file(file, &out);
    let problem = if again_diags.iter().any(Diagnostic::is_error) {
        Some("the formatted text does not parse")
    } else if dump_file(&again) != dump_file(&ast) {
        Some("the formatted text parses to a different program")
    } else if comments::comment_texts(&out) != comments::comment_texts(text) {
        Some("the formatted text lost or changed a comment")
    } else {
        None
    };
    match problem {
        None => Ok(out),
        Some(problem) => Err(vec![
            Diagnostic::error(
                format!("internal formatter error in {name}: {problem}"),
                Span::new(file, 0, 0),
            )
            .with_hint("this is a bug in `colibri fmt`; the file was left alone"),
        ]),
    }
}
