//! The built-in `Result[T, E]` and `Error` (DESIGN §10): building `ok(..)` and `err(..)`,
//! converting values to `Error` through `IntoError`, and the methods of `Option`, `Result`
//! and `Error`.
//!
//! `Result` is the prelude enum at [`RESULT_TYPE`] and `Error` the prelude class at
//! [`ERROR_TYPE`]; both are ordinary `Ty::Adt` types, so patterns, equality, `Show` and
//! exhaustiveness need no special cases. The methods are typed like those of `List`: a
//! signature per method over the receiver's type arguments (`crate::builtins`).

use colibri_syntax::ast;
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::builtins::{Fresh, Sig, func, optional, sig};
use crate::call::Receiver;
use crate::hir::{self, Builtin, Callee, Coercion, ErrorConv, Expr, Intrinsic};
use crate::infer::VarKind;
use crate::traits::{Source, key_of};
use crate::types::{ERROR_TYPE, INTO_ERROR, RESULT_TYPE, Ty, VARIANT_ERR, VARIANT_OK};

pub(crate) const OPTION_METHODS: &[&str] =
    &["is_some", "is_none", "expect", "map", "and_then", "clone"];
pub(crate) const RESULT_METHODS: &[&str] = &[
    "is_ok", "is_err", "expect", "map", "and_then", "ok", "err", "context", "clone",
];
pub(crate) const ERROR_METHODS: &[&str] =
    &["message", "context", "cause", "is[T]", "as[T]", "clone"];

/// The type `Error`.
pub(crate) fn error_ty() -> Ty {
    Ty::Adt(ERROR_TYPE, Vec::new())
}

/// `Result[ok, err]`.
pub(crate) fn result_ty(ok: Ty, err: Ty) -> Ty {
    Ty::Adt(RESULT_TYPE, vec![ok, err])
}

pub(crate) fn is_error_ty(ty: &Ty) -> bool {
    matches!(ty, Ty::Adt(id, _) if *id == ERROR_TYPE)
}

/// The signature of a method of `T?`.
pub(crate) fn option_sig(t: &Ty, name: &str, fresh: Fresh<'_>) -> Option<Sig> {
    Some(match name {
        "is_some" => sig(Builtin::OptionIsSome, vec![], Ty::Bool),
        "is_none" => sig(Builtin::OptionIsNone, vec![], Ty::Bool),
        "expect" => sig(Builtin::OptionExpect, vec![("msg", Ty::Str)], t.clone()),
        "map" => {
            let u = fresh();
            sig(
                Builtin::OptionMap,
                vec![("f", func(vec![t.clone()], u.clone()))],
                optional(u),
            )
        }
        "and_then" => {
            let u = fresh();
            sig(
                Builtin::OptionAndThen,
                vec![("f", func(vec![t.clone()], optional(u.clone())))],
                optional(u),
            )
        }
        _ => return None,
    })
}

/// The signature of a method of `Result[t, e]`.
pub(crate) fn result_sig(t: &Ty, e: &Ty, name: &str, fresh: Fresh<'_>) -> Option<Sig> {
    Some(match name {
        "is_ok" => sig(Builtin::ResultIsOk, vec![], Ty::Bool),
        "is_err" => sig(Builtin::ResultIsErr, vec![], Ty::Bool),
        "expect" => sig(Builtin::ResultExpect, vec![("msg", Ty::Str)], t.clone()),
        "map" => {
            let u = fresh();
            sig(
                Builtin::ResultMap,
                vec![("f", func(vec![t.clone()], u.clone()))],
                result_ty(u, e.clone()),
            )
        }
        "and_then" => {
            let u = fresh();
            sig(
                Builtin::ResultAndThen,
                vec![("f", func(vec![t.clone()], result_ty(u.clone(), e.clone())))],
                result_ty(u, e.clone()),
            )
        }
        "ok" => sig(Builtin::ResultOk, vec![], optional(t.clone())),
        "err" => sig(Builtin::ResultErr, vec![], optional(e.clone())),
        // The receiver has been converted to `Result[T, Error]` by `prepare_receiver`.
        "context" => sig(
            Builtin::ResultContext,
            vec![("msg", Ty::Str)],
            result_ty(t.clone(), error_ty()),
        ),
        _ => return None,
    })
}

/// The signature of a method of `Error`. (`is[T]` and `as[T]` take a type argument and are
/// checked by [`FnCtx::check_error_downcast`].)
pub(crate) fn error_sig(name: &str) -> Option<Sig> {
    Some(match name {
        "message" => sig(Builtin::ErrorMessage, vec![], Ty::Str),
        "context" => sig(Builtin::ErrorContext, vec![("msg", Ty::Str)], error_ty()),
        "cause" => sig(Builtin::ErrorCause, vec![], optional(error_ty())),
        _ => return None,
    })
}

pub(crate) const COROUTINE_METHODS: &[&str] = &["cancel", "done", "join", "join_result", "clone"];

/// The signature of a method of `Coroutine[T]`.
pub(crate) fn coroutine_sig(t: &Ty, name: &str) -> Option<Sig> {
    Some(match name {
        "cancel" => sig(Builtin::CoroutineCancel, vec![], Ty::Unit),
        "done" => sig(Builtin::CoroutineDone, vec![], Ty::Bool),
        "join" => sig(Builtin::CoroutineJoin, vec![], t.clone()),
        "join_result" => sig(
            Builtin::CoroutineJoinResult,
            vec![],
            result_ty(t.clone(), error_ty()),
        ),
        _ => return None,
    })
}

pub(crate) const TASK_METHODS: &[&str] = &["join", "done", "try_join"];

/// The signature of a method of `Task[T]` (DESIGN §14.1).
pub(crate) fn task_sig(t: &Ty, name: &str) -> Option<Sig> {
    Some(match name {
        "join" => sig(Builtin::TaskJoin, vec![], result_ty(t.clone(), error_ty())),
        "done" => sig(Builtin::TaskDone, vec![], Ty::Bool),
        "try_join" => sig(
            Builtin::TaskTryJoin,
            vec![],
            optional(result_ty(t.clone(), error_ty())),
        ),
        _ => return None,
    })
}

pub(crate) const CHANNEL_METHODS: &[&str] = &["send", "recv", "try_recv", "close", "iter"];

/// The signature of a method of `Channel[T]` (DESIGN §14.1, Q3).
pub(crate) fn channel_sig(t: &Ty, name: &str) -> Option<Sig> {
    Some(match name {
        "send" => sig(Builtin::ChannelSend, vec![("value", t.clone())], Ty::Unit),
        "recv" => sig(Builtin::ChannelRecv, vec![], optional(t.clone())),
        "try_recv" => sig(Builtin::ChannelTryRecv, vec![], optional(t.clone())),
        "close" => sig(Builtin::ChannelClose, vec![], Ty::Unit),
        "iter" => sig(
            Builtin::ChannelIter,
            vec![],
            crate::builtins::iter_of(t.clone()),
        ),
        _ => return None,
    })
}

pub(crate) const SHARED_METHODS: &[&str] = &["get"];

/// The signature of a method of `Shared[T]`.
pub(crate) fn shared_sig(t: &Ty, name: &str) -> Option<Sig> {
    Some(match name {
        "get" => sig(Builtin::SharedGet, vec![], t.clone()),
        _ => return None,
    })
}

pub(crate) const MUTEX_METHODS: &[&str] = &["get", "set", "with"];

/// The signature of a method of `Mutex[T]` (DESIGN §14.1). `with` takes the callback that runs
/// under the lock; `fresh` makes the type of its result.
pub(crate) fn mutex_sig(t: &Ty, name: &str, fresh: &mut dyn FnMut() -> Ty) -> Option<Sig> {
    Some(match name {
        "get" => sig(Builtin::MutexGet, vec![], t.clone()),
        "set" => sig(Builtin::MutexSet, vec![("value", t.clone())], Ty::Unit),
        "with" => {
            let r = fresh();
            sig(
                Builtin::MutexWith,
                vec![("f", crate::builtins::func(vec![t.clone()], r.clone()))],
                r,
            )
        }
        _ => return None,
    })
}

pub(crate) const COGROUP_METHODS: &[&str] = &["go", "cancel", "count", "join_all"];

/// The signature of a method of `CoGroup`. (`go` is a special form, see
/// [`FnCtx::check_group_go`].)
pub(crate) fn cogroup_sig(name: &str) -> Option<Sig> {
    Some(match name {
        "cancel" => sig(Builtin::CoGroupCancel, vec![], Ty::Unit),
        "count" => sig(Builtin::CoGroupCount, vec![], Ty::INT),
        "join_all" => sig(Builtin::CoGroupJoinAll, vec![], Ty::Unit),
        _ => return None,
    })
}

pub(crate) const SIGNAL_METHODS: &[&str] = &["emit"];

/// The signature of a method of `Signal`.
pub(crate) fn signal_sig(name: &str) -> Option<Sig> {
    Some(match name {
        "emit" => sig(Builtin::SignalEmit, vec![], Ty::Unit),
        _ => return None,
    })
}

impl FnCtx<'_, '_> {
    /// The receiver of a built-in method and its type. `r.context(msg)` on a `Result[T, E]`
    /// first converts the error to `Error` (through `IntoError`), so the method itself only
    /// ever sees `Result[T, Error]`.
    pub(crate) fn prepare_receiver(&mut self, recv: Expr, method: &str) -> (Expr, Ty) {
        let ty = self.infer.shallow(&recv.ty);
        if method == "context"
            && let Ty::Adt(RESULT_TYPE, args) = &ty
            && let [_, err] = args.as_slice()
            && !is_error_ty(&self.infer.shallow(err))
        {
            let converted = self.result_into_error(recv);
            let ty = self.infer.shallow(&converted.ty);
            return (converted, ty);
        }
        (recv, ty)
    }

    // ---- constructing results ----

    /// `ok(x)` and `err(e)`. The expected type, when it is a `Result`, says what the other
    /// side is; otherwise that side is inferred later. In a position that expects
    /// `Result[T]` (error type `Error`), `err(x)` converts any `IntoError` value, so
    /// `err("message")` works.
    pub fn check_ok_err(
        &mut self,
        ok: bool,
        args: &[ast::Arg],
        expected: Option<&Ty>,
        span: Span,
    ) -> Expr {
        let word = if ok { "ok" } else { "err" };
        let [arg] = args else {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!("`{word}` takes exactly one value, found {}", args.len()),
                    span,
                )
                .with_hint(format!(
                    "write `{word}(x)`; for a `Result[()]` the value is `ok(())`"
                )),
            );
            return self.error_expr(span);
        };
        if let Some(name) = &arg.name {
            self.check_expr(&arg.value, None);
            self.ck.report(Diagnostic::error(
                format!("`{word}` does not take a named argument"),
                name.span,
            ));
            return self.error_expr(span);
        }
        let (t_hint, e_hint) = match expected.map(|t| self.infer.shallow(t)) {
            Some(Ty::Adt(RESULT_TYPE, a)) if a.len() == 2 => {
                (Some(a[0].clone()), Some(a[1].clone()))
            }
            // Something already reported: do not ask for more inference.
            Some(Ty::Error) => (Some(Ty::Error), Some(Ty::Error)),
            _ => (None, None),
        };
        let payload_hint = if ok { t_hint.clone() } else { e_hint.clone() };
        let mut value = self.check_expr(&arg.value, payload_hint.as_ref());
        if !ok && let Some(e) = &e_hint {
            let wanted = self.infer.shallow(e);
            let given = self.infer.shallow(&value.ty);
            if is_error_ty(&wanted) && !is_error_ty(&given) {
                value = self.convert_error(value);
            }
        }
        let other = match if ok { &e_hint } else { &t_hint } {
            Some(t) => t.clone(),
            None => self.fresh_var(VarKind::General),
        };
        let ty = if ok {
            result_ty(value.ty.clone(), other)
        } else {
            result_ty(other, value.ty.clone())
        };
        let variant = if ok { VARIANT_OK } else { VARIANT_ERR };
        self.mk(
            hir::ExprKind::Variant {
                ty: RESULT_TYPE,
                variant,
                args: vec![value],
            },
            ty,
            span,
        )
    }

    /// The value `ok(())` of type `ret` (the end of a `Result[()]` body, DESIGN §6.1).
    pub fn ok_unit(&self, ret: &Ty, span: Span) -> Expr {
        let unit = self.mk(hir::ExprKind::Lit(hir::Lit::Unit), Ty::Unit, span);
        self.mk(
            hir::ExprKind::Variant {
                ty: RESULT_TYPE,
                variant: VARIANT_OK,
                args: vec![unit],
            },
            ret.clone(),
            span,
        )
    }

    // ---- IntoError ----

    /// How a value of type `ty` converts to `Error`, if it implements `IntoError`.
    fn error_conv(&mut self, ty: &Ty) -> Option<ErrorConv> {
        if let Ty::Adt(id, _) = ty
            && self.ck.types[id.0 as usize].is_open_enum()
        {
            return Some(ErrorConv::OpenEnum);
        }
        match self.ck.env().implements(ty, INTO_ERROR, &[]) {
            Ok(Source::User(id)) => Some(ErrorConv::Impl(id)),
            Ok(Source::Derived) => {
                key_of(ty)?;
                self.ck.impl_of(ty, INTO_ERROR).map(ErrorConv::Impl)
            }
            Ok(Source::Bound) => Some(ErrorConv::Bound),
            Ok(Source::Dyn) | Err(_) => None,
        }
    }

    /// Whether a value of type `ty` converts to `Error` (it is one, or implements
    /// `IntoError`).
    pub(crate) fn converts_to_error(&mut self, ty: &Ty) -> bool {
        let ty = self.default_ty(ty);
        is_error_ty(&ty) || self.error_conv(&ty).is_some()
    }

    fn report_not_into_error(&mut self, ty: &Ty, span: Span) {
        let shown = self.show(ty);
        self.ck.report(
            Diagnostic::error(
                format!(
                    "`{shown}` cannot be converted to `Error`: it does not implement `IntoError`"
                ),
                span,
            )
            .with_hint(format!(
                "add `impl IntoError for {shown} {{}}`; the message then comes from its `Show`"
            )),
        );
    }

    /// Converts `value` to an `Error` that keeps it (DESIGN §10.1). An `Error` is returned
    /// as it is; a type without `IntoError` is reported.
    pub(crate) fn convert_error(&mut self, value: Expr) -> Expr {
        let ty = self.default_ty(&value.ty);
        let span = value.span;
        if is_error_ty(&ty) || matches!(ty, Ty::Error | Ty::Never) {
            return value;
        }
        let Some(conv) = self.error_conv(&ty) else {
            self.report_not_into_error(&ty, span);
            return self.error_expr_typed(span, error_ty());
        };
        self.mk(
            hir::ExprKind::Coerce(Box::new(value), Coercion::IntoError(conv)),
            error_ty(),
            span,
        )
    }

    /// Converts the error of a `Result[T, E]` to `Error`, giving `Result[T, Error]`.
    pub(crate) fn result_into_error(&mut self, value: Expr) -> Expr {
        let ty = self.infer.resolve(&value.ty);
        let span = value.span;
        let Ty::Adt(RESULT_TYPE, args) = &ty else {
            return value;
        };
        let [ok, err] = args.as_slice() else {
            return value;
        };
        let err = self.default_ty(err);
        if is_error_ty(&err) || err.is_error() {
            return value;
        }
        let Some(conv) = self.error_conv(&err) else {
            self.report_not_into_error(&err, span);
            return self.error_expr_typed(span, result_ty(ok.clone(), error_ty()));
        };
        self.mk(
            hir::ExprKind::Coerce(Box::new(value), Coercion::ResultIntoError(conv)),
            result_ty(ok.clone(), error_ty()),
            span,
        )
    }

    /// `Error.from(x)`: the explicit conversion of an `IntoError` value to `Error`.
    pub(crate) fn check_error_from(&mut self, args: Option<&[ast::Arg]>, span: Span) -> Expr {
        let Some([arg]) = args else {
            self.check_args_loosely(args.unwrap_or_default());
            self.ck.report(
                Diagnostic::error("`Error.from` is called with exactly one value", span)
                    .with_hint("write `Error.from(value)`"),
            );
            return self.error_expr(span);
        };
        let value = self.check_expr(&arg.value, None);
        let mut out = self.convert_error(value);
        out.span = span;
        out
    }

    /// `error(msg)`: an `Error` carrying the message, so a script writes
    /// `return err(error("bad input"))`. The same value as `Error.from(msg)` for a `str`.
    pub(crate) fn check_error_fn(&mut self, args: &[ast::Arg], span: Span) -> Expr {
        let [arg] = args else {
            self.check_args_loosely(args);
            self.ck.report(
                Diagnostic::error(
                    format!("`error` takes exactly one message, found {}", args.len()),
                    span,
                )
                .with_hint("write `error(\"what went wrong\")`"),
            );
            return self.error_expr(span);
        };
        if let Some(name) = &arg.name {
            self.check_expr(&arg.value, None);
            self.ck.report(Diagnostic::error(
                "`error` does not take a named argument",
                name.span,
            ));
            return self.error_expr(span);
        }
        let msg = self.check_expr(&arg.value, Some(&Ty::Str));
        if !self.expect(&msg, &Ty::Str, None) {
            return self.error_expr_typed(span, error_ty());
        }
        let mut out = self.convert_error(msg);
        out.span = span;
        out
    }

    // ---- e.is[T]() and e.as[T]() ----

    /// `e.is[T]() -> bool` and `e.as[T]() -> T?` on an `Error`: they look at the runtime type
    /// of the value the error keeps (DESIGN §10.2). `T` must be a concrete type.
    pub(crate) fn check_error_downcast(
        &mut self,
        recv: Expr,
        name: &ast::Ident,
        explicit: Option<Vec<Ty>>,
        args: &[ast::Arg],
        span: Span,
    ) -> Expr {
        let method = name.name.as_str();
        self.check_args_loosely(args);
        let wanted = match explicit.as_deref() {
            Some([t]) if args.is_empty() => self.infer.resolve(t),
            _ => {
                self.ck.report(
                    Diagnostic::error(
                        format!("`{method}` takes one type and no arguments"),
                        name.span,
                    )
                    .with_hint(format!("write `e.{method}[MyError]()`")),
                );
                return self.error_expr(span);
            }
        };
        if wanted.is_error() {
            return self.error_expr(span);
        }
        let key = if wanted.has_param() {
            None
        } else {
            key_of(&wanted)
        };
        let Some(key) = key else {
            let shown = self.show(&wanted);
            self.ck.report(
                Diagnostic::error(
                    format!("`{method}[{shown}]` cannot look for this type"),
                    name.span,
                )
                .with_hint("name a concrete type: a struct, enum, class, `str`, `int` or another built-in type"),
            );
            return self.error_expr(span);
        };
        let (intrinsic, ret) = if method == "is" {
            (Intrinsic::ErrorIs(key), Ty::Bool)
        } else {
            (Intrinsic::ErrorAs(key), optional(wanted.clone()))
        };
        self.finish_call(
            Callee::Intrinsic(intrinsic),
            Some(Receiver::Value(recv)),
            Vec::new(),
            ret,
            vec![wanted],
            span,
        )
    }
}
