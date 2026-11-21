//! Coroutines and their scheduler (DESIGN §14.2, ARCHITECTURE §16).
//!
//! A coroutine is a [`Fiber`] running the ordinary recursive evaluator on its own native stack
//! (`corosensei`). `wait` and friends suspend that stack; the scheduler (`Core::sched`, with its
//! policy in `colibri_core::coro`) resumes the ones that are due when the host calls `tick`,
//! through [`Interp::resume_native_stack`]. What stays here is what needs the native stack: the
//! stacks themselves (`Interp::stacks`), starting a coroutine, and every wait that pauses
//! (`pause_context`, `suspend`, `wait_until`, `join`, `join_all`, `await_external`). Cancellation resumes a
//! suspended coroutine with a cancel signal, so its pause returns `Err(Ctl::Cancel)` and the
//! ordinary unwinding (`exit_scope`) runs its `defer`s and drops. No Rust panic or forced unwind
//! is part of the semantics: every coroutine is run to its end before its stack is freed.
//!
//! The `Interp` must not move while a coroutine exists (the suspended stacks hold references
//! into it); the runtime keeps it in a `Box`.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;

use colibri_check::hir::{Builtin, Callee, Expr, ExprKind, Intrinsic};
use colibri_check::types::{COGROUP_TYPE, COROUTINE_TYPE, SIGNAL_TYPE, Ty};
use colibri_core::coro::Sched;
use colibri_core::engine::Resumed;
use colibri_core::external::{
    CancelToken, Completer, ExternalError, panic_message, unwrap_outcome,
};
use colibri_core::prelude::*;
use colibri_core::sched::*;
use colibri_syntax::Span;
use corosensei::stack::{DefaultStack, Stack};
use corosensei::{Coroutine, CoroutineResult, Yielder};

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

/// One slot of the slab of native stacks: the stack while its coroutine is suspended (`None`
/// while it runs, or once it ended), and how much of the stack the evaluator may use.
struct StackSlot {
    co: Option<Box<Co>>,
    budget: usize,
    /// The size the stack was requested with, to pool it under that size when it ends.
    size: usize,
}

/// How many finished stacks the pool keeps. A server that starts one short coroutine per
/// request has one or a few alive at a time, so a handful already turns every start after the
/// first into a reuse. Sixteen covers a burst of that size and bounds what idles: each pooled
/// stack is `DEFAULT_COROUTINE_STACK` (16 MiB) of address space, 256 MiB at most per `Interp`.
/// That address space stays reserved, and under strict overcommit (`vm.overcommit_memory=2`)
/// it is also charged against the commit limit, so a larger pool would cost more there. The
/// pages a deep run touched are given back when the stack is pooled (see `trim_stack`), so an
/// idle pool holds only the hot top of each stack. Beyond the cap a finished stack is
/// unmapped as before.
const STACK_POOL_CAP: usize = 16;

/// The part of a pooled stack, at its top, that keeps its pages: a typical short coroutine
/// stays within it, so reuse neither faults nor calls the system. Below it, the pages a deep
/// run touched are given back.
const STACK_HOT_WINDOW: usize = 256 * 1024;

/// Gives the pages of a finished coroutine's stack below the hot window back to the system, so
/// that a pooled stack does not keep the memory of its deepest run. Returns whether the stack
/// may go into the pool.
#[cfg(unix)]
fn trim_stack(stack: &DefaultStack) -> bool {
    let base = stack.base().get();
    let limit = stack.limit().get();
    let Some(len) = (base - limit).checked_sub(STACK_HOT_WINDOW) else {
        return true;
    };
    // Linux drops the pages at once. Where `MADV_FREE` exists they are reclaimed lazily, when
    // the system is short of memory.
    #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
    let advice = libc::MADV_FREE;
    #[cfg(not(any(target_vendor = "apple", target_os = "freebsd")))]
    let advice = libc::MADV_DONTNEED;
    // SAFETY: `[limit, limit + len)` lies inside the mapping that `DefaultStack` owns, and
    // page-aligned: `limit` is the start of the mapping, and `base` its page-aligned end, less
    // a multiple of every page size in use. The coroutine that used the stack ended (the
    // caller holds the stack by value), so nothing on it is live. The advice keeps the mapping
    // and its protections (the guard page included): it only discards the contents, which
    // nothing reads, because a start rewrites the top of the stack and the rest is written
    // before it is read. A failure leaves the pages as they were; it is ignored.
    unsafe {
        libc::madvise(limit as *mut libc::c_void, len, advice);
    }
    true
}

/// On Windows the committed part of the stack grows through a chain of guard pages, which
/// decommitting would break, so a stack that went deeper than the hot window is dropped (and
/// unmapped) instead of pooled.
#[cfg(windows)]
fn trim_stack(stack: &DefaultStack) -> bool {
    let teb = stack.teb_fields();
    teb.StackBase.saturating_sub(teb.StackLimit) <= STACK_HOT_WINDOW
}

#[cfg(not(any(unix, windows)))]
fn trim_stack(_stack: &DefaultStack) -> bool {
    true
}

/// The native stacks of the coroutines that have not ended: a slab of slots, each holding its
/// stack while its coroutine is suspended and nothing while it runs (the stack is moved out
/// for the resumption, so nothing in here is borrowed while the coroutine uses the
/// interpreter, and a second resumption of a running coroutine finds an empty slot instead of
/// a stack). A finished coroutine's slot goes on the free list for the next start. The
/// scheduler keeps the slot index of each coroutine (an `EngineSlot`) and hands it back at
/// every resumption.
#[derive(Default)]
pub(crate) struct Stacks {
    slots: Vec<StackSlot>,
    free: Vec<u32>,
    /// Stacks of coroutines that ran to their end, ready for the next start. All have size
    /// `pool_size` (the pool is emptied when a start asks for another size).
    pool: Vec<DefaultStack>,
    pool_size: usize,
    /// How many stacks were mapped (not taken from the pool), for tests.
    allocated: usize,
}

impl Stacks {
    /// Stores the stack of a new coroutine with its native stack budget and returns its slot.
    pub(crate) fn insert(&mut self, co: Box<Co>, budget: usize, size: usize) -> u32 {
        let fresh = StackSlot {
            co: Some(co),
            budget,
            size,
        };
        match self.free.pop() {
            Some(index) => {
                self.slots[index as usize] = fresh;
                index
            }
            None => {
                self.slots.push(fresh);
                (self.slots.len() - 1) as u32
            }
        }
    }

    /// A stack of `size` bytes for a new coroutine: a pooled one, or a newly mapped one. Pooled
    /// stacks of another size (the size was changed) are unmapped first.
    pub(crate) fn acquire(&mut self, size: usize) -> std::io::Result<DefaultStack> {
        if self.pool_size != size {
            self.pool.clear();
            self.pool_size = size;
        }
        if let Some(stack) = self.pool.pop() {
            debug_assert!(
                stack.base().get() - stack.limit().get() >= size,
                "a pooled stack is smaller than the requested size"
            );
            return Ok(stack);
        }
        let stack = DefaultStack::new(size)?;
        self.allocated += 1;
        Ok(stack)
    }

    /// The coroutine of `slot` ran to its end: takes its stack back into the pool (or unmaps it
    /// when the pool is full) and frees the slot. Only a coroutine that finished gets here:
    /// `into_stack` panics for one that did not, and a stack that is forgotten (`forget_all`,
    /// a lost stack) never comes through here.
    pub(crate) fn recycle(&mut self, slot: u32, co: Co) {
        let size = self.slots[slot as usize].size;
        let stack = co.into_stack();
        if size == self.pool_size && self.pool.len() < STACK_POOL_CAP && trim_stack(&stack) {
            self.pool.push(stack);
        }
        self.free(slot);
    }

    /// Moves the stack out of `slot` for a resumption, with its budget; `None` if it is not
    /// there (the coroutine is running, or ended).
    #[inline]
    pub(crate) fn take(&mut self, slot: u32) -> Option<(Box<Co>, usize)> {
        let s = self.slots.get_mut(slot as usize)?;
        let co = s.co.take()?;
        Some((co, s.budget))
    }

    /// Puts the stack of a coroutine that paused back into its slot.
    #[inline]
    pub(crate) fn put_back(&mut self, slot: u32, co: Box<Co>) {
        self.slots[slot as usize].co = Some(co);
    }

    /// The coroutine of `slot` ran to its end and its stack was dropped: the slot is free. In
    /// debug builds a slot that still holds a stack, or that is already free, is a bug here.
    #[inline]
    pub(crate) fn free(&mut self, slot: u32) {
        debug_assert!(
            self.slots[slot as usize].co.is_none(),
            "a stack slot was freed while it still holds a stack"
        );
        debug_assert!(!self.free.contains(&slot), "a stack slot was freed twice");
        self.free.push(slot);
    }

    /// How many slots are in use: started and not yet freed, whether the stack is stored or out
    /// for a resumption.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    /// Whether no stack is stored (none suspended, none left over).
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.iter().all(|s| s.co.is_none())
    }

    /// Forgets every stored stack without unwinding it, and empties the slab.
    pub(crate) fn forget_all(&mut self) {
        for s in self.slots.drain(..) {
            if let Some(co) = s.co {
                std::mem::forget(co);
            }
        }
        self.free.clear();
        // Pooled stacks hold no coroutine: they are simply unmapped.
        self.pool.clear();
    }
}

pub(crate) type Y = Yielder<Resume, *mut Interp>;
pub(crate) type Co = Coroutine<Resume, *mut Interp, Finish, DefaultStack>;

impl Interp {
    /// How many coroutine stacks were mapped from the OS so far, as opposed to taken from the
    /// pool of finished ones. For tests of the pool.
    #[doc(hidden)]
    pub fn coroutine_stacks_allocated(&self) -> usize {
        self.stacks.allocated
    }

    /// How many finished coroutine stacks wait in the pool. For tests of the pool.
    #[doc(hidden)]
    pub fn coroutine_stacks_pooled(&self) -> usize {
        self.stacks.pool.len()
    }

    // ---- the scheduler's calls for the host ----
    //
    // `next_wake`, `tick` and `take_coroutine_failure` forward to `Core`/`Sched` on purpose: the
    // runtime's event loop and many tests call them as methods of the engine (`tick` takes the
    // fiber that a heap-frame engine will need, and the tree-walker ignores), so they are the
    // engine's own API, not accessors of the shared state.

    /// How long until the next coroutine needs a tick, for an event loop: `None` when nothing
    /// is alive or every live coroutine waits for an event (a signal, a `join`, a `join_all`:
    /// nothing but code that runs at a tick or before it can wake it), a small polling
    /// interval when something is checked at every tick (`wait_until`), else the time to the
    /// earliest timed wake.
    pub fn next_wake(&mut self, poll: f64) -> Option<f64> {
        self.core.sched.next_wake(poll)
    }

    /// Resumes every due coroutine once, in start order. A coroutine started during the tick
    /// is not resumed in it. Each resumption gets its own instruction budget.
    pub fn tick(&mut self, fiber: &mut Fiber, dt: f64) -> Result<(), Box<PanicInfo>> {
        let _ = fiber;
        Sched::tick(self, dt)
    }

    /// Forgets coroutine panics nobody collected (between tests).
    pub(crate) fn sched_forget_failures(&mut self) {
        self.core.sched.forget_failures();
    }

    /// Panics that no `tick` has reported yet (a coroutine that failed while starting or
    /// being cancelled), for the host to collect after the program ended.
    pub fn take_coroutine_failure(&mut self) -> Option<Box<PanicInfo>> {
        match self.core.sched.take_failures() {
            Err(TickError::Failure(info)) => Some(info),
            _ => None,
        }
    }

    /// Whether an external operation that `fiber` waits for lets the rest of the program run
    /// meanwhile: a coroutine outside a `Drop` hook and a callback is suspended, and the main
    /// fiber drives the event loop where it may. When this is `false` the wait blocks the
    /// interpreter thread, and nothing that another coroutine still has to do can happen until
    /// the operation is done (see [`Interp::await_external_with`]).
    pub fn external_wait_runs_others(&self, fiber: &Fiber) -> bool {
        self.can_suspend_for_external(fiber) || self.can_drive(fiber, 1)
    }

    /// Whether the coroutine on `fiber` may suspend for a background operation: it is a
    /// coroutine, at most one host function deep, outside a `Drop` hook.
    pub(crate) fn can_suspend_for_external(&self, fiber: &Fiber) -> bool {
        fiber.state.coro.is_some() && fiber.state.host_depth <= 1 && self.core.hook_depth == 0
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
        if self.core.sched.is_shutting_down() {
            // Nothing new may start while the program is being torn down.
            self.release_all(values);
            fields[H_STATUS] = Value::Int(CANCELLED);
            return Ok(Value::Obj(ClassObj::new(COROUTINE_TYPE, fields)));
        }
        let handle = ClassObj::new(COROUTINE_TYPE, fields);
        let size = self.core.sched.stack_size();
        let stack = match self.stacks.acquire(size) {
            Ok(s) => s,
            Err(e) => {
                self.release_all(values);
                self.release(Value::Obj(handle));
                let msg = format!("cannot allocate a coroutine stack of {size} bytes: {e}");
                return Err(self.panic(fiber, span, msg));
            }
        };
        let body = move |yielder: &Y, first: Resume| -> Finish {
            // SAFETY: the pointer comes from `resume_native_stack`, which holds the interpreter
            // (and with it `core`) for the whole resumption and uses nothing of it until this
            // coroutine pauses or ends, so at this moment this is the only live reference to
            // the interpreter and to everything in it, `core` included. The interpreter does
            // not move while coroutines live (the runtime keeps it in a `Box`). This
            // reference lives as long as the body, across every pause: while the stack is
            // suspended the resumer uses the interpreter through its own pointer, and each
            // pause is the call `yielder.suspend`, across which this body holds no reference
            // to `core` or to any field (the borrows it takes are all inside one call), and
            // which the compiler cannot see through (`black_box` on the pointer in `suspend`).
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
            // The last releases of this coroutine (its closure and what it captured, its
            // arguments) may have run `Drop` hooks: their panics are this coroutine's failure,
            // not the next fiber's or the host's.
            let result = interp.fold_hook_panics(result);
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
        let index = self.stacks.insert(Box::new(co), budget, size);
        self.core
            .sched
            .register(id, Rc::downgrade(&handle), group, EngineSlot(index));
        // The first run shares the starter's instruction budget: it is part of the starter's
        // own execution.
        self.resume_coro(id, false, false);
        self.core.sched.compact_if_crowded();
        Ok(Value::Obj(handle))
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
        // The resumer uses the interpreter while this stack is suspended. No reference into
        // `self` (not `core`, not a field) is held across the call, and `self` is not used
        // between the call and its return; afterwards `self` is used again, and the compiler
        // must not assume it was untouched meanwhile (`black_box`).
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
            if self.core.sched.is_busy() {
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
                    Wake::Time(self.core.sched.time() + s)
                } else {
                    Wake::Real(self.core.sched.clock_now() + s)
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
        let queued = self.core.sched.ext().submit(Box::new(move || {
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
            self.core.sched.ext().forget(op);
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
                self.core.sched.ext().forget(op);
                token.cancel();
                return Err(ExternalError::CannotStart(message));
            }
            Err(payload) => {
                self.core.sched.ext().forget(op);
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
        self.core.sched.ext().begin(op)
    }

    /// Runs the cancel hooks of an operation whose waiter is giving up. A hook that panics must
    /// not leave the operation's entries behind: they are removed, and the panic is passed on
    /// (so it becomes a panic of the host function that is waiting).
    fn cancel_completion(&mut self, op: u64, token: &CancelToken) {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| token.cancel()))
        {
            self.core.sched.ext().finish(op);
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
            while !self.core.sched.ext().is_completed(op) {
                if self.suspend(fiber, id, Wake::External(op)).is_err() {
                    self.cancel_completion(op, token);
                    outcome = Err(ExternalError::Cancelled);
                    break;
                }
            }
            outcome
        } else if self.can_drive(fiber, 1) {
            self.drive(None, false, &mut |me| me.core.sched.ext().is_completed(op));
            Ok(())
        } else {
            // Nothing can run meanwhile: block this thread until the result is there. Every
            // end of a completer sends or drops the result's sender, so this does not hang.
            let blocked = result_rx.recv();
            self.core.sched.ext().forget(op);
            return unwrap_outcome(blocked.map_err(|_| ()));
        };
        self.core.sched.ext().finish(op);
        waited?;
        unwrap_outcome(result_rx.try_recv().map_err(|_| ()))
    }

    // ---- the engine's half of resuming and the built-ins that pause ----

    /// [`Engine::resume_stack`]: switches to the native stack in `slot` and back.
    #[inline]
    pub(crate) fn resume_native_stack(&mut self, slot: EngineSlot, cancel: bool) -> Resumed {
        // The stack leaves the slab for the resumption: the slot is `None` while the
        // coroutine runs, so a second resumption of it (which the scheduler does not
        // attempt: it resumes suspended coroutines only) would find nothing and report `Gone`.
        let Some((mut co, budget)) = self.stacks.take(slot.0) else {
            return Resumed::Gone;
        };
        let saved_stack = std::mem::replace(&mut self.native_stack_budget, budget);
        // From here to the `black_box` below, no reference into the interpreter (`self`, so
        // `core` and `stacks`) is alive: `co` is owned by this frame, and the coroutine on it
        // has the only reference, through `ptr`.
        let ptr: *mut Interp = self;
        let outcome = co.resume(Resume {
            interp: ptr,
            cancel,
        });
        std::hint::black_box(ptr);
        self.native_stack_budget = saved_stack;
        match outcome {
            CoroutineResult::Yield(_) => {
                // Nested starts may have grown the slab meanwhile; the slot index is still valid.
                self.stacks.put_back(slot.0, co);
                Resumed::Suspended
            }
            CoroutineResult::Return(finish) => {
                // The coroutine ran to its end (it returned, also when cancelled): only now
                // is its stack freed, into the pool.
                self.stacks.recycle(slot.0, *co);
                Resumed::Finished(finish)
            }
        }
    }

    /// `join` and `join_result` on a `Coroutine`, and `join_all` on a `CoGroup`.
    pub(crate) fn pausing_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        recv: &Value,
        span: Span,
    ) -> Eval {
        if op == Builtin::CoGroupJoinAll {
            return self.join_all(fiber, recv, span);
        }
        let Some(h) = handle_of(recv) else {
            return Err(self.internal(fiber, span, "ill-typed coroutine method"));
        };
        let h = Rc::clone(h);
        let result = self.join_coroutine(fiber, &h, span, op == Builtin::CoroutineJoinResult);
        // `h` is a clone of a handle the caller still holds, so this is never the last owner.
        drop(h);
        result
    }

    /// Waits until the coroutine of `h` has ended, then `joined_outcome`.
    fn join_coroutine(
        &mut self,
        fiber: &mut Fiber,
        h: &Rc<ClassObj>,
        span: Span,
        as_result: bool,
    ) -> Eval {
        let what = if as_result { "join_result" } else { "join" };
        let mut status = handle_status(h);
        if status == RUNNING && fiber.state.coro.is_none() {
            let target = Rc::clone(h);
            self.drive_main(fiber, span, what, None, &mut |_| {
                handle_status(&target) != RUNNING
            })?;
            status = handle_status(h);
        }
        if status == RUNNING {
            let id = self.pause_context(fiber, span, what)?;
            if handle_id(h) == id {
                return Err(self.panic(fiber, span, "a coroutine cannot join itself"));
            }
            // Registered with the target: woken when it ends, never polled.
            let target = handle_id(h);
            loop {
                let wake = if as_result {
                    Wake::JoinResult(target)
                } else {
                    Wake::Join(target)
                };
                self.suspend(fiber, id, wake)?;
                status = handle_status(h);
                if status != RUNNING {
                    break;
                }
            }
        }
        self.joined_outcome(fiber, h, status, span, as_result)
    }

    /// `join_all` on a `CoGroup`: pauses until no other member of the group is alive.
    fn join_all(&mut self, fiber: &mut Fiber, recv: &Value, span: Span) -> Eval {
        let Some(g) = group_id(recv) else {
            return Err(self.internal(fiber, span, "ill-typed CoGroup method"));
        };
        let me = fiber.state.coro;
        if !self.core.sched.group_has_others(g, me) {
            return Ok(Value::Unit);
        }
        if me.is_none() {
            self.drive_main(fiber, span, "join_all", None, &mut |it| {
                !it.core.sched.group_has_others(g, me)
            })?;
            return Ok(Value::Unit);
        }
        let id = self.pause_context(fiber, span, "join_all")?;
        while self.core.sched.group_has_others(g, me) {
            self.suspend(fiber, id, Wake::Group(g))?;
        }
        Ok(Value::Unit)
    }

    // ---- shutdown ----

    /// Cancels every live coroutine, newest first, so their `defer`s and drops run before the
    /// module-level values die. Nothing can start meanwhile. While a coroutine runs this does
    /// nothing (see [`Sched::cancel_all_coroutines`]).
    pub fn cancel_all_coroutines(&mut self) {
        let was_running = self.core.sched.coroutine_running();
        Sched::cancel_all_coroutines(self);
        // Every suspended coroutine was resumed with a cancel and ran to its end, which frees
        // its stack, so none is left. This guards the rule that a stack is freed only after
        // its coroutine ended: a stack left here would later be unwound by force. (When this
        // call did nothing because a coroutine is running, the slab is in use and untouched.)
        debug_assert!(
            was_running || self.stacks.is_empty(),
            "a coroutine stack outlived `cancel_all_coroutines`"
        );
    }
}

#[cfg(test)]
mod tests {
    use colibri_core::coro::Sched;
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
                    Err(colibri_core::external::ExternalError::CannotStart(_))
                ));
            }
            // The id that the dropped completer posted is ignored when it is collected.
            interp.core.sched.ext().drain();
            assert!(interp.core.sched.ext().is_idle());
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
        interp.core.sched.ext().drain();
        assert!(interp.core.sched.ext().is_completed(op));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            interp.cancel_completion(op, &token)
        }));
        assert!(outcome.is_err(), "the panic is passed on");
        assert!(interp.core.sched.ext().is_idle());
    }

    /// Loads `src` (checked, no host functions) into a fresh interpreter.
    fn loaded(src: &str, out: Box<dyn std::io::Write>) -> (crate::Interp, colibri_check::Program) {
        let (ast, diags) = colibri_syntax::parse_file(colibri_syntax::FileId(0), src);
        assert!(diags.is_empty(), "{diags:?}");
        let checked = colibri_check::check(&ast);
        let program = checked
            .program
            .unwrap_or_else(|| panic!("{:?}", checked.diagnostics));
        let mut interp = crate::Interp::new(out, 200);
        interp.set_native_stack_budget(64 << 20);
        interp.load(&program);
        (interp, program)
    }

    /// A buffer the test can read back after the interpreter wrote to it.
    #[derive(Clone, Default)]
    struct Out(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl std::io::Write for Out {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Out {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.borrow()).into_owned()
        }
    }

    /// Two coroutines suspended at once hold two slots; when one ends, its slot is reused by
    /// the next start while the other keeps its own, and each resumes where it paused.
    #[test]
    fn a_freed_slot_is_reused_and_a_suspended_neighbour_keeps_its_own() {
        use crate::Fiber;
        let src = "fn slow(tag: str) {\n    wait(2.0)\n    print(tag)\n}\nfn quick(tag: str) {\n    wait(0.5)\n    print(tag)\n}\nfn first() {\n    let _a = go slow(\"slow\")\n    let _b = go quick(\"quick\")\n}\nfn second() {\n    let _c = go quick(\"again\")\n}\n";
        let out = Out::default();
        let (mut interp, program) = loaded(src, Box::new(out.clone()));
        let mut fiber = Fiber::new();
        interp
            .call(&mut fiber, program.fn_by_name("first").unwrap(), vec![])
            .expect("first");
        assert_eq!(interp.stacks.live(), 2, "two coroutines, two slots");
        interp.tick(&mut fiber, 1.0).expect("tick");
        assert_eq!(out.text(), "quick\n");
        assert_eq!(interp.stacks.live(), 1, "the quick one gave its slot back");
        interp
            .call(&mut fiber, program.fn_by_name("second").unwrap(), vec![])
            .expect("second");
        assert_eq!(interp.stacks.live(), 2);
        assert_eq!(interp.stacks.slots.len(), 2, "the freed slot was reused");
        for _ in 0..4 {
            interp.tick(&mut fiber, 0.5).expect("tick");
        }
        assert_eq!(out.text(), "quick\nagain\nslow\n");
        assert_eq!(interp.stacks.live(), 0);
    }

    /// A coroutine that starts another while it runs: its own stack is out of the slab then, and
    /// the slab grows (it may reallocate) before the stack goes back into its slot.
    #[test]
    fn a_nested_start_while_a_stack_is_out_of_its_slot() {
        use crate::Fiber;
        let src = "fn leaf(n: int) {\n    wait(0.5)\n    print(n)\n}\nfn parent() {\n    var i = 0\n    while i < 40 {\n        let _h = go leaf(i)\n        i = i + 1\n    }\n    wait(0.1)\n    print(\"parent\")\n}\nfn main() {\n    let _p = go parent()\n}\n";
        let out = Out::default();
        let (mut interp, program) = loaded(src, Box::new(out.clone()));
        let mut fiber = Fiber::new();
        interp
            .call(&mut fiber, program.fn_by_name("main").unwrap(), vec![])
            .expect("main");
        assert_eq!(interp.stacks.live(), 41, "the parent and its forty leaves");
        for _ in 0..4 {
            interp.tick(&mut fiber, 0.5).expect("tick");
        }
        let text = out.text();
        assert!(text.starts_with("parent\n0\n1\n"), "{text}");
        assert_eq!(text.lines().count(), 41);
        assert_eq!(interp.stacks.live(), 0);
    }

    /// Many start and finish cycles keep the slab at the size of the most that lived at once: a
    /// slot that is never freed would grow it (and the live count) with every cycle.
    #[test]
    fn slots_are_freed_after_many_start_and_finish_cycles() {
        use crate::Fiber;
        let src = "fn work() {\n    wait(0.0)\n}\nfn main() {\n    var i = 0\n    while i < 100 {\n        let h = go work()\n        h.join()\n        i = i + 1\n    }\n}\n";
        let (mut interp, program) = loaded(src, Box::new(std::io::sink()));
        let mut fiber = Fiber::new();
        interp
            .call(&mut fiber, program.fn_by_name("main").unwrap(), vec![])
            .expect("main");
        assert_eq!(interp.stacks.live(), 0, "every slot was freed");
        assert!(
            interp.stacks.slots.len() <= 2,
            "the slab has {} slots after 100 cycles",
            interp.stacks.slots.len()
        );
    }

    /// Many sequential start, join cycles map one stack and reuse it; with the pool full (more
    /// alive at once than the cap) the extra stacks are unmapped when they end.
    #[test]
    fn finished_stacks_are_reused_and_the_pool_is_capped() {
        use crate::Fiber;
        let src = "fn work(n: int) -> int {\n    wait(0.0)\n    return n\n}\nfn seq() {\n    var i = 0\n    while i < 200 {\n        let h = go work(i)\n        h.join()\n        i = i + 1\n    }\n}\nfn burst() {\n    var i = 0\n    while i < 40 {\n        let _h = go work(i)\n        i = i + 1\n    }\n}\n";
        let (mut interp, program) = loaded(src, Box::new(std::io::sink()));
        let mut fiber = Fiber::new();
        interp
            .call(&mut fiber, program.fn_by_name("seq").unwrap(), vec![])
            .expect("seq");
        assert_eq!(
            interp.coroutine_stacks_allocated(),
            1,
            "one stack for 200 starts"
        );
        assert_eq!(interp.coroutine_stacks_pooled(), 1);
        interp
            .call(&mut fiber, program.fn_by_name("burst").unwrap(), vec![])
            .expect("burst");
        assert_eq!(
            interp.coroutine_stacks_allocated(),
            40,
            "39 more were needed at once"
        );
        interp.tick(&mut fiber, 0.1).expect("tick");
        assert_eq!(interp.stacks.live(), 0);
        assert_eq!(interp.coroutine_stacks_pooled(), super::STACK_POOL_CAP);
    }

    /// A coroutine cancelled while suspended runs to its end through its unwinding (its `defer`
    /// runs), so its stack is pooled like any other and the next start reuses it. (A stack that
    /// never ran to its end is not pooled: it is forgotten, see
    /// `a_lost_stack_ends_its_coroutine_as_an_internal_error`.)
    #[test]
    fn a_cancelled_coroutines_stack_is_pooled_and_reused() {
        use crate::Fiber;
        let src = "fn deep(n: int) -> int {\n    if n == 0 {\n        wait(100.0)\n        return 0\n    }\n    return 1 + deep(n - 1)\n}\nfn sleeper() {\n    defer print(\"cleaned\")\n    let _x = deep(50)\n}\nfn quick() -> int {\n    return 7\n}\nfn first() {\n    let h = go sleeper()\n    h.cancel()\n}\nfn second() {\n    let h = go quick()\n    print(h.join())\n}\n";
        let out = Out::default();
        let (mut interp, program) = loaded(src, Box::new(out.clone()));
        let mut fiber = Fiber::new();
        interp
            .call(&mut fiber, program.fn_by_name("first").unwrap(), vec![])
            .expect("first");
        interp.tick(&mut fiber, 0.1).expect("tick");
        assert_eq!(out.text(), "cleaned\n", "the cancelled coroutine unwound");
        assert_eq!(interp.stacks.live(), 0);
        assert_eq!(interp.coroutine_stacks_pooled(), 1);
        interp
            .call(&mut fiber, program.fn_by_name("second").unwrap(), vec![])
            .expect("second");
        assert_eq!(out.text(), "cleaned\n7\n");
        assert_eq!(
            interp.coroutine_stacks_allocated(),
            1,
            "the cancelled one's stack was reused"
        );
    }

    /// Deep recursion in a coroutine that runs on a reused stack (one that a deeper run already
    /// dirtied) still ends in the interpreter's "native stack budget" panic, run after run: the
    /// budget is measured from the stack's base, not from what the memory holds. (The hardware
    /// guard page is not exercised: touching it kills the process.)
    #[test]
    fn stack_overflow_is_reported_on_a_reused_stack() {
        use crate::Fiber;
        let src = "fn d(n: int) -> int {\n    if n == 0 {\n        return 0\n    }\n    return 1 + d(n - 1)\n}\nfn shallow() -> int {\n    return d(10)\n}\nfn main() {\n    let a = go shallow()\n    print(a.join())\n    let b = go d(100000)\n    print(b.join())\n}\n";
        let out = Out::default();
        let (mut interp, program) = loaded(src, Box::new(out.clone()));
        interp.core.sched.set_stack_size(256 * 1024);
        let mut fiber = Fiber::new();
        for round in 0..3 {
            let err = interp
                .call(&mut fiber, program.fn_by_name("main").unwrap(), vec![])
                .expect_err("the deep coroutine overflows");
            assert!(
                format!("{err:?}").contains("native stack budget"),
                "round {round}: {err:?}"
            );
            assert_eq!(
                interp.coroutine_stacks_allocated(),
                1,
                "round {round}: one stack throughout"
            );
        }
        assert_eq!(out.text(), "10\n".repeat(3));
    }

    /// Starts a coroutine of `size` bytes on a stack from `stacks`, runs it to its end by hand
    /// and returns it with its slot, like `resume_native_stack` before it recycles.
    fn finished_coroutine(
        stacks: &mut super::Stacks,
        stack: super::DefaultStack,
        size: usize,
        touch: usize,
    ) -> (u32, super::Co) {
        let co = super::Coroutine::with_stack(stack, move |_y: &super::Y, _r: super::Resume| {
            if touch > 0 {
                touch_stack(touch);
            }
            super::Finish::Cancelled
        });
        let slot = stacks.insert(Box::new(co), 0, size);
        let (mut co, _) = stacks.take(slot).expect("the stack");
        let r = co.resume(super::Resume {
            interp: std::ptr::null_mut(),
            cancel: false,
        });
        assert!(matches!(r, super::CoroutineResult::Return(_)));
        (slot, *co)
    }

    /// Writes to `levels` times 64 KiB of the native stack, one level per call.
    #[inline(never)]
    fn touch_stack(levels: usize) {
        if levels == 0 {
            return;
        }
        let mut buf = [0u8; 64 * 1024];
        buf.fill(levels as u8);
        std::hint::black_box(&mut buf);
        touch_stack(levels - 1);
        std::hint::black_box(&mut buf);
    }

    /// A changed stack size drops the pooled stacks of the old size.
    #[test]
    fn a_new_stack_size_drops_pooled_stacks_of_the_old_one() {
        let mut stacks = super::Stacks::default();
        let a = stacks.acquire(64 * 1024).expect("stack");
        assert_eq!(stacks.allocated, 1);
        let (slot, co) = finished_coroutine(&mut stacks, a, 64 * 1024, 0);
        stacks.recycle(slot, co);
        assert_eq!(stacks.pool.len(), 1);
        let _b = stacks.acquire(128 * 1024).expect("stack");
        assert_eq!(stacks.pool.len(), 0, "the old size was dropped");
        assert_eq!(stacks.allocated, 2);
    }

    /// A coroutine that was started at one size and ends after the size changed must not put
    /// its stack into the pool of the new size: the next start would get a stack smaller than
    /// its budget assumes (and run into the guard page).
    #[test]
    fn a_stack_of_the_old_size_that_ends_late_is_not_pooled() {
        use super::Stack;
        let mut stacks = super::Stacks::default();
        let small = stacks.acquire(64 * 1024).expect("stack");
        // The size changes while the small coroutine is alive: the next start asks for more.
        let big = stacks.acquire(8 << 20).expect("stack");
        let (slot, co) = finished_coroutine(&mut stacks, small, 64 * 1024, 0);
        stacks.recycle(slot, co);
        assert_eq!(
            stacks.pool.len(),
            0,
            "the small stack was unmapped, not pooled"
        );
        let (slot, co) = finished_coroutine(&mut stacks, big, 8 << 20, 0);
        stacks.recycle(slot, co);
        assert_eq!(stacks.pool.len(), 1);
        let again = stacks.acquire(8 << 20).expect("stack");
        assert!(again.base().get() - again.limit().get() >= 8 << 20);
    }

    /// A stack that a coroutine used about 10 MiB of gives those pages back when it is
    /// recycled, and keeps the hot top. Linux only: it counts resident pages with `mincore`,
    /// and only Linux's `MADV_DONTNEED` drops them at once (macOS's `MADV_FREE` is lazy).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_deep_stack_gives_its_pages_back_when_it_is_pooled() {
        use super::Stack;
        fn resident_pages(stack: &super::DefaultStack) -> usize {
            let len = stack.base().get() - stack.limit().get();
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let mut vec = vec![0u8; len.div_ceil(page)];
            // SAFETY: the range is the stack's own mapping, and `vec` has one byte per page.
            let rc = unsafe {
                libc::mincore(
                    stack.limit().get() as *mut libc::c_void,
                    len,
                    vec.as_mut_ptr(),
                )
            };
            assert_eq!(rc, 0);
            vec.iter().filter(|b| **b & 1 != 0).count() * page
        }
        let mut stacks = super::Stacks::default();
        let stack = stacks.acquire(16 << 20).expect("stack");
        let (slot, co) = finished_coroutine(&mut stacks, stack, 16 << 20, 160);
        let stack = co.into_stack();
        let deep = resident_pages(&stack);
        assert!(deep >= 9 << 20, "the run dirtied {deep} bytes");
        // Put it back through the real path: wrap it again in a finished coroutine.
        let (slot2, co) = finished_coroutine(&mut stacks, stack, 16 << 20, 0);
        stacks.recycle(slot2, co);
        stacks.free(slot);
        assert_eq!(stacks.pool.len(), 1);
        let kept = resident_pages(&stacks.pool[0]);
        assert!(
            kept <= super::STACK_HOT_WINDOW + (64 << 10),
            "{kept} bytes stayed resident of {deep}"
        );
    }

    /// Freeing a slot twice would hand it to two coroutines: debug builds stop it at the second
    /// `free`.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "freed twice")]
    fn freeing_a_slot_twice_is_caught() {
        let mut stacks = super::Stacks::default();
        let co = super::Coroutine::new(|_y: &super::Y, _r: super::Resume| super::Finish::Cancelled);
        let slot = stacks.insert(Box::new(co), 0, 0);
        drop(stacks.take(slot));
        stacks.free(slot);
        stacks.free(slot);
    }

    /// If the engine cannot find the stack of a suspended coroutine (`Resumed::Gone`, a broken
    /// invariant), the coroutine ends as a failure with an internal error instead of staying
    /// suspended: the next tick reports it, and `cancel_all_coroutines` terminates. The stack is
    /// taken out of the slab by the test (and forgotten, safely, without unwinding it).
    #[test]
    fn a_lost_stack_ends_its_coroutine_as_an_internal_error() {
        use crate::Fiber;
        let src = "fn worker() {\n    wait(1.0)\n    print(\"never\")\n}\nfn main() {\n    let _a = go worker()\n}\n";
        let out = Out::default();
        let (mut interp, program) = loaded(src, Box::new(out.clone()));
        let mut fiber = Fiber::new();
        interp
            .call(&mut fiber, program.fn_by_name("main").unwrap(), vec![])
            .expect("main");
        assert_eq!(interp.core.sched.coroutine_count(), 1);
        let lost = interp.stacks.take(0).expect("the worker's stack").0;
        std::mem::forget(lost);
        // If the lost coroutine stayed suspended, the cancel below would never end: a watchdog
        // turns that hang into a failure.
        let done = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&done);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(20));
            if flag.load(Ordering::SeqCst) == 0 {
                // Written to the real stderr: the harness captures `eprintln!` and the exit
                // below would discard it.
                let _ = std::io::Write::write_all(
                    &mut std::io::stderr(),
                    b"a_lost_stack_ends_its_coroutine_as_an_internal_error: the lost stack kept its coroutine alive, so cancel_all_coroutines hangs\n",
                );
                std::process::exit(1);
            }
        });
        let failure = interp
            .tick(&mut fiber, 2.0)
            .expect_err("the tick reports it");
        assert!(
            failure.message.contains("internal error"),
            "{}",
            failure.message
        );
        assert_eq!(
            interp.core.sched.coroutine_count(),
            0,
            "the coroutine is over"
        );
        assert_eq!(out.text(), "");
        interp.cancel_all_coroutines();
        done.store(1, Ordering::SeqCst);
        assert_eq!(
            interp.coroutine_stacks_pooled(),
            0,
            "a lost stack is forgotten, never pooled"
        );
    }

    /// Asking the scheduler to resume a coroutine that already finished (its entry is not yet
    /// compacted away) changes nothing: it does not reach the engine, which would report the
    /// stack as lost and turn the finished coroutine into a failure.
    #[test]
    fn resuming_a_finished_entry_does_nothing() {
        use crate::Fiber;
        let src =
            "fn quick() {\n    print(\"quick\")\n}\nfn main() {\n    let _a = go quick()\n}\n";
        let (mut interp, program) = loaded(src, Box::new(std::io::sink()));
        let mut fiber = Fiber::new();
        interp
            .call(&mut fiber, program.fn_by_name("main").unwrap(), vec![])
            .expect("main");
        // It ran to its end at its start, and the next tick has not removed its entry yet.
        assert_eq!(interp.stacks.live(), 0, "the coroutine finished");
        // The first coroutine has id 1.
        Sched::resume_coro(&mut interp, 1, false, true);
        assert!(interp.take_coroutine_failure().is_none());
        interp
            .tick(&mut fiber, 1.0)
            .expect("a later tick still works");
    }
}
