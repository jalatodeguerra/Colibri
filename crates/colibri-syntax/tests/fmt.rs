//! Tests of `colibri fmt`: one test per formatting rule, comment preservation, and the corpus
//! (every golden file and every example of the corpus) for idempotence and AST equality.

use std::path::{Path, PathBuf};

use colibri_syntax::ast_dump::dump_file;
use colibri_syntax::{FileId, format_source, parse_file};

/// Formats `src`, panicking with the diagnostics when it is refused.
fn fmt(src: &str) -> String {
    match format_source("test.colibri", src) {
        Ok(out) => out,
        Err(diags) => panic!("refused {src:?}: {diags:#?}"),
    }
}

/// Asserts `src` formats to `expected`, and that `expected` is a fixed point.
fn check(src: &str, expected: &str) {
    assert_eq!(fmt(src), expected, "input: {src:?}");
    assert_eq!(fmt(expected), expected, "not idempotent: {expected:?}");
}

fn dump(src: &str) -> String {
    let (file, diags) = parse_file(FileId(0), src);
    assert!(diags.is_empty(), "{diags:#?}");
    dump_file(&file)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "colibri") {
            out.push(path);
        }
    }
}

#[test]
fn golden_files_are_stable() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();
    assert!(files.len() > 500, "found {} golden files", files.len());
    let mut failures = Vec::new();
    let mut formatted = 0;
    for path in &files {
        let src = std::fs::read_to_string(path).unwrap();
        let out = match format_source("golden", &src) {
            Ok(out) => out,
            Err(diags) => {
                // Files with intentional syntax errors are refused; anything else is a bug.
                if !diags
                    .iter()
                    .any(|d| d.message.starts_with("internal formatter error"))
                {
                    continue;
                }
                failures.push(format!("{}: {}", path.display(), diags[0].message));
                continue;
            }
        };
        formatted += 1;
        if dump(&out) != dump(&src) {
            failures.push(format!("{}: AST changed", path.display()));
        }
        match format_source("golden", &out) {
            Ok(twice) if twice == out => {}
            _ => failures.push(format!("{}: not idempotent", path.display())),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(formatted > 500, "only {formatted} files formatted");
}

/// The golden files that parse without errors.
fn parseable_golden() -> Vec<(PathBuf, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();
    files
        .into_iter()
        .filter_map(|path| {
            let src = std::fs::read_to_string(&path).unwrap();
            let (_, diags) = parse_file(FileId(0), &src);
            diags.is_empty().then_some((path, src))
        })
        .collect()
}

/// How a stress test adds a comment to a line: (line number, line) to the new text.
type Style = fn(usize, &str) -> String;

/// Puts a comment on every line of every golden file, in three styles, and formats the
/// result. `format_source` itself checks that the program and every comment survive.
#[test]
fn hashed_raw_strings_round_trip() {
    let src = "let j = r#\"[{\"a\": 1}]\"#\nlet k = r##\"x \"# y\"##\n";
    check(src, src);
}

#[test]
fn comments_everywhere_survive() {
    let styles: [(&str, Style); 3] = [
        ("trailing", |n, line| format!("{line} // t{n}")),
        ("own line", |n, line| format!("// o{n}\n{line}")),
        ("inline block", |n, line| format!("/* b{n} */ {line}")),
    ];
    let mut failures = Vec::new();
    for (path, src) in parseable_golden() {
        for (style, add) in &styles {
            let text: String = src
                .lines()
                .enumerate()
                .map(|(n, line)| add(n, line) + "\n")
                .collect();
            if parse_file(FileId(0), &text).1.iter().any(|d| d.is_error()) {
                continue; // a comment landed where the lexer cannot take it
            }
            match format_source("stress", &text) {
                Ok(out) => match format_source("stress", &out) {
                    Ok(again) if again == out => {}
                    _ => failures.push(format!("{} ({style}): not idempotent", path.display())),
                },
                Err(diags) => failures.push(format!(
                    "{} ({style}): {}",
                    path.display(),
                    diags[0].message
                )),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{}",
        failures[..failures.len().min(30)].join("\n")
    );
}

/// The output does not depend on how the input was indented or spaced.
#[test]
fn indentation_of_the_input_does_not_matter() {
    let mut failures = Vec::new();
    for (path, src) in parseable_golden() {
        if src.contains("\"\"\"") || src.contains("/*") || src.contains("r#\"") {
            continue; // the text of these depends on the layout
        }
        let flat: String = src
            .lines()
            .map(|l| l.trim_start().to_string() + "\n")
            .collect();
        if parse_file(FileId(0), &flat).1.iter().any(|d| d.is_error()) {
            continue;
        }
        let (a, b) = (format_source("a", &src), format_source("b", &flat));
        if a != b {
            failures.push(path.display().to_string());
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---- the examples corpus ----

/// The text of every ```colibri block in tests/corpus/design_examples.md.
fn design_blocks() -> Vec<String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/corpus/design_examples.md"
    );
    let text = std::fs::read_to_string(path).expect("the examples corpus is readable");
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        match (&mut current, line.trim_start()) {
            (None, l) if l.starts_with("```colibri") => current = Some(String::new()),
            (Some(_), l) if l.starts_with("```") => blocks.extend(current.take()),
            (Some(body), _) => {
                body.push_str(line);
                body.push('\n');
            }
            _ => {}
        }
    }
    blocks
}

/// Makes the `...` placeholders of the corpus parseable (as the parser tests do).
fn fill_placeholders(src: &str) -> String {
    let kept: Vec<&str> = src.lines().filter(|l| l.trim() != "...").collect();
    kept.join("\n")
        .replace("{ ... }", "{}")
        .replace("(...)", "()")
}

#[test]
fn design_examples_format_stably() {
    let mut checked = 0;
    for block in design_blocks() {
        let src = fill_placeholders(&block);
        if parse_file(FileId(0), &src).1.iter().any(|d| d.is_error()) {
            continue;
        }
        let out = fmt(&src);
        assert_eq!(dump(&out), dump(&src), "AST changed:\n{src}\n---\n{out}");
        assert_eq!(fmt(&out), out, "not idempotent:\n{out}");
        checked += 1;
    }
    assert!(checked >= 25, "only {checked} blocks checked");
}

// ---- layout rules ----

#[test]
fn statements_one_per_line_without_semicolons() {
    check(
        "fn f() { a(); b(); c() }",
        "fn f() {\n    a()\n    b()\n    c()\n}\n",
    );
    check("let x = 1;\nlet y = 2;\n", "let x = 1\nlet y = 2\n");
}

#[test]
fn a_semicolon_after_the_last_expression_statement_is_kept() {
    // `{ f(); }` has no value; `{ f() }` yields `f()`: the `;` is not noise.
    check("fn f() { g(); }", "fn f() { g(); }\n");
    check("fn f() { g() }", "fn f() { g() }\n");
    check(
        "fn f() {\n    a()\n    g();\n}",
        "fn f() {\n    a()\n    g();\n}\n",
    );
    assert_ne!(dump("fn f() { g(); }"), dump("fn f() { g() }"));
}

#[test]
fn spacing_around_operators_commas_and_colons() {
    check(
        "fn f(a:int,b :int=3)->int{return a+b*2-(a)}",
        "fn f(a: int, b: int = 3) -> int { return a + b * 2 - (a) }\n",
    );
    check(
        "let x=[1,2 ,3]\nlet m={\"a\":1}",
        "let x = [1, 2, 3]\nlet m = {\"a\": 1}\n",
    );
    check(
        "let r = 0 .. 10\nlet s = 1 ..= 2",
        "let r = 0..10\nlet s = 1..=2\n",
    );
    check("x+=1\ny<<=2\nz = -a", "x += 1\ny <<= 2\nz = -a\n");
    check("let t = a.b . c ( 1 )", "let t = a.b.c(1)\n");
}

#[test]
fn indentation_is_four_spaces() {
    check(
        "fn f() {\n\tif a {\n  b()\n\t}\n}",
        "fn f() {\n    if a {\n        b()\n    }\n}\n",
    );
}

#[test]
fn blank_lines_collapse_to_one_and_none_at_the_edges() {
    check(
        "\n\n\nlet a = 1\n\n\n\nlet b = 2\nlet c = 3\n\n\n",
        "let a = 1\n\nlet b = 2\nlet c = 3\n",
    );
    check(
        "fn f() {\n\n    a()\n\n\n    b()\n\n}",
        "fn f() {\n    a()\n\n    b()\n}\n",
    );
    check("", "");
    check("\n\n", "");
}

#[test]
fn no_trailing_whitespace_and_one_final_newline() {
    check("let a = 1   \nlet b = 2  \t\n", "let a = 1\nlet b = 2\n");
    check("let a = 1", "let a = 1\n");
    check("let a = 1\r\nlet b = 2\r\n", "let a = 1\nlet b = 2\n");
}

#[test]
fn short_blocks_stay_on_one_line_only_if_written_so() {
    check("fn f() { return 1 }", "fn f() { return 1 }\n");
    check("fn f() {\n    return 1\n}", "fn f() {\n    return 1\n}\n");
    check("fn f() {\n}", "fn f() {}\n");
    check("fn f() { }", "fn f() {}\n");
    check(
        "let s = if x > 0 { \"pos\" } else { log(\"neg\"); \"neg\" }",
        "let s = if x > 0 { \"pos\" } else {\n    log(\"neg\")\n    \"neg\"\n}\n",
    );
}

#[test]
fn a_one_line_block_holds_no_nested_block_statement() {
    check(
        "for x in xs { if x > 1 { print(\"a\") } else { print(\"b\") } }",
        "for x in xs {\n    if x > 1 { print(\"a\") } else { print(\"b\") }\n}\n",
    );
    check(
        "fn f() { match x { A => 1 } }",
        "fn f() {\n    match x {\n        A => 1\n    }\n}\n",
    );
    // Expression-position blocks and plain statements may stay.
    check(
        "let a = if c { 1 } else { 2 }",
        "let a = if c { 1 } else { 2 }\n",
    );
    check("let f = fn() { g() }", "let f = fn() { g() }\n");
    check("for x in xs { g(x) }", "for x in xs { g(x) }\n");
}

#[test]
fn long_lists_go_one_per_line_with_a_trailing_comma() {
    let items: Vec<String> = (0..12).map(|i| format!("item_number_{i}")).collect();
    // Calls are not simple items: one per line.
    let calls: Vec<String> = items.iter().map(|i| format!("{i}()")).collect();
    let src = format!("let xs = [{}]\n", calls.join(", "));
    let mut expected = String::from("let xs = [\n");
    for call in &calls {
        expected.push_str(&format!("    {call},\n"));
    }
    expected.push_str("]\n");
    check(&src, &expected);
    check(
        &format!("fn f({}) {{}}", items.join(": int, ") + ": int"),
        &format!(
            "fn f(\n{}) {{}}\n",
            items
                .iter()
                .map(|i| format!("    {i}: int,\n"))
                .collect::<String>()
        ),
    );
}

#[test]
fn a_line_of_exactly_100_columns_fits_and_101_does_not() {
    let name = "x".repeat(100 - "let a = f(y)".len() + 1);
    let fits = format!("let a = f({name})\n");
    assert_eq!(fits.trim_end().len(), 100);
    check(&fits, &fits);
    let long = format!("let a = f({name}z)\n");
    assert_eq!(fmt(&long), format!("let a = f(\n    {name}z,\n)\n"));
}

#[test]
fn magic_trailing_comma_keeps_a_list_expanded() {
    check(
        "let xs = [\n    1,\n    2,\n]",
        "let xs = [\n    1,\n    2,\n]\n",
    );
    check("f(\n    a,\n    b,\n)", "f(\n    a,\n    b,\n)\n");
    // Without the comma, or with it on one line, the list collapses.
    check("let xs = [\n    1,\n    2\n]", "let xs = [1, 2]\n");
    check("let xs = [1, 2,]", "let xs = [1, 2]\n");
    check(
        "fn f(\n    a: int,\n    b: int,\n) {}",
        "fn f(\n    a: int,\n    b: int,\n) {}\n",
    );
}

#[test]
fn literals_and_struct_literals() {
    check("let p = Point{x:1,y:2}", "let p = Point { x: 1, y: 2 }\n");
    check("let p = Point{x,y}", "let p = Point { x, y }\n");
    check("let p = Point{ }", "let p = Point {}\n");
    check(
        "let m = {:}\nlet n = {\"a\":1,\"b\":2}",
        "let m = {:}\nlet n = {\"a\": 1, \"b\": 2}\n",
    );
    check(
        "let t = (1,)\nlet u = ()\nlet v = (1,2)",
        "let t = (1,)\nlet u = ()\nlet v = (1, 2)\n",
    );
    check(
        "let p = Point {\n    x: 1,\n    y: 2,\n}",
        "let p = Point {\n    x: 1,\n    y: 2,\n}\n",
    );
}

#[test]
fn method_chains_break_before_the_dot() {
    let src = "let names = players.filter(fn(p) => p.is_alive_and_well_in_the_current_level()).map(fn(p) => p.display_name_for_the_score_board()).collect()";
    check(
        src,
        "let names = players\n    .filter(fn(p) => p.is_alive_and_well_in_the_current_level())\n    .map(fn(p) => p.display_name_for_the_score_board())\n    .collect()\n",
    );
    // A chain that fits stays on one line.
    check(
        "let a = xs\n    .filter(f)\n    .map(g)",
        "let a = xs.filter(f).map(g)\n",
    );
    // A capitalised name after the dot never starts a line: it would read as a match arm.
    let enum_chain = format!(
        "let a = Shape.Circle({})",
        "radius_with_a_long_name, ".repeat(5)
    );
    assert!(!fmt(&enum_chain).contains("\n    .Circle"));
}

#[test]
fn binary_chains_break_so_that_they_still_parse() {
    let cond = "first_condition_is_true && second_condition_is_also_true && third_condition_is_quite_long && fourth";
    check(
        &format!("let good = {cond}"),
        "let good = first_condition_is_true\n    && second_condition_is_also_true\n    && third_condition_is_quite_long\n    && fourth\n",
    );
    // Operators of different precedence nest: the inner chain stays together when it fits.
    check(
        "let good = first_condition_is_true && second_condition_is_also_true && third_condition_is_quite_long || fallback",
        "let good = first_condition_is_true && second_condition_is_also_true && third_condition_is_quite_long\n    || fallback\n",
    );
    // Operators other than `&&`, `||` and `or` stay at the end of the line: a line that
    // starts with `+` would be a new statement.
    let sum = "price_of_the_first_item * quantity_of_the_first_item + price_of_the_second_item * quantity_of_the_second + shipping_cost";
    check(
        &format!("let total = {sum}"),
        "let total = price_of_the_first_item * quantity_of_the_first_item +\n    price_of_the_second_item * quantity_of_the_second +\n    shipping_cost\n",
    );
    check(
        "let v = lookup_the_table(key) or default_value_for_this_key_in_the_table or another_fallback_value_here",
        "let v = lookup_the_table(key)\n    or default_value_for_this_key_in_the_table\n    or another_fallback_value_here\n",
    );
}

#[test]
fn closures_as_the_last_argument_stay_on_the_call_line() {
    check(
        "button.on_click(fn() {\n    clicks += 1\n})",
        "button.on_click(fn() {\n    clicks += 1\n})\n",
    );
    check(
        "print(xs.any(fn(x) {\n    return x == 2\n}))",
        "print(xs.any(fn(x) {\n    return x == 2\n}))\n",
    );
    check(
        "items.for_each(fn(item) { process(item); log(item) })",
        "items.for_each(fn(item) {\n    process(item)\n    log(item)\n})\n",
    );
}

#[test]
fn match_arms_are_aligned_in_runs() {
    check(
        "match s {\n    Circle(r) => 1\n    Rect(w, h)  => 2\n    Point => 0\n}",
        "match s {\n    Circle(r)  => 1\n    Rect(w, h) => 2\n    Point      => 0\n}\n",
    );
    // A blank line or a multi-line arm ends the run.
    check(
        "match s {\n    A => 1\n\n    LongName => 2\n    B => {\n        f()\n        g()\n    }\n    C => 3\n}",
        "match s {\n    A => 1\n\n    LongName => 2\n    B => {\n        f()\n        g()\n    }\n    C => 3\n}\n",
    );
    check(
        "match s { A => 1, B => 2 }",
        "match s {\n    A => 1\n    B => 2\n}\n",
    );
}

#[test]
fn if_else_chains() {
    check(
        "if a {\n    x()\n}\nelse if b {\n    y()\n}\nelse {\n    z()\n}",
        "if a {\n    x()\n} else if b {\n    y()\n} else {\n    z()\n}\n",
    );
    check(
        "if let some(x) = y { f(x) }",
        "if let some(x) = y { f(x) }\n",
    );
}

#[test]
fn declarations() {
    check(
        "pub struct P[T: Hash+Eq]{pub x:T\ny:int=3}",
        "pub struct P[T: Hash + Eq] {\n    pub x: T\n    y: int = 3\n}\n",
    );
    check("enum Dir { North, East }", "enum Dir { North, East }\n");
    check(
        "enum Shape {\n    Circle(radius: float)\n    Rect(float,float)\n    Point\n}",
        "enum Shape {\n    Circle(radius: float)\n    Rect(float, float)\n    Point\n}\n",
    );
    check("struct E { }", "struct E {}\n");
    check(
        "impl[T] Show for List[T]{fn show(self)->str=>\"x\"\nfn m(var self){}}",
        "impl[T] Show for List[T] {\n    fn show(self) -> str => \"x\"\n    fn m(var self) {}\n}\n",
    );
    check(
        "interface Iter[T]{fn next(var self)->T?}",
        "interface Iter[T] {\n    fn next(var self) -> T?\n}\n",
    );
    check("type  Cb = fn(int)->int", "type Cb = fn(int) -> int\n");
    check(
        "import a.b as c\nimport rapier.{World,Body as B}",
        "import a.b as c\nimport rapier.{World, Body as B}\n",
    );
    check(
        "extern \"C\" { fn f(a:i32)->i32 }",
        "extern \"C\" {\n    fn f(a: i32) -> i32\n}\n",
    );
    check(
        "test \"adds\" { assert(1+1==2) }",
        "test \"adds\" { assert(1 + 1 == 2) }\n",
    );
    check("const MAX:int=100", "const MAX: int = 100\n");
}

#[test]
fn attributes_go_on_their_own_lines() {
    check("@inline fn f() {}", "@inline\nfn f() {}\n");
    check(
        "@when(os == \"windows\") @link(\"user32\") extern \"C\" { }",
        "@when(os == \"windows\")\n@link(\"user32\")\nextern \"C\" {}\n",
    );
    check(
        "@when(!debug&&(os==\"linux\"||fast)) fn f(){}",
        "@when(!debug && (os == \"linux\" || fast))\nfn f() {}\n",
    );
    check(
        "@repr(C)\npub struct S { a: i32 }",
        "@repr(C)\npub struct S { a: i32 }\n",
    );
}

#[test]
fn field_attributes_stay_on_the_field_line() {
    check(
        "struct S { @rename(\"x\")  a:int, @skip b:int=1, @skip @rename(\"c\") pub c:int=2 }",
        "struct S { @rename(\"x\") a: int, @skip b: int = 1, @skip @rename(\"c\") pub c: int = 2 }\n",
    );
    // Attributes on their own line are joined to the field.
    check(
        "struct S {\n    @rename(\"x\")\n    a: int\n    /// doc\n    @skip\n    b: int = 1\n}",
        "struct S {\n    @rename(\"x\") a: int\n    /// doc\n    @skip b: int = 1\n}\n",
    );
}

#[test]
fn comments_around_field_attributes_stay_where_they_were_written() {
    // Inside the attribute's parentheses, and between the attribute and the field.
    for src in [
        "struct S {\n    @rename(\"y\" /* c */) c: int\n}\n",
        "struct S {\n    @rename(/* c */ \"y\") c: int\n}\n",
        "struct S {\n    @rename(\"y\") /* c */ c: int\n}\n",
        "struct S {\n    /* c */ @rename(\"y\") c: int\n}\n",
        // A comment that ends the attribute's line keeps attribute and field on two lines.
        "struct S {\n    @rename(\"y\") // why\n    c: int\n}\n",
        "struct S {\n    @skip // a\n    @rename(\"y\") // b\n    c: int = 1\n}\n",
        "struct S {\n    // lead\n    @rename(\"y\")\n    // between\n    c: int\n}\n",
        "struct S {\n    @rename(\"y\") /* c */\n    c: int\n}\n",
        "struct S {\n    @rename(\"y\")\n    /* c */\n    pub c: int\n}\n",
    ] {
        check(src, src);
    }
    // Still joined to the field when there is no comment, and the comments are not lost.
    check(
        "struct S {\n    @rename(\"y\")\n    c: int // after\n}",
        "struct S {\n    @rename(\"y\") c: int // after\n}\n",
    );
}

#[test]
fn types_keep_their_meaning() {
    check(
        "let a: (fn(int) -> int)? = none",
        "let a: (fn(int) -> int)? = none\n",
    );
    check(
        "let a: fn(int) -> int? = none",
        "let a: fn(int) -> int? = none\n",
    );
    check(
        "let a: (weak Node)? = none\nlet b: weak Node? = none",
        "let a: (weak Node)? = none\nlet b: weak Node? = none\n",
    );
    check(
        "let a: [u8;4] = [0,0,0,0]\nlet b: (int,) = (1,)",
        "let a: [u8; 4] = [0, 0, 0, 0]\nlet b: (int,) = (1,)\n",
    );
    check(
        "let a: Map[str,List[int]] = x",
        "let a: Map[str, List[int]] = x\n",
    );
}

#[test]
fn patterns() {
    check(
        "match v { (a,b) => 1, Vec2i{x,y:0,..} => 2, some(1)|some(2) as n => 3, -5..=-1 => 4, .Circle(r) => 5 }",
        "match v {\n    (a, b)                 => 1\n    Vec2i { x, y: 0, .. }  => 2\n    some(1) | some(2) as n => 3\n    -5..=-1                => 4\n    .Circle(r)             => 5\n}\n",
    );
    // A binding inside an or-pattern needs its parentheses.
    check(
        "match v { (a as n) | b => 1 }",
        "match v {\n    (a as n) | b => 1\n}\n",
    );
}

#[test]
fn strings_are_left_alone() {
    check(
        "let s = \"hello {name} {x+1:.2}\"\nlet r = r\"raw {x}\"",
        "let s = \"hello {name} {x+1:.2}\"\nlet r = r\"raw {x}\"\n",
    );
    check(
        "let s = \"\"\"\n    a\n      b\n    \"\"\"",
        "let s = \"\"\"\n    a\n      b\n    \"\"\"\n",
    );
}

#[test]
fn an_open_range_at_the_end_of_a_statement_keeps_a_semicolon() {
    // A newline after `..` does not end the statement.
    check("let a = 0..;\nlet b = 1", "let a = 0..;\nlet b = 1\n");
    check("let a = xs[1..]\nlet b = 1", "let a = xs[1..]\nlet b = 1\n");
    check(
        "match x { 1 => 0..,\n _ => 1 }",
        "match x {\n    1 => 0..,\n    _ => 1\n}\n",
    );
}

#[test]
fn files_with_syntax_errors_are_refused() {
    let err = format_source("bad.colibri", "let x = (1 2)\n").unwrap_err();
    assert!(err[0].message.contains("expected `,` or `)`"), "{err:?}");
    assert!(format_source("bad.colibri", "fn f( {").is_err());
}

#[test]
fn unusual_but_valid_spellings_survive() {
    check(
        "let x = 0xFF + 1_000 + 2.50\nlet c = '\\n'",
        "let x = 0xFF + 1_000 + 2.50\nlet c = '\\n'\n",
    );
    check("let a = t.0.1", "let a = t.0.1\n");
    check("let a = - -x\nlet b = !!y", "let a = --x\nlet b = !!y\n");
    check("let f = fn() => fn() => 1", "let f = fn() => fn() => 1\n");
    check("let a = max[f32](1, 2)", "let a = max[f32](1, 2)\n");
    check(
        "for var e in es { e.hp -= 1 }\n'o: while true { break 'o }",
        "for var e in es { e.hp -= 1 }\n'o: while true { break 'o }\n",
    );
    check(
        "let a = x or { return }\nlet b = y or |e| { f(e) }",
        "let a = x or { return }\nlet b = y or |e| { f(e) }\n",
    );
    check(
        "defer f.close()\ndefer { g() }\ngo work(1)",
        "defer f.close()\ndefer { g() }\ngo work(1)\n",
    );
}

// ---- comments ----

#[test]
fn comments_keep_their_lines() {
    let src = "// header\n\n/* block\n   comment */\n/// doc\nfn f() {\n    // inside\n    a() // trailing\n    /* inline */ b()\n    // last\n}\n\n// footer\n";
    check(src, src);
}

#[test]
fn trailing_comments_stay_on_their_line_with_one_space() {
    check(
        "let a = 1      // one\nlet b = 2 //two",
        "let a = 1 // one\nlet b = 2 //two\n",
    );
    check(
        "fn f() { // open\n    a()\n} // close",
        "fn f() { // open\n    a()\n} // close\n",
    );
    check("fn f() { // only\n}", "fn f() { // only\n}\n");
}

#[test]
fn comments_in_lists_and_calls() {
    check(
        "let xs = [\n    1, // one\n    2,\n    // three\n    3,\n    // end\n]",
        "let xs = [\n    1, // one\n    2,\n    // three\n    3,\n    // end\n]\n",
    );
    check(
        "f(a, // about a\n  b)",
        "f(\n    a, // about a\n    b,\n)\n",
    );
    check("f(\n    // nothing\n)", "f(\n    // nothing\n)\n");
    check("let x = f(/* c */ a)", "let x = f(/* c */ a)\n");
}

#[test]
fn comments_in_chains_operands_and_else() {
    check(
        "let z = foo(a)\n    // about map\n    .map(f)\n    .filter(g) // about filter",
        "let z = foo(a)\n    // about map\n    .map(f)\n    .filter(g) // about filter\n",
    );
    check("let x = a + // lhs\n    b", "let x = a + // lhs\n    b\n");
    check(
        "if a {\n    1\n}\n// before else\nelse {\n    2\n}",
        "if a {\n    1\n}\n// before else\nelse {\n    2\n}\n",
    );
}

#[test]
fn comments_in_match_and_declarations() {
    check(
        "match v {\n    // first\n    A => 1 // a\n\n    // second\n    B => 2\n    // end\n}",
        "match v {\n    // first\n    A => 1 // a\n\n    // second\n    B => 2\n    // end\n}\n",
    );
    check(
        "enum E {\n    /// the a\n    A // trailing\n    B\n}\n@inline // attr\nfn f() {}",
        "enum E {\n    /// the a\n    A // trailing\n    B\n}\n@inline // attr\nfn f() {}\n",
    );
}

#[test]
fn two_line_comments_never_share_a_line() {
    // Both would end up after `.filter(..)`; the second must not become part of the first.
    let out = fmt("let r = xs // a\n    .filter(f) // b\n    .map(g)");
    assert_eq!(out.matches("//").count(), 2, "{out}");
    for line in out.lines() {
        assert!(line.matches("//").count() <= 1, "{out}");
    }
}

#[test]
fn comment_only_files() {
    check("// just a comment", "// just a comment\n");
    check("\n\n/* a */  // b\n\n\n// c\n", "/* a */ // b\n\n// c\n");
}

#[test]
fn doc_comments_survive_with_the_ast() {
    let src = "/// Adds.\n/// More.\npub fn add(a: int) -> int => a\n";
    assert_eq!(fmt(src), src);
    assert!(dump(src).contains("(doc \"Adds.\\nMore.\")"));
}

#[test]
fn test_items_are_formatted_like_functions() {
    check(
        "test   \"adds up\"   {\nlet x   =  1+1\n   assert_eq( x,2 )\n}\n",
        "test \"adds up\" {\n    let x = 1 + 1\n    assert_eq(x, 2)\n}\n",
    );
    // Comments stay, blank lines between items are kept at most one, the name is left alone.
    check(
        "// the first\ntest \"a  b\" { // trailing\n    assert(true) // inside\n}\n\n\n\ntest \"empty\" {}\n",
        "// the first\ntest \"a  b\" { // trailing\n    assert(true) // inside\n}\n\ntest \"empty\" {}\n",
    );
    // A test between functions, with `?` in the body; no blank lines are invented.
    check(
        "fn f() -> int => 1\ntest \"q\" {\n    let n = g()?\n    assert_eq(n,1)\n}\nfn g() -> Result[int] => ok(1)\n",
        "fn f() -> int => 1\ntest \"q\" {\n    let n = g()?\n    assert_eq(n, 1)\n}\nfn g() -> Result[int] => ok(1)\n",
    );
}

/// Formats `src`, checks the output is `expected` and that formatting it again changes nothing.
fn check_stable(src: &str, expected: &str) {
    check(src, expected);
    check(expected, expected);
}

#[test]
fn long_signatures_break_the_parameters_before_the_return_type() {
    check_stable(
        "fn plan(files: List[Entry], root: str, another_parameter_name: int, yet_another: str) -> Result[List[Move]] { g()\nh() }",
        "fn plan(\n    files: List[Entry],\n    root: str,\n    another_parameter_name: int,\n    yet_another: str,\n) -> Result[List[Move]] {\n    g()\n    h()\n}\n",
    );
    // The return type is broken only when it still does not fit after the parameters.
    let long_ret = format!("Result[List[Move], {}]", "E".repeat(95));
    let out = fmt(&format!("fn plan(files: List[Entry]) -> {long_ret} {{}}"));
    assert!(
        out.starts_with("fn plan(\n    files: List[Entry],\n) -> Result[\n"),
        "{out}"
    );
    check(&out, &out);
    // No parameters: nothing to break first.
    let out = fmt(&format!("fn plan() -> {long_ret} {{}}"));
    assert!(out.starts_with("fn plan() -> Result[\n"), "{out}");
    // A signature that fits is untouched.
    check_stable(
        "fn f(a: int) -> Result[int] {}",
        "fn f(a: int) -> Result[int] {}\n",
    );
}

#[test]
fn long_lists_of_simple_items_are_filled() {
    let nums: Vec<String> = (1..=31).map(|i| i.to_string()).collect();
    let src = format!("let xs = [{}]\n", nums.join(", "));
    check_stable(
        &src,
        "let xs = [\n    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,\n    27, 28, 29, 30, 31,\n]\n",
    );
    check_stable(
        r#"let n = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa", "lambda", "mu"]"#,
        "let n = [\n    \"alpha\", \"beta\", \"gamma\", \"delta\", \"epsilon\", \"zeta\", \"eta\", \"theta\", \"iota\", \"kappa\", \"lambda\",\n    \"mu\",\n]\n",
    );
    // Negative numbers, names, chars, bools and none are simple too.
    let mixed = "let m = [-1, -2.5, a_name, 'c', true, none, -300000, -400000, -500000, -600000, -700000, -800000, -900000, 1000000]";
    let out = fmt(mixed);
    assert_eq!(out.lines().count(), 4, "{out}");
    check(&out, &out);
    // One more complex item and the list goes one per line.
    let complex = format!("let xs = [{}, f(x)]\n", nums.join(", "));
    let out = fmt(&complex);
    assert!(out.contains("    1,\n    2,\n"), "{out}");
    check(&out, &out);
    for src in [
        "let xs = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, [1]]",
        "let xs = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, a.b, 24, 25, 26, 27, 28, 29, 30]",
        "let xs = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 1 + 2, 24, 25, 26, 27, 28, 29, 30]",
    ] {
        let out = fmt(src);
        assert!(out.contains("    1,\n    2,\n"), "{out}");
        check(&out, &out);
    }
}

#[test]
fn short_simple_lists_keep_the_existing_rules() {
    check_stable("let xs = [1, 2, 3]", "let xs = [1, 2, 3]\n");
    // Magic trailing comma with one item per line stays that way.
    check_stable(
        "let xs = [\n    1,\n    2,\n]",
        "let xs = [\n    1,\n    2,\n]\n",
    );
    // No trailing comma: collapsed.
    check_stable("let xs = [\n    1,\n    2\n]", "let xs = [1, 2]\n");
}

#[test]
fn comments_in_simple_lists_are_kept() {
    let src = "let xs = [\n    1, // one\n    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,\n    // last\n]\n";
    let out = fmt(src);
    assert!(out.contains("// one") && out.contains("// last"), "{out}");
    check(&out, &out);
}

#[test]
fn if_else_blocks_break_together() {
    check_stable(
        "if let Some(value) = lookup_something_long(table, key_with_long_name) { print(\"found a value {value} and more text here\") } else { print(\"nothing\") }",
        "if let Some(value) = lookup_something_long(table, key_with_long_name) {\n    print(\"found a value {value} and more text here\")\n} else {\n    print(\"nothing\")\n}\n",
    );
    check_stable(
        "if cond_with_a_long_name_xxxxxxxxxxxxxxxxxxxxxxxxx { short() } else { a_second_branch_with_a_long_name_xxxxxxxxxxxxxxxxxxxxxx() }",
        "if cond_with_a_long_name_xxxxxxxxxxxxxxxxxxxxxxxxx {\n    short()\n} else {\n    a_second_branch_with_a_long_name_xxxxxxxxxxxxxxxxxxxxxx()\n}\n",
    );
    // An else-if chain breaks as a whole.
    check_stable(
        "let r = if aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa { 1 } else if bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb { 2 } else { some_long_value_name_xxxxxxx }",
        "let r = if aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa {\n    1\n} else if bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb {\n    2\n} else {\n    some_long_value_name_xxxxxxx\n}\n",
    );
    // That fits stays on one line.
    check_stable(
        "let a = if c { 1 } else { 2 }",
        "let a = if c { 1 } else { 2 }\n",
    );
    // A comment before `else` keeps its own line.
    let out = fmt("if c { 1 }\n// why\nelse { 2 }");
    assert!(out.contains("// why"), "{out}");
    check(&out, &out);
}

#[test]
fn if_else_inside_an_expression_does_not_force_the_enclosing_group_to_break() {
    for src in [
        "print(if x > 0 { 1 } else { -1 })",
        "print(get(false) or if n > 3 { 100 } else { 200 })",
        "print(a.and_then(fn(x) => if x > 2 { some(\"big\") } else { none }))",
        "print(f(if c { 1 } else { 2 }, 3))",
        "let v = [if c { 1 } else { 2 }, 3]",
        "let t = (if r > 0 { 1 } else { 2 }) * 3",
        "let w = if a { 1 } else if b { 2 } else { 3 }",
    ] {
        check_stable(src, &format!("{src}\n"));
    }
}

#[test]
fn match_arms_with_an_if_body_keep_their_alignment() {
    check_stable(
        "match x {\n    A => 1\n    LongName => if c { 1 } else { 2 }\n    B => 3\n}",
        "match x {\n    A        => 1\n    LongName => if c { 1 } else { 2 }\n    B        => 3\n}\n",
    );
}

#[test]
fn only_short_simple_items_are_filled() {
    // Items wider than 12 columns: one per line, as for any list.
    let long = [
        "\"2026-11-19 10:00\"",
        "\"2026-11-20 11:00\"",
        "\"2026-11-21 12:00\"",
        "\"2026-11-22 13:00\"",
        "\"2026-11-23 14:00\"",
        "\"2026-11-24 15:00\"",
    ];
    let out = fmt(&format!("let xs = [{}]", long.join(", ")));
    assert_eq!(out.lines().count(), 8, "{out}");
    check(&out, &out);
    // Exactly 12 columns still fills.
    let twelve = format!("\"{}\"", "a".repeat(10));
    let items = vec![twelve; 10];
    let out = fmt(&format!("let xs = [{}]", items.join(", ")));
    assert_eq!(out.lines().count(), 4, "{out}");
}

#[test]
fn a_block_holding_only_a_block_comment_is_stable() {
    check_stable(
        "fn f() {\n    if c { print(\"one\") } else { /* nothing */ }\n}",
        "fn f() {\n    if c { print(\"one\") } else {\n        /* nothing */\n    }\n}\n",
    );
    let out = fmt("fn f() { /* x */ }");
    check(&out, &out);
}

#[test]
fn a_when_condition_keeps_its_comments_and_gets_no_trailing_comma() {
    check(
        "@when(\n    debug // why\n    && os == \"linux\" // second\n)\nfn b() {}\n",
        "@when(\n    debug // why\n        && os == \"linux\" // second\n)\nfn b() {}\n",
    );
    check(
        "@when(debug && /* c */ os == \"linux\")\nfn c() {}\n",
        "@when(debug && /* c */ os == \"linux\")\nfn c() {}\n",
    );
    check(
        "@when(\n    // why\n    debug\n)\nfn d() {}\n",
        "@when(\n    // why\n    debug\n)\nfn d() {}\n",
    );
    // The output parses again: no comma crept in.
    let out = fmt("@when(\n    debug // x\n)\nfn e() {}\n");
    assert!(!out.contains(",\n"), "{out}");
}
