//! Interface implementations: which types implement which interfaces (DESIGN §9.2, §7.4).
//!
//! A type implements an interface through an explicit `impl`, through a derived
//! implementation of a built-in interface (`Eq`, `Ord`, `Hash` follow the structure of the
//! type; `Show` and `Clone` exist for every type), through a bound when it is a type
//! parameter, or because it is the interface value itself. [`Env::implements`] answers the
//! question for bounds, operators and `Map` keys alike, and names the part of the type that
//! is to blame when the answer is no.

use std::collections::HashMap;

use colibri_syntax::Span;

use crate::hir::{ImplId, TypeDef, TypeDefKind, TypeKey};
use crate::types::{
    CLONE, EQ, ERROR_TYPE, HASH, INTO_ERROR, ITER, ITERABLE, OPTION_TYPE, ORD, ParamId, SHOW, Ty,
    TypeId,
};

/// `T: Hash + Eq`, one bound: an interface and its type arguments.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BoundRef {
    pub iface: TypeId,
    pub args: Vec<Ty>,
    pub span: Span,
}

/// A declared type parameter with its bounds.
#[derive(Clone, Debug)]
pub(crate) struct GenericParamInfo {
    pub id: ParamId,
    pub name: String,
    pub span: Span,
    pub bounds: Vec<BoundRef>,
}

impl GenericParamInfo {
    /// The parameter as a type.
    pub fn ty(&self) -> Ty {
        Ty::Param(self.id, self.name.as_str().into())
    }
}

/// An `impl Interface for Type` block, as the checker keeps it.
#[derive(Clone, Debug)]
pub(crate) struct ImplInfo {
    pub iface: TypeId,
    pub iface_args: Vec<Ty>,
    pub target: Ty,
    pub key: TypeKey,
    /// The type parameters of the `impl[..]` header.
    pub generics: Vec<GenericParamInfo>,
    /// One entry per interface method: the impl's own function, if it wrote one.
    pub methods: Vec<Option<crate::hir::FnId>>,
    pub span: Span,
    pub derived: bool,
}

/// How a type satisfies an interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// An explicit `impl`.
    User(ImplId),
    /// Derived from the structure of the type (DESIGN §7.4).
    Derived,
    /// A type parameter with the interface among its bounds.
    Bound,
    /// The interface value itself.
    Dyn,
}

/// The head of a type, which interface dispatch works on.
pub(crate) fn key_of(ty: &Ty) -> Option<TypeKey> {
    Some(match ty {
        Ty::Int(t) => TypeKey::Int(*t),
        Ty::Float => TypeKey::Float,
        Ty::F32 => TypeKey::F32,
        Ty::Bool => TypeKey::Bool,
        Ty::Char => TypeKey::Char,
        Ty::Str => TypeKey::Str,
        Ty::Unit => TypeKey::Unit,
        Ty::Tuple(_) => TypeKey::Tuple,
        Ty::List(_) => TypeKey::List,
        Ty::Map(..) => TypeKey::Map,
        Ty::Set(_) => TypeKey::Set,
        Ty::Range(_) => TypeKey::Range,
        Ty::Ptr(_) => TypeKey::Ptr,
        Ty::Vec(v) => TypeKey::Vec(*v),
        Ty::Option(_) => TypeKey::Adt(OPTION_TYPE),
        Ty::Adt(id, _) => TypeKey::Adt(*id),
        _ => return None,
    })
}

/// What the implementation queries need to see of the checker.
pub(crate) struct Env<'a> {
    pub types: &'a [TypeDef],
    pub impls: &'a [ImplInfo],
    /// The explicit implementations, by implementing type head and interface.
    pub by_key: &'a HashMap<(TypeKey, TypeId), ImplId>,
    /// The bounds of every type parameter.
    pub bounds: &'a HashMap<ParamId, Vec<BoundRef>>,
}

/// Binds the parameters of `pattern` (those in `params`) so that it equals `ty`. An unresolved
/// or erroneous part of `ty` matches anything.
pub(crate) fn match_ty(
    pattern: &Ty,
    ty: &Ty,
    params: &[ParamId],
    subst: &mut Vec<(ParamId, Ty)>,
) -> bool {
    let all = |ps: &[Ty], ts: &[Ty], subst: &mut Vec<(ParamId, Ty)>| {
        ps.len() == ts.len()
            && ps
                .iter()
                .zip(ts)
                .all(|(p, t)| match_ty(p, t, params, subst))
    };
    match (pattern, ty) {
        (Ty::Param(p, _), _) if params.contains(p) => match subst.iter().find(|(q, _)| q == p) {
            Some((_, bound)) => bound == ty || matches!(ty, Ty::Infer(_) | Ty::Error),
            None => {
                subst.push((*p, ty.clone()));
                true
            }
        },
        (_, Ty::Infer(_) | Ty::Error) => true,
        (Ty::Tuple(a), Ty::Tuple(b)) => all(a, b, subst),
        (Ty::Adt(i, a), Ty::Adt(j, b)) | (Ty::Dyn(i, a), Ty::Dyn(j, b)) => {
            i == j && all(a, b, subst)
        }
        (Ty::List(a), Ty::List(b)) | (Ty::Set(a), Ty::Set(b)) | (Ty::Option(a), Ty::Option(b)) => {
            match_ty(a, b, params, subst)
        }
        (Ty::Map(a, b), Ty::Map(c, d)) => {
            match_ty(a, c, params, subst) && match_ty(b, d, params, subst)
        }
        (Ty::Range(a), Ty::Range(b)) | (Ty::Ptr(a), Ty::Ptr(b)) => match_ty(a, b, params, subst),
        (Ty::Fn(a, r), Ty::Fn(b, s)) => all(a, b, subst) && match_ty(r, s, params, subst),
        _ => pattern == ty,
    }
}

impl Env<'_> {
    /// Whether `ty` implements the interface `iface[args]`. On failure, the innermost type
    /// that does not (the type itself, or a field, element or argument inside it).
    pub fn implements(&self, ty: &Ty, iface: TypeId, args: &[Ty]) -> Result<Source, Ty> {
        self.implements_in(ty, iface, args, &mut Vec::new())
    }

    fn implements_in(
        &self,
        ty: &Ty,
        iface: TypeId,
        args: &[Ty],
        visiting: &mut Vec<TypeId>,
    ) -> Result<Source, Ty> {
        match ty {
            // An unresolved or erroneous type is not blamed; it was or will be reported.
            Ty::Infer(_) | Ty::Error | Ty::Never => return Ok(Source::Derived),
            Ty::Param(p, _) => return self.param_implements(ty, *p, iface, args),
            Ty::Dyn(id, dargs) if *id == iface && dargs == args => return Ok(Source::Dyn),
            _ => {}
        }
        // Every type has `Show` and `Clone`: each concrete type derives them (§7.4).
        if iface == SHOW || iface == CLONE {
            return if self.user_impl_for(ty, iface).is_some() {
                self.user(ty, iface, args)
            } else if iface == CLONE
                && matches!(ty, Ty::Adt(id, _) if self.types.get(id.0 as usize).is_some_and(TypeDef::is_host_class))
            {
                // A host object is one thing in the host: there is no copy of it to make.
                Err(ty.clone())
            } else {
                Ok(Source::Derived)
            };
        }
        if self.user_impl_for(ty, iface).is_some() {
            return self.user(ty, iface, args);
        }
        match iface {
            EQ => self.derive_with(ty, iface, visiting, Self::eq_part),
            ORD => self.derive_with(ty, iface, visiting, Self::ord_part),
            HASH => self.derive_with(ty, iface, visiting, Self::hash_part),
            ITERABLE => self.iterable_builtin(ty, args),
            // `str` and `Error` convert to `Error` on their own (DESIGN §10.1).
            INTO_ERROR
                if matches!(ty, Ty::Str) || matches!(ty, Ty::Adt(id, _) if *id == ERROR_TYPE) =>
            {
                Ok(Source::Derived)
            }
            _ => Err(ty.clone()),
        }
    }

    fn param_implements(
        &self,
        ty: &Ty,
        p: ParamId,
        iface: TypeId,
        args: &[Ty],
    ) -> Result<Source, Ty> {
        if iface == SHOW || iface == CLONE {
            return Ok(Source::Derived);
        }
        let bounds = self.bounds.get(&p).map(Vec::as_slice).unwrap_or_default();
        let found = bounds.iter().any(|b| {
            (b.iface == iface && b.args == args)
                // Being ordered implies being comparable for equality.
                || (iface == EQ && b.iface == ORD)
        });
        if found {
            Ok(Source::Bound)
        } else {
            Err(ty.clone())
        }
    }

    /// The explicit implementation of `iface` for the head of `ty`, if there is one.
    fn user_impl_for(&self, ty: &Ty, iface: TypeId) -> Option<ImplId> {
        self.by_key.get(&(key_of(ty)?, iface)).copied()
    }

    /// The built-in collections, ranges and iterators are `Iterable` of their items.
    fn iterable_builtin(&self, ty: &Ty, args: &[Ty]) -> Result<Source, Ty> {
        let item = match ty {
            Ty::List(t) | Ty::Set(t) => (**t).clone(),
            Ty::Map(k, v) => Ty::Tuple(vec![(**k).clone(), (**v).clone()]),
            Ty::Range(_) => Ty::INT,
            Ty::Dyn(id, a) if *id == ITER && a.len() == 1 => a[0].clone(),
            _ => return Err(ty.clone()),
        };
        match args.first() {
            Some(a) if !matches!(a, Ty::Infer(_) | Ty::Error) && *a != item => Err(ty.clone()),
            _ => Ok(Source::Derived),
        }
    }

    /// Checks an explicit implementation against a particular instantiation: the target
    /// pattern must match, the interface arguments must agree and the bounds of the `impl`'s
    /// own parameters must hold.
    fn user(&self, ty: &Ty, iface: TypeId, args: &[Ty]) -> Result<Source, Ty> {
        let Some(id) = self.user_impl_for(ty, iface) else {
            return Err(ty.clone());
        };
        let info = &self.impls[id.0 as usize];
        let params: Vec<ParamId> = info.generics.iter().map(|g| g.id).collect();
        let mut subst = Vec::new();
        if !match_ty(&info.target, ty, &params, &mut subst) {
            return Err(ty.clone());
        }
        let wanted: Vec<Ty> = info.iface_args.iter().map(|a| a.subst(&subst)).collect();
        let agree = wanted.len() == args.len()
            && wanted
                .iter()
                .zip(args)
                .all(|(w, a)| matches!(a, Ty::Infer(_) | Ty::Error) || w == a);
        if !agree {
            return Err(ty.clone());
        }
        for g in &info.generics {
            let Some((_, arg)) = subst.iter().find(|(q, _)| *q == g.id) else {
                continue;
            };
            for b in &g.bounds {
                let bargs: Vec<Ty> = b.args.iter().map(|a| a.subst(&subst)).collect();
                self.implements(arg, b.iface, &bargs)?;
            }
        }
        Ok(Source::User(id))
    }

    /// Derives `iface` from the parts of `ty` (DESIGN §7.4). `part` decides the leaf cases and
    /// says which types have to be checked inside `ty`.
    fn derive_with(
        &self,
        ty: &Ty,
        iface: TypeId,
        visiting: &mut Vec<TypeId>,
        part: fn(&Self, &Ty) -> Part,
    ) -> Result<Source, Ty> {
        let inner = match part(self, ty) {
            Part::Ok => return Ok(Source::Derived),
            Part::Fail => return Err(ty.clone()),
            Part::Inside(inner) => inner,
            Part::Fields(id, targs) => {
                if visiting.contains(&id) {
                    return Ok(Source::Derived);
                }
                visiting.push(id);
                let result = self
                    .field_types(id, &targs)
                    .into_iter()
                    .try_for_each(|f| self.implements_in(&f, iface, &[], visiting).map(|_| ()));
                visiting.pop();
                result?;
                return Ok(Source::Derived);
            }
        };
        for t in &inner {
            self.implements_in(t, iface, &[], visiting)?;
        }
        Ok(Source::Derived)
    }

    /// The types of every field of a struct or class, or payload of an enum, instantiated.
    pub fn field_types(&self, id: TypeId, args: &[Ty]) -> Vec<Ty> {
        let Some(def) = self.types.get(id.0 as usize) else {
            return Vec::new();
        };
        let map: Vec<(ParamId, Ty)> = def
            .params
            .iter()
            .copied()
            .zip(args.iter().cloned())
            .collect();
        let raw: Vec<&Ty> = match &def.kind {
            TypeDefKind::Enum { variants } => variants
                .iter()
                .flat_map(|v| v.fields.iter().map(|f| &f.ty))
                .collect(),
            TypeDefKind::Struct { fields } | TypeDefKind::Class { fields } => {
                fields.iter().map(|f| &f.ty).collect()
            }
            TypeDefKind::Interface { .. } | TypeDefKind::HostClass => Vec::new(),
        };
        raw.into_iter().map(|t| t.subst(&map)).collect()
    }

    fn is_class(&self, id: TypeId) -> bool {
        self.types.get(id.0 as usize).is_some_and(TypeDef::is_class)
    }

    /// `==` is derived for scalars, and structurally for the types made of them; a class is
    /// compared by identity (DESIGN §7.2).
    fn eq_part(&self, ty: &Ty) -> Part {
        match ty {
            Ty::Int(_)
            | Ty::Float
            | Ty::F32
            | Ty::Bool
            | Ty::Char
            | Ty::Str
            | Ty::Unit
            | Ty::Ptr(_)
            | Ty::Vec(_)
            | Ty::Range(_) => Part::Ok,
            Ty::Tuple(items) => Part::Inside(items.clone()),
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) => Part::Inside(vec![(**t).clone()]),
            Ty::Map(k, v) => Part::Inside(vec![(**k).clone(), (**v).clone()]),
            Ty::Adt(id, _) if self.is_class(*id) => Part::Ok,
            Ty::Adt(id, args) => Part::Fields(*id, args.clone()),
            _ => Part::Fail,
        }
    }

    /// `<` is derived for scalars, tuples of ordered parts and enums without payloads.
    fn ord_part(&self, ty: &Ty) -> Part {
        match ty {
            Ty::Int(_) | Ty::Float | Ty::F32 | Ty::Bool | Ty::Char | Ty::Str => Part::Ok,
            Ty::Tuple(items) => Part::Inside(items.clone()),
            Ty::Adt(id, _) => match self.types.get(id.0 as usize).map(|d| &d.kind) {
                Some(TypeDefKind::Enum { variants })
                    if variants.iter().all(|v| v.fields.is_empty()) =>
                {
                    Part::Ok
                }
                _ => Part::Fail,
            },
            _ => Part::Fail,
        }
    }

    /// Hashing is derived structurally. A `float` or `f32` is not hashable (`nan != nan`), nor is a
    /// mutable collection; a class hashes by identity (DESIGN §5.3).
    fn hash_part(&self, ty: &Ty) -> Part {
        match ty {
            Ty::Int(_) | Ty::Bool | Ty::Char | Ty::Str | Ty::Unit | Ty::Ptr(_) => Part::Ok,
            Ty::Vec(v) if v.int => Part::Ok,
            Ty::Tuple(items) => Part::Inside(items.clone()),
            Ty::Option(t) => Part::Inside(vec![(**t).clone()]),
            Ty::Adt(id, _) if self.is_class(*id) => Part::Ok,
            Ty::Adt(id, args) => Part::Fields(*id, args.clone()),
            _ => Part::Fail,
        }
    }
}

/// What deriving needs to look at next.
enum Part {
    Ok,
    Fail,
    /// The element types of a tuple, list or map.
    Inside(Vec<Ty>),
    /// The fields of a struct or the payloads of an enum, instantiated with these arguments.
    Fields(TypeId, Vec<Ty>),
}
