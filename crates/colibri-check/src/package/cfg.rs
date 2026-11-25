//! Conditional compilation: `@when(condition)` on a top-level item (DESIGN §3.6).
//!
//! The loader evaluates the conditions of a file right after parsing it: an item whose
//! condition is false is dropped from the syntax tree, so the checker never sees it (it is
//! parsed, nothing more), and the attribute is removed from the items that stay. A condition
//! is built from
//!
//! - `os == "linux"`, `arch == "aarch64"` (and `!=`): the platform the program is checked for,
//! - `debug`: true unless the build is a release build,
//! - a feature name: true when the feature is on (`[features]` of the package's `colibri.toml`,
//!   or a flag the host or the command line turned on),
//! - `!`, `&&`, `||`, parentheses, `true` and `false`.
//!
//! A name that is none of these, and a value that no platform has, are errors: a condition
//! that can never be true would otherwise hide code silently.

use std::collections::HashSet;

use colibri_syntax::ast::{self, BinOp, Expr, ExprKind, Item, ItemKind, TopLevel, UnOp};
use colibri_syntax::{Diagnostic, Span};

/// The operating systems a condition may name.
pub const KNOWN_OS: &[&str] = &[
    "linux", "macos", "windows", "freebsd", "netbsd", "openbsd", "android", "ios", "web",
];

/// The processor architectures a condition may name.
pub const KNOWN_ARCH: &[&str] = &[
    "x86_64",
    "aarch64",
    "x86",
    "arm",
    "riscv64",
    "wasm32",
    "powerpc64",
    "s390x",
    "loongarch64",
];

/// The values `@when` conditions are decided with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildCfg {
    /// `os == ".."`. The default is the platform the tools run on.
    pub os: String,
    /// `arch == ".."`. The default is the platform the tools run on.
    pub arch: String,
    /// `debug`. True by default: `colibri run`, `check` and `test` are debug builds unless
    /// `--release` says otherwise.
    pub debug: bool,
    /// The features turned on beside the `default` list of each package's manifest.
    pub features: Vec<String>,
    /// Do not turn on the `default` list of the root package's manifest (`--no-default-features`).
    pub no_default_features: bool,
}

/// The operating system of the machine running the program. On the web (`wasm32-unknown-unknown`)
/// `std` has none (its name is empty), and the system is the page: `web`.
fn default_os() -> &'static str {
    if cfg!(target_family = "wasm") {
        "web"
    } else {
        std::env::consts::OS
    }
}

impl Default for BuildCfg {
    fn default() -> Self {
        BuildCfg {
            os: default_os().to_string(),
            arch: std::env::consts::ARCH.to_string(),
            debug: true,
            features: Vec::new(),
            no_default_features: false,
        }
    }
}

impl BuildCfg {
    /// Turns the feature `name` on.
    pub fn with_feature(mut self, name: &str) -> Self {
        if !self.features.iter().any(|f| f == name) {
            self.features.push(name.to_string());
        }
        self
    }

    /// A release build: `debug` is false.
    pub fn release(mut self) -> Self {
        self.debug = false;
        self
    }
}

/// A top-level name that a false `@when` left out of a module, kept so that a use of it can say
/// so instead of "unknown name".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Excluded {
    /// Empty for an item that declares no name (`impl`, `test`, `let _ = ..`).
    pub name: String,
    /// The condition as written: `os == "windows"`.
    pub cond: String,
    /// The whole dropped item, attributes included.
    pub span: colibri_syntax::Span,
}

/// What one package's conditions are decided with.
pub(crate) struct Env<'a> {
    pub cfg: &'a BuildCfg,
    /// The names the package declares as features, and any flag the build turned on.
    pub known: &'a HashSet<String>,
    pub enabled: &'a HashSet<String>,
    /// The package has a `colibri.toml`. A script has none, and so declares no features.
    pub has_manifest: bool,
}

/// Drops the items of `file` whose `@when` is false and removes the attribute from the others.
/// `text` is the source of the file. Problems are added to `diags`; the names of what was
/// dropped are returned.
pub(crate) fn apply(
    file: &mut ast::File,
    text: &str,
    env: &Env<'_>,
    diags: &mut Vec<Diagnostic>,
) -> Vec<Excluded> {
    let mut excluded = Vec::new();
    file.items.retain_mut(|top| {
        let TopLevel::Item(item) = top else {
            return true;
        };
        let cond = item
            .attrs
            .iter()
            .find(|a| a.name.name == "when")
            .and_then(|a| a.args.first())
            .and_then(|e| text.get(e.span.start as usize..e.span.end as usize))
            .map(|c| c.split_whitespace().collect::<Vec<_>>().join(" "));
        if !keep(item, env, diags) {
            // `keep` took the `@when` off the item; its names and its span are still the
            // dropped item's, so nothing needs a copy.
            // An item that brings no name (an `impl`, a `test`, `let _ = ..`) is kept with an
            // empty name: no use asks for it, but a tool that reads what was left out finds it.
            let mut names = names_of(item);
            if names.is_empty() {
                names.push(String::new());
            }
            for name in names {
                excluded.push(Excluded {
                    name,
                    cond: cond.clone().unwrap_or_default(),
                    span: item.span,
                });
            }
            return false;
        }
        // `let _ = expr` is a statement of the script; the parser made it an item only because
        // `@when` came before it, and with the attribute gone it is the statement again.
        if let Item {
            id,
            span,
            kind: ItemKind::Binding(binding),
            public: false,
            attrs,
            ..
        } = item
            && attrs.is_empty()
            && matches!(binding.pat.kind, ast::PatKind::Wild)
        {
            *top = TopLevel::Stmt(ast::Stmt {
                id: *id,
                span: *span,
                kind: ast::StmtKind::Let(binding.clone()),
            });
        }
        true
    });
    excluded
}

/// The names an item brings into its module.
fn names_of(item: &Item) -> Vec<String> {
    match &item.kind {
        ItemKind::Fn(f) => vec![f.name.name.clone()],
        ItemKind::Struct(s) => vec![s.name.name.clone()],
        ItemKind::Enum(e) => vec![e.name.name.clone()],
        ItemKind::Interface(i) => vec![i.name.name.clone()],
        ItemKind::TypeAlias(t) => vec![t.name.name.clone()],
        ItemKind::Binding(b) => match &b.pat.kind {
            ast::PatKind::Ident(id) => vec![id.name.clone()],
            _ => Vec::new(),
        },
        ItemKind::Import(imp) => match &imp.kind {
            ast::ImportKind::Plain => imp
                .path
                .last()
                .map(|i| i.name.clone())
                .into_iter()
                .collect(),
            ast::ImportKind::Alias(a) => vec![a.name.clone()],
            ast::ImportKind::Names(names) => names
                .iter()
                .map(|n| n.alias.as_ref().unwrap_or(&n.name).name.clone())
                .collect(),
        },
        ItemKind::Extern(block) => block.items.iter().flat_map(names_of).collect(),
        ItemKind::Impl(_) | ItemKind::Test(_) | ItemKind::Error => Vec::new(),
    }
}

fn keep(item: &mut Item, env: &Env<'_>, diags: &mut Vec<Diagnostic>) -> bool {
    let mut keep = true;
    let mut seen = false;
    item.attrs.retain(|attr| {
        if attr.name.name != "when" {
            return true;
        }
        if seen {
            diags.push(
                Diagnostic::error("an item has one `@when`", attr.span)
                    .with_hint("combine the conditions: `@when(os == \"linux\" && debug)`"),
            );
            return false;
        }
        seen = true;
        match attr.args.as_slice() {
            [cond] => {
                if let Some(value) = eval(cond, env, diags) {
                    keep = value;
                }
            }
            _ => diags.push(
                Diagnostic::error("`@when` takes one condition", attr.span)
                    .with_hint("write `@when(os == \"windows\")`"),
            ),
        }
        false
    });
    // `@when` does not read in the middle of an item that has no place of its own.
    if matches!(item.kind, ItemKind::Error) {
        return true;
    }
    keep
}

fn eval(e: &Expr, env: &Env<'_>, diags: &mut Vec<Diagnostic>) -> Option<bool> {
    match &e.kind {
        ExprKind::Paren(inner) => eval(inner, env, diags),
        ExprKind::Bool(b) => Some(*b),
        ExprKind::Unary {
            op: UnOp::Not,
            expr,
        } => eval(expr, env, diags).map(|v| !v),
        ExprKind::Binary {
            op: op @ (BinOp::And | BinOp::Or),
            lhs,
            rhs,
        } => {
            // Both sides are always read, so a mistake on the right is reported even when
            // the left decides the answer.
            let (l, r) = (eval(lhs, env, diags), eval(rhs, env, diags));
            let (l, r) = (l?, r?);
            Some(if *op == BinOp::And { l && r } else { l || r })
        }
        ExprKind::Binary {
            op: op @ (BinOp::Eq | BinOp::Ne),
            lhs,
            rhs,
        } => compare(*op == BinOp::Eq, lhs, rhs, env, diags),
        ExprKind::Ident(id) => name(&id.name, id.span, env, diags),
        _ => {
            diags.push(
                Diagnostic::error("this is not a condition `@when` understands", e.span)
                    .with_hint(
                        "a condition is `os == \"linux\"`, `arch != \"x86_64\"`, `debug`, a feature name, or those joined with `!`, `&&`, `||` and parentheses",
                    ),
            );
            None
        }
    }
}

fn name(n: &str, span: Span, env: &Env<'_>, diags: &mut Vec<Diagnostic>) -> Option<bool> {
    match n {
        "debug" => return Some(env.cfg.debug),
        "os" | "arch" => {
            diags.push(
                Diagnostic::error(format!("`{n}` is a text: compare it with `==`"), span)
                    .with_hint(format!("write `{n} == \"{}\"`", current(n, env))),
            );
            return None;
        }
        _ => {}
    }
    if env.known.contains(n) {
        return Some(env.enabled.contains(n));
    }
    let mut known: Vec<&str> = vec!["os", "arch", "debug"];
    let mut features: Vec<&str> = env.known.iter().map(String::as_str).collect();
    features.sort_unstable();
    known.extend(features);
    let hint = if !env.has_manifest {
        // A typo must not quietly mean "off", so a script's flags are only the ones it was
        // given (DESIGN §3.6).
        format!(
            "a script has no manifest: pass `--feature {n}`, or make a package with `[features]` in `colibri.toml`; the names that always exist are {}",
            known.join(", ")
        )
    } else if env.known.is_empty() {
        format!(
            "the names are {}; declare the feature in `colibri.toml` (`[features]` then `{n} = []`), then turn it on with `--feature {n}`",
            known.join(", ")
        )
    } else {
        format!("the names are {}", known.join(", "))
    };
    diags.push(Diagnostic::error(format!("unknown name `{n}` in `@when`"), span).with_hint(hint));
    None
}

fn current<'a>(which: &str, env: &'a Env<'_>) -> &'a str {
    if which == "os" {
        &env.cfg.os
    } else {
        &env.cfg.arch
    }
}

fn compare(
    equal: bool,
    lhs: &Expr,
    rhs: &Expr,
    env: &Env<'_>,
    diags: &mut Vec<Diagnostic>,
) -> Option<bool> {
    let ExprKind::Ident(id) = &lhs.kind else {
        diags.push(
            Diagnostic::error("only `os` and `arch` are compared in `@when`", lhs.span)
                .with_hint("write `os == \"linux\"` or `arch != \"aarch64\"`"),
        );
        return None;
    };
    let which = id.name.as_str();
    let (known, label): (&[&str], &str) = match which {
        "os" => (KNOWN_OS, "operating system"),
        "arch" => (KNOWN_ARCH, "architecture"),
        "debug" => {
            diags.push(
                Diagnostic::error("`debug` is true or false: it is not compared", id.span)
                    .with_hint("write `@when(debug)` or `@when(!debug)`"),
            );
            return None;
        }
        other => {
            diags.push(
                Diagnostic::error(format!("`{other}` is not compared in `@when`"), id.span)
                    .with_hint(
                        "only `os` and `arch` are compared with `==` or `!=`; a feature is a bare name: `@when(name)`",
                    ),
            );
            return None;
        }
    };
    let Some(value) = crate::ffi::plain_string(rhs) else {
        diags.push(
            Diagnostic::error(format!("`{which}` is compared with a text"), rhs.span)
                .with_hint(format!("write `{which} == \"{}\"`", current(which, env))),
        );
        return None;
    };
    // A value that no platform has can never match.
    let host_value = current(which, env);
    if !known.contains(&value.as_str()) && value != host_value {
        diags.push(
            Diagnostic::error(format!("`{value}` is not an {label} name"), rhs.span)
                .with_hint(format!("the {label}s are: {}", known.join(", "))),
        );
        return None;
    }
    let same = host_value == value;
    Some(same == equal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use colibri_syntax::{FileId, parse_file};

    fn run(
        src: &str,
        cfg: &BuildCfg,
        features: &[&str],
        on: &[&str],
    ) -> (Vec<String>, Vec<String>) {
        let (mut file, parse_diags) = parse_file(FileId(0), src);
        assert!(parse_diags.is_empty(), "{parse_diags:?}");
        let known: HashSet<String> = features.iter().map(|s| s.to_string()).collect();
        let enabled: HashSet<String> = on.iter().map(|s| s.to_string()).collect();
        let env = Env {
            cfg,
            known: &known,
            enabled: &enabled,
            has_manifest: true,
        };
        let mut diags = Vec::new();
        apply(&mut file, src, &env, &mut diags);
        let kept = file
            .items
            .iter()
            .filter_map(|t| match t {
                TopLevel::Item(Item {
                    kind: ItemKind::Fn(f),
                    attrs,
                    ..
                }) => {
                    assert!(attrs.iter().all(|a| a.name.name != "when"));
                    Some(f.name.name.clone())
                }
                _ => None,
            })
            .collect();
        (kept, diags.into_iter().map(|d| d.message).collect())
    }

    fn cfg(os: &str, arch: &str, debug: bool) -> BuildCfg {
        BuildCfg {
            os: os.to_string(),
            arch: arch.to_string(),
            debug,
            ..BuildCfg::default()
        }
    }

    #[test]
    fn os_arch_debug_and_features_decide() {
        let src = "@when(os == \"windows\")\nfn win() {}\n@when(os != \"windows\")\nfn unix() {}\n@when(arch == \"aarch64\" && debug)\nfn arm_debug() {}\n@when(fast || !debug)\nfn quick() {}\nfn plain() {}\n";
        let (kept, errs) = run(src, &cfg("windows", "x86_64", true), &["fast"], &[]);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(kept, ["win", "plain"]);
        let (kept, _) = run(src, &cfg("macos", "aarch64", true), &["fast"], &["fast"]);
        assert_eq!(kept, ["unix", "arm_debug", "quick", "plain"]);
        let (kept, _) = run(src, &cfg("linux", "aarch64", false), &["fast"], &[]);
        assert_eq!(kept, ["unix", "quick", "plain"]);
    }

    #[test]
    fn a_misspelt_value_or_name_is_an_error_not_false() {
        let c = cfg("linux", "x86_64", true);
        let (kept, errs) = run("@when(os == \"Linux\")\nfn a() {}\n", &c, &[], &[]);
        assert_eq!(errs, ["`Linux` is not an operating system name"]);
        // Reported, and the item is kept so one mistake does not hide other errors.
        assert_eq!(kept, ["a"]);
        let (_, errs) = run("@when(dbg)\nfn a() {}\n", &c, &["fast"], &[]);
        assert_eq!(errs, ["unknown name `dbg` in `@when`"]);
        // The condition is read in full: the right side is checked although the left decides.
        let (_, errs) = run("@when(debug || turbo)\nfn a() {}\n", &c, &[], &[]);
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn the_host_platform_is_always_a_valid_value() {
        let c = cfg("haiku", "sparc", true);
        let (kept, errs) = run("@when(os == \"haiku\")\nfn a() {}\n", &c, &[], &[]);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(kept, ["a"]);
    }

    #[test]
    fn features_turn_on_what_they_list() {
        let f = crate::package::Features {
            declared: vec![
                ("net".into(), vec!["tls".into()]),
                ("tls".into(), vec![]),
                ("fast".into(), vec![]),
            ],
            default: vec!["fast".into()],
        };
        assert_eq!(f.enabled(&[], false), ["fast"]);
        assert_eq!(f.enabled(&[], true), Vec::<String>::new());
        assert_eq!(f.enabled(&["net".to_string()], true), ["net", "tls"]);
        assert_eq!(
            f.enabled(&["net".to_string()], false),
            ["fast", "net", "tls"]
        );
    }

    #[test]
    fn the_web_is_an_operating_system_a_condition_may_name() {
        let src = "@when(os == \"web\")\nfn page() {}\n@when(os == \"linux\")\nfn desktop() {}\n";
        let (kept, diags) = run(src, &cfg("web", "wasm32", true), &[], &[]);
        assert_eq!((kept, diags), (vec!["page".to_string()], Vec::new()));
        let (kept, diags) = run(src, &cfg("linux", "x86_64", true), &[], &[]);
        assert_eq!((kept, diags), (vec!["desktop".to_string()], Vec::new()));
    }

    #[test]
    fn the_default_system_has_a_name_a_condition_could_use() {
        // On wasm32-unknown-unknown `std::env::consts::OS` is empty.
        assert!(KNOWN_OS.contains(&BuildCfg::default().os.as_str()));
    }
}
