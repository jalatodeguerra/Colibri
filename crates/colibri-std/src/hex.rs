//! `hex`: base16 text of bytes.
//!
//! `encode(data: Bytes) -> str` writes lowercase digits; `decode(text) -> Result[Bytes]` reads
//! either case, and fails on an odd length or a character that is not a hex digit. Text is
//! encoded with `s.to_bytes()` first.

use colibri_runtime::{Bytes, Error, Vm};

use crate::bytes::hex_lower;
use crate::errors::error_kind;
use crate::plain;

error_kind!(
    "hex",
    /// Why a hex call failed (`hex.Error.Invalid`).
    Kind {
        /// The text is not hex (an odd length or a character that is not a digit).
        Invalid,
    }
);

fn decode(text: &str) -> Result<Vec<u8>, String> {
    // A character that is not a digit is reported before the length: a stray space or line
    // break would otherwise show up as an odd length.
    if let Some(i) = text.bytes().position(|c| !c.is_ascii_hexdigit()) {
        let shown = text[i..].chars().next().unwrap_or('?');
        let note = if shown.is_whitespace() {
            " (whitespace is not skipped: strip it first)"
        } else {
            ""
        };
        return Err(format!(
            "hex: `{}` at position {i} is not a hex digit{note}",
            shown.escape_debug()
        ));
    }
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

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("hex");
    plain(m.register_fn_named("encode", ["data"], |data: Bytes| hex_lower(&data)))?;
    plain(m.register_fn_named("decode", ["text"], |text: String| {
        decode(&text).map(Bytes).map_err(|m| Kind::Invalid.error(m))
    }))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        assert_eq!(hex_lower(b"\x00\xffA"), "00ff41");
        assert_eq!(decode("00FF41").unwrap(), b"\x00\xffA");
        assert_eq!(decode("").unwrap(), b"");
        assert!(decode("0").is_err());
        assert!(decode("0g").is_err());
        assert!(decode("é0").is_err());
    }
}
