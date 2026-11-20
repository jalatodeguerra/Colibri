//! Closures at run time: building them from the running frame and calling function values.
//!
//! The closure objects themselves are in `colibri_core::closure`.

use std::rc::Rc;

use colibri_check::hir::{CaptureMode, ClosureExpr};
use colibri_syntax::Span;

pub(crate) use colibri_core::closure::{Captured, ClosureObj};

use crate::ctl::Eval;
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::value::Value;

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
        let closure = Rc::new(ClosureObj {
            f: c.f,
            generation: self.core.generation(),
            captures,
        });
        self.core.track_closure(&closure, Some(span));
        Ok(Value::Closure(closure))
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
                let Some(body) = self.closure_body(c) else {
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
