//! Which declaration every name of the program stands for.
//!
//! The editor features that need to know what a name means (references, rename, the names in
//! scope for completion) share this one pass over the syntax trees of the loaded modules. It
//! resolves what scoping decides itself: locals, parameters, generics, the items of a module,
//! imports, `module.name`, `Type.Variant`, struct literals and patterns. What only a type can
//! decide (the field in `p.x`, the method in `p.m()`, `.Variant`) is taken from what the
//! checker recorded ([`colibri_check::insight::Insight`]); a name the checker did not resolve
//! (the program does not check there) is listed in [`Resolution::unresolved`], so a rename can
//! refuse to guess.
//!
//! Every name that is written in the source gets an [`Occ`]: its span, and what it names.
//! Names that name nothing in the program (the prelude, the standard library, a typo) name
//! [`Target::External`].

use std::collections::HashMap;

use colibri_check::package::ImportTarget;
use colibri_syntax::ast::{
    self, ArmBody, Block, Closure, Expr, ExprKind, FnBody, FnDecl, IndexArg, Item, ItemKind,
    LetStmt, Pat, PatKind, Stmt, StmtKind, StrSeg, TopLevel, TypeExpr, TypeKind,
};
use colibri_syntax::{FileId, Span};

use crate::analysis::Analysis;
use crate::symbols::pattern_names;

/// What a name refers to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// A declaration of the program, by the span of the identifier that declares it.
    Def(Span),
    /// Something the program does not declare (the prelude, a host module), by name.
    External(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefKind {
    Local,
    Param,
    Function,
    Method,
    Type,
    Field,
    Variant,
    Global,
    Generic,
    /// The name an `import` gives a module.
    Module,
    /// The name an `import a.{x as y}` gives something.
    ImportAlias,
}

/// One use or declaration of a name.
#[derive(Clone, Debug)]
pub struct Occ {
    /// The identifier.
    pub span: Span,
    pub target: Target,
    pub decl: bool,
    /// `P { x }`: the identifier stands for a field and a variable at once.
    pub shorthand: bool,
}

#[derive(Clone, Debug)]
pub struct DefInfo {
    pub name: String,
    pub kind: DefKind,
    /// The module (index into the loaded files) that declares it.
    pub module: usize,
    /// Why it cannot be renamed, if it cannot.
    pub fixed: Option<&'static str>,
    /// Whether code outside the file's folder can name it: false for a local, a generic and
    /// a top-level item that is not `pub`. Members of a type and parameters count as visible
    /// (a field or a named argument is used wherever the type or function is).
    pub visible: bool,
}

/// A name that is in scope at the cursor.
#[derive(Clone, Debug)]
pub struct Local {
    pub name: String,
    pub span: Span,
    pub kind: DefKind,
}

/// What a module name stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModRef {
    /// A module of the program: an index into the loaded files.
    Source(usize),
    /// A module the host provides, by name.
    Host(String),
}

/// A field, variant or method a type declares.
#[derive(Clone, Debug)]
pub struct TypeMember {
    pub name: String,
    pub kind: DefKind,
    pub span: Span,
    /// A method that takes `self`.
    pub receiver: bool,
    /// Callable from other packages: not a method declared without `pub`.
    pub public: bool,
}

/// The result of resolving a program.
#[derive(Default)]
pub struct Resolution {
    /// What each module name (by the span of the identifier that declares it) stands for.
    pub module_refs: HashMap<Span, ModRef>,
    /// The fields, variants and methods of each type of the program, by its declaration.
    pub type_members: HashMap<Span, Vec<TypeMember>>,
    /// The names the module at the cursor can use without a qualifier: its own items, imported
    /// names and imported modules.
    pub module_scope: Vec<(String, DefKind, Span)>,
    pub occs: Vec<Occ>,
    pub defs: HashMap<Span, DefInfo>,
    /// Member names (`p.x`, `p.m()`, `.V`) the checker did not resolve.
    pub unresolved: Vec<(Span, String)>,
    /// The locals and parameters in scope at the cursor, innermost first.
    pub scope: Vec<Local>,
}

// ---- what a module declares ----

#[derive(Clone, Debug)]
enum ModTarget {
    Source(usize),
    Host(String),
    Failed,
}

#[derive(Default, Clone)]
struct ModScope {
    /// Functions, module-level bindings and imported names, by name.
    values: HashMap<String, (Span, DefKind)>,
    types: HashMap<String, Span>,
    modules: HashMap<String, (Span, ModTarget)>,
}

#[derive(Default, Clone)]
struct TypeInfo {
    fields: HashMap<String, Span>,
    variants: HashMap<String, Span>,
    methods: HashMap<String, Span>,
}

#[derive(Default, Clone)]
struct FnInfo {
    params: HashMap<String, Span>,
    receiver: bool,
}

/// What a path of names (`util`, `shape.Circle`) stands for.
enum PathRes {
    Module(ModTarget),
    Type(Span),
    Value,
}

struct Walker<'a> {
    a: &'a Analysis,
    res: Resolution,
    mods: Vec<ModScope>,
    types: HashMap<Span, TypeInfo>,
    fns: HashMap<Span, FnInfo>,
    /// An implementation's method stands for the interface's method it implements.
    alias: HashMap<Span, Span>,
    /// Methods of an `impl` declared without `pub`.
    private_fns: std::collections::HashSet<Span>,
    /// What the checker resolved: use to declaration.
    refs: HashMap<Span, Span>,
    /// Named facts: the callee of a call, by call span and name.
    callees: HashMap<(Span, String), Span>,
    /// Declaration identifiers by file, sorted by start.
    decl_spans: HashMap<FileId, Vec<Span>>,
    /// Set once the item-level declarations are all known (and `decl_spans` is sorted).
    frozen: bool,
    cursor: Option<(FileId, u32)>,
    // The module being walked.
    module: usize,
    locals: Vec<HashMap<String, (Span, DefKind)>>,
    generics: Vec<HashMap<String, Span>>,
    self_ty: Option<Span>,
    /// While walking the later alternatives of an or-pattern: the names the first one bound.
    rebind: Option<HashMap<String, Span>>,
}

/// Resolves every name of the loaded program. With a `cursor`, also finds what is in scope
/// there.
pub fn resolve(a: &Analysis, cursor: Option<(FileId, u32)>) -> Resolution {
    let mut w = Walker {
        a,
        res: Resolution::default(),
        mods: vec![ModScope::default(); a.program.files.len()],
        types: HashMap::new(),
        fns: HashMap::new(),
        alias: HashMap::new(),
        private_fns: Default::default(),
        refs: a
            .insight
            .references()
            .iter()
            .map(|(use_at, def)| (*use_at, *def))
            .collect(),
        callees: a
            .insight
            .facts()
            .iter()
            .filter_map(|f| Some(((f.span, f.name.clone()?), f.def?)))
            .collect(),
        decl_spans: HashMap::new(),
        frozen: false,
        cursor,
        module: 0,
        locals: Vec::new(),
        generics: Vec::new(),
        self_ty: None,
        rebind: None,
    };
    for i in 0..a.program.files.len() {
        w.declare_items(i);
    }
    for i in 0..a.program.files.len() {
        w.bind_imports_and_impls(i);
    }
    for list in w.decl_spans.values_mut() {
        list.sort_by_key(|s| s.start);
    }
    w.frozen = true;
    for i in 0..a.program.files.len() {
        w.walk_module(i);
    }
    w.finish()
}

impl Walker<'_> {
    fn finish(mut self) -> Resolution {
        for (span, info) in &self.types {
            let mut list = Vec::new();
            for (name, s) in &info.fields {
                list.push(TypeMember {
                    name: name.clone(),
                    kind: DefKind::Field,
                    span: *s,
                    receiver: false,
                    public: true,
                });
            }
            for (name, s) in &info.variants {
                list.push(TypeMember {
                    name: name.clone(),
                    kind: DefKind::Variant,
                    span: *s,
                    receiver: false,
                    public: true,
                });
            }
            for (name, s) in &info.methods {
                list.push(TypeMember {
                    name: name.clone(),
                    kind: DefKind::Method,
                    span: *s,
                    receiver: self.fns.get(s).is_some_and(|f| f.receiver),
                    public: !self.private_fns.contains(s),
                });
            }
            list.sort_by_key(|m| m.span.start);
            self.res.type_members.insert(*span, list);
        }
        if let Some((file, _)) = self.cursor
            && let Some(m) = self.a.module_of(file)
        {
            let scope = &self.mods[m];
            let mut names: Vec<(String, DefKind, Span)> = Vec::new();
            names.extend(scope.values.iter().map(|(n, (s, k))| (n.clone(), *k, *s)));
            names.extend(
                scope
                    .types
                    .iter()
                    .map(|(n, s)| (n.clone(), DefKind::Type, *s)),
            );
            names.extend(
                scope
                    .modules
                    .iter()
                    .map(|(n, (s, _))| (n.clone(), DefKind::Module, *s)),
            );
            names.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.start.cmp(&b.2.start)));
            self.res.module_scope = names;
        }
        self.res
    }
}

fn items_of(file: &ast::File) -> impl Iterator<Item = &Item> {
    file.items.iter().filter_map(|t| match t {
        TopLevel::Item(i) => Some(i),
        TopLevel::Stmt(_) => None,
    })
}

impl<'a> Walker<'a> {
    // ---- phase A: what each module declares ----

    fn def(&mut self, name: &ast::Ident, kind: DefKind, module: usize) -> Span {
        self.def_with(name, kind, module, None)
    }

    fn def_with(
        &mut self,
        name: &ast::Ident,
        kind: DefKind,
        module: usize,
        fixed: Option<&'static str>,
    ) -> Span {
        self.res.defs.insert(
            name.span,
            DefInfo {
                name: name.name.clone(),
                kind,
                module,
                fixed,
                visible: !matches!(kind, DefKind::Local | DefKind::Generic),
            },
        );
        if !self.frozen {
            self.decl_spans
                .entry(name.span.file)
                .or_default()
                .push(name.span);
        }
        name.span
    }

    fn declare_fn(&mut self, f: &FnDecl, kind: DefKind, module: usize) -> Span {
        let key = self.def(&f.name, kind, module);
        let info = FnInfo {
            params: f
                .params
                .iter()
                .map(|p| (p.name.name.clone(), p.name.span))
                .collect(),
            receiver: f.receiver.is_some(),
        };
        self.fns.insert(key, info);
        key
    }

    fn declare_items(&mut self, module: usize) {
        let file = &self.a.program.files[module];
        for item in items_of(file) {
            match &item.kind {
                ItemKind::Fn(f) => {
                    let key = self.declare_fn(f, DefKind::Function, module);
                    self.mods[module]
                        .values
                        .insert(f.name.name.clone(), (key, DefKind::Function));
                }
                ItemKind::Struct(s) => {
                    let key = self.def(&s.name, DefKind::Type, module);
                    let mut info = TypeInfo::default();
                    for f in &s.fields {
                        let k = self.def(&f.name, DefKind::Field, module);
                        info.fields.insert(f.name.name.clone(), k);
                    }
                    self.types.insert(key, info);
                    self.mods[module].types.insert(s.name.name.clone(), key);
                }
                ItemKind::Enum(e) => {
                    let key = self.def(&e.name, DefKind::Type, module);
                    let mut info = TypeInfo::default();
                    for v in &e.variants {
                        let k = self.def(&v.name, DefKind::Variant, module);
                        info.variants.insert(v.name.name.clone(), k);
                    }
                    self.types.insert(key, info);
                    self.mods[module].types.insert(e.name.name.clone(), key);
                }
                ItemKind::Interface(i) => {
                    let key = self.def(&i.name, DefKind::Type, module);
                    let mut info = TypeInfo::default();
                    for m in &i.items {
                        if let ItemKind::Fn(f) = &m.kind {
                            let k = self.declare_fn(f, DefKind::Method, module);
                            info.methods.insert(f.name.name.clone(), k);
                        }
                    }
                    self.types.insert(key, info);
                    self.mods[module].types.insert(i.name.name.clone(), key);
                }
                ItemKind::TypeAlias(t) => {
                    let key = self.def(&t.name, DefKind::Type, module);
                    self.types.insert(key, TypeInfo::default());
                    self.mods[module].types.insert(t.name.name.clone(), key);
                }
                ItemKind::Binding(b) => {
                    let mut names = Vec::new();
                    binding_names(&b.pat, &mut names);
                    for id in names {
                        let key = self.def(&id, DefKind::Global, module);
                        self.mods[module]
                            .values
                            .insert(id.name.clone(), (key, DefKind::Global));
                    }
                }
                ItemKind::Extern(block) => {
                    for m in &block.items {
                        if let ItemKind::Fn(f) = &m.kind {
                            let key = self.declare_fn(f, DefKind::Function, module);
                            self.mods[module]
                                .values
                                .insert(f.name.name.clone(), (key, DefKind::Function));
                        }
                    }
                }
                ItemKind::Impl(_) | ItemKind::Import(_) | ItemKind::Test(_) | ItemKind::Error => {}
            }
        }
        // What is not marked `pub` cannot be named from another folder.
        for item in items_of(file).filter(|i| !i.public) {
            let mut names: Vec<&ast::Ident> = Vec::new();
            match &item.kind {
                ItemKind::Fn(f) => names.push(&f.name),
                ItemKind::Struct(s) => names.push(&s.name),
                ItemKind::Enum(e) => names.push(&e.name),
                ItemKind::Interface(i) => names.push(&i.name),
                ItemKind::TypeAlias(t) => names.push(&t.name),
                _ => {}
            }
            if let ItemKind::Binding(b) = &item.kind {
                let mut ids = Vec::new();
                binding_names(&b.pat, &mut ids);
                for id in ids {
                    if let Some(info) = self.res.defs.get_mut(&id.span) {
                        info.visible = false;
                    }
                }
            }
            for n in names {
                if let Some(info) = self.res.defs.get_mut(&n.span) {
                    info.visible = false;
                }
            }
        }
    }

    // ---- phase B: imports and impls ----

    fn import_target(&self, module: usize, item: &Item) -> ModTarget {
        match self.a.program.metas[module].imports.get(&item.id) {
            Some(ImportTarget::Module(i)) => ModTarget::Source(*i),
            Some(ImportTarget::Host(name)) => ModTarget::Host(name.clone()),
            _ => ModTarget::Failed,
        }
    }

    fn bind_imports_and_impls(&mut self, module: usize) {
        let file = &self.a.program.files[module];
        for item in items_of(file) {
            let ItemKind::Import(imp) = &item.kind else {
                continue;
            };
            let target = self.import_target(module, item);
            let Some(last) = imp.path.last() else {
                continue;
            };
            match &imp.kind {
                ast::ImportKind::Plain => {
                    let std =
                        matches!(&target, ModTarget::Source(i) if self.a.program.metas[*i].std);
                    let key = self.def_with(
                        last,
                        DefKind::Module,
                        module,
                        Some(if std {
                            "it is a module of the standard library, which has no file to rename"
                        } else {
                            "a module is named by its file: rename the file"
                        }),
                    );
                    self.note_module(key, &target);
                    self.mods[module]
                        .modules
                        .insert(last.name.clone(), (key, target));
                    self.occ(last.span, Target::Def(key), true);
                }
                ast::ImportKind::Alias(alias) => {
                    let key = self.def(alias, DefKind::Module, module);
                    self.note_module(key, &target);
                    self.mods[module]
                        .modules
                        .insert(alias.name.clone(), (key, target));
                    self.occ(alias.span, Target::Def(key), true);
                }
                ast::ImportKind::Names(names) => {
                    for n in names {
                        let (value, ty, external) = match &target {
                            ModTarget::Source(i) => (
                                self.mods[*i].values.get(&n.name.name).copied(),
                                self.mods[*i].types.get(&n.name.name).copied(),
                                None,
                            ),
                            ModTarget::Host(m) => (
                                None,
                                None,
                                Some(Target::External(format!("{m}.{}", n.name.name))),
                            ),
                            ModTarget::Failed => (None, None, None),
                        };
                        let named = value
                            .map(|v| Target::Def(v.0))
                            .or(ty.map(Target::Def))
                            .or(external)
                            .unwrap_or_else(|| Target::External(n.name.name.clone()));
                        self.occ(n.name.span, named, false);
                        match &n.alias {
                            Some(alias) => {
                                // The new name is a declaration of its own.
                                let k = self.def(alias, DefKind::ImportAlias, module);
                                self.occ(alias.span, Target::Def(k), true);
                                let scope = &mut self.mods[module];
                                scope
                                    .values
                                    .insert(alias.name.clone(), (k, DefKind::ImportAlias));
                                scope.types.insert(alias.name.clone(), k);
                            }
                            None => {
                                let scope = &mut self.mods[module];
                                if let Some(v) = value {
                                    scope.values.insert(n.name.name.clone(), v);
                                }
                                if let Some(t) = ty {
                                    scope.types.insert(n.name.name.clone(), t);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Implementations attach their methods to the type they are for.
        for item in items_of(&self.a.program.files[module]) {
            let ItemKind::Impl(imp) = &item.kind else {
                continue;
            };
            let target = self.type_def_of(module, &imp.target);
            let iface = imp
                .interface
                .as_ref()
                .and_then(|t| self.type_def_of(module, t));
            let external_iface = imp.interface.is_some() && iface.is_none();
            for m in &imp.items {
                let ItemKind::Fn(f) = &m.kind else { continue };
                let fixed = external_iface.then_some(
                    "it implements a method of an interface that is declared outside this package",
                );
                let key = self.def_with(&f.name, DefKind::Method, module, fixed);
                if !m.public && imp.interface.is_none() {
                    self.private_fns.insert(key);
                }
                self.fns.insert(
                    key,
                    FnInfo {
                        params: f
                            .params
                            .iter()
                            .map(|p| (p.name.name.clone(), p.name.span))
                            .collect(),
                        receiver: f.receiver.is_some(),
                    },
                );
                match iface {
                    Some(i) => {
                        if let Some(&im) =
                            self.types.get(&i).and_then(|t| t.methods.get(&f.name.name))
                        {
                            self.alias.insert(key, im);
                        }
                    }
                    None => {
                        if let Some(t) = target
                            && let Some(info) = self.types.get_mut(&t)
                        {
                            info.methods.insert(f.name.name.clone(), key);
                        }
                    }
                }
            }
        }
    }

    /// The type a path in a type expression names, if it is a type of the program.
    fn type_def_of(&self, module: usize, t: &TypeExpr) -> Option<Span> {
        let TypeKind::Named { path, .. } = &t.kind else {
            return None;
        };
        match path.as_slice() {
            [one] => self.mods[module].types.get(&one.name).copied(),
            [m, ty] => match self.mods[module].modules.get(&m.name) {
                Some((_, ModTarget::Source(i))) => self.mods[*i].types.get(&ty.name).copied(),
                _ => None,
            },
            _ => None,
        }
    }

    fn note_module(&mut self, key: Span, target: &ModTarget) {
        match target {
            ModTarget::Source(i) => {
                self.res.module_refs.insert(key, ModRef::Source(*i));
            }
            ModTarget::Host(h) => {
                self.res.module_refs.insert(key, ModRef::Host(h.clone()));
            }
            ModTarget::Failed => {}
        }
    }

    // ---- recording ----

    fn occ(&mut self, span: Span, target: Target, decl: bool) {
        let target = match target {
            Target::Def(d) => Target::Def(self.alias.get(&d).copied().unwrap_or(d)),
            other => other,
        };
        self.res.occs.push(Occ {
            span,
            target,
            decl,
            shorthand: false,
        });
    }

    fn mark_shorthand(&mut self, span: Span, from: usize) {
        for o in &mut self.res.occs[from..] {
            if o.span == span {
                o.shorthand = true;
            }
        }
    }

    /// The identifier of the declaration that the checker's `decl` span covers.
    fn canon(&self, decl: Span) -> Option<Span> {
        let list = self.decl_spans.get(&decl.file)?;
        let at = list.partition_point(|s| s.start < decl.start);
        list.get(at)
            .filter(|s| s.start >= decl.start && s.end <= decl.end)
            .copied()
    }

    /// A member name the checker resolved by type: a field, or a variant after a dot.
    fn member_by_ref(&mut self, name: &ast::Ident) -> bool {
        match self.refs.get(&name.span).and_then(|d| self.canon(*d)) {
            Some(def) => {
                self.occ(name.span, Target::Def(def), false);
                true
            }
            None => false,
        }
    }

    fn unresolved(&mut self, name: &ast::Ident) {
        self.res.unresolved.push((name.span, name.name.clone()));
    }

    // ---- scopes ----

    fn lookup_local(&self, name: &str) -> Option<(Span, DefKind)> {
        self.locals.iter().rev().find_map(|s| s.get(name).copied())
    }

    fn lookup_generic(&self, name: &str) -> Option<Span> {
        self.generics
            .iter()
            .rev()
            .find_map(|s| s.get(name).copied())
    }

    fn lookup_value(&self, name: &str) -> Option<(Span, DefKind)> {
        self.lookup_local(name)
            .or_else(|| self.mods[self.module].values.get(name).copied())
    }

    fn declare_local(&mut self, id: &ast::Ident, kind: DefKind) {
        if let Some(existing) = self.rebind.as_ref().and_then(|m| m.get(&id.name)).copied() {
            self.occ(id.span, Target::Def(existing), true);
            return;
        }
        let key = self.def(id, kind, self.module);
        self.occ(id.span, Target::Def(key), true);
        if let Some(scope) = self.locals.last_mut() {
            scope.insert(id.name.clone(), (key, kind));
        }
    }

    fn push(&mut self) {
        self.locals.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.locals.pop();
    }

    /// Remembers what is in scope when the walk passes the cursor.
    fn snapshot(&mut self, span: Span, strict_end: bool) {
        let Some((file, at)) = self.cursor else {
            return;
        };
        let inside = span.file == file
            && span.start <= at
            && if strict_end {
                at < span.end
            } else {
                at <= span.end
            };
        if inside {
            self.take_scope();
        }
    }

    fn take_scope(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for scope in self.locals.iter().rev() {
            let mut entries: Vec<_> = scope.iter().collect();
            entries.sort_by_key(|(_, (span, _))| std::cmp::Reverse(span.start));
            for (name, (span, kind)) in entries {
                if seen.insert(name.clone()) {
                    out.push(Local {
                        name: name.clone(),
                        span: *span,
                        kind: *kind,
                    });
                }
            }
        }
        self.res.scope = out;
    }

    // ---- phase C: walking ----

    fn walk_module(&mut self, module: usize) {
        self.module = module;
        self.locals = vec![HashMap::new()];
        self.generics.clear();
        self.self_ty = None;
        let file = &self.a.program.files[module];
        for top in &file.items {
            match top {
                TopLevel::Item(item) => self.walk_item(item),
                TopLevel::Stmt(s) => self.walk_stmt(s),
            }
        }
    }

    fn walk_item(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Fn(f) => self.walk_fn(f),
            ItemKind::Struct(s) => {
                self.with_generics(&s.generics, |w| {
                    let info = w.types.get(&s.name.span).cloned();
                    w.occ_decl(&s.name, DefKind::Type);
                    for f in &s.fields {
                        let key = info
                            .as_ref()
                            .and_then(|i| i.fields.get(&f.name.name))
                            .copied();
                        if let Some(k) = key {
                            w.occ(f.name.span, Target::Def(k), true);
                        }
                        w.walk_type(&f.ty);
                        if let Some(d) = &f.default {
                            w.walk_expr(d);
                        }
                    }
                });
            }
            ItemKind::Enum(e) => {
                self.with_generics(&e.generics, |w| {
                    w.occ_decl(&e.name, DefKind::Type);
                    let info = w.types.get(&e.name.span).cloned();
                    for v in &e.variants {
                        if let Some(k) = info.as_ref().and_then(|i| i.variants.get(&v.name.name)) {
                            w.occ(v.name.span, Target::Def(*k), true);
                        }
                        match &v.payload {
                            ast::VariantPayload::Unit => {}
                            ast::VariantPayload::Positional(tys) => {
                                tys.iter().for_each(|t| w.walk_type(t));
                            }
                            ast::VariantPayload::Named(fields) => {
                                fields.iter().for_each(|f| w.walk_type(&f.ty));
                            }
                        }
                    }
                });
            }
            ItemKind::Interface(i) => {
                self.with_generics(&i.generics, |w| {
                    w.occ_decl(&i.name, DefKind::Type);
                    w.self_ty = Some(i.name.span);
                    for m in &i.items {
                        if let ItemKind::Fn(f) = &m.kind {
                            w.walk_fn(f);
                        }
                    }
                    w.self_ty = None;
                });
            }
            ItemKind::Impl(imp) => {
                self.with_generics(&imp.generics, |w| {
                    if let Some(i) = &imp.interface {
                        w.walk_type(i);
                    }
                    w.walk_type(&imp.target);
                    w.self_ty = w.type_def_of(w.module, &imp.target);
                    for m in &imp.items {
                        if let ItemKind::Fn(f) = &m.kind {
                            w.walk_fn(f);
                        }
                    }
                    w.self_ty = None;
                });
            }
            ItemKind::TypeAlias(t) => {
                self.with_generics(&t.generics, |w| {
                    w.occ_decl(&t.name, DefKind::Type);
                    w.walk_type(&t.ty);
                });
            }
            ItemKind::Binding(b) => {
                let mut names = Vec::new();
                binding_names(&b.pat, &mut names);
                for id in names {
                    if let Some((k, _)) = self.mods[self.module].values.get(&id.name).copied() {
                        self.occ(id.span, Target::Def(k), true);
                    }
                }
                if let Some(t) = &b.ty {
                    self.walk_type(t);
                }
                self.walk_expr(&b.init);
            }
            ItemKind::Extern(block) => {
                for m in &block.items {
                    if let ItemKind::Fn(f) = &m.kind {
                        self.walk_fn(f);
                    }
                }
            }
            ItemKind::Test(t) => {
                self.push();
                self.walk_block(&t.body);
                self.pop();
            }
            ItemKind::Import(_) | ItemKind::Error => {}
        }
    }

    /// Records the declaration of a type's own name.
    fn occ_decl(&mut self, name: &ast::Ident, _kind: DefKind) {
        if self.res.defs.contains_key(&name.span) {
            self.occ(name.span, Target::Def(name.span), true);
        }
    }

    fn with_generics(&mut self, params: &[ast::GenericParam], f: impl FnOnce(&mut Self)) {
        let mut scope = HashMap::new();
        for p in params {
            let key = self.def(&p.name, DefKind::Generic, self.module);
            self.occ(p.name.span, Target::Def(key), true);
            scope.insert(p.name.name.clone(), key);
        }
        self.generics.push(scope);
        for p in params {
            p.bounds
                .iter()
                .chain(&p.default)
                .for_each(|b| self.walk_type(b));
        }
        f(self);
        self.generics.pop();
    }

    fn walk_fn(&mut self, f: &FnDecl) {
        // The function's own name was declared with the items.
        if self.res.defs.contains_key(&f.name.span) {
            self.occ(f.name.span, Target::Def(f.name.span), true);
        }
        self.with_generics(&f.generics, |w| {
            for p in &f.params {
                if let Some(t) = &p.ty {
                    w.walk_type(t);
                }
                if let Some(d) = &p.default {
                    w.walk_expr(d);
                }
            }
            if let Some(r) = &f.ret {
                w.walk_type(r);
            }
            let outer = std::mem::take(&mut w.locals);
            w.locals = vec![HashMap::new()];
            for p in &f.params {
                w.declare_local(&p.name, DefKind::Param);
            }
            match &f.body {
                Some(FnBody::Block(b)) => w.walk_block(b),
                Some(FnBody::Expr(e)) => w.walk_expr(e),
                None => {}
            }
            w.locals = outer;
        });
    }

    // ---- types ----

    fn walk_type(&mut self, t: &TypeExpr) {
        match &t.kind {
            TypeKind::Named { path, args } => {
                match path.as_slice() {
                    [one] => {
                        let target = if one.name == "Self" {
                            self.self_ty.map(Target::Def)
                        } else if let Some(g) = self.lookup_generic(&one.name) {
                            Some(Target::Def(g))
                        } else {
                            Some(self.mods[self.module].types.get(&one.name).map_or_else(
                                || Target::External(one.name.clone()),
                                |d| Target::Def(*d),
                            ))
                        };
                        if let Some(t) = target {
                            self.occ(one.span, t, false);
                        }
                    }
                    [m, ty] => {
                        let module = self.mods[self.module].modules.get(&m.name).cloned();
                        match module {
                            Some((key, target)) => {
                                self.occ(m.span, Target::Def(key), false);
                                let found = match &target {
                                    ModTarget::Source(i) => {
                                        self.mods[*i].types.get(&ty.name).copied().map(Target::Def)
                                    }
                                    ModTarget::Host(h) => {
                                        Some(Target::External(format!("{h}.{}", ty.name)))
                                    }
                                    ModTarget::Failed => None,
                                };
                                if let Some(f) = found {
                                    self.occ(ty.span, f, false);
                                }
                            }
                            None => {
                                self.occ(m.span, Target::External(m.name.clone()), false);
                            }
                        }
                    }
                    _ => {}
                }
                args.iter().for_each(|a| self.walk_type(a));
            }
            TypeKind::Optional(t) | TypeKind::Weak(t) => self.walk_type(t),
            TypeKind::Tuple(ts) => ts.iter().for_each(|t| self.walk_type(t)),
            TypeKind::Array { elem, len } => {
                self.walk_type(elem);
                self.walk_expr(len);
            }
            TypeKind::Fn { params, ret } => {
                params.iter().for_each(|t| self.walk_type(t));
                if let Some(r) = ret {
                    self.walk_type(r);
                }
            }
            TypeKind::Never | TypeKind::Error => {}
        }
    }

    /// The type definition a type expression names, without recording anything.
    fn type_of_expr(&self, t: &TypeExpr) -> Option<Span> {
        let TypeKind::Named { path, .. } = &t.kind else {
            return None;
        };
        if matches!(path.as_slice(), [one] if one.name == "Self") {
            return self.self_ty;
        }
        if let [one] = path.as_slice()
            && self.lookup_generic(&one.name).is_some()
        {
            return None;
        }
        self.type_def_of(self.module, t)
    }

    // ---- statements and blocks ----

    fn walk_block(&mut self, b: &Block) {
        self.push();
        self.snapshot(b.span, false);
        for s in &b.stmts {
            self.walk_stmt(s);
            if let Some((file, at)) = self.cursor
                && s.span.file == file
                && at > s.span.end
            {
                self.take_scope();
            }
        }
        if let Some(t) = &b.tail {
            self.walk_expr(t);
        }
        self.pop();
    }

    fn walk_let(&mut self, l: &LetStmt) {
        if let Some(t) = &l.ty {
            self.walk_type(t);
        }
        self.walk_expr(&l.init);
        self.walk_pat(&l.pat, DefKind::Local);
    }

    fn walk_stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let(l) => self.walk_let(l),
            StmtKind::Assign { target, value, .. } => {
                self.walk_expr(target);
                self.walk_expr(value);
            }
            StmtKind::Expr(e) => self.walk_expr(e),
            StmtKind::Return(e) => {
                if let Some(e) = e {
                    self.walk_expr(e);
                }
            }
            StmtKind::Break { value, .. } => {
                if let Some(e) = value {
                    self.walk_expr(e);
                }
            }
            StmtKind::Continue { .. } | StmtKind::Error => {}
            StmtKind::Defer(inner) => self.walk_stmt(inner),
            StmtKind::For {
                pat, iter, body, ..
            } => {
                self.walk_expr(iter);
                self.push();
                self.walk_pat(pat, DefKind::Local);
                self.walk_block(body);
                self.pop();
            }
            StmtKind::While { cond, body, .. } => {
                self.walk_expr(cond);
                self.walk_block(body);
            }
            StmtKind::WhileLet {
                pat,
                scrutinee,
                body,
                ..
            } => {
                self.walk_expr(scrutinee);
                self.push();
                self.walk_pat(pat, DefKind::Local);
                self.walk_block(body);
                self.pop();
            }
        }
    }

    // ---- patterns ----

    fn walk_pat(&mut self, p: &Pat, kind: DefKind) {
        match &p.kind {
            PatKind::Ident(id) => {
                if id.is_capitalised() {
                    self.capitalised(id);
                } else {
                    self.declare_local(id, kind);
                }
            }
            PatKind::Bind { pat, name } => {
                self.walk_pat(pat, kind);
                self.declare_local(name, kind);
            }
            PatKind::Some(p) | PatKind::Ok(p) | PatKind::Err(p) => self.walk_pat(p, kind),
            PatKind::Tuple(ps) => ps.iter().for_each(|p| self.walk_pat(p, kind)),
            PatKind::Or(alts) => {
                let before = self.locals.last().cloned().unwrap_or_default();
                let outer = self.rebind.take();
                for (i, alt) in alts.iter().enumerate() {
                    self.walk_pat(alt, kind);
                    if i == 0 {
                        // The names the first alternative bound.
                        let mut map: HashMap<String, Span> = outer.clone().unwrap_or_default();
                        if let Some(scope) = self.locals.last() {
                            for (name, (span, _)) in scope {
                                if before.get(name).is_none_or(|b| b.0 != *span) {
                                    map.insert(name.clone(), *span);
                                }
                            }
                        }
                        self.rebind = Some(map);
                    }
                }
                self.rebind = outer;
            }
            PatKind::Variant { path, args } => {
                self.variant_path(path);
                for a in args.iter().flatten() {
                    self.walk_pat(a, kind);
                }
            }
            PatKind::Struct { path, fields, .. } => {
                let ty = self.struct_pat_type(path);
                for f in fields {
                    let from = self.res.occs.len();
                    if let Some(k) = ty
                        .and_then(|t| self.types.get(&t))
                        .and_then(|i| i.fields.get(&f.name.name))
                        .copied()
                    {
                        self.occ(f.name.span, Target::Def(k), false);
                    } else {
                        self.unresolved(&f.name);
                    }
                    self.walk_pat(&f.pat, kind);
                    if f.shorthand {
                        self.mark_shorthand(f.name.span, from);
                    }
                }
            }
            PatKind::Wild
            | PatKind::Lit(_)
            | PatKind::Range { .. }
            | PatKind::None
            | PatKind::Error => {}
        }
    }

    /// A capitalised name in a pattern: a constant, or a variant of the matched enum.
    fn capitalised(&mut self, id: &ast::Ident) {
        if let Some((k, _)) = self.mods[self.module].values.get(&id.name).copied() {
            self.occ(id.span, Target::Def(k), false);
        } else if !self.member_by_ref(id) {
            self.unresolved(id);
        }
    }

    fn variant_path(&mut self, path: &ast::PatPath) {
        match (path.segments.as_slice(), path.leading_dot) {
            ([name], _) => {
                if !self.member_by_ref(name) {
                    self.unresolved(name);
                }
            }
            ([ty, name], false) => {
                let (t, _) = self.path_head(ty);
                if let PathRes::Type(t) = t {
                    let v = self
                        .types
                        .get(&t)
                        .and_then(|i| i.variants.get(&name.name))
                        .copied();
                    match v {
                        Some(v) => self.occ(name.span, Target::Def(v), false),
                        None => self.unresolved(name),
                    }
                } else if !self.member_by_ref(name) {
                    self.unresolved(name);
                }
            }
            (segments, _) => {
                for s in segments {
                    self.unresolved(s);
                }
            }
        }
    }

    fn struct_pat_type(&mut self, path: &ast::PatPath) -> Option<Span> {
        match path.segments.as_slice() {
            [name] => {
                if name.name == "Self" {
                    return self.self_ty;
                }
                match self.mods[self.module].types.get(&name.name).copied() {
                    Some(t) => {
                        self.occ(name.span, Target::Def(t), false);
                        Some(t)
                    }
                    None => {
                        self.occ(name.span, Target::External(name.name.clone()), false);
                        None
                    }
                }
            }
            _ => None,
        }
    }

    /// The first name of a path: a module, a type or a value; the occurrence is recorded.
    fn path_head(&mut self, id: &ast::Ident) -> (PathRes, Option<Span>) {
        if let Some((d, _)) = self.lookup_local(&id.name) {
            self.occ(id.span, Target::Def(d), false);
            return (PathRes::Value, Some(d));
        }
        if let Some((d, _)) = self.mods[self.module].values.get(&id.name).copied() {
            self.occ(id.span, Target::Def(d), false);
            return (PathRes::Value, Some(d));
        }
        if id.name == "Self"
            && let Some(t) = self.self_ty
        {
            self.occ(id.span, Target::Def(t), false);
            return (PathRes::Type(t), Some(t));
        }
        if let Some(t) = self.mods[self.module].types.get(&id.name).copied() {
            self.occ(id.span, Target::Def(t), false);
            return (PathRes::Type(t), Some(t));
        }
        if let Some((k, target)) = self.mods[self.module].modules.get(&id.name).cloned() {
            self.occ(id.span, Target::Def(k), false);
            return (PathRes::Module(target), Some(k));
        }
        self.occ(id.span, Target::External(id.name.clone()), false);
        (PathRes::Value, None)
    }

    // ---- expressions ----

    /// What an expression written as a path stands for, without recording anything.
    fn peek_path(&self, e: &Expr) -> PathRes {
        match &e.kind {
            ExprKind::Ident(id) => {
                if self.lookup_value(&id.name).is_some() {
                    PathRes::Value
                } else if let Some(t) = self.mods[self.module].types.get(&id.name) {
                    PathRes::Type(*t)
                } else if let Some((_, m)) = self.mods[self.module].modules.get(&id.name) {
                    PathRes::Module(m.clone())
                } else {
                    PathRes::Value
                }
            }
            ExprKind::Field { base, name } => match self.peek_path(base) {
                PathRes::Module(ModTarget::Source(i)) => {
                    if self.mods[i].values.contains_key(&name.name) {
                        PathRes::Value
                    } else if let Some(t) = self.mods[i].types.get(&name.name) {
                        PathRes::Type(*t)
                    } else {
                        PathRes::Value
                    }
                }
                _ => PathRes::Value,
            },
            ExprKind::Paren(inner) => self.peek_path(inner),
            _ => PathRes::Value,
        }
    }

    fn walk_exprs<'e>(&mut self, es: impl IntoIterator<Item = &'e Expr>) {
        for e in es {
            self.walk_expr(e);
        }
    }

    /// `callee` is what the call names, if known: named arguments are the parameters of a
    /// function of the program; those of anything else are not ours to resolve.
    fn walk_args(&mut self, args: &[ast::Arg], callee: Option<Target>) {
        for a in args {
            if let Some(n) = &a.name {
                match &callee {
                    Some(Target::Def(c)) => {
                        let p = self.fns.get(c).and_then(|f| f.params.get(&n.name)).copied();
                        match p {
                            Some(p) => self.occ(n.span, Target::Def(p), false),
                            None => self.unresolved(n),
                        }
                    }
                    Some(Target::External(_)) => {}
                    None => self.unresolved(n),
                }
            }
            self.walk_expr(&a.value);
        }
    }

    fn walk_expr(&mut self, e: &Expr) {
        self.snapshot(e.span, false);
        match &e.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Char(_)
            | ExprKind::Bool(_)
            | ExprKind::None
            | ExprKind::SelfValue
            | ExprKind::Error => {}
            ExprKind::Str(segs) => {
                for s in segs {
                    if let StrSeg::Interp { expr, .. } = s {
                        self.walk_expr(expr);
                    }
                }
            }
            ExprKind::List(es) | ExprKind::Tuple(es) => self.walk_exprs(es),
            ExprKind::Map(kv) => {
                for (k, v) in kv {
                    self.walk_expr(k);
                    self.walk_expr(v);
                }
            }
            ExprKind::Paren(e) | ExprKind::Try(e) | ExprKind::Go(e) => self.walk_expr(e),
            ExprKind::Unary { expr, .. } => self.walk_expr(expr),
            ExprKind::Ident(id) => {
                self.path_head(id);
            }
            ExprKind::DotVariant { name, args } => {
                if !self.member_by_ref(name) {
                    self.unresolved(name);
                }
                if let Some(args) = args {
                    self.walk_args(args, Some(Target::External(String::new())));
                }
            }
            ExprKind::StructLit { ty, fields } => {
                self.walk_type(ty);
                let t = self.type_of_expr(ty);
                for f in fields {
                    let from = self.res.occs.len();
                    match t
                        .and_then(|t| self.types.get(&t))
                        .and_then(|i| i.fields.get(&f.name.name))
                    {
                        Some(k) => {
                            let k = *k;
                            self.occ(f.name.span, Target::Def(k), false);
                        }
                        None => self.unresolved(&f.name),
                    }
                    self.walk_expr(&f.value);
                    if f.shorthand {
                        self.mark_shorthand(f.name.span, from);
                    }
                }
            }
            ExprKind::Binary { lhs, rhs, .. } | ExprKind::Or { lhs, rhs } => {
                self.walk_expr(lhs);
                self.walk_expr(rhs);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.walk_expr(s);
                }
                if let Some(e) = end {
                    self.walk_expr(e);
                }
            }
            ExprKind::Cast { expr, ty } => {
                self.walk_expr(expr);
                self.walk_type(ty);
            }
            ExprKind::OrHandler {
                lhs,
                binding,
                handler,
            } => {
                self.walk_expr(lhs);
                self.push();
                if let Some(b) = binding {
                    self.declare_local(b, DefKind::Local);
                }
                self.walk_block(handler);
                self.pop();
            }
            ExprKind::Call { callee, args } => {
                self.walk_callee(callee, e.span);
                let target = self.callee_target(callee);
                self.walk_args(args, target);
            }
            ExprKind::Field { base, name } => self.walk_field(base, name, e.span),
            ExprKind::TupleIndex { base, .. } => self.walk_expr(base),
            ExprKind::Index { base, args } => {
                self.walk_expr(base);
                for a in args {
                    match a {
                        IndexArg::Expr(e) => self.walk_expr(e),
                        IndexArg::Type(t) => self.walk_type(t),
                    }
                }
            }
            ExprKind::If { cond, then, else_ } => {
                self.walk_expr(cond);
                self.walk_block(then);
                if let Some(e) = else_ {
                    self.walk_expr(e);
                }
            }
            ExprKind::IfLet {
                pat,
                scrutinee,
                then,
                else_,
            } => {
                self.walk_expr(scrutinee);
                self.push();
                self.walk_pat(pat, DefKind::Local);
                self.walk_block(then);
                self.pop();
                if let Some(e) = else_ {
                    self.walk_expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee);
                for arm in arms {
                    self.push();
                    self.walk_pat(&arm.pat, DefKind::Local);
                    if let Some(g) = &arm.guard {
                        self.walk_expr(g);
                    }
                    match &arm.body {
                        ArmBody::Expr(e) => self.walk_expr(e),
                        ArmBody::Stmt(s) => self.walk_stmt(s),
                    }
                    self.pop();
                }
            }
            ExprKind::Closure(c) => self.walk_closure(c),
            ExprKind::Unsafe(b) | ExprKind::Block(b) => self.walk_block(b),
        }
    }

    fn walk_closure(&mut self, c: &Closure) {
        for cap in &c.captures {
            if cap.name.name != "self" {
                self.path_head(&cap.name);
            }
        }
        for p in &c.params {
            if let Some(t) = &p.ty {
                self.walk_type(t);
            }
        }
        if let Some(r) = &c.ret {
            self.walk_type(r);
        }
        self.push();
        for p in &c.params {
            self.declare_local(&p.name, DefKind::Param);
        }
        match &*c.body {
            FnBody::Block(b) => self.walk_block(b),
            FnBody::Expr(e) => self.walk_expr(e),
        }
        self.pop();
    }

    /// The callee of a call: a method name is resolved by what the checker saw at the call.
    fn walk_callee(&mut self, callee: &Expr, call: Span) {
        match &callee.kind {
            ExprKind::Field { base, name } => {
                self.walk_field_in(base, name, callee.span, Some(call))
            }
            _ => self.walk_expr(callee),
        }
    }

    /// The function a callee names, for the parameters of named arguments.
    fn callee_target(&self, callee: &Expr) -> Option<Target> {
        let at = match &callee.kind {
            ExprKind::Ident(id) => id.span,
            ExprKind::Field { name, .. } => name.span,
            _ => return None,
        };
        // The callee was walked just before: its occurrence is among the last.
        let o = self.res.occs.iter().rev().take(64).find(|o| o.span == at)?;
        Some(o.target.clone())
    }

    fn walk_field(&mut self, base: &Expr, name: &ast::Ident, span: Span) {
        self.walk_field_in(base, name, span, None);
    }

    fn walk_field_in(&mut self, base: &Expr, name: &ast::Ident, span: Span, call: Option<Span>) {
        self.snapshot(span, false);
        let path = self.peek_path(base);
        self.walk_expr(base);
        match path {
            PathRes::Module(ModTarget::Source(i)) => {
                let found = self.mods[i]
                    .values
                    .get(&name.name)
                    .map(|v| v.0)
                    .or_else(|| self.mods[i].types.get(&name.name).copied());
                match found {
                    Some(d) => self.occ(name.span, Target::Def(d), false),
                    None => self.occ(name.span, Target::External(name.name.clone()), false),
                }
            }
            PathRes::Module(ModTarget::Host(h)) => {
                self.occ(
                    name.span,
                    Target::External(format!("{h}.{}", name.name)),
                    false,
                );
            }
            PathRes::Module(ModTarget::Failed) => {
                self.occ(name.span, Target::External(name.name.clone()), false);
            }
            PathRes::Type(t) => {
                let info = self.types.get(&t);
                let found = info
                    .and_then(|i| {
                        i.variants
                            .get(&name.name)
                            .or_else(|| i.methods.get(&name.name))
                    })
                    .copied();
                match found {
                    Some(d) => self.occ(name.span, Target::Def(d), false),
                    None => self.occ(name.span, Target::External(name.name.clone()), false),
                }
            }
            PathRes::Value => {
                if self.member_by_ref(name) {
                    return;
                }
                let key = (call.unwrap_or(span), name.name.clone());
                match self.callees.get(&key).and_then(|d| self.canon(*d)) {
                    Some(d) => self.occ(name.span, Target::Def(d), false),
                    None => self.unresolved(name),
                }
            }
        }
    }
}

/// The names a module-level binding declares, as identifiers (a capitalised `const` too).
fn binding_names(pat: &Pat, out: &mut Vec<ast::Ident>) {
    if let PatKind::Ident(id) = &pat.kind {
        out.push(id.clone());
        return;
    }
    let mut spans = Vec::new();
    pattern_names(pat, &mut spans);
    out.extend(
        spans
            .into_iter()
            .map(|(name, span)| ast::Ident { name, span }),
    );
}
