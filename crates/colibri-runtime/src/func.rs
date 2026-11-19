//! Typed handles to Colibri functions.

use std::marker::PhantomData;

use colibri_check::hir::FnId;

use crate::convert::{Args, FromColibri};
use crate::error::Error;
use crate::vm::Vm;

/// A handle to a Colibri function with the Rust signature `A -> R`, from [`Vm::get_fn`].
///
/// It is `Copy` and does not borrow the `Vm`; pass the `Vm` to [`Fn::call`]. The signature is
/// checked once, when the handle is made.
pub struct Fn<A, R> {
    pub(crate) id: FnId,
    /// The `Vm` the handle came from, and which program in it.
    pub(crate) vm: u64,
    pub(crate) epoch: u32,
    _signature: PhantomData<fn(A) -> R>,
}

impl<A, R> Fn<A, R> {
    pub(crate) fn new(id: FnId, vm: u64, epoch: u32) -> Self {
        Fn {
            id,
            vm,
            epoch,
            _signature: PhantomData,
        }
    }
}

impl<A, R> Clone for Fn<A, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<A, R> Copy for Fn<A, R> {}

impl<A, R> std::fmt::Debug for Fn<A, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Fn(#{}, vm {}, epoch {})",
            self.id.0, self.vm, self.epoch
        )
    }
}

impl<A: Args, R: FromColibri> Fn<A, R> {
    /// Calls the function. A panic in Colibri comes back as `Err(Error::Panic { .. })` and
    /// leaves the `Vm` usable.
    pub fn call(&self, vm: &mut Vm, args: A) -> Result<R, Error> {
        let value = vm.call_id(self.id, (self.vm, self.epoch), args.into_values())?;
        R::from_colibri(value)
    }
}
