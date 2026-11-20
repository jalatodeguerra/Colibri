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
        /// The first argument of an operator bound is a type parameter of the function.
        dyn_rhs: bool,
    },
    /// A `Map` key: it must be hashable and comparable.
    MapKey,
    /// A `Set` element.
    SetElem,
    /// The key of `sort_by_key`: it must be orderable.
    SortKey,
    /// A bound the right-hand side of an alias puts on the alias's parameter, met by the
    /// argument of a use: `type S[T] = Sorted[T]`, `S[NotOrd]`.
    AliasBound {
        alias: String,
        /// The type the alias names, as written without arguments.
        underlying: String,
        param: String,
        bound_span: Span,
    },
    /// The bound a type declares on its own type parameter: `struct Sorted[T: Ord]`.
    TypeBound {
        /// The type, as written without arguments.
        ty: String,
        param: String,
        bound_span: Span,
    },
}

/// An integer type whose values carry no width at run time (`i8`..`u32`).
fn is_narrow(ty: &Ty) -> bool {
    matches!(ty, Ty::Int(t) if !matches!(t, crate::types::IntTy::I64 | crate::types::IntTy::U64))
}

/// The narrow integer an operator bound would hand to run-time dispatch: the bounded type
/// itself, or the right-hand type when the bound leaves it to a type parameter.
fn narrow_operand(ob: &Obligation) -> Option<Ty> {
    if !matches!(ob.origin, Origin::Bound { .. }) {
        return None;
    }
    if is_narrow(&ob.ty) {
        return Some(ob.ty.clone());
    }
    match (&ob.origin, ob.args.first()) {
        (Origin::Bound { dyn_rhs: true, .. }, Some(a)) if is_narrow(a) => Some(a.clone()),
        _ => None,
    }
}

impl Checker<'_> {
    /// The implementation facts, for [`Env::implements`].
    pub fn env(&self) -> Env<'_> {
        Env {
            types: &self.types,
            impls: &self.impls,
            by_key: &self.impl_by_key,
            op_impls: &self.op_impls,
            bounds: &self.param_bounds,
        }
    }

    /// Requires a bound now, or once every declaration is collected.
    pub fn require(&mut self, ob: Obligation) -> bool {
        if self.imply_alias_bound(&ob) {
            return true;
        }
        if !self.collected {
            self.pending.push(ob);
            return true;
        }
        self.check_obligation(&ob)
    }

    /// While the right-hand side of `type S[T] = Sorted[T]` is resolved, `Sorted` needing
    /// `T: Ord` makes `Ord` a bound of the alias's `T`: the alias is valid, and a use of it
    /// (`S[NotOrd]`) is checked against the bound (`alias_instance`). The same goes for the key
    /// of a `Map` and the element of a `Set` (`type M[T] = Map[T, int]` takes `T: Hash + Eq`).
    /// Whether `true` was taken.
    fn imply_alias_bound(&mut self, ob: &Obligation) -> bool {
        let (Some(alias), Ty::Param(p, _)) = (self.implying, &ob.ty) else {
            return false;
        };
        let (wanted, bound_span) = match &ob.origin {
            Origin::TypeBound { bound_span, .. } | Origin::AliasBound { bound_span, .. } => {
                (vec![(ob.iface, ob.args.clone())], *bound_span)
            }
            Origin::MapKey | Origin::SetElem => {
                (vec![(HASH, Vec::new()), (EQ, Vec::new())], ob.span)
            }
            _ => return false,
        };
        let Some(g) = self.alias_infos[alias.0 as usize]
            .generics
            .iter_mut()
            .find(|g| g.id == *p)
        else {
            return false;
        };
        for (iface, args) in wanted {
            let bound = crate::traits::BoundRef {
                iface,
                args,
                span: bound_span,
                implied: true,
            };
            if !g
                .bounds
                .iter()
                .any(|b| b.iface == bound.iface && b.args == bound.args)
            {
                g.bounds.push(bound.clone());
                self.param_bounds.entry(*p).or_default().push(bound);
            }
        }
        true
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
        // Generic code reaches an operator through the run-time type of the values, and a
        // `Value::Int` of a narrow width carries none: refuse the narrow integers wherever a
        // bound could hand one to that dispatch, before any impl is consulted.
        if crate::types::is_operator(ob.iface)
            && let Some(narrow) = narrow_operand(ob)
        {
            let diag = self.explain(ob, &narrow);
            self.report(diag);
            return false;
        }
        let blame = match self.env().implements(&ob.ty, ob.iface, &ob.args) {
            Ok(_) => match ob.origin {
                // A key must also be comparable for equality.
                Origin::MapKey | Origin::SetElem => match self.env().implements(&ob.ty, EQ, &[]) {
                    Ok(_) => return true,
                    Err(blame) => blame,
                },
                Origin::Bound { .. }
                | Origin::SortKey
                | Origin::TypeBound { .. }
                | Origin::AliasBound { .. } => return true,
            },
            Err(blame) => blame,
        };
        // An operator bound that fails because a narrow integer is in the way (`W[i32]` through
        // `impl[T: Add] Add for W[T]`) says so; one that fails because an impl is missing does not.
        let blame = if matches!(ob.origin, Origin::Bound { .. }) && !is_narrow(&blame) {
            self.env()
                .narrow_blocker(&ob.ty, ob.iface, &ob.args, 0)
                .unwrap_or(blame)
        } else {
            blame
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
                    Ty::Float | Ty::F32 => "floats are not hashable (`nan` is not equal to itself); use an integer or a string instead".to_string(),
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
            Origin::AliasBound {
                alias,
                underlying,
                param,
                bound_span,
            } => {
                let message = if blame == &ob.ty {
                    format!(
                        "`{shown}` does not implement `{iface_name}`, which the alias `{alias}` needs of its type parameter `{param}` because `{underlying}` does"
                    )
                } else {
                    format!(
                        "`{shown}` does not implement `{iface_name}` because `{blamed}` does not, and the alias `{alias}` needs it of its type parameter `{param}` because `{underlying}` does"
                    )
                };
                let hint = self.bound_hint(ob, blame);
                Diagnostic::error(message, ob.span)
                    .with_label(
                        *bound_span,
                        format!(
                            "the bound `{param}: {iface_name}` of `{underlying}` is declared here"
                        ),
                    )
                    .with_hint(hint)
            }
            Origin::TypeBound {
                ty,
                param,
                bound_span,
            } => {
                let message = if blame == &ob.ty {
                    format!(
                        "`{shown}` does not implement `{iface_name}`, which `{ty}` needs of its type parameter `{param}`"
                    )
                } else {
                    format!(
                        "`{shown}` does not implement `{iface_name}` because `{blamed}` does not, and `{ty}` needs it of its type parameter `{param}`"
                    )
                };
                let hint = self.bound_hint(ob, blame);
                Diagnostic::error(message, ob.span)
                    .with_label(
                        *bound_span,
                        format!("the bound `{param}: {iface_name}` of `{ty}` is declared here"),
                    )
                    .with_hint(hint)
            }
            Origin::Bound {
                param,
                func,
                bound_span,
                ..
            } => {
                if crate::types::is_operator(ob.iface) && is_narrow(blame) {
                    let name = &self.types[ob.iface.0 as usize].name;
                    let message = if blame == &ob.ty {
                        format!("`{blamed}` cannot be used for `{param}: {name}` yet")
                    } else if ob.args.first() == Some(blame) {
                        format!(
                            "`{blamed}` cannot be used as the right-hand type of `{param}: {name}` yet"
                        )
                    } else {
                        format!(
                            "`{blamed}`, the type argument of `{shown}`, cannot be used for `{param}: {name}` yet"
                        )
                    };
                    return Diagnostic::error(
                        message,
                        ob.span,
                    )
                    .with_label(
                        *bound_span,
                        format!("the bound `{param}: {iface_name}` of `{func}` is declared here"),
                    )
                    .with_hint(format!("generic code cannot tell integer widths apart yet; the narrow type here is `{blamed}`: convert that operand to `int` (`int(x)`), or write the function for `{blamed}` alone"));
                }
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
            Ty::Float | Ty::F32 if ob.iface == HASH => {
                "floats are not hashable (`nan` is not equal to itself)".to_string()
            }
            Ty::List(_) | Ty::Map(..) | Ty::Set(_) if ob.iface == HASH => {
                "mutable collections are not hashable; use a tuple or a struct instead".to_string()
            }
            Ty::List(_) | Ty::Map(..) | Ty::Set(_) | Ty::Option(_) if ob.iface == ORD => {
                "collections and options compare for equality only".to_string()
            }
            Ty::Adt(id, _) if ob.iface == ORD && self.types[id.0 as usize].is_class() => {
                format!(
                    "a class has no derived order; implement it: `impl Ord for {blamed} {{ fn cmp(self, other: {blamed}) -> int {{ ... }} }}`"
                )
            }
            Ty::Adt(..) if ob.iface == ORD && self.env().user_eq_blocks_ord(blame) => {
                crate::derive::user_eq_hint(&blamed)
            }
            Ty::Adt(id, _) if ob.iface == ORD => {
                let kind = if self.types[id.0 as usize].is_enum() {
                    "an enum"
                } else {
                    "a struct"
                };
                format!(
                    "{kind} is ordered by its parts, and this one has a part that cannot be ordered; implement `Ord` for it: `impl Ord for {blamed} {{ fn cmp(self, other: {blamed}) -> int {{ ... }} }}`"
                )
            }
            Ty::Int(_) | Ty::Float | Ty::F32
                if crate::types::is_operator(ob.iface)
                    && matches!(ob.args.first(), Some(a @ (Ty::Int(_) | Ty::Float | Ty::F32)) if a != blame) =>
            {
                let other = ob.args.first().map(|a| self.render(a)).unwrap_or_default();
                format!(
                    "`{blamed}` and `{other}` are different types, and an arithmetic operator takes two operands of one type: convert one of them, for example `{blamed}(x)`"
                )
            }
            Ty::Int(_) | Ty::Float | Ty::F32 | Ty::Str | Ty::Bytes | Ty::Bool | Ty::Char
                if crate::types::is_operator(ob.iface)
                    && !matches!(ob.args.first(), Some(Ty::Adt(id, _)) if id.0 >= crate::types::FIRST_USER_TYPE) =>
            {
                format!(
                    "`{blamed}` has no `{}` of this shape and the orphan rule does not let a program add one; use a type of your own, or `int`, `float`, `u64` or `str` where the bound allows it",
                    self.types[ob.iface.0 as usize].name
                )
            }
            _ => format!("add `impl {iface} for {blamed} {{ ... }}`"),
        }
    }
}
