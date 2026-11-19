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

use std::rc::{Rc, Weak};
use std::time::Instant;

use colibri_check::hir::{Callee, Expr, ExprKind, Intrinsic};
use colibri_check::types::{COROUTINE_TYPE, Ty};
use colibri_syntax::Span;
use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};

use crate::budget::Budget;
use crate::ctl::{Ctl, Eval, PanicInfo};
use crate::eval::{Interp, stack_position};
use crate::fiber::Fiber;
use crate::heap::ClassObj;
use crate::value::Value;

/// The default size of the native stack of one coroutine.
pub const DEFAULT_COROUTINE_STACK: usize = 1024 * 1024;

/// How much of a coroutine's stack the evaluator may use before it panics with "stack
/// overflow": the rest is for host functions and built-ins, which are not measured.
const STACK_USABLE_NUM: usize = 3;
const STACK_USABLE_DEN: usize = 4;

/// Slack when comparing times, so `wait(0.1)` five times in a row lands on the same tick as
/// `wait(0.5)` despite rounding.
const TIME_EPSILON: f64 = 1e-9;

/// Where scheduler time comes from, for `wait_real` and for the event loop (`colibri run`).
pub trait Clock {
    /// Seconds since an arbitrary, fixed origin.
    fn now(&mut self) -> f64;
    /// Waits for `seconds` (a fake clock just moves forward).
    fn sleep(&mut self, seconds: f64);
}

/// The wall clock.
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&mut self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }
    fn sleep(&mut self, seconds: f64) {
        if seconds > 0.0 {
            std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
        }
    }
}

/// What a resumption tells the coroutine.
pub(crate) struct Resume {
    pub interp: *mut Interp,
    pub cancel: bool,
}

type Y = Yielder<Resume, *mut Interp>;
type Co = Coroutine<Resume, *mut Interp, Finish, DefaultStack>;

/// How a coroutine ended.
pub(crate) enum Finish {
    Done(Value),
    Panicked(Box<PanicInfo>),
    Cancelled,
}

/// What a fiber knows about the coroutine it runs.
#[derive(Debug)]
pub(crate) struct CoroCtx {
    pub id: u64,
    yielder: *const Y,
}

/// When a suspended coroutine is due.
enum Wake {
    /// At every tick (`wait_until` polls its condition on the coroutine itself).
    Ready,
    /// When scheduler time reaches this.
    Time(f64),
    /// When the tick counter reaches this.
    Frame(u64),
    /// When the real clock reaches this.
    Real(f64),
    /// When this coroutine handle is finished.
    Join(Weak<ClassObj>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Suspended,
    Running,
    Finished,
}

struct Entry {
    id: u64,
    state: State,
    wake: Wake,
    co: Option<Box<Co>>,
    handle: Weak<ClassObj>,
    cancel_requested: bool,
    stack_budget: usize,
}

/// The per-`Interp` coroutine scheduler.
pub(crate) struct Scheduler {
    entries: Vec<Entry>,
    next_id: u64,
    /// Scheduler time: the sum of the `dt` of the ticks so far.
    time: f64,
    /// How many ticks have run.
    frame: u64,
    in_tick: bool,
    /// The coroutines whose stacks are active, outermost first.
    running: Vec<u64>,
    failures: Vec<(u64, Box<PanicInfo>)>,
    finished: usize,
    shutting_down: bool,
    pub stack_size: usize,
    clock: Box<dyn Clock>,
}

impl Scheduler {
    pub fn new() -> Self {
        Scheduler {
            entries: Vec::new(),
            next_id: 1,
            time: 0.0,
            frame: 0,
            in_tick: false,
            running: Vec::new(),
            failures: Vec::new(),
            finished: 0,
            shutting_down: false,
            stack_size: DEFAULT_COROUTINE_STACK,
            clock: Box::new(SystemClock::new()),
        }
    }

    fn find(&self, id: u64) -> Option<usize> {
        self.entries.binary_search_by_key(&id, |e| e.id).ok()
    }
}

/// Handle layout: `[id, status, result, name]`.
const H_ID: usize = 0;
const H_STATUS: usize = 1;
const H_RESULT: usize = 2;
const H_NAME: usize = 3;
const RUNNING: i64 = 0;
const DONE: i64 = 1;
const PANICKED: i64 = 2;
const CANCELLED: i64 = 3;

fn handle_status(h: &ClassObj) -> i64 {
    match h.field(H_STATUS) {
        Some(Value::Int(s)) => s,
        _ => CANCELLED,
    }
}

fn handle_id(h: &ClassObj) -> u64 {
    match h.field(H_ID) {
        Some(Value::Int(i)) => i as u64,
        _ => 0,
    }
}

fn handle_name(h: &ClassObj) -> String {
    match h.field(H_NAME) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "?".to_string(),
    }
}

fn handle_of(v: &Value) -> Option<&Rc<ClassObj>> {
    match v {
        Value::Obj(o) if o.ty() == COROUTINE_TYPE => Some(o),
        _ => None,
    }
}

/// Why a tick did not happen or did not end well.
pub enum TickError {
    /// `tick` was called from a coroutine, or from inside a tick.
    Reentrant,
    /// A coroutine panicked (the first failure, the others as notes).
    Failure(Box<PanicInfo>),
}

impl Interp {
    // ---- configuration and inspection ----

    /// The size of the native stack given to each coroutine (default 1 MiB).
    pub fn set_coroutine_stack_size(&mut self, bytes: usize) {
        self.sched.stack_size = bytes.max(16 * 1024);
    }

    /// Replaces the clock behind `wait_real` and the event loop.
    pub fn set_clock(&mut self, clock: Box<dyn Clock>) {
        self.sched.clock = clock;
    }

    /// The reading of the clock, in seconds.
    pub fn clock_now(&mut self) -> f64 {
        self.sched.clock.now()
    }

    /// Sleeps on the clock.
    pub fn clock_sleep(&mut self, seconds: f64) {
        self.sched.clock.sleep(seconds);
    }

    /// How many coroutines are alive (started and not yet finished).
    pub fn coroutine_count(&self) -> usize {
        self.sched
            .entries
            .iter()
            .filter(|e| e.state != State::Finished)
            .count()
    }

    /// How long until the next coroutine needs a tick, for an event loop: `None` when nothing
    /// is alive, `Some(0.0)`-ish small polling interval when something is checked at every tick
    /// (`wait_until`, `wait_frames`, `join`), else the time to the earliest timed wake.
    pub fn next_wake(&mut self, poll: f64) -> Option<f64> {
        let mut best: Option<f64> = None;
        let mut real_now: Option<f64> = None;
        let time = self.sched.time;
        let mut any = false;
        for e in &self.sched.entries {
            if e.state == State::Finished {
                continue;
            }
            any = true;
            let delay = match &e.wake {
                Wake::Time(d) => (d - time).max(0.0),
                Wake::Real(d) => {
                    let now = *real_now.get_or_insert_with(|| self.sched.clock.now());
                    (d - now).max(0.0)
                }
                Wake::Ready | Wake::Frame(_) | Wake::Join(_) => poll,
            };
            best = Some(best.map_or(delay, |b| b.min(delay)));
        }
        if any { best } else { None }
    }

    // ---- ticking ----

    /// Resumes every due coroutine once, in start order. A coroutine started during the tick
    /// is not resumed in it. Each resumption gets its own instruction budget.
    pub fn tick(&mut self, fiber: &mut Fiber, dt: f64) -> Result<(), Box<PanicInfo>> {
        let _ = fiber;
        let outermost = self.begin_entry();
        if outermost {
            self.drain_root_drops();
        }
        let result = self.tick_core(dt);
        self.end_entry(outermost, false);
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
        if self.sched.in_tick || !self.sched.running.is_empty() {
            return Err(TickError::Reentrant);
        }
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        self.sched.in_tick = true;
        self.sched.time += dt;
        self.sched.frame += 1;
        let mut real_now: Option<f64> = None;
        let count = self.sched.entries.len();
        for i in 0..count {
            let s = &mut self.sched;
            let e = &s.entries[i];
            if e.state != State::Suspended {
                continue;
            }
            let due = match &e.wake {
                Wake::Ready => true,
                Wake::Time(d) => s.time + TIME_EPSILON >= *d,
                Wake::Frame(f) => s.frame >= *f,
                Wake::Real(d) => *real_now.get_or_insert_with(|| s.clock.now()) >= *d,
                Wake::Join(w) => w.upgrade().is_none_or(|h| handle_status(&h) != RUNNING),
            };
            if due {
                let id = e.id;
                self.resume_coro(id, false, true);
            }
        }
        self.sched.in_tick = false;
        self.compact();
        self.take_failures()
    }

    fn compact(&mut self) {
        if self.sched.finished > 0 {
            self.sched.entries.retain(|e| e.state != State::Finished);
            self.sched.finished = 0;
        }
    }

    fn take_failures(&mut self) -> Result<(), TickError> {
        let mut failures = std::mem::take(&mut self.sched.failures).into_iter();
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
        self.sched.failures.clear();
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
            Callee::Host(id) => self.host.bodies.get(id.0 as usize).map(|b| b.name.clone()),
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
            span,
        )
    }

    fn start_coroutine(
        &mut self,
        fiber: &mut Fiber,
        callee: Callee,
        type_args: Vec<Ty>,
        values: Vec<Value>,
        name: String,
        span: Span,
    ) -> Eval {
        let id = self.sched.next_id;
        self.sched.next_id += 1;
        let mut fields = vec![
            Value::Int(id as i64),
            Value::Int(RUNNING),
            Value::Unit,
            Value::Str(Rc::from(name.as_str())),
        ];
        if self.sched.shutting_down {
            // Nothing new may start while the program is being torn down.
            self.release_all(values);
            fields[H_STATUS] = Value::Int(CANCELLED);
            return Ok(Value::Obj(ClassObj::new(COROUTINE_TYPE, fields)));
        }
        let handle = ClassObj::new(COROUTINE_TYPE, fields);
        let size = self.sched.stack_size;
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
            // for the whole resumption; the interpreter does not move while coroutines live.
            let interp: &mut Interp = unsafe { &mut *first.interp };
            let mut fiber = Fiber::new();
            fiber.stack_base = stack_position();
            fiber.coro = Some(CoroCtx {
                id,
                yielder: yielder as *const Y,
            });
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
        self.sched.entries.push(Entry {
            id,
            state: State::Suspended,
            wake: Wake::Ready,
            co: Some(Box::new(co)),
            handle: Rc::downgrade(&handle),
            cancel_requested: false,
            stack_budget: budget,
        });
        // The first run shares the starter's instruction budget: it is part of the starter's
        // own execution.
        self.resume_coro(id, false, false);
        if !self.sched.in_tick
            && self.sched.finished > 32
            && self.sched.finished * 2 > self.sched.entries.len()
        {
            self.compact();
        }
        Ok(Value::Obj(handle))
    }

    // ---- resuming ----

    /// Runs coroutine `id` until it pauses or ends. `fresh` gives the resumption an instruction
    /// budget of its own (the scheduler's ticks); a start shares its starter's.
    fn resume_coro(&mut self, id: u64, cancel: bool, fresh: bool) {
        let Some(idx) = self.sched.find(id) else {
            return;
        };
        let e = &mut self.sched.entries[idx];
        if e.state != State::Suspended {
            return;
        }
        let Some(mut co) = e.co.take() else {
            return;
        };
        e.state = State::Running;
        let stack_budget = e.stack_budget;
        self.sched.running.push(id);
        let saved_stack = std::mem::replace(&mut self.native_stack_budget, stack_budget);
        let saved_budget = fresh.then(|| {
            let allotment = self.budget.allotment;
            std::mem::replace(&mut self.budget, Budget::fresh(allotment))
        });
        let ptr: *mut Interp = self;
        let outcome = co.resume(Resume {
            interp: ptr,
            cancel,
        });
        std::hint::black_box(ptr);
        let exhausted = fresh && self.budget_was_exhausted();
        if let Some(b) = saved_budget {
            self.budget = b;
        }
        self.native_stack_budget = saved_stack;
        self.sched.running.pop();
        let Some(idx) = self.sched.find(id) else {
            // Cannot happen (nothing removes a running entry); free the stack safely.
            std::mem::forget(co);
            return;
        };
        match outcome {
            CoroutineResult::Yield(_) => {
                let e = &mut self.sched.entries[idx];
                e.co = Some(co);
                e.state = State::Suspended;
            }
            CoroutineResult::Return(finish) => {
                drop(co);
                self.sched.entries[idx].state = State::Finished;
                self.sched.finished += 1;
                self.finish_coro(idx, id, finish, exhausted);
            }
        }
    }

    fn finish_coro(&mut self, idx: usize, id: u64, finish: Finish, exhausted: bool) {
        let handle = self.sched.entries[idx].handle.upgrade();
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
                self.sched.failures.push((id, info));
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
        let Some(ctx) = &fiber.coro else {
            let message = format!(
                "`{what}` can only pause a coroutine, but this code is not running in one (start one with `go`)"
            );
            return Err(self.panic(fiber, span, message));
        };
        let id = ctx.id;
        if fiber.host_depth > 0 {
            let message = format!(
                "`{what}` cannot pause inside a host function's callback: the host's stack frame cannot be suspended"
            );
            return Err(self.panic(fiber, span, message));
        }
        if self.hook_depth > 0 {
            let message = format!("`{what}` cannot pause inside a `Drop` hook");
            return Err(self.panic(fiber, span, message));
        }
        Ok(id)
    }

    /// Suspends the running coroutine until `wake`; `Err(Cancel)` if it is cancelled.
    fn suspend(&mut self, fiber: &mut Fiber, id: u64, wake: Wake) -> Eval<()> {
        let Some(idx) = self.sched.find(id) else {
            return Err(Ctl::Cancel);
        };
        let e = &mut self.sched.entries[idx];
        if fiber.cancelling || e.cancel_requested {
            fiber.cancelling = true;
            return Err(Ctl::Cancel);
        }
        e.wake = wake;
        let Some(ctx) = &fiber.coro else {
            return Err(Ctl::Cancel);
        };
        // SAFETY: the yielder lives as long as the coroutine's body, which is running us.
        let yielder: &Y = unsafe { &*ctx.yielder };
        let ptr: *mut Interp = self;
        let input = yielder.suspend(ptr);
        std::hint::black_box(ptr);
        if input.cancel {
            fiber.cancelling = true;
            return Err(Ctl::Cancel);
        }
        Ok(())
    }

    /// `wait`, `wait_frames`, `wait_until`, `wait_real` and `task.tick`.
    pub(crate) fn coro_intrinsic(
        &mut self,
        fiber: &mut Fiber,
        intrinsic: &Intrinsic,
        mut args: Vec<Value>,
        span: Span,
    ) -> Eval {
        let name = match intrinsic {
            Intrinsic::Wait => "wait",
            Intrinsic::WaitFrames => "wait_frames",
            Intrinsic::WaitUntil => "wait_until",
            Intrinsic::WaitReal => "wait_real",
            _ => "task.tick",
        };
        let arg = args.pop().unwrap_or(Value::Unit);
        if matches!(intrinsic, Intrinsic::TaskTick) {
            let Value::Float(dt) = arg else {
                return Err(self.internal(fiber, span, "ill-typed tick"));
            };
            if !self.sched.running.is_empty() || self.sched.in_tick {
                let message = if fiber.coro.is_some() {
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
                    Wake::Time(self.sched.time + s)
                } else {
                    Wake::Real(self.sched.clock.now() + s)
                };
                self.suspend(fiber, id, wake)?;
                Ok(Value::Unit)
            }
            (Intrinsic::WaitFrames, Value::Int(n)) => {
                let n = n.max(1) as u64;
                let wake = Wake::Frame(self.sched.frame.saturating_add(n));
                self.suspend(fiber, id, wake)?;
                Ok(Value::Unit)
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
            if fiber.cancelling {
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
        if fiber.coro.as_ref().is_some_and(|c| c.id == id) {
            // A coroutine cancelling itself stops at once.
            fiber.cancelling = true;
            return Err(Ctl::Cancel);
        }
        let Some(idx) = self.sched.find(id) else {
            return Ok(());
        };
        match self.sched.entries[idx].state {
            State::Finished => {}
            State::Running => self.sched.entries[idx].cancel_requested = true,
            State::Suspended => self.resume_coro(id, true, false),
        }
        Ok(())
    }

    fn join_coroutine(&mut self, fiber: &mut Fiber, h: &Rc<ClassObj>, span: Span) -> Eval {
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
                self.sched.failures.retain(|(f, _)| *f != id);
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
        if self.sched.entries.is_empty() || !self.sched.running.is_empty() {
            return;
        }
        let outermost = self.begin_entry();
        self.sched.shutting_down = true;
        loop {
            let next = self
                .sched
                .entries
                .iter()
                .rev()
                .find(|e| e.state == State::Suspended)
                .map(|e| e.id);
            let Some(id) = next else { break };
            self.resume_coro(id, true, true);
        }
        self.sched.shutting_down = false;
        for e in &mut self.sched.entries {
            if let Some(co) = e.co.take() {
                // Unreachable in practice; a forced unwind must never happen.
                std::mem::forget(co);
            }
        }
        self.sched.entries.clear();
        self.sched.finished = 0;
        self.end_entry(outermost, false);
    }
}
