//! Typed handles to Colibri functions.

use std::marker::PhantomData;

use colibri_check::hir::FnId;

use crate::convert::{Args, FromColibri};
use crate::ctx::Caller;
use crate::error::Error;

/// A handle to a Colibri function with the Rust signature `A -> R`, from
/// [`crate::Vm::get_fn`].
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
    ///
    /// `caller` is the [`crate::Vm`], or the [`crate::Ctx`] of a host function that calls back
    /// into the script (the call then shares the instruction budget of the entry it is in).
    pub fn call(&self, caller: &mut impl Caller, args: A) -> Result<R, Error> {
        let mut cx = caller.ctx();
        let args = args.into_values(&mut cx);
        let value = cx.call_fn(self.id, (self.vm, self.epoch), args)?;
        let result = R::from_colibri(&value, &mut cx);
        cx.release(value);
        result
    }
}
