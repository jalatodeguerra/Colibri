//! The structural `Eq` and `Ord` every struct, enum and tuple derives (DESIGN §7.4).

use std::cmp::Ordering;
use std::rc::{Rc, Weak};

use crate::hooks::{Overrides, Plain, Stop};
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
    equal(a, b, 0, &mut Plain).map_err(|_| TooDeep)
}

/// Equality that consults `ov` first for every user-defined value (a user `impl Eq`).
pub(crate) fn equal_with(a: &Value, b: &Value, ov: &mut dyn Overrides) -> Result<bool, Stop> {
    equal(a, b, 0, ov)
}

/// `==` on two scalars; `None` if they are not both of one scalar type. (Kept out of the
/// recursive function so that its stack frame stays small.)
#[inline(never)]
fn scalar_eq(a: &Value, b: &Value) -> Option<bool> {
    Some(match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::U64(x), Value::U64(y)) => x == y,
        (Value::Ptr(x), Value::Ptr(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::F32(x), Value::F32(y)) => x == y,
        (Value::Char(x), Value::Char(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        _ => return None,
    })
}

/// The ordering of two scalars; the outer `None` if they are not both of one scalar type.
#[inline(never)]
fn scalar_cmp(a: &Value, b: &Value) -> Option<Option<Ordering>> {
    Some(match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Int(y)) => x.partial_cmp(y),
        (Value::U64(x), Value::U64(y)) => x.partial_cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::F32(x), Value::F32(y)) => x.partial_cmp(y),
        (Value::Char(x), Value::Char(y)) => x.partial_cmp(y),
        (Value::Str(x), Value::Str(y)) => x.partial_cmp(y),
        _ => return None,
    })
}

fn equal(a: &Value, b: &Value, depth: u32, ov: &mut dyn Overrides) -> Result<bool, Stop> {
    if depth > MAX_DEPTH {
        return Err(Stop::TooDeep);
    }
    if let Some(answer) = override_eq(a, b, ov) {
        return answer;
    }
    Ok(match (a, b) {
        (Value::Unit, Value::Unit) => true,
        _ if let Some(answer) = scalar_eq(a, b) => answer,
        (Value::Tuple(x), Value::Tuple(y)) => equal_all(x, y, depth, ov)?,
        (Value::Struct(x), Value::Struct(y)) => {
            x.ty == y.ty && equal_all(&x.fields, &y.fields, depth, ov)?
        }
        (Value::Enum(x), Value::Enum(y)) => {
            x.ty == y.ty && x.variant == y.variant && equal_all(&x.payload, &y.payload, depth, ov)?
        }
        (Value::List(_) | Value::Map(_) | Value::Set(_), _) => equal_collections(a, b, depth, ov)?,
        // A class is compared by identity (DESIGN §7.2).
        (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
        (Value::Host(x), Value::Host(y)) => x.index == y.index && x.generation == y.generation,
        (Value::Weak(x), Value::Weak(y)) => Weak::ptr_eq(x, y),
        _ => same_range(a, b),
    })
}

/// `a..b == c..d`: ranges are equal when their ends are.
#[inline(never)]
fn same_range(a: &Value, b: &Value) -> bool {
    matches!((a, b), (Value::Range(x), Value::Range(y)) if x == y)
}

/// The user-written `Eq` of a user-defined value, if it has one.
#[inline(never)]
fn override_eq(a: &Value, b: &Value, ov: &mut dyn Overrides) -> Option<Result<bool, Stop>> {
    match a {
        Value::Struct(_) | Value::Enum(_) | Value::Obj(_) | Value::Host(_) => ov.eq(a, b),
        _ => None,
    }
}

/// The user-written `Ord` of a user-defined value, if it has one.
#[inline(never)]
fn override_cmp(
    a: &Value,
    b: &Value,
    ov: &mut dyn Overrides,
) -> Option<Result<Option<Ordering>, Stop>> {
    match a {
        Value::Struct(_) | Value::Enum(_) | Value::Obj(_) | Value::Host(_) => ov.cmp(a, b),
        _ => None,
    }
}

/// Element-wise equality, kept out of [`equal`] so that its frame stays small.
#[inline(never)]
fn equal_all(xs: &[Value], ys: &[Value], depth: u32, ov: &mut dyn Overrides) -> Result<bool, Stop> {
    if xs.len() != ys.len() {
        return Ok(false);
    }
    for (x, y) in xs.iter().zip(ys) {
        if !equal(x, y, depth + 1, ov)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Structural equality of two collections, kept out of [`equal`] so that its frame stays small
/// (it recurses once per nesting level).
#[inline(never)]
fn equal_collections(
    a: &Value,
    b: &Value,
    depth: u32,
    ov: &mut dyn Overrides,
) -> Result<bool, Stop> {
    Ok(match (a, b) {
        (Value::List(x), Value::List(y)) => {
            if x.len() != y.len() {
                return Ok(false);
            }
            for (p, q) in x.snapshot().iter().zip(y.snapshot().iter()) {
                if !equal(p, q, depth + 1, ov)? {
                    return Ok(false);
                }
            }
            true
        }
        (Value::Map(x), Value::Map(y)) => {
            if x.len() != y.len() {
                return Ok(false);
            }
            for (k, v) in x.snapshot() {
                match y.get(&k) {
                    Some(other) if equal(&v, &other, depth + 1, ov)? => {}
                    _ => return Ok(false),
                }
            }
            true
        }
        (Value::Set(x), Value::Set(y)) => {
            x.len() == y.len() && x.snapshot().iter().all(|item| y.contains(item))
        }
        _ => false,
    })
}

/// `a is b`: whether two reference values are the same object.
pub fn values_identical(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::List(x), Value::List(y)) => Rc::ptr_eq(x, y),
        (Value::Map(x), Value::Map(y)) => Rc::ptr_eq(x, y),
        (Value::Set(x), Value::Set(y)) => Rc::ptr_eq(x, y),
        (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
        (Value::Host(x), Value::Host(y)) => x.index == y.index && x.generation == y.generation,
        _ => false,
    }
}

/// Structural ordering: enums by variant (then payload), tuples and structs field by field.
/// `None` when a float `nan` makes two values unordered.
pub fn values_cmp(a: &Value, b: &Value) -> Result<Option<Ordering>, TooDeep> {
    cmp(a, b, 0, &mut Plain).map_err(|_| TooDeep)
}

/// Ordering that consults `ov` first for every user-defined value (a user `impl Ord`).
pub(crate) fn cmp_with(
    a: &Value,
    b: &Value,
    ov: &mut dyn Overrides,
) -> Result<Option<Ordering>, Stop> {
    cmp(a, b, 0, ov)
}

fn cmp(a: &Value, b: &Value, depth: u32, ov: &mut dyn Overrides) -> Result<Option<Ordering>, Stop> {
    if depth > MAX_DEPTH {
        return Err(Stop::TooDeep);
    }
    if let Some(answer) = override_cmp(a, b, ov) {
        return answer;
    }
    Ok(match (a, b) {
        (Value::Unit, Value::Unit) => Some(Ordering::Equal),
        _ if let Some(answer) = scalar_cmp(a, b) => answer,
        (Value::Tuple(x), Value::Tuple(y)) => lexicographic(x, y, depth, ov)?,
        (Value::Struct(x), Value::Struct(y)) => lexicographic(&x.fields, &y.fields, depth, ov)?,
        (Value::Enum(x), Value::Enum(y)) => match x.variant.cmp(&y.variant) {
            Ordering::Equal => lexicographic(&x.payload, &y.payload, depth, ov)?,
            other => Some(other),
        },
        _ => None,
    })
}

/// Field-by-field ordering, kept out of [`cmp`] so that its frame stays small.
#[inline(never)]
fn lexicographic(
    xs: &[Value],
    ys: &[Value],
    depth: u32,
    ov: &mut dyn Overrides,
) -> Result<Option<Ordering>, Stop> {
    for (x, y) in xs.iter().zip(ys) {
        match cmp(x, y, depth + 1, ov)? {
            Some(Ordering::Equal) => {}
            other => return Ok(other),
        }
    }
    Ok(xs.len().partial_cmp(&ys.len()))
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
