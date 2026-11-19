//! Methods of the built-in types: `str`, `List`, `Map`, `Set`, and `clone` on everything.
//!
//! Each method is described by a signature built from the receiver's type arguments, so the
//! ordinary argument checking and inference apply: `let v = []; v.push(1)` binds the element
//! type of `v` through the parameter of `push`. User generics will reuse the same path.

use colibri_syntax::ast;
use colibri_syntax::{Diagnostic, Span};

use crate::body::FnCtx;
use crate::call::Receiver;
use crate::checker::ParamInfo;
use crate::hir::{self, Builtin, Callee, Expr};
use crate::suggest;
use crate::types::Ty;

/// A method of a built-in type with its types filled in for one receiver.
struct Sig {
    op: Builtin,
    params: Vec<(&'static str, Ty)>,
    ret: Ty,
    /// The element type that must support `==` (for `contains` and `index_of`).
    needs_eq: Option<Ty>,
}

fn sig(op: Builtin, params: Vec<(&'static str, Ty)>, ret: Ty) -> Sig {
    Sig {
        op,
        params,
        ret,
        needs_eq: None,
    }
}

fn optional(t: Ty) -> Ty {
    Ty::Option(Box::new(t))
}

/// Collection methods that take a closure; closures are not supported yet.
const CLOSURE_METHODS: &[&str] = &[
    "map", "filter", "fold", "reduce", "find", "any", "all", "retain", "for_each", "sort_by",
    "flat_map", "position",
];
const STR_METHODS: &[&str] = &["len", "is_empty", "chars", "clone"];
const LIST_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "push",
    "pop",
    "insert",
    "remove",
    "clear",
    "contains",
    "index_of",
    "first",
    "last",
    "reverse",
    "extend",
    "get",
    "enumerate",
    "clone",
];
const MAP_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "get",
    "contains_key",
    "insert",
    "remove",
    "keys",
    "values",
    "clear",
    "clone",
];
const SET_METHODS: &[&str] = &[
    "len", "is_empty", "add", "remove", "contains", "clear", "clone",
];

fn str_sig(name: &str) -> Option<Sig> {
    Some(match name {
        "len" => sig(Builtin::StrLen, vec![], Ty::INT),
        "is_empty" => sig(Builtin::StrIsEmpty, vec![], Ty::Bool),
        _ => return None,
    })
}

fn list_sig(elem: &Ty, name: &str) -> Option<Sig> {
    let t = elem.clone();
    let list = Ty::List(Box::new(t.clone()));
    Some(match name {
        "len" => sig(Builtin::ListLen, vec![], Ty::INT),
        "is_empty" => sig(Builtin::ListIsEmpty, vec![], Ty::Bool),
        "push" => sig(Builtin::ListPush, vec![("item", t)], Ty::Unit),
        "pop" => sig(Builtin::ListPop, vec![], optional(t)),
        "insert" => sig(
            Builtin::ListInsert,
            vec![("index", Ty::INT), ("item", t)],
            Ty::Unit,
        ),
        "remove" => sig(Builtin::ListRemove, vec![("index", Ty::INT)], t),
        "clear" => sig(Builtin::ListClear, vec![], Ty::Unit),
        "contains" => Sig {
            needs_eq: Some(t.clone()),
            ..sig(Builtin::ListContains, vec![("item", t)], Ty::Bool)
        },
        "index_of" => Sig {
            needs_eq: Some(t.clone()),
            ..sig(Builtin::ListIndexOf, vec![("item", t)], optional(Ty::INT))
        },
        "first" => sig(Builtin::ListFirst, vec![], optional(t)),
        "last" => sig(Builtin::ListLast, vec![], optional(t)),
        "reverse" => sig(Builtin::ListReverse, vec![], Ty::Unit),
        "extend" => sig(Builtin::ListExtend, vec![("other", list)], Ty::Unit),
        "get" => sig(Builtin::ListGet, vec![("index", Ty::INT)], optional(t)),
        _ => return None,
    })
}

fn map_sig(key: &Ty, value: &Ty, name: &str) -> Option<Sig> {
    let (k, v) = (key.clone(), value.clone());
    Some(match name {
        "len" => sig(Builtin::MapLen, vec![], Ty::INT),
        "is_empty" => sig(Builtin::MapIsEmpty, vec![], Ty::Bool),
        "get" => sig(Builtin::MapGet, vec![("key", k)], optional(v)),
        "contains_key" => sig(Builtin::MapContainsKey, vec![("key", k)], Ty::Bool),
        "insert" => sig(Builtin::MapInsert, vec![("key", k), ("value", v)], Ty::Unit),
        "remove" => sig(Builtin::MapRemove, vec![("key", k)], optional(v)),
        "keys" => sig(Builtin::MapKeys, vec![], Ty::List(Box::new(k))),
        "values" => sig(Builtin::MapValues, vec![], Ty::List(Box::new(v))),
        "clear" => sig(Builtin::MapClear, vec![], Ty::Unit),
        _ => return None,
    })
}

fn set_sig(elem: &Ty, name: &str) -> Option<Sig> {
    let t = elem.clone();
    Some(match name {
        "len" => sig(Builtin::SetLen, vec![], Ty::INT),
        "is_empty" => sig(Builtin::SetIsEmpty, vec![], Ty::Bool),
        "add" => sig(Builtin::SetAdd, vec![("item", t)], Ty::Unit),
        "remove" => sig(Builtin::SetRemove, vec![("item", t)], Ty::Bool),
        "contains" => sig(Builtin::SetContains, vec![("item", t)], Ty::Bool),
        "clear" => sig(Builtin::SetClear, vec![], Ty::Unit),
        _ => return None,
    })
}

impl FnCtx<'_, '_> {
    /// `recv.name(args)` where `recv` is not a user-defined type. `Err` gives the receiver back
    /// when its type has no built-in methods at all (the caller reports that).
    pub fn check_builtin_method(
        &mut self,
        recv: Expr,
        name: &ast::Ident,
        args: &[ast::Arg],
        span: Span,
    ) -> Result<Expr, Expr> {
        let recv_ty = self.infer.shallow(&recv.ty);
        if name.name == "clone" {
            return Ok(self.check_clone(recv, args, span));
        }
        let (sig, candidates) = match &recv_ty {
            Ty::Str => (str_sig(&name.name), STR_METHODS),
            Ty::List(t) => (list_sig(t, &name.name), LIST_METHODS),
            Ty::Map(k, v) => (map_sig(k, v, &name.name), MAP_METHODS),
            Ty::Set(t) => (set_sig(t, &name.name), SET_METHODS),
            _ => return Err(recv),
        };
        let Some(sig) = sig else {
            self.check_args_loosely(args);
            if CLOSURE_METHODS.contains(&name.name.as_str()) && !matches!(recv_ty, Ty::Str) {
                self.ck.unsupported(
                    &format!("the method `{}`, which takes a closure,", name.name),
                    name.span,
                );
                return Ok(self.error_expr(span));
            }
            let shown = self.show(&recv_ty);
            let diag = Diagnostic::error(
                format!("`{shown}` has no method `{}`", name.name),
                name.span,
            );
            let hint = match name.name.as_str() {
                "enumerate" | "chars" => Some(format!(
                    "`{}()` can only be used directly as the iterable of a `for` loop for now: `for x in value.{}() {{ ... }}`",
                    name.name, name.name
                )),
                "push_back" | "append" | "add" => {
                    Some("use `push` to add to the end of a list".to_string())
                }
                _ => suggest::closest(&name.name, candidates.iter().copied())
                    .map(|s| format!("did you mean `{s}`?")),
            };
            let hint =
                hint.unwrap_or_else(|| format!("its methods are: {}", candidates.join(", ")));
            self.ck.report(diag.with_hint(hint));
            return Ok(self.error_expr(span));
        };
        if let Some(elem) = &sig.needs_eq
            && !self.supports_eq(elem)
        {
            self.check_args_loosely(args);
            let shown = self.show(elem);
            self.ck.report(
                Diagnostic::error(
                    format!(
                        "`{}` needs elements that can be compared with `==`, but `{shown}` cannot",
                        name.name
                    ),
                    name.span,
                )
                .with_hint("compare the parts you care about instead"),
            );
            return Ok(self.error_expr(span));
        }
        let params: Vec<ParamInfo<'static>> = sig
            .params
            .iter()
            .map(|(pname, ty)| ParamInfo {
                name: (*pname).to_string(),
                ty: ty.clone(),
                span: name.span,
                default: None,
                mutable: false,
            })
            .collect();
        let full = format!("{}.{}", self.show(&recv_ty), name.name);
        let Some(bound) = self.bind_args(&full, &params, args, span, 1) else {
            return Ok(self.error_expr_typed(span, sig.ret));
        };
        Ok(self.finish_call(
            Callee::builtin(sig.op),
            Some(Receiver::Value(recv)),
            bound,
            sig.ret,
            span,
        ))
    }

    /// `x.clone()`: a deep copy of any value (DESIGN §5.3, §7.4).
    pub(crate) fn check_clone(&mut self, recv: Expr, args: &[ast::Arg], span: Span) -> Expr {
        if !args.is_empty() {
            self.check_args_loosely(args);
            self.ck.report(Diagnostic::error(
                format!("`clone` takes no arguments, found {}", args.len()),
                span,
            ));
            return self.error_expr(span);
        }
        let ty = recv.ty.clone();
        let call = hir::ExprKind::Call(Box::new(hir::Call {
            callee: Callee::builtin(Builtin::Clone),
            type_args: Vec::new(),
            args: vec![hir::Arg::Value(recv)],
        }));
        self.mk(call, ty, span)
    }
}
