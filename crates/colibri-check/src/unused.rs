//! The unused-`import` warning. A module's imports bind names; an import is unused when none
//! of the names it binds appears anywhere else in the module. The test is syntactic (a local
//! variable that shadows an imported name counts as a use), so it can miss an unused import
//! but never reports a used one.

use std::collections::HashSet;

use colibri_syntax::Diagnostic;
use colibri_syntax::ast::{
    Arg, ArmBody, Block, Closure, Expr, ExprKind, File, FnBody, GenericParam, ImportKind, IndexArg,
    Item, ItemKind, LetStmt, NodeId, Pat, PatKind, Stmt, StmtKind, StrSeg, TopLevel, TypeExpr,
    TypeKind, VariantPayload,
};

use crate::checker::Checker;
use crate::modules::{ModInfo, ModKind};
use crate::package::ImportTarget;

/// Every name that is written in the code of `file`, outside its `import` items.
#[derive(Default)]
struct Mentions(HashSet<String>);

impl Mentions {
    fn name(&mut self, s: &str) {
        if !self.0.contains(s) {
            self.0.insert(s.to_string());
        }
    }

    fn file(&mut self, f: &File) {
        for top in &f.items {
            match top {
                TopLevel::Item(i) => self.item(i),
                TopLevel::Stmt(s) => self.stmt(s),
            }
        }
    }

    fn generics(&mut self, gs: &[GenericParam]) {
        for g in gs {
            for b in &g.bounds {
                self.ty(b);
            }
        }
    }

    fn item(&mut self, item: &Item) {
        for a in &item.attrs {
            for e in &a.args {
                self.expr(e);
            }
        }
        match &item.kind {
            ItemKind::Fn(f) => {
                self.generics(&f.generics);
                for p in &f.params {
                    self.param(p);
                }
                if let Some(r) = &f.ret {
                    self.ty(r);
                }
                match &f.body {
                    Some(FnBody::Block(b)) => self.block(b),
                    Some(FnBody::Expr(e)) => self.expr(e),
                    None => {}
                }
            }
            ItemKind::Struct(s) => {
                self.generics(&s.generics);
                for f in &s.fields {
                    self.ty(&f.ty);
                    if let Some(d) = &f.default {
                        self.expr(d);
                    }
                }
            }
            ItemKind::Enum(e) => {
                self.generics(&e.generics);
                for v in &e.variants {
                    match &v.payload {
                        VariantPayload::Unit => {}
                        VariantPayload::Positional(ts) => ts.iter().for_each(|t| self.ty(t)),
                        VariantPayload::Named(fs) => fs.iter().for_each(|f| self.ty(&f.ty)),
                    }
                }
            }
            ItemKind::Interface(i) => {
                self.generics(&i.generics);
                i.items.iter().for_each(|i| self.item(i));
            }
            ItemKind::Impl(i) => {
                self.generics(&i.generics);
                if let Some(t) = &i.interface {
                    self.ty(t);
                }
                self.ty(&i.target);
                i.items.iter().for_each(|i| self.item(i));
            }
            ItemKind::TypeAlias(a) => {
                self.generics(&a.generics);
                self.ty(&a.ty);
            }
            ItemKind::Binding(l) => self.let_stmt(l),
            ItemKind::Import(_) | ItemKind::Error => {}
            ItemKind::Extern(x) => x.items.iter().for_each(|i| self.item(i)),
            ItemKind::Test(t) => self.block(&t.body),
        }
    }

    fn param(&mut self, p: &colibri_syntax::ast::Param) {
        if let Some(t) = &p.ty {
            self.ty(t);
        }
        if let Some(d) = &p.default {
            self.expr(d);
        }
    }

    fn ty(&mut self, t: &TypeExpr) {
        match &t.kind {
            TypeKind::Named { path, args } => {
                for seg in path {
                    self.name(&seg.name);
                }
                args.iter().for_each(|a| self.ty(a));
            }
            TypeKind::Optional(t) | TypeKind::Weak(t) => self.ty(t),
            TypeKind::Tuple(ts) => ts.iter().for_each(|t| self.ty(t)),
            TypeKind::Array { elem, len } => {
                self.ty(elem);
                self.expr(len);
            }
            TypeKind::Fn { params, ret } => {
                params.iter().for_each(|t| self.ty(t));
                if let Some(r) = ret {
                    self.ty(r);
                }
            }
            TypeKind::Never | TypeKind::Error => {}
        }
    }

    fn block(&mut self, b: &Block) {
        b.stmts.iter().for_each(|s| self.stmt(s));
        if let Some(t) = &b.tail {
            self.expr(t);
        }
    }

    fn let_stmt(&mut self, l: &LetStmt) {
        self.pat(&l.pat);
        if let Some(t) = &l.ty {
            self.ty(t);
        }
        self.expr(&l.init);
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let(l) => self.let_stmt(l),
            StmtKind::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Return(e) => {
                if let Some(e) = e {
                    self.expr(e);
                }
            }
            StmtKind::Break { value, .. } => {
                if let Some(e) = value {
                    self.expr(e);
                }
            }
            StmtKind::Continue { .. } | StmtKind::Error => {}
            StmtKind::Defer(s) => self.stmt(s),
            StmtKind::For {
                pat, iter, body, ..
            } => {
                self.pat(pat);
                self.expr(iter);
                self.block(body);
            }
            StmtKind::While { cond, body, .. } => {
                self.expr(cond);
                self.block(body);
            }
            StmtKind::WhileLet {
                pat,
                scrutinee,
                body,
                ..
            } => {
                self.pat(pat);
                self.expr(scrutinee);
                self.block(body);
            }
        }
    }

    fn args(&mut self, args: &[Arg]) {
        args.iter().for_each(|a| self.expr(&a.value));
    }

    fn closure(&mut self, c: &Closure) {
        for cap in &c.captures {
            self.name(&cap.name.name);
        }
        for p in &c.params {
            self.param(p);
        }
        if let Some(r) = &c.ret {
            self.ty(r);
        }
        match &*c.body {
            FnBody::Block(b) => self.block(b),
            FnBody::Expr(e) => self.expr(e),
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Char(_)
            | ExprKind::Bool(_)
            | ExprKind::None
            | ExprKind::SelfValue
            | ExprKind::TupleIndex { .. }
            | ExprKind::Error => {
                if let ExprKind::TupleIndex { base, .. } = &e.kind {
                    self.expr(base);
                }
            }
            ExprKind::Str(segs) => {
                for s in segs {
                    if let StrSeg::Interp { expr, .. } = s {
                        self.expr(expr);
                    }
                }
            }
            ExprKind::List(es) | ExprKind::Tuple(es) => es.iter().for_each(|e| self.expr(e)),
            ExprKind::Map(kvs) => {
                for (k, v) in kvs {
                    self.expr(k);
                    self.expr(v);
                }
            }
            ExprKind::Paren(e)
            | ExprKind::Try(e)
            | ExprKind::Go(e)
            | ExprKind::Unary { expr: e, .. } => self.expr(e),
            ExprKind::Ident(id) => self.name(&id.name),
            ExprKind::DotVariant { args, .. } => {
                if let Some(a) = args {
                    self.args(a);
                }
            }
            ExprKind::StructLit { ty, fields } => {
                self.ty(ty);
                fields.iter().for_each(|f| self.expr(&f.value));
            }
            ExprKind::Binary { lhs, rhs, .. } | ExprKind::Or { lhs, rhs } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.expr(s);
                }
                if let Some(e) = end {
                    self.expr(e);
                }
            }
            ExprKind::Cast { expr, ty } => {
                self.expr(expr);
                self.ty(ty);
            }
            ExprKind::OrHandler { lhs, handler, .. } => {
                self.expr(lhs);
                self.block(handler);
            }
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                self.args(args);
            }
            ExprKind::Field { base, .. } => self.expr(base),
            ExprKind::Index { base, args } => {
                self.expr(base);
                for a in args {
                    match a {
                        IndexArg::Expr(e) => self.expr(e),
                        IndexArg::Type(t) => self.ty(t),
                    }
                }
            }
            ExprKind::If { cond, then, else_ } => {
                self.expr(cond);
                self.block(then);
                if let Some(e) = else_ {
                    self.expr(e);
                }
            }
            ExprKind::IfLet {
                pat,
                scrutinee,
                then,
                else_,
            } => {
                self.pat(pat);
                self.expr(scrutinee);
                self.block(then);
                if let Some(e) = else_ {
                    self.expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for a in arms {
                    self.pat(&a.pat);
                    if let Some(g) = &a.guard {
                        self.expr(g);
                    }
                    match &a.body {
                        ArmBody::Expr(e) => self.expr(e),
                        ArmBody::Stmt(s) => self.stmt(s),
                    }
                }
            }
            ExprKind::Closure(c) => self.closure(c),
            ExprKind::Unsafe(b) | ExprKind::Block(b) => self.block(b),
        }
    }

    fn pat(&mut self, p: &Pat) {
        match &p.kind {
            PatKind::Wild
            | PatKind::Lit(_)
            | PatKind::Range { .. }
            | PatKind::None
            | PatKind::Error => {}
            // A bare name may be a unit variant or a constant, so it counts as a mention.
            PatKind::Ident(id) => self.name(&id.name),
            PatKind::Variant { path, args } => {
                path.segments.iter().for_each(|s| self.name(&s.name));
                if let Some(a) = args {
                    a.iter().for_each(|p| self.pat(p));
                }
            }
            PatKind::Some(p) | PatKind::Ok(p) | PatKind::Err(p) => self.pat(p),
            PatKind::Tuple(ps) | PatKind::Or(ps) => ps.iter().for_each(|p| self.pat(p)),
            PatKind::Struct { path, fields, .. } => {
                path.segments.iter().for_each(|s| self.name(&s.name));
                fields.iter().for_each(|f| self.pat(&f.pat));
            }
            PatKind::Bind { pat, .. } => self.pat(pat),
        }
    }
}

/// Whether evaluating an initialiser can do anything but build a constant.
fn is_plain_literal(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Char(_)
        | ExprKind::Bool(_)
        | ExprKind::None => true,
        ExprKind::Str(segs) => segs.iter().all(|s| matches!(s, StrSeg::Lit(_))),
        ExprKind::Paren(e) => is_plain_literal(e),
        ExprKind::Unary { expr, .. } => is_plain_literal(expr),
        ExprKind::List(es) | ExprKind::Tuple(es) => es.iter().all(is_plain_literal),
        ExprKind::Map(kvs) => kvs
            .iter()
            .all(|(k, v)| is_plain_literal(k) && is_plain_literal(v)),
        _ => false,
    }
}

impl Checker<'_> {
    /// Whether the module an `import` item names runs code when it loads: a top-level
    /// statement, or a `let`/`var` whose initialiser is not a plain literal.
    fn import_has_load_effects(&self, importer: &ModInfo<'_>, import_item: NodeId) -> bool {
        let Some(ImportTarget::Module(i)) = importer.imports.and_then(|m| m.get(&import_item))
        else {
            return false;
        };
        let target = &self.mods[self.source_base as usize + *i];
        target.files.iter().any(|f| {
            f.items.iter().any(|top| match top {
                TopLevel::Stmt(_) => true,
                TopLevel::Item(item) => {
                    matches!(&item.kind, ItemKind::Binding(b) if !is_plain_literal(&b.init))
                }
            })
        })
    }

    /// Warns about the imports of the program's own modules that bind nothing used. Only when
    /// the program has no errors: a name that failed to resolve would give a wrong answer.
    pub(crate) fn lint_unused_imports(&mut self) {
        if self.error_count() > 0 {
            return;
        }
        let mut found = Vec::new();
        for info in &self.mods {
            if info.kind != ModKind::Source || !info.root {
                continue;
            }
            for file in &info.files {
                let mut used = Mentions::default();
                used.file(file);
                for top in &file.items {
                    let TopLevel::Item(item) = top else { continue };
                    let ItemKind::Import(import) = &item.kind else {
                        continue;
                    };
                    let path = import
                        .path
                        .iter()
                        .map(|i| i.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    match &import.kind {
                        ImportKind::Plain | ImportKind::Alias(_) => {
                            let bound = match &import.kind {
                                ImportKind::Alias(a) => a,
                                _ => match import.path.last() {
                                    Some(l) => l,
                                    None => continue,
                                },
                            };
                            if !bound.name.starts_with('_') && !used.0.contains(&bound.name) {
                                let mut hint = format!(
                                    "nothing named `{}` is used; remove the import",
                                    bound.name
                                );
                                if self.import_has_load_effects(info, item.id) {
                                    hint.push_str(&format!(
                                        ", or keep it for what the module does when it loads: `import {path} as _{}`",
                                        bound.name
                                    ));
                                }
                                found.push(
                                    Diagnostic::warning(
                                        format!("unused import `{path}`"),
                                        item.span,
                                    )
                                    .with_hint(hint),
                                );
                            }
                        }
                        ImportKind::Names(names) => {
                            for n in names {
                                let bound = n.alias.as_ref().unwrap_or(&n.name);
                                if !bound.name.starts_with('_') && !used.0.contains(&bound.name) {
                                    found.push(
                                        Diagnostic::warning(
                                            format!("unused import `{}` from `{path}`", bound.name),
                                            bound.span,
                                        )
                                        .with_hint("remove it from the import list"),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        for d in found {
            self.report_lint(d);
        }
    }
}
