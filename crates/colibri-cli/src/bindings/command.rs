//! `colibri bindings`: builds the project runner and writes each binding's interface (`<key>.cbi`)
//! next to the `colibri.toml` that declares the key.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use colibri_runtime::ProjectBinding;

use super::build::{self, Env, Options, Profile, Runner};
use crate::depscmd::{self, DepFlags};

/// What `colibri bindings` was asked to do.
pub struct Args<'a> {
    pub path: &'a Path,
    pub release: bool,
    /// Also copy the runner here.
    pub output: Option<&'a Path>,
    /// Write nothing; fail if an interface would change.
    pub check: bool,
    pub deps: DepFlags,
}

/// `colibri bindings`.
pub fn run(args: &Args<'_>) -> ExitCode {
    if let Err(code) = depscmd::prepare(args.path, args.deps) {
        return code;
    }
    let env = match Env::current() {
        Ok(env) => env,
        Err(e) => return fail(&e),
    };
    match bindings(&env, args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => fail(&e),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(1)
}

/// Builds the runner, then brings the interfaces up to date (or, with `check`, only looks).
/// `Ok(false)` is `--check` finding a stale interface.
pub fn bindings(env: &Env, args: &Args<'_>) -> Result<bool, String> {
    let (root, found) = build::project(args.path)?;
    let options = Options {
        profile: if args.release {
            Profile::Release
        } else {
            Profile::Run
        },
        locked: args.deps.locked,
        offline: args.deps.offline,
        write_lock: !args.check,
        write_interfaces: false,
        rebuilding: false,
    };
    // Nothing is written to the project before the build has succeeded.
    let runner = build::ensure_runner(env, &root, &found, options)?;
    if let Some(out) = args.output {
        copy_runner(&runner, out)?;
    }
    // Asked live: what this command says is what the runner says now, whatever was kept before.
    let report = write_interfaces_with(&runner, &found, args.check, true)?;
    for path in &report.changed {
        eprintln!(
            "{}: {}",
            path.display(),
            if args.check { "out of date" } else { "written" }
        );
    }
    // The cache of a git dependency is never written to: its author commits the interface.
    for path in &report.git_stale {
        eprintln!(
            "warning: {}: a git dependency's interface is out of date; its author must regenerate and commit it",
            path.display()
        );
    }
    if args.check && runner.lock_changed {
        eprintln!(
            "{}: missing or out of date (run `colibri bindings` to write it)",
            root.join(build::LOCK_FILE).display()
        );
    }
    Ok(!(args.check && (!report.changed.is_empty() || runner.lock_changed)))
}

/// What [`write_interfaces`] found.
#[derive(Debug, Default)]
pub struct Report {
    /// Interfaces of the project's own packages that were (or, with `check`, would be) written.
    pub changed: Vec<PathBuf>,
    /// Interfaces of git dependencies that differ: reported, never written.
    pub git_stale: Vec<PathBuf>,
}

/// Compares each binding's `<key>.cbi` with the one the runner writes and, unless `check`, puts the
/// runner's there (never inside a git checkout). The runner's are asked once and kept next to it.
#[doc(hidden)]
pub fn write_interfaces(
    runner: &Runner,
    found: &[ProjectBinding],
    check: bool,
) -> Result<Report, String> {
    write_interfaces_with(runner, found, check, false)
}

/// [`write_interfaces`]; with `live` the runner is asked now and the kept copy replaced (what
/// `colibri bindings` does), else a kept copy is believed. A kept copy that cannot be read is
/// dropped and the runner asked again.
#[doc(hidden)]
pub fn write_interfaces_with(
    runner: &Runner,
    found: &[ProjectBinding],
    check: bool,
    live: bool,
) -> Result<Report, String> {
    let mut asked = live;
    loop {
        let dir = live_interfaces(runner, asked)?;
        let mut texts = Vec::new();
        let mut damaged = None;
        for b in found {
            match std::fs::read_to_string(dir.join(format!("{}.cbi", b.dep.name))) {
                Ok(text) => texts.push(text),
                Err(e) => {
                    damaged = Some(format!(
                        "the runner wrote no interface for `{}`: {e}",
                        b.dep.name
                    ));
                    break;
                }
            }
        }
        if let Some(problem) = damaged {
            if asked {
                return Err(problem);
            }
            // The kept folder was damaged (or is another runner's): ask the runner itself.
            asked = true;
            continue;
        }
        let mut report = Report::default();
        for (b, text) in found.iter().zip(texts) {
            let same = std::fs::read_to_string(&b.interface).is_ok_and(|old| old == text);
            if same {
                continue;
            }
            if b.from_git {
                report.git_stale.push(b.interface.clone());
                continue;
            }
            if !check {
                build::write_if_changed(&b.interface, &text)?;
            }
            report.changed.push(b.interface.clone());
        }
        return Ok(report);
    }
}

/// The folder of the interfaces `runner` writes: kept from the first time it was asked (it is
/// made aside and moved into place, so a folder that is there is whole). With `fresh` the kept
/// folder is dropped and the runner asked again.
fn live_interfaces(runner: &Runner, fresh: bool) -> Result<PathBuf, String> {
    let dir = build::interfaces_dir(&runner.path);
    if fresh {
        let _ = std::fs::remove_dir_all(&dir);
    } else if dir.is_dir() {
        return Ok(dir);
    }
    let scratch = runner
        .path
        .with_file_name(format!("interfaces-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let asked = ask_runner(runner, &scratch);
    let moved = asked.and_then(|()| match std::fs::rename(&scratch, &dir) {
        Ok(()) => Ok(()),
        // Another process put it there first: the same text.
        Err(_) if dir.is_dir() => Ok(()),
        Err(e) => Err(format!("cannot keep the interfaces of the runner: {e}")),
    });
    let _ = std::fs::remove_dir_all(&scratch);
    moved.map(|()| dir)
}

fn ask_runner(runner: &Runner, scratch: &Path) -> Result<(), String> {
    // A binary that was just written can be "busy" while another thread's
    // fork still holds its write handle; that clears in milliseconds.
    let mut tries = 0;
    let status = loop {
        match Command::new(&runner.path)
            .arg("__write-interfaces")
            .arg(scratch)
            .status()
        {
            Ok(status) => break status,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 20 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                return Err(format!(
                    "cannot run the runner `{}`: {e}",
                    runner.path.display()
                ));
            }
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "the runner `{}` could not write the interfaces (it exited with {})",
            runner.path.display(),
            status
                .code()
                .map_or("a signal".to_string(), |c| c.to_string())
        ))
    }
}

fn copy_runner(runner: &Runner, out: &Path) -> Result<(), String> {
    let tmp = out.with_extension("tmp-colibri");
    std::fs::copy(&runner.path, &tmp)
        .map_err(|e| format!("cannot write `{}`: {e}", out.display()))?;
    std::fs::rename(&tmp, out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot write `{}`: {e}", out.display())
    })
}
