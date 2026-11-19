//! The interfaces every type derives automatically (DESIGN §7.4): `Eq` for structs, enums and
//! tuples whenever all their parts support it, `Ord` only for payload-free enums and tuples.
//! `Show` works for every value that exists today.

use crate::body::FnCtx;
use crate::hir::TypeDefKind;
use crate::types::{Ty, TypeId};

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
            Ty::Adt(id, _) => {
                if visiting.contains(&id) {
                    return true;
                }
                visiting.push(id);
                let def = &self.ck.types[id.0 as usize];
                let ok = match &def.kind {
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
            Ty::Unit => Some("`()` has only one value; there is nothing to order".to_string()),
            _ => None,
        }
    }
}
