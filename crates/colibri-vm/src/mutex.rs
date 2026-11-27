//! `Mutex[T]` (DESIGN §14.1, ARCHITECTURE §36.7): the engine's half.
//!
//! The value, the lock and the handle are [`colibri_core::mutex`]; this is what needs the machine:
//! copying a value into the mutex's portable form and out of it on a walk stack, and running the
//! callback of `with` as a native call while the lock is held.

use std::ops::Range;
use std::sync::Arc;

use colibri_check::hir::Builtin;
use colibri_core::ctl::{Ctl, Eval};
use colibri_core::mutex::{MutexInner, address};
use colibri_core::portable::{Portable, RebuildError, Refused, from_portable_ref};
use colibri_core::prelude::*;
use colibri_core::shared::holds_a_cell;
use colibri_core::value::Value;
use colibri_syntax::Span;

use crate::fiber::VmFiber;
use crate::machine::Machine;
use crate::stacks::{WalkRole, on_walk_stack};

impl Machine {
    /// Copies `value` into its portable form, for a `Mutex` to keep. `value` is released.
    fn mutex_portable(
        &mut self,
        fiber: &mut VmFiber,
        value: Value,
        span: Span,
    ) -> Result<Portable, Ctl> {
        let budget = self.native_stack_budget;
        let copied = on_walk_stack(WalkRole::Sender, budget, |guard| {
            self.to_portable(&value, guard)
        });
        self.release(value);
        let root = match copied {
            Ok(root) => root,
            Err(e) if e.refused == Refused::MemoryLimit => {
                return Err(self.memory_exceeded(fiber, span));
            }
            Err(e) => return Err(self.panic(fiber, span, e.to_string())),
        };
        if holds_a_cell(&root) {
            return Err(self.panic(
                fiber,
                span,
                "a `Mutex` cannot hold a closure that captures a `let mut` variable: its copy would not follow the variable",
            ));
        }
        Ok(root)
    }

    /// `Mutex(x)`: copies `x` into the mutex and gives its handle.
    pub(crate) fn new_mutex(
        &mut self,
        fiber: &mut VmFiber,
        args: Range<usize>,
        span: Span,
    ) -> Eval {
        let value = std::mem::replace(&mut fiber.regs[args.start], Value::Unit);
        let root = self.mutex_portable(fiber, value, span)?;
        Ok(self.core.new_mutex(root))
    }

    /// A copy of the value of `inner`, for this isolate. The lock is held by `held`.
    fn mutex_copy(
        &mut self,
        fiber: &mut VmFiber,
        held: &colibri_core::mutex::Held<'_>,
        span: Span,
    ) -> Result<Value, Ctl> {
        let budget = self.native_stack_budget;
        let built = held.read(|root| {
            on_walk_stack(WalkRole::Receiver, budget, |guard| {
                from_portable_ref(root, &mut self.core, guard)
            })
        });
        match built {
            Ok(copy) => Ok(copy),
            Err(RebuildError::MemoryLimit) => Err(self.memory_exceeded(fiber, span)),
            Err(e) => Err(self.panic(
                fiber,
                span,
                format!("a `Mutex` value cannot be rebuilt here: {e}"),
            )),
        }
    }

    /// Takes the lock of `inner`. A thread that already holds a lock (the callback of a `with` is
    /// running) never waits for another one.
    fn mutex_hold<'a>(
        &mut self,
        fiber: &mut VmFiber,
        inner: &'a Arc<MutexInner>,
        span: Span,
    ) -> Result<colibri_core::mutex::Held<'a>, Ctl> {
        if let Some(held) = self.core.mutex_held() {
            let message = if held == address(inner) {
                "this `Mutex` is locked by the `with` that is running: its callback cannot use the same mutex (`get`, `set` and `with` would wait for the lock they hold)"
            } else {
                "a `with` callback cannot use another `Mutex`: threads that took two mutexes in opposite orders would wait for each other for ever (read the other one before the `with`, or keep both values in one `Mutex`)"
            };
            return Err(self.panic(fiber, span, message));
        }
        let core = &self.core;
        match inner.hold(&|| core.cancelled()) {
            Ok(held) => Ok(held),
            // The `Vm` was asked to stop while the lock was taken: as a `join` that is stopped.
            Err(_) => Err(self.budget_exhausted(fiber, span)),
        }
    }

    /// `get`, `set` and `with` of a `Mutex`.
    pub(crate) fn mutex_ops(
        &mut self,
        fiber: &mut VmFiber,
        op: Builtin,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        let Some(inner) = args.first().and_then(|r| self.core.mutex_of(r)) else {
            return Err(self.internal(fiber, span, "a `Mutex` method on something that is not one"));
        };
        match op {
            Builtin::MutexGet => {
                let held = self.mutex_hold(fiber, &inner, span)?;
                self.mutex_copy(fiber, &held, span)
            }
            Builtin::MutexSet => {
                // The copy is made before the lock is taken: it is held only for the swap.
                let value = std::mem::replace(&mut args[1], Value::Unit);
                let new = self.mutex_portable(fiber, value, span)?;
                let held = self.mutex_hold(fiber, &inner, span)?;
                drop(held.store(new));
                Ok(Value::Unit)
            }
            Builtin::MutexWith => {
                let f = std::mem::replace(&mut args[1], Value::Unit);
                let result = self.mutex_with(fiber, &inner, &f, span);
                self.release(f);
                result
            }
            _ => Err(self.internal(fiber, span, "not a method of `Mutex`")),
        }
    }

    /// `m.with(f)`: takes the lock, gives `f` a copy of the value, and stores what `f` leaves in
    /// its parameter. If `f` panics (or is stopped), nothing is stored: the value is as it was
    /// and the lock is let go.
    fn mutex_with(
        &mut self,
        fiber: &mut VmFiber,
        inner: &Arc<MutexInner>,
        f: &Value,
        span: Span,
    ) -> Eval {
        let held = self.mutex_hold(fiber, inner, span)?;
        let copy = self.mutex_copy(fiber, &held, span)?;
        // From here to the end, this isolate holds the lock: it uses no mutex, and nothing in it
        // can pause or wait (the callback is native, so `wait` and `join` refuse).
        self.core.set_mutex_held(Some(address(inner)));
        Self::enter_native(fiber);
        let (outcome, last) = self.call_taking_first(fiber, f, vec![copy], span);
        Self::leave_native(fiber);
        let stored = match outcome {
            Err(ctl) => {
                if let Some(x) = last {
                    self.release(x);
                }
                Err(ctl)
            }
            Ok(result) => match last {
                Some(x) => match self.mutex_portable(fiber, x, span) {
                    Ok(new) => Ok((result, new)),
                    Err(ctl) => {
                        self.release(result);
                        Err(ctl)
                    }
                },
                None => {
                    self.release(result);
                    Err(self.internal(fiber, span, "the callback of `with` gave no value back"))
                }
            },
        };
        self.core.set_mutex_held(None);
        let (result, new) = stored?;
        drop(held.store(new));
        Ok(result)
    }
}
