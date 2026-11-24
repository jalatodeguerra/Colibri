//! Host functions: Rust closures scripts can call (EMBEDDING §1).
//!
//! Every host function, whatever its Rust signature, becomes one non-generic [`RawHostFn`] that
//! takes the context and the arguments as a slice and returns one value. The generic part
//! (reading each argument, converting the result) is only the wrapper [`HostFn::into_raw`]
//! builds, so an engine other than the interpreter can call the same table.

use std::marker::PhantomData;
use std::rc::Rc;

pub use colibri_core::host::HostFault;

use crate::Error;
use crate::cap::Cap;
use crate::convert::{FromColibri, IntoColibri, Value};
use crate::ctx::Ctx;
use crate::types::{SigDesc, TypeRegistry};
use crate::vm::Vm;

/// A host function in the form the VM calls: the context, the arguments, one result.
pub type RawHostFn = Rc<dyn Fn(&mut Ctx<'_>, &[Value]) -> Result<Value, HostFault>>;

/// A Rust closure or function that can be registered with [`Vm::register_fn`]: up to eight
/// parameters that are [`FromColibri`], optionally after a leading `&mut Ctx`, returning
/// something [`IntoColibri`] (a `Result<T, E: Display>` becomes `Result[T]`).
///
/// `Marker` only tells the arities and the `Ctx` forms apart; it is inferred.
pub trait HostFn<Marker>: 'static {
    /// The Colibri signature, computed once at registration.
    fn sig(reg: &mut TypeRegistry) -> SigDesc;
    /// The type-erased function.
    fn into_raw(self) -> RawHostFn;
}

/// Marks a host function without a leading context parameter.
pub struct Plain<Args>(PhantomData<fn(Args)>);
/// Marks a host function whose first parameter is `&mut Ctx`.
pub struct WithCtx<Args>(PhantomData<fn(Args)>);

pub(crate) fn conversion_fault(index: usize, error: Error) -> HostFault {
    match error {
        // A destroyed or busy host object has a message of its own, which scripts see as is.
        Error::HostObject(message) => HostFault::Message(message),
        error => HostFault::Panic(format!("argument {} could not be read: {error}", index + 1)),
    }
}

pub(crate) fn arity_fault(wanted: usize, got: usize) -> HostFault {
    HostFault::Panic(format!(
        "internal error: called with {got} arguments instead of {wanted}"
    ))
}

macro_rules! host_fn {
    ($count:expr; $(($name:ident, $index:tt)),*) => {
        impl<Func, Ret, $($name),*> HostFn<Plain<($($name,)*)>> for Func
        where
            Func: Fn($($name),*) -> Ret + 'static,
            Ret: IntoColibri,
            $($name: FromColibri,)*
        {
            fn sig(reg: &mut TypeRegistry) -> SigDesc {
                SigDesc {
                    params: vec![$(<$name as FromColibri>::ty(reg)),*],
                    ret: <Ret as IntoColibri>::ty(reg),
                }
            }

            #[allow(unused_variables)]
            fn into_raw(self) -> RawHostFn {
                Rc::new(move |cx: &mut Ctx<'_>, args: &[Value]| {
                    if args.len() != $count {
                        return Err(arity_fault($count, args.len()));
                    }
                    let result = self(
                        $(<$name as FromColibri>::from_colibri(&args[$index], cx)
                            .map_err(|e| conversion_fault($index, e))?),*
                    );
                    Ok(result.into_colibri(cx))
                })
            }
        }

        impl<Func, Ret, $($name),*> HostFn<WithCtx<($($name,)*)>> for Func
        where
            Func: Fn(&mut Ctx<'_>, $($name),*) -> Ret + 'static,
            Ret: IntoColibri,
            $($name: FromColibri,)*
        {
            fn sig(reg: &mut TypeRegistry) -> SigDesc {
                SigDesc {
                    params: vec![$(<$name as FromColibri>::ty(reg)),*],
                    ret: <Ret as IntoColibri>::ty(reg),
                }
            }

            #[allow(unused_variables, non_snake_case)]
            fn into_raw(self) -> RawHostFn {
                Rc::new(move |cx: &mut Ctx<'_>, args: &[Value]| {
                    if args.len() != $count {
                        return Err(arity_fault($count, args.len()));
                    }
                    $(let $name = <$name as FromColibri>::from_colibri(&args[$index], cx)
                        .map_err(|e| conversion_fault($index, e))?;)*
                    let result = self(cx, $($name),*);
                    Ok(result.into_colibri(cx))
                })
            }
        }
    };
}

host_fn!(0;);
host_fn!(1; (A, 0));
host_fn!(2; (A, 0), (B, 1));
host_fn!(3; (A, 0), (B, 1), (C, 2));
host_fn!(4; (A, 0), (B, 1), (C, 2), (D, 3));
host_fn!(5; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4));
host_fn!(6; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5));
host_fn!(7; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6));
host_fn!(8; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6), (H, 7));

/// What [`Vm::register_fn`] returns: tag the function with the capability it needs.
pub struct Registration<'a> {
    pub(crate) vm: &'a mut Vm,
    pub(crate) index: usize,
}

impl Registration<'_> {
    /// The function needs this capability: scripts can use it only when the host granted it
    /// (`VmBuilder::allow`).
    pub fn requires(self, cap: Cap) -> Self {
        self.vm.set_requires(self.index, cap);
        self
    }

    /// The function never suspends its caller: it does not call `run_async`, `run_async_cancellable`
    /// or `try_await_completion` (it may still block the thread, as a plain file read does; that
    /// blocks every coroutine the same way on both engines). A function that does suspend its
    /// caller must not be marked. The bytecode VM runs a coroutine's call of any other host
    /// function on a native stack of its own, so that the coroutine can be suspended inside it
    /// (this costs a switch of stacks); a marked function runs on the caller's stack, where
    /// [`crate::Ctx::can_suspend`] reads `false`. The tree-walker takes no notice.
    pub fn no_await(self) -> Self {
        self.vm.set_no_await(self.index);
        self
    }

    /// The function is deprecated: a script that calls it gets the warning "`name` is
    /// deprecated: message" (an empty `message` leaves the colon out). The interface file
    /// carries it as `@deprecated("message")`.
    pub fn deprecated(self, message: &str) -> Self {
        self.vm.set_deprecated(self.index, message);
        self
    }

    /// The function reads the type arguments of each call ([`Ctx::type_args`]), so a call has
    /// to name a concrete type: `fn decode[T](..)` is checked not to be called with a type
    /// parameter of the caller, whose type is not known at runtime. It must be generic (take
    /// or return a [`Value`]), or this is an [`Error::Registration`].
    pub fn typed(self) -> Result<Self, Error> {
        self.vm.set_typed(self.index)?;
        Ok(self)
    }
}

/// The text of a panic payload.
pub(crate) fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic that is not a message".to_string()
    }
}
