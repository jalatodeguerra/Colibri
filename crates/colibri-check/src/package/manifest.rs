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
}

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
            ["package"] => package(table, &mut name, &mut ffi, &mut allow_ffi, &mut errors),
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
    })
}

fn package(
    table: &Table,
    name: &mut Option<(String, Span)>,
    ffi: &mut bool,
    allow_ffi: &mut Vec<(String, Span)>,
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
            (other, _) => errors.push(
                Diagnostic::error(
                    format!("unknown key `{other}` in `[package]`"),
                    entry.key_span,
                )
                .with_hint("the keys are `name`, `ffi` and `allow-ffi`"),
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
                .with_hint("a dependency has `path`, or `git` with `rev`, `tag` or `branch`"),
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
}
