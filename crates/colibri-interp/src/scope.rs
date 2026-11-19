//! Leaving scopes: the one place where bindings die and `defer` statements run (DESIGN §6.1,
//! §11.2; ARCHITECTURE §6).
//!
//! Each frame keeps one last-in first-out list of cleanups. Binding a local pushes it, and a
//! `defer` pushes its statement. A scope remembers how long the list was when it began and, on
//! every way out (normal end, `break`, `continue`, `return`, panic), runs the entries above
//! that mark in reverse. Drops and defers therefore interleave in exactly the order they were
//! registered.

use std::rc::Rc;

use colibri_check::hir::{LocalId, Stmt};

use crate::ctl::{Ctl, Eval, PanicInfo};
use crate::eval::Interp;
use crate::fiber::{Cleanup, Fiber};
use crate::value::Value;

impl Interp {
    /// Where the current scope's cleanups start.
    pub(crate) fn scope_mark(fiber: &Fiber) -> usize {
        fiber.frames.last().map_or(0, |f| f.cleanups.len())
    }

    /// Registers a freshly bound local for release at scope exit.
    pub(crate) fn push_slot(fiber: &mut Fiber, local: LocalId) {
        if let Some(frame) = fiber.frames.last_mut() {
            frame.cleanups.push(Cleanup::Slot(local));
        }
    }

    /// Registers a `defer` statement.
    pub(crate) fn push_defer(fiber: &mut Fiber, stmt: Rc<Stmt>) {
        if let Some(frame) = fiber.frames.last_mut() {
            frame.cleanups.push(Cleanup::Defer(stmt));
        }
    }

    /// Stores `value` in a local's slot, registers it for release and releases whatever the
    /// slot held.
    pub(crate) fn bind_local(&mut self, fiber: &mut Fiber, local: LocalId, value: Value) {
        let old = match fiber.frames.last_mut() {
            Some(frame) => match frame.slots.get_mut(local.0 as usize) {
                Some(slot) => std::mem::replace(slot, value),
                None => value,
            },
            None => value,
        };
        Self::push_slot(fiber, local);
        self.release(old);
    }

    /// Runs the cleanups above `mark`, last in first out, and returns `result` with their
    /// effect folded in: a panic in a cleanup replaces a result that was not a panic, and
    /// becomes a note of one that was, so every remaining cleanup still runs.
    pub(crate) fn exit_scope(&mut self, fiber: &mut Fiber, mark: usize, result: Eval) -> Eval {
        let mut result = result;
        loop {
            let entry = match fiber.frames.last_mut() {
                Some(frame) if frame.cleanups.len() > mark => frame.cleanups.pop(),
                _ => None,
            };
            match entry {
                None => return self.fold_hook_panics(result),
                Some(Cleanup::Slot(local)) => {
                    let old = match fiber.frames.last_mut() {
                        Some(frame) => frame
                            .slots
                            .get_mut(local.0 as usize)
                            .map(|s| std::mem::replace(s, Value::Unit)),
                        None => None,
                    };
                    if let Some(old) = old {
                        self.release(old);
                    }
                }
                Some(Cleanup::Defer(stmt)) => {
                    if let Err(ctl) = self.exec_stmt(fiber, &stmt) {
                        result = self.fold_cleanup_failure(result, ctl);
                    }
                }
            }
        }
    }

    /// A deferred statement ended abnormally while `result` was already on its way out.
    fn fold_cleanup_failure(&mut self, result: Eval, failure: Ctl) -> Eval {
        let failure = match failure {
            Ctl::Panic(info) => info,
            Ctl::Cancel => Box::new(PanicInfo {
                message: "cancelled".to_string(),
                span: colibri_syntax::Span::new(colibri_syntax::FileId(0), 0, 0),
                stack: Vec::new(),
                notes: Vec::new(),
            }),
            // The checker does not allow these in a `defer`; they carry nothing to keep.
            Ctl::Break(_, v) | Ctl::Return(v) => {
                self.release(v);
                return result;
            }
            Ctl::Continue(_) => return result,
        };
        match result {
            Err(Ctl::Panic(mut first)) => {
                first.notes.push(format!(
                    "a deferred statement also panicked: {}",
                    failure.message
                ));
                first.notes.extend(failure.notes);
                Err(Ctl::Panic(first))
            }
            other => {
                self.release_outcome(other);
                Err(Ctl::Panic(failure))
            }
        }
    }

    /// Folds the panics of `Drop` hooks that ran meanwhile into `result`: the first panic is
    /// the one reported, later ones ride along as notes (ARCHITECTURE §6).
    pub(crate) fn fold_hook_panics(&mut self, result: Eval) -> Eval {
        if self.hook_panics.is_empty() {
            return result;
        }
        let mut panics = std::mem::take(&mut self.hook_panics).into_iter();
        let mut result = result;
        for failure in &mut panics {
            result = match result {
                Err(Ctl::Panic(mut first)) => {
                    first
                        .notes
                        .push(format!("a `Drop` hook also panicked: {}", failure.message));
                    first.notes.extend(failure.notes);
                    Err(Ctl::Panic(first))
                }
                other => {
                    self.release_outcome(other);
                    Err(Ctl::Panic(Box::new(failure)))
                }
            };
        }
        result
    }

    /// Releases the value an evaluation produced, when it produced one.
    pub(crate) fn release_outcome(&mut self, result: Eval) {
        match result {
            Ok(v) | Err(Ctl::Return(v) | Ctl::Break(_, v)) => self.release(v),
            Err(_) => {}
        }
    }
}
