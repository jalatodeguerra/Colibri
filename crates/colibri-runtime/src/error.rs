//! The error type of the embedding API.

use std::fmt;

use colibri_syntax::{Diagnostic, SourceMap};

use crate::render::render_diagnostic;

/// Diagnostics together with the sources they point into.
#[derive(Clone, Debug)]
pub struct Report {
    pub sources: SourceMap,
    pub diagnostics: Vec<Diagnostic>,
}

impl Report {
    /// Whether any diagnostic is an error (warnings alone do not stop a program).
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }

    /// Every diagnostic rendered with source snippets (plain text when `color` is false).
    pub fn render(&self, color: bool) -> String {
        self.diagnostics
            .iter()
            .map(|d| render_diagnostic(d, &self.sources, color))
            .collect()
    }
}

/// A position in a source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub file: String,
    /// 1-based.
    pub line: u32,
    /// 1-based, in characters.
    pub column: u32,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

/// One entry of a Colibri stack trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackFrame {
    /// The function's name (`<script>` for top-level statements).
    pub function: String,
    pub location: Location,
}

/// What can go wrong when loading or running a script.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The file could not be read.
    Io { path: String, message: String },
    /// The source has errors; the report holds the diagnostics and the sources.
    Compile(Report),
    /// The program panicked (DESIGN §10.4). The `Vm` stays usable.
    Panic {
        message: String,
        /// Where the panic happened; `None` only for internal errors.
        location: Option<Location>,
        /// The Colibri call stack, innermost first.
        stack: Vec<StackFrame>,
        /// Further panics raised by `defer` statements while this one unwound (DESIGN §6.1).
        notes: Vec<String>,
    },
    /// No function with this name is defined.
    FnNotFound { name: String },
    /// The Rust-side signature differs from the Colibri function's.
    SignatureMismatch {
        name: String,
        /// The signature in the Colibri source.
        colibri: String,
        /// The signature the Rust types ask for.
        rust: String,
    },
    /// A program is already loaded; one `Vm` runs one program for now.
    AlreadyLoaded,
    /// No program is loaded.
    NotLoaded,
    /// The function handle belongs to another `Vm`, or to a program that has since been
    /// replaced.
    StaleHandle,
    /// A value could not be converted (a bug in the embedding layer).
    Internal(String),
}

/// How many frames of a long stack trace are shown at each end.
const TRACE_EDGE: usize = 12;

impl Error {
    /// A human-readable form: rendered diagnostics for compile errors, `panic: message` and a
    /// stack trace for panics, the message for the rest.
    pub fn render(&self, color: bool) -> String {
        match self {
            Error::Compile(report) => report.render(color),
            Error::Panic {
                message,
                location,
                stack,
                notes,
            } => {
                let mut out = format!("panic: {message}\n");
                if stack.is_empty()
                    && let Some(at) = location
                {
                    out.push_str(&format!("  at {at}\n"));
                }
                let shown = |out: &mut String, frame: &StackFrame| {
                    out.push_str(&format!("  at {} ({})\n", frame.function, frame.location));
                };
                if stack.len() > 2 * TRACE_EDGE {
                    stack[..TRACE_EDGE].iter().for_each(|f| shown(&mut out, f));
                    let hidden = stack.len() - 2 * TRACE_EDGE;
                    out.push_str(&format!("  ... {hidden} more calls ...\n"));
                    stack[stack.len() - TRACE_EDGE..]
                        .iter()
                        .for_each(|f| shown(&mut out, f));
                } else {
                    stack.iter().for_each(|f| shown(&mut out, f));
                }
                for note in notes {
                    out.push_str(&format!("  note: {note}\n"));
                }
                out
            }
            other => format!("{other}\n"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, message } => write!(f, "cannot read `{path}`: {message}"),
            Error::Compile(report) => {
                let errors: Vec<&Diagnostic> =
                    report.diagnostics.iter().filter(|d| d.is_error()).collect();
                write!(f, "{} compile error(s)", errors.len())?;
                for d in errors {
                    let (line, col) = report.sources.line_col(d.span.file, d.span.start);
                    write!(
                        f,
                        "\n  {}:{line}:{col}: {}",
                        report.sources.name(d.span.file),
                        d.message
                    )?;
                }
                Ok(())
            }
            Error::Panic {
                message, location, ..
            } => match location {
                Some(at) => write!(f, "panic: {message} (at {at})"),
                None => write!(f, "panic: {message}"),
            },
            Error::FnNotFound { name } => write!(f, "no function named `{name}`"),
            Error::SignatureMismatch {
                name,
                colibri,
                rust,
            } => write!(
                f,
                "`{name}` is `{colibri}` in Colibri, but the Rust side uses it as `{rust}`"
            ),
            Error::AlreadyLoaded => f.write_str("a program is already loaded in this Vm"),
            Error::NotLoaded => f.write_str("no program is loaded"),
            Error::StaleHandle => {
                f.write_str("this function handle belongs to another Vm or to a replaced program")
            }
            Error::Internal(message) => write!(f, "internal error: {message}"),
        }
    }
}

impl std::error::Error for Error {}
