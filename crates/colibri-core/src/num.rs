//! Numeric semantics of every width (DESIGN §5.1; ARCHITECTURE §6).
//!
//! Integers are `Value::Int(i64)` for every type but `u64`, which is `Value::U64(u64)`. The
//! operator tells the width, so each operation widens its operands to `i128`, computes
//! exactly, and checks the result against the type's range: that is the whole of "overflow
//! traps in every mode". Functions return the text of the panic on failure.

use colibri_check::hir::{ArithOp, BitOp, Num, NumMethod};
use colibri_check::types::IntTy;

use crate::show::{show_f32, show_float};
use crate::value::Value;

/// A numeric failure: the panic message.
#[doc(hidden)]
pub type Trap = String;

/// The integer in `v` (of any integer type), exactly.
#[doc(hidden)]
pub fn int_of(v: &Value) -> Option<i128> {
    match v {
        Value::Int(i) => Some(i128::from(*i)),
        Value::U64(u) => Some(i128::from(*u)),
        _ => None,
    }
}

/// The value of integer type `ty` that is `v`; `v` must be in range.
#[doc(hidden)]
pub fn make_int(ty: IntTy, v: i128) -> Value {
    debug_assert!(ty.fits(v));
    if ty == IntTy::U64 {
        Value::U64(v as u64)
    } else {
        Value::Int(v as i64)
    }
}

/// `v` as a value of `ty`, or the overflow trap.
fn checked(ty: IntTy, v: Option<i128>) -> Result<i128, Trap> {
    match v {
        Some(v) if ty.fits(v) => Ok(v),
        _ => Err("integer overflow".to_string()),
    }
}

/// `v` reduced to `ty` by wrapping around (two's complement).
fn wrap(ty: IntTy, v: i128) -> i128 {
    let bits = ty.bits();
    let unsigned = ((v as u128) & ((1u128 << bits) - 1)) as i128;
    if ty.is_signed() && unsigned >= 1i128 << (bits - 1) {
        unsigned - (1i128 << bits)
    } else {
        unsigned
    }
}

/// `a op b` on integers of type `ty`: traps on overflow and on division by zero; `/`
/// truncates toward zero and `%` takes the sign of the dividend.
#[doc(hidden)]
pub fn int_arith(op: ArithOp, ty: IntTy, a: i128, b: i128) -> Result<i128, Trap> {
    match op {
        ArithOp::Add => checked(ty, a.checked_add(b)),
        ArithOp::Sub => checked(ty, a.checked_sub(b)),
        ArithOp::Mul => checked(ty, a.checked_mul(b)),
        ArithOp::Div if b == 0 => Err("division by zero".to_string()),
        ArithOp::Div => checked(ty, a.checked_div(b)),
        ArithOp::Rem if b == 0 => Err("modulo by zero".to_string()),
        // `MIN % -1` is mathematically 0 but traps, like `MIN / -1`.
        ArithOp::Rem if a == ty.min() && b == -1 => Err("integer overflow".to_string()),
        ArithOp::Rem => checked(ty, a.checked_rem(b)),
    }
}

/// `-a` on an integer of type `ty`.
#[doc(hidden)]
pub fn int_neg(ty: IntTy, a: i128) -> Result<i128, Trap> {
    checked(ty, a.checked_neg())
}

/// `~a` on an integer of type `ty`.
#[doc(hidden)]
pub fn int_not(ty: IntTy, a: i128) -> i128 {
    wrap(ty, !a)
}

/// `a op b` for the bitwise operators on integers of type `ty`. A shift count outside
/// `0..bits` traps; bits shifted out of `<<` are lost without a trap (a shift is a bit
/// operation, not arithmetic), and `>>` is arithmetic on signed types, logical on unsigned.
#[doc(hidden)]
pub fn int_bit(op: BitOp, ty: IntTy, a: i128, b: i128) -> Result<i128, Trap> {
    Ok(match op {
        BitOp::And => a & b,
        BitOp::Or => a | b,
        BitOp::Xor => a ^ b,
        BitOp::Shl | BitOp::Shr => {
            if b < 0 || b >= i128::from(ty.bits()) {
                return Err(format!("shift by {b} is out of range for `{}`", ty.name()));
            }
            if op == BitOp::Shl {
                wrap(ty, a << b)
            } else {
                a >> b
            }
        }
    })
}

/// `a op b` on `f64`s (IEEE).
#[doc(hidden)]
pub fn float_arith(op: ArithOp, a: f64, b: f64) -> f64 {
    match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => a / b,
        ArithOp::Rem => a % b,
    }
}

/// `a op b` on `f32`s: computed in single precision, so every result is rounded to `f32`.
#[doc(hidden)]
pub fn f32_arith(op: ArithOp, a: f32, b: f32) -> f32 {
    match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => a / b,
        ArithOp::Rem => a % b,
    }
}

/// The conversion call `to(x)` for `x` of type `from` (DESIGN §5.1).
#[doc(hidden)]
pub fn convert(from: Num, to: Num, v: &Value) -> Result<Value, Trap> {
    let fits = |ty: IntTy, n: i128| -> Result<Value, Trap> {
        if ty.fits(n) {
            Ok(make_int(ty, n))
        } else {
            Err(format!("value {n} does not fit in `{}`", ty.name()))
        }
    };
    match (v, to) {
        (Value::Int(_) | Value::U64(_), Num::Int(ty)) => fits(ty, int_of(v).unwrap_or(0)),
        (Value::Int(_) | Value::U64(_), Num::Float) => {
            Ok(Value::Float(int_of(v).unwrap_or(0) as f64))
        }
        (Value::Int(_) | Value::U64(_), Num::F32) => Ok(Value::F32(int_of(v).unwrap_or(0) as f32)),
        (Value::Float(x), Num::Float) => Ok(Value::Float(*x)),
        (Value::Float(x), Num::F32) => Ok(Value::F32(*x as f32)),
        (Value::F32(x), Num::Float) => Ok(Value::Float(f64::from(*x))),
        (Value::F32(x), Num::F32) => Ok(Value::F32(*x)),
        (Value::Float(x), Num::Int(ty)) => float_to_int(*x, &show_float(*x), ty),
        (Value::F32(x), Num::Int(ty)) => float_to_int(f64::from(*x), &show_f32(*x), ty),
        _ => Err(format!(
            "cannot convert to `{}` (an ill-typed conversion from `{}`)",
            to.name(),
            from.name()
        )),
    }
}

/// A float truncated toward zero; `nan` and values outside the type trap.
fn float_to_int(x: f64, shown: &str, ty: IntTy) -> Result<Value, Trap> {
    if x.is_nan() {
        return Err(format!("cannot convert `nan` to `{}`", ty.name()));
    }
    let t = x.trunc();
    // Both bounds are exact in `f64` (powers of two), so the comparison is exact.
    let (low, high) = (ty.min() as f64, (ty.max() + 1) as f64);
    if t >= low && t < high {
        Ok(make_int(ty, t as i128))
    } else {
        Err(format!("value {shown} does not fit in `{}`", ty.name()))
    }
}

/// `x.method(args)` on a number of type `num`; `args[0]` is the receiver.
#[doc(hidden)]
pub fn method(m: NumMethod, num: Num, args: &[Value]) -> Result<Value, Trap> {
    match num {
        Num::Int(ty) => int_method(m, ty, args),
        Num::Float => {
            let x = float_arg(args, 0)?;
            Ok(match m {
                NumMethod::Abs => Value::Float(x.abs()),
                NumMethod::Sqrt => Value::Float(x.sqrt()),
                NumMethod::Floor => Value::Float(x.floor()),
                NumMethod::Ceil => Value::Float(x.ceil()),
                NumMethod::Round => Value::Float(x.round()),
                NumMethod::Min => Value::Float(x.min(float_arg(args, 1)?)),
                NumMethod::Max => Value::Float(x.max(float_arg(args, 1)?)),
                NumMethod::IsNan => Value::Bool(x.is_nan()),
                _ => return Err("an ill-typed float method".to_string()),
            })
        }
        Num::F32 => {
            let x = match args.first() {
                Some(Value::F32(x)) => *x,
                _ => return Err("an ill-typed f32 method".to_string()),
            };
            let other = || match args.get(1) {
                Some(Value::F32(y)) => Ok(*y),
                _ => Err("an ill-typed f32 method".to_string()),
            };
            Ok(match m {
                NumMethod::Abs => Value::F32(x.abs()),
                NumMethod::Sqrt => Value::F32(x.sqrt()),
                NumMethod::Floor => Value::F32(x.floor()),
                NumMethod::Ceil => Value::F32(x.ceil()),
                NumMethod::Round => Value::F32(x.round()),
                NumMethod::Min => Value::F32(x.min(other()?)),
                NumMethod::Max => Value::F32(x.max(other()?)),
                NumMethod::IsNan => Value::Bool(x.is_nan()),
                _ => return Err("an ill-typed f32 method".to_string()),
            })
        }
    }
}

fn float_arg(args: &[Value], i: usize) -> Result<f64, Trap> {
    match args.get(i) {
        Some(Value::Float(x)) => Ok(*x),
        _ => Err("an ill-typed float method".to_string()),
    }
}

fn int_method(m: NumMethod, ty: IntTy, args: &[Value]) -> Result<Value, Trap> {
    let ill = || "an ill-typed integer method".to_string();
    let a = args.first().and_then(int_of).ok_or_else(ill)?;
    let b = args.get(1).and_then(int_of);
    let other = || b.ok_or_else(ill);
    let make = |n: i128| Ok(make_int(ty, n));
    let wrapping = |n: i128| Ok(make_int(ty, wrap(ty, n)));
    // Only a `u64` product can leave `i128`, and then it is far above the type's maximum.
    let saturating = |n: Option<i128>| {
        let n = n.unwrap_or(ty.max());
        Ok(make_int(ty, n.clamp(ty.min(), ty.max())))
    };
    let option = |n: Option<i128>| {
        Ok(Value::option(
            n.filter(|n| ty.fits(*n)).map(|n| make_int(ty, n)),
        ))
    };
    match m {
        NumMethod::WrappingAdd => wrapping(a.wrapping_add(other()?)),
        NumMethod::WrappingSub => wrapping(a.wrapping_sub(other()?)),
        NumMethod::WrappingMul => wrapping(a.wrapping_mul(other()?)),
        NumMethod::CheckedAdd => option(a.checked_add(other()?)),
        NumMethod::CheckedSub => option(a.checked_sub(other()?)),
        NumMethod::CheckedMul => option(a.checked_mul(other()?)),
        NumMethod::CheckedDiv => {
            let b = other()?;
            option(if b == 0 { None } else { a.checked_div(b) })
        }
        NumMethod::SaturatingAdd => saturating(a.checked_add(other()?)),
        NumMethod::SaturatingSub => saturating(a.checked_sub(other()?)),
        NumMethod::SaturatingMul => saturating(a.checked_mul(other()?)),
        NumMethod::Abs => make(checked(ty, a.checked_abs())?),
        NumMethod::Min => make(a.min(other()?)),
        NumMethod::Max => make(a.max(other()?)),
        NumMethod::Pow => {
            let exp = other()?;
            if exp < 0 {
                return Err(format!("negative exponent {exp} in `pow`"));
            }
            make(int_pow(ty, a, exp)?)
        }
        _ => Err(ill()),
    }
}

/// `a` to the power `exp` (not negative), trapping when the result leaves `ty`.
fn int_pow(ty: IntTy, a: i128, exp: i128) -> Result<i128, Trap> {
    match a {
        0 => return Ok(i128::from(exp == 0)),
        1 => return Ok(1),
        -1 => return Ok(if exp % 2 == 0 { 1 } else { -1 }),
        _ => {}
    }
    // |a| >= 2, so more than 127 factors cannot fit any type.
    if exp > 127 {
        return Err("integer overflow".to_string());
    }
    let mut result = 1i128;
    for _ in 0..exp {
        result = checked(ty, result.checked_mul(a))?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_traps_at_the_width_of_the_type() {
        assert!(int_arith(ArithOp::Add, IntTy::U8, 255, 1).is_err());
        assert_eq!(int_arith(ArithOp::Add, IntTy::U8, 254, 1), Ok(255));
        assert!(int_arith(ArithOp::Div, IntTy::I32, i128::from(i32::MIN), -1).is_err());
        assert!(int_arith(ArithOp::Rem, IntTy::I32, i128::from(i32::MIN), -1).is_err());
        assert_eq!(int_arith(ArithOp::Rem, IntTy::I32, -7, 3), Ok(-1));
        assert_eq!(int_arith(ArithOp::Div, IntTy::I32, -7, 2), Ok(-3));
        assert!(
            int_arith(
                ArithOp::Mul,
                IntTy::U64,
                i128::from(u64::MAX),
                i128::from(u64::MAX)
            )
            .is_err()
        );
    }

    #[test]
    fn shifts_check_the_count_and_drop_bits() {
        assert_eq!(
            int_bit(BitOp::Shl, IntTy::U8, 0b1000_0001, 1),
            Ok(0b0000_0010)
        );
        assert_eq!(int_bit(BitOp::Shl, IntTy::I8, 64, 1), Ok(-128));
        assert_eq!(int_bit(BitOp::Shr, IntTy::I8, -128, 7), Ok(-1));
        assert_eq!(int_bit(BitOp::Shr, IntTy::U8, 128, 7), Ok(1));
        assert!(int_bit(BitOp::Shl, IntTy::U8, 1, 8).is_err());
        assert!(int_bit(BitOp::Shr, IntTy::I64, 1, -1).is_err());
        assert_eq!(int_not(IntTy::U8, 0), 255);
        assert_eq!(int_not(IntTy::I8, 0), -1);
    }

    #[test]
    fn conversions_check_the_range() {
        let u8_ = Num::Int(IntTy::U8);
        assert!(convert(Num::INT, u8_, &Value::Int(300)).is_err());
        assert_eq!(
            convert(Num::INT, u8_, &Value::Int(255)),
            Ok(Value::Int(255))
        );
        assert!(convert(Num::Float, u8_, &Value::Float(-1.0)).is_err());
        assert_eq!(
            convert(Num::Float, u8_, &Value::Float(-0.5)),
            Ok(Value::Int(0))
        );
        assert!(convert(Num::Float, Num::INT, &Value::Float(f64::NAN)).is_err());
        assert!(convert(Num::Float, Num::INT, &Value::Float(9.3e18)).is_err());
        assert_eq!(
            convert(Num::Float, Num::Int(IntTy::U64), &Value::Float(1.0e19)),
            Ok(Value::U64(10_000_000_000_000_000_000))
        );
    }

    #[test]
    fn wrapping_and_saturating() {
        let args = [Value::Int(250), Value::Int(10)];
        let u8_ = Num::Int(IntTy::U8);
        assert_eq!(
            method(NumMethod::WrappingAdd, u8_, &args),
            Ok(Value::Int(4))
        );
        assert_eq!(
            method(NumMethod::SaturatingAdd, u8_, &args),
            Ok(Value::Int(255))
        );
        assert_eq!(
            method(NumMethod::CheckedAdd, u8_, &args),
            Ok(Value::option(None))
        );
        let sub = [Value::Int(5), Value::Int(10)];
        assert_eq!(
            method(NumMethod::SaturatingSub, u8_, &sub),
            Ok(Value::Int(0))
        );
        assert_eq!(
            method(NumMethod::WrappingSub, u8_, &sub),
            Ok(Value::Int(251))
        );
    }
}
