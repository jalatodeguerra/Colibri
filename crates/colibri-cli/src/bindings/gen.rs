//! Writes the cargo workspace of a project runner: the runner is a binary whose `main` calls
//! [`crate::runner_main`] with the project's binding crates, built against the engine source of
//! [`super::engine`]. Everything here is text in, text out (apart from reading a path crate's
//! `Cargo.toml`): no cargo, no network. The text is canonical, so equal projects give equal files.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use colibri_runtime::{GitSpec, RustDependency, RustSource};
use toml::de::{DeTable, DeValue};

use super::engine::{Engine, field};

/// The name of the generated binary, in `<target>/<profile>/`.
pub const RUNNER_BIN: &str = "colibri-project-runner";

/// The engine's own `Cargo.lock`: a fresh project lock is seeded from it so that the engine's
/// third-party crates are the versions Colibri itself was built and tested with.
const OWN_LOCK: &str = include_str!("../../../../Cargo.lock");

/// Where the crate of a binding comes from, with a path made absolute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CrateSource {
    Registry { version: String },
    Path { dir: PathBuf },
    Git { url: String, spec: GitSpec },
}

/// One binding crate of the runner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingCrate {
    /// The `[rust-dependencies]` key: the module name.
    pub key: String,
    /// The cargo package name.
    pub package: String,
    pub source: CrateSource,
    pub features: Vec<String>,
    pub default_features: bool,
}

/// The files of the generated workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Generated {
    pub cargo_toml: String,
    pub main_rs: String,
    /// Colibri's own lock without the workspace crates, for a project that has no lock yet.
    pub seed_lock: String,
}

/// The `[rust-dependencies]` of a manifest as crates: path crates get their package name from
/// their `Cargo.toml` (`root` is the folder of the manifest) and are refused when cargo would
/// build two Colibri runtimes or when the crate takes its fields from a workspace. Every problem
/// is reported, none stops the others.
pub fn resolve_crates(
    deps: &[RustDependency],
    root: &Path,
) -> Result<Vec<BindingCrate>, Vec<String>> {
    let mut crates = Vec::new();
    let mut errors = Vec::new();
    for dep in deps {
        if !dep.name.is_ascii() {
            errors.push(format!(
                "the Rust dependency `{}` cannot be built: its key becomes a Cargo dependency name, which is ASCII only",
                dep.name
            ));
            continue;
        }
        let mut features: Vec<String> = dep.features.iter().map(|(f, _)| f.clone()).collect();
        features.sort();
        features.dedup();
        let (package, source) = match &dep.source {
            RustSource::Binding {
                crate_name,
                version,
            } => {
                if crate_name == "colibri-runtime" {
                    errors.push(format!(
                        "the Rust dependency `{}` is `colibri-runtime` itself: a binding is a crate that depends on it",
                        dep.name
                    ));
                    continue;
                }
                (
                    crate_name.clone(),
                    CrateSource::Registry {
                        version: version.clone(),
                    },
                )
            }
            RustSource::Git {
                url,
                spec,
                crate_name,
            } => (
                crate_name.clone(),
                CrateSource::Git {
                    url: url.clone(),
                    spec: spec.clone(),
                },
            ),
            RustSource::Path { path } => {
                let dir = root.join(path);
                match read_path_crate(&dir, path) {
                    Ok(package) => {
                        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
                        (package, CrateSource::Path { dir })
                    }
                    Err(message) => {
                        errors.push(format!("the Rust dependency `{}`: {message}", dep.name));
                        continue;
                    }
                }
            }
        };
        crates.push(BindingCrate {
            key: dep.name.clone(),
            package,
            source,
            features,
            default_features: dep.default_features,
        });
    }
    if errors.is_empty() {
        crates.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(crates)
    } else {
        Err(errors)
    }
}

/// The package name of the crate in `dir` (written as `shown` in the manifest), or why the
/// runner cannot be built with it.
fn read_path_crate(dir: &Path, shown: &str) -> Result<String, String> {
    let manifest = dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).map_err(|_| {
        format!(
            "`{shown}` has no readable `Cargo.toml` (expected `{}`)",
            display_path(&manifest)
        )
    })?;
    package_of(&text, shown)
}

/// [`read_path_crate`] on the text of the `Cargo.toml`.
fn package_of(text: &str, shown: &str) -> Result<String, String> {
    let parsed =
        DeTable::parse(text).map_err(|e| format!("`{shown}/Cargo.toml` is not valid TOML: {e}"))?;
    let table = parsed.get_ref();
    let Some(name) = field(table, "package")
        .and_then(DeValue::as_table)
        .and_then(|p| field(p, "name"))
        .and_then(DeValue::as_str)
    else {
        return Err(format!(
            "`{shown}/Cargo.toml` has no `[package] name` (a path must name a crate, not a workspace)"
        ));
    };
    if let Some(at) = inherited_field(table, "") {
        return Err(format!(
            "`{shown}/Cargo.toml` takes `{at}` from a workspace; write the fields out in the crate's own manifest (the runner builds it outside that workspace)"
        ));
    }
    if let Some(how) = names_runtime_by_path_or_git(table) {
        return Err(format!(
            "`{shown}/Cargo.toml` names `colibri-runtime` by {how}, which would build a second copy of the runtime next to the runner's; write `colibri-runtime = \"0.1\"` and the runner patches it to its own"
        ));
    }
    Ok(name.to_string())
}

/// The path of the first `workspace = true` in a manifest (`package.version`,
/// `dependencies.x`, `lints`), if any.
fn inherited_field(table: &DeTable<'_>, prefix: &str) -> Option<String> {
    for (key, value) in table {
        let key = key.get_ref().as_ref();
        if prefix.is_empty() && key == "workspace" {
            continue;
        }
        let here = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };
        if let DeValue::Table(inner) = value.get_ref() {
            if field(inner, "workspace").is_some_and(|v| matches!(v, DeValue::Boolean(true))) {
                return Some(here);
            }
            if let Some(found) = inherited_field(inner, &here) {
                return Some(found);
            }
        }
    }
    None
}

/// `"path"` or `"git"` if a dependency table of the manifest names `colibri-runtime` that way.
fn names_runtime_by_path_or_git(table: &DeTable<'_>) -> Option<&'static str> {
    let mut sections: Vec<&DeTable<'_>> = Vec::new();
    for key in ["dependencies", "build-dependencies"] {
        sections.extend(field(table, key).and_then(DeValue::as_table));
    }
    if let Some(targets) = field(table, "target").and_then(DeValue::as_table) {
        for (_, target) in targets {
            let Some(target) = target.get_ref().as_table() else {
                continue;
            };
            for key in ["dependencies", "build-dependencies"] {
                sections.extend(field(target, key).and_then(DeValue::as_table));
            }
        }
    }
    for section in sections {
        for (key, value) in section {
            let Some(spec) = value.get_ref().as_table() else {
                continue;
            };
            let renamed = field(spec, "package").and_then(DeValue::as_str);
            if key.get_ref().as_ref() != "colibri-runtime" && renamed != Some("colibri-runtime") {
                continue;
            }
            if field(spec, "path").is_some() {
                return Some("path");
            }
            if field(spec, "git").is_some() {
                return Some("git");
            }
        }
    }
    None
}

/// The files of the workspace for these binding crates and this engine source.
pub fn generate(engine: &Engine, crates: &[BindingCrate]) -> Result<Generated, String> {
    let mut sorted: Vec<&BindingCrate> = crates.iter().collect();
    sorted.sort_by(|a, b| a.key.cmp(&b.key));
    for c in &sorted {
        if !c
            .key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return Err(format!("`{}` cannot be a Cargo dependency name", c.key));
        }
    }
    Ok(Generated {
        cargo_toml: cargo_toml(engine, &sorted)?,
        main_rs: main_rs(&sorted),
        seed_lock: seed_lock(),
    })
}

/// The cargo dependency name of a binding.
fn alias(c: &BindingCrate) -> String {
    format!("binding_{}", c.key)
}

fn cargo_toml(engine: &Engine, crates: &[&BindingCrate]) -> Result<String, String> {
    let mut out = String::new();
    out.push_str(
        "# Generated by `colibri`. Do not edit: it is rewritten from `[rust-dependencies]`.\n\n",
    );
    out.push_str("[package]\nname = \"colibri-project-runner\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n");
    out.push_str("# A cargo root of its own: the profiles below are the only ones that count.\n[workspace]\n\n");
    out.push_str(&format!(
        "[[bin]]\nname = \"{RUNNER_BIN}\"\npath = \"src/main.rs\"\n\n"
    ));
    out.push_str("[dependencies]\n");
    match engine {
        Engine::Path(root) => {
            let lang = member_path(root, "colibri-cli");
            let _ = writeln!(out, "colibri-lang = {{ path = {} }}", quote_path(&lang)?);
        }
        Engine::Git { url, rev } => {
            let _ = writeln!(
                out,
                "colibri-lang = {{ git = {}, rev = {} }}",
                quote(url),
                quote(rev)
            );
        }
    }
    for c in crates {
        let _ = writeln!(out, "{} = {}", alias(c), dependency_table(c)?);
    }
    out.push_str("\n# Binding crates name `colibri-runtime = \"0.1\"`; this is the one runtime they get.\n[patch.crates-io]\n");
    match engine {
        Engine::Path(root) => {
            let runtime = member_path(root, "colibri-runtime");
            let _ = writeln!(
                out,
                "colibri-runtime = {{ path = {} }}",
                quote_path(&runtime)?
            );
        }
        Engine::Git { url, rev } => {
            let _ = writeln!(
                out,
                "colibri-runtime = {{ git = {}, rev = {} }}",
                quote(url),
                quote(rev)
            );
        }
    }
    // `colibri run` builds `colibri-run`, `colibri build` builds `release` (as `colibri` itself
    // is built). A binding's panic must reach the host boundary, so both unwind.
    out.push_str(
        "\n[profile.colibri-run]\ninherits = \"release\"\nlto = false\ncodegen-units = 16\npanic = \"unwind\"\n\n\
         [profile.release]\nlto = \"fat\"\ncodegen-units = 1\nstrip = true\npanic = \"unwind\"\n",
    );
    Ok(out)
}

/// `<root>/crates/<member>`, always joined with `/` (cargo reads it on every platform).
fn member_path(root: &Path, member: &str) -> String {
    let root = display_path(root);
    format!("{}/crates/{member}", root.trim_end_matches(['/', '\\']))
}

fn dependency_table(c: &BindingCrate) -> Result<String, String> {
    let mut parts = vec![format!("package = {}", quote(&c.package))];
    match &c.source {
        CrateSource::Registry { version } => parts.push(format!("version = {}", quote(version))),
        CrateSource::Path { dir } => {
            parts.push(format!("path = {}", quote_path(&display_path(dir))?));
        }
        CrateSource::Git { url, spec } => {
            parts.push(format!("git = {}", quote(url)));
            if let Some((key, value)) = spec.key_value() {
                parts.push(format!("{key} = {}", quote(value)));
            }
        }
    }
    if !c.features.is_empty() {
        let list: Vec<String> = c.features.iter().map(|f| quote(f)).collect();
        parts.push(format!("features = [{}]", list.join(", ")));
    }
    if !c.default_features {
        parts.push("default-features = false".to_string());
    }
    Ok(format!("{{ {} }}", parts.join(", ")))
}

/// The text of `src/main.rs`: the `BINDINGS` table, the marker and `main`.
fn main_rs(crates: &[&BindingCrate]) -> String {
    let mut out = String::new();
    out.push_str("// Generated by `colibri`. Do not edit.\n\n");
    out.push_str("use colibri_lang::bindings::Binding;\n\n");
    out.push_str("static BINDINGS: &[Binding] = &[\n");
    for c in crates {
        let _ = writeln!(
            out,
            "    Binding {{ name: {}, register: {}::__colibri_binding::register }},",
            quote(&c.key),
            alias(c)
        );
    }
    out.push_str("];\n\n");
    let marker = bindings_marker(crates);
    let _ = writeln!(
        out,
        "/// Tells `colibri build --runner` which bindings this binary holds.\n#[used]\nstatic BINDINGS_MARKER: [u8; {}] = *b\"{}\";\n",
        marker.len(),
        marker.replace('\u{1}', "\\x01").replace('\u{2}', "\\x02")
    );
    out.push_str(
        "fn main() -> std::process::ExitCode {\n    std::hint::black_box(&BINDINGS_MARKER);\n    colibri_lang::runner_main(BINDINGS)\n}\n",
    );
    out
}

/// Start of the marker a runner holds for its bindings (the twin of `colibri-runner-version`).
pub const MARKER_PREFIX: &str = "\u{1}colibri-bindings=";

/// `\x01colibri-bindings=<16 hex digits>\x02`: a hash of the binding table (key, package and
/// source of each crate), so that two runners with the same bindings carry the same marker.
fn bindings_marker(crates: &[&BindingCrate]) -> String {
    let mut text = String::new();
    for c in crates {
        let _ = writeln!(
            text,
            "{}\0{}\0{:?}\0{:?}\0{}",
            c.key, c.package, c.source, c.features, c.default_features
        );
    }
    format!("{MARKER_PREFIX}{:016x}\u{2}", fnv1a64(text.as_bytes()))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Colibri's own lock, for a project without one.
pub fn seed_lock() -> String {
    seed_from(OWN_LOCK)
}

/// `lock` without the packages that have no `source`: the workspace's own crates, which cargo
/// writes again for the runner. Every registry and git crate keeps its version and checksum.
pub fn seed_from(lock: &str) -> String {
    let text = lock.replace("\r\n", "\n");
    let mut header = Vec::new();
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in text.lines() {
        if line == "[[package]]" {
            blocks.push(vec![line]);
        } else if let Some(block) = blocks.last_mut() {
            block.push(line);
        } else {
            header.push(line);
        }
    }
    let mut parts = vec![header.join("\n").trim_end().to_string()];
    for mut block in blocks {
        if !block.iter().any(|l| l.starts_with("source = ")) {
            continue;
        }
        while block.last() == Some(&"") {
            block.pop();
        }
        parts.push(block.join("\n"));
    }
    let mut out = parts.join("\n\n");
    out.push('\n');
    out
}

/// A path as text for the manifest: without the `\\?\` prefix Windows' `canonicalize` adds
/// (cargo does not like it), and `\\?\UNC\server\share` as `\\server\share`.
pub fn display_path(path: &Path) -> String {
    strip_verbatim(&path.to_string_lossy())
}

/// See [`display_path`].
pub fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

fn quote_path(path: &str) -> Result<String, String> {
    if path.contains('\u{FFFD}') {
        return Err(format!(
            "the path `{path}` is not valid UTF-8, which a TOML file cannot hold"
        ));
    }
    Ok(quote(path))
}

/// `s` as a TOML basic string.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `[rust-dependencies]` of this manifest text, read by the real manifest parser.
    fn deps(table: &str) -> Vec<RustDependency> {
        let text = format!("[package]\nname = \"app\"\n\n[rust-dependencies]\n{table}\n");
        colibri_runtime::parse_manifest(colibri_syntax::FileId(0), &text)
            .unwrap_or_else(|e| panic!("{e:?}"))
            .rust_dependencies
    }

    fn registry(key: &str, package: &str) -> BindingCrate {
        BindingCrate {
            key: key.to_string(),
            package: package.to_string(),
            source: CrateSource::Registry {
                version: "0.1".to_string(),
            },
            features: Vec::new(),
            default_features: true,
        }
    }

    fn engine() -> Engine {
        Engine::Path(PathBuf::from("/src/colibri"))
    }

    const PROFILES: &str = "\n[profile.colibri-run]\ninherits = \"release\"\nlto = false\ncodegen-units = 16\npanic = \"unwind\"\n\n[profile.release]\nlto = \"fat\"\ncodegen-units = 1\nstrip = true\npanic = \"unwind\"\n";

    fn head(deps: &str, patch: &str) -> String {
        format!(
            "# Generated by `colibri`. Do not edit: it is rewritten from `[rust-dependencies]`.\n\n\
             [package]\nname = \"colibri-project-runner\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n\
             # A cargo root of its own: the profiles below are the only ones that count.\n[workspace]\n\n\
             [[bin]]\nname = \"colibri-project-runner\"\npath = \"src/main.rs\"\n\n\
             [dependencies]\n{deps}\n\
             # Binding crates name `colibri-runtime = \"0.1\"`; this is the one runtime they get.\n[patch.crates-io]\n{patch}"
        )
    }

    #[test]
    fn golden_cargo_toml_for_a_path_binding() {
        let crates = [BindingCrate {
            key: "native".to_string(),
            package: "colibri-native".to_string(),
            source: CrateSource::Path {
                dir: PathBuf::from("/work/app/native"),
            },
            features: vec!["simd".to_string(), "fast".to_string()],
            default_features: false,
        }];
        let got = generate(&engine(), &crates).unwrap().cargo_toml;
        let want = head(
            "colibri-lang = { path = \"/src/colibri/crates/colibri-cli\" }\nbinding_native = { package = \"colibri-native\", path = \"/work/app/native\", features = [\"simd\", \"fast\"], default-features = false }\n",
            "colibri-runtime = { path = \"/src/colibri/crates/colibri-runtime\" }\n",
        ) + PROFILES;
        assert_eq!(got, want, "\n{got}");
    }

    #[test]
    fn golden_cargo_toml_for_git_and_registry_bindings_sorted_by_key() {
        let crates = [
            BindingCrate {
                key: "zeta".to_string(),
                package: "zeta-crate".to_string(),
                source: CrateSource::Git {
                    url: "https://example.test/zeta".to_string(),
                    spec: GitSpec::Rev("abc123".to_string()),
                },
                features: Vec::new(),
                default_features: true,
            },
            registry("alpha", "colibri-alpha"),
        ];
        let engine = Engine::Git {
            url: "https://example.test/colibri".to_string(),
            rev: "0123abcd".to_string(),
        };
        let got = generate(&engine, &crates).unwrap().cargo_toml;
        let want = head(
            "colibri-lang = { git = \"https://example.test/colibri\", rev = \"0123abcd\" }\nbinding_alpha = { package = \"colibri-alpha\", version = \"0.1\" }\nbinding_zeta = { package = \"zeta-crate\", git = \"https://example.test/zeta\", rev = \"abc123\" }\n",
            "colibri-runtime = { git = \"https://example.test/colibri\", rev = \"0123abcd\" }\n",
        ) + PROFILES;
        assert_eq!(got, want, "\n{got}");
        // The same inputs in another order write the same file.
        let swapped = [crates[1].clone(), crates[0].clone()];
        assert_eq!(generate(&engine, &swapped).unwrap().cargo_toml, got);
    }

    #[test]
    fn golden_main_rs() {
        let crates = [
            registry("alpha", "colibri-alpha"),
            registry("beta", "colibri-beta"),
        ];
        let got = generate(&engine(), &crates).unwrap().main_rs;
        let marker = {
            let refs: Vec<&BindingCrate> = crates.iter().collect();
            bindings_marker(&refs)
        };
        assert!(marker.starts_with(MARKER_PREFIX) && marker.ends_with('\u{2}'));
        assert_eq!(marker.len(), MARKER_PREFIX.len() + 16 + 1);
        let escaped = marker.replace('\u{1}', "\\x01").replace('\u{2}', "\\x02");
        let want = format!(
            "// Generated by `colibri`. Do not edit.\n\nuse colibri_lang::bindings::Binding;\n\nstatic BINDINGS: &[Binding] = &[\n    Binding {{ name: \"alpha\", register: binding_alpha::__colibri_binding::register }},\n    Binding {{ name: \"beta\", register: binding_beta::__colibri_binding::register }},\n];\n\n/// Tells `colibri build --runner` which bindings this binary holds.\n#[used]\nstatic BINDINGS_MARKER: [u8; {}] = *b\"{escaped}\";\n\nfn main() -> std::process::ExitCode {{\n    std::hint::black_box(&BINDINGS_MARKER);\n    colibri_lang::runner_main(BINDINGS)\n}}\n",
            marker.len()
        );
        assert_eq!(got, want, "\n{got}");
    }

    #[test]
    fn the_marker_follows_the_bindings() {
        let one = [registry("alpha", "colibri-alpha")];
        let two = [registry("alpha", "colibri-alpha"), registry("beta", "x")];
        let m = |c: &[BindingCrate]| bindings_marker(&c.iter().collect::<Vec<_>>());
        assert_eq!(m(&one), m(&one.clone()));
        assert_ne!(m(&one), m(&two));
        let mut other = one.clone();
        other[0].features = vec!["f".to_string()];
        assert_ne!(m(&one), m(&other));
    }

    /// Strings a path can hold, in the order of the spec: spaces, quotes, backslashes,
    /// non-ASCII, a `\\?\` prefix, control characters.
    const NASTY: &[&str] = &[
        "/home/a b/native",
        "/home/it's \"quoted\"/native",
        r"C:\Users\a b\native",
        r"\\server\share\native",
        "/home/José Ñandú/ネイティブ/native",
        "/tab\there/new\nline/native",
        r#"C:\dir\with"quote"\and\\double"#,
        "/del\u{7f}ete/native",
    ];

    #[test]
    fn quoted_text_parses_back_to_the_same_text() {
        for text in NASTY {
            let source = format!("v = {}\n", quote(text));
            let parsed =
                DeTable::parse(&source).unwrap_or_else(|e| panic!("{text:?}: {e}\n{source}"));
            let got = field(parsed.get_ref(), "v")
                .and_then(DeValue::as_str)
                .unwrap();
            assert_eq!(got, *text, "{source}");
        }
    }

    #[test]
    fn verbatim_prefixes_are_stripped_by_hand() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\a b\x"), r"C:\Users\a b\x");
        assert_eq!(
            strip_verbatim(r"\\?\UNC\server\share\x"),
            r"\\server\share\x"
        );
        assert_eq!(strip_verbatim(r"\\server\share"), r"\\server\share");
        assert_eq!(strip_verbatim("/plain/path"), "/plain/path");
        // And what is written still parses back as the stripped text.
        for (given, shown) in [
            (r#"\\?\C:\Users\ñ "q"\native"#, r#"C:\Users\ñ "q"\native"#),
            (r"\\?\UNC\srv\sh are\native", r"\\srv\sh are\native"),
        ] {
            let source = format!("v = {}\n", quote_path(&strip_verbatim(given)).unwrap());
            let parsed = DeTable::parse(&source).unwrap();
            let got = field(parsed.get_ref(), "v")
                .and_then(DeValue::as_str)
                .unwrap();
            assert_eq!(got, shown);
        }
    }

    #[test]
    fn a_whole_generated_file_keeps_every_nasty_path() {
        for text in NASTY {
            let crates = [BindingCrate {
                key: "native".to_string(),
                package: "colibri-native".to_string(),
                source: CrateSource::Path {
                    dir: PathBuf::from(text),
                },
                features: Vec::new(),
                default_features: true,
            }];
            let engine = Engine::Path(PathBuf::from(text));
            let got = generate(&engine, &crates).unwrap().cargo_toml;
            let parsed = DeTable::parse(&got).unwrap_or_else(|e| panic!("{text:?}: {e}\n{got}"));
            let table = parsed.get_ref();
            let dependencies = field(table, "dependencies")
                .and_then(DeValue::as_table)
                .unwrap();
            let native = field(dependencies, "binding_native")
                .and_then(DeValue::as_table)
                .unwrap();
            assert_eq!(field(native, "path").and_then(DeValue::as_str), Some(*text));
            let lang = field(dependencies, "colibri-lang")
                .and_then(DeValue::as_table)
                .unwrap();
            let lang_path = field(lang, "path").and_then(DeValue::as_str).unwrap();
            assert_eq!(lang_path, format!("{text}/crates/colibri-cli"));
        }
    }

    /// The `(name, version, checksum)` of every package with a source.
    fn third_party(lock: &str) -> Vec<(String, String, Option<String>)> {
        let parsed = DeTable::parse(lock).unwrap();
        let packages = field(parsed.get_ref(), "package").unwrap();
        let DeValue::Array(packages) = packages else {
            panic!("no packages")
        };
        let mut out = Vec::new();
        for p in packages {
            let p = p.get_ref().as_table().unwrap();
            let text = |k: &str| field(p, k).and_then(DeValue::as_str).map(str::to_string);
            if text("source").is_some() {
                out.push((
                    text("name").unwrap(),
                    text("version").unwrap(),
                    text("checksum"),
                ));
            }
        }
        out
    }

    #[test]
    fn the_seeded_lock_keeps_every_third_party_version() {
        let own = third_party(OWN_LOCK);
        assert!(
            own.len() > 50,
            "the engine's lock lists its dependencies: {}",
            own.len()
        );
        let seeded = seed_lock();
        assert_eq!(third_party(&seeded), own);
        // The workspace's own crates are gone.
        let all = DeTable::parse(&seeded).unwrap();
        let DeValue::Array(packages) = field(all.get_ref(), "package").unwrap() else {
            panic!()
        };
        for p in packages {
            let p = p.get_ref().as_table().unwrap();
            assert!(field(p, "source").is_some(), "{p:?}");
            let name = field(p, "name").and_then(DeValue::as_str).unwrap();
            assert!(!name.starts_with("colibri-"), "{name}");
        }
        assert!(seeded.starts_with("# This file is automatically @generated by Cargo."));
        assert!(seeded.contains("\nversion = 4\n"));
        assert!(seeded.ends_with("\n") && !seeded.ends_with("\n\n"));
    }

    #[test]
    fn seeding_drops_sourceless_packages_and_reads_crlf() {
        let lock = "# header\nversion = 4\n\n[[package]]\nname = \"ours\"\nversion = \"0.1.0\"\ndependencies = [\n \"dep\",\n]\n\n[[package]]\nname = \"dep\"\nversion = \"1.2.3\"\nsource = \"registry+x\"\nchecksum = \"abc\"\n";
        let want = "# header\nversion = 4\n\n[[package]]\nname = \"dep\"\nversion = \"1.2.3\"\nsource = \"registry+x\"\nchecksum = \"abc\"\n";
        assert_eq!(seed_from(lock), want);
        assert_eq!(seed_from(&lock.replace('\n', "\r\n")), want);
    }

    /// A project folder with these `Cargo.toml` files (path, text), removed at the end.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str, files: &[(&str, &str)]) -> Tree {
            let dir =
                std::env::temp_dir().join(format!("colibri-gen-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for (rel, text) in files {
                let full = dir.join(rel);
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(full, text).unwrap();
            }
            Tree(dir)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const GOOD: &str = "[package]\nname = \"colibri-native\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ncolibri-runtime = \"0.1\"\n";

    fn refusal(name: &str, cargo_toml: &str) -> String {
        let tree = Tree::new(name, &[("native/Cargo.toml", cargo_toml)]);
        let errors = resolve_crates(&deps("native = { path = \"native/\" }"), &tree.0)
            .expect_err("a refusal");
        assert_eq!(errors.len(), 1, "{errors:?}");
        errors.into_iter().next().unwrap()
    }

    #[test]
    fn a_path_crate_gets_its_package_name_from_its_manifest() {
        let tree = Tree::new("good", &[("native/Cargo.toml", GOOD)]);
        let got = resolve_crates(
            &deps("native = { path = \"native/\", features = [\"b\", \"a\"] }\nrapier = { binding = \"colibri-rapier\", version = \"0.1\", default-features = false }"),
            &tree.0,
        )
        .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(
            (got[0].key.as_str(), got[0].package.as_str()),
            ("native", "colibri-native")
        );
        assert_eq!(got[0].features, ["a", "b"]);
        match &got[0].source {
            CrateSource::Path { dir } => assert!(dir.ends_with("native"), "{dir:?}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(got[1].key, "rapier");
        assert!(!got[1].default_features);
    }

    #[test]
    fn two_runtimes_are_refused_with_the_fix() {
        let by_path = GOOD.replace("\"0.1\"", "{ path = \"../../runtime\" }");
        let by_git = GOOD.replace("\"0.1\"", "{ git = \"https://example.test/c\" }");
        let renamed = GOOD.replace(
            "colibri-runtime = \"0.1\"",
            "rt = { package = \"colibri-runtime\", path = \"/x\" }",
        );
        let targeted = format!(
            "{GOOD}\n[target.'cfg(unix)'.dependencies]\ncolibri-runtime = {{ path = \"/x\" }}\n"
        );
        for (name, text, how) in [
            ("path", by_path, "path"),
            ("git", by_git, "git"),
            ("renamed", renamed, "path"),
            ("target", targeted, "path"),
        ] {
            let message = refusal(name, &text);
            assert!(
                message.contains(&format!("names `colibri-runtime` by {how}")),
                "{name}: {message}"
            );
            assert!(
                message.contains("write `colibri-runtime = \"0.1\"`"),
                "{name}: {message}"
            );
        }
        // The right spellings are fine.
        let tree = Tree::new(
            "versions",
            &[(
                "native/Cargo.toml",
                &GOOD.replace("\"0.1\"", "{ version = \"0.1\", default-features = false }"),
            )],
        );
        assert!(resolve_crates(&deps("native = { path = \"native/\" }"), &tree.0).is_ok());
    }

    #[test]
    fn workspace_inheritance_is_refused_with_the_fix() {
        let in_package = GOOD.replace("version = \"0.1.0\"", "version.workspace = true");
        let in_dependency = GOOD.replace(
            "colibri-runtime = \"0.1\"",
            "colibri-runtime = \"0.1\"\nserde = { workspace = true }",
        );
        let lints = format!("{GOOD}\n[lints]\nworkspace = true\n");
        for (name, text, at) in [
            ("pkg", in_package, "package.version"),
            ("dep", in_dependency, "dependencies.serde"),
            ("lints", lints, "lints"),
        ] {
            let message = refusal(name, &text);
            assert!(
                message.contains(&format!("takes `{at}` from a workspace")),
                "{name}: {message}"
            );
            assert!(
                message.contains("write the fields out"),
                "{name}: {message}"
            );
        }
    }

    #[test]
    fn a_path_without_a_crate_is_refused() {
        let tree = Tree::new("none", &[("native/readme.txt", "x")]);
        let errors = resolve_crates(&deps("native = { path = \"native/\" }"), &tree.0).unwrap_err();
        assert!(
            errors[0].contains("has no readable `Cargo.toml`"),
            "{errors:?}"
        );
        let message = refusal("virtual", "[workspace]\nmembers = []\n");
        assert!(message.contains("has no `[package] name`"), "{message}");
        let message = refusal("broken", "[package\n");
        assert!(message.contains("is not valid TOML"), "{message}");
    }

    #[test]
    fn every_problem_is_reported_not_only_the_first() {
        let tree = Tree::new(
            "many",
            &[
                (
                    "a/Cargo.toml",
                    &GOOD.replace("0.1.0", "0.1.0\nrust-version.workspace = true"),
                ),
                (
                    "b/Cargo.toml",
                    &GOOD.replace("\"0.1\"", "{ path = \"/x\" }"),
                ),
            ],
        );
        let errors = resolve_crates(&deps("a = { path = \"a\" }\nb = { path = \"b\" }"), &tree.0)
            .unwrap_err();
        assert_eq!(errors.len(), 2, "{errors:?}");
    }

    #[test]
    fn a_non_ascii_key_cannot_name_a_cargo_dependency() {
        let errors = resolve_crates(
            &deps("\"físicas\" = { binding = \"colibri-fisicas\", version = \"0.1\" }"),
            Path::new("/p"),
        )
        .unwrap_err();
        assert!(errors[0].contains("ASCII only"), "{errors:?}");
    }
}
