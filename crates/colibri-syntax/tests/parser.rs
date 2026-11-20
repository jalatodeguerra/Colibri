//! Parser tests: S-expression dumps, error messages, recovery and the
//! examples corpus (tests/corpus/design_examples.md).

use colibri_syntax::ast::{ExprKind, ItemKind, StmtKind, TopLevel};
use colibri_syntax::ast_dump::dump_file;
use colibri_syntax::{Diagnostic, FileId, parse_file, parse_interface_file};

const F: FileId = FileId(0);

/// Parses `src`, asserts there are no diagnostics, returns the dump.
fn dump(src: &str) -> String {
    let (file, diags) = parse_file(F, src);
    assert!(
        diags.is_empty(),
        "unexpected diagnostics for {src:?}: {diags:#?}"
    );
    dump_file(&file)
}

/// Asserts that every `(source, expected dump)` pair matches.
fn check(cases: &[(&str, &str)]) {
    for (src, expected) in cases {
        assert_eq!(dump(src), *expected, "source: {src:?}");
    }
}

/// Parses `src` and returns its dump and diagnostics.
fn parse_dump(src: &str) -> (String, Vec<Diagnostic>) {
    let (file, diags) = parse_file(F, src);
    (dump_file(&file), diags)
}

/// The single error diagnostic of `src`.
fn one_error(src: &str) -> Diagnostic {
    let (_, diags) = parse_dump(src);
    assert_eq!(diags.len(), 1, "diagnostics for {src:?}: {diags:#?}");
    diags.into_iter().next().unwrap()
}

/// The source text a diagnostic points at.
fn pointed_at<'a>(src: &'a str, d: &Diagnostic) -> &'a str {
    &src[d.span.start as usize..d.span.end as usize]
}

// ---- precedence and associativity ----

#[test]
fn precedence_of_every_level() {
    check(&[
        ("1 + 2 * 3", "(binary + (int 1) (binary * (int 2) (int 3)))"),
        ("a * b + c", "(binary + (binary * a b) c)"),
        ("a as T * 2", "(binary * (as a T) (int 2))"),
        ("-a as T", "(as (unary - a) T)"),
        ("-a.b", "(unary - (field a b))"),
        ("!a?", "(unary ! (try a))"),
        ("a + b..c", "(range .. (binary + a b) c)"),
        ("a..b + c", "(range .. a (binary + b c))"),
        ("a << 1 + 2", "(binary << a (binary + (int 1) (int 2)))"),
        ("a & b << c", "(binary & a (binary << b c))"),
        ("a ^ b & c", "(binary ^ a (binary & b c))"),
        ("a | b ^ c", "(binary | a (binary ^ b c))"),
        ("a is b | c", "(binary is a (binary | b c))"),
        ("a == b is c", "(binary == a (binary is b c))"),
        ("a && b == c", "(binary && a (binary == b c))"),
        ("a || b && c", "(binary || a (binary && b c))"),
        ("a or b || c", "(or a (binary || b c))"),
        ("~a | b", "(binary | (unary ~ a) b)"),
        (
            "a or b || c && d == e is f | g ^ h & i << j + k * l as T",
            "(or a (binary || b (binary && c (binary == d (binary is e (binary | f (binary ^ g \
             (binary & h (binary << i (binary + j (binary * k (as l T))))))))))))",
        ),
    ]);
}

#[test]
fn associativity() {
    check(&[
        ("1 - 2 - 3", "(binary - (binary - (int 1) (int 2)) (int 3))"),
        ("a / b * c", "(binary * (binary / a b) c)"),
        ("a || b || c", "(binary || (binary || a b) c)"),
        ("a or b or c", "(or (or a b) c)"),
        ("a << b << c", "(binary << (binary << a b) c)"),
        ("x as A as B", "(as (as x A) B)"),
    ]);
}

#[test]
fn or_is_looser_than_everything_and_tighter_than_assignment() {
    check(&[
        ("a || b or c", "(or (binary || a b) c)"),
        ("a == b or c", "(or (binary == a b) c)"),
        ("x = a or b", "(assign = x (or a b))"),
        ("x += a || b or c", "(assign += x (or (binary || a b) c))"),
        ("f()? or g()", "(or (try (call f)) (call g))"),
        ("a?.b?", "(try (field (try a) b))"),
        (
            "cache.get(k) or disk.load(k) or fetch(k)?",
            "(or (or (call (field cache get) k) (call (field disk load) k)) (try (call fetch k)))",
        ),
    ]);
}

#[test]
fn ranges() {
    check(&[
        ("a..b", "(range .. a b)"),
        ("a..=b", "(range ..= a b)"),
        ("0..n + 1", "(range .. (int 0) (binary + n (int 1)))"),
        ("xs[1..3]", "(index xs (range .. (int 1) (int 3)))"),
        ("xs[..n]", "(index xs (range .. _ n))"),
        ("xs[a..]", "(index xs (range .. a _))"),
        ("xs[..]", "(index xs (range .. _ _))"),
        ("xs[..=n]", "(index xs (range ..= _ n))"),
    ]);
}

#[test]
fn chained_comparison_is_an_error_with_a_hint() {
    let src = "a < b < c";
    let d = one_error(src);
    assert_eq!(d.message, "comparison operators cannot be chained");
    assert_eq!(pointed_at(src, &d), "<");
    assert!(d.hint.as_deref().unwrap().contains("a < b && b < c"));
    assert_eq!(one_error("a == b != c").message, d.message);
    // Recovery still builds a tree.
    assert_eq!(parse_dump(src).0, "(binary < (binary < a b) c)");
}

#[test]
fn chained_ranges_are_an_error() {
    assert_eq!(one_error("a..b..c").message, "ranges cannot be chained");
}

#[test]
fn and_and_not_get_targeted_errors_and_recover() {
    let (out, diags) = parse_dump("let a = x and y");
    assert_eq!(out, "(let a (binary && x y))");
    assert_eq!(diags[0].message, "`and` is not an operator in Colibri");
    assert_eq!(diags[0].hint.as_deref(), Some("use `&&` instead of `and`"));

    let (out, diags) = parse_dump("let b = not x");
    assert_eq!(out, "(let b (unary ! x))");
    assert_eq!(diags[0].hint.as_deref(), Some("use `!` instead of `not`"));
}

// ---- literals and simple expressions ----

#[test]
fn literals_and_collections() {
    check(&[
        ("42", "(int 42)"),
        ("3.5", "(float 3.5)"),
        ("'a'", "(char 'a')"),
        ("true", "(bool true)"),
        ("none", "none"),
        ("[1, 2]", "(list (int 1) (int 2))"),
        ("[]", "(list)"),
        ("()", "(tuple)"),
        ("(1,)", "(tuple (int 1))"),
        ("(1, 2)", "(tuple (int 1) (int 2))"),
        ("(1)", "(paren (int 1))"),
        ("t.0.1", "(tindex (tindex t 0) 1)"),
        ("self.x", "(field self x)"),
        ("ok(v)", "(call ok v)"),
        ("some(v)", "(call some v)"),
        (".Move", "(dot Move)"),
        (
            ".Move(vec2(1, 0))",
            "(dot Move (call vec2 (int 1) (int 0)))",
        ),
        (
            "log(\"x\", level: \"warn\")",
            "(call log (str \"x\") level=(str \"warn\"))",
        ),
        ("e.as[T]()", "(call (index (field e as) T))"),
    ]);
}

#[test]
fn trailing_commas_are_allowed_everywhere() {
    check(&[
        ("f(1, 2,)", "(call f (int 1) (int 2))"),
        ("[1, 2,]", "(list (int 1) (int 2))"),
        ("(1, 2,)", "(tuple (int 1) (int 2))"),
        ("let m = {\"a\": 1,}", "(let m (map ((str \"a\") (int 1))))"),
        ("P { a: 1, b: 2, }", "(struct P (a (int 1)) (b (int 2)))"),
        ("f(\n  1,\n  2,\n)", "(call f (int 1) (int 2))"),
        ("fn f[T,](a: T,) {}", "(fn f [T] (params (a : T)) (block))"),
        ("enum E { A(int,), B, }", "(enum E (A int) B)"),
    ]);
}

#[test]
fn an_interface_parameter_may_have_a_default() {
    check(&[
        (
            "interface Add[R = Self, O = Self] { fn add(self, other: R) -> O }",
            "(interface Add [R = Self, O = Self] (fn add (params self (other : R)) -> O))",
        ),
        (
            "interface C[T: Eq + Hash = int] { fn f(self) }",
            "(interface C [T: Eq + Hash = int] (fn f (params self)))",
        ),
        ("fn f[T = int]() {}", "(fn f [T = int] (params) (block))"),
    ]);
}

// ---- strings ----

#[test]
fn interpolation_holes() {
    check(&[
        ("\"hi {name}!\"", "(str \"hi \" (interp name) \"!\")"),
        ("\"{a + b}\"", "(str (interp (binary + a b)))"),
        ("\"{p.pos}\"", "(str (interp (field p pos)))"),
        ("\"{{literal}}\"", "(str \"{literal}\")"),
        (
            "\"{x:.2} {n:>8} {id:x}\"",
            "(str (interp x \".2\") \" \" (interp n \">8\") \" \" (interp id \"x\"))",
        ),
        ("\"{m[\"key\"]}\"", "(str (interp (index m (str \"key\"))))"),
        (
            "\"{f(\"a{b}c\")}\"",
            "(str (interp (call f (str \"a\" (interp b) \"c\"))))",
        ),
        (
            "\"{xs.len() * 2}\"",
            "(str (interp (binary * (call (field xs len)) (int 2))))",
        ),
        ("r\"{raw}\"", "(str \"{raw}\")"),
        (
            "\"\"\"\n  a {x}\n  b\n\"\"\"",
            "(str \"a \" (interp x) \"\\nb\")",
        ),
    ]);
}

#[test]
fn interpolation_errors_point_into_the_hole() {
    let src = "let s = \"ab {x y}\"";
    let d = one_error(src);
    assert!(
        d.message.contains("in string interpolation"),
        "{}",
        d.message
    );
    assert_eq!(pointed_at(src, &d), "y");

    let src = "let s = \"{1 +}\"";
    let d = one_error(src);
    assert_eq!(d.message, "expected an expression, found end of file");
}

#[test]
fn spans_of_interpolated_expressions_are_absolute() {
    let src = "let s = \"a{m[\"k\"]}\"";
    let (file, _) = parse_file(F, src);
    let TopLevel::Item(item) = &file.items[0] else {
        panic!("expected an item");
    };
    let ItemKind::Binding(binding) = &item.kind else {
        panic!("expected a binding");
    };
    let ExprKind::Str(segs) = &binding.init.kind else {
        panic!("expected a string");
    };
    let colibri_syntax::ast::StrSeg::Interp { expr, .. } = &segs[1] else {
        panic!("expected a hole");
    };
    assert_eq!(
        &src[expr.span.start as usize..expr.span.end as usize],
        "m[\"k\"]"
    );
}

// ---- brace rules (DESIGN §3.5) ----

#[test]
fn block_versus_map() {
    check(&[
        ("{}", "(block)"),
        ("{ a() }", "(block (tail (call a)))"),
        ("let m = {:}", "(let m (map))"),
        (
            "let m = {\"a\": 1, \"b\": 2}",
            "(let m (map ((str \"a\") (int 1)) ((str \"b\") (int 2))))",
        ),
        ("f({\"a\": 1})", "(call f (map ((str \"a\") (int 1))))"),
        ("x = {k: v}", "(assign = x (map (k v)))"),
        (
            "let m = {\n  \"a\": 1\n  \"b\": 2\n}",
            "(let m (map ((str \"a\") (int 1)) ((str \"b\") (int 2))))",
        ),
        ("fn f() => {:}", "(fn f (params) (=> (map)))"),
        ("fn f() {}", "(fn f (params) (block))"),
        ("if a {} else {}", "(if a (block) (block))"),
        ("unsafe { x }", "(unsafe (block (tail x)))"),
        (
            "defer { cleanup() }",
            "(defer (block (tail (call cleanup))))",
        ),
        ("test \"t\" { }", "(test \"t\" (block))"),
        ("match x { A => {} }", "(match x (arm A (block)))"),
        (
            "match x { A => ({\"a\": 1}) }",
            "(match x (arm A (paren (map ((str \"a\") (int 1))))))",
        ),
    ]);
}

#[test]
fn empty_braces_in_an_expression_hint_at_the_empty_map() {
    let src = "let m = {}";
    let d = one_error(src);
    assert!(d.message.contains("`{}`"), "{}", d.message);
    assert!(d.hint.as_deref().unwrap().contains("{:}"));
    assert_eq!(pointed_at(src, &d), "{}");
}

#[test]
fn a_map_in_an_arm_needs_parentheses() {
    let src = "match x { A => {\"a\": 1} }";
    let d = one_error(src);
    assert!(d.hint.as_deref().unwrap().contains("({"), "{:?}", d.hint);
}

#[test]
fn a_map_entry_without_a_colon_explains_the_brace_rule() {
    let (_, diags) = parse_dump("let m = {a, b}");
    let d = &diags[0];
    assert_eq!(d.message, "expected `:` in a map entry, found `,`");
    assert!(d.hint.as_deref().unwrap().contains("block"));
}

#[test]
fn struct_literals_are_forbidden_in_heads_and_allowed_in_parentheses() {
    check(&[
        (
            "if p == (Player { name: \"x\" }) { 1 }",
            "(if (binary == p (paren (struct Player (name (str \"x\"))))) (block (tail (int 1))))",
        ),
        (
            "while p == (P { a: 1 }) {}",
            "(while (binary == p (paren (struct P (a (int 1))))) (block))",
        ),
        ("for x in foo {}", "(for x foo (block))"),
        ("match foo { _ => 1 }", "(match foo (arm _ (int 1)))"),
        (
            "for x in f(P { a: 1 }) {}",
            "(for x (call f (struct P (a (int 1)))) (block))",
        ),
        ("if a {} else {}", "(if a (block) (block))"),
        (
            "let v = if a { P { x: 1 } } else { P { x: 2 } }",
            "(let v (if a (block (tail (struct P (x (int 1))))) (block (tail (struct P (x (int 2)))))))",
        ),
        ("for i in 0.. {}", "(for i (range .. (int 0) _) (block))"),
    ]);
}

#[test]
fn a_struct_literal_in_an_if_head_is_one_error_with_a_hint() {
    let src = "if p == Player { name: \"x\" } { 1 }";
    let (out, diags) = parse_dump(src);
    assert_eq!(diags.len(), 1, "{diags:#?}");
    assert_eq!(diags[0].message, "struct literals are not allowed here");
    assert!(diags[0].hint.as_deref().unwrap().contains("(Player {"));
    // Recovery parses the literal anyway.
    assert!(out.starts_with("(if (binary == p (struct Player"), "{out}");
}

#[test]
fn struct_literal_forms() {
    check(&[
        ("Vec2i { x, y }", "(struct Vec2i x y)"),
        ("Vec2i { x: 1, y }", "(struct Vec2i (x (int 1)) y)"),
        (
            "Pair[int, str] { first: 1, second: \"a\" }",
            "(struct Pair[int, str] (first (int 1)) (second (str \"a\")))",
        ),
        ("game.Thing { a: 1 }", "(struct game.Thing (a (int 1)))"),
        ("Empty {}", "(struct Empty)"),
        (
            "P {\n  a: 1\n  b: 2\n}",
            "(struct P (a (int 1)) (b (int 2)))",
        ),
    ]);
}

// ---- generic instantiation versus indexing (DESIGN §3.5) ----

#[test]
fn generic_instantiation_and_indexing_share_one_node() {
    check(&[
        ("max[f32](a, b)", "(call (index max f32) a b)"),
        ("list[i](x)", "(call (index list i) x)"),
        ("xs[i]", "(index xs i)"),
        ("Map[str, int]()", "(call (index Map str int))"),
        ("List[List[int]]()", "(call (index List (index List int)))"),
        (
            "f[fn(int) -> int](g)",
            "(call (index f (ty fn(int) -> int)) g)",
        ),
        (
            "f[fn(int, str) -> (int, str)](g)",
            "(call (index f (ty fn(int, str) -> (int, str))) g)",
        ),
        ("foo[[u8; 4]]", "(index foo (ty [u8; (int 4)]))"),
        ("foo[weak Node]", "(index foo (ty weak Node))"),
        (
            "Channel[Msg](capacity: 64)",
            "(call (index Channel Msg) capacity=(int 64))",
        ),
        (
            "json.decode[Player](text)",
            "(call (index (field json decode) Player) text)",
        ),
        ("m[\"k\"]", "(index m (str \"k\"))"),
        ("xs[i + 1][j]", "(index (index xs (binary + i (int 1))) j)"),
    ]);
}

#[test]
fn a_bad_index_argument_reports_the_expression_error() {
    let d = one_error("x = a[1 +]");
    assert_eq!(d.message, "expected an expression, found `]`");
}

// ---- closures ----

#[test]
fn closures_of_every_form() {
    check(&[
        (
            "fn(x) => x * 2",
            "(closure (params x) (=> (binary * x (int 2))))",
        ),
        (
            "fn(x: int) -> int => x",
            "(closure (params (x : int)) -> int (=> x))",
        ),
        (
            "fn() { return 1 }",
            "(closure (params) (block (return (int 1))))",
        ),
        ("fn(var x) => x", "(closure (params (var x)) (=> x))"),
        (
            "fn() use(weak self) { run() }",
            "(closure (params) (use weak self) (block (tail (call run))))",
        ),
        (
            "fn(a, b) use(x, weak self) => a",
            "(closure (params a b) (use x weak self) (=> a))",
        ),
        (
            "xs.map(fn(p) => p.name)",
            "(call (field xs map) (closure (params p) (=> (field p name))))",
        ),
        (
            "b.on_click(fn() {\n  clicks += 1\n})",
            "(call (field b on_click) (closure (params) (block (assign += clicks (int 1)))))",
        ),
        ("let use = 1", "(let use (int 1))"),
    ]);
}

// ---- error handling forms (DESIGN §10.3) ----

#[test]
fn or_handler_blocks() {
    check(&[
        (
            "let c = load() or |e| { log(e); return }",
            "(let c (or-handler (call load) |e| (block (call log e) (return))))",
        ),
        (
            "let t = read(p) or { continue }",
            "(let t (or-handler (call read p) _ (block (continue))))",
        ),
        (
            "x = a or { 1 } or b",
            "(assign = x (or (or-handler a _ (block (tail (int 1)))) b))",
        ),
        (
            "let c = f()?\n    or |e| { 0 }",
            "(let c (or-handler (try (call f)) |e| (block (tail (int 0)))))",
        ),
        (
            "if load() or { return } { 1 }",
            "(if (or-handler (call load) _ (block (return))) (block (tail (int 1))))",
        ),
    ]);
}

// ---- if and match ----

#[test]
fn if_expressions() {
    check(&[
        (
            "let s = if x > 0 { \"pos\" } else { log(\"neg\"); \"neg\" }",
            "(let s (if (binary > x (int 0)) (block (tail (str \"pos\"))) (block (call log (str \"neg\")) (tail (str \"neg\")))))",
        ),
        (
            "if a { 1 } else if b { 2 } else { 3 }",
            "(if a (block (tail (int 1))) (if b (block (tail (int 2))) (block (tail (int 3)))))",
        ),
        (
            "if a {\n  1\n} else if b {\n  2\n} else {\n  3\n}",
            "(if a (block (tail (int 1))) (if b (block (tail (int 2))) (block (tail (int 3)))))",
        ),
        (
            "if let some(x) = m { x } else { 0 }",
            "(if-let (some x) m (block (tail x)) (block (tail (int 0))))",
        ),
        (
            "if a { 1 } else if let ok(v) = r { v }",
            "(if a (block (tail (int 1))) (if-let (ok v) r (block (tail v))))",
        ),
        (
            "fn max[T: Ord](a: T, b: T) -> T => if a > b { a } else { b }",
            "(fn max [T: Ord] (params (a : T) (b : T)) -> T (=> (if (binary > a b) (block (tail a)) (block (tail b)))))",
        ),
    ]);
}

#[test]
fn match_arms_take_expressions_blocks_and_statements() {
    check(&[
        (
            "match c {\n  Move(d) => pos += d\n  Quit => {}\n  Heal(n) => hp = n\n  _ => return\n}",
            "(match c (arm (variant Move d) (assign += pos d)) (arm Quit (block)) (arm (variant Heal n) (assign = hp n)) (arm _ (return)))",
        ),
        (
            "match c { A => 1, B => 2, }",
            "(match c (arm A (int 1)) (arm B (int 2)))",
        ),
        (
            "match c { A => 1\n B if x > 1 => 2 }",
            "(match c (arm A (int 1)) (arm B (if (binary > x (int 1))) (int 2)))",
        ),
        (
            "return match s { A => 1\n B => 2 }",
            "(return (match s (arm A (int 1)) (arm B (int 2))))",
        ),
        (
            "for x in xs { match x { None2 => continue, _ => break } }",
            "(for x xs (block (tail (match x (arm None2 (continue)) (arm _ (break))))))",
        ),
    ]);
}

// ---- patterns (DESIGN §8.2) ----

fn pat(p: &str) -> String {
    let out = dump(&format!("match x {{ {p} => 0 }}"));
    let inner = out
        .strip_prefix("(match x (arm ")
        .and_then(|s| s.strip_suffix(" (int 0)))"))
        .unwrap_or_else(|| panic!("unexpected dump {out}"));
    inner.to_string()
}

#[test]
fn every_kind_of_pattern() {
    let cases = [
        ("_", "_"),
        ("x", "x"),
        ("Quit", "Quit"),
        ("0", "(lit 0)"),
        ("-1", "(lit -1)"),
        ("-1.5", "(lit -1.5)"),
        ("\"s\"", "(lit \"s\")"),
        ("'a'", "(lit 'a')"),
        ("true", "(lit true)"),
        ("0..10", "(range .. 0 10)"),
        ("0..=9", "(range ..= 0 9)"),
        ("'a'..='z'", "(range ..= 'a' 'z')"),
        ("-5..0", "(range .. -5 0)"),
        ("Circle(r)", "(variant Circle r)"),
        ("Shape.Circle(r)", "(variant Shape.Circle r)"),
        (".Circle(r)", "(variant .Circle r)"),
        (".Quit", "(path .Quit)"),
        ("Shape.Point", "(path Shape.Point)"),
        ("Rect(w, h)", "(variant Rect w h)"),
        ("Rect(_, h)", "(variant Rect _ h)"),
        ("some(x)", "(some x)"),
        ("none", "none"),
        ("ok(v)", "(ok v)"),
        ("err(e)", "(err e)"),
        ("some(Circle(r))", "(some (variant Circle r))"),
        ("(a, b)", "(tuple a b)"),
        ("(a,)", "(tuple a)"),
        ("()", "(tuple)"),
        ("Vec2i { x, y }", "(struct Vec2i x y)"),
        ("Vec2i { x: px, y: _ }", "(struct Vec2i (x px) (y _))"),
        ("Vec2i { x, .. }", "(struct Vec2i x ..)"),
        ("a | b", "(or a b)"),
        (
            "Heal(n) | Regen(n)",
            "(or (variant Heal n) (variant Regen n))",
        ),
        ("Move(d) as cmd", "(bind (variant Move d) cmd)"),
        ("1 | 2 | 3", "(or (lit 1) (lit 2) (lit 3))"),
    ];
    for (src, expected) in cases {
        assert_eq!(pat(src), expected, "pattern {src}");
    }
}

#[test]
fn guards_and_binding_patterns() {
    check(&[
        (
            "match c { Move(d) if d.len() > 10 => 1, Heal(n) | Regen(n) => 2 }",
            "(match c (arm (variant Move d) (if (binary > (call (field d len)) (int 10))) (int 1)) (arm (or (variant Heal n) (variant Regen n)) (int 2)))",
        ),
        ("let (a, b) = f()", "(let (tuple a b) (call f))"),
        ("let Vec2i { x, y } = v", "(let (struct Vec2i x y) v)"),
    ]);
}

#[test]
fn pattern_idents_know_their_capitalisation() {
    let (file, _) = parse_file(F, "let Quit = 1\nlet x = 2");
    let names: Vec<bool> = file
        .items
        .iter()
        .map(|top| match top {
            TopLevel::Item(item) => match &item.kind {
                ItemKind::Binding(b) => match &b.pat.kind {
                    colibri_syntax::ast::PatKind::Ident(id) => id.is_capitalised(),
                    _ => panic!("expected an identifier pattern"),
                },
                _ => panic!("expected a binding"),
            },
            TopLevel::Stmt(_) => panic!("expected an item"),
        })
        .collect();
    assert_eq!(names, [true, false]);
}

#[test]
fn a_pattern_arm_needs_a_fat_arrow() {
    let d = one_error("match x { A -> 1 }");
    assert_eq!(
        d.message,
        "expected `=>` after the match pattern, found `->`"
    );
    assert!(d.hint.is_some());
}

// ---- statements ----

#[test]
fn statements() {
    check(&[
        ("let x = 1", "(let x (int 1))"),
        ("var y: int = 2", "(var y : int (int 2))"),
        ("const MAX = 100", "(const MAX (int 100))"),
        ("p.x += 1", "(assign += (field p x) (int 1))"),
        ("flags &= ~mask", "(assign &= flags (unary ~ mask))"),
        ("flags |= 1 << n", "(assign |= flags (binary << (int 1) n))"),
        ("h ^= h >> 7", "(assign ^= h (binary >> h (int 7)))"),
        ("x <<= 3", "(assign <<= x (int 3))"),
        ("x >>= n", "(assign >>= x n)"),
        ("a[i] -= 2", "(assign -= (index a i) (int 2))"),
        (
            "a *= 2\nb /= 2\nc %= 2",
            "(assign *= a (int 2))\n(assign /= b (int 2))\n(assign %= c (int 2))",
        ),
        ("t.0 = 1", "(assign = (tindex t 0) (int 1))"),
        ("go work(1)", "(go (call work (int 1)))"),
        ("defer f.close()", "(defer (call (field f close)))"),
        ("defer x = 1", "(defer (assign = x (int 1)))"),
        ("a(); b()", "(call a)\n(call b)"),
        (
            "while i < n { i += 1 }",
            "(while (binary < i n) (block (assign += i (int 1))))",
        ),
    ]);
}

#[test]
fn blocks_yield_their_trailing_expression_unless_ended_by_a_semicolon() {
    check(&[
        ("{ a(); b() }", "(block (call a) (tail (call b)))"),
        ("{ a(); b(); }", "(block (call a) (call b))"),
        ("{ a()\n b()\n}", "(block (call a) (tail (call b)))"),
        ("{ x = 1 }", "(block (assign = x (int 1)))"),
        (
            "{ if a { 1 } }",
            "(block (tail (if a (block (tail (int 1))))))",
        ),
    ]);
}

#[test]
fn loops_and_labels() {
    check(&[
        ("for x in xs { }", "(for x xs (block))"),
        (
            "for (i, x) in xs.enumerate() { }",
            "(for (tuple i x) (call (field xs enumerate)) (block))",
        ),
        (
            "for var e in enemies { e.hp -= 1 }",
            "(for var e enemies (block (assign -= (field e hp) (int 1))))",
        ),
        ("for i in 0..n { }", "(for i (range .. (int 0) n) (block))"),
        (
            "while let some(x) = it.next() { }",
            "(while-let (some x) (call (field it next)) (block))",
        ),
        (
            "'outer: for a in xs { for b in ys { break 'outer } }",
            "(for 'outer a xs (block (for b ys (block (break 'outer)))))",
        ),
        (
            "'o: while c { continue 'o }",
            "(while 'o c (block (continue 'o)))",
        ),
        ("break 'outer 5", "(break 'outer (int 5))"),
        ("break 5", "(break (int 5))"),
        ("break", "(break)"),
        ("continue", "(continue)"),
        ("return", "(return)"),
        ("{ return }", "(block (return))"),
    ]);
}

#[test]
fn newline_termination_and_continuation() {
    check(&[
        (
            "let names = players\n    .filter(fn(p) => p.alive())\n    .map(fn(p) => p.name)",
            "(let names (call (field (call (field players filter) (closure (params p) (=> (call (field p alive))))) map) (closure (params p) (=> (field p name)))))",
        ),
        (
            "let total = price * qty +\n            shipping",
            "(let total (binary + (binary * price qty) shipping))",
        ),
        ("let x = a\n    or b", "(let x (or a b))"),
        (
            "let x = a\n    && b\n    || c",
            "(let x (binary || (binary && a b) c))",
        ),
        (
            "let r = if a { 1 }\nelse { 2 }",
            "(let r (if a (block (tail (int 1))) (block (tail (int 2)))))",
        ),
        ("a()\nb()", "(call a)\n(call b)"),
        ("draw(\n  \"s\",\n  x, y\n)", "(call draw (str \"s\") x y)"),
    ]);
}

// ---- items ----

#[test]
fn functions() {
    check(&[
        (
            "fn add(a: int, b: int) -> int { return a + b }",
            "(fn add (params (a : int) (b : int)) -> int (block (return (binary + a b))))",
        ),
        (
            "fn greet(name: str) -> str => \"Hello, {name}!\"",
            "(fn greet (params (name : str)) -> str (=> (str \"Hello, \" (interp name) \"!\")))",
        ),
        (
            "fn log(msg: str, level: str = \"info\") { }",
            "(fn log (params (msg : str) (level : str = (str \"info\"))) (block))",
        ),
        (
            "pub fn max[T: Ord + Hash, U](a: T) -> T { }",
            "(fn pub max [T: Ord + Hash, U] (params (a : T)) -> T (block))",
        ),
        (
            "impl P { fn area(self) -> float => 0 }",
            "(impl P (fn area (params self) -> float (=> (int 0))))",
        ),
        (
            "impl P { fn step(var self, dt: float) {} }",
            "(impl P (fn step (params (var self) (dt : float)) (block)))",
        ),
        ("fn f() -> () {}", "(fn f (params) -> () (block))"),
        (
            "fn f() -> List[(int, str)] {}",
            "(fn f (params) -> List[(int, str)] (block))",
        ),
        (
            "fn f(cb: fn(int) -> bool) {}",
            "(fn f (params (cb : fn(int) -> bool)) (block))",
        ),
        (
            "fn f(p: ptr[u8], a: [u8; 4], o: int?, w: weak Node) {}",
            "(fn f (params (p : ptr[u8]) (a : [u8; (int 4)]) (o : int?) (w : weak Node)) (block))",
        ),
        (
            "fn f(p: game.Player) {}",
            "(fn f (params (p : game.Player)) (block))",
        ),
    ]);
}

#[test]
fn type_definitions() {
    check(&[
        (
            "struct Particle {\n  pos: vec2\n  life: float = 1.0\n}",
            "(struct Particle (pos : vec2) (life : float = (float 1.0)))",
        ),
        (
            "pub struct P { pub name: str, hp: i32 }",
            "(struct pub P (pub name : str) (hp : i32))",
        ),
        (
            "struct Pair[A, B] { first: A\n second: B }",
            "(struct Pair [A, B] (first : A) (second : B))",
        ),
        (
            "class Node {\n  children: List[Node] = []\n  parent: weak Node\n}",
            "(class Node (children : List[Node] = (list)) (parent : weak Node))",
        ),
        (
            "enum Command {\n  Move(vec2)\n  Heal(int)\n  Quit\n}",
            "(enum Command (Move vec2) (Heal int) Quit)",
        ),
        (
            "enum Dir { North, East, South, West }",
            "(enum Dir North East South West)",
        ),
        (
            "enum Shape { Circle(radius: float)\n Rect(w: float, h: float)\n Point }",
            "(enum Shape (Circle (radius : float)) (Rect (w : float) (h : float)) Point)",
        ),
        (
            "enum Tree[T] { Leaf\n Node(left: Tree[T], value: T, right: Tree[T]) }",
            "(enum Tree [T] Leaf (Node (left : Tree[T]) (value : T) (right : Tree[T])))",
        ),
        (
            "type Grid = Map[ivec2, Tile]",
            "(type Grid = Map[ivec2, Tile])",
        ),
        (
            "type Pairs[T] = List[(T, T)]",
            "(type Pairs [T] = List[(T, T)])",
        ),
        ("struct Empty {}", "(struct Empty)"),
    ]);
}

#[test]
fn interfaces_and_impls() {
    check(&[
        (
            "interface Shape {\n  fn area(self) -> float\n  fn name(self) -> str => \"shape\"\n}",
            "(interface Shape (fn area (params self) -> float) (fn name (params self) -> str (=> (str \"shape\"))))",
        ),
        (
            "interface Iter[T] { fn next(var self) -> T? }",
            "(interface Iter [T] (fn next (params (var self)) -> T?))",
        ),
        (
            "impl Circle { fn new() -> Circle => Circle { r: 1 } }",
            "(impl Circle (fn new (params) -> Circle (=> (struct Circle (r (int 1))))))",
        ),
        ("impl Shape for Circle {}", "(impl Shape for Circle)"),
        (
            "impl[T] Iface for List[T] {}",
            "(impl [T] Iface for List[T])",
        ),
        (
            "impl[T: Ord] Iface[T] for Tree[T] {}",
            "(impl [T: Ord] Iface[T] for Tree[T])",
        ),
    ]);
}

#[test]
fn field_attributes() {
    check(&[(
        "struct S { @rename(\"x\") a: int, @skip b: int = 1 }",
        "(struct S ((@rename (str \"x\")) a : int) ((@skip) b : int = (int 1)))",
    )]);
}

#[test]
fn modules_externs_tests_and_attributes() {
    check(&[
        ("import math", "(import math)"),
        ("import game.player", "(import game.player)"),
        ("import game.enemy as foe", "(import game.enemy as foe)"),
        (
            "import rapier.{World, Body}",
            "(import rapier {World, Body})",
        ),
        ("import a.{B as C, D}", "(import a {B as C, D})"),
        (
            "extern \"C\" {\n  fn Init(w: i32, title: cstr)\n  fn Closed() -> bool\n}",
            "(extern \"C\" (fn Init (params (w : i32) (title : cstr))) (fn Closed (params) -> bool))",
        ),
        (
            "test \"adds\" { assert(1 + 1 == 2) }",
            "(test \"adds\" (block (tail (call assert (binary == (binary + (int 1) (int 1)) (int 2))))))",
        ),
        (
            "@when(os == \"windows\")\n@link(\"user32\")\nextern \"C\" { fn M() -> i32 }",
            "(extern (@when (binary == os (str \"windows\"))) (@link (str \"user32\")) \"C\" (fn M (params) -> i32))",
        ),
        (
            "@repr(C)\nstruct V { x: f32 }",
            "(struct (@repr C) V (x : f32))",
        ),
        ("@inline\nfn f() {}", "(fn (@inline) f (params) (block))"),
        (
            "@deprecated(\"old\") pub fn f() {}",
            "(fn (@deprecated (str \"old\")) pub f (params) (block))",
        ),
        ("const GRAVITY = 9.81", "(const GRAVITY (float 9.81))"),
        ("let config = load()", "(let config (call load))"),
        ("var score = 0", "(var score (int 0))"),
        ("pub const X = 1", "(const pub X (int 1))"),
    ]);
}

#[test]
fn doc_comments_attach_to_items_fields_and_variants() {
    check(&[
        (
            "/// Does a thing.\n/// Twice.\nfn f() {}",
            "(fn (doc \"Does a thing.\\nTwice.\") f (params) (block))",
        ),
        (
            "struct S {\n  /// the x\n  x: int\n  y: int\n}",
            "(struct S ((doc \"the x\") x : int) (y : int))",
        ),
        (
            "enum E {\n  /// first\n  A\n  B\n}",
            "(enum E ((doc \"first\") A) B)",
        ),
        ("/// stray\nlet x = 1\n", "(let (doc \"stray\") x (int 1))"),
    ]);
}

#[test]
fn files_mix_items_and_statements() {
    let src = "import math\n\nlet p = player.spawn(\"Ana\")\nfn helper() {}\nprint(p)\n";
    assert_eq!(
        dump(src),
        "(import math)\n(let p (call (field player spawn) (str \"Ana\")))\n(fn helper (params) (block))\n(call print p)"
    );
    let (file, _) = parse_file(F, src);
    let kinds: Vec<&str> = file
        .items
        .iter()
        .map(|t| match t {
            TopLevel::Item(_) => "item",
            TopLevel::Stmt(_) => "stmt",
        })
        .collect();
    assert_eq!(kinds, ["item", "item", "item", "stmt"]);
}

// ---- syntax errors ----

#[test]
fn error_messages_say_what_was_expected_and_found() {
    let src = "foo(1 2)";
    let d = one_error(src);
    assert_eq!(d.message, "expected `,` or `)`, found number `2`");
    assert_eq!(pointed_at(src, &d), "2");

    let src = "fn f( { }";
    let d = one_error(src);
    assert_eq!(d.message, "expected a parameter name, found `{`");
    assert_eq!(pointed_at(src, &d), "{");

    let src = "let x\nlet y = 1";
    let d = one_error(src);
    assert!(
        d.message
            .starts_with("expected `=` and an initial value, found end of line")
    );
    assert!(d.hint.as_deref().unwrap().contains("let name = value"));
}

#[test]
fn assignment_in_a_condition() {
    let src = "if x = 5 { }";
    let d = one_error(src);
    assert_eq!(
        d.message,
        "expected a condition after `if`, found assignment `=`"
    );
    assert_eq!(pointed_at(src, &d), "=");
    assert!(d.hint.as_deref().unwrap().contains("`==`"));

    let d = one_error("while x = 5 { }");
    assert!(d.message.contains("`while`"));
    let d = one_error("let y = x = 5");
    assert!(
        d.hint
            .as_deref()
            .unwrap()
            .contains("assignment is a statement")
    );
}

#[test]
fn missing_brace_after_a_head() {
    let src = "if x > 0 print(x)";
    let d = one_error(src);
    assert_eq!(
        d.message,
        "expected `{` after the `if` condition, found identifier `print`"
    );
    assert!(d.hint.as_deref().unwrap().contains("must be a block"));
    assert!(
        one_error("for x in xs print(x)")
            .message
            .contains("`for` header")
    );
    assert!(
        one_error("while x print(x)")
            .message
            .contains("`while` condition")
    );
    assert!(
        one_error("fn f() print(1)")
            .message
            .contains("start the body of `f`")
    );
}

#[test]
fn local_items_are_rejected_with_a_hint_and_skipped() {
    let src = "fn main() {\n    fn inner() { }\n    struct S { a: int }\n    print(1)\n}";
    let (out, diags) = parse_dump(src);
    assert_eq!(diags.len(), 2, "{diags:#?}");
    assert_eq!(
        diags[0].message,
        "functions cannot be declared inside a block"
    );
    assert!(diags[0].hint.as_deref().unwrap().contains("closure"));
    assert_eq!(
        diags[1].message,
        "`struct` declarations are only allowed at module level"
    );
    assert_eq!(
        out,
        "(fn main (params) (block (error) (error) (tail (call print (int 1)))))"
    );
}

#[test]
fn assignment_targets_must_be_places() {
    let src = "foo() = 3";
    let d = one_error(src);
    assert_eq!(d.message, "cannot assign to this expression");
    assert_eq!(pointed_at(src, &d), "foo()");
}

#[test]
fn go_needs_a_call() {
    assert!(one_error("go foo").message.contains("function call"));
}

#[test]
fn stray_and_unclosed_braces() {
    assert_eq!(one_error("}").message, "unmatched `}`");
    let src = "fn g() {\n  print(1)\n";
    let d = one_error(src);
    assert_eq!(d.message, "unclosed `{`");
    assert_eq!(pointed_at(src, &d), "{");
}

#[test]
fn missing_separator_between_elements() {
    let d = one_error("match x { A => 1 B => 2 }");
    assert_eq!(
        d.message,
        "expected `,` or a new line between match arms, found identifier `B`"
    );
}

#[test]
fn field_declarations_and_literals_ask_for_a_colon() {
    let d = one_error("struct S { name str }");
    assert!(
        d.message
            .starts_with("expected `:` and a type after field `name`")
    );
    let d = one_error("let p = P { a = 1 }");
    assert_eq!(d.message, "expected `:` after field `a`, found `=`");
}

#[test]
fn reserved_words_cannot_be_names() {
    let d = one_error("fn match() {}");
    assert_eq!(d.message, "expected a function name, found keyword `match`");
    assert!(d.hint.as_deref().unwrap().contains("reserved word"));
}

#[test]
fn lexer_diagnostics_are_included() {
    let (_, diags) = parse_dump("let x = \"abc");
    assert!(
        diags.iter().any(|d| d.message.contains("unterminated")),
        "{diags:#?}"
    );
}

#[test]
fn three_independent_errors_give_three_diagnostics() {
    let src = "\
fn one() -> int {
    return 1 +* 2
}

fn two() {
    if x = 5 { }
    print(1)
}

struct Bad {
    name str
    fine: int
}

fn three() => 3
";
    let (out, diags) = parse_dump(src);
    assert_eq!(diags.len(), 3, "{diags:#?}");
    assert!(diags.iter().all(Diagnostic::is_error));
    let lines: Vec<usize> = diags
        .iter()
        .map(|d| src[..d.span.start as usize].matches('\n').count() + 1)
        .collect();
    assert_eq!(lines, [2, 6, 11]);
    // The parser keeps the valid parts: later statements, fields and items.
    assert!(out.contains("(call print (int 1))"), "{out}");
    assert!(out.contains("(fine : int)"), "{out}");
    assert!(out.ends_with("(fn three (params) (=> (int 3)))"), "{out}");
}

#[test]
fn recovery_inside_lists_loses_one_element() {
    let (out, diags) = parse_dump("enum E { A, 5, B }\nstruct S { a: int, b: , c: int }");
    assert_eq!(diags.len(), 2, "{diags:#?}");
    assert_eq!(out, "(enum E A B)\n(struct S (a : int) (c : int))");
}

#[test]
fn deep_nesting_is_an_error_not_a_stack_overflow() {
    for src in [
        "(".repeat(100_000),
        "-".repeat(100_000) + "x",
        "[".repeat(100_000),
        "{".repeat(100_000),
        "let x: ".to_string() + &"weak ".repeat(100_000),
        "if a { } ".to_string() + &"else if a { } ".repeat(10_000),
        "a".to_string() + &".b".repeat(100_000),
        "x = \"".to_string() + &"{\"".repeat(100_000),
    ] {
        let (_, diags) = parse_dump(&src);
        assert!(!diags.is_empty());
    }
}

// ---- node ids and spans ----

#[test]
fn node_ids_are_unique() {
    let src = "fn f(a: int = 1) -> List[int] {\n  let x = \"{a + 1}\"\n  match a { Some1(b) => b, _ => 0 }\n  for i in 0..3 { go g(i) }\n}\nstruct S { x: int = 2 }\nenum E { A(int) }";
    let (file, diags) = parse_file(F, src);
    assert!(diags.is_empty(), "{diags:#?}");
    let debug = format!("{file:?}");
    let mut ids: Vec<u32> = debug
        .split("NodeId(")
        .skip(1)
        .map(|rest| rest.split(')').next().unwrap().parse().unwrap())
        .collect();
    assert!(ids.len() > 30, "only {} ids", ids.len());
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), total, "node ids repeat");
}

#[test]
fn spans_cover_the_source_of_each_node() {
    let src = "let v = foo.bar(1, 2) + x";
    let (file, _) = parse_file(F, src);
    let TopLevel::Item(item) = &file.items[0] else {
        panic!("expected an item");
    };
    assert_eq!(&src[item.span.start as usize..item.span.end as usize], src);
    let ItemKind::Binding(b) = &item.kind else {
        panic!("expected a binding");
    };
    let text = |s: colibri_syntax::Span| &src[s.start as usize..s.end as usize];
    assert_eq!(text(b.init.span), "foo.bar(1, 2) + x");
    let ExprKind::Binary { lhs, rhs, .. } = &b.init.kind else {
        panic!("expected a binary expression");
    };
    assert_eq!(text(lhs.span), "foo.bar(1, 2)");
    assert_eq!(text(rhs.span), "x");
    assert_eq!(text(b.pat.span), "v");

    let src = "{ a() }";
    let (file, _) = parse_file(F, src);
    let TopLevel::Stmt(stmt) = &file.items[0] else {
        panic!("expected a statement");
    };
    assert_eq!(text_of(src, stmt.span), "{ a() }");
    assert!(matches!(stmt.kind, StmtKind::Expr(_)));
}

fn text_of(src: &str, s: colibri_syntax::Span) -> &str {
    &src[s.start as usize..s.end as usize]
}

// ---- robustness ----

#[test]
fn never_panics_on_truncated_or_scrambled_input() {
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for (_, block) in corpus() {
        let src = fill_placeholders(&block);
        // Every prefix, and every suffix.
        for end in 0..=src.len() {
            if src.is_char_boundary(end) {
                parse_file(F, &src[..end]);
                parse_file(F, &src[end..]);
            }
        }
        // Random deletions and swaps.
        let chars: Vec<char> = src.chars().collect();
        for _ in 0..30 {
            let mut copy = chars.clone();
            for _ in 0..4 {
                let i = (next() % copy.len() as u64) as usize;
                let j = (next() % copy.len() as u64) as usize;
                if next() % 2 == 0 {
                    copy.remove(i);
                    if copy.is_empty() {
                        break;
                    }
                } else {
                    copy.swap(i, j);
                }
            }
            parse_file(F, &copy.into_iter().collect::<String>());
        }
    }
}

#[test]
fn never_panics_on_token_soup() {
    const PIECES: &[&str] = &[
        "fn", "let", "var", "if", "else", "for", "in", "match", "{", "}", "(", ")", "[", "]", ",",
        ".", "..", "=>", "->", "=", "==", "<", "+", "?", "or", "as", "is", "x", "Foo", "1",
        "\"s{x}\"", "'a'", "'l", ":", ";", "\n", "|", "&&", "!", "weak", "self", "use", "struct",
        "enum", "impl", "import", "@", "extern", "test", "go", "defer", "return", "some", "none",
        "_", "_",
    ];
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..5000 {
        let n = 1 + next() % 40;
        let src: String = (0..n)
            .map(|_| PIECES[(next() % PIECES.len() as u64) as usize])
            .collect::<Vec<_>>()
            .join(" ");
        parse_file(F, &src);
    }
}

// ---- the examples corpus ----

/// Blocks of the corpus that are fragments rather than files: a substring of
/// the block and the reason. None are needed today.
const SKIP: &[(&str, &str)] = &[];

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

/// The text of a corpus block that is an interface file (`.cbi`): it starts with the
/// `@interface(..)` header (after the comment line `colibri-runtime` writes). The loader blanks
/// the header out before it parses the rest, and so does this.
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

/// Replaces the `...` placeholders of the doc with parseable text: a line of
/// just `...` is removed, `{ ... }` becomes `{}` and `(...)` becomes `()`.
fn fill_placeholders(src: &str) -> String {
    let kept: Vec<&str> = src.lines().filter(|l| l.trim() != "...").collect();
    kept.join("\n")
        .replace("{ ... }", "{}")
        .replace("(...)", "()")
}

#[test]
fn every_design_block_parses_without_diagnostics() {
    let blocks = corpus();
    assert!(blocks.len() >= 25, "found only {} blocks", blocks.len());
    for (_, reason) in SKIP {
        assert!(!reason.is_empty());
    }
    for (substring, _) in SKIP {
        assert!(
            blocks.iter().any(|(_, b)| b.contains(substring)),
            "stale skip entry {substring:?}"
        );
    }
    let mut failures = Vec::new();
    for (line, block) in &blocks {
        if SKIP.iter().any(|(s, _)| block.contains(s)) {
            continue;
        }
        let src = fill_placeholders(block);
        // The interface of a host (`.cbi`) is a block of the notes too: functions without
        // bodies are what an interface file is.
        let (_, diags) = match interface_text(&src) {
            Some(text) => parse_interface_file(F, &text),
            None => parse_file(F, &src),
        };
        if !diags.is_empty() {
            let msgs: Vec<String> = diags
                .iter()
                .map(|d| {
                    let line_in_block = src[..d.span.start as usize].matches('\n').count() + 1;
                    format!("  block line {line_in_block}: {}", d.message)
                })
                .collect();
            failures.push(format!(
                "design_examples.md line {line}:\n{}",
                msgs.join("\n")
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn design_apply_function_dump() {
    let src = "fn apply(p: Player, cmd: Command) -> Player {\n    var next = p\n    match cmd {\n        Move(dir) => next.pos += dir\n        Heal(n)   => next.hp = math.min(next.hp + n, 100)\n        Quit      => {}\n    }\n    return next\n}";
    assert_eq!(
        dump(src),
        "(fn apply (params (p : Player) (cmd : Command)) -> Player (block (var next p) \
         (match cmd (arm (variant Move dir) (assign += (field next pos) dir)) \
         (arm (variant Heal n) (assign = (field next hp) (call (field math min) (binary + (field next hp) n) (int 100)))) \
         (arm Quit (block))) (return next)))"
    );
}

#[test]
fn match_arms_with_leading_dot_variants() {
    let (_, diags) = colibri_syntax::parse_file(
        colibri_syntax::FileId(0),
        "fn f(s: Shape) -> float {\n    return match s {\n        .Circle(r) => r\n        .Rect(w, h) => w * h\n        .Point => 0\n    }\n}\n",
    );
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn when_refuses_a_trailing_comma() {
    let d = one_error("@when(debug,)\nfn f() {}\n");
    assert_eq!(
        d.message,
        "`@when` takes one condition, without a trailing comma"
    );
    assert_eq!(pointed_at("@when(debug,)\nfn f() {}\n", &d), ",");
    // Other attributes are not affected.
    assert!(parse_dump("@deprecated(\"x\",)\nfn f() {}\n").1.is_empty());
}
