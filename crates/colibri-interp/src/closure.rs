//! Closure objects and variable cells (ARCHITECTURE §3).
//!
//! A closure is a lifted function plus the variables it captured. A *by-value* capture holds a
//! copy of the value, a *cell* capture shares a [`CellObj`] with the frame that owns the
//! variable (and with every other closure that captured it), and a *weak* capture holds a weak
//! reference to a class object. Like the other heap objects these die only through
//! `Interp::release`, so what a closure captured dies when the closure does.

use std::cell::RefCell;
use std::fmt;

use colibri_check::hir::{CaptureMode, FnId, LocalId};

use crate::value::Value;

/// A variable that lives in a shared cell: written by whoever holds the cell, seen by all.
pub struct CellObj {
    value: RefCell<Value>,
}

impl CellObj {
    pub fn new(value: Value) -> std::rc::Rc<CellObj> {
        std::rc::Rc::new(CellObj {
            value: RefCell::new(value),
        })
    }

    /// The current value (a reference bump for shared values).
    pub fn get(&self) -> Value {
        self.value.borrow().clone()
    }

    /// Stores a value and returns the old one, to be released by the caller.
    pub fn set(&self, v: Value) -> Value {
        std::mem::replace(&mut *self.value.borrow_mut(), v)
    }

    /// Takes the value out, leaving `()`. Only `release` and a moving receiver do this.
    pub fn take(&self) -> Value {
        self.set(Value::Unit)
    }

    /// Changes the value in place with `f`, which must not run Colibri code.
    pub fn update<R>(&self, f: impl FnOnce(&mut Value) -> R) -> R {
        f(&mut self.value.borrow_mut())
    }
}

impl fmt::Debug for CellObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cell")
    }
}

/// One captured variable.
#[derive(Debug)]
pub struct Captured {
    /// The local of the closure's body that holds it while the closure runs.
    pub inner: LocalId,
    pub mode: CaptureMode,
    /// The copy (`ByValue`), the shared [`Value::Cell`] (`Cell`) or the [`Value::Weak`] (`Weak`).
    pub value: Value,
}

/// A closure: the lifted function and its captured variables.
#[derive(Debug)]
pub struct ClosureObj {
    pub f: FnId,
    pub captures: Vec<Captured>,
}

impl ClosureObj {
    /// Moves the captured values out, for destruction. Only `release` calls this.
    pub fn take_values(&mut self) -> Vec<Value> {
        self.captures
            .iter_mut()
            .map(|c| std::mem::replace(&mut c.value, Value::Unit))
            .collect()
    }
}

// ---- running closures ----

use std::rc::Rc;

use colibri_check::hir::ClosureExpr;
use colibri_syntax::Span;

use crate::ctl::Eval;
use crate::eval::Interp;
use crate::fiber::Fiber;

impl Interp {
    /// Builds the closure value for `c`: each capture reads its variable in the running frame.
    pub(crate) fn make_closure(&mut self, fiber: &mut Fiber, c: &ClosureExpr, span: Span) -> Eval {
        let mut captures = Vec::with_capacity(c.captures.len());
        for cap in &c.captures {
            let slot = fiber
                .frames
                .last()
                .and_then(|f| f.slots.get(cap.outer.0 as usize))
                .cloned();
            let value = match (cap.mode, slot) {
                (CaptureMode::ByValue, Some(Value::Cell(cell))) => Some(cell.get()),
                (CaptureMode::ByValue, Some(v)) => Some(v),
                (CaptureMode::Cell, Some(v @ Value::Cell(_))) => Some(v),
                (CaptureMode::Weak, Some(v)) => match crate::eval::unwrap_cell_ref(&v) {
                    Value::Obj(o) => Some(Value::Weak(Rc::downgrade(&o))),
                    _ => None,
                },
                _ => None,
            };
            match value {
                Some(value) => captures.push(Captured {
                    inner: cap.inner,
                    mode: cap.mode,
                    value,
                }),
                None => {
                    let released = captures.into_iter().map(|c: Captured| c.value).collect();
                    self.release_all(released);
                    return Err(self.internal(
                        fiber,
                        span,
                        "a closure captured a missing variable",
                    ));
                }
            }
        }
        Ok(Value::Closure(Rc::new(ClosureObj { f: c.f, captures })))
    }

    /// Calls a function value (`Fn` or `Closure`) with `args`, which the callee owns. The
    /// caller keeps `f` alive (it sits on its temporaries) for the whole call.
    pub(crate) fn call_function_value(
        &mut self,
        fiber: &mut Fiber,
        f: &Value,
        args: Vec<Value>,
        span: Span,
    ) -> Eval {
        match f {
            Value::Fn(id, _) => {
                let Some(body) = self.fns.get(id.0 as usize).cloned() else {
                    self.release_all(args);
                    return Err(self.internal(fiber, span, "unknown function"));
                };
                self.call_body(fiber, body, args)
            }
            Value::Closure(c) => {
                let Some(body) = self.fns.get(c.f.0 as usize).cloned() else {
                    self.release_all(args);
                    return Err(self.internal(fiber, span, "unknown closure body"));
                };
                let captures = c
                    .captures
                    .iter()
                    .map(|cap| {
                        let value = match (&cap.mode, &cap.value) {
                            // A weak capture is a strong `T?` for the length of the call.
                            (CaptureMode::Weak, Value::Weak(w)) => {
                                Value::option(w.upgrade().map(Value::Obj))
                            }
                            (_, v) => v.clone(),
                        };
                        (cap.inner, value)
                    })
                    .collect();
                self.call_frame(fiber, body, args, captures).0
            }
            _ => {
                self.release_all(args);
                Err(self.internal(fiber, span, "a call of something that is not a function"))
            }
        }
    }
}
