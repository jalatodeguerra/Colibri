//! `hex`: base16 text of text and of bytes (a `List[int]` of 0..=255 until there is a `Bytes`
//! type).
//!
//! `encode(text)` and `encode_bytes(bytes)` write lowercase digits; `decode(text)` and
//! `decode_bytes(text)` read either case, and fail on an odd length or a character that is not
//! a hex digit. `decode` also needs the bytes to be UTF-8 (`hex.Error.NotText`).

use colibri_runtime::{Error, TypedError, Vm};

use crate::bytes::{from_ints, out_of_range, to_ints};
use crate::errors::error_kind;
use crate::plain;

error_kind!(
    "hex",
    /// Why a hex call failed (`hex.Error.Invalid`).
    Kind {
        /// The text is not hex (an odd length or a character that is not a digit).
        Invalid,
        /// The bytes are fine but are not UTF-8 text, so they cannot be a `str`.
        NotText,
        /// A byte list holds a value outside 0..=255.
        OutOfRange,
    }
);

fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 15)] as char);
    }
    out
}

fn decode(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err(format!(
            "hex: the length {} is odd, two digits make a byte",
            text.len()
        ));
    }
    let digit = |i: usize, c: u8| {
        (c as char).to_digit(16).ok_or_else(|| {
            let shown = text[i..].chars().next().unwrap_or('?');
            format!("hex: `{shown}` at position {i} is not a hex digit")
        })
    };
    text.as_bytes()
        .chunks(2)
        .enumerate()
        .map(|(n, pair)| Ok((digit(n * 2, pair[0])? * 16 + digit(n * 2 + 1, pair[1])?) as u8))
        .collect()
}

fn text_of(bytes: Vec<u8>) -> Result<String, TypedError> {
    String::from_utf8(bytes).map_err(|e| {
        Kind::NotText.error(format!(
            "hex: the decoded bytes are not UTF-8 text ({}); use decode_bytes",
            e.utf8_error()
        ))
    })
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("hex");
    plain(m.register_fn_named("encode", ["text"], |text: String| encode(text.as_bytes())))?;
    plain(
        m.register_fn_named("encode_bytes", ["bytes"], |items: Vec<i64>| {
            let bytes = from_ints(&items)
                .map_err(|at| Kind::OutOfRange.error(out_of_range("hex", &items, at)))?;
            Ok::<_, TypedError>(encode(&bytes))
        }),
    )?;
    plain(m.register_fn_named("decode", ["text"], |text: String| {
        text_of(decode(&text).map_err(|m| Kind::Invalid.error(m))?)
    }))?;
    plain(
        m.register_fn_named("decode_bytes", ["text"], |text: String| {
            decode(&text)
                .map(|b| to_ints(&b))
                .map_err(|m| Kind::Invalid.error(m))
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        assert_eq!(encode(b"\x00\xffA"), "00ff41");
        assert_eq!(decode("00FF41").unwrap(), b"\x00\xffA");
        assert_eq!(decode("").unwrap(), b"");
        assert!(decode("0").is_err());
        assert!(decode("0g").is_err());
        assert!(decode("é0").is_err());
    }
}
