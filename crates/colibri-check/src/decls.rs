//! Collecting user-defined types (structs, enums, aliases) and `impl` blocks.
//!
//! Types are collected in two passes so they may refer to each other in any order: first every
//! name is declared, then fields, payloads and aliases are resolved.

use colibri_syntax::ast::{self, ItemKind, StructKind, VariantPayload};
use colibri_syntax::{Diagnostic, Span};

use crate::checker::Checker;
use crate::generics::SigCtx;
use crate::hir::{FieldDef, TypeDef, TypeDefKind, VariantDef};
use crate::host::HOST_FILE;
use crate::modules::{AliasId, BASE, ModId, ModKind, TypeEntity};
use crate::suggest;
use crate::traits::GenericParamInfo;
use crate::types::{Ty, TypeId};

/// Names that cannot be redefined as types.
pub(crate) const BUILTIN_TYPES: &[&str] = &[
    "int", "float", "bool", "str", "char", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64",
    "f32", "f64", "List", "Map", "Set", "Option", "Result", "ptr", "cstr",
];

/// The classes the prelude declares: a program cannot declare its own.
const PRELUDE_CLASSES: &[&str] = &["Error", "Coroutine", "CoGroup", "Signal", "StrBuilder"];

/// What HIR does not keep about a user type.
#[derive(Clone, Default)]
pub(crate) struct TypeExtra<'a> {
    /// The default of each struct field, parallel to its fields.
    pub defaults: Vec<Option<&'a ast::Expr>>,
    /// The type parameters, in order (without bounds: a type's own parameters take none yet).
    pub generics: Vec<GenericParamInfo>,
    /// The module that declares the type: where its field types and defaults are resolved.
    pub module: Option<ModId>,
    /// Whether each field is `pub` (exported from the package), parallel to the fields.
    pub field_public: Vec<bool>,
}

pub(crate) enum AliasState<'a> {
    Pending(&'a ast::TypeExpr),
    Resolving,
    /// The target type, written with the alias's own type parameters.
    Done(Ty),
}

pub(crate) struct AliasInfo<'a> {
    pub name: String,
    pub module: ModId,
    pub name_span: Span,
    pub generics: Vec<GenericParamInfo>,
    pub state: AliasState<'a>,
}

impl<'a> Checker<'a> {
    /// Pass one: registers the name of every struct, enum, interface and alias.
    pub(crate) fn declare_types(&mut self, items: &[&'a ast::Item]) {
        for item in items {
            match &item.kind {
                ItemKind::Struct(s) => {
                    let what = match s.kind {
                        StructKind::Struct => "struct",
                        StructKind::Class => "class",
                    };
                    if let Some(id) = self.declare_type(&s.name, what, &s.generics, item.public) {
                        let fields = Vec::new();
                        let host = item.span.file == HOST_FILE;
                        self.types[id.0 as usize].kind = match s.kind {
                            StructKind::Struct => TypeDefKind::Struct { fields },
                            StructKind::Class if host => TypeDefKind::HostClass,
                            StructKind::Class => TypeDefKind::Class { fields },
                        };
                        // Objects of the host's classes stay on the thread of their `Vm`.
                        self.types[id.0 as usize].sendable = !(host && s.kind == StructKind::Class);
                        self.note_repr_attrs(id, item, s.kind == StructKind::Class);
                    }
                }
                ItemKind::Enum(e) => {
                    // `@module enum Error { .. }` in a host module's interface: the module's own
                    // enum (`fs.Error`), see `HostModule::register_module_enum`.
                    let own = item.attrs.iter().any(|a| a.name.name == "module")
                        && self.cur_mod != BASE
                        && self.mods[self.cur_mod.0 as usize].kind == ModKind::Host;
                    self.declaring_module_enum = own;
                    let declared = self.declare_type(&e.name, "enum", &e.generics, item.public);
                    self.declaring_module_enum = false;
                    if let Some(id) = declared {
                        self.types[id.0 as usize].kind = TypeDefKind::Enum {
                            variants: Vec::new(),
                        };
                        self.types[id.0 as usize].open = own;
                    }
                }
                ItemKind::Interface(i) => {
                    if let Some(id) =
                        self.declare_type(&i.name, "interface", &i.generics, item.public)
                    {
                        self.types[id.0 as usize].kind = TypeDefKind::Interface {
                            methods: Vec::new(),
                        };
                        self.declare_interface(id, i);
                    }
                }
                ItemKind::TypeAlias(t) if self.name_is_free(&t.name, "type alias") => {
                    let generics = self.type_params(&t.generics);
                    let id = AliasId(self.alias_infos.len() as u32);
                    self.alias_infos.push(AliasInfo {
                        name: t.name.name.clone(),
                        module: self.cur_mod,
                        name_span: t.name.span,
                        generics,
                        state: AliasState::Pending(&t.ty),
                    });
                    self.aliases.insert(t.name.name.clone(), id);
                    self.export_type(&t.name.name, TypeEntity::Alias(id), item.public);
                }
                _ => {}
            }
        }
    }

    /// Allocates the type parameters of a type or alias. Bounds on them are not supported
    /// yet: write them on the `impl` blocks and functions that need them.
    pub(crate) fn type_params(&mut self, decls: &[ast::GenericParam]) -> Vec<GenericParamInfo> {
        let mut out: Vec<GenericParamInfo> = Vec::new();
        for d in decls {
            if out.iter().any(|g| g.name == d.name.name) {
                self.report(Diagnostic::error(
                    format!(
                        "the type parameter `{}` is declared more than once",
                        d.name.name
                    ),
                    d.name.span,
                ));
                continue;
            }
            if let Some(b) = d.bounds.first() {
                self.unsupported("a bound on the type parameter of a type", b.span);
            }
            out.push(GenericParamInfo {
                id: self.new_param(),
                name: d.name.name.clone(),
                span: d.name.span,
                bounds: Vec::new(),
            });
        }
        out
    }

    /// The name of the named host module being declared, if the current module is one.
    fn declaring_host_module(&self) -> Option<String> {
        let info = &self.mods[self.cur_mod.0 as usize];
        (self.cur_mod != BASE && info.kind == ModKind::Host).then(|| info.name.clone())
    }

    /// Whether `name` can be declared as a type; reports a clash otherwise.
    fn name_is_free(&mut self, name: &ast::Ident, what: &str) -> bool {
        // A host module may declare its own `enum Error` (`fs.Error`, DESIGN §10.2): inside the
        // module it shadows the prelude's `Error`, which `Result[T]` still means. Nobody else
        // may reuse the name.
        let module_error = name.name == "Error" && what == "enum" && self.declaring_module_enum;
        if BUILTIN_TYPES.contains(&name.name.as_str())
            || (!module_error
                && !crate::prelude::in_prelude(name.span)
                && PRELUDE_CLASSES.contains(&name.name.as_str()))
        {
            self.report(Diagnostic::error(
                format!("`{}` is a built-in type and cannot be redefined", name.name),
                name.span,
            ));
            return false;
        }
        let previous = self
            .type_ids
            .get(&name.name)
            .filter(|&&id| {
                // A module's own enum shares its name with the prelude's `Error`.
                if self.declaring_module_enum && id == crate::types::ERROR_TYPE {
                    return false;
                }
                // A named host module's type is its own: it hides a type of the same name
                // that the default host module declares (the embedder's `Child` and
                // `process.Child`), inside that module. Everywhere else the two are told
                // apart by the module name.
                !(self.declaring_host_module().is_some()
                    && id.0 >= crate::types::FIRST_USER_TYPE
                    && self.type_extra[id.0 as usize].module == Some(BASE))
            })
            .map(|id| self.types[id.0 as usize].span)
            .or_else(|| {
                self.aliases
                    .get(&name.name)
                    .map(|a| self.alias_infos[a.0 as usize].name_span)
            });
        if previous.is_some() || self.poisoned_types.contains(&name.name) {
            let mut diag = Diagnostic::error(
                format!("the type `{}` is defined more than once", name.name),
                name.span,
            );
            if let Some(prev) = previous {
                diag = diag.with_label(prev, "first defined here");
            }
            self.report(diag.with_hint(format!("a {what} and a type share one namespace")));
            return false;
        }
        true
    }

    fn declare_type(
        &mut self,
        name: &ast::Ident,
        what: &str,
        generics: &[ast::GenericParam],
        public: bool,
    ) -> Option<TypeId> {
        if !self.name_is_free(name, what) {
            return None;
        }
        let generics = self.type_params(generics);
        let id = TypeId(self.types.len() as u32);
        self.types.push(TypeDef {
            name: name.name.clone(),
            span: name.span,
            params: generics.iter().map(|g| g.id).collect(),
            sendable: true,
            // What a named host module declares is that module's own.
            module: self.declaring_host_module(),
            open: false,
            kind: TypeDefKind::Struct { fields: Vec::new() },
        });
        self.type_extra.push(TypeExtra {
            defaults: Vec::new(),
            generics,
            module: Some(self.cur_mod),
            field_public: Vec::new(),
        });
        self.type_ids.insert(name.name.clone(), id);
        self.export_type(&name.name, TypeEntity::Adt(id), public);
        Some(id)
    }

    /// Pass two: resolves fields, payloads, interface signatures and aliases of one group of
    /// items that share a scope (the base, or one module).
    pub(crate) fn define_types(&mut self, items: &[&'a ast::Item]) {
        self.define_adts(items);
        self.define_interfaces(items);
        self.define_aliases();
        let mut from = self.infinite_checked;
        self.check_infinite_sizes(&mut from);
    }

    /// Structs and enums: fields and payloads.
    pub(crate) fn define_adts(&mut self, items: &[&'a ast::Item]) {
        for item in items {
            match &item.kind {
                ItemKind::Struct(s) => {
                    if let Some(id) = self.own_type(&s.name) {
                        let mark = self.enter_type_params(id);
                        self.define_struct(id, s);
                        self.leave_generics(mark);
                    }
                }
                ItemKind::Enum(e) => {
                    if let Some(id) = self.own_type(&e.name) {
                        let mark = self.enter_type_params(id);
                        self.define_enum(id, e);
                        self.leave_generics(mark);
                    }
                }
                _ => {}
            }
        }
    }

    /// Interfaces last: their signatures may name any type.
    pub(crate) fn define_interfaces(&mut self, items: &[&'a ast::Item]) {
        for item in items {
            if let ItemKind::Interface(i) = &item.kind
                && let Some(id) = self.own_type(&i.name)
            {
                self.define_interface(id, i);
            }
        }
    }

    /// Reports errors in aliases nobody uses, too.
    pub(crate) fn define_aliases(&mut self) {
        let mut ids: Vec<AliasId> = self.aliases.values().copied().collect();
        ids.sort_by_key(|a| a.0);
        for id in ids {
            self.alias_body(id);
        }
    }

    /// Checks the types declared since the last call (`from` is where to start; it moves).
    pub(crate) fn check_infinite_sizes(&mut self, from: &mut usize) {
        for t in *from..self.types.len() {
            self.check_infinite_size(TypeId(t as u32));
        }
        *from = self.types.len();
        self.infinite_checked = *from;
    }

    /// The type declared by exactly this name (not an earlier one it clashed with).
    fn own_type(&self, name: &ast::Ident) -> Option<TypeId> {
        let &id = self.type_ids.get(&name.name)?;
        (self.types[id.0 as usize].span == name.span).then_some(id)
    }

    /// Puts the type parameters of type `id` in scope.
    pub(crate) fn enter_type_params(&mut self, id: TypeId) -> usize {
        let generics = self.type_extra[id.0 as usize].generics.clone();
        self.enter_generics(&generics)
    }

    fn define_struct(&mut self, id: TypeId, s: &'a ast::StructDecl) {
        let is_class = s.kind == StructKind::Class;
        if self.types[id.0 as usize].is_host_class() {
            // Declared in the host interface, which has no fields to resolve.
            return;
        }
        let mut fields: Vec<FieldDef> = Vec::new();
        let mut defaults = Vec::new();
        let mut field_public = Vec::new();
        // Fields of a host struct belong to the host: all of them are visible.
        let host = self.types[id.0 as usize].span.file == HOST_FILE;
        for f in &s.fields {
            if let Some(prev) = fields.iter().find(|d| d.name == f.name.name) {
                let prev_span = prev.span;
                self.report(
                    Diagnostic::error(
                        format!("the field `{}` is declared more than once", f.name.name),
                        f.name.span,
                    )
                    .with_label(prev_span, "first declared here"),
                );
                continue;
            }
            let (ty, weak) = self.resolve_field_type(&f.ty, is_class);
            let (rename, skip) = self.field_attrs(f, &ty);
            fields.push(FieldDef {
                name: f.name.name.clone(),
                ty,
                span: f.name.span,
                weak,
                default: None,
                rename,
                skip,
            });
            defaults.push(f.default.as_ref());
            field_public.push(f.public || host);
        }
        self.check_field_keys(&fields);
        self.types[id.0 as usize].kind = if is_class {
            TypeDefKind::Class { fields }
        } else {
            TypeDefKind::Struct { fields }
        };
        self.type_extra[id.0 as usize].defaults = defaults;
        self.type_extra[id.0 as usize].field_public = field_public;
    }

    /// Reads `@rename("key")` and `@skip` on a field (DESIGN §7.5); anything else is not
    /// supported on a field. A skipped field must be buildable without the JSON: it needs a
    /// default value or an optional type (it is `none`).
    fn field_attrs(&mut self, f: &'a ast::Field, ty: &Ty) -> (Option<String>, bool) {
        let mut rename: Option<String> = None;
        let mut skip = false;
        for attr in &f.attrs {
            match attr.name.name.as_str() {
                "rename" => match attr.args.as_slice() {
                    [e] if crate::ffi::plain_string(e).is_some() => {
                        let key = crate::ffi::plain_string(e).unwrap_or_default();
                        if rename.is_some() {
                            self.report(Diagnostic::error("a field has one `@rename`", attr.span));
                        } else if key.is_empty() {
                            self.report(Diagnostic::error(
                                "`@rename` needs a name that is not empty",
                                attr.span,
                            ));
                        } else {
                            rename = Some(key);
                        }
                    }
                    _ => self.report(
                        Diagnostic::error("`@rename` takes the new name as a string", attr.span)
                            .with_hint("write `@rename(\"user_id\") id: int`"),
                    ),
                },
                "skip" => {
                    if !attr.args.is_empty() {
                        self.report(Diagnostic::error("`@skip` takes no arguments", attr.span));
                    }
                    skip = true;
                }
                other => {
                    self.unsupported(&format!("the attribute `@{other}` on a field"), attr.span)
                }
            }
        }
        if skip && f.default.is_none() && !matches!(ty, Ty::Option(_)) {
            self.report(
                Diagnostic::error(
                    format!(
                        "the `@skip` field `{}` needs a default value, because `json.decode` cannot read it",
                        f.name.name
                    ),
                    f.name.span,
                )
                .with_hint(format!(
                    "write `@skip {}: ... = <default>`, or make the type optional",
                    f.name.name
                )),
            );
        }
        if skip && rename.is_some() {
            self.report(Diagnostic::error(
                "a `@skip` field has no key, so `@rename` does nothing on it",
                f.name.span,
            ));
        }
        (rename, skip)
    }

    /// Two fields that serialize under the same key would overwrite each other.
    fn check_field_keys(&mut self, fields: &[FieldDef]) {
        for (i, f) in fields.iter().enumerate().filter(|(_, f)| !f.skip) {
            if let Some(prev) = fields[..i].iter().find(|p| !p.skip && p.key() == f.key()) {
                let prev_span = prev.span;
                self.report(
                    Diagnostic::error(
                        format!(
                            "the fields `{}` and `{}` both serialize as `{}`",
                            prev.name,
                            f.name,
                            f.key()
                        ),
                        f.span,
                    )
                    .with_label(prev_span, "the same key here"),
                );
            }
        }
    }

    fn define_enum(&mut self, id: TypeId, e: &'a ast::EnumDecl) {
        let mut variants: Vec<VariantDef> = Vec::new();
        for v in &e.variants {
            if let Some(prev) = variants.iter().find(|d| d.name == v.name.name) {
                let prev_span = prev.span;
                self.report(
                    Diagnostic::error(
                        format!("the variant `{}` is declared more than once", v.name.name),
                        v.name.span,
                    )
                    .with_label(prev_span, "first declared here"),
                );
                continue;
            }
            let (fields, named) = match &v.payload {
                VariantPayload::Unit => (Vec::new(), false),
                VariantPayload::Positional(tys) => {
                    let fields = tys
                        .iter()
                        .enumerate()
                        .map(|(i, t)| FieldDef {
                            name: i.to_string(),
                            ty: self.resolve_type(t),
                            span: t.span,
                            weak: false,
                            default: None,
                            rename: None,
                            skip: false,
                        })
                        .collect();
                    (fields, false)
                }
                VariantPayload::Named(named) => {
                    let mut fields: Vec<FieldDef> = Vec::new();
                    for f in named {
                        if fields.iter().any(|d| d.name == f.name.name) {
                            self.report(Diagnostic::error(
                                format!("the field `{}` is declared more than once", f.name.name),
                                f.name.span,
                            ));
                            continue;
                        }
                        let ty = self.resolve_type(&f.ty);
                        fields.push(FieldDef {
                            name: f.name.name.clone(),
                            ty,
                            span: f.name.span,
                            weak: false,
                            default: None,
                            rename: None,
                            skip: false,
                        });
                    }
                    (fields, true)
                }
            };
            variants.push(VariantDef {
                name: v.name.name.clone(),
                span: v.name.span,
                fields,
                named,
            });
        }
        self.types[id.0 as usize].kind = TypeDefKind::Enum { variants };
    }

    /// The type of a declared field and whether it is `weak`. A `weak T` field (DESIGN §11.5)
    /// is only allowed in a class and only for a class `T`; its type is `T`.
    fn resolve_field_type(&mut self, t: &ast::TypeExpr, is_class: bool) -> (Ty, bool) {
        let ast::TypeKind::Weak(inner) = &t.kind else {
            return (self.resolve_type(t), false);
        };
        let ty = self.resolve_type(inner);
        if ty.is_error() {
            return (Ty::Error, false);
        }
        if !is_class {
            self.report(
                Diagnostic::error("only a class can have a `weak` field", t.span)
                    .with_hint("a struct is copied, so a weak reference in it would be too; hold the weak reference in a class"),
            );
            return (Ty::Error, false);
        }
        if let Ty::Adt(id, _) = &ty
            && self.types[id.0 as usize].is_host_class()
        {
            let shown = self.render(&ty);
            self.report(
                Diagnostic::error(
                    format!("`weak` cannot refer to the host class `{shown}`"),
                    t.span,
                )
                .with_hint(
                    "the host decides how long its objects live; ask it with `alive()` instead",
                ),
            );
            return (Ty::Error, false);
        }
        let target_is_class = matches!(&ty, Ty::Adt(id, _) if self.types[id.0 as usize].is_class());
        if !target_is_class {
            let shown = self.render(&ty);
            self.report(
                Diagnostic::error(format!("`weak` needs a class, but `{shown}` is not one"), t.span)
                    .with_hint("only class objects have an identity to refer to weakly; other values are plain values"),
            );
            return (Ty::Error, false);
        }
        (ty, true)
    }

    /// The type an alias stands for, written with the alias's own type parameters, resolving
    /// it on first use (in the module that declares it).
    pub(crate) fn alias_body(&mut self, id: AliasId) -> Ty {
        let info = &mut self.alias_infos[id.0 as usize];
        match std::mem::replace(&mut info.state, AliasState::Resolving) {
            AliasState::Done(ty) => {
                info.state = AliasState::Done(ty.clone());
                ty
            }
            AliasState::Resolving => {
                let span = info.name_span;
                let name = info.name.clone();
                info.state = AliasState::Done(Ty::Error);
                self.report(
                    Diagnostic::error(format!("the type alias `{name}` refers to itself"), span)
                        .with_hint(
                            "an alias is another name for a type, so it cannot contain itself",
                        ),
                );
                Ty::Error
            }
            AliasState::Pending(target) => {
                let generics = info.generics.clone();
                let module = info.module;
                let back = self.enter_module(module);
                let mark = self.enter_generics(&generics);
                let saved = self.self_ty.take();
                let ty = self.resolve_type(target);
                self.self_ty = saved;
                self.leave_generics(mark);
                self.enter_module(back);
                self.alias_infos[id.0 as usize].state = AliasState::Done(ty.clone());
                ty
            }
        }
    }

    /// `Alias[args]`: the aliased type with the arguments substituted.
    pub(crate) fn alias_instance(&mut self, id: AliasId, args: &[ast::TypeExpr], span: Span) -> Ty {
        let generics = self.alias_infos[id.0 as usize].generics.clone();
        if args.len() != generics.len() {
            let name = self.alias_infos[id.0 as usize].name.clone();
            self.report_type_arity(&name, generics.len(), args.len(), span);
            for a in args {
                self.resolve_type(a);
            }
            return Ty::Error;
        }
        let tys: Vec<Ty> = args.iter().map(|a| self.resolve_type(a)).collect();
        let body = self.alias_body(id);
        if tys.iter().any(Ty::is_error) {
            return Ty::Error;
        }
        body.subst(&Self::param_map(&generics, &tys))
    }

    /// A struct that contains itself by value could never be built.
    fn check_infinite_size(&mut self, id: TypeId) {
        let def = &self.types[id.0 as usize];
        if !matches!(def.kind, TypeDefKind::Struct { .. }) {
            return;
        }
        let mut seen = Vec::new();
        let contains = def
            .fields()
            .iter()
            .any(|f| self.contains_by_value(&f.ty, id, &mut seen));
        if contains {
            let (name, span) = (def.name.clone(), def.span);
            self.report(
                Diagnostic::error(format!("the struct `{name}` contains itself"), span).with_hint(
                    "a struct is stored by value, so it would have infinite size; enums may be recursive",
                ),
            );
        }
    }

    fn contains_by_value(&self, ty: &Ty, target: TypeId, seen: &mut Vec<TypeId>) -> bool {
        match ty {
            Ty::Tuple(items) => items
                .iter()
                .any(|t| self.contains_by_value(t, target, seen)),
            Ty::Adt(id, _) if *id == target => true,
            // A class is held by reference, so it never makes a struct infinite.
            Ty::Adt(id, _) if self.types[id.0 as usize].is_class() => false,
            Ty::Adt(id, _) => {
                if seen.contains(id) {
                    return false;
                }
                seen.push(*id);
                self.types[id.0 as usize]
                    .fields()
                    .iter()
                    .any(|f| self.contains_by_value(&f.ty, target, seen))
            }
            _ => false,
        }
    }

    // ---- impl blocks ----

    pub(crate) fn collect_impl(&mut self, decl: &'a ast::ImplDecl, item: &'a ast::Item) {
        let (generics, mark) = self.declare_generics(&decl.generics);
        // `Self` is not available in the header: it is what the header says.
        let saved = self.self_ty.take();
        let target = self.resolve_type(&decl.target);
        match &decl.interface {
            Some(iface) => self.collect_iface_impl(decl, item, iface, target, &generics),
            None => self.collect_inherent_impl(decl, target, &generics),
        }
        self.self_ty = saved;
        self.leave_generics(mark);
    }

    fn collect_inherent_impl(
        &mut self,
        decl: &'a ast::ImplDecl,
        target: Ty,
        generics: &[GenericParamInfo],
    ) {
        let owner = match &target {
            Ty::Adt(id, _) => *id,
            Ty::Error => return,
            other => {
                let shown = self.render(other);
                self.report(
                    Diagnostic::error(
                        format!("cannot add methods to the built-in type `{shown}`"),
                        decl.target.span,
                    )
                    .with_hint("methods can only be added to your own structs, enums and classes; to add behaviour to another type, declare an interface and implement it"),
                );
                return;
            }
        };
        let class = self.types[owner.0 as usize].is_class();
        let ctx = SigCtx {
            self_ty: Some(target.clone()),
            outer: generics.to_vec(),
            owner: Some(self.types[owner.0 as usize].name.clone()),
            class,
        };
        for member in &decl.items {
            if let ItemKind::Fn(f) = &member.kind {
                self.collect_method(owner, member, f, &ctx);
            }
        }
    }

    fn collect_method(
        &mut self,
        owner: TypeId,
        item: &'a ast::Item,
        decl: &'a ast::FnDecl,
        ctx: &SigCtx,
    ) {
        let id = self.add_fn(item, decl, ctx);
        let name = &decl.name;
        let def = &self.types[owner.0 as usize];
        let type_name = def.display_name().into_owned();
        if let Some(v) = def.variants().iter().find(|v| v.name == name.name) {
            let vspan = v.span;
            self.report(
                Diagnostic::error(
                    format!("`{}` is already a variant of `{type_name}`", name.name),
                    name.span,
                )
                .with_label(vspan, "the variant is declared here"),
            );
            return;
        }
        match self.methods.get(&(owner, name.name.clone())) {
            Some(&prev) => {
                let prev_span = self.fns[prev.0 as usize].name_span;
                self.report(
                    Diagnostic::error(
                        format!(
                            "`{}` is defined more than once for `{type_name}`",
                            name.name
                        ),
                        name.span,
                    )
                    .with_label(prev_span, "first defined here"),
                );
            }
            None => {
                self.methods.insert((owner, name.name.clone()), id);
            }
        }
    }

    /// The names of the methods, static functions and variants of a type, for "did you mean".
    pub(crate) fn member_names(&self, ty: TypeId) -> Vec<String> {
        let mut names: Vec<String> = self
            .methods
            .keys()
            .filter(|(t, _)| *t == ty)
            .map(|(_, n)| n.clone())
            .collect();
        names.extend(
            self.types[ty.0 as usize]
                .variants()
                .iter()
                .map(|v| v.name.clone()),
        );
        names.sort();
        names
    }

    /// The closest of `candidates` to `name`, as a hint text.
    pub(crate) fn did_you_mean(name: &str, candidates: &[String]) -> Option<String> {
        suggest::closest(name, candidates.iter().map(String::as_str))
            .map(|s| format!("did you mean `{s}`?"))
    }
}
