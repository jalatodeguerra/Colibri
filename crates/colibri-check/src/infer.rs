//! Unification of inference variables, scoped to one function body (DESIGN §5.4, §17.3).
//!
//! Variables come in three kinds. A *general* variable stands for any type. An *integer
//! literal* variable (the type of an untyped `1`) may become any integer or float type, and a
//! *float literal* variable (the type of `2.0`) only a float type, which is how untyped
//! numeric literals adapt to what they are used as (DESIGN §3.4). Whatever is left unbound at
//! the end of the function defaults to `int` / `float`.

use crate::types::{Ty, TyVar};

/// What a variable is allowed to become.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarKind {
    General,
    /// The type of an untyped integer literal.
    IntLit,
    /// The type of an untyped float literal.
    FloatLit,
}

#[derive(Clone, Debug)]
struct Var {
    kind: VarKind,
    bound: Option<Ty>,
}

/// Two types that cannot be made equal.
#[derive(Clone, Debug, PartialEq)]
pub struct Mismatch;

/// The unification table of one function body.
#[derive(Default, Debug)]
pub struct InferCtx {
    vars: Vec<Var>,
}

impl InferCtx {
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh unbound variable of the given kind.
    pub fn fresh(&mut self, kind: VarKind) -> Ty {
        self.vars.push(Var { kind, bound: None });
        Ty::Infer(TyVar(self.vars.len() as u32 - 1))
    }

    /// The kind of `ty` if it is an unbound variable.
    pub fn unbound_kind(&self, ty: &Ty) -> Option<VarKind> {
        match self.shallow(ty) {
            Ty::Infer(v) => Some(self.vars[v.0 as usize].kind),
            _ => None,
        }
    }

    /// Follows variable bindings at the top of `ty`.
    pub fn shallow(&self, ty: &Ty) -> Ty {
        let mut ty = ty.clone();
        while let Ty::Infer(v) = ty {
            match &self.vars[v.0 as usize].bound {
                Some(next) => ty = next.clone(),
                None => return Ty::Infer(v),
            }
        }
        ty
    }

    /// `ty` with every bound variable replaced; unbound ones stay.
    pub fn resolve(&self, ty: &Ty) -> Ty {
        self.map(ty, &mut |v| Ty::Infer(v))
    }

    /// Like [`InferCtx::resolve`], but unbound literal variables become their default (`int`,
    /// `float`) and unbound general variables become [`Ty::Error`]. Returns whether a general
    /// variable was left unresolved.
    pub fn finalize(&mut self, ty: &Ty) -> (Ty, bool) {
        let mut unresolved = false;
        let kinds: Vec<VarKind> = self.vars.iter().map(|v| v.kind).collect();
        let out = self.map(ty, &mut |v| match kinds[v.0 as usize] {
            VarKind::IntLit => Ty::INT,
            VarKind::FloatLit => Ty::Float,
            VarKind::General => {
                unresolved = true;
                Ty::Error
            }
        });
        (out, unresolved)
    }

    fn map(&self, ty: &Ty, unbound: &mut dyn FnMut(TyVar) -> Ty) -> Ty {
        let ty = self.shallow(ty);
        let mut go = |t: &Ty| self.map(t, unbound);
        match &ty {
            Ty::Infer(v) => unbound(*v),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(&mut go).collect()),
            Ty::Adt(id, ts) => Ty::Adt(*id, ts.iter().map(&mut go).collect()),
            Ty::Dyn(id, ts) => Ty::Dyn(*id, ts.iter().map(&mut go).collect()),
            Ty::List(t) => Ty::List(Box::new(go(t))),
            Ty::Set(t) => Ty::Set(Box::new(go(t))),
            Ty::Option(t) => Ty::Option(Box::new(go(t))),
            Ty::Range(t) => Ty::Range(Box::new(go(t))),
            Ty::Map(k, v) => Ty::Map(Box::new(go(k)), Box::new(go(v))),
            Ty::Result(t, e) => Ty::Result(Box::new(go(t)), Box::new(go(e))),
            Ty::Fn(ps, r) => Ty::Fn(ps.iter().map(&mut go).collect(), Box::new(go(r))),
            _ => ty,
        }
    }

    /// Makes `a` and `b` the same type, binding variables as needed. [`Ty::Error`] unifies with
    /// everything.
    pub fn unify(&mut self, a: &Ty, b: &Ty) -> Result<(), Mismatch> {
        let a = self.shallow(a);
        let b = self.shallow(b);
        match (&a, &b) {
            (Ty::Error, _) | (_, Ty::Error) => Ok(()),
            (Ty::Infer(x), Ty::Infer(y)) if x == y => Ok(()),
            (Ty::Infer(x), Ty::Infer(y)) => {
                let (kx, ky) = (self.kind(*x), self.kind(*y));
                // Bind the more general variable to the more specific one. An integer literal
                // meeting a float literal becomes a float.
                let (from, to) = match (kx, ky) {
                    (VarKind::General, _) => (*x, *y),
                    (_, VarKind::General) => (*y, *x),
                    (VarKind::IntLit, _) => (*x, *y),
                    (_, VarKind::IntLit) => (*y, *x),
                    _ => (*x, *y),
                };
                self.bind(from, Ty::Infer(to));
                Ok(())
            }
            (Ty::Infer(v), other) | (other, Ty::Infer(v)) => self.bind_to(*v, other),
            (Ty::Tuple(xs), Ty::Tuple(ys))
            | (Ty::Adt(_, xs), Ty::Adt(_, ys))
            | (Ty::Dyn(_, xs), Ty::Dyn(_, ys))
                if xs.len() == ys.len() =>
            {
                if let (Ty::Adt(i, _), Ty::Adt(j, _)) | (Ty::Dyn(i, _), Ty::Dyn(j, _)) = (&a, &b)
                    && i != j
                {
                    return Err(Mismatch);
                }
                xs.iter().zip(ys).try_for_each(|(x, y)| self.unify(x, y))
            }
            (Ty::List(x), Ty::List(y))
            | (Ty::Set(x), Ty::Set(y))
            | (Ty::Option(x), Ty::Option(y))
            | (Ty::Range(x), Ty::Range(y)) => self.unify(x, y),
            (Ty::Map(k1, v1), Ty::Map(k2, v2)) | (Ty::Result(k1, v1), Ty::Result(k2, v2)) => {
                self.unify(k1, k2)?;
                self.unify(v1, v2)
            }
            (Ty::Fn(p1, r1), Ty::Fn(p2, r2)) if p1.len() == p2.len() => {
                p1.iter().zip(p2).try_for_each(|(x, y)| self.unify(x, y))?;
                self.unify(r1, r2)
            }
            _ if a == b => Ok(()),
            _ => Err(Mismatch),
        }
    }

    /// Whether `actual` can be used where `expected` is wanted: unification, except that
    /// [`Ty::Never`] converts to anything.
    pub fn coerce(&mut self, actual: &Ty, expected: &Ty) -> Result<(), Mismatch> {
        if self.shallow(actual) == Ty::Never {
            return Ok(());
        }
        self.unify(actual, expected)
    }

    fn kind(&self, v: TyVar) -> VarKind {
        self.vars[v.0 as usize].kind
    }

    fn bind(&mut self, v: TyVar, ty: Ty) {
        self.vars[v.0 as usize].bound = Some(ty);
    }

    fn bind_to(&mut self, v: TyVar, ty: &Ty) -> Result<(), Mismatch> {
        let ok = match self.kind(v) {
            VarKind::General => !self.occurs(v, ty),
            VarKind::IntLit => matches!(ty, Ty::Int(_) | Ty::Float),
            VarKind::FloatLit => matches!(ty, Ty::Float),
        };
        if ok {
            self.bind(v, ty.clone());
            Ok(())
        } else {
            Err(Mismatch)
        }
    }

    fn occurs(&self, v: TyVar, ty: &Ty) -> bool {
        match self.shallow(ty) {
            Ty::Infer(w) => v == w,
            Ty::Tuple(ts) | Ty::Adt(_, ts) | Ty::Dyn(_, ts) => ts.iter().any(|t| self.occurs(v, t)),
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Range(t) => self.occurs(v, &t),
            Ty::Map(a, b) | Ty::Result(a, b) => self.occurs(v, &a) || self.occurs(v, &b),
            Ty::Fn(ps, r) => ps.iter().any(|t| self.occurs(v, t)) || self.occurs(v, &r),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_adapts_to_float_but_not_to_str() {
        let mut cx = InferCtx::new();
        let lit = cx.fresh(VarKind::IntLit);
        assert!(cx.unify(&lit, &Ty::Float).is_ok());
        assert_eq!(cx.resolve(&lit), Ty::Float);

        let lit = cx.fresh(VarKind::IntLit);
        assert!(cx.unify(&lit, &Ty::Str).is_err());
    }

    #[test]
    fn float_literal_rejects_int() {
        let mut cx = InferCtx::new();
        let lit = cx.fresh(VarKind::FloatLit);
        assert!(cx.unify(&lit, &Ty::INT).is_err());
        assert!(cx.unify(&lit, &Ty::Float).is_ok());
    }

    #[test]
    fn int_and_float_literals_unify_to_float() {
        let mut cx = InferCtx::new();
        let a = cx.fresh(VarKind::IntLit);
        let b = cx.fresh(VarKind::FloatLit);
        assert!(cx.unify(&a, &b).is_ok());
        assert_eq!(cx.finalize(&a).0, Ty::Float);
        assert_eq!(cx.finalize(&b).0, Ty::Float);
    }

    #[test]
    fn unbound_literals_default() {
        let mut cx = InferCtx::new();
        let a = cx.fresh(VarKind::IntLit);
        let b = cx.fresh(VarKind::FloatLit);
        let c = cx.fresh(VarKind::General);
        assert_eq!(cx.finalize(&a), (Ty::INT, false));
        assert_eq!(cx.finalize(&b), (Ty::Float, false));
        assert_eq!(cx.finalize(&c), (Ty::Error, true));
    }

    #[test]
    fn general_variables_bind_and_chain() {
        let mut cx = InferCtx::new();
        let a = cx.fresh(VarKind::General);
        let b = cx.fresh(VarKind::General);
        assert!(cx.unify(&a, &b).is_ok());
        assert!(cx.unify(&b, &Ty::Str).is_ok());
        assert_eq!(cx.resolve(&a), Ty::Str);
        assert!(cx.unify(&a, &Ty::Bool).is_err());
    }

    #[test]
    fn occurs_check_rejects_infinite_types() {
        let mut cx = InferCtx::new();
        let a = cx.fresh(VarKind::General);
        let list = Ty::List(Box::new(a.clone()));
        assert!(cx.unify(&a, &list).is_err());
    }

    #[test]
    fn structural_unification() {
        let mut cx = InferCtx::new();
        let a = cx.fresh(VarKind::General);
        let x = Ty::Tuple(vec![a.clone(), Ty::Str]);
        let y = Ty::Tuple(vec![Ty::Bool, Ty::Str]);
        assert!(cx.unify(&x, &y).is_ok());
        assert_eq!(cx.resolve(&a), Ty::Bool);
        assert!(
            cx.unify(&Ty::Tuple(vec![Ty::Bool]), &Ty::Tuple(vec![]))
                .is_err()
        );
    }

    #[test]
    fn error_and_never() {
        let mut cx = InferCtx::new();
        assert!(cx.unify(&Ty::Error, &Ty::Str).is_ok());
        assert!(cx.coerce(&Ty::Never, &Ty::Str).is_ok());
        assert!(cx.unify(&Ty::Never, &Ty::Str).is_err());
    }
}
