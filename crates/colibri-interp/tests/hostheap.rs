//! The host heap through the interpreter: borrows, removal and the order in which what is left
//! is dropped. (These tests lived beside the slab; the slab is in `colibri-core` now.)

use colibri_core::prelude::*;
use std::any::TypeId as AnyTypeId;
use std::cell::RefCell;
use std::rc::Rc;

use colibri_interp::{HostBorrowError, Interp};

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
    let (i, g) = it
        .core_mut()
        .host_insert(any(), "Noisy", Box::new(Noisy("a", log)));
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
    assert!(it.core_mut().host_remove(i, g, any()).is_err());
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
    let (i, g) = it
        .core_mut()
        .host_insert(any(), "Noisy", Box::new(Noisy("a", Rc::default())));
    let mut shared = it.host_borrow(None, i, g, any(), false).unwrap();
    assert!(shared.downcast_mut::<Noisy>().is_none());
    assert!(shared.downcast_ref::<String>().is_none());
}

#[test]
fn a_wrong_rust_type_is_reported_not_downcast() {
    let mut it = interp();
    let (i, g) = it
        .core_mut()
        .host_insert(any(), "Noisy", Box::new(Noisy("a", Rc::default())));
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
    let (i, g) = it
        .core_mut()
        .host_insert(any(), "Noisy", Box::new(Noisy("a", Rc::clone(&log))));
    assert!(it.core().host_alive(i, g));
    let taken = it
        .core_mut()
        .host_remove(i, g, any())
        .unwrap()
        .expect("it was there");
    assert!(!it.core().host_alive(i, g));
    assert!(log.borrow().is_empty(), "removing hands the object over");
    drop(taken);
    assert_eq!(*log.borrow(), ["a"]);
    assert!(it.core_mut().host_remove(i, g, any()).unwrap().is_none());
    let (j, h) = it
        .core_mut()
        .host_insert(any(), "Noisy", Box::new(Noisy("b", log)));
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
        it.core_mut()
            .host_insert(any(), "Noisy", Box::new(Noisy(name, Rc::clone(&log))));
    }
    let (i, g) = (1, 0);
    it.core_mut().host_remove(i, g, any()).unwrap();
    assert_eq!(*log.borrow(), ["b"]);
    drop(it);
    assert_eq!(*log.borrow(), ["b", "a", "c"]);
}
