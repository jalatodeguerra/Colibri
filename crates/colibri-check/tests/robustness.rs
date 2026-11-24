//! The checker never panics, whatever it is given: every example of the corpus, mangled
//! variants of them, and random token soup.

use colibri_syntax::{FileId, parse_file, parse_interface_file};

const F: FileId = FileId(0);

/// The text of `src` when it is an interface file, with its `@interface(..)` header blanked out.
fn interface_text(src: &str) -> Option<String> {
    let mut offset = 0;
    for line in src.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with("@interface(") {
            let content = line.trim_end_matches(['\n', '\r']);
            let mut masked = src[..offset].to_string();
            masked.push_str(&" ".repeat(content.len()));
            masked.push_str(&src[offset + content.len()..]);
            return Some(masked);
        }
        if !(t.is_empty() || t.starts_with("//")) {
            return None;
        }
        offset += line.len();
    }
    None
}

/// Parses and checks `src`; the result does not matter, only that nothing panics.
fn check_str(src: &str) -> usize {
    // A host's interface file (`@interface(..)` header, functions without bodies) is parsed
    // as one, with the header blanked out as the loader does.
    let (ast, _) = match interface_text(src) {
        Some(text) => parse_interface_file(F, &text),
        None => parse_file(F, src),
    };
    colibri_check::check(&ast).diagnostics.len()
}

/// The text of every ```colibri block in tests/corpus/design_examples.md with its start line.
fn corpus() -> Vec<(usize, String)> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/corpus/design_examples.md"
    );
    let text = std::fs::read_to_string(path).expect("the examples corpus is readable");
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
        "let mut",
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
        "use",
        "weak",
        "..",
        "..=",
        "fn(x)",
        "fn() {",
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
        "u8",
        "i32",
        "u64",
        "f32",
        "<<",
        ">>",
        "&",
        "|",
        "^",
        "~",
        "<<=",
        ">>=",
        "&=",
        "|=",
        "^=",
        ".MAX",
        "u8(",
        "wrapping_add",
        "255",
        "18446744073709551615",
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
        "let mut b = 2.5",
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
        "let mut m = {\"a\": 1}",
        "let e: Map[str, int] = {:}",
        "xs[0] = a",
        "xs[a] += 1",
        "m[\"a\"] += 1",
        "xs.push(a)",
        "print(xs.pop())",
        "for x in xs { print(x) }",
        "for mut x in xs { x += 1 }",
        "for (k, v) in m { print(k) }",
        "for (i, x) in xs.enumerate() { print(i) }",
        "defer print(a)",
        "defer xs.clear()",
        "let o: int? = some(a)",
        "if let some(v) = o { print(v) }",
        "let s = Set([1, 2])",
        "let c = xs.clone()",
        "print(xs == c)",
        "let f2 = fn(q: int) => q + a",
        "let f3 = fn(q) => q",
        "let f4 = fn() { a += 1 }",
        "let f5: fn(int) -> int = fn(q) => q * 2",
        "print(f5(a))",
        "print(xs.map(fn(q) => q + a))",
        "xs.retain(fn(q) => q > a)",
        "xs.sort_by(fn(p, q) => p - q)",
        "print(xs[1..])",
        "print(xs.fold(0, fn(acc, q) => acc + q))",
        "let r = 1..5",
        "for i in r { print(i) }",
        "let fs = [fn() => 1, fn() => 2]",
        "print(\"abc\"[..2])",
        "let r1 = res(a)",
        "let r2 = res(a)?",
        "let r3 = res(a) or 0",
        "let r4 = res(a) or res(1) or 2",
        "let r5 = res(a) or |e| { return 0 }",
        "let r6 = res(a) or { return 1 }",
        "let r7 = opt(a)?",
        "let r8 = opt(a) or { continue }",
        "let r9 = opt(a) or |e| { break }",
        "let r10 = res(a).context(\"c\")?",
        "let r11 = res(a).map(fn(q) => q + 1).and_then(fn(q) => res(q))",
        "let r12: Result[int] = err(\"boom\")",
        "let r13: Result[int, str] = ok(a)",
        "if let err(e) = res(a) { print(e) }",
        "match res(a) { ok(v) => print(v), err(e) => print(e) }",
        "print(res(a).is_ok())",
        "print(res(a).expect(\"m\"))",
        "print(opt(a).expect(\"m\"))",
        "let ee = Error.from(\"x\")",
        "print(Error.from(a).is[int]())",
        "print(Error.from(a).as[int]())",
        "print(Error.from(a).message())",
        "print(Error.from(a).context(\"c\").cause())",
        "panic(\"p\")",
        "ok(a)?",
        "err(a)",
        "let r14 = res(a)?.nope",
        "let r15 = (res(a) or 0)?",
    ];
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..3000 {
        let n = 1 + rng.next() % 8;
        let body: Vec<&str> = (0..n)
            .map(|_| STMTS[(rng.next() % STMTS.len() as u64) as usize])
            .collect();
        let src = format!(
            "fn res(x: int) -> Result[int, str] => ok(x)\nfn opt(x: int) -> int? => some(x)\nfn f(x: int, b: float = 1) -> int {{\n{}\n}}\nlet g = 1\nfn main() {{\n{}\n}}\n",
            body.join("\n"),
            body.iter().rev().cloned().collect::<Vec<_>>().join("\n")
        );
        check_str(&src);
    }
}

#[test]
fn closure_programs_do_not_panic() {
    // Closures, function types, weak captures, iterators and slices, shuffled and mangled.
    const FRAGMENTS: &[&str] = &[
        "let a = 1",
        "let mut n = 0",
        "class K { v: int, cb: fn(int) -> int }",
        "struct S { f: fn() }",
        "let f = fn(x: int) -> int => x + n",
        "let g = fn(x) => x",
        "let h = fn() { n += 1 }",
        "let k = fn() use(weak a) => 1",
        "let k = fn() use(weak) {}",
        "let k = fn() use(weak n, weak n) => 1",
        "let k = fn() use(strong n) => 1",
        "let m = fn(mut q: int) { q += 1 }",
        "let p = fn(a: int, a: int) => a",
        "let q = fn(x: int) -> fn(int) -> int { return fn(y: int) => x + y }",
        "let r = fn() { return }",
        "let r = fn() -> int { }",
        "let r = fn() -> int { return \"s\" }",
        "fn make() -> fn() -> int { let mut c = 0\n return fn() { c += 1\n return c } }",
        "fn apply[T, U](x: T, f: fn(T) -> U) -> U => f(x)",
        "fn id[T](x: T) -> T => x",
        "fn bad(f: fn(int) -> ) {}",
        "fn bad(f: fn(int -> int) {}",
        "print(apply(1, fn(z) => z))",
        "print(apply(1, id))",
        "let i: fn(int) -> int = id",
        "let i = id[int]",
        "let i = id[int, int]",
        "print(f(1))",
        "print(f(1, 2))",
        "print(f(a: 1))",
        "print(f == f)",
        "print(f)",
        "print(make()())",
        "print((fn() => 1)())",
        "print(n())",
        "let xs = [3, 1, 2]",
        "print(xs.map(fn(q) => q * 2))",
        "print(xs.filter(fn(q) => q))",
        "xs.sort_by(fn(p, q) => p < q)",
        "xs.sort_by_key(fn(p) => 1.5)",
        "xs.retain(fn(q) { xs.push(q)\n return true })",
        "print(xs.fold(0, fn(acc, q) => acc + q))",
        "print(xs.fold(\"\", fn(acc, q) => acc + q))",
        "print(xs[1..2])",
        "print(xs[..])",
        "print(xs[2..1])",
        "print(xs[1..] = 1)",
        "xs[0..1] = [1]",
        "let t = \"héllo\"",
        "print(t[0..2])",
        "print(t[..9])",
        "print(t.split(\"\"))",
        "print(t.replace(\"\", \"x\"))",
        "for c in t.chars() { print(c) }",
        "for (i, c) in t.chars().enumerate() { print(i) }",
        "let mut it = xs.iter()",
        "print(it.next())",
        "xs.push(1)",
        "let w = 1..",
        "let w = ..5",
        "let w = 1.0..2.0",
        "let w = 1..=3",
        "print(w.len())",
        "print(w.rev())",
        "for z in w.rev() { print(z) }",
        "for z in w { print(z) }",
        "for z in 5 { print(z) }",
        "let kk = K { v: 1, cb: fn(z) => z }",
        "print(kk.cb(1))",
        "print(kk.v(1))",
        "let ss = S { f: fn() {} }",
        "ss.f()",
        "impl Iter[int] for S { fn next(mut self) -> int? => none }",
        "impl Iterable[int] for K { fn iter(self) -> Iter[int] => S { f: fn() {} } }",
        "for z in ss { print(z) }",
        "for z in kk { print(z) }",
        "print(ss.enumerate())",
        "let c2 = fn() => c2()",
        "let c3 = fn() => (fn() => c3)()",
        "print(K.nope)",
        "print(S.f)",
        "let e: Iter[int] = xs.iter()",
        "let e = xs.iter().enumerate().enumerate()",
        "let t1 = fn() => ok(1)?",
        "let t2 = fn() -> Result[int] { let v = ok(1)?\n return ok(v) }",
        "let t3 = fn(q) => q?",
        "let t4 = fn() => none or 1",
        "let t5 = fn() { let v = err(\"x\") or |e| { return }\n print(v) }",
        "let t6 = xs.map(fn(q) => ok(q)?)",
    ];
    let mut rng = Rng(0x0dd5_1ced_cafe_f00d);
    for _ in 0..4000 {
        let n = 1 + rng.next() % 9;
        let body: Vec<&str> = (0..n)
            .map(|_| FRAGMENTS[(rng.next() % FRAGMENTS.len() as u64) as usize])
            .collect();
        let src = format!("fn main() {{\n{}\n}}\n", body.join("\n"));
        check_str(&src);
        // The same fragments at module level, where declarations are allowed.
        check_str(&body.join("\n"));
        // And cut off in the middle.
        let cut = (rng.next() % (src.len() as u64 + 1)) as usize;
        if src.is_char_boundary(cut) {
            check_str(&src[..cut]);
        }
    }
}

#[test]
fn numeric_programs_do_not_panic() {
    // Sized integers, `f32`, conversions, constants and the bitwise operators, shuffled.
    const FRAGMENTS: &[&str] = &[
        "let a: u8 = 255",
        "let b: u8 = 256",
        "let c: i8 = -129",
        "let mut d: u64 = 18446744073709551615",
        "let e: u64 = 18446744073709551616",
        "let f: f32 = 0.1",
        "let g: float = f",
        "let h: f32 = g",
        "let i: int = a",
        "let j = a + b",
        "let k = a & b",
        "let l = a << 3",
        "let m = d >> 63",
        "let n = d << d",
        "let o = ~a",
        "let p = ~f",
        "let q = f & 1",
        "let r = true | false",
        "let s = -a",
        "let t = -d",
        "let u = u8(1000)",
        "let v = u8(f)",
        "let w = f32(a, b)",
        "let x = int(\"1\")",
        "let y = u8.MAX + 1",
        "let z = i64.MIN.abs()",
        "let a1 = f32.NAN.is_nan()",
        "let a2 = a.wrapping_add(b)",
        "let a3 = a.checked_mul(a) or 0",
        "let a4 = a.pow(f)",
        "let a5 = a.nothing()",
        "let a6 = u8.NOPE",
        "let a7 = u8",
        "d <<= 64",
        "d >>= a",
        "a &= 1.5",
        "a |= true",
        "a ^= a",
        "f += g",
        "g += f",
        "f <<= 1",
        "for q in 0..a { print(q) }",
        "for q in 0..d { print(q) }",
        "let m1 = match a { 0 => 1, 256 => 2, 1..=300 => 3, _ => 4 }",
        "let m2: Set[f32] = Set([])",
        "let m3: List[u8] = [1, 2, 300]",
        "print(m3.sum())",
        "print(\"{a:x} {f:x} {d:b} {f:.2}\")",
        "let m4 = a as u16",
        "impl Show for u8 { fn show(self) -> str => \"x\" }",
        "let m5 = 1 << 100",
        "let m6 = 0xFFFF_FFFF_FFFF_FFFF_F",
        "let m7 = -9223372036854775809",
        "let m8 = - - 1",
        "let m9 = ~~~a",
        "let n1 = a == b",
        "let n2 = f == g",
        "let n3 = f < 1",
        "let n4 = 1.5 < a",
    ];
    let mut rng = Rng(0x7a5e_11c0_ffee_d00d);
    for _ in 0..4000 {
        let n = 1 + rng.next() % 9;
        let body: Vec<&str> = (0..n)
            .map(|_| FRAGMENTS[(rng.next() % FRAGMENTS.len() as u64) as usize])
            .collect();
        let prefix = "let a: u8 = 1\nlet b: u8 = 2\nlet f: f32 = 0.5\nlet g: float = 2.0\nlet mut d: u64 = 3\n";
        let src = format!("fn main() {{\n{prefix}{}\n}}\n", body.join("\n"));
        check_str(&src);
        check_str(&body.join("\n"));
        let cut = (rng.next() % (src.len() as u64 + 1)) as usize;
        if src.is_char_boundary(cut) {
            check_str(&src[..cut]);
        }
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
        "let mut q: Map[Sq, int] = {:}",
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
