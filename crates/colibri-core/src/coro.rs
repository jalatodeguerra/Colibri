//! The scheduler's policy (DESIGN §14.2, ARCHITECTURE §16): ticking, starting and finishing
//! coroutines, cancellation, signals, groups, and running the event loop on the main fiber.
//!
//! None of it pauses: a coroutine pauses itself in engine code, and the scheduler resumes it
//! through [`Engine::resume_stack`]. The data is in [`crate::sched`]. Ticks run in a fixed
//! order: the coroutines that were woken by an event and queued (a signal's waiters, and the
//! waiters of a `join_result` whose target ended) come first, in the order they were woken, then
//! every other due coroutine in start order (which includes the waiters of a `join` or
//! `join_all` that ended, see [`Wake::Woken`]). Nothing is polled for a join: waiters are kept
//! in lists with what they wait for. Each resumption from a tick gets an instruction budget
//! of its own; a coroutine's first run shares its starter's.

use colibri_check::hir::Builtin;
use colibri_syntax::Span;

use crate::budget::Budget;
use crate::ctl::{Ctl, Eval, PanicInfo};
use crate::engine::{Engine, Resumed};
use crate::fiber::FiberState;
use crate::heap::ClassObj;
use crate::host::{err_variant, error_value, ok_variant};
use crate::prelude::*;
use crate::sched::*;
use crate::value::Value;

/// A panic with no position and no stack, for a failure that belongs to no running code.
fn bare_panic_info(message: &str) -> Box<PanicInfo> {
    Box::new(PanicInfo {
        message: message.to_string(),
        span: Span::new(colibri_syntax::FileId(0), 0, 0),
        stack: Vec::new(),
        notes: Vec::new(),
        budget: false,
        debug_serial: 0,
        memory: None,
    })
}

/// What follows the name of a pausing operation in the panic for one that runs inside native code.
const NATIVE_PAUSE: &str = "cannot pause inside code that a built-in operation calls (a `Show`, `Eq`, `Ord`, `Hash` or `Clone` impl, or an iterator's `next`)";

/// The scheduler's policy that engines and the runtime call, for any engine.
#[doc(hidden)]
pub trait Sched: Engine {
    /// Common checks of `wait` and `join`: a coroutine is running, and no host function, `Drop`
    /// hook or native operation sits on its stack. Gives the coroutine's id.
    fn pause_context(
        &mut self,
        fiber: &mut Self::Fiber,
        span: Span,
        what: &str,
    ) -> Result<u64, Ctl> {
        let state = Self::fiber_state(fiber);
        let Some(id) = state.coro else {
            let message = format!(
                "`{what}` can only pause a coroutine, but this code is not running in one (start one with `go`)"
            );
            return Err(self.panic(fiber, span, message));
        };
        if state.host_depth > 0 {
            let message = format!(
                "`{what}` cannot pause inside a host function's callback: the host's stack frame cannot be suspended"
            );
            return Err(self.panic(fiber, span, message));
        }
        if self.core().hook_depth > 0 {
            let message = format!("`{what}` cannot pause inside a `Drop` hook");
            return Err(self.panic(fiber, span, message));
        }
        if state.native_depth > 0 {
            let message = format!("`{what}` {NATIVE_PAUSE}");
            return Err(self.panic(fiber, span, message));
        }
        Ok(id)
    }

    // ---- ticking ----

    /// Resumes every due coroutine once, in start order. A coroutine started during the tick
    /// is not resumed in it. Each resumption gets its own instruction budget.
    fn tick(&mut self, dt: f64) -> Result<(), Box<PanicInfo>> {
        let entry = self.core_mut().begin_entry();
        if entry.is_outermost() {
            self.drain_root_drops();
        }
        let result = self.tick_core(dt);
        self.core_mut().end_entry(entry, false);
        match result {
            Ok(()) => Ok(()),
            Err(TickError::Failure(info)) => Err(info),
            Err(TickError::Reentrant) => Err(bare_panic_info(
                "`tick` cannot be called from a coroutine or from inside a tick",
            )),
        }
    }

    /// One tick, then the failures of the coroutines as one panic.
    fn tick_core(&mut self, dt: f64) -> Result<(), TickError> {
        self.tick_run(dt)?;
        self.core_mut().sched.take_failures()
    }

    // ---- starting and cancelling ----

    /// Runs coroutine `id` until it pauses or ends. `fresh` gives the resumption an instruction
    /// budget of its own (the scheduler's ticks); a start shares its starter's.
    #[inline]
    fn resume_coro(&mut self, id: u64, cancel: bool, fresh: bool) {
        let core = self.core_mut();
        let s = &mut core.sched;
        let Some(idx) = s.find(id) else {
            return;
        };
        let e = &mut s.entries[idx];
        if e.state != State::Suspended {
            return;
        }
        e.state = State::Running;
        let slot = e.slot;
        if cancel {
            // A cancelled waiter leaves the list of what it waited for.
            let wake = std::mem::replace(&mut e.wake, Wake::Ready);
            s.forget_waiter(id, &wake);
        }
        s.resumptions += 1;
        s.running.push(id);
        let saved_budget = fresh.then(|| {
            let allotment = core.budget.allotment;
            let slice = core.safepoints.slice();
            core.close_budget();
            let fresh = Budget::fresh(allotment, slice, core.safepoints.progress);
            std::mem::replace(&mut core.budget, fresh)
        });
        let saved_memory = fresh.then(|| core.begin_resumption_memory());
        // No reference into the core is held across this call: the engine runs the
        // coroutine, which uses the core through the engine's own pointer.
        let outcome = self.resume_stack(id, slot, cancel);
        let core = self.core_mut();
        let exhausted = if fresh { core.exhaustion() } else { None };
        if let Some(m) = saved_memory {
            core.end_resumption_memory(m);
        }
        if let Some(b) = saved_budget {
            // What the coroutine spent counts toward the slice; the entry it interrupted goes on
            // from there.
            core.close_budget();
            core.budget = b;
            core.sync_slice();
            core.open_budget();
        }
        let s = &mut core.sched;
        s.running.pop();
        let Some(idx) = s.find(id) else {
            // Cannot happen (nothing removes a running entry).
            return;
        };
        let (finish, exhausted) = match outcome {
            Resumed::Suspended => {
                s.entries[idx].state = State::Suspended;
                return;
            }
            Resumed::Finished(finish) => (finish, exhausted),
            // The engine cannot find the coroutine it was asked to resume. The scheduler only
            // resumes suspended coroutines, so the engine has lost its side of one: a broken
            // invariant. The coroutine ends as a failure (a tick reports it, its group and
            // joiners see it end), instead of staying suspended to be resumed at every tick,
            // and instead of keeping `cancel_all_coroutines` from ever finishing.
            Resumed::Gone => (
                Finish::Panicked(bare_panic_info(
                    "internal error: the engine lost the stack of a coroutine",
                )),
                None,
            ),
        };
        s.entries[idx].state = State::Finished;
        s.finished += 1;
        self.finish_coro(idx, id, finish, exhausted);
    }

    /// Cancels every live coroutine, newest first, so their `defer`s and drops run before the
    /// module-level values die. Nothing can start meanwhile. Does nothing while a coroutine runs
    /// (a host function that calls this from inside one changes nothing). Afterwards no
    /// coroutine is suspended: every one ran to its end, which is when an engine frees what it
    /// kept for it.
    fn cancel_all_coroutines(&mut self) {
        let s = &self.core().sched;
        if s.entries.is_empty() || s.coroutine_running() {
            return;
        }
        let entry = self.core_mut().begin_entry();
        self.core_mut().sched.shutting_down = true;
        loop {
            let next = self
                .core()
                .sched
                .entries
                .iter()
                .rev()
                .find(|e| e.state == State::Suspended)
                .map(|e| e.id);
            let Some(id) = next else { break };
            self.resume_coro(id, true, true);
        }
        let s = &mut self.core_mut().sched;
        s.shutting_down = false;
        s.entries.clear();
        s.finished = 0;
        s.signals.clear();
        s.joiners.clear();
        s.group_waiters.clear();
        // (Every parked coroutine was cancelled above, which took its wait out already.)
        for (_, w) in s.chan_waits.drain() {
            w.token.cancel();
        }
        s.signaled.clear();
        s.groups.clear();
        s.ext.clear_completed();
        self.core_mut().end_entry(entry, false);
    }

    /// Cancels the suspended coroutines with an id of `first` or more, newest first: the ones
    /// a REPL entry started when it failed and its code went away. Does nothing while a
    /// coroutine runs.
    fn cancel_coroutines_from(&mut self, first: u64) {
        if self.core().sched.coroutine_running() {
            return;
        }
        let entry = self.core_mut().begin_entry();
        self.cancel_coroutines_since(first);
        self.core_mut().end_entry(entry, false);
    }

    /// Cancels the suspended coroutines with an id of `first` or more, newest first, whether or
    /// not a coroutine is running (a task that its joiner ran inline leaves some behind, and the
    /// joiner may be a coroutine itself).
    fn cancel_coroutines_since(&mut self, first: u64) {
        // A `defer` or a `Drop` hook that runs during a cancel may start coroutines, which are
        // cancelled in turn; a few rounds are enough for any program that is not building them
        // forever.
        for _ in 0..64 {
            let ids: Vec<u64> = self
                .core()
                .sched
                .entries
                .iter()
                .rev()
                .filter(|e| e.id >= first && e.state == State::Suspended)
                .map(|e| e.id)
                .collect();
            if ids.is_empty() {
                break;
            }
            for id in ids {
                self.cancel_id(id);
            }
        }
    }

    // ---- driving the scheduler from the main fiber ----

    /// May the main fiber run the event loop now? `depth` is how many host functions may be
    /// active (0 for a Colibri-level wait, 1 for a host function waiting on its own thread).
    fn can_drive(&self, fiber: &Self::Fiber, depth: u32) -> bool {
        let state = Self::fiber_state(fiber);
        let core = self.core();
        state.coro.is_none()
            && state.host_depth <= depth
            && state.native_depth == 0
            && core.hook_depth == 0
            && !core.sched.is_busy()
            && !core.sched.is_shutting_down()
    }

    /// Runs the event loop on the main fiber, as the loop after `main` does (ticks with the
    /// time elapsed on the clock, sleeping until the next wake or a completion), until `done`
    /// holds or the clock reaches `deadline`. With `report`, a coroutine's failure ends the
    /// wait with that panic unless `done` holds. Always ticks once first.
    fn drive(
        &mut self,
        deadline: Option<f64>,
        report: bool,
        done: &mut dyn FnMut(&mut Self) -> bool,
    ) -> Drive {
        let mut last = self.core_mut().sched.clock_now();
        loop {
            let now = self.core_mut().sched.clock_now();
            let dt = (now - last).max(0.0);
            last = now;
            if self.tick_run(dt).is_err() {
                return Drive::Stuck;
            }
            if done(self) {
                return Drive::Done;
            }
            if report && let Err(TickError::Failure(info)) = self.core_mut().sched.take_failures() {
                return Drive::Failure(info);
            }
            let s = &mut self.core_mut().sched;
            let now = s.clock_now();
            let mut delay = match (s.next_wake(0.001), deadline) {
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
            self.core_mut().sched.clock_sleep(delay);
        }
    }

    /// [`Sched::drive`] for a wait on the main fiber: the checks first, and a panic when the
    /// wait could never end.
    fn drive_main(
        &mut self,
        fiber: &mut Self::Fiber,
        span: Span,
        what: &str,
        deadline: Option<f64>,
        done: &mut dyn FnMut(&mut Self) -> bool,
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
    fn wait_on_main(&mut self, fiber: &mut Self::Fiber, seconds: f64, span: Span) -> Eval<()> {
        let deadline = self.core_mut().sched.clock_now() + seconds;
        self.drive_main(fiber, span, "wait", Some(deadline), &mut |_| false)
    }

    // ---- joining ----

    /// What `join` (`as_result` false) or `join_result` (true) gives for a coroutine that has
    /// ended. `join` returns the value and panics for a panic or a cancel of the target.
    /// `join_result` returns `ok(value)`, or `err(e)` for either of those. A panic that the
    /// target left in the failure queue is taken out of it: whoever joins it reports it
    /// instead. `status` is the handle's status, which is not `RUNNING`. The pausing is the
    /// engine's; this is what both do afterwards.
    fn joined_outcome(
        &mut self,
        fiber: &mut Self::Fiber,
        h: &ClassObj,
        status: i64,
        span: Span,
        as_result: bool,
    ) -> Eval {
        let message = match status {
            DONE => {
                let v = h.field(H_RESULT).unwrap_or(Value::Unit);
                return Ok(if as_result { ok_variant(v) } else { v });
            }
            PANICKED => {
                let name = handle_name(h);
                self.core_mut().sched.forget_failure_of(handle_id(h));
                let msg = match h.field(H_RESULT) {
                    Some(Value::Str(s)) => s.to_string(),
                    _ => String::new(),
                };
                if as_result {
                    format!("the coroutine `{name}` panicked: {msg}")
                } else {
                    format!("the joined coroutine `{name}` panicked: {msg}")
                }
            }
            _ if as_result => format!("the coroutine `{}` was cancelled", handle_name(h)),
            _ => format!("the joined coroutine `{}` was cancelled", handle_name(h)),
        };
        if as_result {
            Ok(err_variant(error_value(&message)))
        } else {
            Err(self.panic(fiber, span, message))
        }
    }

    // ---- handle methods that never pause ----

    /// `done` and `cancel` of a `Coroutine` handle (`join` pauses, and is the engine's).
    fn coroutine_builtin(
        &mut self,
        fiber: &mut Self::Fiber,
        op: Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval {
        let Some(h) = handle_of(recv) else {
            return Err(self.internal(fiber, span, "ill-typed coroutine method"));
        };
        let h = std::rc::Rc::clone(h);
        let result = match op {
            Builtin::CoroutineDone => Ok(Value::Bool(handle_status(&h) != RUNNING)),
            Builtin::CoroutineCancel => self.cancel_coroutine(fiber, &h).map(|()| Value::Unit),
            _ => Err(self.internal(fiber, span, "a pausing coroutine method")),
        };
        // `h` is a clone of a handle the caller still holds, so this is never the last owner.
        drop(h);
        result
    }

    /// `count` and `cancel` of a `CoGroup` (`join_all` pauses, and is the engine's).
    fn cogroup_builtin(
        &mut self,
        fiber: &mut Self::Fiber,
        op: Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval {
        let Some(g) = group_id(recv) else {
            return Err(self.internal(fiber, span, "ill-typed CoGroup method"));
        };
        let me = Self::fiber_state(fiber).coro;
        match op {
            Builtin::CoGroupCount => {
                let n = self.core().sched.groups.get(&g).map_or(0, Vec::len);
                Ok(Value::Int(n as i64))
            }
            Builtin::CoGroupCancel => {
                let members = self
                    .core()
                    .sched
                    .groups
                    .get(&g)
                    .cloned()
                    .unwrap_or_default();
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
                    Self::fiber_state_mut(fiber).cancelling = true;
                    return Err(Ctl::Cancel);
                }
                Ok(Value::Unit)
            }
            _ => Err(self.internal(fiber, span, "a pausing CoGroup method")),
        }
    }

    /// `emit` of a `Signal`: its waiters resume at the next tick, in wait order.
    fn signal_builtin(&mut self, fiber: &mut Self::Fiber, recv: &Value, span: Span) -> Eval {
        let Some(sid) = signal_id(recv) else {
            return Err(self.internal(fiber, span, "ill-typed Signal method"));
        };
        let s = &mut self.core_mut().sched;
        if let Some(waiters) = s.signals.remove(&sid) {
            for id in waiters {
                if let Some(idx) = s.find(id) {
                    let e = &mut s.entries[idx];
                    if e.state == State::Suspended && matches!(e.wake, Wake::Signal(w) if w == sid)
                    {
                        e.wake = Wake::Signaled;
                        s.signaled.push(id);
                    }
                }
            }
        }
        Ok(Value::Unit)
    }
}

/// The parts of the policy that are not for an engine to call: they index into the scheduler's
/// entries or run only inside the public ones. Crate-private, with the same blanket impl.
pub(crate) trait SchedInternal: Engine {
    /// One tick, leaving the coroutines' failures queued.
    fn tick_run(&mut self, dt: f64) -> Result<(), TickError> {
        let s = &mut self.core_mut().sched;
        if s.is_busy() {
            return Err(TickError::Reentrant);
        }
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        s.in_tick = true;
        s.ticks += 1;
        s.ext.drain();
        s.time += dt;
        s.frame += 1;
        let mut real_now: Option<f64> = None;
        let count = s.entries.len();
        // Coroutines whose signal was emitted before this tick resume first, in emission order.
        let queue = std::mem::take(&mut s.signaled);
        let frame = s.frame;
        for id in queue {
            let s = &mut self.core_mut().sched;
            let Some(idx) = s.find(id) else {
                continue;
            };
            let e = &mut s.entries[idx];
            if e.state == State::Suspended && matches!(e.wake, Wake::Signaled | Wake::Joined(_)) {
                e.ticked = frame;
                self.resume_coro(id, false, true);
            }
        }
        for i in 0..count {
            let s = &mut self.core_mut().sched;
            let e = &s.entries[i];
            // (`frame` also moves in a nested tick, so the guard compares with this tick's own: a
            // coroutine resumed from the signalled queue may lie after this index.)
            if e.state != State::Suspended || e.ticked == frame || e.id < s.floor {
                continue;
            }
            let due = match &e.wake {
                // Woken by an event, never found due by looking.
                Wake::Signal(_)
                | Wake::Signaled
                | Wake::Joined(_)
                | Wake::Join(_)
                | Wake::JoinResult(_)
                | Wake::Group(_) => false,
                Wake::Ready | Wake::Woken => true,
                Wake::Time(d) => s.time + TIME_EPSILON >= *d,
                Wake::Real(d) => *real_now.get_or_insert_with(|| s.clock.now()) >= *d,
                Wake::External(op) => s.ext.is_completed(*op),
            };
            if due {
                let id = e.id;
                self.resume_coro(id, false, true);
            }
        }
        let s = &mut self.core_mut().sched;
        s.in_tick = false;
        s.ticks -= 1;
        // A tick that a task run inline in a coroutine's join makes, inside the tick that resumed
        // that coroutine, must leave the entries where they are: the outer loop goes on by index.
        if s.ticks == 0 {
            s.compact();
        }
        Ok(())
    }

    /// Records how coroutine `id` ended: its handle's status and result, its group, and a
    /// panic that nobody has collected yet.
    fn finish_coro(
        &mut self,
        idx: usize,
        id: u64,
        finish: Finish,
        exhausted: Option<crate::budget::Exhaustion>,
    ) {
        let s = &mut self.core_mut().sched;
        let handle = s.entries[idx].handle.upgrade();
        if let Some(g) = s.entries[idx].group {
            s.leave_group(g, id);
        }
        // Whoever waits for this coroutine in `join` or `join_result` is woken, in the order they
        // began waiting.
        s.wake_joiners(id);
        let finish = match finish {
            Finish::Panicked(info) => Finish::Panicked(info),
            other if exhausted.is_some() => {
                if let Finish::Done(v) = other {
                    self.release(v);
                }
                let mut info = match exhausted {
                    Some(crate::budget::Exhaustion::Memory(limit)) => {
                        let mut info = bare_panic_info(&crate::mem::limit_message(limit));
                        info.memory = Some(limit);
                        info
                    }
                    _ => bare_panic_info("the instruction budget is exhausted"),
                };
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
                    let old = h.set_field(
                        H_RESULT,
                        Value::Str(std::rc::Rc::from(info.message.as_str())),
                    );
                    if let Ok(old) = old {
                        self.release(old);
                    }
                }
                // Always queued: a report holds it back only while a woken `join_result`
                // waiter has not run (`take_failures`); a budget panic is never held.
                self.core_mut().sched.failures.push((id, info));
            }
        }
    }

    /// Cancels the coroutine `id` (not the running one): a suspended coroutine unwinds at
    /// once, one that is mid-run higher up the resume chain stops at its next pause.
    fn cancel_id(&mut self, id: u64) {
        let s = &mut self.core_mut().sched;
        let Some(idx) = s.find(id) else {
            return;
        };
        match s.entries[idx].state {
            State::Finished => {}
            State::Running => s.entries[idx].cancel_requested = true,
            State::Suspended => self.resume_coro(id, true, false),
        }
    }

    /// The last reference to a `CoGroup` died: its coroutines are cancelled, newest first.
    fn group_died(&mut self, obj: &ClassObj) {
        let Some(Value::Int(g)) = obj.field(0) else {
            return;
        };
        if let Some(members) = self.core_mut().sched.groups.remove(&(g as u64)) {
            for id in members.into_iter().rev() {
                self.cancel_id(id);
            }
        }
    }

    /// The checks of a wait on the main fiber, as panics with a message that says why.
    fn check_drive(&mut self, fiber: &mut Self::Fiber, span: Span, what: &str) -> Eval<()> {
        let core = self.core();
        let why = if Self::fiber_state(fiber).host_depth > 0 {
            "inside a host function's callback"
        } else if core.hook_depth > 0 {
            "inside a `Drop` hook"
        } else if Self::fiber_state(fiber).native_depth > 0 {
            "inside code that a built-in operation calls (a `Show`, `Eq`, `Ord`, `Hash` or `Clone` impl, or an iterator's `next`)"
        } else if core.sched.is_busy() {
            "inside a tick"
        } else if core.sched.is_shutting_down() {
            "while the program is shutting down"
        } else {
            return Ok(());
        };
        let message = format!(
            "`{what}` on the main fiber runs the event loop, which cannot be done {why} (ARCHITECTURE section 22)"
        );
        Err(self.panic(fiber, span, message))
    }

    /// `cancel` of a `Coroutine` handle.
    fn cancel_coroutine(&mut self, fiber: &mut Self::Fiber, h: &ClassObj) -> Eval<()> {
        if handle_status(h) != RUNNING {
            return Ok(());
        }
        let id = handle_id(h);
        let state: &mut FiberState = Self::fiber_state_mut(fiber);
        if state.coro == Some(id) {
            // A coroutine cancelling itself stops at once.
            state.cancelling = true;
            return Err(Ctl::Cancel);
        }
        self.cancel_id(id);
        Ok(())
    }
}

impl<E: Engine> Sched for E {}

impl<E: Engine> SchedInternal for E {}
