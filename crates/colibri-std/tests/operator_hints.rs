//! The messages and hints a program gets when it uses operators and the vec library the wrong way.
//! Hints cannot be asserted in goldens, so they are asserted here, on the diagnostics themselves.

use colibri_runtime::{self as colibri, CheckConfig};

/// The first error of `src` as `(message, hint)`.
fn first_error(src: &str) -> (String, String) {
    let config = CheckConfig {
        sources: colibri_std::sources(),
        interfaces: colibri_std::interfaces(),
        ..CheckConfig::default()
    };
    let report = colibri::check_source_with("main.colibri", src, &config);
    let d = report
        .diagnostics
        .iter()
        .find(|d| d.severity == colibri_syntax::Severity::Error)
        .unwrap_or_else(|| panic!("no error for:\n{src}"));
    (d.message.clone(), d.hint.clone().unwrap_or_default())
}

const VEC3: &str = "import vec\nlet v = vec.vec3(1.0, 2.0, 3.0)\n";

fn with_vec3(rest: &str) -> (String, String) {
    first_error(&format!("{VEC3}{rest}"))
}

#[test]
fn a_swizzle_field_that_is_a_method_says_to_call_it() {
    let (message, hint) = with_vec3("print(v.xy)\n");
    assert_eq!(message, "the struct `vec3` has no field `xy`");
    assert_eq!(hint, "`xy` is a method, not a field: call it, `value.xy()`");
}

#[test]
fn another_swizzle_says_to_use_the_constructor() {
    let (message, hint) = with_vec3("print(v.zyx)\n");
    assert_eq!(message, "the struct `vec3` has no field `zyx`");
    assert!(hint.contains("there are no swizzles"), "{hint}");
    assert!(hint.contains("`vec.vec3(v.z, v.y, v.x)`"), "{hint}");
}

#[test]
fn the_swizzle_example_fits_the_type_it_is_about() {
    for (make, field, example) in [
        ("vec.vec2(1.0, 2.0)", "zyx", "`vec.vec2(v.y, v.x)`"),
        ("vec.ivec3(1, 2, 3)", "zyx", "`vec.ivec3(v.z, v.y, v.x)`"),
        (
            "vec.vec4(1.0, 2.0, 3.0, 4.0)",
            "zyx",
            "`vec.vec4(v.z, v.y, v.x, v.w)`",
        ),
        ("vec.ivec2(1, 2)", "yx", "`vec.ivec2(v.y, v.x)`"),
    ] {
        let (_, hint) = first_error(&format!("import vec\nlet v = {make}\nprint(v.{field})\n"));
        assert!(hint.contains(example), "{make}.{field}: {hint}");
    }
}

#[test]
fn a_splat_constructor_says_to_write_every_component() {
    let (message, hint) = with_vec3("let w = vec.vec3(7.0)\n");
    assert_eq!(
        message,
        "missing arguments for `y`, `z` in the call to `std.vec.vec3`"
    );
    assert!(hint.starts_with("there is no splat constructor"), "{hint}");
}

#[test]
fn a_conversion_constructor_says_to_use_to_ivec() {
    let (message, hint) = with_vec3("let w = vec.ivec3(v)\n");
    assert_eq!(message, "`ivec3` takes its components, not a `vec3`");
    assert!(hint.contains("`v.to_ivec()`"), "{hint}");
}

#[test]
fn a_number_on_the_left_says_which_number_type_works() {
    let (message, hint) = with_vec3("let w = 2.0 * vec.ivec3(1, 2, 3)\n");
    assert_eq!(message, "cannot apply `*` to `{float}` and `ivec3`");
    assert!(hint.contains("must be of type `i32` here"), "{hint}");
    assert!(hint.contains("or write the `ivec3` first"), "{hint}");
}

#[test]
fn rem_on_a_struct_says_it_is_for_numbers() {
    let (message, hint) = with_vec3("let w = v % v\n");
    assert_eq!(message, "`%` cannot be applied to values of type `vec3`");
    assert!(
        hint.starts_with("`%` is defined for numbers only"),
        "{hint}"
    );
}

#[test]
fn a_bare_vec_name_says_to_import_the_module() {
    let (message, hint) = first_error("let v = vec2(1.0, 2.0)\n");
    assert_eq!(message, "unknown name `vec2`");
    assert!(
        hint.contains("write `import vec` and then `vec.vec2(..)`"),
        "{hint}"
    );
}

#[test]
fn an_impl_the_orphan_rule_refuses_is_not_suggested() {
    let (message, hint) =
        first_error("struct M {}\nimpl Add[M, M] for str {\n    fn add(self, o: M) -> M => o\n}\n");
    assert_eq!(
        message,
        "cannot implement the built-in `Add` for the built-in type `str`"
    );
    assert!(hint.contains("when its right-hand type is yours"), "{hint}");
    let (message, _) = first_error(
        "struct M {}\nimpl Add[int, M] for int {\n    fn add(self, o: int) -> M => M {}\n}\n",
    );
    assert_eq!(
        message,
        "cannot implement the built-in `Add` for the built-in type `int`"
    );
}

#[test]
fn a_missing_bound_on_a_number_does_not_suggest_a_refused_impl() {
    let (message, hint) =
        first_error("fn f[T: Add](a: T, b: T) -> T => a + b\nlet x = f(true, false)\n");
    assert_eq!(message, "`bool` does not implement `Add[bool, bool]`");
    assert!(!hint.contains("impl Add"), "{hint}");
}

#[test]
fn a_duplicate_impl_says_right_hand_types_are_told_apart_by_head() {
    let (message, hint) = first_error(
        "struct M {}\nimpl Add[M] for M {\n    fn add(self, o: M) -> M => o\n}\nimpl Add[M] for M {\n    fn add(self, o: M) -> M => o\n}\n",
    );
    assert_eq!(message, "`Add` is implemented for `M` more than once");
    assert!(hint.contains("told apart by their head only"), "{hint}");
    assert!(
        hint.contains("`Add[Box[int]]` and `Add[Box[str]]`"),
        "{hint}"
    );
}

#[test]
fn a_missing_impl_for_a_user_type_is_suggested() {
    let (message, hint) =
        first_error("fn f[T: Add](a: T, b: T) -> T => a + b\nstruct S {}\nlet x = f(S {}, S {})\n");
    assert_eq!(message, "`S` does not implement `Add[S, S]`");
    assert_eq!(hint, "add `impl Add[S, S] for S { ... }`");
}

#[test]
fn a_narrow_integer_inside_a_generic_impl_gets_the_cannot_be_used_message() {
    let (message, hint) = first_error(
        "struct W[T] { v: T }\nimpl[T: Add] Add for W[T] {\n    fn add(self, o: W[T]) -> W[T] => W { v: self.v + o.v }\n}\nfn twice[T: Add](a: T) -> T => a + a\nlet w = twice(W { v: i32(2) })\n",
    );
    assert_eq!(
        message,
        "`i32`, the type argument of `W[i32]`, cannot be used for `T: Add` yet"
    );
    assert!(hint.contains("the narrow type here is `i32`"), "{hint}");
    assert!(!hint.contains("impl Add"), "{hint}");
}

#[test]
fn a_bound_naming_i32_with_an_int_left_operand_is_a_plain_missing_impl() {
    let (message, hint) = first_error(
        "fn named[T: Mul[i32, T]](a: T, k: i32) -> T => a * k\nlet x = named(int(2), i32(3))\n",
    );
    assert_eq!(message, "`int` does not implement `Mul[i32, int]`");
    assert!(
        hint.contains("`int` and `i32` are different types"),
        "{hint}"
    );
    assert!(!hint.contains("use a type of your own"), "{hint}");
}

#[test]
fn a_missing_impl_is_not_blamed_on_a_narrow_integer() {
    // `Money` has `Mul[int]` but no `Mul[i32]`: the impl is missing, nothing is narrow.
    let (message, hint) = first_error(
        "struct Money { c: int }\nimpl Mul[int] for Money {\n    fn mul(self, k: int) -> Money => Money { c: self.c * k }\n}\nfn named[T: Mul[i32, T]](a: T, k: i32) -> T => a * k\nlet x = named(Money { c: 2 }, i32(3))\n",
    );
    assert_eq!(message, "`Money` does not implement `Mul[i32, Money]`");
    assert_eq!(hint, "add `impl Mul[i32, Money] for Money { ... }`");
    // `W[T]` has no `Add` at all.
    let (message, _) = first_error(
        "struct W[T] { v: T }\nfn twice[T: Add](a: T) -> T => a + a\nlet x = twice(W { v: i32(2) })\n",
    );
    assert_eq!(message, "`W[i32]` does not implement `Add[W[i32], W[i32]]`");
}
