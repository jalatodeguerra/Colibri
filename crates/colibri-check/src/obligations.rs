//! Bounds that must hold, and the messages for when they do not.
//!
//! A call of a generic function, a `Map` key or a coercion to an interface value *requires*
//! that a type implements an interface. While a body is being inferred the type is often not
//! known yet, so the requirement is recorded as an [`Obligation`] and decided at the end of
//! the body, when every inference variable has its final type (DESIGN §5.4).

use colibri_syntax::{Diagnostic, Span};

use crate::checker::Checker;
use crate::traits::Env;
use crate::types::{EQ, HASH, ORD, Ty, TypeId};

/// Something that must implement an interface, and why we ask.
#[derive(Clone, Debug)]
pub(crate) struct Obligation {
    pub ty: Ty,
    pub iface: TypeId,
    pub args: Vec<Ty>,
    /// Where the requirement arises (the call, the literal).
    pub span: Span,
    pub origin: Origin,
}

#[derive(Clone, Debug)]
pub(crate) enum Origin {
    /// A declared bound of a type parameter: `T: Ord` of `max`.
    Bound {
        param: String,
        func: String,
        bound_span: Span,
    },
    /// A `Map` key: it must be hashable and comparable.
    MapKey,
    /// A `Set` element.
    SetElem,
    /// The key of `sort_by_key`: it must be orderable.
    SortKey,
}

impl Checker<'_> {
    /// The implementation facts, for [`Env::implements`].
    pub fn env(&self) -> Env<'_> {
        Env {
            types: &self.types,
            impls: &self.impls,
            by_key: &self.impl_by_key,
            bounds: &self.param_bounds,
        }
    }

    /// Requires a bound now, or once every declaration is collected.
    pub fn require(&mut self, ob: Obligation) -> bool {
        if !self.collected {
            self.pending.push(ob);
            return true;
        }
        self.check_obligation(&ob)
    }

    /// Decides the requirements made while collecting declarations.
    pub fn flush_pending(&mut self) {
        for ob in std::mem::take(&mut self.pending) {
            self.check_obligation(&ob);
        }
    }

    /// `Map` keys and `Set` elements must be hashable (DESIGN §5.3).
    pub fn require_hashable(&mut self, ty: &Ty, what: &'static str, span: Span) -> bool {
        let origin = if what == "Map" {
            Origin::MapKey
        } else {
            Origin::SetElem
        };
        self.require(Obligation {
            ty: ty.clone(),
            iface: HASH,
            args: Vec::new(),
            span,
            origin,
        })
    }

    /// Decides one requirement, reporting it when it does not hold.
    pub fn check_obligation(&mut self, ob: &Obligation) -> bool {
        let blame = match self.env().implements(&ob.ty, ob.iface, &ob.args) {
            Ok(_) => match ob.origin {
                // A key must also be comparable for equality.
                Origin::MapKey | Origin::SetElem => match self.env().implements(&ob.ty, EQ, &[]) {
                    Ok(_) => return true,
                    Err(blame) => blame,
                },
                Origin::Bound { .. } | Origin::SortKey => return true,
            },
            Err(blame) => blame,
        };
        let diag = self.explain(ob, &blame);
        self.report(diag);
        false
    }

    /// The diagnostic for an unmet requirement; `blame` is the part of the type at fault.
    fn explain(&self, ob: &Obligation, blame: &Ty) -> Diagnostic {
        let shown = self.render(&ob.ty);
        let blamed = self.render(blame);
        let iface_name = self.render(&Ty::Dyn(ob.iface, ob.args.clone()));
        match &ob.origin {
            Origin::MapKey | Origin::SetElem => {
                let role = if matches!(ob.origin, Origin::MapKey) {
                    "a `Map` key"
                } else {
                    "a `Set` element"
                };
                let message = if blame == &ob.ty {
                    format!("`{shown}` cannot be used as {role}")
                } else {
                    format!("`{shown}` cannot be used as {role}, because it contains a `{blamed}`")
                };
                let hint = match blame {
                    Ty::Float => "floats are not hashable (`nan` is not equal to itself); use an integer or a string instead".to_string(),
                    Ty::List(_) | Ty::Map(..) | Ty::Set(_) => "mutable collections are not hashable; use a tuple, a struct or a frozen copy as the key".to_string(),
                    Ty::Param(_, name) => format!("add the bounds `{name}: Hash + Eq` to the type parameter"),
                    Ty::Adt(..) => format!("implement `Hash` and `Eq` for `{blamed}`, or key on one of its fields"),
                    _ => "a key must implement `Hash` and `Eq`".to_string(),
                };
                Diagnostic::error(message, ob.span).with_hint(hint)
            }
            Origin::SortKey => Diagnostic::error(
                format!("`{shown}` cannot be used as a sort key, because it cannot be ordered"),
                ob.span,
            )
            .with_hint(self.bound_hint(ob, blame)),
            Origin::Bound {
                param,
                func,
                bound_span,
            } => {
                let message = if blame == &ob.ty {
                    format!("`{shown}` does not implement `{iface_name}`")
                } else if ob.iface == HASH {
                    format!("`{shown}` is not `Hash` because `{blamed}` is not hashable")
                } else {
                    format!(
                        "`{shown}` does not implement `{iface_name}` because `{blamed}` does not"
                    )
                };
                let hint = self.bound_hint(ob, blame);
                Diagnostic::error(message, ob.span)
                    .with_label(
                        *bound_span,
                        format!("the bound `{param}: {iface_name}` of `{func}` is declared here"),
                    )
                    .with_hint(hint)
            }
        }
    }

    /// What to do about an unmet bound.
    fn bound_hint(&self, ob: &Obligation, blame: &Ty) -> String {
        let blamed = self.render(blame);
        let iface = self.render(&Ty::Dyn(ob.iface, ob.args.clone()));
        match blame {
            Ty::Param(_, name) => {
                format!("add the bound to the type parameter: `{name}: {iface}`")
            }
            Ty::Float if ob.iface == HASH => {
                "floats are not hashable (`nan` is not equal to itself)".to_string()
            }
            Ty::List(_) | Ty::Map(..) | Ty::Set(_) if ob.iface == HASH => {
                "mutable collections are not hashable; use a tuple or a struct instead".to_string()
            }
            Ty::List(_) | Ty::Map(..) | Ty::Set(_) | Ty::Option(_) if ob.iface == ORD => {
                "collections and options compare for equality only".to_string()
            }
            Ty::Adt(id, _) if ob.iface == ORD && self.types[id.0 as usize].is_enum() => {
                format!(
                    "only enums without data derive `Ord`; implement it: `impl Ord for {blamed} {{ fn cmp(self, other: {blamed}) -> int {{ ... }} }}`"
                )
            }
            Ty::Adt(id, _) if ob.iface == ORD && !self.types[id.0 as usize].is_enum() => {
                format!(
                    "structs do not derive `Ord`; implement it: `impl Ord for {blamed} {{ fn cmp(self, other: {blamed}) -> int {{ ... }} }}`"
                )
            }
            _ => format!("add `impl {iface} for {blamed} {{ ... }}`"),
        }
    }
}
