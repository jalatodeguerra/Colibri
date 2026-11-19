//! Lexer and statement-termination tests (DESIGN §3, §12).

use colibri_syntax::{FileId, StrPart, TokenKind, insert_terminators, lex, tokenize};

const F: FileId = FileId(0);

fn render(kinds: impl Iterator<Item = TokenKind>) -> String {
    kinds
        .filter(|k| *k != TokenKind::Eof)
        .map(|k| k.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Raw tokens (with `Newline`), asserting there are no diagnostics.
fn raw(src: &str) -> String {
    let (tokens, diags) = lex(F, src);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    render(tokens.into_iter().map(|t| t.kind))
}

/// The parser's stream (terminators inserted), asserting no diagnostics.
fn toks(src: &str) -> String {
    let (tokens, diags) = tokenize(F, src);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    render(tokens.into_iter().map(|t| t.kind))
}

fn errors(src: &str) -> Vec<String> {
    lex(F, src).1.into_iter().map(|d| d.message).collect()
}

/// The parts of the first string token: `L(text)` or `I(expr)` / `I(expr|spec)`.
fn parts(src: &str) -> Vec<String> {
    let (tokens, diags) = lex(F, src);
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    let TokenKind::Str(lit) = &tokens[0].kind else {
        panic!("not a string: {:?}", tokens[0].kind);
    };
    lit.parts
        .iter()
        .map(|p| match p {
            StrPart::Lit(s) => format!("L({s})"),
            StrPart::Interp { expr, spec } => {
                let text = &src[expr.start as usize..expr.end as usize];
                match spec {
                    Some(spec) => format!("I({text}|{spec})"),
                    None => format!("I({text})"),
                }
            }
        })
        .collect()
}

// ---- keywords, identifiers ----

#[test]
fn every_keyword() {
    let words = "fn let var const type return if else for in while break continue match \
                 struct class enum interface impl import pub as is self own borrow weak \
                 extern unsafe defer test go some none ok err or and not true false";
    let (tokens, diags) = lex(F, words);
    assert!(diags.is_empty());
    let texts: Vec<_> = words.split_whitespace().collect();
    assert_eq!(tokens.len(), texts.len() + 1);
    for (tok, word) in tokens.iter().zip(&texts) {
        assert_eq!(tok.kind.fixed_text(), Some(*word), "{:?}", tok.kind);
    }
}

#[test]
fn use_is_an_identifier() {
    assert_eq!(raw("use"), "Ident(use)");
}

#[test]
fn identifiers_and_underscore() {
    assert_eq!(
        raw("foo _x x_1 _ __"),
        "Ident(foo) Ident(_x) Ident(x_1) _ Ident(__)"
    );
    assert_eq!(raw("héros größe"), "Ident(héros) Ident(größe)");
    assert_eq!(raw("fnord letter"), "Ident(fnord) Ident(letter)");
}

// ---- numbers ----

#[test]
fn integers() {
    assert_eq!(
        raw("42 1_000_000 0xFF 0b1010 0o17 0xFF_FF 0"),
        "Int(42) Int(1000000) Int(255) Int(10) Int(15) Int(65535) Int(0)"
    );
    assert_eq!(raw("0XaB 0B11"), "Int(171) Int(3)");
    assert_eq!(raw("18446744073709551615"), "Int(18446744073709551615)");
    assert_eq!(raw("0xFFFFFFFFFFFFFFFF"), "Int(18446744073709551615)");
}

#[test]
fn floats() {
    assert_eq!(
        raw("3.14 2.0 1e-9 1_000.5 1e5 2.5E+3 1E2"),
        "Float(3.14) Float(2.0) Float(1e-9) Float(1000.5) Float(100000.0) Float(2500.0) Float(100.0)"
    );
}

#[test]
fn integer_overflow() {
    assert_eq!(
        errors("18446744073709551616"),
        ["integer literal is too large"]
    );
    assert_eq!(
        errors("0x1_0000_0000_0000_0000"),
        ["integer literal is too large"]
    );
    let (tokens, diags) = lex(F, "99999999999999999999");
    assert!(diags[0].hint.is_some());
    assert_eq!(tokens[0].kind, TokenKind::Int(0));
}

#[test]
fn bad_number_literals() {
    assert_eq!(errors("0x"), ["expected digits after `0x`"]);
    assert_eq!(errors("0b102"), ["invalid digit `2` in binary literal"]);
    assert_eq!(errors("0o8"), ["invalid digit `8` in octal literal"]);
    assert_eq!(errors("12px"), ["invalid suffix `px` on number literal"]);
    assert_eq!(
        errors("0xFFu8"),
        ["invalid digit `u` in hexadecimal literal"]
    );
    assert_eq!(errors("1e999"), ["float literal is too large"]);
}

#[test]
fn ranges_and_field_access_are_not_floats() {
    assert_eq!(raw("0..10"), "Int(0) .. Int(10)");
    assert_eq!(raw("0..=10"), "Int(0) ..= Int(10)");
    assert_eq!(raw("1..2"), "Int(1) .. Int(2)");
    assert_eq!(raw("x.0"), "Ident(x) . Int(0)");
    assert_eq!(raw("t.1.0"), "Ident(t) . Int(1) . Int(0)");
    assert_eq!(raw("t.0.1"), "Ident(t) . Int(0) . Int(1)");
    assert_eq!(raw("1.foo()"), "Int(1) . Ident(foo) ( )");
    assert_eq!(raw("1.5.max(2)"), "Float(1.5) . Ident(max) ( Int(2) )");
    assert_eq!(raw("a[0].1"), "Ident(a) [ Int(0) ] . Int(1)");
}

// ---- chars and labels ----

#[test]
fn chars() {
    assert_eq!(
        raw(r"'a' 'Z' '0' ' ' 'é' '😀'"),
        "Char('a') Char('Z') Char('0') Char(' ') Char('é') Char('😀')"
    );
    assert_eq!(
        raw(r#"'\n' '\r' '\t' '\\' '\'' '\"' '\0' '\{' '\}'"#),
        "Char('\\n') Char('\\r') Char('\\t') Char('\\\\') Char('\\'') Char('\"') Char('\\0') Char('{') Char('}')"
    );
    assert_eq!(raw(r"'\u{1F600}' '\u{41}'"), "Char('😀') Char('A')");
    assert_eq!(raw("'{' '}' '\"'"), "Char('{') Char('}') Char('\"')");
}

#[test]
fn labels() {
    assert_eq!(raw("break 'outer"), "break Label('outer)");
    assert_eq!(
        raw("'outer: for x in xs { continue 'outer }"),
        "Label('outer) : for Ident(x) in Ident(xs) { continue Label('outer) }"
    );
    // A single-letter label is distinguished from a char by the closing quote.
    assert_eq!(raw("'a 'a'"), "Label('a) Char('a')");
    assert_eq!(raw("'_x"), "Label('_x)");
}

#[test]
fn bad_chars() {
    assert_eq!(errors("'ab'"), ["a char literal holds one character"]);
    let (_, diags) = lex(F, "'ab'");
    assert_eq!(
        diags[0].hint.as_deref(),
        Some("use double quotes for strings: \"ab\"")
    );
    assert_eq!(errors("'😀😀'"), ["a char literal holds one character"]);
    assert_eq!(errors("''"), ["empty char literal"]);
    assert_eq!(errors("'"), ["unterminated char literal"]);
    assert_eq!(errors(r"'\q'"), ["unknown escape `\\q`"]);
    assert_eq!(
        errors(r"'\u{110000}'"),
        ["`\\u{...}` is not a valid Unicode character"]
    );
    assert_eq!(errors(r"'\u41'"), ["expected `{` after `\\u`"]);
}

// ---- strings ----

#[test]
fn plain_strings_and_escapes() {
    assert_eq!(parts(r#""""#), Vec::<String>::new());
    assert_eq!(parts(r#""hello""#), ["L(hello)"]);
    assert_eq!(
        parts(r#""a\nb\tc\r\\\"\'\0\{\}\u{1F600}\u{e9}""#),
        ["L(a\nb\tc\r\\\"'\0{}😀é)"]
    );
    assert_eq!(parts(r#""héllo wörld 😀""#), ["L(héllo wörld 😀)"]);
}

#[test]
fn bad_escapes() {
    assert_eq!(errors(r#""a\qb""#), ["unknown escape `\\q`"]);
    assert_eq!(errors(r#""\u{}""#), ["`\\u{...}` takes 1 to 6 hex digits"]);
    assert_eq!(
        errors(r#""\u{D800}""#),
        ["`\\u{...}` is not a valid Unicode character"]
    );
}

#[test]
fn interpolation() {
    assert_eq!(
        parts(r#""Hello, {name}!""#),
        ["L(Hello, )", "I(name)", "L(!)"]
    );
    assert_eq!(parts(r#""{a}{b}""#), ["I(a)", "I(b)"]);
    assert_eq!(parts(r#""{a + b * 2}""#), ["I(a + b * 2)"]);
    assert_eq!(parts(r#""{p.pos}""#), ["I(p.pos)"]);
}

#[test]
fn interpolation_hole_span_points_into_the_source() {
    let src = r#"let s = "x = {x:.2}!""#;
    let (tokens, _) = lex(F, src);
    let TokenKind::Str(lit) = &tokens[3].kind else {
        panic!()
    };
    let StrPart::Interp { expr, spec } = &lit.parts[1] else {
        panic!()
    };
    assert_eq!(&src[expr.start as usize..expr.end as usize], "x");
    assert_eq!(spec.as_deref(), Some(".2"));
    // The token span covers the whole literal including quotes.
    let span = tokens[3].span;
    assert_eq!(
        &src[span.start as usize..span.end as usize],
        r#""x = {x:.2}!""#
    );
}

#[test]
fn interpolation_format_specs() {
    assert_eq!(parts(r#""{x:.2}""#), ["I(x|.2)"]);
    assert_eq!(parts(r#""{n:>8}""#), ["I(n|>8)"]);
    assert_eq!(parts(r#""{id:x}""#), ["I(id|x)"]);
    assert_eq!(parts(r#""{n:08.3} units""#), ["I(n|08.3)", "L( units)"]);
}

#[test]
fn interpolation_nesting() {
    // Nested quotes.
    assert_eq!(parts(r#""{m["key"]}""#), [r#"I(m["key"])"#]);
    assert_eq!(
        parts(r#""a {m["k"]} b {m["j"]} c""#),
        ["L(a )", r#"I(m["k"])"#, "L( b )", r#"I(m["j"])"#, "L( c)"]
    );
    // Map literal: its colon is at depth >= 1, so it is not a spec.
    assert_eq!(parts(r#""{f(a, {"k": 1})}""#), [r#"I(f(a, {"k": 1}))"#]);
    // Colon inside a nested string and inside brackets.
    assert_eq!(parts(r#""{m["a:b"]}""#), [r#"I(m["a:b"])"#]);
    assert_eq!(parts(r#""{s[1..2]:>4}""#), ["I(s[1..2]|>4)"]);
    // Strings inside strings inside holes.
    assert_eq!(parts(r#""{f("x{y}z")}""#), [r#"I(f("x{y}z"))"#]);
    assert_eq!(
        parts(r#""{f("a {g("b")} c")}""#),
        [r#"I(f("a {g("b")} c"))"#]
    );
    // Braces and quotes in char literals.
    assert_eq!(parts(r#""{c == '}'}""#), ["I(c == '}')"]);
    assert_eq!(parts(r#""{c == '"'}""#), [r#"I(c == '"')"#]);
    // Escaped quote in a nested string.
    assert_eq!(parts(r#""{f("a\"b")}""#), [r#"I(f("a\"b"))"#]);
    // Closure with a block body.
    assert_eq!(
        parts(r#""{xs.map(fn(x) => {x * 2}).len()}""#),
        ["I(xs.map(fn(x) => {x * 2}).len())"]
    );
}

#[test]
fn literal_braces() {
    assert_eq!(parts(r#""{{}}""#), ["L({})"]);
    assert_eq!(parts(r#""a {{x}} {y}""#), ["L(a {x} )", "I(y)"]);
    assert_eq!(parts(r#""{{{x}}}""#), ["L({)", "I(x)", "L(})"]);
    assert_eq!(parts(r#""\{ \}""#), ["L({ })"]);
}

#[test]
fn raw_strings() {
    assert_eq!(
        parts(r#"r"C:\path\{not interpolated}""#),
        [r"L(C:\path\{not interpolated})"]
    );
    assert_eq!(parts(r#"r"{{}} \n""#), [r"L({{}} \n)"]);
    assert_eq!(parts(r#"r"""#), Vec::<String>::new());
    assert_eq!(raw(r#"r"a" r"b""#), r#"Str("a") Str("b")"#);
    // `r` on its own is still an identifier.
    assert_eq!(raw("r rx r.x"), "Ident(r) Ident(rx) Ident(r) . Ident(x)");
}

#[test]
fn string_errors() {
    assert_eq!(errors(r#""abc"#), ["unterminated string literal"]);
    assert_eq!(errors("\"abc\nlet x = 1"), ["unterminated string literal"]);
    assert_eq!(errors(r#"r"abc"#), ["unterminated string literal"]);
    assert_eq!(errors("\"\"\"abc"), ["unterminated string literal"]);
    assert_eq!(errors(r#""{x"#), ["unterminated interpolation"]);
    assert_eq!(errors("\"{x\"\n"), ["unterminated interpolation"]);
    assert_eq!(errors(r#""{}""#), ["empty interpolation"]);
    assert_eq!(errors(r#""{ }""#), ["empty interpolation"]);
    assert_eq!(errors(r#""a } b""#), ["unmatched `}` in string"]);
}

#[test]
fn unterminated_string_points_at_its_start_and_hints() {
    let (_, diags) = lex(F, "let s = \"oops\nlet t = 1");
    assert_eq!(diags.len(), 1);
    assert_eq!((diags[0].span.start, diags[0].span.end), (8, 9));
    assert!(diags[0].hint.as_deref().unwrap().contains("started here"));
}

#[test]
fn lexing_continues_after_a_string_error() {
    assert_eq!(
        toks_ignoring_errors("let s = \"oops\nlet t = 1"),
        "let Ident(s) = Str(\"oops\") Semi let Ident(t) = Int(1) Semi"
    );
}

fn toks_ignoring_errors(src: &str) -> String {
    render(tokenize(F, src).0.into_iter().map(|t| t.kind))
}

// ---- multi-line strings ----

#[test]
fn multiline_basic_and_indentation() {
    let src = "\"\"\"\n    hello\n      world\n    end\n    \"\"\"";
    assert_eq!(parts(src), ["L(hello\n  world\nend)"]);
    // Closing delimiter at column 0.
    let src = "\"\"\"\n  a\n  b\n\"\"\"";
    assert_eq!(parts(src), ["L(a\nb)"]);
    // Blank lines don't count toward the common indent and stay blank.
    let src = "\"\"\"\n    a\n\n    b\n  \"\"\"";
    assert_eq!(parts(src), ["L(a\n\nb)"]);
    // Text on the opening line counts as a line.
    let src = "\"\"\"x\n    y\"\"\"";
    assert_eq!(parts(src), ["L(x\n    y)"]);
    // Single-line triple quotes.
    assert_eq!(parts("\"\"\"abc\"\"\""), ["L(abc)"]);
    assert_eq!(parts("\"\"\"\n\"\"\""), Vec::<String>::new());
}

#[test]
fn multiline_allows_quotes_and_escapes() {
    let src = "\"\"\"\n  say \"hi\"\n  tab\\there\n  \"\"\"";
    assert_eq!(parts(src), ["L(say \"hi\"\ntab\there)"]);
}

#[test]
fn multiline_crlf() {
    let src = "\"\"\"\r\n  a\r\n  b\r\n  \"\"\"";
    assert_eq!(parts(src), ["L(a\nb)"]);
}

#[test]
fn multiline_with_interpolation() {
    let src = "\"\"\"\n    name: {name}\n      score: {score:>4} pts\n    {x}\n    \"\"\"";
    assert_eq!(
        parts(src),
        [
            "L(name: )",
            "I(name)",
            "L(\n  score: )",
            "I(score|>4)",
            "L( pts\n)",
            "I(x)"
        ]
    );
    // Hole at the very start of a line pins the indentation to that line.
    let src = "\"\"\"\n    a\n  {x}\n  \"\"\"";
    assert_eq!(parts(src), ["L(  a\n)", "I(x)"]);
    // Holes may contain quotes, and braces in the text are escaped as usual.
    let src = "\"\"\"\n  {m[\"k\"]} {{json}}\n  \"\"\"";
    assert_eq!(parts(src), ["I(m[\"k\"])", "L( {json})"]);
    // A hole may span lines.
    let src = "\"\"\"\n  a {f(\n    1,\n    2)} b\n  \"\"\"";
    assert_eq!(parts(src), ["L(a )", "I(f(\n    1,\n    2))", "L( b)"]);
}

#[test]
fn raw_multiline() {
    let src = "r\"\"\"\n    {\n      \"a\": 1\\n\n    }\n    \"\"\"";
    assert_eq!(parts(src), ["L({\n  \"a\": 1\\n\n})"]);
    let src = "r\"\"\"\n  {{x}}\n  \"\"\"";
    assert_eq!(parts(src), ["L({{x}})"]);
}

#[test]
fn multiline_hole_span_is_in_the_original_source() {
    let src = "\"\"\"\n    a {x}\n    \"\"\"";
    let (tokens, _) = lex(F, src);
    let TokenKind::Str(lit) = &tokens[0].kind else {
        panic!()
    };
    let StrPart::Interp { expr, .. } = &lit.parts[1] else {
        panic!()
    };
    assert_eq!(&src[expr.start as usize..expr.end as usize], "x");
}

// ---- punctuation ----

#[test]
fn punctuation() {
    assert_eq!(
        raw(
            "( ) [ ] { } , . .. ..= : ; -> => = == != < <= > >= + - * / % += -= *= /= %= && || ! ? @ | & ^ ~ << >>"
        ),
        "( ) [ ] { } , . .. ..= : Semi -> => = == != < <= > >= + - * / % += -= *= /= %= && || ! ? @ | & ^ ~ << >>"
    );
    assert_eq!(
        raw("&= |= ^= <<= >>= << >> <= >= & | ^ <"),
        "&= |= ^= <<= >>= << >> <= >= & | ^ <"
    );
    assert_eq!(raw("a<<=1"), "Ident(a) <<= Int(1)");
    assert_eq!(raw("a>>=b"), "Ident(a) >>= Ident(b)");
    assert_eq!(raw("a&&=b"), "Ident(a) && = Ident(b)");
    assert_eq!(raw("a..b"), "Ident(a) .. Ident(b)");
    assert_eq!(raw("x==-1"), "Ident(x) == - Int(1)");
    assert_eq!(raw("|e|"), "| Ident(e) |");
    assert_eq!(raw("a&&b||c"), "Ident(a) && Ident(b) || Ident(c)");
    assert_eq!(raw("@repr(C)"), "@ Ident(repr) ( Ident(C) )");
}

#[test]
fn bad_characters() {
    assert_eq!(errors("a # b"), ["unexpected character `#`"]);
    assert_eq!(errors("$"), ["unexpected character `$`"]);
    let (tokens, _) = lex(F, "a # b");
    assert_eq!(
        render(tokens.into_iter().map(|t| t.kind)),
        "Ident(a) Error Ident(b)"
    );
}

// ---- comments, newlines ----

#[test]
fn comments_are_skipped() {
    assert_eq!(raw("a // comment\nb"), "Ident(a) Newline Ident(b)");
    assert_eq!(raw("a /* c */ b"), "Ident(a) Ident(b)");
    assert_eq!(raw("a /* x /* nested */ still */ b"), "Ident(a) Ident(b)");
    assert_eq!(raw("a //// not doc\nb"), "Ident(a) Newline Ident(b)");
    assert_eq!(raw("/* a // b */ c"), "Ident(c)");
    assert_eq!(raw("// a /* b\nc"), "Newline Ident(c)");
    assert_eq!(raw("a /\n b"), "Ident(a) / Newline Ident(b)");
}

#[test]
fn block_comment_with_a_newline_counts_as_one() {
    assert_eq!(raw("a /* x\ny */ b"), "Ident(a) Newline Ident(b)");
    assert_eq!(toks("a /* x\ny */ b"), "Ident(a) Semi Ident(b) Semi");
}

#[test]
fn doc_comments() {
    assert_eq!(raw("/// hello\nfn"), r#"Doc("hello") Newline fn"#);
    assert_eq!(raw("///no space"), r#"Doc("no space")"#);
    assert_eq!(raw("///  two"), r#"Doc(" two")"#);
    assert_eq!(raw("///\nx"), r#"Doc("") Newline Ident(x)"#);
    assert_eq!(raw("/// crlf\r\nx"), r#"Doc("crlf") Newline Ident(x)"#);
}

#[test]
fn comment_errors() {
    assert_eq!(errors("a /* open"), ["unterminated block comment"]);
    assert_eq!(
        errors("a /* open /* nested */"),
        ["unterminated block comment"]
    );
    let (_, diags) = lex(F, "a /* open");
    assert_eq!((diags[0].span.start, diags[0].span.end), (2, 4));
}

#[test]
fn crlf_is_one_newline() {
    assert_eq!(
        raw("a\r\nb\nc\r\n"),
        "Ident(a) Newline Ident(b) Newline Ident(c) Newline"
    );
    let (tokens, _) = lex(F, "a\r\nb");
    assert_eq!((tokens[1].span.start, tokens[1].span.end), (1, 3));
}

#[test]
fn always_ends_with_eof() {
    for src in ["", "a", "\"abc", "/* x", "'"] {
        let (tokens, _) = lex(F, src);
        let last = tokens.last().unwrap();
        assert_eq!(last.kind, TokenKind::Eof, "{src:?}");
        assert_eq!(last.span.start as usize, src.len());
    }
}

#[test]
fn token_spans() {
    let src = "let x = 42";
    let (tokens, _) = lex(F, src);
    let texts: Vec<_> = tokens
        .iter()
        .map(|t| &src[t.span.start as usize..t.span.end as usize])
        .collect();
    assert_eq!(texts, ["let", "x", "=", "42", ""]);
}

// ---- statement termination (DESIGN §3.2) ----

#[test]
fn newline_ends_a_statement_after_enders() {
    assert_eq!(toks("x\ny"), "Ident(x) Semi Ident(y) Semi");
    assert_eq!(
        toks("1\n2.5\n'a'\n\"s\"\n'l"),
        "Int(1) Semi Float(2.5) Semi Char('a') Semi Str(\"s\") Semi Label('l) Semi"
    );
    assert_eq!(
        toks("true\nfalse\nnone\nself"),
        "true Semi false Semi none Semi self Semi"
    );
    assert_eq!(
        toks("f()\nxs[0]\n}"),
        "Ident(f) ( ) Semi Ident(xs) [ Int(0) ] Semi } Semi"
    );
    assert_eq!(
        toks("return\nbreak\ncontinue"),
        "return Semi break Semi continue Semi"
    );
    assert_eq!(toks("x?\n_\n"), "Ident(x) ? Semi _ Semi");
    assert_eq!(
        toks("break 'outer\nx"),
        "break Label('outer) Semi Ident(x) Semi"
    );
    assert_eq!(
        toks("let s = \"a {b}\"\nx"),
        "let Ident(s) = Str(\"a \"{12..13}) Semi Ident(x) Semi"
    );
}

#[test]
fn newline_does_not_end_after_other_tokens() {
    assert_eq!(toks("a +\nb"), "Ident(a) + Ident(b) Semi");
    assert_eq!(toks("a,\nb"), "Ident(a) , Ident(b) Semi");
    assert_eq!(toks("let x =\n1"), "let Ident(x) = Int(1) Semi");
    assert_eq!(
        toks("f(a) ->\nint"),
        "Ident(f) ( Ident(a) ) -> Ident(int) Semi"
    );
    assert_eq!(toks("if x &&\ny {"), "if Ident(x) && Ident(y) {");
    assert_eq!(toks("x or\ny"), "Ident(x) or Ident(y) Semi");
    assert_eq!(toks("fn\nf"), "fn Ident(f) Semi");
}

#[test]
fn design_example_operator_at_end_of_line() {
    let src = "let total = price * qty +\n            shipping\n";
    assert_eq!(
        toks(src),
        "let Ident(total) = Ident(price) * Ident(qty) + Ident(shipping) Semi"
    );
}

#[test]
fn design_example_leading_dot_chain() {
    let src = "let names = players\n    .filter(fn(p) => p.alive())\n    .map(fn(p) => p.name)\n";
    assert_eq!(
        toks(src),
        "let Ident(names) = Ident(players) . Ident(filter) ( fn ( Ident(p) ) => Ident(p) . Ident(alive) ( ) ) \
         . Ident(map) ( fn ( Ident(p) ) => Ident(p) . Ident(name) ) Semi"
    );
}

#[test]
fn design_example_multiline_call() {
    let src = "draw_text(\n    \"score: {score}\",\n    x, y\n)\nnext()";
    assert_eq!(
        toks(src),
        "Ident(draw_text) ( Str(\"score: \"{24..29}) , Ident(x) , Ident(y) ) Semi Ident(next) ( ) Semi"
    );
}

#[test]
fn brackets_suppress_newlines() {
    assert_eq!(toks("[1\n2\n]"), "[ Int(1) Int(2) ] Semi");
    assert_eq!(toks("f(a\nb)"), "Ident(f) ( Ident(a) Ident(b) ) Semi");
    assert_eq!(
        toks("f((a\nb)\nc)"),
        "Ident(f) ( ( Ident(a) Ident(b) ) Ident(c) ) Semi"
    );
    assert_eq!(
        toks("[\n1,\n2,\n]\nx"),
        "[ Int(1) , Int(2) , ] Semi Ident(x) Semi"
    );
}

#[test]
fn braces_in_parentheses_get_terminators() {
    let src = "f(fn() {\n a()\n b()\n})";
    assert_eq!(
        toks(src),
        "Ident(f) ( fn ( ) { Ident(a) ( ) Semi Ident(b) ( ) Semi } ) Semi"
    );
    // Back to suppressed newlines after the closure's `}`.
    let src = "f(fn() {\n a()\n},\n 1\n)\ng()";
    assert_eq!(
        toks(src),
        "Ident(f) ( fn ( ) { Ident(a) ( ) Semi } , Int(1) ) Semi Ident(g) ( ) Semi"
    );
    // A paren inside a brace inside a paren.
    let src = "f({\n g(\n 1\n)\n})";
    assert_eq!(toks(src), "Ident(f) ( { Ident(g) ( Int(1) ) Semi } ) Semi");
}

#[test]
fn leading_continuations() {
    assert_eq!(toks("a\n.b"), "Ident(a) . Ident(b) Semi");
    assert_eq!(toks("a\n\n  .b"), "Ident(a) . Ident(b) Semi");
    assert_eq!(toks("a\nor b"), "Ident(a) or Ident(b) Semi");
    assert_eq!(toks("a\n&& b"), "Ident(a) && Ident(b) Semi");
    assert_eq!(toks("x\n|| y"), "Ident(x) || Ident(y) Semi");
    assert_eq!(
        toks("if c {\n}\nelse {\n}"),
        "if Ident(c) { } else { } Semi"
    );
}

#[test]
fn leading_dot_variant_starts_a_new_line() {
    // `.Circle(r) =>` is a match arm with a variant shorthand, not a method call.
    assert_eq!(
        toks(
            "a
.Circle"
        ),
        "Ident(a) Semi . Ident(Circle) Semi"
    );
    assert_eq!(
        toks(
            "a
.circle"
        ),
        "Ident(a) . Ident(circle) Semi"
    );
}

#[test]
fn else_on_the_next_line() {
    let src = "if a {\n  f()\n}\nelse {\n  g()\n}\nh()";
    assert_eq!(
        toks(src),
        "if Ident(a) { Ident(f) ( ) Semi } else { Ident(g) ( ) Semi } Semi Ident(h) ( ) Semi"
    );
    let src = "if a {\n  f()\n} else {\n  g()\n}";
    assert_eq!(
        toks(src),
        "if Ident(a) { Ident(f) ( ) Semi } else { Ident(g) ( ) Semi } Semi"
    );
}

#[test]
fn leading_range_is_not_a_continuation() {
    assert_eq!(toks("a\n..b"), "Ident(a) Semi .. Ident(b) Semi");
    assert_eq!(toks("a\n..=b"), "Ident(a) Semi ..= Ident(b) Semi");
}

#[test]
fn other_leading_operators_are_not_continuations() {
    assert_eq!(toks("a\n+ b"), "Ident(a) Semi + Ident(b) Semi");
    assert_eq!(toks("a\n| b"), "Ident(a) Semi | Ident(b) Semi");
    assert_eq!(toks("a\n? b"), "Ident(a) Semi ? Ident(b) Semi");
}

#[test]
fn collapsing_and_edges() {
    // Blank lines and comments collapse.
    assert_eq!(toks("a\n\n\n// c\n\nb"), "Ident(a) Semi Ident(b) Semi");
    // Nothing at the start, nothing after `{`.
    assert_eq!(
        toks("\n\nfn f() {\n\nx\n}"),
        "fn Ident(f) ( ) { Ident(x) Semi } Semi"
    );
    // Explicit semicolons are not doubled up.
    assert_eq!(toks("a;\nb"), "Ident(a) Semi Ident(b) Semi");
    assert_eq!(toks("a; b; c"), "Ident(a) Semi Ident(b) Semi Ident(c) Semi");
    // Final terminator before Eof only when the last token can end a statement.
    assert_eq!(toks("a"), "Ident(a) Semi");
    assert_eq!(toks("a +"), "Ident(a) +");
    assert_eq!(toks("a;"), "Ident(a) Semi");
    assert_eq!(toks(""), "");
    assert_eq!(toks("\n\n"), "");
    // No terminator at Eof inside an unclosed paren.
    let (tokens, _) = tokenize(F, "f(a\n");
    assert_eq!(
        render(tokens.into_iter().map(|t| t.kind)),
        "Ident(f) ( Ident(a)"
    );
}

#[test]
fn crlf_terminates_like_lf() {
    assert_eq!(
        toks("a\r\nb\r\n.c"),
        "Ident(a) Semi Ident(b) . Ident(c) Semi"
    );
}

#[test]
fn implicit_semi_has_an_empty_span_at_the_newline() {
    let (tokens, _) = tokenize(F, "ab\ncd");
    let semi = &tokens[1];
    assert_eq!(semi.kind, TokenKind::Semi);
    assert_eq!((semi.span.start, semi.span.end), (2, 2));
    let eof_semi = &tokens[3];
    assert_eq!((eof_semi.span.start, eof_semi.span.end), (5, 5));
}

#[test]
fn doc_comments_pass_through_without_affecting_termination() {
    // The doc comment is kept, and does not stop `.` from continuing a line.
    assert_eq!(
        toks("a\n/// note\n.b"),
        r#"Ident(a) Doc("note") . Ident(b) Semi"#
    );
    assert_eq!(
        toks("/// doc\nfn f() {}\n/// next\nfn g() {}"),
        r#"Doc("doc") fn Ident(f) ( ) { } Semi Doc("next") fn Ident(g) ( ) { } Semi"#
    );
    // A doc comment right after a statement does not swallow its terminator.
    assert_eq!(
        toks("x /// trailing\ny"),
        r#"Ident(x) Doc("trailing") Semi Ident(y) Semi"#
    );
}

#[test]
fn small_program() {
    let src = "\
fn main() {
    let xs = [1, 2, 3]
    for x in xs {
        print(\"{x}\")
    }
    return
}
";
    assert_eq!(
        toks(src),
        "fn Ident(main) ( ) { let Ident(xs) = [ Int(1) , Int(2) , Int(3) ] Semi \
         for Ident(x) in Ident(xs) { Ident(print) ( Str({69..70}) ) Semi } Semi \
         return Semi } Semi"
    );
}

#[test]
fn insert_terminators_is_pure_on_token_streams() {
    let (raw_tokens, _) = lex(F, "a\nb");
    let out = insert_terminators(raw_tokens.clone());
    assert_eq!(out.len(), 5); // a ; b ; eof
    assert!(raw_tokens.iter().any(|t| t.kind == TokenKind::Newline));
    assert!(out.iter().all(|t| t.kind != TokenKind::Newline));
}
