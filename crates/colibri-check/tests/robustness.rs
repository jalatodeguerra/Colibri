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
        "class",
        "weak",
        "defer",
        "List",
        "Map",
        "Set",
        "[]",
        "{:}",
        "..",
        "Node",
        "clone",
        "enumerate",
        "chars",
        "push",
        "interface",
        "Self",
        "self",
        "T",
        "U",
        "Drop",
        "Show",
        "Eq",
        "Ord",
        "Hash",
        "Clone",
        "Shape",
        "area",
        "drop",
        "[T]",
        "[int]",
        "[T: Ord]",
        "impl[T]",
        "for T",
        "+",
        "Box[T]",
        "Pair[int, str]",
        "type",
        "Dyn",
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
        "let xs = [1, 2, 3]",
        "let xs: List[int] = []",
        "var m = {\"a\": 1}",
        "let e: Map[str, int] = {:}",
        "xs[0] = a",
        "xs[a] += 1",
        "m[\"a\"] += 1",
        "xs.push(a)",
        "print(xs.pop())",
        "for x in xs { print(x) }",
        "for var x in xs { x += 1 }",
        "for (k, v) in m { print(k) }",
        "for (i, x) in xs.enumerate() { print(i) }",
        "defer print(a)",
        "defer xs.clear()",
        "let o: int? = some(a)",
        "if let some(v) = o { print(v) }",
        "let s = Set([1, 2])",
        "let c = xs.clone()",
        "print(xs == c)",
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
fn generic_and_interface_programs_do_not_panic() {
    // Declarations of generics, interfaces and impls, shuffled with uses of them.
    const DECLS: &[&str] = &[
        "fn id[T](x: T) -> T => x",
        "fn max[T: Ord](a: T, b: T) -> T { if a > b { return a } return b }",
        "fn bad[T: Nope](a: T) -> T => a",
        "fn twice[T: Show + Clone](a: T) -> (T, T) => (a, a.clone())",
        "fn dup[T, T](a: T) {}",
        "struct Box[T] { v: T }",
        "struct Pair[A, B] { a: A, b: B }",
        "enum Tree[T] { Leaf, Node(Tree[T], T, Tree[T]) }",
        "class Cell[T] { v: T }",
        "type Grid[T] = List[List[T]]",
        "type Loop[T] = Loop[T]",
        "interface Shape { fn area(self) -> float\n fn name(self) -> str => \"s\" }",
        "interface Gen[T] { fn get(self) -> T }",
        "interface Mixed { fn make() -> int }",
        "interface Dup { fn a(self) fn a(self) }",
        "struct Sq { s: float }",
        "class Res { n: int }",
        "impl Shape for Sq { fn area(self) -> float => self.s }",
        "impl Shape for Sq { }",
        "impl Shape for int { fn area(self) -> float => 1.0 }",
        "impl Show for int { fn show(self) -> str => \"i\" }",
        "impl[T] Shape for Box[T] { fn area(self) -> float => 1.0 }",
        "impl[T: Ord] Box[T] { fn big(self, o: T) -> bool => self.v > o }",
        "impl[A, B] Pair[A, B] { fn swap(self) -> Pair[B, A] => Pair { a: self.b, b: self.a } }",
        "impl Gen[int] for Sq { fn get(self) -> int => 1 }",
        "impl Drop for Res { fn drop(self) { print(self.n) } }",
        "impl Drop for Sq { fn drop(self) {} }",
        "impl Eq for Sq { fn eq(self, o: Sq) -> bool => true }",
        "impl Ord for Sq { fn cmp(self, o: Sq) -> int => 0 }",
        "impl Clone for Sq { fn clone(self) -> Sq => self }",
        "impl Hash for Sq { fn hash(self) -> int => 1 }",
        "impl Nope for Sq {}",
        "impl Box { }",
    ];
    const USES: &[&str] = &[
        "let a = id(1)",
        "let b = id[str](\"s\")",
        "let c = id[int, int](1)",
        "let d = max(1, 2)",
        "let e = max(Sq { s: 1.0 }, Sq { s: 2.0 })",
        "let f: Box[int] = Box { v: 1 }",
        "let g = Box { v: [] }",
        "let h = Pair { a: 1, b: \"x\" }.swap()",
        "let i: Tree[int] = .Leaf",
        "let j = Tree.Node(.Leaf, 1, .Leaf)",
        "let k: Shape = Sq { s: 1.0 }",
        "let l = Sq { s: 2.0 } as Shape",
        "let m = [Sq { s: 1.0 }, Sq { s: 2.0 }]",
        "let n: List[Shape] = [Sq { s: 1.0 }]",
        "print(k.area())",
        "print(k.name())",
        "print(k.nope())",
        "print(Sq { s: 1.0 }.area())",
        "let o = Res { n: 1 }",
        "let p = Cell { v: o }",
        "print(Sq { s: 1.0 } == Sq { s: 2.0 })",
        "print(Sq { s: 1.0 } < Sq { s: 2.0 })",
        "var q: Map[Sq, int] = {:}",
        "let r = Box { v: 1 }.big(2)",
        "let s: Grid[int] = [[1]]",
        "let t = Box.v",
        "let u = Gen",
        "print(Sq { s: 1.0 }.get())",
        "print(5.area())",
        "print(twice(Box { v: 1 }))",
    ];
    let mut rng = Rng(0xfeed_beef_1357_9bdf);
    for _ in 0..4000 {
        let decls: Vec<&str> = (0..1 + rng.next() % 8)
            .map(|_| DECLS[(rng.next() % DECLS.len() as u64) as usize])
            .collect();
        let uses: Vec<&str> = (0..1 + rng.next() % 6)
            .map(|_| USES[(rng.next() % USES.len() as u64) as usize])
            .collect();
        let src = format!(
            "{}\nfn main() {{\n{}\n}}\n",
            decls.join("\n"),
            uses.join("\n")
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

/// Every `.colibri` file under `tests/golden`, as source text.
fn golden_sources() -> Vec<String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "colibri")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                out.push(text);
            }
        }
    }
    let mut out = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden"),
        &mut out,
    );
    out
}

#[test]
fn truncated_and_scrambled_golden_programs_do_not_panic() {
    let sources = golden_sources();
    assert!(sources.len() > 200, "found only {} files", sources.len());
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for src in &sources {
        for end in (0..=src.len()).step_by(7) {
            if src.is_char_boundary(end) {
                check_str(&src[..end]);
            }
        }
        let chars: Vec<char> = src.chars().collect();
        if chars.is_empty() {
            continue;
        }
        for _ in 0..6 {
            let mut copy = chars.clone();
            for _ in 0..3 {
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
