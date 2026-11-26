//! Git dependencies on the command line: `colibri update`, and the step that `run`, `check` and
//! `test` take before loading a program (ARCHITECTURE §11.6b).
//!
//! The loader never uses the network. These commands do, once, here: they resolve every git
//! dependency, fill the cache and write `colibri.lock`; the loader then reads only the lock
//! and the cache.

use std::path::Path;
use std::process::ExitCode;

use clap::Args;
use colibri_runtime::{SyncOptions, Update, sync_dependencies};

/// The flags every command that loads a package takes.
#[derive(Args, Clone, Copy, Debug, Default)]
pub struct DepFlags {
    /// Fail if `colibri.lock` is missing or would change, instead of writing it
    #[arg(long)]
    pub locked: bool,
    /// Never use the network: git dependencies must already be in the cache
    #[arg(long)]
    pub offline: bool,
}

/// Makes `colibri.lock` and the cache match the manifest of the package `target` is in (a
/// package folder, or a script next to a `colibri.toml`), printing what changed to stderr.
/// `Err` is the exit code to stop with.
pub fn prepare(target: &Path, flags: DepFlags) -> Result<(), ExitCode> {
    refuse_package_file(target)?;
    sync(target, flags, Update::Nothing)
}

/// A `colibri.toml` or `colibri.lock` is not a script (`check`, `run` and `test` take scripts
/// and package folders): says so, naming the file, and never reads it. `Err` is the exit
/// code, 1 as for a file that cannot be read.
pub fn refuse_package_file(target: &Path) -> Result<(), ExitCode> {
    if !target.is_dir()
        && let Some(message) = colibri_runtime::not_a_script(target)
    {
        eprintln!("error: {}: {message}", target.display());
        return Err(ExitCode::from(1));
    }
    Ok(())
}

/// [`prepare`] for a caller that has no terminal (the debug adapter): the notes are returned,
/// and a failure is the message that `prepare` would have printed.
pub fn prepare_quiet(target: &Path, flags: DepFlags) -> Result<Vec<String>, String> {
    if !target.is_dir()
        && let Some(message) = colibri_runtime::not_a_script(target)
    {
        return Err(format!("{}: {message}", target.display()));
    }
    let root = if target.is_dir() {
        target
    } else {
        target.parent().unwrap_or(Path::new(""))
    };
    let options = SyncOptions {
        update: Update::Nothing,
        locked: flags.locked,
        offline: flags.offline,
    };
    sync_dependencies(root, &options).map(|report| report.notes)
}

fn sync(target: &Path, flags: DepFlags, update: Update) -> Result<(), ExitCode> {
    let root = if target.is_dir() {
        target
    } else {
        target.parent().unwrap_or(Path::new(""))
    };
    let options = SyncOptions {
        update,
        locked: flags.locked,
        offline: flags.offline,
    };
    match sync_dependencies(root, &options) {
        Ok(report) => {
            for note in &report.notes {
                eprintln!("  {note}");
            }
            Ok(())
        }
        Err(message) => {
            eprintln!("error: {message}");
            Err(ExitCode::from(1))
        }
    }
}

/// `colibri update [name..]`: resolves branches, tags and default branches again (all
/// dependencies, or the named ones) and writes the new commits to `colibri.lock`.
pub fn update(dir: &Path, names: Vec<String>, flags: DepFlags) -> ExitCode {
    // `--path` wants the folder of the package; the manifest itself is a natural mistake.
    if !dir.is_dir()
        && let Some(message) = colibri_runtime::not_a_script(dir)
    {
        eprintln!("error: {}: {message}", dir.display());
        return ExitCode::from(1);
    }
    if !dir.join(colibri_runtime::MANIFEST_FILE).is_file() {
        eprintln!(
            "error: there is no `{}` in `{}`",
            colibri_runtime::MANIFEST_FILE,
            if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            }
            .display()
        );
        return ExitCode::from(1);
    }
    let update = if names.is_empty() {
        Update::All
    } else {
        Update::Names(names)
    };
    match sync(dir, flags, update) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}
