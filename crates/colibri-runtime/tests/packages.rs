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
fn registry_dependencies_are_not_supported_yet() {
    let text = manifest_error(
        "registry",
        &[
            (
                "colibri.toml",
                "[package]\nname = \"a\"\n[dependencies]\nfoo = { version = \"1\" }\n",
            ),
            ("main.colibri", "fn main() {}\n"),
        ],
    );
    assert!(
        text.contains("registry dependencies are not supported yet"),
        "{text}"
    );
}

#[test]
fn git_dependencies_are_not_fetched_by_loading() {
    // `load_package` reads `colibri.lock` and the cache; fetching is the command line tool's job.
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
    assert!(text.contains("not in `colibri.lock` yet"), "{text}");
    assert!(text.contains("colibri check"), "{text}");
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

const APP_MANIFEST: &str = "[package]\nname = \"app\"\n";

fn check_all(dir: &Path) -> colibri::Report {
    colibri::check_package_all_with(dir, &colibri::CheckConfig::default()).unwrap()
}

#[test]
fn a_module_nobody_imports_may_have_top_level_statements() {
    let dir = tree(
        "script-module",
        &[
            ("colibri.toml", APP_MANIFEST),
            (
                "main.colibri",
                "import util\nfn main() { print(\"{util.f()}\") }\n",
            ),
            ("util.colibri", "pub fn f() -> int { return 1 }\n"),
            ("examples/demo.colibri", "print(\"demo script\")\n"),
        ],
    );
    let report = check_all(&dir);
    assert!(!report.has_errors(), "{}", report.render(false));
    // The statements are still checked.
    std::fs::write(dir.join("examples/demo.colibri"), "print(1 + \"a\")\n").unwrap();
    let report = check_all(&dir);
    assert!(report.has_errors(), "{}", report.render(false));
    // `run` does not load it at all, and it never runs under `test`.
    std::fs::write(
        dir.join("examples/demo.colibri"),
        "print(\"demo script\")\n",
    )
    .unwrap();
    let (mut vm, out) = vm();
    vm.load_tests(&dir).unwrap();
    assert_eq!(out.contents(), "");
}

#[test]
fn an_imported_module_still_may_not_have_top_level_statements() {
    let dir = tree(
        "imported-stmt",
        &[
            ("colibri.toml", APP_MANIFEST),
            (
                "main.colibri",
                "import util\nfn main() { print(\"{util.f()}\") }\n",
            ),
            (
                "util.colibri",
                "print(\"hi\")\npub fn f() -> int { return 1 }\n",
            ),
        ],
    );
    let report = check_all(&dir);
    assert!(
        report
            .render(false)
            .contains("top-level statements are only allowed in the entry file"),
        "{}",
        report.render(false)
    );
}

// Unix only: creating a symlink needs a privilege on Windows that a test cannot count on.
#[cfg(unix)]
#[test]
fn the_folder_walk_does_not_follow_links() {
    use std::os::unix::fs::symlink;
    let dir = tree(
        "links",
        &[
            ("colibri.toml", APP_MANIFEST),
            ("main.colibri", "fn main() { print(\"main\") }\n"),
            (
                "sub/m.colibri",
                "pub fn f() -> int {\n    let spare = 1\n    return 2\n}\n",
            ),
        ],
    );
    symlink("../sub", dir.join("sub/again")).unwrap();
    symlink("sub", dir.join("subl")).unwrap();
    symlink("sub/m.colibri", dir.join("linked.colibri")).unwrap();
    let report = check_all(&dir);
    let text = report.render(false);
    assert!(!report.has_errors(), "{text}");
    assert_eq!(
        text.matches("Warning: unused variable `spare`").count(),
        1,
        "{text}"
    );
    // A file reached through a link and by its own name is one module.
    let (mut vm, _) = vm();
    vm.load_tests(&dir).unwrap();
    assert_eq!(
        vm.render_warnings(false)
            .matches("Warning: unused variable `spare`")
            .count(),
        1
    );
}

#[test]
fn deprecation_warnings_are_for_the_apps_code_not_a_dependencys() {
    let dir = tree(
        "depdeprecated",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nlib = { path = \"../lib\" }\n",
            ),
            (
                "app/main.colibri",
                "import lib\nfn main() {\n    print(\"{lib.old()}\")\n}\n",
            ),
            ("lib/colibri.toml", "[package]\nname = \"lib\"\n"),
            (
                "lib/lib.colibri",
                "@deprecated(\"use fresh\")\npub fn old() -> int => 1\npub fn fresh() -> int => 2\nfn inside() -> int => old()\n",
            ),
        ],
    );
    let (mut vm, _) = vm();
    vm.load_package(dir.join("app")).unwrap();
    let warnings = vm.render_warnings(false);
    // The app's use warns once; the library's own use of its function does not.
    assert_eq!(
        warnings.matches("`old` is deprecated: use fresh").count() / 2,
        1,
        "{warnings}"
    );
    assert!(warnings.contains("app/main.colibri") || warnings.contains("main.colibri"));
    assert!(!warnings.contains("lib.colibri"), "{warnings}");
}

const WHEN_SOURCE: &str = "@when(debug)\nfn mode() -> str => \"debug\"\n@when(!debug)\nfn mode() -> str => \"release\"\n@when(gpu)\nfn device() -> str => \"gpu\"\n@when(!gpu)\nfn device() -> str => \"cpu\"\nfn main() {\n    print(\"{mode()} {device()}\")\n}\n";

#[test]
fn an_embedder_sets_the_when_flags_on_the_vm_and_in_the_check_config() {
    let run = |builder: colibri::VmBuilder| {
        let out = colibri::SharedBuffer::new();
        let mut vm = builder.stdout(Box::new(out.clone())).build();
        vm.load_source("main.colibri", WHEN_SOURCE).unwrap();
        vm.run_main().unwrap();
        out.contents()
    };
    assert_eq!(run(colibri::Vm::builder().feature("gpu")), "debug gpu\n");
    assert_eq!(
        run(colibri::Vm::builder().feature("gpu").release()),
        "release gpu\n"
    );
    assert_eq!(
        run(colibri::Vm::builder()
            .build_cfg(colibri::BuildCfg::default().with_feature("gpu").release())),
        "release gpu\n"
    );
    // Checking without a `Vm` reads the same settings from the config.
    // A script has no manifest to declare features in: the flag is unknown until it is given.
    let report = colibri::check_source("main.colibri", WHEN_SOURCE);
    assert!(report.render(false).contains("unknown name `gpu`"));
    let config = colibri::CheckConfig {
        cfg: colibri::BuildCfg::default().with_feature("gpu"),
        ..colibri::CheckConfig::default()
    };
    let ok = colibri::check_source_with("main.colibri", WHEN_SOURCE, &config);
    assert!(!ok.has_errors(), "{}", ok.render(false));
    // A condition naming an unknown flag is an error in both.
    let bad = colibri::check_source_with(
        "main.colibri",
        "@when(nope)\nfn f() {}\nfn main() {}\n",
        &config,
    );
    assert!(bad.render(false).contains("unknown name `nope`"));
}

#[test]
fn a_dependency_has_its_own_features_and_the_apps_flags_do_not_reach_it() {
    let dir = tree(
        "depfeatures",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nlib = { path = \"../lib\" }\n\n[features]\nfancy = []\n",
            ),
            (
                "app/main.colibri",
                "import lib\n@when(fancy)\nfn extra() -> str => \"fancy\"\n@when(!fancy)\nfn extra() -> str => \"plain\"\nfn main() {\n    print(\"{lib.name()} {extra()}\")\n}\n",
            ),
            (
                "lib/colibri.toml",
                "[package]\nname = \"lib\"\n\n[features]\ndefault = [\"std_names\"]\nstd_names = []\nrare = []\n",
            ),
            (
                "lib/lib.colibri",
                "@when(std_names)\npub fn name() -> str => \"lib-default\"\n@when(!std_names)\npub fn name() -> str => \"lib-other\"\n@when(rare)\nfn never_here() -> int => nowhere\n",
            ),
        ],
    );
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .feature("fancy")
        .build();
    vm.load_package(dir.join("app")).unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "lib-default fancy\n");
    // The app's flag is not a feature of the library: it is the library's own default list
    // that decides, so `rare` stays off and `never_here` is not even checked.
}

#[test]
fn feature_flags_follow_the_manifest_name_rules() {
    for (name, why) in [
        ("debug", "it has a meaning of its own"),
        ("os", "it has a meaning of its own"),
        ("true", "it has a meaning of its own"),
        ("", "cannot be empty"),
        ("a b", "is not a valid feature name"),
        ("fn", "it is a keyword"),
    ] {
        let mut vm = colibri::Vm::builder().feature(name).build();
        let err = vm
            .load_source("main.colibri", "fn main() {}\n")
            .expect_err(name);
        let text = errors(err);
        assert!(text.contains("feature name"), "[{name}] {text}");
        assert!(text.contains(why), "[{name}] {text}");
    }
    // The same rule for a check without a `Vm`.
    let config = colibri::CheckConfig {
        cfg: colibri::BuildCfg::default().with_feature("debug"),
        ..colibri::CheckConfig::default()
    };
    let report = colibri::check_source_with("main.colibri", "fn main() {}\n", &config);
    assert!(report.has_errors(), "{}", report.render(false));
    // A good name still works.
    let mut vm = colibri::Vm::builder().feature("fast_2").build();
    vm.load_source("main.colibri", "fn main() {}\n").unwrap();
}

#[test]
fn a_member_left_out_by_when_says_so_through_its_module() {
    let dir = tree(
        "leftout",
        &[
            (
                "run.colibri",
                "import util\nfn main() { print(\"{util.twice(2)}\") }\n",
            ),
            (
                "util.colibri",
                "@when(os == \"linux\" && os == \"macos\")\npub fn twice(n: int) -> int => n * 2\n",
            ),
        ],
    );
    let (mut vm, _) = vm();
    let text = load_err(&mut vm, |vm| vm.load_file(dir.join("run.colibri")));
    assert!(text.contains("has no `twice`"), "{text}");
    assert!(
        text.contains("`twice` exists but is left out here by `@when("),
        "{text}"
    );
}

// ---- features of dependencies (DESIGN §3.6) ----

const LIB_MANIFEST: &str = "[package]\nname = \"lib\"\n\n[features]\ndefault = [\"z\"]\nx = []\ny = []\nz = []\nboth = [\"x\", \"y\"]\n";
const LIB_SOURCE: &str = "@when(x)\nfn has_x() -> bool => true\n@when(!x)\nfn has_x() -> bool => false\n@when(y)\nfn has_y() -> bool => true\n@when(!y)\nfn has_y() -> bool => false\n@when(z)\nfn has_z() -> bool => true\n@when(!z)\nfn has_z() -> bool => false\npub fn flags() -> str => \"x={has_x()} y={has_y()} z={has_z()}\"\n";

fn run_app(dir: &Path) -> Result<String, String> {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    match vm.load_package(dir) {
        Ok(()) => {
            vm.run_main().unwrap();
            Ok(out.contents())
        }
        Err(e) => Err(errors(e)),
    }
}

#[test]
fn a_dependency_entry_turns_on_features_of_the_dependency() {
    let dir = tree(
        "depfeat_on",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nlib = { path = \"../lib\", features = [\"x\"] }\n",
            ),
            (
                "app/main.colibri",
                "import lib\nfn main() { print(lib.flags()) }\n",
            ),
            ("lib/colibri.toml", LIB_MANIFEST),
            ("lib/lib.colibri", LIB_SOURCE),
        ],
    );
    // `x` as asked, `z` from the dependency's own default list.
    assert_eq!(
        run_app(&dir.join("app")).unwrap(),
        "x=true y=false z=true\n"
    );
}

#[test]
fn default_features_false_leaves_the_default_list_off() {
    let dir = tree(
        "depfeat_nodefault",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nlib = { path = \"../lib\", features = [\"both\"], default-features = false }\n",
            ),
            (
                "app/main.colibri",
                "import lib\nfn main() { print(lib.flags()) }\n",
            ),
            ("lib/colibri.toml", LIB_MANIFEST),
            ("lib/lib.colibri", LIB_SOURCE),
        ],
    );
    // `both` turns on `x` and `y` (a feature may list features); `z` is a default and is off.
    assert_eq!(
        run_app(&dir.join("app")).unwrap(),
        "x=true y=true z=false\n"
    );
    // Nothing asked and no defaults: everything is off.
    fs::write(
        dir.join("app/colibri.toml"),
        "[package]\nname = \"app\"\n\n[dependencies]\nlib = { path = \"../lib\", default-features = false }\n",
    )
    .unwrap();
    assert_eq!(
        run_app(&dir.join("app")).unwrap(),
        "x=false y=false z=false\n"
    );
}

#[test]
fn features_of_a_dependency_are_the_union_over_everything_that_depends_on_it() {
    // app -> a -> lib (x), app -> b -> lib (y, default-features = false), app -> lib (nothing).
    let dir = tree(
        "depfeat_diamond",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\na = { path = \"../a\" }\nb = { path = \"../b\" }\n",
            ),
            (
                "app/main.colibri",
                "import lib\nimport a\nimport b\nfn main() { print(lib.flags()) }\n",
            ),
            (
                "a/colibri.toml",
                "[package]\nname = \"a\"\n\n[dependencies]\nlib = { path = \"../lib\", features = [\"x\"], default-features = false }\n",
            ),
            ("a/lib.colibri", "pub fn id() -> int => 1\n"),
            (
                "b/colibri.toml",
                "[package]\nname = \"b\"\n\n[dependencies]\nlib = { path = \"../lib\", features = [\"y\"], default-features = false }\n",
            ),
            ("b/lib.colibri", "pub fn id() -> int => 2\n"),
            ("lib/colibri.toml", LIB_MANIFEST),
            ("lib/lib.colibri", LIB_SOURCE),
        ],
    );
    // The app has to list lib itself to import it: add it with no features.
    fs::write(
        dir.join("app/colibri.toml"),
        "[package]\nname = \"app\"\n\n[dependencies]\na = { path = \"../a\" }\nb = { path = \"../b\" }\nlib = { path = \"../lib\", default-features = false }\n",
    )
    .unwrap();
    // x (from a) and y (from b); every dependent turned the defaults off, so `z` is off.
    assert_eq!(
        run_app(&dir.join("app")).unwrap(),
        "x=true y=true z=false\n"
    );
    // One dependent that does not turn them off brings `z` back.
    fs::write(
        dir.join("app/colibri.toml"),
        "[package]\nname = \"app\"\n\n[dependencies]\na = { path = \"../a\" }\nb = { path = \"../b\" }\nlib = { path = \"../lib\" }\n",
    )
    .unwrap();
    assert_eq!(run_app(&dir.join("app")).unwrap(), "x=true y=true z=true\n");
}

#[test]
fn a_feature_the_dependency_does_not_declare_names_both_manifests() {
    let dir = tree(
        "depfeat_unknown",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nlib = { path = \"../lib\", features = [\"turbo\"] }\n",
            ),
            (
                "app/main.colibri",
                "import lib\nfn main() { print(lib.flags()) }\n",
            ),
            ("lib/colibri.toml", LIB_MANIFEST),
            ("lib/lib.colibri", LIB_SOURCE),
        ],
    );
    let text = run_app(&dir.join("app")).unwrap_err();
    assert!(
        text.contains(
            "the package `app` asks for the feature `turbo` of `lib`, which `lib` does not declare"
        ),
        "{text}"
    );
    // Files are named the system's way (`\` on Windows).
    let text = text.replace('\\', "/");
    assert!(text.contains("app/colibri.toml"), "{text}");
    assert!(text.contains("lib/colibri.toml"), "{text}");
    assert!(
        text.contains("the declared features are: default, x, y, z, both")
            || text.contains("the declared features are:"),
        "{text}"
    );
}

#[test]
fn the_apps_flags_reach_only_the_root_package() {
    let dir = tree(
        "depfeat_root_only",
        &[
            (
                "app/colibri.toml",
                "[package]\nname = \"app\"\n\n[dependencies]\nlib = { path = \"../lib\" }\n\n[features]\nx = []\n",
            ),
            (
                "app/main.colibri",
                "import lib\nfn main() { print(lib.flags()) }\n",
            ),
            ("lib/colibri.toml", LIB_MANIFEST),
            ("lib/lib.colibri", LIB_SOURCE),
        ],
    );
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .feature("x")
        .build_cfg(colibri::BuildCfg {
            no_default_features: true,
            ..colibri::BuildCfg::default().with_feature("x")
        })
        .build();
    vm.load_package(dir.join("app")).unwrap();
    vm.run_main().unwrap();
    // `--feature x` and `--no-default-features` are the app's; the library keeps its defaults.
    assert_eq!(out.contents(), "x=false y=false z=true\n");
}
