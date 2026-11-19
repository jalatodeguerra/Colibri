//! Collecting user-defined types (structs, enums, aliases) and `impl` blocks.
//!
//! Types are collected in two passes so they may refer to each other in any order: first every
//! name is declared, then fields, payloads and aliases are resolved.

use colibri_syntax::ast::{self, ItemKind, StructKind, VariantPayload};
use colibri_syntax::{Diagnostic, Span};

use crate::checker::Checker;
use crate::generics::SigCtx;
use crate::hir::{FieldDef, TypeDef, TypeDefKind, VariantDef};
use crate::suggest;
use crate::traits::GenericParamInfo;
use crate::types::{Ty, TypeId};

/// Names that cannot be redefined as types.
const BUILTIN_TYPES: &[&str] = &[
    "int", "float", "bool", "str", "char", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64",
    "f32", "f64", "vec2", "vec3", "vec4", "List", "Map", "Set", "Option", "Result",
];

/// What HIR does not keep about a user type.
#[derive(Clone, Default)]
pub(crate) struct TypeExtra<'a> {
    /// The default of each struct field, parallel to its fields.
    pub defaults: Vec<Option<&'a ast::Expr>>,
    /// The type parameters, in order (without bounds: a type's own parameters take none yet).
    pub generics: Vec<GenericParamInfo>,
}

pub(crate) enum AliasState<'a> {
    Pending(&'a ast::TypeExpr),
    Resolving,
    /// The target type, written with the alias's own type parameters.
    Done(Ty),
}

pub(crate) struct AliasInfo<'a> {
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
                    if let Some(id) = self.declare_type(&s.name, what, &s.generics) {
                        let fields = Vec::new();
                        self.types[id.0 as usize].kind = match s.kind {
                            StructKind::Struct => TypeDefKind::Struct { fields },
                            StructKind::Class => TypeDefKind::Class { fields },
                        };
                    }
                }
                ItemKind::Enum(e) => {
                    if let Some(id) = self.declare_type(&e.name, "enum", &e.generics) {
                        self.types[id.0 as usize].kind = TypeDefKind::Enum {
                            variants: Vec::new(),
                        };
                    }
                }
                ItemKind::Interface(i) => {
                    if let Some(id) = self.declare_type(&i.name, "interface", &i.generics) {
                        self.types[id.0 as usize].kind = TypeDefKind::Interface {
                            methods: Vec::new(),
                        };
                        self.declare_interface(id, i);
                    }
                }
                ItemKind::TypeAlias(t) if self.name_is_free(&t.name, "type alias") => {
                    let generics = self.type_params(&t.generics);
                    self.aliases.insert(
                        t.name.name.clone(),
                        AliasInfo {
                            name_span: t.name.span,
                            generics,
                            state: AliasState::Pending(&t.ty),
                        },
                    );
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

    /// Whether `name` can be declared as a type; reports a clash otherwise.
    fn name_is_free(&mut self, name: &ast::Ident, what: &str) -> bool {
        if BUILTIN_TYPES.contains(&name.name.as_str()) {
            self.report(Diagnostic::error(
                format!("`{}` is a built-in type and cannot be redefined", name.name),
                name.span,
            ));
            return false;
        }
        let previous = self
            .type_ids
            .get(&name.name)
            .map(|id| self.types[id.0 as usize].span)
            .or_else(|| self.aliases.get(&name.name).map(|a| a.name_span));
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
            kind: TypeDefKind::Struct { fields: Vec::new() },
        });
        self.type_extra.push(TypeExtra {
            defaults: Vec::new(),
            generics,
        });
        self.type_ids.insert(name.name.clone(), id);
        Some(id)
    }

    /// Pass two: resolves fields, payloads, interface signatures and aliases.
    pub(crate) fn define_types(&mut self, items: &[&'a ast::Item]) {
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
        // Interfaces last: their signatures may name any type.
        for item in items {
            if let ItemKind::Interface(i) = &item.kind
                && let Some(id) = self.own_type(&i.name)
            {
                self.define_interface(id, i);
            }
        }
        // Report errors in aliases nobody uses, too.
        let names: Vec<String> = self.aliases.keys().cloned().collect();
        for name in names {
            self.alias_body(&name);
        }
        for t in 0..self.types.len() {
            self.check_infinite_size(TypeId(t as u32));
        }
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
        let mut fields: Vec<FieldDef> = Vec::new();
        let mut defaults = Vec::new();
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
            fields.push(FieldDef {
                name: f.name.name.clone(),
                ty,
                span: f.name.span,
                weak,
            });
            defaults.push(f.default.as_ref());
        }
        self.types[id.0 as usize].kind = if is_class {
            TypeDefKind::Class { fields }
        } else {
            TypeDefKind::Struct { fields }
        };
        self.type_extra[id.0 as usize].defaults = defaults;
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
    /// it on first use. `None` if `name` is no alias.
    pub(crate) fn alias_body(&mut self, name: &str) -> Option<Ty> {
        let info = self.aliases.get_mut(name)?;
        match std::mem::replace(&mut info.state, AliasState::Resolving) {
            AliasState::Done(ty) => {
                info.state = AliasState::Done(ty.clone());
                Some(ty)
            }
            AliasState::Resolving => {
                let span = info.name_span;
                info.state = AliasState::Done(Ty::Error);
                self.report(
                    Diagnostic::error(format!("the type alias `{name}` refers to itself"), span)
                        .with_hint(
                            "an alias is another name for a type, so it cannot contain itself",
                        ),
                );
                Some(Ty::Error)
            }
            AliasState::Pending(target) => {
                let generics = info.generics.clone();
                let mark = self.enter_generics(&generics);
                let saved = self.self_ty.take();
                let ty = self.resolve_type(target);
                self.self_ty = saved;
                self.leave_generics(mark);
                if let Some(info) = self.aliases.get_mut(name) {
                    info.state = AliasState::Done(ty.clone());
                }
                Some(ty)
            }
        }
    }

    /// `Alias[args]`: the aliased type with the arguments substituted.
    pub(crate) fn alias_instance(&mut self, name: &str, args: &[ast::TypeExpr], span: Span) -> Ty {
        let Some(generics) = self.aliases.get(name).map(|a| a.generics.clone()) else {
            return Ty::Error;
        };
        if args.len() != generics.len() {
            self.report_type_arity(name, generics.len(), args.len(), span);
            for a in args {
                self.resolve_type(a);
            }
            return Ty::Error;
        }
        let tys: Vec<Ty> = args.iter().map(|a| self.resolve_type(a)).collect();
        let body = self.alias_body(name).unwrap_or(Ty::Error);
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
        let type_name = def.name.clone();
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
