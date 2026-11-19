//! The checker's module-level state and its phases: collect every declaration (so order does
//! not matter), check each body, order the module-level initialisers, assemble the program.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use colibri_syntax::ast::{self, BindKind, ItemKind, PatKind, TopLevel, TypeKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::decls::{AliasInfo, TypeExtra};
use crate::generics::SigCtx;
use crate::hir::{
    Body, FieldDef, FnDef, FnId, GlobalDef, GlobalId, ImplDef, ImplId, Program, TypeDef,
    TypeDefKind, TypeKey, VariantDef,
};
use crate::ifaces::IfaceInfo;
use crate::obligations::Obligation;
use crate::prelude;
use crate::suggest;
use crate::traits::{BoundRef, GenericParamInfo, ImplInfo};
use crate::types::{ERROR_TYPE, FnSig, ParamId, RESULT_TYPE, Ty, TypeId};

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
    /// A `var self` receiver: the local is assignable.
    pub mutable: bool,
}

pub(crate) struct FnInfo<'a> {
    /// The source name; `Type.method` for a method.
    pub name: String,
    pub name_span: Span,
    pub span: Span,
    /// All parameters; a method's first one is `self`.
    pub params: Vec<ParamInfo<'a>>,
    /// `Some(mutable)` for a method with a `self` receiver (`var self` is `Some(true)`).
    pub receiver: Option<bool>,
    pub ret: Ty,
    pub ret_span: Option<Span>,
    /// `None` when the declaration uses something unsupported: calls are then checked loosely.
    pub decl: Option<&'a ast::FnDecl>,
    pub body: Option<Rc<Body>>,
    pub deps: Vec<Dep>,
    /// Every type parameter in scope: those of the enclosing `impl` or interface first, then
    /// the function's own. A call instantiates all of them.
    pub generics: Vec<GenericParamInfo>,
    /// What `Self` means in the body (the implementing type, or the interface's `Self`).
    pub self_ty: Option<Ty>,
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
    pub file: &'a ast::File,
    pub diags: Vec<Diagnostic>,
    /// While positive, diagnostics are dropped (re-checking a default argument at a call).
    muted: u32,
    pub values: HashMap<String, ValueRef>,
    /// Type names that exist but cannot be used yet (they were declared with an unsupported
    /// construct), so uses of them do not cascade into "unknown type".
    pub poisoned_types: HashSet<String>,
    pub fns: Vec<FnInfo<'a>>,
    pub globals: Vec<GlobalInfo<'a>>,
    /// Every user-defined struct, enum, class and interface, indexed by [`TypeId`].
    pub types: Vec<TypeDef>,
    /// What HIR does not keep about each type, parallel to `types`.
    pub type_extra: Vec<TypeExtra<'a>>,
    pub type_ids: HashMap<String, TypeId>,
    pub aliases: HashMap<String, AliasInfo<'a>>,
    /// Methods and static functions by owner and name (inherent `impl` blocks).
    pub methods: HashMap<(TypeId, String), FnId>,
    /// Whether every declaration is collected, so bounds can be decided at once instead of
    /// being postponed.
    pub collected: bool,
    /// Bounds written in declarations before the types and impls were all known, checked once
    /// they are (`Map` keys and `Set` elements in field types and signatures, for example).
    pub pending: Vec<Obligation>,
    /// The next unused [`ParamId`] (0 is the prelude `Option`'s `T`, 1 and 2 are `Result`'s `T` and `E`).
    pub next_param: u32,
    /// The bounds of every type parameter declared so far.
    pub param_bounds: HashMap<ParamId, Vec<BoundRef>>,
    /// The type parameters visible to the type being resolved, innermost last.
    pub type_scope: Vec<(String, Ty)>,
    /// What `Self` means where the type being resolved is written.
    pub self_ty: Option<Ty>,
    /// The interfaces, by type id.
    pub ifaces: HashMap<TypeId, IfaceInfo<'a>>,
    /// Explicit `impl Interface for Type` blocks and the derived implementations that were
    /// used, indexed by [`ImplId`].
    pub impls: Vec<ImplInfo>,
    /// The explicit implementations, by implementing type head and interface.
    pub impl_by_key: HashMap<(TypeKey, TypeId), ImplId>,
    /// The derived implementations made so far (built-in interfaces only).
    pub derived_impls: HashMap<(TypeKey, TypeId), ImplId>,
    script: Vec<&'a ast::Stmt>,
    script_body: Option<Rc<Body>>,
    /// The native stack position where checking started, and how far below it checking may go.
    stack_base: usize,
    stack_budget: usize,
}

/// The prelude `Option` enum (index 0 of the type table): `none` and `some(T)`, where the
/// payload's `T` stands for the type argument of `Ty::Option`.
fn prelude_option(span: Span) -> TypeDef {
    TypeDef {
        name: "Option".to_string(),
        span,
        params: vec![ParamId(0)],
        kind: TypeDefKind::Enum {
            variants: vec![
                VariantDef {
                    name: "none".to_string(),
                    span,
                    fields: Vec::new(),
                    named: false,
                },
                VariantDef {
                    name: "some".to_string(),
                    span,
                    fields: vec![FieldDef {
                        name: "0".to_string(),
                        ty: Ty::Param(ParamId(0), "T".into()),
                        span,
                        weak: false,
                    }],
                    named: false,
                },
            ],
        },
    }
}

/// The prelude `Result` enum (index 1 of the type table): `ok(T)` and `err(E)`, whose payloads
/// are the type parameters `ParamId(1)` and `ParamId(2)`.
fn prelude_result(span: Span) -> TypeDef {
    let variant = |name: &str, param: u32, pname: &str| VariantDef {
        name: name.to_string(),
        span,
        fields: vec![FieldDef {
            name: "0".to_string(),
            ty: Ty::Param(ParamId(param), pname.into()),
            span,
            weak: false,
        }],
        named: false,
    };
    TypeDef {
        name: "Result".to_string(),
        span,
        params: vec![ParamId(1), ParamId(2)],
        kind: TypeDefKind::Enum {
            variants: vec![variant("ok", 1, "T"), variant("err", 2, "E")],
        },
    }
}

/// A position on the native stack, comparable with another one.
pub(crate) fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// Names that exist without a declaration.
pub(crate) const BUILTIN_FNS: &[&str] = &["print", "panic"];

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
            types: vec![prelude_option(file.span), prelude_result(file.span)],
            type_extra: vec![TypeExtra::default(), TypeExtra::default()],
            type_ids: HashMap::new(),
            aliases: HashMap::new(),
            methods: HashMap::new(),
            collected: false,
            pending: Vec::new(),
            next_param: 3,
            param_bounds: HashMap::new(),
            type_scope: Vec::new(),
            self_ty: None,
            ifaces: HashMap::new(),
            impls: Vec::new(),
            impl_by_key: HashMap::new(),
            derived_impls: HashMap::new(),
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

    pub fn report(&mut self, mut diag: Diagnostic) {
        // The prelude has no source to show, so a label inside it cannot be rendered.
        diag.labels.retain(|(span, _)| !prelude::in_prelude(*span));
        if self.muted == 0 {
            self.diags.push(diag);
        }
    }

    /// How many errors (not warnings) have been reported so far.
    pub fn error_count(&self) -> usize {
        self.diags.iter().filter(|d| d.is_error()).count()
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
        let prelude_file: &'static ast::File = prelude::prelude();
        let prelude_items: Vec<&'a ast::Item> = prelude_file
            .items
            .iter()
            .filter_map(|top| match top {
                TopLevel::Item(item) => Some(item),
                TopLevel::Stmt(_) => None,
            })
            .collect();
        let items: Vec<&'a ast::Item> = file
            .items
            .iter()
            .filter_map(|top| match top {
                TopLevel::Item(item) => Some(item),
                TopLevel::Stmt(_) => None,
            })
            .collect();
        let all: Vec<&'a ast::Item> = prelude_items.iter().chain(&items).copied().collect();
        // Types first, so every signature can name them whatever the order of the file.
        self.declare_types(&all);
        self.define_types(&all);
        for item in &prelude_items {
            self.collect_item(item);
        }
        for top in &file.items {
            match top {
                TopLevel::Item(item) => self.collect_item(item),
                TopLevel::Stmt(stmt) => self.script.push(stmt),
            }
        }
        self.collected = true;
        self.flush_pending();
    }

    fn collect_item(&mut self, item: &'a ast::Item) {
        for attr in &item.attrs {
            self.unsupported(&format!("the attribute `@{}`", attr.name.name), attr.span);
        }
        match &item.kind {
            ItemKind::Fn(decl) => self.collect_fn(item, decl),
            ItemKind::Binding(binding) => self.collect_global(item, binding),
            // Declared and defined before the other items.
            ItemKind::Struct(_)
            | ItemKind::Enum(_)
            | ItemKind::TypeAlias(_)
            | ItemKind::Interface(_) => {}
            ItemKind::Impl(decl) => self.collect_impl(decl, item),
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
        let id = self.add_fn(item, decl, &SigCtx::default());
        // A clash keeps the later declaration out of the name table but still checks its body.
        self.declare_value(&decl.name.name, decl.name.span, ValueRef::Fn(id));
    }

    /// Resolves a function's signature and records it; the body is checked later.
    pub(crate) fn add_fn(
        &mut self,
        item: &'a ast::Item,
        decl: &'a ast::FnDecl,
        ctx: &SigCtx,
    ) -> FnId {
        let saved = std::mem::replace(&mut self.self_ty, ctx.self_ty.clone());
        let sig = self.resolve_sig(decl, ctx);
        self.self_ty = saved;
        let poisoned = sig.poisoned || decl.body.is_none();
        let name = match &ctx.owner {
            Some(owner) => format!("{owner}.{}", decl.name.name),
            None => decl.name.name.clone(),
        };
        let id = FnId(self.fns.len() as u32);
        self.fns.push(FnInfo {
            name,
            name_span: decl.name.span,
            span: item.span,
            params: sig.params,
            receiver: sig.receiver,
            ret: sig.ret,
            ret_span: sig.ret_span,
            decl: if poisoned { None } else { Some(decl) },
            body: None,
            deps: Vec::new(),
            generics: sig.generics,
            self_ty: ctx.self_ty.clone(),
        });
        id
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
                self.resolve_named(path[0].name.as_str(), args, t.span)
            }
            TypeKind::Tuple(items) if items.is_empty() => Ty::Unit,
            TypeKind::Tuple(items) => {
                Ty::Tuple(items.iter().map(|item| self.resolve_type(item)).collect())
            }
            TypeKind::Optional(inner) => match self.resolve_type(inner) {
                Ty::Error => Ty::Error,
                inner => Ty::Option(Box::new(inner)),
            },
            TypeKind::Weak(inner) => {
                self.resolve_type(inner);
                self.report(
                    Diagnostic::error("`weak` is only allowed on the fields of a class", t.span)
                        .with_hint("a weak reference is a field that does not keep its target alive: `parent: weak Node`"),
                );
                Ty::Error
            }
            TypeKind::Array { .. } => {
                self.unsupported("fixed-size array type", t.span);
                Ty::Error
            }
            TypeKind::Fn { params, ret } => {
                let params: Vec<Ty> = params.iter().map(|p| self.resolve_type(p)).collect();
                let ret = match ret {
                    Some(r) => self.resolve_type(r),
                    None => Ty::Unit,
                };
                if params.iter().any(Ty::is_error) || ret.is_error() {
                    return Ty::Error;
                }
                Ty::Fn(params, Box::new(ret))
            }
            TypeKind::Error => Ty::Error,
        }
    }

    /// A type written as a name with optional type arguments.
    fn resolve_named(&mut self, name: &str, args: &[ast::TypeExpr], span: Span) -> Ty {
        if let Some(param) = self.scoped_param(name) {
            if !args.is_empty() {
                self.report(Diagnostic::error(
                    format!("the type parameter `{name}` takes no type arguments"),
                    span,
                ));
                return Ty::Error;
            }
            return param;
        }
        match name {
            "Self" => {
                return match self.self_ty.clone() {
                    Some(t) => t,
                    None => {
                        self.report(
                            Diagnostic::error(
                                "`Self` is only available inside an `impl` or an `interface`",
                                span,
                            )
                            .with_hint("write the type's name instead"),
                        );
                        Ty::Error
                    }
                };
            }
            "List" | "Set" | "Option" | "Map" => {
                return self.resolve_builtin_generic(name, args, span);
            }
            // `Range` is not reserved: a program's own `Range` type wins.
            "Range" if !self.type_ids.contains_key(name) => {
                return self.resolve_builtin_generic(name, args, span);
            }
            "Result" => return self.resolve_result(args, span),
            _ => {}
        }
        if let Some(&id) = self.type_ids.get(name) {
            return self.instantiate_type(id, args, span);
        }
        if self.aliases.contains_key(name) {
            return self.alias_instance(name, args, span);
        }
        if !args.is_empty() && !self.poisoned_types.contains(name) {
            let known = Ty::numeric_by_name(name).is_some()
                || matches!(name, "int" | "bool" | "str" | "char");
            if known {
                self.report(Diagnostic::error(
                    format!("`{name}` takes no type arguments"),
                    span,
                ));
                return Ty::Error;
            }
        }
        match name {
            "bool" => Ty::Bool,
            "str" => Ty::Str,
            "char" => Ty::Char,
            _ if Ty::numeric_by_name(name).is_some() => {
                Ty::numeric_by_name(name).unwrap_or(Ty::Error)
            }
            "vec2" | "vec3" | "vec4" => {
                self.unsupported(&format!("the type `{name}`"), span);
                Ty::Error
            }
            _ if self.poisoned_types.contains(name) => Ty::Error,
            _ => {
                self.report_unknown_type(name, span);
                Ty::Error
            }
        }
    }

    /// `Result[T]` (shorthand for `Result[T, Error]`) and `Result[T, E]` (DESIGN §10.1).
    fn resolve_result(&mut self, args: &[ast::TypeExpr], span: Span) -> Ty {
        if !(1..=2).contains(&args.len()) {
            self.report(
                Diagnostic::error(
                    format!("`Result` takes 1 or 2 type arguments, found {}", args.len()),
                    span,
                )
                .with_hint("write `Result[T]` (the error is `Error`) or `Result[T, E]`"),
            );
            for a in args {
                self.resolve_type(a);
            }
            return Ty::Error;
        }
        let mut tys: Vec<Ty> = args.iter().map(|a| self.resolve_type(a)).collect();
        if tys.iter().any(Ty::is_error) {
            return Ty::Error;
        }
        if tys.len() == 1 {
            tys.push(Ty::Adt(ERROR_TYPE, Vec::new()));
        }
        Ty::Adt(RESULT_TYPE, tys)
    }

    /// `Name[args]` for a user-defined struct, enum, class or interface.
    fn instantiate_type(&mut self, id: TypeId, args: &[ast::TypeExpr], span: Span) -> Ty {
        let def = &self.types[id.0 as usize];
        let (name, wanted, is_iface) = (def.name.clone(), def.params.len(), def.is_interface());
        if args.len() != wanted {
            self.report_type_arity(&name, wanted, args.len(), span);
            for a in args {
                self.resolve_type(a);
            }
            return Ty::Error;
        }
        let tys: Vec<Ty> = args.iter().map(|a| self.resolve_type(a)).collect();
        if tys.iter().any(Ty::is_error) {
            return Ty::Error;
        }
        if is_iface {
            Ty::Dyn(id, tys)
        } else {
            Ty::Adt(id, tys)
        }
    }

    pub(crate) fn report_type_arity(
        &mut self,
        name: &str,
        wanted: usize,
        found: usize,
        span: Span,
    ) {
        let plural = if wanted == 1 { "" } else { "s" };
        let mut diag = Diagnostic::error(
            format!("`{name}` takes {wanted} type argument{plural}, found {found}"),
            span,
        );
        if wanted > 0 {
            let example: Vec<&str> = ["int", "str", "float", "bool"]
                .into_iter()
                .cycle()
                .take(wanted)
                .collect();
            diag = diag.with_hint(format!("write it like `{name}[{}]`", example.join(", ")));
        } else {
            diag = diag.with_hint(format!("`{name}` is not generic"));
        }
        self.report(diag);
    }

    /// `List[T]`, `Set[T]`, `Option[T]` and `Map[K, V]`.
    fn resolve_builtin_generic(&mut self, name: &str, args: &[ast::TypeExpr], span: Span) -> Ty {
        let wanted = if name == "Map" { 2 } else { 1 };
        if args.len() != wanted {
            let example = match name {
                "Map" => "Map[str, int]",
                "List" => "List[int]",
                "Range" => "Range[int]",
                "Set" => "Set[int]",
                _ => "Option[int]",
            };
            let plural = if wanted == 1 { "" } else { "s" };
            self.report(
                Diagnostic::error(
                    format!(
                        "`{name}` takes {wanted} type argument{plural}, found {}",
                        args.len()
                    ),
                    span,
                )
                .with_hint(format!("write it like `{example}`")),
            );
            for a in args {
                self.resolve_type(a);
            }
            return Ty::Error;
        }
        let tys: Vec<Ty> = args.iter().map(|a| self.resolve_type(a)).collect();
        if tys.iter().any(Ty::is_error) {
            return Ty::Error;
        }
        let mut tys = tys.into_iter();
        let mut next = || Box::new(tys.next().unwrap_or(Ty::Error));
        match name {
            "List" => Ty::List(next()),
            "Option" => Ty::Option(next()),
            "Range" => {
                let elem = next();
                if *elem != Ty::INT && !elem.is_error() {
                    self.report(
                        Diagnostic::error(
                            format!("`Range[{elem}]` is not supported: ranges are of `int`"),
                            args[0].span,
                        )
                        .with_hint("write `Range[int]`"),
                    );
                    return Ty::Error;
                }
                Ty::Range(elem)
            }
            "Set" => {
                let elem = next();
                self.require_hashable(&elem, "Set", args[0].span);
                Ty::Set(elem)
            }
            _ => {
                let (k, v) = (next(), next());
                self.require_hashable(&k, "Map", args[0].span);
                Ty::Map(k, v)
            }
        }
    }

    fn report_unknown_type(&mut self, name: &str, span: Span) {
        let mut diag = Diagnostic::error(format!("unknown type `{name}`"), span);
        let known = ["int", "float", "bool", "str", "char", "i64", "f64"];
        let declared = self
            .type_ids
            .keys()
            .chain(self.aliases.keys())
            .chain(self.poisoned_types.iter())
            .map(String::as_str);
        if let Some(s) = suggest::closest(name, known.into_iter().chain(declared)) {
            diag = diag.with_hint(format!("did you mean `{s}`?"));
        }
        self.report(diag);
    }

    /// The user-defined type or alias called `name` (without type arguments: a generic type
    /// is returned with none), if there is one.
    pub fn type_by_name(&mut self, name: &str) -> Option<Ty> {
        if let Some(&id) = self.type_ids.get(name) {
            return Some(if self.types[id.0 as usize].is_interface() {
                Ty::Dyn(id, Vec::new())
            } else {
                Ty::Adt(id, Vec::new())
            });
        }
        if let Some(alias) = self.aliases.get(name) {
            // A generic alias cannot stand alone; it is usable only with its arguments.
            if !alias.generics.is_empty() {
                return Some(Ty::Error);
            }
            let span = alias.name_span;
            return Some(self.alias_instance(name, &[], span));
        }
        None
    }

    /// `ty` as shown in a message, with user-defined types by name.
    pub fn render(&self, ty: &Ty) -> String {
        ty.render(&self.types)
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

    /// Checks each default argument and field default once so its errors are reported once,
    /// not per use.
    fn check_defaults(&mut self) {
        for t in 0..self.types.len() {
            let defaults = self.type_extra[t].defaults.clone();
            for (i, default) in defaults.into_iter().enumerate() {
                if let Some(default) = default {
                    let field = &self.types[t].fields()[i];
                    let name = format!("<default {}>", field.name);
                    // A weak field's default is an optional: `none` or `some(object)`.
                    let ty = if field.weak {
                        Ty::Option(Box::new(field.ty.clone()))
                    } else {
                        field.ty.clone()
                    };
                    let generics = self.type_extra[t].generics.clone();
                    let mark = self.enter_generics(&generics);
                    FnCtx::new(self, name, None).check_default(default, &ty);
                    self.leave_generics(mark);
                }
            }
        }
        for f in 0..self.fns.len() {
            let params = self.fns[f].params.clone();
            let generics = self.fns[f].generics.clone();
            let self_ty = self.fns[f].self_ty.clone();
            let mark = self.enter_generics(&generics);
            let saved = std::mem::replace(&mut self.self_ty, self_ty);
            for p in params {
                if let Some(default) = p.default {
                    let name = format!("<default {}>", p.name);
                    FnCtx::new(self, name, None).check_default(default, &p.ty);
                }
            }
            self.self_ty = saved;
            self.leave_generics(mark);
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
        let mutates_self = info.receiver == Some(true);
        let (generics, self_ty) = (info.generics.clone(), info.self_ty.clone());
        let mark = self.enter_generics(&generics);
        let saved = std::mem::replace(&mut self.self_ty, self_ty);
        let (body, deps) = FnCtx::new(self, name, Some((ret.clone(), ret_span)))
            .mutating_self(mutates_self)
            .check_fn_body(decl, &params, &ret, ret_span, span);
        self.self_ty = saved;
        self.leave_generics(mark);
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
        // `main` may return `Result[()]`: an `err` is reported and the program exits with 1.
        let unit_result = matches!(
            &info.ret,
            Ty::Adt(RESULT_TYPE, args) if args.first() == Some(&Ty::Unit)
        );
        if !info.params.is_empty() || info.ret != Ty::Unit && !info.ret.is_error() && !unit_result {
            let diag = Diagnostic::error(
                "`main` must take no arguments and return nothing or `Result[()]`",
                info.name_span,
            )
            .with_hint("write `fn main() { ... }` or `fn main() -> Result[()] { ... }`");
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
                type_params: f.generics.len() as u32,
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
        let impls = self
            .impls
            .iter()
            .map(|i| ImplDef {
                iface: i.iface,
                iface_args: i.iface_args.clone(),
                target: i.target.clone(),
                key: i.key,
                methods: i
                    .methods
                    .iter()
                    .enumerate()
                    .map(|(m, own)| {
                        own.or_else(|| {
                            self.ifaces
                                .get(&i.iface)
                                .and_then(|info| info.methods.get(m))
                                .and_then(|m| m.default)
                        })
                    })
                    .collect(),
                derived: i.derived,
                span: i.span,
            })
            .collect();
        Some(Program {
            impls,
            fns,
            globals,
            init_order,
            script: self.script_body.clone()?,
            main,
            types: self.types.clone(),
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
