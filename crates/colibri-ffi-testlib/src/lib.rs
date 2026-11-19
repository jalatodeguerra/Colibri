//! The path of the C test library, built by `build.rs`. See `c/testlib.c` for what it exports.

/// The shared library the FFI tests load.
pub const PATH: &str = env!("COLIBRI_TESTLIB");
