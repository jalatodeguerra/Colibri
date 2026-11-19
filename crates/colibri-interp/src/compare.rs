//! The structural `Eq` and `Ord` every struct, enum and tuple derives (DESIGN §7.4).

use std::cmp::Ordering;

use crate::value::Value;

/// Values nested deeper than this are not compared (the answer would need a deeper Rust
/// recursion than is safe); the caller turns it into a panic.
const MAX_DEPTH: u32 = 2_000;

/// A comparison gave up because the values nest too deeply.
#[derive(Debug, PartialEq, Eq)]
pub struct TooDeep;

/// Structural equality. Floats compare as IEEE numbers, so a value containing `nan` is not
/// equal to itself, exactly as `nan == nan` is false.
pub fn values_equal(a: &Value, b: &Value) -> Result<bool, TooDeep> {
    equal(a, b, 0)
}

fn equal(a: &Value, b: &Value, depth: u32) -> Result<bool, TooDeep> {
    if depth > MAX_DEPTH {
        return Err(TooDeep);
    }
    let all = |xs: &[Value], ys: &[Value]| -> Result<bool, TooDeep> {
        if xs.len() != ys.len() {
            return Ok(false);
        }
        for (x, y) in xs.iter().zip(ys) {
            if !equal(x, y, depth + 1)? {
                return Ok(false);
            }
        }
        Ok(true)
    };
    Ok(match (a, b) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Char(x), Value::Char(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Tuple(x), Value::Tuple(y)) => all(x, y)?,
        (Value::Struct(x), Value::Struct(y)) => x.ty == y.ty && all(&x.fields, &y.fields)?,
        (Value::Enum(x), Value::Enum(y)) => {
            x.ty == y.ty && x.variant == y.variant && all(&x.payload, &y.payload)?
        }
        _ => false,
    })
}

/// Structural ordering: enums by variant (then payload), tuples and structs field by field.
/// `None` when a float `nan` makes two values unordered.
pub fn values_cmp(a: &Value, b: &Value) -> Result<Option<Ordering>, TooDeep> {
    cmp(a, b, 0)
}

fn cmp(a: &Value, b: &Value, depth: u32) -> Result<Option<Ordering>, TooDeep> {
    if depth > MAX_DEPTH {
        return Err(TooDeep);
    }
    let lexicographic = |xs: &[Value], ys: &[Value]| -> Result<Option<Ordering>, TooDeep> {
        for (x, y) in xs.iter().zip(ys) {
            match cmp(x, y, depth + 1)? {
                Some(Ordering::Equal) => {}
                other => return Ok(other),
            }
        }
        Ok(xs.len().partial_cmp(&ys.len()))
    };
    Ok(match (a, b) {
        (Value::Unit, Value::Unit) => Some(Ordering::Equal),
        (Value::Bool(x), Value::Bool(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Int(y)) => x.partial_cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::Char(x), Value::Char(y)) => x.partial_cmp(y),
        (Value::Str(x), Value::Str(y)) => x.partial_cmp(y),
        (Value::Tuple(x), Value::Tuple(y)) => lexicographic(x, y)?,
        (Value::Struct(x), Value::Struct(y)) => lexicographic(&x.fields, &y.fields)?,
        (Value::Enum(x), Value::Enum(y)) => match x.variant.cmp(&y.variant) {
            Ordering::Equal => lexicographic(&x.payload, &y.payload)?,
            other => Some(other),
        },
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use colibri_check::types::TypeId;

    use super::*;

    fn int(i: i64) -> Value {
        Value::Int(i)
    }

    #[test]
    fn tuples_compare_lexicographically() {
        let a = Value::tuple(vec![int(1), Value::str("b")]);
        let b = Value::tuple(vec![int(1), Value::str("c")]);
        let c = Value::tuple(vec![int(2), Value::str("a")]);
        assert_eq!(values_cmp(&a, &b), Ok(Some(Ordering::Less)));
        assert_eq!(values_cmp(&c, &b), Ok(Some(Ordering::Greater)));
        assert_eq!(values_cmp(&a, &a), Ok(Some(Ordering::Equal)));
    }

    #[test]
    fn enums_order_by_declaration() {
        let (n, e) = (
            Value::variant(TypeId(0), 0, vec![]),
            Value::variant(TypeId(0), 1, vec![]),
        );
        assert_eq!(values_cmp(&n, &e), Ok(Some(Ordering::Less)));
        assert_eq!(values_equal(&n, &e), Ok(false));
        assert_eq!(values_equal(&e, &e), Ok(true));
    }

    #[test]
    fn nan_makes_values_unordered_and_unequal() {
        let nan = Value::tuple(vec![Value::Float(f64::NAN)]);
        assert_eq!(values_equal(&nan, &nan), Ok(false));
        assert_eq!(values_cmp(&nan, &nan), Ok(None));
    }

    #[test]
    fn structural_equality_looks_inside() {
        let a = Value::strukt(TypeId(0), vec![int(1), Value::tuple(vec![int(2)])]);
        let b = Value::strukt(TypeId(0), vec![int(1), Value::tuple(vec![int(2)])]);
        let c = Value::strukt(TypeId(0), vec![int(1), Value::tuple(vec![int(3)])]);
        assert_eq!(values_equal(&a, &b), Ok(true));
        assert_eq!(values_equal(&a, &c), Ok(false));
    }

    #[test]
    fn very_deep_values_give_up_instead_of_overflowing() {
        let mut a = Value::Unit;
        let mut b = Value::Unit;
        for _ in 0..50_000 {
            a = Value::variant(TypeId(0), 0, vec![a]);
            b = Value::variant(TypeId(0), 0, vec![b]);
        }
        assert_eq!(values_equal(&a, &b), Err(TooDeep));
    }
}
