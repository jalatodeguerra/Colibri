//! Exhaustiveness and reachability of `match` arms: the usefulness algorithm of Maranget,
//! "Warnings for pattern matching" (JFP 2007).
//!
//! Patterns are lowered to a small constructor language. Enums, `bool`, tuples, structs and
//! `()` have finite signatures. Integers, chars and strings are infinite domains: a literal or
//! range is an opaque constructor that never completes a signature, so only `_` or a binding
//! covers the rest. Ranges are compared by identity, not by overlap, which can only make the
//! analysis report fewer unreachable arms, never a wrong one.
//!
//! The analysis has a work budget so that a pathological `match` cannot stall the checker;
//! when it runs out it sets [`Analysis::gave_up`], and the checker reports an error instead of
//! accepting a `match` it could not prove exhaustive.

use std::cell::{Cell, RefCell};

use crate::hir::{GlobalId, Lit, Pat, PatKind, TypeDef, TypeDefKind};
use crate::types::{Ty, TypeId, VARIANT_NONE, VARIANT_SOME};

/// How many recursive steps one analysis may take.
const STEP_BUDGET: u32 = 200_000;
/// How many example missing patterns are collected.
const MAX_WITNESSES: usize = 16;

/// A pattern as the analysis sees it.
#[derive(Clone, Debug)]
enum P {
    Wild,
    Ctor(Ctor, Vec<P>),
    Or(Vec<P>),
}

/// A constructor: what a pattern tests at its head.
#[derive(Clone, Debug, PartialEq)]
enum Ctor {
    /// The only constructor of a tuple, a struct or `()`.
    Single,
    Variant(u32),
    Bool(bool),
    /// A literal, range or constant: compared for identity, never exhausts its type.
    Opaque(Opaque),
}

#[derive(Clone, Debug, PartialEq)]
enum Opaque {
    Lit(Lit),
    Range(Option<Lit>, Option<Lit>, bool),
    Const(GlobalId),
}

impl P {
    /// Matches every value.
    fn irrefutable(&self) -> bool {
        match self {
            P::Wild => true,
            P::Or(alts) => alts.iter().any(P::irrefutable),
            P::Ctor(..) => false,
        }
    }
}

fn lower(pat: &Pat) -> P {
    match &pat.kind {
        PatKind::Wild | PatKind::Bind(_, None) => P::Wild,
        PatKind::Bind(_, Some(inner)) => lower(inner),
        PatKind::Lit(Lit::Bool(b)) => P::Ctor(Ctor::Bool(*b), Vec::new()),
        PatKind::Lit(lit) => P::Ctor(Ctor::Opaque(Opaque::Lit(lit.clone())), Vec::new()),
        PatKind::Range { lo, hi, inclusive } => P::Ctor(
            Ctor::Opaque(Opaque::Range(lo.clone(), hi.clone(), *inclusive)),
            Vec::new(),
        ),
        PatKind::Const(g) => P::Ctor(Ctor::Opaque(Opaque::Const(*g)), Vec::new()),
        PatKind::Variant { variant, args, .. } => {
            P::Ctor(Ctor::Variant(*variant), args.iter().map(lower).collect())
        }
        PatKind::Struct { fields, .. } => P::Ctor(Ctor::Single, fields.iter().map(lower).collect()),
        PatKind::Tuple(items) => P::Ctor(Ctor::Single, items.iter().map(lower).collect()),
        PatKind::Or(alts) => P::Or(alts.iter().map(lower).collect()),
    }
}

/// A pattern that is not matched by any arm, for the error message.
#[derive(Clone, Debug)]
enum W {
    Wild,
    Ctor(Head, Vec<W>),
}

#[derive(Clone, Debug)]
enum Head {
    Variant(String),
    Tuple,
    Struct(String, Vec<String>),
    Bool(bool),
}

impl W {
    fn is_wild(&self) -> bool {
        matches!(self, W::Wild)
    }

    fn render(&self) -> String {
        match self {
            W::Wild => "_".to_string(),
            W::Ctor(head, args) => {
                let list = |args: &[W]| args.iter().map(W::render).collect::<Vec<_>>().join(", ");
                match head {
                    Head::Bool(b) => b.to_string(),
                    Head::Variant(name) if args.is_empty() => name.clone(),
                    Head::Variant(name) => format!("{name}({})", list(args)),
                    Head::Tuple if args.len() == 1 => format!("({},)", list(args)),
                    Head::Tuple => format!("({})", list(args)),
                    Head::Struct(name, fields) => {
                        let shown: Vec<String> = fields
                            .iter()
                            .zip(args)
                            .filter(|(_, a)| !a.is_wild())
                            .map(|(f, a)| format!("{f}: {}", a.render()))
                            .collect();
                        if shown.is_empty() {
                            format!("{name} {{ .. }}")
                        } else if shown.len() == fields.len() {
                            format!("{name} {{ {} }}", shown.join(", "))
                        } else {
                            format!("{name} {{ {}, .. }}", shown.join(", "))
                        }
                    }
                }
            }
        }
    }
}

/// One arm as the analysis needs it.
pub struct ArmInfo<'p> {
    pub pat: &'p Pat,
    /// An arm with a guard may fail after its pattern matched, so it covers nothing.
    pub guarded: bool,
}

/// What the analysis found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Analysis {
    /// Example values no arm matches (rendered as patterns); empty when exhaustive.
    pub missing: Vec<String>,
    /// Whether more examples exist than are listed.
    pub more_missing: bool,
    /// Indices of arms that can never run because earlier arms cover them.
    pub unreachable: Vec<usize>,
    /// The work budget ran out before the analysis finished; nothing else is reported.
    pub gave_up: bool,
    /// The enums that can gain variants (a host module's own) that took part.
    pub open_enums: Vec<TypeId>,
    /// There are missing cases, and they would all be covered if those enums had only the
    /// variants they list: a missing `_` is then not the fault of the arms listed.
    pub only_open: bool,
}

/// Analyses `arms` against a scrutinee of type `scrutinee`. The type must be resolved.
pub fn analyze(types: &[TypeDef], scrutinee: &Ty, arms: &[ArmInfo<'_>]) -> Analysis {
    analyze_within(types, scrutinee, arms, STEP_BUDGET)
}

/// [`analyze`] with a budget of `budget` steps for each of its two questions.
fn analyze_within(
    types: &[TypeDef],
    scrutinee: &Ty,
    arms: &[ArmInfo<'_>],
    budget: u32,
) -> Analysis {
    let cx = Cx {
        types,
        budget: Cell::new(budget),
        steps: Cell::new(0),
        open: RefCell::new(Vec::new()),
        open_as_closed: Cell::new(false),
    };
    let tys = [scrutinee.clone()];
    let lowered: Vec<P> = arms.iter().map(|arm| lower(arm.pat)).collect();
    // A pattern that does not fit the type was reported already; analysing it would only
    // produce follow-on noise.
    if !lowered.iter().all(|p| cx.shaped(p, scrutinee)) {
        return Analysis::default();
    }
    let mut rows: Vec<Vec<P>> = Vec::new();
    let mut unreachable = Vec::new();
    // The "this arm never runs" questions have a budget of their own: running out of it only
    // means that no arm is reported as unreachable, never that the match is "too complex".
    // It is a tenth of the whole one: the answer is a nicety, and the time of a match that
    // cannot be searched is spent here first.
    let mut reach_ran_out = false;
    cx.budget.set(budget / 10);
    for (i, (arm, pat)) in arms.iter().zip(lowered).enumerate() {
        let row = vec![pat];
        if !reach_ran_out {
            if !cx.useful(&rows, &row, &tys) && !cx.exhausted() {
                unreachable.push(i);
            }
            reach_ran_out = cx.exhausted();
        }
        if !arm.guarded {
            rows.push(row);
        }
    }
    if reach_ran_out {
        unreachable.clear();
    }
    cx.steps.set(0);
    cx.budget.set(budget);
    // An unguarded arm that matches everything makes the match exhaustive: no search.
    let found = if rows.iter().any(|r| r.iter().all(P::irrefutable)) {
        Vec::new()
    } else {
        cx.witnesses(&rows, &tys)
    };
    let more_missing = found.len() >= MAX_WITNESSES;
    let open_enums = cx.open.borrow().clone();
    // Would the match be exhaustive if the open enums had no variants but those listed? Then
    // the cases are missing because they are open, not because an arm is absent.
    let only_open = !found.is_empty() && !open_enums.is_empty() && !cx.exhausted() && {
        // The second pass has a budget of its own: running out of it must not turn a real
        // "missing case" of the first into "could not prove exhaustive". If it runs out, the
        // answer is "not only because of open enums".
        let spent = cx.steps.replace(0);
        cx.open_as_closed.set(true);
        let closed = cx.witnesses(&rows, &tys);
        let ran_out = cx.exhausted();
        cx.open_as_closed.set(false);
        cx.steps.set(spent);
        !ran_out && closed.is_empty()
    };
    let missing = if cx.exhausted() {
        Vec::new()
    } else {
        found
            .iter()
            .filter_map(|w| w.first().map(W::render))
            .collect()
    };
    Analysis {
        missing,
        more_missing,
        unreachable: if cx.exhausted() {
            Vec::new()
        } else {
            unreachable
        },
        gave_up: cx.exhausted(),
        open_enums,
        only_open,
    }
}

struct Cx<'t> {
    types: &'t [TypeDef],
    budget: Cell<u32>,
    steps: Cell<u32>,
    /// The open enums (see [`TypeDef::is_open_enum`]) the analysis met.
    open: RefCell<Vec<TypeId>>,
    /// Treat an open enum as closed over its listed variants (the second question of
    /// [`analyze`]).
    open_as_closed: Cell<bool>,
}

impl Cx<'_> {
    fn exhausted(&self) -> bool {
        self.steps.get() > self.budget.get()
    }

    /// Counts a step; `false` once the budget is gone.
    fn step(&self) -> bool {
        self.steps.set(self.steps.get().saturating_add(1));
        !self.exhausted()
    }

    /// Every constructor of `ty`, or `None` for an infinite domain.
    fn all_ctors(&self, ty: &Ty) -> Option<Vec<Ctor>> {
        match ty {
            Ty::Bool => Some(vec![Ctor::Bool(false), Ctor::Bool(true)]),
            Ty::Unit | Ty::Tuple(_) => Some(vec![Ctor::Single]),
            Ty::Option(_) => Some(vec![
                Ctor::Variant(VARIANT_NONE),
                Ctor::Variant(VARIANT_SOME),
            ]),
            Ty::Adt(id, _) => match &self.types.get(id.0 as usize)?.kind {
                TypeDefKind::Struct { .. } | TypeDefKind::Class { .. } => Some(vec![Ctor::Single]),
                // A host module's own enum can gain variants (DESIGN §10.2): only a catch-all
                // arm covers it, like an integer.
                TypeDefKind::Enum { .. }
                    if self.types[id.0 as usize].is_open_enum() && !self.open_as_closed.get() =>
                {
                    let mut open = self.open.borrow_mut();
                    if !open.contains(id) {
                        open.push(*id);
                    }
                    None
                }
                TypeDefKind::Enum { variants } => {
                    Some((0..variants.len() as u32).map(Ctor::Variant).collect())
                }
                TypeDefKind::Interface { .. } | TypeDefKind::HostClass => None,
            },
            _ => None,
        }
    }

    /// The types of the sub-patterns of constructor `c` of a value of type `ty`.
    fn sub_tys(&self, c: &Ctor, ty: &Ty) -> Vec<Ty> {
        match (c, ty) {
            (Ctor::Single, Ty::Tuple(items)) => items.clone(),
            (Ctor::Variant(VARIANT_SOME), Ty::Option(t)) => vec![(**t).clone()],
            (Ctor::Variant(_), Ty::Option(_)) => Vec::new(),
            (Ctor::Single, Ty::Adt(id, args)) => self
                .types
                .get(id.0 as usize)
                .map(|d| {
                    let map: Vec<_> = d.params.iter().copied().zip(args.iter().cloned()).collect();
                    // A weak field is matched as the `Option` it reads as.
                    d.fields()
                        .iter()
                        .map(|f| {
                            let t = f.ty.subst(&map);
                            if f.weak { Ty::Option(Box::new(t)) } else { t }
                        })
                        .collect()
                })
                .unwrap_or_default(),
            (Ctor::Variant(v), Ty::Adt(id, args)) => self
                .types
                .get(id.0 as usize)
                .and_then(|d| {
                    let map: Vec<_> = d.params.iter().copied().zip(args.iter().cloned()).collect();
                    d.variants()
                        .get(*v as usize)
                        .map(|v| v.fields.iter().map(|f| f.ty.subst(&map)).collect())
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn head_of(&self, c: &Ctor, ty: &Ty) -> Head {
        match (c, ty) {
            (Ctor::Bool(b), _) => Head::Bool(*b),
            (Ctor::Variant(v), Ty::Option(_)) => {
                Head::Variant(if *v == VARIANT_SOME { "some" } else { "none" }.to_string())
            }
            (Ctor::Variant(v), Ty::Adt(id, _)) => {
                let name = self
                    .types
                    .get(id.0 as usize)
                    .and_then(|d| d.variants().get(*v as usize))
                    .map_or_else(|| "?".to_string(), |v| v.name.clone());
                Head::Variant(name)
            }
            (Ctor::Single, Ty::Adt(id, _)) => match self.types.get(id.0 as usize) {
                Some(def) => Head::Struct(
                    def.display_name().into_owned(),
                    def.fields().iter().map(|f| f.name.clone()).collect(),
                ),
                None => Head::Tuple,
            },
            _ => Head::Tuple,
        }
    }

    /// Whether a pattern fits the shape of the type it is matched against.
    fn shaped(&self, p: &P, ty: &Ty) -> bool {
        match p {
            P::Wild => true,
            P::Or(alts) => alts.iter().all(|a| self.shaped(a, ty)),
            P::Ctor(c, args) => {
                let fits = match c {
                    Ctor::Opaque(_) => args.is_empty(),
                    Ctor::Bool(_) => *ty == Ty::Bool && args.is_empty(),
                    Ctor::Single => {
                        matches!(ty, Ty::Unit | Ty::Tuple(_))
                            || matches!(ty, Ty::Adt(id, _) if self.types.get(id.0 as usize).is_some_and(|d| !d.is_enum()))
                    }
                    Ctor::Variant(v) => match ty {
                        Ty::Option(_) => *v <= VARIANT_SOME,
                        Ty::Adt(id, _) => self
                            .types
                            .get(id.0 as usize)
                            .is_some_and(|d| (*v as usize) < d.variants().len()),
                        _ => false,
                    },
                };
                let subs = self.sub_tys(c, ty);
                fits && subs.len() == args.len()
                    && args.iter().zip(&subs).all(|(a, t)| self.shaped(a, t))
            }
        }
    }

    /// Whether `q` matches some value that no row of `matrix` matches.
    fn useful(&self, matrix: &[Vec<P>], q: &[P], tys: &[Ty]) -> bool {
        if !self.step() {
            return true;
        }
        let Some(head) = q.first() else {
            return matrix.is_empty();
        };
        let rows = expand(matrix);
        match head {
            P::Or(alts) => alts.iter().any(|alt| {
                let mut q2 = vec![alt.clone()];
                q2.extend_from_slice(&q[1..]);
                self.useful(&rows, &q2, tys)
            }),
            P::Ctor(c, args) => {
                let mut q2 = args.clone();
                q2.extend_from_slice(&q[1..]);
                let mut tys2 = self.sub_tys(c, &tys[0]);
                tys2.extend_from_slice(&tys[1..]);
                self.useful(&specialize(&rows, c, args.len()), &q2, &tys2)
            }
            P::Wild => {
                let heads = head_ctors(&rows);
                match self.all_ctors(&tys[0]) {
                    Some(all) if all.iter().all(|c| heads.contains(c)) => all.iter().any(|c| {
                        let sub = self.sub_tys(c, &tys[0]);
                        let mut q2 = vec![P::Wild; sub.len()];
                        q2.extend_from_slice(&q[1..]);
                        let mut tys2 = sub.clone();
                        tys2.extend_from_slice(&tys[1..]);
                        self.useful(&specialize(&rows, c, sub.len()), &q2, &tys2)
                    }),
                    _ => self.useful(&default_rows(&rows), &q[1..], &tys[1..]),
                }
            }
        }
    }

    /// Example vectors of values (one entry per type of `tys`) that no row matches.
    fn witnesses(&self, matrix: &[Vec<P>], tys: &[Ty]) -> Vec<Vec<W>> {
        if !self.step() {
            return Vec::new();
        }
        let Some(ty) = tys.first() else {
            return if matrix.is_empty() {
                vec![Vec::new()]
            } else {
                Vec::new()
            };
        };
        let rows = expand(matrix);
        let heads = head_ctors(&rows);
        let mut out: Vec<Vec<W>> = Vec::new();
        match self.all_ctors(ty) {
            Some(all) if all.iter().all(|c| heads.contains(c)) => {
                for c in &all {
                    let sub = self.sub_tys(c, ty);
                    let mut tys2 = sub.clone();
                    tys2.extend_from_slice(&tys[1..]);
                    for w in self.witnesses(&specialize(&rows, c, sub.len()), &tys2) {
                        let (args, rest) = w.split_at(sub.len());
                        let mut v = vec![W::Ctor(self.head_of(c, ty), args.to_vec())];
                        v.extend_from_slice(rest);
                        out.push(v);
                        if out.len() >= MAX_WITNESSES {
                            return out;
                        }
                    }
                }
            }
            other => {
                let rest = self.witnesses(&default_rows(&rows), &tys[1..]);
                let missing: Vec<W> = match other {
                    Some(all) => all
                        .iter()
                        .filter(|c| !heads.contains(c))
                        .map(|c| {
                            let n = self.sub_tys(c, ty).len();
                            W::Ctor(self.head_of(c, ty), vec![W::Wild; n])
                        })
                        .collect(),
                    None => vec![W::Wild],
                };
                for m in &missing {
                    for w in &rest {
                        let mut v = vec![m.clone()];
                        v.extend_from_slice(w);
                        out.push(v);
                        if out.len() >= MAX_WITNESSES {
                            return out;
                        }
                    }
                }
            }
        }
        out
    }
}

/// Splits rows whose first pattern is an or-pattern into one row per alternative.
fn expand(matrix: &[Vec<P>]) -> Vec<Vec<P>> {
    let mut out = Vec::with_capacity(matrix.len());
    let mut work: Vec<Vec<P>> = matrix.iter().rev().cloned().collect();
    while let Some(row) = work.pop() {
        match row.first() {
            Some(P::Or(alts)) => {
                for alt in alts.iter().rev() {
                    let mut r = vec![alt.clone()];
                    r.extend_from_slice(&row[1..]);
                    work.push(r);
                }
            }
            _ => out.push(row),
        }
    }
    out
}

/// The constructors at the head of each row.
fn head_ctors(rows: &[Vec<P>]) -> Vec<Ctor> {
    rows.iter()
        .filter_map(|r| match r.first() {
            Some(P::Ctor(c, _)) => Some(c.clone()),
            _ => None,
        })
        .collect()
}

/// The rows that match a value built with `c`, with `c`'s sub-patterns in front.
fn specialize(rows: &[Vec<P>], c: &Ctor, arity: usize) -> Vec<Vec<P>> {
    rows.iter()
        .filter_map(|row| {
            let (head, rest) = row.split_first()?;
            match head {
                P::Wild => {
                    let mut r = vec![P::Wild; arity];
                    r.extend_from_slice(rest);
                    Some(r)
                }
                P::Ctor(c2, args) if c2 == c => {
                    let mut r = args.clone();
                    r.extend_from_slice(rest);
                    Some(r)
                }
                _ => None,
            }
        })
        .collect()
}

/// The rows whose first pattern matches anything, without that pattern.
fn default_rows(rows: &[Vec<P>]) -> Vec<Vec<P>> {
    rows.iter()
        .filter_map(|row| match row.split_first() {
            Some((P::Wild, rest)) => Some(rest.to_vec()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use colibri_syntax::{FileId, Span};

    use super::*;
    use crate::hir::{FieldDef, LocalId, VariantDef};
    use crate::types::TypeId;

    const SP: Span = Span {
        file: FileId(0),
        start: 0,
        end: 0,
    };

    fn pat(kind: PatKind, ty: Ty) -> Pat {
        Pat { kind, ty, span: SP }
    }

    fn wild(ty: &Ty) -> Pat {
        pat(PatKind::Wild, ty.clone())
    }

    fn bind(ty: &Ty) -> Pat {
        pat(PatKind::Bind(LocalId(0), None), ty.clone())
    }

    fn field(name: &str, ty: Ty) -> FieldDef {
        FieldDef {
            name: name.into(),
            ty,
            span: SP,
            weak: false,
            default: None,
            rename: None,
            skip: false,
        }
    }

    fn variant(name: &str, fields: Vec<Ty>) -> VariantDef {
        VariantDef {
            name: name.into(),
            span: SP,
            fields: fields
                .into_iter()
                .enumerate()
                .map(|(i, t)| field(&i.to_string(), t))
                .collect(),
            named: false,
        }
    }

    /// `enum Shape { Circle(float), Rect(float, float), Point }` (type 0), a struct
    /// `P { x: int, ok: bool }` (type 1) and `enum Dir { N, E }` (type 2).
    fn types() -> Vec<TypeDef> {
        vec![
            TypeDef {
                name: "Shape".into(),
                span: SP,
                params: Vec::new(),
                module: None,
                open: false,
                kind: TypeDefKind::Enum {
                    variants: vec![
                        variant("Circle", vec![Ty::Float]),
                        variant("Rect", vec![Ty::Float, Ty::Float]),
                        variant("Point", vec![]),
                    ],
                },
            },
            TypeDef {
                name: "P".into(),
                span: SP,
                params: Vec::new(),
                module: None,
                open: false,
                kind: TypeDefKind::Struct {
                    fields: vec![field("x", Ty::INT), field("ok", Ty::Bool)],
                },
            },
            TypeDef {
                name: "Dir".into(),
                span: SP,
                params: Vec::new(),
                module: None,
                open: false,
                kind: TypeDefKind::Enum {
                    variants: vec![variant("N", vec![]), variant("E", vec![])],
                },
            },
        ]
    }

    fn shape() -> Ty {
        Ty::Adt(TypeId(0), vec![])
    }

    fn circle(arg: Pat) -> Pat {
        pat(
            PatKind::Variant {
                ty: TypeId(0),
                variant: 0,
                args: vec![arg],
            },
            shape(),
        )
    }

    fn rect() -> Pat {
        pat(
            PatKind::Variant {
                ty: TypeId(0),
                variant: 1,
                args: vec![wild(&Ty::Float), wild(&Ty::Float)],
            },
            shape(),
        )
    }

    fn point() -> Pat {
        pat(
            PatKind::Variant {
                ty: TypeId(0),
                variant: 2,
                args: vec![],
            },
            shape(),
        )
    }

    fn boolean(b: bool) -> Pat {
        pat(PatKind::Lit(Lit::Bool(b)), Ty::Bool)
    }

    fn int(n: i64) -> Pat {
        pat(PatKind::Lit(Lit::Int(n)), Ty::INT)
    }

    fn tuple(items: Vec<Pat>) -> Pat {
        let ty = Ty::Tuple(items.iter().map(|p| p.ty.clone()).collect());
        pat(PatKind::Tuple(items), ty)
    }

    fn run(ty: &Ty, arms: &[(&Pat, bool)]) -> Analysis {
        let infos: Vec<ArmInfo<'_>> = arms
            .iter()
            .map(|&(pat, guarded)| ArmInfo { pat, guarded })
            .collect();
        analyze(&types(), ty, &infos)
    }

    fn missing(ty: &Ty, pats: &[Pat]) -> Vec<String> {
        let arms: Vec<(&Pat, bool)> = pats.iter().map(|p| (p, false)).collect();
        run(ty, &arms).missing
    }

    #[test]
    fn complete_enum_match_is_exhaustive() {
        let pats = [circle(bind(&Ty::Float)), rect(), point()];
        assert!(missing(&shape(), &pats).is_empty());
    }

    #[test]
    fn missing_variant_is_listed_with_wildcard_payload() {
        let pats = [circle(wild(&Ty::Float)), point()];
        assert_eq!(missing(&shape(), &pats), ["Rect(_, _)"]);
    }

    #[test]
    fn several_missing_variants_are_all_listed() {
        let pats = [point()];
        assert_eq!(missing(&shape(), &pats), ["Circle(_)", "Rect(_, _)"]);
    }

    #[test]
    fn wildcard_and_binding_cover_everything() {
        assert!(missing(&shape(), &[wild(&shape())]).is_empty());
        assert!(missing(&shape(), &[bind(&shape())]).is_empty());
        assert!(missing(&Ty::INT, &[bind(&Ty::INT)]).is_empty());
    }

    #[test]
    fn literals_never_exhaust_an_infinite_domain() {
        assert_eq!(missing(&Ty::INT, &[int(0), int(1)]), ["_"]);
        let both = [int(0), wild(&Ty::INT)];
        assert!(missing(&Ty::INT, &both).is_empty());
    }

    #[test]
    fn bool_needs_both_values() {
        assert_eq!(missing(&Ty::Bool, &[boolean(true)]), ["false"]);
        assert!(missing(&Ty::Bool, &[boolean(true), boolean(false)]).is_empty());
    }

    #[test]
    fn tuples_are_checked_column_by_column() {
        let ty = Ty::Tuple(vec![Ty::Bool, Ty::Bool]);
        let pats = [
            tuple(vec![boolean(true), boolean(true)]),
            tuple(vec![boolean(false), boolean(false)]),
        ];
        assert_eq!(missing(&ty, &pats), ["(false, true)", "(true, false)"]);
        let full = [
            tuple(vec![boolean(true), wild(&Ty::Bool)]),
            tuple(vec![boolean(false), wild(&Ty::Bool)]),
        ];
        assert!(missing(&ty, &full).is_empty());
    }

    #[test]
    fn nested_patterns_report_the_nested_hole() {
        // (Shape, bool) with `(Circle(_), _)` and `(_, true)` leaves (Rect | Point, false).
        let ty = Ty::Tuple(vec![shape(), Ty::Bool]);
        let pats = [
            tuple(vec![circle(wild(&Ty::Float)), wild(&Ty::Bool)]),
            tuple(vec![wild(&shape()), boolean(true)]),
        ];
        assert_eq!(
            missing(&ty, &pats),
            ["(Rect(_, _), false)", "(Point, false)"]
        );
    }

    #[test]
    fn struct_patterns_show_only_the_interesting_fields() {
        let ty = Ty::Adt(TypeId(1), vec![]);
        let only_true = pat(
            PatKind::Struct {
                ty: TypeId(1),
                fields: vec![wild(&Ty::INT), boolean(true)],
                weak: vec![],
            },
            ty.clone(),
        );
        assert_eq!(missing(&ty, &[only_true]), ["P { ok: false, .. }"]);
        assert_eq!(missing(&ty, &[]), ["P { .. }"]);
    }

    #[test]
    fn or_patterns_cover_each_alternative() {
        let or = pat(PatKind::Or(vec![circle(wild(&Ty::Float)), rect()]), shape());
        assert_eq!(missing(&shape(), &[or]), ["Point"]);
    }

    #[test]
    fn a_guarded_arm_covers_nothing() {
        let (a, b) = (point(), wild(&shape()));
        let result = run(&shape(), &[(&a, true)]);
        assert_eq!(result.missing.len(), 3);
        let result = run(&shape(), &[(&a, true), (&b, false)]);
        assert!(result.missing.is_empty());
        // The wildcard after a guarded arm is still reachable.
        assert!(result.unreachable.is_empty());
    }

    #[test]
    fn arms_after_a_full_cover_are_unreachable() {
        let (a, b, c) = (wild(&shape()), point(), bind(&shape()));
        let result = run(&shape(), &[(&a, false), (&b, false), (&c, false)]);
        assert_eq!(result.unreachable, [1, 2]);
    }

    #[test]
    fn repeated_variant_is_unreachable() {
        let (a, b, c) = (point(), point(), rect());
        let result = run(&shape(), &[(&a, false), (&b, false), (&c, false)]);
        assert_eq!(result.unreachable, [1]);
    }

    #[test]
    fn repeated_literal_is_unreachable_but_ranges_are_compared_by_identity() {
        let (a, b, c) = (int(1), int(1), wild(&Ty::INT));
        let result = run(&Ty::INT, &[(&a, false), (&b, false), (&c, false)]);
        assert_eq!(result.unreachable, [1]);
    }

    #[test]
    fn empty_enum_needs_no_arms() {
        let mut ts = types();
        ts.push(TypeDef {
            name: "Never".into(),
            span: SP,
            params: Vec::new(),
            module: None,
            open: false,
            kind: TypeDefKind::Enum { variants: vec![] },
        });
        let ty = Ty::Adt(TypeId(3), vec![]);
        assert!(analyze(&ts, &ty, &[]).missing.is_empty());
    }

    #[test]
    fn a_match_with_no_arms_misses_everything() {
        assert_eq!(missing(&Ty::INT, &[]), ["_"]);
    }

    #[test]
    fn unit_pattern_is_exhaustive() {
        let unit = pat(PatKind::Tuple(vec![]), Ty::Unit);
        assert!(missing(&Ty::Unit, &[unit]).is_empty());
    }

    #[test]
    fn wide_matches_stay_within_budget() {
        // 14 bool columns with every row `(true, ..., false)`: the analysis must terminate.
        let ty = Ty::Tuple(vec![Ty::Bool; 14]);
        let rows: Vec<Pat> = (0..14)
            .map(|i| {
                tuple(
                    (0..14)
                        .map(|j| {
                            if i == j {
                                boolean(true)
                            } else {
                                wild(&Ty::Bool)
                            }
                        })
                        .collect(),
                )
            })
            .collect();
        let result = missing(&ty, &rows);
        assert!(result.len() <= MAX_WITNESSES);
    }

    /// `enum Host { A, B }` (type 3) of a host module, which can gain variants.
    fn with_open_enum() -> Vec<TypeDef> {
        let mut ts = types();
        ts.push(TypeDef {
            name: "Host".into(),
            span: SP,
            params: Vec::new(),
            module: Some("host".into()),
            open: true,
            kind: TypeDefKind::Enum {
                variants: vec![variant("A", vec![]), variant("B", vec![])],
            },
        });
        ts
    }

    #[test]
    fn a_second_pass_that_runs_out_does_not_turn_missing_cases_into_giving_up() {
        // `(Host, bool x 9)`: for `A` and for `B` there is an arm `false` at column i (any
        // other column anything) for each i, and one with all `true`: every case of the
        // closed `Host` is covered, so only the open enum's `_` is missing. The first question
        // finds that at once; the second (`Host` taken as closed) has to search everything again.
        let k = 9;
        let ts = with_open_enum();
        let host = Ty::Adt(TypeId(3), vec![]);
        let mut cols = vec![host.clone()];
        cols.extend(vec![Ty::Bool; k]);
        let ty = Ty::Tuple(cols);
        let mut rows: Vec<Pat> = Vec::new();
        for variant in 0..2 {
            for i in 0..=k {
                let name = pat(
                    PatKind::Variant {
                        ty: TypeId(3),
                        variant,
                        args: vec![],
                    },
                    host.clone(),
                );
                let mut items = vec![name];
                items.extend((0..k).map(|j| {
                    if i == k {
                        boolean(true)
                    } else if j == i {
                        boolean(false)
                    } else {
                        wild(&Ty::Bool)
                    }
                }));
                rows.push(tuple(items));
            }
        }
        let arms: Vec<ArmInfo<'_>> = rows
            .iter()
            .map(|pat| ArmInfo {
                pat,
                guarded: false,
            })
            .collect();
        let full = analyze_within(&ts, &ty, &arms, STEP_BUDGET);
        assert!(!full.gave_up && full.only_open, "{full:?}");
        assert_eq!(full.open_enums, [TypeId(3)]);
        // The first question takes under 20 steps and the second 112: a budget of 112 holds each
        // but not their sum, which is what gave "could not prove exhaustive" before.
        let tight = analyze_within(&ts, &ty, &arms, 112);
        assert!(
            !tight.gave_up,
            "the second question has a budget of its own"
        );
        assert!(tight.only_open && !tight.missing.is_empty());
        assert_eq!(tight.missing, full.missing);
        // Out of budget in the first question is still giving up.
        assert!(analyze_within(&ts, &ty, &arms, 5).gave_up);
    }

    #[test]
    fn a_final_wildcard_makes_a_match_exhaustive_however_many_arms_come_first() {
        // A tuple of 14 bools, one arm for each ordered pair (column i true, column j
        // false) and a final `_`: the reachability questions used up the budget of the
        // whole analysis and the match was reported as too complex.
        let k = 14;
        let ty = Ty::Tuple(vec![Ty::Bool; k]);
        let mut rows: Vec<Pat> = Vec::new();
        for i in 0..k {
            for j in 0..k {
                if i != j {
                    rows.push(tuple(
                        (0..k)
                            .map(|c| {
                                if c == i {
                                    boolean(true)
                                } else if c == j {
                                    boolean(false)
                                } else {
                                    wild(&Ty::Bool)
                                }
                            })
                            .collect(),
                    ));
                }
            }
        }
        rows.push(wild(&ty));
        let arms: Vec<ArmInfo<'_>> = rows
            .iter()
            .map(|pat| ArmInfo {
                pat,
                guarded: false,
            })
            .collect();
        let started = std::time::Instant::now();
        let out = analyze(&[], &ty, &arms);
        eprintln!("14 bools, 183 arms: {:?}", started.elapsed());
        assert!(!out.gave_up, "{out:?}");
        assert!(out.missing.is_empty());
        // Only the reporting of unreachable arms is given up when its own budget runs out.
        let tight = analyze_within(&[], &ty, &arms, 50);
        assert!(!tight.gave_up && tight.unreachable.is_empty(), "{tight:?}");
    }
}
