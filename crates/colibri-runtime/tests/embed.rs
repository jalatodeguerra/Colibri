//! A Rust host embedding Colibri: load a script, call its functions with typed arguments,
//! recover from panics and capture output.

use colibri_runtime as colibri;

const SCRIPT: &str = r#"
var calls = 0

fn add(a: int, b: int) -> int => a + b

fn scale(x: float, by: float = 2) -> float => x * by

fn greet(name: str) -> str => "Hello, {name}!"

fn is_big(n: int) -> bool => n > 100

fn shout(msg: str) {
    print(msg + "!")
}

fn count() -> int {
    calls += 1
    return calls
}

fn divide(a: int, b: int) -> int => a / b

fn initial(s: str) -> char => 'x'

fn main() {
    print("main ran")
}

print("loaded")
"#;

fn vm_with_output() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    (vm, out)
}

fn loaded() -> (colibri::Vm, colibri::SharedBuffer) {
    let (mut vm, out) = vm_with_output();
    vm.load_source("host.colibri", SCRIPT)
        .expect("script loads");
    (vm, out)
}

#[test]
fn load_runs_top_level_statements_and_main_runs_on_request() {
    let (mut vm, out) = loaded();
    assert_eq!(out.contents(), "loaded\n");
    vm.run_main().expect("main runs");
    assert_eq!(out.contents(), "loaded\nmain ran\n");
}

#[test]
fn calls_with_typed_arguments_and_results() {
    let (mut vm, _) = loaded();
    let add = vm.get_fn::<(i64, i64), i64>("add").unwrap();
    assert_eq!(add.call(&mut vm, (1, 2)).unwrap(), 3);
    assert_eq!(add.call(&mut vm, (-5, 2)).unwrap(), -3);

    let scale = vm.get_fn::<(f64, f64), f64>("scale").unwrap();
    assert_eq!(scale.call(&mut vm, (1.5, 4.0)).unwrap(), 6.0);

    let greet = vm.get_fn::<(&str,), String>("greet").unwrap();
    assert_eq!(greet.call(&mut vm, ("Ana",)).unwrap(), "Hello, Ana!");
    let greet_owned = vm.get_fn::<(String,), String>("greet").unwrap();
    assert_eq!(
        greet_owned.call(&mut vm, ("Leo".to_string(),)).unwrap(),
        "Hello, Leo!"
    );

    let is_big = vm.get_fn::<(i64,), bool>("is_big").unwrap();
    assert!(is_big.call(&mut vm, (500,)).unwrap());
    assert!(!is_big.call(&mut vm, (5,)).unwrap());

    let initial = vm.get_fn::<(&str,), char>("initial").unwrap();
    assert_eq!(initial.call(&mut vm, ("s",)).unwrap(), 'x');
}

#[test]
fn unit_results_and_output() {
    let (mut vm, out) = loaded();
    let shout = vm.get_fn::<(&str,), ()>("shout").unwrap();
    shout.call(&mut vm, ("hey",)).unwrap();
    assert!(out.contents().ends_with("hey!\n"));
}

#[test]
fn handles_are_copy_and_do_not_borrow_the_vm() {
    let (mut vm, _) = loaded();
    let count = vm.get_fn::<(), i64>("count").unwrap();
    let copy = count; // `Fn` is `Copy`
    assert_eq!(count.call(&mut vm, ()).unwrap(), 1);
    assert_eq!(copy.call(&mut vm, ()).unwrap(), 2);
    // Module-level state lives in the Vm between calls.
    assert_eq!(count.call(&mut vm, ()).unwrap(), 3);
}

#[test]
fn a_rust_signature_that_does_not_match_is_an_error() {
    let (vm, _) = loaded();
    let wrong_arg = vm.get_fn::<(&str,), i64>("add").unwrap_err();
    match wrong_arg {
        colibri::Error::SignatureMismatch {
            name,
            colibri,
            rust,
        } => {
            assert_eq!(name, "add");
            assert_eq!(colibri, "fn(int, int) -> int");
            assert_eq!(rust, "fn(str) -> int");
        }
        other => panic!("unexpected error: {other:?}"),
    }
    let wrong_ret = vm.get_fn::<(i64, i64), f64>("add").unwrap_err();
    assert!(matches!(
        wrong_ret,
        colibri::Error::SignatureMismatch { .. }
    ));
    assert!(wrong_ret.to_string().contains("`fn(int, int) -> int`"));
    assert!(wrong_ret.to_string().contains("`fn(int, int) -> float`"));
    // A unit-returning function is `()`, not `i64`.
    assert!(vm.get_fn::<(&str,), i64>("shout").is_err());
    assert!(vm.get_fn::<(&str,), ()>("shout").is_ok());
}

#[test]
fn an_unknown_function_is_not_found() {
    let (vm, _) = loaded();
    match vm.get_fn::<(), ()>("nope").unwrap_err() {
        colibri::Error::FnNotFound { name } => assert_eq!(name, "nope"),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn a_panic_is_an_error_and_the_vm_stays_usable() {
    let (mut vm, _) = loaded();
    let divide = vm.get_fn::<(i64, i64), i64>("divide").unwrap();
    let count = vm.get_fn::<(), i64>("count").unwrap();
    assert_eq!(count.call(&mut vm, ()).unwrap(), 1);

    let err = divide.call(&mut vm, (1, 0)).unwrap_err();
    match &err {
        colibri::Error::Panic {
            message,
            location,
            stack,
            ..
        } => {
            assert_eq!(message, "division by zero");
            let at = location.as_ref().expect("a location");
            assert_eq!((at.file.as_str(), at.line), ("host.colibri", 21));
            assert_eq!(stack.len(), 1);
            assert_eq!(stack[0].function, "divide");
        }
        other => panic!("unexpected error: {other:?}"),
    }
    let text = err.render(false);
    assert!(text.starts_with("panic: division by zero\n"), "{text}");
    assert!(text.contains("at divide (host.colibri:21:"), "{text}");

    // The next call works, and module state is intact.
    assert_eq!(divide.call(&mut vm, (6, 3)).unwrap(), 2);
    assert_eq!(count.call(&mut vm, ()).unwrap(), 2);
    let add = vm.get_fn::<(i64, i64), i64>("add").unwrap();
    assert_eq!(add.call(&mut vm, (40, 2)).unwrap(), 42);
}

#[test]
fn overflow_is_a_panic_to_the_host_too() {
    let (mut vm, _) = loaded();
    let add = vm.get_fn::<(i64, i64), i64>("add").unwrap();
    let err = add.call(&mut vm, (i64::MAX, 1)).unwrap_err();
    assert!(matches!(&err, colibri::Error::Panic { message, .. } if message == "integer overflow"));
    assert_eq!(add.call(&mut vm, (1, 1)).unwrap(), 2);
}

#[test]
fn compile_errors_carry_diagnostics_and_render() {
    let (mut vm, _) = vm_with_output();
    let err = vm
        .load_source("bad.colibri", "let x: int = \"s\"\nprint(y)\n")
        .unwrap_err();
    let colibri::Error::Compile(report) = &err else {
        panic!("expected compile errors, got {err:?}");
    };
    assert!(report.has_errors());
    assert_eq!(report.diagnostics.len(), 2);
    let plain = err.render(false);
    assert!(plain.contains("expected `int`, found `str`"), "{plain}");
    assert!(plain.contains("unknown name `y`"), "{plain}");
    assert!(plain.contains("bad.colibri"), "{plain}");
    assert!(err.to_string().contains("2 compile error(s)"));
    // Nothing was loaded, so the Vm can load something else.
    assert!(matches!(
        vm.get_fn::<(), ()>("main"),
        Err(colibri::Error::NotLoaded)
    ));
    vm.load_source("ok.colibri", "fn main() {}\n").unwrap();
}

#[test]
fn syntax_errors_are_compile_errors_too() {
    let (mut vm, _) = vm_with_output();
    let err = vm
        .load_source("syntax.colibri", "print(1 2)\n")
        .unwrap_err();
    assert!(matches!(err, colibri::Error::Compile(_)));
}

#[test]
fn a_panic_while_loading_discards_the_program() {
    let (mut vm, out) = vm_with_output();
    let err = vm
        .load_source("boom.colibri", "print(\"first\")\nlet x = 1 / 0\n")
        .unwrap_err();
    assert!(matches!(err, colibri::Error::Panic { .. }));
    // A script runs top to bottom, so the line before the panic ran.
    assert_eq!(out.contents(), "first\n");
    assert!(matches!(
        vm.get_fn::<(), ()>("main"),
        Err(colibri::Error::NotLoaded)
    ));
    vm.load_source("fine.colibri", "fn f() -> int => 1\n")
        .unwrap();
    assert!(vm.get_fn::<(), i64>("f").is_ok());
}

#[test]
fn a_second_load_is_refused_and_handles_belong_to_their_vm() {
    let (mut vm, _) = vm_with_output();
    vm.load_source("a.colibri", "fn f() -> int => 1\n").unwrap();
    assert!(matches!(
        vm.load_source("b.colibri", "fn g() -> int => 2\n"),
        Err(colibri::Error::AlreadyLoaded)
    ));
    let f = vm.get_fn::<(), i64>("f").unwrap();
    assert_eq!(f.call(&mut vm, ()).unwrap(), 1);

    // A handle only works on the Vm it came from.
    let (mut other, _) = vm_with_output();
    assert!(matches!(
        f.call(&mut other, ()),
        Err(colibri::Error::StaleHandle)
    ));
    other
        .load_source("c.colibri", "fn f() -> int => 3\n")
        .unwrap();
    assert!(matches!(
        f.call(&mut other, ()),
        Err(colibri::Error::StaleHandle)
    ));
    let g = other.get_fn::<(), i64>("f").unwrap();
    assert_eq!(g.call(&mut other, ()).unwrap(), 3);
}

#[test]
fn load_file_reads_a_script_from_disk() {
    let dir = std::env::temp_dir().join(format!("colibri-embed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("script.colibri");
    std::fs::write(&path, "fn twice(n: int) -> int => n * 2\n").unwrap();
    let (mut vm, _) = vm_with_output();
    vm.load_file(&path).unwrap();
    let twice = vm.get_fn::<(i64,), i64>("twice").unwrap();
    assert_eq!(twice.call(&mut vm, (21,)).unwrap(), 42);

    let (mut vm, _) = vm_with_output();
    let err = vm.load_file(dir.join("missing.colibri")).unwrap_err();
    assert!(matches!(err, colibri::Error::Io { .. }));
}

#[test]
fn run_main_without_a_program_or_without_main() {
    let (mut vm, _) = vm_with_output();
    assert!(matches!(vm.run_main(), Err(colibri::Error::NotLoaded)));
    vm.load_source("nomain.colibri", "print(\"x\")\n").unwrap();
    vm.run_main().expect("no main is fine");
}

#[test]
fn warnings_do_not_stop_a_program() {
    let (mut vm, _) = vm_with_output();
    vm.load_source(
        "warn.colibri",
        "fn f() -> int {\n    let x = 1\n    let x = 2\n    return x\n}\n",
    )
    .unwrap();
    assert_eq!(vm.warnings().len(), 1);
    assert!(vm.render_warnings(false).contains("shadows"));
    let f = vm.get_fn::<(), i64>("f").unwrap();
    assert_eq!(f.call(&mut vm, ()).unwrap(), 2);
}

#[test]
fn the_call_depth_limit_is_configurable() {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out))
        .max_call_depth(50)
        .build();
    vm.load_source(
        "depth.colibri",
        "fn down(n: int) -> int => if n == 0 { 0 } else { 1 + down(n - 1) }\n",
    )
    .unwrap();
    let down = vm.get_fn::<(i64,), i64>("down").unwrap();
    assert_eq!(down.call(&mut vm, (40,)).unwrap(), 40);
    let err = down.call(&mut vm, (60,)).unwrap_err();
    match err {
        colibri::Error::Panic { message, stack, .. } => {
            assert!(message.contains("stack overflow"), "{message}");
            assert_eq!(stack.len(), 50);
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(down.call(&mut vm, (10,)).unwrap(), 10);
}

#[test]
fn runaway_recursion_cannot_crash_the_host() {
    // The default call-depth limit is far above what a small thread's stack can hold, so the
    // native stack budget is what stops this: a panic, not an abort.
    let (mut vm, _) = vm_with_output();
    vm.load_source("loop.colibri", "fn f(n: int) -> int => f(n + 1) + 1\n")
        .unwrap();
    let f = vm.get_fn::<(i64,), i64>("f").unwrap();
    let err = f.call(&mut vm, (0,)).unwrap_err();
    assert!(
        matches!(&err, colibri::Error::Panic { message, .. } if message.starts_with("stack overflow")),
        "{err:?}"
    );
}

#[test]
fn check_source_reports_without_running() {
    let ok = colibri::check_source("ok.colibri", "print(\"never run\")\n");
    assert!(!ok.has_errors());
    let bad = colibri::check_source("bad.colibri", "let x: str = 1\n");
    assert!(bad.has_errors());
    assert!(bad.render(false).contains("expected `str`"));
}

#[test]
fn a_function_can_use_structs_enums_and_match_internally() {
    let (mut vm, out) = vm_with_output();
    vm.load_source(
        "shapes.colibri",
        r#"
enum Shape { Circle(float), Rect(float, float), Dot }
struct Pair { lo: int, hi: int }

fn area_of(kind: int, a: float, b: float) -> float {
    let s = match kind {
        0 => Shape.Circle(a)
        1 => Shape.Rect(a, b)
        _ => Shape.Dot
    }
    return match s {
        Circle(r) => 3.0 * r * r
        Rect(w, h) => w * h
        Dot => 0.0
    }
}

fn spread(a: int, b: int) -> int {
    let p = if a < b { Pair { lo: a, hi: b } } else { Pair { lo: b, hi: a } }
    return p.hi - p.lo
}

fn label(n: int) -> str {
    return match n {
        0 => "zero"
        1..=9 => "small"
        _ => "big"
    }
}

fn show_it() {
    print(Shape.Rect(1.0, 2.0))
}
"#,
    )
    .unwrap();
    let area = vm
        .get_fn::<(i64, f64, f64), f64>("area_of")
        .expect("signature matches");
    assert_eq!(area.call(&mut vm, (0, 2.0, 0.0)).unwrap(), 12.0);
    assert_eq!(area.call(&mut vm, (1, 2.0, 4.5)).unwrap(), 9.0);
    assert_eq!(area.call(&mut vm, (2, 0.0, 0.0)).unwrap(), 0.0);
    let spread = vm.get_fn::<(i64, i64), i64>("spread").unwrap();
    assert_eq!(spread.call(&mut vm, (9, 4)).unwrap(), 5);
    let label = vm.get_fn::<(i64,), String>("label").unwrap();
    assert_eq!(label.call(&mut vm, (5,)).unwrap(), "small");
    assert_eq!(label.call(&mut vm, (50,)).unwrap(), "big");
    vm.get_fn::<(), ()>("show_it")
        .unwrap()
        .call(&mut vm, ())
        .unwrap();
    assert_eq!(out.contents(), "Rect(1.0, 2.0)\n");
}

#[test]
fn host_signatures_cannot_name_user_types() {
    let (mut vm, _) = vm_with_output();
    vm.load_source(
        "p.colibri",
        "struct P { x: int }\nfn make() -> P => P { x: 1 }\n",
    )
    .unwrap();
    // A struct is not a host type, so the signature never matches and nothing panics.
    assert!(vm.get_fn::<(), i64>("make").is_err());
}

const NUMERIC_SCRIPT: &str = r#"
fn scale(v: f32, by: f32) -> f32 => v * by
fn widen(v: f32) -> float => v
fn add_bytes(a: u8, b: u8) -> u8 => a + b
fn mix(a: i8, b: i16, c: i32, d: u16, e: u32) -> i64 => int(a) + int(b) + int(c) + int(d) + int(e)
fn big(x: u64) -> u64 => x - 1
fn tag(x: i32) {
    print("tag {x}")
}
"#;

fn numeric_vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let (mut vm, out) = vm_with_output();
    vm.load_source("numeric.colibri", NUMERIC_SCRIPT)
        .expect("script loads");
    (vm, out)
}

#[test]
fn f32_and_sized_integers_cross_the_boundary() {
    let (mut vm, out) = numeric_vm();
    let scale = vm.get_fn::<(f32, f32), f32>("scale").unwrap();
    assert_eq!(scale.call(&mut vm, (0.1, 3.0)).unwrap(), 0.1f32 * 3.0);
    let widen = vm.get_fn::<(f32,), f64>("widen").unwrap();
    assert_eq!(widen.call(&mut vm, (0.1,)).unwrap(), f64::from(0.1f32));
    let add = vm.get_fn::<(u8, u8), u8>("add_bytes").unwrap();
    assert_eq!(add.call(&mut vm, (100, 155)).unwrap(), 255);
    let mix = vm.get_fn::<(i8, i16, i32, u16, u32), i64>("mix").unwrap();
    assert_eq!(
        mix.call(&mut vm, (-128, -32768, i32::MIN, 65535, u32::MAX))
            .unwrap(),
        -128 - 32768 + i64::from(i32::MIN) + 65535 + i64::from(u32::MAX)
    );
    let big = vm.get_fn::<(u64,), u64>("big").unwrap();
    assert_eq!(big.call(&mut vm, (u64::MAX,)).unwrap(), u64::MAX - 1);
    vm.get_fn::<(i32,), ()>("tag")
        .unwrap()
        .call(&mut vm, (-7,))
        .unwrap();
    assert_eq!(out.contents(), "tag -7\n");
}

#[test]
fn arithmetic_overflow_in_a_sized_function_is_a_panic_to_the_host() {
    let (mut vm, _) = numeric_vm();
    let add = vm.get_fn::<(u8, u8), u8>("add_bytes").unwrap();
    match add.call(&mut vm, (200, 100)).unwrap_err() {
        colibri::Error::Panic { message, .. } => assert_eq!(message, "integer overflow"),
        other => panic!("unexpected error: {other:?}"),
    }
    // The VM is still usable.
    assert_eq!(add.call(&mut vm, (1, 2)).unwrap(), 3);
}

#[test]
fn sized_signatures_are_checked_against_the_script() {
    let (vm, _) = numeric_vm();
    // `f32` is not `f64`, `u8` is not `i64`, `u8` is not `i8`, `i64` is `int`.
    assert!(vm.get_fn::<(f64, f64), f64>("scale").is_err());
    assert!(vm.get_fn::<(i64, i64), i64>("add_bytes").is_err());
    assert!(vm.get_fn::<(i8, i8), i8>("add_bytes").is_err());
    assert!(vm.get_fn::<(u64,), i64>("big").is_err());
    let err = vm.get_fn::<(f64,), f64>("widen").unwrap_err();
    assert!(err.to_string().contains("`fn(f32) -> float`"), "{err}");
    assert!(err.to_string().contains("`fn(float) -> float`"), "{err}");
    assert!(vm.get_fn::<(f32,), f64>("widen").is_ok());
}
