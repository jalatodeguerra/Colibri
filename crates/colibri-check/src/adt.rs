//! Checking structs, enums and tuples: literals, variant construction (`Shape.Circle(1)`,
//! `.Circle(1)`), field and tuple-element access, and `Type.name` paths.

use colibri_syntax::ast::{self, ExprKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::call::Bound;
use crate::checker::ParamInfo;
use crate::hir::{self, Expr, FieldDef, FieldIdx, TypeDefKind, ValueProj};
use crate::types::{Ty, TypeId};

impl FnCtx<'_, '_> {
    // ---- tuples ----

    /// `(a, b)`: the expected tuple type, if there is one, types the elements.
    pub fn check_tuple(&mut self, items: &[ast::Expr], expected: Option<&Ty>, span: Span) -> Expr {
        let hints: Vec<Option<Ty>> = match expected.map(|t| self.infer.shallow(t)) {
            Some(Ty::Tuple(tys)) if tys.len() == items.len() => tys.into_iter().map(Some).collect(),
            _ => vec![None; items.len()],
        };
        let values: Vec<Expr> = items
            .iter()
            .zip(&hints)
            .map(|(item, hint)| self.check_expr(item, hint.as_ref()))
            .collect();
        let ty = Ty::Tuple(values.iter().map(|v| v.ty.clone()).collect());
        self.mk(hir::ExprKind::Tuple(values), ty, span)
    }

    // ---- fields and tuple elements ----

    /// The index and type of the field `name` of a struct value of type `base`, reporting why
    /// not when there is none. `None` also for [`Ty::Error`], which was reported already.
    pub fn field_of(
        &mut self,
        base: &Ty,
        name: &ast::Ident,
        base_span: Span,
    ) -> Option<(FieldIdx, Ty)> {
        let ty = self.infer.shallow(base);
        match &ty {
            Ty::Error => None,
            Ty::Adt(id, _) => {
                let def = &self.ck.types[id.0 as usize];
                match &def.kind {
                    TypeDefKind::Struct { fields } => {
                        if let Some(i) = fields.iter().position(|f| f.name == name.name) {
                            return Some((FieldIdx(i as u32), fields[i].ty.clone()));
                        }
                        let names: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
                        let mut diag = Diagnostic::error(
                            format!("the struct `{}` has no field `{}`", def.name, name.name),
                            name.span,
                        );
                        diag = diag.with_hint(
                            crate::checker::Checker::did_you_mean(&name.name, &names)
                                .unwrap_or_else(|| {
                                    if names.is_empty() {
                                        "it has no fields".to_string()
                                    } else {
                                        format!("its fields are: {}", names.join(", "))
                                    }
                                }),
                        );
                        diag =
                            diag.with_label(def.span, format!("`{}` is declared here", def.name));
                        self.ck.report(diag);
                        None
                    }
                    TypeDefKind::Enum { .. } => {
                        let diag = Diagnostic::error(
                            format!("the enum `{}` has no fields", def.name),
                            name.span,
                        )
                        .with_hint("use `match` to look at the data inside a variant");
                        self.ck.report(diag);
                        None
                    }
                }
            }
            Ty::Tuple(_) => {
                self.ck.report(
                    Diagnostic::error(format!("a tuple has no field `{}`", name.name), name.span)
                        .with_hint("tuple elements are numbered: `t.0`, `t.1`"),
                );
                None
            }
            other => {
                let shown = self.show(other);
                self.ck.report(
                    Diagnostic::error(format!("`{shown}` has no field `{}`", name.name), name.span)
                        .with_label(base_span, format!("this is `{shown}`")),
                );
                None
            }
        }
    }

    /// The type of element `index` of a tuple value of type `base`.
    pub fn tuple_elem_of(&mut self, base: &Ty, index: u32, span: Span) -> Option<Ty> {
        let ty = self.infer.shallow(base);
        match &ty {
            Ty::Error => None,
            Ty::Tuple(items) => match items.get(index as usize) {
                Some(t) => Some(t.clone()),
                None => {
                    let n = items.len();
                    let plural = if n == 1 { "" } else { "s" };
                    self.ck.report(Diagnostic::error(
                        format!("this tuple has {n} element{plural}, so there is no `.{index}`"),
                        span,
                    ));
                    None
                }
            },
            other => {
                let shown = self.show(other);
                self.ck.report(Diagnostic::error(
                    format!("`{shown}` is not a tuple, so it has no `.{index}`"),
                    span,
                ));
                None
            }
        }
    }

    /// `base.name` used as a value.
    pub fn check_field_expr(&mut self, base: &ast::Expr, name: &ast::Ident, span: Span) -> Expr {
        if let ExprKind::Ident(id) = &base.kind
            && let Some(ty) = self.type_path(id)
        {
            return self.check_type_member(ty, name, None, span);
        }
        let value = self.check_expr(base, None);
        let Some((idx, ty)) = self.field_of(&value.ty, name, base.span) else {
            return self.error_expr(span);
        };
        self.project(value, ValueProj::Field(idx), ty, &name.name, span)
    }

    /// `base.0` used as a value.
    pub fn check_tuple_index_expr(&mut self, base: &ast::Expr, index: u32, span: Span) -> Expr {
        let value = self.check_expr(base, None);
        let Some(ty) = self.tuple_elem_of(&value.ty, index, span) else {
            return self.error_expr(span);
        };
        self.project(
            value,
            ValueProj::TupleIdx(index),
            ty,
            &index.to_string(),
            span,
        )
    }

    /// Selects a part of `value`: a longer path when it is a place, a projection otherwise.
    fn project(&mut self, value: Expr, proj: ValueProj, ty: Ty, name: &str, span: Span) -> Expr {
        match value.kind {
            hir::ExprKind::Read(mut place) => {
                place.path.push(proj);
                place.ty = ty.clone();
                place.display = format!("{}.{name}", place.display);
                self.mk(hir::ExprKind::Read(place), ty, span)
            }
            kind => {
                let base = Expr {
                    kind,
                    ty: value.ty,
                    span: value.span,
                };
                self.mk(hir::ExprKind::Project(Box::new(base), proj), ty, span)
            }
        }
    }

    /// `self`, as a value.
    pub fn check_self(&mut self, span: Span) -> Expr {
        match self.lookup_local("self") {
            Some(local) => {
                let place = self.local_place(local, "self");
                let ty = place.ty.clone();
                self.mk(hir::ExprKind::Read(Box::new(place)), ty, span)
            }
            None => {
                self.report_self_outside_method(span);
                self.error_expr(span)
            }
        }
    }

    // ---- type paths ----

    /// The type a bare name refers to when it is used as the start of `Type.member`: only if no
    /// variable or function has that name.
    pub fn type_path(&mut self, id: &ast::Ident) -> Option<TypeId> {
        let name = id.name.as_str();
        if self.lookup_local(name).is_some() || self.ck.values.contains_key(name) {
            return None;
        }
        match self.ck.type_by_name(name)? {
            Ty::Adt(id, _) => Some(id),
            _ => None,
        }
    }

    /// `Type.name` or `Type.name(args)`: an enum variant or a static function.
    pub fn check_type_member(
        &mut self,
        ty: TypeId,
        name: &ast::Ident,
        args: Option<&[ast::Arg]>,
        span: Span,
    ) -> Expr {
        let def = &self.ck.types[ty.0 as usize];
        if let Some(v) = def.variants().iter().position(|v| v.name == name.name) {
            return self.construct_variant(ty, v as u32, args, span);
        }
        if let Some(&f) = self.ck.methods.get(&(ty, name.name.clone())) {
            let info = &self.ck.fns[f.0 as usize];
            if info.receiver.is_some() {
                let (shown, tname) = (name.name.clone(), def.name.clone());
                self.check_args_loosely_opt(args);
                self.ck.report(
                    Diagnostic::error(
                        format!(
                            "`{shown}` is a method of `{tname}`, so it needs a value to act on"
                        ),
                        name.span,
                    )
                    .with_hint(format!("call it on a value: `value.{shown}(...)`")),
                );
                return self.error_expr(span);
            }
            return match args {
                Some(args) => self.check_fn_call(f, args, span),
                None => {
                    let shown = name.name.clone();
                    self.ck.report(
                        Diagnostic::error(
                            format!("`{shown}` is a function, not a value"),
                            name.span,
                        )
                        .with_hint(format!("call it: `{}.{shown}(...)`", def.name)),
                    );
                    self.error_expr(span)
                }
            };
        }
        let tname = def.name.clone();
        let candidates = self.ck.member_names(ty);
        self.check_args_loosely_opt(args);
        let mut diag = Diagnostic::error(
            format!("`{tname}` has no function or variant `{}`", name.name),
            name.span,
        );
        if let Some(hint) = crate::checker::Checker::did_you_mean(&name.name, &candidates) {
            diag = diag.with_hint(hint);
        }
        self.ck.report(diag);
        self.error_expr(span)
    }

    fn check_args_loosely_opt(&mut self, args: Option<&[ast::Arg]>) {
        for a in args.unwrap_or_default() {
            self.check_expr(&a.value, None);
        }
    }

    // ---- enum variants ----

    /// `.Variant` or `.Variant(args)`: the expected type says which enum.
    pub fn check_dot_variant(
        &mut self,
        name: &ast::Ident,
        args: Option<&[ast::Arg]>,
        expected: Option<&Ty>,
        span: Span,
    ) -> Expr {
        let known = expected.map(|t| self.infer.shallow(t));
        match known {
            Some(Ty::Adt(id, _)) if self.ck.types[id.0 as usize].is_enum() => {
                let def = &self.ck.types[id.0 as usize];
                match def.variants().iter().position(|v| v.name == name.name) {
                    Some(v) => self.construct_variant(id, v as u32, args, span),
                    None => {
                        let tname = def.name.clone();
                        let names: Vec<String> =
                            def.variants().iter().map(|v| v.name.clone()).collect();
                        self.check_args_loosely_opt(args);
                        let mut diag = Diagnostic::error(
                            format!("the enum `{tname}` has no variant `{}`", name.name),
                            name.span,
                        );
                        if let Some(hint) =
                            crate::checker::Checker::did_you_mean(&name.name, &names)
                        {
                            diag = diag.with_hint(hint);
                        }
                        self.ck.report(diag);
                        self.error_expr(span)
                    }
                }
            }
            Some(Ty::Error) => {
                self.check_args_loosely_opt(args);
                self.error_expr(span)
            }
            other => {
                self.check_args_loosely_opt(args);
                let owners = self.enums_with_variant(&name.name);
                let mut diag = match &other {
                    Some(t) if !matches!(t, Ty::Infer(_)) => {
                        let shown = self.show(t);
                        Diagnostic::error(
                            format!(
                                "`.{}` builds an enum variant, but `{shown}` is expected here",
                                name.name
                            ),
                            span,
                        )
                    }
                    _ => Diagnostic::error(
                        format!("cannot tell which enum `.{}` belongs to", name.name),
                        span,
                    ),
                };
                if other.is_none() || matches!(other, Some(Ty::Infer(_))) {
                    diag = diag.with_hint(match owners.as_slice() {
                        [one] => format!(
                            "write `{one}.{}`, or give the value a type with `let x: {one} = ...`",
                            name.name
                        ),
                        _ => format!(
                            "write the enum's name, like `Type.{}`, or give the value a type with `let x: Type = ...`",
                            name.name
                        ),
                    });
                }
                self.ck.report(diag);
                self.error_expr(span)
            }
        }
    }

    fn enums_with_variant(&self, variant: &str) -> Vec<String> {
        self.ck
            .types
            .iter()
            .filter(|t| t.variants().iter().any(|v| v.name == variant))
            .map(|t| t.name.clone())
            .collect()
    }

    /// Builds `Type.Variant(args)` (or the unit variant when `args` is `None`).
    pub fn construct_variant(
        &mut self,
        ty: TypeId,
        variant: u32,
        args: Option<&[ast::Arg]>,
        span: Span,
    ) -> Expr {
        let def = &self.ck.types[ty.0 as usize];
        let tname = def.name.clone();
        let v = def.variants()[variant as usize].clone();
        let result = Ty::Adt(ty, Vec::new());
        let full = format!("{tname}.{}", v.name);
        let make = |this: &Self, args: Vec<Expr>| {
            this.mk(
                hir::ExprKind::Variant { ty, variant, args },
                result.clone(),
                span,
            )
        };
        let fail = |this: &Self| this.mk(hir::ExprKind::Error, result.clone(), span);
        let Some(args) = args else {
            if v.fields.is_empty() {
                return make(self, Vec::new());
            }
            self.ck.report(
                Diagnostic::error(format!("`{full}` carries data"), span).with_hint(format!(
                    "write `{full}({})`",
                    self.payload_shape(&v.fields, v.named)
                )),
            );
            return fail(self);
        };
        if v.fields.is_empty() {
            self.check_args_loosely_opt(Some(args));
            self.ck.report(
                Diagnostic::error(format!("`{full}` carries no data"), span)
                    .with_hint(format!("write `{full}` without parentheses")),
            );
            return fail(self);
        }
        if v.named {
            let params: Vec<ParamInfo<'_>> = v
                .fields
                .iter()
                .map(|f| ParamInfo {
                    name: f.name.clone(),
                    ty: f.ty.clone(),
                    span: f.span,
                    default: None,
                    mutable: false,
                })
                .collect();
            let Some(bound) = self.bind_args(&full, &params, args, span, 0) else {
                return fail(self);
            };
            let (values, lets, temps) = self.order_args(bound);
            let call = make(self, values);
            return self.wrap_hoisted(call, lets, temps, span);
        }
        if let Some(named) = args.iter().find_map(|a| a.name.as_ref()) {
            self.check_args_loosely_opt(Some(args));
            self.ck.report(
                Diagnostic::error(
                    format!("`{full}` has no field named `{}`", named.name),
                    named.span,
                )
                .with_hint(format!(
                    "its payload is positional: `{full}({})`",
                    self.payload_shape(&v.fields, false)
                )),
            );
            return fail(self);
        }
        if args.len() != v.fields.len() {
            self.check_args_loosely_opt(Some(args));
            let plural = if v.fields.len() == 1 { "" } else { "s" };
            self.ck.report(
                Diagnostic::error(
                    format!(
                        "`{full}` takes {} value{plural}, found {}",
                        v.fields.len(),
                        args.len()
                    ),
                    span,
                )
                .with_hint(format!(
                    "write `{full}({})`",
                    self.payload_shape(&v.fields, false)
                )),
            );
            return fail(self);
        }
        let mut values = Vec::new();
        let mut ok = true;
        for (arg, field) in args.iter().zip(&v.fields) {
            let value = self.check_expr(&arg.value, Some(&field.ty));
            let reason = Some((
                field.span,
                format!("`{full}` carries a `{}` here", self.show(&field.ty)),
            ));
            ok &= self.expect(&value, &field.ty, reason);
            values.push(value);
        }
        if ok { make(self, values) } else { fail(self) }
    }

    /// `float, float` or `radius: float`: what a variant's payload looks like, for hints.
    fn payload_shape(&self, fields: &[FieldDef], named: bool) -> String {
        fields
            .iter()
            .map(|f| match named {
                true => format!("{}: {}", f.name, self.show(&f.ty)),
                false => self.show(&f.ty),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    // ---- struct literals ----

    /// `Player { name: "Ana", hp }`: fields may come in any order, omitted ones take their
    /// declared default.
    pub fn check_struct_lit(
        &mut self,
        ty_expr: &ast::TypeExpr,
        fields: &[ast::FieldInit],
        span: Span,
    ) -> Expr {
        let ty = self.ck.resolve_type(ty_expr);
        let loose = |this: &mut Self| {
            for f in fields {
                this.check_expr(&f.value, None);
            }
        };
        let id = match &ty {
            Ty::Error => {
                loose(self);
                return self.error_expr(span);
            }
            Ty::Adt(id, _) if !self.ck.types[id.0 as usize].is_enum() => *id,
            Ty::Adt(id, _) => {
                loose(self);
                let tname = self.ck.types[id.0 as usize].name.clone();
                self.ck.report(
                    Diagnostic::error(format!("`{tname}` is an enum, not a struct"), ty_expr.span)
                        .with_hint(format!("build a variant instead: `{tname}.Variant(...)`")),
                );
                return self.error_expr(span);
            }
            other => {
                loose(self);
                let shown = self.show(other);
                self.ck.report(Diagnostic::error(
                    format!("`{shown}` is not a struct, so it cannot be built with `{{ ... }}`"),
                    ty_expr.span,
                ));
                return self.error_expr(span);
            }
        };
        let def = &self.ck.types[id.0 as usize];
        let tname = def.name.clone();
        let decls: Vec<FieldDef> = def.fields().to_vec();
        let defaults = self.ck.type_extra[id.0 as usize].defaults.clone();
        let mut given: Vec<Option<Bound>> = decls.iter().map(|_| None).collect();
        let mut ok = true;
        for (source, init) in fields.iter().enumerate() {
            let Some(i) = decls.iter().position(|f| f.name == init.name.name) else {
                self.check_expr(&init.value, None);
                let names: Vec<String> = decls.iter().map(|f| f.name.clone()).collect();
                let mut diag = Diagnostic::error(
                    format!("the struct `{tname}` has no field `{}`", init.name.name),
                    init.name.span,
                );
                diag = diag.with_hint(
                    crate::checker::Checker::did_you_mean(&init.name.name, &names)
                        .unwrap_or_else(|| format!("its fields are: {}", names.join(", "))),
                );
                self.ck.report(diag);
                ok = false;
                continue;
            };
            let field = &decls[i];
            let value = self.check_expr(&init.value, Some(&field.ty));
            let reason = Some((
                field.span,
                format!(
                    "the field `{}` is declared `{}` here",
                    field.name,
                    self.show(&field.ty)
                ),
            ));
            ok &= self.expect(&value, &field.ty, reason);
            if given[i].is_some() {
                self.ck.report(Diagnostic::error(
                    format!("the field `{}` is given more than once", field.name),
                    init.name.span,
                ));
                ok = false;
            } else {
                given[i] = Some(Bound {
                    expr: value,
                    source: Some(source),
                });
            }
        }
        let mut bound = Vec::new();
        let mut missing: Vec<&FieldDef> = Vec::new();
        for (i, slot) in given.into_iter().enumerate() {
            match (slot, defaults.get(i).copied().flatten()) {
                (Some(b), _) => bound.push(b),
                (None, Some(default)) => {
                    let expr = self.check_default_at_call(default, &decls[i].ty, span);
                    bound.push(Bound { expr, source: None });
                }
                (None, None) => missing.push(&decls[i]),
            }
        }
        if !missing.is_empty() && ok {
            let names: Vec<String> = missing.iter().map(|f| format!("`{}`", f.name)).collect();
            let plural = if missing.len() == 1 { "" } else { "s" };
            let mut diag = Diagnostic::error(
                format!(
                    "missing field{plural} {} in the `{tname}` literal",
                    names.join(", ")
                ),
                span,
            );
            for f in &missing {
                diag = diag.with_label(
                    f.span,
                    format!("`{}` is declared here without a default", f.name),
                );
            }
            self.ck.report(diag);
            ok = false;
        }
        let result = Ty::Adt(id, Vec::new());
        if !ok {
            return self.mk(hir::ExprKind::Error, result, span);
        }
        let (values, lets, temps) = self.order_args(bound);
        let literal = self.mk(
            hir::ExprKind::Struct {
                ty: id,
                fields: values,
            },
            result,
            span,
        );
        self.wrap_hoisted(literal, lets, temps, span)
    }
}
