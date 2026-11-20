//! The interfaces every type derives automatically (DESIGN §7.4), as the body checker asks
//! about them: `==` needs `Eq`, `<` needs `Ord`, a `Map` key needs `Hash` and `Eq`. The rules
//! live in [`crate::traits`]; this file adapts them to types that are still being inferred.

use colibri_syntax::Span;

use crate::body::FnCtx;
use crate::checker::Checker;
use crate::hir::TypeDefKind;
use crate::obligations::{Obligation, Origin};
use crate::types::{EQ, HASH, ORD, Ty};

/// Whether `ty` still has an unresolved inference variable somewhere inside.
pub(crate) fn has_infer(ty: &Ty) -> bool {
    match ty {
        Ty::Infer(_) => true,
        Ty::Tuple(ts) | Ty::Adt(_, ts) | Ty::Dyn(_, ts) => ts.iter().any(has_infer),
        Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Range(t) | Ty::Ptr(t) => has_infer(t),
        Ty::Map(a, b) => has_infer(a) || has_infer(b),
        Ty::Fn(ps, r) => ps.iter().any(has_infer) || has_infer(r),
        _ => false,
    }
}

impl Checker<'_> {
    /// Where in `ty` the part `blame` that cannot be ordered is, in words: "its field `inner.items`
    /// (`List[int]`) cannot be ordered", or the variant payload for an enum.
    pub(crate) fn ord_blame(&self, ty: &Ty, blame: &Ty) -> Option<String> {
        let (path, found) = self.blame_path(ty, blame, 0)?;
        Some(format!(
            "{path} (`{}`) cannot be ordered",
            self.render(&found)
        ))
    }

    /// The path from `ty` to the part `blame` and how it is written: "its field `a.b`",
    /// "the payload of its variant `Circle`".
    fn blame_path(&self, ty: &Ty, blame: &Ty, depth: usize) -> Option<(String, Ty)> {
        if depth > 6 {
            return None;
        }
        let Ty::Adt(id, args) = ty else { return None };
        let def = self.types.get(id.0 as usize)?;
        let map = Checker::param_map_of(def, args);
        match &def.kind {
            TypeDefKind::Struct { fields } | TypeDefKind::Class { fields } => {
                for f in fields {
                    let fty = f.ty.subst(&map);
                    if &fty == blame || self.env().implements(&fty, crate::types::ORD, &[]).is_err()
                    {
                        if &fty == blame {
                            return Some((format!("its field `{}`", f.name), fty));
                        }
                        if let Some((path, leaf)) = self.blame_path(&fty, blame, depth + 1) {
                            let inner = path
                                .strip_prefix("its field `")
                                .and_then(|p| p.strip_suffix('`'))
                                .map(str::to_string);
                            return Some(match inner {
                                Some(inner) => (format!("its field `{}.{inner}`", f.name), leaf),
                                None => (format!("its field `{}`", f.name), fty),
                            });
                        }
                        return Some((format!("its field `{}`", f.name), fty));
                    }
                }
                None
            }
            TypeDefKind::Enum { variants } => {
                for v in variants {
                    for f in &v.fields {
                        let fty = f.ty.subst(&map);
                        if &fty == blame
                            || self.env().implements(&fty, crate::types::ORD, &[]).is_err()
                        {
                            return Some((format!("the payload of its variant `{}`", v.name), fty));
                        }
                    }
                }
                None
            }
            _ => None,
        }
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
                    self.ck.types[id.0 as usize].display_name()
                )),
                _ => None,
            };
        }
        match &ty {
            Ty::Adt(id, _) => {
                let def = &self.ck.types[id.0 as usize];
                let name = def.display_name();
                if self.ck.env().user_eq_blocks_ord(&ty) {
                    return Some(user_eq_hint(&name));
                }
                // A struct or an enum with data is ordered by its parts: say which one is not.
                let why = match self.ck.env().implements(&ty, crate::types::ORD, &[]) {
                    Err(blame) => self.ck.ord_blame(&ty, &blame),
                    Ok(_) => None,
                };
                Some(match (def.is_class(), why) {
                    (true, _) => format!(
                        "`{name}` is a class, which has no derived order; implement `Ord`: `impl Ord for {name} {{ fn cmp(self, other: {name}) -> int {{ ... }} }}`, or compare one of its fields"
                    ),
                    (false, Some(why)) => format!(
                        "`{name}` is ordered by its parts, but {why}; implement `Ord` for `{name}` (`impl Ord for {name} {{ fn cmp(self, other: {name}) -> int {{ ... }} }}`), or compare the parts that can be ordered"
                    ),
                    (false, None) => format!(
                        "`{name}` cannot be ordered; implement `Ord`: `impl Ord for {name} {{ fn cmp(self, other: {name}) -> int {{ ... }} }}`"
                    ),
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

/// Why a type with its own `Eq` is not ordered, and what to write.
pub(crate) fn user_eq_hint(name: &str) -> String {
    format!(
        "`{name}` has its own `impl Eq`, so its order is not derived (it could disagree with your `==`); write `impl Ord for {name} {{ fn cmp(self, other: {name}) -> int {{ ... }} }}` as well, and make `cmp` return 0 exactly when the values are equal"
    )
}
