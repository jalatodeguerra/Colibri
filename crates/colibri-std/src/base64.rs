//! `base64`: RFC 4648 encoding of text and of bytes (a `List[int]` of 0..=255 until there is a
//! `Bytes` type).
//!
//! - `encode(text)`, `decode(text) -> Result[str]`: the UTF-8 bytes of a `str`; `decode` fails
//!   with `base64.Error.NotText` when the bytes are not UTF-8 (use `decode_bytes`).
//! - `encode_bytes(bytes) -> Result[str]`, `decode_bytes(text) -> Result[List[int]]`.
//! - `encode_url`, `encode_url_bytes`, `decode_url`, `decode_url_bytes`: the URL and file name
//!   safe alphabet (`-` and `_`), with no `=` padding.
//!
//! The standard forms write and require the `=` padding; neither form accepts whitespace.
//! When `Bytes` arrives the `_bytes` functions are the ones that grow an overload.

use colibri_runtime::{Error, Vm};

use crate::bytes::{from_ints, out_of_range, to_ints};
use crate::errors::error_kind;
use crate::plain;

error_kind!(
    "base64",
    /// Why a base64 call failed (`base64.Error.Invalid`).
    Kind {
        /// The text is not base64 (a bad character, bad length or bad padding).
        Invalid,
        /// The bytes are fine but are not UTF-8 text, so they cannot be a `str`.
        NotText,
        /// A byte list holds a value outside 0..=255.
        OutOfRange,
    }
);

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let sextets = chunk.len() + 1;
        for i in 0..4 {
            if i < sextets {
                out.push(alphabet[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

fn decode(text: &str, alphabet: &[u8; 64], padded: bool) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    let (body, pad) = {
        let pad = bytes.iter().rev().take_while(|&&b| b == b'=').count();
        (&bytes[..bytes.len() - pad], pad)
    };
    if padded {
        if !bytes.len().is_multiple_of(4) {
            return Err(format!(
                "base64: the length {} is not a multiple of 4 (is the `=` padding missing?)",
                bytes.len()
            ));
        }
        if pad > 2 {
            return Err("base64: at most two `=` may end the text".to_string());
        }
    } else if pad > 0 {
        return Err("base64: the URL form has no `=` padding".to_string());
    }
    if body.len() % 4 == 1 {
        return Err("base64: a lone character cannot end the text".to_string());
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 2);
    let mut bits = 0u32;
    let mut count = 0;
    for (i, &c) in body.iter().enumerate() {
        let Some(v) = alphabet.iter().position(|&a| a == c) else {
            let shown = text[i..].chars().next().unwrap_or('?');
            return Err(format!(
                "base64: `{shown}` at position {i} is not in the alphabet"
            ));
        };
        bits = (bits << 6) | v as u32;
        count += 1;
        if count == 4 {
            out.extend_from_slice(&[(bits >> 16) as u8, (bits >> 8) as u8, bits as u8]);
            bits = 0;
            count = 0;
        }
    }
    // The unused low bits of the last character must be zero, or two texts would mean the same.
    match count {
        2 => {
            if bits & 0xF != 0 {
                return Err("base64: the last character has stray bits set".to_string());
            }
            out.push((bits >> 4) as u8);
        }
        3 => {
            if bits & 0x3 != 0 {
                return Err("base64: the last character has stray bits set".to_string());
            }
            out.extend_from_slice(&[(bits >> 10) as u8, (bits >> 2) as u8]);
        }
        _ => {}
    }
    if padded && pad != [0, 0, 2, 1][count] {
        return Err("base64: the `=` padding does not match the length".to_string());
    }
    Ok(out)
}

fn text_of(bytes: Vec<u8>) -> Result<String, colibri_runtime::TypedError> {
    String::from_utf8(bytes).map_err(|e| {
        Kind::NotText.error(format!(
            "base64: the decoded bytes are not UTF-8 text ({}); use decode_bytes",
            e.utf8_error()
        ))
    })
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("base64");
    // The standard alphabet, with `=` padding.
    plain(m.register_fn_named("encode", ["text"], |text: String| {
        encode(text.as_bytes(), STANDARD, true)
    }))?;
    plain(
        m.register_fn_named("encode_bytes", ["bytes"], |items: Vec<i64>| {
            let bytes = from_ints(&items)
                .map_err(|at| Kind::OutOfRange.error(out_of_range("base64", &items, at)))?;
            Ok::<_, colibri_runtime::TypedError>(encode(&bytes, STANDARD, true))
        }),
    )?;
    plain(m.register_fn_named("decode", ["text"], |text: String| {
        let bytes = decode(&text, STANDARD, true).map_err(|m| Kind::Invalid.error(m))?;
        text_of(bytes)
    }))?;
    plain(
        m.register_fn_named("decode_bytes", ["text"], |text: String| {
            decode(&text, STANDARD, true)
                .map(|b| to_ints(&b))
                .map_err(|m| Kind::Invalid.error(m))
        }),
    )?;
    // The URL-safe alphabet, no padding.
    plain(m.register_fn_named("encode_url", ["text"], |text: String| {
        encode(text.as_bytes(), URL, false)
    }))?;
    plain(
        m.register_fn_named("encode_url_bytes", ["bytes"], |items: Vec<i64>| {
            let bytes = from_ints(&items)
                .map_err(|at| Kind::OutOfRange.error(out_of_range("base64", &items, at)))?;
            Ok::<_, colibri_runtime::TypedError>(encode(&bytes, URL, false))
        }),
    )?;
    plain(m.register_fn_named("decode_url", ["text"], |text: String| {
        let bytes = decode(&text, URL, false).map_err(|m| Kind::Invalid.error(m))?;
        text_of(bytes)
    }))?;
    plain(
        m.register_fn_named("decode_url_bytes", ["text"], |text: String| {
            decode(&text, URL, false)
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
    fn rfc_4648_vectors() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes(), STANDARD, true), coded);
            assert_eq!(decode(coded, STANDARD, true).unwrap(), plain.as_bytes());
            let unpadded = coded.trim_end_matches('=');
            assert_eq!(encode(plain.as_bytes(), URL, false), unpadded);
            assert_eq!(decode(unpadded, URL, false).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn url_alphabet_differs_on_62_and_63() {
        let bytes = [0xfb, 0xff, 0xfe];
        assert_eq!(encode(&bytes, STANDARD, true), "+//+");
        assert_eq!(encode(&bytes, URL, false), "-__-");
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "Zg", "Zg=", "Z===", "Zm9v!A==", "Zh==", "Zm9=", " Zm9v", "Zm9v\n", "====", "A",
        ] {
            assert!(decode(bad, STANDARD, true).is_err(), "{bad:?}");
        }
        assert!(decode("Zg==", URL, false).is_err());
        assert!(decode("Zm9vY", URL, false).is_err());
    }
}
