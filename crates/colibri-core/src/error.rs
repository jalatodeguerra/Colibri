//! The `Error` object (DESIGN §10): its layout, how one is made and how a link of the chain is
//! read. This module is the one owner of the layout; the methods of `Error` (`message`,
//! `cause`, `context`, `is`, `as`) are in the interpreter's `results.rs`.
//!
//! An `Error` is a class object of the prelude type [`ERROR_TYPE`] with four fields:
//!
//! | field | root error | error made by `context` |
//! |-------|-----------|--------------------------|
//! | 0 value | the original typed value (kept as it was, so `as[T]` returns it intact) | `()` |
//! | 1 impl | the index of its `IntoError` implementation, or `-1` | `-1` |
//! | 2 context | `()` | the context message |
//! | 3 cause | `()` | the wrapped `Error` |
//!
//! So an error is a chain from the outermost `context` down to the root, which holds the
//! value.

use std::rc::Rc;

use colibri_check::types::ERROR_TYPE;

use crate::heap::ClassObj;
use crate::value::Value;

const VALUE: usize = 0;
const IMPL: usize = 1;
const CONTEXT: usize = 2;
const CAUSE: usize = 3;

/// What one link of an error chain contributes.
#[doc(hidden)]
pub enum Link {
    /// A message added by `context`, and the error it wraps.
    Context(Rc<str>, Rc<ClassObj>),
    /// The root: the kept value, the index of its `IntoError` implementation (`-1`: none) and
    /// the message the host gave it, if any (a host error keeps a typed value, an enum, and
    /// says what happened in words: the message wins over the `Show` of the value).
    Root(Value, i64, Option<Rc<str>>),
}

/// Reads one link of an error object.
#[doc(hidden)]
pub fn link(obj: &ClassObj) -> Link {
    let field = |i| obj.field(i).unwrap_or(Value::Unit);
    match (field(CONTEXT), field(CAUSE)) {
        (Value::Str(message), Value::Obj(cause)) => Link::Context(message, cause),
        (Value::Str(message), Value::Unit) => Link::Root(field(VALUE), -1, Some(message)),
        _ => {
            let imp = match field(IMPL) {
                Value::Int(i) => i,
                _ => -1,
            };
            Link::Root(field(VALUE), imp, None)
        }
    }
}

/// A new root error that keeps `value`, converted through the implementation `impl_id`.
#[doc(hidden)]
pub fn new_error(value: Value, impl_id: i64) -> Value {
    Value::Obj(ClassObj::new(
        ERROR_TYPE,
        vec![value, Value::Int(impl_id), Value::Unit, Value::Unit],
    ))
}

/// A new root error that keeps the typed `value` (so `e.is[T]()` and `e.as[T]()` see it)
/// and shows as `message`.
pub(crate) fn new_typed_error(value: Value, message: &str) -> Value {
    Value::Obj(ClassObj::new(
        ERROR_TYPE,
        vec![value, Value::Int(-1), Value::str(message), Value::Unit],
    ))
}

/// `context`: a new error with the message `message` around `cause`.
#[doc(hidden)]
pub fn new_context_error(cause: Value, message: Rc<str>) -> Value {
    Value::Obj(ClassObj::new(
        ERROR_TYPE,
        vec![Value::Unit, Value::Int(-1), Value::Str(message), cause],
    ))
}
