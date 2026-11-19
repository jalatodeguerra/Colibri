//! Formatting a value with a validated format spec (DESIGN §12).

use colibri_check::hir::{Align, FormatSpec, FormatType};

use crate::show::{show, show_float};
use crate::value::Value;

/// Where padding goes.
#[derive(Clone, Copy)]
enum Place {
    Left,
    Right,
    Center,
    /// Between the sign and the digits (the `0` flag).
    AfterSign,
}

/// Formats `v` as `spec` says. The checker has validated the spec against the value's type,
/// so combinations that cannot occur fall back to the `Show` form.
pub fn format_value(v: &Value, spec: &FormatSpec) -> String {
    let (sign, body, numeric) = match v {
        Value::Int(i) => {
            let digits = match spec.ty {
                None => i.unsigned_abs().to_string(),
                Some(FormatType::LowerHex) => format!("{:x}", i.unsigned_abs()),
                Some(FormatType::UpperHex) => format!("{:X}", i.unsigned_abs()),
                Some(FormatType::Binary) => format!("{:b}", i.unsigned_abs()),
                Some(FormatType::Octal) => format!("{:o}", i.unsigned_abs()),
            };
            (if *i < 0 { "-" } else { "" }, digits, true)
        }
        Value::Float(x) => {
            let negative = x.is_sign_negative() && !x.is_nan();
            let digits = match spec.precision {
                Some(p) if x.is_finite() => format!("{:.*}", p, x.abs()),
                _ => show_float(x.abs()),
            };
            (if negative { "-" } else { "" }, digits, true)
        }
        other => {
            let text = show(&[], other);
            let text = match spec.precision {
                Some(p) => text.chars().take(p).collect(),
                None => text.into_owned(),
            };
            ("", text, false)
        }
    };
    pad(sign, &body, spec, numeric)
}

fn pad(sign: &str, body: &str, spec: &FormatSpec, numeric: bool) -> String {
    let len = sign.chars().count() + body.chars().count();
    if len >= spec.width {
        return format!("{sign}{body}");
    }
    let fill_count = spec.width - len;
    let (fill, place) = match spec.align {
        None if spec.zero => ('0', Place::AfterSign),
        None if numeric => (spec.fill, Place::Right),
        None => (spec.fill, Place::Left),
        Some(a) => {
            let fill = if spec.zero && spec.fill == ' ' {
                '0'
            } else {
                spec.fill
            };
            let place = match a {
                Align::Left => Place::Left,
                Align::Right => Place::Right,
                Align::Center => Place::Center,
            };
            (fill, place)
        }
    };
    let fill_of = |n: usize| fill.to_string().repeat(n);
    match place {
        Place::Left => format!("{sign}{body}{}", fill_of(fill_count)),
        Place::Right => format!("{}{sign}{body}", fill_of(fill_count)),
        Place::Center => {
            let left = fill_count / 2;
            format!(
                "{}{sign}{body}{}",
                fill_of(left),
                fill_of(fill_count - left)
            )
        }
        Place::AfterSign => format!("{sign}{}{body}", fill_of(fill_count)),
    }
}

#[cfg(test)]
mod tests {
    use colibri_check::format::parse_spec;

    use super::*;

    fn fmt(v: Value, spec: &str) -> String {
        format_value(&v, &parse_spec(spec).expect("valid spec"))
    }

    #[test]
    fn precision_on_floats_is_digits_after_the_point() {
        assert_eq!(fmt(Value::Float(1.23456), ".2"), "1.23");
        assert_eq!(fmt(Value::Float(2.0), ".3"), "2.000");
        assert_eq!(fmt(Value::Float(-0.001), ".1"), "-0.0");
        assert_eq!(fmt(Value::Float(f64::INFINITY), ".2"), "inf");
        assert_eq!(fmt(Value::Float(f64::NAN), ".2"), "nan");
    }

    #[test]
    fn width_alignment_and_fill() {
        assert_eq!(fmt(Value::Int(42), ">6"), "    42");
        assert_eq!(fmt(Value::Int(42), "6"), "    42");
        assert_eq!(fmt(Value::Int(42), "<6"), "42    ");
        assert_eq!(fmt(Value::Int(42), "^6"), "  42  ");
        assert_eq!(fmt(Value::Int(42), "*^7"), "**42***");
        assert_eq!(fmt(Value::str("ab"), "5"), "ab   ");
        assert_eq!(fmt(Value::str("ab"), ">5"), "   ab");
        assert_eq!(fmt(Value::str("abcdef"), ".3"), "abc");
        assert_eq!(fmt(Value::str("日本"), ">4"), "  日本");
    }

    #[test]
    fn zero_flag_pads_after_the_sign() {
        assert_eq!(fmt(Value::Int(-42), "06"), "-00042");
        assert_eq!(fmt(Value::Float(2.5), "07.2"), "0002.50");
        assert_eq!(fmt(Value::Int(7), "<03"), "700");
    }

    #[test]
    fn integer_bases() {
        assert_eq!(fmt(Value::Int(255), "x"), "ff");
        assert_eq!(fmt(Value::Int(255), "X"), "FF");
        assert_eq!(fmt(Value::Int(5), "b"), "101");
        assert_eq!(fmt(Value::Int(8), "o"), "10");
        assert_eq!(fmt(Value::Int(-255), "x"), "-ff");
        assert_eq!(fmt(Value::Int(255), "08b"), "11111111");
        assert_eq!(fmt(Value::Int(i64::MIN), "x"), "-8000000000000000");
    }

    #[test]
    fn narrower_than_the_value_does_not_truncate() {
        assert_eq!(fmt(Value::Int(123456), "3"), "123456");
    }
}
