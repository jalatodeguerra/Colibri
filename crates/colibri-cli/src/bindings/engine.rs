//! Where a project runner gets the engine crates from. Colibri is not on a registry, so the
//! generated workspace names a source tree or a git commit for `colibri-lang` and patches
//! `colibri-runtime` (which binding crates name as `"0.1"`) to the same place.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use toml::de::{DeTable, DeValue};

/// The environment variable that names a Colibri source tree.
pub const SOURCE_VAR: &str = "COLIBRI_SOURCE";

/// A place the engine crates can be built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Engine {
    /// A source tree: the folder that holds the workspace `Cargo.toml` and `crates/`.
    Path(PathBuf),
    /// A commit of the repository.
    Git { url: String, rev: String },
}

/// What [`resolve`] looks at, so that each case can be tested without the machine's own state.
pub struct Inputs {
    /// `COLIBRI_SOURCE`, if set.
    pub env: Option<OsString>,
    /// The source tree this `colibri` was compiled from.
    pub recorded: Option<PathBuf>,
    /// The version of this `colibri`.
    pub version: String,
    /// The repository url and the commit this `colibri` was compiled from, if known.
    pub git: Option<(String, String)>,
}

impl Inputs {
    /// The facts of this process.
    pub fn current() -> Inputs {
        let recorded = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let git = option_env!("COLIBRI_GIT_COMMIT")
            .filter(|rev| !rev.is_empty())
            .map(|rev| (env!("CARGO_PKG_REPOSITORY").to_string(), rev.to_string()));
        Inputs {
            env: std::env::var_os(SOURCE_VAR).filter(|v| !v.is_empty()),
            recorded,
            version: env!("CARGO_PKG_VERSION").to_string(),
            git,
        }
    }
}

/// Chooses the engine source: `COLIBRI_SOURCE`, else the tree this `colibri` came from if it is
/// still there with the same version, else the git repository at the exact commit, else an error
/// that says what to set.
pub fn resolve(inputs: &Inputs) -> Result<Engine, String> {
    if let Some(value) = &inputs.env {
        // Absolute: the generated manifest is read from another folder.
        let given = PathBuf::from(value);
        let dir = std::fs::canonicalize(&given)
            .map(|p| PathBuf::from(super::r#gen::strip_verbatim(&p.to_string_lossy())))
            .unwrap_or_else(|_| given.clone());
        return match tree_version(&dir) {
            Some(_) => Ok(Engine::Path(dir)),
            None => Err(format!(
                "{SOURCE_VAR} is `{}`, which is not a Colibri source tree (it needs a `Cargo.toml` that lists the workspace and `crates/colibri-cli` and `crates/colibri-runtime`)",
                given.display()
            )),
        };
    }
    let mut why = String::new();
    match &inputs.recorded {
        Some(dir) => match tree_version(dir) {
            Some(version) if version == inputs.version => {
                return Ok(Engine::Path(dir.clone()));
            }
            Some(version) => why.push_str(&format!(
                "\n  the source this `colibri` was built from, `{}`, is now version {version}, not {}",
                dir.display(),
                inputs.version
            )),
            None => why.push_str(&format!(
                "\n  the source this `colibri` was built from, `{}`, is gone",
                dir.display()
            )),
        },
        None => why.push_str("\n  this `colibri` does not know where it was built from"),
    }
    if let Some((url, rev)) = &inputs.git {
        return Ok(Engine::Git {
            url: url.clone(),
            rev: rev.clone(),
        });
    }
    why.push_str(
        "\n  this `colibri` was not built from a git checkout, so there is no commit to fetch",
    );
    Err(format!(
        "cannot find the Colibri source to build the project runner from:{why}\nset {SOURCE_VAR} to a checkout of Colibri {}",
        inputs.version
    ))
}

/// The version of the Colibri workspace in `dir`, if `dir` is one.
fn tree_version(dir: &Path) -> Option<String> {
    for member in ["colibri-cli", "colibri-runtime"] {
        if !dir.join("crates").join(member).join("Cargo.toml").is_file() {
            return None;
        }
    }
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let parsed = DeTable::parse(&text).ok()?;
    let workspace = field(parsed.get_ref(), "workspace")?.as_table()?;
    field(workspace, "members")?;
    field(workspace, "package")?
        .as_table()
        .and_then(|p| field(p, "version"))?
        .as_str()
        .map(str::to_string)
}

pub(super) fn field<'a>(table: &'a DeTable<'_>, key: &str) -> Option<&'a DeValue<'a>> {
    table
        .iter()
        .find(|(k, _)| k.get_ref().as_ref() == key)
        .map(|(_, v)| v.get_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder that looks like a Colibri checkout of `version`, removed at the end.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str, version: &str) -> Tree {
            let dir =
                std::env::temp_dir().join(format!("colibri-engine-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for member in ["colibri-cli", "colibri-runtime"] {
                let crate_dir = dir.join("crates").join(member);
                std::fs::create_dir_all(&crate_dir).unwrap();
                std::fs::write(crate_dir.join("Cargo.toml"), "[package]\n").unwrap();
            }
            std::fs::write(
                dir.join("Cargo.toml"),
                format!(
                    "[workspace]\nmembers = [\"crates/*\"]\n[workspace.package]\nversion = \"{version}\"\n"
                ),
            )
            .unwrap();
            Tree(dir)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn inputs(env: Option<&Path>, recorded: Option<&Path>, git: bool) -> Inputs {
        Inputs {
            env: env.map(|p| p.as_os_str().to_owned()),
            recorded: recorded.map(Path::to_path_buf),
            version: "0.1.0".to_string(),
            git: git.then(|| {
                (
                    "https://example.test/colibri".to_string(),
                    "abc123".to_string(),
                )
            }),
        }
    }

    #[test]
    fn the_environment_comes_first() {
        let env = Tree::new("env", "9.9.9");
        let recorded = Tree::new("env-rec", "0.1.0");
        let got = resolve(&inputs(Some(&env.0), Some(&recorded.0), true)).unwrap();
        assert_eq!(got, Engine::Path(env.0.clone()));
    }

    #[test]
    fn a_relative_environment_becomes_absolute() {
        // A tree inside the current folder, named relatively.
        let name = format!("colibri-engine-{}-rel", std::process::id());
        let tree = Tree::new("rel-src", "0.1.0");
        let here = std::env::current_dir().unwrap().join(&name);
        let _ = std::fs::remove_dir_all(&here);
        std::fs::rename(&tree.0, &here).unwrap();
        let moved = Tree(here.clone());
        let got = resolve(&inputs(Some(Path::new(&name)), None, false)).unwrap();
        let Engine::Path(dir) = got else {
            panic!("a path")
        };
        assert!(dir.is_absolute(), "{dir:?}");
        assert!(dir.ends_with(&name), "{dir:?}");
        drop(moved);
    }

    #[test]
    fn a_bad_environment_is_an_error_not_a_fallback() {
        let recorded = Tree::new("badenv-rec", "0.1.0");
        let nothing =
            std::env::temp_dir().join(format!("colibri-engine-{}-none", std::process::id()));
        let err = resolve(&inputs(Some(&nothing), Some(&recorded.0), true)).unwrap_err();
        assert!(
            err.contains("COLIBRI_SOURCE") && err.contains("not a Colibri source tree"),
            "{err}"
        );
    }

    #[test]
    fn the_recorded_tree_counts_when_it_is_there_with_the_same_version() {
        let recorded = Tree::new("rec", "0.1.0");
        let got = resolve(&inputs(None, Some(&recorded.0), true)).unwrap();
        assert_eq!(got, Engine::Path(recorded.0.clone()));
    }

    #[test]
    fn a_recorded_tree_of_another_version_or_gone_falls_to_git() {
        let other = Tree::new("other", "0.2.0");
        let want = Engine::Git {
            url: "https://example.test/colibri".to_string(),
            rev: "abc123".to_string(),
        };
        assert_eq!(resolve(&inputs(None, Some(&other.0), true)).unwrap(), want);
        let gone = std::env::temp_dir().join(format!("colibri-engine-{}-gone", std::process::id()));
        assert_eq!(resolve(&inputs(None, Some(&gone), true)).unwrap(), want);
    }

    #[test]
    fn without_any_source_the_error_says_what_to_set() {
        let gone =
            std::env::temp_dir().join(format!("colibri-engine-{}-gone2", std::process::id()));
        let err = resolve(&inputs(None, Some(&gone), false)).unwrap_err();
        assert!(
            err.contains("set COLIBRI_SOURCE to a checkout of Colibri 0.1.0"),
            "{err}"
        );
        assert!(err.contains("is gone"), "{err}");
    }

    #[test]
    fn this_checkout_is_a_source_tree_of_this_version() {
        let current = Inputs::current();
        let dir = current.recorded.clone().unwrap();
        assert_eq!(
            tree_version(&dir).as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }
}
