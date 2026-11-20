//! The host heap: the objects of the embedding program that scripts hold (EMBEDDING §3).
//!
//! One slab per interpreter. A slot has a generation, an owner (the script or the host), the
//! type of its object, and a cell with a borrow flag and the object itself. A script holds an
//! object through `Value::Host(Rc<HostRef>)`, which names slot and generation; a slot whose
//! generation moved on is a destroyed object, and every use of a stale reference is detected,
//! never a wrong object.
//!
//! * An object the script owns was returned by value from the host. When the last script
//!   reference is released, the slot is freed and the object's Rust `Drop` runs (it is the
//!   moment a class `Drop` hook would run, and it is traced the same way).
//! * An object the host owns was inserted with `Vm::insert`. Script references never free it;
//!   the host removes it, which bumps the generation.
//!
//! Borrowing uses an explicit flag, never a `RefCell` panic: `0` free, `n > 0` shared by `n`
//! guards, `-1` exclusive. An exclusive guard takes the object out of its cell for the length
//! of the borrow, and puts it back when it is dropped, so a call back into the script that
//! reaches the same object finds the flag set and gets a defined error instead of aliasing.

use std::any::{Any, TypeId as AnyTypeId};
use std::cell::Cell;
use std::rc::{Rc, Weak};

use colibri_check::hir::Body;
use colibri_check::types::TypeId;

use crate::fiber::Fiber;

pub use colibri_core::hostheap::HostRef;

/// The storage of one object, shared with the guards that borrow it.
struct HostCell {
    /// `0` free, positive: how many shared guards, `-1`: an exclusive guard.
    flag: Cell<isize>,
    /// The host function that took the borrow (the first one, for shared borrows); `None`
    /// when the host code took it between calls.
    holder: Cell<Option<Rc<Body>>>,
    /// Empty while an exclusive guard has the object, or after it was taken out for good.
    obj: Cell<Option<Rc<dyn Any>>>,
}

enum Owner {
    /// The script owns it: the weak reference lets the host turn a handle back into a value.
    Script(Weak<HostRef>),
    Host,
}

struct Slot {
    generation: u32,
    live: bool,
    owner: Owner,
    any: AnyTypeId,
    /// The Colibri name of the class, for messages.
    name: &'static str,
    cell: Rc<HostCell>,
}

/// Why a host object could not be borrowed or removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostBorrowError {
    /// The host destroyed the object (or the reference is not one of this heap's).
    Dead { name: String },
    /// An enclosing call holds the object in a way that conflicts with this use. `holder` is
    /// the host function that holds it, `None` when host code outside any call does.
    InUse {
        name: String,
        holder: Option<String>,
    },
    /// The object has another Rust type than the caller asked for.
    WrongType { name: String },
    /// The object belongs to the script: only the host's own objects can be removed.
    ScriptOwned { name: String },
}

impl HostBorrowError {
    /// The text of the panic a script sees.
    pub fn message(&self) -> String {
        match self {
            HostBorrowError::Dead { name } => format!("`{name}` was destroyed by the host"),
            HostBorrowError::InUse {
                name,
                holder: Some(holder),
            } => {
                format!("host object `{name}` is already in use by an enclosing call (`{holder}`)")
            }
            HostBorrowError::InUse { name, holder: None } => {
                format!("host object `{name}` is already borrowed by the host")
            }
            HostBorrowError::WrongType { name } => {
                format!("internal error: the host object is not the Rust type `{name}` describes")
            }
            HostBorrowError::ScriptOwned { name } => {
                format!("this `{name}` is owned by the script, so the host cannot remove it")
            }
        }
    }
}

/// The error for a conflicting borrow of `cell`.
fn in_use(cell: &HostCell, name: String) -> HostBorrowError {
    let holder = cell.holder.take();
    let text = holder.as_ref().map(|body| body.name.clone());
    cell.holder.set(holder);
    HostBorrowError::InUse { name, holder: text }
}

/// A borrow of a host object. Dropping it ends the borrow.
pub struct HostGuard {
    cell: Rc<HostCell>,
    obj: Option<Rc<dyn Any>>,
    exclusive: bool,
}

impl HostGuard {
    /// The object as a `T`, if it is one.
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.obj.as_deref()?.downcast_ref::<T>()
    }

    /// The object as a mutable `T`, if it is one and the borrow is exclusive.
    pub fn downcast_mut<T: 'static>(&mut self) -> Option<&mut T> {
        if !self.exclusive {
            return None;
        }
        Rc::get_mut(self.obj.as_mut()?)?.downcast_mut::<T>()
    }
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        if self.exclusive {
            self.cell.obj.set(self.obj.take());
            self.cell.flag.set(0);
            self.cell.holder.set(None);
        } else {
            self.obj = None;
            let left = self.cell.flag.get() - 1;
            self.cell.flag.set(left);
            if left == 0 {
                self.cell.holder.set(None);
            }
        }
    }
}

/// The slab.
#[derive(Default)]
pub(crate) struct HostHeap {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl HostHeap {
    fn insert(
        &mut self,
        any: AnyTypeId,
        name: &'static str,
        obj: Box<dyn Any>,
        owner: Owner,
    ) -> (u32, u32) {
        let cell = Rc::new(HostCell {
            flag: Cell::new(0),
            holder: Cell::new(None),
            obj: Cell::new(Some(Rc::from(obj))),
        });
        if let Some(i) = self.free.pop() {
            let slot = &mut self.slots[i as usize];
            slot.live = true;
            slot.owner = owner;
            slot.any = any;
            slot.name = name;
            slot.cell = cell;
            return (i, slot.generation);
        }
        self.slots.push(Slot {
            generation: 0,
            live: true,
            owner,
            any,
            name,
            cell,
        });
        (self.slots.len() as u32 - 1, 0)
    }

    fn slot(&self, index: u32, generation: u32) -> Option<&Slot> {
        self.slots
            .get(index as usize)
            .filter(|s| s.live && s.generation == generation)
    }

    /// Ends the life of the slot: the generation moves on and the index can be reused.
    fn retire(&mut self, index: u32) -> Option<Rc<dyn Any>> {
        let slot = self.slots.get_mut(index as usize)?;
        slot.live = false;
        slot.generation = slot.generation.wrapping_add(1);
        slot.owner = Owner::Host;
        let obj = slot.cell.obj.take();
        self.free.push(index);
        obj
    }

    pub(crate) fn alive(&self, index: u32, generation: u32) -> bool {
        self.slot(index, generation).is_some()
    }

    pub(crate) fn live_count(&self) -> usize {
        self.slots.iter().filter(|s| s.live).count()
    }

    pub(crate) fn borrow(
        &self,
        index: u32,
        generation: u32,
        any: AnyTypeId,
        exclusive: bool,
        holder: Option<Rc<Body>>,
    ) -> Result<HostGuard, HostBorrowError> {
        let Some(slot) = self.slot(index, generation) else {
            return Err(HostBorrowError::Dead {
                name: self
                    .slots
                    .get(index as usize)
                    .map_or_else(String::new, |s| s.name.to_string()),
            });
        };
        let name = slot.name.to_string();
        if slot.any != any {
            return Err(HostBorrowError::WrongType { name });
        }
        let cell = &slot.cell;
        let flag = cell.flag.get();
        let conflict = if exclusive { flag != 0 } else { flag < 0 };
        if conflict {
            return Err(in_use(cell, name));
        }
        let obj = if exclusive {
            cell.flag.set(-1);
            cell.holder.set(holder);
            cell.obj.take()
        } else {
            if flag == 0 {
                cell.holder.set(holder);
            }
            cell.flag.set(flag + 1);
            let obj = cell.obj.take();
            cell.obj.set(obj.clone());
            obj
        };
        Ok(HostGuard {
            cell: Rc::clone(cell),
            obj,
            exclusive,
        })
    }
}

impl crate::eval::Interp {
    /// Moves an object into the host heap as the host's own: scripts may hold references to
    /// it, none of them frees it. The result is its `(index, generation)`.
    pub fn host_insert(
        &mut self,
        any: AnyTypeId,
        name: &'static str,
        obj: Box<dyn Any>,
    ) -> (u32, u32) {
        self.host.heap.insert(any, name, obj, Owner::Host)
    }

    /// Moves an object into the host heap as the script's: it is freed (and its `Drop` runs)
    /// when the last script reference is released. `ty` is its class in the loaded program.
    pub fn host_new_object(
        &mut self,
        ty: TypeId,
        any: AnyTypeId,
        name: &'static str,
        obj: Box<dyn Any>,
    ) -> crate::value::Value {
        let (index, generation) = self.host.heap.insert(any, name, obj, Owner::Host);
        let reference = Rc::new(HostRef {
            ty,
            index,
            generation,
        });
        self.host.heap.slots[index as usize].owner = Owner::Script(Rc::downgrade(&reference));
        crate::value::Value::Host(reference)
    }

    /// A script value for the object `(index, generation)` of class `ty`. If the object was
    /// destroyed the value is stale: calling a method on it panics.
    pub fn host_value(&self, ty: TypeId, index: u32, generation: u32) -> crate::value::Value {
        let shared = match self.host.heap.slot(index, generation) {
            Some(Slot {
                owner: Owner::Script(weak),
                ..
            }) => weak.upgrade(),
            _ => None,
        };
        crate::value::Value::Host(shared.unwrap_or_else(|| {
            Rc::new(HostRef {
                ty,
                index,
                generation,
            })
        }))
    }

    /// Whether the object is still there.
    pub fn host_alive(&self, index: u32, generation: u32) -> bool {
        self.host.heap.alive(index, generation)
    }

    /// How many objects the host heap holds.
    pub fn host_object_count(&self) -> usize {
        self.host.heap.live_count()
    }

    /// Borrows an object, shared or exclusive, on behalf of the host function that `fiber`
    /// is running (`None`: host code between calls).
    pub fn host_borrow(
        &self,
        fiber: Option<&Fiber>,
        index: u32,
        generation: u32,
        any: AnyTypeId,
        exclusive: bool,
    ) -> Result<HostGuard, HostBorrowError> {
        let holder = fiber
            .and_then(|f| f.frames.last())
            .map(|frame| Rc::clone(&frame.body));
        self.host
            .heap
            .borrow(index, generation, any, exclusive, holder)
    }

    /// Takes the host's own object out of the heap and bumps the generation, so every
    /// reference to it is stale. `Ok(None)` if it is already gone.
    pub fn host_remove(
        &mut self,
        index: u32,
        generation: u32,
        any: AnyTypeId,
    ) -> Result<Option<Rc<dyn Any>>, HostBorrowError> {
        let Some(slot) = self.host.heap.slot(index, generation) else {
            return Ok(None);
        };
        let name = slot.name.to_string();
        if slot.any != any {
            return Err(HostBorrowError::WrongType { name });
        }
        if matches!(slot.owner, Owner::Script(_)) {
            return Err(HostBorrowError::ScriptOwned { name });
        }
        let cell = &slot.cell;
        if cell.flag.get() != 0 {
            return Err(in_use(cell, name));
        }
        Ok(self.host.heap.retire(index))
    }

    /// The last script reference to an object was released: if the script owns the object,
    /// it is freed here and its Rust `Drop` runs.
    pub(crate) fn free_host_object(&mut self, reference: &HostRef) {
        let Some(slot) = self.host.heap.slot(reference.index, reference.generation) else {
            return;
        };
        if !matches!(slot.owner, Owner::Script(_)) {
            return;
        }
        let name = slot.name;
        if let Some(trace) = self.core.trace.as_mut() {
            let label = self
                .core
                .types
                .get(reference.ty.0 as usize)
                .map_or_else(|| name.to_string(), |def| def.name.clone());
            trace.push(label);
        }
        // An object that is borrowed cannot lose its last reference: the call that borrows it
        // holds an argument. If it ever did, the borrow would end by itself and the object
        // would be dropped with the guard's `Rc`.
        let obj = self.host.heap.retire(reference.index);
        self.drop_host_object(name, obj);
    }

    /// Runs the Rust `Drop` of an object; a panic in it is reported like a panic in a `Drop`
    /// hook, to whichever statement finishes next.
    fn drop_host_object(&mut self, name: &str, obj: Option<Rc<dyn Any>>) {
        let Some(obj) = obj else {
            return;
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(obj)));
        if let Err(payload) = result {
            let message = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "a panic that is not a message".to_string()
            };
            self.core.hook_panics.push(crate::ctl::PanicInfo {
                message: format!("dropping the host object `{name}` panicked: {message}"),
                span: colibri_syntax::Span::new(colibri_syntax::FileId(0), 0, 0),
                stack: Vec::new(),
                notes: Vec::new(),
                budget: false,
            });
        }
    }

    /// Drops every object still in the host heap, in index order (the last step of a `Vm`'s
    /// life, after the roots and the module values).
    pub(crate) fn drop_host_heap(&mut self) {
        for index in 0..self.host.heap.slots.len() {
            let slot = &mut self.host.heap.slots[index];
            if !slot.live {
                continue;
            }
            slot.live = false;
            slot.generation = slot.generation.wrapping_add(1);
            let obj = slot.cell.obj.take();
            // Nobody is left to hear about a panic now.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(obj)));
        }
        self.host.heap.free.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::eval::Interp;

    fn interp() -> Interp {
        Interp::new(Box::new(std::io::sink()), 100)
    }

    /// Records when it is dropped.
    struct Noisy(&'static str, Rc<RefCell<Vec<&'static str>>>);

    impl Drop for Noisy {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    fn any() -> AnyTypeId {
        AnyTypeId::of::<Noisy>()
    }

    #[test]
    fn shared_borrows_stack_and_exclude_an_exclusive_one() {
        let log = Rc::default();
        let mut it = interp();
        let (i, g) = it.host_insert(any(), "Noisy", Box::new(Noisy("a", log)));
        let one = it.host_borrow(None, i, g, any(), false).unwrap();
        let two = it.host_borrow(None, i, g, any(), false).unwrap();
        assert_eq!(one.downcast_ref::<Noisy>().unwrap().0, "a");
        assert_eq!(two.downcast_ref::<Noisy>().unwrap().0, "a");
        let busy = it.host_borrow(None, i, g, any(), true).err().unwrap();
        assert_eq!(
            busy,
            HostBorrowError::InUse {
                name: "Noisy".into(),
                holder: None
            }
        );
        assert!(it.host_remove(i, g, any()).is_err());
        drop(one);
        assert!(it.host_borrow(None, i, g, any(), true).is_err());
        drop(two);
        let mut mine = it.host_borrow(None, i, g, any(), true).unwrap();
        assert!(mine.downcast_mut::<Noisy>().is_some());
        // An exclusive borrow excludes everything, including a read.
        assert!(it.host_borrow(None, i, g, any(), false).is_err());
        drop(mine);
        assert!(it.host_borrow(None, i, g, any(), false).is_ok());
    }

    #[test]
    fn a_shared_guard_cannot_be_written_through() {
        let mut it = interp();
        let (i, g) = it.host_insert(any(), "Noisy", Box::new(Noisy("a", Rc::default())));
        let mut shared = it.host_borrow(None, i, g, any(), false).unwrap();
        assert!(shared.downcast_mut::<Noisy>().is_none());
        assert!(shared.downcast_ref::<String>().is_none());
    }

    #[test]
    fn a_wrong_rust_type_is_reported_not_downcast() {
        let mut it = interp();
        let (i, g) = it.host_insert(any(), "Noisy", Box::new(Noisy("a", Rc::default())));
        let wrong = it
            .host_borrow(None, i, g, AnyTypeId::of::<String>(), false)
            .err()
            .unwrap();
        assert!(matches!(wrong, HostBorrowError::WrongType { .. }));
    }

    #[test]
    fn removing_bumps_the_generation_and_reuses_the_slot() {
        let log = Rc::default();
        let mut it = interp();
        let (i, g) = it.host_insert(any(), "Noisy", Box::new(Noisy("a", Rc::clone(&log))));
        assert!(it.host_alive(i, g));
        let taken = it.host_remove(i, g, any()).unwrap().expect("it was there");
        assert!(!it.host_alive(i, g));
        assert!(log.borrow().is_empty(), "removing hands the object over");
        drop(taken);
        assert_eq!(*log.borrow(), ["a"]);
        assert!(it.host_remove(i, g, any()).unwrap().is_none());
        let (j, h) = it.host_insert(any(), "Noisy", Box::new(Noisy("b", log)));
        assert_eq!(i, j);
        assert_ne!(g, h);
        assert!(it.host_borrow(None, i, g, any(), false).is_err());
        assert!(it.host_borrow(None, j, h, any(), false).is_ok());
    }

    #[test]
    fn the_heap_drops_what_is_left_in_index_order() {
        let log = Rc::default();
        let mut it = interp();
        for name in ["a", "b", "c"] {
            it.host_insert(any(), "Noisy", Box::new(Noisy(name, Rc::clone(&log))));
        }
        let (i, g) = (1, 0);
        it.host_remove(i, g, any()).unwrap();
        assert_eq!(*log.borrow(), ["b"]);
        drop(it);
        assert_eq!(*log.borrow(), ["b", "a", "c"]);
    }
}
