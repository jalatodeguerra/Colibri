//! The one control channel of the evaluator (ARCHITECTURE §2).

use colibri_syntax::Span;

use crate::value::Value;

/// Why evaluation stopped early. No Rust panic is ever part of the semantics.
#[derive(Debug)]
pub enum Ctl {
    Panic(Box<PanicInfo>),
    /// Coroutine cancellation: unwinds like a panic without a message.
    Cancel,
}

/// The result of evaluating something.
pub type Eval<T = Value> = Result<T, Ctl>;

/// One line of a stack trace.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceEntry {
    /// The function's name; `<script>` and `<init x>` for module-level code.
    pub function: String,
    /// Where that function was (the faulting operation, or the call it was making).
    pub span: Span,
}

/// A panic: its message, where it happened and the Colibri stack at that moment (innermost
/// call first).
#[derive(Clone, Debug, PartialEq)]
pub struct PanicInfo {
    pub message: String,
    pub span: Span,
    pub stack: Vec<TraceEntry>,
    /// Further panics that happened while this one was unwinding (in `defer` statements); the
    /// first panic is the one reported and these ride along.
    pub notes: Vec<String>,
    /// A limit the host set ran out (EMBEDDING §5): the instruction budget, or with `memory` set
    /// the memory limit. The panic is not the program's own: it is never a `join_result`'s value
    /// and nobody takes it over.
    pub budget: bool,
    /// The memory limit in bytes that was exceeded; set together with `budget`.
    pub memory: Option<u64>,
    /// Set by a debugger that stopped at this panic (`colibri_vm::debug`): which one it was, so
    /// that the panic is not reported again as it passes through further frames. `0` otherwise.
    pub debug_serial: u64,
}

/// Evaluation outcomes the host can see: only panics and cancellation leave the engine.
#[doc(hidden)]
pub fn into_panic(ctl: Ctl, fallback: Span) -> Box<PanicInfo> {
    match ctl {
        Ctl::Panic(info) => info,
        Ctl::Cancel => Box::new(PanicInfo {
            message: "cancelled".to_string(),
            span: fallback,
            stack: Vec::new(),
            notes: Vec::new(),
            budget: false,
            debug_serial: 0,
            memory: None,
        }),
    }
}
