//! Readers of the builder objects that built-ins implement natively: the text of a
//! `StrBuilder` and the bytes of a `BytesBuilder`.
//!
//! `Show` of such a value must work without an engine, so the readers live here; the methods
//! that create and change the objects stay with the built-ins.

use crate::heap::ClassObj;
use crate::value::Value;

/// The text a `StrBuilder` holds. Its object has two fields: the chunks appended so far (a
/// list of strings, so an append is O(1)) and the total byte length.
pub(crate) fn builder_text(obj: &ClassObj) -> String {
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

/// The bytes a `BytesBuilder` holds so far.
pub(crate) fn builder_bytes(obj: &ClassObj) -> Vec<u8> {
    match obj.field(0) {
        Some(Value::ByteBuf(buf)) => buf.borrow().clone(),
        _ => Vec::new(),
    }
}
