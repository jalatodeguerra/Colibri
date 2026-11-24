//! A compact, deterministic S-expression dump of the AST for tests and
//! debugging. Spans and node ids are left out.
//!
//! Examples: `(let x (binary + (int 1) (int 2)))`,
//! `(fn add (params (a : int) (b : int)) -> int (block (tail (binary + a b))))`.

use crate::ast::*;

/// Dumps a whole file, one top-level entry per line.
pub fn dump_file(file: &File) -> String {
    file.items
        .iter()
        .map(|top| match top {
            TopLevel::Item(item) => dump_item(item),
            TopLevel::Stmt(stmt) => dump_stmt(stmt),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn list(kind: &str, parts: Vec<String>) -> String {
    if parts.is_empty() {
        format!("({kind})")
    } else {
        format!("({kind} {})", parts.join(" "))
    }
}

fn label(l: &Option<Label>) -> Option<String> {
    l.as_ref().map(|l| format!("'{}", l.name))
}

// ---- items ----

pub fn dump_item(item: &Item) -> String {
    let mut modifiers = Vec::new();
    for attr in &item.attrs {
        let mut parts = vec![format!("@{}", attr.name.name)];
        parts.extend(attr.args.iter().map(dump_expr));
        modifiers.push(format!("({})", parts.join(" ")));
    }
    if let Some(doc) = &item.doc {
        modifiers.push(format!("(doc {doc:?})"));
    }
    if item.public {
        modifiers.push("pub".to_string());
    }
    let (kw, rest) = match &item.kind {
        ItemKind::Fn(f) => ("fn", dump_fn_parts(f)),
        ItemKind::Struct(s) => {
            let kw = if s.kind == StructKind::Class {
                "class"
            } else {
                "struct"
            };
            let mut rest = vec![s.name.name.clone()];
            rest.extend(generics(&s.generics));
            rest.extend(s.fields.iter().map(dump_field));
            (kw, rest)
        }
        ItemKind::Enum(e) => {
            let mut rest = vec![e.name.name.clone()];
            rest.extend(generics(&e.generics));
            rest.extend(e.variants.iter().map(dump_variant));
            ("enum", rest)
        }
        ItemKind::Interface(i) => {
            let mut rest = vec![i.name.name.clone()];
            rest.extend(generics(&i.generics));
            rest.extend(i.items.iter().map(dump_item));
            ("interface", rest)
        }
        ItemKind::Impl(i) => {
            let mut rest = Vec::new();
            rest.extend(generics(&i.generics));
            if let Some(iface) = &i.interface {
                rest.push(ty_str(iface));
                rest.push("for".to_string());
            }
            rest.push(ty_str(&i.target));
            rest.extend(i.items.iter().map(dump_item));
            ("impl", rest)
        }
        ItemKind::TypeAlias(t) => {
            let mut rest = vec![t.name.name.clone()];
            rest.extend(generics(&t.generics));
            rest.push("=".to_string());
            rest.push(ty_str(&t.ty));
            ("type", rest)
        }
        ItemKind::Binding(l) => (bind_kw(l.kind), let_parts(l)),
        ItemKind::Import(i) => {
            let path = i
                .path
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(".");
            let mut rest = vec![path];
            match &i.kind {
                ImportKind::Plain => {}
                ImportKind::Alias(a) => {
                    rest.push("as".to_string());
                    rest.push(a.name.clone());
                }
                ImportKind::Names(names) => {
                    let names: Vec<String> = names
                        .iter()
                        .map(|n| match &n.alias {
                            Some(a) => format!("{} as {}", n.name.name, a.name),
                            None => n.name.name.clone(),
                        })
                        .collect();
                    rest.push(format!("{{{}}}", names.join(", ")));
                }
            }
            ("import", rest)
        }
        ItemKind::Extern(e) => {
            let mut rest = vec![format!("{:?}", e.abi)];
            rest.extend(e.items.iter().map(dump_item));
            ("extern", rest)
        }
        ItemKind::Test(t) => ("test", vec![format!("{:?}", t.name), dump_block(&t.body)]),
        ItemKind::Error => ("error", Vec::new()),
    };
    modifiers.extend(rest);
    list(kw, modifiers)
}

fn bind_kw(kind: BindKind) -> &'static str {
    match kind {
        BindKind::Let => "let",
        BindKind::Var => "let mut",
        BindKind::Const => "const",
    }
}

fn generics(params: &[GenericParam]) -> Option<String> {
    if params.is_empty() {
        return None;
    }
    let parts: Vec<String> = params
        .iter()
        .map(|p| {
            let mut s = p.name.name.clone();
            if !p.bounds.is_empty() {
                let bounds: Vec<String> = p.bounds.iter().map(ty_str).collect();
                s = format!("{s}: {}", bounds.join(" + "));
            }
            if let Some(d) = &p.default {
                s = format!("{s} = {}", ty_str(d));
            }
            s
        })
        .collect();
    Some(format!("[{}]", parts.join(", ")))
}

fn dump_fn_parts(f: &FnDecl) -> Vec<String> {
    let mut rest = vec![f.name.name.clone()];
    rest.extend(generics(&f.generics));
    let mut params = Vec::new();
    if let Some(r) = &f.receiver {
        params.push(if r.mutable { "(mut self)" } else { "self" }.to_string());
    }
    params.extend(f.params.iter().map(dump_param));
    rest.push(list("params", params));
    if let Some(ret) = &f.ret {
        rest.push(format!("-> {}", ty_str(ret)));
    }
    if let Some(body) = &f.body {
        rest.push(dump_body(body));
    }
    rest
}

fn dump_body(body: &FnBody) -> String {
    match body {
        FnBody::Block(b) => dump_block(b),
        FnBody::Expr(e) => format!("(=> {})", dump_expr(e)),
    }
}

fn dump_param(p: &Param) -> String {
    let mut parts = Vec::new();
    if p.mutable {
        parts.push("mut".to_string());
    }
    parts.push(p.name.name.clone());
    if let Some(ty) = &p.ty {
        parts.push(":".to_string());
        parts.push(ty_str(ty));
    }
    if let Some(d) = &p.default {
        parts.push("=".to_string());
        parts.push(dump_expr(d));
    }
    if parts.len() == 1 {
        parts.remove(0)
    } else {
        format!("({})", parts.join(" "))
    }
}

fn dump_field(f: &Field) -> String {
    let mut parts = Vec::new();
    if let Some(doc) = &f.doc {
        parts.push(format!("(doc {doc:?})"));
    }
    for attr in &f.attrs {
        let mut inner = vec![format!("@{}", attr.name.name)];
        inner.extend(attr.args.iter().map(dump_expr));
        parts.push(format!("({})", inner.join(" ")));
    }
    if f.public {
        parts.push("pub".to_string());
    }
    parts.push(f.name.name.clone());
    parts.push(":".to_string());
    parts.push(ty_str(&f.ty));
    if let Some(d) = &f.default {
        parts.push("=".to_string());
        parts.push(dump_expr(d));
    }
    format!("({})", parts.join(" "))
}

fn dump_variant(v: &Variant) -> String {
    let mut parts = Vec::new();
    if let Some(doc) = &v.doc {
        parts.push(format!("(doc {doc:?})"));
    }
    parts.push(v.name.name.clone());
    match &v.payload {
        VariantPayload::Unit => {}
        VariantPayload::Positional(types) => parts.extend(types.iter().map(ty_str)),
        VariantPayload::Named(fields) => parts.extend(
            fields
                .iter()
                .map(|f| format!("({} : {})", f.name.name, ty_str(&f.ty))),
        ),
    }
    if parts.len() == 1 {
        parts.remove(0)
    } else {
        format!("({})", parts.join(" "))
    }
}

// ---- types ----

/// A type as source-like text: `List[T]`, `T?`, `fn(int) -> int`.
pub fn ty_str(ty: &TypeExpr) -> String {
    match &ty.kind {
        TypeKind::Named { path, args } => {
            let name = path
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(".");
            if args.is_empty() {
                name
            } else {
                let args: Vec<String> = args.iter().map(ty_str).collect();
                format!("{name}[{}]", args.join(", "))
            }
        }
        TypeKind::Optional(inner) => format!("{}?", ty_str(inner)),
        TypeKind::Weak(inner) => format!("weak {}", ty_str(inner)),
        TypeKind::Tuple(items) => {
            let items: Vec<String> = items.iter().map(ty_str).collect();
            if items.len() == 1 {
                format!("({},)", items[0])
            } else {
                format!("({})", items.join(", "))
            }
        }
        TypeKind::Array { elem, len } => format!("[{}; {}]", ty_str(elem), dump_expr(len)),
        TypeKind::Fn { params, ret } => {
            let params: Vec<String> = params.iter().map(ty_str).collect();
            match ret {
                Some(r) => format!("fn({}) -> {}", params.join(", "), ty_str(r)),
                None => format!("fn({})", params.join(", ")),
            }
        }
        TypeKind::Never => "!".to_string(),
        TypeKind::Error => "<error>".to_string(),
    }
}

// ---- statements ----

pub fn dump_block(b: &Block) -> String {
    let mut parts: Vec<String> = b.stmts.iter().map(dump_stmt).collect();
    if let Some(t) = &b.tail {
        parts.push(format!("(tail {})", dump_expr(t)));
    }
    list("block", parts)
}

fn let_parts(l: &LetStmt) -> Vec<String> {
    let mut parts = vec![dump_pat(&l.pat)];
    if let Some(ty) = &l.ty {
        parts.push(":".to_string());
        parts.push(ty_str(ty));
    }
    parts.push(dump_expr(&l.init));
    parts
}

fn assign_str(op: AssignOp) -> &'static str {
    match op {
        AssignOp::Assign => "=",
        AssignOp::Add => "+=",
        AssignOp::Sub => "-=",
        AssignOp::Mul => "*=",
        AssignOp::Div => "/=",
        AssignOp::Rem => "%=",
        AssignOp::BitAnd => "&=",
        AssignOp::BitOr => "|=",
        AssignOp::BitXor => "^=",
        AssignOp::Shl => "<<=",
        AssignOp::Shr => ">>=",
    }
}

pub fn dump_stmt(s: &Stmt) -> String {
    match &s.kind {
        StmtKind::Let(l) => list(bind_kw(l.kind), let_parts(l)),
        StmtKind::Assign { op, target, value } => list(
            "assign",
            vec![
                assign_str(*op).to_string(),
                dump_expr(target),
                dump_expr(value),
            ],
        ),
        StmtKind::Expr(e) => dump_expr(e),
        StmtKind::Return(v) => list("return", v.iter().map(dump_expr).collect()),
        StmtKind::Break { label: l, value } => {
            let mut parts: Vec<String> = label(l).into_iter().collect();
            parts.extend(value.iter().map(dump_expr));
            list("break", parts)
        }
        StmtKind::Continue { label: l } => list("continue", label(l).into_iter().collect()),
        StmtKind::Defer(inner) => list("defer", vec![dump_stmt(inner)]),
        StmtKind::For {
            label: l,
            mutable,
            pat,
            iter,
            body,
        } => {
            let mut parts: Vec<String> = label(l).into_iter().collect();
            if *mutable {
                parts.push("mut".to_string());
            }
            parts.push(dump_pat(pat));
            parts.push(dump_expr(iter));
            parts.push(dump_block(body));
            list("for", parts)
        }
        StmtKind::While {
            label: l,
            cond,
            body,
        } => {
            let mut parts: Vec<String> = label(l).into_iter().collect();
            parts.push(dump_expr(cond));
            parts.push(dump_block(body));
            list("while", parts)
        }
        StmtKind::WhileLet {
            label: l,
            pat,
            scrutinee,
            body,
        } => {
            let mut parts: Vec<String> = label(l).into_iter().collect();
            parts.push(dump_pat(pat));
            parts.push(dump_expr(scrutinee));
            parts.push(dump_block(body));
            list("while-let", parts)
        }
        StmtKind::Error => "(error)".to_string(),
    }
}

// ---- expressions ----

fn dump_arg(a: &Arg) -> String {
    match &a.name {
        Some(n) => format!("{}={}", n.name, dump_expr(&a.value)),
        None => dump_expr(&a.value),
    }
}

fn dump_opt(e: &Option<Box<Expr>>) -> String {
    e.as_ref().map_or("_".to_string(), |e| dump_expr(e))
}

fn unop_str(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "!",
        UnOp::BitNot => "~",
    }
}

fn binop_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::Is => "is",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::BitAnd => "&",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
    }
}

fn else_parts(else_: &Option<Box<Expr>>) -> Vec<String> {
    else_.iter().map(|e| dump_expr(e)).collect()
}

pub fn dump_expr(e: &Expr) -> String {
    match &e.kind {
        ExprKind::Int(n) => format!("(int {n})"),
        ExprKind::Float(x) => format!("(float {x:?})"),
        ExprKind::Char(c) => format!("(char {c:?})"),
        ExprKind::Bool(b) => format!("(bool {b})"),
        ExprKind::None => "none".to_string(),
        ExprKind::Str(segs) => list(
            "str",
            segs.iter()
                .map(|seg| match seg {
                    StrSeg::Lit(s) => format!("{s:?}"),
                    StrSeg::Interp { expr, spec } => {
                        let mut parts = vec![dump_expr(expr)];
                        parts.extend(spec.iter().map(|s| format!("{s:?}")));
                        list("interp", parts)
                    }
                })
                .collect(),
        ),
        ExprKind::List(items) => list("list", items.iter().map(dump_expr).collect()),
        ExprKind::Map(entries) => list(
            "map",
            entries
                .iter()
                .map(|(k, v)| format!("({} {})", dump_expr(k), dump_expr(v)))
                .collect(),
        ),
        ExprKind::Tuple(items) => list("tuple", items.iter().map(dump_expr).collect()),
        ExprKind::Paren(inner) => list("paren", vec![dump_expr(inner)]),
        ExprKind::Ident(id) => id.name.clone(),
        ExprKind::SelfValue => "self".to_string(),
        ExprKind::DotVariant { name, args } => {
            let mut parts = vec![name.name.clone()];
            match args {
                Some(args) if args.is_empty() => parts.push("()".to_string()),
                Some(args) => parts.extend(args.iter().map(dump_arg)),
                None => {}
            }
            list("dot", parts)
        }
        ExprKind::StructLit { ty, fields } => {
            let mut parts = vec![ty_str(ty)];
            parts.extend(fields.iter().map(|f| {
                if f.shorthand {
                    f.name.name.clone()
                } else {
                    format!("({} {})", f.name.name, dump_expr(&f.value))
                }
            }));
            list("struct", parts)
        }
        ExprKind::Unary { op, expr } => {
            list("unary", vec![unop_str(*op).to_string(), dump_expr(expr)])
        }
        ExprKind::Binary { op, lhs, rhs } => list(
            "binary",
            vec![binop_str(*op).to_string(), dump_expr(lhs), dump_expr(rhs)],
        ),
        ExprKind::Range {
            start,
            end,
            inclusive,
        } => list(
            "range",
            vec![
                if *inclusive { "..=" } else { ".." }.to_string(),
                dump_opt(start),
                dump_opt(end),
            ],
        ),
        ExprKind::Cast { expr, ty } => list("as", vec![dump_expr(expr), ty_str(ty)]),
        ExprKind::Or { lhs, rhs } => list("or", vec![dump_expr(lhs), dump_expr(rhs)]),
        ExprKind::OrHandler {
            lhs,
            binding,
            handler,
        } => list(
            "or-handler",
            vec![
                dump_expr(lhs),
                binding
                    .as_ref()
                    .map_or("_".to_string(), |b| format!("|{}|", b.name)),
                dump_block(handler),
            ],
        ),
        ExprKind::Try(inner) => list("try", vec![dump_expr(inner)]),
        ExprKind::Go(call) => list("go", vec![dump_expr(call)]),
        ExprKind::Call { callee, args } => {
            let mut parts = vec![dump_expr(callee)];
            parts.extend(args.iter().map(dump_arg));
            list("call", parts)
        }
        ExprKind::Field { base, name } => list("field", vec![dump_expr(base), name.name.clone()]),
        ExprKind::TupleIndex { base, index } => {
            list("tindex", vec![dump_expr(base), index.to_string()])
        }
        ExprKind::Index { base, args } => {
            let mut parts = vec![dump_expr(base)];
            parts.extend(args.iter().map(|a| match a {
                IndexArg::Expr(e) => dump_expr(e),
                IndexArg::Type(t) => format!("(ty {})", ty_str(t)),
            }));
            list("index", parts)
        }
        ExprKind::If { cond, then, else_ } => {
            let mut parts = vec![dump_expr(cond), dump_block(then)];
            parts.extend(else_parts(else_));
            list("if", parts)
        }
        ExprKind::IfLet {
            pat,
            scrutinee,
            then,
            else_,
        } => {
            let mut parts = vec![dump_pat(pat), dump_expr(scrutinee), dump_block(then)];
            parts.extend(else_parts(else_));
            list("if-let", parts)
        }
        ExprKind::Match { scrutinee, arms } => {
            let mut parts = vec![dump_expr(scrutinee)];
            parts.extend(arms.iter().map(|arm| {
                let mut p = vec![dump_pat(&arm.pat)];
                if let Some(g) = &arm.guard {
                    p.push(list("if", vec![dump_expr(g)]));
                }
                p.push(match &arm.body {
                    ArmBody::Expr(e) => dump_expr(e),
                    ArmBody::Stmt(s) => dump_stmt(s),
                });
                list("arm", p)
            }));
            list("match", parts)
        }
        ExprKind::Closure(c) => {
            let mut parts = vec![list("params", c.params.iter().map(dump_param).collect())];
            if let Some(ret) = &c.ret {
                parts.push(format!("-> {}", ty_str(ret)));
            }
            if !c.captures.is_empty() {
                let caps = c
                    .captures
                    .iter()
                    .map(|cap| {
                        if cap.weak {
                            format!("weak {}", cap.name.name)
                        } else {
                            cap.name.name.clone()
                        }
                    })
                    .collect();
                parts.push(list("use", caps));
            }
            parts.push(dump_body(&c.body));
            list("closure", parts)
        }
        ExprKind::Unsafe(b) => list("unsafe", vec![dump_block(b)]),
        ExprKind::Block(b) => dump_block(b),
        ExprKind::Error => "(error)".to_string(),
    }
}

// ---- patterns ----

fn lit_str(l: &PatLit) -> String {
    let sign = if l.neg { "-" } else { "" };
    match &l.lit {
        Lit::Int(n) => format!("{sign}{n}"),
        Lit::Float(x) => format!("{sign}{x:?}"),
        Lit::Char(c) => format!("{c:?}"),
        Lit::Bool(b) => b.to_string(),
        Lit::Str(s) => format!("{s:?}"),
    }
}

fn path_str(p: &PatPath) -> String {
    let path = p
        .segments
        .iter()
        .map(|s| s.name.as_str())
        .collect::<Vec<_>>()
        .join(".");
    if p.leading_dot {
        format!(".{path}")
    } else {
        path
    }
}

pub fn dump_pat(p: &Pat) -> String {
    match &p.kind {
        PatKind::Wild => "_".to_string(),
        PatKind::Ident(id) => id.name.clone(),
        PatKind::Lit(l) => list("lit", vec![lit_str(l)]),
        PatKind::Range { lo, hi, inclusive } => list(
            "range",
            vec![
                if *inclusive { "..=" } else { ".." }.to_string(),
                lo.as_ref().map_or("_".to_string(), lit_str),
                hi.as_ref().map_or("_".to_string(), lit_str),
            ],
        ),
        PatKind::Variant { path, args } => match args {
            Some(args) => {
                let mut parts = vec![path_str(path)];
                parts.extend(args.iter().map(dump_pat));
                list("variant", parts)
            }
            None => list("path", vec![path_str(path)]),
        },
        PatKind::Some(inner) => list("some", vec![dump_pat(inner)]),
        PatKind::None => "none".to_string(),
        PatKind::Ok(inner) => list("ok", vec![dump_pat(inner)]),
        PatKind::Err(inner) => list("err", vec![dump_pat(inner)]),
        PatKind::Tuple(items) => list("tuple", items.iter().map(dump_pat).collect()),
        PatKind::Struct { path, fields, rest } => {
            let mut parts = vec![path_str(path)];
            parts.extend(fields.iter().map(|f| {
                if f.shorthand {
                    f.name.name.clone()
                } else {
                    format!("({} {})", f.name.name, dump_pat(&f.pat))
                }
            }));
            if *rest {
                parts.push("..".to_string());
            }
            list("struct", parts)
        }
        PatKind::Or(alts) => list("or", alts.iter().map(dump_pat).collect()),
        PatKind::Bind { pat, name } => list("bind", vec![dump_pat(pat), name.name.clone()]),
        PatKind::Error => "(error)".to_string(),
    }
}
