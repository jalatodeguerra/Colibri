//! The checker's module-level state and its phases: collect every declaration (so order does
//! not matter), check each body, order the module-level initialisers, assemble the program.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use colibri_syntax::ast::{self, BindKind, ItemKind, PatKind, TopLevel, TypeKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::hir::{Body, FnDef, FnId, GlobalDef, GlobalId, Program};
use crate::suggest;
use crate::types::{FnSig, Ty};

/// What a module-level name refers to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ValueRef {
    Fn(FnId),
    Global(GlobalId),
}

/// A reference from one body to a module-level item, used to order initialisers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Dep {
    Fn(FnId),
    Global(GlobalId),
}

#[derive(Clone)]
pub(crate) struct ParamInfo<'a> {
    pub name: String,
    pub ty: Ty,
    pub span: Span,
    pub default: Option<&'a ast::Expr>,
}

pub(crate) struct FnInfo<'a> {
    pub name: String,
    pub name_span: Span,
    pub span: Span,
    pub params: Vec<ParamInfo<'a>>,
    pub ret: Ty,
    pub ret_span: Option<Span>,
    /// `None` when the declaration uses something unsupported: calls are then checked loosely.
    pub decl: Option<&'a ast::FnDecl>,
    pub body: Option<Rc<Body>>,
    pub deps: Vec<Dep>,
}

impl FnInfo<'_> {
    pub fn is_poisoned(&self) -> bool {
        self.decl.is_none()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GlobalState {
    Unchecked,
    InProgress,
    Done,
}

pub(crate) struct GlobalInfo<'a> {
    pub name: String,
    pub name_span: Span,
    pub span: Span,
    pub mutable: bool,
    pub kind: BindKind,
    pub decl: &'a ast::LetStmt,
    /// The declared type, or the inferred one once the initialiser has been checked.
    pub ty: Option<Ty>,
    pub annotation_span: Option<Span>,
    state: GlobalState,
    pub body: Option<Rc<Body>>,
    pub deps: Vec<Dep>,
}

/// The result of checking a file.
#[derive(Debug)]
pub struct Checked {
    /// The program, present only when there are no errors.
    pub program: Option<Program>,
    /// Errors and warnings, ordered by position.
    pub diagnostics: Vec<Diagnostic>,
}

pub(crate) struct Checker<'a> {
    file: &'a ast::File,
    diags: Vec<Diagnostic>,
    /// While positive, diagnostics are dropped (re-checking a default argument at a call).
    muted: u32,
    pub values: HashMap<String, ValueRef>,
    /// Type names that exist but cannot be used yet (they were declared with an unsupported
    /// construct), so uses of them do not cascade into "unknown type".
    poisoned_types: HashSet<String>,
    pub fns: Vec<FnInfo<'a>>,
    pub globals: Vec<GlobalInfo<'a>>,
    script: Vec<&'a ast::Stmt>,
    script_body: Option<Rc<Body>>,
    /// The native stack position where checking started, and how far below it checking may go.
    stack_base: usize,
    stack_budget: usize,
}

/// A position on the native stack, comparable with another one.
pub(crate) fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// Names that exist without a declaration.
pub(crate) const BUILTIN_FNS: &[&str] = &["print"];

impl<'a> Checker<'a> {
    pub fn new(file: &'a ast::File, stack_budget: usize) -> Self {
        Checker {
            file,
            diags: Vec::new(),
            muted: 0,
            values: HashMap::new(),
            poisoned_types: HashSet::new(),
            fns: Vec::new(),
            globals: Vec::new(),
            script: Vec::new(),
            script_body: None,
            stack_base: 0,
            stack_budget,
        }
    }

    /// Whether checking has used up its share of the native stack.
    pub fn out_of_stack(&self) -> bool {
        stack_position().abs_diff(self.stack_base) > self.stack_budget
    }

    pub fn run(mut self) -> Checked {
        self.stack_base = stack_position();
        self.collect();
        for g in 0..self.globals.len() {
            self.ensure_global(GlobalId(g as u32));
        }
        self.check_defaults();
        for f in 0..self.fns.len() {
            self.check_fn(FnId(f as u32));
        }
        self.check_script();
        let init_order = self.init_order();
        let main = self.check_main();
        self.diags.sort_by_key(|d| (d.span.file.0, d.span.start));
        let has_errors = self.diags.iter().any(Diagnostic::is_error);
        let program = if has_errors {
            None
        } else {
            self.assemble(init_order, main)
        };
        Checked {
            program,
            diagnostics: self.diags,
        }
    }

    // ---- diagnostics ----

    pub fn report(&mut self, diag: Diagnostic) {
        if self.muted == 0 {
            self.diags.push(diag);
        }
    }

    pub fn mute(&mut self) {
        self.muted += 1;
    }

    pub fn unmute(&mut self) {
        self.muted -= 1;
    }

    /// Reports a construct the current slice does not implement.
    pub fn unsupported(&mut self, what: &str, span: Span) {
        let diag = Diagnostic::error(format!("{what} is not supported yet"), span)
            .with_hint("Colibri is being built in slices; see docs/PHASE1.md for what comes next");
        self.report(diag);
    }

    // ---- collecting declarations ----

    fn collect(&mut self) {
        let file = self.file;
        for top in &file.items {
            match top {
                TopLevel::Item(item) => self.collect_item(item),
                TopLevel::Stmt(stmt) => self.script.push(stmt),
            }
        }
    }

    fn collect_item(&mut self, item: &'a ast::Item) {
        for attr in &item.attrs {
            self.unsupported(&format!("the attribute `@{}`", attr.name.name), attr.span);
        }
        match &item.kind {
            ItemKind::Fn(decl) => self.collect_fn(item, decl),
            ItemKind::Binding(binding) => self.collect_global(item, binding),
            ItemKind::Struct(s) => {
                let what = match s.kind {
                    ast::StructKind::Struct => "`struct`",
                    ast::StructKind::Class => "`class`",
                };
                self.poisoned_types.insert(s.name.name.clone());
                self.unsupported(what, s.name.span);
            }
            ItemKind::Enum(e) => {
                self.poisoned_types.insert(e.name.name.clone());
                self.unsupported("`enum`", e.name.span);
            }
            ItemKind::Interface(i) => {
                self.poisoned_types.insert(i.name.name.clone());
                self.unsupported("`interface`", i.name.span);
            }
            ItemKind::TypeAlias(t) => {
                self.poisoned_types.insert(t.name.name.clone());
                self.unsupported("`type` alias", t.name.span);
            }
            ItemKind::Impl(_) => self.unsupported("`impl`", item.span),
            ItemKind::Import(_) => self.unsupported("`import`", item.span),
            ItemKind::Extern(_) => self.unsupported("`extern`", item.span),
            ItemKind::Test(_) => self.unsupported("`test`", item.span),
            ItemKind::Error => {}
        }
    }

    /// Declares a module-level name, reporting a clash.
    fn declare_value(&mut self, name: &str, span: Span, value: ValueRef) -> bool {
        if BUILTIN_FNS.contains(&name) {
            self.report(Diagnostic::error(
                format!("`{name}` is a built-in function and cannot be redefined"),
                span,
            ));
            return false;
        }
        if let Some(prev) = self.values.get(name).copied() {
            let prev_span = match prev {
                ValueRef::Fn(f) => self.fns[f.0 as usize].name_span,
                ValueRef::Global(g) => self.globals[g.0 as usize].name_span,
            };
            self.report(
                Diagnostic::error(format!("`{name}` is defined more than once"), span)
                    .with_label(prev_span, "first defined here"),
            );
            return false;
        }
        self.values.insert(name.to_string(), value);
        true
    }

    fn collect_fn(&mut self, item: &'a ast::Item, decl: &'a ast::FnDecl) {
        let mut poisoned = false;
        if let Some(receiver) = &decl.receiver {
            self.unsupported("method declarations", receiver.span);
            poisoned = true;
        }
        if !decl.generics.is_empty() {
            self.unsupported("generic function", decl.generics[0].name.span);
            poisoned = true;
        }
        // Type parameters are unusable for now; naming them keeps uses from cascading.
        let generic_names: Vec<String> = decl
            .generics
            .iter()
            .map(|g| g.name.name.clone())
            .filter(|n| self.poisoned_types.insert(n.clone()))
            .collect();
        let mut params: Vec<ParamInfo<'a>> = Vec::new();
        for p in &decl.params {
            if params.iter().any(|q| q.name == p.name.name) {
                self.report(Diagnostic::error(
                    format!("the parameter `{}` is declared more than once", p.name.name),
                    p.name.span,
                ));
            }
            let ty = match &p.ty {
                Some(t) => self.resolve_type(t),
                None => {
                    self.report(
                        Diagnostic::error(
                            format!("the parameter `{}` needs a type", p.name.name),
                            p.name.span,
                        )
                        .with_hint(format!("write `{}: int`, for example", p.name.name)),
                    );
                    Ty::Error
                }
            };
            if p.mutable {
                self.unsupported("`var` parameter", p.span);
            }
            params.push(ParamInfo {
                name: p.name.name.clone(),
                ty,
                span: p.name.span,
                default: p.default.as_ref(),
            });
        }
        let ret = match &decl.ret {
            Some(t) => self.resolve_type(t),
            None => Ty::Unit,
        };
        let ret_span = decl.ret.as_ref().map(|t| t.span);
        for name in &generic_names {
            self.poisoned_types.remove(name);
        }
        if decl.body.is_none() {
            poisoned = true;
        }
        let id = FnId(self.fns.len() as u32);
        self.fns.push(FnInfo {
            name: decl.name.name.clone(),
            name_span: decl.name.span,
            span: item.span,
            params,
            ret,
            ret_span,
            decl: if poisoned { None } else { Some(decl) },
            body: None,
            deps: Vec::new(),
        });
        // A clash keeps the later declaration out of the name table but still checks its body.
        self.declare_value(&decl.name.name, decl.name.span, ValueRef::Fn(id));
    }

    fn collect_global(&mut self, item: &'a ast::Item, binding: &'a ast::LetStmt) {
        let PatKind::Ident(name) = &binding.pat.kind else {
            self.unsupported("a pattern in a module-level binding", binding.pat.span);
            return;
        };
        let (ty, annotation_span) = match &binding.ty {
            Some(t) => (Some(self.resolve_type(t)), Some(t.span)),
            None => (None, None),
        };
        let id = GlobalId(self.globals.len() as u32);
        self.globals.push(GlobalInfo {
            name: name.name.clone(),
            name_span: name.span,
            span: item.span,
            mutable: binding.kind == BindKind::Var,
            kind: binding.kind,
            decl: binding,
            ty,
            annotation_span,
            state: GlobalState::Unchecked,
            body: None,
            deps: Vec::new(),
        });
        self.declare_value(&name.name, name.span, ValueRef::Global(id));
    }

    // ---- types ----

    /// Resolves a type written in source.
    pub fn resolve_type(&mut self, t: &ast::TypeExpr) -> Ty {
        match &t.kind {
            TypeKind::Named { path, args } => {
                if path.len() != 1 {
                    self.unsupported("qualified type name", t.span);
                    return Ty::Error;
                }
                let name = path[0].name.as_str();
                if !args.is_empty() {
                    self.unsupported(&format!("the generic type `{name}[..]`"), t.span);
                    return Ty::Error;
                }
                match name {
                    "int" | "i64" => Ty::INT,
                    "float" | "f64" => Ty::Float,
                    "bool" => Ty::Bool,
                    "str" => Ty::Str,
                    "char" => Ty::Char,
                    "i8" | "i16" | "i32" | "u8" | "u16" | "u32" | "u64" | "f32" | "vec2"
                    | "vec3" | "vec4" | "List" | "Map" | "Set" | "Option" | "Result" => {
                        self.unsupported(&format!("the type `{name}`"), t.span);
                        Ty::Error
                    }
                    _ if self.poisoned_types.contains(name) => Ty::Error,
                    _ => {
                        let known = ["int", "float", "bool", "str", "char", "i64", "f64"];
                        let mut diag = Diagnostic::error(format!("unknown type `{name}`"), t.span);
                        if let Some(s) = suggest::closest(
                            name,
                            known
                                .into_iter()
                                .chain(self.poisoned_types.iter().map(String::as_str)),
                        ) {
                            diag = diag.with_hint(format!("did you mean `{s}`?"));
                        }
                        self.report(diag);
                        Ty::Error
                    }
                }
            }
            TypeKind::Tuple(items) if items.is_empty() => Ty::Unit,
            TypeKind::Tuple(_) => {
                self.unsupported("tuple type", t.span);
                Ty::Error
            }
            TypeKind::Optional(_) => {
                self.unsupported("optional type (`T?`)", t.span);
                Ty::Error
            }
            TypeKind::Weak(_) => {
                self.unsupported("`weak` reference", t.span);
                Ty::Error
            }
            TypeKind::Array { .. } => {
                self.unsupported("fixed-size array type", t.span);
                Ty::Error
            }
            TypeKind::Fn { .. } => {
                self.unsupported("function type", t.span);
                Ty::Error
            }
            TypeKind::Error => Ty::Error,
        }
    }

    // ---- module-level values ----

    /// The type of a module-level value, checking its initialiser first if the type is not
    /// declared. A value that (indirectly) needs itself gets [`Ty::Error`]; the initialisation
    /// cycle is reported by [`Checker::init_order`].
    pub fn global_ty(&mut self, g: GlobalId) -> Ty {
        if let Some(ty) = &self.globals[g.0 as usize].ty {
            return ty.clone();
        }
        self.ensure_global(g);
        self.globals[g.0 as usize].ty.clone().unwrap_or(Ty::Error)
    }

    fn ensure_global(&mut self, g: GlobalId) {
        let info = &mut self.globals[g.0 as usize];
        if info.state != GlobalState::Unchecked {
            return;
        }
        info.state = GlobalState::InProgress;
        let name = info.name.clone();
        let decl = info.decl;
        let declared = info.ty.clone();
        let annotation_span = info.annotation_span;
        let span = info.span;
        let (body, ty, deps) = FnCtx::new(self, format!("<init {name}>"), None).check_initialiser(
            &decl.init,
            declared.as_ref(),
            annotation_span,
            span,
        );
        let info = &mut self.globals[g.0 as usize];
        info.ty = Some(ty);
        info.body = Some(Rc::new(body));
        info.deps = deps;
        info.state = GlobalState::Done;
    }

    // ---- bodies ----

    /// Checks each default argument once so its errors are reported once, not per call.
    fn check_defaults(&mut self) {
        for f in 0..self.fns.len() {
            let params = self.fns[f].params.clone();
            for p in params {
                if let Some(default) = p.default {
                    let name = format!("<default {}>", p.name);
                    FnCtx::new(self, name, None).check_default(default, &p.ty);
                }
            }
        }
    }

    fn check_fn(&mut self, f: FnId) {
        let Some(decl) = self.fns[f.0 as usize].decl else {
            return;
        };
        let info = &self.fns[f.0 as usize];
        let (name, span, params, ret, ret_span) = (
            info.name.clone(),
            info.span,
            info.params.clone(),
            info.ret.clone(),
            info.ret_span,
        );
        let (body, deps) = FnCtx::new(self, name, Some((ret.clone(), ret_span)))
            .check_fn_body(decl, &params, &ret, ret_span, span);
        let info = &mut self.fns[f.0 as usize];
        info.body = Some(Rc::new(body));
        info.deps = deps;
    }

    fn check_script(&mut self) {
        let stmts = std::mem::take(&mut self.script);
        let span = self.file.span;
        // The script runs after every initialiser, so its dependencies need no ordering.
        let body = FnCtx::new(self, "<script>".to_string(), None).check_script(&stmts, span);
        self.script_body = Some(Rc::new(body));
    }

    /// `fn main` must be callable with no arguments and return nothing.
    fn check_main(&mut self) -> Option<FnId> {
        let ValueRef::Fn(id) = *self.values.get("main")? else {
            return None;
        };
        let info = &self.fns[id.0 as usize];
        if !info.params.is_empty() || info.ret != Ty::Unit && !info.ret.is_error() {
            let diag = Diagnostic::error(
                "`main` must take no arguments and return nothing",
                info.name_span,
            )
            .with_hint("write `fn main() { ... }`");
            self.report(diag);
        }
        Some(id)
    }

    // ---- initialisation order ----

    /// Orders the module-level initialisers so each runs after the values it uses, directly or
    /// through the functions it calls (DESIGN §4.2). A cycle through a value is an error naming
    /// the chain; recursion between functions alone is fine.
    fn init_order(&mut self) -> Vec<GlobalId> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum State {
            White,
            Visiting,
            Done,
        }
        let nf = self.fns.len();
        let node_index = |d: Dep| match d {
            Dep::Fn(f) => f.0 as usize,
            Dep::Global(g) => nf + g.0 as usize,
        };
        let mut state = vec![State::White; nf + self.globals.len()];
        let mut order = Vec::new();
        let mut reported: HashSet<GlobalId> = HashSet::new();
        for start in 0..self.globals.len() {
            let start = Dep::Global(GlobalId(start as u32));
            if state[node_index(start)] != State::White {
                continue;
            }
            state[node_index(start)] = State::Visiting;
            // (node, next dependency to visit)
            let mut stack: Vec<(Dep, usize)> = vec![(start, 0)];
            while let Some(&(node, next)) = stack.last() {
                let deps = match node {
                    Dep::Fn(f) => &self.fns[f.0 as usize].deps,
                    Dep::Global(g) => &self.globals[g.0 as usize].deps,
                };
                let Some(&dep) = deps.get(next) else {
                    stack.pop();
                    state[node_index(node)] = State::Done;
                    if let Dep::Global(g) = node {
                        order.push(g);
                    }
                    continue;
                };
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                match state[node_index(dep)] {
                    State::White => {
                        state[node_index(dep)] = State::Visiting;
                        stack.push((dep, 0));
                    }
                    State::Done => {}
                    State::Visiting => {
                        let from = stack.iter().position(|&(n, _)| n == dep).unwrap_or(0);
                        let cycle: Vec<Dep> = stack[from..].iter().map(|&(n, _)| n).collect();
                        self.report_cycle(&cycle, &mut reported);
                    }
                }
            }
        }
        order
    }

    fn report_cycle(&mut self, cycle: &[Dep], reported: &mut HashSet<GlobalId>) {
        let globals: Vec<GlobalId> = cycle
            .iter()
            .filter_map(|d| match d {
                Dep::Global(g) => Some(*g),
                Dep::Fn(_) => None,
            })
            .collect();
        // A cycle of functions only is plain recursion.
        if globals.is_empty() || globals.iter().all(|g| reported.contains(g)) {
            return;
        }
        reported.extend(globals.iter().copied());
        let name = |d: &Dep| match d {
            Dep::Fn(f) => format!("`{}()`", self.fns[f.0 as usize].name),
            Dep::Global(g) => format!("`{}`", self.globals[g.0 as usize].name),
        };
        let mut chain: Vec<String> = cycle.iter().map(name).collect();
        chain.push(name(&cycle[0]));
        let first = self.globals[globals[0].0 as usize].name_span;
        let mut diag = Diagnostic::error(
            format!("initialisation cycle: {}", chain.join(" -> ")),
            first,
        )
        .with_hint("module-level values are initialised in dependency order, so they cannot depend on each other in a loop; compute one of them inside a function instead");
        for g in &globals[1..] {
            let info = &self.globals[g.0 as usize];
            diag = diag.with_label(
                info.name_span,
                format!("`{}` is part of the cycle", info.name),
            );
        }
        self.report(diag);
    }

    // ---- output ----

    fn assemble(&mut self, init_order: Vec<GlobalId>, main: Option<FnId>) -> Option<Program> {
        let mut fns = Vec::new();
        for f in &self.fns {
            fns.push(FnDef {
                name: f.name.clone(),
                sig: FnSig {
                    params: f.params.iter().map(|p| p.ty.clone()).collect(),
                    ret: f.ret.clone(),
                },
                span: f.span,
                body: f.body.clone()?,
            });
        }
        let mut globals = Vec::new();
        for g in &self.globals {
            globals.push(GlobalDef {
                name: g.name.clone(),
                ty: g.ty.clone()?,
                mutable: g.mutable,
                span: g.span,
                init: g.body.clone()?,
            });
        }
        Some(Program {
            fns,
            globals,
            init_order,
            script: self.script_body.clone()?,
            main,
        })
    }

    /// Names in scope at module level, for "did you mean".
    pub fn module_names(&self) -> impl Iterator<Item = &str> {
        self.values
            .keys()
            .map(String::as_str)
            .chain(BUILTIN_FNS.iter().copied())
    }
}
