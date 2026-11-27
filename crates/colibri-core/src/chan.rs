//! Channels between thread tasks (DESIGN §14.1, ARCHITECTURE §36.5).
//!
//! A `Channel[T]` the program holds is a class object of `CHANNEL_TYPE` that names an entry of the
//! [`ChanTable`] of its [`Core`]; the entry is an `Arc<ChanInner>`, the queue every isolate that
//! has the channel shares. The handle crosses threads as that `Arc` ([`ChanHandle`] is a
//! [`CrossesAsIs`]), and what is sent crosses as a [`Portable`], so no reference count is ever
//! shared between threads.
//!
//! # Waiting
//!
//! A receive on an empty channel and a send on a full one wait. The waiter is a [`Completer`] of
//! the scheduler of the isolate that waits, kept in the channel with a number of its own; whoever
//! changes the channel in a way that matters to the waiter completes it, which wakes that
//! isolate's event loop (a coroutine parked on `Wake::External`, the main fiber's loop, or a
//! thread blocked on the isolate's completions). What the wake-up is about is put in `done`,
//! under the same lock, so a woken waiter always finds it:
//!
//! - `send` hands the value straight to the first receiver that waits (`Done::Got`), so the queue
//!   stays empty while there is a receiver;
//! - `recv` that frees a place in a full channel moves the value of the first sender that waits
//!   into the queue, and wakes it (`Done::Sent`), so blocked senders go in order;
//! - `close` wakes every waiter (`Done::Closed`).
//!
//! A waiter that gives up (its coroutine was cancelled, the `Vm` ends) runs a cancel hook of its
//! token, which takes it out of the channel; a value that was handed to it and not collected goes
//! back to the next receiver, or to the front of the queue. So a value is never lost to a waiter
//! that is gone.
//!
//! # Not detected
//!
//! A channel is closed only by `close()`. Dropping handles closes nothing, and a `recv` that can
//! never be satisfied waits for ever (v1, plan §7).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use colibri_check::types::CHANNEL_TYPE;

use crate::external::{CancelToken, Completer};
use crate::heap::ClassObj;
use crate::portable::{CrossesAsIs, Portable};
use crate::state::Core;
use crate::value::Value;

/// What a woken waiter finds.
#[derive(Debug)]
pub enum Done {
    /// A receiver: the value that was sent to it.
    Got(Portable),
    /// A sender: its value is in the queue.
    Sent,
    /// The channel was closed while it waited (a receiver gets `none`, a sender panics).
    Closed,
}

struct Waiter {
    id: u64,
    wake: Completer<()>,
}

struct State {
    queue: VecDeque<Portable>,
    closed: bool,
    /// Receivers that wait, in order. Not empty only while `queue` is.
    receivers: VecDeque<Waiter>,
    /// Senders that wait, each with its value, in order. Not empty only while the queue is full.
    senders: VecDeque<(Waiter, Portable)>,
    /// What woken waiters have not collected yet, by waiter.
    done: HashMap<u64, Done>,
    next: u64,
}

/// The queue every isolate that holds the channel shares.
pub struct ChanInner {
    state: Mutex<State>,
    capacity: Option<usize>,
}

// What a worker sends to another isolate is `Send`; the channel itself is shared by all.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<ChanInner>();
};

/// A waiter registered in a channel: what the isolate that waits keeps of it.
#[derive(Clone)]
pub struct Ticket {
    /// The waiter's number in the channel.
    pub id: u64,
    /// The scheduler's number of the operation that wakes the isolate.
    pub op: u64,
    /// Cancelling it takes the waiter out of the channel (it runs the hook registered with it).
    pub token: Arc<CancelToken>,
}

/// What `send` found.
pub enum Sent {
    Done,
    /// The channel is closed.
    Closed,
    /// The channel is full: the value waits in the channel for room, the caller waits for the
    /// ticket.
    Wait(Ticket),
}

/// What `recv` found.
pub enum Received {
    Item(Portable),
    /// The channel is closed and empty.
    End,
    /// Nothing is there: the caller waits for the ticket.
    Wait(Ticket),
}

impl ChanInner {
    /// A channel with room for `capacity` values, or without a limit.
    pub fn new(capacity: Option<usize>) -> Arc<ChanInner> {
        Arc::new(ChanInner {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                closed: false,
                receivers: VecDeque::new(),
                senders: VecDeque::new(),
                done: HashMap::new(),
                next: 0,
            }),
            capacity,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic cannot leave the state half-changed (every change is a few moves with nothing
        // in between that can fail), so a poisoned lock is still good.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether a `send` can have to wait (the channel has a capacity).
    pub fn is_bounded(&self) -> bool {
        self.capacity.is_some()
    }

    fn has_room(&self, s: &State) -> bool {
        self.capacity.is_none_or(|c| s.queue.len() < c)
    }

    /// Sends `value`. `wait` makes the waiter's completer when the channel is full; it runs under
    /// the channel's lock, so a wake-up cannot slip between the check and the registration.
    pub fn send(
        self: &Arc<Self>,
        value: Portable,
        wait: &mut dyn FnMut() -> Completer<()>,
    ) -> Sent {
        let mut s = self.lock();
        if s.closed {
            return Sent::Closed;
        }
        if let Some(w) = s.receivers.pop_front() {
            s.done.insert(w.id, Done::Got(value));
            w.wake.complete(());
            return Sent::Done;
        }
        if self.has_room(&s) {
            s.queue.push_back(value);
            return Sent::Done;
        }
        let ticket = Self::register(&mut s, wait, |w| Entry::Sender(w, value));
        drop(s);
        self.on_give_up(&ticket);
        Sent::Wait(ticket)
    }

    /// Takes the next value, or says why there is none.
    pub fn recv(self: &Arc<Self>, wait: &mut dyn FnMut() -> Completer<()>) -> Received {
        let mut s = self.lock();
        if let Some(item) = self.pop(&mut s) {
            return Received::Item(item);
        }
        if s.closed {
            return Received::End;
        }
        let ticket = Self::register(&mut s, wait, Entry::Receiver);
        drop(s);
        self.on_give_up(&ticket);
        Received::Wait(ticket)
    }

    /// The next value if there is one, without waiting.
    pub fn try_recv(&self) -> Option<Portable> {
        let mut s = self.lock();
        self.pop(&mut s)
    }

    /// Takes a value off the queue, and lets the first blocked sender into the room it leaves.
    fn pop(&self, s: &mut State) -> Option<Portable> {
        let item = s.queue.pop_front()?;
        while self.has_room(s) {
            let Some((w, v)) = s.senders.pop_front() else {
                break;
            };
            s.queue.push_back(v);
            s.done.insert(w.id, Done::Sent);
            w.wake.complete(());
        }
        Some(item)
    }

    /// Closes the channel: what is queued can still be received, every waiter is woken, and a send
    /// is refused from now on. Closing again does nothing.
    pub fn close(&self) {
        let mut s = self.lock();
        if s.closed {
            return;
        }
        s.closed = true;
        for w in std::mem::take(&mut s.receivers) {
            s.done.insert(w.id, Done::Closed);
            w.wake.complete(());
        }
        // The values of the senders that waited are dropped here, and credited to the account
        // that was charged for them.
        for (w, value) in std::mem::take(&mut s.senders) {
            s.done.insert(w.id, Done::Closed);
            drop(value);
            w.wake.complete(());
        }
    }

    /// Whether the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Whether the waiter `id` has been woken and has not collected what it was woken for.
    pub fn has_done(&self, id: u64) -> bool {
        self.lock().done.contains_key(&id)
    }

    /// What the woken waiter `id` was woken for; `None` when it was not woken.
    pub fn collect(&self, id: u64) -> Option<Done> {
        self.lock().done.remove(&id)
    }

    /// How many values are queued, and how many receivers and senders wait (for tests).
    pub fn counts(&self) -> (usize, usize, usize) {
        let s = self.lock();
        (s.queue.len(), s.receivers.len(), s.senders.len())
    }

    /// The waiter `id` gives up. It leaves the lists it is in. A value that was handed to it and
    /// not collected goes to the next receiver, or back to the front of the queue.
    pub fn abandon(&self, id: u64) {
        let mut s = self.lock();
        if let Some(pos) = s.receivers.iter().position(|w| w.id == id) {
            s.receivers.remove(pos);
            return;
        }
        if let Some(pos) = s.senders.iter().position(|(w, _)| w.id == id) {
            s.senders.remove(pos);
            return;
        }
        if let Some(Done::Got(value)) = s.done.remove(&id) {
            if let Some(w) = s.receivers.pop_front() {
                s.done.insert(w.id, Done::Got(value));
                w.wake.complete(());
            } else {
                s.queue.push_front(value);
            }
        }
    }

    /// Registers a waiter, with the completer `wait` makes, in the list `entry` says.
    fn register(
        s: &mut State,
        wait: &mut dyn FnMut() -> Completer<()>,
        entry: impl FnOnce(Waiter) -> Entry,
    ) -> Ticket {
        let id = s.next;
        s.next += 1;
        let wake = wait();
        let ticket = Ticket {
            id,
            op: wake.op(),
            token: Arc::clone(wake.token()),
        };
        match entry(Waiter { id, wake }) {
            Entry::Receiver(w) => s.receivers.push_back(w),
            Entry::Sender(w, v) => s.senders.push_back((w, v)),
        }
        ticket
    }

    /// Makes cancelling the ticket's token take the waiter out of the channel. Registered after
    /// the lock is let go: a hook that runs at once (a token that was cancelled already) takes
    /// the lock itself.
    fn on_give_up(self: &Arc<Self>, ticket: &Ticket) {
        let chan = Arc::clone(self);
        let id = ticket.id;
        ticket.token.on_cancel(move || chan.abandon(id));
    }
}

enum Entry {
    Receiver(Waiter),
    Sender(Waiter, Portable),
}

impl fmt::Debug for ChanInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.lock();
        write!(
            f,
            "Channel {{ queued: {}, receivers: {}, senders: {}, closed: {} }}",
            s.queue.len(),
            s.receivers.len(),
            s.senders.len(),
            s.closed
        )
    }
}

/// A channel as it crosses to another thread: the `Arc` itself. The receiver makes a handle of
/// its own isolate for it.
#[derive(Debug)]
pub struct ChanHandle(pub Arc<ChanInner>);

impl CrossesAsIs for ChanHandle {
    fn materialise(&self, core: &mut Core) -> Value {
        core.channel_handle(Arc::clone(&self.0))
    }

    fn address(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }
}

/// The channels an engine holds a handle of, by the number inside the handle.
#[derive(Default)]
pub struct ChanTable {
    entries: HashMap<u64, Arc<ChanInner>>,
    next: u64,
}

impl Core {
    /// A new channel and its handle.
    pub fn new_channel(&mut self, capacity: Option<usize>) -> Value {
        self.channel_handle(ChanInner::new(capacity))
    }

    /// A handle of this engine to `inner`. Each handle is an object of its own, and the table
    /// keeps the `Arc` until the last reference to the object dies (`chan_died`).
    pub fn channel_handle(&mut self, inner: Arc<ChanInner>) -> Value {
        let id = self.chans.next;
        self.chans.next += 1;
        self.chans.entries.insert(id, inner);
        Value::Obj(ClassObj::new(CHANNEL_TYPE, vec![Value::Int(id as i64)]))
    }

    /// The channel a handle names.
    pub fn chan_of(&self, v: &Value) -> Option<Arc<ChanInner>> {
        let (Value::Obj(o) | Value::SomeObj(o)) = v else {
            return None;
        };
        let id = chan_id_of(o)?;
        self.chans.entries.get(&id).cloned()
    }

    /// The handle `id` died: its channel is let go of (it lives on while another isolate or
    /// handle holds it).
    pub fn chan_died(&mut self, id: u64) {
        self.chans.entries.remove(&id);
    }

    /// How many handles this engine holds (for tests).
    pub fn chan_count(&self) -> usize {
        self.chans.entries.len()
    }
}

/// The iterator `for x in ch` loops over: it receives from the channel the handle names.
pub fn channel_iterator(channel: Value) -> Value {
    Value::Iter(crate::iter::IterObj::over_channel(channel))
}

/// The number inside a `Channel` handle.
pub fn chan_id_of(o: &ClassObj) -> Option<u64> {
    if o.ty() != CHANNEL_TYPE {
        return None;
    }
    match o.field(0) {
        Some(Value::Int(n)) => Some(n as u64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::Scheduler;

    fn int(n: i64) -> Portable {
        Portable::Int(n)
    }

    fn value(p: Portable) -> i64 {
        match p {
            Portable::Int(n) => n,
            other => panic!("not an int: {other:?}"),
        }
    }

    /// A waiter of `sched`: the completer a channel is given, and a way to see it woken.
    fn waiter(sched: &mut Scheduler) -> impl FnMut() -> Completer<()> + '_ {
        move || sched.begin_completion::<()>().0
    }

    fn woken(sched: &mut Scheduler, t: &Ticket) -> bool {
        sched.ext().drain();
        sched.ext().is_completed(t.op)
    }

    fn wait_of(r: Received) -> Ticket {
        match r {
            Received::Wait(t) => t,
            _ => panic!("expected a wait"),
        }
    }

    #[test]
    fn a_send_to_a_waiting_receiver_hands_the_value_over_and_the_queue_stays_empty() {
        let mut sched = Scheduler::new();
        let ch = ChanInner::new(None);
        let t = wait_of(ch.recv(&mut waiter(&mut sched)));
        assert!(!woken(&mut sched, &t));
        assert!(matches!(
            ch.send(int(7), &mut waiter(&mut sched)),
            Sent::Done
        ));
        // The value went to the waiter, not to the queue.
        assert_eq!(ch.counts(), (0, 0, 0));
        assert!(woken(&mut sched, &t));
        assert_eq!(
            value(match ch.collect(t.id) {
                Some(Done::Got(p)) => p,
                _ => panic!("not handed over"),
            }),
            7
        );
        // Nothing is left to collect.
        assert!(ch.collect(t.id).is_none());
    }

    #[test]
    fn receivers_are_served_in_the_order_they_began_to_wait() {
        let mut sched = Scheduler::new();
        let ch = ChanInner::new(None);
        let first = wait_of(ch.recv(&mut waiter(&mut sched)));
        let second = wait_of(ch.recv(&mut waiter(&mut sched)));
        for n in [10, 20] {
            assert!(matches!(
                ch.send(int(n), &mut waiter(&mut sched)),
                Sent::Done
            ));
        }
        let got = |ch: &Arc<ChanInner>, t: &Ticket| match ch.collect(t.id) {
            Some(Done::Got(p)) => value(p),
            _ => panic!("not handed over"),
        };
        assert_eq!((got(&ch, &first), got(&ch, &second)), (10, 20));
    }

    #[test]
    fn a_receive_that_makes_room_lets_the_first_blocked_sender_in_and_wakes_it() {
        let mut sched = Scheduler::new();
        let ch = ChanInner::new(Some(1));
        assert!(matches!(
            ch.send(int(1), &mut waiter(&mut sched)),
            Sent::Done
        ));
        let Sent::Wait(a) = ch.send(int(2), &mut waiter(&mut sched)) else {
            panic!("the channel is full")
        };
        let Sent::Wait(b) = ch.send(int(3), &mut waiter(&mut sched)) else {
            panic!("the channel is full")
        };
        assert_eq!(ch.counts(), (1, 0, 2));
        // One place is freed: the first sender's value takes it, the second still waits.
        let Received::Item(p) = ch.recv(&mut waiter(&mut sched)) else {
            panic!("a value is queued")
        };
        assert_eq!(value(p), 1);
        assert_eq!(ch.counts(), (1, 0, 1));
        assert!(woken(&mut sched, &a) && !woken(&mut sched, &b));
        assert!(matches!(ch.collect(a.id), Some(Done::Sent)));
        // Values come out in the order they were sent.
        let mut out = Vec::new();
        while let Some(p) = ch.try_recv() {
            out.push(value(p));
        }
        assert_eq!(out, vec![2, 3]);
        assert!(matches!(ch.collect(b.id), Some(Done::Sent)));
    }

    #[test]
    fn close_wakes_every_waiter_and_drops_what_blocked_senders_held() {
        let mut sched = Scheduler::new();
        let full = ChanInner::new(Some(1));
        assert!(matches!(
            full.send(int(1), &mut waiter(&mut sched)),
            Sent::Done
        ));
        let Sent::Wait(s) = full.send(int(2), &mut waiter(&mut sched)) else {
            panic!("full")
        };
        full.close();
        assert!(woken(&mut sched, &s));
        assert!(matches!(full.collect(s.id), Some(Done::Closed)));
        // What was queued can still be received, then the channel is at its end.
        assert!(matches!(
            full.recv(&mut waiter(&mut sched)),
            Received::Item(_)
        ));
        assert!(matches!(full.recv(&mut waiter(&mut sched)), Received::End));
        assert!(matches!(
            full.send(int(3), &mut waiter(&mut sched)),
            Sent::Closed
        ));
        let empty = ChanInner::new(None);
        let r1 = wait_of(empty.recv(&mut waiter(&mut sched)));
        let r2 = wait_of(empty.recv(&mut waiter(&mut sched)));
        empty.close();
        empty.close();
        assert!(woken(&mut sched, &r1) && woken(&mut sched, &r2));
        assert!(matches!(empty.collect(r1.id), Some(Done::Closed)));
        assert!(matches!(empty.collect(r2.id), Some(Done::Closed)));
        assert!(empty.is_closed());
    }

    #[test]
    fn a_waiter_that_gives_up_leaves_the_channel() {
        let mut sched = Scheduler::new();
        let ch = ChanInner::new(None);
        let gone = wait_of(ch.recv(&mut waiter(&mut sched)));
        let stays = wait_of(ch.recv(&mut waiter(&mut sched)));
        assert_eq!(ch.counts().1, 2);
        gone.token.cancel();
        assert_eq!(ch.counts().1, 1);
        // The next value goes to the one that stayed.
        assert!(matches!(
            ch.send(int(5), &mut waiter(&mut sched)),
            Sent::Done
        ));
        assert!(matches!(ch.collect(stays.id), Some(Done::Got(_))));
    }

    #[test]
    fn a_value_handed_to_a_waiter_that_then_gives_up_is_not_lost() {
        let mut sched = Scheduler::new();
        let ch = ChanInner::new(None);
        let gone = wait_of(ch.recv(&mut waiter(&mut sched)));
        assert!(matches!(
            ch.send(int(9), &mut waiter(&mut sched)),
            Sent::Done
        ));
        // It was handed over, and the waiter is cancelled before it collects.
        assert_eq!(ch.counts(), (0, 0, 0));
        gone.token.cancel();
        assert_eq!(ch.counts(), (1, 0, 0));
        assert_eq!(value(ch.try_recv().expect("the value came back")), 9);
        // With another receiver waiting, it goes to that one instead.
        let gone = wait_of(ch.recv(&mut waiter(&mut sched)));
        let other = wait_of(ch.recv(&mut waiter(&mut sched)));
        assert!(matches!(
            ch.send(int(4), &mut waiter(&mut sched)),
            Sent::Done
        ));
        gone.token.cancel();
        assert!(matches!(ch.collect(other.id), Some(Done::Got(_))));
        assert_eq!(ch.counts(), (0, 0, 0));
    }

    #[test]
    fn a_sender_that_gives_up_takes_its_value_out_of_the_channel() {
        let mut sched = Scheduler::new();
        let ch = ChanInner::new(Some(1));
        assert!(matches!(
            ch.send(int(1), &mut waiter(&mut sched)),
            Sent::Done
        ));
        let Sent::Wait(s) = ch.send(int(2), &mut waiter(&mut sched)) else {
            panic!("full")
        };
        s.token.cancel();
        assert_eq!(ch.counts(), (1, 0, 0));
        assert_eq!(value(ch.try_recv().unwrap()), 1);
        assert!(ch.try_recv().is_none());
    }

    #[test]
    fn many_threads_send_and_receive_without_loss_or_repeat() {
        const SENDERS: i64 = 4;
        const EACH: i64 = 500;
        let ch = ChanInner::new(Some(3));
        let mut handles = Vec::new();
        for s in 0..SENDERS {
            let ch = Arc::clone(&ch);
            handles.push(std::thread::spawn(move || {
                let mut sched = Scheduler::new();
                for i in 0..EACH {
                    let p = int(s * EACH + i);
                    let r = ch.send(p, &mut waiter(&mut sched));
                    match r {
                        Sent::Done => {}
                        Sent::Wait(t) => {
                            while !ch.has_done(t.id) {
                                sched.ext().wait(std::time::Duration::from_millis(5));
                            }
                            assert!(matches!(ch.collect(t.id), Some(Done::Sent)));
                            sched.ext().finish(t.op);
                        }
                        Sent::Closed => panic!("closed"),
                    }
                }
                Vec::<i64>::new()
            }));
        }
        let mut receivers = Vec::new();
        for _ in 0..3 {
            let ch = Arc::clone(&ch);
            receivers.push(std::thread::spawn(move || {
                let mut sched = Scheduler::new();
                let mut got = Vec::new();
                loop {
                    let r = ch.recv(&mut waiter(&mut sched));
                    match r {
                        Received::Item(p) => got.push(value(p)),
                        Received::End => return got,
                        Received::Wait(t) => {
                            while !ch.has_done(t.id) {
                                sched.ext().wait(std::time::Duration::from_millis(5));
                            }
                            sched.ext().finish(t.op);
                            match ch.collect(t.id) {
                                Some(Done::Got(p)) => got.push(value(p)),
                                Some(Done::Closed) => return got,
                                _ => panic!("woken for nothing"),
                            }
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        ch.close();
        let mut all: Vec<i64> = receivers
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        all.sort_unstable();
        assert_eq!(all, (0..SENDERS * EACH).collect::<Vec<_>>());
    }
}
