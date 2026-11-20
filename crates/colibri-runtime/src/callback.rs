//! Callbacks: Colibri functions and closures the host keeps (EMBEDDING §6).
//!
//! A [`Callback`] is what a host function parameter of the Colibri type `fn(A..) -> R` becomes.
//! The closure lives in the interpreter's root slab, which keeps it alive; the callback holds
//! only an index and generations, so it works after the `Vm` is gone (as an error) and never
//! keeps a script's object alive by accident. Dropping the last clone queues the root for
//! release; the queue is drained at the next entry into the `Vm`, because releasing can run a
//! `Drop` hook, which must not happen at an arbitrary point of the host's code.

use std::marker::PhantomData;
use std::rc::Rc;

use colibri_interp::Value as Inner;

use crate::convert::{Args, FromColibri, IntoColibri, Value, wrong};
use crate::ctx::{Caller, Cx};
use crate::error::Error;
use crate::types::{TypeDesc, TypeRegistry};

/// A root handle: slot, generation of the slot, epoch of the slab.
pub(crate) use colibri_interp::RootHandle;

/// The roots whose last callback was dropped, waiting for the `Vm` to release them.
pub(crate) use colibri_interp::RootDropQueue as DropQueue;

struct Shared {
    handle: RootHandle,
    vm: u64,
    queue: DropQueue,
    /// The host module whose function received the callback: its bare type names are the
    /// module's when the callback is called later.
    module: Option<Rc<str>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        let mut pending = self.queue.take();
        pending.push(self.handle);
        self.queue.set(pending);
    }
}

/// A Colibri function value with the Rust signature `A -> R`, kept by the host.
///
/// It is `Clone` (clones share one root) and not `Send`. After the `Vm` shut down or loaded
/// another program, calling it is [`Error::StaleHandle`].
pub struct Callback<A, R> {
    shared: Rc<Shared>,
    _signature: PhantomData<fn(A) -> R>,
}

impl<A, R> Clone for Callback<A, R> {
    fn clone(&self) -> Self {
        Callback {
            shared: Rc::clone(&self.shared),
            _signature: PhantomData,
        }
    }
}

impl<A, R> std::fmt::Debug for Callback<A, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Callback(root {})", self.shared.handle.0)
    }
}

impl<A: Args, R: FromColibri> Callback<A, R> {
    /// Calls the function. A panic in it comes back as `Err(Error::Panic { .. })` and leaves
    /// the `Vm` usable. Pass the `Vm`, or the `Ctx` of the host function that is running (a
    /// reentrant call shares its instruction budget).
    pub fn call(&self, caller: &mut impl Caller, args: A) -> Result<R, Error> {
        let mut cx = caller.ctx();
        if cx.interp.vm_id() != self.shared.vm {
            return Err(Error::StaleHandle);
        }
        let Some(f) = cx.interp.rooted(self.shared.handle).cloned() else {
            return Err(Error::StaleHandle);
        };
        if self.shared.module.is_some() {
            cx.module = self.shared.module.clone();
        }
        let args = args.into_values(&mut cx);
        let value = cx.call_value(&Value(f), args)?;
        let result = R::from_colibri(&value, &mut cx);
        cx.release(value);
        result
    }
}

fn is_callable(v: &Inner) -> bool {
    matches!(v, Inner::Fn(..) | Inner::Closure(_))
}

impl<A: Args, R: FromColibri> FromColibri for Callback<A, R> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Fn(A::tys(reg), Box::new(R::ty(reg)))
    }

    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error> {
        if !is_callable(&value.0) {
            return Err(wrong(
                &<Self as FromColibri>::ty(&mut TypeRegistry::new()),
                &value.0,
            ));
        }
        let handle = cx.interp.root(value.0.clone());
        Ok(Callback {
            shared: Rc::new(Shared {
                handle,
                vm: cx.interp.vm_id(),
                queue: cx.interp.root_drop_queue(),
                module: cx.module.clone(),
            }),
            _signature: PhantomData,
        })
    }
}

impl<A: Args, R: FromColibri> IntoColibri for Callback<A, R> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        <Self as FromColibri>::ty(reg)
    }

    /// # Panics
    /// If the callback is stale (its program was replaced).
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        match cx.interp.rooted(self.shared.handle) {
            Some(v) if cx.interp.vm_id() == self.shared.vm => Value(v.clone()),
            _ => panic!("a stale callback cannot be passed to a script"),
        }
    }
}
