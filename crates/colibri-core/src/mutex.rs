//! `Mutex[T]` (DESIGN §14.1, ARCHITECTURE §36.7): a value that threads change one at a time.
//!
//! The value lives as a [`Portable`] in a [`MutexInner`], an `Arc` that every isolate with a handle
//! holds. The `Mutex` the program holds is a class object of `MUTEX_TYPE` that names an entry of the
//! [`MutexTable`] of its [`Core`], like a channel or a `Shared`; the handle crosses threads as the
//! `Arc` ([`MutexHandle`] is a [`CrossesAsIs`]), so sending a `Mutex` copies nothing.
//!
//! Every operation first takes the lock of the mutex ([`MutexInner::hold`]), then copies the value
//! out of its portable form or into it. The lock is a flag next to the value, not a guard of the
//! standard library's `Mutex` kept across the call: a `with` runs the program's callback while it
//! is held, and the waiters are woken by a condition variable and look at the cancel flag, so a
//! `Vm` that is dropped does not wait for ever behind a lock.
//!
//! The lock is never held across a pause. The callback of `with` is native code that calls user
//! code, so nothing in it can pause the coroutine, and a thread that holds a lock waits for no
//! task and no channel. While an isolate holds the lock, it uses no other mutex either (not even
//! the same one), which leaves no cycle of locks for a deadlock to hide in.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use colibri_check::types::MUTEX_TYPE;

use crate::heap::ClassObj;
use crate::portable::{CrossesAsIs, Portable};
use crate::state::Core;
use crate::value::Value;

/// How long a thread waits for the lock before it looks at the cancel flag again.
const POLL: Duration = Duration::from_millis(50);

struct State {
    value: Portable,
    /// A `get`, `set` or `with` is running.
    locked: bool,
}

/// The value of a `Mutex`, and its lock: shared by every isolate that has a handle.
pub struct MutexInner {
    state: Mutex<State>,
    freed: Condvar,
}

// The `Arc` is what crosses threads, so this must hold without any `unsafe impl`.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<MutexInner>();
};

/// The wait for a lock was ended by the cancel flag.
#[derive(Debug, PartialEq, Eq)]
pub struct Stopped;

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl MutexInner {
    /// A `Mutex` holding `value`.
    pub fn new(value: Portable) -> Arc<MutexInner> {
        Arc::new(MutexInner {
            state: Mutex::new(State {
                value,
                locked: false,
            }),
            freed: Condvar::new(),
        })
    }

    /// Takes the lock, waiting while another operation holds it. `stop` is asked every 50 ms of
    /// waiting: when it says so, the wait ends with [`Stopped`] (the `Vm` is going away).
    pub fn hold(&self, stop: &dyn Fn() -> bool) -> Result<Held<'_>, Stopped> {
        let mut state = lock(&self.state);
        while state.locked {
            if stop() {
                return Err(Stopped);
            }
            state = self
                .freed
                .wait_timeout(state, POLL)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        state.locked = true;
        Ok(Held { inner: self })
    }

    /// Whether an operation holds the lock now (for tests).
    pub fn is_locked(&self) -> bool {
        lock(&self.state).locked
    }
}

impl fmt::Debug for MutexInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Mutex { .. }")
    }
}

/// The lock of a mutex, held. Dropping it lets go with the value as it is; [`Held::store`] puts a
/// new value first. Every path out of an operation, a panic included, drops it.
pub struct Held<'a> {
    inner: &'a MutexInner,
}

impl Held<'_> {
    /// Reads the value (copying it out is `f`'s business).
    pub fn read<R>(&self, f: impl FnOnce(&Portable) -> R) -> R {
        f(&lock(&self.inner.state).value)
    }

    /// Replaces the value and lets go of the lock. Gives the old value back, so that it is dropped
    /// outside the lock.
    pub fn store(self, new: Portable) -> Portable {
        let old = std::mem::replace(&mut lock(&self.inner.state).value, new);
        drop(self);
        old
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        lock(&self.inner.state).locked = false;
        self.inner.freed.notify_all();
    }
}

/// A `Mutex` as it crosses to another thread: the `Arc` itself. The receiver makes a handle of its
/// own isolate for it.
#[derive(Debug)]
pub struct MutexHandle(pub Arc<MutexInner>);

impl CrossesAsIs for MutexHandle {
    fn materialise(&self, core: &mut Core) -> Value {
        core.mutex_handle(Arc::clone(&self.0))
    }

    fn address(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }
}

/// The `Mutex`es an engine holds a handle of, and the one it holds the lock of.
#[derive(Default)]
pub struct MutexTable {
    entries: HashMap<u64, Arc<MutexInner>>,
    next: u64,
    /// The address of the mutex whose `with` is running in this isolate, if one is.
    held: Option<usize>,
}

/// The address that names a mutex.
pub fn address(inner: &Arc<MutexInner>) -> usize {
    Arc::as_ptr(inner) as usize
}

impl Core {
    /// A new `Mutex` of `root`, and its handle.
    pub fn new_mutex(&mut self, root: Portable) -> Value {
        self.mutex_handle(MutexInner::new(root))
    }

    /// A handle of this engine to `inner`. Each handle is an object of its own, and the table
    /// keeps the `Arc` until the last reference to the object dies ([`Core::mutex_died`]).
    pub fn mutex_handle(&mut self, inner: Arc<MutexInner>) -> Value {
        let id = self.mutexes.next;
        self.mutexes.next += 1;
        self.mutexes.entries.insert(id, inner);
        Value::Obj(ClassObj::new(MUTEX_TYPE, vec![Value::Int(id as i64)]))
    }

    /// The mutex a handle names.
    pub fn mutex_of(&self, v: &Value) -> Option<Arc<MutexInner>> {
        let (Value::Obj(o) | Value::SomeObj(o)) = v else {
            return None;
        };
        let id = mutex_id_of(o)?;
        self.mutexes.entries.get(&id).cloned()
    }

    /// The handle `id` died: its mutex lives on while another handle holds it.
    pub fn mutex_died(&mut self, id: u64) {
        self.mutexes.entries.remove(&id);
    }

    /// How many handles this engine holds (for tests).
    pub fn mutex_count(&self) -> usize {
        self.mutexes.entries.len()
    }

    /// The mutex whose `with` is running in this isolate, by address.
    pub fn mutex_held(&self) -> Option<usize> {
        self.mutexes.held
    }

    /// Records that a `with` of the mutex at `addr` is running (`None`: it ended).
    pub fn set_mutex_held(&mut self, addr: Option<usize>) {
        self.mutexes.held = addr;
    }
}

/// The number inside a `Mutex` handle.
pub fn mutex_id_of(o: &ClassObj) -> Option<u64> {
    if o.ty() != MUTEX_TYPE {
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
    use std::sync::atomic::{AtomicBool, Ordering};

    fn never() -> bool {
        false
    }

    #[test]
    fn a_second_hold_waits_for_the_first_to_let_go() {
        let m = MutexInner::new(Portable::Int(1));
        let first = m.hold(&never).unwrap();
        assert!(m.is_locked());
        let got = AtomicBool::new(false);
        std::thread::scope(|s| {
            let waiter = s.spawn(|| {
                let held = m.hold(&never).unwrap();
                got.store(true, Ordering::SeqCst);
                held.read(|v| matches!(v, Portable::Int(5)))
            });
            // The waiter cannot have the lock while the first hold lives. (Nothing here asks the
            // waiter to be ready: it is let in only after the store below, whatever the timing.)
            std::thread::sleep(Duration::from_millis(30));
            assert!(!got.load(Ordering::SeqCst));
            let old = first.store(Portable::Int(5));
            assert!(matches!(old, Portable::Int(1)));
            assert!(waiter.join().unwrap(), "the waiter saw the stored value");
        });
        assert!(!m.is_locked());
    }

    #[test]
    fn dropping_a_hold_leaves_the_value_as_it_was() {
        let m = MutexInner::new(Portable::Int(7));
        drop(m.hold(&never).unwrap());
        assert!(!m.is_locked());
        let held = m.hold(&never).unwrap();
        assert!(held.read(|v| matches!(v, Portable::Int(7))));
    }

    #[test]
    fn a_wait_ends_when_the_stop_flag_is_raised() {
        let m = MutexInner::new(Portable::Int(0));
        let _held = m.hold(&never).unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            let waiter = s.spawn(|| m.hold(&|| stop.load(Ordering::SeqCst)).map(|_| ()));
            std::thread::sleep(Duration::from_millis(20));
            stop.store(true, Ordering::SeqCst);
            assert_eq!(waiter.join().unwrap(), Err(Stopped));
        });
        assert!(m.is_locked(), "the holder still has it");
    }

    #[test]
    fn a_handle_names_its_mutex_and_dies_with_its_object() {
        let mut core = Core::new(Box::new(std::io::sink()));
        let inner = MutexInner::new(Portable::Int(3));
        let a = core.mutex_handle(Arc::clone(&inner));
        let b = core.mutex_handle(Arc::clone(&inner));
        assert_eq!(core.mutex_count(), 2);
        assert!(Arc::ptr_eq(&core.mutex_of(&a).unwrap(), &inner));
        assert!(core.mutex_of(&Value::Int(1)).is_none());
        let (Value::Obj(x), Value::Obj(y)) = (&a, &b) else {
            panic!()
        };
        assert_ne!(mutex_id_of(x), mutex_id_of(y));
        assert_eq!(Arc::strong_count(&inner), 3);
        core.mutex_died(mutex_id_of(x).unwrap());
        core.mutex_died(mutex_id_of(y).unwrap());
        assert_eq!(Arc::strong_count(&inner), 1);
        for o in [x, y] {
            drop(o.take_fields());
        }
    }

    #[test]
    fn the_held_mutex_of_an_isolate_is_what_was_recorded() {
        let mut core = Core::new(Box::new(std::io::sink()));
        assert_eq!(core.mutex_held(), None);
        core.set_mutex_held(Some(0x10));
        assert_eq!(core.mutex_held(), Some(0x10));
        core.set_mutex_held(None);
        assert_eq!(core.mutex_held(), None);
    }
}
