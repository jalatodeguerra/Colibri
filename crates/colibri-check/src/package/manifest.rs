//! `colibri.toml` (ARCHITECTURE §11.6): the package name and local-path dependencies.

use colibri_syntax::{Diagnostic, FileId, Span};

use super::toml::{self, Entry, Table, ValueKind};

/// A dependency listed in `[dependencies]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Dependency {
    /// The name `import` uses.
    pub name: String,
    pub name_span: Span,
    /// The folder of the dependency, as written (relative to the manifest's folder).
    pub path: String,
    pub path_span: Span,
}

/// What `colibri.toml` says.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub name_span: Span,
    pub dependencies: Vec<Dependency>,
    /// `ffi = true` in `[package]`: the package may contain `extern` blocks (DESIGN §15.3).
    pub ffi: bool,
}

/// Parses a manifest; every problem is a diagnostic in file `file`.
pub fn parse(file: FileId, text: &str) -> Result<Manifest, Vec<Diagnostic>> {
    let tables = toml::parse(file, text).map_err(|d| vec![d])?;
    let mut errors = Vec::new();
    let mut name = None;
    let mut ffi = false;
    let mut dependencies: Vec<Dependency> = Vec::new();
    for table in &tables {
        let path: Vec<&str> = table.path.iter().map(String::as_str).collect();
        match path.as_slice() {
            [] => {
                if let Some(e) = table.entries.first() {
                    errors.push(Diagnostic::error(
                        format!("`{}` must be inside a table such as `[package]`", e.key),
                        e.key_span,
                    ));
                }
            }
            ["package"] => package(table, &mut name, &mut ffi, &mut errors),
            ["dependencies"] => {
                for entry in &table.entries {
                    dependency(
                        &entry.key,
                        entry.key_span,
                        &entry.value,
                        &mut dependencies,
                        &mut errors,
                    );
                }
            }
            ["dependencies", dep] => {
                // `[dependencies.foo]` with `path = ".."` under it.
                let value = toml::Value {
                    span: table.span,
                    kind: ValueKind::Table(table.entries.clone()),
                };
                dependency(dep, table.span, &value, &mut dependencies, &mut errors);
            }
            _ => errors.push(
                Diagnostic::error(
                    format!("unknown table `[{}]`", table.path.join(".")),
                    table.span,
                )
                .with_hint("`colibri.toml` has `[package]` and `[dependencies]`"),
            ),
        }
    }
    let Some((name, name_span)) = name else {
        if errors.is_empty() {
            errors.push(
                Diagnostic::error("`colibri.toml` has no package name", Span::new(file, 0, 0))
                    .with_hint("add a `[package]` table with `name = \"mygame\"`"),
            );
        }
        return Err(errors);
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Manifest {
        name,
        name_span,
        dependencies,
        ffi,
    })
}

fn package(
    table: &Table,
    name: &mut Option<(String, Span)>,
    ffi: &mut bool,
    errors: &mut Vec<Diagnostic>,
) {
    for entry in &table.entries {
        match (entry.key.as_str(), &entry.value.kind) {
            ("name", ValueKind::Str(s)) => {
                if !is_name(s) {
                    errors.push(
                        Diagnostic::error(
                            format!("`{s}` is not a valid package name"),
                            entry.value.span,
                        )
                        .with_hint("use letters, digits and `_`, starting with a letter or `_`"),
                    );
                }
                *name = Some((s.clone(), entry.value.span));
            }
            ("name", _) => errors.push(Diagnostic::error(
                "the package `name` must be a string",
                entry.value.span,
            )),
            ("ffi", ValueKind::Bool(b)) => *ffi = *b,
            ("ffi", _) => errors.push(
                Diagnostic::error("`ffi` must be `true` or `false`", entry.value.span)
                    .with_hint("write `ffi = true` to let the package contain `extern` blocks"),
            ),
            (other, _) => errors.push(
                Diagnostic::error(
                    format!("unknown key `{other}` in `[package]`"),
                    entry.key_span,
                )
                .with_hint("the keys are `name` and `ffi`"),
            ),
        }
    }
}

fn dependency(
    name: &str,
    name_span: Span,
    value: &toml::Value,
    out: &mut Vec<Dependency>,
    errors: &mut Vec<Diagnostic>,
) {
    if !is_name(name) {
        errors.push(
            Diagnostic::error(
                format!("`{name}` is not a valid dependency name"),
                name_span,
            )
            .with_hint("it is the name `import` uses: letters, digits and `_`"),
        );
        return;
    }
    if let Some(prev) = out.iter().find(|d| d.name == name) {
        errors.push(
            Diagnostic::error(
                format!("the dependency `{name}` is listed more than once"),
                name_span,
            )
            .with_label(prev.name_span, "first listed here"),
        );
        return;
    }
    let entries: &[Entry] = match &value.kind {
        ValueKind::Table(entries) => entries,
        ValueKind::Str(_) | ValueKind::Bool(_) => {
            errors.push(
                Diagnostic::error(
                    format!("the dependency `{name}` must say where it is"),
                    value.span,
                )
                .with_hint(format!("write `{name} = {{ path = \"../{name}\" }}`")),
            );
            return;
        }
    };
    let mut path: Option<(String, Span)> = None;
    for entry in entries {
        match (entry.key.as_str(), &entry.value.kind) {
            ("path", ValueKind::Str(p)) => path = Some((p.clone(), entry.value.span)),
            ("path", _) => errors.push(Diagnostic::error(
                "`path` must be a string",
                entry.value.span,
            )),
            ("git" | "rev" | "branch" | "tag", _) => errors.push(
                Diagnostic::error(
                    format!(
                        "git dependencies are not supported yet (`{}` in `{name}`)",
                        entry.key
                    ),
                    entry.key_span,
                )
                .with_hint("use a local folder: `{ path = \"../name\" }`"),
            ),
            ("version" | "registry", _) => errors.push(
                Diagnostic::error(
                    format!(
                        "registry dependencies are not supported yet (`{}` in `{name}`)",
                        entry.key
                    ),
                    entry.key_span,
                )
                .with_hint("use a local folder: `{ path = \"../name\" }`"),
            ),
            (other, _) => errors.push(
                Diagnostic::error(
                    format!("unknown key `{other}` in the dependency `{name}`"),
                    entry.key_span,
                )
                .with_hint("a dependency has `path`"),
            ),
        }
    }
    match path {
        Some((path, path_span)) => out.push(Dependency {
            name: name.to_string(),
            name_span,
            path,
            path_span,
        }),
        None if errors.is_empty() => errors.push(
            Diagnostic::error(format!("the dependency `{name}` has no `path`"), value.span)
                .with_hint(format!("write `{name} = {{ path = \"../{name}\" }}`")),
        ),
        None => {}
    }
}

/// Whether `s` can be a package or dependency name: what an `import` can say.
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Manifest {
        parse(FileId(0), text).expect("a valid manifest")
    }

    fn errs(text: &str) -> Vec<String> {
        parse(FileId(0), text)
            .expect_err("an invalid manifest")
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn a_name_and_path_dependencies() {
        let m = ok(
            "[package]\nname = \"game\"\n\n[dependencies]\nphysics = { path = \"../physics\" }\n\n[dependencies.audio]\npath = \"libs/audio\"\n",
        );
        assert_eq!(m.name, "game");
        assert_eq!(m.dependencies.len(), 2);
        assert_eq!(m.dependencies[0].name, "physics");
        assert_eq!(m.dependencies[0].path, "../physics");
        assert_eq!(m.dependencies[1].path, "libs/audio");
    }

    #[test]
    fn a_manifest_without_dependencies_is_fine() {
        assert!(ok("[package]\nname = \"x\"\n").dependencies.is_empty());
    }

    #[test]
    fn git_and_registry_dependencies_are_refused_clearly() {
        let e = errs(
            "[package]\nname = \"x\"\n[dependencies]\nfoo = { git = \"https://example.com/foo\" }\n",
        );
        assert!(
            e.iter()
                .any(|m| m.contains("git dependencies are not supported yet")),
            "{e:?}"
        );
        let e = errs("[package]\nname = \"x\"\n[dependencies]\nfoo = { version = \"1\" }\n");
        assert!(
            e.iter()
                .any(|m| m.contains("registry dependencies are not supported yet")),
            "{e:?}"
        );
    }

    #[test]
    fn other_mistakes_each_have_a_message() {
        assert!(errs("").iter().any(|m| m.contains("no package name")));
        assert!(
            errs("[package]\n")
                .iter()
                .any(|m| m.contains("no package name"))
        );
        assert!(errs("name = \"x\"\n")[0].contains("must be inside a table"));
        assert!(errs("[package]\nname = \"my game\"\n")[0].contains("not a valid package name"));
        assert!(errs("[package]\nname = { a = \"b\" }\n")[0].contains("must be a string"));
        assert!(
            errs("[package]\nname = \"x\"\nversion = \"1\"\n")[0].contains("unknown key `version`")
        );
        assert!(errs("[package]\nname = \"x\"\n[lib]\n")[0].contains("unknown table"));
        assert!(
            errs("[package]\nname = \"x\"\n[dependencies]\nfoo = \"../foo\"\n")[0]
                .contains("must say where")
        );
        assert!(
            errs("[package]\nname = \"x\"\n[dependencies]\nfoo = { }\n")[0].contains("no `path`")
        );
        assert!(
            errs("[package]\nname = \"x\"\n[dependencies]\nfoo = { path = \"a\", x = \"b\" }\n")[0]
                .contains("unknown key `x`")
        );
        assert!(errs("[package]\nname = \"x\"\n[dependencies]\nfoo = { path = \"a\" }\n[dependencies.foo]\npath = \"b\"\n")[0].contains("more than once"));
        assert!(errs("a = 1")[0].contains("numbers"));
    }
}
