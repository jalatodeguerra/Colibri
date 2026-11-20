//! The messages for attributes that do not exist (DESIGN §3.6).

use colibri_syntax::{FileId, parse_file};

fn errors(src: &str) -> Vec<(String, Option<String>)> {
    let (ast, parse) = parse_file(FileId(0), src);
    assert!(parse.is_empty(), "{parse:?}");
    colibri_check::check(&ast)
        .diagnostics
        .into_iter()
        .map(|d| (d.message, d.hint))
        .collect()
}

#[test]
fn inline_says_it_was_removed() {
    let found = errors("@inline\nfn f() {}\n");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, "unknown attribute `@inline`");
    let hint = found[0].1.as_deref().unwrap();
    assert!(hint.contains("`@inline` was removed"), "{hint}");
    assert!(hint.contains("engine's decision"), "{hint}");
}

#[test]
fn any_other_unknown_attribute_lists_the_known_ones() {
    let found = errors("@fast\nfn f() {}\n");
    assert_eq!(found[0].0, "unknown attribute `@fast`");
    let hint = found[0].1.as_deref().unwrap();
    for known in ["@when", "@deprecated", "@repr(C)", "@link"] {
        assert!(hint.contains(known), "{hint}");
    }
    let field = errors("struct S {\n    @fast x: int\n}\n");
    assert_eq!(field[0].0, "unknown attribute `@fast` on a field");
    assert!(field[0].1.as_deref().unwrap().contains("@rename"));
    let method = errors("struct S {}\nimpl S {\n    @fast\n    fn m(self) {}\n}\n");
    assert_eq!(method[0].0, "unknown attribute `@fast` on a method");
    let test = errors("@fast\ntest \"t\" {}\n");
    assert_eq!(test[0].0, "unknown attribute `@fast`");
}

#[test]
fn unorderable_field_is_named_in_the_hint() {
    let found = errors(
        "class H { id: int }\nstruct E { k: int\n h: H }\nfn f(a: E, b: E) -> bool => a < b\n",
    );
    assert_eq!(found.len(), 1, "{found:?}");
    let hint = found[0].1.as_deref().unwrap();
    assert!(
        hint.contains("its field `h` (`H`) cannot be ordered"),
        "{hint}"
    );
}

// ---- every attribute at every position (DESIGN §3.6) ----
//
// An attribute is either accepted and effective, or refused with a message. Never silently
// accepted and ignored. Each row is a program with the attribute `{A}` written at one position;
// `{USE}` is a use of the thing, where there is something to be deprecated.

use colibri_check::package::{BuildCfg, Entry, LoadOptions, MemoryFs, load_with};
use colibri_syntax::SourceMap;
use colibri_syntax::ast::TopLevel;

struct Run {
    hints: Vec<String>,
    errors: Vec<String>,
    warnings: Vec<String>,
    items: usize,
}

fn run(src: &str) -> Run {
    let mut fs = MemoryFs::new();
    fs.insert("m.colibri", src);
    let mut sources = SourceMap::new();
    let loaded = load_with(
        &fs,
        &mut sources,
        Entry::File(std::path::Path::new("m.colibri")),
        &[],
        LoadOptions {
            all_modules: false,
            cfg: BuildCfg::default(),
            std_sources: Vec::new(),
        },
    )
    .expect("loads");
    let items = loaded
        .files
        .last()
        .unwrap()
        .items
        .iter()
        .filter(|t| matches!(t, TopLevel::Item(_)))
        .count();
    let mut diags = loaded.diagnostics.clone();
    if !loaded.has_errors() {
        let mut env = colibri_check::HostEnv::new();
        env.grant("ffi");
        diags = colibri_check::check_modules(
            &loaded.inputs(),
            &loaded.packages,
            &env,
            colibri_check::DEFAULT_STACK_BUDGET,
        )
        .diagnostics;
    }
    let mut out = Run {
        hints: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        items,
    };
    for d in diags {
        out.hints.extend(d.hint.clone());
        if d.is_error() {
            out.errors.push(d.message);
        } else {
            out.warnings.push(d.message);
        }
    }
    out
}

/// (position, program, the use that makes `@deprecated` warn)
const POSITIONS: &[(&str, &str, &str)] = &[
    ("fn", "{A}\nfn f() {}\nfn main() { {USE} }", "f()"),
    (
        "struct",
        "{A}\nstruct S { x: int }\nfn main() { {USE} }",
        "let s = S { x: 1 }\nprint(s.x)",
    ),
    (
        "class",
        "{A}\nclass S { x: int }\nfn main() { {USE} }",
        "let s = S { x: 1 }\nprint(s.x)",
    ),
    (
        "enum",
        "{A}\nenum E { A, B }\nfn main() { {USE} }",
        "let e = E.A\nprint(e == E.B)",
    ),
    (
        "interface",
        "{A}\ninterface I { fn m(self) -> int }\n{USE}\nfn main() {}",
        "fn g(i: I) -> int => 1",
    ),
    (
        "impl",
        "struct S {}\n{A}\nimpl S { fn m(self) {} }\nfn main() {}",
        "",
    ),
    (
        "impl_iface",
        "struct S {}\ninterface I { fn m(self) }\n{A}\nimpl I for S { fn m(self) {} }\nfn main() {}",
        "",
    ),
    (
        "alias",
        "{A}\ntype T = int\nfn main() { {USE} }",
        "let x: T = 1\nprint(x)",
    ),
    (
        "import",
        "{A}\nimport math\nfn main() { print(math.abs(-1)) }",
        "",
    ),
    (
        "extern",
        "{A}\nextern \"C\" { fn sqrt(x: float) -> float }\nfn main() {}",
        "",
    ),
    ("test", "{A}\ntest \"t\" {}\nfn main() {}", ""),
    ("const", "{A}\nconst C = 1\nfn main() { {USE} }", "print(C)"),
    ("let", "{A}\nlet x = 1\nfn main() { {USE} }", "print(x)"),
    ("var", "{A}\nvar x = 1\nfn main() { {USE} }", "print(x)"),
    ("let_wild", "{A}\nlet _ = 1\nfn main() {}", ""),
    ("stmt", "{A}\nprint(1)", ""),
    (
        "field",
        "struct S {\n    {A}\n    x: int = 0\n}\nfn main() { {USE} }",
        "let s = S { x: 1 }\nprint(s.x)",
    ),
    (
        "class_field",
        "class S {\n    {A}\n    x: int = 0\n}\nfn main() { {USE} }",
        "let s = S { x: 1 }\nprint(s.x)",
    ),
    (
        "variant",
        "enum E {\n    {A}\n    A,\n    B\n}\nfn main() {}",
        "",
    ),
    (
        "impl_member",
        "struct S {}\nimpl S {\n    {A}\n    fn m(self) {}\n}\nfn main() { {USE} }",
        "let s = S {}\ns.m()",
    ),
    (
        "impl_iface_member",
        "struct S {}\ninterface I { fn m(self) }\nimpl I for S {\n    {A}\n    fn m(self) {}\n}\nfn main() { {USE} }",
        "let s = S {}\ns.m()",
    ),
    (
        "iface_member_body",
        "interface I {\n    {A}\n    fn m(self) -> int => 1\n}\n{USE}\nfn main() {}",
        "fn g(i: I) -> int => i.m()",
    ),
    (
        "iface_member_nobody",
        "struct S {}\ninterface I {\n    {A}\n    fn m(self) -> int\n}\nimpl I for S { fn m(self) -> int => 1 }\n{USE}\nfn main() {}",
        "fn g(i: I) -> int => i.m()",
    ),
    (
        "extern_member",
        "extern \"C\" {\n    {A}\n    fn sqrt(x: float) -> float\n}\nfn main() { {USE} }",
        "print(sqrt(4.0))",
    ),
    ("param", "fn f({A} x: int) {}\nfn main() {}", ""),
    (
        "local_let",
        "fn main() {\n    {A}\n    let x = 1\n    print(x)\n}",
        "",
    ),
    ("local_stmt", "fn main() {\n    {A}\n    print(1)\n}", ""),
    (
        "closure_param",
        "fn main() {\n    let f = fn({A} x: int) => x\n    print(f(1))\n}",
        "",
    ),
    (
        "match_arm",
        "fn main() {\n    match 1 {\n        {A}\n        1 => print(1),\n        _ => print(2),\n    }\n}",
        "",
    ),
    (
        "struct_lit",
        "struct S { x: int }\nfn main() {\n    let s = S { {A} x: 1 }\n    print(s.x)\n}",
        "",
    ),
    ("call_arg", "fn main() {\n    print({A} 1)\n}", ""),
    ("generic", "fn f[{A} T]() {}\nfn main() {}", ""),
    (
        "type",
        "fn main() {\n    let x: {A} int = 1\n    print(x)\n}",
        "",
    ),
];

const ATTRIBUTES: &[&str] = &[
    "when(false)",
    "when(true)",
    "deprecated(\"old\")",
    "repr(C)",
    "link(\"m\")",
    "rename(\"k\")",
    "skip",
    "foo",
    "inline",
    "module",
    "host",
    "requires(fs)",
    "typed",
    "positional",
];

/// Positions where `@when` drops the item.
const WHEN_AT: &[&str] = &[
    "fn",
    "struct",
    "class",
    "enum",
    "interface",
    "impl",
    "impl_iface",
    "alias",
    "import",
    "extern",
    "test",
    "const",
    "let",
    "var",
    "let_wild",
];

/// Positions where `@deprecated` makes uses warn.
const DEPRECATED_AT: &[&str] = &[
    "fn",
    "struct",
    "class",
    "enum",
    "interface",
    "alias",
    "const",
    "let",
    "var",
    "field",
    "class_field",
    "impl_member",
    "impl_iface_member",
    "iface_member_body",
    "iface_member_nobody",
    "extern_member",
];

/// What an attribute does where it is accepted besides `@when` and `@deprecated`: these are
/// read by the code that handles the declaration (FFI layout, JSON keys), which has its own
/// tests; here they must be accepted at exactly these positions.
fn reads(attr: &str, position: &str) -> bool {
    match attr {
        "repr(C)" => position == "struct",
        "link(\"m\")" => position == "extern",
        "rename(\"k\")" | "skip" => position == "field" || position == "class_field",
        _ => false,
    }
}

fn program(template: &str, attr: &str, usage: &str) -> String {
    let attr = if attr.is_empty() {
        String::new()
    } else {
        format!("@{attr}")
    };
    template.replace("{A}", &attr).replace("{USE}", usage)
}

#[test]
fn every_attribute_at_every_position_is_effective_or_refused() {
    let mut problems = Vec::new();
    for &(position, template, usage) in POSITIONS {
        // Without the attribute the program is fine, so a refusal is the attribute's doing.
        let base = run(&program(template, "", ""));
        // (`stmt` has no `fn main`, and `import math` needs the host: those only need to parse.)
        if !base.errors.is_empty() && position != "import" {
            problems.push(format!(
                "{position}: the base program fails: {:?}",
                base.errors
            ));
            continue;
        }
        for &attr in ATTRIBUTES {
            let name = attr.split('(').next().unwrap();
            let with_use = if name == "deprecated" { usage } else { "" };
            let r = run(&program(template, attr, with_use));
            let refused = !r.errors.is_empty();
            let accepted = match name {
                "when" => WHEN_AT.contains(&position),
                "deprecated" => DEPRECATED_AT.contains(&position),
                _ => reads(attr, position),
            };
            let tag = format!("@{attr} at `{position}`");
            if !accepted {
                if !refused {
                    problems.push(format!("{tag}: silently accepted"));
                }
                continue;
            }
            if refused && !(position == "import" && r.errors.iter().all(|e| e.contains("math"))) {
                problems.push(format!("{tag}: refused: {:?}", r.errors));
                continue;
            }
            match name {
                // Effective: the item is gone (false) or kept (true).
                "when" => {
                    let kept = run(&program(template, "when(true)", ""));
                    // `let _ = ..` is a statement when it stays, and nothing when it is dropped.
                    let want = if attr == "when(false)" && position != "let_wild" {
                        kept.items - 1
                    } else {
                        kept.items
                    };
                    if r.items != want {
                        problems.push(format!("{tag}: {} items, expected {want}", r.items));
                    }
                }
                // Effective: the use warns, and only the use.
                "deprecated" if !r.warnings.iter().any(|w| w.contains("deprecated: old")) => {
                    problems.push(format!("{tag}: no warning: {:?}", r.warnings));
                }
                _ => {}
            }
        }
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[test]
fn a_refused_attribute_says_so_in_one_error() {
    // One error per attribute, never two for one mistake.
    for (src, want) in [
        (
            "@deprecated(\"x\")\ntest \"t\" {}\n",
            "`@deprecated` does not apply to a `test`",
        ),
        (
            "@deprecated(\"x\")\nextern \"C\" { fn sqrt(x: float) -> float }\n",
            "`@deprecated` does not apply to an `extern` block",
        ),
        (
            "@foo\nextern \"C\" { fn sqrt(x: float) -> float }\n",
            "unknown attribute `@foo`",
        ),
        ("@foo\ntest \"t\" {}\n", "unknown attribute `@foo`"),
    ] {
        let r = run(src);
        assert_eq!(r.errors, [want], "{src}");
    }
}

#[test]
fn a_name_left_out_by_when_says_so() {
    let r = run(
        "@when(os == \"linux\" && os == \"macos\")\nfn f() -> int => 1\nfn main() { print(f()) }\n",
    );
    assert_eq!(r.errors, ["unknown name `f`"]);
    assert_eq!(
        r.hints,
        ["`f` exists but is left out here by `@when(os == \"linux\" && os == \"macos\")`"]
    );
    // Types, constants and imports too.
    let r = run(
        "@when(false)\nstruct Gone { x: int }\n@when(  debug &&\n false )\nconst K = 1\n@when(false)\nimport math\nfn main() {\n    let g = Gone { x: 1 }\n    print(K)\n    print(math.abs(1))\n}\n",
    );
    let hints = r.hints.join("\n");
    assert!(
        hints.contains("`Gone` exists but is left out here by `@when(false)`"),
        "{hints}"
    );
    assert!(
        hints.contains("`K` exists but is left out here by `@when(debug && false)`"),
        "{hints}"
    );
    assert!(
        hints.contains("`math` exists but is left out here by `@when(false)`"),
        "{hints}"
    );
    let t = run("@when(false)\nstruct Gone { x: int }\nfn f(g: Gone) {}\nfn main() {}\n");
    assert!(
        t.hints.join("\n").contains("`Gone` exists but is left out"),
        "{:?}",
        t.hints
    );
}

#[test]
fn a_type_with_its_own_eq_says_to_write_ord_too() {
    let r = run(
        "struct V { a: int }\nimpl Eq for V { fn eq(self, o: V) -> bool => true }\nfn main() {\n    let x = V { a: 1 }\n    print(x < x)\n    var l = [x]\n    l.sort()\n}\n",
    );
    assert_eq!(r.errors.len(), 2, "{:?}", r.errors);
    assert!(
        r.hints
            .iter()
            .all(|h| h.contains("`V` has its own `impl Eq`")
                && h.contains("write `impl Ord for V")
                && h.contains("exactly when the values are equal")),
        "{:?}",
        r.hints
    );
    // A bound says the same.
    let r = run(
        "struct V { a: int }\nimpl Eq for V { fn eq(self, o: V) -> bool => true }\nfn m[T: Ord](a: T) {}\nfn main() { m(V { a: 1 }) }\n",
    );
    assert!(
        r.hints.iter().any(|h| h.contains("has its own `impl Eq`")),
        "{:?}",
        r.hints
    );
}

#[test]
fn a_script_does_not_guess_what_a_feature_name_means() {
    // No manifest: a name nobody passed is an error, with the way out.
    let r = run("@when(fsat)\nfn f() {}\nfn main() {}\n");
    assert_eq!(r.errors, ["unknown name `fsat` in `@when`"]);
    assert!(r.hints[0].starts_with(
        "a script has no manifest: pass `--feature fsat`, or make a package with `[features]` in `colibri.toml`"
    ), "{:?}", r.hints);
    // Passed on the command line, it is a feature.
    let mut fs = MemoryFs::new();
    fs.insert("m.colibri", "@when(fast)\nfn f() {}\nfn main() {}\n");
    let mut sources = SourceMap::new();
    let loaded = load_with(
        &fs,
        &mut sources,
        Entry::File(std::path::Path::new("m.colibri")),
        &[],
        LoadOptions {
            all_modules: false,
            cfg: BuildCfg::default().with_feature("fast"),
            std_sources: Vec::new(),
        },
    )
    .unwrap();
    assert!(!loaded.has_errors(), "{:?}", loaded.diagnostics);
}

#[test]
fn checking_without_a_loader_answers_when_with_not_supported_yet() {
    // `check` and `check_with_host` take one parsed file and never see the loader, which is what
    // decides `@when` (EMBEDDING §13): they say so instead of keeping or dropping the item.
    let found = errors("@when(debug)\nfn f() {}\nfn main() {}\n");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, "the attribute `@when` is not supported yet");
}
