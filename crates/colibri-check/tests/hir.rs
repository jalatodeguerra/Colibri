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
    let src = "fn f() {\n    let x = 1\n    let x = 2\n}\n";
    let (ast, _) = parse_file(FileId(0), src);
    let checked = colibri_check::check(&ast);
    assert!(checked.program.is_some());
    assert_eq!(checked.diagnostics.len(), 1);
    assert_eq!(checked.diagnostics[0].severity, Severity::Warning);
    assert_eq!(checked.diagnostics[0].labels.len(), 1);
}

#[test]
fn unknown_names_get_suggestions() {
    let src = "fn main() {\n    let counter = 1\n    print(countr)\n}\n";
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
