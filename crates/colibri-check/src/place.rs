//! Places: assignment targets and `var self` receivers (ARCHITECTURE §1 and §4).
//!
//! A place is an anchor (a local or a module-level value) plus a path of field and tuple
//! projections. Structs and tuples are values, so writing through a path is only allowed when
//! the anchor itself is mutable.

use colibri_syntax::ast::{self, BindKind, ExprKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::{FnCtx, LocalKind};
use crate::checker::{Dep, ValueRef};
use crate::hir::{Anchor, LocalId, Place, ValueProj};

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

    /// The place an expression names: a variable, or a field or tuple element of one.
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
                let mut place = self.place_base(base)?;
                let (idx, ty) = self.field_of(&place.ty, name, base.span)?;
                place.path.push(ValueProj::Field(idx));
                place.ty = ty;
                place.display = format!("{}.{}", place.display, name.name);
                Some(place)
            }
            ExprKind::TupleIndex { base, index } => {
                let mut place = self.place_base(base)?;
                let ty = self.tuple_elem_of(&place.ty, *index, target.span)?;
                place.path.push(ValueProj::TupleIdx(*index));
                place.ty = ty;
                place.display = format!("{}.{}", place.display, index);
                Some(place)
            }
            ExprKind::Index { .. } => {
                self.ck.unsupported("assignment to an element", target.span);
                None
            }
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

    /// The place a field or tuple access starts from.
    fn place_base(&mut self, base: &ast::Expr) -> Option<Place> {
        match &base.kind {
            ExprKind::Paren(_)
            | ExprKind::Ident(_)
            | ExprKind::SelfValue
            | ExprKind::Field { .. }
            | ExprKind::TupleIndex { .. }
            | ExprKind::Index { .. } => self.resolve_place(base),
            ExprKind::Error => None,
            _ => {
                // Check it for its own errors; the value is a temporary either way.
                let value = self.check_expr(base, None);
                if !value.ty.is_error() {
                    self.ck.report(
                        Diagnostic::error(
                            "cannot assign to a part of a temporary value",
                            base.span,
                        )
                        .with_hint("the change would be lost; store the value in a `var` first"),
                    );
                }
                None
            }
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
        if let Some(local) = self.lookup_local(name) {
            return Some(self.local_place(local, name));
        }
        match self.ck.values.get(name).copied() {
            Some(ValueRef::Global(g)) => {
                let ty = self.ck.global_ty(g);
                self.add_dep(Dep::Global(g));
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
            Root::Mutable => return true,
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
