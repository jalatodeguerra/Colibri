//! `base64`: RFC 4648 encoding of bytes.
//!
//! - `encode(data: Bytes) -> str`, `decode(text) -> Result[Bytes]`: the standard alphabet, with
//!   `=` padding. Text is encoded with `s.to_bytes()` first, and a decoded `Bytes` is read as
//!   text with `b.to_str()`.
//! - `encode_url`, `decode_url`: the URL and file name safe alphabet (`-` and `_`), with no `=`
//!   padding.
//!
//! The standard form writes and requires the `=` padding; neither form accepts whitespace.

use colibri_runtime::{Bytes, Error, Vm};

use crate::errors::error_kind;
use crate::plain;

error_kind!(
    "base64",
    /// Why a base64 call failed (`base64.Error.Invalid`).
    Kind {
        /// The text is not base64 (a bad character, bad length or bad padding).
        Invalid,
    }
);

/// A base64 alphabet with its inverse, built at compile time: decoding looks each character up
/// in `value` instead of scanning the 64 letters for it.
struct Alphabet {
    /// The character of each sextet value.
    chars: [u8; 64],
    /// The sextet value of each byte, or [`Alphabet::NOT_IN`] for a byte outside the alphabet.
    value: [u8; 256],
}

impl Alphabet {
    const NOT_IN: u8 = 0xFF;

    const fn new(chars: &[u8; 64]) -> Alphabet {
        let mut value = [Alphabet::NOT_IN; 256];
        let mut i = 0;
        while i < 64 {
            value[chars[i] as usize] = i as u8;
            i += 1;
        }
        Alphabet {
            chars: *chars,
            value,
        }
    }
}

const STANDARD: &Alphabet =
    &Alphabet::new(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/");
const URL: &Alphabet =
    &Alphabet::new(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_");

fn encode(bytes: &[u8], alphabet: &Alphabet, pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let sextets = chunk.len() + 1;
        for i in 0..4 {
            if i < sextets {
                out.push(alphabet.chars[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

fn decode(text: &str, alphabet: &Alphabet, padded: bool) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    let (body, pad) = {
        let pad = bytes.iter().rev().take_while(|&&b| b == b'=').count();
        (&bytes[..bytes.len() - pad], pad)
    };
    // A character that does not belong is the first thing to report: a stray space or line
    // break would otherwise show up as a wrong length.
    if let Some(i) = body
        .iter()
        .position(|&c| alphabet.value[c as usize] == Alphabet::NOT_IN)
    {
        let shown = text[i..].chars().next().unwrap_or('?');
        let note = if shown.is_whitespace() {
            " (whitespace is not skipped: strip it first)"
        } else {
            ""
        };
        return Err(format!(
            "base64: `{}` at position {i} is not in the alphabet{note}",
            shown.escape_debug()
        ));
    }
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
    for &c in body {
        bits = (bits << 6) | u32::from(alphabet.value[c as usize]);
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

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("base64");
    // The standard alphabet, with `=` padding.
    plain(m.register_fn_named("encode", ["data"], |data: Bytes| {
        encode(&data, STANDARD, true)
    }))?;
    plain(m.register_fn_named("decode", ["text"], |text: String| {
        decode(&text, STANDARD, true)
            .map(Bytes)
            .map_err(|m| Kind::Invalid.error(m))
    }))?;
    // The URL-safe alphabet, no padding.
    plain(m.register_fn_named("encode_url", ["data"], |data: Bytes| {
        encode(&data, URL, false)
    }))?;
    plain(m.register_fn_named("decode_url", ["text"], |text: String| {
        decode(&text, URL, false)
            .map(Bytes)
            .map_err(|m| Kind::Invalid.error(m))
    }))?;
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
    fn every_byte_value_round_trips() {
        let all: Vec<u8> = (0..=255).collect();
        for len in 0..=all.len() {
            let data = &all[..len];
            let text = encode(data, STANDARD, true);
            assert_eq!(
                decode(&text, STANDARD, true).unwrap(),
                data,
                "standard, {len}"
            );
            let text = encode(data, URL, false);
            assert_eq!(decode(&text, URL, false).unwrap(), data, "url, {len}");
        }
        // Also each byte value alone and at every position of a 3-byte group.
        for b in 0..=255u8 {
            for data in [[b, 0, 0], [0, b, 0], [0, 0, b]] {
                let text = encode(&data, STANDARD, true);
                assert_eq!(decode(&text, STANDARD, true).unwrap(), data);
            }
        }
    }

    #[test]
    fn every_byte_outside_the_alphabet_is_rejected() {
        for b in 0..=127u8 {
            let c = b as char;
            let text = format!("Zm9{c}");
            let ok_std = STANDARD.chars.contains(&b);
            assert_eq!(decode(&text, STANDARD, true).is_ok(), ok_std, "{b}");
            let ok_url = URL.chars.contains(&b);
            assert_eq!(decode(&text, URL, false).is_ok(), ok_url, "{b}");
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

    #[test]
    fn a_bad_character_is_named_before_the_length_is_judged() {
        for (text, standard, want) in [
            ("Zm9v\n", false, "`\\n` at position 4"),
            ("Zm9v\n", true, "`\\n` at position 4"),
            (" Zm9v", true, "` ` at position 0"),
            (" Zm9v", false, "` ` at position 0"),
            ("Zm 9v", true, "` ` at position 2"),
            ("Zm9!", false, "`!` at position 3"),
        ] {
            let result = if standard {
                decode(text, STANDARD, true)
            } else {
                decode(text, URL, false)
            };
            let message = result.unwrap_err();
            assert!(message.contains(want), "{text:?}: {message}");
        }
        let message = decode("Zm9v\n", URL, false).unwrap_err();
        assert!(message.contains("whitespace is not skipped"), "{message}");
    }
}
