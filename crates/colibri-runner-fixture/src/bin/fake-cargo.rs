//! A stand-in for `cargo build` (see `colibri_lang::bindings::build`). It is driven by variables
//! the test sets on the cargo process:
//!
//! - `FAKE_CARGO_LOG`: a file to append one line of arguments to;
//! - `FAKE_CARGO_RUNNER`: the runner to copy to `<target-dir>/<profile>/colibri-project-runner`;
//! - `FAKE_CARGO_SLEEP_MS`: wait first (to overlap two builds);
//! - `FAKE_CARGO_FAIL`: `build` (a compile error) or `network` (a download failure);
//! - `FAKE_CARGO_DUP_RUNTIME`: resolve a lock with two `colibri-runtime` packages.
//!
//! It also writes the lock a build would: the one it was given plus a marker line, once.

use std::io::Write;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(log) = std::env::var_os("FAKE_CARGO_LOG") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .expect("the log opens");
        writeln!(file, "{}", args.join(" ")).expect("the log is written");
    }
    if let Some(ms) = std::env::var("FAKE_CARGO_SLEEP_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    let after = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| panic!("fake cargo: no {flag}"))
    };
    match std::env::var("FAKE_CARGO_FAIL").as_deref() {
        Ok("build") => {
            eprintln!("error: could not compile `colibri-project-runner` (bin)");
            std::process::exit(101);
        }
        Ok("network") => {
            eprintln!("error: failed to download `unicode-width v0.2.0`");
            std::process::exit(101);
        }
        _ => {}
    }
    let manifest = PathBuf::from(after("--manifest-path"));
    let lock = manifest.with_file_name("Cargo.lock");
    let mut text = std::fs::read_to_string(&lock).unwrap_or_default();
    if !text.contains("# resolved by fake cargo") {
        text.push_str("\n# resolved by fake cargo\n");
    }
    if std::env::var_os("FAKE_CARGO_DUP_RUNTIME").is_some() && !text.contains("colibri-runtime") {
        for v in ["0.1.0", "0.1.1"] {
            text.push_str(&format!(
                "\n[[package]]\nname = \"colibri-runtime\"\nversion = \"{v}\"\n"
            ));
        }
    }
    std::fs::write(&lock, text).expect("the lock is written");

    let out_dir = PathBuf::from(after("--target-dir")).join(after("--profile"));
    std::fs::create_dir_all(&out_dir).expect("the target folder");
    let name = format!("{}{}", after("--bin"), std::env::consts::EXE_SUFFIX);
    let tmp = out_dir.join(format!("{name}.fake{}", std::process::id()));
    let runner = std::env::var_os("FAKE_CARGO_RUNNER").expect("FAKE_CARGO_RUNNER");
    std::fs::copy(runner, &tmp).expect("the runner is copied");
    std::fs::rename(&tmp, out_dir.join(name)).expect("the runner is put in place");
}
