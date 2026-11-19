//! List and map literals, and the constructors `List[T]()`, `Map[K, V]()`, `Set[T]()` and
//! `Set(list)` (DESIGN §5.2).

use colibri_syntax::ast::{self, ExprKind, IndexArg, TypeKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::hir::{self, Builtin, Callee, Expr};
use crate::infer::VarKind;
use crate::types::Ty;

/// Reads an expression such as `Map[str, List[int]]` back as a type (the parser cannot tell
/// `List[int]()` from an index until it sees what follows).
pub(crate) fn expr_as_type(e: &ast::Expr) -> Option<ast::TypeExpr> {
    let kind = match &e.kind {
        ExprKind::Ident(id) => TypeKind::Named {
            path: vec![id.clone()],
            args: Vec::new(),
        },
        // `shapes.Shape`: a type of an imported module (the checker decides if `shapes` is one).
        ExprKind::Field { base, name } => {
            let ExprKind::Ident(module) = &base.kind else {
                return None;
            };
            TypeKind::Named {
                path: vec![module.clone(), name.clone()],
                args: Vec::new(),
            }
        }
        ExprKind::Index { base, args } => {
            let path = match &base.kind {
                ExprKind::Ident(id) => vec![id.clone()],
                ExprKind::Field { base, name } => match &base.kind {
                    ExprKind::Ident(module) => vec![module.clone(), name.clone()],
                    _ => return None,
                },
                _ => return None,
            };
            let args = args
                .iter()
                .map(|a| match a {
                    IndexArg::Type(t) => Some(t.clone()),
                    IndexArg::Expr(x) => expr_as_type(x),
                })
                .collect::<Option<Vec<_>>>()?;
            TypeKind::Named { path, args }
        }
        ExprKind::Tuple(items) => {
            TypeKind::Tuple(items.iter().map(expr_as_type).collect::<Option<Vec<_>>>()?)
        }
        ExprKind::Try(inner) => TypeKind::Optional(Box::new(expr_as_type(inner)?)),
        ExprKind::Paren(inner) => return expr_as_type(inner),
        _ => return None,
    };
    Some(ast::TypeExpr {
        id: e.id,
        span: e.span,
        kind,
    })
}

impl FnCtx<'_, '_> {
    /// `[a, b]`; the expected list type, if there is one, types the elements.
    pub fn check_list_lit(
        &mut self,
        items: &[ast::Expr],
        expected: Option<&Ty>,
        span: Span,
    ) -> Expr {
        let elem = match expected.map(|t| self.infer.shallow(t)) {
            Some(Ty::List(t)) => *t,
            // The annotation was already reported; do not report the literal too.
            Some(Ty::Error) => Ty::Error,
            _ => self.fresh_var(VarKind::General),
        };
        let mut values = Vec::new();
        let mut first: Option<Span> = None;
        for item in items {
            let value = self.check_expr(item, Some(&elem));
            let reason = first.map(|s| (s, "the first element sets the list's type".to_string()));
            self.expect(&value, &elem, reason);
            first.get_or_insert(item.span);
            values.push(value);
        }
        self.mk(
            hir::ExprKind::ListLit(values),
            Ty::List(Box::new(elem)),
            span,
        )
    }

    /// `{k: v}` and `{:}`.
    pub fn check_map_lit(
        &mut self,
        entries: &[(ast::Expr, ast::Expr)],
        expected: Option<&Ty>,
        span: Span,
    ) -> Expr {
        let (key, value) = match expected.map(|t| self.infer.shallow(t)) {
            Some(Ty::Map(k, v)) => (*k, *v),
            Some(Ty::Error) => (Ty::Error, Ty::Error),
            _ => (
                self.fresh_var(VarKind::General),
                self.fresh_var(VarKind::General),
            ),
        };
        let mut out = Vec::new();
        let mut first: Option<(Span, Span)> = None;
        for (k, v) in entries {
            let kv = self.check_expr(k, Some(&key));
            let vv = self.check_expr(v, Some(&value));
            let key_reason =
                first.map(|(s, _)| (s, "the first key sets the map's key type".to_string()));
            let value_reason =
                first.map(|(_, s)| (s, "the first value sets the map's value type".to_string()));
            self.expect(&kv, &key, key_reason);
            self.expect(&vv, &value, value_reason);
            first.get_or_insert((k.span, v.span));
            out.push((kv, vv));
        }
        if let Some((k, _)) = entries.first()
            && !self.require_hash(&key, "Map", k.span)
        {
            return self.error_expr(span);
        }
        self.mk(
            hir::ExprKind::MapLit(out),
            Ty::Map(Box::new(key), Box::new(value)),
            span,
        )
    }

    /// `List[T]()`, `Map[K, V]()`, `Set[T]()` and `Set(list)`. `None` when `callee` is not one
    /// of them.
    pub fn check_constructor(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::Arg],
        expected: Option<&Ty>,
        span: Span,
    ) -> Option<Expr> {
        match &callee.kind {
            ExprKind::Index { base, .. } => {
                let ExprKind::Ident(id) = &base.kind else {
                    return None;
                };
                if !matches!(id.name.as_str(), "List" | "Map" | "Set")
                    || self.lookup_local(&id.name).is_some()
                    || self.ck.values.contains_key(&id.name)
                {
                    return None;
                }
                Some(self.check_empty_constructor(callee, &id.name, args, span))
            }
            ExprKind::Ident(id) if self.lookup_local(&id.name).is_none() => {
                if self.ck.values.contains_key(&id.name) {
                    return None;
                }
                match id.name.as_str() {
                    name if Ty::numeric_by_name(name).is_some() => {
                        let target = Ty::numeric_by_name(name)?;
                        Some(self.check_conversion(target, args, span))
                    }
                    "Set" => Some(self.check_set_from(args, expected, span)),
                    "CoGroup" | "Signal" => {
                        Some(self.check_group_or_signal_new(&id.name, args, span))
                    }
                    "List" | "Map" => {
                        self.check_args_loosely(args);
                        let example = if id.name == "List" {
                            "List[int]()"
                        } else {
                            "Map[str, int]()"
                        };
                        self.ck.report(
                            Diagnostic::error(
                                format!(
                                    "`{}` needs its type arguments to build an empty one",
                                    id.name
                                ),
                                callee.span,
                            )
                            .with_hint(format!(
                                "write `{example}`, or use a literal with a type annotation"
                            )),
                        );
                        Some(self.error_expr(span))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `CoGroup()` and `Signal()` (DESIGN §14.2).
    fn check_group_or_signal_new(&mut self, name: &str, args: &[ast::Arg], span: Span) -> Expr {
        if !args.is_empty() {
            self.check_args_loosely(args);
            self.ck.report(Diagnostic::error(
                format!("`{name}()` takes no arguments"),
                span,
            ));
            return self.error_expr(span);
        }
        let (ty_id, intrinsic) = if name == "CoGroup" {
            (crate::types::COGROUP_TYPE, hir::Intrinsic::NewCoGroup)
        } else {
            (crate::types::SIGNAL_TYPE, hir::Intrinsic::NewSignal)
        };
        self.mk(
            hir::ExprKind::Call(Box::new(hir::Call {
                callee: hir::Callee::Intrinsic(intrinsic),
                type_args: Vec::new(),
                args: Vec::new(),
            })),
            Ty::Adt(ty_id, Vec::new()),
            span,
        )
    }

    /// `List[int]()`, `Map[str, int]()`, `Set[int]()`: an empty collection.
    fn check_empty_constructor(
        &mut self,
        callee: &ast::Expr,
        name: &str,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        let Some(texpr) = expr_as_type(callee) else {
            self.check_args_loosely(args);
            self.ck
                .report(Diagnostic::error("expected a type here", callee.span));
            return self.error_expr(span);
        };
        let ty = self.ck.resolve_type(&texpr);
        if !args.is_empty() {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!("`{name}[..]()` builds an empty collection and takes no arguments"),
                    span,
                )
                .with_hint(if name == "List" {
                    "to start from values, write a list literal: `[1, 2, 3]`"
                } else if name == "Set" {
                    "to start from values, write `Set([1, 2, 3])`"
                } else {
                    "to start from entries, write a map literal: `{\"a\": 1}`"
                }),
            );
            return self.error_expr(span);
        }
        match &ty {
            Ty::List(_) => self.mk(hir::ExprKind::ListLit(Vec::new()), ty, span),
            Ty::Map(..) => self.mk(hir::ExprKind::MapLit(Vec::new()), ty, span),
            Ty::Set(_) => self.builtin_call(Builtin::SetNew, Vec::new(), ty, span),
            _ => self.error_expr(span),
        }
    }

    /// `Set([1, 2, 3])`: the elements of a list, without duplicates.
    fn check_set_from(&mut self, args: &[ast::Arg], expected: Option<&Ty>, span: Span) -> Expr {
        let [arg] = args else {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!("`Set` takes one list, found {} arguments", args.len()),
                    span,
                )
                .with_hint("write `Set([1, 2, 3])`, or `Set[int]()` for an empty set"),
            );
            return self.error_expr(span);
        };
        let hint = match expected.map(|t| self.infer.shallow(t)) {
            Some(Ty::Set(t)) => Some(Ty::List(t)),
            _ => None,
        };
        let value = self.check_expr(&arg.value, hint.as_ref());
        match self.infer.shallow(&value.ty) {
            Ty::Error => self.error_expr(span),
            Ty::List(elem) => {
                if !self.require_hash(&elem, "Set", arg.value.span) {
                    return self.error_expr(span);
                }
                self.builtin_call(Builtin::SetFromList, vec![value], Ty::Set(elem), span)
            }
            other => {
                let shown = self.show(&other);
                self.ck.report(
                    Diagnostic::error(
                        format!("`Set` is built from a list, but this is a `{shown}`"),
                        arg.value.span,
                    )
                    .with_hint("write `Set([1, 2, 3])`"),
                );
                self.error_expr(span)
            }
        }
    }

    /// A call of a built-in operation with every argument given.
    pub fn builtin_call(&self, op: Builtin, args: Vec<Expr>, ty: Ty, span: Span) -> Expr {
        let call = hir::ExprKind::Call(Box::new(hir::Call {
            callee: Callee::builtin(op),
            type_args: Vec::new(),
            args: args.into_iter().map(hir::Arg::Value).collect(),
        }));
        self.mk(call, ty, span)
    }
}
