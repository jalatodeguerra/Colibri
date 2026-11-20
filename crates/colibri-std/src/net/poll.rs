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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

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
    /// cell, after the clean-ups.
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
        for cleanup in cleanups {
            cleanup();
        }
        wake(why);
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
    read: Option<u64>,
    write: Option<u64>,
}

struct Waiter {
    cell: Arc<Cell>,
    /// The registration this waits on; `None` for a timer.
    reg: Option<u64>,
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
    /// The next key of a registration or a waiter (one counter, never reused).
    next: u64,
    regs: HashMap<u64, Reg>,
    by_raw: HashMap<u64, u64>,
    waiters: HashMap<u64, Waiter>,
    /// `(deadline, waiter)` of the waiters with a deadline, plus stale entries of waiters that
    /// ended another way (skipped when they come up; the heap is rebuilt when they outnumber the
    /// live ones).
    heap: BinaryHeap<Reverse<(Instant, u64)>>,
    live_deadlines: usize,
    /// The deadline the thread is sleeping towards (`None`: no deadline, or not asleep yet): an
    /// arm with an earlier deadline wakes it so that it plans again.
    planned: Option<Instant>,
    /// Cells to resolve once the lock is released.
    to_fire: Vec<(Arc<Cell>, Why)>,
}

struct Shared {
    os: polling::Poller,
    table: Mutex<Table>,
    /// How many times the thread went round its loop. A poller that sleeps without a timeout
    /// does not move it while nothing happens; one that ticks does (a test looks).
    loops: AtomicU64,
}

/// Resolves the cells that a locked operation collected, after the lock is gone.
fn fire_all(to_fire: Vec<(Arc<Cell>, Why)>) {
    for (cell, why) in to_fire {
        cell.fire(why);
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Table> {
        lock(&self.table)
    }

    fn unlock_and_fire(&self, mut table: MutexGuard<'_, Table>) {
        let to_fire = std::mem::take(&mut table.to_fire);
        drop(table);
        fire_all(to_fire);
    }

    fn notify(&self) {
        let _ = self.os.notify();
    }

    /// Takes a waiter out of the table, and its socket's registration along with it when it was
    /// the last waiter there. Idempotent: the id of a waiter that is gone does nothing.
    fn remove_waiter(&self, t: &mut Table, id: u64) -> Option<Waiter> {
        let waiter = t.waiters.remove(&id)?;
        if waiter.deadline.is_some() {
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
        if let Some(reg) = waiter.reg {
            if let Some(r) = t.regs.get_mut(&reg) {
                match waiter.dir {
                    Dir::Read => r.read = None,
                    Dir::Write => r.write = None,
                }
            }
            self.rearm(t, reg);
        }
        Some(waiter)
    }

    /// After the interest of a registration changed or an event used up its one shot: deletes
    /// the registration when nothing waits on it any more (and only then releases the socket
    /// reference), otherwise arms what remains.
    fn rearm(&self, t: &mut Table, key: u64) {
        let Some(reg) = t.regs.get(&key) else { return };
        if reg.read.is_none() && reg.write.is_none() {
            let reg = t.regs.remove(&key).expect("looked up above");
            t.by_raw.remove(&reg.raw);
            // Failing here means the system has dropped the registration already; there is
            // nothing left to undo. The reference is released after the call, never before.
            let _ = self.os.delete(borrowed(&*reg.socket));
            drop(reg);
            return;
        }
        let event = polling::Event::new(key as usize, reg.read.is_some(), reg.write.is_some());
        if self.os.modify(borrowed(&*reg.socket), event).is_err() {
            // The remaining waiters cannot be armed: wake them, so that they try their system
            // call and find out why (and fail to arm again, with an error of their own).
            let ids = [reg.read, reg.write];
            for id in ids.into_iter().flatten() {
                if let Some(w) = t.waiters.remove(&id) {
                    if w.deadline.is_some() {
                        t.live_deadlines -= 1;
                    }
                    t.to_fire.push((w.cell, Why::Ready));
                }
            }
            let reg = t.regs.remove(&key).expect("looked up above");
            t.by_raw.remove(&reg.raw);
            let _ = self.os.delete(borrowed(&*reg.socket));
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
            if live && let Some(waiter) = self.remove_waiter(t, id) {
                t.to_fire.push((waiter.cell, Why::TimedOut));
            }
        }
        None
    }

    /// An event of the operating system's poller for the registration `key`.
    fn event(&self, t: &mut Table, key: u64, readable: bool, writable: bool) {
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
                if w.deadline.is_some() {
                    t.live_deadlines -= 1;
                }
                t.to_fire.push((w.cell, Why::Ready));
            }
        }
        // The one shot is used up whatever came: arm what is left, or delete.
        self.rearm(t, key);
    }
}

/// The thread's loop.
fn run(shared: &Shared) {
    let mut events = polling::Events::with_capacity(NonZeroUsize::new(EVENTS).expect("not zero"));
    loop {
        shared.loops.fetch_add(1, Ordering::Relaxed);
        let planned = {
            let mut t = shared.lock();
            if t.halt.is_some() {
                break;
            }
            let planned = shared.expire(&mut t, Instant::now());
            t.planned = planned;
            shared.unlock_and_fire(t);
            planned
        };
        events.clear();
        let timeout = planned.map(|d| d.saturating_duration_since(Instant::now()));
        if let Err(e) = shared.os.wait(&mut events, timeout) {
            let mut t = shared.lock();
            if t.halt.is_none() {
                t.halt = Some(Halt::Failed(e.to_string()));
            }
            break;
        }
        let mut t = shared.lock();
        for event in events.iter() {
            shared.event(&mut t, event.key as u64, event.readable, event.writable);
        }
        shared.unlock_and_fire(t);
    }
    // Leaving: delete every registration before its socket reference is released, then drop the
    // waiters unresolved (their callbacks are dropped with them: a waiter that holds a
    // completer is woken by that with "ended without a result", and every other holder finds
    // the cell empty).
    let mut t = shared.lock();
    let regs: Vec<Reg> = t.regs.drain().map(|(_, r)| r).collect();
    t.by_raw.clear();
    for reg in regs {
        let _ = shared.os.delete(borrowed(&*reg.socket));
    }
    t.waiters.clear();
    t.heap.clear();
    t.live_deadlines = 0;
    t.to_fire.clear();
}

/// Why a poller could not be started.
#[derive(Debug)]
pub(super) enum StartError {
    /// The system would not make the poller (out of descriptors, say).
    System(io::Error),
    /// The system would not start its thread.
    Thread(io::Error),
}

/// The poller thread of one `Vm`. Dropping it stops the thread and joins it.
pub(super) struct Poller {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
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
            }),
            loops: AtomicU64::new(0),
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
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Whether the thread runs and takes waits (not stopped, not broken by a failed wait).
    pub(super) fn is_alive(&self) -> bool {
        self.shared.lock().halt.is_none()
            && lock(&self.thread)
                .as_ref()
                .is_some_and(|t| !t.is_finished())
    }

    /// Waits for `source` to be readable or writable (`dir`) until `deadline`, and resolves
    /// `cell` with [`Why::Ready`] or [`Why::TimedOut`]. If the socket is ready already it
    /// reports at once (through the thread, not from this call). The poller holds a reference to
    /// `source` until the wait ends, whichever way it ends. Does nothing if `cell` is resolved
    /// already. An `Err` means nothing was armed and the cell is still unresolved; the caller makes
    /// its system call to learn why, or fails.
    ///
    /// At most one wait per direction on one socket: a second is an error (the queues of turns
    /// of the objects never allow it).
    pub(super) fn arm<S: Source>(
        &self,
        source: &Arc<S>,
        dir: Dir,
        deadline: Option<Instant>,
        cell: &Arc<Cell>,
    ) -> io::Result<()> {
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
        let weak = Arc::downgrade(&self.shared);
        let removal = move || {
            if let Some(shared) = weak.upgrade() {
                let mut t = shared.lock();
                shared.remove_waiter(&mut t, id);
                shared.unlock_and_fire(t);
            }
        };
        if !cell.on_resolve(removal) {
            return Ok(());
        }
        let reg_key = match key {
            Some(k) => {
                let reg = t.regs.get_mut(&k).expect("by_raw and regs agree");
                match dir {
                    Dir::Read => reg.read = Some(id),
                    Dir::Write => reg.write = Some(id),
                }
                let event =
                    polling::Event::new(k as usize, reg.read.is_some(), reg.write.is_some());
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
                let event = polling::Event::new(k as usize, dir == Dir::Read, dir == Dir::Write);
                // SAFETY: the table owns `source` (an `Arc`) from here until the registration
                // is deleted, and every path that removes a `Reg` from the table (`rearm`,
                // the exit of the thread) calls `delete` before it drops the `Reg`, so the
                // socket is open and the same one for as long as it is registered.
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
        drop(t);
        // Windows needs the thread woken for every change (see the module comment).
        if cfg!(windows) || wake {
            self.shared.notify();
        }
        Ok(())
    }

    /// Resolves `cell` with [`Why::TimedOut`] at `deadline`, with no socket involved. Does
    /// nothing if `cell` is resolved already.
    pub(super) fn timer(&self, deadline: Instant, cell: &Arc<Cell>) -> io::Result<()> {
        let mut t = self.shared.lock();
        if let Some(halt) = &t.halt {
            return Err(halt_error(halt));
        }
        let id = t.next;
        t.next += 1;
        let weak = Arc::downgrade(&self.shared);
        let removal = move || {
            if let Some(shared) = weak.upgrade() {
                let mut t = shared.lock();
                shared.remove_waiter(&mut t, id);
                shared.unlock_and_fire(t);
            }
        };
        if !cell.on_resolve(removal) {
            return Ok(());
        }
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
        drop(t);
        if wake {
            self.shared.notify();
        }
        Ok(())
    }

    /// Records the deadline of a new waiter; whether the thread must be woken to plan again
    /// (the deadline is earlier than the one it sleeps towards, or it sleeps without one).
    fn note_deadline(&self, t: &mut Table, id: u64, deadline: Option<Instant>) -> bool {
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
        {
            let mut t = self.shared.lock();
            if t.halt.is_none() {
                t.halt = Some(Halt::Stopped);
            }
        }
        self.shared.notify();
        if let Some(thread) = lock(&self.thread).take() {
            // A thread that panicked has nothing more to say here.
            let _ = thread.join();
        }
    }
}

/// What the `Vm` keeps for `net`: the poller, made by the first wait that needs it and joined
/// when the `Vm` is dropped (the `Vm`'s host state drops with it).
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

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
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

    /// A connected loopback pair, both ends non-blocking: the end that waits (shared, as the
    /// poller holds a reference to it) and its peer.
    fn pair() -> (Arc<TcpStream>, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        (Arc::new(server), peer)
    }

    /// Writes until the socket takes no more; how many bytes that was.
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

    /// Nothing is registered and nobody waits, and the table holds no reference to `socket`.
    fn assert_clean<S>(poller: &Poller, socket: &Arc<S>) {
        assert_eq!(poller.counts(), (0, 0));
        assert_eq!(Arc::strong_count(socket), 1);
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
        assert_quiet(&rx);
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
        assert_eq!(p.counts(), (1, 2), "one registration, two waiters");
        assert_quiet(&write_rx);
        assert_quiet(&read_rx);
        // Data for the read: the read ends. The write may or may not be told too (a wake-up
        // may be spurious); either way it ends when the peer has made room.
        b.write_all(b"x").unwrap();
        assert_eq!(expect(&read_rx), Why::Ready);
        assert_eq!(drain_until(&b, &write_rx), Why::Ready);
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
        // A socket that is bound and does not listen: nothing can accept there, and its port is
        // not given to another test meanwhile.
        let held = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        held.bind(&SockAddr::from(
            "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .unwrap();
        let target = held.local_addr().unwrap();
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        socket.set_nonblocking(true).unwrap();
        let at_once = match socket.connect(&target) {
            Ok(()) => panic!("connected to a socket that does not listen"),
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => true,
            Err(e) if e.kind() == ErrorKind::WouldBlock => false,
            #[cfg(unix)]
            Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => false,
            Err(e) => panic!("connect: {e}"),
        };
        let socket = Arc::new(socket);
        let (cell, rx) = cell();
        p.arm(&socket, Dir::Write, None, &cell).unwrap();
        assert_eq!(expect(&rx), Why::Ready);
        let error = socket.take_error().unwrap();
        if !at_once {
            assert_eq!(
                error.expect("the failure is the socket's error").kind(),
                ErrorKind::ConnectionRefused
            );
        }
        assert_clean(&p, &socket);
        drop(held);
    }

    /// A peer that resets the connection wakes a write wait whose socket is full (so it was
    /// not ready for writing before): the system reports the reset to a write interest too.
    #[test]
    fn a_peer_reset_ends_a_write_wait() {
        let p = poller();
        let (a, b) = pair();
        fill(&a);
        let (cell, rx) = cell();
        p.arm(&a, Dir::Write, None, &cell).unwrap();
        assert_quiet(&rx);
        // Closing with a zero linger time sends a reset instead of an orderly end.
        SockRef::from(&b).set_linger(Some(Duration::ZERO)).unwrap();
        drop(b);
        assert_eq!(expect(&rx), Why::Ready);
        assert!(
            (&*a).write(b"x").is_err(),
            "the write should fail after a reset"
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

    /// Dropping the poller stops its thread and joins it, and releases every socket reference
    /// and every callback of the waits that were still armed.
    #[test]
    fn dropping_the_poller_joins_the_thread_and_releases_what_it_held() {
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
        // Only the table holds the cells from here on.
        drop((read, write, timer));
        drop(p);
        assert!(gone(), "the thread still holds the poller's state");
        assert_eq!(Arc::strong_count(&a), 1);
        assert_eq!(Arc::strong_count(&c), 1);
        for rx in [&read_rx, &write_rx, &timer_rx] {
            // Unresolved: the callbacks were dropped, not run.
            assert!(matches!(
                rx.recv_timeout(CEILING),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ));
        }
    }

    /// A stopped poller refuses new waits instead of keeping them for ever.
    #[test]
    fn a_wait_cannot_be_armed_on_a_poller_that_has_stopped() {
        let p = poller();
        let shared = Arc::clone(&p.shared);
        let (a, _b) = pair();
        drop(p);
        let (cell, rx) = cell();
        let stopped = Poller {
            shared,
            thread: Mutex::new(None),
        };
        assert!(stopped.arm(&a, Dir::Read, None, &cell).is_err());
        assert!(stopped.timer(Instant::now(), &cell).is_err());
        assert!(!stopped.is_alive());
        assert_quiet(&rx);
        // Dropping the stand-in is harmless.
        drop(stopped);
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
}
