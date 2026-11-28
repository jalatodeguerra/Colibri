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
    };
    // Nothing is written to the project before the build has succeeded.
    let runner = build::ensure_runner(env, &root, &found, options)?;
    if let Some(out) = args.output {
        copy_runner(&runner, out)?;
    }
    let stale = write_interfaces(&runner, &found, args.check)?;
    for path in &stale {
        eprintln!(
            "{}: {}",
            path.display(),
            if args.check { "out of date" } else { "written" }
        );
    }
    Ok(!(args.check && !stale.is_empty()))
}

/// Asks the runner for every interface and puts each at its place. Returns the files that were
/// (or, with `check`, would be) changed.
fn write_interfaces(
    runner: &Runner,
    found: &[ProjectBinding],
    check: bool,
) -> Result<Vec<PathBuf>, String> {
    let scratch = runner
        .path
        .with_file_name(format!("interfaces-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let result = ask_runner(runner, &scratch).and_then(|()| {
        let mut changed = Vec::new();
        for b in found {
            let text = std::fs::read_to_string(scratch.join(format!("{}.cbi", b.dep.name)))
                .map_err(|e| format!("the runner wrote no interface for `{}`: {e}", b.dep.name))?;
            let same = std::fs::read_to_string(&b.interface).is_ok_and(|old| old == text);
            if same {
                continue;
            }
            if !check {
                build::write_if_changed(&b.interface, &text)?;
            }
            changed.push(b.interface.clone());
        }
        Ok(changed)
    });
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn ask_runner(runner: &Runner, scratch: &Path) -> Result<(), String> {
    let status = Command::new(&runner.path)
        .arg("__write-interfaces")
        .arg(scratch)
        .status()
        .map_err(|e| format!("cannot run the runner `{}`: {e}", runner.path.display()))?;
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
