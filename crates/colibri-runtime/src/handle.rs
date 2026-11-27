//! Handles to host objects (EMBEDDING §3): [`Handle`] for the host side, [`Ref`] and
//! [`RefMut`] for borrowing an object, and the conversions that move host objects across the
//! boundary.
//!
//! A [`Handle`] is `(index, generation, vm)`: it names a slot of the host heap and which life of
//! that slot, so it never keeps an object alive and never reaches a different object. Using a
//! stale one is an error on the host side and a panic (```Job` was destroyed by the host``)
//! in a script.

use std::any::TypeId as AnyTypeId;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use colibri_core::host::HostFault;
use colibri_core::hostheap::{HostBorrowError, HostGuard};
use colibri_core::value::Value as Inner;

use crate::class::HostType;
use crate::convert::{FromColibri, IntoColibri, Value, wrong};
use crate::ctx::{Ctx, Cx};
use crate::engine::HostCx;
use crate::error::Error;
use crate::types::{TypeDesc, TypeRegistry};

/// A reference to a host object that the host keeps, with no say in its lifetime: it is
/// `(index, generation, vm)`, `Copy`, and not `Send` (a `Vm` stays on its thread).
///
/// Make one with [`crate::Vm::insert`], read one from a script argument (a host function
/// parameter of type `Handle<T>`), and use it with [`crate::Vm::get`], [`crate::Vm::get_mut`]
/// and [`crate::Vm::remove`]. Once the object is removed the handle is stale: the host methods
/// report it, a script that still holds the object panics when it calls a method, and a
/// function returning `Option<Handle<T>>` gives the script `none`.
pub struct Handle<T> {
    pub(crate) index: u32,
    pub(crate) generation: u32,
    pub(crate) vm: u64,
    // A raw pointer keeps the handle `!Send` and `!Sync`.
    _class: PhantomData<*const T>,
}

impl<T> Handle<T> {
    pub(crate) fn new(index: u32, generation: u32, vm: u64) -> Self {
        Handle {
            index,
            generation,
            vm,
            _class: PhantomData,
        }
    }

    /// The slot of the object in the host heap.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Which life of the slot the handle is for.
    pub fn generation(&self) -> u32 {
        self.generation
    }
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        (self.index, self.generation, self.vm) == (other.index, other.generation, other.vm)
    }
}

impl<T> Eq for Handle<T> {}

impl<T> Hash for Handle<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.index, self.generation, self.vm).hash(state);
    }
}

impl<T: HostType> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Handle<{}>(#{}, gen {})",
            T::NAME,
            self.index,
            self.generation
        )
    }
}

/// A shared borrow of a host object: from a script argument (`|this: &World, b: Ref<Body>|`)
/// or [`crate::Vm::get`]. Dropping it ends the borrow; while it lives, a `&mut self` method
/// reaching the same object panics (or, on the host side, gets an error).
pub struct Ref<T: HostType> {
    guard: HostGuard,
    _class: PhantomData<fn() -> T>,
}

impl<T: HostType> Deref for Ref<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // The borrow checked the Rust type when it was made.
        self.guard
            .downcast_ref::<T>()
            .expect("a Ref holds an object of its own type")
    }
}

impl<T: HostType + fmt::Debug> fmt::Debug for Ref<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

/// An exclusive borrow of a host object, from [`crate::Vm::get_mut`] or [`Ctx::get_mut`].
pub struct RefMut<T: HostType> {
    guard: HostGuard,
    _class: PhantomData<fn() -> T>,
}

impl<T: HostType> Deref for RefMut<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard
            .downcast_ref::<T>()
            .expect("a RefMut holds an object of its own type")
    }
}

impl<T: HostType> DerefMut for RefMut<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard
            .downcast_mut::<T>()
            .expect("a RefMut holds an object of its own type")
    }
}

fn borrow_error(e: HostBorrowError) -> Error {
    Error::HostObject(e.message())
}

/// The receiver of a method: borrowed for the call, with the exact messages a script sees.
pub(crate) fn borrow_receiver<T: HostType>(
    cx: &Ctx<'_>,
    value: &Value,
    exclusive: bool,
) -> Result<HostGuard, HostFault> {
    let Inner::Host(r) = &value.0 else {
        return Err(HostFault::Panic(format!(
            "internal error: the receiver is not a `{}` object",
            T::NAME
        )));
    };
    cx.eng
        .host_borrow(true, r.index, r.generation, AnyTypeId::of::<T>(), exclusive)
        .map_err(|e| HostFault::Message(e.message()))
}

fn borrow<T: HostType>(
    interp: &dyn HostCx,
    in_call: bool,
    h: Handle<T>,
    exclusive: bool,
) -> Result<HostGuard, Error> {
    if h.vm != interp.core().vm_id() {
        return Err(Error::StaleHandle);
    }
    interp
        .host_borrow(
            in_call,
            h.index,
            h.generation,
            AnyTypeId::of::<T>(),
            exclusive,
        )
        .map_err(borrow_error)
}

fn insert<T: HostType>(interp: &mut dyn HostCx, obj: T) -> Handle<T> {
    let (index, generation) =
        interp
            .core_mut()
            .host_insert(AnyTypeId::of::<T>(), T::NAME, Box::new(obj));
    Handle::new(index, generation, interp.core().vm_id())
}

fn remove<T: HostType>(interp: &mut dyn HostCx, h: Handle<T>) -> Result<Option<T>, Error> {
    if h.vm != interp.core().vm_id() {
        return Err(Error::StaleHandle);
    }
    match interp
        .core_mut()
        .host_remove(h.index, h.generation, AnyTypeId::of::<T>())
    {
        Ok(None) => Ok(None),
        Ok(Some(obj)) => match obj.downcast::<T>().map(std::rc::Rc::try_unwrap) {
            Ok(Ok(value)) => Ok(Some(value)),
            _ => Err(Error::Internal(
                "a removed host object is still referenced".to_string(),
            )),
        },
        Err(HostBorrowError::InUse { name, holder }) => Err(Error::HostObject(match holder {
            Some(holder) => format!(
                "cannot remove the host object `{name}`: it is in use by an enclosing call (`{holder}`)"
            ),
            None => format!("cannot remove the host object `{name}`: it is borrowed"),
        })),
        Err(e) => Err(borrow_error(e)),
    }
}

fn is_alive<T>(interp: &dyn HostCx, h: Handle<T>) -> bool {
    h.vm == interp.core().vm_id() && interp.core().host_alive(h.index, h.generation)
}

impl crate::Vm {
    /// Moves `obj` into the host heap, owned by the host: scripts may hold the object (a host
    /// function can hand out the handle) but never free it. Register the type first
    /// ([`crate::Vm::register_type`]) so scripts can name it.
    pub fn insert<T: HostType>(&mut self, obj: T) -> Handle<T> {
        insert(self.eng_mut(), obj)
    }

    /// Takes the object out of the host heap and returns it. Every handle and every script
    /// reference to it is stale afterwards. `Ok(None)` if it was removed before; an error if it
    /// is borrowed right now, or if the script owns it (it came out of a host function by value).
    pub fn remove<T: HostType>(&mut self, h: Handle<T>) -> Result<Option<T>, Error> {
        remove(self.eng_mut(), h)
    }

    /// Borrows the object for reading. An error if it was destroyed, or is borrowed exclusively
    /// (an enclosing call holds it).
    pub fn get<T: HostType>(&self, h: Handle<T>) -> Result<Ref<T>, Error> {
        borrow(self.eng(), false, h, false).map(|guard| Ref {
            guard,
            _class: PhantomData,
        })
    }

    /// Borrows the object for changing it. An error if it was destroyed or is borrowed at all.
    pub fn get_mut<T: HostType>(&mut self, h: Handle<T>) -> Result<RefMut<T>, Error> {
        borrow(self.eng(), false, h, true).map(|guard| RefMut {
            guard,
            _class: PhantomData,
        })
    }

    /// Whether the host still has the object `h` names.
    pub fn is_alive<T: HostType>(&self, h: Handle<T>) -> bool {
        is_alive(self.eng(), h)
    }

    /// How many host objects exist (script-owned and host-owned): a diagnostic for tests and
    /// leak hunts.
    pub fn host_object_count(&self) -> usize {
        self.eng().core().host_object_count()
    }
}

impl Ctx<'_> {
    /// Like [`crate::Vm::insert`], from inside a host function.
    pub fn insert<T: HostType>(&mut self, obj: T) -> Handle<T> {
        insert(&mut *self.eng, obj)
    }

    /// Like [`crate::Vm::remove`], from inside a host function.
    pub fn remove<T: HostType>(&mut self, h: Handle<T>) -> Result<Option<T>, Error> {
        remove(&mut *self.eng, h)
    }

    /// Like [`crate::Vm::get`]; a conflict names the host function that is running.
    pub fn get<T: HostType>(&self, h: Handle<T>) -> Result<Ref<T>, Error> {
        borrow(&*self.eng, true, h, false).map(|guard| Ref {
            guard,
            _class: PhantomData,
        })
    }

    /// Like [`crate::Vm::get_mut`]; a conflict names the host function that is running.
    pub fn get_mut<T: HostType>(&mut self, h: Handle<T>) -> Result<RefMut<T>, Error> {
        borrow(&*self.eng, true, h, true).map(|guard| RefMut {
            guard,
            _class: PhantomData,
        })
    }

    /// Whether the host still has the object `h` names.
    pub fn is_alive<T: HostType>(&self, h: Handle<T>) -> bool {
        is_alive(&*self.eng, h)
    }
}

// ---- conversions ----

/// A host type returned by value moves into the host heap, owned by the script: it is freed,
/// and its Rust `Drop` runs, when the last script reference is released.
impl<T: HostType> IntoColibri for T {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        reg.define_host_type::<T>()
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        let ty = cx.host_class::<T>();
        Value(
            cx.eng
                .core_mut()
                .host_new_object(ty, AnyTypeId::of::<T>(), T::NAME, Box::new(self)),
        )
    }
}

impl<T: HostType> IntoColibri for Handle<T> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        reg.define_host_type::<T>()
    }

    /// # Panics
    /// If the handle belongs to another `Vm`.
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        assert!(
            self.vm == cx.eng.core().vm_id(),
            "a `Handle<{}>` of another Vm cannot be passed to this one",
            T::NAME
        );
        let ty = cx.host_class::<T>();
        Value(cx.eng.core().host_value(ty, self.index, self.generation))
    }

    fn is_live(&self, cx: &Cx<'_>) -> bool {
        is_alive(&*cx.eng, *self)
    }
}

impl<T: HostType> FromColibri for Handle<T> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        reg.define_host_type::<T>()
    }
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error> {
        match &value.0 {
            Inner::Host(r) => Ok(Handle::new(r.index, r.generation, cx.eng.core().vm_id())),
            other => Err(wrong(&TypeDesc::Host(T::NAME.to_string()), other)),
        }
    }
}

impl<T: HostType> FromColibri for Ref<T> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        reg.define_host_type::<T>()
    }
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error> {
        let Inner::Host(r) = &value.0 else {
            return Err(wrong(&TypeDesc::Host(T::NAME.to_string()), &value.0));
        };
        cx.eng
            .host_borrow(true, r.index, r.generation, AnyTypeId::of::<T>(), false)
            .map(|guard| Ref {
                guard,
                _class: PhantomData,
            })
            .map_err(borrow_error)
    }
}

impl<T: HostType> FromColibri for RefMut<T> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        reg.define_host_type::<T>()
    }
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error> {
        let Inner::Host(r) = &value.0 else {
            return Err(wrong(&TypeDesc::Host(T::NAME.to_string()), &value.0));
        };
        cx.eng
            .host_borrow(true, r.index, r.generation, AnyTypeId::of::<T>(), true)
            .map(|guard| RefMut {
                guard,
                _class: PhantomData,
            })
            .map_err(borrow_error)
    }
}

/// What an exported function's `&X` parameter is converted to before the call: the function
/// borrows the owned value (`&str` from a `String`, `&[T]` from a `Vec<T>`, `&V` from a value
/// struct, `&Class` from a [`Ref`]). Written by `#[colibri::export]`, not by hosts.
#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "`&{Self}` cannot be a parameter of an exported function",
    label = "not exportable by reference",
    note = "a reference parameter is `&str`, `&[T]`, `&V` for a `#[colibri::export(value)]` type, or `&T` for an exported host class; take other types by value"
)]
pub trait RefParam {
    /// The owned form the script's argument converts to.
    type Owned: FromColibri;
}

/// The same for `&mut X`, which only an exported host class allows (a [`RefMut`]).
#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "`&mut {Self}` cannot be a parameter of an exported function",
    label = "not exportable by mutable reference",
    note = "only an exported host class can be taken as `&mut`; a value type is copied in, so take it by value"
)]
pub trait MutParam {
    /// The owned form the script's argument converts to.
    type Owned: FromColibri;
}

impl RefParam for str {
    type Owned = String;
}

impl<T: FromColibri> RefParam for [T] {
    type Owned = Vec<T>;
}

impl<T: HostType> RefParam for T {
    type Owned = Ref<T>;
}

impl<T: HostType> MutParam for T {
    type Owned = RefMut<T>;
}
