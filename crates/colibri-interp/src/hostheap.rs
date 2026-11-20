//! The host heap as the interpreter offers it: the objects of the embedding program that
//! scripts hold (EMBEDDING §3). The slab and its rules are in [`colibri_core::hostheap`]; this
//! file adds what needs a [`Fiber`], the host function that is running.

use std::any::{Any, TypeId as AnyTypeId};
use std::rc::Rc;

use colibri_check::types::TypeId;

pub use colibri_core::hostheap::{HostBorrowError, HostGuard, HostRef};

use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::value::Value;

impl Interp {
    /// Moves an object into the host heap as the host's own: scripts may hold references to
    /// it, none of them frees it. The result is its `(index, generation)`.
    pub fn host_insert(
        &mut self,
        any: AnyTypeId,
        name: &'static str,
        obj: Box<dyn Any>,
    ) -> (u32, u32) {
        self.core.host_insert(any, name, obj)
    }

    /// Moves an object into the host heap as the script's: it is freed (and its `Drop` runs)
    /// when the last script reference is released. `ty` is its class in the loaded program.
    pub fn host_new_object(
        &mut self,
        ty: TypeId,
        any: AnyTypeId,
        name: &'static str,
        obj: Box<dyn Any>,
    ) -> Value {
        self.core.host_new_object(ty, any, name, obj)
    }

    /// A script value for the object `(index, generation)` of class `ty`. If the object was
    /// destroyed the value is stale: calling a method on it panics.
    pub fn host_value(&self, ty: TypeId, index: u32, generation: u32) -> Value {
        self.core.host_value(ty, index, generation)
    }

    /// Whether the object is still there.
    pub fn host_alive(&self, index: u32, generation: u32) -> bool {
        self.core.host_alive(index, generation)
    }

    /// How many objects the host heap holds.
    pub fn host_object_count(&self) -> usize {
        self.core.host_object_count()
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
        self.core
            .host_borrow(holder, index, generation, any, exclusive)
    }

    /// Takes the host's own object out of the heap and bumps the generation, so every
    /// reference to it is stale. `Ok(None)` if it is already gone.
    pub fn host_remove(
        &mut self,
        index: u32,
        generation: u32,
        any: AnyTypeId,
    ) -> Result<Option<Rc<dyn Any>>, HostBorrowError> {
        self.core.host_remove(index, generation, any)
    }
}
