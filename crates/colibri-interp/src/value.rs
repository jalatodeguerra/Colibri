//! Runtime values (ARCHITECTURE §3).
//!
//! Only the variants the language slice needs exist. The rest of the architecture's `Value`
//! (structs, tuples, enums, collections, class objects, closures, host handles) joins this enum
//! as those features land; strings are already reference counted like every heap value will be.

use std::borrow::Cow;
use std::rc::Rc;

/// A Colibri value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Unit,
    Bool(bool),
    /// Every integer width (`int` is 64-bit; narrower types are range-checked on operations).
    Int(i64),
    Float(f64),
    Char(char),
    Str(Rc<str>),
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Str(Rc::from(s))
    }
}

/// The `Show` form of a value: what `print` writes and `"{x}"` interpolates.
///
/// Floats print in the shortest form that reads back as the same number, and always look like
/// floats: `1.5`, `2.0`, `-0.0`, `1e-7`, `1e16` (Rust's `{:?}` rules: exponent form below
/// `1e-4` and from `1e16` up), plus `inf`, `-inf` and `nan`.
pub fn show(v: &Value) -> Cow<'_, str> {
    match v {
        Value::Unit => Cow::Borrowed("()"),
        Value::Bool(true) => Cow::Borrowed("true"),
        Value::Bool(false) => Cow::Borrowed("false"),
        Value::Int(i) => Cow::Owned(i.to_string()),
        Value::Float(x) => Cow::Owned(show_float(*x)),
        Value::Char(c) => Cow::Owned(c.to_string()),
        Value::Str(s) => Cow::Borrowed(s),
    }
}

/// The `Show` form of a float.
pub fn show_float(x: f64) -> String {
    if x.is_nan() {
        "nan".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "inf" } else { "-inf" }.to_string()
    } else {
        format!("{x:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_always_look_like_floats() {
        assert_eq!(show_float(1.5), "1.5");
        assert_eq!(show_float(2.0), "2.0");
        assert_eq!(show_float(-0.0), "-0.0");
        assert_eq!(show_float(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(show_float(1e-9), "1e-9");
        assert_eq!(show_float(1e16), "1e16");
        assert_eq!(show_float(123456789.0), "123456789.0");
        assert_eq!(show_float(f64::INFINITY), "inf");
        assert_eq!(show_float(f64::NEG_INFINITY), "-inf");
        assert_eq!(show_float(f64::NAN), "nan");
    }

    #[test]
    fn show_of_each_value() {
        assert_eq!(show(&Value::Unit), "()");
        assert_eq!(show(&Value::Bool(true)), "true");
        assert_eq!(show(&Value::Int(-7)), "-7");
        assert_eq!(show(&Value::Char('x')), "x");
        assert_eq!(show(&Value::str("a b")), "a b");
    }
}
