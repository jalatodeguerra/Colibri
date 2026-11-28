//! Embeds the web runner (`colibri.wasm`) in `colibri`, if the build says where it is.
//!
//! `COLIBRI_WEB_RUNNER` is the absolute path of the runner the release pipeline built from
//! `crates/colibri-web` and compressed with `gzip -9n` (a raw `.wasm` works too, but it is 3.6 MB
//! and the tool's size gate is 15 MB: see `colibri build --web` in the README). Without it the cfg
//! `web_runner` is not set, and `colibri build --web` says that this `colibri` was built without
//! the runner. A path that is not there, or a file that is neither gzip nor wasm, fails the build:
//! a release that silently lacks its runner is worse than one that does not build.

use std::path::Path;

/// The commit this `colibri` is built from, for a project runner that cannot find the source tree
/// (`bindings::engine`): `git rev-parse HEAD`, if this is a git checkout and git is there.
fn git_commit() {
    println!("cargo:rerun-if-env-changed=COLIBRI_GIT_COMMIT");
    if let Some(commit) = std::env::var_os("COLIBRI_GIT_COMMIT").filter(|v| !v.is_empty()) {
        println!(
            "cargo:rustc-env=COLIBRI_GIT_COMMIT={}",
            commit.to_string_lossy()
        );
        return;
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // The commit changes with `HEAD` and with the branch it names.
    for what in ["HEAD", "logs/HEAD"] {
        if let Some(path) = git(&["rev-parse", "--git-path", what]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(commit) = git(&["rev-parse", "HEAD"]).filter(|c| c.len() == 40) {
        println!("cargo:rustc-env=COLIBRI_GIT_COMMIT={commit}");
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    git_commit();
    // The platform of this `colibri`: part of what a project runner's cache key is made of.
    println!(
        "cargo:rustc-env=COLIBRI_HOST_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
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
