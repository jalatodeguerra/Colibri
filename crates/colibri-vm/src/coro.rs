//! Coroutines on heap frames (vm-plan §2.8, DESIGN §14.2).
//!
//! A coroutine is a [`VmFiber`]: a register file and a frame stack on the heap, kept in a slab
//! (`Machine::fibers`) while it is paused. The scheduler (`colibri_core::coro::Sched`, with its
//! data in `Core::sched`) resumes it through [`Engine::resume_stack`], which takes the fiber out
//! of its slot, runs the dispatch loop from the frames it left, and puts it back when a pausing
//! instruction ends the loop (`Exit::Suspend`) or frees it when the bottom frame returns. There
//! is no native stack to switch, and nothing here holds a reference into `Core` across a
//! resumption: the coroutine uses the machine through the same `&mut self`.
//!
//! A pausing instruction (`Pause`, `Join`, `SuspendReady`) checks `pause_context` (which also
//! refuses inside a host function, a `Drop` hook and native code that calls user code), asks the
//! scheduler to record the wait (`prepare_suspend`; an `Err` there is a cancellation) and ends the
//! loop with the frame's pc saved. A `wait` is over when it pauses (the resumption goes on with
//! the next instruction); a `join` runs again on resumption, and so looks at the handle once
//! more, which is what the tree-walker's loop does. A resumption with a cancel signal marks the
//! fiber cancelling and unwinds from the pausing instruction with `Ctl::Cancel`, so `defer`s and
//! drops run through the landing pads like for a panic.

use std::rc::Rc;

use colibri_check::hir::Callee;
use colibri_check::types::{COROUTINE_TYPE, Ty};
use colibri_core::coro::Sched;
use colibri_core::ctl::{Ctl, Eval};
use colibri_core::engine::{Engine, Resumed};
use colibri_core::heap::ClassObj;
use colibri_core::prelude::*;
use colibri_core::sched::*;
use colibri_core::stack_position;

use colibri_core::value::Value;
use colibri_syntax::Span;

use crate::bytecode::{Chunk, JoinKind, PauseKind, Site};
use crate::fiber::{CoEntry, CoStart, VmFiber};
use crate::machine::Machine;
use crate::run::Exit;

/// The refusal of a host call that can wait while a coroutine is alive (vm-plan step 12 lifts it).
pub const HOST_WAIT_REFUSAL: &str =
    "the bytecode VM does not support a host function that can wait, beside running coroutines yet";

/// How many finished fibers the pool keeps for the next starts (their stacks keep their
/// capacity). A server that starts a short coroutine per request has a few alive at a time.
const FIBER_POOL_CAP: usize = 16;

/// The fibers of the coroutines that have not ended: a slab of slots, each holding its fiber
/// while the coroutine is paused and nothing while it runs (the fiber is moved out for the
/// resumption, so a second resumption of a running coroutine finds an empty slot and reports
/// `Gone`). A finished coroutine's slot goes on the free list for the next start. The scheduler
/// keeps the slot index of each coroutine (an `EngineSlot`) and hands it back at every
/// resumption.
#[doc(hidden)]
#[derive(Default)]
pub struct FiberSlab {
    slots: Vec<Option<Box<VmFiber>>>,
    free: Vec<u32>,
    /// Fibers of coroutines that ran to their end, emptied, ready for the next start. Boxed
    /// so that a fiber moves in and out of the slab as a pointer, never by value.
    #[allow(clippy::vec_box)]
    pool: Vec<Box<VmFiber>>,
}

impl FiberSlab {
    /// An empty fiber for a new coroutine: a pooled one, or a new one.
    pub fn acquire(&mut self) -> Box<VmFiber> {
        self.pool.pop().unwrap_or_default()
    }

    /// Stores the fiber of a new coroutine and returns its slot.
    pub fn insert(&mut self, fiber: Box<VmFiber>) -> u32 {
        match self.free.pop() {
            Some(index) => {
                debug_assert!(
                    self.slots[index as usize].is_none(),
                    "a free slot holds a fiber"
                );
                self.slots[index as usize] = Some(fiber);
                index
            }
            None => {
                self.slots.push(Some(fiber));
                (self.slots.len() - 1) as u32
            }
        }
    }

    /// Moves the fiber out of `slot` for a resumption; `None` if it is not there (the coroutine
    /// is running, or ended).
    #[inline]
    pub fn take(&mut self, slot: u32) -> Option<Box<VmFiber>> {
        self.slots.get_mut(slot as usize)?.take()
    }

    /// Puts the fiber of a coroutine that paused back into its slot.
    #[inline]
    pub fn put_back(&mut self, slot: u32, fiber: Box<VmFiber>) {
        let place = &mut self.slots[slot as usize];
        debug_assert!(place.is_none(), "a slot got a second fiber");
        *place = Some(fiber);
    }

    /// The coroutine of `slot` ran to its end and its fiber is gone: the slot is free. In debug
    /// builds a slot that still holds a fiber, or that is already free, is a bug here.
    #[inline]
    pub fn free(&mut self, slot: u32) {
        debug_assert!(
            self.slots[slot as usize].is_none(),
            "a fiber slot was freed while it still holds a fiber"
        );
        debug_assert!(!self.free.contains(&slot), "a fiber slot was freed twice");
        self.free.push(slot);
    }

    /// The coroutine of `slot` ran to its end: its emptied fiber goes into the pool (unless the
    /// pool is full) and the slot is free.
    pub fn recycle(&mut self, slot: u32, mut fiber: Box<VmFiber>) {
        if self.pool.len() < FIBER_POOL_CAP {
            fiber.reset();
            self.pool.push(fiber);
        }
        self.free(slot);
    }

    /// How many slots are in use: started and not yet freed, whether the fiber is stored or out
    /// for a resumption.
    pub fn live(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    /// How many slots the slab has made, in use or not.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// How many emptied fibers wait for the next starts.
    pub fn pooled(&self) -> usize {
        self.pool.len()
    }

    /// Whether no fiber is stored (none paused, none left over).
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// Drops every stored fiber and empties the slab. What a fiber still holds is plain data
    /// here (nothing runs): this is for a machine that is going away.
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.free.clear();
        self.pool.clear();
    }
}

impl Machine {
    /// How many coroutine slots are in use (for tests of the slab).
    #[doc(hidden)]
    pub fn coroutine_slots_live(&self) -> usize {
        self.fibers.live()
    }

    /// How many coroutine slots exist (for tests of the slab).
    #[doc(hidden)]
    pub fn coroutine_slots_made(&self) -> usize {
        self.fibers.capacity()
    }

    /// How many emptied fibers wait in the pool (for tests of the slab).
    #[doc(hidden)]
    pub fn coroutine_fibers_pooled(&self) -> usize {
        self.fibers.pooled()
    }

    /// Takes the fiber of slot `slot` out of the slab and forgets it (`mem::forget`: whatever it
    /// holds is leaked on purpose), as a broken engine would lose it (for the test of
    /// `Resumed::Gone`).
    #[doc(hidden)]
    pub fn lose_coroutine_fiber(&mut self, slot: u32) -> bool {
        match self.fibers.take(slot) {
            Some(fiber) => {
                std::mem::forget(fiber);
                true
            }
            None => false,
        }
    }

    /// Records where the native stack stands when the host (or the event loop) calls in with no
    /// coroutine running: coroutines resumed from here measure their native recursion from it.
    #[inline]
    pub(crate) fn note_stack_root(&mut self) {
        if !self.core.sched.coroutine_running() {
            self.stack_root = stack_position();
        }
    }

    // ---- starting ----

    /// `Go`: evaluates nothing (the values are in the window), then runs the call on a new
    /// coroutine until it first pauses.
    #[inline(never)]
    pub(crate) fn op_go(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [d, a, site]: [u16; 3],
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let Site::Go(g) = &chunk.sites[usize::from(site)] else {
            return Err(self.internal(fiber, span, "`go` without its site"));
        };
        let from = base + usize::from(a);
        let mut values: Vec<Value> = fiber.regs[from..from + usize::from(g.argc)].to_vec();
        let group_value = g.grouped.then(|| values.remove(0));
        let group = group_value.as_ref().and_then(group_id);
        let name = match &g.callee {
            Callee::Fn(id) => self.fns.get(id.0 as usize).map(|s| s.body.name.clone()),
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
        let started = self.start_coroutine(
            fiber,
            g.callee.clone(),
            g.type_args.clone(),
            values,
            name,
            group,
            span,
        );
        if let Some(v) = group_value {
            self.release(v);
        }
        Self::put_reg(fiber, base + usize::from(d), started?);
        Ok(())
    }

    /// Makes the coroutine of a `go`: its handle, its fiber in the slab, its entry in the
    /// scheduler; the first run shares the starter's instruction budget.
    #[allow(clippy::too_many_arguments)]
    fn start_coroutine(
        &mut self,
        fiber: &mut VmFiber,
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
        // A first run happens on the starter's native stack, so a chain of coroutines that each
        // start the next one before pausing is bounded like any other native recursion.
        if stack_position().abs_diff(fiber.stack_base) > self.native_stack_budget {
            self.release_all(values);
            let message = format!(
                "stack overflow: the native stack budget of {} bytes is used up",
                self.native_stack_budget
            );
            return Err(self.panic(fiber, span, message));
        }
        let handle = ClassObj::new(COROUTINE_TYPE, fields);
        let mut co = self.fibers.acquire();
        co.state.coro = Some(id);
        co.start = Some(Box::new(CoStart {
            callee,
            type_args,
            values,
            span,
        }));
        let slot = self.fibers.insert(co);
        self.core
            .sched
            .register(id, Rc::downgrade(&handle), group, EngineSlot(slot));
        // The first run shares the starter's instruction budget: it is part of the starter's
        // own execution.
        self.resume_coro(id, false, false);
        self.core.sched.compact_if_crowded();
        // The new coroutine met a refusal: the program stops here, at its `go`.
        if self.pending_refusal.is_some() {
            self.release(Value::Obj(handle));
            return self.settle_refusal(fiber, span, Ok(Value::Unit));
        }
        Ok(Value::Obj(handle))
    }

    // ---- resuming ----

    /// [`Engine::resume_stack`]: runs the fiber of `slot` until it pauses or ends.
    pub(crate) fn resume_fiber(&mut self, id: u64, slot: EngineSlot, cancel: bool) -> Resumed {
        // The fiber leaves the slab for the resumption: the slot is empty while the coroutine
        // runs, so a second resumption of it (which the scheduler does not attempt: it resumes
        // suspended coroutines only) would find nothing and report `Gone`. Nested starts may grow
        // the slab meanwhile; the slot index stays valid.
        let Some(mut fiber) = self.fibers.take(slot.0) else {
            return Resumed::Gone;
        };
        debug_assert_eq!(
            fiber.state.coro,
            Some(id),
            "the slot holds another coroutine"
        );
        fiber.stack_base = self.stack_root;
        let outcome = self.run_coroutine(&mut fiber, cancel);
        match outcome {
            Resumed::Suspended => self.fibers.put_back(slot.0, fiber),
            _ => self.fibers.recycle(slot.0, fiber),
        }
        outcome
    }

    /// Runs the coroutine from where it stands: its first call, the instruction after a pause
    /// (or the pausing instruction itself, for a `join`), or the unwinding of a cancel.
    fn run_coroutine(&mut self, fiber: &mut VmFiber, cancel: bool) -> Resumed {
        let result: Result<Exit, Ctl> = if let Some(start) = fiber.start.take() {
            if cancel {
                // Cancelled before it ran: nothing of it started. After a refusal nothing
                // runs, hooks included: the arguments are leaked, as the refusal does.
                if self.refused {
                    std::mem::forget(start.values);
                } else {
                    self.release_all(start.values);
                }
                Err(Ctl::Cancel)
            } else {
                match self.begin_coroutine(fiber, *start) {
                    Ok(None) => self.exec(fiber),
                    Ok(Some(v)) => Ok(Exit::Return(v)),
                    Err(ctl) => Err(ctl),
                }
            }
        } else if cancel {
            fiber.state.cancelling = true;
            // The pause the coroutine is in is where the cancellation starts: its frame's pc is
            // after that instruction, like for any failure of an instruction.
            match self.catch(fiber, Ctl::Cancel, false) {
                Ok(()) => self.exec(fiber),
                Err(ctl) => Err(ctl),
            }
        } else {
            if fiber.redo
                && let Some(top) = fiber.frames.last_mut()
            {
                top.pc -= 1;
            }
            self.exec(fiber)
        };
        let finish = match result {
            Ok(Exit::Suspend) => return Resumed::Suspended,
            Ok(Exit::Return(v)) => self.end_coroutine(fiber, Ok(v)),
            Err(ctl) => self.end_coroutine(fiber, Err(ctl)),
        };
        // A refusal ends the program, not just this coroutine: whatever resumed it raises it.
        if let Finish::Panicked(info) = &finish
            && info.message.starts_with(crate::unwind::REFUSAL)
            && let Some(id) = fiber.state.coro
        {
            self.pending_refusal = Some((id, info.message.clone()));
        }
        Resumed::Finished(finish)
    }

    /// What resumed coroutines gives back to the code that resumed them (`go`, a wait or a join
    /// on the main fiber, `task.tick`): when a coroutine ended with a refusal meanwhile, the
    /// refusal is raised on this fiber, at this instruction, with this fiber's trace, and
    /// `result` (whatever it was) is dropped. A refusal runs no pads, so the program stops here.
    pub(crate) fn settle_refusal<T>(
        &mut self,
        fiber: &mut VmFiber,
        span: Span,
        result: Result<T, Ctl>,
    ) -> Result<T, Ctl> {
        let Some((id, message)) = self.pending_refusal.take() else {
            return result;
        };
        // The coroutine's own failure is this refusal: it is not reported a second time.
        self.core.sched.forget_failure_of(id);
        Err(self.panic(fiber, span, message))
    }

    /// Calls what `go` named on the new fiber. `Ok(None)`: a frame is pushed, and the loop runs
    /// it. `Ok(Some(v))`: the call was over at once (a host function, a derived method).
    fn begin_coroutine(
        &mut self,
        fiber: &mut VmFiber,
        start: CoStart,
    ) -> Result<Option<Value>, Ctl> {
        let CoStart {
            callee,
            type_args,
            mut values,
            span,
        } = start;
        match callee {
            Callee::Fn(id) => self.begin_fn(fiber, id, values, None, None, span),
            Callee::Host(id) => self
                .call_host(fiber, id, &type_args, values, span)
                .map(Some),
            Callee::Value => {
                if values.is_empty() {
                    return Err(self.internal(fiber, span, "a call without a function"));
                }
                let f = values.remove(0);
                match &f {
                    Value::Fn(id, _) => {
                        let id = *id;
                        self.begin_fn(fiber, id, values, None, Some(f), span)
                    }
                    Value::Closure(c) => {
                        let c = Rc::clone(c);
                        match self.closure_chunk(&c) {
                            Ok(chunk) => self.begin_chunk(fiber, chunk, values, Some(&c), Some(f)),
                            Err(msg) => {
                                self.release_all(values);
                                self.release(f);
                                Err(self.internal(fiber, span, &msg))
                            }
                        }
                    }
                    _ => {
                        self.release_all(values);
                        self.release(f);
                        Err(self.internal(
                            fiber,
                            span,
                            "a call of something that is not a function",
                        ))
                    }
                }
            }
            Callee::Bound { iface, method } | Callee::Dyn { iface, method } => {
                let Some(recv) = values.first().cloned() else {
                    return Err(self.internal(fiber, span, "an interface call without a receiver"));
                };
                let (impl_id, inner) = match &recv {
                    Value::Dyn(d) => (Some(d.impl_id), d.val.clone()),
                    other => (self.impl_for(other, iface), other.clone()),
                };
                values[0] = inner;
                drop(recv);
                match self.target_of(impl_id, method) {
                    Some(f) => self.begin_fn(fiber, f, values, None, None, span),
                    None => self.derived_call(fiber, iface, values, span).map(Some),
                }
            }
            Callee::Intrinsic(_) => {
                self.release_all(values);
                Err(self.internal(fiber, span, "an intrinsic is not a call"))
            }
        }
    }

    fn begin_fn(
        &mut self,
        fiber: &mut VmFiber,
        f: colibri_check::hir::FnId,
        values: Vec<Value>,
        closure: Option<&colibri_core::closure::ClosureObj>,
        keep: Option<Value>,
        span: Span,
    ) -> Result<Option<Value>, Ctl> {
        match self.chunk_of(f) {
            Some(Ok(chunk)) => self.begin_chunk(fiber, chunk, values, closure, keep),
            other => {
                self.release_all(values);
                if let Some(k) = keep {
                    self.release(k);
                }
                let message = match other {
                    Some(Err(msg)) => format!("cannot compile: {msg}"),
                    _ => "unknown function".to_string(),
                };
                Err(self.internal(fiber, span, &message))
            }
        }
    }

    /// Pushes the entry frame of the coroutine over its arguments. `keep` is the function value
    /// that was called, released after the call as the tree-walker does.
    fn begin_chunk(
        &mut self,
        fiber: &mut VmFiber,
        chunk: Rc<Chunk>,
        values: Vec<Value>,
        closure: Option<&colibri_core::closure::ClosureObj>,
        keep: Option<Value>,
    ) -> Result<Option<Value>, Ctl> {
        let nparams = chunk.nparams as usize;
        match self.push_entry(fiber, chunk, values, closure) {
            Ok(base) => {
                debug_assert_eq!(base, 0, "a coroutine's entry frame starts its fiber");
                fiber.entry = Some(CoEntry {
                    nparams,
                    callee: keep,
                });
                Ok(None)
            }
            Err((result, _)) => {
                if let Some(k) = keep {
                    self.release(k);
                }
                match result {
                    Err(ctl) => Err(ctl),
                    Ok(v) => Ok(Some(v)),
                }
            }
        }
    }

    /// The coroutine ended (it returned, panicked or was cancelled): releases what its entry
    /// owns and folds the panics of `Drop` hooks that ran meanwhile into how it ended.
    fn end_coroutine(&mut self, fiber: &mut VmFiber, result: Eval) -> Finish {
        // Landing pads release their holds. Only a refusal, which runs no pads, leaves any.
        debug_assert!(
            fiber.holds.is_empty() || self.refused,
            "a coroutine ended with a mutation guard held"
        );
        fiber.holds.clear();
        fiber.unwinding.clear();
        let result = match fiber.entry.take() {
            Some(CoEntry { nparams, callee }) => {
                let (result, _) = self.finish_entry(fiber, 0, nparams, false, result);
                if let Some(f) = callee {
                    self.release(f);
                }
                result
            }
            None => result,
        };
        fiber.frames.clear();
        self.release_temps(fiber, 0);
        // The last releases of this coroutine (its closure and what it captured, its
        // arguments) may have run `Drop` hooks: their panics are this coroutine's failure, not
        // the next fiber's or the host's.
        let result = self.fold_hook_panics(result);
        match result {
            Ok(v) | Err(Ctl::Return(v)) => Finish::Done(v),
            Err(Ctl::Cancel) => Finish::Cancelled,
            Err(Ctl::Panic(info)) => Finish::Panicked(info),
            Err(Ctl::Break(_, v)) => {
                self.release(v);
                Finish::Cancelled
            }
            Err(Ctl::Continue(_)) => Finish::Cancelled,
        }
    }

    // ---- pausing ----

    /// Pauses the running coroutine until `wake`. `Err(Cancel)` when it is cancelled (no pause
    /// happens). `redo`: the instruction runs again when the coroutine is resumed. The caller
    /// then ends the dispatch loop with `Exit::Suspend`.
    fn suspend(
        &mut self,
        fiber: &mut VmFiber,
        id: u64,
        wake: Wake,
        redo: bool,
        span: Span,
    ) -> Result<(), Ctl> {
        if fiber.entries != 1 {
            // `pause_context` refuses every native caller the VM knows of; a nested loop it did
            // not know of would be lost by a pause.
            return Err(self.internal(fiber, span, "a coroutine paused inside a native call"));
        }
        if self
            .core
            .sched
            .prepare_suspend(id, &mut fiber.state, wake)
            .is_err()
        {
            return Err(Ctl::Cancel);
        }
        fiber.redo = redo;
        Ok(())
    }

    /// `SuspendReady`, the pause of the loop of `wait_until`: due at the next tick.
    #[inline(never)]
    pub(crate) fn op_suspend_ready(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        pc: usize,
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let Some(id) = fiber.state.coro else {
            return Err(self.internal(
                fiber,
                span,
                "a pause reached a fiber that is not a coroutine",
            ));
        };
        self.suspend(fiber, id, Wake::Ready, false, span)
    }

    /// `Pause`: `wait`, `wait_real` and `wait_for`. `Ok(true)`: the coroutine paused, and the
    /// loop must end; `Ok(false)`: it is done (the main fiber's `wait` ran the event loop).
    #[inline(never)]
    pub(crate) fn op_pause(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        a: u16,
        kind: PauseKind,
    ) -> Result<bool, Ctl> {
        let span = chunk.spans[pc - 1];
        let name = match kind {
            PauseKind::Wait => "wait",
            PauseKind::WaitReal => "wait_real",
            PauseKind::WaitFor => "wait_for",
        };
        let at = base + usize::from(a);
        if kind == PauseKind::Wait && fiber.state.coro.is_none() {
            let Value::Float(s) = fiber.regs[at] else {
                return Err(self.internal(fiber, span, "ill-typed wait"));
            };
            if s.is_nan() {
                return Err(self.panic(fiber, span, "`wait` needs a number of seconds, not NaN"));
            }
            let waited = self.wait_on_main(fiber, s.max(0.0), span);
            self.settle_refusal(fiber, span, waited)?;
            return Ok(false);
        }
        let id = self.pause_context(fiber, span, name)?;
        let wake = match (kind, &fiber.regs[at]) {
            (PauseKind::Wait | PauseKind::WaitReal, Value::Float(s)) => {
                let s = *s;
                if s.is_nan() {
                    return Err(self.panic(
                        fiber,
                        span,
                        format!("`{name}` needs a number of seconds, not NaN"),
                    ));
                }
                let s = s.max(0.0);
                if kind == PauseKind::Wait {
                    Wake::Time(self.core.sched.time() + s)
                } else {
                    Wake::Real(self.core.sched.clock_now() + s)
                }
            }
            // The signal stays in its register while this coroutine waits on it.
            (PauseKind::WaitFor, v) => match signal_id(v) {
                Some(sid) => Wake::Signal(sid),
                None => return Err(self.internal(fiber, span, "ill-typed wait_for")),
            },
            _ => return Err(self.internal(fiber, span, "ill-typed wait")),
        };
        self.suspend(fiber, id, wake, false, span)?;
        Ok(true)
    }

    /// `Join`: `join`, `join_result` and `join_all`. `Ok(true)`: the coroutine paused, and the
    /// loop must end (the instruction runs again when it is resumed); `Ok(false)`: the result
    /// is in `d`.
    #[inline(never)]
    pub(crate) fn op_join(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [d, a]: [u16; 2],
        kind: JoinKind,
    ) -> Result<bool, Ctl> {
        let span = chunk.spans[pc - 1];
        let recv = fiber.regs[base + usize::from(a)].clone();
        let result = if kind == JoinKind::JoinAll {
            self.join_all(fiber, &recv, span)
        } else {
            self.join_coroutine(fiber, &recv, span, kind == JoinKind::JoinResult)
        };
        // `recv` is a clone of a handle the register still holds: never the last owner.
        drop(recv);
        match result? {
            Some(v) => {
                Self::put_reg(fiber, base + usize::from(d), v);
                Ok(false)
            }
            None => Ok(true),
        }
    }

    /// Waits until the coroutine of the handle has ended, then `joined_outcome`. `None`: paused.
    fn join_coroutine(
        &mut self,
        fiber: &mut VmFiber,
        recv: &Value,
        span: Span,
        as_result: bool,
    ) -> Result<Option<Value>, Ctl> {
        let what = if as_result { "join_result" } else { "join" };
        let Some(h) = handle_of(recv) else {
            return Err(self.internal(fiber, span, "ill-typed coroutine method"));
        };
        let h = Rc::clone(h);
        let mut status = handle_status(&h);
        if status == RUNNING && fiber.state.coro.is_none() {
            let target = Rc::clone(&h);
            let driven = self.drive_main(fiber, span, what, None, &mut |_| {
                handle_status(&target) != RUNNING
            });
            self.settle_refusal(fiber, span, driven)?;
            status = handle_status(&h);
        }
        if status == RUNNING {
            let id = self.pause_context(fiber, span, what)?;
            if handle_id(&h) == id {
                return Err(self.panic(fiber, span, "a coroutine cannot join itself"));
            }
            // Registered with the target: woken when it ends, never polled.
            let target = handle_id(&h);
            let wake = if as_result {
                Wake::JoinResult(target)
            } else {
                Wake::Join(target)
            };
            self.suspend(fiber, id, wake, true, span)?;
            return Ok(None);
        }
        self.joined_outcome(fiber, &h, status, span, as_result)
            .map(Some)
    }

    /// `join_all` on a `CoGroup`: pauses until no other member of the group is alive.
    fn join_all(
        &mut self,
        fiber: &mut VmFiber,
        recv: &Value,
        span: Span,
    ) -> Result<Option<Value>, Ctl> {
        let Some(g) = group_id(recv) else {
            return Err(self.internal(fiber, span, "ill-typed CoGroup method"));
        };
        let me = fiber.state.coro;
        if !self.core.sched.group_has_others(g, me) {
            return Ok(Some(Value::Unit));
        }
        if me.is_none() {
            let driven = self.drive_main(fiber, span, "join_all", None, &mut |it| {
                !it.core.sched.group_has_others(g, me)
            });
            self.settle_refusal(fiber, span, driven)?;
            return Ok(Some(Value::Unit));
        }
        let id = self.pause_context(fiber, span, "join_all")?;
        self.suspend(fiber, id, Wake::Group(g), true, span)?;
        Ok(None)
    }

    // ---- the intrinsics that do not pause ----

    /// `task.tick(dt)`.
    pub(crate) fn task_tick(&mut self, fiber: &mut VmFiber, dt: f64, span: Span) -> Eval {
        if self.core.sched.is_busy() {
            let message = if fiber.state.coro.is_some() {
                "`task.tick` cannot be called from a coroutine"
            } else {
                "`task.tick` cannot be called from inside a tick"
            };
            return Err(self.panic(fiber, span, message));
        }
        let ticked = match self.tick_core(dt) {
            Ok(()) => Ok(Value::Unit),
            Err(TickError::Failure(info)) => Err(Ctl::Panic(info)),
            Err(TickError::Reentrant) => Err(self.panic(fiber, span, "`task.tick` is busy")),
        };
        self.settle_refusal(fiber, span, ticked)
    }

    /// `CoGroup()` and `Signal()`: a new id in an object of the type.
    pub(crate) fn new_scheduler_object(&mut self, ty: colibri_check::types::TypeId) -> Value {
        let id = self.core.sched.alloc_id();
        Value::Obj(ClassObj::new(ty, vec![Value::Int(id as i64)]))
    }
}

/// Cancels every live coroutine: see [`Sched::cancel_all_coroutines`]. Afterwards every one ran
/// to its end, which is when its fiber is freed.
impl Machine {
    pub fn cancel_all_coroutines(&mut self) {
        let was_running = self.core.sched.coroutine_running();
        self.note_stack_root();
        Sched::cancel_all_coroutines(self);
        // Every suspended coroutine was resumed with a cancel and ran to its end, which frees
        // its fiber, so none is left. (When this call did nothing because a coroutine is
        // running, the slab is in use and untouched.)
        debug_assert!(
            was_running || self.fibers.is_empty(),
            "a coroutine fiber outlived `cancel_all_coroutines`"
        );
    }
}
