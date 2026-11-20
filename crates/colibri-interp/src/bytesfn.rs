//! `Bytes` and `BytesBuilder` (DESIGN §5.1, §12): an immutable byte string that is one byte per
//! byte in memory, and a builder that appends to a growable buffer.
//!
//! A `Bytes` is a value like `str`: copying it bumps a reference count, `==`, ordering and
//! hashing are by content. A `BytesBuilder` is a class object whose only field is the buffer
//! (a [`Value::ByteBuf`], never visible to Colibri code).

use std::rc::Rc;

use colibri_check::hir::Builtin;
use colibri_syntax::Span;

use crate::ctl::Eval;
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::heap::ClassObj;
use crate::value::Value;

/// How a `Bytes` shows (`print`, `"{b}"`): `b"hello\x00\xff"`, with printable ASCII as it is,
/// `\\`, `\"`, `\n`, `\r` and `\t` escaped, and every other byte as `\xNN`.
pub(crate) fn show_bytes(bytes: &[u8]) -> String {
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
pub(crate) fn builder_bytes(obj: &ClassObj) -> Vec<u8> {
    match obj.field(0) {
        Some(Value::ByteBuf(buf)) => buf.borrow().clone(),
        _ => Vec::new(),
    }
}

/// The error text for bytes that are not UTF-8: where the first bad byte is.
pub(crate) fn utf8_message(e: &std::str::Utf8Error) -> String {
    let at = e.valid_up_to();
    match e.error_len() {
        Some(_) => format!(
            "the bytes are not valid UTF-8: byte {at} is not part of a valid character (`to_str_lossy()` replaces such bytes)"
        ),
        None => format!(
            "the bytes are not valid UTF-8: they end in the middle of a character that starts at byte {at} (`to_str_lossy()` replaces such bytes)"
        ),
    }
}

impl Interp {
    /// A `Bytes` method or constructor; `args[0]` is the receiver (the string or list for the
    /// constructors).
    pub(crate) fn bytes_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        args: &[Value],
        span: Span,
    ) -> Eval {
        match op {
            Builtin::BytesEmpty => return Ok(Value::Bytes(Rc::from(Vec::new()))),
            Builtin::StrToBytes => {
                let Some(Value::Str(s)) = args.first() else {
                    return Err(self.internal(fiber, span, "`to_bytes` on a non-string"));
                };
                return Ok(Value::bytes(s.as_bytes()));
            }
            Builtin::BytesFromList => {
                let Some(Value::List(list)) = args.first() else {
                    return Err(self.internal(fiber, span, "`Bytes` of a non-list"));
                };
                let mut bytes = Vec::with_capacity(list.len());
                for item in list.snapshot() {
                    match item {
                        Value::Int(n) if (0..=255).contains(&n) => bytes.push(n as u8),
                        _ => return Err(self.internal(fiber, span, "a `u8` out of range")),
                    }
                }
                return Ok(Value::Bytes(Rc::from(bytes)));
            }
            _ => {}
        }
        let Some(Value::Bytes(b)) = args.first() else {
            return Err(self.internal(fiber, span, "a `Bytes` method on a non-`Bytes`"));
        };
        let int_arg = |i: usize| match args.get(i) {
            Some(Value::Int(n)) => Some(*n),
            _ => None,
        };
        let other = match args.get(1) {
            Some(Value::Bytes(o)) => Some(Rc::clone(o)),
            _ => None,
        };
        Ok(match op {
            Builtin::BytesLen => Value::Int(b.len() as i64),
            Builtin::BytesIsEmpty => Value::Bool(b.is_empty()),
            Builtin::BytesAt => {
                let Some(i) = int_arg(1) else {
                    return Err(self.internal(fiber, span, "ill-typed `Bytes` index"));
                };
                match usize::try_from(i).ok().and_then(|i| b.get(i)) {
                    Some(&byte) => Value::Int(i64::from(byte)),
                    None => {
                        return Err(self.panic(
                            fiber,
                            span,
                            format!("index {i} out of bounds for Bytes of length {}", b.len()),
                        ));
                    }
                }
            }
            Builtin::BytesGet => {
                let Some(i) = int_arg(1) else {
                    return Err(self.internal(fiber, span, "ill-typed `get`"));
                };
                Value::option(
                    usize::try_from(i)
                        .ok()
                        .and_then(|i| b.get(i))
                        .map(|&byte| Value::Int(i64::from(byte))),
                )
            }
            Builtin::BytesSlice => {
                let (lo, hi) = self.slice_bounds(fiber, args, b.len(), "Bytes", span)?;
                Value::bytes(&b[lo..hi])
            }
            Builtin::BytesSliceOf => {
                let (Some(lo), Some(hi)) = (int_arg(1), int_arg(2)) else {
                    return Err(self.internal(fiber, span, "ill-typed `slice`"));
                };
                let (lo, hi) = self.slice_positions(
                    fiber,
                    (Some(lo), Some(hi)),
                    false,
                    b.len(),
                    "Bytes",
                    span,
                )?;
                Value::bytes(&b[lo..hi])
            }
            Builtin::BytesToList => {
                Value::list(b.iter().map(|&x| Value::Int(i64::from(x))).collect())
            }
            Builtin::BytesToStr => match std::str::from_utf8(b) {
                Ok(s) => Interp::ok_variant(Value::str(s)),
                Err(e) => Interp::err_variant(Interp::error_value(&utf8_message(&e))),
            },
            Builtin::BytesToStrLossy => Value::str(&String::from_utf8_lossy(b)),
            Builtin::BytesStartsWith
            | Builtin::BytesEndsWith
            | Builtin::BytesFind
            | Builtin::BytesContains => {
                let Some(o) = other else {
                    return Err(self.internal(fiber, span, "ill-typed `Bytes` method"));
                };
                match op {
                    Builtin::BytesStartsWith => Value::Bool(b.starts_with(&o)),
                    Builtin::BytesEndsWith => Value::Bool(b.ends_with(&o)),
                    _ => {
                        let at = find(b, &o);
                        if op == Builtin::BytesContains {
                            Value::Bool(at.is_some())
                        } else {
                            Value::option(at.map(|i| Value::Int(i as i64)))
                        }
                    }
                }
            }
            _ => return Err(self.internal(fiber, span, "ill-typed built-in call")),
        })
    }

    /// A `BytesBuilder` method; `args[0]` is the builder.
    pub(crate) fn bytes_builder_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        args: &[Value],
        span: Span,
    ) -> Eval {
        let Some(Value::Obj(obj)) = args.first() else {
            return Err(self.internal(fiber, span, "a byte builder method on a non-builder"));
        };
        let Some(Value::ByteBuf(buf)) = obj.field(0) else {
            return Err(self.internal(fiber, span, "a malformed byte builder"));
        };
        let mut buf = buf.borrow_mut();
        Ok(match (op, args.get(1)) {
            (Builtin::BytesBuilderPush, Some(Value::Int(n))) => {
                buf.push(*n as u8);
                Value::Unit
            }
            (Builtin::BytesBuilderPushBytes, Some(Value::Bytes(b))) => {
                buf.extend_from_slice(b);
                Value::Unit
            }
            (Builtin::BytesBuilderPushStr, Some(Value::Str(s))) => {
                buf.extend_from_slice(s.as_bytes());
                Value::Unit
            }
            (Builtin::BytesBuilderLen, _) => Value::Int(buf.len() as i64),
            (Builtin::BytesBuilderIsEmpty, _) => Value::Bool(buf.is_empty()),
            (Builtin::BytesBuilderClear, _) => {
                buf.clear();
                Value::Unit
            }
            (Builtin::BytesBuilderToBytes, _) => Value::bytes(&buf),
            _ => {
                drop(buf);
                return Err(self.internal(fiber, span, "ill-typed built-in call"));
            }
        })
    }
}

/// The offset of the first occurrence of `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let first = needle[0];
    let last_start = hay.len().checked_sub(needle.len())?;
    let mut at = 0;
    while at <= last_start {
        let rest = &hay[at..=last_start];
        let i = rest.iter().position(|&b| b == first)?;
        at += i;
        if hay[at..].starts_with(needle) {
            return Some(at);
        }
        at += 1;
    }
    None
}
