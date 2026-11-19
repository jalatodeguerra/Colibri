//! The numeric tower in the checker's output: operators carry their type, literals are
//! resolved and checked, and mistakes come with the hint DESIGN §5.1 promises.

use colibri_check::hir::*;
use colibri_check::types::{IntTy, Ty};
use colibri_syntax::{FileId, parse_file};

fn check(src: &str) -> (Option<Program>, Vec<(String, Option<String>)>) {
    let (ast, parse_diags) = parse_file(FileId(0), src);
    assert!(parse_diags.is_empty(), "{parse_diags:?}");
    let checked = colibri_check::check(&ast);
    let diags = checked
        .diagnostics
        .iter()
        .map(|d| (d.message.clone(), d.hint.clone()))
        .collect();
    (checked.program, diags)
}

fn program(src: &str) -> Program {
    let (program, diags) = check(src);
    program.unwrap_or_else(|| panic!("expected no errors, got {diags:?}"))
}

/// The first error's message and hint.
fn first_error(src: &str) -> (String, String) {
    let (program, diags) = check(src);
    assert!(program.is_none(), "expected an error");
    let (message, hint) = diags.into_iter().next().expect("a diagnostic");
    (message, hint.unwrap_or_default())
}

fn body_of(p: &Program, name: &str) -> std::rc::Rc<Body> {
    let id = p.fn_by_name(name).expect("function exists");
    std::rc::Rc::clone(&p.fns[id.0 as usize].body)
}

/// The tail expression of a function with an `=>` body.
fn tail(body: &Body) -> &Expr {
    body.block.tail.as_deref().expect("an expression body")
}

#[test]
fn arithmetic_names_its_width() {
    let p = program(
        "fn a(x: u8, y: u8) -> u8 => x + y\nfn b(x: i32) -> i32 => x * 2\nfn c(x: u64) -> u64 => x / 3\nfn d(x: f32) -> f32 => x - 1\n",
    );
    let op = |name: &str| match tail(&body_of(&p, name)).kind {
        ExprKind::Binary(op, ..) => op,
        ref other => panic!("{other:?}"),
    };
    assert_eq!(op("a"), BinOp::Arith(ArithOp::Add, Num::Int(IntTy::U8)));
    assert_eq!(op("b"), BinOp::Arith(ArithOp::Mul, Num::Int(IntTy::I32)));
    assert_eq!(op("c"), BinOp::Arith(ArithOp::Div, Num::Int(IntTy::U64)));
    assert_eq!(op("d"), BinOp::Arith(ArithOp::Sub, Num::F32));
}

#[test]
fn bitwise_operators_name_the_type_of_the_left_operand() {
    let p = program(
        "fn a(x: u8, y: u8) -> u8 => x & y\nfn b(x: i16, n: u8) -> i16 => x << n\nfn c(x: u64) -> u64 => ~x\nfn d(x: i32) -> bool => x & 1 == 0\n",
    );
    assert!(matches!(
        tail(&body_of(&p, "a")).kind,
        ExprKind::Binary(BinOp::Bit(BitOp::And, IntTy::U8), ..)
    ));
    assert!(matches!(
        tail(&body_of(&p, "b")).kind,
        ExprKind::Binary(BinOp::Bit(BitOp::Shl, IntTy::I16), ..)
    ));
    assert!(matches!(
        tail(&body_of(&p, "c")).kind,
        ExprKind::Unary(UnOp::BitNot(IntTy::U64), _)
    ));
    // `&` binds tighter than `==`.
    let body = body_of(&p, "d");
    let ExprKind::Binary(BinOp::Cmp(CmpOp::Eq, Operand::Int(IntTy::I32)), lhs, _) =
        &tail(&body).kind
    else {
        panic!("expected a comparison");
    };
    assert!(matches!(
        lhs.kind,
        ExprKind::Binary(BinOp::Bit(BitOp::And, IntTy::I32), ..)
    ));
}

#[test]
fn literals_take_the_type_they_are_used_as() {
    let p = program(
        "fn a() -> u8 => 255\nfn b() -> f32 => 3\nfn c() -> u64 => 18446744073709551615\nfn d() -> i8 => -128\nfn e() -> f32 => 0.5\n",
    );
    assert!(matches!(
        tail(&body_of(&p, "a")).kind,
        ExprKind::Lit(Lit::Int(255))
    ));
    assert!(matches!(tail(&body_of(&p, "b")).kind, ExprKind::Lit(Lit::F32(x)) if x == 3.0));
    assert!(matches!(
        tail(&body_of(&p, "c")).kind,
        ExprKind::Lit(Lit::U64(u64::MAX))
    ));
    assert!(matches!(
        tail(&body_of(&p, "d")).kind,
        ExprKind::Lit(Lit::Int(-128))
    ));
    assert!(matches!(tail(&body_of(&p, "e")).kind, ExprKind::Lit(Lit::F32(x)) if x == 0.5));
    assert_eq!(tail(&body_of(&p, "b")).ty, Ty::F32);
}

#[test]
fn f32_widens_through_an_explicit_coercion() {
    let p = program("fn a(x: f32) -> float => x\nfn b(x: f32, y: float) -> float => x * y\n");
    assert!(matches!(
        tail(&body_of(&p, "a")).kind,
        ExprKind::Coerce(_, Coercion::F32ToFloat)
    ));
    let body = body_of(&p, "b");
    let ExprKind::Binary(BinOp::Arith(ArithOp::Mul, Num::Float), l, _) = &tail(&body).kind else {
        panic!("expected a float multiplication");
    };
    assert!(matches!(l.kind, ExprKind::Coerce(_, Coercion::F32ToFloat)));
}

#[test]
fn f32_arithmetic_is_not_widened_before_it_runs() {
    // `a * b` is an `f32` product that widens afterwards, not a `float` product.
    let p = program("fn f(a: f32, b: f32) -> float => a * b\n");
    let body = body_of(&p, "f");
    let ExprKind::Coerce(inner, Coercion::F32ToFloat) = &tail(&body).kind else {
        panic!("expected a widening of the product");
    };
    assert!(matches!(
        inner.kind,
        ExprKind::Binary(BinOp::Arith(ArithOp::Mul, Num::F32), ..)
    ));
}

#[test]
fn conversion_calls_name_both_types() {
    let p = program("fn f(x: i32) -> u8 => u8(x)\nfn g(x: f32) -> int => int(x)\n");
    let body = body_of(&p, "f");
    let ExprKind::Call(call) = &tail(&body).kind else {
        panic!("expected a call");
    };
    assert_eq!(
        call.callee,
        Callee::Intrinsic(Intrinsic::Convert {
            from: Num::Int(IntTy::I32),
            to: Num::Int(IntTy::U8)
        })
    );
    let body = body_of(&p, "g");
    let ExprKind::Call(call) = &tail(&body).kind else {
        panic!("expected a call");
    };
    assert_eq!(
        call.callee,
        Callee::Intrinsic(Intrinsic::Convert {
            from: Num::F32,
            to: Num::INT
        })
    );
}

#[test]
fn constants_are_literals_of_their_type() {
    let p = program("fn f() -> u8 => u8.MAX\nfn g() -> i64 => int.MIN\nfn h() -> u64 => u64.MAX\n");
    assert!(matches!(
        tail(&body_of(&p, "f")).kind,
        ExprKind::Lit(Lit::Int(255))
    ));
    assert!(matches!(
        tail(&body_of(&p, "g")).kind,
        ExprKind::Lit(Lit::Int(i64::MIN))
    ));
    assert!(matches!(
        tail(&body_of(&p, "h")).kind,
        ExprKind::Lit(Lit::U64(u64::MAX))
    ));
}

#[test]
fn bool_operands_of_bitwise_operators_point_to_the_logical_ones() {
    let (message, hint) = first_error("fn f(a: bool, b: bool) -> bool => a & b\n");
    assert_eq!(message, "`&` cannot be applied to a value of type `bool`");
    assert!(hint.contains("`&&`"), "{hint}");
    let (_, hint) = first_error("fn f(a: bool, b: bool) -> bool => a | b\n");
    assert!(hint.contains("`||`"), "{hint}");
    let (_, hint) = first_error("fn f(a: bool, b: bool) -> bool => a ^ b\n");
    assert!(hint.contains("`!=`"), "{hint}");
    let (_, hint) = first_error("fn f(a: bool) -> bool => ~a\n");
    assert!(hint.contains("`!`"), "{hint}");
}

#[test]
fn float_does_not_narrow_to_f32_and_the_hint_names_the_conversion() {
    let (message, hint) = first_error("fn f(x: float) -> f32 => x\n");
    assert_eq!(message, "expected `f32`, found `float`");
    assert!(hint.contains("f32(x)"), "{hint}");
    let (message, hint) = first_error("fn f(x: int) -> u8 => x\n");
    assert_eq!(message, "expected `u8`, found `int`");
    assert!(hint.contains("u8(x)"), "{hint}");
}

#[test]
fn mixing_number_types_in_an_operator_explains_the_rule() {
    let (message, hint) = first_error("fn f(a: u8, b: i32) -> bool => a == b\n");
    assert_eq!(message, "cannot apply `==` to `u8` and `i32`");
    assert!(hint.contains("only `f32` widens to `float`"), "{hint}");
}

#[test]
fn literal_errors_say_what_the_type_holds() {
    let (message, hint) = first_error("fn f() -> u8 => 256\n");
    assert_eq!(message, "literal 256 does not fit in `u8`");
    assert_eq!(hint, "`u8` holds 0 to 255");
    let (message, _) = first_error("fn f() -> i8 => -129\n");
    assert_eq!(message, "literal -129 does not fit in `i8`");
    let (message, _) = first_error("fn f() -> int => 18446744073709551615\n");
    assert_eq!(
        message,
        "literal 18446744073709551615 does not fit in `int`"
    );
}

#[test]
fn as_numeric_conversions_point_to_the_call_form() {
    let (message, hint) = first_error("fn f(x: int) -> u8 => x as u8\n");
    assert_eq!(message, "cannot convert to `u8` with `as`");
    assert!(hint.contains("u8(x)"), "{hint}");
}

#[test]
fn i64_is_int_and_f64_is_float() {
    let p = program("fn f(a: i64, b: f64) -> (int, float) => (a, b)\n");
    let id = p.fn_by_name("f").unwrap();
    let sig = &p.fns[id.0 as usize].sig;
    assert_eq!(sig.params, [Ty::INT, Ty::Float]);
}
