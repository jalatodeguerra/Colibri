//! The error type of the embedding API.

use std::fmt;

use colibri_syntax::{Diagnostic, SourceMap};

use crate::render::render_diagnostics;

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
        render_diagnostics(&self.diagnostics, &self.sources, color)
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
    /// `main` returned `err(e)` (DESIGN §10.3): `message` is the whole error chain. The CLI
    /// prints it to stderr and exits with code 1 (a panic exits with 101).
    MainReturnedErr { message: String },
    /// A test (`Vm::run_test`) returned `err(e)`: `message` is the whole error chain.
    TestReturnedErr { message: String },
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
    /// A value could not be converted: it has another Colibri type than the Rust side
    /// expects, or does not fit the Rust type (a negative `int` for a `usize`).
    Convert(String),
    /// An internal inconsistency of the embedding layer.
    Internal(String),
    /// A host function or type could not be registered (EMBEDDING §1): the name is taken, is not
    /// a valid name, or a type was described in two ways.
    Registration { name: String, message: String },
    /// The instruction budget ran out (EMBEDDING §5). The `Vm` stays usable; the next entry gets
    /// a fresh allotment.
    BudgetExhausted {
        /// Where the script was when the budget ran out; `None` if nothing was running.
        location: Option<Location>,
        /// The Colibri call stack, innermost first.
        stack: Vec<StackFrame>,
    },
    /// An interface file (`.cbi`) does not match what the host registered.
    InterfaceMismatch {
        path: String,
        /// The hash of the interface the host registered.
        expected: String,
        /// The hash the file says it has, if it could be read.
        found: Option<String>,
    },
    /// An interface file could not be understood.
    Interface { path: String, message: String },
    /// A host object could not be used: the host destroyed it, an enclosing call has it
    /// borrowed, or it belongs to the script. The message is the one a script would see.
    HostObject(String),
    /// An `extern` function could not be bound to its C library at load time: the library or
    /// the symbol is missing, or this build has no C FFI (the `ffi` feature is off).
    Ffi(String),
}

/// How many frames of a long stack trace are shown at each end, unless the environment variable
/// `COLIBRI_FULL_TRACE` is set to something other than `0`.
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
            } => render_trace(format!("panic: {message}\n"), location, stack, notes),
            Error::BudgetExhausted { location, stack } => render_trace(
                "error: the instruction budget is exhausted\n".to_string(),
                location,
                stack,
                &[],
            ),
            Error::MainReturnedErr { message } | Error::TestReturnedErr { message } => {
                format!("error: {message}\n")
            }
            other => format!("{other}\n"),
        }
    }
}

/// A headline, then the stack, then the notes.
fn render_trace(
    mut out: String,
    location: &Option<Location>,
    stack: &[StackFrame],
    notes: &[String],
) -> String {
    if stack.is_empty()
        && let Some(at) = location
    {
        out.push_str(&format!("  at {at}\n"));
    }
    let shown = |out: &mut String, frame: &StackFrame| {
        out.push_str(&format!("  at {} ({})\n", frame.function, frame.location));
    };
    let full = std::env::var_os("COLIBRI_FULL_TRACE").is_some_and(|v| !v.is_empty() && v != "0");
    if stack.len() > 2 * TRACE_EDGE && !full {
        stack[..TRACE_EDGE].iter().for_each(|f| shown(&mut out, f));
        let hidden = stack.len() - 2 * TRACE_EDGE;
        out.push_str(&format!(
            "  ... {hidden} more calls (set COLIBRI_FULL_TRACE=1 to show them all) ...\n"
        ));
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
            Error::MainReturnedErr { message } | Error::TestReturnedErr { message } => {
                write!(f, "error: {message}")
            }
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
            Error::Convert(message) => write!(f, "cannot convert the value: {message}"),
            Error::Internal(message) => write!(f, "internal error: {message}"),
            Error::Registration { name, message } => {
                write!(f, "cannot register `{name}`: {message}")
            }
            Error::BudgetExhausted { location, .. } => match location {
                Some(at) => write!(f, "the instruction budget is exhausted (at {at})"),
                None => f.write_str("the instruction budget is exhausted"),
            },
            Error::InterfaceMismatch {
                path,
                expected,
                found,
            } => match found {
                Some(found) => write!(
                    f,
                    "the interface file `{path}` has hash {found}, but the host's interface has hash {expected}"
                ),
                None => write!(
                    f,
                    "the interface file `{path}` does not match the host's interface (hash {expected})"
                ),
            },
            Error::Interface { path, message } => write!(f, "{path}: {message}"),
            Error::HostObject(message) => f.write_str(message),
            Error::Ffi(message) => write!(f, "cannot link C: {message}"),
        }
    }
}

impl std::error::Error for Error {}
