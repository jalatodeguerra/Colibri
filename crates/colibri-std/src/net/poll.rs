//! The poller of `net`: one thread per `Vm` that waits for the readiness of every socket that a
//! coroutine waits for, and for deadlines, and sleeps without a timeout while nothing is due.
//!
//! The unit of waiting is a [`Cell`]: one per suspension, holding the callback that wakes the
//! waiter and the clean-ups of everything the wait is registered with (an entry of this poller,
//! a place in a queue of turns). Several parties may resolve a cell: the poller (the socket is
//! ready, or the deadline came), whoever closes the object, the queue of turns, and the cancel
//! hook of the waiting coroutine. The first one to take the callback out of the cell runs the
//! clean-ups and then the callback; everyone after finds the cell empty and does nothing. So
//! when the callback runs, the registrations of its wait are gone already, and the socket
//! reference that the entry held is dropped (the registration is deleted from the operating
//! system's poller before the reference is released).
//!
//! The poller does no socket I/O. A woken waiter makes its system call itself, on its own
//! thread, and arms again if the call would still block. Arming looks at the current state of
//! the socket (the registrations are one-shot, and a one-shot registration reports a socket that
//! is ready at the moment it is armed), so no readiness is lost between a call that would block
//! and the arm that follows it.
//!
//! One socket has one registration with the operating system, whatever waits on it: a read and a
//! write may wait at once (a full duplex connection), and the registration's interest is the
//! union of the two. After an event the remaining interest is armed again.
//!
//! Which system does what (read in the source of `polling` 3.11; the notes of the change
//! have the lines):
//!
//! - Linux: epoll with `EPOLLONESHOT`; its descriptors (the epoll instance, an eventfd and a
//!   timerfd) are close-on-exec. `add`, `modify` and `delete` are `epoll_ctl` calls, which the
//!   kernel allows while another thread is in `epoll_wait`. Timeouts use the timerfd, so they are
//!   not rounded to a millisecond.
//! - macOS: kqueue with `EV_ONESHOT` for each of the two filters; close-on-exec is set right
//!   after the kqueue is made. The crate keeps its own set of registered descriptors under a
//!   lock, so `add`, `modify` and `delete` are safe against a `wait` in another thread.
//! - Windows: the AFD driver through a completion port. A change made while the poller thread
//!   is not inside the port wait is queued and applied when it next starts to wait; the check of
//!   that state and the queueing are not one step, so a change can be queued just after the
//!   thread decided to sleep. Every arm therefore wakes the poller on Windows (one wake per
//!   wait, never periodic). A failed connect and a reset or aborted connection are reported
//!   as readable and writable both. Timeouts are rounded up to a millisecond (and the system
//!   clock's resolution applies).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{JoinHandle, ThreadId};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket};

/// The most events one `wait` of the poller thread takes at once; it takes the rest at its next
/// turn, so no socket starves another.
const EVENTS: usize = 1024;

/// What a wait ended with. The reason is all the waiter is told: it looks at its socket (or at
/// its object's flag) to find out the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Why {
    /// The socket is ready (or in an error or hung-up state: the system call that follows says
    /// which), possibly spuriously.
    Ready,
    /// The deadline came.
    TimedOut,
    /// The object was closed (or its queue of turns was woken to look at the flag).
    Closed,
    /// It is the turn of the waiting ticket.
    Turn,
    /// The waiting coroutine was cancelled: nobody reads this, the wait is only being removed.
    Cancelled,
    /// The poller is gone (stopped, or broken by a failed wait or a panic) and this wait will
    /// never be answered: the waiter makes its system call and, finding it still blocked, arms
    /// again and gets the error that a gone poller gives.
    Failed,
}

/// Which way a wait looks at its socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dir {
    Read,
    Write,
}

type Wake = Box<dyn FnOnce(Why) + Send>;
type Cleanup = Box<dyn FnOnce() + Send>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One suspension's wake callback and the clean-ups of its registrations.
pub(super) struct Cell {
    state: Mutex<CellState>,
}

struct CellState {
    wake: Option<Wake>,
    cleanups: Vec<Cleanup>,
}

impl Cell {
    /// A cell whose callback is `wake`. It runs at most once, on the thread that resolves the
    /// cell (possibly the poller's own), after the clean-ups.
    ///
    /// The rule for everything that is given to the poller: the callback, the clean-ups and the
    /// sources must not own the [`Poller`] (an `Arc<Poller>`), directly or through a connection's
    /// shared state. Their drops may run on the poller thread, and the poller is stopped by its
    /// owner (`NetState`), not by the last reference going; a closure that holds the poller would
    /// keep it alive past the `Vm` and could be the one that drops it on its own thread.
    pub(super) fn new(wake: impl FnOnce(Why) + Send + 'static) -> Arc<Cell> {
        Arc::new(Cell {
            state: Mutex::new(CellState {
                wake: Some(Box::new(wake)),
                cleanups: Vec::new(),
            }),
        })
    }

    /// Resolves the cell with `why`: runs the clean-ups, then the callback. `false` (and nothing
    /// happens) if another party resolved it first. Never call it with a lock held that a
    /// clean-up takes (the table of the poller, the state of a queue of turns).
    pub(super) fn fire(&self, why: Why) -> bool {
        let (wake, cleanups) = {
            let mut st = lock(&self.state);
            match st.wake.take() {
                Some(wake) => (wake, std::mem::take(&mut st.cleanups)),
                None => return false,
            }
        };
        // A clean-up that panics must not keep the callback from running (the waiter would never
        // wake): run everything, then pass the first panic on.
        let mut panic = None;
        for cleanup in cleanups {
            if let Err(p) = catch_unwind(AssertUnwindSafe(cleanup)) {
                panic.get_or_insert(p);
            }
        }
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| wake(why))) {
            panic.get_or_insert(p);
        }
        if let Some(p) = panic {
            resume_unwind(p);
        }
        true
    }

    /// Whether the cell has been resolved.
    pub(super) fn is_resolved(&self) -> bool {
        lock(&self.state).wake.is_none()
    }

    /// Adds a clean-up that runs when the cell is resolved. `false` if it was resolved already:
    /// the clean-up is not kept, and the caller must not register what it would have removed.
    /// It may be called with a lock held that the clean-up itself takes, because it only takes
    /// the lock of the cell (and `fire` never holds that one while it runs clean-ups).
    pub(super) fn on_resolve(&self, cleanup: impl FnOnce() + Send + 'static) -> bool {
        let mut st = lock(&self.state);
        if st.wake.is_none() {
            return false;
        }
        st.cleanups.push(Box::new(cleanup));
        true
    }
}

/// A socket that can wait with the poller: the table keeps a reference to it for as long as it
/// is registered, so it is not closed (and its descriptor not reused) before the registration is
/// deleted.
#[cfg(unix)]
pub(super) trait Source: AsFd + AsRawFd + Send + Sync + 'static {}
#[cfg(unix)]
impl<T: AsFd + AsRawFd + Send + Sync + 'static> Source for T {}
#[cfg(windows)]
pub(super) trait Source: AsSocket + AsRawSocket + Send + Sync + 'static {}
#[cfg(windows)]
impl<T: AsSocket + AsRawSocket + Send + Sync + 'static> Source for T {}

/// What identifies a socket to the system, as a number. (The cast is from a signed descriptor on
/// Unix and from a type whose width is the system's on Windows.)
#[cfg_attr(windows, allow(clippy::unnecessary_cast))]
fn raw_id(source: &dyn Source) -> u64 {
    #[cfg(unix)]
    {
        source.as_raw_fd() as u64
    }
    #[cfg(windows)]
    {
        source.as_raw_socket() as u64
    }
}

#[cfg(unix)]
fn borrowed(source: &dyn Source) -> BorrowedFd<'_> {
    source.as_fd()
}

#[cfg(windows)]
fn borrowed(source: &dyn Source) -> BorrowedSocket<'_> {
    source.as_socket()
}

/// A socket's registration with the operating system's poller.
struct Reg {
    socket: Arc<dyn Source>,
    raw: u64,
    /// The waiters (by id) that wait for the socket to become readable and writable. At most
    /// one each: the queues of turns of the objects allow one read and one write at a time.
    read: Option<usize>,
    write: Option<usize>,
}

struct Waiter {
    cell: Arc<Cell>,
    /// The registration this waits on; `None` for a timer.
    reg: Option<usize>,
    dir: Dir,
    deadline: Option<Instant>,
}

/// Why the poller thread is gone or going.
enum Halt {
    /// Its owner dropped it.
    Stopped,
    /// The system call that waits failed: every arm fails from now on, and the next wait that
    /// needs a poller makes a new one.
    Failed(String),
}

struct Table {
    halt: Option<Halt>,
    /// The next key of a registration or a waiter (one counter, never reused; a `usize` because
    /// it travels through the system's poller as an event's user data).
    next: usize,
    regs: HashMap<usize, Reg>,
    by_raw: HashMap<u64, usize>,
    waiters: HashMap<usize, Waiter>,
    /// `(deadline, waiter)` of the waiters with a deadline, plus stale entries of waiters that
    /// ended another way (skipped when they come up; the heap is rebuilt when they outnumber the
    /// live ones).
    heap: BinaryHeap<Reverse<(Instant, usize)>>,
    live_deadlines: usize,
    /// The deadline the thread is sleeping towards (`None`: no deadline, or not asleep yet): an
    /// arm with an earlier deadline wakes it so that it plans again.
    planned: Option<Instant>,
    /// Cells to resolve once the lock is released.
    to_fire: Vec<(Arc<Cell>, Why)>,
    /// What is dropped once the lock is released. The rule: a socket is dropped under the lock
    /// (right after its registration is deleted), so that whoever resolved a wait can rely on
    /// the descriptor being released when its `fire` has returned (`close()` promises that a
    /// listener's port is free when it returns); closing a plain socket cannot call back into
    /// the poller. Everything that can run arbitrary code when it drops (a cell, with its
    /// callback and clean-ups) goes here and is dropped after the lock.
    trash: Vec<Arc<Cell>>,
}

struct Shared {
    os: polling::Poller,
    table: Mutex<Table>,
    /// How many times the thread went round its loop. A poller that sleeps without a timeout
    /// does not move it while nothing happens; one that ticks does (a test looks).
    loops: AtomicU64,
    /// Whether the thread caught a panic (a test looks: the halt reason does not say when the
    /// poller was stopped first).
    panicked: AtomicBool,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // The last owner is gone with the table possibly not empty (a poller that was never run
        // to its end): delete what is registered before the sockets are released, as every other
        // path does, whatever the order in which the fields drop.
        let table = self.table.get_mut().unwrap_or_else(PoisonError::into_inner);
        for (_, reg) in table.regs.drain() {
            let _ = self.os.delete(borrowed(&*reg.socket));
        }
    }
}

/// The longest the thread sleeps in one `wait`, whatever its next deadline is; the loop plans
/// again after every wake, so a farther deadline costs one wake per bound and nothing else.
/// Some systems refuse or mangle a very long timeout: macOS's `kevent` fails with `EINVAL` for
/// more than about three years (taken from a reading of the XNU source and from Go's netpoller,
/// not run here), and Windows turns more than 49.7 days (`u32::MAX` ms) into "no timeout".
const MAX_SLEEP: Duration = Duration::from_secs(3600);

/// What the thread passes to the system's `wait`.
fn sleep_for(planned: Option<Instant>, now: Instant) -> Option<Duration> {
    planned.map(|d| d.saturating_duration_since(now).min(MAX_SLEEP))
}

/// Resolves the cells that a locked operation collected, after the lock is gone: every one of
/// them, also when a callback panics; the first panic is given back for the caller to deal with.
pub(super) fn fire_all(to_fire: Vec<(Arc<Cell>, Why)>) -> Option<Box<dyn std::any::Any + Send>> {
    let mut panic = None;
    for (cell, why) in to_fire {
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| cell.fire(why))) {
            panic.get_or_insert(p);
        }
    }
    panic
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Table> {
        lock(&self.table)
    }

    /// Releases the lock, drops what was thrown away under it, and resolves the cells that were
    /// collected. A panic of a callback is given back (every cell was resolved first).
    fn unlock_and_fire(
        &self,
        mut table: MutexGuard<'_, Table>,
    ) -> Option<Box<dyn std::any::Any + Send>> {
        let to_fire = std::mem::take(&mut table.to_fire);
        let trash = std::mem::take(&mut table.trash);
        drop(table);
        drop(trash);
        fire_all(to_fire)
    }

    /// As `unlock_and_fire` for a caller that is not the poller thread: a panic of a callback
    /// is the caller's panic.
    fn unlock_and_fire_here(&self, table: MutexGuard<'_, Table>) {
        if let Some(p) = self.unlock_and_fire(table) {
            resume_unwind(p);
        }
    }

    fn notify(&self) {
        let _ = self.os.notify();
    }

    /// Takes a waiter out of the table, and its socket's registration along with it when it was
    /// the last waiter there. Idempotent: the id of a waiter that is gone does nothing. The
    /// waiter's cell is thrown away (dropped after the unlock).
    fn remove_waiter(&self, t: &mut Table, id: usize) {
        let Some(waiter) = t.waiters.remove(&id) else {
            return;
        };
        let reg = waiter.reg;
        let dir = waiter.dir;
        self.ended(t, &waiter);
        t.trash.push(waiter.cell);
        if let Some(reg) = reg {
            if let Some(r) = t.regs.get_mut(&reg) {
                match dir {
                    Dir::Read => r.read = None,
                    Dir::Write => r.write = None,
                }
            }
            self.rearm(t, reg);
        }
    }

    /// A waiter has left the table by any path: its deadline no longer counts, and the heap is
    /// rebuilt when the entries that nobody is waiting for outnumber the live ones (one live
    /// early deadline at the top would otherwise pin every stale entry behind it).
    fn ended(&self, t: &mut Table, waiter: &Waiter) {
        if waiter.deadline.is_none() {
            return;
        }
        t.live_deadlines -= 1;
        if t.heap.len() > 64 && t.heap.len() > 2 * t.live_deadlines {
            let live: Vec<_> = t
                .waiters
                .iter()
                .filter_map(|(id, w)| w.deadline.map(|d| Reverse((d, *id))))
                .collect();
            t.heap = live.into();
        }
    }

    /// After the interest of a registration changed or an event used up its one shot: deletes
    /// the registration when nothing waits on it any more (and only then releases the socket
    /// reference, after the unlock), otherwise arms what remains.
    fn rearm(&self, t: &mut Table, key: usize) {
        let Some(reg) = t.regs.get(&key) else { return };
        if reg.read.is_none() && reg.write.is_none() {
            let reg = t.regs.remove(&key).expect("looked up above");
            t.by_raw.remove(&reg.raw);
            // Failing here means the system has dropped the registration already; there is
            // nothing left to undo. The reference is released after the call, never before.
            let _ = self.os.delete(borrowed(&*reg.socket));
            drop(reg.socket);
            return;
        }
        let event = polling::Event::new(key, reg.read.is_some(), reg.write.is_some());
        if self.os.modify(borrowed(&*reg.socket), event).is_err() {
            // The remaining waiters cannot be armed: wake them, so that they try their system
            // call and find out why (and fail to arm again, with an error of their own).
            let ids = [reg.read, reg.write];
            for id in ids.into_iter().flatten() {
                if let Some(w) = t.waiters.remove(&id) {
                    self.ended(t, &w);
                    t.to_fire.push((w.cell, Why::Ready));
                }
            }
            let reg = t.regs.remove(&key).expect("looked up above");
            t.by_raw.remove(&reg.raw);
            let _ = self.os.delete(borrowed(&*reg.socket));
            drop(reg.socket);
        }
    }

    /// Resolves the waiters whose deadline has come, and gives the deadline of the next one.
    fn expire(&self, t: &mut Table, now: Instant) -> Option<Instant> {
        while let Some(&Reverse((deadline, id))) = t.heap.peek() {
            let live = t
                .waiters
                .get(&id)
                .is_some_and(|w| w.deadline == Some(deadline));
            if live && deadline > now {
                return Some(deadline);
            }
            t.heap.pop();
            if live && let Some(cell) = t.waiters.get(&id).map(|w| Arc::clone(&w.cell)) {
                t.to_fire.push((cell, Why::TimedOut));
                self.remove_waiter(t, id);
            }
        }
        None
    }

    /// An event of the operating system's poller for the registration `key`.
    fn event(&self, t: &mut Table, key: usize, readable: bool, writable: bool) {
        let Some(reg) = t.regs.get_mut(&key) else {
            // Its registration was deleted after the event was queued: stale.
            return;
        };
        let mut ids = Vec::with_capacity(2);
        if readable {
            ids.extend(reg.read.take());
        }
        if writable {
            ids.extend(reg.write.take());
        }
        for id in ids {
            if let Some(w) = t.waiters.remove(&id) {
                self.ended(t, &w);
                t.to_fire.push((w.cell, Why::Ready));
            }
        }
        // The one shot is used up whatever came: arm what is left, or delete.
        self.rearm(t, key);
    }

    /// The thread's loop, until the poller is halted or something panics.
    fn run_loop(&self) {
        let mut events =
            polling::Events::with_capacity(NonZeroUsize::new(EVENTS).expect("not zero"));
        loop {
            self.loops.fetch_add(1, Ordering::Relaxed);
            let planned = {
                let mut t = self.lock();
                if t.halt.is_some() {
                    return;
                }
                let planned = self.expire(&mut t, Instant::now());
                t.planned = planned;
                if let Some(p) = self.unlock_and_fire(t) {
                    resume_unwind(p);
                }
                planned
            };
            events.clear();
            if let Err(e) = self
                .os
                .wait(&mut events, sleep_for(planned, Instant::now()))
            {
                let mut t = self.lock();
                if t.halt.is_none() {
                    t.halt = Some(Halt::Failed(e.to_string()));
                }
                return;
            }
            let mut t = self.lock();
            for event in events.iter() {
                self.event(&mut t, event.key, event.readable, event.writable);
            }
            if let Some(p) = self.unlock_and_fire(t) {
                resume_unwind(p);
            }
        }
    }

    /// The thread's end, whatever the reason (a stop, a failed wait, a panic): marks the poller
    /// halted so that nothing is armed on it from now on, deletes every registration, and then
    /// resolves every waiter that is left with `Why::Failed`, also those whose cell is held
    /// elsewhere (a cancel hook, an object's slot), which would otherwise never be woken. The
    /// sockets are released under the lock and the cells let go after it is gone.
    fn leave(&self) {
        let (waiters, to_fire, trash) = {
            let mut t = self.lock();
            if t.halt.is_none() {
                t.halt = Some(Halt::Failed("the poller thread ended".to_string()));
            }
            for reg in t.regs.values() {
                let _ = self.os.delete(borrowed(&*reg.socket));
            }
            t.by_raw.clear();
            t.heap.clear();
            t.live_deadlines = 0;
            // The sockets are released here, under the lock, as in `rearm`: a closer whose
            // `fire` wins from now on returns with the descriptor already released.
            t.regs.clear();
            (
                std::mem::take(&mut t.waiters),
                std::mem::take(&mut t.to_fire),
                std::mem::take(&mut t.trash),
            )
        };
        let cells = to_fire
            .into_iter()
            .chain(waiters.into_values().map(|w| (w.cell, Why::Failed)));
        // A callback that panics here is lost with the thread; the others still run.
        let _ = fire_all(cells.collect());
        drop(trash);
    }
}

/// The thread's body.
fn run(shared: &Shared) {
    if let Err(panic) = catch_unwind(AssertUnwindSafe(|| shared.run_loop())) {
        shared.panicked.store(true, Ordering::SeqCst);
        let mut t = shared.lock();
        if t.halt.is_none() {
            let why = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(no message)".to_string());
            t.halt = Some(Halt::Failed(format!("the poller thread panicked: {why}")));
        }
    }
    shared.leave();
}

/// Why a poller could not be started.
#[derive(Debug)]
pub(super) enum StartError {
    /// The system would not make the poller (out of descriptors, say).
    System(io::Error),
    /// The system would not start its thread.
    Thread(io::Error),
}

/// The poller thread of one `Vm`. Its owner (`NetState`) stops it with [`Poller::stop`]; being
/// dropped does the same, and never joins from the poller's own thread.
pub(super) struct Poller {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    thread_id: ThreadId,
}

impl Poller {
    /// Makes the poller and starts its thread (`colibri-netpoll`: Linux cuts thread names to
    /// fifteen bytes).
    pub(super) fn start() -> Result<Poller, StartError> {
        let os = polling::Poller::new().map_err(StartError::System)?;
        let shared = Arc::new(Shared {
            os,
            table: Mutex::new(Table {
                halt: None,
                next: 0,
                regs: HashMap::new(),
                by_raw: HashMap::new(),
                waiters: HashMap::new(),
                heap: BinaryHeap::new(),
                live_deadlines: 0,
                planned: None,
                to_fire: Vec::new(),
                trash: Vec::new(),
            }),
            loops: AtomicU64::new(0),
            panicked: AtomicBool::new(false),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("colibri-netpoll".to_string())
                .spawn(move || run(&shared))
                .map_err(StartError::Thread)?
        };
        Ok(Poller {
            shared,
            thread_id: thread.thread().id(),
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Whether the thread runs and takes waits (not stopped, not broken by a failed wait or a
    /// panic).
    pub(super) fn is_alive(&self) -> bool {
        self.shared.lock().halt.is_none()
            && lock(&self.thread)
                .as_ref()
                .is_some_and(|t| !t.is_finished())
    }

    /// Stops the thread and joins it (unless this is the poller's own thread, which cannot join
    /// itself): every wait that is still armed is resolved with [`Why::Failed`] before this
    /// returns, and no wait can be armed afterwards. Calling it again does nothing.
    pub(super) fn stop(&self) {
        {
            let mut t = self.shared.lock();
            if t.halt.is_none() {
                t.halt = Some(Halt::Stopped);
            }
        }
        self.shared.notify();
        if std::thread::current().id() == self.thread_id {
            return;
        }
        if let Some(thread) = lock(&self.thread).take() {
            // A thread that panicked has nothing more to say here.
            let _ = thread.join();
        }
    }

    /// Waits for `source` to be readable or writable (`dir`) until `deadline`, and resolves
    /// `cell` with [`Why::Ready`] or [`Why::TimedOut`]. If the socket is ready already it
    /// reports at once (through the thread, not from this call). The poller holds a reference to
    /// `source` until the wait ends, whichever way it ends. Does nothing if `cell` is resolved
    /// already. An `Err` means nothing was armed and the cell is still unresolved, with nothing
    /// left on it; the caller makes its system call to learn why, or fails. A poller that has
    /// stopped or whose thread is gone refuses every arm.
    ///
    /// At most one wait per direction on one socket: a second is an error (the queues of turns
    /// of the objects never allow it).
    ///
    /// `source` and `cell` (its callback and clean-ups) must not own the `Poller`: see
    /// [`Cell::new`].
    pub(super) fn arm<S: Source>(
        &self,
        source: &Arc<S>,
        dir: Dir,
        deadline: Option<Instant>,
        cell: &Arc<Cell>,
    ) -> io::Result<()> {
        if cell.is_resolved() {
            return Ok(());
        }
        let source: Arc<dyn Source> = Arc::clone(source) as Arc<dyn Source>;
        let raw = raw_id(&*source);
        let mut t = self.shared.lock();
        if let Some(halt) = &t.halt {
            return Err(halt_error(halt));
        }
        let key = t.by_raw.get(&raw).copied();
        if let Some(reg) = key.and_then(|k| t.regs.get(&k)) {
            let taken = match dir {
                Dir::Read => reg.read.is_some(),
                Dir::Write => reg.write.is_some(),
            };
            if taken {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a wait in this direction is armed on the socket already",
                ));
            }
        }
        let id = t.next;
        t.next += 1;
        let reg_key = match key {
            Some(k) => {
                let reg = t.regs.get_mut(&k).expect("by_raw and regs agree");
                match dir {
                    Dir::Read => reg.read = Some(id),
                    Dir::Write => reg.write = Some(id),
                }
                let event = polling::Event::new(k, reg.read.is_some(), reg.write.is_some());
                if let Err(e) = self.shared.os.modify(borrowed(&*reg.socket), event) {
                    match dir {
                        Dir::Read => reg.read = None,
                        Dir::Write => reg.write = None,
                    }
                    return Err(e);
                }
                k
            }
            None => {
                let k = t.next;
                t.next += 1;
                let event = polling::Event::new(k, dir == Dir::Read, dir == Dir::Write);
                // SAFETY: the table owns `source` (an `Arc`) from here until the registration
                // is deleted. Every path that removes a `Reg` from the table calls `delete`
                // before the socket reference is released: `rearm` (both of its branches), the
                // thread's `leave`, and `Drop for Shared` (a table that is not empty when the
                // last owner goes). So the socket is open, and the same one, for as long as it
                // is registered.
                unsafe { self.shared.os.add(raw_for_add(&*source), event)? };
                t.regs.insert(
                    k,
                    Reg {
                        socket: source,
                        raw,
                        read: (dir == Dir::Read).then_some(id),
                        write: (dir == Dir::Write).then_some(id),
                    },
                );
                t.by_raw.insert(raw, k);
                k
            }
        };
        t.waiters.insert(
            id,
            Waiter {
                cell: Arc::clone(cell),
                reg: Some(reg_key),
                dir,
                deadline,
            },
        );
        let wake = self.note_deadline(&mut t, id, deadline);
        // Only now, with nothing that can fail left, does the cell learn how to remove this
        // wait. If it was resolved in between, the wait is taken out again.
        if !cell.on_resolve(self.removal(id)) {
            self.shared.remove_waiter(&mut t, id);
            self.shared.unlock_and_fire_here(t);
            return Ok(());
        }
        drop(t);
        // Windows needs the thread woken for every change (see the module comment).
        if cfg!(windows) || wake {
            self.shared.notify();
        }
        Ok(())
    }

    /// The clean-up that takes the wait `id` out of the table when its cell is resolved.
    fn removal(&self, id: usize) -> impl FnOnce() + Send + use<> {
        let weak = Arc::downgrade(&self.shared);
        move || {
            if let Some(shared) = weak.upgrade() {
                let mut t = shared.lock();
                shared.remove_waiter(&mut t, id);
                shared.unlock_and_fire_here(t);
            }
        }
    }

    /// Resolves `cell` with [`Why::TimedOut`] at `deadline`, with no socket involved. Does
    /// nothing if `cell` is resolved already; refuses on a poller that has stopped. The same rule
    /// as for `arm`: the cell must not own the `Poller`.
    pub(super) fn timer(&self, deadline: Instant, cell: &Arc<Cell>) -> io::Result<()> {
        if cell.is_resolved() {
            return Ok(());
        }
        let mut t = self.shared.lock();
        if let Some(halt) = &t.halt {
            return Err(halt_error(halt));
        }
        let id = t.next;
        t.next += 1;
        t.waiters.insert(
            id,
            Waiter {
                cell: Arc::clone(cell),
                reg: None,
                dir: Dir::Read,
                deadline: Some(deadline),
            },
        );
        let wake = self.note_deadline(&mut t, id, Some(deadline));
        if !cell.on_resolve(self.removal(id)) {
            self.shared.remove_waiter(&mut t, id);
            self.shared.unlock_and_fire_here(t);
            return Ok(());
        }
        drop(t);
        if wake {
            self.shared.notify();
        }
        Ok(())
    }

    /// Records the deadline of a new waiter; whether the thread must be woken to plan again
    /// (the deadline is earlier than the one it sleeps towards, or it sleeps without one).
    fn note_deadline(&self, t: &mut Table, id: usize, deadline: Option<Instant>) -> bool {
        let Some(deadline) = deadline else {
            return false;
        };
        t.live_deadlines += 1;
        t.heap.push(Reverse((deadline, id)));
        t.planned.is_none_or(|planned| deadline < planned)
    }

    /// How many registrations with the system and waiters the table holds (for the tests).
    #[cfg(test)]
    pub(super) fn counts(&self) -> (usize, usize) {
        let t = self.shared.lock();
        (t.regs.len(), t.waiters.len())
    }

    /// How many times the thread has gone round its loop (for the tests).
    #[cfg(test)]
    pub(super) fn loops(&self) -> u64 {
        self.shared.loops.load(Ordering::Relaxed)
    }

    /// How many entries the deadline heap holds, stale ones included (for the tests).
    #[cfg(test)]
    pub(super) fn heap_len(&self) -> usize {
        self.shared.lock().heap.len()
    }

    /// The shared state itself, so that a test can look at it after the poller is gone.
    #[cfg(test)]
    fn shared_for_test(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    /// A probe that says whether the poller's shared state is gone, which it is once the thread
    /// has left and the poller has been dropped.
    #[cfg(test)]
    pub(super) fn gone_probe(&self) -> impl Fn() -> bool + use<> {
        let weak = Arc::downgrade(&self.shared);
        move || weak.upgrade().is_none()
    }
}

fn halt_error(halt: &Halt) -> io::Error {
    match halt {
        Halt::Stopped => io::Error::other("the poller has stopped"),
        Halt::Failed(why) => io::Error::other(format!("the poller failed: {why}")),
    }
}

#[cfg(unix)]
fn raw_for_add(source: &dyn Source) -> std::os::fd::RawFd {
    source.as_raw_fd()
}

#[cfg(windows)]
fn raw_for_add(source: &dyn Source) -> std::os::windows::io::RawSocket {
    source.as_raw_socket()
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the `Vm` keeps for `net`: the poller, made by the first wait that needs it, stopped and
/// joined when the `Vm` is dropped (the `Vm`'s host state drops with it), whoever else still
/// holds an `Arc` of it.
#[derive(Default)]
pub(super) struct NetState {
    poller: Option<Arc<Poller>>,
}

impl NetState {
    /// The poller, started if there is none (or the last one broke). A failure to start it is
    /// not kept: the next wait tries again.
    pub(super) fn poller(&mut self) -> Result<Arc<Poller>, StartError> {
        if let Some(poller) = &self.poller
            && poller.is_alive()
        {
            return Ok(Arc::clone(poller));
        }
        let poller = Arc::new(Poller::start()?);
        self.poller = Some(Arc::clone(&poller));
        Ok(poller)
    }
}

impl Drop for NetState {
    fn drop(&mut self) {
        if let Some(poller) = self.poller.take() {
            poller.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Barrier, mpsc};
    use std::time::Duration;

    use socket2::{Domain, Protocol, SockAddr, SockRef, Socket, Type};

    use super::*;

    /// How long a test waits for something that has to happen: a ceiling, never the thing
    /// measured (a loaded machine, or a system with coarse timers, may need much longer than
    /// the thing takes).
    const CEILING: Duration = Duration::from_secs(30);

    fn poller() -> Poller {
        Poller::start().expect("the poller starts")
    }

    /// A cell whose callback sends the reason.
    fn cell() -> (Arc<Cell>, mpsc::Receiver<Why>) {
        let (tx, rx) = mpsc::channel();
        (
            Cell::new(move |why| {
                let _ = tx.send(why);
            }),
            rx,
        )
    }

    fn expect(rx: &mpsc::Receiver<Why>) -> Why {
        rx.recv_timeout(CEILING)
            .expect("the wait should have ended")
    }

    /// Nothing has resolved the cell yet.
    fn assert_quiet(rx: &mpsc::Receiver<Why>) {
        assert!(
            matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "the wait ended too early"
        );
    }

    /// The size of the buffers of the pair: fixed, so that the system's automatic sizing does not
    /// open the peer's window later on (macOS does, on loopback). Setup, not what is tested.
    const BUFFER: usize = 64 * 1024;

    /// A connected loopback pair, both ends non-blocking: the end that waits (shared, as the
    /// poller holds a reference to it) and its peer. The writer's send buffer and the peer's
    /// receive buffer have a fixed size (set before the connection, as the system wants for the
    /// receive buffer).
    fn pair() -> (Arc<TcpStream>, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        peer.set_recv_buffer_size(BUFFER).unwrap();
        peer.connect(&SockAddr::from(listener.local_addr().unwrap()))
            .unwrap();
        let peer = TcpStream::from(peer);
        let (server, _) = listener.accept().unwrap();
        SockRef::from(&server).set_send_buffer_size(BUFFER).unwrap();
        server.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        (Arc::new(server), peer)
    }

    /// Writes until the first `WouldBlock`; how many bytes it took. Setup, not what is tested:
    /// the buffers have a fixed size (see `BUFFER`), and a test that follows must not rely on the
    /// socket staying unwritable (a window update can still make room), so each says how it can
    /// still fail.
    fn fill(socket: &TcpStream) -> usize {
        let chunk = vec![0u8; 64 * 1024];
        let mut total = 0;
        loop {
            match (&*socket).write(&chunk) {
                Ok(n) => total += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return total,
                Err(e) => panic!("filling the send buffer: {e}"),
            }
            assert!(total < 1 << 30, "the send buffer never filled");
        }
    }

    /// What the product code does with a write wait: write until the socket would block or fails,
    /// and on `WouldBlock` arm again and wait again (a `Ready` only means "try again": room for
    /// some bytes, a reset, or nothing at all). The kind of the error that ends it, within the
    /// ceiling.
    fn write_until_it_fails(p: &Poller, socket: &Arc<TcpStream>) -> ErrorKind {
        let began = Instant::now();
        let chunk = vec![0u8; 16 * 1024];
        loop {
            assert!(
                began.elapsed() < CEILING,
                "the write never failed: the reset did not end the write wait"
            );
            match (&**socket).write(&chunk) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    let (cell, rx) = cell();
                    p.arm(socket, Dir::Write, Some(began + CEILING), &cell)
                        .unwrap();
                    match rx.recv_timeout(CEILING) {
                        Ok(Why::Ready | Why::TimedOut) => {}
                        other => panic!("the write wait ended with {other:?}"),
                    }
                }
                Err(e) => return e.kind(),
            }
        }
    }

    /// Reads from `peer` until the wait ends, and gives the reason.
    fn drain_until(peer: &TcpStream, rx: &mpsc::Receiver<Why>) -> Why {
        let began = Instant::now();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match rx.try_recv() {
                Ok(why) => return why,
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => panic!("the wait was dropped"),
            }
            match (&*peer).read(&mut buf) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("draining: {e}"),
            }
            assert!(began.elapsed() < CEILING, "the wait never ended");
        }
    }

    /// Right after a read and a write wait were armed on one socket: either the table holds one
    /// registration with two waiters (the thing under test), or the write wait has already ended
    /// (a system may report a socket as writable from a low-water mark, so an early `Ready` on a
    /// write wait is honest), and then the read wait still holds the registration and the reason
    /// of the write is given back. The test can still fail: two registrations, a lost read, or
    /// anything but those two shapes.
    fn both_or_write_already_ended(p: &Poller, write_rx: &mpsc::Receiver<Why>) -> Option<Why> {
        match p.counts() {
            (1, 2) => None,
            (1, 1) => Some(expect(write_rx)),
            other => {
                panic!("neither one registration with two waiters nor the read alone: {other:?}")
            }
        }
    }

    /// Nothing is registered and nobody waits, and the table holds no reference to `socket`. The
    /// count is checked at once, with no waiting: a socket is released under the table lock, so
    /// when the call that ended the wait has returned, the reference is gone.
    fn assert_clean<S>(poller: &Poller, socket: &Arc<S>) {
        assert_eq!(poller.counts(), (0, 0));
        assert_eq!(
            Arc::strong_count(socket),
            1,
            "the table still holds a reference to the socket"
        );
    }

    #[test]
    fn a_read_wait_ends_when_data_arrives() {
        let p = poller();
        let (a, mut b) = pair();
        let (cell, rx) = cell();
        p.arm(&a, Dir::Read, None, &cell).unwrap();
        assert_eq!(p.counts(), (1, 1));
        assert_eq!(Arc::strong_count(&a), 2, "the table holds a reference");
        assert_quiet(&rx);
        b.write_all(b"x").unwrap();
        assert_eq!(expect(&rx), Why::Ready);
        assert_clean(&p, &a);
    }

    /// A socket that is ready when it is armed reports at once: nothing is lost between a call
    /// that would block and the arm that follows it.
    #[test]
    fn a_socket_that_is_ready_when_armed_reports_at_once() {
        let p = poller();
        let (a, mut b) = pair();
        b.write_all(b"x").unwrap();
        // The byte may need a moment to cross the loopback, so arm until it has: a wait that
        // is armed before the byte arrives is woken by it, and one armed after is ready.
        let (cell, rx) = cell();
        p.arm(&a, Dir::Read, None, &cell).unwrap();
        assert_eq!(expect(&rx), Why::Ready);
        // Write readiness: an empty send buffer is ready from the start.
        let (cell, rx) = self::cell();
        p.arm(&a, Dir::Write, None, &cell).unwrap();
        assert_eq!(expect(&rx), Why::Ready);
        assert_clean(&p, &a);
    }

    #[test]
    fn a_write_wait_ends_when_the_peer_makes_room() {
        let p = poller();
        let (a, b) = pair();
        fill(&a);
        let (cell, rx) = cell();
        p.arm(&a, Dir::Write, None, &cell).unwrap();
        // (No claim that it is quiet before the peer reads: an honest early `Ready` would only
        // mean "try again".)
        assert_eq!(drain_until(&b, &rx), Why::Ready);
        assert_clean(&p, &a);
    }

    /// A read and a write wait on one socket at once (a full duplex connection): one
    /// registration with the system holds both, and each ends on its own event.
    #[test]
    fn a_read_and_a_write_can_wait_on_one_socket_together() {
        let p = poller();
        let (a, mut b) = pair();
        fill(&a);
        let (write_cell, write_rx) = cell();
        let (read_cell, read_rx) = self::cell();
        p.arm(&a, Dir::Write, None, &write_cell).unwrap();
        p.arm(&a, Dir::Read, None, &read_cell).unwrap();
        let early = both_or_write_already_ended(&p, &write_rx);
        assert_quiet(&read_rx);
        // Data for the read: the read ends. The write may or may not be told too (a wake-up
        // may be spurious); either way it ends when the peer has made room.
        b.write_all(b"x").unwrap();
        assert_eq!(expect(&read_rx), Why::Ready);
        assert_eq!(
            early.unwrap_or_else(|| drain_until(&b, &write_rx)),
            Why::Ready
        );
        assert_clean(&p, &a);
        // And the other way round: the write ends first, the read stays armed.
        let (write_cell, write_rx) = self::cell();
        let (read_cell, read_rx) = self::cell();
        let mut left = [0u8; 16];
        // The byte that is still unread would make the read ready at once.
        while let Ok(n) = (&*a).read(&mut left) {
            if n == 0 {
                break;
            }
        }
        p.arm(&a, Dir::Read, None, &read_cell).unwrap();
        p.arm(&a, Dir::Write, None, &write_cell).unwrap();
        assert_eq!(expect(&write_rx), Why::Ready);
        b.write_all(b"y").unwrap();
        assert_eq!(expect(&read_rx), Why::Ready);
        assert_clean(&p, &a);
        // And with the read armed first and the write blocked: arming the write must not take
        // the read out of the system's interest, or the data that comes next is never seen.
        while let Ok(n) = (&*a).read(&mut left) {
            if n == 0 {
                break;
            }
        }
        fill(&a);
        let (write_cell, write_rx) = self::cell();
        let (read_cell, read_rx) = self::cell();
        p.arm(&a, Dir::Read, None, &read_cell).unwrap();
        p.arm(&a, Dir::Write, None, &write_cell).unwrap();
        let early = both_or_write_already_ended(&p, &write_rx);
        assert_quiet(&read_rx);
        b.write_all(b"z").unwrap();
        assert_eq!(expect(&read_rx), Why::Ready);
        assert_eq!(
            early.unwrap_or_else(|| drain_until(&b, &write_rx)),
            Why::Ready
        );
        assert_clean(&p, &a);
    }

    #[test]
    fn a_second_wait_in_one_direction_on_a_socket_is_refused() {
        let p = poller();
        let (a, _b) = pair();
        let (first, first_rx) = cell();
        let (second, second_rx) = cell();
        p.arm(&a, Dir::Read, None, &first).unwrap();
        let refused = p.arm(&a, Dir::Read, None, &second).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::AlreadyExists);
        assert_eq!(p.counts(), (1, 1));
        assert!(!second.is_resolved());
        assert_quiet(&second_rx);
        // The first is untouched; ending it leaves nothing behind.
        assert!(first.fire(Why::Closed));
        assert_eq!(expect(&first_rx), Why::Closed);
        assert_clean(&p, &a);
    }

    #[test]
    fn a_deadline_ends_a_wait_and_not_before_it() {
        let p = poller();
        let (a, _b) = pair();
        let (cell, rx) = cell();
        let deadline = Instant::now() + Duration::from_millis(100);
        p.arm(&a, Dir::Read, Some(deadline), &cell).unwrap();
        assert_eq!(expect(&rx), Why::TimedOut);
        assert!(Instant::now() >= deadline, "it fired before its time");
        assert_clean(&p, &a);
    }

    #[test]
    fn a_timer_ends_with_no_socket_involved() {
        let p = poller();
        let (cell, rx) = cell();
        let deadline = Instant::now() + Duration::from_millis(50);
        p.timer(deadline, &cell).unwrap();
        assert_eq!(p.counts(), (0, 1));
        assert_eq!(expect(&rx), Why::TimedOut);
        assert!(Instant::now() >= deadline);
        assert_eq!(p.counts(), (0, 0));
    }

    /// A wait whose deadline is earlier than the one the thread sleeps towards wakes it, so
    /// that it plans again: the first wait has an hour, the second a tenth of a second, and the
    /// second ends long before the ceiling.
    #[test]
    fn a_deadline_earlier_than_the_current_sleep_fires_on_time() {
        let p = poller();
        let (a, _b) = pair();
        let (c, d) = pair();
        let (late, late_rx) = cell();
        p.arm(
            &a,
            Dir::Read,
            Some(Instant::now() + Duration::from_secs(3600)),
            &late,
        )
        .unwrap();
        // Let the thread go to sleep towards the late deadline: a timer that ends is a point
        // where it has planned (it plans before it sleeps) and then sleeps.
        let (marker, marker_rx) = cell();
        p.timer(Instant::now() + Duration::from_millis(20), &marker)
            .unwrap();
        assert_eq!(expect(&marker_rx), Why::TimedOut);
        let (early, early_rx) = self::cell();
        p.arm(
            &c,
            Dir::Read,
            Some(Instant::now() + Duration::from_millis(100)),
            &early,
        )
        .unwrap();
        assert_eq!(expect(&early_rx), Why::TimedOut);
        assert_quiet(&late_rx);
        drop(d);
        assert_eq!(p.counts(), (1, 1));
    }

    /// Resolving a cell from outside (a cancel, a close) removes its registration and its
    /// deadline, tells the waiter once, and later data wakes nobody.
    #[test]
    fn cancel_and_close_remove_the_registration_and_run_the_callback_once() {
        let p = poller();
        let (a, mut b) = pair();
        for why in [Why::Cancelled, Why::Closed] {
            let (cell, rx) = cell();
            p.arm(
                &a,
                Dir::Read,
                Some(Instant::now() + Duration::from_secs(3600)),
                &cell,
            )
            .unwrap();
            assert_eq!(p.counts(), (1, 1));
            // From another thread, as a cancel or a close is.
            let resolver = {
                let cell = Arc::clone(&cell);
                std::thread::spawn(move || cell.fire(why))
            };
            assert!(resolver.join().unwrap());
            assert_eq!(expect(&rx), why);
            assert_clean(&p, &a);
            assert!(!cell.fire(Why::Ready), "a second resolution does something");
            assert!(matches!(
                rx.recv_timeout(CEILING),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ));
        }
        // The registration is really gone: data does not reach anyone, and a new wait works.
        b.write_all(b"x").unwrap();
        let (cell, rx) = cell();
        p.arm(&a, Dir::Read, None, &cell).unwrap();
        assert_eq!(expect(&rx), Why::Ready);
        assert_clean(&p, &a);
    }

    #[test]
    fn arming_a_cell_that_is_resolved_already_does_nothing() {
        let p = poller();
        let (a, _b) = pair();
        let (cell, rx) = cell();
        assert!(cell.fire(Why::Cancelled));
        assert_eq!(expect(&rx), Why::Cancelled);
        p.arm(
            &a,
            Dir::Read,
            Some(Instant::now() + Duration::from_secs(3600)),
            &cell,
        )
        .unwrap();
        p.timer(Instant::now() + Duration::from_secs(3600), &cell)
            .unwrap();
        assert_clean(&p, &a);
        assert_eq!(p.heap_len(), 0);
    }

    /// The deadline heap does not keep the entries of waits that ended another way for ever.
    #[test]
    fn the_heap_does_not_pile_up_stale_deadlines() {
        let p = poller();
        let cells: Vec<_> = (0..1000)
            .map(|_| {
                let (cell, rx) = cell();
                p.timer(Instant::now() + Duration::from_secs(3600), &cell)
                    .unwrap();
                (cell, rx)
            })
            .collect();
        assert_eq!(p.counts(), (0, 1000));
        for (cell, rx) in &cells {
            assert!(cell.fire(Why::Cancelled));
            assert_eq!(expect(rx), Why::Cancelled);
        }
        assert_eq!(p.counts(), (0, 0));
        assert!(p.heap_len() <= 64, "{} stale entries", p.heap_len());
    }

    /// A connect to a port where nothing listens: the writable wait that a connect in flight
    /// arms ends (Linux and macOS report the failure as an error state of the socket, Windows
    /// through its own failure event), and the socket says why.
    #[test]
    fn a_connect_to_a_closed_port_ends_the_writable_wait() {
        let p = poller();
        // A port that is closed now: a listener that was bound and dropped, as `tests/net.rs`
        // does (its refusal is real on every system). A socket that is only bound would not do:
        // macOS drops a SYN that reaches a bound socket in the closed state without a reset, so
        // that connect is never refused there, it only times out.
        //
        // The connecting socket is bound first, to a port of its own, and the listener is bound
        // after it: the two ports differ, so the connect cannot be a TCP self-connect (a socket
        // whose source port equals the destination port connects to itself when nothing
        // listens). Another process could in principle take the freed port before the connect;
        // then the connect succeeds and the test says so instead of passing.
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        socket
            .bind(&SockAddr::from(
                "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .unwrap();
        let target = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            SockAddr::from(listener.local_addr().unwrap())
        };
        socket.set_nonblocking(true).unwrap();
        // Some systems refuse a loopback connect inside the call itself (the answer comes back
        // before `connect` returns); others return "in progress" and report the refusal on the
        // socket later. Look at what this one did: only a connect in progress has a wait to
        // test, and for one that failed at once there is no connect left for a wait to end.
        let in_progress = match socket.connect(&target) {
            Ok(()) => panic!("connected to a socket that does not listen"),
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => false,
            Err(e) if e.kind() == ErrorKind::WouldBlock => true,
            #[cfg(unix)]
            Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => true,
            Err(e) => panic!("connect: {e}"),
        };
        let socket = Arc::new(socket);
        if !in_progress {
            eprintln!("poller test: the connect was refused inside the call");
        }
        if in_progress {
            let (cell, rx) = cell();
            p.arm(&socket, Dir::Write, None, &cell).unwrap();
            eprintln!("poller test: the connect was in progress; waiting for the refusal");
            let why = rx.recv_timeout(CEILING).expect(
                "the connect was in progress and the writable wait on its socket did not end \
                 within the ceiling",
            );
            assert_eq!(why, Why::Ready);
            let error = socket.take_error().unwrap();
            assert_eq!(
                error.expect("the failure is the socket's error").kind(),
                ErrorKind::ConnectionRefused
            );
        }
        assert_clean(&p, &socket);
    }

    /// A peer that resets the connection ends the write waits of a socket that was full, in the
    /// way the product code uses them: write until it would block, wait for readiness, write
    /// again, until a write fails because the peer is gone. (A wait can also end for an honest
    /// early reason, room for a few bytes, so a `Ready` alone proves nothing: the loop goes on to
    /// the failure, and a reset that never woke a write wait would end it at the ceiling.)
    #[test]
    fn a_peer_reset_ends_a_write_wait() {
        let p = poller();
        let (a, b) = pair();
        fill(&a);
        // The wait is armed before the reset comes, so that the reset is what has to wake it.
        let (cell, rx) = cell();
        p.arm(&a, Dir::Write, None, &cell).unwrap();
        // Closing with a zero linger time sends a reset instead of an orderly end.
        SockRef::from(&b).set_linger(Some(Duration::ZERO)).unwrap();
        drop(b);
        assert_eq!(expect(&rx), Why::Ready);
        let kind = write_until_it_fails(&p, &a);
        assert!(
            matches!(
                kind,
                ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::ConnectionAborted
            ),
            "the write failed with {kind:?}, which does not mean the peer is gone"
        );
        assert_clean(&p, &a);
    }

    /// An idle poller does not wake up: with a hundred waits armed, no deadline and no
    /// traffic, its loop does not go round. (It asserts that nothing happened, so a slow machine
    /// cannot fail it.) The marker timer is a point where every earlier arm has been seen: the
    /// thread has planned, slept, and now goes back to sleep without a timeout.
    #[test]
    fn an_idle_poller_never_wakes_up() {
        let p = poller();
        let pairs: Vec<_> = (0..100).map(|_| pair()).collect();
        let waits: Vec<_> = pairs
            .iter()
            .map(|(a, _)| {
                let (cell, rx) = cell();
                p.arm(a, Dir::Read, None, &cell).unwrap();
                (cell, rx)
            })
            .collect();
        let (marker, marker_rx) = cell();
        p.timer(Instant::now() + Duration::from_millis(50), &marker)
            .unwrap();
        assert_eq!(expect(&marker_rx), Why::TimedOut);
        assert_eq!(p.counts(), (100, 100));
        let seen = p.loops();
        std::thread::sleep(Duration::from_millis(300));
        // One for a spurious return that the system is allowed; a tick (50 ms, say) would add
        // six in this time.
        assert!(
            p.loops() <= seen + 1,
            "the poller went round {} times with nothing to do",
            p.loops() - seen
        );
        for (_, rx) in &waits {
            assert_quiet(rx);
        }
    }

    /// Dropping the poller stops its thread and joins it, resolves every wait that was still
    /// armed with `Why::Failed`, and releases every socket reference.
    #[test]
    fn dropping_the_poller_joins_the_thread_and_resolves_and_releases_what_it_held() {
        let p = poller();
        let gone = p.gone_probe();
        let (a, _b) = pair();
        let (c, _d) = pair();
        let (read, read_rx) = cell();
        let (write, write_rx) = self::cell();
        let (timer, timer_rx) = self::cell();
        p.arm(&a, Dir::Read, None, &read).unwrap();
        p.arm(
            &c,
            Dir::Read,
            Some(Instant::now() + Duration::from_secs(3600)),
            &write,
        )
        .unwrap();
        p.timer(Instant::now() + Duration::from_secs(3600), &timer)
            .unwrap();
        assert!(!gone());
        drop(p);
        assert!(gone(), "the thread still holds the poller's state");
        assert_eq!(Arc::strong_count(&a), 1);
        assert_eq!(Arc::strong_count(&c), 1);
        for rx in [&read_rx, &write_rx, &timer_rx] {
            assert_eq!(expect(rx), Why::Failed);
        }
        // The cells are still held here, as a cancel hook would hold them: resolved already.
        assert!(read.is_resolved() && write.is_resolved() && timer.is_resolved());
    }

    /// A stopped poller refuses new waits instead of keeping them for ever.
    #[test]
    fn a_wait_cannot_be_armed_on_a_poller_that_has_stopped() {
        let p = poller();
        let (a, _b) = pair();
        p.stop();
        let (cell, rx) = cell();
        assert!(p.arm(&a, Dir::Read, None, &cell).is_err());
        assert!(p.timer(Instant::now(), &cell).is_err());
        assert!(!p.is_alive());
        assert!(!cell.is_resolved());
        assert_quiet(&rx);
        assert_eq!(p.counts(), (0, 0));
        // Stopping again does nothing.
        p.stop();
    }

    /// M1: when the thread leaves because its wait failed, a waiter whose cell is held
    /// elsewhere as well (the cancel hook of its suspension, an object's slot) is resolved, not
    /// left hanging with the poller replaced behind its back.
    #[test]
    fn a_poller_whose_wait_failed_resolves_waiters_whose_cell_is_held_elsewhere() {
        let mut state = NetState::default();
        let p = state.poller().unwrap();
        let (a, _b) = pair();
        let (c, rx) = cell();
        let held_by_the_hook = Arc::clone(&c);
        p.arm(&a, Dir::Read, None, &c).unwrap();
        drop(c);
        // What a failed `wait` does: the thread records the failure and leaves.
        p.shared.lock().halt = Some(Halt::Failed("simulated".into()));
        p.shared.notify();
        assert_eq!(expect(&rx), Why::Failed);
        assert!(held_by_the_hook.is_resolved());
        let began = Instant::now();
        while p.is_alive() || lock(&p.thread).as_ref().is_some_and(|t| !t.is_finished()) {
            assert!(began.elapsed() < CEILING);
            std::thread::yield_now();
        }
        assert_eq!(p.counts(), (0, 0));
        assert_eq!(Arc::strong_count(&a), 1);
        // The next wait gets a new poller, and the old one refuses.
        let q = state.poller().unwrap();
        assert!(!Arc::ptr_eq(&p, &q));
        assert!(p.timer(Instant::now(), &held_by_the_hook).is_ok());
        let (d, _drx) = self::cell();
        assert!(p.timer(Instant::now(), &d).is_err());
    }

    /// M2: a callback that panics does not kill the thread silently. The others in the same batch
    /// and every other waiter are resolved (`Why::Failed` for the ones that were not due), and the
    /// poller refuses new waits.
    #[test]
    fn a_panicking_callback_fails_the_poller_and_resolves_everything_else() {
        let p = poller();
        let (a, mut ap) = pair();
        let (b, _bp) = pair();
        let (c, mut cp) = pair();
        let bad = Cell::new(|_why| panic!("callback panics"));
        p.arm(&a, Dir::Read, None, &bad).unwrap();
        let (other, other_rx) = cell();
        p.arm(&b, Dir::Read, None, &other).unwrap();
        // Due in the same batch as the bad one, after it.
        let (same_batch, same_batch_rx) = self::cell();
        p.arm(&c, Dir::Read, None, &same_batch).unwrap();
        ap.write_all(b"x").unwrap();
        cp.write_all(b"x").unwrap();
        // Both are resolved, whichever order the system reported them in.
        assert!(matches!(expect(&same_batch_rx), Why::Ready | Why::Failed));
        assert_eq!(expect(&other_rx), Why::Failed);
        let began = Instant::now();
        while p.is_alive() {
            assert!(began.elapsed() < CEILING);
            std::thread::yield_now();
        }
        let (late, _late_rx) = self::cell();
        assert!(p.arm(&c, Dir::Read, None, &late).is_err());
        assert!(!late.is_resolved());
        assert_clean(&p, &b);
        assert_clean(&p, &c);
    }

    /// A clean-up that panics does not keep the callback from running.
    #[test]
    fn a_cleanup_that_panics_does_not_keep_the_callback_from_running() {
        let (cell, rx) = cell();
        assert!(cell.on_resolve(|| panic!("clean-up panics")));
        let outcome = catch_unwind(AssertUnwindSafe(|| cell.fire(Why::Closed)));
        assert!(outcome.is_err(), "the panic is passed on");
        assert_eq!(expect(&rx), Why::Closed);
    }

    /// The callback runs after the clean-ups: when it runs, the registrations of its wait are gone.
    #[test]
    fn the_callback_runs_after_the_cleanups() {
        let cleaned = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(AtomicUsize::new(usize::MAX));
        let cell = {
            let (cleaned, seen) = (Arc::clone(&cleaned), Arc::clone(&seen));
            Cell::new(move |_| seen.store(cleaned.load(Ordering::SeqCst), Ordering::SeqCst))
        };
        for _ in 0..3 {
            let cleaned = Arc::clone(&cleaned);
            assert!(cell.on_resolve(move || {
                cleaned.fetch_add(1, Ordering::SeqCst);
            }));
        }
        assert!(cell.fire(Why::Ready));
        assert_eq!(seen.load(Ordering::SeqCst), 3);
    }

    /// A refused arm leaves no clean-up on the cell, and a resolved cell armed again does
    /// nothing, also where the direction is taken.
    #[test]
    fn a_refused_or_resolved_arm_leaves_nothing_on_the_cell() {
        let p = poller();
        let (a, _b) = pair();
        let (first, _first_rx) = cell();
        p.arm(&a, Dir::Read, None, &first).unwrap();
        let (second, second_rx) = self::cell();
        assert!(p.arm(&a, Dir::Read, None, &second).is_err());
        assert_eq!(lock(&second.state).cleanups.len(), 0);
        // A read on a second idle pair: it cannot end by itself, so the counts are certain.
        let (c, _d) = pair();
        assert!(p.arm(&c, Dir::Read, None, &second).is_ok());
        assert_eq!(lock(&second.state).cleanups.len(), 1);
        assert_eq!(p.counts(), (2, 2));
        assert!(second.fire(Why::Cancelled));
        assert_eq!(expect(&second_rx), Why::Cancelled);
        assert!(
            p.arm(&c, Dir::Read, None, &second).is_ok(),
            "resolved: does nothing"
        );
        assert_eq!(p.counts(), (1, 1));
        assert!(first.fire(Why::Closed));
    }

    /// S1: waits that end by an event leave no stale deadline behind, also while one live early
    /// deadline sits at the top of the heap.
    #[test]
    fn deadlines_of_waits_ended_by_events_do_not_pile_up_either() {
        let p = poller();
        let (a, _ap) = pair();
        let (pin, _pin_rx) = cell();
        p.arm(
            &a,
            Dir::Read,
            Some(Instant::now() + Duration::from_secs(3600)),
            &pin,
        )
        .unwrap();
        let (b, mut bp) = pair();
        let mut byte = [0u8; 1];
        for _ in 0..500 {
            let (c, rx) = self::cell();
            p.arm(
                &b,
                Dir::Read,
                Some(Instant::now() + Duration::from_secs(7200)),
                &c,
            )
            .unwrap();
            bp.write_all(b"x").unwrap();
            assert_eq!(expect(&rx), Why::Ready);
            let began = Instant::now();
            while (&*b).read(&mut byte).is_err() {
                assert!(began.elapsed() < CEILING);
                std::thread::yield_now();
            }
        }
        assert!(
            p.heap_len() <= 64 + 2,
            "{} entries for one live deadline",
            p.heap_len()
        );
        assert_eq!(p.counts(), (1, 1));
    }

    /// S2: what the thread asks the system to sleep is bounded, however far the deadline is.
    #[test]
    fn the_sleep_asked_of_the_system_is_bounded() {
        let now = Instant::now();
        assert_eq!(sleep_for(None, now), None);
        let short = now + Duration::from_millis(50);
        assert_eq!(sleep_for(Some(short), now), Some(Duration::from_millis(50)));
        let years = now
            .checked_add(Duration::from_secs(10 * 365 * 86_400))
            .unwrap_or(now + Duration::from_secs(100 * 86_400));
        assert_eq!(sleep_for(Some(years), now), Some(MAX_SLEEP));
        assert_eq!(
            sleep_for(Some(now), now + Duration::from_secs(1)),
            Some(Duration::ZERO)
        );
    }

    /// A wait with a deadline of years next to a short one: both are planned, the short one
    /// fires, and the poller goes on to hold the long one.
    #[test]
    fn a_deadline_of_years_does_not_disturb_a_short_one() {
        let p = poller();
        let (a, _ap) = pair();
        let (c, _cp) = pair();
        let far = Instant::now()
            .checked_add(Duration::from_secs(10 * 365 * 86_400))
            .unwrap_or(Instant::now() + Duration::from_secs(100 * 86_400));
        let (long, long_rx) = cell();
        p.arm(&a, Dir::Read, Some(far), &long).unwrap();
        let (short, short_rx) = self::cell();
        p.arm(
            &c,
            Dir::Read,
            Some(Instant::now() + Duration::from_millis(50)),
            &short,
        )
        .unwrap();
        assert_eq!(expect(&short_rx), Why::TimedOut);
        assert!(p.is_alive());
        assert_quiet(&long_rx);
        assert_eq!(p.counts(), (1, 1));
    }

    /// S4: the state of the `Vm` stops the thread, whoever else holds the poller; a closure that
    /// holds the poller (against the rule) is dropped on the poller's own thread without a join of
    /// itself, and the thread is gone after the state is dropped.
    #[test]
    fn the_state_stops_the_thread_even_if_someone_else_holds_the_poller() {
        let mut state = NetState::default();
        let held = state.poller().unwrap();
        let (a, _b) = pair();
        // Against the rule on purpose: the callback owns the poller, so the last reference is
        // dropped on the poller thread when the wait is resolved there.
        let (tx, rx) = mpsc::channel();
        let owner = Arc::clone(&held);
        let cell = Cell::new(move |why| {
            let _ = tx.send(why);
            drop(owner);
        });
        held.arm(&a, Dir::Read, None, &cell).unwrap();
        drop(cell);
        let gone = held.gone_probe();
        drop(state);
        assert_eq!(expect(&rx), Why::Failed);
        assert!(!held.is_alive(), "the thread outlived the state");
        let (late, _late_rx) = self::cell();
        assert!(held.timer(Instant::now(), &late).is_err());
        assert!(!gone());
        drop(held);
        assert!(gone());
    }

    /// The last reference to a poller dropped on the poller's own thread (by a callback that owns
    /// it, against the rule) must not try to join that thread: the stop is a stop, not a failure.
    #[test]
    fn the_last_reference_dropped_on_the_poller_thread_stops_it_without_a_failure() {
        let p = Arc::new(poller());
        let shared = p.shared_for_test();
        let (a, mut b) = pair();
        let (tx, rx) = mpsc::channel();
        let owner = Arc::clone(&p);
        let cell = Cell::new(move |why| {
            let _ = tx.send(why);
            drop(owner);
        });
        p.arm(&a, Dir::Read, None, &cell).unwrap();
        drop((cell, p));
        b.write_all(b"x").unwrap();
        assert_eq!(expect(&rx), Why::Ready);
        // The thread is done when nothing but this test holds the shared state.
        let began = Instant::now();
        while Arc::strong_count(&shared) != 1 {
            assert!(began.elapsed() < CEILING, "the poller thread never left");
            std::thread::yield_now();
        }
        assert!(
            !shared.panicked.load(Ordering::SeqCst),
            "the poller failed instead of stopping"
        );
        assert!(matches!(shared.lock().halt, Some(Halt::Stopped)));
    }

    /// A read and a write waiting together on a socket whose peer resets it: both are told.
    #[test]
    fn a_peer_reset_ends_a_read_and_a_write_waiting_together() {
        let p = poller();
        let (a, b) = pair();
        fill(&a);
        let (write_cell, write_rx) = cell();
        let (read_cell, read_rx) = self::cell();
        p.arm(&a, Dir::Write, None, &write_cell).unwrap();
        p.arm(&a, Dir::Read, None, &read_cell).unwrap();
        let early = both_or_write_already_ended(&p, &write_rx);
        SockRef::from(&b).set_linger(Some(Duration::ZERO)).unwrap();
        drop(b);
        assert_eq!(expect(&read_rx), Why::Ready);
        assert_eq!(early.unwrap_or_else(|| expect(&write_rx)), Why::Ready);
        assert_clean(&p, &a);
    }

    #[test]
    fn the_state_of_a_vm_starts_one_poller_and_keeps_it() {
        let mut state = NetState::default();
        let first = state.poller().unwrap();
        let second = state.poller().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let gone = first.gone_probe();
        drop((first, second));
        assert!(!gone(), "the state keeps the poller");
        drop(state);
        assert!(gone());
    }

    /// Many sockets wait on one thread; each wake-up reaches the right waiter.
    #[test]
    fn many_waits_each_get_their_own_event() {
        let p = poller();
        let mut pairs: Vec<_> = (0..200).map(|_| pair()).collect();
        let waits: Vec<_> = pairs
            .iter()
            .map(|(a, _)| {
                let (cell, rx) = cell();
                p.arm(a, Dir::Read, None, &cell).unwrap();
                rx
            })
            .collect();
        assert_eq!(p.counts(), (200, 200));
        // The odd ones get data, in the order of the list; the even ones stay quiet.
        for (i, (_, peer)) in pairs.iter_mut().enumerate() {
            if i % 2 == 1 {
                peer.write_all(b"x").unwrap();
            }
        }
        for (i, rx) in waits.iter().enumerate() {
            if i % 2 == 1 {
                assert_eq!(expect(rx), Why::Ready, "wait {i}");
            }
        }
        for (i, rx) in waits.iter().enumerate() {
            if i % 2 == 0 {
                assert_quiet(rx);
            }
        }
        assert_eq!(p.counts(), (100, 100));
    }

    /// Arming while the thread sleeps is noticed (on Windows a change made just as the thread
    /// goes to sleep is only seen if it is woken; see the module comment).
    #[test]
    fn a_wait_armed_while_the_poller_sleeps_is_noticed() {
        let p = poller();
        let (a, mut b) = pair();
        for round in 0..300 {
            let (cell, rx) = cell();
            p.arm(&a, Dir::Read, None, &cell).unwrap();
            b.write_all(b"x").unwrap();
            assert_eq!(expect(&rx), Why::Ready, "round {round}");
            let mut byte = [0u8; 1];
            // Take the byte back out (it may still be in flight on the loopback).
            let began = Instant::now();
            while (&*a).read(&mut byte).is_err() {
                assert!(began.elapsed() < CEILING);
                std::thread::yield_now();
            }
        }
        assert_clean(&p, &a);
    }

    /// The poller's event, a close and a cancel all reach one cell at once, thousands of times:
    /// the callback runs exactly once, the registration is deleted, and the socket reference is
    /// dropped, whichever of them wins.
    #[test]
    fn an_event_a_close_and_a_cancel_racing_on_one_cell_resolve_it_once() {
        let p = poller();
        let (a, b) = pair();
        let mut seen = [0usize; 3];
        for round in 0..2000 {
            let calls = Arc::new(AtomicUsize::new(0));
            let (tx, rx) = mpsc::channel();
            let cell = {
                let calls = Arc::clone(&calls);
                Cell::new(move |why| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(why);
                })
            };
            p.arm(
                &a,
                Dir::Read,
                Some(Instant::now() + Duration::from_secs(3600)),
                &cell,
            )
            .unwrap();
            let start = Arc::new(Barrier::new(3));
            let actors: Vec<_> = (0..3)
                .map(|who| {
                    let (start, cell) = (Arc::clone(&start), Arc::clone(&cell));
                    let mut peer = (who == 0).then(|| b.try_clone().unwrap());
                    std::thread::spawn(move || {
                        start.wait();
                        match who {
                            0 => {
                                let _ = peer.as_mut().unwrap().write(b"x");
                            }
                            1 => {
                                cell.fire(Why::Closed);
                            }
                            _ => {
                                cell.fire(Why::Cancelled);
                            }
                        }
                    })
                })
                .collect();
            for actor in actors {
                actor.join().unwrap();
            }
            // Whoever won, the callback has run (or is running on the poller thread: wait for
            // it), and by then its clean-ups have run.
            let why = expect(&rx);
            seen[match why {
                Why::Ready => 0,
                Why::Closed => 1,
                Why::Cancelled => 2,
                other => panic!("round {round}: unexpected reason {other:?}"),
            }] += 1;
            assert!(
                matches!(
                    rx.recv_timeout(CEILING),
                    Err(mpsc::RecvTimeoutError::Disconnected)
                ),
                "round {round}: a second message"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1, "round {round}");
            assert_clean(&p, &a);
            assert!(p.heap_len() <= 64, "stale deadlines pile up");
            // The byte that the writer may have sent is for the next round to find: take it.
            let began = Instant::now();
            let mut byte = [0u8; 1];
            loop {
                match (&*a).read(&mut byte) {
                    Ok(_) => break,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        // Not here yet, or the writer's write was never made (it is always
                        // made: the barrier releases it). Wait for it.
                        assert!(began.elapsed() < CEILING, "round {round}: no byte");
                        std::thread::yield_now();
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        }
        // Every kind of winner is legal; the invariants above held for each of them.
        assert_eq!(seen.iter().sum::<usize>(), 2000);
        drop(b);
    }

    /// A spin of 0 to 2,000 iterations, different every round (a small xorshift: the point is
    /// only that the closer's arrival is spread over the time the poller thread needs to take
    /// an event, so that both orders happen).
    fn jitter(round: u64) -> u64 {
        let mut x = round.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % 2001
    }

    fn spin(iterations: u64) {
        for _ in 0..iterations {
            std::hint::spin_loop();
        }
    }

    /// The rule of the table: a socket is released under the table lock, so that when the call
    /// that ended a wait (a close, here) has returned, the table no longer holds the socket, also
    /// when the poller thread is handling an event on it at the same time. The closer waits a
    /// random spin before it fires, so that it wins about as often as it loses.
    #[test]
    fn a_closer_racing_an_event_finds_the_socket_released_when_it_returns() {
        let p = poller();
        let mut closed = 0;
        for round in 0..8000u64 {
            let (a, mut b) = pair();
            let (cell, rx) = cell();
            p.arm(&a, Dir::Read, None, &cell).unwrap();
            let start = Arc::new(Barrier::new(2));
            let writer = {
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    let _ = b.write(b"x");
                    b
                })
            };
            start.wait();
            spin(jitter(round));
            if cell.fire(Why::Closed) {
                closed += 1;
                assert_eq!(
                    Arc::strong_count(&a),
                    1,
                    "round {round}: the table held the socket after the close returned"
                );
                assert_eq!(p.counts(), (0, 0), "round {round}");
                assert_eq!(expect(&rx), Why::Closed);
            } else {
                // The event won: the callback runs on the poller thread; the table's hold was
                // released before it was called.
                assert_eq!(expect(&rx), Why::Ready);
                assert_clean(&p, &a);
            }
            let _b = writer.join().unwrap();
        }
        eprintln!("poller test: the closer won {closed} of 8000 rounds");
    }

    /// A listener that the table was the last holder of is closed when the close returns, also
    /// when a connection arrives (an event for the poller thread to take) while the closer
    /// comes. "Closed" is that nobody holds the listener any more: the test's own reference is
    /// dropped before the race, so the table's is the last, and its drop is the close of the
    /// descriptor. (The port is not bound again to look: a connection that arrived is torn down
    /// by the system for a while after the close, and the port stays in use meanwhile.)
    #[test]
    fn a_listener_the_table_held_is_closed_when_the_close_returns() {
        let p = poller();
        let mut closed = 0;
        for round in 0..4000u64 {
            let listener = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
            let addr = listener.local_addr().unwrap();
            let held = Arc::downgrade(&listener);
            let (cell, rx) = cell();
            p.arm(&listener, Dir::Read, None, &cell).unwrap();
            drop(listener);
            let start = Arc::new(Barrier::new(2));
            let client = {
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    // Started and let go, never waited for: a connect that is refused (the close
                    // came first, one of the orders) may take seconds to be told so on some
                    // systems, and the result does not depend on it.
                    let socket =
                        Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
                    socket.set_nonblocking(true).unwrap();
                    let _ = socket.connect(&SockAddr::from(addr));
                    drop(socket);
                })
            };
            start.wait();
            spin(jitter(round));
            if cell.fire(Why::Closed) {
                closed += 1;
                assert_eq!(
                    held.strong_count(),
                    0,
                    "round {round}: the table held the listener after the close returned"
                );
                assert_eq!(expect(&rx), Why::Closed);
            } else {
                // The event won: the table's hold was released before the callback ran.
                assert_eq!(expect(&rx), Why::Ready);
                assert_eq!(held.strong_count(), 0, "round {round}");
            }
            client.join().unwrap();
        }
        eprintln!("poller test: the closer won {closed} of 4000 rounds");
    }
}
