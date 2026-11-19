//! The interfaces every type derives automatically (DESIGN §7.4): `Eq` for structs, enums and
//! tuples whenever all their parts support it, `Ord` only for payload-free enums and tuples.
//! Collections compare structurally (DESIGN §5.3) and classes by identity. `Hash` follows
//! DESIGN §5.3: structs, enums and tuples structurally, classes by identity; mutable collections
//! are not hashable, and neither is `float` (`nan != nan`). `Show` and `Clone` work for every
//! value.

use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::checker::Checker;
use crate::hir::{TypeDef, TypeDefKind};
use crate::infer::VarKind;
use crate::types::{Ty, TypeId};

/// The part of `ty` that makes it unhashable, if there is one.
pub(crate) fn unhashable_part(
    types: &[TypeDef],
    ty: &Ty,
    visiting: &mut Vec<TypeId>,
) -> Option<Ty> {
    match ty {
        Ty::Float | Ty::List(_) | Ty::Map(..) | Ty::Set(_) => Some(ty.clone()),
        Ty::Tuple(items) => items
            .iter()
            .find_map(|t| unhashable_part(types, t, visiting)),
        Ty::Option(t) => unhashable_part(types, t, visiting),
        Ty::Adt(id, _) => {
            let def = types.get(id.0 as usize)?;
            if def.is_class() || visiting.contains(id) {
                return None;
            }
            visiting.push(*id);
            let found = match &def.kind {
                TypeDefKind::Enum { variants } => variants
                    .iter()
                    .flat_map(|v| &v.fields)
                    .find_map(|f| unhashable_part(types, &f.ty, visiting)),
                _ => def
                    .fields()
                    .iter()
                    .find_map(|f| unhashable_part(types, &f.ty, visiting)),
            };
            visiting.pop();
            found
        }
        _ => None,
    }
}

/// A `Map` key or `Set` element type somewhere inside `ty` that is not hashable, with whether
/// it is a map (`"Map"`) or a set.
pub(crate) fn unhashable_key(types: &[TypeDef], ty: &Ty) -> Option<(&'static str, Ty)> {
    let inner = |t: &Ty| unhashable_key(types, t);
    match ty {
        Ty::Map(k, v) => unhashable_part(types, k, &mut Vec::new())
            .map(|_| ("Map", (**k).clone()))
            .or_else(|| inner(k))
            .or_else(|| inner(v)),
        Ty::Set(t) => unhashable_part(types, t, &mut Vec::new())
            .map(|_| ("Set", (**t).clone()))
            .or_else(|| inner(t)),
        Ty::List(t) | Ty::Option(t) => inner(t),
        Ty::Tuple(items) => items.iter().find_map(inner),
        _ => None,
    }
}

impl Checker<'_> {
    /// Reports (and returns `false`) when `ty` cannot be a `Map` key or `Set` element
    /// (`what` is `"Map"` or `"Set"`). Before the types are defined the check is postponed.
    pub fn require_hashable(&mut self, ty: &Ty, what: &'static str, span: Span) -> bool {
        if !self.types_defined {
            self.pending_hash.push((ty.clone(), what, span));
            return true;
        }
        let Some(bad) = unhashable_part(&self.types, ty, &mut Vec::new()) else {
            return true;
        };
        let role = if what == "Map" {
            "a `Map` key"
        } else {
            "a `Set` element"
        };
        let shown = self.render(ty);
        let hint = if bad == Ty::Float {
            "floats are not hashable (`nan` is not equal to itself); use an integer or a string instead"
        } else {
            "mutable collections are not hashable; use a tuple, a struct or a frozen copy as the key"
        };
        let message = if bad == *ty {
            format!("`{shown}` cannot be used as {role}")
        } else {
            let inner = self.render(&bad);
            format!("`{shown}` cannot be used as {role}, because it contains a `{inner}`")
        };
        self.report(Diagnostic::error(message, span).with_hint(hint));
        false
    }

    /// Runs the checks that were postponed until the types were defined.
    pub fn flush_pending_hash(&mut self) {
        self.types_defined = true;
        for (ty, what, span) in std::mem::take(&mut self.pending_hash) {
            self.require_hashable(&ty, what, span);
        }
    }
}

impl FnCtx<'_, '_> {
    /// Whether `==` and `!=` work on values of this type.
    pub fn supports_eq(&self, ty: &Ty) -> bool {
        self.eq_ty(ty, &mut Vec::new())
    }

    fn eq_ty(&self, ty: &Ty, visiting: &mut Vec<TypeId>) -> bool {
        match self.infer.shallow(ty) {
            Ty::Int(_) | Ty::Float | Ty::Bool | Ty::Char | Ty::Str | Ty::Unit | Ty::Error => true,
            Ty::Infer(_) => true,
            Ty::Tuple(items) => items.iter().all(|t| self.eq_ty(t, visiting)),
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) => self.eq_ty(&t, visiting),
            Ty::Map(k, v) => self.eq_ty(&k, visiting) && self.eq_ty(&v, visiting),
            Ty::Adt(id, _) => {
                if visiting.contains(&id) {
                    return true;
                }
                visiting.push(id);
                let def = &self.ck.types[id.0 as usize];
                let ok = match &def.kind {
                    // A class is compared by identity, whatever its fields hold.
                    TypeDefKind::Class { .. } => true,
                    TypeDefKind::Struct { fields } => {
                        fields.iter().all(|f| self.eq_ty(&f.ty, visiting))
                    }
                    TypeDefKind::Enum { variants } => variants
                        .iter()
                        .all(|v| v.fields.iter().all(|f| self.eq_ty(&f.ty, visiting))),
                };
                visiting.pop();
                ok
            }
            _ => false,
        }
    }

    /// Whether `<`, `<=`, `>` and `>=` work on values of this type.
    pub fn supports_ord(&self, ty: &Ty) -> bool {
        match self.infer.shallow(ty) {
            Ty::Int(_) | Ty::Float | Ty::Bool | Ty::Char | Ty::Str | Ty::Error => true,
            Ty::Infer(_) => self.is_numeric(ty),
            Ty::Tuple(items) => items.iter().all(|t| self.supports_ord(t)),
            Ty::Adt(id, _) => {
                let def = &self.ck.types[id.0 as usize];
                matches!(&def.kind, TypeDefKind::Enum { variants } if variants.iter().all(|v| v.fields.is_empty()))
            }
            _ => false,
        }
    }

    /// Whether `ty` may be a `Map` key or `Set` element, as far as is known now (a type that is
    /// still being inferred counts as hashable until it resolves).
    pub fn supports_hash(&self, ty: &Ty) -> bool {
        let resolved = self.infer.resolve(ty);
        if let Ty::Infer(_) = resolved {
            return self.infer.unbound_kind(&resolved) != Some(VarKind::FloatLit);
        }
        unhashable_part(&self.ck.types, &resolved, &mut Vec::new()).is_none()
    }

    /// Reports an unhashable `Map` key or `Set` element type at `span`.
    pub fn require_hash(&mut self, ty: &Ty, what: &'static str, span: Span) -> bool {
        if self.supports_hash(ty) {
            return true;
        }
        let ty = self.infer.resolve(ty);
        self.ck.require_hashable(&ty, what, span)
    }

    /// Why a comparison is not available, as a hint.
    pub fn comparison_hint(&self, ty: &Ty, equality: bool) -> Option<String> {
        let ty = self.infer.shallow(ty);
        if equality {
            return None;
        }
        match &ty {
            Ty::Adt(id, _) => {
                let def = &self.ck.types[id.0 as usize];
                Some(if def.is_enum() {
                    format!(
                        "`{}` has variants with data; only enums without data compare by declaration order. Use `match`, or implement `Ord` (interfaces are not supported yet)",
                        def.name
                    )
                } else {
                    format!(
                        "structs do not derive `Ord`: implement it for `{0}` (interfaces are not supported yet), or compare fields, like `a.{1} < b.{1}`",
                        def.name,
                        def.fields().first().map_or("field", |f| f.name.as_str())
                    )
                })
            }
            Ty::Tuple(_) => Some(
                "a tuple can be ordered only when every element can; structs and enums with data cannot"
                    .to_string(),
            ),
            Ty::List(_) | Ty::Map(..) | Ty::Set(_) | Ty::Option(_) => Some(
                "collections and options compare for equality only; compare their elements instead"
                    .to_string(),
            ),
            Ty::Unit => Some("`()` has only one value; there is nothing to order".to_string()),
            _ => None,
        }
    }
}
