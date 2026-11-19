//! Compiles `c/testlib.c` into a shared library at build time, with the C compiler the `cc`
//! crate finds (`CC`, `cc`, `cl`...), and tells the crate where it is.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=c/testlib.c");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target = std::env::var("TARGET").unwrap_or_default();
    let (file, windows) = if target.contains("windows") {
        ("colibri_testlib.dll", true)
    } else if target.contains("apple") {
        ("libcolibri_testlib.dylib", false)
    } else {
        ("libcolibri_testlib.so", false)
    };
    let lib = out_dir.join(file);
    let compiler = cc::Build::new()
        .file("c/testlib.c")
        .opt_level(1)
        .pic(true)
        .try_get_compiler()
        .expect("no C compiler found");
    let mut cmd = compiler.to_command();
    if windows && compiler.is_like_msvc() {
        cmd.arg("/LD")
            .arg("c/testlib.c")
            .arg(format!("/Fe:{}", lib.display()))
            .arg(format!("/Fo:{}\\", out_dir.display()));
    } else {
        cmd.arg(if target.contains("apple") {
            "-dynamiclib"
        } else {
            "-shared"
        })
        .arg("-fPIC")
        .arg("c/testlib.c")
        .arg("-o")
        .arg(&lib);
    }
    let status = cmd.status().expect("cannot run the C compiler");
    assert!(status.success(), "compiling c/testlib.c failed");
    println!("cargo:rustc-env=COLIBRI_TESTLIB={}", lib.display());
}
