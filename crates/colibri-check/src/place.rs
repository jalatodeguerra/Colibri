//! Places: assignment targets and `var self` receivers (ARCHITECTURE §1 and §4).
//!
//! A place is an anchor (a local or a module-level value) plus a path of field and tuple
//! projections. Structs and tuples are values, so writing through a path is only allowed when
//! the anchor itself is mutable.

use colibri_syntax::ast::{self, BindKind, ExprKind, IndexArg};
use colibri_syntax::{Diagnostic, Span};

use crate::body::{FnCtx, LocalKind};
use crate::checker::{Dep, ValueRef};
use crate::deprecated::DeprKey;
use crate::hir::{self, Anchor, Builtin, Expr, LocalId, Place, ValueProj};
use crate::types::Ty;

/// What a field or element access starts from: a place, or a computed value.
/// What `base[index]` is: a place that can be assigned to, or (a `Bytes`) a value.
enum Indexed {
    Place(Place),
    Value(Expr),
}

enum Base {
    Place(Place),
    Value(Expr),
}

impl Base {
    fn ty(&self) -> Ty {
        match self {
            Base::Place(p) => p.ty.clone(),
            Base::Value(e) => e.ty.clone(),
        }
    }
}

/// A short source-like text of an expression, for messages (`enemies`, `world.enemies`,
/// `xs[i]`). Anything complicated shows as `..`.
pub(crate) fn expr_text(e: &ast::Expr) -> String {
    match &e.kind {
        ExprKind::Ident(id) => id.name.clone(),
        ExprKind::SelfValue => "self".to_string(),
        ExprKind::Paren(inner) => expr_text(inner),
        ExprKind::Field { base, name } => format!("{}.{}", expr_text(base), name.name),
        ExprKind::TupleIndex { base, index } => format!("{}.{index}", expr_text(base)),
        ExprKind::Int(n) => n.to_string(),
        ExprKind::Index { base, args } => {
            let inner = match args.as_slice() {
                [IndexArg::Expr(i)] => expr_text(i),
                _ => "..".to_string(),
            };
            format!("{}[{inner}]", expr_text(base))
        }
        ExprKind::Call { callee, .. } => format!("{}(..)", expr_text(callee)),
        _ => "..".to_string(),
    }
}

/// Why a place is being used mutably, for the wording of errors.
#[derive(Clone, Copy)]
pub(crate) enum Action<'a> {
    Assign,
    /// Calling the named `var self` method on the place.
    Call(&'a str),
}

/// Whether the root of a place may change.
enum Root {
    Mutable,
    Let,
    Const,
    Param,
    /// `self` of a method that does not take `var self`.
    ReadOnlySelf,
}

impl FnCtx<'_, '_> {
    /// Resolves an assignment target, checking that it may be assigned to. After an error the
    /// place is still returned (when it exists) so the right-hand side is checked too.
    pub fn check_place(&mut self, target: &ast::Expr) -> Option<Place> {
        let place = self.resolve_place(target)?;
        self.require_mutable(&place, target.span, Action::Assign);
        Some(place)
    }

    /// The place an expression names: a variable, a field or tuple element of one, a list
    /// element or map entry, or a field of a class object. Collections and classes are
    /// references, so the place is *anchored* at the innermost one and everything before it is an
    /// ordinary read (ARCHITECTURE §1): `a.b[i].c[j].x` anchors on the element `[j]` with path
    /// `[x]`.
    pub fn resolve_place(&mut self, target: &ast::Expr) -> Option<Place> {
        match &target.kind {
            ExprKind::Paren(inner) => self.resolve_place(inner),
            ExprKind::Ident(id) => self.ident_place(id, target.span),
            ExprKind::SelfValue => match self.lookup_local("self") {
                Some(local) => Some(self.local_place(local, "self")),
                None => {
                    self.report_self_outside_method(target.span);
                    None
                }
            },
            ExprKind::Field { base, name } => {
                if let Some(q) = self.qualified_parts(base, name) {
                    return self.ident_place(&q, target.span);
                }
                let base_place = self.place_base(base)?;
                let base_ty = base_place.ty();
                if let Ty::Vec(v) = self.infer.shallow(&base_ty) {
                    let (proj, ty) = self.vec_place_step(v, name, target.span)?;
                    let mut place = self.value_part(base_place, base)?;
                    place.path.push(proj);
                    place.ty = ty;
                    place.display = format!("{}.{}", place.display, name.name);
                    return Some(place);
                }
                let (idx, ty) = self.field_of(&base_ty, name, base.span)?;
                if self.is_class_ty(&base_ty) {
                    let object = self.base_object(base_place, base)?;
                    return Some(Place {
                        anchor: Anchor::ClassField(Box::new(object), idx),
                        path: Vec::new(),
                        ty,
                        display: expr_text(target),
                    });
                }
                let mut place = self.value_part(base_place, base)?;
                place.path.push(ValueProj::Field(idx));
                place.ty = ty;
                place.display = format!("{}.{}", place.display, name.name);
                Some(place)
            }
            ExprKind::TupleIndex { base, index } => {
                let base_place = self.place_base(base)?;
                let ty = self.tuple_elem_of(&base_place.ty(), *index, target.span)?;
                let mut place = self.value_part(base_place, base)?;
                place.path.push(ValueProj::TupleIdx(*index));
                place.ty = ty;
                place.display = format!("{}.{}", place.display, index);
                Some(place)
            }
            ExprKind::Index { base, args } => self.index_place(base, args, target),
            ExprKind::Error => None,
            _ => {
                self.ck.report(Diagnostic::error(
                    "this expression cannot be assigned to",
                    target.span,
                ));
                None
            }
        }
    }

    /// What a field or element access starts from.
    fn place_base(&mut self, base: &ast::Expr) -> Option<Base> {
        match &base.kind {
            ExprKind::Paren(_)
            | ExprKind::Ident(_)
            | ExprKind::SelfValue
            | ExprKind::Field { .. }
            | ExprKind::TupleIndex { .. }
            | ExprKind::Index { .. } => self.resolve_place(base).map(Base::Place),
            ExprKind::Error => None,
            _ => {
                let value = self.check_expr(base, None);
                if value.ty.is_error() {
                    None
                } else {
                    Some(Base::Value(value))
                }
            }
        }
    }

    /// The part of a value-typed base that a field or tuple step walks into: only a place can
    /// be changed, so a temporary is an error.
    fn value_part(&mut self, base: Base, span_of: &ast::Expr) -> Option<Place> {
        match base {
            Base::Place(place) => Some(place),
            Base::Value(_) => {
                self.ck.report(
                    Diagnostic::error("cannot assign to a part of a temporary value", span_of.span)
                        .with_hint("the change would be lost; store the value in a `var` first"),
                );
                None
            }
        }
    }

    /// The class object a base evaluates to, as an expression. A weak field reads as an
    /// `Option`, so it cannot be the base of a field access.
    fn base_object(&mut self, base: Base, base_ast: &ast::Expr) -> Option<Expr> {
        match base {
            Base::Value(e) => Some(e),
            Base::Place(place) => {
                if self.is_weak_place(&place) {
                    self.report_weak_use(&place.display, base_ast.span);
                    return None;
                }
                let ty = place.ty.clone();
                Some(self.mk(hir::ExprKind::Read(Box::new(place)), ty, base_ast.span))
            }
        }
    }

    /// Whether a place is a `weak` class field (its stored type is the class, its reads give
    /// an `Option`).
    pub fn is_weak_place(&self, place: &Place) -> bool {
        let Anchor::ClassField(object, idx) = &place.anchor else {
            return false;
        };
        if !place.path.is_empty() {
            return false;
        }
        let Ty::Adt(id, _) = self.infer.shallow(&object.ty) else {
            return false;
        };
        self.ck.types[id.0 as usize]
            .fields()
            .get(idx.0 as usize)
            .is_some_and(|f| f.weak)
    }

    pub fn report_weak_use(&mut self, name: &str, span: Span) {
        self.ck.report(
            Diagnostic::error(
                format!("`{name}` is a weak reference, so it reads as an optional value"),
                span,
            )
            .with_hint(format!(
                "unwrap it first: `if let some(x) = {name} {{ ... }}`"
            )),
        );
    }

    /// Whether `ty` is a class type.
    pub fn is_class_ty(&self, ty: &Ty) -> bool {
        matches!(self.infer.shallow(ty), Ty::Adt(id, _) if self.ck.types[id.0 as usize].is_class())
    }

    /// Whether `ty` is a class of the host.
    pub fn is_host_class_ty(&self, ty: &Ty) -> bool {
        matches!(self.infer.shallow(ty), Ty::Adt(id, _) if self.ck.types[id.0 as usize].is_host_class())
    }

    /// `base[index]` as a place: a list element or a map entry.
    pub fn index_place(
        &mut self,
        base: &ast::Expr,
        args: &[IndexArg],
        whole: &ast::Expr,
    ) -> Option<Place> {
        match self.index_target(base, args, whole, false)? {
            Indexed::Place(place) => Some(place),
            Indexed::Value(_) => None,
        }
    }

    /// `base[index]`. A `Bytes` is not a place (it is immutable): read as a value it is the
    /// `BytesAt` call, and as the target of an assignment it is an error.
    fn index_target(
        &mut self,
        base: &ast::Expr,
        args: &[IndexArg],
        whole: &ast::Expr,
        read: bool,
    ) -> Option<Indexed> {
        let [IndexArg::Expr(index)] = args else {
            self.check_index_args_loosely(base, args);
            self.ck.unsupported("a generic instantiation", whole.span);
            return None;
        };
        if let ExprKind::Ident(id) = &base.kind
            && self.lookup_local(&id.name).is_none()
            && !matches!(self.ck.values.get(&id.name), Some(ValueRef::Global(_)))
        {
            self.ck.unsupported("a generic instantiation", whole.span);
            return None;
        }
        if self.slice_range(args).is_some() {
            self.check_expr(base, None);
            self.ck.report(
                Diagnostic::error("a slice is a copy, so it cannot be assigned to", index.span)
                    .with_hint("assign to the elements instead: `xs[i] = value`"),
            );
            return None;
        }
        let collection = self.check_expr(base, None);
        let name = expr_text(base);
        match self.infer.shallow(&collection.ty) {
            Ty::Error => {
                self.check_expr(index, None);
                None
            }
            Ty::List(elem) => {
                let idx = self.check_expr(index, Some(&Ty::INT));
                let reason = Some((index.span, "a list is indexed by an `int`".to_string()));
                if !self.expect(&idx, &Ty::INT, reason) {
                    return None;
                }
                Some(Indexed::Place(Place {
                    anchor: Anchor::ListElem(Box::new(collection), Box::new(idx), name),
                    path: Vec::new(),
                    ty: *elem,
                    display: expr_text(whole),
                }))
            }
            Ty::Map(key, value) => {
                let k = self.check_expr(index, Some(&key));
                let reason = Some((
                    base.span,
                    format!("this map's keys are `{}`", self.show(&key)),
                ));
                if !self.expect(&k, &key, reason) {
                    return None;
                }
                Some(Indexed::Place(Place {
                    anchor: Anchor::MapEntry(Box::new(collection), Box::new(k), name),
                    path: Vec::new(),
                    ty: *value,
                    display: expr_text(whole),
                }))
            }
            Ty::Bytes if read => {
                let idx = self.check_expr(index, Some(&Ty::INT));
                let reason = Some((index.span, "`Bytes` is indexed by an `int`".to_string()));
                if !self.expect(&idx, &Ty::INT, reason) {
                    return None;
                }
                Some(Indexed::Value(self.builtin_call(
                    Builtin::BytesAt,
                    vec![collection, idx],
                    crate::builtins::BYTE,
                    whole.span,
                )))
            }
            other => {
                self.check_expr(index, None);
                let shown = self.show(&other);
                let message = if other == Ty::Bytes {
                    "cannot assign to an element of a `Bytes`: it is immutable".to_string()
                } else {
                    format!("cannot index into a value of type `{shown}`")
                };
                let mut diag = Diagnostic::error(message, whole.span);
                diag = match other {
                    Ty::Str => diag.with_hint(
                        "strings cannot be indexed by position; use `s.chars()` to go through the characters",
                    ),
                    Ty::Bytes => diag.with_hint(
                        "a `Bytes` is immutable; build a new one with `BytesBuilder`",
                    ),
                    _ => diag.with_hint("only lists, maps and `Bytes` can be indexed"),
                };
                self.ck.report(diag);
                None
            }
        }
    }

    fn check_index_args_loosely(&mut self, base: &ast::Expr, args: &[IndexArg]) {
        self.check_expr(base, None);
        for a in args {
            if let IndexArg::Expr(e) = a {
                self.check_expr(e, None);
            }
        }
    }

    /// `base[index]` used as a value.
    pub fn check_index_expr(
        &mut self,
        base: &ast::Expr,
        args: &[IndexArg],
        whole: &ast::Expr,
    ) -> Expr {
        if let Some(range) = self.slice_range(args) {
            return self.check_slice(base, range, whole.span);
        }
        if let ExprKind::Ident(id) = &base.kind
            && !self.is_local(&id.name)
            && let Some(f) = self.lookup_fn(&id.name)
        {
            // `max[int]`: a generic function instantiated, as a value.
            return match self.explicit_type_args(args) {
                Some(explicit) => self.fn_value(f, Some(explicit), None, whole.span),
                None => self.error_expr(whole.span),
            };
        }
        match self.index_target(base, args, whole, true) {
            Some(Indexed::Place(place)) => {
                let ty = place.ty.clone();
                self.mk(hir::ExprKind::Read(Box::new(place)), ty, whole.span)
            }
            Some(Indexed::Value(value)) => value,
            None => self.error_expr(whole.span),
        }
    }

    pub fn local_place(&self, local: LocalId, name: &str) -> Place {
        Place {
            anchor: Anchor::Local(local),
            path: Vec::new(),
            ty: self.local_ty(local),
            display: name.to_string(),
        }
    }

    fn ident_place(&mut self, id: &ast::Ident, span: Span) -> Option<Place> {
        let name = id.name.as_str();
        let plain_assign = std::mem::take(&mut self.whole_assign_target);
        if let Some(local) = self.lookup_local(name) {
            if !plain_assign {
                self.flags[local.0 as usize].read = true;
            }
            return Some(self.local_place(local, name));
        }
        match self.ck.values.get(name).copied() {
            Some(ValueRef::Global(g)) => {
                let ty = self.ck.global_ty(g);
                self.add_dep(Dep::Global(g), id.span);
                self.ck.note_deprecated_use(DeprKey::Global(g), id.span);
                Some(Place {
                    anchor: Anchor::Global(g),
                    path: Vec::new(),
                    ty,
                    display: name.to_string(),
                })
            }
            Some(ValueRef::Fn(_)) => {
                self.ck.report(Diagnostic::error(
                    format!("cannot assign to the function `{name}`"),
                    span,
                ));
                None
            }
            None => {
                self.report_unknown_name(name, span);
                None
            }
        }
    }

    pub fn report_self_outside_method(&mut self, span: Span) {
        self.ck.report(
            Diagnostic::error("`self` can only be used inside a method", span)
                .with_hint("methods are declared in an `impl` block and take `self` first"),
        );
    }

    /// How the root of `place` was declared: its name, where, and whether it may change.
    fn root_of(&self, place: &Place) -> Option<(String, Span, Root)> {
        match &place.anchor {
            Anchor::Local(id) => {
                let info = &self.locals[id.0 as usize];
                let root = match self.kinds[id.0 as usize] {
                    LocalKind::Var | LocalKind::Temp => Root::Mutable,
                    LocalKind::Let => Root::Let,
                    LocalKind::Const => Root::Const,
                    LocalKind::Param if info.name == "self" => Root::ReadOnlySelf,
                    LocalKind::Param => Root::Param,
                };
                Some((info.name.clone(), info.span, root))
            }
            Anchor::Global(g) => {
                let info = &self.ck.globals[g.0 as usize];
                let root = if info.mutable {
                    Root::Mutable
                } else if info.kind == BindKind::Const {
                    Root::Const
                } else {
                    Root::Let
                };
                Some((info.name.clone(), info.name_span, root))
            }
            _ => None,
        }
    }

    /// Checks that `place` may be changed; reports why not and returns `false` otherwise.
    pub fn require_mutable(&mut self, place: &Place, span: Span, action: Action<'_>) -> bool {
        let Some((name, decl, root)) = self.root_of(place) else {
            return true;
        };
        let whole = place.path.is_empty();
        let what = match action {
            Action::Assign => format!("assign to `{}`", place.display),
            Action::Call(method) => format!("call `{method}` on `{}`", place.display),
        };
        let diag = match root {
            Root::Mutable => {
                // A later assignment decides whether a captured variable needs a cell.
                if let Anchor::Local(id) = &place.anchor
                    && let Some(f) = self.flags.get_mut(id.0 as usize)
                {
                    f.assigned = true;
                }
                return true;
            }
            Root::Let | Root::Const => {
                let is_let = matches!(root, Root::Let);
                let word = if is_let { "variable" } else { "constant" };
                let message = match (whole, action) {
                    (true, Action::Assign) => {
                        format!("cannot assign to the {word} `{name}`, which is not mutable")
                    }
                    (true, Action::Call(_)) => format!("cannot {what}, which is not mutable"),
                    (false, _) => format!("cannot {what}, because `{name}` is not mutable"),
                };
                let hint = if is_let {
                    format!("declare it with `var {name} = ...` to allow changing it")
                } else {
                    "constants never change; use `var` for a value that does".to_string()
                };
                Diagnostic::error(message, span)
                    .with_label(decl, format!("`{name}` is declared here without `var`"))
                    .with_hint(hint)
            }
            Root::Param => Diagnostic::error(
                match (whole, action) {
                    (true, Action::Assign) => {
                        format!("cannot assign to the parameter `{name}`")
                    }
                    _ => format!("cannot {what}, because the parameter `{name}` is immutable"),
                },
                span,
            )
            .with_label(decl, "parameters are immutable")
            .with_hint(format!("copy it first: `var {name}2 = {name}`")),
            Root::ReadOnlySelf => Diagnostic::error(
                format!("cannot {what}, because `self` is read-only in this method"),
                span,
            )
            .with_label(decl, "`self` is declared here")
            .with_hint("declare the method with `var self` if it should change the value"),
        };
        self.ck.report(diag);
        false
    }
}
