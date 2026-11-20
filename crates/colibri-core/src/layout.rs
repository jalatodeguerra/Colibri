//! The layout of the objects that built-ins implement natively, as far as the deep operations
//! need to read them: how a `Bytes` shows, the text of a `StrBuilder`, the bytes of a
//! `BytesBuilder`, and the links of an `Error` chain.
//!
//! `Show` of such a value must work without an engine, so these readers live here; the methods
//! that create and change the objects stay with the built-ins.

use std::rc::Rc;

use crate::heap::ClassObj;
use crate::value::Value;

/// The fields of an `Error` object (see the table in `results.rs` of the interpreter).
#[doc(hidden)]
pub const VALUE: usize = 0;
#[doc(hidden)]
pub const IMPL: usize = 1;
#[doc(hidden)]
pub const CONTEXT: usize = 2;
#[doc(hidden)]
pub const CAUSE: usize = 3;

/// The text a `StrBuilder` holds. Its object has two fields: the chunks appended so far (a
/// list of strings, so an append is O(1)) and the total byte length.
#[doc(hidden)]
pub fn builder_text(obj: &ClassObj) -> String {
    obj.with_fields(|fields| match fields.first() {
        Some(Value::List(chunks)) => chunks.with(|items| {
            let mut out = String::with_capacity(match fields.get(1) {
                Some(Value::Int(n)) => usize::try_from(*n).unwrap_or(0),
                _ => 0,
            });
            for item in items {
                if let Value::Str(s) = item {
                    out.push_str(s);
                }
            }
            out
        }),
        _ => String::new(),
    })
}

/// How a `Bytes` shows (`print`, `"{b}"`): `b"hello\x00\xff"`, with printable ASCII as it is,
/// `\\`, `\"`, `\n`, `\r` and `\t` escaped, and every other byte as `\xNN`.
#[doc(hidden)]
pub fn show_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 3);
    out.push_str("b\"");
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(char::from(b)),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push('"');
    out
}

/// The bytes a `BytesBuilder` holds so far.
#[doc(hidden)]
pub fn builder_bytes(obj: &ClassObj) -> Vec<u8> {
    match obj.field(0) {
        Some(Value::ByteBuf(buf)) => buf.borrow().clone(),
        _ => Vec::new(),
    }
}

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
