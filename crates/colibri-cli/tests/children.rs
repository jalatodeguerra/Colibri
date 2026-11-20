//! Programs that `process.run`, `shell` and `status` are waiting for must not outlive the
//! `colibri` that started them, whichever way it ends.

// Unix only: the programs are started with `sh`, and "still running" is asked of the system
// (`/proc` on Linux, `ps` elsewhere). Windows has no `sh` to rely on.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// The program: three coroutines each run a program that writes its process number into a
/// file and then becomes a 60-second sleep; main waits (up to 20 s) for all three files, then
/// ends the program the way `ending` says.
fn program(dir: &str, ending: &str) -> String {
    format!(
        r#"import process
import fs
import os

fn by_run() {{
    let _ = process.run("sh", ["-c", "echo $$ > {dir}/run.pid; exec sleep 60"])
}}
fn by_shell() {{
    let _ = process.shell("echo $$ > {dir}/shell.pid; exec sleep 60")
}}
fn by_status() {{
    let _ = process.status("sh", ["-c", "echo $$ > {dir}/status.pid; exec sleep 60"])
}}

fn main() {{
    go by_run()
    go by_shell()
    go by_status()
    var tries = 0
    while tries < 2000 {{
        if fs.exists("{dir}/run.pid") && fs.exists("{dir}/shell.pid") && fs.exists("{dir}/status.pid") {{
            break
        }}
        wait(0.01)
        tries += 1
    }}
    {ending}
}}
"#
    )
}

/// Whether process `pid` still runs (a zombie that nobody collected yet does not).
#[cfg(target_os = "linux")]
fn alive(pid: &str) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => false,
        Ok(stat) => {
            let after_name = stat.rsplit(')').next().unwrap_or("");
            !after_name.trim_start().starts_with('Z')
        }
    }
}

/// The same question on a Unix without `/proc` (macOS): `ps -o stat= -p PID` prints the state
/// (`S`, `R`, `Z` for a zombie...) and nothing for a process that is gone.
#[cfg(not(target_os = "linux"))]
fn alive(pid: &str) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .expect("`ps` runs");
    let state = String::from_utf8_lossy(&out.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

fn check_no_child_is_left(name: &str, ending: &str, code: i32) {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("colibri-children-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.colibri");
    fs::write(&file, program(dir.to_str().unwrap(), ending)).unwrap();
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_colibri"))
        .args(["run", file.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(started.elapsed() < Duration::from_secs(30));
    for name in ["run", "shell", "status"] {
        let pid = fs::read_to_string(dir.join(format!("{name}.pid")))
            .unwrap_or_else(|_| panic!("the {name} child never started"))
            .trim()
            .to_string();
        assert!(
            !alive(&pid),
            "the child of `{name}` (pid {pid}) is still running"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_panic_leaves_no_child_behind() {
    check_no_child_is_left("panic", "panic(\"stop\")", 101);
}

#[test]
fn os_exit_leaves_no_child_behind() {
    check_no_child_is_left("exit", "os.exit(3)", 3);
}
