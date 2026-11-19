//! Without the `ffi` feature a program with an `extern` block checks but does not load, and the
//! error says why (EMBEDDING §5). Run with `cargo test -p colibri-runtime --no-default-features`.

#![cfg(not(feature = "ffi"))]

use colibri_runtime as colibri;

#[test]
fn extern_blocks_give_a_clear_error_in_a_build_without_ffi() {
    let mut vm = colibri::Vm::builder().allow(colibri::Cap::Ffi).build();
    let e = vm
        .load_source(
            "t.colibri",
            "extern \"C\" { fn abs(x: i32) -> i32 }\nfn main() {}\n",
        )
        .unwrap_err();
    assert!(matches!(e, colibri::Error::Ffi(_)), "{e}");
    assert!(
        e.to_string().contains("this build of Colibri has no C FFI"),
        "{e}"
    );
    // The Vm stays usable.
    vm.load_source("t.colibri", "fn main() { print(1) }\n")
        .unwrap();
}
