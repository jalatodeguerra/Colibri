//! `colibri.toml` (ARCHITECTURE §11.6): the package name and local-path dependencies.

use colibri_syntax::{Diagnostic, FileId, Span};

use super::toml::{self, Entry, Table, ValueKind};

/// A dependency listed in `[dependencies]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Dependency {
    /// The name `import` uses.
    pub name: String,
    pub name_span: Span,
    pub source: Source,
    /// `features = ["a", "b"]`: the features of the dependency this package asks for, with the
    /// place of each name (DESIGN §3.6).
    pub features: Vec<(String, Span)>,
    /// `default-features = false` turns off the dependency's `default` list (for this
    /// dependent; another one may still ask for it).
    pub default_features: bool,
}

/// A crate listed in `[rust-dependencies]` (DESIGN §15.5): a Rust crate that registers a binding
/// module, built into the project's runner. Nothing here calls cargo: the key is the module name
/// (`import native`), and `<key>.cbi` next to the manifest is what `colibri check` reads.
#[derive(Clone, Debug, PartialEq)]
pub struct RustDependency {
    /// The key: the name `import` uses and the module name of the `.cbi` header.
    pub name: String,
    /// Where the key is written.
    pub span: Span,
    pub source: RustSource,
    /// `features = [..]`, passed to cargo, with the place of each name.
    pub features: Vec<(String, Span)>,
    /// `default-features = false` is passed to cargo.
    pub default_features: bool,
}

/// Where the crate of a [`RustDependency`] comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum RustSource {
    /// `{ binding = "colibri-rapier", version = "0.1" }`: a crate of the registry.
    Binding { crate_name: String, version: String },
    /// `{ path = "native/" }`: a crate in the project (its package name is read from its
    /// `Cargo.toml` when the runner is made, not here).
    Path { path: String },
    /// `{ git = "https://..", rev = "..", crate = "x" }`.
    Git {
        url: String,
        spec: GitSpec,
        crate_name: String,
    },
}

impl RustSource {
    /// A text that is equal for two sources that name one crate. A git url is compared without
    /// a trailing `/` or `.git`. A path is compared by where it points: in the manifest's
    /// folder `dir` when given (so two packages' `native/` differ), else as written.
    pub(super) fn identity(&self, dir: Option<&std::path::Path>) -> String {
        match self {
            RustSource::Binding { crate_name, .. } => format!("crate {crate_name}"),
            RustSource::Git {
                crate_name, url, ..
            } => {
                let url = url.trim_end_matches('/');
                let url = url.strip_suffix(".git").unwrap_or(url);
                let url = url.trim_end_matches('/');
                format!("git {url} crate {crate_name}")
            }
            RustSource::Path { path } => {
                let trimmed = path.trim_end_matches(['/', '\\']);
                let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
                match dir {
                    Some(dir) => format!(
                        "path {}",
                        super::fs::normalize(&dir.join(trimmed))
                            .to_string_lossy()
                            .replace('\\', "/")
                    ),
                    None => format!("path {trimmed}"),
                }
            }
        }
    }
}

/// Where a dependency comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A folder, as written (relative to the manifest's folder).
    Path { path: String, path_span: Span },
    /// A git repository; its checkout is found through `colibri.lock` (ARCHITECTURE §11.6b).
    Git(GitSource),
}

/// `{ git = "<url>", rev = ".." }` (or `tag`, or `branch`, or none of them).
#[derive(Clone, Debug, PartialEq)]
pub struct GitSource {
    /// The URL as written; anything `git clone` takes, except a relative path.
    pub url: String,
    pub url_span: Span,
    pub spec: GitSpec,
}

/// Which commit of the repository a git dependency wants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitSpec {
    /// The head of the remote's default branch.
    DefaultBranch,
    /// A commit, by (a prefix of) its hash.
    Rev(String),
    Tag(String),
    Branch(String),
}

impl GitSpec {
    /// The manifest key that says it, and its value (`None` for the default branch).
    pub fn key_value(&self) -> Option<(&'static str, &str)> {
        match self {
            GitSpec::DefaultBranch => None,
            GitSpec::Rev(v) => Some(("rev", v)),
            GitSpec::Tag(v) => Some(("tag", v)),
            GitSpec::Branch(v) => Some(("branch", v)),
        }
    }

    /// How a message names it: `rev abc1234`, `the default branch`.
    pub fn describe(&self) -> String {
        match self.key_value() {
            None => "the default branch".to_string(),
            Some((key, value)) => format!("{key} `{value}`"),
        }
    }
}

/// What `colibri.toml` says.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub name_span: Span,
    pub dependencies: Vec<Dependency>,
    /// `ffi = true` in `[package]`: the package may contain `extern` blocks (DESIGN §15.3).
    pub ffi: bool,
    /// `allow-ffi = ["name"]` in `[package]`: the git dependencies (direct or transitive) that
    /// may declare `ffi = true`. Only the root package's list counts (DESIGN §15.3).
    pub allow_ffi: Vec<String>,
    /// Where each name of `allow_ffi` is written.
    pub allow_ffi_spans: Vec<Span>,
    /// `[rust-dependencies]`: the Rust crates that provide binding modules (DESIGN §15.5).
    pub rust_dependencies: Vec<RustDependency>,
    /// `allow-rust = ["name"]` in `[package]`: the git dependencies that may declare
    /// `[rust-dependencies]`. Only the root package's list counts.
    pub allow_rust: Vec<String>,
    /// Where each name of `allow_rust` is written.
    pub allow_rust_spans: Vec<Span>,
    /// `[features]`: the feature flags `@when(name)` can test (DESIGN §3.6).
    pub features: Features,
    /// `[build]`: what `colibri build` adds to the program.
    pub build: Build,
}

/// The `[build]` table of `colibri.toml`:
///
/// ```toml
/// [build]
/// assets = ["data/", "img/*.png"]   # the files `import assets` can read, see [`super::assets`]
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Build {
    /// The patterns of `assets`, as written, with the place of each.
    pub assets: Vec<(String, Span)>,
}

/// The `[features]` table of `colibri.toml`:
///
/// ```toml
/// [features]
/// default = ["audio"]   # on unless the build says `--no-default-features`
/// audio = []            # a feature; the list holds the features it turns on too
/// net = ["audio"]
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Features {
    /// Every declared feature with the features it turns on, in the order written.
    pub declared: Vec<(String, Vec<String>)>,
    /// The `default` list: the features that are on unless the build turns the defaults off.
    pub default: Vec<String>,
}

impl Features {
    /// Whether the package declares a feature called `name`.
    pub fn has(&self, name: &str) -> bool {
        self.declared.iter().any(|(n, _)| n == name)
    }

    /// The features that are on when the build asks for `requested` (and, unless
    /// `no_defaults`, the `default` list), each with everything it turns on.
    pub fn enabled(&self, requested: &[String], no_defaults: bool) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut todo: Vec<String> = requested.to_vec();
        if !no_defaults {
            todo.extend(self.default.iter().cloned());
        }
        while let Some(name) = todo.pop() {
            if out.contains(&name) {
                continue;
            }
            if let Some((_, more)) = self.declared.iter().find(|(n, _)| *n == name) {
                todo.extend(more.iter().cloned());
            }
            out.push(name);
        }
        out.sort();
        out
    }
}

/// The names a feature may not take: they mean something in a `@when` condition already.
/// Why `name` cannot be a feature name (a message and a hint), or `None` when it can: letters,
/// digits and `_`, not starting with a digit, no keyword, and none of the names `@when` gives a
/// meaning of its own. The rule for `[features]` keys, `--feature` and `VmBuilder::feature`.
pub fn feature_name_problem(name: &str) -> Option<(String, String)> {
    if name.is_empty() {
        return Some((
            "a feature name cannot be empty".to_string(),
            "use letters, digits and `_`, as in `--feature fast`".to_string(),
        ));
    }
    if RESERVED_FEATURES.contains(&name) {
        return Some((
            format!("`{name}` is not a valid feature name: it has a meaning of its own in `@when`"),
            "`os`, `arch` and `debug` are decided by the build, `default` is the list of features that are on by default, and `true` and `false` are conditions; choose another name".to_string(),
        ));
    }
    if !is_name(name) {
        let why = if colibri_syntax::TokenKind::keyword(name).is_some() {
            "it is a keyword"
        } else {
            "use letters, digits and `_`, and do not start with a digit"
        };
        return Some((
            format!("`{name}` is not a valid feature name"),
            why.to_string(),
        ));
    }
    None
}

pub const RESERVED_FEATURES: &[&str] = &["os", "arch", "debug", "default", "true", "false"];

/// Parses a manifest; every problem is a diagnostic in file `file`. The file is read through
/// [`toml::read_config`]: one with a credential in it is refused as a whole.
pub fn parse(file: FileId, text: &str) -> Result<Manifest, Vec<Diagnostic>> {
    from_tables(file, toml::read_config(file, text, "colibri.toml").tables)
}

/// The manifest that the tables of a `colibri.toml` read by [`toml::read_config`] mean.
pub fn from_tables(
    file: FileId,
    tables: Result<Vec<toml::Table>, Vec<Diagnostic>>,
) -> Result<Manifest, Vec<Diagnostic>> {
    let tables = tables?;
    let mut errors = Vec::new();
    let mut name = None;
    let mut ffi = false;
    let mut allow_ffi: Vec<(String, Span)> = Vec::new();
    let mut allow_rust: Vec<(String, Span)> = Vec::new();
    let mut dependencies: Vec<Dependency> = Vec::new();
    let mut rust_dependencies: Vec<RustDependency> = Vec::new();
    let mut features = Features::default();
    let mut build = Build::default();
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
            ["package"] => package(
                table,
                &mut name,
                &mut ffi,
                &mut allow_ffi,
                &mut allow_rust,
                &mut errors,
            ),
            ["rust-dependencies"] => {
                for entry in &table.entries {
                    rust_dependency(
                        &entry.key,
                        entry.key_span,
                        &entry.value,
                        &mut rust_dependencies,
                        &mut errors,
                    );
                }
            }
            ["rust-dependencies", dep] => {
                let value = toml::Value {
                    span: table.span,
                    kind: ValueKind::Table(table.entries.clone()),
                };
                rust_dependency(dep, table.span, &value, &mut rust_dependencies, &mut errors);
            }
            ["features"] => parse_features(table, &mut features, &mut errors),
            ["build"] => parse_build(table, &mut build, &mut errors),
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
                .with_hint(
                    "`colibri.toml` has `[package]`, `[dependencies]`, `[rust-dependencies]`, `[features]` and `[build]`",
                ),
            ),
        }
    }
    check_rust_clashes(&dependencies, &rust_dependencies, &mut errors);
    let Some((name, name_span)) = name else {
        if errors.is_empty() {
            errors.push(
                Diagnostic::error("`colibri.toml` has no package name", Span::new(file, 0, 0))
                    .with_hint("add a `[package]` table with `name = \"myapp\"`"),
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
        allow_ffi_spans: allow_ffi.iter().map(|(_, s)| *s).collect(),
        allow_ffi: allow_ffi.into_iter().map(|(n, _)| n).collect(),
        rust_dependencies,
        allow_rust_spans: allow_rust.iter().map(|(_, s)| *s).collect(),
        allow_rust: allow_rust.into_iter().map(|(n, _)| n).collect(),
        features,
        build,
    })
}

fn parse_build(table: &Table, build: &mut Build, errors: &mut Vec<Diagnostic>) {
    for entry in &table.entries {
        match (entry.key.as_str(), &entry.value.kind) {
            ("assets", ValueKind::Array(items)) => {
                for item in items {
                    match &item.kind {
                        ValueKind::Str(text) => match super::assets::pattern_problem(text) {
                            None => build.assets.push((text.clone(), item.span)),
                            Some(problem) => errors.push(
                                Diagnostic::error(
                                    format!("`{text}` is not a valid asset pattern: {problem}"),
                                    item.span,
                                )
                                .with_hint(
                                    "write a path relative to the package folder, as in `\"data/\"` or `\"img/*.png\"`",
                                ),
                            ),
                        },
                        _ => errors.push(Diagnostic::error(
                            "`assets` lists patterns as strings",
                            item.span,
                        )),
                    }
                }
            }
            ("assets", _) => errors.push(
                Diagnostic::error("`assets` must be a list of patterns", entry.value.span)
                    .with_hint("write `assets = [\"data/\", \"img/logo.png\"]`"),
            ),
            (other, _) => errors.push(
                Diagnostic::error(
                    format!("unknown key `{other}` in `[build]`"),
                    entry.key_span,
                )
                .with_hint("the only key is `assets`"),
            ),
        }
    }
}

fn parse_features(table: &Table, features: &mut Features, errors: &mut Vec<Diagnostic>) {
    let names = |value: &toml::Value, errors: &mut Vec<Diagnostic>| -> Option<Vec<String>> {
        let ValueKind::Array(items) = &value.kind else {
            errors.push(
                Diagnostic::error(
                    "a feature is a list of the features it turns on",
                    value.span,
                )
                .with_hint("write `audio = []` or `net = [\"audio\"]`"),
            );
            return None;
        };
        let mut out = Vec::new();
        for item in items {
            match &item.kind {
                ValueKind::Str(n) => out.push(n.clone()),
                _ => errors.push(Diagnostic::error(
                    "a feature list holds feature names as strings",
                    item.span,
                )),
            }
        }
        Some(out)
    };
    let mut default: Option<(Vec<String>, Vec<Span>)> = None;
    for entry in &table.entries {
        if entry.key == "default" {
            if let Some(list) = names(&entry.value, errors) {
                let spans = match &entry.value.kind {
                    ValueKind::Array(items) => items.iter().map(|i| i.span).collect(),
                    _ => Vec::new(),
                };
                default = Some((list, spans));
            }
            continue;
        }
        if let Some((message, hint)) = feature_name_problem(&entry.key) {
            errors.push(Diagnostic::error(message, entry.key_span).with_hint(hint));
            continue;
        }
        if features.has(&entry.key) {
            errors.push(Diagnostic::error(
                format!("the feature `{}` is listed more than once", entry.key),
                entry.key_span,
            ));
            continue;
        }
        if let Some(list) = names(&entry.value, errors) {
            features.declared.push((entry.key.clone(), list));
        }
    }
    // Everything a list names must be declared.
    let spans_of = |entry_key: &str| -> Vec<Span> {
        table
            .entries
            .iter()
            .find(|e| e.key == entry_key)
            .and_then(|e| match &e.value.kind {
                ValueKind::Array(items) => Some(items.iter().map(|i| i.span).collect()),
                _ => None,
            })
            .unwrap_or_default()
    };
    let mut check = |owner: &str, list: &[String]| {
        let spans = spans_of(owner);
        for (i, name) in list.iter().enumerate() {
            if !features.has(name) {
                let known: Vec<&str> = features.declared.iter().map(|(n, _)| n.as_str()).collect();
                let span = spans.get(i).copied().unwrap_or(table.span);
                errors.push(
                    Diagnostic::error(format!("`{name}` is not a declared feature"), span)
                        .with_hint(if known.is_empty() {
                            "declare it as a key of `[features]`: `name = []`".to_string()
                        } else {
                            format!("the declared features are: {}", known.join(", "))
                        }),
                );
            }
        }
    };
    for (owner, list) in features.declared.clone() {
        check(&owner, &list);
    }
    if let Some((list, _)) = &default {
        check("default", list);
        features.default = list.clone();
    }
}

fn package(
    table: &Table,
    name: &mut Option<(String, Span)>,
    ffi: &mut bool,
    allow_ffi: &mut Vec<(String, Span)>,
    allow_rust: &mut Vec<(String, Span)>,
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
                        .with_hint("use letters, digits and `_`, starting with a letter or `_`, and not a keyword"),
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
            ("allow-ffi", ValueKind::Array(items)) => {
                for item in items {
                    match &item.kind {
                        ValueKind::Str(n) if is_name(n) => allow_ffi.push((n.clone(), item.span)),
                        _ => errors.push(
                            Diagnostic::error("`allow-ffi` lists package names", item.span)
                                .with_hint(
                                    "write `allow-ffi = [\"name\"]`: the name `import` uses",
                                ),
                        ),
                    }
                }
            }
            ("allow-ffi", _) => errors.push(
                Diagnostic::error(
                    "`allow-ffi` must be a list of package names",
                    entry.value.span,
                )
                .with_hint("write `allow-ffi = [\"name\"]`"),
            ),
            ("allow-rust", ValueKind::Array(items)) => {
                for item in items {
                    match &item.kind {
                        ValueKind::Str(n) if is_name(n) => allow_rust.push((n.clone(), item.span)),
                        _ => errors.push(
                            Diagnostic::error("`allow-rust` lists package names", item.span)
                                .with_hint(
                                    "write `allow-rust = [\"name\"]`: the name `import` uses",
                                ),
                        ),
                    }
                }
            }
            ("allow-rust", _) => errors.push(
                Diagnostic::error(
                    "`allow-rust` must be a list of package names",
                    entry.value.span,
                )
                .with_hint("write `allow-rust = [\"name\"]`"),
            ),
            (other, _) => errors.push(
                Diagnostic::error(
                    format!("unknown key `{other}` in `[package]`"),
                    entry.key_span,
                )
                .with_hint("the keys are `name`, `ffi`, `allow-ffi` and `allow-rust`"),
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
            .with_hint("it is the name `import` uses: letters, digits and `_`, not a keyword"),
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
        ValueKind::Str(_) | ValueKind::Bool(_) | ValueKind::Array(_) => {
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
    let before = errors.len();
    let mut path: Option<(String, Span)> = None;
    let mut git: Option<(String, Span)> = None;
    let mut specs: Vec<(&'static str, String, Span, Span)> = Vec::new();
    let mut features: Vec<(String, Span)> = Vec::new();
    let mut default_features = true;
    for entry in entries {
        let vspan = entry.value.span;
        match (entry.key.as_str(), &entry.value.kind) {
            ("path", ValueKind::Str(p)) => path = Some((p.clone(), vspan)),
            ("path", _) => errors.push(Diagnostic::error("`path` must be a string", vspan)),
            ("git", ValueKind::Str(u)) => git = Some((u.clone(), vspan)),
            ("git", _) => errors.push(
                Diagnostic::error("`git` must be a string", vspan)
                    .with_hint("write `git = \"https://example.com/name.git\"`"),
            ),
            (key @ ("rev" | "tag" | "branch"), ValueKind::Str(v)) => {
                let key = match key {
                    "rev" => "rev",
                    "tag" => "tag",
                    _ => "branch",
                };
                specs.push((key, v.clone(), entry.key_span, vspan));
            }
            ("rev" | "tag" | "branch", _) => errors.push(Diagnostic::error(
                format!("`{}` must be a string", entry.key),
                vspan,
            )),
            ("features", ValueKind::Array(items)) => {
                for item in items {
                    match &item.kind {
                        ValueKind::Str(f) => {
                            if let Some((message, hint)) = feature_name_problem(f) {
                                errors.push(Diagnostic::error(message, item.span).with_hint(hint));
                            } else if !features.iter().any(|(n, _)| n == f) {
                                features.push((f.clone(), item.span));
                            }
                        }
                        _ => errors.push(
                            Diagnostic::error("`features` lists feature names", item.span)
                                .with_hint("write `features = [\"fast\"]`"),
                        ),
                    }
                }
            }
            ("features", _) => errors.push(
                Diagnostic::error("`features` must be a list of feature names", vspan)
                    .with_hint("write `features = [\"fast\"]`"),
            ),
            ("default-features", ValueKind::Bool(b)) => default_features = *b,
            ("default-features", _) => errors.push(
                Diagnostic::error("`default-features` must be `true` or `false`", vspan)
                    .with_hint("write `default-features = false` to leave the dependency's default features off"),
            ),
            ("version" | "registry", _) => errors.push(
                Diagnostic::error(
                    format!(
                        "registry dependencies are not supported yet (`{}` in `{name}`)",
                        entry.key
                    ),
                    entry.key_span,
                )
                .with_hint("use a git repository or a local folder: `{ git = \"..\" }` or `{ path = \"../name\" }`"),
            ),
            (other, _) => errors.push(
                Diagnostic::error(
                    format!("unknown key `{other}` in the dependency `{name}`"),
                    entry.key_span,
                )
                .with_hint("a dependency has `path`, or `git` with `rev`, `tag` or `branch`, and may have `features` and `default-features`"),
            ),
        }
    }
    if let [first, second, ..] = specs.as_slice() {
        errors.push(
            Diagnostic::error(
                format!(
                    "the dependency `{name}` says both `{}` and `{}`: use only one of `rev`, `tag` and `branch`",
                    first.0, second.0
                ),
                second.2,
            )
            .with_label(first.2, "first given here"),
        );
    }
    if let (Some((_, path_span)), Some((_, git_span))) = (&path, &git) {
        errors.push(
            Diagnostic::error(
                format!("the dependency `{name}` has both `path` and `git`"),
                *git_span,
            )
            .with_label(*path_span, "`path` is here")
            .with_hint("a dependency is either a local folder or a git repository"),
        );
    }
    if git.is_none() && path.is_some() {
        for (key, _, key_span, _) in &specs {
            errors.push(
                Diagnostic::error(
                    format!("`{key}` in the dependency `{name}` only goes with `git`"),
                    *key_span,
                )
                .with_hint("a local folder has no revision"),
            );
        }
    } else if git.is_none()
        && let Some((key, _, key_span, _)) = specs.first()
    {
        errors.push(
            Diagnostic::error(
                format!("`{key}` in the dependency `{name}` needs a `git` url"),
                *key_span,
            )
            .with_hint(format!(
                "write `{name} = {{ git = \"<url>\", {key} = \"..\" }}`"
            )),
        );
    }
    if errors.len() > before {
        return;
    }
    match (path, git) {
        (Some((path, path_span)), None) => out.push(Dependency {
            name: name.to_string(),
            name_span,
            source: Source::Path { path, path_span },
            features,
            default_features,
        }),
        (None, Some((url, url_span))) => {
            if let Some(why) = super::git::userinfo_problem(&url) {
                errors.push(
                    Diagnostic::error(
                        format!("the url of the dependency `{name}` has credentials in it: {why}"),
                        url_span,
                    )
                    .with_hint(
                        "keep credentials out of `colibri.toml` (it is shared and copied into `colibri.lock`): use a git credential helper (`git config --global credential.helper`) or an ssh key",
                    ),
                );
                return;
            }
            if let Some(d) = bad_url(&url, url_span) {
                errors.push(d);
                return;
            }
            let url = super::git::lower_scheme(&url);
            let spec = match specs.pop() {
                None => GitSpec::DefaultBranch,
                Some((key, value, _, vspan)) => {
                    if let Some(d) = bad_spec(key, &value, vspan) {
                        errors.push(d);
                        return;
                    }
                    match key {
                        "rev" => GitSpec::Rev(value),
                        "tag" => GitSpec::Tag(value),
                        _ => GitSpec::Branch(value),
                    }
                }
            };
            out.push(Dependency {
                name: name.to_string(),
                name_span,
                source: Source::Git(GitSource {
                    url,
                    url_span,
                    spec,
                }),
                features,
                default_features,
            });
        }
        _ => errors.push(
            Diagnostic::error(
                format!("the dependency `{name}` has no `path` or `git`"),
                value.span,
            )
            .with_hint(format!(
                "write `{name} = {{ path = \"../{name}\" }}` or `{name} = {{ git = \"<url>\" }}`"
            )),
        ),
    }
}

/// A crate name as cargo takes it: letters, digits, `-` and `_`, starting with a letter.
fn is_crate_name(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn rust_dependency(
    name: &str,
    name_span: Span,
    value: &toml::Value,
    out: &mut Vec<RustDependency>,
    errors: &mut Vec<Diagnostic>,
) {
    if !is_name(name) {
        errors.push(
            Diagnostic::error(
                format!("`{name}` is not a valid name for a Rust dependency"),
                name_span,
            )
            .with_hint(
                "the key is the module name `import` uses: letters, digits and `_`, not a keyword",
            ),
        );
        return;
    }
    if let Some(prev) = out.iter().find(|d| d.name == name) {
        errors.push(
            Diagnostic::error(
                format!("the Rust dependency `{name}` is listed more than once"),
                name_span,
            )
            .with_label(prev.span, "first listed here"),
        );
        return;
    }
    let ValueKind::Table(entries) = &value.kind else {
        errors.push(
            Diagnostic::error(
                format!("the Rust dependency `{name}` must say where the crate is"),
                value.span,
            )
            .with_hint(format!(
                "write `{name} = {{ path = \"native/\" }}`, `{{ binding = \"crate\", version = \"0.1\" }}` or `{{ git = \"<url>\", crate = \"crate\" }}`"
            )),
        );
        return;
    };
    let before = errors.len();
    let string = |key: &str, vspan: Span, errors: &mut Vec<Diagnostic>, v: &str| {
        let empty = v.trim().is_empty();
        if empty {
            errors.push(Diagnostic::error(format!("`{key}` is empty"), vspan));
        }
        (!empty).then(|| (v.to_string(), vspan))
    };
    let mut binding: Option<(String, Span)> = None;
    let mut version: Option<(String, Span)> = None;
    let mut path: Option<(String, Span)> = None;
    let mut git: Option<(String, Span)> = None;
    let mut crate_name: Option<(String, Span)> = None;
    let mut specs: Vec<(&'static str, String, Span, Span)> = Vec::new();
    let mut features: Vec<(String, Span)> = Vec::new();
    let mut default_features = true;
    // The keys that choose a source, in the order written, for the "more than one" message.
    let mut sources: Vec<(&'static str, Span)> = Vec::new();
    for entry in entries {
        let vspan = entry.value.span;
        let kspan = entry.key_span;
        match (entry.key.as_str(), &entry.value.kind) {
            ("binding", ValueKind::Str(v)) => {
                sources.push(("binding", kspan));
                binding = string("binding", vspan, errors, v);
            }
            ("path", ValueKind::Str(v)) => {
                sources.push(("path", kspan));
                path = string("path", vspan, errors, v);
            }
            ("git", ValueKind::Str(v)) => {
                sources.push(("git", kspan));
                git = string("git", vspan, errors, v);
            }
            ("version", ValueKind::Str(v)) => version = string("version", vspan, errors, v),
            ("crate", ValueKind::Str(v)) => crate_name = string("crate", vspan, errors, v),
            (key @ ("binding" | "path" | "git" | "version" | "crate"), _) => errors
                .push(Diagnostic::error(format!("`{key}` must be a string"), vspan)),
            (key @ ("rev" | "tag" | "branch"), ValueKind::Str(v)) => {
                let key = match key {
                    "rev" => "rev",
                    "tag" => "tag",
                    _ => "branch",
                };
                specs.push((key, v.clone(), kspan, vspan));
            }
            ("rev" | "tag" | "branch", _) => errors.push(Diagnostic::error(
                format!("`{}` must be a string", entry.key),
                vspan,
            )),
            ("features", ValueKind::Array(items)) => {
                for item in items {
                    match &item.kind {
                        // Cargo's feature names, not Colibri's: any non-empty text.
                        ValueKind::Str(f) if !f.is_empty() => {
                            if !features.iter().any(|(n, _)| n == f) {
                                features.push((f.clone(), item.span));
                            }
                        }
                        _ => errors.push(
                            Diagnostic::error("`features` lists feature names", item.span)
                                .with_hint("write `features = [\"serde\"]`"),
                        ),
                    }
                }
            }
            ("features", _) => errors.push(
                Diagnostic::error("`features` must be a list of feature names", vspan)
                    .with_hint("write `features = [\"serde\"]`"),
            ),
            ("default-features", ValueKind::Bool(b)) => default_features = *b,
            ("default-features", _) => errors.push(
                Diagnostic::error("`default-features` must be `true` or `false`", vspan)
                    .with_hint("write `default-features = false` to leave the crate's default features off"),
            ),
            (other, _) => errors.push(
                Diagnostic::error(
                    format!("unknown key `{other}` in the Rust dependency `{name}`"),
                    kspan,
                )
                .with_hint("a Rust dependency is `binding` with `version`, or `path`, or `git` with `crate` and optionally `rev`, `tag` or `branch`; it may have `features` and `default-features`"),
            ),
        }
    }
    if let [first, second, ..] = sources.as_slice() {
        errors.push(
            Diagnostic::error(
                format!(
                    "the Rust dependency `{name}` says both `{}` and `{}`: use only one of `binding`, `path` and `git`",
                    first.0, second.0
                ),
                second.1,
            )
            .with_label(first.1, "first given here"),
        );
    } else if sources.is_empty() && errors.len() == before {
        errors.push(
            Diagnostic::error(
                format!("the Rust dependency `{name}` has no `binding`, `path` or `git`"),
                value.span,
            )
            .with_hint(format!(
                "write `{name} = {{ path = \"native/\" }}`, `{{ binding = \"crate\", version = \"0.1\" }}` or `{{ git = \"<url>\", crate = \"crate\" }}`"
            )),
        );
    }
    let key_span = |key: &str| {
        entries
            .iter()
            .find(|e| e.key == key)
            .map_or(value.span, |e| e.key_span)
    };
    if let [first, second, ..] = specs.as_slice() {
        errors.push(
            Diagnostic::error(
                format!(
                    "the Rust dependency `{name}` says both `{}` and `{}`: use only one of `rev`, `tag` and `branch`",
                    first.0, second.0
                ),
                second.2,
            )
            .with_label(first.2, "first given here"),
        );
    }
    if git.is_none() {
        if let Some((key, _, kspan, _)) = specs.first() {
            errors.push(
                Diagnostic::error(
                    format!("`{key}` in the Rust dependency `{name}` only goes with `git`"),
                    *kspan,
                )
                .with_hint("a crate from the registry or a folder has no revision here"),
            );
        }
        if crate_name.is_some() {
            errors.push(
                Diagnostic::error(
                    format!("`crate` in the Rust dependency `{name}` only goes with `git`"),
                    key_span("crate"),
                )
                .with_hint("`binding` already names the crate"),
            );
        }
    }
    if binding.is_none() && version.is_some() {
        errors.push(
            Diagnostic::error(
                format!("`version` in the Rust dependency `{name}` only goes with `binding`"),
                key_span("version"),
            )
            .with_hint("a `path` crate has its version in its own `Cargo.toml`"),
        );
    }
    if binding.is_some() && version.is_none() {
        errors.push(
            Diagnostic::error(
                format!("the Rust dependency `{name}` has `binding` but no `version`"),
                key_span("binding"),
            )
            .with_hint(format!(
                "write `{name} = {{ binding = \"{}\", version = \"0.1\" }}`",
                binding.as_ref().map_or("crate", |(b, _)| b.as_str())
            )),
        );
    }
    if git.is_some() && crate_name.is_none() {
        errors.push(
            Diagnostic::error(
                format!("the Rust dependency `{name}` has `git` but no `crate`"),
                key_span("git"),
            )
            .with_hint(
                "`crate` names the package inside the repository: `crate = \"colibri-rapier\"`",
            ),
        );
    }
    for (key, found) in [("binding", &binding), ("crate", &crate_name)] {
        if let Some((n, span)) = found
            && !is_crate_name(n)
        {
            errors.push(
                Diagnostic::error(format!("`{n}` is not a crate name"), *span).with_hint(format!(
                    "`{key}` is a Cargo package name: letters, digits, `-` and `_`"
                )),
            );
        }
    }
    if errors.len() > before {
        return;
    }
    let source = match (binding, version, path, git, crate_name) {
        (Some((crate_name, _)), Some((version, _)), None, None, None) => RustSource::Binding {
            crate_name,
            version,
        },
        (None, None, Some((path, _)), None, None) => RustSource::Path { path },
        (None, None, None, Some((url, url_span)), Some((crate_name, _))) => {
            if let Some(why) = super::git::userinfo_problem(&url) {
                errors.push(
                    Diagnostic::error(
                        format!(
                            "the url of the Rust dependency `{name}` has credentials in it: {why}"
                        ),
                        url_span,
                    )
                    .with_hint(
                        "keep credentials out of `colibri.toml`: use a git credential helper or an ssh key",
                    ),
                );
                return;
            }
            if let Some(d) = bad_url(&url, url_span) {
                errors.push(d);
                return;
            }
            let spec = match specs.pop() {
                None => GitSpec::DefaultBranch,
                Some((key, value, _, vspan)) => {
                    if let Some(d) = bad_spec(key, &value, vspan) {
                        errors.push(d);
                        return;
                    }
                    match key {
                        "rev" => GitSpec::Rev(value),
                        "tag" => GitSpec::Tag(value),
                        _ => GitSpec::Branch(value),
                    }
                }
            };
            RustSource::Git {
                url: super::git::lower_scheme(&url),
                spec,
                crate_name,
            }
        }
        // Every other combination was reported above.
        _ => return,
    };
    out.push(RustDependency {
        name: name.to_string(),
        span: name_span,
        source,
        features,
        default_features,
    });
}

/// The rules between tables: a Rust dependency's key may not be a `[dependencies]` key (both are
/// names for `import`), and one crate may not be listed under two keys (it would be
/// registered twice).
fn check_rust_clashes(
    dependencies: &[Dependency],
    rust: &[RustDependency],
    errors: &mut Vec<Diagnostic>,
) {
    for (i, r) in rust.iter().enumerate() {
        if let Some(d) = dependencies.iter().find(|d| d.name == r.name) {
            errors.push(
                Diagnostic::error(
                    format!(
                        "`{}` is both a dependency and a Rust dependency: one name cannot be imported two ways",
                        r.name
                    ),
                    r.span,
                )
                .with_label(d.name_span, "listed in `[dependencies]` here")
                .with_hint("rename one of them: the key of a Rust dependency is its module name"),
            );
        }
        let id = r.source.identity(None);
        if let Some(first) = rust[..i].iter().find(|o| o.source.identity(None) == id) {
            errors.push(
                Diagnostic::error(
                    format!(
                        "the Rust dependency `{}` is the same crate as `{}`",
                        r.name, first.name
                    ),
                    r.span,
                )
                .with_label(first.span, "first listed here")
                .with_hint("a crate registers one module: list it once, under the name `import` should use"),
            );
        }
    }
}

/// Where a repository url leads: to somewhere on the network, or to a folder of this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlKind {
    /// `https://`, `ssh://`, `git://` or `user@host:path`.
    Remote,
    /// `file://` or an absolute path.
    Local,
}

/// What kind of url `url` is, or why it is not one the tool takes: a url with an allowed
/// scheme (`https`, `ssh`, `git`, `file`), an scp-like `user@host:path`, or an absolute path.
/// Anything else (`r4`, `sub/repo`, `.git`) would be read relative to the folder the tool
/// happens to run in.
pub fn classify_url(url: &str) -> Result<UrlKind, String> {
    classify(url).map_err(|(message, _)| message)
}

/// [`classify_url`] with the hint that fits the error, if one does.
fn classify(url: &str) -> Result<UrlKind, (String, Option<&'static str>)> {
    if let Some(problem) = super::git::transport_problem(url) {
        return Err((problem, None));
    }
    if let Some(problem) = option_like(url) {
        return Err((
            format!(
                "`{}` is not a git url: {problem}",
                super::git::redact_url(url)
            ),
            None,
        ));
    }
    if let Some((scheme, rest)) = url.split_once("://") {
        if rest.is_empty() {
            return Err((
                format!(
                    "`{}` is not a git url: it names no repository",
                    super::git::redact_url(url)
                ),
                None,
            ));
        }
        return Ok(if scheme.eq_ignore_ascii_case("file") {
            UrlKind::Local
        } else {
            UrlKind::Remote
        });
    }
    if is_absolute_path(url) {
        return Ok(UrlKind::Local);
    }
    if let Some((user_host, path)) = url.split_once(':')
        && let Some((user, host)) = user_host.split_once('@')
        && !user.is_empty()
        && !host.is_empty()
        && !host.starts_with('-')
        && !user_host.contains('/')
        && !path.is_empty()
    {
        return Ok(UrlKind::Remote);
    }
    Err((
        format!(
            "`{}` is not a git url: write an `https://`, `ssh://`, `git://` or `file://` url, `user@host:path`, or an absolute path",
            super::git::redact_url(url)
        ),
        Some(
            "a relative path would mean a different place from each folder the tool is started in; for a folder inside this repository use `path`",
        ),
    ))
}

/// Whether an `ssh` url or an scp-like `user@host:path` has a part that `ssh` (or the remote
/// side) would read as an option: a user, host or path that starts with `-`
/// (`ssh://-oProxyCommand=x/r`, `u@h:-oProxyCommand=x`).
fn option_like(url: &str) -> Option<&'static str> {
    const HOST: &str = "the host starts with `-`, which `ssh` would read as an option";
    const PATH: &str = "the path starts with `-`, which would be read as an option";
    const PORT: &str = "the port is not a number";
    // What `ssh` would read as the host: percent-escapes undone, a bracket pair (an address
    // in brackets) taken off.
    let dashed = |host: &str| {
        let host = percent_decode(host);
        host.trim_start_matches('[').starts_with('-')
    };
    if let Some((scheme, rest)) = url.split_once("://") {
        if !scheme.eq_ignore_ascii_case("ssh") {
            return None;
        }
        let authority = rest.split('/').next().unwrap_or("");
        let host_port = authority.rsplit('@').next().unwrap_or("");
        if dashed(authority) || dashed(host_port) {
            return Some(HOST);
        }
        // `host:port`, the port after the last `:` outside brackets.
        let after_bracket = host_port.rsplit(']').next().unwrap_or(host_port);
        if let Some((_, port)) = after_bracket.split_once(':')
            && !port.bytes().all(|b| b.is_ascii_digit())
        {
            return Some(PORT);
        }
        return None;
    }
    if is_absolute_path(url) {
        return None;
    }
    let (user_host, path) = url.split_once(':')?;
    let (_, host) = user_host.split_once('@')?;
    if dashed(user_host) || dashed(host) {
        return Some(HOST);
    }
    percent_decode(path).starts_with('-').then_some(PATH)
}

/// `text` with its `%XX` escapes undone (an escape that is not valid stays as written).
fn percent_decode(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(super) fn is_absolute_path(url: &str) -> bool {
    let b = url.as_bytes();
    url.starts_with('/')
        || url.starts_with("\\\\")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'/' | b'\\'))
        || std::path::Path::new(url).is_absolute()
}

/// What is wrong with a git url, if anything. It goes to `git` as an argument, so it must not
/// look like an option, and a relative path would mean a different place for each folder the
/// tool is started from.
fn bad_url(url: &str, span: Span) -> Option<Diagnostic> {
    if url.trim().is_empty() {
        return Some(Diagnostic::error("`git` is empty", span));
    }
    if let Some(problem) = super::git::transport_problem(url) {
        return Some(
            Diagnostic::error(problem, span)
                .with_hint("use an `https://`, `ssh://`, `git://` or `file://` url, or a path"),
        );
    }
    if url.starts_with('-') || url.chars().any(|c| c.is_control() || c == ' ') {
        return Some(Diagnostic::error(
            format!("`{}` is not a git url", super::git::redact_url(url)),
            span,
        ));
    }
    if url == "." || url == ".." || url.starts_with("./") || url.starts_with("../") {
        return Some(
            Diagnostic::error(
                format!(
                    "the git url `{}` is a relative path",
                    super::git::redact_url(url)
                ),
                span,
            )
                .with_hint("write an absolute path or a `file://` url; for a folder inside this repository use `path`"),
        );
    }
    classify(url).err().map(|(message, hint)| {
        let d = Diagnostic::error(message, span);
        match hint {
            Some(hint) => d.with_hint(hint),
            None => d,
        }
    })
}

fn bad_spec(key: &str, value: &str, span: Span) -> Option<Diagnostic> {
    if value.is_empty() {
        return Some(Diagnostic::error(format!("`{key}` is empty"), span));
    }
    if key == "rev" {
        let hex = value.chars().all(|c| c.is_ascii_hexdigit());
        if !hex || !(7..=40).contains(&value.len()) {
            return Some(
                Diagnostic::error(format!("`{value}` is not a commit hash"), span).with_hint(
                    "`rev` is 7 to 40 hexadecimal digits; for a name use `tag` or `branch`",
                ),
            );
        }
    } else if !valid_ref_name(value) {
        return Some(
            Diagnostic::error(format!("`{value}` is not a valid {key} name"), span)
                .with_hint("it must be a name `git check-ref-format` accepts"),
        );
    }
    None
}

/// Whether `name` is a branch or tag name by the rules of `git check-ref-format` (and does not
/// start with `-`, which `git` would read as an option).
pub fn valid_ref_name(name: &str) -> bool {
    if name.is_empty()
        || name == "@"
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
    {
        return false;
    }
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return false;
    }
    name.split('/')
        .all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
}

/// Whether `s` can be a package or dependency name: what an `import` can say (so not a
/// keyword either: `import fn` does not parse).
pub fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
        && colibri_syntax::TokenKind::keyword(s).is_none()
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
        let paths: Vec<&str> = m
            .dependencies
            .iter()
            .filter_map(|d| match &d.source {
                Source::Path { path, .. } => Some(path.as_str()),
                Source::Git(_) => None,
            })
            .collect();
        assert_eq!(paths, ["../physics", "libs/audio"]);
    }

    #[test]
    fn the_build_table_lists_asset_patterns() {
        let m = ok(
            "[package]\nname = \"x\"\n\n[build]\nassets = [\"data/\", \"img/*.png\", \"logo.svg\"]\n",
        );
        let texts: Vec<&str> = m.build.assets.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, ["data/", "img/*.png", "logo.svg"]);
        assert!(ok("[package]\nname = \"x\"\n").build.assets.is_empty());
        assert!(
            ok("[package]\nname = \"x\"\n[build]\n")
                .build
                .assets
                .is_empty()
        );
    }

    #[test]
    fn a_bad_build_table_is_named() {
        let head = "[package]\nname = \"x\"\n[build]\n";
        let one = |body: &str| errs(&format!("{head}{body}")).join("; ");
        assert!(one("assets = \"data/\"").contains("must be a list"));
        // The reader of `colibri.toml` only knows arrays of strings, and says so first.
        assert!(one("assets = [1]").contains("holds strings"));
        assert!(one("assets = [\"../x\"]").contains("not a valid asset pattern"));
        assert!(one("assets = [\"/etc\"]").contains("not a valid asset pattern"));
        assert!(one("assets = [\"a\\\\b\"]").contains("not a valid asset pattern"));
        assert!(one("other = \"x\"").contains("unknown key `other` in `[build]`"));
    }

    #[test]
    fn a_manifest_without_dependencies_is_fine() {
        assert!(ok("[package]\nname = \"x\"\n").dependencies.is_empty());
    }

    fn git_dep(text: &str) -> GitSource {
        let m = ok(&format!(
            "[package]\nname = \"x\"\n[dependencies]\n{text}\n"
        ));
        match &m.dependencies[0].source {
            Source::Git(g) => g.clone(),
            other => panic!("not a git dependency: {other:?}"),
        }
    }

    fn dep_errs(text: &str) -> Vec<String> {
        errs(&format!(
            "[package]\nname = \"x\"\n[dependencies]\n{text}\n"
        ))
    }

    #[test]
    fn git_dependencies_with_a_rev_a_tag_a_branch_or_nothing() {
        let g = git_dep("foo = { git = \"https://example.com/foo\" }");
        assert_eq!(g.url, "https://example.com/foo");
        assert_eq!(g.spec, GitSpec::DefaultBranch);
        let g = git_dep("foo = { git = \"/srv/foo\", rev = \"abc1234\" }");
        assert_eq!(g.spec, GitSpec::Rev("abc1234".into()));
        let g = git_dep("foo = { git = \"file:///srv/foo\", tag = \"v1.2.0\" }");
        assert_eq!(g.spec, GitSpec::Tag("v1.2.0".into()));
        let g = git_dep("foo = { git = \"git@host:a/foo.git\", branch = \"dev/x\" }");
        assert_eq!(g.spec, GitSpec::Branch("dev/x".into()));
        assert_eq!(g.spec.describe(), "branch `dev/x`");
    }

    #[test]
    fn bad_git_dependencies_each_have_a_message() {
        let both = dep_errs("foo = { git = \"/u\", rev = \"abc1234\", tag = \"v1\" }");
        assert!(both[0].contains("both `rev` and `tag`"), "{both:?}");
        assert!(
            dep_errs("foo = { git = \"/u\", tag = \"a\", branch = \"b\" }")[0]
                .contains("only one of")
        );
        assert!(
            dep_errs("foo = { git = \"/u\", path = \"p\" }")[0].contains("both `path` and `git`")
        );
        assert!(dep_errs("foo = { rev = \"abc1234\" }")[0].contains("needs a `git` url"));
        assert!(
            dep_errs("foo = { path = \"p\", tag = \"v1\" }")[0].contains("only goes with `git`")
        );
        assert!(
            dep_errs("foo = { git = \"/u\", rev = \"main\" }")[0].contains("not a commit hash")
        );
        assert!(dep_errs("foo = { git = \"/u\", rev = \"abc\" }")[0].contains("not a commit hash"));
        assert!(dep_errs("foo = { git = \"/u\", tag = \"\" }")[0].contains("is empty"));
        assert!(
            dep_errs("foo = { git = \"/u\", branch = \"-x\" }")[0].contains("not a valid branch")
        );
        assert!(dep_errs("foo = { git = \"\" }")[0].contains("`git` is empty"));
        assert!(dep_errs("foo = { git = \"--upload-pack=x\" }")[0].contains("not a git url"));
        assert!(dep_errs("foo = { git = \"ext::sh -c whoami\" }")[0].contains("remote helper"));
        assert!(dep_errs("foo = { git = \"fd::17\" }")[0].contains("remote helper"));
        assert!(dep_errs("foo = { git = \"http://h/r\" }")[0].contains("transport `http`"));
        assert!(dep_errs("foo = { git = \"../foo\" }")[0].contains("relative path"));
        assert!(dep_errs("foo = { git = true }")[0].contains("must be a string"));
        assert!(
            dep_errs("foo = { git = \"/u\", tag = true }")[0].contains("`tag` must be a string")
        );
    }

    #[test]
    fn registry_dependencies_are_refused_clearly() {
        let e = dep_errs("foo = { version = \"1\" }");
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
        assert!(errs("[package]\nname = \"if\"\n")[0].contains("not a valid package name"));
        assert!(!is_name("fn") && !is_name("true") && is_name("fnord") && is_name("game_2"));
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
            errs("[package]\nname = \"x\"\n[dependencies]\nfoo = { }\n")[0]
                .contains("no `path` or `git`")
        );
        assert!(
            errs("[package]\nname = \"x\"\n[dependencies]\nfoo = { path = \"a\", x = \"b\" }\n")[0]
                .contains("unknown key `x`")
        );
        assert!(errs("[package]\nname = \"x\"\n[dependencies]\nfoo = { path = \"a\" }\n[dependencies.foo]\npath = \"b\"\n")[0].contains("more than once"));
        assert!(errs("a = 1")[0].contains("numbers"));
    }

    #[test]
    fn a_git_url_is_a_url_an_scp_address_or_an_absolute_path() {
        for (url, kind) in [
            ("https://example.com/r.git", UrlKind::Remote),
            ("ssh://git@host/r", UrlKind::Remote),
            ("git://host/r", UrlKind::Remote),
            ("git@host:me/r.git", UrlKind::Remote),
            ("file:///srv/r", UrlKind::Local),
            ("/srv/r", UrlKind::Local),
            ("C:\\repos\\r", UrlKind::Local),
            ("C:/repos/r", UrlKind::Local),
        ] {
            assert_eq!(classify_url(url), Ok(kind), "{url}");
        }
        for bad in [
            "r4",
            "sub/repo",
            ".git",
            "..\\x",
            "host:path",
            "@host:p",
            "u@-oProxy:p",
            "u@host:",
            "https://",
            "x/y:z",
        ] {
            let e = classify_url(bad).unwrap_err();
            assert!(
                e.contains("not a git url") || e.contains("not allowed"),
                "{bad}: {e}"
            );
        }
        for bad in ["r4", "sub/repo", ".git", "..\\x"] {
            let e = dep_errs(&format!(
                "foo = {{ git = \"{}\" }}",
                bad.replace('\\', "\\\\")
            ));
            assert!(e[0].contains("not a git url"), "{bad}: {e:?}");
        }
    }

    #[test]
    fn an_ssh_or_scp_url_that_looks_like_an_option_is_refused() {
        for bad in [
            "ssh://-oProxyCommand=x/r",
            "ssh://-oProxyCommand=x",
            "ssh://git@-oProxyCommand=x/r",
            "SSH://-v@host/r",
            "u@h:-oProxyCommand=x",
            "git@-host:r.git",
            "-u@host:r.git",
        ] {
            let e = classify_url(bad).unwrap_err();
            assert!(
                e.contains("not a git url") && e.contains("`-`"),
                "{bad}: {e}"
            );
            let d = dep_errs(&format!("foo = {{ git = \"{bad}\" }}"));
            assert_eq!(d.len(), 1, "{bad}: {d:?}");
            assert!(d[0].contains("not a git url"), "{bad}: {d:?}");
        }
        for ok in [
            "ssh://git@host/-r",
            "ssh://git@host:22/r",
            "git@host:me/-r.git",
            "https://host/-x",
        ] {
            assert!(classify_url(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn an_option_hidden_by_brackets_or_escapes_or_a_port_is_refused() {
        for bad in [
            "ssh://[-oProxyCommand=x]/r",
            "ssh://%2doProxyCommand=x/r",
            "ssh://git@%2DoProxyCommand=x/r",
            "git@[-oProxyCommand=x]:r",
            "git@%2doProxyCommand=x:r",
            "u@h:%2doProxyCommand=x",
        ] {
            let e = classify_url(bad).unwrap_err();
            assert!(
                e.contains("not a git url") && e.contains("`-`"),
                "{bad}: {e}"
            );
            let d = dep_errs(&format!("foo = {{ git = \"{bad}\" }}"));
            assert_eq!(d.len(), 1, "{bad}: {d:?}");
        }
        for bad in ["ssh://h:-p/r", "ssh://git@h:-oX/r", "ssh://h:2a2/r"] {
            let e = classify_url(bad).unwrap_err();
            assert!(e.contains("the port is not a number"), "{bad}: {e}");
        }
        for ok in [
            "ssh://h:22/r",
            "ssh://git@h:2222/r",
            "ssh://[::1]:22/r",
            "ssh://h/r",
        ] {
            assert!(classify_url(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn each_url_error_has_only_the_hint_that_fits_it() {
        let hint = |url: &str| {
            let text =
                format!("[package]\nname = \"x\"\n[dependencies]\nfoo = {{ git = \"{url}\" }}\n");
            let d = parse(FileId(0), &text).expect_err("a bad url").remove(0);
            d.hint
        };
        // A relative path: the hint about the folder the tool is started in.
        let h = hint("sub/repo").expect("a hint");
        assert!(h.contains("different place"), "{h}");
        // Not a path at all: nothing about relative paths.
        for url in [
            "ssh://-oX/r",
            "ssh://h:-p/r",
            "git@[-oX]:r",
            "ext::sh -c x",
            "https://",
        ] {
            let h = hint(url);
            assert!(
                h.as_deref().is_none_or(|h| !h.contains("different place")),
                "{url}: {h:?}"
            );
        }
    }

    #[test]
    fn credentials_in_a_git_url_are_refused_without_printing_them() {
        for url in [
            "https://user:s3cret@host/r.git",
            "https://s3cret@host/r.git",
            "HTTPS://user:s3cret@host/r.git",
            "ssh://git:s3cret@host/r.git",
        ] {
            let e = dep_errs(&format!("foo = {{ git = \"{url}\" }}"));
            assert_eq!(e.len(), 1, "{url}: {e:?}");
            assert!(e[0].contains("user name and password"), "{}", e[0]);
            assert!(!e[0].contains("s3cret"), "{}", e[0]);
        }
        let d = parse(
            FileId(0),
            "[package]\nname = \"x\"\n[dependencies]\nfoo = { git = \"https://user:s3cret@host/r.git\" }\n",
        )
        .unwrap_err();
        let hint = d[0].hint.clone().unwrap();
        assert!(
            hint.contains("credential helper") && !hint.contains("s3cret"),
            "{hint}"
        );
        // A user name for ssh and the scp form are fine.
        for url in [
            "ssh://git@host/r.git",
            "git@host:me/r.git",
            "https://host/r.git",
        ] {
            assert_eq!(git_dep(&format!("foo = {{ git = \"{url}\" }}")).url, url);
        }
    }

    #[test]
    fn an_upper_case_scheme_is_written_in_lower_case() {
        let m = ok(
            "[package]\nname = \"x\"\n\n[dependencies]\nfoo = { git = \"HTTPS://Example.com/Foo\" }\nbar = { git = \"FILE:///tmp/Bar\" }\n",
        );
        let urls: Vec<&str> = m
            .dependencies
            .iter()
            .filter_map(|d| match &d.source {
                Source::Git(g) => Some(g.url.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(urls, ["https://Example.com/Foo", "file:///tmp/Bar"]);
    }

    #[test]
    fn allow_ffi_lists_package_names() {
        let m = ok("[package]\nname = \"x\"\nallow-ffi = [\"gfx\", \"audio\"]\n");
        assert_eq!(m.allow_ffi, ["gfx", "audio"]);
        assert!(ok("[package]\nname = \"x\"\n").allow_ffi.is_empty());
        assert!(
            errs("[package]\nname = \"x\"\nallow-ffi = \"gfx\"\n")[0].contains("must be a list")
        );
        assert!(
            errs("[package]\nname = \"x\"\nallow-ffi = [\"a b\"]\n")[0]
                .contains("lists package names")
        );
    }

    #[test]
    fn branch_and_tag_names_follow_git_check_ref_format() {
        for ok in [
            "main",
            "v1.2.0",
            "dev/x",
            "release-1_2",
            "a@b",
            "feature/x.y",
            "ünï",
        ] {
            assert!(valid_ref_name(ok), "{ok}");
        }
        for bad in [
            "", "@", "-x", "a b", "a~1", "a^", "a:b", "a?", "a*", "a[0]", "a\\b", "a..b", "a//b",
            "/a", "a/", "a.", "a.lock", "x/a.lock", "x.lock/a", ".hidden", "a/.b", "a@{1}", "a\tb",
            "a\u{7f}",
        ] {
            assert!(!valid_ref_name(bad), "{bad:?}");
        }
        assert!(
            dep_errs("foo = { git = \"/u\", tag = \"v1.lock\" }")[0].contains("not a valid tag")
        );
        assert!(
            dep_errs("foo = { git = \"/u\", branch = \"a@{1}\" }")[0]
                .contains("not a valid branch")
        );
    }

    #[test]
    fn a_dependency_may_ask_for_features_and_turn_defaults_off() {
        let m = ok(
            "[package]\nname = \"a\"\n\n[dependencies]\nlib = { path = \"../lib\", features = [\"x\", \"y\"], default-features = false }\nother = { path = \"../o\" }\n",
        );
        let lib = &m.dependencies[0];
        let names: Vec<&str> = lib.features.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["x", "y"]);
        assert!(!lib.default_features);
        let other = &m.dependencies[1];
        assert!(other.features.is_empty() && other.default_features);
    }

    #[test]
    fn bad_dependency_features_are_errors() {
        let bad = |dep: &str| {
            let text = format!(
                "[package]\nname = \"a\"\n\n[dependencies]\nlib = {{ path = \"../lib\", {dep} }}\n"
            );
            parse(FileId(0), &text).unwrap_err()[0].message.clone()
        };
        assert_eq!(
            bad("features = \"x\""),
            "`features` must be a list of feature names"
        );
        assert!(bad("features = [\"debug\"]").contains("`debug` is not a valid feature name"));
        assert_eq!(
            bad("default-features = \"no\""),
            "`default-features` must be `true` or `false`"
        );
    }

    fn rust(text: &str) -> Vec<RustDependency> {
        ok(&format!(
            "[package]\nname = \"x\"\n[rust-dependencies]\n{text}\n"
        ))
        .rust_dependencies
    }

    fn rust_errs(text: &str) -> Vec<String> {
        errs(&format!(
            "[package]\nname = \"x\"\n[rust-dependencies]\n{text}\n"
        ))
    }

    /// The message and the text its span covers, for the first error.
    fn rust_err_at(text: &str) -> (String, String) {
        let full = format!("[package]\nname = \"x\"\n[rust-dependencies]\n{text}\n");
        let d = parse(FileId(0), &full).expect_err("an error").remove(0);
        let at = full[d.span.start as usize..d.span.end as usize].to_string();
        (d.message, at)
    }

    #[test]
    fn rust_dependencies_in_each_form() {
        let all = rust(
            "rapier = { binding = \"colibri-rapier\", version = \"0.1\" }\nnative = { path = \"native/\" }\nextra = { git = \"https://example.com/x.git\", rev = \"abc1234\", crate = \"x\" }",
        );
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].name, "rapier");
        assert_eq!(
            all[0].source,
            RustSource::Binding {
                crate_name: "colibri-rapier".into(),
                version: "0.1".into()
            }
        );
        assert_eq!(
            all[1].source,
            RustSource::Path {
                path: "native/".into()
            }
        );
        assert_eq!(
            all[2].source,
            RustSource::Git {
                url: "https://example.com/x.git".into(),
                spec: GitSpec::Rev("abc1234".into()),
                crate_name: "x".into()
            }
        );
        assert!(
            all.iter()
                .all(|r| r.default_features && r.features.is_empty())
        );
    }

    #[test]
    fn rust_git_takes_a_tag_a_branch_or_nothing() {
        let spec = |extra: &str| match &rust(&format!(
            "x = {{ git = \"https://example.com/x.git\", crate = \"x\"{extra} }}"
        ))[0]
            .source
        {
            RustSource::Git { spec, .. } => spec.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(spec(""), GitSpec::DefaultBranch);
        assert_eq!(spec(", tag = \"v1\""), GitSpec::Tag("v1".into()));
        assert_eq!(spec(", branch = \"dev\""), GitSpec::Branch("dev".into()));
    }

    #[test]
    fn rust_dependency_as_its_own_table_with_features() {
        let m = ok(
            "[package]\nname = \"x\"\n[rust-dependencies.native]\npath = \"native\"\nfeatures = [\"a\", \"b\"]\ndefault-features = false\n",
        );
        let r = &m.rust_dependencies[0];
        assert_eq!(r.name, "native");
        let names: Vec<&str> = r.features.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert!(!r.default_features);
    }

    #[test]
    fn no_rust_dependencies_by_default() {
        let m = ok("[package]\nname = \"x\"\n");
        assert!(m.rust_dependencies.is_empty() && m.allow_rust.is_empty());
    }

    #[test]
    fn allow_rust_lists_names_with_their_places() {
        let m = ok("[package]\nname = \"x\"\nallow-rust = [\"physics\", \"audio\"]\n");
        assert_eq!(m.allow_rust, ["physics", "audio"]);
        assert_eq!(m.allow_rust_spans.len(), 2);
        assert!(
            errs("[package]\nname = \"x\"\nallow-rust = \"a\"\n")[0].contains("must be a list")
        );
        assert!(
            errs("[package]\nname = \"x\"\nallow-rust = [\"fn\"]\n")[0]
                .contains("lists package names")
        );
    }

    #[test]
    fn rust_unknown_keys_are_reported_at_the_key() {
        let (m, at) = rust_err_at("n = { path = \"p\", colour = \"red\" }");
        assert!(m.contains("unknown key `colour`"), "{m}");
        assert_eq!(at, "colour");
        // `registry` and `optional` are cargo's, not ours.
        assert!(rust_errs("n = { path = \"p\", optional = true }")[0].contains("unknown key"));
    }

    #[test]
    fn rust_needs_exactly_one_source() {
        let (m, at) = rust_err_at("n = { features = [\"a\"] }");
        assert!(m.contains("has no `binding`, `path` or `git`"), "{m}");
        assert!(at.contains("features"), "{at}");
        let (m, at) = rust_err_at("n = { path = \"p\", binding = \"c\", version = \"1\" }");
        assert!(m.contains("both `path` and `binding`"), "{m}");
        assert_eq!(at, "binding");
        let (m, at) =
            rust_err_at("n = { path = \"p\", git = \"https://example.com/x.git\", crate = \"c\" }");
        assert!(m.contains("both `path` and `git`"), "{m}");
        assert_eq!(at, "git");
        assert!(rust_errs("n = \"p\"")[0].contains("must say where the crate is"));
    }

    #[test]
    fn rust_binding_needs_a_version_and_only_it_takes_one() {
        let (m, at) = rust_err_at("n = { binding = \"colibri-x\" }");
        assert!(m.contains("`binding` but no `version`"), "{m}");
        assert_eq!(at, "binding");
        let (m, at) = rust_err_at("n = { path = \"p\", version = \"1\" }");
        assert!(
            m.contains("`version`") && m.contains("only goes with `binding`"),
            "{m}"
        );
        assert_eq!(at, "version");
    }

    #[test]
    fn rust_git_needs_a_crate_and_only_it_takes_one() {
        let (m, at) = rust_err_at("n = { git = \"https://example.com/x.git\" }");
        assert!(m.contains("`git` but no `crate`"), "{m}");
        assert_eq!(at, "git");
        let (m, at) = rust_err_at("n = { binding = \"c\", version = \"1\", crate = \"c\" }");
        assert!(
            m.contains("`crate`") && m.contains("only goes with `git`"),
            "{m}"
        );
        assert_eq!(at, "crate");
        let (m, at) = rust_err_at("n = { path = \"p\", rev = \"abc1234\" }");
        assert!(
            m.contains("`rev`") && m.contains("only goes with `git`"),
            "{m}"
        );
        assert_eq!(at, "rev");
        let (m, _) = rust_err_at(
            "n = { git = \"https://example.com/x.git\", crate = \"c\", rev = \"abc1234\", tag = \"t\" }",
        );
        assert!(
            m.contains("use only one of `rev`, `tag` and `branch`"),
            "{m}"
        );
        // The url and revision rules of `[dependencies]` apply too.
        assert!(rust_errs("n = { git = \"../x\", crate = \"c\" }")[0].contains("relative path"));
        assert!(
            rust_errs("n = { git = \"https://u:s3cret@example.com/x\", crate = \"c\" }")[0]
                .contains("user name and password")
        );
        assert!(
            rust_errs("n = { git = \"https://example.com/x\", crate = \"c\", rev = \"zz\" }")[0]
                .contains("not a commit hash")
        );
    }

    #[test]
    fn rust_key_must_be_a_module_name() {
        for key in ["fn", "my-lib", "1x", "a.b"] {
            // A dotted key is a nested table in TOML, so quote it.
            let (m, at) = rust_err_at(&format!("\"{key}\" = {{ path = \"p\" }}"));
            assert!(
                m.contains("not a valid name for a Rust dependency"),
                "{key}: {m}"
            );
            assert!(at.contains(key), "{key}: {at}");
        }
    }

    #[test]
    fn rust_key_may_not_repeat_or_match_a_dependency() {
        let (m, at) = rust_err_at("n = { path = \"a\" }\n[rust-dependencies.n]\npath = \"b\"");
        assert!(m.contains("listed more than once"), "{m}");
        assert!(at.contains("rust-dependencies.n"), "{at}");
        let text = "[package]\nname = \"x\"\n[dependencies]\nnative = { path = \"../n\" }\n[rust-dependencies]\nnative = { path = \"native\" }\n";
        let d = parse(FileId(0), text).expect_err("a clash").remove(0);
        assert!(
            d.message
                .contains("both a dependency and a Rust dependency")
        );
        assert_eq!(&text[d.span.start as usize..d.span.end as usize], "native");
        // The rust key, not the dependency key, is the one the error points at.
        assert!(d.span.start as usize > text.find("[rust-dependencies]").unwrap());
    }

    #[test]
    fn one_crate_under_two_keys_is_an_error_at_the_second() {
        let text = "[package]\nname = \"x\"\n[rust-dependencies]\na = { binding = \"colibri-x\", version = \"0.1\" }\nb = { binding = \"colibri-x\", version = \"0.2\" }\n";
        let d = parse(FileId(0), text).expect_err("same crate").remove(0);
        assert!(
            d.message.contains("`b` is the same crate as `a`"),
            "{}",
            d.message
        );
        assert_eq!(&text[d.span.start as usize..d.span.end as usize], "b");
        // The same folder written two ways, and the same crate from the registry and from git.
        assert!(
            rust_errs("a = { path = \"native\" }\nb = { path = \"./native/\" }")[0]
                .contains("same crate")
        );
        assert!(
            rust_errs(
                "a = { binding = \"c\", version = \"1\" }\nb = { binding = \"c\", version = \"1\" }"
            )[0]
            .contains("same crate")
        );
        // Different crates, or the same name from another place, are fine.
        assert_eq!(
            rust("a = { path = \"one\" }\nb = { path = \"two\" }").len(),
            2
        );
    }

    #[test]
    fn a_git_crate_is_the_same_with_or_without_dot_git_and_a_slash() {
        let e = rust_errs(
            "a = { git = \"https://example.com/r.git\", crate = \"c\" }\nb = { git = \"https://example.com/r/\", crate = \"c\" }",
        );
        assert!(e[0].contains("same crate"), "{e:?}");
        // Another crate of the same repository is another crate.
        assert_eq!(
            rust("a = { git = \"https://example.com/r\", crate = \"c\" }\nb = { git = \"https://example.com/r\", crate = \"d\" }")
                .len(),
            2
        );
    }
}
