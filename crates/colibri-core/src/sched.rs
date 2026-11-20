//! The data of the coroutine scheduler (DESIGN §14.2, ARCHITECTURE §16): which coroutines
//! exist, what each waits for, the clock and the background operations. It knows nothing of how
//! a coroutine is run. An engine keeps the running state of each coroutine itself (the
//! tree-walker: a native stack) and is asked to run it through [`crate::engine::Engine`].

use std::collections::HashMap;
use std::rc::{Rc, Weak};

use colibri_check::types::{COGROUP_TYPE, COROUTINE_TYPE, SIGNAL_TYPE};

use crate::clock::{Clock, SystemClock};
use crate::ctl::PanicInfo;
use crate::external::Externals;
use crate::fiber::FiberState;
use crate::heap::ClassObj;
use crate::value::Value;

/// The default size of the native stack of one coroutine.
pub const DEFAULT_COROUTINE_STACK: usize = 16 * 1024 * 1024;

/// Slack when comparing times, so `wait(0.1)` five times in a row lands on the same tick as
/// `wait(0.5)` despite rounding.
pub const TIME_EPSILON: f64 = 1e-9;

/// How a coroutine ended.
#[doc(hidden)]
pub enum Finish {
    Done(Value),
    Panicked(Box<PanicInfo>),
    Cancelled,
}

/// When a suspended coroutine is due.
#[doc(hidden)]
pub enum Wake {
    /// At every tick (`wait_until` polls its condition on the coroutine itself).
    Ready,
    /// When scheduler time reaches this.
    Time(f64),
    /// When the real clock reaches this.
    Real(f64),
    /// When this coroutine handle is finished.
    Join(Weak<ClassObj>),
    /// Waiting for this signal (no polling: the coroutine is in the signal's waiter list).
    Signal(u64),
    /// The signal was emitted; the coroutine resumes at the next tick, in emission order.
    Signaled,
    /// `join_all` on this group: when no other coroutine of it is alive.
    Group(u64),
    /// A background operation (`Interp::await_external`): when its thread has finished. Needs
    /// no polling: the event loop sleeps until the thread posts its completion.
    External(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[doc(hidden)]
pub enum State {
    Suspended,
    Running,
    Finished,
}

/// What the scheduler knows of one coroutine.
#[doc(hidden)]
pub struct Entry {
    pub id: u64,
    pub state: State,
    pub wake: Wake,
    pub handle: Weak<ClassObj>,
    pub cancel_requested: bool,
    /// How much of the coroutine's native stack the evaluator may use (the engine's measure).
    pub stack_budget: usize,
    /// The `CoGroup` that owns this coroutine.
    pub group: Option<u64>,
    /// The tick in which this coroutine was already resumed (as a signal waiter).
    pub ticked: u64,
}

/// The per-`Interp` coroutine scheduler.
#[doc(hidden)]
pub struct Scheduler {
    pub entries: Vec<Entry>,
    pub next_id: u64,
    /// Scheduler time: the sum of the `dt` of the ticks so far.
    pub time: f64,
    /// How many ticks have run.
    pub frame: u64,
    pub in_tick: bool,
    /// The coroutines whose stacks are active, outermost first.
    pub running: Vec<u64>,
    pub failures: Vec<(u64, Box<PanicInfo>)>,
    pub finished: usize,
    pub shutting_down: bool,
    pub stack_size: usize,
    pub clock: Box<dyn Clock>,
    /// The coroutines waiting for each signal, in wait order.
    pub signals: HashMap<u64, Vec<u64>>,
    /// Coroutines whose signal was emitted, in the order they will resume next tick.
    pub signaled: Vec<u64>,
    /// The live coroutines of each `CoGroup`, in start order.
    pub groups: HashMap<u64, Vec<u64>>,
    /// How many times any coroutine was resumed (for tests and tuning).
    pub resumptions: u64,
    pub ext: Externals,
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
            signals: HashMap::new(),
            signaled: Vec::new(),
            groups: HashMap::new(),
            resumptions: 0,
            ext: Externals::new(),
        }
    }

    pub fn find(&self, id: u64) -> Option<usize> {
        self.entries.binary_search_by_key(&id, |e| e.id).ok()
    }

    /// A fresh id, for a coroutine, a `CoGroup`, a `Signal` or a background operation. Ids
    /// only grow, so `entries` stays sorted by id.
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Records the coroutine `id` (from [`Scheduler::alloc_id`]) as started and not yet run: it
    /// is due at the next tick, and belongs to `group` if it has one. The engine keeps
    /// whatever it needs to run the coroutine; `stack_budget` is the engine's measure of how
    /// deep the coroutine may go, handed back by [`Scheduler::stack_budget`].
    pub fn register(
        &mut self,
        id: u64,
        handle: Weak<ClassObj>,
        group: Option<u64>,
        stack_budget: usize,
    ) {
        self.entries.push(Entry {
            id,
            state: State::Suspended,
            wake: Wake::Ready,
            handle,
            cancel_requested: false,
            stack_budget,
            group,
            ticked: 0,
        });
        if let Some(g) = group {
            self.groups.entry(g).or_default().push(id);
        }
    }

    /// The engine's measure of how deep the coroutine `id` may go, as given to `register`.
    pub fn stack_budget(&self, id: u64) -> Option<usize> {
        Some(self.entries[self.find(id)?].stack_budget)
    }

    /// Whether `id` has been asked to cancel (it was running higher up the resume chain when
    /// asked, so it stops at its next pause).
    pub fn is_cancel_requested(&self, id: u64) -> bool {
        self.find(id)
            .is_some_and(|idx| self.entries[idx].cancel_requested)
    }

    /// The bookkeeping of a coroutine that is about to pause until `wake`: `Err` when it must
    /// not (it is cancelled, or the scheduler does not know it) and `fiber` is marked
    /// cancelling; otherwise it is recorded as waiting. The engine then does the pausing
    /// itself (the tree-walker switches stacks; a VM returns to its loop).
    pub fn prepare_suspend(
        &mut self,
        id: u64,
        fiber: &mut FiberState,
        wake: Wake,
    ) -> Result<(), Cancelled> {
        let Some(idx) = self.find(id) else {
            return Err(Cancelled);
        };
        if fiber.cancelling || self.entries[idx].cancel_requested {
            fiber.cancelling = true;
            return Err(Cancelled);
        }
        if let Wake::Signal(s) = &wake {
            self.signals.entry(*s).or_default().push(id);
        }
        self.entries[idx].wake = wake;
        Ok(())
    }
}

/// The running coroutine is cancelled and must not pause.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct Cancelled;

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle layout: `[id, status, result, name]`.
pub const H_ID: usize = 0;
pub const H_STATUS: usize = 1;
pub const H_RESULT: usize = 2;
pub const H_NAME: usize = 3;
pub const RUNNING: i64 = 0;
pub const DONE: i64 = 1;
pub const PANICKED: i64 = 2;
pub const CANCELLED: i64 = 3;

#[doc(hidden)]
pub fn handle_status(h: &ClassObj) -> i64 {
    match h.field(H_STATUS) {
        Some(Value::Int(s)) => s,
        _ => CANCELLED,
    }
}

#[doc(hidden)]
pub fn handle_id(h: &ClassObj) -> u64 {
    match h.field(H_ID) {
        Some(Value::Int(i)) => i as u64,
        _ => 0,
    }
}

#[doc(hidden)]
pub fn handle_name(h: &ClassObj) -> String {
    match h.field(H_NAME) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "?".to_string(),
    }
}

#[doc(hidden)]
pub fn handle_of(v: &Value) -> Option<&Rc<ClassObj>> {
    match v {
        Value::Obj(o) if o.ty() == COROUTINE_TYPE => Some(o),
        _ => None,
    }
}

/// The id of a `CoGroup` value.
#[doc(hidden)]
pub fn group_id(v: &Value) -> Option<u64> {
    match v {
        Value::Obj(o) if o.ty() == COGROUP_TYPE => match o.field(0) {
            Some(Value::Int(i)) => Some(i as u64),
            _ => None,
        },
        _ => None,
    }
}

/// The id of a `Signal` value.
#[doc(hidden)]
pub fn signal_id(v: &Value) -> Option<u64> {
    match v {
        Value::Obj(o) if o.ty() == SIGNAL_TYPE => match o.field(0) {
            Some(Value::Int(i)) => Some(i as u64),
            _ => None,
        },
        _ => None,
    }
}

/// How driving the scheduler from the main fiber ended.
#[doc(hidden)]
pub enum Drive {
    Done,
    Failure(Box<PanicInfo>),
    /// Nothing is scheduled that could end the wait.
    Stuck,
}

/// Why a tick did not happen or did not end well.
#[doc(hidden)]
pub enum TickError {
    /// `tick` was called from a coroutine, or from inside a tick.
    Reentrant,
    /// A coroutine panicked (the first failure, the others as notes).
    Failure(Box<PanicInfo>),
}
