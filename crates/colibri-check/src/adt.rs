//! Checking structs, enums and tuples: literals, variant construction (`Shape.Circle(1)`,
//! `.Circle(1)`), field and tuple-element access, and `Type.name` paths.

use colibri_syntax::ast::{self, ExprKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::call::Bound;
use crate::checker::{Checker, ParamInfo};
use crate::hir::{self, Expr, FieldDef, FieldIdx, TypeDef, TypeDefKind, ValueProj};
use crate::infer::VarKind;
use crate::modules::BASE;
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
            Ty::Adt(id, targs) => {
                let def = &self.ck.types[id.0 as usize];
                match &def.kind {
                    TypeDefKind::Struct { fields } | TypeDefKind::Class { fields } => {
                        if let Some(i) = fields.iter().position(|f| f.name == name.name) {
                            let map = Checker::param_map_of(def, targs);
                            let found = (FieldIdx(i as u32), fields[i].ty.subst(&map));
                            let decl = fields[i].span;
                            if !self.ck.field_visible(*id, i) {
                                self.ck.report_private("field", &name.name, *id, name.span);
                                return None;
                            }
                            self.ck.note_ref(name.span, decl);
                            return Some(found);
                        }
                        let names: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
                        let mut diag = Diagnostic::error(
                            format!(
                                "the {} `{}` has no field `{}`",
                                kind_word(def),
                                def.display_name(),
                                name.name
                            ),
                            name.span,
                        );
                        diag = diag.with_hint(
                            Checker::did_you_mean(&name.name, &names).unwrap_or_else(|| {
                                if names.is_empty() {
                                    "it has no fields".to_string()
                                } else {
                                    format!("its fields are: {}", names.join(", "))
                                }
                            }),
                        );
                        diag = diag.with_label(
                            def.span,
                            format!("`{}` is declared here", def.display_name()),
                        );
                        self.ck.report(diag);
                        None
                    }
                    TypeDefKind::HostClass => {
                        let diag = Diagnostic::error(
                            format!("the host class `{}` has no fields", def.display_name()),
                            name.span,
                        )
                        .with_hint(
                            "its state belongs to the host program; use the methods it provides",
                        );
                        self.ck.report(diag);
                        None
                    }
                    TypeDefKind::Interface { .. } => {
                        let diag = Diagnostic::error(
                            format!("the interface `{}` has no fields", def.display_name()),
                            name.span,
                        )
                        .with_hint("call one of its methods instead");
                        self.ck.report(diag);
                        None
                    }
                    TypeDefKind::Enum { .. } => {
                        let diag = Diagnostic::error(
                            format!("the enum `{}` has no fields", def.display_name()),
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
        if let Some(q) = self.qualified_parts(base, name) {
            return self.check_ident(&q, span);
        }
        if let Some((ty, owner_args)) = self.type_path_expr(base) {
            return self.check_type_member(ty, owner_args, name, None, span);
        }
        if let ExprKind::Ident(id) = &base.kind
            && let Some(ty) = self.numeric_type_name(&id.name)
        {
            return self.check_num_const(ty, name, span);
        }
        let value = self.check_expr(base, None);
        self.field_value(value, base, name, span)
    }

    /// `value.name` for a value that is already checked (`base` is its source, for messages).
    pub fn field_value(
        &mut self,
        value: Expr,
        base: &ast::Expr,
        name: &ast::Ident,
        span: Span,
    ) -> Expr {
        if let Ty::Vec(v) = self.infer.shallow(&value.ty) {
            return self.vec_field_value(v, value, name, span);
        }
        let Some((idx, ty)) = self.field_of(&value.ty, name, base.span) else {
            return self.error_expr(span);
        };
        if let Ty::Adt(id, _) = self.infer.shallow(&value.ty)
            && self.ck.types[id.0 as usize].is_class()
        {
            return self.class_field_read(value, id, idx, ty, base, name, span);
        }
        self.project(value, ValueProj::Field(idx), ty, &name.name, span)
    }

    /// `object.field` on a class: a place anchored at the field. A weak field reads as an
    /// `Option` of its class.
    #[allow(clippy::too_many_arguments)]
    fn class_field_read(
        &mut self,
        object: Expr,
        class: TypeId,
        idx: FieldIdx,
        ty: Ty,
        base: &ast::Expr,
        name: &ast::Ident,
        span: Span,
    ) -> Expr {
        let weak = self.ck.types[class.0 as usize].fields()[idx.0 as usize].weak;
        let place = hir::Place {
            anchor: hir::Anchor::ClassField(Box::new(object), idx),
            path: Vec::new(),
            ty: ty.clone(),
            display: format!("{}.{}", crate::place::expr_text(base), name.name),
        };
        let read = self.mk(hir::ExprKind::Read(Box::new(place)), ty.clone(), span);
        if weak {
            let opt = Ty::Option(Box::new(ty));
            return self.mk(hir::ExprKind::FromWeak(Box::new(read)), opt, span);
        }
        read
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

    /// The type an expression names when it starts `Type.member`: `Tree` or `Pair[int, str]`.
    /// The second part holds the type arguments written, if any.
    pub fn type_path_expr(&mut self, e: &ast::Expr) -> Option<(TypeId, Option<Vec<Ty>>)> {
        match &e.kind {
            ExprKind::Ident(id) => self.type_path(id).map(|t| (t, None)),
            // `shapes.Shape`: a type of an imported module.
            ExprKind::Field { .. } => {
                let q = self.qualified(e)?;
                self.type_path(&q).map(|t| (t, None))
            }
            ExprKind::Index { base, args } => {
                let id = match &base.kind {
                    ExprKind::Ident(id) => id.clone(),
                    _ => self.qualified(base)?,
                };
                let ty = self.type_path(&id)?;
                let wanted = self.ck.types[ty.0 as usize].params.len();
                let given = match self.explicit_type_args(args) {
                    Some(g) if g.len() == wanted => g,
                    Some(g) => {
                        let name = id.name.clone();
                        self.ck.report_type_arity(&name, wanted, g.len(), e.span);
                        vec![Ty::Error; wanted]
                    }
                    None => vec![Ty::Error; wanted],
                };
                Some((ty, Some(given)))
            }
            _ => None,
        }
    }

    /// The type arguments of a type named without any: one fresh variable per parameter.
    pub(crate) fn fresh_args(&mut self, ty: TypeId) -> Vec<Ty> {
        let n = self.ck.types[ty.0 as usize].params.len();
        (0..n).map(|_| self.fresh_var(VarKind::General)).collect()
    }

    /// `Type.name` or `Type.name(args)`: an enum variant or a static function.
    pub fn check_type_member(
        &mut self,
        ty: TypeId,
        owner_args: Option<Vec<Ty>>,
        name: &ast::Ident,
        args: Option<&[ast::Arg]>,
        span: Span,
    ) -> Expr {
        if ty == crate::types::ERROR_TYPE && name.name == "from" {
            return self.check_error_from(args, span);
        }
        let owner_args = match owner_args {
            Some(a) => a,
            None => self.fresh_args(ty),
        };
        let def = &self.ck.types[ty.0 as usize];
        if let Some(v) = def.variants().iter().position(|v| v.name == name.name) {
            return self.construct_variant(ty, owner_args, v as u32, args, span);
        }
        if let Some(&f) = self.ck.methods.get(&(ty, name.name.clone())) {
            if !self.ck.fn_visible(f) {
                self.check_args_loosely_opt(args);
                self.ck
                    .report_private("function", &name.name, ty, name.span);
                return self.error_expr(span);
            }
            let info = &self.ck.fns[f.0 as usize];
            if info.receiver.is_some() {
                let (shown, tname) = (name.name.clone(), def.display_name().into_owned());
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
                Some(args) => {
                    let owner = Ty::Adt(ty, owner_args);
                    self.check_fn_call_with(f, None, Some(owner), args, span)
                }
                None => {
                    let owner = Ty::Adt(ty, owner_args);
                    self.fn_value(f, None, Some(owner), span)
                }
            };
        }
        let tname = def.display_name().into_owned();
        let candidates = self.ck.member_names(ty);
        self.check_args_loosely_opt(args);
        let mut diag = Diagnostic::error(
            format!("`{tname}` has no function or variant `{}`", name.name),
            name.span,
        );
        if let Some(hint) = Checker::did_you_mean(&name.name, &candidates) {
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
            Some(Ty::Adt(id, owner_args)) if self.ck.types[id.0 as usize].is_enum() => {
                let def = &self.ck.types[id.0 as usize];
                match def.variants().iter().position(|v| v.name == name.name) {
                    Some(v) => {
                        let decl = def.variants()[v].span;
                        self.ck.note_ref(name.span, decl);
                        self.construct_variant(id, owner_args, v as u32, args, span)
                    }
                    None => {
                        let tname = def.display_name().into_owned();
                        let names: Vec<String> =
                            def.variants().iter().map(|v| v.name.clone()).collect();
                        self.check_args_loosely_opt(args);
                        let mut diag = Diagnostic::error(
                            format!("the enum `{tname}` has no variant `{}`", name.name),
                            name.span,
                        );
                        if let Some(hint) = Checker::did_you_mean(&name.name, &names) {
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
        owner_args: Vec<Ty>,
        variant: u32,
        args: Option<&[ast::Arg]>,
        span: Span,
    ) -> Expr {
        let def = &self.ck.types[ty.0 as usize];
        let tname = def.display_name().into_owned();
        let map = Checker::param_map_of(def, &owner_args);
        let mut v = def.variants()[variant as usize].clone();
        for f in &mut v.fields {
            f.ty = f.ty.subst(&map);
        }
        let result = Ty::Adt(ty, owner_args);
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
                    module: BASE,
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
        expected: Option<&Ty>,
        span: Span,
    ) -> Expr {
        let ty = self.resolve_type_infer(ty_expr);
        if let (Ty::Adt(id, _), Some(Ty::Adt(eid, _))) =
            (&ty, expected.map(|t| self.infer.shallow(t)))
            && *id == eid
        {
            let _ = self.infer.unify(&ty, expected.unwrap_or(&ty));
        }
        let loose = |this: &mut Self| {
            for f in fields {
                this.check_expr(&f.value, None);
            }
        };
        let (id, targs) = match &ty {
            Ty::Error => {
                loose(self);
                return self.error_expr(span);
            }
            Ty::Adt(id, _) if *id == crate::types::ERROR_TYPE => {
                loose(self);
                self.ck.report(
                    Diagnostic::error("`Error` cannot be built with `{ ... }`", ty_expr.span)
                        .with_hint("convert a value that implements `IntoError`: `Error.from(value)`, or return `err(value)` from a function that returns `Result[T]`"),
                );
                return self.error_expr(span);
            }
            Ty::Adt(id, _) if crate::types::prelude_class_maker(*id).is_some() => {
                loose(self);
                let tname = self.ck.types[id.0 as usize].display_name().into_owned();
                let maker = crate::types::prelude_class_maker(*id).unwrap_or_default();
                self.ck.report(
                    Diagnostic::error(
                        format!("`{tname}` cannot be built with `{{ ... }}`"),
                        ty_expr.span,
                    )
                    .with_hint(format!("`{tname}` is built with {maker}")),
                );
                return self.error_expr(span);
            }
            Ty::Adt(id, _) if self.ck.types[id.0 as usize].is_host_class() => {
                loose(self);
                let tname = self.ck.types[id.0 as usize].display_name().into_owned();
                self.ck.report(
                    Diagnostic::error(
                        format!("`{tname}` is a host class and cannot be built with `{{ ... }}`"),
                        ty_expr.span,
                    )
                    .with_hint("only the host program creates its objects; call one of the functions it provides"),
                );
                return self.error_expr(span);
            }
            Ty::Adt(id, targs) if !self.ck.types[id.0 as usize].is_enum() => (*id, targs.clone()),
            Ty::Adt(id, _) => {
                loose(self);
                let tname = self.ck.types[id.0 as usize].display_name().into_owned();
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
        let hidden: Vec<String> = (0..self.ck.types[id.0 as usize].fields().len())
            .filter(|&i| !self.ck.field_visible(id, i))
            .map(|i| format!("`{}`", self.ck.types[id.0 as usize].fields()[i].name))
            .collect();
        if !hidden.is_empty() {
            loose(self);
            let tname = self.ck.types[id.0 as usize].display_name().into_owned();
            let package = self.ck.type_package(id);
            let plural = if hidden.len() == 1 { "" } else { "s" };
            let mut diag = Diagnostic::error(
                format!(
                    "`{tname}` cannot be built with `{{ ... }}` here: the field{plural} {} {} not exported from the package `{package}`",
                    hidden.join(", "),
                    if hidden.len() == 1 { "is" } else { "are" },
                ),
                ty_expr.span,
            );
            let ctors = self.ck.constructors_of(id);
            diag = diag.with_hint(if ctors.is_empty() {
                format!("`{tname}` exports no function that builds one; add a `pub fn` constructor next to it")
            } else {
                format!("use a constructor: {}", ctors.join(", "))
            });
            self.ck.report(diag);
            return self.error_expr(span);
        }
        let def = &self.ck.types[id.0 as usize];
        let is_class = def.is_class();
        let word = kind_word(def);
        let tname = def.display_name().into_owned();
        let map = Checker::param_map_of(def, &targs);
        let decls: Vec<FieldDef> = def
            .fields()
            .iter()
            .map(|f| FieldDef {
                ty: f.ty.subst(&map),
                ..f.clone()
            })
            .collect();
        let defaults = self.ck.type_extra[id.0 as usize].defaults.clone();
        let owner_mod = self.ck.type_extra[id.0 as usize].module.unwrap_or(BASE);
        let mut given: Vec<Option<Bound>> = decls.iter().map(|_| None).collect();
        let mut ok = true;
        for (source, init) in fields.iter().enumerate() {
            let Some(i) = decls.iter().position(|f| f.name == init.name.name) else {
                self.check_expr(&init.value, None);
                let names: Vec<String> = decls.iter().map(|f| f.name.clone()).collect();
                let mut diag = Diagnostic::error(
                    format!("the {word} `{tname}` has no field `{}`", init.name.name),
                    init.name.span,
                );
                diag = diag.with_hint(
                    Checker::did_you_mean(&init.name.name, &names)
                        .unwrap_or_else(|| format!("its fields are: {}", names.join(", "))),
                );
                self.ck.report(diag);
                ok = false;
                continue;
            };
            let field = &decls[i];
            let mut value = self.check_expr(&init.value, Some(&field.ty));
            let reason = Some((
                field.span,
                format!(
                    "the field `{}` is declared `{}` here",
                    field.name,
                    self.show(&field.ty)
                ),
            ));
            if field.weak {
                match self.coerce_to_weak(value, &field.ty, reason) {
                    Some(weak) => value = weak,
                    None => {
                        ok = false;
                        continue;
                    }
                }
            } else {
                ok &= self.expect(&value, &field.ty, reason);
            }
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
                (None, Some(default)) if decls[i].weak => {
                    let wanted = Ty::Option(Box::new(decls[i].ty.clone()));
                    let value = self.check_default_at_call(default, &wanted, span, owner_mod);
                    let expr = self.mk(
                        hir::ExprKind::ToWeak(Box::new(value)),
                        decls[i].ty.clone(),
                        span,
                    );
                    bound.push(Bound { expr, source: None });
                }
                (None, Some(default)) => {
                    let expr = self.check_default_at_call(default, &decls[i].ty, span, owner_mod);
                    bound.push(Bound { expr, source: None });
                }
                // A weak field that is not mentioned starts out empty (DESIGN §7.2: the
                // `Node { name: "root" }` example has no parent).
                (None, None) if decls[i].weak => {
                    let none = self.none_of(&decls[i].ty, span);
                    let expr = self.mk(
                        hir::ExprKind::ToWeak(Box::new(none)),
                        decls[i].ty.clone(),
                        span,
                    );
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
        let result = Ty::Adt(id, targs);
        if !ok {
            return self.mk(hir::ExprKind::Error, result, span);
        }
        let (values, lets, temps) = self.order_args(bound);
        let kind = if is_class {
            hir::ExprKind::Object {
                ty: id,
                fields: values,
            }
        } else {
            hir::ExprKind::Struct {
                ty: id,
                fields: values,
            }
        };
        let literal = self.mk(kind, result, span);
        self.wrap_hoisted(literal, lets, temps, span)
    }
}

/// `struct` or `class`, for messages.
pub(crate) fn kind_word(def: &TypeDef) -> &'static str {
    if def.is_class() { "class" } else { "struct" }
}
