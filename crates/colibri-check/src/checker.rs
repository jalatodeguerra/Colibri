//! The checker's module-level state and its phases: collect every declaration (so order does
//! not matter), check each body, order the module-level initialisers, assemble the program.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use colibri_syntax::ast::{self, BindKind, ItemKind, PatKind, TopLevel, TypeKind};
use colibri_syntax::{Diagnostic, Span};

use crate::ModuleInput;
use crate::body::FnCtx;
use crate::decls::{AliasInfo, TypeExtra};
use crate::generics::SigCtx;
use crate::hir::{
    Block, Body, FieldDef, FnDef, FnId, GlobalDef, GlobalId, HostFnDef, HostFnId, ImplDef, ImplId,
    Program, TestDef, TypeDef, TypeDefKind, TypeKey, VariantDef,
};
use crate::host::{self, HOST_FILE, HostEnv};
use crate::ifaces::IfaceInfo;
use crate::modules::{AliasId, BASE, Exports, ModId, ModInfo, ModKind, Scope};
use crate::obligations::Obligation;
use crate::package::{ModuleMeta, PackageNode};
use crate::prelude;
use crate::suggest;
use crate::traits::{BoundRef, GenericParamInfo, ImplInfo};
use crate::types::{ERROR_TYPE, FnSig, ParamId, RESULT_TYPE, Ty, TypeId, VecTy};

/// What a module-level name refers to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ValueRef {
    Fn(FnId),
    Global(GlobalId),
}

/// What the checker knows about a function the host provides.
#[derive(Clone, Debug)]
pub(crate) struct HostFnInfo {
    pub id: HostFnId,
    /// The capability the function needs.
    pub requires: Option<String>,
    /// Its parameter names are placeholders (`a`, `b`, ..): named arguments are rejected.
    pub positional: bool,
}

/// One piece of the entry file's top-level code, in source order.
#[derive(Clone, Copy)]
pub(crate) enum ScriptStep<'a> {
    Stmt(&'a ast::Stmt),
    /// A module-level `let` or `var` of the entry file: in a script it is initialised when
    /// execution reaches it (DESIGN §4.2).
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
    /// The module whose names the default is written in.
    pub module: ModId,
}

/// A `test` block: its function, declaration and module.
pub(crate) struct TestInfo<'a> {
    pub f: FnId,
    pub decl: &'a ast::TestDecl,
    pub span: Span,
    pub module: ModId,
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
    pub deps: Vec<(Dep, Span)>,
    /// Every type parameter in scope: those of the enclosing `impl` or interface first, then
    /// the function's own. A call instantiates all of them.
    pub generics: Vec<GenericParamInfo>,
    /// What `Self` means in the body (the implementing type, or the interface's `Self`).
    pub self_ty: Option<Ty>,
    /// Set for a function the host provides; it has no body.
    pub host: Option<HostFnInfo>,
    /// The module whose names the signature and body use.
    pub module: ModId,
    /// Marked `pub` (exported from its package). Host functions are.
    pub public: bool,
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
    pub deps: Vec<(Dep, Span)>,
    pub module: ModId,
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
    /// The entry file: where problems with no better place are reported.
    pub file: &'a ast::File,
    /// Every module: the base (prelude, default host module), the named host modules, then the
    /// source modules in initialisation order.
    pub mods: Vec<ModInfo<'a>>,
    /// The module being checked; its scope is in the fields below.
    pub cur_mod: ModId,
    /// Where the source modules start in `mods`.
    pub source_base: u32,
    pub package_names: Vec<String>,
    /// Imported module names of the current scope.
    pub mod_aliases: HashMap<String, ModId>,
    /// Where imported names came from (current scope), for clash messages.
    pub origins: HashMap<String, String>,
    /// What the host provides and which capabilities it granted.
    pub host: &'a HostEnv,
    /// Set while the items of the host interface are collected.
    collecting_host: bool,
    /// How many host functions are declared so far.
    host_fn_count: u32,
    /// Whether each host function (by [`HostFnId`]) reads its call's type arguments.
    pub(crate) host_typed: Vec<bool>,
    /// The type ids of host declarations are `FIRST_USER_TYPE ..` this far.
    host_type_count: u32,
    /// The `@repr(C)` structs (DESIGN §15.2) and their layouts, once computed.
    pub(crate) repr_c: HashSet<TypeId>,
    pub(crate) c_layouts: std::collections::BTreeMap<TypeId, crate::hir::CLayout>,
    pub(crate) c_layout_visiting: HashSet<TypeId>,
    /// The C function behind each function of an `extern` block, by its [`HostFnId`].
    pub(crate) host_ext: HashMap<u32, crate::hir::ExternDef>,
    /// Set while the signature of an `extern` function is resolved: `cstr` is allowed.
    pub(crate) in_extern: bool,
    /// The `extern` block being collected was refused (no capability): its functions exist
    /// but are not callable, so the refusal is reported once.
    pub(crate) extern_denied: bool,
    /// Set while the functions of an `extern` block are collected: its `@link`.
    pub(crate) extern_lib: Option<Option<String>>,
    pub diags: Vec<Diagnostic>,
    /// What the zonker records for [`crate::insight`], when asked to.
    pub facts: Option<Vec<crate::insight::RawFact>>,
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
    pub aliases: HashMap<String, AliasId>,
    /// The vector types in scope (`vec.vec2`, or `vec2` after `import vec.{vec2}`), which are
    /// built in but only nameable after `import vec`.
    pub vec_types: HashMap<String, VecTy>,
    /// Every type alias, indexed by [`AliasId`].
    pub alias_infos: Vec<AliasInfo<'a>>,
    /// How many types have been checked for infinite size.
    pub infinite_checked: usize,
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
    /// The entry file's top-level statements and `let`/`var` bindings, in source order.
    script: Vec<ScriptStep<'a>>,
    script_body: Option<Rc<Body>>,
    /// For each step of the script, the module-level items it refers to (see
    /// [`FnCtx::check_script`]).
    script_deps: Vec<Vec<(Dep, Span)>>,
    /// Initialise the entry file's values in dependency order even when it has no `fn main`
    /// (see [`crate::CheckOptions`]).
    dependency_order: bool,
    /// The `test` blocks of the source modules, in collection order.
    tests: Vec<TestInfo<'a>>,
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
        sendable: true,
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
        sendable: true,
        kind: TypeDefKind::Enum {
            variants: vec![variant("ok", 1, "T"), variant("err", 2, "E")],
        },
    }
}

/// The body of a host function in [`Program::fns`]: empty, since the host runs it. It names the
/// function in stack traces.
fn host_stub_body(f: &FnInfo<'_>) -> Body {
    Body {
        name: f.name.clone(),
        span: f.span,
        param_count: f.params.len() as u32,
        locals: Vec::new(),
        block: Block {
            locals: Vec::new(),
            stmts: Vec::new(),
            tail: None,
            ty: Ty::Unit,
            span: f.span,
        },
        ret: f.ret.clone(),
        mutates_self: false,
        needs_type_args: false,
    }
}

/// A position on the native stack, comparable with another one.
pub(crate) fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// Names that exist without a declaration.
pub(crate) const BUILTIN_FNS: &[&str] = &[
    "print",
    "panic",
    "assert",
    "assert_eq",
    "wait",
    "wait_frames",
    "wait_until",
    "wait_real",
    "wait_for",
];

impl<'a> Checker<'a> {
    pub fn new(
        inputs: &[ModuleInput<'a>],
        packages: &[PackageNode],
        host: &'a HostEnv,
        stack_budget: usize,
    ) -> Self {
        let file = inputs.last().map_or(prelude::prelude(), |m| m.file);
        let mut mods = vec![ModInfo {
            name: "host".to_string(),
            kind: ModKind::Base,
            files: Vec::new(),
            package: None,
            entry: false,
            root: false,
            imports: None,
            scope: Scope::default(),
            exports: Exports::default(),
            alias_spans: HashMap::new(),
            ffi: false,
        }];
        for name in host.named_modules() {
            let mut exports = Exports::default();
            if name == "std.vec" {
                // The vector types are built into the checker; this module is what names them.
                for v in VecTy::ALL {
                    exports.types.insert(
                        v.name().to_string(),
                        crate::modules::Export {
                            target: crate::modules::TypeEntity::Vec(v),
                            public: true,
                        },
                    );
                }
            }
            mods.push(ModInfo {
                files: host.module_files(&name),
                name,
                kind: ModKind::Host,
                package: None,
                entry: false,
                root: false,
                imports: None,
                scope: Scope::default(),
                exports,
                alias_spans: HashMap::new(),
                ffi: false,
            });
        }
        let source_base = mods.len() as u32;
        for input in inputs {
            let meta: &'a ModuleMeta = input.meta;
            mods.push(ModInfo {
                name: meta.name.clone(),
                kind: ModKind::Source,
                files: vec![input.file],
                package: Some(meta.package),
                entry: meta.entry,
                root: !meta.dependency,
                imports: Some(&meta.imports),
                scope: Scope::default(),
                exports: Exports::default(),
                alias_spans: HashMap::new(),
                ffi: meta.ffi,
            });
        }
        Checker {
            file,
            mods,
            cur_mod: BASE,
            source_base,
            package_names: packages.iter().map(|p| p.name.clone()).collect(),
            mod_aliases: HashMap::new(),
            origins: HashMap::new(),
            alias_infos: Vec::new(),
            infinite_checked: 0,
            host,
            collecting_host: false,
            host_fn_count: 0,
            host_typed: Vec::new(),
            host_type_count: 0,
            repr_c: HashSet::new(),
            c_layouts: std::collections::BTreeMap::new(),
            c_layout_visiting: HashSet::new(),
            host_ext: HashMap::new(),
            in_extern: false,
            extern_denied: false,
            extern_lib: None,
            diags: Vec::new(),
            facts: None,
            muted: 0,
            values: HashMap::new(),
            poisoned_types: HashSet::new(),
            fns: Vec::new(),
            globals: Vec::new(),
            types: vec![prelude_option(file.span), prelude_result(file.span)],
            type_extra: vec![TypeExtra::default(), TypeExtra::default()],
            type_ids: HashMap::new(),
            aliases: HashMap::new(),
            vec_types: HashMap::new(),
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
            script_deps: Vec::new(),
            dependency_order: false,
            tests: Vec::new(),
            stack_base: 0,
            stack_budget,
        }
    }

    /// Whether checking has used up its share of the native stack.
    pub fn out_of_stack(&self) -> bool {
        stack_position().abs_diff(self.stack_base) > self.stack_budget
    }

    /// Initialises the entry file's values in dependency order even when it is a script.
    pub fn dependency_order(mut self, yes: bool) -> Self {
        self.dependency_order = yes;
        self
    }

    /// Makes the checker record what every expression resolved to (see [`crate::insight`]).
    pub fn recording(mut self) -> Self {
        self.facts = Some(Vec::new());
        self
    }

    pub fn run(self) -> Checked {
        self.run_with_insight().0
    }

    pub fn run_with_insight(mut self) -> (Checked, Option<crate::insight::Insight>) {
        self.stack_base = stack_position();
        self.collect();
        for g in 0..self.globals.len() {
            self.ensure_global(GlobalId(g as u32));
        }
        self.check_defaults();
        for f in 0..self.fns.len() {
            self.check_fn(FnId(f as u32));
        }
        for t in 0..self.tests.len() {
            self.check_test(t);
        }
        // A script (an entry file without `fn main`) runs its `let`, `var` and statements top
        // to bottom (DESIGN §4.2); otherwise every value is initialised first.
        let script_mode = !self.dependency_order && self.entry_main().is_none();
        if !script_mode {
            self.script.retain(|s| matches!(s, ScriptStep::Stmt(_)));
        }
        self.check_script();
        let script_globals: Vec<GlobalId> = self
            .script
            .iter()
            .filter_map(|s| match s {
                ScriptStep::Global(g) => Some(*g),
                ScriptStep::Stmt(_) => None,
            })
            .collect();
        let mut init_order = self.init_order(&script_globals);
        if script_mode {
            self.check_script_order();
        }
        let script_from = init_order.len();
        init_order.extend(script_globals);
        let main = self.check_main();
        self.diags.sort_by_key(|d| (d.span.file.0, d.span.start));
        let has_errors = self.diags.iter().any(Diagnostic::is_error);
        let program = if has_errors {
            None
        } else {
            self.assemble(init_order, script_from, main)
        };
        let insight = self.facts.take().map(|raw| self.build_insight(raw));
        let checked = Checked {
            program,
            diagnostics: std::mem::take(&mut self.diags),
        };
        (checked, insight)
    }

    // ---- diagnostics ----

    pub fn report(&mut self, mut diag: Diagnostic) {
        // The prelude has no source to show, so a label inside it cannot be rendered.
        diag.labels.retain(|(span, _)| !prelude::in_prelude(*span));
        // Neither has a host interface (its text is not among the program's sources): a
        // problem found in one is shown at the start of the program.
        if diag.span.file == HOST_FILE {
            diag.span = Span::new(self.file.span.file, 0, 0);
            diag.message = format!("in the host interface: {}", diag.message);
        }
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
        let prelude_file: &'static ast::File = prelude::prelude();
        let prelude_items: Vec<&'a ast::Item> = prelude_file
            .items
            .iter()
            .filter_map(|top| match top {
                TopLevel::Item(item) => Some(item),
                TopLevel::Stmt(_) => None,
            })
            .collect();
        let host: &'a HostEnv = self.host;
        // The base: the prelude and the default host module, whose names every module sees.
        // Host types come right after the prelude's, so their ids are the same in every
        // program that uses the same interface.
        let host_items: Vec<&'a ast::Item> = host.default_items().collect();
        let base_items: Vec<&'a ast::Item> =
            prelude_items.iter().chain(&host_items).copied().collect();
        self.declare_types(&base_items);
        self.define_types(&base_items);
        for item in &prelude_items {
            self.collect_item(item);
        }
        self.collecting_host = true;
        for item in &host_items {
            self.collect_host_item(item);
        }
        self.collecting_host = false;
        // Every other module starts from a copy of the base.
        let base_scope = self.snapshot_scope();
        for m in 1..self.mods.len() {
            self.mods[m].scope = base_scope.clone();
        }
        let others: Vec<ModId> = (1..self.mods.len() as u32).map(ModId).collect();
        // Types first, so every signature can name them whatever the order of the files.
        let mut module_items: Vec<Vec<&'a ast::Item>> = Vec::new();
        let mut host_end = self.types.len();
        for &m in &others {
            let items = self.mods[m.0 as usize].items();
            self.enter_module(m);
            self.declare_types(&items);
            if self.mods[m.0 as usize].kind == ModKind::Host {
                host_end = self.types.len();
            }
            module_items.push(items);
        }
        self.host_type_count = host_end as u32 - crate::types::FIRST_USER_TYPE;
        for &m in &others {
            self.bind_imports_types(m);
        }
        for (k, &m) in others.iter().enumerate() {
            self.enter_module(m);
            self.define_adts(&module_items[k]);
        }
        for (k, &m) in others.iter().enumerate() {
            self.enter_module(m);
            self.define_interfaces(&module_items[k]);
        }
        for &m in &others {
            self.enter_module(m);
            self.define_aliases();
        }
        let mut from = self.infinite_checked;
        self.check_infinite_sizes(&mut from);
        self.layout_repr_c();
        for (k, &m) in others.iter().enumerate() {
            self.enter_module(m);
            match self.mods[m.0 as usize].kind {
                ModKind::Host => {
                    self.collecting_host = true;
                    for item in &module_items[k] {
                        self.collect_host_item(item);
                    }
                    self.collecting_host = false;
                }
                _ => {
                    let entry = self.mods[m.0 as usize].entry;
                    let tops: Vec<&'a ast::File> = self.mods[m.0 as usize].files.clone();
                    for file in tops {
                        let mut reported = false;
                        for top in &file.items {
                            match top {
                                TopLevel::Item(item) => {
                                    let before = self.globals.len();
                                    self.collect_item(item);
                                    let script_value = matches!(
                                        &item.kind,
                                        ItemKind::Binding(b) if b.kind != BindKind::Const
                                    );
                                    if entry && script_value {
                                        let added = before as u32..self.globals.len() as u32;
                                        self.script
                                            .extend(added.map(|g| ScriptStep::Global(GlobalId(g))));
                                    }
                                }
                                TopLevel::Stmt(stmt) if entry => {
                                    self.script.push(ScriptStep::Stmt(stmt));
                                }
                                TopLevel::Stmt(stmt) => {
                                    if !reported {
                                        reported = true;
                                        self.report_top_level_statement(m, stmt.span);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        for &m in &others {
            self.bind_imports_values(m);
        }
        self.enter_module(BASE);
        self.collected = true;
        self.flush_pending();
    }

    fn report_top_level_statement(&mut self, m: ModId, span: Span) {
        let name = self.mods[m.0 as usize].name.clone();
        self.report(
            Diagnostic::error(
                format!("top-level statements are only allowed in the entry file, not in `{name}`"),
                span,
            )
            .with_hint("an imported module holds declarations only (`fn`, `struct`, `let`, ...); move the statement into a function and call it from the entry file"),
        );
    }

    /// A declaration of the host interface. Types were declared with everything else; a
    /// function or method becomes a [`FnInfo`] without a body (`add_fn` remembers its id and
    /// capability), and the methods of a host class are inherent methods like any other.
    fn collect_host_item(&mut self, item: &'a ast::Item) {
        match &item.kind {
            ItemKind::Fn(decl) => {
                let id = self.add_fn(item, decl, &SigCtx::default());
                self.declare_value(&decl.name.name, decl.name.span, ValueRef::Fn(id), true);
            }
            ItemKind::Impl(decl) => self.collect_impl(decl, item),
            // A constant of a host module (`math.PI`): an ordinary module-level constant.
            ItemKind::Binding(binding) => self.collect_global(item, binding),
            _ => {}
        }
    }

    fn collect_item(&mut self, item: &'a ast::Item) {
        for attr in &item.attrs {
            let known = match (&item.kind, attr.name.name.as_str()) {
                // Read when the struct is declared, and by `collect_extern`.
                (ItemKind::Struct(_), "repr") | (ItemKind::Extern(_), "link") => true,
                (_, "repr") => {
                    self.report(Diagnostic::error(
                        "`@repr(C)` applies to a `struct`",
                        attr.span,
                    ));
                    true
                }
                (_, "link") => {
                    self.report(Diagnostic::error(
                        "`@link` applies to an `extern \"C\"` block",
                        attr.span,
                    ));
                    true
                }
                _ => false,
            };
            if !known {
                self.unsupported(&format!("the attribute `@{}`", attr.name.name), attr.span);
            }
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
            // Bound after every declaration is known (`bind_imports_types` / `_values`).
            ItemKind::Import(_) => {}
            ItemKind::Extern(block) => self.collect_extern(item, block),
            ItemKind::Test(decl) => self.collect_test(item, decl),
            ItemKind::Error => {}
        }
    }

    /// Declares a module-level name, reporting a clash.
    pub(crate) fn declare_value(
        &mut self,
        name: &str,
        span: Span,
        value: ValueRef,
        public: bool,
    ) -> bool {
        if BUILTIN_FNS.contains(&name) {
            self.report(Diagnostic::error(
                format!("`{name}` is a built-in function and cannot be redefined"),
                span,
            ));
            return false;
        }
        if let Some(ValueRef::Fn(f)) = self.values.get(name).copied()
            && let Some(host) = &self.fns[f.0 as usize].host
            && !self.host_ext.contains_key(&host.id.0)
        {
            self.report(
                Diagnostic::error(
                    format!("`{name}` is a host function and cannot be redefined"),
                    span,
                )
                .with_hint("the host program already provides this name; pick another one"),
            );
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
        self.export_value(name, value, public);
        true
    }

    fn collect_fn(&mut self, item: &'a ast::Item, decl: &'a ast::FnDecl) {
        let id = self.add_fn(item, decl, &SigCtx::default());
        // A clash keeps the later declaration out of the name table but still checks its body.
        self.declare_value(
            &decl.name.name,
            decl.name.span,
            ValueRef::Fn(id),
            item.public,
        );
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
        let is_extern = self.extern_lib.is_some();
        let host = (self.collecting_host || is_extern).then(|| {
            let id = HostFnId(self.host_fn_count);
            self.host_fn_count += 1;
            let typed = !is_extern && item.attrs.iter().any(|a| a.name.name == "typed");
            self.host_typed.push(typed);
            HostFnInfo {
                id,
                requires: if is_extern {
                    Some("ffi".to_string())
                } else {
                    host::requires_of(item)
                },
                positional: !is_extern && item.attrs.iter().any(|a| a.name.name == "positional"),
            }
        });
        let poisoned = sig.poisoned
            || (decl.body.is_none() && host.is_none())
            || (is_extern && self.extern_denied);
        // A function of a module other than the entry is named after its module.
        let prefix = match &self.mods[self.cur_mod.0 as usize] {
            m if m.kind == ModKind::Source && !m.entry => format!("{}.", m.name),
            _ => String::new(),
        };
        let name = match &ctx.owner {
            Some(owner) => format!("{prefix}{owner}.{}", decl.name.name),
            None => format!("{prefix}{}", decl.name.name),
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
            public: item.public || self.collecting_host,
            host,
            module: self.cur_mod,
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
            module: self.cur_mod,
        });
        self.declare_value(
            &name.name,
            name.span,
            ValueRef::Global(id),
            item.public || self.collecting_host,
        );
    }

    // ---- types ----

    /// Resolves a type written in source.
    pub fn resolve_type(&mut self, t: &ast::TypeExpr) -> Ty {
        match &t.kind {
            TypeKind::Named { path, args } => {
                // `player.Player` is the name `player.Player` in the scope (ARCHITECTURE §11.3).
                if path.len() == 1 {
                    return self.resolve_named(path[0].name.as_str(), args, t.span);
                }
                let name = path
                    .iter()
                    .map(|i| i.name.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                self.resolve_named(&name, args, t.span)
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
            TypeKind::Never => {
                self.report(
                    Diagnostic::error("`!` is only allowed as a return type", t.span).with_hint(
                        "write it after `->`: `fn die(msg: str) -> !` is a function that never returns",
                    ),
                );
                Ty::Error
            }
            TypeKind::Fn { params, ret } => {
                let params: Vec<Ty> = params.iter().map(|p| self.resolve_type(p)).collect();
                let ret = match ret {
                    Some(r) => self.resolve_ret_type(r),
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

    /// A type written after `->`: like [`Self::resolve_type`], but `!` is allowed.
    pub fn resolve_ret_type(&mut self, t: &ast::TypeExpr) -> Ty {
        match t.kind {
            TypeKind::Never => Ty::Never,
            _ => self.resolve_type(t),
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
            "ptr" if !self.type_ids.contains_key(name) && !self.aliases.contains_key(name) => {
                return self.resolve_ptr(args, span);
            }
            "cstr" if !self.type_ids.contains_key(name) && !self.aliases.contains_key(name) => {
                return self.resolve_cstr(args, span);
            }
            _ => {}
        }
        if let Some(&id) = self.type_ids.get(name) {
            return self.instantiate_type(id, args, span);
        }
        if let Some(&alias) = self.aliases.get(name) {
            return self.alias_instance(alias, args, span);
        }
        // The text of a host interface may always say `vec.vec2` (it has no imports).
        let vec = self.vec_types.get(name).copied().or_else(|| {
            (span.file == HOST_FILE)
                .then(|| name.strip_prefix("vec."))
                .flatten()
                .and_then(VecTy::from_name)
        });
        if let Some(v) = vec {
            if !args.is_empty() {
                self.report(Diagnostic::error(
                    format!("`{name}` takes no type arguments"),
                    span,
                ));
                return Ty::Error;
            }
            return Ty::Vec(v);
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
        if let Some((alias, member)) = self.split_qualified(name)
            && let Some(diag) = self.unknown_member(alias, member, span)
        {
            self.report(diag);
            return;
        }
        let mut diag = Diagnostic::error(format!("unknown type `{name}`"), span);
        if VecTy::from_name(name).is_some() {
            diag = diag.with_hint(format!(
                "the vector types are in the `vec` module: write `import vec` and then `vec.{name}`, or `import vec.{{{name}}}`"
            ));
            self.report(diag);
            return;
        }
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
        if let Some(&id) = self.aliases.get(name) {
            let alias = &self.alias_infos[id.0 as usize];
            // A generic alias cannot stand alone; it is usable only with its arguments.
            if !alias.generics.is_empty() {
                return Some(Ty::Error);
            }
            let span = alias.name_span;
            return Some(self.alias_instance(id, &[], span));
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
        let module = info.module;
        let back = self.enter_module(module);
        let (body, ty, deps) = FnCtx::new(self, format!("<init {name}>"), None).check_initialiser(
            &decl.init,
            declared.as_ref(),
            annotation_span,
            span,
        );
        self.enter_module(back);
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
                    let back = self.enter_module(self.type_extra[t].module.unwrap_or(BASE));
                    let mark = self.enter_generics(&generics);
                    FnCtx::new(self, name, None).check_default(default, &ty);
                    self.leave_generics(mark);
                    self.enter_module(back);
                }
            }
        }
        for f in 0..self.fns.len() {
            let params = self.fns[f].params.clone();
            let generics = self.fns[f].generics.clone();
            let self_ty = self.fns[f].self_ty.clone();
            let back = self.enter_module(self.fns[f].module);
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
            self.enter_module(back);
        }
    }

    fn check_fn(&mut self, f: FnId) {
        let Some(decl) = self.fns[f.0 as usize].decl else {
            return;
        };
        if self.fns[f.0 as usize].host.is_some() {
            return;
        }
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
        let module = info.module;
        let back = self.enter_module(module);
        let mark = self.enter_generics(&generics);
        let saved = std::mem::replace(&mut self.self_ty, self_ty);
        let (body, deps) = FnCtx::new(self, name, Some((ret.clone(), ret_span)))
            .mutating_self(mutates_self)
            .check_fn_body(decl, &params, &ret, ret_span, span);
        self.self_ty = saved;
        self.leave_generics(mark);
        self.enter_module(back);
        let info = &mut self.fns[f.0 as usize];
        info.body = Some(Rc::new(body));
        info.deps = deps;
    }

    /// A `test "name" { .. }` block is a function `() -> Result[()]` that nothing calls: it is
    /// checked like any other body, so `?` works and an `err` fails the test (ARCHITECTURE §15).
    fn collect_test(&mut self, item: &'a ast::Item, decl: &'a ast::TestDecl) {
        for attr in &item.attrs {
            self.unsupported(&format!("the attribute `@{}`", attr.name.name), attr.span);
        }
        let module = self.cur_mod;
        if let Some(prev) = self
            .tests
            .iter()
            .find(|t| t.module == module && t.decl.name == decl.name)
        {
            let prev_span = prev.span;
            self.report(
                Diagnostic::error(
                    format!(
                        "the test \"{}\" is defined more than once in this module",
                        decl.name
                    ),
                    item.span,
                )
                .with_label(prev_span, "first defined here")
                .with_hint("test names are unique per module; they are what `colibri test --filter` and the report show"),
            );
        }
        let prefix = match &self.mods[module.0 as usize] {
            m if m.kind == ModKind::Source && !m.entry => format!("{}.", m.name),
            _ => String::new(),
        };
        let f = FnId(self.fns.len() as u32);
        self.fns.push(FnInfo {
            name: format!("{prefix}test \"{}\"", decl.name),
            name_span: item.span,
            span: item.span,
            params: Vec::new(),
            receiver: None,
            ret: crate::optres::result_ty(Ty::Unit, crate::optres::error_ty()),
            ret_span: None,
            decl: None,
            body: None,
            deps: Vec::new(),
            generics: Vec::new(),
            self_ty: None,
            host: None,
            module,
            public: false,
        });
        self.tests.push(TestInfo {
            f,
            decl,
            span: item.span,
            module,
        });
    }

    fn check_test(&mut self, t: usize) {
        let (f, decl, span, module) = {
            let t = &self.tests[t];
            (t.f, t.decl, t.span, t.module)
        };
        let info = &self.fns[f.0 as usize];
        let (name, ret) = (info.name.clone(), info.ret.clone());
        let back = self.enter_module(module);
        let (body, deps) = FnCtx::new(self, name, Some((ret.clone(), None)))
            .check_test_body(&decl.body, &ret, span);
        self.enter_module(back);
        let info = &mut self.fns[f.0 as usize];
        info.body = Some(Rc::new(body));
        info.deps = deps;
    }

    fn check_script(&mut self) {
        let steps = self.script.clone();
        let span = self.file.span;
        let back = self.enter_module(self.entry_mod());
        let (body, deps) =
            FnCtx::new(self, "<script>".to_string(), None).check_script(&steps, span);
        self.enter_module(back);
        self.script_body = Some(Rc::new(body));
        self.script_deps = deps;
    }

    /// The entry file's `fn main`, if it has one.
    fn entry_main(&self) -> Option<FnId> {
        let entry = self.entry_mod();
        match self.mods[entry.0 as usize]
            .exports
            .values
            .get("main")?
            .target
        {
            ValueRef::Fn(id) => Some(id),
            ValueRef::Global(_) => None,
        }
    }

    /// `fn main` must be callable with no arguments and return nothing.
    fn check_main(&mut self) -> Option<FnId> {
        let id = self.entry_main()?;
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
    /// the chain; recursion between functions alone is fine. The values in `script` are left
    /// out: the script initialises them in source order (see [`Checker::check_script_order`]).
    fn init_order(&mut self, script: &[GlobalId]) -> Vec<GlobalId> {
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
        // Done already: they are not ordered here, and a use of one is checked elsewhere.
        for g in script {
            state[node_index(Dep::Global(*g))] = State::Done;
        }
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
                let Some(&(dep, _)) = deps.get(next) else {
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

    /// In a script, every value is initialised when execution reaches its line, so a step must
    /// not use (directly or through the functions it refers to) a script value declared at or
    /// after it, and a value initialised before the script must not use any (DESIGN §4.2).
    fn check_script_order(&mut self) {
        let mut position: HashMap<GlobalId, usize> = HashMap::new();
        for (i, step) in self.script.iter().enumerate() {
            if let ScriptStep::Global(g) = step {
                position.insert(*g, i);
            }
        }
        if position.is_empty() {
            return;
        }
        let mut reported: HashSet<GlobalId> = HashSet::new();
        let script_deps = std::mem::take(&mut self.script_deps);
        for (i, deps) in script_deps.iter().enumerate() {
            for &(dep, span) in deps {
                let later = |g: GlobalId| position.get(&g).is_some_and(|&j| j >= i);
                let Some(path) = self.reach_global(dep, span, &later) else {
                    continue;
                };
                let g = path.last().map(|&(d, _)| d);
                let Some(Dep::Global(g)) = g else { continue };
                if reported.insert(g) {
                    let own = position.get(&g) == Some(&i);
                    self.report_early_use(&path, None, own);
                }
            }
        }
        self.script_deps = script_deps;
        for g in 0..self.globals.len() as u32 {
            let g = GlobalId(g);
            if position.contains_key(&g) {
                continue;
            }
            let deps = self.globals[g.0 as usize].deps.clone();
            for (dep, span) in deps {
                let in_script = |g: GlobalId| position.contains_key(&g);
                if let Some(path) = self.reach_global(dep, span, &in_script) {
                    self.report_early_use(&path, Some(g), false);
                    break;
                }
            }
        }
    }

    /// The shortest path from `start` (used at `span`) through functions to a value for which
    /// `wanted` holds: each item with the span where the one before it uses it. Reading a value
    /// does not run its initialiser, so the search does not go through values.
    fn reach_global(
        &self,
        start: Dep,
        span: Span,
        wanted: &dyn Fn(GlobalId) -> bool,
    ) -> Option<Vec<(Dep, Span)>> {
        let mut parent: HashMap<Dep, Option<usize>> = HashMap::new();
        let mut queue: Vec<(Dep, Span, Option<usize>)> = vec![(start, span, None)];
        parent.insert(start, None);
        let mut head = 0;
        while head < queue.len() {
            let (node, _, _) = queue[head];
            match node {
                Dep::Global(g) if wanted(g) => {
                    let mut path = Vec::new();
                    let mut at = Some(head);
                    while let Some(k) = at {
                        path.push((queue[k].0, queue[k].1));
                        at = queue[k].2;
                    }
                    path.reverse();
                    return Some(path);
                }
                Dep::Global(_) => {}
                Dep::Fn(f) => {
                    for &(next, use_span) in &self.fns[f.0 as usize].deps {
                        if let std::collections::hash_map::Entry::Vacant(e) = parent.entry(next) {
                            e.insert(Some(head));
                            queue.push((next, use_span, Some(head)));
                        }
                    }
                }
            }
            head += 1;
        }
        None
    }

    /// Reports a use of a script value before its line runs. `path` leads from what the step
    /// (or the initialiser of `early`, a value initialised before the script) uses to the
    /// value; `own` means the step is the value's own line.
    fn report_early_use(&mut self, path: &[(Dep, Span)], early: Option<GlobalId>, own: bool) {
        let Some(&(Dep::Global(g), _)) = path.last() else {
            return;
        };
        let use_span = path[0].1;
        let info = &self.globals[g.0 as usize];
        let (name, decl) = (info.name.clone(), info.name_span);
        let through: Vec<String> = path[..path.len() - 1]
            .iter()
            .map(|&(d, _)| match d {
                Dep::Fn(f) => format!("`{}()`", self.fns[f.0 as usize].name),
                Dep::Global(g) => format!("`{}`", self.globals[g.0 as usize].name),
            })
            .collect();
        let via = if through.is_empty() {
            String::new()
        } else {
            format!(" (through {})", through.join(" -> "))
        };
        let mut diag = match early {
            None if own => Diagnostic::error(
                format!("`{name}` is used in its own initialiser{via}"),
                use_span,
            )
            .with_label(decl, format!("`{name}` is initialised here"))
            .with_hint(format!(
                "`{name}` has no value until its initialiser finishes; compute it without reading `{name}`"
            )),
            None => Diagnostic::error(
                format!("`{name}` is used before it is initialised{via}"),
                use_span,
            )
            .with_label(decl, format!("`{name}` is initialised here, further down"))
            .with_hint(format!(
                "a script runs top to bottom; move `{name}` above its first use, or move this code into a function that is called after it"
            )),
            Some(e) => {
                let early_name = self.globals[e.0 as usize].name.clone();
                Diagnostic::error(
                    format!(
                        "`{early_name}` is initialised before the script runs, so it cannot use `{name}`{via}"
                    ),
                    use_span,
                )
                .with_label(decl, format!("`{name}` is initialised by the script, here"))
                .with_hint(
                    "imported modules' values and `const`s are initialised before the script's first line; compute this inside a function instead",
                )
            }
        };
        if path.len() > 1 {
            let &(_, last_use) = path.last().unwrap_or(&path[0]);
            diag = diag.with_label(last_use, format!("`{name}` is used here"));
        }
        self.report(diag);
    }

    // ---- output ----

    fn assemble(
        &mut self,
        init_order: Vec<GlobalId>,
        script_from: usize,
        main: Option<FnId>,
    ) -> Option<Program> {
        let mut fns = Vec::new();
        let mut host_fns: Vec<(HostFnId, HostFnDef)> = Vec::new();
        for (i, f) in self.fns.iter().enumerate() {
            let body = match &f.host {
                Some(h) => {
                    host_fns.push((
                        h.id,
                        HostFnDef {
                            name: f.name.clone(),
                            module: self.mods[f.module.0 as usize].name.clone(),
                            f: FnId(i as u32),
                            requires: h.requires.clone(),
                            ext: self.host_ext.get(&h.id.0).cloned(),
                        },
                    ));
                    Rc::new(host_stub_body(f))
                }
                None => f.body.clone()?,
            };
            fns.push(FnDef {
                name: f.name.clone(),
                sig: FnSig {
                    params: f.params.iter().map(|p| p.ty.clone()).collect(),
                    ret: f.ret.clone(),
                },
                span: f.span,
                body,
                type_params: f.generics.len() as u32,
                host: f.host.as_ref().map(|h| h.id),
            });
        }
        host_fns.sort_by_key(|(id, _)| id.0);
        let host_fns = host_fns.into_iter().map(|(_, d)| d).collect();
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
            script_from,
            script: self.script_body.clone()?,
            main,
            tests: self.assemble_tests(),
            types: self.types.clone(),
            host_fns,
            host_types: self.host_type_count,
            c_layouts: self.c_layouts.clone(),
        })
    }

    fn assemble_tests(&self) -> Vec<TestDef> {
        self.tests
            .iter()
            .map(|t| {
                let m = &self.mods[t.module.0 as usize];
                TestDef {
                    name: t.decl.name.clone(),
                    module: m.name.clone(),
                    root: m.root,
                    f: t.f,
                    span: t.span,
                }
            })
            .collect()
    }

    /// The module the program starts from.
    pub fn entry_mod(&self) -> ModId {
        ModId(self.mods.len() as u32 - 1)
    }

    /// Names in scope at module level, for "did you mean".
    pub fn module_names(&self) -> impl Iterator<Item = &str> {
        self.values
            .keys()
            .map(String::as_str)
            .chain(BUILTIN_FNS.iter().copied())
    }
}
