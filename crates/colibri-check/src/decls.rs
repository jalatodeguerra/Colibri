//! Collecting user-defined types (structs, enums, aliases) and `impl` blocks.
//!
//! Types are collected in two passes so they may refer to each other in any order: first every
//! name is declared, then fields, payloads and aliases are resolved.

use colibri_syntax::ast::{self, ItemKind, StructKind, VariantPayload};
use colibri_syntax::{Diagnostic, Span};

use crate::checker::Checker;
use crate::hir::{FieldDef, TypeDef, TypeDefKind, VariantDef};
use crate::suggest;
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
}

pub(crate) enum AliasState<'a> {
    Pending(&'a ast::TypeExpr),
    Resolving,
    Done(Ty),
}

pub(crate) struct AliasInfo<'a> {
    pub name_span: Span,
    pub state: AliasState<'a>,
}

impl<'a> Checker<'a> {
    /// Pass one: registers the name of every struct, enum and alias.
    pub(crate) fn declare_types(&mut self, items: &[&'a ast::Item]) {
        for item in items {
            match &item.kind {
                ItemKind::Struct(s) => {
                    let what = match s.kind {
                        StructKind::Struct => "struct",
                        StructKind::Class => "class",
                    };
                    if s.kind == StructKind::Class {
                        self.poison_type(&s.name, "`class`");
                    } else if !s.generics.is_empty() {
                        self.poison_type(&s.name, "a generic `struct`");
                    } else if let Some(id) = self.declare_type(&s.name, what) {
                        self.types[id.0 as usize].kind = TypeDefKind::Struct { fields: Vec::new() };
                    }
                }
                ItemKind::Enum(e) => {
                    if !e.generics.is_empty() {
                        self.poison_type(&e.name, "a generic `enum`");
                    } else if let Some(id) = self.declare_type(&e.name, "enum") {
                        self.types[id.0 as usize].kind = TypeDefKind::Enum {
                            variants: Vec::new(),
                        };
                    }
                }
                ItemKind::TypeAlias(t) => {
                    if !t.generics.is_empty() {
                        self.poison_type(&t.name, "a generic `type` alias");
                    } else if self.name_is_free(&t.name, "type alias") {
                        self.aliases.insert(
                            t.name.name.clone(),
                            AliasInfo {
                                name_span: t.name.span,
                                state: AliasState::Pending(&t.ty),
                            },
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Remembers the name of a type that cannot be used yet, and says why.
    fn poison_type(&mut self, name: &ast::Ident, what: &str) {
        self.poisoned_types.insert(name.name.clone());
        self.unsupported(what, name.span);
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

    fn declare_type(&mut self, name: &ast::Ident, what: &str) -> Option<TypeId> {
        if !self.name_is_free(name, what) {
            return None;
        }
        let id = TypeId(self.types.len() as u32);
        self.types.push(TypeDef {
            name: name.name.clone(),
            span: name.span,
            kind: TypeDefKind::Struct { fields: Vec::new() },
        });
        self.type_extra.push(TypeExtra::default());
        self.type_ids.insert(name.name.clone(), id);
        Some(id)
    }

    /// Pass two: resolves fields, payloads and aliases.
    pub(crate) fn define_types(&mut self, items: &[&'a ast::Item]) {
        for item in items {
            match &item.kind {
                ItemKind::Struct(s) if s.kind == StructKind::Struct && s.generics.is_empty() => {
                    if let Some(&id) = self.type_ids.get(&s.name.name)
                        && self.types[id.0 as usize].span == s.name.span
                    {
                        self.define_struct(id, s);
                    }
                }
                ItemKind::Enum(e) if e.generics.is_empty() => {
                    if let Some(&id) = self.type_ids.get(&e.name.name)
                        && self.types[id.0 as usize].span == e.name.span
                    {
                        self.define_enum(id, e);
                    }
                }
                _ => {}
            }
        }
        // Report errors in aliases nobody uses, too.
        let names: Vec<String> = self.aliases.keys().cloned().collect();
        for name in names {
            self.alias_ty(&name);
        }
        for t in 0..self.types.len() {
            self.check_infinite_size(TypeId(t as u32));
        }
    }

    fn define_struct(&mut self, id: TypeId, s: &'a ast::StructDecl) {
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
            let ty = self.resolve_type(&f.ty);
            fields.push(FieldDef {
                name: f.name.name.clone(),
                ty,
                span: f.name.span,
            });
            defaults.push(f.default.as_ref());
        }
        self.types[id.0 as usize].kind = TypeDefKind::Struct { fields };
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

    /// The type an alias stands for, resolving it on first use. `None` if `name` is no alias.
    pub(crate) fn alias_ty(&mut self, name: &str) -> Option<Ty> {
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
                let ty = self.resolve_type(target);
                if let Some(info) = self.aliases.get_mut(name) {
                    info.state = AliasState::Done(ty.clone());
                }
                Some(ty)
            }
        }
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
        if decl.interface.is_some() {
            self.unsupported("`impl` of an interface", item.span);
            return;
        }
        if !decl.generics.is_empty() {
            self.unsupported("a generic `impl`", decl.generics[0].name.span);
            return;
        }
        let target = self.resolve_type(&decl.target);
        let owner = match target {
            Ty::Adt(id, _) => id,
            Ty::Error => return,
            other => {
                let shown = self.render(&other);
                self.report(
                    Diagnostic::error(
                        format!("cannot add methods to the built-in type `{shown}`"),
                        decl.target.span,
                    )
                    .with_hint("methods can only be added to your own structs and enums"),
                );
                return;
            }
        };
        for member in &decl.items {
            if let ItemKind::Fn(f) = &member.kind {
                self.collect_method(owner, member, f);
            }
        }
    }

    fn collect_method(&mut self, owner: TypeId, item: &'a ast::Item, decl: &'a ast::FnDecl) {
        let id = self.add_fn(item, decl, Some(owner));
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
