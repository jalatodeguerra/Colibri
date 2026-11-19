//! Packages and `import` (ARCHITECTURE §11): loading from disk and from memory, `colibri.toml`,
//! path dependencies and named host modules.

use std::fs;
use std::path::{Path, PathBuf};

use colibri::{Callback, Ctx, HostType, TypeBuilder};
use colibri_runtime as colibri;

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    (vm, out)
}

/// A fresh temporary folder holding `files` (`(relative path, text)`).
fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-pkg-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for (path, text) in files {
        let full = dir.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, text).unwrap();
    }
    dir
}

fn errors(err: colibri::Error) -> String {
    match err {
        colibri::Error::Compile(report) => report.render(false),
        other => other.render(false),
    }
}

fn load_err(
    vm: &mut colibri::Vm,
    f: impl FnOnce(&mut colibri::Vm) -> Result<(), colibri::Error>,
) -> String {
    errors(f(vm).expect_err("loading fails"))
}

const MAIN_USES_UTIL: &str = "import util\nfn main() { print(\"{util.twice(21)}\") }\n";
const UTIL: &str = "pub fn twice(n: int) -> int { return n * 2 }\n";

#[test]
fn load_file_loads_its_siblings() {
    let dir = tree(
        "file",
        &[("run.colibri", MAIN_USES_UTIL), ("util.colibri", UTIL)],
    );
    let (mut vm, out) = vm();
    vm.load_file(dir.join("run.colibri")).unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "42\n");
}

#[test]
fn load_file_finds_subfolders_as_packages() {
    let dir = tree(
        "sub",
        &[
            (
                "main.colibri",
                "import ui.button\nfn main() { print(button.label()) }\n",
            ),
            (
                "ui/button.colibri",
                "pub fn label() -> str { return \"ok\" }\n",
            ),
        ],
    );
    let (mut vm, out) = vm();
    vm.load_file(dir.join("main.colibri")).unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "ok\n");
}

#[test]
fn load_package_reads_the_manifest_and_starts_at_main() {
    let dir = tree(
        "pkg",
        &[
            ("colibri.toml", "[package]\nname = \"game\"\n"),
            ("main.colibri", MAIN_USES_UTIL),
            ("util.colibri", UTIL),
        ],
    );
    let (mut vm, out) = vm();
    vm.load_package(&dir).unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "42\n");
}

#[test]
fn load_package_without_a_main_file_is_an_error() {
    let dir = tree(
        "nomain",
        &[("colibri.toml", "[package]\nname = \"game\"\n")],
    );
    let (mut vm, _) = vm();
    let err = vm.load_package(&dir).expect_err("no main.colibri");
    assert!(err.to_string().contains("main.colibri"), "{err}");
}

#[test]
fn load_sources_takes_text_and_names_files_by_path() {
    let (mut vm, out) = vm();
    vm.load_sources(&[
        (
            "app/main.colibri",
            "import game.player\nfn main() { print(player.hello()) }\n",
        ),
        (
            "app/game/player.colibri",
            "pub fn hello() -> str { return \"hi\" }\n",
        ),
    ])
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "hi\n");
}

#[test]
fn load_sources_reports_errors_with_the_files_path() {
    let (mut vm, _) = vm();
    let text = load_err(&mut vm, |vm| {
        vm.load_sources(&[
            ("app/main.colibri", "import lib\nfn main() { lib.f() }\n"),
            (
                "app/lib.colibri",
                "pub fn f() -> int {\n    return \"x\"\n}\n",
            ),
        ])
    });
    assert!(text.contains("app/lib.colibri"), "{text}");
}

#[test]
fn load_source_has_no_siblings() {
    let (mut vm, _) = vm();
    let text = load_err(&mut vm, |vm| vm.load_source("a.colibri", "import util\n"));
    assert!(text.contains("no module `util`"), "{text}");
}

#[test]
fn a_panic_trace_names_the_file_it_happened_in() {
    let (mut vm, _) = vm();
    vm.load_sources(&[
        (
            "app/main.colibri",
            "import lib\nfn main() { lib.boom(0) }\n",
        ),
        (
            "app/lib.colibri",
            "pub fn boom(n: int) -> int {\n    return 1 / n\n}\n",
        ),
    ])
    .unwrap();
    let err = vm.run_main().expect_err("panics");
    let colibri::Error::Panic {
        location, stack, ..
    } = &err
    else {
        panic!("a panic, got {err:?}");
    };
    assert_eq!(location.as_ref().unwrap().file, "app/lib.colibri");
    assert!(stack.iter().any(|f| f.location.file == "app/main.colibri"));
    assert!(stack.iter().any(|f| f.function == "lib.boom"), "{stack:?}");
}

#[test]
fn path_dependencies_are_imported_by_their_key() {
    let dir = tree(
        "dep",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nphysics = { path = \"../physics\" }\n",
            ),
            (
                "app/main.colibri",
                "import physics\nimport physics.vec\nfn main() { print(\"{physics.g()} {vec.zero()}\") }\n",
            ),
            ("physics/colibri.toml", "[package]\nname = \"physics\"\n"),
            ("physics/lib.colibri", "pub fn g() -> int { return 10 }\n"),
            ("physics/vec.colibri", "pub fn zero() -> int { return 0 }\n"),
        ],
    );
    let (mut vm, out) = vm();
    vm.load_package(dir.join("app")).unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "10 0\n");
}

fn manifest_error(name: &str, files: &[(&str, &str)]) -> String {
    let dir = tree(name, files);
    let (mut vm, _) = vm();
    match vm.load_package(&dir) {
        Ok(()) => panic!("loading should fail"),
        Err(e) => errors(e),
    }
}

#[test]
fn git_dependencies_are_not_supported_yet() {
    let text = manifest_error(
        "git",
        &[
            (
                "colibri.toml",
                "[package]\nname = \"a\"\n[dependencies]\nfoo = { git = \"https://x/y\" }\n",
            ),
            ("main.colibri", "fn main() {}\n"),
        ],
    );
    assert!(
        text.contains("git dependencies are not supported yet"),
        "{text}"
    );
}

#[test]
fn manifest_errors_say_where_and_why() {
    let bad_toml = manifest_error(
        "badtoml",
        &[
            ("colibri.toml", "[package]\nname = 1\n"),
            ("main.colibri", "fn main() {}\n"),
        ],
    );
    assert!(bad_toml.contains("numbers are not supported"), "{bad_toml}");
    assert!(bad_toml.contains("colibri.toml"), "{bad_toml}");

    let missing_path = manifest_error(
        "nopath",
        &[
            (
                "colibri.toml",
                "[package]\nname = \"a\"\n[dependencies]\nfoo = { path = \"../nowhere\" }\n",
            ),
            ("main.colibri", "fn main() {}\n"),
        ],
    );
    assert!(missing_path.contains("nowhere"), "{missing_path}");
}

#[test]
fn a_dependency_whose_name_differs_from_its_key_is_an_error() {
    let dir = tree(
        "mismatch2",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"a\"\n[dependencies]\nfoo = { path = \"../other\" }\n",
            ),
            ("app/main.colibri", "fn main() {}\n"),
            ("other/colibri.toml", "[package]\nname = \"bar\"\n"),
            ("other/lib.colibri", "\n"),
        ],
    );
    let (mut vm, _) = vm();
    let err = vm.load_package(dir.join("app")).expect_err("name mismatch");
    let text = errors(err);
    assert!(text.contains("bar") && text.contains("foo"), "{text}");
}

#[test]
fn a_local_file_and_a_dependency_with_one_name_are_ambiguous() {
    let dir = tree(
        "ambig",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"a\"\n[dependencies]\nutil = { path = \"../util\" }\n",
            ),
            ("app/main.colibri", "import util\nfn main() {}\n"),
            ("app/util.colibri", UTIL),
            ("util/colibri.toml", "[package]\nname = \"util\"\n"),
            ("util/lib.colibri", UTIL),
        ],
    );
    let (mut vm, _) = vm();
    let text = errors(vm.load_package(dir.join("app")).expect_err("ambiguous"));
    assert!(
        text.contains("ambiguous") || text.contains("more than one"),
        "{text}"
    );
    assert!(text.contains("util.colibri"), "{text}");
}

// ---- named host modules ----

struct Body {
    mass: i64,
}

impl HostType for Body {
    const NAME: &'static str = "Body";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.static_fn("new", ["mass"], |mass: i64| Body { mass });
        d.method("mass", [], |this: &Body| this.mass);
        d.method(
            "each",
            ["f"],
            |cx: &mut Ctx, _this: &Body, f: Callback<(i64,), ()>| {
                f.call(cx, (1,)).map_err(|e| e.to_string())
            },
        );
    }
}

fn physics_vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let (mut vm, out) = vm();
    let mut m = vm.module("physics");
    m.register_fn("gravity", || 10i64).unwrap();
    m.register_type::<Body>().unwrap();
    (vm, out)
}

fn run_source(vm: &mut colibri::Vm, src: &str) -> Result<(), colibri::Error> {
    vm.load_source("game.colibri", src)?;
    vm.run_main()
}

#[test]
fn a_named_host_module_is_imported() {
    let (mut vm, out) = physics_vm();
    run_source(
        &mut vm,
        "import physics\nfn main() {\n    let b = physics.Body.new(3)\n    print(\"{physics.gravity()} {b.mass()}\")\n}\n",
    )
    .unwrap();
    assert_eq!(out.contents(), "10 3\n");
}

#[test]
fn a_host_module_supports_selective_imports_and_aliases() {
    let (mut vm, out) = physics_vm();
    run_source(
        &mut vm,
        "import physics.{gravity, Body}\nimport physics as p\nfn main() {\n    let b = Body.new(2)\n    print(\"{gravity()} {p.gravity()} {b.mass()}\")\n}\n",
    )
    .unwrap();
    assert_eq!(out.contents(), "10 10 2\n");
}

#[test]
fn a_named_host_module_is_not_in_scope_without_import() {
    let (mut vm, _) = physics_vm();
    let text = load_err(&mut vm, |vm| {
        run_source(vm, "fn main() { print(\"{gravity()}\") }\n")
    });
    assert!(text.contains("gravity"), "{text}");
}

#[test]
fn the_default_host_module_needs_no_import() {
    let (mut vm, out) = vm();
    vm.register_fn("answer", || 42i64).unwrap();
    run_source(&mut vm, "fn main() { print(\"{answer()}\") }\n").unwrap();
    assert_eq!(out.contents(), "42\n");
}

#[test]
fn importing_a_module_nobody_registered_is_an_error() {
    let (mut vm, _) = vm();
    let text = load_err(&mut vm, |vm| {
        run_source(vm, "import physics\nfn main() {}\n")
    });
    assert!(text.contains("no module `physics`"), "{text}");
}

#[test]
fn a_host_module_and_a_local_file_with_one_name_are_ambiguous() {
    let (mut vm, _) = physics_vm();
    let text = load_err(&mut vm, |vm| {
        vm.load_sources(&[
            ("app/main.colibri", "import physics\nfn main() {}\n"),
            (
                "app/physics.colibri",
                "pub fn gravity() -> int { return 1 }\n",
            ),
        ])
    });
    assert!(
        text.contains("physics.colibri") && text.contains("host"),
        "{text}"
    );
}

#[test]
fn module_names_must_be_identifiers_and_not_std() {
    let (mut vm, _) = vm();
    assert!(vm.module("std").register_fn("f", || 1i64).is_err());
    assert!(vm.module("not a name").register_fn("f", || 1i64).is_err());
    assert_eq!(vm.module_names(), vec!["host".to_string()]);
}

#[test]
fn each_module_has_its_own_interface_that_round_trips() {
    let (mut vm, _) = physics_vm();
    vm.register_fn("answer", || 42i64).unwrap();
    assert_eq!(
        vm.module_names(),
        vec!["host".to_string(), "physics".to_string()]
    );

    let default = vm.interface_text();
    assert!(
        default.contains("module = \"host\"") && !default.contains("gravity"),
        "{default}"
    );

    let physics = vm.module("physics").interface_text();
    assert!(physics.contains("module = \"physics\""), "{physics}");
    assert!(
        physics.contains("gravity") && physics.contains("Body"),
        "{physics}"
    );

    let dir = tree("iface", &[]);
    fs::create_dir_all(&dir).unwrap();
    let path: &Path = &dir.join("physics.cbi");
    assert!(vm.module("physics").write_interface(path).unwrap());
    vm.module("physics").verify_interface(path).unwrap();
    // The default module's interface is not this file.
    assert!(vm.verify_interface(path).is_err());
}

const REGISTRY: &str = "let started = announce()\nfn announce() -> int {\n    print(\"registry initialised\")\n    return 1\n}\n";

fn warnings_of(name: &str, files: &[(&str, &str)]) -> String {
    let dir = tree(name, files);
    let (mut vm, _) = vm();
    vm.load_package(&dir).unwrap();
    vm.render_warnings(false)
}

#[test]
fn an_unused_import_of_a_module_with_load_effects_says_how_to_keep_it() {
    let warnings = warnings_of(
        "effects",
        &[
            ("colibri.toml", "[package]\nname = \"app\"\n"),
            (
                "main.colibri",
                "import registry\nfn main() { print(\"main\") }\n",
            ),
            ("registry.colibri", REGISTRY),
        ],
    );
    assert!(warnings.contains("unused import `registry`"), "{warnings}");
    assert!(
        warnings.contains("`import registry as _registry`"),
        "{warnings}"
    );
}

#[test]
fn an_unused_import_of_a_pure_module_just_says_remove_it() {
    let warnings = warnings_of(
        "pure",
        &[
            ("colibri.toml", "[package]\nname = \"app\"\n"),
            (
                "main.colibri",
                "import util\nfn main() { print(\"main\") }\n",
            ),
            (
                "util.colibri",
                "let LIMIT = 10\nlet NAME = \"x\"\npub fn f() -> int { return LIMIT }\n",
            ),
        ],
    );
    assert!(warnings.contains("unused import `util`"), "{warnings}");
    assert!(!warnings.contains("as _util"), "{warnings}");
}

#[test]
fn an_underscore_alias_keeps_the_import_without_a_warning_and_still_loads_it() {
    let dir = tree(
        "underscore",
        &[
            ("colibri.toml", "[package]\nname = \"app\"\n"),
            (
                "main.colibri",
                "import registry as _registry\nfn main() { print(\"main\") }\n",
            ),
            ("registry.colibri", REGISTRY),
        ],
    );
    let (mut vm, out) = vm();
    vm.load_package(&dir).unwrap();
    assert_eq!(vm.render_warnings(false), "");
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "registry initialised\nmain\n");
}

#[test]
fn lints_are_reported_for_the_apps_code_not_a_dependencys() {
    let dir = tree(
        "deplint",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nlib = { path = \"../lib\" }\n",
            ),
            (
                "app/main.colibri",
                "import lib\nfn main() {\n    let mine = 1\n    print(\"{lib.f()}\")\n}\n",
            ),
            ("lib/colibri.toml", "[package]\nname = \"lib\"\n"),
            (
                "lib/lib.colibri",
                "import helper\npub fn f() -> int {\n    let spare = 1\n    return 2\n}\n",
            ),
            ("lib/helper.colibri", "pub fn g() -> int { return 1 }\n"),
        ],
    );
    let (mut vm, _) = vm();
    vm.load_package(dir.join("app")).unwrap();
    let warnings = vm.render_warnings(false);
    assert!(warnings.contains("unused variable `mine`"), "{warnings}");
    assert!(!warnings.contains("spare"), "{warnings}");
    assert!(!warnings.contains("unused import `helper`"), "{warnings}");
}
