//! The host heap as the interpreter offers it: the objects of the embedding program that
//! scripts hold (EMBEDDING §3). The slab and its rules are in [`colibri_core::hostheap`]; this
//! file adds what needs a [`Fiber`], the host function that is running.

use std::any::TypeId as AnyTypeId;
use std::rc::Rc;

pub use colibri_core::hostheap::{HostBorrowError, HostGuard};

use crate::eval::Interp;
use crate::fiber::Fiber;

impl Interp {
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
}
