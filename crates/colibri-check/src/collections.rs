//! List and map literals, and the constructors `List[T]()`, `Map[K, V]()`, `Set[T]()` and
//! `Set(list)` (DESIGN §5.2).

use colibri_syntax::ast::{self, ExprKind, IndexArg, TypeKind};
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::hir::{self, Builtin, Callee, Expr};
use crate::infer::VarKind;
use crate::sendable::SendOrigin;
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
                if !matches!(id.name.as_str(), "List" | "Map" | "Set" | "Channel")
                    || self.lookup_local(&id.name).is_some()
                    || self.ck.values.contains_key(&id.name)
                {
                    return None;
                }
                if id.name == "Channel" {
                    return Some(self.check_channel_new(Some(callee), args, expected, span));
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
                    "StrBuilder" => Some(self.check_str_builder_new(args, span)),
                    "Bytes" => Some(self.check_bytes_new(args, span)),
                    "BytesBuilder" => Some(self.check_bytes_builder_new(args, span)),
                    "Channel" => Some(self.check_channel_new(None, args, expected, span)),
                    "Shared" => Some(self.check_shared_new(args, expected, span)),
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

    /// `StrBuilder()` (DESIGN §12): an empty string builder.
    fn check_str_builder_new(&mut self, args: &[ast::Arg], span: Span) -> Expr {
        if !args.is_empty() {
            self.check_args_loosely(args);
            self.ck
                .report(Diagnostic::error("`StrBuilder()` takes no arguments", span));
            return self.error_expr(span);
        }
        self.builtin_call(
            Builtin::StrBuilderNew,
            Vec::new(),
            Ty::Adt(crate::types::STR_BUILDER_TYPE, Vec::new()),
            span,
        )
    }

    /// `BytesBuilder()` (DESIGN §12): an empty byte builder.
    fn check_bytes_builder_new(&mut self, args: &[ast::Arg], span: Span) -> Expr {
        if !args.is_empty() {
            self.check_args_loosely(args);
            self.ck.report(Diagnostic::error(
                "`BytesBuilder()` takes no arguments",
                span,
            ));
            return self.error_expr(span);
        }
        self.builtin_call(
            Builtin::BytesBuilderNew,
            Vec::new(),
            Ty::Adt(crate::types::BYTES_BUILDER_TYPE, Vec::new()),
            span,
        )
    }

    /// `Bytes()` (no bytes) and `Bytes(list)` (a `List[u8]`), DESIGN §5.1.
    fn check_bytes_new(&mut self, args: &[ast::Arg], span: Span) -> Expr {
        let [arg] = args else {
            if args.is_empty() {
                return self.builtin_call(Builtin::BytesEmpty, Vec::new(), Ty::Bytes, span);
            }
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!("`Bytes` takes one list, found {} arguments", args.len()),
                    span,
                )
                .with_hint("write `Bytes([1, 2, 255])`, or `Bytes()` for no bytes"),
            );
            return self.error_expr(span);
        };
        let want = Ty::List(Box::new(crate::builtins::BYTE));
        let value = self.check_expr(&arg.value, Some(&want));
        let reason = Some((
            arg.value.span,
            "`Bytes` is built from a list of `u8`".to_string(),
        ));
        if !self.expect(&value, &want, reason) {
            return self.error_expr(span);
        }
        self.builtin_call(Builtin::BytesFromList, vec![value], Ty::Bytes, span)
    }

    /// `Channel[T]()` and `Channel[T](capacity: n)` (DESIGN §14.1): a queue between threads. A
    /// channel without a capacity is unbounded. `callee` is the `Channel[T]` that was written;
    /// without it (`Channel()`) the element type comes from the context.
    fn check_channel_new(
        &mut self,
        callee: Option<&ast::Expr>,
        args: &[ast::Arg],
        expected: Option<&Ty>,
        span: Span,
    ) -> Expr {
        let elem = match callee {
            Some(callee) => {
                let Some(texpr) = expr_as_type(callee) else {
                    self.check_args_loosely(args);
                    self.ck
                        .report(Diagnostic::error("expected a type here", callee.span));
                    return self.error_expr(span);
                };
                match self.ck.resolve_type(&texpr) {
                    Ty::Adt(crate::types::CHANNEL_TYPE, a) if a.len() == 1 => a[0].clone(),
                    Ty::Error => {
                        self.check_args_loosely(args);
                        return self.error_expr(span);
                    }
                    _ => {
                        self.check_args_loosely(args);
                        self.ck.report(Diagnostic::error(
                            "`Channel` takes one type: the type of what it carries",
                            callee.span,
                        ));
                        return self.error_expr(span);
                    }
                }
            }
            None => match expected.map(|t| self.infer.shallow(t)) {
                Some(Ty::Adt(crate::types::CHANNEL_TYPE, a)) if a.len() == 1 => a[0].clone(),
                _ => {
                    self.check_args_loosely(args);
                    self.ck.report(
                        Diagnostic::error("cannot tell what this channel carries", span)
                            .with_hint("write the type: `Channel[int]()`"),
                    );
                    return self.error_expr(span);
                }
            },
        };
        let mut call_args = Vec::new();
        match args {
            [] => {}
            [a] if a.name.as_ref().is_none_or(|n| n.name == "capacity") => {
                let cap = self.check_expr(&a.value, Some(&Ty::INT));
                let reason = Some((
                    a.value.span,
                    "the capacity is a number of values".to_string(),
                ));
                if !self.expect(&cap, &Ty::INT, reason) {
                    return self.error_expr(span);
                }
                call_args.push(hir::Arg::Value(cap));
            }
            _ => {
                self.check_args_loosely(args);
                self.ck.report(
                    Diagnostic::error("`Channel` takes no arguments, or one: `capacity`", span)
                        .with_hint("write `Channel[int]()` or `Channel[int](capacity: 8)`"),
                );
                return self.error_expr(span);
            }
        }
        self.require_sendable(&elem, SendOrigin::ChannelElem, span);
        self.mk(
            hir::ExprKind::Call(Box::new(hir::Call {
                callee: hir::Callee::Intrinsic(hir::Intrinsic::NewChannel),
                type_args: Vec::new(),
                args: call_args,
            })),
            Ty::Adt(crate::types::CHANNEL_TYPE, vec![elem]),
            span,
        )
    }

    /// `Shared(x)` (DESIGN §14.1): a frozen copy of `x` that every thread can read with `get()`.
    fn check_shared_new(&mut self, args: &[ast::Arg], expected: Option<&Ty>, span: Span) -> Expr {
        let [arg] = args else {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error("`Shared` takes exactly one value", span)
                    .with_hint("write `Shared(table)`"),
            );
            return self.error_expr(span);
        };
        if arg.name.is_some() {
            self.check_args_loosely(args);
            self.ck.report(Diagnostic::error(
                "`Shared` takes its value without a name",
                span,
            ));
            return self.error_expr(span);
        }
        let want = match expected.map(|t| self.infer.shallow(t)) {
            Some(Ty::Adt(crate::types::SHARED_TYPE, a)) if a.len() == 1 => Some(a[0].clone()),
            _ => None,
        };
        let value = self.check_expr(&arg.value, want.as_ref());
        self.note_thread_boundary(&value);
        let ty = value.ty.clone();
        self.require_sendable(&ty, SendOrigin::SharedValue, arg.value.span);
        self.mk(
            hir::ExprKind::Call(Box::new(hir::Call {
                callee: hir::Callee::Intrinsic(hir::Intrinsic::NewShared),
                type_args: Vec::new(),
                args: vec![hir::Arg::Value(value)],
            })),
            Ty::Adt(crate::types::SHARED_TYPE, vec![ty]),
            span,
        )
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
