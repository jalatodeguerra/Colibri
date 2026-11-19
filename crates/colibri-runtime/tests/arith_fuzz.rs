//! Differential tests: random numeric expressions run through Colibri must agree with Rust's
//! native checked and wrapping arithmetic, on the result or on the first fault, for every
//! integer width, for shifts and bitwise operators, for the `wrapping_*` and `saturating_*`
//! methods, for conversions, and for `f32` arithmetic.

use colibri_runtime as colibri;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Runs `src`; `Ok` is what it printed, `Err` the panic message.
fn run(src: &str) -> Result<String, String> {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    match vm.load_source("fuzz.colibri", src) {
        Ok(()) => Ok(out.contents()),
        Err(colibri::Error::Panic { message, .. }) => Err(message),
        Err(other) => panic!("{src}: {other:?}"),
    }
}

/// An integer type, as Rust sees it and as Colibri names it.
trait Int: Copy + std::fmt::Display + std::fmt::Debug {
    /// The name in Colibri source and in messages (`int` for `i64`).
    const NAME: &'static str;
    const BITS: u32;
    const SIGNED: bool;
    /// Edge values and a few ordinary ones.
    fn pool() -> Vec<Self>;
    fn random(rng: &mut Rng) -> Self;
    fn add(self, o: Self) -> Option<Self>;
    fn sub(self, o: Self) -> Option<Self>;
    fn mul(self, o: Self) -> Option<Self>;
    fn div(self, o: Self) -> Option<Self>;
    fn rem(self, o: Self) -> Option<Self>;
    fn neg(self) -> Option<Self>;
    fn bitand(self, o: Self) -> Self;
    fn bitor(self, o: Self) -> Self;
    fn bitxor(self, o: Self) -> Self;
    fn not(self) -> Self;
    /// `self << n` and `self >> n` for `n < BITS`.
    fn shl(self, n: u32) -> Self;
    fn shr(self, n: u32) -> Self;
    fn wrapping(self, op: char, o: Self) -> Self;
    fn saturating(self, op: char, o: Self) -> Self;
    fn is_zero(self) -> bool;
    /// The source text of a literal of this type.
    fn source(self) -> String;
}

macro_rules! int_impl {
    ($t:ty, $name:expr, $signed:expr) => {
        impl Int for $t {
            const NAME: &'static str = $name;
            const BITS: u32 = <$t>::BITS;
            const SIGNED: bool = $signed;
            fn pool() -> Vec<Self> {
                let mut v = vec![0, 1, 2, 3, 7, 10, 100, <$t>::MAX, <$t>::MAX - 1, <$t>::MIN];
                v.push(<$t>::MIN + 1);
                v.push(<$t>::MAX / 2);
                v.push(<$t>::MAX / 2 + 1);
                if $signed {
                    v.extend([-1i64, -2, -3, -7, -10].map(|x| x as $t));
                }
                v
            }
            fn random(rng: &mut Rng) -> Self {
                let pool = Self::pool();
                if rng.below(3) == 0 {
                    rng.next() as $t
                } else {
                    pool[rng.below(pool.len() as u64) as usize]
                }
            }
            fn add(self, o: Self) -> Option<Self> {
                self.checked_add(o)
            }
            fn sub(self, o: Self) -> Option<Self> {
                self.checked_sub(o)
            }
            fn mul(self, o: Self) -> Option<Self> {
                self.checked_mul(o)
            }
            fn div(self, o: Self) -> Option<Self> {
                self.checked_div(o)
            }
            fn rem(self, o: Self) -> Option<Self> {
                self.checked_rem(o)
            }
            fn neg(self) -> Option<Self> {
                // `checked_neg` exists for unsigned types too (only 0 succeeds), but Colibri
                // refuses to negate an unsigned value at compile time, so it is never asked.
                self.checked_neg()
            }
            fn bitand(self, o: Self) -> Self {
                self & o
            }
            fn bitor(self, o: Self) -> Self {
                self | o
            }
            fn bitxor(self, o: Self) -> Self {
                self ^ o
            }
            fn not(self) -> Self {
                !self
            }
            fn shl(self, n: u32) -> Self {
                self.wrapping_shl(n)
            }
            fn shr(self, n: u32) -> Self {
                self >> n
            }
            fn wrapping(self, op: char, o: Self) -> Self {
                match op {
                    'a' => self.wrapping_add(o),
                    's' => self.wrapping_sub(o),
                    _ => self.wrapping_mul(o),
                }
            }
            fn saturating(self, op: char, o: Self) -> Self {
                match op {
                    'A' => self.saturating_add(o),
                    'S' => self.saturating_sub(o),
                    _ => self.saturating_mul(o),
                }
            }
            fn is_zero(self) -> bool {
                self == 0
            }
            fn source(self) -> String {
                format!("{}({})", $name, self)
            }
        }
    };
}

int_impl!(i8, "i8", true);
int_impl!(i16, "i16", true);
int_impl!(i32, "i32", true);
int_impl!(i64, "int", true);
int_impl!(u8, "u8", false);
int_impl!(u16, "u16", false);
int_impl!(u32, "u32", false);
int_impl!(u64, "u64", false);

/// An expression over one integer type.
enum Expr<T> {
    Lit(T),
    Neg(Box<Expr<T>>),
    Not(Box<Expr<T>>),
    /// `+ - * / % & | ^`, or a method: `a s m` wrapping, `A S M` saturating.
    Bin(char, Box<Expr<T>>, Box<Expr<T>>),
    /// `<` is `<<`, `>` is `>>`; the count is an `int` literal.
    Shift(char, Box<Expr<T>>, i64),
}

fn generate<T: Int>(rng: &mut Rng, depth: u32) -> Expr<T> {
    if depth == 0 || rng.below(4) == 0 {
        return Expr::Lit(T::random(rng));
    }
    match rng.below(12) {
        0 if T::SIGNED => Expr::Neg(Box::new(generate(rng, depth - 1))),
        1 => Expr::Not(Box::new(generate(rng, depth - 1))),
        2 | 3 => {
            let count = rng.below(T::BITS as u64 + 4) as i64 - 2;
            let op = if rng.below(2) == 0 { '<' } else { '>' };
            Expr::Shift(op, Box::new(generate(rng, depth - 1)), count)
        }
        _ => {
            let ops = [
                '+', '-', '*', '/', '%', '&', '|', '^', 'a', 's', 'm', 'A', 'S', 'M',
            ];
            let op = ops[rng.below(ops.len() as u64) as usize];
            Expr::Bin(
                op,
                Box::new(generate(rng, depth - 1)),
                Box::new(generate(rng, depth - 1)),
            )
        }
    }
}

fn source<T: Int>(e: &Expr<T>) -> String {
    match e {
        Expr::Lit(v) => v.source(),
        Expr::Neg(x) => format!("(-{})", source(x)),
        Expr::Not(x) => format!("(~{})", source(x)),
        Expr::Bin(op, a, b) => {
            let method = match op {
                'a' => Some("wrapping_add"),
                's' => Some("wrapping_sub"),
                'm' => Some("wrapping_mul"),
                'A' => Some("saturating_add"),
                'S' => Some("saturating_sub"),
                'M' => Some("saturating_mul"),
                _ => None,
            };
            match method {
                Some(m) => format!("{}.{m}({})", source(a), source(b)),
                None => format!("({} {op} {})", source(a), source(b)),
            }
        }
        Expr::Shift(op, a, n) => {
            let symbol = if *op == '<' { "<<" } else { ">>" };
            let count = if *n < 0 {
                format!("({n})")
            } else {
                n.to_string()
            };
            format!("({} {symbol} {count})", source(a))
        }
    }
}

fn eval<T: Int>(e: &Expr<T>) -> Result<T, String> {
    let overflow = || "integer overflow".to_string();
    match e {
        Expr::Lit(v) => Ok(*v),
        Expr::Neg(x) => eval(x)?.neg().ok_or_else(overflow),
        Expr::Not(x) => Ok(eval(x)?.not()),
        Expr::Bin(op, a, b) => {
            let (a, b) = (eval(a)?, eval(b)?);
            match op {
                '+' => a.add(b).ok_or_else(overflow),
                '-' => a.sub(b).ok_or_else(overflow),
                '*' => a.mul(b).ok_or_else(overflow),
                '/' if b.is_zero() => Err("division by zero".to_string()),
                '/' => a.div(b).ok_or_else(overflow),
                '%' if b.is_zero() => Err("modulo by zero".to_string()),
                '%' => a.rem(b).ok_or_else(overflow),
                '&' => Ok(a.bitand(b)),
                '|' => Ok(a.bitor(b)),
                '^' => Ok(a.bitxor(b)),
                'a' | 's' | 'm' => Ok(a.wrapping(*op, b)),
                _ => Ok(a.saturating(*op, b)),
            }
        }
        Expr::Shift(op, a, n) => {
            let a = eval(a)?;
            if *n < 0 || *n >= i64::from(T::BITS) {
                return Err(format!("shift by {n} is out of range for `{}`", T::NAME));
            }
            Ok(if *op == '<' {
                a.shl(*n as u32)
            } else {
                a.shr(*n as u32)
            })
        }
    }
}

/// Runs random expressions of type `T` and counts how many ended in a fault.
fn fuzz<T: Int>(seed: u64, rounds: usize) -> usize {
    let mut rng = Rng(seed);
    let mut faults = 0;
    for _ in 0..rounds {
        let expr = generate::<T>(&mut rng, 4);
        let text = source(&expr);
        match (eval(&expr), run(&format!("print({text})\n"))) {
            (Ok(n), Ok(printed)) => assert_eq!(printed, format!("{n}\n"), "{} {text}", T::NAME),
            (Err(want), Err(got)) => {
                faults += 1;
                assert_eq!(got, want, "{} {text}", T::NAME);
            }
            (want, got) => panic!(
                "{} {text}: Rust says {want:?}, Colibri says {got:?}",
                T::NAME
            ),
        }
    }
    faults
}

macro_rules! fuzz_type {
    ($fn_name:ident, $t:ty, $seed:expr) => {
        #[test]
        fn $fn_name() {
            let faults = fuzz::<$t>($seed, 600);
            assert!(
                faults > 40,
                "only {faults} faults: the generator is too tame"
            );
        }
    };
}

fuzz_type!(i8_expressions_agree_with_rust, i8, 0x2545_f491_4f6c_dd1d);
fuzz_type!(i16_expressions_agree_with_rust, i16, 0x1234_5678_9abc_def1);
fuzz_type!(i32_expressions_agree_with_rust, i32, 0x0f0f_1234_5555_aaa1);
fuzz_type!(int_expressions_agree_with_rust, i64, 0x9e37_79b9_7f4a_7c15);
fuzz_type!(u8_expressions_agree_with_rust, u8, 0x7777_1111_3333_5555);
fuzz_type!(u16_expressions_agree_with_rust, u16, 0x4242_4242_1357_9bdf);
fuzz_type!(u32_expressions_agree_with_rust, u32, 0x0dd5_1ced_cafe_f00d);
fuzz_type!(u64_expressions_agree_with_rust, u64, 0x5eed_5eed_1bad_b002);

/// `x << n`, `x >> n` and the bitwise operators on every width, with the count of any
/// integer type: exhaustive over the interesting counts.
fn shifts_of<T: Int>() {
    for v in T::pool() {
        for n in 0..=T::BITS + 1 {
            for (symbol, left) in [("<<", true), (">>", false)] {
                let src = format!("let x = {}\nlet n = {n}\nprint(x {symbol} n)\n", v.source());
                let want = if n >= T::BITS {
                    Err(format!("shift by {n} is out of range for `{}`", T::NAME))
                } else if left {
                    Ok(format!("{}\n", v.shl(n)))
                } else {
                    Ok(format!("{}\n", v.shr(n)))
                };
                assert_eq!(run(&src), want, "{} {src}", T::NAME);
            }
        }
    }
}

#[test]
fn every_shift_count_of_every_width_agrees_with_rust() {
    shifts_of::<i8>();
    shifts_of::<i16>();
    shifts_of::<i32>();
    shifts_of::<i64>();
    shifts_of::<u8>();
    shifts_of::<u16>();
    shifts_of::<u32>();
    shifts_of::<u64>();
}

#[test]
fn shift_counts_may_be_of_any_integer_type() {
    // A count of type `u64` above `i64::MAX` is just out of range, never a wrong answer.
    assert_eq!(
        run("let x: u8 = 1\nlet n: u64 = 18446744073709551615\nprint(x << n)\n"),
        Err("shift by 18446744073709551615 is out of range for `u8`".to_string())
    );
    assert_eq!(
        run("let x: u64 = 1\nlet n: i8 = 63\nprint(x << n)\n"),
        Ok("9223372036854775808\n".to_string())
    );
    assert_eq!(
        run("let x: i32 = 5\nlet n: i8 = -1\nprint(x >> n)\n"),
        Err("shift by -1 is out of range for `i32`".to_string())
    );
}

/// `T(v)` for an `int` value agrees with Rust's `TryFrom`.
fn conversions_of<T: Int + TryFrom<i64>>(rng: &mut Rng) {
    let mut values: Vec<i64> = vec![
        0,
        1,
        -1,
        127,
        128,
        -128,
        -129,
        255,
        256,
        32767,
        32768,
        65535,
        65536,
        i64::from(i32::MAX),
        i64::from(i32::MIN),
        i64::from(u32::MAX),
        i64::MAX,
        i64::MIN,
    ];
    values.extend((0..30).map(|_| rng.next() as i64 >> rng.below(60)));
    for v in values {
        let src = format!("let v: int = {}\nprint({}(v))\n", v.source_int(), T::NAME);
        let want = match T::try_from(v) {
            Ok(t) => Ok(format!("{t}\n")),
            Err(_) => Err(format!("value {v} does not fit in `{}`", T::NAME)),
        };
        assert_eq!(run(&src), want, "{src}");
    }
}

trait SourceInt {
    fn source_int(self) -> String;
}

impl SourceInt for i64 {
    /// `i64::MIN` has no positive literal, so it is written as an expression.
    fn source_int(self) -> String {
        if self == i64::MIN {
            "(-9223372036854775807 - 1)".to_string()
        } else {
            self.to_string()
        }
    }
}

#[test]
fn integer_conversions_agree_with_try_from() {
    let mut rng = Rng(0x2468_ace0_1357_9bdf);
    conversions_of::<i8>(&mut rng);
    conversions_of::<i16>(&mut rng);
    conversions_of::<i32>(&mut rng);
    conversions_of::<i64>(&mut rng);
    conversions_of::<u8>(&mut rng);
    conversions_of::<u16>(&mut rng);
    conversions_of::<u32>(&mut rng);
    conversions_of::<u64>(&mut rng);
}

/// `T(x)` for a `float` agrees with a truncating, range-checked Rust conversion.
fn float_conversions_of<T: Int>(want: impl Fn(f64) -> Option<T>) {
    let finite = [
        0.0,
        0.5,
        -0.5,
        1.9,
        -1.9,
        127.9,
        128.0,
        255.5,
        256.0,
        -128.5,
        -129.0,
        32767.9,
        65535.99,
        65536.0,
        2147483647.9,
        2147483648.0,
        4294967295.5,
        4294967296.0,
        9.2e18,
        9.3e18,
        1.8446744073709552e19,
        1.0e300,
        -1.0e300,
    ];
    for x in finite {
        let src = format!("let x: float = {x:?}\nprint({}(x))\n", T::NAME);
        match (want(x), run(&src)) {
            (Some(t), Ok(printed)) => assert_eq!(printed, format!("{t}\n"), "{src}"),
            (None, Err(message)) => {
                assert!(
                    message.contains(&format!("does not fit in `{}`", T::NAME)),
                    "{src}: {message}"
                );
            }
            (want, got) => panic!("{src}: Rust says {want:?}, Colibri says {got:?}"),
        }
    }
    for (name, expr) in [
        ("nan", "zero / zero"),
        ("inf", "1.0 / zero"),
        ("-inf", "-1.0 / zero"),
    ] {
        let src = format!("let zero: float = 0.0\nprint({}({expr}))\n", T::NAME);
        let message = run(&src).expect_err("a fault");
        assert!(message.contains(name), "{src}: {message}");
    }
}

/// A truncating float to integer conversion that fails outside the type, as Rust's `as`
/// would saturate but Colibri traps.
fn truncated<T: Int + TryFrom<i128>>(x: f64) -> Option<T> {
    if x.is_nan() {
        return None;
    }
    let t = x.trunc();
    // Exact: `i128` holds every float below 2^127, and the bound check is done there.
    if t.abs() >= 1.0e30 {
        return None;
    }
    T::try_from(t as i128).ok()
}

#[test]
fn float_to_integer_conversions_truncate_and_trap() {
    float_conversions_of::<i8>(truncated::<i8>);
    float_conversions_of::<i16>(truncated::<i16>);
    float_conversions_of::<i32>(truncated::<i32>);
    float_conversions_of::<i64>(truncated::<i64>);
    float_conversions_of::<u8>(truncated::<u8>);
    float_conversions_of::<u16>(truncated::<u16>);
    float_conversions_of::<u32>(truncated::<u32>);
    float_conversions_of::<u64>(truncated::<u64>);
}

/// `f32` expressions: every operation rounds to single precision.
enum FExpr {
    Lit(f32),
    Bin(char, Box<FExpr>, Box<FExpr>),
}

fn f32_generate(rng: &mut Rng, depth: u32) -> FExpr {
    if depth == 0 || rng.below(4) == 0 {
        let pool = [
            0.0f32,
            0.1,
            0.2,
            0.5,
            1.0,
            3.0,
            1.0 / 3.0,
            16_777_216.0,
            1.0e10,
            1.0e-10,
            3.4028235e38,
            1.0e-45,
            -2.5,
            -0.1,
            123_456.79,
        ];
        let v = pool[rng.below(pool.len() as u64) as usize];
        return FExpr::Lit(if rng.below(5) == 0 { -v } else { v });
    }
    let op = ['+', '-', '*', '/'][rng.below(4) as usize];
    FExpr::Bin(
        op,
        Box::new(f32_generate(rng, depth - 1)),
        Box::new(f32_generate(rng, depth - 1)),
    )
}

fn f32_source(e: &FExpr) -> String {
    match e {
        // The value as an exact `f64`, so the only rounding is `f32(..)` itself.
        FExpr::Lit(v) => format!("f32({:?})", f64::from(*v)),
        FExpr::Bin(op, a, b) => format!("({} {op} {})", f32_source(a), f32_source(b)),
    }
}

fn f32_eval(e: &FExpr) -> f32 {
    match e {
        FExpr::Lit(v) => *v,
        FExpr::Bin(op, a, b) => {
            let (a, b) = (f32_eval(a), f32_eval(b));
            match op {
                '+' => a + b,
                '-' => a - b,
                '*' => a * b,
                _ => a / b,
            }
        }
    }
}

fn f32_show(x: f32) -> String {
    if x.is_nan() {
        "nan".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "inf" } else { "-inf" }.to_string()
    } else {
        format!("{x:?}")
    }
}

#[test]
fn f32_arithmetic_agrees_with_rust_f32() {
    let mut rng = Rng(0x3141_5926_5358_9793);
    for _ in 0..800 {
        let expr = f32_generate(&mut rng, 4);
        let text = f32_source(&expr);
        let got = run(&format!("print({text})\n"));
        assert_eq!(
            got,
            Ok(format!("{}\n", f32_show(f32_eval(&expr)))),
            "{text}"
        );
    }
}

#[test]
fn f32_to_float_widening_is_exact_and_integers_round_to_nearest() {
    let mut rng = Rng(0x1010_2020_3030_4040);
    for _ in 0..200 {
        let x = f32::from_bits(rng.next() as u32);
        if !x.is_finite() {
            continue;
        }
        let src = format!(
            "let x = f32({:?})\nlet y: float = x\nprint(y)\n",
            f64::from(x)
        );
        let want = format!("{:?}\n", f64::from(x));
        assert_eq!(run(&src), Ok(want), "{src}");
    }
    let mut rng = Rng(0x5555_6666_7777_8888);
    for _ in 0..200 {
        let v = rng.next() as i64 >> rng.below(40);
        let src = format!("let v: int = {v}\nprint(f32(v))\nprint(float(v))\n");
        let want = format!("{}\n{:?}\n", f32_show(v as f32), v as f64);
        assert_eq!(run(&src), Ok(want), "{src}");
    }
}
