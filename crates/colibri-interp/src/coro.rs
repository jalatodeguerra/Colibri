//! Coroutines and their scheduler (DESIGN §14.2, ARCHITECTURE §16).
//!
//! A coroutine is a [`Fiber`] running the ordinary recursive evaluator on its own native stack
//! (`corosensei`). `wait` and friends suspend that stack; the scheduler ([`Scheduler`], one per
//! `Interp`) resumes the ones that are due when the host calls `tick`. Cancellation resumes a
//! suspended coroutine with a cancel signal, so its pause returns `Err(Ctl::Cancel)` and the
//! ordinary unwinding (`exit_scope`) runs its `defer`s and drops. No Rust panic or forced unwind
//! is part of the semantics: every coroutine is run to its end before its stack is freed.
//!
//! The `Interp` must not move while a coroutine exists (the suspended stacks hold references
//! into it); the runtime keeps it in a `Box`.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;

use colibri_check::hir::{Callee, Expr, ExprKind, Intrinsic};
use colibri_check::types::{COGROUP_TYPE, COROUTINE_TYPE, SIGNAL_TYPE, Ty};
use colibri_core::clock::Clock;
use colibri_core::external::{
    CancelToken, Completer, ExternalError, panic_message, unwrap_outcome,
};
use colibri_core::prelude::*;
use colibri_core::sched::*;
use colibri_syntax::Span;
use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};

use crate::budget::Budget;
use crate::ctl::{Ctl, Eval, PanicInfo};
use crate::eval::{Interp, stack_position};
use crate::fiber::Fiber;
use crate::heap::ClassObj;
use crate::value::Value;

/// How much of a coroutine's stack the evaluator may use before it panics with "stack
/// overflow": the rest is for host functions and built-ins, which are not measured.
const STACK_USABLE_NUM: usize = 3;
const STACK_USABLE_DEN: usize = 4;

/// What a resumption tells the coroutine.
pub(crate) struct Resume {
    pub interp: *mut Interp,
    pub cancel: bool,
}

pub(crate) type Y = Yielder<Resume, *mut Interp>;
pub(crate) type Co = Coroutine<Resume, *mut Interp, Finish, DefaultStack>;

impl Interp {
    // ---- configuration and inspection ----

    /// The size of the native stack given to each coroutine (default 16 MiB).
    pub fn set_coroutine_stack_size(&mut self, bytes: usize) {
        self.core.sched.stack_size = bytes.max(16 * 1024);
    }

    /// Replaces the clock behind `wait_real` and the event loop.
    pub fn set_clock(&mut self, clock: Box<dyn Clock>) {
        self.core.sched.clock = clock;
    }

    /// The reading of the clock, in seconds.
    pub fn clock_now(&mut self) -> f64 {
        self.core.sched.clock.now()
    }

    /// Sleeps on the clock.
    pub fn clock_sleep(&mut self, seconds: f64) {
        let s = &mut self.core.sched;
        if !s.ext.has_pending() {
            s.clock.sleep(seconds);
        } else {
            // Background operations are running: a completion ends the sleep early.
            let ext = &mut s.ext;
            s.clock.sleep_interruptible(seconds, &mut |d| ext.wait(d));
        }
    }

    /// How many coroutines are alive (started and not yet finished).
    pub fn coroutine_count(&self) -> usize {
        self.core
            .sched
            .entries
            .iter()
            .filter(|e| e.state != State::Finished)
            .count()
    }

    /// How many coroutine resumptions have happened (starts, ticks and cancellations).
    pub fn coroutine_resumptions(&self) -> u64 {
        self.core.sched.resumptions
    }

    /// How long until the next coroutine needs a tick, for an event loop: `None` when nothing
    /// is alive or every live coroutine waits for a signal (nothing but an emit can wake it), `Some(0.0)`-ish small polling interval when something is checked at every tick
    /// (`wait_until`, `join`), else the time to the earliest timed wake.
    pub fn next_wake(&mut self, poll: f64) -> Option<f64> {
        let mut best: Option<f64> = None;
        let mut real_now: Option<f64> = None;
        let time = self.core.sched.time;
        let mut any = false;
        for e in &self.core.sched.entries {
            if e.state == State::Finished {
                continue;
            }
            // A coroutine waiting for a signal needs no tick of its own: only an emit (from
            // code that runs at a tick or before it) can wake it.
            if e.state == State::Suspended && matches!(e.wake, Wake::Signal(_)) {
                continue;
            }
            any = true;
            let delay = match &e.wake {
                Wake::Signaled => 0.0,
                Wake::Time(d) => (d - time).max(0.0),
                Wake::Real(d) => {
                    let now = *real_now.get_or_insert_with(|| self.core.sched.clock.now());
                    (d - now).max(0.0)
                }
                // Only a completion (which ends the sleep) can wake it.
                Wake::External(_) => f64::INFINITY,
                Wake::Ready | Wake::Join(_) | Wake::Group(_) | Wake::Signal(_) => poll,
            };
            best = Some(best.map_or(delay, |b| b.min(delay)));
        }
        if any {
            best
        } else if !self.core.sched.ext.has_pending() {
            None
        } else {
            // The main fiber waits for a background operation: only its completion helps.
            Some(f64::INFINITY)
        }
    }

    // ---- ticking ----

    /// Resumes every due coroutine once, in start order. A coroutine started during the tick
    /// is not resumed in it. Each resumption gets its own instruction budget.
    pub fn tick(&mut self, fiber: &mut Fiber, dt: f64) -> Result<(), Box<PanicInfo>> {
        let _ = fiber;
        let outermost = self.core.begin_entry();
        if outermost {
            self.drain_root_drops();
        }
        let result = self.tick_core(dt);
        self.core.end_entry(outermost, false);
        match result {
            Ok(()) => Ok(()),
            Err(TickError::Failure(info)) => Err(info),
            Err(TickError::Reentrant) => Err(Box::new(PanicInfo {
                message: "`tick` cannot be called from a coroutine or from inside a tick"
                    .to_string(),
                span: Span::new(colibri_syntax::FileId(0), 0, 0),
                stack: Vec::new(),
                notes: Vec::new(),
                budget: false,
            })),
        }
    }

    fn tick_core(&mut self, dt: f64) -> Result<(), TickError> {
        self.tick_run(dt)?;
        self.take_failures()
    }

    /// One tick, leaving the coroutines' failures queued.
    fn tick_run(&mut self, dt: f64) -> Result<(), TickError> {
        if self.core.sched.in_tick || !self.core.sched.running.is_empty() {
            return Err(TickError::Reentrant);
        }
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        self.core.sched.in_tick = true;
        self.core.sched.ext.drain();
        self.core.sched.time += dt;
        self.core.sched.frame += 1;
        let mut real_now: Option<f64> = None;
        let count = self.core.sched.entries.len();
        // Coroutines whose signal was emitted before this tick resume first, in emission order.
        let queue = std::mem::take(&mut self.core.sched.signaled);
        let frame = self.core.sched.frame;
        for id in queue {
            let Some(idx) = self.core.sched.find(id) else {
                continue;
            };
            let e = &mut self.core.sched.entries[idx];
            if e.state == State::Suspended && matches!(e.wake, Wake::Signaled) {
                e.ticked = frame;
                self.resume_coro(id, false, true);
            }
        }
        for i in 0..count {
            let s = &mut self.core.sched;
            let e = &s.entries[i];
            if e.state != State::Suspended || e.ticked == s.frame {
                continue;
            }
            let due = match &e.wake {
                Wake::Signal(_) | Wake::Signaled => false,
                Wake::Group(g) => s.groups.get(g).is_none_or(|m| m.iter().all(|c| *c == e.id)),
                Wake::Ready => true,
                Wake::Time(d) => s.time + TIME_EPSILON >= *d,
                Wake::Real(d) => *real_now.get_or_insert_with(|| s.clock.now()) >= *d,
                Wake::Join(w) => w.upgrade().is_none_or(|h| handle_status(&h) != RUNNING),
                Wake::External(op) => s.ext.is_completed(*op),
            };
            if due {
                let id = e.id;
                self.resume_coro(id, false, true);
            }
        }
        self.core.sched.in_tick = false;
        self.compact();
        Ok(())
    }

    fn compact(&mut self) {
        if self.core.sched.finished > 0 {
            self.core
                .sched
                .entries
                .retain(|e| e.state != State::Finished);
            self.core.sched.finished = 0;
        }
    }

    fn take_failures(&mut self) -> Result<(), TickError> {
        let mut failures = std::mem::take(&mut self.core.sched.failures).into_iter();
        let Some((_, mut first)) = failures.next() else {
            return Ok(());
        };
        for (_, other) in failures {
            first.notes.push(format!(
                "another coroutine also panicked: {}",
                other.message
            ));
        }
        Err(TickError::Failure(first))
    }

    /// Forgets coroutine panics nobody collected (between tests).
    pub(crate) fn sched_forget_failures(&mut self) {
        self.core.sched.failures.clear();
    }

    /// Panics that no `tick` has reported yet (a coroutine that failed while starting or
    /// being cancelled), for the host to collect after the program ended.
    pub fn take_coroutine_failure(&mut self) -> Option<Box<PanicInfo>> {
        match self.take_failures() {
            Err(TickError::Failure(info)) => Some(info),
            _ => None,
        }
    }

    // ---- starting ----

    /// `go call(..)`: evaluates the call's arguments here, then runs the call on a new
    /// coroutine until it first pauses.
    pub(crate) fn eval_go(&mut self, fiber: &mut Fiber, call: &Expr, span: Span) -> Eval {
        self.eval_go_in(fiber, None, call, span)
    }

    /// `group.go(call(..))`: the group is evaluated first, then the arguments.
    pub(crate) fn eval_go_group(
        &mut self,
        fiber: &mut Fiber,
        group: &Expr,
        call: &Expr,
        span: Span,
    ) -> Eval {
        let g = self.eval_twin(fiber, group)?;
        let gid = group_id(&g);
        let result = self.eval_go_in(fiber, gid, call, span);
        self.release(g);
        result
    }

    fn eval_go_in(
        &mut self,
        fiber: &mut Fiber,
        group: Option<u64>,
        call: &Expr,
        span: Span,
    ) -> Eval {
        let ExprKind::Call(c) = &call.kind else {
            return Err(self.internal(fiber, span, "`go` without a call"));
        };
        let mut values = Vec::with_capacity(c.args.len());
        for arg in &c.args {
            match arg {
                colibri_check::hir::Arg::Value(e) => values.push(self.eval_twin(fiber, e)?),
                colibri_check::hir::Arg::Place(_) => {
                    return Err(self.internal(fiber, span, "`go` with a receiver place"));
                }
            }
        }
        let name = match &c.callee {
            Callee::Fn(id) => self.fns.get(id.0 as usize).map(|b| b.name.clone()),
            Callee::Host(id) => self
                .core
                .host
                .bodies
                .get(id.0 as usize)
                .map(|b| b.name.clone()),
            Callee::Value => match values.first() {
                Some(Value::Fn(_, name)) => Some(name.to_string()),
                _ => Some("<closure>".to_string()),
            },
            _ => None,
        }
        .unwrap_or_else(|| "<method>".to_string());
        if let Some(frame) = fiber.frames.last_mut() {
            frame.span = span;
        }
        self.start_coroutine(
            fiber,
            c.callee.clone(),
            c.type_args.clone(),
            values,
            name,
            group,
            span,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_coroutine(
        &mut self,
        fiber: &mut Fiber,
        callee: Callee,
        type_args: Vec<Ty>,
        values: Vec<Value>,
        name: String,
        group: Option<u64>,
        span: Span,
    ) -> Eval {
        let id = self.core.sched.alloc_id();
        let mut fields = vec![
            Value::Int(id as i64),
            Value::Int(RUNNING),
            Value::Unit,
            Value::Str(Rc::from(name.as_str())),
        ];
        if self.core.sched.shutting_down {
            // Nothing new may start while the program is being torn down.
            self.release_all(values);
            fields[H_STATUS] = Value::Int(CANCELLED);
            return Ok(Value::Obj(ClassObj::new(COROUTINE_TYPE, fields)));
        }
        let handle = ClassObj::new(COROUTINE_TYPE, fields);
        let size = self.core.sched.stack_size;
        let stack = match DefaultStack::new(size) {
            Ok(s) => s,
            Err(e) => {
                self.release_all(values);
                self.release(Value::Obj(handle));
                let msg = format!("cannot allocate a coroutine stack of {size} bytes: {e}");
                return Err(self.panic(fiber, span, msg));
            }
        };
        let body = move |yielder: &Y, first: Resume| -> Finish {
            // SAFETY: the pointer comes from `resume_coro`, which holds the interpreter
            // (and with it `core`) for the whole resumption and does not touch it until this
            // coroutine pauses or ends: the resumer's `&mut Interp` is not used while
            // `co.resume` runs, so this is the only live reference to the interpreter and to
            // everything in it, `core` included. The interpreter does not move while
            // coroutines live (the runtime keeps it in a `Box`). On this stack no `&mut Core`
            // is held across `yielder.suspend`: `suspend` re-derives its references from
            // `self` after it returns.
            let interp: &mut Interp = unsafe { &mut *first.interp };
            let mut fiber = Fiber::new();
            fiber.stack_base = stack_position();
            fiber.state.coro = Some(id);
            fiber.yielder = Some(yielder as *const Y);
            let result = if first.cancel {
                interp.release_all(values);
                Err(Ctl::Cancel)
            } else {
                interp.call_values(&mut fiber, &callee, &type_args, values, span)
            };
            fiber.frames.clear();
            interp.release_temps(&mut fiber, 0);
            match result {
                Ok(v) | Err(Ctl::Return(v)) => Finish::Done(v),
                Err(Ctl::Cancel) => Finish::Cancelled,
                Err(Ctl::Panic(info)) => Finish::Panicked(info),
                Err(Ctl::Break(_, v)) => {
                    interp.release(v);
                    Finish::Cancelled
                }
                Err(Ctl::Continue(_)) => Finish::Cancelled,
            }
        };
        let co: Co = Coroutine::with_stack(stack, body);
        let budget = size * STACK_USABLE_NUM / STACK_USABLE_DEN;
        self.stacks.insert(id, Box::new(co));
        self.core
            .sched
            .register(id, Rc::downgrade(&handle), group, budget);
        // The first run shares the starter's instruction budget: it is part of the starter's
        // own execution.
        self.resume_coro(id, false, false);
        if !self.core.sched.in_tick
            && self.core.sched.finished > 32
            && self.core.sched.finished * 2 > self.core.sched.entries.len()
        {
            self.compact();
        }
        Ok(Value::Obj(handle))
    }

    // ---- resuming ----

    /// Runs coroutine `id` until it pauses or ends. `fresh` gives the resumption an instruction
    /// budget of its own (the scheduler's ticks); a start shares its starter's.
    fn resume_coro(&mut self, id: u64, cancel: bool, fresh: bool) {
        let Some(idx) = self.core.sched.find(id) else {
            return;
        };
        let e = &mut self.core.sched.entries[idx];
        if e.state != State::Suspended {
            return;
        }
        let Some(mut co) = self.stacks.remove(&id) else {
            return;
        };
        let e = &mut self.core.sched.entries[idx];
        e.state = State::Running;
        let stack_budget = e.stack_budget;
        if cancel && let Wake::Signal(s) = e.wake {
            // A cancelled waiter leaves the signal's list.
            if let Some(list) = self.core.sched.signals.get_mut(&s) {
                if let Some(pos) = list.iter().rposition(|w| *w == id) {
                    list.remove(pos);
                }
                if list.is_empty() {
                    self.core.sched.signals.remove(&s);
                }
            }
        }
        self.core.sched.resumptions += 1;
        self.core.sched.running.push(id);
        let saved_stack = std::mem::replace(&mut self.native_stack_budget, stack_budget);
        let saved_budget = fresh.then(|| {
            let allotment = self.core.budget.allotment;
            std::mem::replace(&mut self.core.budget, Budget::fresh(allotment))
        });
        // From here to the `black_box` below, no reference into the interpreter (`self`, and
        // so `core`) is used: the coroutine on `co` has the only one, through `ptr`.
        let ptr: *mut Interp = self;
        let outcome = co.resume(Resume {
            interp: ptr,
            cancel,
        });
        std::hint::black_box(ptr);
        let exhausted = fresh && self.core.budget_was_exhausted();
        if let Some(b) = saved_budget {
            self.core.budget = b;
        }
        self.native_stack_budget = saved_stack;
        self.core.sched.running.pop();
        let Some(idx) = self.core.sched.find(id) else {
            // Cannot happen (nothing removes a running entry); free the stack safely.
            std::mem::forget(co);
            return;
        };
        match outcome {
            CoroutineResult::Yield(_) => {
                self.stacks.insert(id, co);
                self.core.sched.entries[idx].state = State::Suspended;
            }
            CoroutineResult::Return(finish) => {
                drop(co);
                self.core.sched.entries[idx].state = State::Finished;
                self.core.sched.finished += 1;
                self.finish_coro(idx, id, finish, exhausted);
            }
        }
    }

    fn finish_coro(&mut self, idx: usize, id: u64, finish: Finish, exhausted: bool) {
        let handle = self.core.sched.entries[idx].handle.upgrade();
        if let Some(g) = self.core.sched.entries[idx].group {
            self.leave_group(g, id);
        }
        let finish = match finish {
            Finish::Panicked(info) => Finish::Panicked(info),
            other if exhausted => {
                if let Finish::Done(v) = other {
                    self.release(v);
                }
                let mut info = self.bare_panic_info("the instruction budget is exhausted");
                info.budget = true;
                Finish::Panicked(info)
            }
            other => other,
        };
        match finish {
            Finish::Done(v) => match &handle {
                Some(h) => {
                    let _ = h.set_field(H_STATUS, Value::Int(DONE));
                    if let Ok(old) = h.set_field(H_RESULT, v) {
                        self.release(old);
                    }
                }
                None => self.release(v),
            },
            Finish::Cancelled => {
                if let Some(h) = &handle {
                    let _ = h.set_field(H_STATUS, Value::Int(CANCELLED));
                }
            }
            Finish::Panicked(info) => {
                if let Some(h) = &handle {
                    let _ = h.set_field(H_STATUS, Value::Int(PANICKED));
                    let old = h.set_field(H_RESULT, Value::Str(Rc::from(info.message.as_str())));
                    if let Ok(old) = old {
                        self.release(old);
                    }
                }
                self.core.sched.failures.push((id, info));
            }
        }
    }

    fn leave_group(&mut self, group: u64, id: u64) {
        if let Some(members) = self.core.sched.groups.get_mut(&group) {
            if let Some(pos) = members.iter().rposition(|m| *m == id) {
                members.remove(pos);
            }
            if members.is_empty() {
                self.core.sched.groups.remove(&group);
            }
        }
    }

    fn bare_panic_info(&self, message: &str) -> Box<PanicInfo> {
        Box::new(PanicInfo {
            message: message.to_string(),
            span: Span::new(colibri_syntax::FileId(0), 0, 0),
            stack: Vec::new(),
            notes: Vec::new(),
            budget: false,
        })
    }

    // ---- pausing ----

    /// Common checks of `wait` and `join`: a coroutine is running, and no host function or
    /// `Drop` hook sits on its stack.
    fn pause_context(&mut self, fiber: &mut Fiber, span: Span, what: &str) -> Result<u64, Ctl> {
        let Some(id) = fiber.state.coro else {
            let message = format!(
                "`{what}` can only pause a coroutine, but this code is not running in one (start one with `go`)"
            );
            return Err(self.panic(fiber, span, message));
        };
        if fiber.state.host_depth > 0 {
            let message = format!(
                "`{what}` cannot pause inside a host function's callback: the host's stack frame cannot be suspended"
            );
            return Err(self.panic(fiber, span, message));
        }
        if self.core.hook_depth > 0 {
            let message = format!("`{what}` cannot pause inside a `Drop` hook");
            return Err(self.panic(fiber, span, message));
        }
        Ok(id)
    }

    /// Suspends the running coroutine until `wake`; `Err(Cancel)` if it is cancelled.
    fn suspend(&mut self, fiber: &mut Fiber, id: u64, wake: Wake) -> Eval<()> {
        if self
            .core
            .sched
            .prepare_suspend(id, &mut fiber.state, wake)
            .is_err()
        {
            return Err(Ctl::Cancel);
        }
        let Some(yielder) = fiber.yielder else {
            return Err(Ctl::Cancel);
        };
        // SAFETY: the yielder lives as long as the coroutine's body, which is running us (the
        // fiber it was stored in belongs to that body and dies with it). Nothing of `core` is
        // borrowed here: `prepare_suspend` has returned.
        let yielder: &Y = unsafe { &*yielder };
        let ptr: *mut Interp = self;
        // The resumer uses the interpreter while this stack is suspended; after
        // `suspend` returns, `self` is used again and the compiler must not assume it was
        // untouched meanwhile (`black_box`).
        let input = yielder.suspend(ptr);
        std::hint::black_box(ptr);
        if input.cancel {
            fiber.state.cancelling = true;
            return Err(Ctl::Cancel);
        }
        Ok(())
    }

    /// `wait`, `wait_until`, `wait_real` and `task.tick`.
    pub(crate) fn coro_intrinsic(
        &mut self,
        fiber: &mut Fiber,
        intrinsic: &Intrinsic,
        mut args: Vec<Value>,
        span: Span,
    ) -> Eval {
        let name = match intrinsic {
            Intrinsic::Wait => "wait",
            Intrinsic::WaitUntil => "wait_until",
            Intrinsic::WaitReal => "wait_real",
            Intrinsic::WaitFor => "wait_for",
            _ => "task.tick",
        };
        if matches!(intrinsic, Intrinsic::NewCoGroup | Intrinsic::NewSignal) {
            let id = self.core.sched.alloc_id();
            let ty = if matches!(intrinsic, Intrinsic::NewCoGroup) {
                COGROUP_TYPE
            } else {
                SIGNAL_TYPE
            };
            return Ok(Value::Obj(ClassObj::new(ty, vec![Value::Int(id as i64)])));
        }
        let arg = args.pop().unwrap_or(Value::Unit);
        if matches!(intrinsic, Intrinsic::TaskTick) {
            let Value::Float(dt) = arg else {
                return Err(self.internal(fiber, span, "ill-typed tick"));
            };
            if !self.core.sched.running.is_empty() || self.core.sched.in_tick {
                let message = if fiber.state.coro.is_some() {
                    "`task.tick` cannot be called from a coroutine"
                } else {
                    "`task.tick` cannot be called from inside a tick"
                };
                return Err(self.panic(fiber, span, message));
            }
            return match self.tick_core(dt) {
                Ok(()) => Ok(Value::Unit),
                Err(TickError::Failure(info)) => Err(Ctl::Panic(info)),
                Err(TickError::Reentrant) => Err(self.panic(fiber, span, "`task.tick` is busy")),
            };
        }
        if let (Intrinsic::Wait, Value::Float(s), None) = (intrinsic, &arg, &fiber.state.coro) {
            if s.is_nan() {
                return Err(self.panic(fiber, span, "`wait` needs a number of seconds, not NaN"));
            }
            let s = s.max(0.0);
            return self.wait_on_main(fiber, s, span).map(|()| Value::Unit);
        }
        let id = match self.pause_context(fiber, span, name) {
            Ok(id) => id,
            Err(ctl) => {
                self.release(arg);
                return Err(ctl);
            }
        };
        match (intrinsic, arg) {
            (Intrinsic::Wait, Value::Float(s)) | (Intrinsic::WaitReal, Value::Float(s)) => {
                if s.is_nan() {
                    return Err(self.panic(
                        fiber,
                        span,
                        format!("`{name}` needs a number of seconds, not NaN"),
                    ));
                }
                let s = s.max(0.0);
                let wake = if matches!(intrinsic, Intrinsic::Wait) {
                    Wake::Time(self.core.sched.time + s)
                } else {
                    Wake::Real(self.core.sched.clock.now() + s)
                };
                self.suspend(fiber, id, wake)?;
                Ok(Value::Unit)
            }
            (Intrinsic::WaitFor, s) => {
                let result = match signal_id(&s) {
                    // `s` stays alive while this coroutine waits on it.
                    Some(sid) => self.suspend(fiber, id, Wake::Signal(sid)),
                    None => Err(self.internal(fiber, span, "ill-typed wait_for")),
                };
                self.release(s);
                result.map(|()| Value::Unit)
            }
            (Intrinsic::WaitUntil, f) => {
                let result = self.wait_until(fiber, id, &f, span);
                self.release(f);
                result.map(|()| Value::Unit)
            }
            (_, other) => {
                self.release(other);
                Err(self.internal(fiber, span, "ill-typed wait"))
            }
        }
    }

    fn wait_until(&mut self, fiber: &mut Fiber, id: u64, f: &Value, span: Span) -> Eval<()> {
        loop {
            if fiber.state.cancelling {
                return Err(Ctl::Cancel);
            }
            if let Some(frame) = fiber.frames.last_mut() {
                frame.span = span;
            }
            match self.call_function_value(fiber, f, Vec::new(), span)? {
                Value::Bool(true) => return Ok(()),
                _ => self.suspend(fiber, id, Wake::Ready)?,
            }
        }
    }

    // ---- driving the scheduler from the main fiber ----

    /// May the main fiber run the event loop now? `depth` is how many host functions may be
    /// active (0 for a Colibri-level wait, 1 for a host function waiting on its own thread).
    fn can_drive(&self, fiber: &Fiber, depth: u32) -> bool {
        fiber.state.coro.is_none()
            && fiber.state.host_depth <= depth
            && self.core.hook_depth == 0
            && !self.core.sched.in_tick
            && self.core.sched.running.is_empty()
            && !self.core.sched.shutting_down
    }

    /// Whether an external operation that `fiber` waits for lets the rest of the program run
    /// meanwhile: a coroutine outside a `Drop` hook and a callback is suspended, and the main
    /// fiber drives the event loop where it may. When this is `false` the wait blocks the
    /// interpreter thread, and nothing that another coroutine still has to do can happen until
    /// the operation is done (see [`Interp::await_external_with`]).
    pub fn external_wait_runs_others(&self, fiber: &Fiber) -> bool {
        self.can_suspend_for_external(fiber) || self.can_drive(fiber, 1)
    }

    fn can_suspend_for_external(&self, fiber: &Fiber) -> bool {
        fiber.state.coro.is_some() && fiber.state.host_depth <= 1 && self.core.hook_depth == 0
    }

    /// The checks of a wait on the main fiber, as panics with a message that says why.
    fn check_drive(&mut self, fiber: &mut Fiber, span: Span, what: &str) -> Eval<()> {
        let why = if fiber.state.host_depth > 0 {
            "inside a host function's callback"
        } else if self.core.hook_depth > 0 {
            "inside a `Drop` hook"
        } else if self.core.sched.in_tick || !self.core.sched.running.is_empty() {
            "inside a tick"
        } else if self.core.sched.shutting_down {
            "while the program is shutting down"
        } else {
            return Ok(());
        };
        let message = format!(
            "`{what}` on the main fiber runs the event loop, which cannot be done {why} (ARCHITECTURE section 22)"
        );
        Err(self.panic(fiber, span, message))
    }

    /// Runs the event loop on the main fiber, as the loop after `main` does (ticks with the
    /// time elapsed on the clock, sleeping until the next wake or a completion), until `done`
    /// holds or the clock reaches `deadline`. With `report`, a coroutine's failure ends the
    /// wait with that panic unless `done` holds. Always ticks once first.
    fn drive(
        &mut self,
        deadline: Option<f64>,
        report: bool,
        done: &mut dyn FnMut(&mut Interp) -> bool,
    ) -> Drive {
        let mut last = self.core.sched.clock.now();
        loop {
            let now = self.core.sched.clock.now();
            let dt = (now - last).max(0.0);
            last = now;
            if self.tick_run(dt).is_err() {
                return Drive::Stuck;
            }
            if done(self) {
                return Drive::Done;
            }
            if report && let Err(TickError::Failure(info)) = self.take_failures() {
                return Drive::Failure(info);
            }
            let now = self.core.sched.clock.now();
            let mut delay = match (self.next_wake(0.001), deadline) {
                (Some(d), _) => d,
                (None, Some(_)) => f64::INFINITY,
                (None, None) => return Drive::Stuck,
            };
            if let Some(d) = deadline {
                if now + TIME_EPSILON >= d {
                    return Drive::Done;
                }
                delay = delay.min(d - now);
            }
            self.clock_sleep(delay);
        }
    }

    fn drive_main(
        &mut self,
        fiber: &mut Fiber,
        span: Span,
        what: &str,
        deadline: Option<f64>,
        done: &mut dyn FnMut(&mut Interp) -> bool,
    ) -> Eval<()> {
        self.check_drive(fiber, span, what)?;
        match self.drive(deadline, true, done) {
            Drive::Done => Ok(()),
            Drive::Failure(info) => Err(Ctl::Panic(info)),
            Drive::Stuck => {
                let message = format!(
                    "`{what}` on the main fiber would wait forever: no coroutine is left that a clock or a completion could wake"
                );
                Err(self.panic(fiber, span, message))
            }
        }
    }

    /// `wait(seconds)` on the main fiber: runs the event loop for that long.
    fn wait_on_main(&mut self, fiber: &mut Fiber, seconds: f64, span: Span) -> Eval<()> {
        let deadline = self.core.sched.clock.now() + seconds;
        self.drive_main(fiber, span, "wait", Some(deadline), &mut |_| false)
    }

    // ---- background operations ----

    /// Runs `work` on a background thread (an idle one of the pool, or a new one: an
    /// operation never waits behind another) and waits for its result without blocking other
    /// coroutines: the calling coroutine is suspended with `Wake::External`, and the event
    /// loop (or the host's `tick`) resumes it once the thread has finished. On the main
    /// fiber the wait runs the event loop instead, so coroutines keep running. Where neither
    /// is possible (inside a host callback or a `Drop` hook) it just blocks. Meant to be
    /// called from a host function (host depth 1); it must not hold a borrow of host objects
    /// across the call, because other coroutines run meanwhile. `Err(Cancelled)` means the
    /// coroutine was cancelled while waiting: the host function must unwind (its result would
    /// be discarded; an operation whose thread has not started it yet is dropped).
    pub fn await_external<R: Send + 'static>(
        &mut self,
        fiber: &mut Fiber,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> Result<R, ExternalError> {
        self.await_external_with(fiber, move |_| work())
    }

    /// [`Interp::await_external`] for work that can stop early: `work` is given a
    /// [`CancelToken`] that is cancelled when the waiting coroutine is cancelled or the `Vm`
    /// ends. An operation that holds a child process registers a hook that kills it; the hook
    /// runs on the cancelling side before it goes on, so nothing is left running.
    pub fn await_external_with<R: Send + 'static>(
        &mut self,
        fiber: &mut Fiber,
        work: impl FnOnce(&CancelToken) -> R + Send + 'static,
    ) -> Result<R, ExternalError> {
        let (completer, result_rx) = self.begin_completion::<R>();
        let op = completer.op();
        let token = Arc::clone(completer.token());
        let queued = self.core.sched.ext.submit(Box::new(move || {
            // Set when the waiter gives up, so an operation still in the queue is not run.
            if completer.token().is_cancelled() {
                return;
            }
            let token = Arc::clone(completer.token());
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&token)));
            match outcome {
                Ok(value) => completer.complete(value),
                Err(payload) => completer.fail(panic_message(payload.as_ref())),
            }
        }));
        if let Err(message) = queued {
            self.core.sched.ext.forget(op);
            return Err(ExternalError::CannotStart(message));
        }
        self.wait_completion(fiber, op, &token, result_rx)
    }

    /// Waits for an operation that some other thread completes. `start` runs at once on the
    /// calling thread and is given the [`Completer`] to hand to whoever will finish the
    /// operation; an `Err` from it (the reason it could not begin) is
    /// [`ExternalError::CannotStart`] and nothing waits. Then the call waits exactly as
    /// [`Interp::await_external_with`] does: the coroutine suspends with `Wake::External` and
    /// the event loop (or the host's `tick`) resumes it after `complete`; the main fiber runs
    /// the event loop instead; where neither is possible the thread blocks. The same rules
    /// hold: the host function must not hold a borrow of host objects across the call, and
    /// `Err(Cancelled)` means the host function must unwind, the token's hooks having run
    /// already (a hook that panics there is a panic of the host function, and the operation's
    /// entries are gone by then). A completer dropped without a result is `Err(Failed)`.
    #[doc(hidden)]
    pub fn await_completion<R: Send + 'static>(
        &mut self,
        fiber: &mut Fiber,
        start: impl FnOnce(Completer<R>) -> Result<(), String>,
    ) -> Result<R, ExternalError> {
        let (completer, result_rx) = self.begin_completion::<R>();
        let op = completer.op();
        let token = Arc::clone(completer.token());
        // A `start` that fails or panics leaves nothing of the operation behind: its entries in
        // the scheduler go, and the hooks it registered on the token run.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| start(completer))) {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                self.core.sched.ext.forget(op);
                token.cancel();
                return Err(ExternalError::CannotStart(message));
            }
            Err(payload) => {
                self.core.sched.ext.forget(op);
                // The dropped completer has posted its id; collecting it later finds the
                // operation no longer pending and ignores it. A hook that panics too does not
                // replace the panic of `start`, which is the one that is passed on.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| token.cancel()));
                std::panic::resume_unwind(payload);
            }
        }
        self.wait_completion(fiber, op, &token, result_rx)
    }

    /// Registers a new operation with the scheduler and makes the end that completes it.
    fn begin_completion<R>(&mut self) -> (Completer<R>, mpsc::Receiver<Result<R, String>>) {
        let op = self.core.sched.alloc_id();
        self.core.sched.ext.begin(op)
    }

    /// Runs the cancel hooks of an operation whose waiter is giving up. A hook that panics must
    /// not leave the operation's entries behind: they are removed, and the panic is passed on
    /// (so it becomes a panic of the host function that is waiting).
    fn cancel_completion(&mut self, op: u64, token: &CancelToken) {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| token.cancel()))
        {
            self.core.sched.ext.finish(op);
            std::panic::resume_unwind(payload);
        }
    }

    /// The one wait loop of every background operation (suspend, drive the loop, or block).
    fn wait_completion<R>(
        &mut self,
        fiber: &mut Fiber,
        op: u64,
        token: &CancelToken,
        result_rx: mpsc::Receiver<Result<R, String>>,
    ) -> Result<R, ExternalError> {
        let coro = fiber.state.coro;
        let waited = if let (Some(id), true) = (coro, self.can_suspend_for_external(fiber)) {
            let mut outcome = Ok(());
            while !self.core.sched.ext.is_completed(op) {
                if self.suspend(fiber, id, Wake::External(op)).is_err() {
                    self.cancel_completion(op, token);
                    outcome = Err(ExternalError::Cancelled);
                    break;
                }
            }
            outcome
        } else if self.can_drive(fiber, 1) {
            self.drive(None, false, &mut |me| me.core.sched.ext.is_completed(op));
            Ok(())
        } else {
            // Nothing can run meanwhile: block this thread until the result is there. Every
            // end of a completer sends or drops the result's sender, so this does not hang.
            let blocked = result_rx.recv();
            self.core.sched.ext.forget(op);
            return unwrap_outcome(blocked.map_err(|_| ()));
        };
        self.core.sched.ext.finish(op);
        waited?;
        unwrap_outcome(result_rx.try_recv().map_err(|_| ()))
    }

    // ---- handle methods ----

    pub(crate) fn coroutine_builtin(
        &mut self,
        fiber: &mut Fiber,
        op: colibri_check::hir::Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval {
        use colibri_check::hir::Builtin;
        let Some(h) = handle_of(recv) else {
            return Err(self.internal(fiber, span, "ill-typed coroutine method"));
        };
        let h = Rc::clone(h);
        let result = match op {
            Builtin::CoroutineDone => Ok(Value::Bool(handle_status(&h) != RUNNING)),
            Builtin::CoroutineCancel => self.cancel_coroutine(fiber, &h).map(|()| Value::Unit),
            _ => self.join_coroutine(fiber, &h, span),
        };
        // `h` is a clone of a handle the caller still holds, so this is never the last owner.
        drop(h);
        result
    }

    fn cancel_coroutine(&mut self, fiber: &mut Fiber, h: &ClassObj) -> Eval<()> {
        if handle_status(h) != RUNNING {
            return Ok(());
        }
        let id = handle_id(h);
        if fiber.state.coro == Some(id) {
            // A coroutine cancelling itself stops at once.
            fiber.state.cancelling = true;
            return Err(Ctl::Cancel);
        }
        self.cancel_id(id);
        Ok(())
    }

    /// Cancels the coroutine `id` (not the running one): a suspended coroutine unwinds at
    /// once, one that is mid-run higher up the resume chain stops at its next pause.
    fn cancel_id(&mut self, id: u64) {
        let Some(idx) = self.core.sched.find(id) else {
            return;
        };
        match self.core.sched.entries[idx].state {
            State::Finished => {}
            State::Running => self.core.sched.entries[idx].cancel_requested = true,
            State::Suspended => self.resume_coro(id, true, false),
        }
    }

    /// The last reference to a `CoGroup` died: its coroutines are cancelled, newest first.
    pub(crate) fn group_died(&mut self, obj: &ClassObj) {
        let Some(Value::Int(g)) = obj.field(0) else {
            return;
        };
        if let Some(members) = self.core.sched.groups.remove(&(g as u64)) {
            for id in members.into_iter().rev() {
                self.cancel_id(id);
            }
        }
    }

    pub(crate) fn cogroup_builtin(
        &mut self,
        fiber: &mut Fiber,
        op: colibri_check::hir::Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval {
        use colibri_check::hir::Builtin;
        let Some(g) = group_id(recv) else {
            return Err(self.internal(fiber, span, "ill-typed CoGroup method"));
        };
        let me = fiber.state.coro;
        match op {
            Builtin::CoGroupCount => {
                let n = self.core.sched.groups.get(&g).map_or(0, Vec::len);
                Ok(Value::Int(n as i64))
            }
            Builtin::CoGroupCancel => {
                let members = self.core.sched.groups.get(&g).cloned().unwrap_or_default();
                let mut cancel_self = false;
                for id in members.into_iter().rev() {
                    if Some(id) == me {
                        cancel_self = true;
                    } else {
                        self.cancel_id(id);
                    }
                }
                if cancel_self {
                    // A member cancelling its own group stops last, at once.
                    fiber.state.cancelling = true;
                    return Err(Ctl::Cancel);
                }
                Ok(Value::Unit)
            }
            _ => {
                let others = |s: &Scheduler| {
                    s.groups
                        .get(&g)
                        .is_some_and(|m| m.iter().any(|c| Some(*c) != me))
                };
                if !others(&self.core.sched) {
                    return Ok(Value::Unit);
                }
                if me.is_none() {
                    self.drive_main(fiber, span, "join_all", None, &mut |it| {
                        !others(&it.core.sched)
                    })?;
                    return Ok(Value::Unit);
                }
                let id = self.pause_context(fiber, span, "join_all")?;
                while others(&self.core.sched) {
                    self.suspend(fiber, id, Wake::Group(g))?;
                }
                Ok(Value::Unit)
            }
        }
    }

    pub(crate) fn signal_builtin(&mut self, fiber: &mut Fiber, recv: &Value, span: Span) -> Eval {
        let Some(sid) = signal_id(recv) else {
            return Err(self.internal(fiber, span, "ill-typed Signal method"));
        };
        if let Some(waiters) = self.core.sched.signals.remove(&sid) {
            for id in waiters {
                if let Some(idx) = self.core.sched.find(id) {
                    let e = &mut self.core.sched.entries[idx];
                    if e.state == State::Suspended && matches!(e.wake, Wake::Signal(s) if s == sid)
                    {
                        e.wake = Wake::Signaled;
                        self.core.sched.signaled.push(id);
                    }
                }
            }
        }
        Ok(Value::Unit)
    }

    fn join_coroutine(&mut self, fiber: &mut Fiber, h: &Rc<ClassObj>, span: Span) -> Eval {
        if handle_status(h) == RUNNING && fiber.state.coro.is_none() {
            let target = Rc::clone(h);
            self.drive_main(fiber, span, "join", None, &mut |_| {
                handle_status(&target) != RUNNING
            })?;
        }
        if handle_status(h) == RUNNING {
            let id = self.pause_context(fiber, span, "join")?;
            if handle_id(h) == id {
                return Err(self.panic(fiber, span, "a coroutine cannot join itself"));
            }
            while handle_status(h) == RUNNING {
                self.suspend(fiber, id, Wake::Join(Rc::downgrade(h)))?;
            }
        }
        match handle_status(h) {
            DONE => Ok(h.field(H_RESULT).unwrap_or(Value::Unit)),
            PANICKED => {
                let id = handle_id(h);
                self.core.sched.failures.retain(|(f, _)| *f != id);
                let msg = match h.field(H_RESULT) {
                    Some(Value::Str(s)) => s.to_string(),
                    _ => String::new(),
                };
                let message = format!("the joined coroutine `{}` panicked: {msg}", handle_name(h));
                Err(self.panic(fiber, span, message))
            }
            _ => {
                let message = format!("the joined coroutine `{}` was cancelled", handle_name(h));
                Err(self.panic(fiber, span, message))
            }
        }
    }

    // ---- shutdown ----

    /// Cancels every live coroutine, newest first, so their `defer`s and drops run before the
    /// module-level values die. Nothing can start meanwhile.
    pub fn cancel_all_coroutines(&mut self) {
        if self.core.sched.entries.is_empty() || !self.core.sched.running.is_empty() {
            return;
        }
        let outermost = self.core.begin_entry();
        self.core.sched.shutting_down = true;
        loop {
            let next = self
                .core
                .sched
                .entries
                .iter()
                .rev()
                .find(|e| e.state == State::Suspended)
                .map(|e| e.id);
            let Some(id) = next else { break };
            self.resume_coro(id, true, true);
        }
        self.core.sched.shutting_down = false;
        for (_, co) in self.stacks.drain() {
            // Unreachable in practice (every suspended coroutine was resumed with a cancel and
            // ran to its end, which frees its stack); a forced unwind must never happen.
            std::mem::forget(co);
        }
        self.core.sched.entries.clear();
        self.core.sched.finished = 0;
        self.core.sched.signals.clear();
        self.core.sched.signaled.clear();
        self.core.sched.groups.clear();
        self.core.sched.ext.clear_completed();
        self.core.end_entry(outermost, false);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A `start` that panics or fails leaves nothing of the operation in the scheduler, and the
    /// hook it had registered has run; a panic is passed on.
    #[test]
    fn a_start_that_panics_or_fails_leaves_nothing_behind() {
        use crate::{Fiber, Interp};
        let mut interp = Interp::new(Box::new(std::io::sink()), 100);
        let mut fiber = Fiber::new();
        let hooks = Arc::new(AtomicUsize::new(0));
        for panics in [true, false] {
            let hooks_here = Arc::clone(&hooks);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                interp.await_completion::<u8>(&mut fiber, |completer| {
                    let hooks = Arc::clone(&hooks_here);
                    completer.token().on_cancel(move || {
                        hooks.fetch_add(1, Ordering::SeqCst);
                    });
                    if panics {
                        panic!("start panics");
                    }
                    Err("cannot start".to_string())
                })
            }));
            assert_eq!(outcome.is_err(), panics);
            if !panics {
                assert!(matches!(
                    outcome.unwrap(),
                    Err(crate::ExternalError::CannotStart(_))
                ));
            }
            // The id that the dropped completer posted is ignored when it is collected.
            interp.core.sched.ext.drain();
            assert!(interp.core.sched.ext.is_idle());
        }
        assert_eq!(hooks.load(Ordering::SeqCst), 2, "the hooks ran");
    }

    /// A cancel hook that panics, when the waiter gives up, leaves nothing of the operation in
    /// the scheduler (not even the completion that had already arrived), and the panic is passed
    /// on.
    #[test]
    fn a_cancel_hook_that_panics_still_forgets_the_operation() {
        use crate::Interp;
        let mut interp = Interp::new(Box::new(std::io::sink()), 100);
        let (completer, _result_rx) = interp.begin_completion::<u8>();
        let op = completer.op();
        let token = Arc::clone(completer.token());
        token.on_cancel(|| panic!("the hook panics"));
        // The completion arrived before the waiter noticed the cancellation.
        drop(completer);
        interp.core.sched.ext.drain();
        assert!(interp.core.sched.ext.is_completed(op));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            interp.cancel_completion(op, &token)
        }));
        assert!(outcome.is_err(), "the panic is passed on");
        assert!(interp.core.sched.ext.is_idle());
    }
}
