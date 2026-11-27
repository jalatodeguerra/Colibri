//! `Channel[T]` of thread tasks (DESIGN §14.1, ARCHITECTURE §36.5): the engine's half.
//!
//! The queue, its waiters and their wake-ups are [`colibri_core::chan`]; this is what needs the
//! machine: copying a value to and from its portable form on a walk stack, and waiting.
//!
//! A `send` or `recv` that finds nothing to do registers a waiter in the channel (a `Completer`
//! of this isolate's scheduler) and waits for it the way a `join` waits for a task
//! (`tasks.rs`):
//!
//! - a coroutine parks its heap fiber on `Wake::External(op)`; the instruction runs again when it
//!   is resumed, finds its waiter's outcome in the channel and goes on. The coroutine's entry in
//!   the scheduler (`ChanWait`) is what a cancel needs to take the waiter out of the channel;
//! - the main fiber runs the event loop until the waiter is woken (with the scope of an inline
//!   task, if it runs in one: the loop sees only the coroutines of that scope);
//! - where neither is allowed (a `Drop` hook, a host callback, native code that calls user
//!   code, shutdown) the thread blocks on its completions. A worker that is cancelled while it
//!   waits stops with the unwinding of an exhausted budget.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use colibri_check::hir::Builtin;
use colibri_core::chan::{ChanInner, Done, Received, Sent, Ticket};
use colibri_core::ctl::{Ctl, Eval};
use colibri_core::portable::{Portable, RebuildError, Refused, from_portable};
use colibri_core::prelude::*;
use colibri_core::sched::{ChanWait, Wake};
use colibri_core::value::Value;
use colibri_syntax::Span;

use crate::bytecode::{ChanKind, Chunk};
use crate::fiber::VmFiber;
use crate::machine::Machine;
use crate::stacks::{WalkRole, on_walk_stack};

/// What a receive came to.
pub(crate) enum Recv {
    Item(Value),
    /// The channel is closed and empty.
    End,
    /// The coroutine parked; the instruction runs again when it is resumed.
    Parked,
}

/// How often a thread that blocks looks at whether it was cancelled.
const BLOCK_POLL: Duration = Duration::from_millis(50);

/// How long the event loop of a worker sleeps at most: it looks at the pool's cancel flag
/// between sleeps (the main `Vm` has no flag and sleeps until a completion comes).
pub(crate) const DRIVE_POLL: f64 = 0.05;

impl Machine {
    /// `Channel[T]()` and `Channel[T](capacity: n)`.
    pub(crate) fn new_channel(
        &mut self,
        fiber: &mut VmFiber,
        args: Range<usize>,
        span: Span,
    ) -> Eval {
        let capacity = if args.is_empty() {
            None
        } else {
            match fiber.regs[args.start] {
                Value::Int(n) if n >= 1 => Some(usize::try_from(n).unwrap_or(usize::MAX)),
                Value::Int(n) => {
                    return Err(self.panic(
                        fiber,
                        span,
                        format!("a channel needs a capacity of at least 1, not {n}"),
                    ));
                }
                _ => return Err(self.internal(fiber, span, "ill-typed channel capacity")),
            }
        };
        Ok(self.core.new_channel(capacity))
    }

    /// `Op::Chan`: `send` and `recv`. `Ok(true)`: the coroutine parked, and the dispatch loop must
    /// end (the instruction runs again when it is resumed); `Ok(false)`: the result is in `d`.
    #[inline(never)]
    pub(crate) fn op_chan(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [d, a]: [u16; 2],
        kind: ChanKind,
    ) -> Result<bool, Ctl> {
        let span = chunk.spans[pc - 1];
        let from = base + usize::from(a);
        // Clones of what the registers still hold: never the last owner, so dropping them is fine.
        let channel = fiber.regs[from].clone();
        let result = match kind {
            ChanKind::Recv => match self.chan_recv(fiber, &channel, true, span)? {
                Recv::Item(v) => Some(self.core.option(Some(v))),
                Recv::End => Some(self.core.option(None)),
                Recv::Parked => None,
            },
            ChanKind::Send | ChanKind::SendClone => {
                let value = fiber.regs[from + 1].clone();
                let sent = self.chan_send(
                    fiber,
                    &channel,
                    &value,
                    kind == ChanKind::SendClone,
                    true,
                    span,
                );
                drop(value);
                sent?.then_some(Value::Unit)
            }
        };
        drop(channel);
        match result {
            Some(v) => {
                Self::put_reg(fiber, base + usize::from(d), v);
                Ok(false)
            }
            None => Ok(true),
        }
    }

    /// `ForIterNext` over a channel's iterator: `Ok(Some(true))` an item went to `d`,
    /// `Ok(Some(false))` the channel is closed and empty (the iterator is let go of),
    /// `Ok(None)` the coroutine parked.
    #[inline(never)]
    pub(crate) fn op_for_channel(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [d, a]: [u16; 2],
        channel: &Value,
    ) -> Result<Option<bool>, Ctl> {
        let span = chunk.spans[pc - 1];
        match self.chan_recv(fiber, channel, true, span)? {
            Recv::Parked => Ok(None),
            Recv::Item(item) => {
                if !chunk.hidden
                    && let Err(ctl) = self.step(fiber, span)
                {
                    self.release(item);
                    return Err(ctl);
                }
                Self::put_reg(fiber, base + usize::from(d), item);
                Ok(Some(true))
            }
            Recv::End => {
                // Out of values: the iterator lets go of its channel, as a finished one does.
                let it = fiber.regs[base + usize::from(a)].clone();
                let obj = match &it {
                    Value::Iter(o) => Some(Rc::clone(o)),
                    Value::Dyn(dv) => match &dv.val {
                        Value::Iter(o) => Some(Rc::clone(o)),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(obj) = obj {
                    self.release_all(obj.take_inner());
                }
                Ok(Some(false))
            }
        }
    }

    /// The handle of the channel an iterator in a register receives from, if it is a channel's.
    pub(crate) fn channel_of_iter(v: &Value) -> Option<Value> {
        match v {
            Value::Iter(o) => o.channel(),
            Value::Dyn(dv) => match &dv.val {
                Value::Iter(o) => o.channel(),
                _ => None,
            },
            _ => None,
        }
    }

    /// A receive. With `may_park`, a coroutine that has to wait parks (`Recv::Parked`); without,
    /// every wait is in place.
    pub(crate) fn chan_recv(
        &mut self,
        fiber: &mut VmFiber,
        channel: &Value,
        may_park: bool,
        span: Span,
    ) -> Result<Recv, Ctl> {
        let Some(chan) = self.core.chan_of(channel) else {
            return Err(self.internal(fiber, span, "ill-typed channel method"));
        };
        // Back from a park: what the waiter was woken for.
        let done = match self.woken(fiber) {
            Some(done) => done,
            None => {
                let received = chan.recv(&mut || self.core.sched.begin_completion::<()>().0);
                match received {
                    Received::Item(p) => Done::Got(p),
                    Received::End => Done::Closed,
                    Received::Wait(ticket) => {
                        if self.chan_wait(fiber, &chan, &ticket, false, may_park, span)? {
                            return Ok(Recv::Parked);
                        }
                        let Some(done) = chan.collect(ticket.id) else {
                            return Err(self.internal(
                                fiber,
                                span,
                                "a receiver was woken for nothing",
                            ));
                        };
                        done
                    }
                }
            }
        };
        match done {
            Done::Got(p) => self.rebuild(fiber, p, span).map(Recv::Item),
            Done::Closed => Ok(Recv::End),
            Done::Sent => Err(self.internal(fiber, span, "a receiver was told it had sent")),
        }
    }

    /// A send. `clone`: the value is the operand of a `.clone()` that was left out. `Ok(true)` when
    /// it is sent, `Ok(false)` when the coroutine parked.
    pub(crate) fn chan_send(
        &mut self,
        fiber: &mut VmFiber,
        channel: &Value,
        value: &Value,
        clone: bool,
        may_park: bool,
        span: Span,
    ) -> Result<bool, Ctl> {
        let Some(chan) = self.core.chan_of(channel) else {
            return Err(self.internal(fiber, span, "ill-typed channel method"));
        };
        // Back from a park: the value is in the channel (or the channel was closed).
        match self.woken(fiber) {
            Some(Done::Sent) => return Ok(true),
            Some(Done::Closed) => return Err(self.closed(fiber, span)),
            Some(Done::Got(_)) => {
                return Err(self.internal(fiber, span, "a sender was handed a value"));
            }
            None => {}
        }
        let portable = self.copy_for_send(fiber, value, clone, span)?;
        match chan.send(portable, &mut || self.core.sched.begin_completion::<()>().0) {
            Sent::Done => Ok(true),
            Sent::Closed => Err(self.closed(fiber, span)),
            Sent::Wait(ticket) => {
                if self.chan_wait(fiber, &chan, &ticket, true, may_park, span)? {
                    return Ok(false);
                }
                match chan.collect(ticket.id) {
                    Some(Done::Sent) => Ok(true),
                    Some(Done::Closed) => Err(self.closed(fiber, span)),
                    _ => Err(self.internal(
                        fiber,
                        span,
                        "a sender was woken for something it did not wait for",
                    )),
                }
            }
        }
    }

    fn closed(&mut self, fiber: &mut VmFiber, span: Span) -> Ctl {
        self.panic(fiber, span, "cannot send on a closed channel")
    }

    /// What the coroutine of `fiber` was woken for, if it was parked in a channel operation (its
    /// instruction is running again). The operation's entry in the scheduler is finished.
    fn woken(&mut self, fiber: &VmFiber) -> Option<Done> {
        let id = fiber.state.coro?;
        let wait = self.core.sched.take_chan_wait(id)?;
        self.core.sched.ext().finish(wait.op);
        let done = wait.chan.collect(wait.waiter);
        if done.is_none() {
            // Resumed without being woken (nothing does that today): leave the channel.
            wait.token.cancel();
        }
        done
    }

    /// The portable form of a value to send: copied on a walk stack (a part that cannot cross, or
    /// a copy that would pass the memory limit, is a panic of the sender). With `clone`, the value
    /// is the operand of the `.clone()` that the program wrote: a user `impl Clone` anywhere in
    /// the program could run in that clone, and a `Drop` hook would run on the temporary clone, so
    /// neither must be skipped: then the clone is made. Without either, the clone is a structural
    /// deep copy that nothing can observe, and so is the walk (the second copy is left out).
    fn copy_for_send(
        &mut self,
        fiber: &mut VmFiber,
        value: &Value,
        clone: bool,
        span: Span,
    ) -> Result<Portable, Ctl> {
        let cloned = if clone
            && (self.core.dispatch.has_user_clone() || self.core.dispatch.has_user_drop())
        {
            Some(self.clone_deep(fiber, value, span)?)
        } else {
            None
        };
        let budget = self.native_stack_budget;
        let source = cloned.as_ref().unwrap_or(value);
        let copied = on_walk_stack(WalkRole::Sender, budget, |guard| {
            self.to_portable(source, guard)
        });
        if let Some(c) = cloned {
            self.release(c);
        }
        match copied {
            Ok(p) => Ok(p),
            Err(e) if e.refused == Refused::MemoryLimit => Err(self.memory_exceeded(fiber, span)),
            Err(e) => Err(self.panic(fiber, span, e.to_string())),
        }
    }

    /// A received value, rebuilt on the receiver's walk stack.
    fn rebuild(&mut self, fiber: &mut VmFiber, p: Portable, span: Span) -> Result<Value, Ctl> {
        let budget = self.native_stack_budget;
        let rebuilt = on_walk_stack(WalkRole::Receiver, budget, |guard| {
            from_portable(p, &mut self.core, guard)
        });
        match rebuilt {
            Ok(v) => Ok(v),
            Err(RebuildError::MemoryLimit) => Err(self.memory_exceeded(fiber, span)),
            Err(e) => Err(self.panic(
                fiber,
                span,
                format!("a value received from a channel cannot be rebuilt: {e}"),
            )),
        }
    }

    /// Waits for the waiter `ticket` of `chan`. `Ok(true)`: the coroutine parked (the caller ends
    /// its instruction); `Ok(false)`: the waiter was woken, and its outcome is in the channel.
    /// On every `Err` the waiter has left the channel.
    fn chan_wait(
        &mut self,
        fiber: &mut VmFiber,
        chan: &Arc<ChanInner>,
        ticket: &Ticket,
        sending: bool,
        may_park: bool,
        span: Span,
    ) -> Result<bool, Ctl> {
        let what = if sending { "send" } else { "recv" };
        let state = &fiber.state;
        let quiet = state.host_depth == 0
            && state.native_depth == 0
            && self.core.hook_depth == 0
            && !self.core.sched.is_shutting_down();
        let result = if state.coro.is_some() && quiet && may_park {
            self.park_in_channel(fiber, chan, ticket, sending, span)
        } else if state.coro.is_none() && quiet && !self.core.sched.is_busy() {
            self.drive_for_channel(fiber, chan, ticket, what, span)
                .map(|()| false)
        } else {
            self.block_for_channel(fiber, chan, ticket, span)
                .map(|()| false)
        };
        match result {
            // Parked: the scheduler finishes the operation when the coroutine runs again or is
            // cancelled.
            Ok(true) => Ok(true),
            Ok(false) => {
                self.core.sched.ext().finish(ticket.op);
                Ok(false)
            }
            Err(ctl) => {
                // The waiter gives up: its hook takes it out of the channel (and passes on a value
                // that was handed to it).
                ticket.token.cancel();
                self.core.sched.ext().finish(ticket.op);
                Err(ctl)
            }
        }
    }

    /// A coroutine waits: its heap fiber parks on the waiter's operation.
    fn park_in_channel(
        &mut self,
        fiber: &mut VmFiber,
        chan: &Arc<ChanInner>,
        ticket: &Ticket,
        sending: bool,
        span: Span,
    ) -> Result<bool, Ctl> {
        let id = self.pause_context(fiber, span, if sending { "send" } else { "recv" })?;
        self.core.sched.set_chan_wait(
            id,
            ChanWait {
                chan: Arc::clone(chan),
                waiter: ticket.id,
                op: ticket.op,
                token: Arc::clone(&ticket.token),
                sending,
            },
        );
        match self.suspend(fiber, id, Wake::External(ticket.op), true, span) {
            Ok(()) => Ok(true),
            Err(ctl) => {
                self.core.sched.take_chan_wait(id);
                Err(ctl)
            }
        }
    }

    /// The main fiber waits: it runs the event loop until the waiter is woken. (On a worker the
    /// loop also looks at the cancel flag of the pool, so a task that waits for a channel that
    /// nobody closes ends when the `Vm` does.)
    fn drive_for_channel(
        &mut self,
        fiber: &mut VmFiber,
        chan: &Arc<ChanInner>,
        ticket: &Ticket,
        what: &str,
        span: Span,
    ) -> Result<(), Ctl> {
        let id = ticket.id;
        loop {
            let deadline = self
                .core
                .has_cancel_flag()
                .then(|| self.core.sched.clock_now() + DRIVE_POLL);
            let driven = self.drive_main(fiber, span, what, deadline, &mut |it| {
                chan.has_done(id) || it.core.cancelled()
            });
            self.settle_refusal(fiber, span, driven)?;
            if chan.has_done(id) {
                return Ok(());
            }
            if self.core.cancelled() {
                return Err(self.budget_exhausted(fiber, span));
            }
        }
    }

    /// The thread waits for the waiter, because nothing else may run meanwhile (a `Drop` hook, a
    /// host callback, native code that calls user code, shutdown, or a coroutine that may not
    /// pause here).
    fn block_for_channel(
        &mut self,
        fiber: &mut VmFiber,
        chan: &Arc<ChanInner>,
        ticket: &Ticket,
        span: Span,
    ) -> Result<(), Ctl> {
        loop {
            if chan.has_done(ticket.id) {
                return Ok(());
            }
            if self.core.cancelled() {
                return Err(self.budget_exhausted(fiber, span));
            }
            if cfg!(target_family = "wasm") {
                // The page's only thread: nothing could ever wake it.
                return Err(self.panic(
                    fiber,
                    span,
                    "this channel operation would wait forever: nothing can run while it waits here",
                ));
            }
            self.core.sched.ext().wait(BLOCK_POLL);
        }
    }

    /// `try_recv`, `close` and `iter` of a channel; `send` and `recv` where they cannot be an
    /// instruction (they block).
    pub(crate) fn channel_ops(
        &mut self,
        fiber: &mut VmFiber,
        op: Builtin,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        let Some(channel) = args.first().cloned() else {
            return Err(self.internal(fiber, span, "ill-typed channel method"));
        };
        match op {
            Builtin::ChannelSend => {
                let Some(value) = args.get(1).cloned() else {
                    return Err(self.internal(fiber, span, "ill-typed channel method"));
                };
                self.chan_send(fiber, &channel, &value, false, false, span)?;
                Ok(Value::Unit)
            }
            Builtin::ChannelRecv => match self.chan_recv(fiber, &channel, false, span)? {
                Recv::Item(v) => Ok(self.core.option(Some(v))),
                Recv::End | Recv::Parked => Ok(self.core.option(None)),
            },
            Builtin::ChannelTryRecv => {
                let Some(chan) = self.core.chan_of(&channel) else {
                    return Err(self.internal(fiber, span, "ill-typed channel method"));
                };
                match chan.try_recv() {
                    Some(p) => {
                        let v = self.rebuild(fiber, p, span)?;
                        Ok(self.core.option(Some(v)))
                    }
                    None => Ok(self.core.option(None)),
                }
            }
            Builtin::ChannelClose => {
                let Some(chan) = self.core.chan_of(&channel) else {
                    return Err(self.internal(fiber, span, "ill-typed channel method"));
                };
                chan.close();
                Ok(Value::Unit)
            }
            Builtin::ChannelIter => Ok(colibri_core::chan::channel_iterator(channel)),
            _ => Err(self.internal(fiber, span, "not a channel method")),
        }
    }

    /// The next value of a channel's iterator where the loop is not the instruction that parks.
    pub(crate) fn channel_next_value(
        &mut self,
        fiber: &mut VmFiber,
        channel: &Value,
        span: Span,
    ) -> Eval<Option<Value>> {
        match self.chan_recv(fiber, channel, false, span)? {
            Recv::Item(v) => Ok(Some(v)),
            Recv::End | Recv::Parked => Ok(None),
        }
    }
}
