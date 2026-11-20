//! Where user-written implementations take over from the structural defaults.
//!
//! Equality, ordering, `Show` and `clone` are implemented structurally (DESIGN §7.4), over
//! plain values, by functions that know nothing of the engine. A user `impl Eq for T`
//! (or `Ord`, `Show`, `Clone`) replaces the default for `T` wherever `T` appears, also nested
//! inside a list or a struct. Those functions therefore ask an [`Overrides`] first; the
//! engine answers by running the user's method, and [`Plain`] never overrides anything.

use std::cmp::Ordering;

use crate::ctl::Ctl;
use crate::value::Value;

/// Why a structural operation stopped early.
#[derive(Debug)]
#[doc(hidden)]
pub enum Stop {
    /// The values nest too deeply for the native stack.
    TooDeep,
    /// A user-written method panicked.
    Ctl(Box<Ctl>),
}

/// An answer from a user-written method: `None` means "no implementation, use the default".
#[doc(hidden)]
pub type Answer<T> = Option<Result<T, Stop>>;

/// The user-written implementations that replace structural defaults.
#[doc(hidden)]
pub trait Overrides {
    fn eq(&mut self, _a: &Value, _b: &Value) -> Answer<bool> {
        None
    }
    fn cmp(&mut self, _a: &Value, _b: &Value) -> Answer<Option<Ordering>> {
        None
    }
    fn show(&mut self, _v: &Value) -> Answer<String> {
        None
    }
    fn clone_value(&mut self, _v: &Value) -> Answer<Value> {
        None
    }
    /// The message of the root of an `Error`: the user's `IntoError.message`, run on the kept
    /// value (`None`: use its `Show`).
    fn error_message(&mut self, _value: &Value, _impl_id: i64) -> Answer<String> {
        None
    }
}

/// No user-written implementations.
#[doc(hidden)]
pub struct Plain;

impl Overrides for Plain {}
