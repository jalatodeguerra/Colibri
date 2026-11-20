//! `colibri.lock` (ARCHITECTURE §11.6b): the commit each git dependency was resolved to.
//!
//! It is the same TOML subset as `colibri.toml`, one table per git dependency (direct or
//! transitive), sorted by name, so the text is the same on every run:
//!
//! ```toml
//! [git.physics]
//! url = "https://example.com/physics.git"
//! tag = "v1.2.0"
//! commit = "5f2a9c1e0b7d4a6c8e3f1a2b4c5d6e7f8091a2b3"
//! ```
//!
//! `also = ["branch:main"]` lists further specs that other dependents of the same package use
//! for the same commit.
//!
//! `rev`, `tag` or `branch` repeats what `colibri.toml` asked for (none for the default
//! branch): an entry is used only while it still matches the manifest.

use std::collections::BTreeMap;

use colibri_syntax::{Diagnostic, FileId};

use super::manifest::{GitSource, GitSpec};
use super::toml::{self, ValueKind};

/// The lock file's name; it sits next to the root package's `colibri.toml`.
pub const LOCK_FILE: &str = "colibri.lock";

/// What to do about a lock file that cannot be used: it is only a cache of resolutions.
pub const RECREATE_HINT: &str = "delete `colibri.lock` and run `colibri update` to recreate it";

/// A parse error of the lock file as one line of text: where it is, what is wrong, and what
/// to do about it.
pub fn describe_error(text: &str, error: &Diagnostic) -> String {
    let start = (error.span.start as usize).min(text.len());
    let line = text.as_bytes()[..start]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1;
    let hint = match &error.hint {
        Some(h) => format!("{h}; {RECREATE_HINT}"),
        None => RECREATE_HINT.to_string(),
    };
    format!("line {line}: {} ({hint})", error.message)
}

const HEADER: &str = "# colibri.lock: written by `colibri`; do not edit by hand.\n\
# It pins every git dependency to the commit it was resolved to. `colibri update` moves it.\n";

/// One pinned git dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockEntry {
    pub url: String,
    pub spec: GitSpec,
    /// Other specs that two dependents of one package use for the same commit (`tag = "v1"`
    /// here, `branch = "main"` there): sorted, and without `spec`.
    pub also: Vec<GitSpec>,
    /// The full 40-digit commit hash.
    pub commit: String,
}

impl LockEntry {
    /// Whether the manifest still asks for what this entry resolved.
    ///
    /// For a `rev` the commit must also start with it: the entry is what pins the dependency,
    /// so an edit of the lock alone must not be able to point a `rev` at another commit.
    pub fn matches(&self, source: &GitSource) -> bool {
        self.url == source.url && self.asks(&source.spec) && self.commit_fits(&source.spec)
    }

    /// Whether `spec` is one of the specs this entry was made for.
    pub fn asks(&self, spec: &GitSpec) -> bool {
        self.spec == *spec || self.also.contains(spec)
    }

    /// Whether the commit is one that `spec` can mean (only a `rev` says so by itself).
    pub fn commit_fits(&self, spec: &GitSpec) -> bool {
        match spec {
            GitSpec::Rev(rev) => self.commit.starts_with(&rev.to_ascii_lowercase()),
            _ => true,
        }
    }

    /// Records one more spec that means the same commit.
    pub fn add_also(&mut self, spec: GitSpec) {
        if !self.asks(&spec) {
            self.also.push(spec);
            self.also.sort_by_key(|s| s.describe());
        }
    }
}

/// `tag:v1`, `branch:main`, `rev:abc1234`, `default`: a spec in the `also` list.
fn spec_text(spec: &GitSpec) -> String {
    match spec.key_value() {
        None => "default".to_string(),
        Some((k, v)) => format!("{k}:{v}"),
    }
}

fn parse_spec_text(text: &str) -> Option<GitSpec> {
    if text == "default" {
        return Some(GitSpec::DefaultBranch);
    }
    let (kind, value) = text.split_once(':')?;
    let value = value.to_string();
    match kind {
        "rev" => Some(GitSpec::Rev(value)),
        "tag" => Some(GitSpec::Tag(value)),
        "branch" => Some(GitSpec::Branch(value)),
        _ => None,
    }
}

/// The contents of `colibri.lock`, by dependency name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lock {
    pub entries: BTreeMap<String, LockEntry>,
}

impl Lock {
    pub fn parse(file: FileId, text: &str) -> Result<Lock, Diagnostic> {
        Self::from_tables(toml::read_config(file, text, LOCK_FILE).tables)
    }

    /// The lock that the tables of a `colibri.lock` read by [`toml::read_config`] mean; of
    /// several errors, the first.
    pub fn from_tables(
        tables: Result<Vec<toml::Table>, Vec<Diagnostic>>,
    ) -> Result<Lock, Diagnostic> {
        let tables = tables.map_err(|mut errors| errors.swap_remove(0))?;
        let mut lock = Lock::default();
        for table in &tables {
            let path: Vec<&str> = table.path.iter().map(String::as_str).collect();
            let name = match path.as_slice() {
                [] => {
                    if let Some(e) = table.entries.first() {
                        return Err(Diagnostic::error(
                            format!("`{}` must be inside a `[git.name]` table", e.key),
                            e.key_span,
                        ));
                    }
                    continue;
                }
                ["git", name] => *name,
                _ => {
                    return Err(Diagnostic::error(
                        format!(
                            "unknown table `[{}]` in `{LOCK_FILE}`",
                            table.path.join(".")
                        ),
                        table.span,
                    )
                    .with_hint("a lock file has `[git.name]` tables"));
                }
            };
            let (mut url, mut commit, mut spec) = (None, None, None);
            let mut also = Vec::new();
            for entry in &table.entries {
                if entry.key == "also" {
                    let ValueKind::Array(items) = &entry.value.kind else {
                        return Err(Diagnostic::error(
                            "`also` must be a list of strings",
                            entry.value.span,
                        ));
                    };
                    for item in items {
                        let parsed = match &item.kind {
                            ValueKind::Str(t) => parse_spec_text(t),
                            _ => None,
                        };
                        let Some(spec) = parsed else {
                            return Err(Diagnostic::error(
                                "an `also` entry is `tag:name`, `branch:name`, `rev:hash` or `default`",
                                item.span,
                            ));
                        };
                        also.push(spec);
                    }
                    continue;
                }
                let ValueKind::Str(value) = &entry.value.kind else {
                    return Err(Diagnostic::error(
                        format!("`{}` must be a string", entry.key),
                        entry.value.span,
                    ));
                };
                match entry.key.as_str() {
                    "url" => url = Some(value.clone()),
                    "commit" => commit = Some((value.clone(), entry.value.span)),
                    "rev" => spec = Some(GitSpec::Rev(value.clone())),
                    "tag" => spec = Some(GitSpec::Tag(value.clone())),
                    "branch" => spec = Some(GitSpec::Branch(value.clone())),
                    other => {
                        return Err(Diagnostic::error(
                            format!("unknown key `{other}` in `[git.{name}]`"),
                            entry.key_span,
                        ));
                    }
                }
            }
            let (Some(url), Some((commit, commit_span))) = (url, commit) else {
                return Err(Diagnostic::error(
                    format!("`[git.{name}]` needs a `url` and a `commit`"),
                    table.span,
                ));
            };
            if commit.len() != 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(Diagnostic::error(
                    format!("`{commit}` is not a full commit hash"),
                    commit_span,
                ));
            }
            lock.entries.insert(name.to_string(), {
                let spec = spec.unwrap_or(GitSpec::DefaultBranch);
                also.retain(|a| *a != spec);
                also.sort_by_key(|s: &GitSpec| s.describe());
                also.dedup();
                LockEntry {
                    url,
                    spec,
                    also,
                    commit,
                }
            });
        }
        Ok(lock)
    }

    /// The file's text: deterministic, so a lock that did not change is not rewritten.
    pub fn render(&self) -> String {
        let mut out = String::from(HEADER);
        for (name, e) in &self.entries {
            out.push_str(&format!(
                "\n[git.{}]\nurl = {}\n",
                key(name),
                quote(&super::git::redact_url(&e.url))
            ));
            if let Some((k, v)) = e.spec.key_value() {
                out.push_str(&format!("{k} = {}\n", quote(v)));
            }
            if !e.also.is_empty() {
                let also: Vec<String> = e.also.iter().map(|s| quote(&spec_text(s))).collect();
                out.push_str(&format!("also = [{}]\n", also.join(", ")));
            }
            out.push_str(&format!("commit = \"{}\"\n", e.commit));
        }
        out
    }
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A table key: bare when TOML allows it, quoted otherwise.
fn key(name: &str) -> String {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        name.to_string()
    } else {
        quote(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C1: &str = "5f2a9c1e0b7d4a6c8e3f1a2b4c5d6e7f8091a2b3";
    const C2: &str = "0123456789abcdef0123456789abcdef01234567";

    fn entry(url: &str, spec: GitSpec, commit: &str) -> LockEntry {
        LockEntry {
            url: url.into(),
            spec,
            also: Vec::new(),
            commit: commit.into(),
        }
    }

    #[test]
    fn a_lock_renders_sorted_and_reads_back() {
        let mut lock = Lock::default();
        lock.entries.insert(
            "zeta".into(),
            entry("https://x/z.git", GitSpec::Tag("v1".into()), C1),
        );
        lock.entries
            .insert("alpha".into(), entry("/srv/a", GitSpec::DefaultBranch, C2));
        lock.entries.insert(
            "mid".into(),
            entry("file:///m", GitSpec::Rev("0123456".into()), C2),
        );
        let text = lock.render();
        assert!(text.find("[git.alpha]").unwrap() < text.find("[git.mid]").unwrap());
        assert!(text.find("[git.mid]").unwrap() < text.find("[git.zeta]").unwrap());
        assert!(text.contains("tag = \"v1\"\ncommit = \""));
        let alpha = text.split("[git.alpha]").nth(1).unwrap();
        assert!(!alpha.split("[git.").next().unwrap().contains("tag"));
        assert_eq!(Lock::parse(FileId(0), &text).unwrap(), lock);
        assert_eq!(Lock::parse(FileId(0), &text).unwrap().render(), text);
    }

    #[test]
    fn urls_with_quotes_and_backslashes_survive() {
        let mut lock = Lock::default();
        lock.entries.insert(
            "a".into(),
            entry("C:\\repos\\\"a\"", GitSpec::DefaultBranch, C1),
        );
        assert_eq!(Lock::parse(FileId(0), &lock.render()).unwrap(), lock);
    }

    #[test]
    fn a_syntax_error_in_the_lock_names_the_lock_and_in_the_manifest_the_manifest() {
        let lock = |t: &str| Lock::parse(FileId(0), t).unwrap_err().message;
        for text in ["[git.a]\nurl = 1\n", "[git.a]\nurl = [1]\n"] {
            let m = lock(text);
            assert!(
                m.contains("`colibri.lock`") && !m.contains("colibri.toml"),
                "{m}"
            );
        }
        for text in ["[package]\nname = 1\n", "[package]\nname = [1]\n"] {
            let m = crate::package::manifest::parse(FileId(0), text).unwrap_err()[0]
                .message
                .clone();
            assert!(
                m.contains("`colibri.toml`") && !m.contains("colibri.lock"),
                "{m}"
            );
        }
    }

    #[test]
    fn a_bad_lock_says_what_is_wrong() {
        let bad = |t: &str| Lock::parse(FileId(0), t).unwrap_err().message;
        assert!(bad("x = \"1\"").contains("must be inside"));
        assert!(bad("[deps.a]").contains("unknown table"));
        assert!(bad("[git.a]\nurl = \"u\"\n").contains("needs a `url` and a `commit`"));
        assert!(bad("[git.a]\nurl = \"u\"\ncommit = \"abc\"\n").contains("full commit hash"));
        assert!(
            bad(&format!(
                "[git.a]\nurl = \"u\"\ncommit = \"{C1}\"\nx = \"1\"\n"
            ))
            .contains("unknown key")
        );
    }

    #[test]
    fn an_entry_matches_only_what_the_manifest_still_says() {
        let e = entry("u", GitSpec::Tag("v1".into()), C1);
        let src = |url: &str, spec| GitSource {
            url: url.into(),
            url_span: colibri_syntax::Span::new(FileId(0), 0, 0),
            spec,
        };
        assert!(e.matches(&src("u", GitSpec::Tag("v1".into()))));
        assert!(!e.matches(&src("u", GitSpec::Tag("v2".into()))));
        assert!(!e.matches(&src("u", GitSpec::Branch("v1".into()))));
        assert!(!e.matches(&src("w", GitSpec::Tag("v1".into()))));
    }

    #[test]
    fn specs_that_mean_one_commit_are_kept_in_the_entry() {
        let mut e = entry("u", GitSpec::Tag("v1".into()), C1);
        e.add_also(GitSpec::Branch("main".into()));
        e.add_also(GitSpec::Tag("v1".into()));
        e.add_also(GitSpec::DefaultBranch);
        assert_eq!(e.also.len(), 2);
        let mut lock = Lock::default();
        lock.entries.insert("a".into(), e.clone());
        let text = lock.render();
        assert!(
            text.contains("also = [\"branch:main\", \"default\"]"),
            "{text}"
        );
        assert_eq!(Lock::parse(FileId(0), &text).unwrap(), lock);
        let src = |spec| GitSource {
            url: "u".into(),
            url_span: colibri_syntax::Span::new(FileId(0), 0, 0),
            spec,
        };
        assert!(e.matches(&src(GitSpec::Branch("main".into()))));
        assert!(e.matches(&src(GitSpec::Tag("v1".into()))));
        assert!(!e.matches(&src(GitSpec::Branch("dev".into()))));
        assert!(
            Lock::parse(FileId(0), "[git.a]\nurl = \"u\"\ncommit = \"5f2a9c1e0b7d4a6c8e3f1a2b4c5d6e7f8091a2b3\"\nalso = [\"nope\"]\n")
                .is_err()
        );
    }

    #[test]
    fn a_rev_entry_matches_only_a_commit_that_starts_with_the_rev() {
        let src = |rev: &str| GitSource {
            url: "u".into(),
            url_span: colibri_syntax::Span::new(FileId(0), 0, 0),
            spec: GitSpec::Rev(rev.into()),
        };
        let e = entry("u", GitSpec::Rev("5F2A9C1".into()), C1);
        assert!(e.matches(&src("5F2A9C1")));
        let moved = entry("u", GitSpec::Rev("5f2a9c1".into()), C2);
        assert!(!moved.matches(&src("5f2a9c1")));
        assert!(!moved.commit_fits(&GitSpec::Rev("5f2a9c1".into())));
    }

    #[test]
    fn a_parse_error_says_the_line_and_what_to_do() {
        let text = "[git.a]\nurl = \"u\"\nnonsense\n";
        let e = Lock::parse(FileId(0), text).unwrap_err();
        let said = describe_error(text, &e);
        assert!(said.starts_with("line 3: "), "{said}");
        assert!(
            said.contains("delete `colibri.lock` and run `colibri update`"),
            "{said}"
        );
    }
}
