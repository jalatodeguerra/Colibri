//! The vector types (DESIGN §15.1; ARCHITECTURE "Vector types as built").
//!
//! A `vecN` is up to four `f32`s and an `ivecN` up to four `i32`s, stored inline in the value:
//! no allocation, no reference count, so a copy is a copy. Arithmetic is component-wise in
//! single precision (`f32`), left to right, without fused operations, so a result is the same
//! on every platform. Integer vectors trap on overflow and division by zero like `int`.
//! Functions return the text of the panic on failure.

// Component loops index two or three arrays by the same lane; an iterator chain is no clearer.
#![allow(clippy::needless_range_loop)]

use colibri_check::hir::{ArithOp, VecMethod, VecShape};
use colibri_check::types::VecTy;

use crate::show::show_f32;
use crate::value::Value;

/// A float vector value: `n` components of `c` are in use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FVec {
    pub n: u8,
    pub c: [f32; 4],
}

/// An integer vector value: `n` components of `c` are in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IVec {
    pub n: u8,
    pub c: [i32; 4],
}

type Trap = String;

fn overflow() -> Trap {
    "integer overflow".to_string()
}

/// The text of a vector value: `vec2(1.0, 2.5)`, `ivec3(1, 2, 3)`.
pub fn show_vec(v: &Value) -> Option<String> {
    let (name, parts): (&str, Vec<String>) = match v {
        Value::Vec(f) => (
            VecTy { int: false, n: f.n }.name(),
            f.c[..f.n as usize].iter().map(|x| show_f32(*x)).collect(),
        ),
        Value::IVec(i) => (
            VecTy { int: true, n: i.n }.name(),
            i.c[..i.n as usize].iter().map(i32::to_string).collect(),
        ),
        _ => return None,
    };
    Some(format!("{name}({})", parts.join(", ")))
}

/// The component `i` of a vector value, as a scalar value.
pub fn component(v: &Value, i: u8) -> Option<Value> {
    match v {
        Value::Vec(f) if i < f.n => Some(Value::F32(f.c[i as usize])),
        Value::IVec(x) if i < x.n => Some(Value::Int(i64::from(x.c[i as usize]))),
        _ => None,
    }
}

/// Sets the component `i` of a vector value; the old component, or `None` if the value is not
/// a vector of that kind with such a component.
pub fn set_component(v: &mut Value, i: u8, new: &Value) -> Option<Value> {
    match (v, new) {
        (Value::Vec(f), Value::F32(x)) if i < f.n => {
            Some(Value::F32(std::mem::replace(&mut f.c[i as usize], *x)))
        }
        (Value::IVec(f), Value::Int(x)) if i < f.n => {
            let x = i32::try_from(*x).ok()?;
            Some(Value::Int(i64::from(std::mem::replace(
                &mut f.c[i as usize],
                x,
            ))))
        }
        _ => None,
    }
}

/// `vecN(..)` and `ivecN(..)`: from `n` scalars, one scalar, or one vector of either kind.
pub fn construct(ty: VecTy, args: &[Value]) -> Result<Value, Trap> {
    let n = ty.n as usize;
    let ill = || "an ill-typed vector constructor".to_string();
    if ty.int {
        let mut c = [0i32; 4];
        match args {
            [Value::Vec(f)] if f.n == ty.n => {
                for i in 0..n {
                    c[i] = float_to_i32(f.c[i])?;
                }
            }
            [Value::IVec(v)] if v.n == ty.n => c = v.c,
            [Value::Int(x)] => c[..n].fill(i32::try_from(*x).map_err(|_| ill())?),
            xs if xs.len() == n => {
                for (slot, x) in c.iter_mut().zip(xs) {
                    let Value::Int(x) = x else { return Err(ill()) };
                    *slot = i32::try_from(*x).map_err(|_| ill())?;
                }
            }
            _ => return Err(ill()),
        }
        Ok(Value::IVec(IVec { n: ty.n, c }))
    } else {
        let mut c = [0f32; 4];
        match args {
            [Value::IVec(v)] if v.n == ty.n => {
                for i in 0..n {
                    c[i] = v.c[i] as f32;
                }
            }
            [Value::Vec(f)] if f.n == ty.n => c = f.c,
            [Value::F32(x)] => c[..n].fill(*x),
            xs if xs.len() == n => {
                for (slot, x) in c.iter_mut().zip(xs) {
                    let Value::F32(x) = x else { return Err(ill()) };
                    *slot = *x;
                }
            }
            _ => return Err(ill()),
        }
        Ok(Value::Vec(FVec { n: ty.n, c }))
    }
}

/// `ivec2(v)`: truncates toward zero like `i32(x)`, and traps on `nan` or a value out of range.
fn float_to_i32(x: f32) -> Result<i32, Trap> {
    if x.is_nan() {
        return Err("cannot convert `nan` to `i32`".to_string());
    }
    let t = x.trunc();
    if !(-2147483648.0..2147483648.0).contains(&t) {
        return Err(format!("value {} does not fit in `i32`", show_f32(x)));
    }
    Ok(t as i32)
}

/// `v.zyx`: the components picked by `lanes`.
pub fn swizzle(v: &Value, lanes: &[u8; 4], len: u8) -> Result<Value, Trap> {
    let len_us = len as usize;
    match v {
        Value::Vec(f) => {
            let mut c = [0f32; 4];
            for i in 0..len_us {
                c[i] = f.c[lanes[i] as usize];
            }
            Ok(Value::Vec(FVec { n: len, c }))
        }
        Value::IVec(f) => {
            let mut c = [0i32; 4];
            for i in 0..len_us {
                c[i] = f.c[lanes[i] as usize];
            }
            Ok(Value::IVec(IVec { n: len, c }))
        }
        _ => Err("an ill-typed swizzle".to_string()),
    }
}

fn f32_op(op: ArithOp, a: f32, b: f32) -> f32 {
    match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => a / b,
        ArithOp::Rem => a % b,
    }
}

fn i32_op(op: ArithOp, a: i32, b: i32) -> Result<i32, Trap> {
    match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div if b == 0 => return Err("division by zero".to_string()),
        ArithOp::Div => a.checked_div(b),
        ArithOp::Rem if b == 0 => return Err("modulo by zero".to_string()),
        ArithOp::Rem => a.checked_rem(b),
    }
    .ok_or_else(overflow)
}

/// A vector operator: `a + b`, `v * s`, `s * v`.
pub fn arith(op: ArithOp, shape: VecShape, l: &Value, r: &Value) -> Result<Value, Trap> {
    let ill = || "an ill-typed vector operation".to_string();
    match (shape, l, r) {
        (VecShape::VecVec, Value::Vec(a), Value::Vec(b)) if a.n == b.n => {
            let mut c = [0f32; 4];
            for i in 0..a.n as usize {
                c[i] = f32_op(op, a.c[i], b.c[i]);
            }
            Ok(Value::Vec(FVec { n: a.n, c }))
        }
        (VecShape::VecScalar, Value::Vec(a), Value::F32(s)) => {
            let mut c = [0f32; 4];
            for i in 0..a.n as usize {
                c[i] = f32_op(op, a.c[i], *s);
            }
            Ok(Value::Vec(FVec { n: a.n, c }))
        }
        (VecShape::ScalarVec, Value::F32(s), Value::Vec(b)) => {
            let mut c = [0f32; 4];
            for i in 0..b.n as usize {
                c[i] = f32_op(op, *s, b.c[i]);
            }
            Ok(Value::Vec(FVec { n: b.n, c }))
        }
        (VecShape::VecVec, Value::IVec(a), Value::IVec(b)) if a.n == b.n => {
            let mut c = [0i32; 4];
            for i in 0..a.n as usize {
                c[i] = i32_op(op, a.c[i], b.c[i])?;
            }
            Ok(Value::IVec(IVec { n: a.n, c }))
        }
        (VecShape::VecScalar, Value::IVec(a), Value::Int(s)) => {
            let s = i32::try_from(*s).map_err(|_| ill())?;
            let mut c = [0i32; 4];
            for i in 0..a.n as usize {
                c[i] = i32_op(op, a.c[i], s)?;
            }
            Ok(Value::IVec(IVec { n: a.n, c }))
        }
        (VecShape::ScalarVec, Value::Int(s), Value::IVec(b)) => {
            let s = i32::try_from(*s).map_err(|_| ill())?;
            let mut c = [0i32; 4];
            for i in 0..b.n as usize {
                c[i] = i32_op(op, s, b.c[i])?;
            }
            Ok(Value::IVec(IVec { n: b.n, c }))
        }
        _ => Err(ill()),
    }
}

/// `-v`
pub fn neg(v: &Value) -> Result<Value, Trap> {
    match v {
        Value::Vec(a) => {
            let mut c = [0f32; 4];
            for i in 0..a.n as usize {
                c[i] = -a.c[i];
            }
            Ok(Value::Vec(FVec { n: a.n, c }))
        }
        Value::IVec(a) => {
            let mut c = [0i32; 4];
            for i in 0..a.n as usize {
                c[i] = a.c[i].checked_neg().ok_or_else(overflow)?;
            }
            Ok(Value::IVec(IVec { n: a.n, c }))
        }
        _ => Err("an ill-typed vector negation".to_string()),
    }
}

/// The sum of squares in `f32`, left to right.
fn len_sq_f(a: &FVec) -> f32 {
    let mut s = a.c[0] * a.c[0];
    for i in 1..a.n as usize {
        s += a.c[i] * a.c[i];
    }
    s
}

fn dot_f(a: &FVec, b: &FVec) -> f32 {
    let mut s = a.c[0] * b.c[0];
    for i in 1..a.n as usize {
        s += a.c[i] * b.c[i];
    }
    s
}

fn dot_i(a: &IVec, b: &IVec) -> Result<i32, Trap> {
    let mut s: i32 = a.c[0].checked_mul(b.c[0]).ok_or_else(overflow)?;
    for i in 1..a.n as usize {
        let p = a.c[i].checked_mul(b.c[i]).ok_or_else(overflow)?;
        s = s.checked_add(p).ok_or_else(overflow)?;
    }
    Ok(s)
}

fn zip_f(a: &FVec, b: &FVec, f: impl Fn(f32, f32) -> f32) -> Value {
    let mut c = [0f32; 4];
    for i in 0..a.n as usize {
        c[i] = f(a.c[i], b.c[i]);
    }
    Value::Vec(FVec { n: a.n, c })
}

fn zip_i(a: &IVec, b: &IVec, f: impl Fn(i32, i32) -> i32) -> Value {
    let mut c = [0i32; 4];
    for i in 0..a.n as usize {
        c[i] = f(a.c[i], b.c[i]);
    }
    Value::IVec(IVec { n: a.n, c })
}

/// A method of a vector type; `args[0]` is the receiver.
pub fn method(m: VecMethod, args: &[Value]) -> Result<Value, Trap> {
    let ill = || "an ill-typed vector method".to_string();
    match (m, args) {
        // ---- float vectors ----
        (VecMethod::Dot, [Value::Vec(a), Value::Vec(b)]) => Ok(Value::F32(dot_f(a, b))),
        (VecMethod::Len, [Value::Vec(a)]) => Ok(Value::F32(len_sq_f(a).sqrt())),
        (VecMethod::LenSq, [Value::Vec(a)]) => Ok(Value::F32(len_sq_f(a))),
        (VecMethod::Distance, [Value::Vec(a), Value::Vec(b)]) => {
            let d = zip_f(a, b, |x, y| x - y);
            let Value::Vec(d) = d else { return Err(ill()) };
            Ok(Value::F32(len_sq_f(&d).sqrt()))
        }
        (VecMethod::Normalize, [Value::Vec(a)]) => {
            // The zero vector normalizes to itself (not `nan`s).
            let len = len_sq_f(a).sqrt();
            if len == 0.0 {
                return Ok(Value::Vec(*a));
            }
            let mut c = [0f32; 4];
            for i in 0..a.n as usize {
                c[i] = a.c[i] / len;
            }
            Ok(Value::Vec(FVec { n: a.n, c }))
        }
        (VecMethod::Lerp, [Value::Vec(a), Value::Vec(b), Value::Float(t)]) => {
            // `a + (b - a) * t`, with `t` narrowed to `f32`.
            let t = *t as f32;
            Ok(zip_f(a, b, |x, y| x + (y - x) * t))
        }
        (VecMethod::Abs, [Value::Vec(a)]) => {
            let mut c = a.c;
            c.iter_mut().for_each(|x| *x = x.abs());
            Ok(Value::Vec(FVec { n: a.n, c }))
        }
        (VecMethod::Min, [Value::Vec(a), Value::Vec(b)]) => Ok(zip_f(a, b, f32::min)),
        (VecMethod::Max, [Value::Vec(a), Value::Vec(b)]) => Ok(zip_f(a, b, f32::max)),
        (VecMethod::Cross, [Value::Vec(a), Value::Vec(b)]) if a.n == 3 => {
            let (x, y) = (&a.c, &b.c);
            Ok(Value::Vec(FVec {
                n: 3,
                c: [
                    y[2] * x[1] - y[1] * x[2],
                    y[0] * x[2] - y[2] * x[0],
                    y[1] * x[0] - y[0] * x[1],
                    0.0,
                ],
            }))
        }
        // ---- integer vectors ----
        (VecMethod::Dot, [Value::IVec(a), Value::IVec(b)]) => {
            Ok(Value::Int(i64::from(dot_i(a, b)?)))
        }
        (VecMethod::LenSq, [Value::IVec(a)]) => Ok(Value::Int(i64::from(dot_i(a, a)?))),
        (VecMethod::Len, [Value::IVec(a)]) => Ok(Value::F32(len_sq_i64(a).sqrt() as f32)),
        (VecMethod::Distance, [Value::IVec(a), Value::IVec(b)]) => {
            let mut s: i64 = 0;
            for i in 0..a.n as usize {
                let d = i64::from(a.c[i]) - i64::from(b.c[i]);
                s += d * d;
            }
            Ok(Value::F32((s as f64).sqrt() as f32))
        }
        (VecMethod::Abs, [Value::IVec(a)]) => {
            let mut c = a.c;
            for x in &mut c[..a.n as usize] {
                *x = x.checked_abs().ok_or_else(overflow)?;
            }
            Ok(Value::IVec(IVec { n: a.n, c }))
        }
        (VecMethod::Min, [Value::IVec(a), Value::IVec(b)]) => Ok(zip_i(a, b, i32::min)),
        (VecMethod::Max, [Value::IVec(a), Value::IVec(b)]) => Ok(zip_i(a, b, i32::max)),
        (VecMethod::Cross, [Value::IVec(a), Value::IVec(b)]) if a.n == 3 => {
            let cross = |p: usize, q: usize| -> Result<i32, Trap> {
                let l = a.c[p].checked_mul(b.c[q]).ok_or_else(overflow)?;
                let r = a.c[q].checked_mul(b.c[p]).ok_or_else(overflow)?;
                l.checked_sub(r).ok_or_else(overflow)
            };
            Ok(Value::IVec(IVec {
                n: 3,
                c: [cross(1, 2)?, cross(2, 0)?, cross(0, 1)?, 0],
            }))
        }
        _ => Err(ill()),
    }
}

/// The squared length of an integer vector, exactly (in 64 bits, as a float).
fn len_sq_i64(a: &IVec) -> f64 {
    let s: i64 = a.c[..a.n as usize]
        .iter()
        .map(|&x| i64::from(x) * i64::from(x))
        .sum();
    s as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(x: f32, y: f32) -> Value {
        Value::Vec(FVec {
            n: 2,
            c: [x, y, 0.0, 0.0],
        })
    }

    #[test]
    fn shows_like_floats() {
        assert_eq!(show_vec(&v2(1.0, 2.5)).as_deref(), Some("vec2(1.0, 2.5)"));
    }

    #[test]
    fn normalizing_zero_is_zero() {
        let z = v2(0.0, 0.0);
        assert_eq!(
            method(VecMethod::Normalize, std::slice::from_ref(&z)).unwrap(),
            z
        );
    }

    #[test]
    fn integer_vectors_trap() {
        let a = Value::IVec(IVec {
            n: 2,
            c: [i32::MAX, 1, 0, 0],
        });
        let one = Value::IVec(IVec {
            n: 2,
            c: [1, 1, 0, 0],
        });
        assert!(arith(ArithOp::Add, VecShape::VecVec, &a, &one).is_err());
        assert!(arith(ArithOp::Div, VecShape::VecVec, &one, &a).is_ok());
        let zero = Value::IVec(IVec { n: 2, c: [0; 4] });
        assert!(arith(ArithOp::Div, VecShape::VecVec, &one, &zero).is_err());
    }
}
