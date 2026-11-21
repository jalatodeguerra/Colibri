//! Standard modules written in Colibri (`Vm::std_source_module`, `CheckConfig::sources`): found by
//! `import name` and `import std.name`, virtual files named `<std>/name.colibri`, no warnings
//! from them, and an ordinary program around them.

use std::fs;
use std::path::PathBuf;

use colibri::{CheckConfig, ReloadOutcome, SourceModule, Vm};
use colibri_runtime as colibri;

/// A test-only module. `unused` is a warning anywhere else; here it is not reported.
const DEMO: &str = "pub struct Pair { pub a: int, pub b: int }

pub fn double(x: int) -> int {
    let unused = 1
    return x * 2
}

pub fn pair(a: int, b: int) -> Pair => Pair { a: a, b: b }

impl Add for Pair {
    fn add(self, o: Pair) -> Pair => Pair { a: self.a + o.a, b: self.b + o.b }
}

pub fn fail(n: int) -> int => 100 / n
";

fn vm() -> (Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
    vm.std_source_module("demo", DEMO);
    (vm, out)
}

#[test]
fn import_finds_the_module_by_its_short_and_its_full_name() {
    let (mut vm, out) = vm();
    vm.load_source(
        "main.colibri",
        "import demo\nimport std.demo as again\nprint(demo.double(21))\nprint(again.double(4))\nlet p = demo.pair(1, 2) + demo.pair(10, 20)\nprint(\"{p.a} {p.b}\")\n",
    )
    .unwrap();
    assert_eq!(out.contents(), "42\n8\n11 22\n");
}

#[test]
fn names_can_be_imported_from_it() {
    let (mut vm, out) = vm();
    vm.load_source(
        "main.colibri",
        "import demo.{double, Pair}\nlet p: Pair = Pair { a: 1, b: 2 }\nprint(double(p.b))\n",
    )
    .unwrap();
    assert_eq!(out.contents(), "4\n");
}

#[test]
fn a_program_that_does_not_import_it_does_not_read_it() {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
    // Text that does not parse: it would be an error if it were read.
    vm.std_source_module("broken", "this is not colibri {{{");
    vm.load_source("main.colibri", "print(1)\n").unwrap();
    assert_eq!(out.contents(), "1\n");
    let mut vm2 = Vm::builder().build();
    vm2.std_source_module("broken", "this is not colibri {{{");
    assert!(vm2.load_source("main.colibri", "import broken\n").is_err());
}

#[test]
fn a_local_file_of_the_same_name_is_ambiguous() {
    let (mut vm, _) = vm();
    let err = vm
        .load_sources(&[
            ("main.colibri", "import demo\nprint(demo.double(1))\n"),
            ("demo.colibri", "pub fn double(x: int) -> int => x\n"),
        ])
        .unwrap_err();
    let text = err.render(false);
    assert!(text.contains("`import demo` is ambiguous"), "{text}");
    assert!(
        text.contains("the standard library module `std.demo`"),
        "{text}"
    );
    assert!(text.contains("write `import std.demo`"), "{text}");
}

#[test]
fn the_full_name_is_never_a_local_file() {
    let (mut vm, out) = vm();
    vm.load_sources(&[
        (
            "main.colibri",
            "import std.demo\nimport demo2\nprint(demo.double(3))\nprint(demo2.double(3))\n",
        ),
        ("demo2.colibri", "pub fn double(x: int) -> int => x\n"),
    ])
    .unwrap();
    assert_eq!(out.contents(), "6\n3\n");
}

#[test]
fn warnings_in_the_module_are_not_reported() {
    let (mut vm, _) = vm();
    vm.load_source("main.colibri", "import demo\nprint(demo.double(1))\n")
        .unwrap();
    assert!(
        vm.warnings().is_empty(),
        "{:?}",
        vm.warnings().iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    // The program's own warnings still are.
    let (mut vm, _) = self::vm();
    vm.load_source(
        "main.colibri",
        "import demo\nfn f() { let unused = 1 }\nprint(demo.double(1))\n",
    )
    .unwrap();
    assert_eq!(vm.warnings().len(), 1);
    assert!(vm.warnings()[0].message.contains("unused variable"));
}

#[test]
fn a_panic_names_the_virtual_file() {
    let (mut vm, _) = vm();
    vm.load_source(
        "main.colibri",
        "import demo\nfn go_wrong() -> int => demo.fail(0)\n",
    )
    .unwrap();
    let run = vm.get_fn::<(), i64>("go_wrong").unwrap();
    let err = run.call(&mut vm, ()).unwrap_err();
    let colibri::Error::Panic { stack, .. } = &err else {
        panic!("expected a panic");
    };
    assert_eq!(stack[0].function, "std.demo.fail");
    let text = err.render(false);
    assert!(
        text.contains("at std.demo.fail (<std>/demo.colibri:"),
        "{text}"
    );
}

#[test]
fn the_text_may_have_windows_line_endings() {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
    vm.std_source_module(
        "crlf",
        "pub fn answer() -> int {\r\n    // a comment\r\n    return 42\r\n}\r\n",
    );
    vm.load_source("main.colibri", "import crlf\nprint(crlf.answer())\n")
        .unwrap();
    assert_eq!(out.contents(), "42\n");
}

#[test]
fn when_in_the_module_sees_the_system_but_not_the_programs_features() {
    let src = "@when(os == \"linux\")\npub fn name() -> str => \"linux\"\n@when(os == \"windows\")\npub fn name() -> str => \"windows\"\n@when(os != \"linux\" && os != \"windows\")\npub fn name() -> str => \"other\"\n";
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
    vm.std_source_module("sys", src);
    vm.load_source("main.colibri", "import sys\nprint(sys.name())\n")
        .unwrap();
    let want = if cfg!(target_os = "linux") {
        "linux\n"
    } else if cfg!(windows) {
        "windows\n"
    } else {
        "other\n"
    };
    assert_eq!(out.contents(), want);
}

#[test]
fn a_standard_module_can_import_another() {
    let out = colibri::SharedBuffer::new();
    let mut vm = Vm::builder().stdout(Box::new(out.clone())).build();
    vm.std_source_module("inner", "pub fn one() -> int => 1\n");
    vm.std_source_module(
        "outer",
        "import inner\npub fn two() -> int => inner.one() + 1\n",
    );
    vm.load_source("main.colibri", "import outer\nprint(outer.two())\n")
        .unwrap();
    assert_eq!(out.contents(), "2\n");
}

#[test]
fn check_sources_takes_them_through_the_config() {
    let config = CheckConfig {
        sources: vec![SourceModule {
            name: "demo",
            text: DEMO,
        }],
        ..CheckConfig::default()
    };
    let ok = colibri::check_source_with(
        "main.colibri",
        "import demo\nprint(demo.double(1))\n",
        &config,
    );
    assert!(ok.diagnostics.is_empty(), "{:?}", ok.diagnostics);
    let bad = colibri::check_source_with(
        "main.colibri",
        "import demo\nprint(demo.nothing(1))\n",
        &config,
    );
    assert!(bad.has_errors());
    let without = colibri::check_source("main.colibri", "import demo\n");
    assert!(without.has_errors(), "no source module without the config");
}

fn tree(name: &str, text: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-stdsrc-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("main.colibri"), text).unwrap();
    dir
}

#[test]
fn a_hot_reload_of_the_program_still_patches_and_ignores_the_module() {
    let v1 = "import demo\nfn answer() -> int => demo.double(20) + 1\n";
    let dir = tree("reload", v1);
    let (mut vm, _) = vm();
    vm.load_file(dir.join("main.colibri")).unwrap();
    let answer = vm.get_fn::<(), i64>("answer").unwrap();
    assert_eq!(answer.call(&mut vm, ()).unwrap(), 41);
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Unchanged
    ));
    fs::write(dir.join("main.colibri"), v1.replace("+ 1", "+ 2")).unwrap();
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Patched { functions: 1 }
    ));
    assert_eq!(answer.call(&mut vm, ()).unwrap(), 42);
    // Whitespace is not a change.
    fs::write(
        dir.join("main.colibri"),
        format!("{}\n\n", v1.replace("+ 1", "+  2")),
    )
    .unwrap();
    assert!(matches!(
        vm.reload_changed().unwrap(),
        ReloadOutcome::Unchanged
    ));
}

#[test]
fn a_feature_of_the_program_is_unknown_inside_the_module() {
    // The module's tree has no manifest, so it declares no feature: using the program's `fast`
    // there is an error in the module, not a silent `false`.
    let src = "@when(fast)\npub fn mode() -> str => \"on\"\n";
    let cfg = colibri::BuildCfg::default().with_feature("fast");
    let mut vm = Vm::builder()
        .stdout(Box::new(std::io::sink()))
        .build_cfg(cfg)
        .build();
    vm.std_source_module("feat", src);
    let err = vm
        .load_source("main.colibri", "import feat\nprint(feat.mode())\n")
        .unwrap_err();
    let colibri::Error::Compile(report) = &err else {
        panic!("expected a compile error, got {err}");
    };
    let d = report
        .diagnostics
        .iter()
        .find(|d| d.message == "unknown name `fast` in `@when`")
        .unwrap_or_else(|| panic!("{:?}", report.diagnostics));
    assert_eq!(report.sources.name(d.span.file), "<std>/feat.colibri");
}
