//! Differential test: random integer expressions run through Colibri must agree with Rust's
//! checked arithmetic on the result or on the first fault.

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

enum Expr {
    Lit(i64),
    Neg(Box<Expr>),
    Bin(char, Box<Expr>, Box<Expr>),
}

const LITERALS: &[i64] = &[
    0,
    1,
    -1,
    2,
    3,
    -3,
    7,
    -7,
    10,
    100,
    4_294_967_296,
    i64::MAX,
    i64::MIN,
    i64::MAX - 1,
    i64::MIN + 1,
];

fn generate(rng: &mut Rng, depth: u32) -> Expr {
    if depth == 0 || rng.below(4) == 0 {
        return Expr::Lit(LITERALS[rng.below(LITERALS.len() as u64) as usize]);
    }
    if rng.below(8) == 0 {
        return Expr::Neg(Box::new(generate(rng, depth - 1)));
    }
    let op = ['+', '-', '*', '/', '%'][rng.below(5) as usize];
    Expr::Bin(
        op,
        Box::new(generate(rng, depth - 1)),
        Box::new(generate(rng, depth - 1)),
    )
}

fn source(e: &Expr) -> String {
    match e {
        Expr::Lit(i) if *i < 0 => format!("({i})"),
        Expr::Lit(i) => i.to_string(),
        Expr::Neg(x) => format!("(-{})", source(x)),
        Expr::Bin(op, a, b) => format!("({} {op} {})", source(a), source(b)),
    }
}

fn eval(e: &Expr) -> Result<i64, &'static str> {
    match e {
        Expr::Lit(i) => Ok(*i),
        Expr::Neg(x) => eval(x)?.checked_neg().ok_or("integer overflow"),
        Expr::Bin(op, a, b) => {
            let (a, b) = (eval(a)?, eval(b)?);
            match op {
                '+' => a.checked_add(b).ok_or("integer overflow"),
                '-' => a.checked_sub(b).ok_or("integer overflow"),
                '*' => a.checked_mul(b).ok_or("integer overflow"),
                '/' if b == 0 => Err("division by zero"),
                '/' => a.checked_div(b).ok_or("integer overflow"),
                '%' if b == 0 => Err("modulo by zero"),
                _ => a.checked_rem(b).ok_or("integer overflow"),
            }
        }
    }
}

#[test]
fn integer_expressions_agree_with_checked_rust_arithmetic() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut faults = 0;
    for _ in 0..1500 {
        let expr = generate(&mut rng, 4);
        let text = source(&expr);
        let out = colibri::SharedBuffer::new();
        let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
        let result = vm.load_source("fuzz.colibri", &format!("print({text})\n"));
        match (eval(&expr), result) {
            (Ok(n), Ok(())) => assert_eq!(out.contents(), format!("{n}\n"), "{text}"),
            (Err(want), Err(colibri::Error::Panic { message, .. })) => {
                faults += 1;
                assert_eq!(message, want, "{text}");
            }
            (want, got) => panic!("{text}: Rust says {want:?}, Colibri says {got:?}"),
        }
    }
    assert!(
        faults > 100,
        "only {faults} faults: the generator is too tame"
    );
}
