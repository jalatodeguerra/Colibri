//! Modules, packages and imports inside the checker (ARCHITECTURE §11.3).
//!
//! Every module has a [`Scope`]: the names it can use. The scope of the module being checked is
//! kept in the `Checker`'s own fields (`values`, `type_ids`, ...), so every existing lookup
//! works unchanged; [`Checker::enter_module`] swaps another module's tables in. A qualified name
//! (`player.spawn`) is a scope entry whose name contains a dot.

use std::collections::{HashMap, HashSet};

use colibri_syntax::ast::{self, ExprKind, ImportKind, ItemKind, NodeId, TopLevel};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::checker::{Checker, ValueRef};
use crate::hir::FnId;
use crate::package::ImportTarget;
use crate::suggest;
use crate::types::{Ty, TypeId};

/// A module the checker knows: index 0 is the base (prelude and the default host module).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ModId(pub u32);

/// The module of the prelude and the default `host` module.
pub(crate) const BASE: ModId = ModId(0);

/// A type alias, by index into `Checker::alias_infos`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AliasId(pub u32);

/// What a type name refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TypeEntity {
    Adt(TypeId),
    Alias(AliasId),
    /// A vector type, exported by the `vec` module of the standard library.
    Vec(crate::types::VecTy),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ModKind {
    Base,
    /// A named host module (`import physics`).
    Host,
    Source,
}

/// The names a module can use.
#[derive(Clone, Default)]
pub(crate) struct Scope {
    pub values: HashMap<String, ValueRef>,
    pub type_ids: HashMap<String, TypeId>,
    pub aliases: HashMap<String, AliasId>,
    pub vec_types: HashMap<String, crate::types::VecTy>,
    pub poisoned_types: HashSet<String>,
    /// Imported modules, by the name they are used under.
    pub modules: HashMap<String, ModId>,
    /// Where a name that was imported came from, for messages about clashes.
    pub origins: HashMap<String, String>,
}

/// An item a module declares.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Export<T> {
    pub target: T,
    /// Marked `pub`: visible outside the package.
    pub public: bool,
}

#[derive(Default)]
pub(crate) struct Exports {
    pub values: HashMap<String, Export<ValueRef>>,
    pub types: HashMap<String, Export<TypeEntity>>,
}

pub(crate) struct ModInfo<'a> {
    /// `game.player`, or the host module's name.
    pub name: String,
    pub kind: ModKind,
    /// The syntax trees of its items: one for a source module, any number for a host module.
    pub files: Vec<&'a ast::File>,
    /// The package (folder) a source module is in; `None` for host modules and the base.
    pub package: Option<usize>,
    pub entry: bool,
    /// An unimported module of the package (see `ModuleMeta::script`).
    pub script: bool,
    /// Not a module of a dependency: the program's own (false for a dependency's).
    pub root: bool,
    pub imports: Option<&'a HashMap<NodeId, ImportTarget>>,
    /// The scope while the module is not the one being checked.
    pub scope: Scope,
    pub exports: Exports,
    /// Where each `import` bound a module name, for clash messages.
    pub alias_spans: HashMap<String, Span>,
    /// The module's package may contain `extern` blocks (`ffi = true` in its manifest).
    pub ffi: bool,
}

impl<'a> ModInfo<'a> {
    pub fn items(&self) -> Vec<&'a ast::Item> {
        self.files
            .iter()
            .flat_map(|f| f.items.iter())
            .filter_map(|top| match top {
                TopLevel::Item(item) => Some(item),
                TopLevel::Stmt(_) => None,
            })
            .collect()
    }
}

/// An import of the module being checked, resolved.
struct BoundImport<'a> {
    import: &'a ast::Import,
    target: ModId,
}

impl<'a> Checker<'a> {
    /// Makes `m` the module being checked; returns the one that was. Pass the result to
    /// [`Checker::enter_module`] again to go back.
    pub(crate) fn enter_module(&mut self, m: ModId) -> ModId {
        let prev = self.cur_mod;
        if prev != m {
            self.swap_scope(prev);
            self.swap_scope(m);
            self.cur_mod = m;
        }
        prev
    }

    fn swap_scope(&mut self, m: ModId) {
        let s = &mut self.mods[m.0 as usize].scope;
        std::mem::swap(&mut self.values, &mut s.values);
        std::mem::swap(&mut self.type_ids, &mut s.type_ids);
        std::mem::swap(&mut self.aliases, &mut s.aliases);
        std::mem::swap(&mut self.vec_types, &mut s.vec_types);
        std::mem::swap(&mut self.poisoned_types, &mut s.poisoned_types);
        std::mem::swap(&mut self.mod_aliases, &mut s.modules);
        std::mem::swap(&mut self.origins, &mut s.origins);
    }

    /// A copy of the scope being used (the base, when modules are created).
    pub(crate) fn snapshot_scope(&self) -> Scope {
        Scope {
            values: self.values.clone(),
            type_ids: self.type_ids.clone(),
            aliases: self.aliases.clone(),
            vec_types: self.vec_types.clone(),
            poisoned_types: self.poisoned_types.clone(),
            modules: self.mod_aliases.clone(),
            origins: self.origins.clone(),
        }
    }

    /// The package of module `m`, if it is a source module.
    fn package_of(&self, m: ModId) -> Option<usize> {
        self.mods[m.0 as usize].package
    }

    /// Whether code of the current module may use what module `owner` declares, given whether
    /// it is `pub`. The base and host modules export everything.
    pub(crate) fn can_see(&self, owner: ModId, public: bool) -> bool {
        public
            || self.mods[owner.0 as usize].kind != ModKind::Source
            || self.package_of(owner) == self.package_of(self.cur_mod)
    }

    pub(crate) fn package_name(&self, m: ModId) -> String {
        self.package_of(m)
            .and_then(|p| self.package_names.get(p).cloned())
            .unwrap_or_else(|| self.mods[m.0 as usize].name.clone())
    }

    // ---- declaring ----

    /// Records that the current module declares a value.
    pub(crate) fn export_value(&mut self, name: &str, target: ValueRef, public: bool) {
        let m = self.cur_mod.0 as usize;
        self.mods[m]
            .exports
            .values
            .entry(name.to_string())
            .or_insert(crate::modules::Export { target, public });
    }

    /// Records that the current module declares a type or alias.
    pub(crate) fn export_type(&mut self, name: &str, target: TypeEntity, public: bool) {
        let m = self.cur_mod.0 as usize;
        self.mods[m]
            .exports
            .types
            .entry(name.to_string())
            .or_insert(crate::modules::Export { target, public });
    }

    // ---- imports ----

    fn bound_imports(&mut self, m: ModId, report_missing: bool) -> Vec<BoundImport<'a>> {
        let info = &self.mods[m.0 as usize];
        let Some(map) = info.imports else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut missing: Vec<(&'a ast::Import, Span)> = Vec::new();
        for item in info.items() {
            let ItemKind::Import(import) = &item.kind else {
                continue;
            };
            let target = match map.get(&item.id) {
                Some(ImportTarget::Module(i)) => ModId(self.source_base + *i as u32),
                Some(ImportTarget::Host(name)) => {
                    match self
                        .mods
                        .iter()
                        .position(|x| x.kind == ModKind::Host && x.name == *name)
                    {
                        Some(i) => ModId(i as u32),
                        None => continue,
                    }
                }
                // The loader reported why.
                Some(ImportTarget::Failed) => continue,
                None => {
                    missing.push((import, item.span));
                    continue;
                }
            };
            out.push(BoundImport { import, target });
        }
        if report_missing {
            for (import, span) in missing {
                let path = Self::import_path(import);
                self.report(
                    Diagnostic::error(format!("no module `{path}`"), span)
                        .with_hint("this program was checked without a folder to find modules in"),
                );
            }
        }
        out
    }

    /// The dotted path of an import, for messages.
    fn import_path(import: &ast::Import) -> String {
        import
            .path
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>()
            .join(".")
    }

    /// The modules a host module's interface says it uses (`uses = ["std.json"]` in its
    /// header): their types are written `json.Value` in it, so each is bound under its last
    /// name segment, the way `import json` would, but only for types.
    pub(crate) fn bind_host_uses(&mut self, m: ModId) {
        if self.mods[m.0 as usize].kind != ModKind::Host {
            return;
        }
        let module = self.mods[m.0 as usize].name.clone();
        let host: &'a crate::host::HostEnv = self.host;
        let saved = self.enter_module(m);
        for used in host.uses_of(&module) {
            let target = self
                .mods
                .iter()
                .position(|x| x.kind == ModKind::Host && x.name == used);
            let Some(target) = target else {
                // `HostEnv::missing_uses` tells the host before it gets here; a checker used
                // without that check says so on the module's first line.
                let at = colibri_syntax::Span::new(crate::host::HOST_FILE, 0, 0);
                self.report(Diagnostic::error(
                    format!(
                        "the interface of the module `{module}` names types of the module `{used}`, whose interface was not given"
                    ),
                    at,
                ));
                continue;
            };
            let target = ModId(target as u32);
            let short = used.rsplit('.').next().unwrap_or(&used).to_string();
            let alias = ast::Ident {
                name: short.clone(),
                span: colibri_syntax::Span::new(crate::host::HOST_FILE, 0, 0),
            };
            self.bind_module_alias(&alias, target, &used);
            for (name, exp) in self.sorted_type_exports(target) {
                self.insert_type(&format!("{short}.{name}"), exp.target);
            }
        }
        self.enter_module(saved);
    }

    /// Imports, first pass: binds module names and brings in types (and aliases), which are all
    /// declared by now. Values come in [`Checker::bind_imports_values`].
    pub(crate) fn bind_imports_types(&mut self, m: ModId) {
        let saved = self.enter_module(m);
        for imp in self.bound_imports(m, true) {
            let path = Self::import_path(imp.import);
            match &imp.import.kind {
                ImportKind::Plain | ImportKind::Alias(_) => {
                    let alias = match &imp.import.kind {
                        ImportKind::Alias(a) => a.clone(),
                        _ => imp
                            .import
                            .path
                            .last()
                            .cloned()
                            .expect("a path has a segment"),
                    };
                    self.bind_module_alias(&alias, imp.target, &path);
                    for (name, exp) in self.sorted_type_exports(imp.target) {
                        if self.can_see(imp.target, exp.public) {
                            self.insert_type(&format!("{}.{name}", alias.name), exp.target);
                        }
                    }
                }
                ImportKind::Names(names) => {
                    for n in names {
                        let bound = n.alias.as_ref().unwrap_or(&n.name);
                        let found = self.mods[imp.target.0 as usize]
                            .exports
                            .types
                            .get(&n.name.name)
                            .copied();
                        if let Some(exp) = found
                            && self.can_see(imp.target, exp.public)
                        {
                            let what = format!("`{}` imported from `{path}`", n.name.name);
                            self.bind_type_name(bound, exp.target, what);
                        }
                    }
                }
            }
        }
        self.enter_module(saved);
    }

    /// Imports, second pass: values, and the errors about names the module does not have.
    pub(crate) fn bind_imports_values(&mut self, m: ModId) {
        let saved = self.enter_module(m);
        for imp in self.bound_imports(m, false) {
            let path = Self::import_path(imp.import);
            match &imp.import.kind {
                ImportKind::Plain | ImportKind::Alias(_) => {
                    let alias = match &imp.import.kind {
                        ImportKind::Alias(a) => a.clone(),
                        _ => imp
                            .import
                            .path
                            .last()
                            .cloned()
                            .expect("a path has a segment"),
                    };
                    // The module name was bound in the first pass; a value of the same name
                    // declared since is a clash.
                    if let Some(&ValueRef::Fn(_) | &ValueRef::Global(_)) =
                        self.values.get(&alias.name)
                        && self.mod_aliases.get(&alias.name) == Some(&imp.target)
                    {
                        let existing = self.describe_name(&alias.name);
                        let new = format!("the module `{path}` imported as `{}`", alias.name);
                        self.report_ambiguous(&alias.name, alias.span, &existing, &new);
                    }
                    let mut exports: Vec<(String, ValueRef, bool)> = self.mods
                        [imp.target.0 as usize]
                        .exports
                        .values
                        .iter()
                        .map(|(n, e)| (n.clone(), e.target, e.public))
                        .collect();
                    exports.sort_by(|a, b| a.0.cmp(&b.0));
                    for (name, target, public) in exports {
                        if self.can_see(imp.target, public) {
                            self.values.insert(format!("{}.{name}", alias.name), target);
                        }
                    }
                }
                ImportKind::Names(names) => {
                    for n in names {
                        let bound = n.alias.as_ref().unwrap_or(&n.name);
                        let value = self.mods[imp.target.0 as usize]
                            .exports
                            .values
                            .get(&n.name.name)
                            .copied();
                        let ty = self.mods[imp.target.0 as usize]
                            .exports
                            .types
                            .get(&n.name.name)
                            .copied();
                        let mut visible = false;
                        if let Some(exp) = value
                            && self.can_see(imp.target, exp.public)
                        {
                            visible = true;
                            let what = format!("`{}` imported from `{path}`", n.name.name);
                            self.bind_value_name(bound, exp.target, what);
                        }
                        if let Some(exp) = ty
                            && self.can_see(imp.target, exp.public)
                        {
                            visible = true;
                        }
                        if !visible {
                            self.report_missing_name(
                                imp.target,
                                &path,
                                &n.name,
                                value.is_some() || ty.is_some(),
                            );
                        }
                    }
                }
            }
        }
        self.enter_module(saved);
    }

    fn sorted_type_exports(&self, m: ModId) -> Vec<(String, Export<TypeEntity>)> {
        let mut v: Vec<(String, Export<TypeEntity>)> = self.mods[m.0 as usize]
            .exports
            .types
            .iter()
            .map(|(n, e)| (n.clone(), *e))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    fn insert_type(&mut self, name: &str, entity: TypeEntity) {
        match entity {
            TypeEntity::Adt(id) => {
                self.type_ids.insert(name.to_string(), id);
            }
            TypeEntity::Alias(id) => {
                self.aliases.insert(name.to_string(), id);
            }
            TypeEntity::Vec(v) => {
                self.vec_types.insert(name.to_string(), v);
            }
        }
    }

    /// What a name in the scope stands for, in words, for a clash message.
    fn describe_name(&self, name: &str) -> String {
        if let Some(origin) = self.origins.get(name) {
            return origin.clone();
        }
        let own = self.mods[self.cur_mod.0 as usize]
            .exports
            .values
            .contains_key(name)
            || self.mods[self.cur_mod.0 as usize]
                .exports
                .types
                .contains_key(name);
        if own {
            format!("`{name}` defined in this module")
        } else {
            format!("the built-in `{name}`")
        }
    }

    fn report_ambiguous(&mut self, name: &str, span: Span, existing: &str, new: &str) {
        self.report(
            Diagnostic::error(
                format!("the name `{name}` is ambiguous: it could be {existing}, or {new}"),
                span,
            )
            .with_hint(
                "rename one of them with `as`, or import the module and write `module.name`",
            ),
        );
    }

    /// Binds `import a.b` (as `alias`).
    fn bind_module_alias(&mut self, alias: &ast::Ident, target: ModId, path: &str) {
        if self.mod_aliases.get(&alias.name) == Some(&target) {
            return;
        }
        let clash = self.type_ids.contains_key(&alias.name)
            || self.aliases.contains_key(&alias.name)
            || self.vec_types.contains_key(&alias.name)
            || self.mod_aliases.contains_key(&alias.name);
        if clash {
            let existing = self.describe_name(&alias.name);
            let new = format!("the module `{path}` imported as `{}`", alias.name);
            self.report_ambiguous(&alias.name, alias.span, &existing, &new);
            return;
        }
        self.mod_aliases.insert(alias.name.clone(), target);
        self.origins.insert(
            alias.name.clone(),
            format!("the module `{path}` imported as `{}`", alias.name),
        );
        let cur = self.cur_mod.0 as usize;
        self.mods[cur]
            .alias_spans
            .insert(alias.name.clone(), alias.span);
    }

    /// Binds a name brought in by `import a.{X}` in the type namespace.
    fn bind_type_name(&mut self, bound: &ast::Ident, entity: TypeEntity, what: String) {
        let same = match entity {
            TypeEntity::Adt(id) => self.type_ids.get(&bound.name) == Some(&id),
            TypeEntity::Alias(id) => self.aliases.get(&bound.name) == Some(&id),
            TypeEntity::Vec(v) => self.vec_types.get(&bound.name) == Some(&v),
        };
        if same {
            return;
        }
        let clash = self.type_ids.contains_key(&bound.name)
            || self.aliases.contains_key(&bound.name)
            || self.vec_types.contains_key(&bound.name)
            || self.mod_aliases.contains_key(&bound.name);
        if clash {
            let existing = self.describe_name(&bound.name);
            self.report_ambiguous(&bound.name, bound.span, &existing, &what);
            return;
        }
        self.insert_type(&bound.name, entity);
        self.origins.insert(bound.name.clone(), what);
    }

    /// Binds a name brought in by `import a.{f}` in the value namespace.
    fn bind_value_name(&mut self, bound: &ast::Ident, target: ValueRef, what: String) {
        let same = match (self.values.get(&bound.name), target) {
            (Some(ValueRef::Fn(a)), ValueRef::Fn(b)) => *a == b,
            (Some(ValueRef::Global(a)), ValueRef::Global(b)) => *a == b,
            _ => false,
        };
        if same {
            return;
        }
        if self.values.contains_key(&bound.name) || self.mod_aliases.contains_key(&bound.name) {
            let existing = self.describe_name(&bound.name);
            self.report_ambiguous(&bound.name, bound.span, &existing, &what);
            return;
        }
        self.values.insert(bound.name.clone(), target);
        self.origins.insert(bound.name.clone(), what);
    }

    /// `import a.{X}` where `a` has no (visible) `X`.
    fn report_missing_name(&mut self, target: ModId, path: &str, name: &ast::Ident, exists: bool) {
        let info = &self.mods[target.0 as usize];
        if exists {
            let package = self.package_name(target);
            let diag = Diagnostic::error(
                format!("`{}` is private to the package `{package}`", name.name),
                name.span,
            )
            .with_hint(format!(
                "mark it `pub` in `{path}` to export it from the package"
            ));
            self.report(diag);
            return;
        }
        let candidates: Vec<&str> = info
            .exports
            .values
            .iter()
            .map(|(n, e)| (n, e.public))
            .chain(info.exports.types.iter().map(|(n, e)| (n, e.public)))
            .filter(|(_, public)| *public || info.kind != ModKind::Source)
            .map(|(n, _)| n.as_str())
            .collect();
        let mut diag = Diagnostic::error(
            format!("the module `{path}` has no `{}`", name.name),
            name.span,
        );
        if let Some(hint) = removed_item_hint(path, &name.name) {
            diag = diag.with_hint(hint);
        } else if let Some(s) = suggest::closest(&name.name, candidates.iter().copied()) {
            diag = diag.with_hint(format!("did you mean `{s}`?"));
        } else if candidates.is_empty() {
            diag = diag.with_hint("it exports nothing; mark what should be visible with `pub`");
        }
        self.report(diag);
    }

    // ---- qualified names that were not found ----

    /// A diagnostic for `alias.member` that is not in scope: the module has no such item, or
    /// has it but only inside its package.
    pub(crate) fn unknown_member(
        &self,
        alias: &str,
        member: &str,
        span: Span,
    ) -> Option<Diagnostic> {
        let &m = self.mod_aliases.get(alias)?;
        let info = &self.mods[m.0 as usize];
        let hidden = info
            .exports
            .values
            .get(member)
            .map(|e| e.public)
            .or_else(|| info.exports.types.get(member).map(|e| e.public));
        if hidden == Some(false) {
            return Some(
                Diagnostic::error(
                    format!(
                        "`{alias}.{member}` is private to the package `{}`",
                        self.package_name(m)
                    ),
                    span,
                )
                .with_hint(format!(
                    "mark `{member}` with `pub` in `{}` to export it from the package",
                    info.name
                )),
            );
        }
        let visible: Vec<&str> = info
            .exports
            .values
            .iter()
            .map(|(n, e)| (n, e.public))
            .chain(info.exports.types.iter().map(|(n, e)| (n, e.public)))
            .filter(|(_, public)| *public || self.can_see(m, false))
            .map(|(n, _)| n.as_str())
            .collect();
        let mut diag = Diagnostic::error(
            format!("the module `{}` has no `{member}`", info.name),
            span,
        );
        if let Some(hint) = removed_item_hint(&info.name, member) {
            diag = diag.with_hint(hint);
        } else if let Some(s) = suggest::closest(member, visible.iter().copied()) {
            diag = diag.with_hint(format!("did you mean `{alias}.{s}`?"));
        }
        Some(diag)
    }

    /// `alias.member` split at the first dot, if `alias` is an imported module.
    pub(crate) fn split_qualified<'n>(&self, name: &'n str) -> Option<(&'n str, &'n str)> {
        let (alias, member) = name.split_once('.')?;
        self.mod_aliases
            .contains_key(alias)
            .then_some((alias, member))
    }
}

/// What replaced a standard library function that was removed, for the diagnostic of a
/// program that still uses it. `module` is the module as written (`fs`, `std.fs`).
fn removed_item_hint(module: &str, item: &str) -> Option<&'static str> {
    let module = module.strip_prefix("std.").unwrap_or(module);
    Some(match (module, item) {
        ("fs", "size") => "`fs.size` was removed: use `fs.stat(path)?.size`",
        ("fs", "modified") => "`fs.modified` was removed: use `fs.stat(path)?.modified`",
        ("fs", "remove_empty_dir") => "`fs.remove_empty_dir` was renamed: use `fs.remove_dir`",
        ("str", "pad_left") => "`str.pad_left` was removed: use the method, `s.pad_left(width)`",
        ("str", "pad_right") => "`str.pad_right` was removed: use the method, `s.pad_right(width)`",
        _ => return None,
    })
}

// ---- visibility of fields and methods ----

impl Checker<'_> {
    /// Whether the code being checked may use field `i` of the struct or class `ty`: it is
    /// `pub`, or the code is in the type's package.
    pub(crate) fn field_visible(&self, ty: TypeId, i: usize) -> bool {
        let extra = &self.type_extra[ty.0 as usize];
        let Some(owner) = extra.module else {
            return true;
        };
        let public = extra.field_public.get(i).copied().unwrap_or(true);
        self.can_see(owner, public)
    }

    /// Whether the code being checked may call the method or static function `f`.
    pub(crate) fn fn_visible(&self, f: FnId) -> bool {
        let info = &self.fns[f.0 as usize];
        self.can_see(info.module, info.public)
    }

    /// The package of the module that declares type `ty`, for messages.
    pub(crate) fn type_package(&self, ty: TypeId) -> String {
        self.package_name(self.type_extra[ty.0 as usize].module.unwrap_or(BASE))
    }

    /// `what` (a field or method) of `ty` is not exported from its package.
    pub(crate) fn report_private(&mut self, what: &str, name: &str, ty: TypeId, span: Span) {
        let tname = self.types[ty.0 as usize].display_name().into_owned();
        let package = self.type_package(ty);
        self.report(
            Diagnostic::error(
                format!("the {what} `{name}` of `{tname}` is private to the package `{package}`"),
                span,
            )
            .with_hint(format!(
                "mark it `pub` where `{tname}` is declared, or use what `{tname}` exports"
            )),
        );
    }

    /// The exported functions that build a `ty`, for the hint about a private field: functions
    /// of its module and static functions of its own, written the way a caller would.
    pub(crate) fn constructors_of(&self, ty: TypeId) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let returns_ty = |ret: &Ty| matches!(ret, Ty::Adt(id, _) if *id == ty);
        if let Some(owner) = self.type_extra[ty.0 as usize].module {
            let module = &self.mods[owner.0 as usize];
            let short = module
                .name
                .rsplit('.')
                .next()
                .unwrap_or(&module.name)
                .to_string();
            for (name, exp) in &module.exports.values {
                if let ValueRef::Fn(f) = exp.target
                    && exp.public
                    && returns_ty(&self.fns[f.0 as usize].ret)
                {
                    out.push(format!("{short}.{name}(..)"));
                }
            }
        }
        let tname = &self.types[ty.0 as usize].name;
        for ((owner, name), f) in &self.methods {
            let info = &self.fns[f.0 as usize];
            if *owner == ty && info.public && info.receiver.is_none() && returns_ty(&info.ret) {
                out.push(format!("{tname}.{name}(..)"));
            }
        }
        out.sort();
        out
    }
}

impl FnCtx<'_, '_> {
    /// `module.name` where `module` is an imported module (and not a variable of that name):
    /// the name as the scope knows it, `"module.name"`.
    pub(crate) fn qualified_parts(
        &self,
        base: &ast::Expr,
        name: &ast::Ident,
    ) -> Option<ast::Ident> {
        let ExprKind::Ident(alias) = &base.kind else {
            return None;
        };
        if self.is_local(&alias.name) || !self.ck.mod_aliases.contains_key(&alias.name) {
            return None;
        }
        Some(ast::Ident {
            name: format!("{}.{}", alias.name, name.name),
            span: alias.span.to(name.span),
        })
    }

    /// Like [`FnCtx::qualified_parts`] for a whole expression.
    pub(crate) fn qualified(&self, e: &ast::Expr) -> Option<ast::Ident> {
        match &e.kind {
            ExprKind::Field { base, name } => self.qualified_parts(base, name),
            ExprKind::Paren(inner) => self.qualified(inner),
            _ => None,
        }
    }
}
