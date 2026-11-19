//! `colibri new <name>`: creates a package folder that checks, runs and passes its test.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

/// What `main.colibri` of a new package says: a function with a test, and `main` using it.
const MAIN: &str = "\
fn greeting(who: str) -> str => \"Hello, {who}!\"

fn main() {
    print(greeting(\"world\"))
}

test \"greeting names who it greets\" {
    assert_eq(greeting(\"Colibri\"), \"Hello, Colibri!\")
}
";

/// Creates the package in `dir`; its name is the last part of the path. `Err` is a message
/// for the user.
pub fn create(dir: &Path) -> Result<(), String> {
    let name = dir.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        format!(
            "`{}` has no folder name to use as the package name",
            dir.display()
        )
    })?;
    if !colibri_runtime::is_package_name(name) {
        return Err(format!(
            "`{name}` is not a valid package name: use letters, digits and `_`, not starting with a digit (it is what `import` says)"
        ));
    }
    if dir.exists() {
        let non_empty = match fs::read_dir(dir) {
            Ok(mut entries) => entries.next().is_some(),
            Err(_) => true,
        };
        if !dir.is_dir() || non_empty {
            return Err(format!(
                "`{}` already exists and is not an empty folder; nothing was changed",
                dir.display()
            ));
        }
    }
    let write = |path: std::path::PathBuf, text: &str| {
        fs::write(&path, text).map_err(|e| format!("cannot write `{}`: {e}", path.display()))
    };
    fs::create_dir_all(dir).map_err(|e| format!("cannot create `{}`: {e}", dir.display()))?;
    write(
        dir.join("colibri.toml"),
        &format!("[package]\nname = \"{name}\"\n"),
    )?;
    write(dir.join("main.colibri"), MAIN)
}

/// Runs the command; the exit code is 1 when the package could not be created.
pub fn run(dir: &Path) -> ExitCode {
    match create(dir) {
        Ok(()) => {
            println!("Created the package in `{}`.", dir.display());
            println!("Try:  cd {} && colibri run && colibri test", dir.display());
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(1)
        }
    }
}
