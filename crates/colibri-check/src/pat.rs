//! Checking patterns (DESIGN §8.2): what they match, which names they bind, and that every
//! alternative of an or-pattern binds the same names.

use colibri_syntax::ast::{self, BindKind, Lit as AstLit, PatKind as AstPat};
use colibri_syntax::{Diagnostic, Span};

use crate::body::{FnCtx, LocalKind};
use crate::checker::{Checker, Dep, ValueRef};
use crate::hir::{GlobalId, Lit, LocalId, Pat, PatKind};
use crate::types::{Ty, TypeId};

/// The names a pattern binds, and how.
pub(crate) struct PatCx {
    /// How the bindings are declared (`let`, `var`, ...).
    pub kind: LocalKind,
    /// The bindings made so far in the alternative being checked.
    bound: Vec<(String, LocalId)>,
    /// While checking a later alternative of an or-pattern: the bindings of the first one,
    /// which the later ones must reuse.
    template: Option<Vec<(String, LocalId)>>,
}

impl PatCx {
    pub fn new(kind: LocalKind) -> Self {
        PatCx {
            kind,
            bound: Vec::new(),
            template: None,
        }
    }
}

impl FnCtx<'_, '_> {
    fn pat(&self, kind: PatKind, ty: &Ty, span: Span) -> Pat {
        Pat {
            kind,
            ty: ty.clone(),
            span,
        }
    }

    /// Checks `pat` against a value of type `expected`.
    pub fn check_pat(&mut self, pat: &ast::Pat, expected: &Ty, cx: &mut PatCx) -> Pat {
        if !self.enter() {
            self.ck.report(Diagnostic::error(
                "this pattern is nested too deeply",
                pat.span,
            ));
            return self.pat(PatKind::Wild, expected, pat.span);
        }
        let out = self.check_pat_inner(pat, expected, cx);
        self.leave();
        out
    }

    fn check_pat_inner(&mut self, pat: &ast::Pat, expected: &Ty, cx: &mut PatCx) -> Pat {
        let span = pat.span;
        match &pat.kind {
            AstPat::Wild | AstPat::Error => self.pat(PatKind::Wild, expected, span),
            AstPat::Ident(id) if id.is_capitalised() => self.capitalised_pat(id, expected, span),
            AstPat::Ident(id) => {
                let local = self.bind_name(id, expected, cx);
                self.pat(PatKind::Bind(local, None), expected, span)
            }
            AstPat::Lit(lit) => self.lit_pat(lit, expected, span),
            AstPat::Range { lo, hi, inclusive } => {
                self.range_pat(lo.as_ref(), hi.as_ref(), *inclusive, expected, span)
            }
            AstPat::Variant { path, args } => {
                self.variant_pat(path, args.as_deref(), expected, cx, span)
            }
            AstPat::Tuple(items) => self.tuple_pat(items, expected, cx, span),
            AstPat::Struct { path, fields, rest } => {
                self.struct_pat(path, fields, *rest, expected, cx, span)
            }
            AstPat::Or(alts) => self.or_pat(alts, expected, cx, span),
            AstPat::Bind { pat: inner, name } => {
                let inner = self.check_pat(inner, expected, cx);
                let local = self.bind_name(name, expected, cx);
                self.pat(PatKind::Bind(local, Some(Box::new(inner))), expected, span)
            }
            AstPat::Some(_) | AstPat::None | AstPat::Ok(_) | AstPat::Err(_) => {
                self.ck.unsupported("an `Option` or `Result` pattern", span);
                self.pat(PatKind::Wild, expected, span)
            }
        }
    }

    // ---- bindings ----

    /// Declares (or, in a later alternative, reuses) the local for a binding.
    fn bind_name(&mut self, id: &ast::Ident, ty: &Ty, cx: &mut PatCx) -> LocalId {
        if let Some((_, existing)) = cx.bound.iter().find(|(n, _)| *n == id.name) {
            let existing = *existing;
            self.ck.report(
                Diagnostic::error(
                    format!("`{}` is bound more than once in this pattern", id.name),
                    id.span,
                )
                .with_label(self.locals[existing.0 as usize].span, "first bound here")
                .with_hint("use a different name for one of them"),
            );
            return existing;
        }
        let local = match &cx.template {
            Some(template) => match template.iter().find(|(n, _)| *n == id.name) {
                Some(&(_, first)) => {
                    let first_ty = self.local_ty(first);
                    if self.infer.unify(&first_ty, ty).is_err() {
                        let (a, b) = (self.show(&first_ty), self.show(ty));
                        self.ck.report(
                            Diagnostic::error(
                                format!(
                                    "`{}` has type `{a}` in one alternative and `{b}` in another",
                                    id.name
                                ),
                                id.span,
                            )
                            .with_label(self.locals[first.0 as usize].span, "bound here")
                            .with_hint("every alternative of an or-pattern must bind the same names with the same types"),
                        );
                    }
                    first
                }
                None => {
                    self.ck.report(
                        Diagnostic::error(
                            format!("`{}` is not bound in every alternative", id.name),
                            id.span,
                        )
                        .with_hint("every alternative of an or-pattern must bind the same names"),
                    );
                    self.declare(&id.name, id.span, ty.clone(), cx.kind)
                }
            },
            None => self.declare(&id.name, id.span, ty.clone(), cx.kind),
        };
        cx.bound.push((id.name.clone(), local));
        local
    }

    fn or_pat(&mut self, alts: &[ast::Pat], expected: &Ty, cx: &mut PatCx, span: Span) -> Pat {
        let before = cx.bound.len();
        let mut out = Vec::new();
        let Some((first, rest)) = alts.split_first() else {
            return self.pat(PatKind::Wild, expected, span);
        };
        out.push(self.check_pat(first, expected, cx));
        let names: Vec<(String, LocalId)> = cx.bound[before..].to_vec();
        for alt in rest {
            let mut sub = PatCx {
                kind: cx.kind,
                bound: Vec::new(),
                template: Some(names.clone()),
            };
            out.push(self.check_pat(alt, expected, &mut sub));
            for (name, _) in &names {
                if !sub.bound.iter().any(|(n, _)| n == name) {
                    self.ck.report(
                        Diagnostic::error(
                            format!("`{name}` is not bound in every alternative"),
                            alt.span,
                        )
                        .with_hint("every alternative of an or-pattern must bind the same names"),
                    );
                }
            }
        }
        self.pat(PatKind::Or(out), expected, span)
    }

    // ---- capitalised names ----

    /// A capitalised name is a variant or a constant, and must resolve to one (DESIGN §8.2).
    fn capitalised_pat(&mut self, id: &ast::Ident, expected: &Ty, span: Span) -> Pat {
        let known = self.infer.shallow(expected);
        if known.is_error() {
            return self.pat(PatKind::Wild, expected, span);
        }
        if let Some(enum_id) = self.enum_of(&known)
            && let Some(v) = self.variant_index(enum_id, &id.name)
        {
            let payload = self.ck.types[enum_id.0 as usize].variants()[v as usize]
                .fields
                .len();
            if payload > 0 {
                let tname = self.ck.types[enum_id.0 as usize].name.clone();
                let blanks = vec!["_"; payload].join(", ");
                self.ck.report(
                    Diagnostic::error(
                        format!(
                            "the variant `{}` of `{tname}` carries {payload} value{}",
                            id.name,
                            if payload == 1 { "" } else { "s" }
                        ),
                        span,
                    )
                    .with_hint(format!(
                        "write `{}({blanks})` to match it and ignore the data",
                        id.name
                    )),
                );
                return self.pat(PatKind::Wild, expected, span);
            }
            return self.pat(
                PatKind::Variant {
                    ty: enum_id,
                    variant: v,
                    args: Vec::new(),
                },
                expected,
                span,
            );
        }
        match self.ck.values.get(&id.name).copied() {
            Some(ValueRef::Global(g)) if self.ck.globals[g.0 as usize].kind == BindKind::Const => {
                self.const_pat(g, expected, span)
            }
            Some(ValueRef::Global(_)) => {
                self.ck.report(
                    Diagnostic::error(format!("`{}` is not a constant", id.name), span)
                        .with_hint("only `const` values can be used as patterns; to bind a new name, use lower case"),
                );
                self.pat(PatKind::Wild, expected, span)
            }
            _ => {
                self.report_unknown_pattern_name(id, &known, span);
                self.pat(PatKind::Wild, expected, span)
            }
        }
    }

    fn report_unknown_pattern_name(&mut self, id: &ast::Ident, known: &Ty, span: Span) {
        let mut candidates: Vec<String> = Vec::new();
        let message = match self.enum_of(known) {
            Some(e) => {
                let def = &self.ck.types[e.0 as usize];
                candidates.extend(def.variants().iter().map(|v| v.name.clone()));
                format!("the enum `{}` has no variant `{}`", def.name, id.name)
            }
            None => format!("`{}` is not a variant or a constant", id.name),
        };
        candidates.extend(
            self.ck
                .values
                .iter()
                .filter(|(_, v)| {
                    matches!(v, ValueRef::Global(g) if self.ck.globals[g.0 as usize].kind == BindKind::Const)
                })
                .map(|(n, _)| n.clone()),
        );
        let mut diag = Diagnostic::error(message, span);
        diag = diag.with_hint(
            Checker::did_you_mean(&id.name, &candidates).unwrap_or_else(|| {
                "a capitalised name in a pattern must be a variant or a constant; use lower case to bind a value".to_string()
            }),
        );
        self.ck.report(diag);
    }

    fn const_pat(&mut self, g: GlobalId, expected: &Ty, span: Span) -> Pat {
        let ty = self.ck.global_ty(g);
        self.add_dep(Dep::Global(g));
        let shown = self.show(&ty);
        if self.infer.unify(&ty, expected).is_err() {
            self.report_pat_mismatch(expected, &ty, span);
        } else if matches!(self.infer.shallow(&ty), Ty::Float) {
            let name = self.ck.globals[g.0 as usize].name.clone();
            self.ck.report(
                Diagnostic::error(
                    format!(
                        "the constant `{name}` is a `{shown}`, which cannot be used as a pattern"
                    ),
                    span,
                )
                .with_hint("floats are not matched exactly; use a guard such as `x if x == LIMIT`"),
            );
        }
        self.pat(PatKind::Const(g), expected, span)
    }

    fn report_pat_mismatch(&mut self, expected: &Ty, found: &Ty, span: Span) {
        let (e, f) = (self.show(expected), self.show(found));
        self.ck.report(Diagnostic::error(
            format!("this pattern is a `{f}`, but the value is a `{e}`"),
            span,
        ));
    }

    // ---- literals and ranges ----

    fn lit_pat(&mut self, lit: &ast::PatLit, expected: &Ty, span: Span) -> Pat {
        let Some((value, ty)) = self.pat_lit(lit) else {
            return self.pat(PatKind::Wild, expected, span);
        };
        if self.infer.unify(expected, &ty).is_err() {
            self.report_pat_mismatch(expected, &ty, span);
        } else if matches!(value, Lit::Int(_)) && self.infer.shallow(expected) == Ty::Float {
            self.ck.report(float_pattern(span));
        }
        self.pat(PatKind::Lit(value), expected, span)
    }

    /// The value and type of a literal pattern; `None` after an error.
    fn pat_lit(&mut self, lit: &ast::PatLit) -> Option<(Lit, Ty)> {
        Some(match &lit.lit {
            AstLit::Int(n) => {
                let value = if lit.neg {
                    i64::try_from(-(*n as i128)).ok()
                } else {
                    i64::try_from(*n).ok()
                };
                let Some(value) = value else {
                    self.ck.report(
                        Diagnostic::error("this integer literal does not fit in `int`", lit.span)
                            .with_hint("`int` is a 64-bit signed integer"),
                    );
                    return None;
                };
                (
                    Lit::Int(value),
                    self.fresh_var(crate::infer::VarKind::IntLit),
                )
            }
            AstLit::Float(_) => {
                self.ck.report(float_pattern(lit.span));
                return None;
            }
            AstLit::Char(c) => (Lit::Char(*c), Ty::Char),
            AstLit::Bool(b) => (Lit::Bool(*b), Ty::Bool),
            AstLit::Str(s) => (Lit::Str(s.as_str().into()), Ty::Str),
        })
    }

    fn range_pat(
        &mut self,
        lo: Option<&ast::PatLit>,
        hi: Option<&ast::PatLit>,
        inclusive: bool,
        expected: &Ty,
        span: Span,
    ) -> Pat {
        let wild = |this: &Self| this.pat(PatKind::Wild, expected, span);
        let lo_v = match lo {
            Some(l) => match self.pat_lit(l) {
                Some(v) => Some(v),
                None => return wild(self),
            },
            None => None,
        };
        let hi_v = match hi {
            Some(h) => match self.pat_lit(h) {
                Some(v) => Some(v),
                None => return wild(self),
            },
            None => None,
        };
        let ends: Vec<&(Lit, Ty)> = lo_v.iter().chain(hi_v.iter()).collect();
        let Some(first) = ends.first() else {
            return wild(self);
        };
        let range_ty = first.1.clone();
        if !matches!(first.0, Lit::Int(_) | Lit::Char(_)) {
            self.ck.report(
                Diagnostic::error("only integer and `char` ranges can be patterns", span)
                    .with_hint("use a guard for other comparisons"),
            );
            return wild(self);
        }
        for end in &ends {
            if self.infer.unify(&range_ty, &end.1).is_err() {
                self.ck.report(Diagnostic::error(
                    "the two ends of a range pattern must have the same type",
                    span,
                ));
                return wild(self);
            }
        }
        if self.infer.unify(expected, &range_ty).is_err() {
            self.report_pat_mismatch(expected, &range_ty, span);
        } else if matches!(first.0, Lit::Int(_)) && self.infer.shallow(expected) == Ty::Float {
            self.ck.report(float_pattern(span));
        }
        if let (Some((Lit::Int(a), _)), Some((Lit::Int(b), _))) = (&lo_v, &hi_v)
            && (a > b || (!inclusive && a == b))
        {
            self.ck.report(
                Diagnostic::error("this range pattern is empty", span)
                    .with_hint("the start must be below the end (`a..b`) or equal to it (`a..=b`)"),
            );
        }
        if let (Some((Lit::Char(a), _)), Some((Lit::Char(b), _))) = (&lo_v, &hi_v)
            && (a > b || (!inclusive && a == b))
        {
            self.ck
                .report(Diagnostic::error("this range pattern is empty", span));
        }
        self.pat(
            PatKind::Range {
                lo: lo_v.map(|v| v.0),
                hi: hi_v.map(|v| v.0),
                inclusive,
            },
            expected,
            span,
        )
    }

    // ---- tuples and structs ----

    fn tuple_pat(&mut self, items: &[ast::Pat], expected: &Ty, cx: &mut PatCx, span: Span) -> Pat {
        let known = self.infer.shallow(expected);
        let elems: Vec<Ty> = match &known {
            Ty::Unit if items.is_empty() => Vec::new(),
            Ty::Tuple(tys) if tys.len() == items.len() => tys.clone(),
            Ty::Infer(_) => {
                let tys: Vec<Ty> = items
                    .iter()
                    .map(|_| self.fresh_var(crate::infer::VarKind::General))
                    .collect();
                let _ = self.infer.unify(&known, &Ty::Tuple(tys.clone()));
                tys
            }
            Ty::Error => vec![Ty::Error; items.len()],
            other => {
                let shown = self.show(other);
                let message = match other {
                    Ty::Tuple(tys) => format!(
                        "this pattern has {} element{}, but the tuple has {}",
                        items.len(),
                        if items.len() == 1 { "" } else { "s" },
                        tys.len()
                    ),
                    _ => format!("this is a tuple pattern, but the value is a `{shown}`"),
                };
                self.ck.report(Diagnostic::error(message, span));
                vec![Ty::Error; items.len()]
            }
        };
        let subs = items
            .iter()
            .zip(&elems)
            .map(|(item, ty)| self.check_pat(item, ty, cx))
            .collect();
        self.pat(PatKind::Tuple(subs), expected, span)
    }

    fn struct_pat(
        &mut self,
        path: &ast::PatPath,
        fields: &[ast::FieldPat],
        rest: bool,
        expected: &Ty,
        cx: &mut PatCx,
        span: Span,
    ) -> Pat {
        let wild_fields = |this: &mut Self, cx: &mut PatCx| {
            for f in fields {
                this.check_pat(&f.pat, &Ty::Error, cx);
            }
        };
        let name = match path.segments.as_slice() {
            [one] if !path.leading_dot => one,
            _ => {
                wild_fields(self, cx);
                self.ck.report(
                    Diagnostic::error("a struct pattern needs the name of a struct", span)
                        .with_hint("write `Name { field, .. }`"),
                );
                return self.pat(PatKind::Wild, expected, span);
            }
        };
        let id = match self.ck.type_by_name(&name.name) {
            Some(Ty::Adt(id, _)) => id,
            Some(Ty::Error) => {
                wild_fields(self, cx);
                return self.pat(PatKind::Wild, expected, span);
            }
            _ => {
                wild_fields(self, cx);
                let mut diag =
                    Diagnostic::error(format!("`{}` is not a struct", name.name), name.span);
                if let Some(e) = self.enum_of(&self.infer.shallow(expected))
                    && self.variant_index(e, &name.name).is_some()
                {
                    diag = diag.with_hint(format!(
                        "`{0}` is an enum variant; match it with `{0}(...)`",
                        name.name
                    ));
                }
                self.ck.report(diag);
                return self.pat(PatKind::Wild, expected, span);
            }
        };
        if self.ck.types[id.0 as usize].is_enum() {
            wild_fields(self, cx);
            let tname = self.ck.types[id.0 as usize].name.clone();
            self.ck.report(
                Diagnostic::error(format!("`{tname}` is an enum, not a struct"), name.span)
                    .with_hint(format!("match its variants: `{tname}.Variant(...)`")),
            );
            return self.pat(PatKind::Wild, expected, span);
        }
        let struct_ty = Ty::Adt(id, Vec::new());
        if self.infer.unify(expected, &struct_ty).is_err() {
            self.report_pat_mismatch(expected, &struct_ty, span);
        }
        let def = &self.ck.types[id.0 as usize];
        let decls = def.fields().to_vec();
        let tname = def.name.clone();
        let mut subs: Vec<Option<Pat>> = decls.iter().map(|_| None).collect();
        for f in fields {
            let Some(i) = decls.iter().position(|d| d.name == f.name.name) else {
                self.check_pat(&f.pat, &Ty::Error, cx);
                let names: Vec<String> = decls.iter().map(|d| d.name.clone()).collect();
                let mut diag = Diagnostic::error(
                    format!("the struct `{tname}` has no field `{}`", f.name.name),
                    f.name.span,
                );
                if let Some(hint) = Checker::did_you_mean(&f.name.name, &names) {
                    diag = diag.with_hint(hint);
                }
                self.ck.report(diag);
                continue;
            };
            if subs[i].is_some() {
                self.ck.report(Diagnostic::error(
                    format!("the field `{}` appears twice in this pattern", f.name.name),
                    f.name.span,
                ));
                self.check_pat(&f.pat, &Ty::Error, cx);
                continue;
            }
            subs[i] = Some(self.check_pat(&f.pat, &decls[i].ty, cx));
        }
        let missing: Vec<&str> = decls
            .iter()
            .zip(&subs)
            .filter(|(_, s)| s.is_none())
            .map(|(d, _)| d.name.as_str())
            .collect();
        if !rest && !missing.is_empty() {
            let list: Vec<String> = missing.iter().map(|m| format!("`{m}`")).collect();
            self.ck.report(
                Diagnostic::error(
                    format!("this pattern does not mention {}", list.join(", ")),
                    span,
                )
                .with_hint(format!(
                    "add `{}: _`, or `..` to ignore the other fields",
                    missing[0]
                )),
            );
        }
        let fields = subs
            .into_iter()
            .zip(&decls)
            .map(|(s, d)| s.unwrap_or_else(|| self.pat(PatKind::Wild, &d.ty, span)))
            .collect();
        self.pat(PatKind::Struct { ty: id, fields }, expected, span)
    }

    // ---- variants ----

    fn enum_of(&self, ty: &Ty) -> Option<TypeId> {
        match ty {
            Ty::Adt(id, _) if self.ck.types[id.0 as usize].is_enum() => Some(*id),
            _ => None,
        }
    }

    fn variant_index(&self, ty: TypeId, name: &str) -> Option<u32> {
        self.ck.types[ty.0 as usize]
            .variants()
            .iter()
            .position(|v| v.name == name)
            .map(|i| i as u32)
    }

    fn variant_pat(
        &mut self,
        path: &ast::PatPath,
        args: Option<&[ast::Pat]>,
        expected: &Ty,
        cx: &mut PatCx,
        span: Span,
    ) -> Pat {
        let fallback = |this: &mut Self, cx: &mut PatCx| {
            for a in args.unwrap_or_default() {
                this.check_pat(a, &Ty::Error, cx);
            }
            this.pat(PatKind::Wild, expected, span)
        };
        let known = self.infer.shallow(expected);
        let (enum_id, name) = match (path.segments.as_slice(), path.leading_dot) {
            ([name], _) => {
                // `Circle(r)` and `.Circle(r)`: the value's type says which enum.
                if known.is_error() {
                    return fallback(self, cx);
                }
                match self.enum_of(&known) {
                    Some(e) => (e, name),
                    None => {
                        let mut diag = if matches!(known, Ty::Infer(_)) {
                            Diagnostic::error(
                                format!("cannot tell which enum `{}` belongs to", name.name),
                                span,
                            )
                            .with_hint("write the enum's name, like `Type.Variant`")
                        } else {
                            let shown = self.show(&known);
                            Diagnostic::error(
                                format!(
                                    "this is an enum variant pattern, but the value is a `{shown}`"
                                ),
                                span,
                            )
                        };
                        if let Some(owner) = self.single_enum_with(&name.name) {
                            diag = diag.with_hint(format!("write `{owner}.{}`", name.name));
                        }
                        self.ck.report(diag);
                        return fallback(self, cx);
                    }
                }
            }
            ([ty_name, name], false) => {
                let Some(Ty::Adt(e, _)) = self.ck.type_by_name(&ty_name.name) else {
                    let known_type = matches!(self.ck.type_by_name(&ty_name.name), Some(Ty::Error));
                    if !known_type {
                        self.ck.report(Diagnostic::error(
                            format!("`{}` is not an enum", ty_name.name),
                            ty_name.span,
                        ));
                    }
                    return fallback(self, cx);
                };
                if !self.ck.types[e.0 as usize].is_enum() {
                    self.ck.report(Diagnostic::error(
                        format!("`{}` is a struct, not an enum", ty_name.name),
                        ty_name.span,
                    ));
                    return fallback(self, cx);
                }
                let enum_ty = Ty::Adt(e, Vec::new());
                if self.infer.unify(expected, &enum_ty).is_err() {
                    self.report_pat_mismatch(expected, &enum_ty, span);
                    return fallback(self, cx);
                }
                (e, name)
            }
            _ => {
                self.ck.unsupported("a qualified pattern path", span);
                return fallback(self, cx);
            }
        };
        let def = &self.ck.types[enum_id.0 as usize];
        let tname = def.name.clone();
        let Some(v) = self.variant_index(enum_id, &name.name) else {
            let names: Vec<String> = def.variants().iter().map(|v| v.name.clone()).collect();
            let mut diag = Diagnostic::error(
                format!("the enum `{tname}` has no variant `{}`", name.name),
                name.span,
            );
            if let Some(hint) = Checker::did_you_mean(&name.name, &names) {
                diag = diag.with_hint(hint);
            }
            self.ck.report(diag);
            return fallback(self, cx);
        };
        let variant = def.variants()[v as usize].clone();
        let expected_args = variant.fields.len();
        let given = args.map_or(0, <[ast::Pat]>::len);
        if given != expected_args {
            let plural = if expected_args == 1 { "" } else { "s" };
            let message = if expected_args == 0 {
                format!("`{tname}.{}` carries no data", variant.name)
            } else {
                format!(
                    "`{tname}.{}` carries {expected_args} value{plural}, but the pattern has {given}",
                    variant.name
                )
            };
            let hint = if expected_args == 0 {
                format!("write `{}` without parentheses", variant.name)
            } else {
                format!(
                    "write `{}({})`",
                    variant.name,
                    vec!["_"; expected_args].join(", ")
                )
            };
            self.ck
                .report(Diagnostic::error(message, span).with_hint(hint));
            return fallback(self, cx);
        }
        let subs = args
            .unwrap_or_default()
            .iter()
            .zip(&variant.fields)
            .map(|(a, f)| self.check_pat(a, &f.ty, cx))
            .collect();
        self.pat(
            PatKind::Variant {
                ty: enum_id,
                variant: v,
                args: subs,
            },
            expected,
            span,
        )
    }

    fn single_enum_with(&self, variant: &str) -> Option<String> {
        let mut owners = self
            .ck
            .types
            .iter()
            .filter(|t| t.variants().iter().any(|v| v.name == variant));
        let first = owners.next()?;
        owners.next().is_none().then(|| first.name.clone())
    }
}

fn float_pattern(span: Span) -> Diagnostic {
    Diagnostic::error("a float cannot be used as a pattern", span)
        .with_hint("floats are not matched exactly; use a guard, for example `x if x > 0.5`")
}
