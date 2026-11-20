//! `str`: what the string methods (`len`, `trim`, `split`, `replace`, ...) do not cover:
//! parsing numbers, lines, repeating and trimming prefixes. (Padding is a method of `str`:
//! `s.pad_left(width)`, `s.pad_right_with(width, fill)`.)

use colibri_runtime::{Ctx, Error, TypedError, Vm};

use crate::errors::error_kind;
use crate::plain;

error_kind!(
    "str",
    /// Why a text could not be read as a number or a bool (`str.Error.OutOfRange`).
    Kind {
        /// The text is empty.
        Empty,
        /// The text is not written like the thing asked for.
        Invalid,
        /// A whole number too big or too small for an `int`.
        OutOfRange,
    }
);

fn parse_int(text: &str) -> Result<i64, TypedError> {
    text.parse::<i64>().map_err(|e| {
        use std::num::IntErrorKind::{Empty, InvalidDigit, NegOverflow, PosOverflow};
        let (kind, why) = match e.kind() {
            Empty => (Kind::Empty, "the text is empty"),
            InvalidDigit => (Kind::Invalid, "it has a character that is not a digit"),
            PosOverflow | NegOverflow => (Kind::OutOfRange, "the number does not fit in an int"),
            _ => (Kind::Invalid, "it is not a number"),
        };
        kind.error(format!("cannot read `{text}` as an int: {why}"))
    })
}

fn parse_float(text: &str) -> Result<f64, TypedError> {
    text.parse::<f64>().map_err(|_| {
        let kind = if text.is_empty() {
            Kind::Empty
        } else {
            Kind::Invalid
        };
        kind.error(format!("cannot read `{text}` as a float"))
    })
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("str");
    plain(m.register_fn_named("parse_int", ["text"], |text: String| parse_int(&text)))?;
    plain(m.register_fn_named("parse_float", ["text"], |text: String| parse_float(&text)))?;
    plain(
        m.register_fn_named("parse_bool", ["text"], |text: String| match text.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => {
                // Like `parse_int("")` and `parse_float("")`: nothing at all is `Empty`.
                let kind = if text.is_empty() {
                    Kind::Empty
                } else {
                    Kind::Invalid
                };
                Err(kind.error(format!(
                    "cannot read `{text}` as a bool (`true` or `false`)"
                )))
            }
        }),
    )?;
    // Split at `\n`, dropping a `\r` before it and the empty piece after a final newline.
    plain(m.register_fn_named("lines", ["text"], |text: String| {
        text.lines().map(str::to_string).collect::<Vec<_>>()
    }))?;
    plain(m.register_fn_named(
        "repeat",
        ["text", "times"],
        |cx: &mut Ctx, text: String, times: i64| {
            let Ok(times) = usize::try_from(times) else {
                cx.raise(format!("str.repeat: {times} is not a count"));
            };
            if text.len().saturating_mul(times) > (1 << 30) {
                cx.raise("str.repeat: the result would be more than a gigabyte");
            }
            text.repeat(times)
        },
    ))?;
    plain(m.register_fn_named("trim_start", ["text"], |text: String| {
        text.trim_start().to_string()
    }))?;
    plain(m.register_fn_named("trim_end", ["text"], |text: String| {
        text.trim_end().to_string()
    }))?;
    plain(m.register_fn_named(
        "strip_prefix",
        ["text", "prefix"],
        |text: String, prefix: String| text.strip_prefix(&prefix).map(str::to_string),
    ))?;
    plain(m.register_fn_named(
        "strip_suffix",
        ["text", "suffix"],
        |text: String, suffix: String| text.strip_suffix(&suffix).map(str::to_string),
    ))?;
    Ok(())
}
