//! Handing a command over to the project runner. A package with `[rust-dependencies]` runs in a
//! `colibri` that has those bindings built in: `run`, `test` and `repl` find (or build) it with
//! [`crate::bindings::build::ensure_runner`] and give it the command line unchanged. A runner
//! never hands over again: it sets [`GUARD`] for itself and what it starts.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::bindings::build::{self, Env, Options, Profile};
use crate::bindings::r#gen::MARKER_PREFIX;
use crate::depscmd::{self, DepFlags};
use crate::platform;

/// Set (to `1`) in the environment of a project runner: a `colibri` that sees it runs here.
pub const GUARD: &str = "COLIBRI_PROJECT_RUNNER";

/// What a command that may be handed over knows of itself.
pub struct Request<'a> {
    /// The script or folder the command works on (`.` for `repl`).
    pub file: &'a Path,
    /// `--runner FILE`.
    pub runner: Option<&'a Path>,
    pub deps: DepFlags,
    /// `run --watch`: the runner is kept running by this process, which also looks at the Rust
    /// code (see [`crate::supervise`]).
    pub watch: bool,
}

/// `Some(code)` when the project runner ran the command (or could not be had: the error is
/// already printed), `None` when it is this `colibri`'s to run: the command is in a runner, has
/// no `colibri.toml` beside it, or the package has no Rust bindings.
pub fn delegate(request: &Request<'_>) -> Option<ExitCode> {
    if std::env::var_os(GUARD).is_some() {
        return None;
    }
    let folder = package_folder(request.file);
    // A plain script, and a package without Rust bindings, are left alone: nothing is read but the
    // manifest, and the cache is not touched.
    let wants = manifest_wants_bindings(&folder.join(colibri_runtime::MANIFEST_FILE));
    if wants == Wants::No && request.runner.is_none() {
        return None;
    }
    if !folder.join(colibri_runtime::MANIFEST_FILE).is_file() {
        if let Some(given) = request.runner {
            return Some(fail(&format!(
                "`--runner {}`: `{}` has no `colibri.toml`, so no Rust bindings to run with it",
                given.display(),
                folder.display()
            )));
        }
        return None;
    }
    match hand_over(request, &folder, wants) {
        Ok(code) => code,
        Err(message) => Some(fail(&message)),
    }
}

/// The folder of the package of a script (or the folder given), as the loader sees it.
pub(crate) fn package_folder(file: &Path) -> PathBuf {
    if file.is_dir() {
        file.to_path_buf()
    } else {
        file.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }
}

/// What the manifest of a package says about Rust bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wants {
    /// Neither `[rust-dependencies]` nor `[dependencies]` (or no manifest that can be read: the
    /// command reports it as it always did).
    No,
    /// Dependencies, one of which may declare bindings.
    Maybe,
    /// `[rust-dependencies]`.
    Yes,
}

pub(crate) fn manifest_wants_bindings(manifest: &Path) -> Wants {
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return Wants::No;
    };
    let Ok(table) = toml::de::DeTable::parse(&text) else {
        return Wants::No;
    };
    let table = table.get_ref();
    let filled = |key: &str| {
        table
            .get(key)
            .is_some_and(|v| v.get_ref().as_table().is_some_and(|t| !t.is_empty()))
    };
    if filled("rust-dependencies") {
        Wants::Yes
    } else if filled("dependencies") {
        Wants::Maybe
    } else {
        Wants::No
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(1)
}

fn hand_over(
    request: &Request<'_>,
    folder: &Path,
    wants: Wants,
) -> Result<Option<ExitCode>, String> {
    // Once: the command that runs here does not prepare again.
    if let Err(code) = depscmd::prepare_once(request.file, request.deps) {
        return Ok(Some(code));
    }
    // A package that only has dependencies is asked from the manifests alone (no source is read):
    // most of them have no bindings, and for them the command must start as fast as ever.
    if wants == Wants::Maybe
        && request.runner.is_none()
        && !colibri_runtime::declares_rust_bindings(
            &colibri_runtime::DiskFs,
            folder,
            crate::build_cfg(),
        )?
    {
        return Ok(None);
    }
    let (root, found) = build::project(folder)?;
    let runner = match request.runner {
        Some(given) => {
            if found.is_empty() {
                return Err(format!(
                    "`--runner {}`: this project declares no `[rust-dependencies]`, so it has no runner",
                    given.display()
                ));
            }
            check_runner(given, &found)?;
            given.to_path_buf()
        }
        None => {
            if found.is_empty() {
                return Ok(None);
            }
            let env = Env::current()?;
            let options = Options {
                profile: Profile::Run,
                locked: request.deps.locked,
                offline: request.deps.offline,
                write_lock: true,
                write_interfaces: true,
                rebuilding: false,
            };
            let runner = build::ensure_runner(&env, &root, &found, options)?;
            say_interfaces(&runner);
            if request.watch {
                return Ok(Some(crate::supervise::supervise(
                    crate::supervise::Project {
                        env,
                        folder: folder.to_path_buf(),
                        root,
                        found,
                        options,
                    },
                    runner.path,
                )));
            }
            runner.path
        }
    };
    Ok(Some(run_runner(&runner)))
}

/// What the runner says of the project's interfaces: the ones it wrote after a build (one line
/// each), and one warning for those that differ and were not written.
pub fn say_interfaces(runner: &build::Runner) {
    for path in &runner.written {
        eprintln!("updated {} (the Rust bindings changed)", path.display());
    }
    if !runner.stale.is_empty() {
        let names: Vec<String> = runner
            .stale
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        eprintln!(
            "warning: the interface of the Rust bindings is out of date: {}; run `colibri bindings` to update it",
            names.join(", ")
        );
    }
    for (key, path) in &runner.git_stale {
        eprintln!(
            "warning: the interface of the git dependency `{key}` is out of date: {}; `colibri build` refuses it until you `colibri update` it to a revision whose author regenerated it, or depend on a local copy with `path =`",
            path.display()
        );
    }
    if let Some(problem) = &runner.interface_problem {
        eprintln!("warning: cannot compare the interfaces of the Rust bindings: {problem}");
    }
}

/// Refuses a runner that is not a `colibri` of this version holding exactly the bindings of the
/// package, saying what is missing.
fn check_runner(runner: &Path, found: &[colibri_runtime::ProjectBinding]) -> Result<(), String> {
    use std::io::Seek;
    let shown = runner.display();
    let mut file = std::fs::File::open(runner)
        .map_err(|e| format!("cannot read the runner `{shown}`: {e}"))?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err(format!("the runner `{shown}` is not a file"));
    }
    let read = |e: std::io::Error| format!("cannot read the runner `{shown}`: {e}");
    // A program made by `colibri build` is a runner with a pack after it: it would run its own
    // program, not the command's. (`debug --runner` uses this check too.)
    if crate::pack::has_pack(&mut file)? {
        return Err(format!("the runner `{shown}` is a built program already"));
    }
    file.rewind().map_err(read)?;
    match platform::runner_version(&mut file).map_err(read)? {
        Some(v) if v == platform::own_version() => {}
        Some(v) => {
            return Err(format!(
                "the runner `{shown}` is `colibri` {v}, and this is {}: use a runner of the same version",
                platform::own_version()
            ));
        }
        None => {
            return Err(format!(
                "the runner `{shown}` does not say which `colibri` version it is: it is not a `colibri` of this version"
            ));
        }
    }
    file.rewind().map_err(read)?;
    check_bindings(&mut file, runner, found)
}

/// Refuses a runner whose bindings marker is not the one of `found` (the file is at its start).
pub(crate) fn check_bindings(
    file: &mut std::fs::File,
    runner: &Path,
    found: &[colibri_runtime::ProjectBinding],
) -> Result<(), String> {
    let shown = runner.display();
    let read = |e: std::io::Error| format!("cannot read the runner `{shown}`: {e}");
    let (expected, keys) = build::expected_marker(found)?;
    let expected = expected
        .strip_prefix(MARKER_PREFIX)
        .and_then(|m| m.strip_suffix('\u{2}'))
        .unwrap_or_default()
        .to_string();
    let needs = format!("the bindings of this package ({})", keys.join(", "));
    match platform::marker_after(file, MARKER_PREFIX).map_err(read)? {
        Some(have) if have == expected => Ok(()),
        Some(_) => Err(format!(
            "the runner `{shown}` holds other Rust bindings than this package needs: {needs}; build one with `colibri bindings -o FILE`"
        )),
        None => Err(format!(
            "the runner `{shown}` is a plain `colibri`, with none of {needs} built in; build a runner with `colibri bindings -o FILE`"
        )),
    }
}

fn run_runner(runner: &Path) -> ExitCode {
    let mut command = Command::new(runner);
    // The command line as it was given: a runner parses it as `colibri` does.
    command.args(std::env::args_os().skip(1)).env(GUARD, "1");
    hand_over_process(command, runner)
}

#[cfg(unix)]
fn hand_over_process(mut command: Command, runner: &Path) -> ExitCode {
    use std::os::unix::process::CommandExt;
    // The runner takes this process's place: same pid, same terminal, same exit status.
    let e = command.exec();
    fail(&format!(
        "cannot run the runner `{}`: {e}",
        runner.display()
    ))
}

/// Windows has no `exec`: the runner is a child, and this process is only a wait. Ctrl-C reaches
/// both (they share the console); this one ignores it, so the runner alone decides what it means,
/// and its exit code is returned unchanged.
#[cfg(windows)]
fn hand_over_process(mut command: Command, runner: &Path) -> ExitCode {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };
    unsafe extern "system" fn ignore(kind: u32) -> i32 {
        i32::from(kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT)
    }
    // SAFETY: the routine only reads its argument.
    unsafe {
        SetConsoleCtrlHandler(Some(ignore), 1);
    }
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => fail(&format!(
            "cannot run the runner `{}`: {e}",
            runner.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_manifest_with_dependencies_asks_for_the_runner() {
        let dir = std::env::temp_dir().join(format!("colibri-delegate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wants = |text: &str| {
            let p = dir.join("colibri.toml");
            std::fs::write(&p, text).unwrap();
            manifest_wants_bindings(&p)
        };
        assert_eq!(wants("[package]\nname = \"a\"\n"), Wants::No);
        assert_eq!(
            wants("[package]\nname = \"a\"\n\n[dependencies]\n"),
            Wants::No
        );
        assert_eq!(
            wants("[package]\nname = \"a\"\n\n[rust-dependencies]\nx = { path = \"x\" }\n"),
            Wants::Yes
        );
        assert_eq!(
            wants("[package]\nname = \"a\"\n\n[dependencies]\nb = { path = \"b\" }\n"),
            Wants::Maybe
        );
        assert_eq!(wants("not = toml ="), Wants::No);
        assert_eq!(
            manifest_wants_bindings(&dir.join("missing.toml")),
            Wants::No
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
