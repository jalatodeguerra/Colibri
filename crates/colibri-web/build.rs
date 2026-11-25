//! The wasm shadow stack. The default of 1 MiB is small for this engine: parsing and checking
//! recurse on it (a deeply nested expression), and so do the deep operations on values (`show`,
//! `==`, `clone`) and a host function that calls back into Colibri. On the web a stack overflow
//! is not a panic that can be reported: it traps and ends the instance, so the stack is
//! made large and `runner::NATIVE_STACK_BUDGET` (below its size) makes the engine report "stack
//! overflow" as an ordinary panic before it gets there.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_FAMILY").is_ok_and(|f| f == "wasm") {
        // Keep in step with `runner::STACK_SIZE`.
        println!("cargo:rustc-link-arg-cdylib=-zstack-size=16777216");
    }
}
