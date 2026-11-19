//! The interfaces every type derives automatically (DESIGN §7.4), as the body checker asks
//! about them: `==` needs `Eq`, `<` needs `Ord`, a `Map` key needs `Hash` and `Eq`. The rules
//! live in [`crate::traits`]; this file adapts them to types that are still being inferred.

use colibri_syntax::Span;

use crate::body::FnCtx;
use crate::obligations::{Obligation, Origin};
use crate::types::{EQ, HASH, ORD, Ty};

/// Whether `ty` still has an unresolved inference variable somewhere inside.
pub(crate) fn has_infer(ty: &Ty) -> bool {
    match ty {
        Ty::Infer(_) => true,
        Ty::Tuple(ts) | Ty::Adt(_, ts) | Ty::Dyn(_, ts) => ts.iter().any(has_infer),
        Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Range(t) => has_infer(t),
        Ty::Map(a, b) | Ty::Result(a, b) => has_infer(a) || has_infer(b),
        Ty::Fn(ps, r) => ps.iter().any(has_infer) || has_infer(r),
        _ => false,
    }
}

impl FnCtx<'_, '_> {
    /// Whether `==` and `!=` work on values of this type.
    pub fn supports_eq(&self, ty: &Ty) -> bool {
        let resolved = self.infer.resolve(ty);
        match resolved {
            Ty::Infer(_) => true,
            _ => self.ck.env().implements(&resolved, EQ, &[]).is_ok(),
        }
    }

    /// Whether `<`, `<=`, `>` and `>=` work on values of this type.
    pub fn supports_ord(&self, ty: &Ty) -> bool {
        let resolved = self.infer.resolve(ty);
        match resolved {
            Ty::Infer(_) => self.is_numeric(ty),
            _ => self.ck.env().implements(&resolved, ORD, &[]).is_ok(),
        }
    }

    /// Requires `ty` to be a hashable key (`what` is `"Map"` or `"Set"`): decided now when the
    /// type is known, at the end of the body otherwise.
    pub fn require_hash(&mut self, ty: &Ty, what: &'static str, span: Span) -> bool {
        let resolved = self.infer.resolve(ty);
        if has_infer(&resolved) {
            let origin = if what == "Map" {
                Origin::MapKey
            } else {
                Origin::SetElem
            };
            self.obligations.push(Obligation {
                ty: ty.clone(),
                iface: HASH,
                args: Vec::new(),
                span,
                origin,
            });
            return true;
        }
        self.ck.require_hashable(&resolved, what, span)
    }

    /// Why a comparison is not available, as a hint.
    pub fn comparison_hint(&self, ty: &Ty, equality: bool) -> Option<String> {
        let ty = self.infer.shallow(ty);
        if let Ty::Param(_, name) = &ty {
            let iface = if equality { "Eq" } else { "Ord" };
            return Some(format!(
                "a type parameter supports only what its bounds promise; add `{name}: {iface}`"
            ));
        }
        if equality {
            return match &ty {
                Ty::Fn(..) => Some(
                    "functions and closures have no `==`; compare something they work on, or keep an id next to the function"
                        .to_string(),
                ),
                Ty::Adt(id, _) => Some(format!(
                    "`{0}` has a part that cannot be compared; implement `Eq` for it: `impl Eq for {0} {{ fn eq(self, other: {0}) -> bool {{ ... }} }}`",
                    self.ck.types[id.0 as usize].name
                )),
                _ => None,
            };
        }
        match &ty {
            Ty::Adt(id, _) => {
                let def = &self.ck.types[id.0 as usize];
                Some(if def.is_enum() {
                    format!(
                        "`{0}` has variants with data; only enums without data compare by declaration order. Use `match`, or implement `Ord`: `impl Ord for {0} {{ fn cmp(self, other: {0}) -> int {{ ... }} }}`",
                        def.name
                    )
                } else {
                    format!(
                        "structs do not derive `Ord`: implement it for `{0}` (`impl Ord for {0} {{ fn cmp(self, other: {0}) -> int {{ ... }} }}`), or compare fields, like `a.{1} < b.{1}`",
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
            Ty::Fn(..) => Some("functions and closures cannot be compared".to_string()),
            _ => None,
        }
    }
}
