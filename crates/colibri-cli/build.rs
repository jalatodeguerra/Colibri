//! Embeds the web runner (`colibri.wasm`) in `colibri`, if the build says where it is.
//!
//! `COLIBRI_WEB_RUNNER` is the absolute path of the runner the release pipeline built from
//! `crates/colibri-web` and compressed with `gzip -9n` (a raw `.wasm` works too, but it is 3.6 MB
//! and the tool's size gate is 15 MB: see `colibri build --web` in the README). Without it the cfg
//! `web_runner` is not set, and `colibri build --web` says that this `colibri` was built without
//! the runner. A path that is not there, or a file that is neither gzip nor wasm, fails the build:
//! a release that silently lacks its runner is worse than one that does not build.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=COLIBRI_WEB_RUNNER");
    println!("cargo:rustc-check-cfg=cfg(web_runner)");
    let Some(value) = std::env::var_os("COLIBRI_WEB_RUNNER").filter(|v| !v.is_empty()) else {
        return;
    };
    let path = Path::new(&value);
    assert!(
        path.is_absolute(),
        "COLIBRI_WEB_RUNNER must be an absolute path (a build script runs in the crate's folder): {}",
        path.display()
    );
    let head = std::fs::read(path).unwrap_or_else(|e| {
        panic!(
            "COLIBRI_WEB_RUNNER `{}` cannot be read: {e}",
            path.display()
        )
    });
    assert!(
        head.starts_with(&[0x1f, 0x8b]) || head.starts_with(b"\0asm"),
        "COLIBRI_WEB_RUNNER `{}` is neither gzip data nor a wasm module",
        path.display()
    );
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rustc-cfg=web_runner");
    println!("cargo:rustc-env=COLIBRI_WEB_RUNNER_FILE={}", path.display());
}
