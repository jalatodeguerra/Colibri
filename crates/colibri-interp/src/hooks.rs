//! Where user-written implementations take over from the structural defaults.
//!
//! Equality, ordering, `Show` and `clone` are implemented structurally (DESIGN §7.4), over
//! plain values, by functions that know nothing of the interpreter. A user `impl Eq for T`
//! (or `Ord`, `Show`, `Clone`) replaces the default for `T` wherever `T` appears, also nested
//! inside a list or a struct. Those functions therefore ask an [`Overrides`] first; the
//! interpreter answers by running the user's method, and [`Plain`] never overrides anything.

use std::cmp::Ordering;

use crate::ctl::Ctl;
use crate::value::Value;

/// Why a structural operation stopped early.
#[derive(Debug)]
pub(crate) enum Stop {
    /// The values nest too deeply for the native stack.
    TooDeep,
    /// A user-written method panicked.
    Ctl(Box<Ctl>),
}

/// An answer from a user-written method: `None` means "no implementation, use the default".
pub(crate) type Answer<T> = Option<Result<T, Stop>>;

/// The user-written implementations that replace structural defaults.
pub(crate) trait Overrides {
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
}

/// No user-written implementations.
pub(crate) struct Plain;

impl Overrides for Plain {}
