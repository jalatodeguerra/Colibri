//! Shared front-end types for Colibri: source files, spans and diagnostics.

mod diagnostic;
mod source;

pub use diagnostic::{Diagnostic, Severity};
pub use source::{FileId, SourceMap, Span};
