//! The checker never panics, whatever it is given: every DESIGN.md example, mangled
//! variants of them, and random token soup.

use colibri_syntax::{FileId, parse_file};

const F: FileId = FileId(0);

/// Parses and checks `src`; the result does not matter, only that nothing panics.
fn check_str(src: &str) -> usize {
    let (ast, _) = parse_file(F, src);
    colibri_check::check(&ast).diagnostics.len()
}

/// The text of every ```colibri block in DESIGN.md with its start line.
fn corpus() -> Vec<(usize, String)> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/DESIGN.md");
    let text = std::fs::read_to_string(path).expect("DESIGN.md is readable");
    let mut blocks = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (i, line) in text.lines().enumerate() {
        match (&mut current, line.trim_start()) {
            (None, l) if l.starts_with("```colibri") => current = Some((i + 1, String::new())),
            (Some(_), l) if l.starts_with("```") => blocks.extend(current.take()),
            (Some((_, body)), _) => {
                body.push_str(line);
                body.push('\n');
            }
            _ => {}
        }
    }
    blocks
}

/// Replaces the `...` placeholders of the doc with parseable text.
fn fill_placeholders(src: &str) -> String {
    let kept: Vec<&str> = src.lines().filter(|l| l.trim() != "...").collect();
    kept.join("\n")
        .replace("{ ... }", "{}")
        .replace("(...)", "()")
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn every_design_block_goes_through_the_checker_without_panicking() {
    let blocks = corpus();
    assert!(blocks.len() >= 25, "found only {} blocks", blocks.len());
    let mut diagnostics = 0;
    for (_, block) in &blocks {
        diagnostics += check_str(&fill_placeholders(block));
    }
    // The examples use features that are not implemented yet, so the checker has plenty to say.
    assert!(diagnostics > 0);
}

#[test]
fn truncated_and_scrambled_design_blocks_do_not_panic() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for (_, block) in corpus() {
        let src = fill_placeholders(&block);
        for end in (0..=src.len()).step_by(3) {
            if src.is_char_boundary(end) {
                check_str(&src[..end]);
                check_str(&src[end..]);
            }
        }
        let chars: Vec<char> = src.chars().collect();
        for _ in 0..20 {
            let mut copy = chars.clone();
            for _ in 0..4 {
                let i = (rng.next() % copy.len() as u64) as usize;
                let j = (rng.next() % copy.len() as u64) as usize;
                if rng.next().is_multiple_of(2) {
                    copy.remove(i);
                    if copy.is_empty() {
                        break;
                    }
                } else {
                    copy.swap(i, j);
                }
            }
            check_str(&copy.into_iter().collect::<String>());
        }
    }
}

#[test]
fn token_soup_does_not_panic() {
    const PIECES: &[&str] = &[
        "fn",
        "let",
        "var",
        "const",
        "if",
        "else",
        "while",
        "break",
        "continue",
        "return",
        "main",
        "print",
        "{",
        "}",
        "(",
        ")",
        "[",
        "]",
        ",",
        ".",
        "=>",
        "->",
        "=",
        "==",
        "+=",
        "<",
        "+",
        "-",
        "*",
        "/",
        "%",
        "&&",
        "||",
        "!",
        "x",
        "y",
        "f",
        "int",
        "float",
        "str",
        "bool",
        "1",
        "2.5",
        "0xFF",
        "9223372036854775808",
        "\"s\"",
        "\"a{x}b\"",
        "\"{x:.2}\"",
        "\"{1:q}\"",
        "'a'",
        "'outer",
        ":",
        ";",
        "\n",
        "match",
        "for",
        "struct",
        "enum",
        "impl",
        "import",
        "some",
        "none",
        "ok",
        "err",
        "?",
        "or",
        "as",
        "is",
        "()",
        "_",
        "@inline",
        "pub",
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..8000 {
        let n = 1 + rng.next() % 40;
        let src: String = (0..n)
            .map(|_| PIECES[(rng.next() % PIECES.len() as u64) as usize])
            .collect::<Vec<_>>()
            .join(" ");
        check_str(&src);
    }
}

#[test]
fn programs_built_from_statements_do_not_panic() {
    // Valid-looking functions with random statement shuffles exercise the body checker.
    const STMTS: &[&str] = &[
        "let a = 1",
        "var b = 2.5",
        "a = 3",
        "b += 1",
        "print(a)",
        "print(\"{b:.1}\")",
        "return",
        "return a",
        "if a > 1 { print(a) } else { print(b) }",
        "while a < 3 { a += 1 }",
        "break",
        "continue",
        "f(1)",
        "f(a, b: 2)",
        "g(a)",
        "let c = if a > 0 { 1 } else { 2 }",
        "let a = \"s\"",
        "{ let d = 1 }",
    ];
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..3000 {
        let n = 1 + rng.next() % 8;
        let body: Vec<&str> = (0..n)
            .map(|_| STMTS[(rng.next() % STMTS.len() as u64) as usize])
            .collect();
        let src = format!(
            "fn f(x: int, b: float = 1) -> int {{\n{}\n}}\nlet g = 1\nfn main() {{\n{}\n}}\n",
            body.join("\n"),
            body.iter().rev().cloned().collect::<Vec<_>>().join("\n")
        );
        check_str(&src);
    }
}

#[test]
fn deeply_nested_input_is_an_error_not_a_crash() {
    let nested = format!("let x = {}1{}\n", "(".repeat(500), ")".repeat(500));
    check_str(&nested);
    // A long chain is fine with room to recurse and an error without it.
    let chain = format!("let x = 1{}\n", " + 1".repeat(900));
    let (ast, _) = parse_file(F, &chain);
    let roomy = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            colibri_check::check_with_stack_budget(&ast, 200 << 20)
                .diagnostics
                .len()
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(roomy, 0);
    let (ast, _) = parse_file(F, &chain);
    let tight = colibri_check::check_with_stack_budget(&ast, 64 << 10);
    assert!(
        tight
            .diagnostics
            .iter()
            .any(|d| d.message.contains("nested too deeply"))
    );
    let blocks = format!(
        "fn f() {}{}{}\n",
        "{ ",
        "if true { ".repeat(60),
        "} ".repeat(61)
    );
    check_str(&blocks);
}
