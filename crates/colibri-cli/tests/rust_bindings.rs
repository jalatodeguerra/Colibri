//! `[rust-dependencies]` and `colibri check` (B3): each key is a binding module whose interface
//! is `<key>.cbi` next to `colibri.toml`; no other `.cbi` is read for it, nothing calls cargo.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct Out {
    stderr: String,
    code: i32,
}

/// A fresh package folder, removed at the end.
struct Pkg(PathBuf);

impl Pkg {
    fn new(name: &str, manifest_extra: &str, main: &str) -> Pkg {
        let dir = std::env::temp_dir().join(format!("colibri-rust-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let pkg = Pkg(dir);
        pkg.write(
            "colibri.toml",
            &format!("[package]\nname = \"app\"\n{manifest_extra}"),
        );
        pkg.write("main.colibri", main);
        pkg
    }

    fn write(&self, rel: &str, text: &str) {
        let full = self.0.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, text).unwrap();
    }

    fn run(&self) -> Out {
        self.command("run")
    }

    fn check(&self) -> Out {
        self.command("check")
    }

    fn command(&self, verb: &str) -> Out {
        // An empty `PATH` and a dead `CARGO`: a check that tried to run cargo would fail.
        let out = Command::new(env!("CARGO_BIN_EXE_colibri"))
            .args([verb, self.0.to_str().unwrap()])
            .env("PATH", "")
            .env("CARGO", "colibri-no-such-cargo")
            .output()
            .expect("the colibri binary runs");
        Out {
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().unwrap_or(-1),
        }
    }
}

impl Drop for Pkg {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The interface text of a module `module` with `add(a: int, b: int) -> int`, written as the
/// binding generator will write it.
fn interface_of(module: &str) -> String {
    let mut vm = colibri_runtime::Vm::builder().build();
    vm.module(module)
        .register_fn("add", |a: i64, b: i64| a + b)
        .unwrap();
    vm.module(module).interface_text()
}

const MANIFEST: &str = "\n[rust-dependencies]\nnative = { path = \"native/\" }\n";
const CALL: &str = "import native\nfn main() {\n    print(native.add(1, 2))\n}\n";

#[test]
fn a_binding_interface_next_to_the_manifest_type_checks_a_call() {
    let pkg = Pkg::new("ok", MANIFEST, CALL);
    pkg.write("native.cbi", &interface_of("native"));
    let out = pkg.check();
    assert_eq!(out.code, 0, "{}", out.stderr);
}

#[test]
fn a_wrong_argument_type_for_a_binding_function_is_an_error() {
    let pkg = Pkg::new(
        "wrong",
        MANIFEST,
        "import native\nfn main() {\n    print(native.add(\"one\", 2))\n}\n",
    );
    pkg.write("native.cbi", &interface_of("native"));
    let out = pkg.check();
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("add"), "{}", out.stderr);
}

#[test]
fn a_missing_interface_says_to_generate_it() {
    let pkg = Pkg::new("missing", MANIFEST, CALL);
    let out = pkg.check();
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains(
            "`native` is a Rust binding: run `colibri bindings` to generate `native.cbi`"
        ),
        "{}",
        out.stderr
    );
    // The error is at the key, in the manifest.
    assert!(out.stderr.contains("colibri.toml"), "{}", out.stderr);
}

#[test]
fn an_unrelated_cbi_is_not_a_binding() {
    // `engine.cbi` sits at the top of the package but no key names it, and `native.cbi` is
    // missing: the program must not type-check through the wrong file.
    let pkg = Pkg::new("unrelated", MANIFEST, CALL);
    pkg.write("engine.cbi", &interface_of("native"));
    let out = pkg.check();
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr.contains("run `colibri bindings`"),
        "{}",
        out.stderr
    );
    // And a package with no keys does not read it either.
    let plain = Pkg::new("plain", "", CALL);
    plain.write("engine.cbi", &interface_of("native"));
    assert_eq!(plain.check().code, 1);
}

#[test]
fn an_interface_for_another_module_is_refused_under_the_key() {
    let pkg = Pkg::new("renamed", MANIFEST, CALL);
    pkg.write("native.cbi", &interface_of("other"));
    let out = pkg.check();
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("module = \"native\""), "{}", out.stderr);
}

#[test]
fn a_key_with_a_standard_module_name_is_refused() {
    let pkg = Pkg::new(
        "std_name",
        "\n[rust-dependencies]\nmath = { path = \"native/\" }\n",
        "import math\nfn main() {}\n",
    );
    pkg.write("math.cbi", &interface_of("math"));
    let out = pkg.check();
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains(
            "the Rust dependency `math` has the name of the standard library module `std.math`"
        ),
        "{}",
        out.stderr
    );
}

#[test]
fn manifest_errors_are_reported_by_check() {
    let pkg = Pkg::new(
        "bad_manifest",
        "\n[rust-dependencies]\nnative = { binding = \"colibri-native\" }\n",
        CALL,
    );
    let out = pkg.check();
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("`binding` but no `version`"),
        "{}",
        out.stderr
    );
}

// ---- bindings declared by a dependency ----

const LIB_MAIN: &str = "import lib\nfn main() {\n    print(lib.total())\n}\n";
const LIB_SRC: &str = "import native\npub fn total() -> int {\n    return native.add(1, 2)\n}\n";

/// A root with a path dependency `lib` whose manifest has `lib_extra` after its `[package]`.
fn with_lib(name: &str, root_extra: &str, lib_extra: &str) -> Pkg {
    let pkg = Pkg::new(
        name,
        &format!("{root_extra}\n[dependencies]\nlib = {{ path = \"lib\" }}\n"),
        LIB_MAIN,
    );
    pkg.write(
        "lib/colibri.toml",
        &format!("[package]\nname = \"lib\"\n{lib_extra}"),
    );
    pkg.write("lib/lib.colibri", LIB_SRC);
    pkg
}

const LIB_NATIVE: &str = "\n[rust-dependencies]\nnative = { path = \"native/\" }\n";

#[test]
fn a_path_dependency_may_declare_and_use_a_binding_without_allow_rust() {
    let pkg = with_lib("dep_ok", "", LIB_NATIVE);
    pkg.write("lib/native.cbi", &interface_of("native"));
    let out = pkg.check();
    assert_eq!(out.code, 0, "{}", out.stderr);
}

#[test]
fn a_dependency_binding_without_its_interface_is_reported_at_its_key() {
    let pkg = with_lib("dep_missing", "", LIB_NATIVE);
    let out = pkg.check();
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("`native` is a Rust binding: run `colibri bindings`"),
        "{}",
        out.stderr
    );
    // The snippet is the line of lib's manifest, which the root's manifest does not have.
    assert!(out.stderr.contains("native = { path"), "{}", out.stderr);
    // One error: the import of the module without an interface adds nothing to it.
    assert_eq!(out.stderr.matches("Error:").count(), 1, "{}", out.stderr);
}

#[test]
fn a_binding_key_is_one_module_in_the_whole_program() {
    // The root and `lib` both say `native`: the second key is the error.
    let pkg = with_lib("dep_twice", MANIFEST, LIB_NATIVE);
    pkg.write("native.cbi", &interface_of("native"));
    pkg.write("lib/native.cbi", &interface_of("native"));
    let out = pkg.check();
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("`native` is already declared by another package"),
        "{}",
        out.stderr
    );
}

#[test]
fn one_crate_under_two_keys_in_two_packages_is_refused() {
    let pkg = with_lib(
        "dep_same_crate",
        "\n[rust-dependencies]\nfirst = { path = \"native\" }\n",
        "\n[rust-dependencies]\nnative = { path = \"../native\" }\n",
    );
    pkg.write("first.cbi", &interface_of("first"));
    pkg.write("lib/native.cbi", &interface_of("native"));
    let out = pkg.check();
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("is the same crate as `first`"),
        "{}",
        out.stderr
    );
}

#[test]
fn only_the_declaring_package_may_import_a_binding() {
    // `lib` declares `native`; the root and a sibling `other` import it without declaring it.
    let pkg = with_lib("visibility", "", LIB_NATIVE);
    pkg.write("lib/native.cbi", &interface_of("native"));
    pkg.write("main.colibri", &format!("import native\n{LIB_MAIN}"));
    let out = pkg.check();
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr.contains(
            "`native` is a Rust binding declared by the package `lib`, not by this package"
        ),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("declares it under `[rust-dependencies]`"),
        "{}",
        out.stderr
    );
    assert_eq!(out.stderr.matches("Error:").count(), 1, "{}", out.stderr);

    let pkg = with_lib("visibility_sibling", "", LIB_NATIVE);
    pkg.write("lib/native.cbi", &interface_of("native"));
    pkg.write(
        "colibri.toml",
        "[package]\nname = \"app\"\n[dependencies]\nlib = { path = \"lib\" }\nother = { path = \"other\" }\n",
    );
    pkg.write("other/colibri.toml", "[package]\nname = \"other\"\n");
    pkg.write(
        "other/lib.colibri",
        "import native\npub fn two() -> int {\n    return native.add(1, 1)\n}\n",
    );
    pkg.write(
        "main.colibri",
        "import lib\nimport other\nfn main() {\n    print(lib.total() + other.two())\n}\n",
    );
    let out = pkg.check();
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr.contains("declared by the package `lib`"),
        "{}",
        out.stderr
    );
}

#[test]
fn two_packages_each_with_their_own_native_folder_are_two_crates() {
    let pkg = with_lib(
        "two_folders",
        "\n[rust-dependencies]\nfirst = { path = \"native\" }\n",
        "\n[rust-dependencies]\nnative = { path = \"native\" }\n",
    );
    pkg.write("first.cbi", &interface_of("first"));
    pkg.write("lib/native.cbi", &interface_of("native"));
    let out = pkg.check();
    assert_eq!(out.code, 0, "{}", out.stderr);
}

#[test]
fn a_malformed_interface_is_reported_at_its_key() {
    let pkg = Pkg::new("malformed", MANIFEST, CALL);
    pkg.write("native.cbi", "this is not an interface");
    let out = pkg.check();
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("`native.cbi` is not a valid interface"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("native = { path"), "{}", out.stderr);
}

struct Json(i64);
impl colibri_runtime::HostType for Json {
    const NAME: &'static str = "Value";
    fn describe(d: &mut colibri_runtime::TypeBuilder<'_, Self>) {
        d.method("int", [], |this: &Json| this.0);
    }
}

#[test]
fn an_interface_that_uses_a_missing_module_is_reported_at_its_key() {
    let mut vm = colibri_runtime::Vm::builder().build();
    vm.module("json2").register_type::<Json>().unwrap();
    let mut m = vm.module("native");
    m.uses("json2").unwrap();
    m.register_fn_named("parse", ["text"], |t: String| Json(t.len() as i64))
        .unwrap();
    let text = vm.module("native").interface_text();
    let pkg = Pkg::new("uses_missing", MANIFEST, CALL);
    pkg.write("native.cbi", &text);
    let out = pkg.check();
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("names types of the module `json2`, which is not available"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("native = { path"), "{}", out.stderr);
}

#[test]
fn run_agrees_with_check_about_a_local_file_named_like_the_key() {
    let pkg = Pkg::new("ambiguous", MANIFEST, CALL);
    pkg.write("native.cbi", &interface_of("native"));
    pkg.write(
        "native.colibri",
        "pub fn add(a: int, b: int) -> int {\n    return a - b\n}\n",
    );
    for out in [pkg.check(), pkg.run()] {
        assert_eq!(out.code, 1, "{}", out.stderr);
        assert!(
            out.stderr.contains("`import native` is ambiguous"),
            "{}",
            out.stderr
        );
    }
}

#[test]
fn run_says_a_binding_needs_the_project_runner() {
    let pkg = Pkg::new("run_needs_runner", MANIFEST, CALL);
    pkg.write("native.cbi", &interface_of("native"));
    let out = pkg.run();
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(
        out.stderr.contains(
            "`native` is a Rust binding: a program that uses it runs in the project runner"
        ),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains("looked for"), "{}", out.stderr);
}
