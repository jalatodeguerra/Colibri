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
use crate::optres::{
    ERROR_METHODS, OPTION_METHODS, RESULT_METHODS, error_sig, option_sig, result_sig,
};
use crate::suggest;
use crate::types::{ERROR_TYPE, ITER, RESULT_TYPE, Ty};

/// What a method needs of the element type beyond its signature.
#[derive(Clone)]
enum Need {
    Nothing,
    /// `==` on the element (`contains`, `index_of`).
    Eq(Ty),
    /// `<` on the element (`sort`, `min`, `max`, and the key of `sort_by_key`).
    Ord(Ty),
    /// A number (`sum`).
    Number(Ty),
    /// Strings (`join`).
    Text(Ty),
}

/// A method of a built-in type with its types filled in for one receiver.
pub(crate) struct Sig {
    pub op: Builtin,
    pub params: Vec<(&'static str, Ty)>,
    pub ret: Ty,
    need: Need,
}

pub(crate) fn sig(op: Builtin, params: Vec<(&'static str, Ty)>, ret: Ty) -> Sig {
    Sig {
        op,
        params,
        ret,
        need: Need::Nothing,
    }
}

pub(crate) fn optional(t: Ty) -> Ty {
    Ty::Option(Box::new(t))
}

pub(crate) fn func(params: Vec<Ty>, ret: Ty) -> Ty {
    Ty::Fn(params, Box::new(ret))
}

/// `Iter[t]`: what `iter()`, `chars()` and friends give.
pub(crate) fn iter_of(t: Ty) -> Ty {
    Ty::Dyn(ITER, vec![t])
}

/// Makes a fresh type for a method's own type parameter (`map[U]`).
pub(crate) type Fresh<'a> = &'a mut dyn FnMut() -> Ty;

const STR_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "chars",
    "bytes",
    "contains",
    "starts_with",
    "ends_with",
    "split",
    "trim",
    "to_upper",
    "to_lower",
    "replace",
    "find",
    "clone",
];
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
    "iter",
    "enumerate",
    "map",
    "filter",
    "retain",
    "any",
    "all",
    "find",
    "find_index",
    "fold",
    "for_each",
    "sort",
    "sort_by",
    "sort_by_key",
    "min",
    "max",
    "sum",
    "join",
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
    "iter",
    "enumerate",
    "clone",
];
const SET_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "add",
    "remove",
    "contains",
    "clear",
    "iter",
    "enumerate",
    "clone",
];
const RANGE_METHODS: &[&str] = &[
    "contains",
    "len",
    "is_empty",
    "iter",
    "rev",
    "enumerate",
    "clone",
];

fn str_sig(name: &str) -> Option<Sig> {
    let text = Ty::Str;
    Some(match name {
        "len" => sig(Builtin::StrLen, vec![], Ty::INT),
        "is_empty" => sig(Builtin::StrIsEmpty, vec![], Ty::Bool),
        "chars" => sig(Builtin::StrChars, vec![], iter_of(Ty::Char)),
        "bytes" => sig(Builtin::StrBytes, vec![], iter_of(Ty::INT)),
        "contains" => sig(Builtin::StrContains, vec![("sub", text)], Ty::Bool),
        "starts_with" => sig(Builtin::StrStartsWith, vec![("prefix", text)], Ty::Bool),
        "ends_with" => sig(Builtin::StrEndsWith, vec![("suffix", text)], Ty::Bool),
        "split" => sig(
            Builtin::StrSplit,
            vec![("sep", text)],
            Ty::List(Box::new(Ty::Str)),
        ),
        "trim" => sig(Builtin::StrTrim, vec![], Ty::Str),
        "to_upper" => sig(Builtin::StrToUpper, vec![], Ty::Str),
        "to_lower" => sig(Builtin::StrToLower, vec![], Ty::Str),
        "replace" => sig(
            Builtin::StrReplace,
            vec![("from", Ty::Str), ("to", Ty::Str)],
            Ty::Str,
        ),
        "find" => sig(Builtin::StrFind, vec![("sub", text)], optional(Ty::INT)),
        _ => return None,
    })
}

fn range_sig(name: &str) -> Option<Sig> {
    Some(match name {
        "contains" => sig(Builtin::RangeContains, vec![("value", Ty::INT)], Ty::Bool),
        "len" => sig(Builtin::RangeLen, vec![], Ty::INT),
        "is_empty" => sig(Builtin::RangeIsEmpty, vec![], Ty::Bool),
        "iter" => sig(Builtin::Iter, vec![], iter_of(Ty::INT)),
        "rev" => sig(Builtin::RangeRev, vec![], iter_of(Ty::INT)),
        _ => return None,
    })
}

fn list_sig(elem: &Ty, name: &str, fresh: Fresh<'_>) -> Option<Sig> {
    let t = elem.clone();
    let list = Ty::List(Box::new(t.clone()));
    let ordered = |op, params, ret| Sig {
        need: Need::Ord(t.clone()),
        ..sig(op, params, ret)
    };
    Some(match name {
        "iter" => sig(Builtin::Iter, vec![], iter_of(t)),
        "map" => {
            let u = fresh();
            sig(
                Builtin::ListMap,
                vec![("f", func(vec![t], u.clone()))],
                Ty::List(Box::new(u)),
            )
        }
        "filter" => sig(
            Builtin::ListFilter,
            vec![("f", func(vec![t], Ty::Bool))],
            list,
        ),
        "retain" => sig(
            Builtin::ListRetain,
            vec![("f", func(vec![t], Ty::Bool))],
            Ty::Unit,
        ),
        "any" => sig(
            Builtin::ListAny,
            vec![("f", func(vec![t], Ty::Bool))],
            Ty::Bool,
        ),
        "all" => sig(
            Builtin::ListAll,
            vec![("f", func(vec![t], Ty::Bool))],
            Ty::Bool,
        ),
        "find" => sig(
            Builtin::ListFind,
            vec![("f", func(vec![t.clone()], Ty::Bool))],
            optional(t),
        ),
        "find_index" => sig(
            Builtin::ListFindIndex,
            vec![("f", func(vec![t], Ty::Bool))],
            optional(Ty::INT),
        ),
        "fold" => {
            let u = fresh();
            sig(
                Builtin::ListFold,
                vec![
                    ("init", u.clone()),
                    ("f", func(vec![u.clone(), t], u.clone())),
                ],
                u,
            )
        }
        "for_each" => sig(
            Builtin::ListForEach,
            vec![("f", func(vec![t], Ty::Unit))],
            Ty::Unit,
        ),
        "sort" => ordered(Builtin::ListSort, vec![], Ty::Unit),
        "sort_by" => sig(
            Builtin::ListSortBy,
            vec![("f", func(vec![t.clone(), t], Ty::INT))],
            Ty::Unit,
        ),
        "sort_by_key" => {
            let k = fresh();
            Sig {
                need: Need::Ord(k.clone()),
                ..sig(
                    Builtin::ListSortByKey,
                    vec![("f", func(vec![t], k))],
                    Ty::Unit,
                )
            }
        }
        "min" => ordered(Builtin::ListMin, vec![], optional(t.clone())),
        "max" => ordered(Builtin::ListMax, vec![], optional(t.clone())),
        "sum" => Sig {
            need: Need::Number(t.clone()),
            ..sig(Builtin::ListSum, vec![], t)
        },
        "join" => Sig {
            need: Need::Text(t),
            ..sig(Builtin::ListJoin, vec![("sep", Ty::Str)], Ty::Str)
        },
        _ => return list_basic_sig(elem, name),
    })
}

fn list_basic_sig(elem: &Ty, name: &str) -> Option<Sig> {
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
            need: Need::Eq(t.clone()),
            ..sig(Builtin::ListContains, vec![("item", t)], Ty::Bool)
        },
        "index_of" => Sig {
            need: Need::Eq(t.clone()),
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
        "iter" => sig(
            Builtin::Iter,
            vec![],
            iter_of(Ty::Tuple(vec![key.clone(), value.clone()])),
        ),
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
        "iter" => sig(Builtin::Iter, vec![], iter_of(t)),
        _ => return None,
    })
}

/// Whether a built-in type has the method `name` (other than `clone`, which every type has).
pub(crate) fn builtin_method_exists(ty: &Ty, name: &str) -> bool {
    let mut fresh = || Ty::Error;
    match ty {
        Ty::Str => str_sig(name).is_some(),
        Ty::List(t) => list_sig(t, name, &mut fresh).is_some(),
        Ty::Map(k, v) => map_sig(k, v, name).is_some(),
        Ty::Set(t) => set_sig(t, name).is_some(),
        Ty::Range(_) => range_sig(name).is_some(),
        Ty::Option(t) => option_sig(t, name, &mut fresh).is_some(),
        Ty::Adt(RESULT_TYPE, a) if a.len() == 2 => {
            result_sig(&a[0], &a[1], name, &mut fresh).is_some()
        }
        Ty::Adt(ERROR_TYPE, _) => error_sig(name).is_some(),
        _ => false,
    }
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
        let (recv, recv_ty) = self.prepare_receiver(recv, &name.name);
        if name.name == "clone" {
            return Ok(self.check_clone(recv, args, span));
        }
        let mut fresh = || self.infer.fresh(crate::infer::VarKind::General);
        let (sig, candidates) = match &recv_ty {
            Ty::Str => (str_sig(&name.name), STR_METHODS),
            Ty::List(t) => (list_sig(t, &name.name, &mut fresh), LIST_METHODS),
            Ty::Map(k, v) => (map_sig(k, v, &name.name), MAP_METHODS),
            Ty::Set(t) => (set_sig(t, &name.name), SET_METHODS),
            Ty::Range(_) => (range_sig(&name.name), RANGE_METHODS),
            Ty::Option(t) => (option_sig(t, &name.name, &mut fresh), OPTION_METHODS),
            Ty::Adt(RESULT_TYPE, a) if a.len() == 2 => (
                result_sig(&a[0], &a[1], &name.name, &mut fresh),
                RESULT_METHODS,
            ),
            Ty::Adt(ERROR_TYPE, _) => (error_sig(&name.name), ERROR_METHODS),
            _ => return Err(recv),
        };
        let Some(sig) = sig else {
            if name.name == "enumerate" {
                return Ok(self.check_enumerate(recv, args, span));
            }
            self.check_args_loosely(args);
            let shown = self.show(&recv_ty);
            let diag = Diagnostic::error(
                format!("`{shown}` has no method `{}`", name.name),
                name.span,
            );
            let hint = match name.name.as_str() {
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
        if let Need::Eq(elem) = &sig.need
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
        if let Need::Ord(elem) = &sig.need
            && matches!(recv_ty, Ty::List(_))
            && name.name != "sort_by_key"
            && !self.supports_ord(elem)
        {
            self.check_args_loosely(args);
            self.report_not_ordered(&name.name, elem, name.span);
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
        if !self.check_need(&sig.need, &name.name, name.span, span) {
            return Ok(self.error_expr_typed(span, sig.ret));
        }
        Ok(self.finish_call(
            Callee::builtin(sig.op),
            Some(Receiver::Value(recv)),
            bound,
            sig.ret,
            Vec::new(),
            span,
        ))
    }

    /// The requirements a method makes of its element (or key) type that can only be decided
    /// once the arguments are checked. `false` after reporting.
    fn check_need(&mut self, need: &Need, method: &str, at: Span, whole: Span) -> bool {
        match need {
            Need::Nothing | Need::Eq(_) => true,
            Need::Ord(ty) => {
                let key = self.infer.shallow(ty);
                if matches!(key, Ty::Infer(_)) && self.unbound_is_open(&key) {
                    // Decided at the end of the body, when the key type is known.
                    self.obligations.push(crate::obligations::Obligation {
                        ty: ty.clone(),
                        iface: crate::types::ORD,
                        args: Vec::new(),
                        span: whole,
                        origin: crate::obligations::Origin::SortKey,
                    });
                    return true;
                }
                if self.supports_ord(ty) {
                    true
                } else {
                    self.report_not_ordered(method, ty, at);
                    false
                }
            }
            Need::Number(ty) => {
                let t = self.default_ty(ty);
                if matches!(t, Ty::Int(_) | Ty::Float | Ty::Error) {
                    return true;
                }
                let shown = self.show(&t);
                self.ck.report(
                    Diagnostic::error(
                        format!("`sum` needs a list of `int` or `float`, but this is a list of `{shown}`"),
                        at,
                    )
                    .with_hint("add the elements yourself with `fold`, or map them to numbers first"),
                );
                false
            }
            Need::Text(ty) => {
                let t = self.infer.shallow(ty);
                if matches!(t, Ty::Str | Ty::Error) || self.infer.unify(&t, &Ty::Str).is_ok() {
                    return true;
                }
                let shown = self.show(&t);
                self.ck.report(
                    Diagnostic::error(
                        format!("`join` needs a list of `str`, but this is a list of `{shown}`"),
                        at,
                    )
                    .with_hint("turn the elements into strings first: `xs.map(fn(x) => \"{x}\").join(\", \")`"),
                );
                false
            }
        }
    }

    fn unbound_is_open(&self, ty: &Ty) -> bool {
        matches!(
            self.infer.unbound_kind(ty),
            Some(crate::infer::VarKind::General)
        )
    }

    fn report_not_ordered(&mut self, method: &str, elem: &Ty, at: Span) {
        let shown = self.show(elem);
        let mut diag = Diagnostic::error(
            format!("`{method}` needs values that can be ordered with `<`, but `{shown}` cannot"),
            at,
        );
        if let Some(hint) = self.comparison_hint(elem, false) {
            diag = diag.with_hint(hint);
        }
        self.ck.report(diag);
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
