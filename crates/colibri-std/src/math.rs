//! `math`: constants and functions on `int` and `float`.
//!
//! Everything that is not an exact IEEE 754 operation (`sin`, `exp`, `ln`, `pow`, ...) is
//! computed by the `libm` crate, a pure Rust port of musl's C library, so a program prints the
//! same digits on every platform. `sqrt`, `floor`, `ceil`, `round`, `trunc` and the arithmetic
//! are exact by definition and use the hardware.

use colibri_runtime::{Ctx, Error, Vm};

use crate::plain;

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("math");
    m.register_const_float("PI", std::f64::consts::PI)?;
    m.register_const_float("TAU", std::f64::consts::TAU)?;
    m.register_const_float("E", std::f64::consts::E)?;
    m.register_const_float("SQRT2", std::f64::consts::SQRT_2)?;
    m.register_const_float("LN2", std::f64::consts::LN_2)?;
    m.register_const_float("LN10", std::f64::consts::LN_10)?;

    // One argument, one float result, through `libm`.
    macro_rules! unary {
        ($($name:literal => $f:expr),* $(,)?) => {$(
            plain(m.register_fn_named($name, ["x"], |x: f64| -> f64 { $f(x) }))?;
        )*};
    }
    unary! {
        "sin" => libm::sin, "cos" => libm::cos, "tan" => libm::tan,
        "asin" => libm::asin, "acos" => libm::acos, "atan" => libm::atan,
        "sinh" => libm::sinh, "cosh" => libm::cosh, "tanh" => libm::tanh,
        "exp" => libm::exp, "exp2" => libm::exp2,
        "ln" => libm::log, "log2" => libm::log2, "log10" => libm::log10,
        "cbrt" => libm::cbrt,
        "sqrt" => f64::sqrt, "floor" => f64::floor, "ceil" => f64::ceil,
        "round" => f64::round, "trunc" => f64::trunc,
        "fract" => |x: f64| x - x.trunc(),
        "radians" => |degrees: f64| degrees * (std::f64::consts::PI / 180.0),
        "degrees" => |radians: f64| radians * (180.0 / std::f64::consts::PI),
        "signf" => |x: f64| if x.is_nan() || x == 0.0 { x } else { x.signum() },
    }

    plain(m.register_fn_named("atan2", ["y", "x"], |y: f64, x: f64| libm::atan2(y, x)))?;
    plain(m.register_fn_named("pow", ["x", "y"], |x: f64, y: f64| libm::pow(x, y)))?;
    plain(m.register_fn_named("hypot", ["x", "y"], |x: f64, y: f64| libm::hypot(x, y)))?;
    plain(
        m.register_fn_named("log", ["x", "base"], |x: f64, base: f64| {
            libm::log(x) / libm::log(base)
        }),
    )?;
    plain(
        m.register_fn_named("lerp", ["a", "b", "t"], |a: f64, b: f64, t: f64| {
            a + (b - a) * t
        }),
    )?;
    plain(m.register_fn_named("is_nan", ["x"], |x: f64| x.is_nan()))?;
    plain(m.register_fn_named("is_finite", ["x"], |x: f64| x.is_finite()))?;
    plain(m.register_fn_named(
        "clampf",
        ["x", "lo", "hi"],
        |cx: &mut Ctx, x: f64, lo: f64, hi: f64| {
            if lo > hi || lo.is_nan() || hi.is_nan() {
                cx.raise(format!("clampf: the range {lo}..{hi} is empty"));
            }
            x.clamp(lo, hi)
        },
    ))?;

    // Integers.
    plain(m.register_fn_named("min", ["a", "b"], |a: i64, b: i64| a.min(b)))?;
    plain(m.register_fn_named("max", ["a", "b"], |a: i64, b: i64| a.max(b)))?;
    plain(m.register_fn_named(
        "clamp",
        ["x", "lo", "hi"],
        |cx: &mut Ctx, x: i64, lo: i64, hi: i64| {
            if lo > hi {
                cx.raise(format!("clamp: the range {lo}..{hi} is empty"));
            }
            x.clamp(lo, hi)
        },
    ))?;
    plain(m.register_fn_named("abs", ["x"], |cx: &mut Ctx, x: i64| {
        x.checked_abs()
            .unwrap_or_else(|| cx.raise("integer overflow"))
    }))?;
    plain(m.register_fn_named("sign", ["x"], |x: i64| x.signum()))?;
    plain(
        m.register_fn_named("floor_div", ["a", "b"], |cx: &mut Ctx, a: i64, b: i64| {
            if b == 0 {
                cx.raise("division by zero");
            }
            let Some(q) = a.checked_div(b) else {
                cx.raise("integer overflow");
            };
            // Rust truncates toward zero; Python floors, so step down when the signs differ and
            // there is a remainder.
            if a % b != 0 && ((a < 0) != (b < 0)) {
                q - 1
            } else {
                q
            }
        }),
    )?;
    plain(
        m.register_fn_named("floor_mod", ["a", "b"], |cx: &mut Ctx, a: i64, b: i64| {
            if b == 0 {
                cx.raise("modulo by zero");
            }
            let Some(r) = a.checked_rem(b) else {
                // `i64::MIN % -1` is zero in the mathematics; Rust calls it overflow.
                return 0;
            };
            if r != 0 && ((r < 0) != (b < 0)) {
                r + b
            } else {
                r
            }
        }),
    )?;
    plain(
        m.register_fn_named("gcd", ["a", "b"], |cx: &mut Ctx, a: i64, b: i64| {
            let (mut x, mut y) = (a.unsigned_abs(), b.unsigned_abs());
            while y != 0 {
                (x, y) = (y, x % y);
            }
            i64::try_from(x).unwrap_or_else(|_| cx.raise("integer overflow"))
        }),
    )?;
    Ok(())
}
