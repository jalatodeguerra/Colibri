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
        fs::write(self.0.join(rel), text).unwrap();
    }

    fn check(&self) -> Out {
        // An empty `PATH` and a dead `CARGO`: a check that tried to run cargo would fail.
        let out = Command::new(env!("CARGO_BIN_EXE_colibri"))
            .args(["check", self.0.to_str().unwrap()])
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
