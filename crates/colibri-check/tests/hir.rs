//! The shape of the HIR the checker produces: what engines can rely on.

use colibri_check::hir::*;
use colibri_check::types::{IntTy, Ty};
use colibri_syntax::{FileId, Severity, parse_file};

fn check(src: &str) -> (Option<Program>, Vec<String>) {
    let (ast, parse_diags) = parse_file(FileId(0), src);
    assert!(parse_diags.is_empty(), "{parse_diags:?}");
    let checked = colibri_check::check(&ast);
    let messages = checked
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect();
    (checked.program, messages)
}

fn program(src: &str) -> Program {
    let (program, messages) = check(src);
    program.unwrap_or_else(|| panic!("expected no errors, got {messages:?}"))
}

fn body_of(p: &Program, name: &str) -> std::rc::Rc<Body> {
    let id = p.fn_by_name(name).expect("function exists");
    std::rc::Rc::clone(&p.fns[id.0 as usize].body)
}

/// The tail expression of a function with an `=>` body.
fn tail(body: &Body) -> &Expr {
    body.block.tail.as_deref().expect("an expression body")
}

#[test]
fn locals_are_flat_slots_listed_per_block_in_declaration_order() {
    let p = program(
        "fn f(a: int, b: int) {\n    let x = 1\n    {\n        let y = 2\n        let z = 3\n    }\n    var w = 4\n}\n",
    );
    let body = body_of(&p, "f");
    assert_eq!(body.param_count, 2);
    let names: Vec<&str> = body.locals.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "x", "y", "z", "w"]);
    // The function block's drop list starts with the parameters.
    let top: Vec<u32> = body.block.locals.iter().map(|l| l.0).collect();
    assert_eq!(top, [0, 1, 2, 5]);
    let StmtKind::Expr(Expr {
        kind: ExprKind::Block(inner),
        ..
    }) = &body.block.stmts[1].kind
    else {
        panic!("expected a nested block, got {:?}", body.block.stmts[1]);
    };
    let inner: Vec<u32> = inner.locals.iter().map(|l| l.0).collect();
    assert_eq!(inner, [3, 4]);
    assert!(body.locals[5].mutable && !body.locals[2].mutable);
    assert!(body.locals.iter().all(|l| l.storage == Storage::Slot));
}

#[test]
fn integer_operations_carry_their_width() {
    let p = program("fn f(a: int, b: int) -> int => a + b * 2\nfn g(x: float) -> float => x / 2\n");
    let f = body_of(&p, "f");
    let ExprKind::Binary(op, _, rhs) = &tail(&f).kind else {
        panic!("expected a binary operation");
    };
    assert_eq!(*op, BinOp::Arith(ArithOp::Add, Num::Int(IntTy::I64)));
    let ExprKind::Binary(inner, _, _) = &rhs.kind else {
        panic!("expected a nested operation");
    };
    assert_eq!(*inner, BinOp::Arith(ArithOp::Mul, Num::Int(IntTy::I64)));

    let g = body_of(&p, "g");
    let ExprKind::Binary(op, _, two) = &tail(&g).kind else {
        panic!("expected a binary operation");
    };
    assert_eq!(*op, BinOp::Arith(ArithOp::Div, Num::Float));
    // The untyped literal `2` became a float literal.
    assert!(matches!(two.kind, ExprKind::Lit(Lit::Float(x)) if x == 2.0));
    assert_eq!(two.ty, Ty::Float);
}

#[test]
fn string_plus_is_a_concatenation_and_comparisons_name_their_operand() {
    let p = program("fn f(a: str) -> str => a + \"!\"\nfn g(a: str, b: str) -> bool => a < b\n");
    assert!(matches!(
        tail(&body_of(&p, "f")).kind,
        ExprKind::Binary(BinOp::Concat, ..)
    ));
    assert!(matches!(
        tail(&body_of(&p, "g")).kind,
        ExprKind::Binary(BinOp::Cmp(CmpOp::Lt, Operand::Str), ..)
    ));
}

#[test]
fn literals_are_typed_at_the_binding() {
    let p = program("fn f() {\n    let a = 1\n    let b: float = 1\n    let c = 1.5\n}\n");
    let body = body_of(&p, "f");
    let tys: Vec<&Ty> = body.locals.iter().map(|l| &l.ty).collect();
    assert_eq!(tys, [&Ty::INT, &Ty::Float, &Ty::Float]);
    let StmtKind::Let { init, .. } = &body.block.stmts[1].kind else {
        panic!("expected a let");
    };
    assert!(matches!(init.kind, ExprKind::Lit(Lit::Float(x)) if x == 1.0));
}

#[test]
fn calls_are_resolved_with_defaults_and_named_arguments_made_positional() {
    let p = program(
        "fn log(msg: str, level: str = \"info\", n: int = 1) {}\nfn main() {\n    log(n: 5, msg: \"hi\")\n}\n",
    );
    let main = body_of(&p, "main");
    let StmtKind::Expr(Expr {
        kind: ExprKind::Call(call),
        ..
    }) = &main.block.stmts[0].kind
    else {
        panic!("expected a call, got {:?}", main.block.stmts[0]);
    };
    assert_eq!(call.callee, Callee::Fn(p.fn_by_name("log").unwrap()));
    assert!(call.type_args.is_empty());
    assert_eq!(call.args.len(), 3);
    let lits: Vec<String> = call
        .args
        .iter()
        .map(|a| match a {
            Arg::Value(Expr {
                kind: ExprKind::Lit(Lit::Str(s)),
                ..
            }) => s.to_string(),
            Arg::Value(Expr {
                kind: ExprKind::Lit(Lit::Int(i)),
                ..
            }) => i.to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(lits, ["hi", "info", "5"]);
}

#[test]
fn interpolation_becomes_a_concat_of_resolved_show_operations() {
    let p = program("fn f(n: int, s: str) -> str => \"a{n}b{s}c{n:>4}\"\n");
    let f = body_of(&p, "f");
    let ExprKind::StrConcat(parts) = &tail(&f).kind else {
        panic!("expected a concatenation");
    };
    assert_eq!(parts.len(), 6);
    let kinds: Vec<&str> = parts
        .iter()
        .map(|part| match &part.kind {
            ExprKind::Lit(Lit::Str(_)) => "text",
            ExprKind::Read(_) => "str",
            ExprKind::Call(call) => match &call.callee {
                Callee::Intrinsic(Intrinsic::Show) => "show",
                Callee::Intrinsic(Intrinsic::Format(_)) => "format",
                other => panic!("unexpected callee {other:?}"),
            },
            other => panic!("unexpected part {other:?}"),
        })
        .collect();
    assert_eq!(kinds, ["text", "show", "text", "str", "text", "format"]);
    assert!(parts.iter().all(|p| p.ty == Ty::Str));
}

#[test]
fn while_is_a_loop_around_an_if_with_a_break() {
    let p = program("fn f() {\n    var i = 0\n    while i < 3 {\n        i += 1\n    }\n}\n");
    let body = body_of(&p, "f");
    let StmtKind::Expr(Expr {
        kind: ExprKind::Loop { id, body: lp },
        ..
    }) = &body.block.stmts[1].kind
    else {
        panic!("expected a loop");
    };
    let StmtKind::Expr(Expr {
        kind: ExprKind::If { else_, .. },
        ..
    }) = &lp.stmts[0].kind
    else {
        panic!("expected an if");
    };
    let exit = else_.as_ref().expect("an else branch");
    assert!(matches!(exit.stmts[0].kind, StmtKind::Break(target) if target == *id));
}

#[test]
fn assignments_use_explicit_places() {
    let p = program("var g = 1\nfn f() {\n    var x = 1\n    x = 2\n    x += 3\n    g -= 1\n}\n");
    let body = body_of(&p, "f");
    let StmtKind::Assign { place, op, .. } = &body.block.stmts[1].kind else {
        panic!("expected an assignment");
    };
    assert!(matches!(place.anchor, Anchor::Local(LocalId(0))));
    assert!(place.path.is_empty());
    assert_eq!((place.display.as_str(), op.is_none()), ("x", true));
    let StmtKind::Assign { op, .. } = &body.block.stmts[2].kind else {
        panic!("expected an assignment");
    };
    assert_eq!(*op, Some(BinOp::Arith(ArithOp::Add, Num::Int(IntTy::I64))));
    let StmtKind::Assign { place, .. } = &body.block.stmts[3].kind else {
        panic!("expected an assignment");
    };
    assert!(matches!(place.anchor, Anchor::Global(GlobalId(0))));
}

#[test]
fn globals_are_ordered_by_dependency_and_script_statements_are_a_body() {
    let p = program("let a = b + 1\nlet b = 2\nprint(a)\nfn main() {}\n");
    let order: Vec<&str> = p
        .init_order
        .iter()
        .map(|g| p.globals[g.0 as usize].name.as_str())
        .collect();
    assert_eq!(order, ["b", "a"]);
    assert_eq!(p.script.name, "<script>");
    assert_eq!(p.script.block.stmts.len(), 1);
    assert_eq!(p.main, p.fn_by_name("main"));
    assert_eq!(p.globals[0].ty, Ty::INT);
}

#[test]
fn out_of_order_named_arguments_are_evaluated_in_source_order() {
    let p = program(
        "fn t(n: int) -> int => n\nfn sub(a: int, b: int) -> int => a - b\nfn main() {\n    sub(b: t(1), a: t(2))\n}\n",
    );
    let main = body_of(&p, "main");
    let StmtKind::Expr(Expr {
        kind: ExprKind::Block(block),
        ..
    }) = &main.block.stmts[0].kind
    else {
        panic!("expected the call to be wrapped in temporaries");
    };
    assert_eq!(block.locals.len(), 2);
    assert_eq!(block.stmts.len(), 2);
}

#[test]
fn every_expression_has_a_resolved_type() {
    fn walk(e: &Expr) {
        assert!(!matches!(e.ty, Ty::Infer(_)), "unresolved type in {e:?}");
        match &e.kind {
            ExprKind::Unary(_, a) => walk(a),
            ExprKind::Binary(_, a, b) | ExprKind::And(a, b) | ExprKind::Or(a, b) => {
                walk(a);
                walk(b);
            }
            ExprKind::Call(c) => c.args.iter().for_each(|a| {
                if let Arg::Value(v) = a {
                    walk(v);
                }
            }),
            ExprKind::StrConcat(parts) => parts.iter().for_each(walk),
            ExprKind::If { cond, then, else_ } => {
                walk(cond);
                block(then);
                if let Some(b) = else_ {
                    block(b);
                }
            }
            ExprKind::Block(b) => block(b),
            ExprKind::Loop { body, .. } => block(body),
            _ => {}
        }
    }
    fn block(b: &Block) {
        assert!(!matches!(b.ty, Ty::Infer(_)));
        for s in &b.stmts {
            match &s.kind {
                StmtKind::Let { init, .. } | StmtKind::Expr(init) => walk(init),
                StmtKind::Assign { rhs, .. } => walk(rhs),
                StmtKind::Return(Some(v)) => walk(v),
                _ => {}
            }
        }
        if let Some(t) = &b.tail {
            walk(t);
        }
    }
    let p = program(
        "fn f(n: int) -> float {\n    let a = 1 + 2\n    let b = 2.5 * 4\n    if n > 1 { return b } else { return 1 }\n}\nprint(f(1))\nprint(1 == 1)\n",
    );
    for f in &p.fns {
        block(&f.body.block);
        assert!(f.body.locals.iter().all(|l| !matches!(l.ty, Ty::Infer(_))));
    }
    block(&p.script.block);
}

#[test]
fn errors_prevent_a_program_but_not_more_diagnostics() {
    let (program, messages) = check("let a: int = \"s\"\nlet b = nope\nfn f() -> str => 1\n");
    assert!(program.is_none());
    assert_eq!(messages.len(), 3, "{messages:?}");
}

#[test]
fn warnings_do_not_prevent_a_program() {
    let src = "fn f() {\n    let x = 1\n    print(x)\n    let x = 2\n    print(x)\n}\n";
    let (ast, _) = parse_file(FileId(0), src);
    let checked = colibri_check::check(&ast);
    assert!(checked.program.is_some());
    assert_eq!(checked.diagnostics.len(), 1);
    assert_eq!(checked.diagnostics[0].severity, Severity::Warning);
    assert_eq!(checked.diagnostics[0].labels.len(), 1);
}

#[test]
fn unknown_names_get_suggestions() {
    let src = "fn main() {\n    let counter = 1\n    print(counter)\n    print(countr)\n}\n";
    let (ast, _) = parse_file(FileId(0), src);
    let checked = colibri_check::check(&ast);
    let hint = checked.diagnostics[0].hint.as_deref();
    assert_eq!(hint, Some("did you mean `counter`?"));
}

#[test]
fn mismatches_point_at_the_annotation_too() {
    let src = "let x: int = \"s\"\n";
    let (ast, _) = parse_file(FileId(0), src);
    let checked = colibri_check::check(&ast);
    let d = &checked.diagnostics[0];
    assert_eq!(d.message, "expected `int`, found `str`");
    assert_eq!(d.labels.len(), 1);
    assert_eq!(d.labels[0].1, "expected because of this type annotation");
    assert_eq!(&src[d.span.start as usize..d.span.end as usize], "\"s\"");
    assert_eq!(
        &src[d.labels[0].0.start as usize..d.labels[0].0.end as usize],
        "int"
    );
}

#[test]
fn struct_literals_hold_one_value_per_field_with_defaults_filled() {
    let p =
        program("struct P { a: int, b: int = 7, c: str }\nfn f() -> P => P { c: \"x\", a: 1 }\n");
    let f = body_of(&p, "f");
    let ExprKind::Struct { fields, .. } = &tail(&f).kind else {
        panic!("expected a struct literal, got {:?}", tail(&f));
    };
    assert_eq!(fields.len(), 3);
    assert!(matches!(&fields[1].kind, ExprKind::Lit(Lit::Int(7))));
}

#[test]
fn match_stays_a_match_with_typed_patterns_and_bound_locals() {
    let p = program(
        "enum E { A(int), B }\nfn f(e: E) -> int => match e { A(n) if n > 0 => n, A(_) => 0, B => -1 }\n",
    );
    let f = body_of(&p, "f");
    let ExprKind::Match { arms, .. } = &tail(&f).kind else {
        panic!("expected a match, got {:?}", tail(&f));
    };
    assert_eq!(arms.len(), 3);
    assert!(arms[0].guard.is_some() && arms[1].guard.is_none());
    // The pattern's binding is the first local of the arm's block.
    assert_eq!(arms[0].body.locals.len(), 1);
    assert!(matches!(
        &arms[0].pat.kind,
        PatKind::Variant { variant: 0, args, .. } if matches!(args[0].kind, PatKind::Bind(..))
    ));
    assert!(matches!(
        &arms[2].pat.kind,
        PatKind::Variant { variant: 1, .. }
    ));
}

#[test]
fn for_over_a_range_is_its_own_node_with_a_fresh_binding() {
    let p = program("fn f() {\n    for i in 0..=3 {\n        print(i)\n    }\n}\n");
    let f = body_of(&p, "f");
    let StmtKind::Expr(Expr {
        kind: ExprKind::For(l),
        ..
    }) = &f.block.stmts[0].kind
    else {
        panic!("expected a for loop, got {:?}", f.block.stmts[0]);
    };
    assert!(matches!(
        l.source,
        ForSource::Range {
            inclusive: true,
            ..
        }
    ));
    let ForBinding::Local(binding) = l.binding else {
        panic!("the loop variable is bound");
    };
    assert!(l.body.locals.contains(&binding));
}

#[test]
fn var_self_methods_are_marked_and_take_a_place_receiver() {
    let p = program(
        "struct C { n: int }\nimpl C {\n    fn inc(var self) {\n        self.n += 1\n    }\n    fn get(self) -> int => self.n\n}\nfn main() {\n    var c = C { n: 0 }\n    c.inc()\n}\n",
    );
    assert!(body_of(&p, "C.inc").mutates_self);
    assert!(!body_of(&p, "C.get").mutates_self);
}

#[test]
fn structural_comparison_is_its_own_operand() {
    let p = program("struct P { x: int }\nfn f(a: P, b: P) -> bool => a == b\n");
    let f = body_of(&p, "f");
    let ExprKind::Binary(BinOp::Cmp(CmpOp::Eq, operand), _, _) = &tail(&f).kind else {
        panic!("expected a comparison, got {:?}", tail(&f));
    };
    assert_eq!(*operand, Operand::Structural);
}

#[test]
fn tuple_destructuring_is_a_pattern_statement() {
    let p = program("fn f() -> int {\n    let (a, b) = (1, 2)\n    return a + b\n}\n");
    let f = body_of(&p, "f");
    assert!(matches!(
        &f.block.stmts[0].kind,
        StmtKind::LetPat { pat: Pat { kind: PatKind::Tuple(items), .. }, .. } if items.len() == 2
    ));
}

#[test]
fn non_exhaustive_matches_and_unreachable_arms_are_reported() {
    let (program, messages) = check("enum E { A, B }\nfn f(e: E) -> int => match e { A => 1 }\n");
    assert!(program.is_none());
    assert!(
        messages.iter().any(|m| m.contains("missing case: B")),
        "{messages:?}"
    );
    let (program, _) = check("fn f(n: int) -> int => match n { _ => 1, 0 => 2 }\n");
    assert!(program.is_some(), "an unreachable arm is only a warning");
}

/// The first call expression of a function's body (searching the tail and statements).
fn first_call(body: &Body) -> Option<&Call> {
    fn in_expr(e: &Expr) -> Option<&Call> {
        match &e.kind {
            ExprKind::Call(c) => Some(c),
            ExprKind::Coerce(inner, _) => in_expr(inner),
            _ => None,
        }
    }
    body.block.tail.as_deref().and_then(in_expr).or_else(|| {
        body.block.stmts.iter().find_map(|s| match &s.kind {
            StmtKind::Let { init, .. } | StmtKind::Expr(init) => in_expr(init),
            _ => None,
        })
    })
}

#[test]
fn generic_calls_record_their_type_arguments() {
    let p = program(
        "fn id[T](x: T) -> T => x\nfn pair[A, B](a: A, b: B) -> (A, B) => (a, b)\nfn f() -> int => id(1)\nfn g() -> (int, str) => pair(1, \"s\")\n",
    );
    let f = body_of(&p, "f");
    assert_eq!(first_call(&f).expect("a call").type_args, [Ty::INT]);
    let g = body_of(&p, "g");
    assert_eq!(
        first_call(&g).expect("a call").type_args,
        [Ty::INT, Ty::Str]
    );
    assert_eq!(
        p.fns[p.fn_by_name("pair").unwrap().0 as usize].type_params,
        2
    );
}

#[test]
fn an_interface_value_is_a_coerce_node_naming_the_implementation() {
    let p = program(
        "interface Shape { fn area(self) -> float }\nstruct Sq { s: float }\nimpl Shape for Sq { fn area(self) -> float => self.s }\nfn f() -> Shape => Sq { s: 1.0 }\n",
    );
    let body = body_of(&p, "f");
    let ExprKind::Coerce(_, Coercion::ToDyn(id)) = &tail(&body).kind else {
        panic!("expected a coercion, got {:?}", tail(&body).kind);
    };
    let imp = &p.impls[id.0 as usize];
    assert!(!imp.derived);
    assert_eq!(imp.methods.len(), 1);
    assert!(imp.methods[0].is_some());
}

#[test]
fn calls_through_bounds_and_interface_values_are_dispatched_by_index() {
    let p = program(
        "interface Shape { fn area(self) -> float }\nstruct Sq { s: float }\nimpl Shape for Sq { fn area(self) -> float => self.s }\nfn bound[T: Shape](x: T) -> float => x.area()\nfn dynamic(x: Shape) -> float => x.area()\n",
    );
    let bound = body_of(&p, "bound");
    let call = first_call(&bound).expect("a call");
    assert!(matches!(call.callee, Callee::Bound { method: 0, .. }));
    let dynamic = body_of(&p, "dynamic");
    let call = first_call(&dynamic).expect("a call");
    assert!(matches!(call.callee, Callee::Dyn { method: 0, .. }));
}

#[test]
fn default_methods_fill_the_implementation_table() {
    let p = program(
        "interface Greeter { fn name(self) -> str\n fn greet(self) -> str => \"hi\" }\nstruct A { n: int }\nimpl Greeter for A { fn name(self) -> str => \"a\" }\n",
    );
    let imp = p.impls.iter().find(|i| !i.derived).expect("an impl");
    assert!(imp.methods.iter().all(Option::is_some));
}

#[test]
fn generic_errors_are_diagnosed_not_panicked() {
    let (p, messages) =
        check("fn id[T](x: T) -> T => x\nfn main() {\n    let a = id[int, int](1)\n}\n");
    assert!(p.is_none());
    assert!(
        messages.iter().any(|m| m.contains("takes 1 type argument")),
        "{messages:?}"
    );
}

fn warnings_of(src: &str) -> Vec<String> {
    let (ast, _) = parse_file(FileId(0), src);
    let checked = colibri_check::check(&ast);
    checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn unused_locals_warn_and_underscore_names_do_not() {
    let w = warnings_of(
        "fn f() {\n    let a = 1\n    var b = 2\n    b = 3\n    let _c = 4\n    let d = 5\n    print(d)\n}\n",
    );
    assert_eq!(w, ["unused variable `a`", "unused variable `b`"]);
}

#[test]
fn a_local_counts_as_used_when_read_captured_or_changed_through_a_field() {
    let src = "struct P { x: int }\nfn f() {\n    let a = 1\n    let g = fn() => a\n    print(g())\n    var p = P { x: 1 }\n    p.x = 2\n    var n = 0\n    n += 1\n}\n";
    assert!(warnings_of(src).is_empty(), "{:?}", warnings_of(src));
}

#[test]
fn unused_variable_in_a_closure_body_warns_once() {
    let w = warnings_of("fn f() {\n    let g = fn() {\n        let z = 1\n    }\n    g()\n}\n");
    assert_eq!(w, ["unused variable `z`"]);
}

#[test]
fn converting_text_to_a_number_points_at_the_parse_functions() {
    let hint = |src: &str| {
        let (ast, _) = parse_file(FileId(0), src);
        let checked = colibri_check::check(&ast);
        checked.diagnostics[0].hint.clone().unwrap_or_default()
    };
    let h = hint("let a = int(\"42\")\n");
    assert!(h.contains("str.parse_int(text)"), "{h}");
    let h = hint("let a = float(\"1.5\")\n");
    assert!(h.contains("str.parse_float(text)"), "{h}");
    let h = hint("let a = u8(\"1\")\n");
    assert!(h.contains("parse_int") && h.contains("`u8(..)`"), "{h}");
    // Other types keep the general hint.
    let h = hint("let a = int(true)\n");
    assert!(
        h.contains("converts between number types; this is a `bool`"),
        "{h}"
    );
}

/// The first diagnostic's message and hint.
fn first_diagnostic(src: &str) -> (String, String) {
    let (ast, _) = parse_file(FileId(0), src);
    let checked = colibri_check::check(&ast);
    let d = &checked.diagnostics[0];
    (d.message.clone(), d.hint.clone().unwrap_or_default())
}

#[test]
fn a_str_where_bytes_is_expected_says_how_to_convert() {
    let (message, hint) = first_diagnostic("fn f(b: Bytes) {}\nfn main() {\n    f(\"x\")\n}\n");
    assert_eq!(message, "expected `Bytes`, found `str`");
    assert!(hint.contains("use `s.to_bytes()`"), "{hint}");
    let (_, hint) = first_diagnostic("let b: Bytes = \"x\"\n");
    assert!(hint.contains("use `s.to_bytes()`"), "{hint}");
}

#[test]
fn bytes_where_a_str_is_expected_says_how_to_convert() {
    let (message, hint) = first_diagnostic("fn f(s: str) {}\nfn main() {\n    f(Bytes())\n}\n");
    assert_eq!(message, "expected `str`, found `Bytes`");
    assert!(
        hint.contains("use `b.to_str()`") && hint.contains("`b.to_str_lossy()`"),
        "{hint}"
    );
}

#[test]
fn assigning_to_a_bytes_element_says_it_is_immutable() {
    let (message, hint) =
        first_diagnostic("fn main() {\n    var b = Bytes([1, 2])\n    b[0] = 9\n}\n");
    assert_eq!(
        message,
        "cannot assign to an element of a `Bytes`: it is immutable"
    );
    assert!(hint.contains("BytesBuilder"), "{hint}");
}
