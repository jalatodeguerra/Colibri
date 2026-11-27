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
use crate::modules::BASE;
use crate::optres::{
    CHANNEL_METHODS, COGROUP_METHODS, COROUTINE_METHODS, ERROR_METHODS, MUTEX_METHODS,
    OPTION_METHODS, RESULT_METHODS, SHARED_METHODS, SIGNAL_METHODS, TASK_METHODS, channel_sig,
    cogroup_sig, coroutine_sig, error_sig, mutex_sig, option_sig, result_sig, shared_sig,
    signal_sig, task_sig,
};
use crate::suggest;
use crate::types::{
    BYTES_BUILDER_TYPE, CHANNEL_TYPE, COGROUP_TYPE, COROUTINE_TYPE, ERROR_TYPE, ITER, IntTy,
    MUTEX_TYPE, RESULT_TYPE, SHARED_TYPE, SIGNAL_TYPE, STR_BUILDER_TYPE, TASK_TYPE, Ty,
};

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
    "rfind",
    "split_once",
    "rsplit_once",
    "pad_left",
    "pad_right",
    "pad_left_with",
    "pad_right_with",
    "to_bytes",
    "clone",
];
const BYTES_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "get",
    "slice",
    "iter",
    "enumerate",
    "to_list",
    "to_str",
    "to_str_lossy",
    "starts_with",
    "ends_with",
    "find",
    "contains",
    "clone",
];
const BYTES_BUILDER_METHODS: &[&str] = &[
    "push",
    "push_bytes",
    "push_str",
    "len",
    "is_empty",
    "clear",
    "to_bytes",
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
    "zip",
    "flat_map",
    "sorted",
    "sorted_by",
    "dedup",
    "chunks",
    "windows",
    "clone",
];
const MAP_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "get",
    "get_or",
    "contains_key",
    "insert",
    "remove",
    "keys",
    "values",
    "entries",
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
    "union",
    "intersection",
    "difference",
    "symmetric_difference",
    "is_subset",
    "is_superset",
    "is_disjoint",
    "iter",
    "enumerate",
    "clone",
];
const STR_BUILDER_METHODS: &[&str] = &[
    "push",
    "push_char",
    "write",
    "len",
    "is_empty",
    "clear",
    "to_str",
    "clone",
];
const RANGE_METHODS: &[&str] = &[
    "contains",
    "len",
    "is_empty",
    "iter",
    "rev",
    "to_list",
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
        "rfind" => sig(Builtin::StrRFind, vec![("sub", text)], optional(Ty::INT)),
        "split_once" => sig(
            Builtin::StrSplitOnce,
            vec![("sep", text)],
            optional(Ty::Tuple(vec![Ty::Str, Ty::Str])),
        ),
        "rsplit_once" => sig(
            Builtin::StrRSplitOnce,
            vec![("sep", text)],
            optional(Ty::Tuple(vec![Ty::Str, Ty::Str])),
        ),
        "pad_left" => sig(Builtin::StrPadLeft, vec![("width", Ty::INT)], Ty::Str),
        "pad_right" => sig(Builtin::StrPadRight, vec![("width", Ty::INT)], Ty::Str),
        "pad_left_with" => sig(
            Builtin::StrPadLeft,
            vec![("width", Ty::INT), ("fill", Ty::Char)],
            Ty::Str,
        ),
        "pad_right_with" => sig(
            Builtin::StrPadRight,
            vec![("width", Ty::INT), ("fill", Ty::Char)],
            Ty::Str,
        ),
        "to_bytes" => sig(Builtin::StrToBytes, vec![], Ty::Bytes),
        _ => return None,
    })
}

/// The type of one byte.
pub(crate) const BYTE: Ty = Ty::Int(IntTy::U8);

fn bytes_sig(name: &str) -> Option<Sig> {
    let bytes = Ty::Bytes;
    Some(match name {
        "len" => sig(Builtin::BytesLen, vec![], Ty::INT),
        "is_empty" => sig(Builtin::BytesIsEmpty, vec![], Ty::Bool),
        "get" => sig(Builtin::BytesGet, vec![("index", Ty::INT)], optional(BYTE)),
        "slice" => sig(
            Builtin::BytesSliceOf,
            vec![("start", Ty::INT), ("end", Ty::INT)],
            Ty::Bytes,
        ),
        "iter" => sig(Builtin::Iter, vec![], iter_of(BYTE)),
        "to_list" => sig(Builtin::BytesToList, vec![], Ty::List(Box::new(BYTE))),
        "to_str" => sig(
            Builtin::BytesToStr,
            vec![],
            crate::optres::result_ty(Ty::Str, crate::optres::error_ty()),
        ),
        "to_str_lossy" => sig(Builtin::BytesToStrLossy, vec![], Ty::Str),
        "starts_with" => sig(Builtin::BytesStartsWith, vec![("prefix", bytes)], Ty::Bool),
        "ends_with" => sig(Builtin::BytesEndsWith, vec![("suffix", bytes)], Ty::Bool),
        "find" => sig(Builtin::BytesFind, vec![("sub", bytes)], optional(Ty::INT)),
        "contains" => sig(Builtin::BytesContains, vec![("sub", bytes)], Ty::Bool),
        _ => return None,
    })
}

fn bytes_builder_sig(name: &str) -> Option<Sig> {
    Some(match name {
        "push" => sig(Builtin::BytesBuilderPush, vec![("byte", BYTE)], Ty::Unit),
        "push_bytes" => sig(
            Builtin::BytesBuilderPushBytes,
            vec![("bytes", Ty::Bytes)],
            Ty::Unit,
        ),
        "push_str" => sig(Builtin::BytesBuilderPushStr, vec![("s", Ty::Str)], Ty::Unit),
        "len" => sig(Builtin::BytesBuilderLen, vec![], Ty::INT),
        "is_empty" => sig(Builtin::BytesBuilderIsEmpty, vec![], Ty::Bool),
        "clear" => sig(Builtin::BytesBuilderClear, vec![], Ty::Unit),
        "to_bytes" => sig(Builtin::BytesBuilderToBytes, vec![], Ty::Bytes),
        _ => return None,
    })
}

fn str_builder_sig(name: &str) -> Option<Sig> {
    Some(match name {
        "push" => sig(Builtin::StrBuilderPush, vec![("s", Ty::Str)], Ty::Unit),
        "push_char" => sig(Builtin::StrBuilderPushChar, vec![("c", Ty::Char)], Ty::Unit),
        "len" => sig(Builtin::StrBuilderLen, vec![], Ty::INT),
        "is_empty" => sig(Builtin::StrBuilderIsEmpty, vec![], Ty::Bool),
        "clear" => sig(Builtin::StrBuilderClear, vec![], Ty::Unit),
        "to_str" => sig(Builtin::StrBuilderToStr, vec![], Ty::Str),
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
        "to_list" => sig(Builtin::RangeToList, vec![], Ty::List(Box::new(Ty::INT))),
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
        "sorted" => ordered(Builtin::ListSorted, vec![], list),
        "sorted_by" => sig(
            Builtin::ListSortedBy,
            vec![("f", func(vec![t.clone(), t], Ty::INT))],
            list,
        ),
        "zip" => {
            let u = fresh();
            sig(
                Builtin::ListZip,
                vec![("other", Ty::List(Box::new(u.clone())))],
                Ty::List(Box::new(Ty::Tuple(vec![t, u]))),
            )
        }
        "flat_map" => {
            let u = fresh();
            sig(
                Builtin::ListFlatMap,
                vec![("f", func(vec![t], Ty::List(Box::new(u.clone()))))],
                Ty::List(Box::new(u)),
            )
        }
        // Only the prelude may call these (see `check_builtin_method`).
        "__guard" => sig(Builtin::ListGuardCheck, vec![], Ty::Unit),
        "__keep" => sig(
            Builtin::ListKeepMask,
            vec![("mask", Ty::List(Box::new(Ty::Bool)))],
            Ty::Unit,
        ),
        "__reorder" => sig(
            Builtin::ListSetOrder,
            vec![("order", Ty::List(Box::new(Ty::INT)))],
            Ty::Unit,
        ),
        "__pick" => sig(
            Builtin::ListPickOrder,
            vec![("order", Ty::List(Box::new(Ty::INT)))],
            list,
        ),
        "dedup" => Sig {
            need: Need::Eq(t.clone()),
            ..sig(Builtin::ListDedup, vec![], Ty::Unit)
        },
        "chunks" => sig(
            Builtin::ListChunks,
            vec![("size", Ty::INT)],
            Ty::List(Box::new(list)),
        ),
        "windows" => sig(
            Builtin::ListWindows,
            vec![("size", Ty::INT)],
            Ty::List(Box::new(list)),
        ),
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
        "get_or" => sig(
            Builtin::MapGetOr,
            vec![("key", k), ("default", v.clone())],
            v,
        ),
        "entries" => sig(
            Builtin::MapEntries,
            vec![],
            Ty::List(Box::new(Ty::Tuple(vec![k, v]))),
        ),
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
    let set_ty = || Ty::Set(Box::new(elem.clone()));
    let set = set_ty();
    Some(match name {
        "len" => sig(Builtin::SetLen, vec![], Ty::INT),
        "is_empty" => sig(Builtin::SetIsEmpty, vec![], Ty::Bool),
        "add" => sig(Builtin::SetAdd, vec![("item", t)], Ty::Unit),
        "remove" => sig(Builtin::SetRemove, vec![("item", t)], Ty::Bool),
        "contains" => sig(Builtin::SetContains, vec![("item", t)], Ty::Bool),
        "clear" => sig(Builtin::SetClear, vec![], Ty::Unit),
        "union" => sig(Builtin::SetUnion, vec![("other", set)], set_ty()),
        "intersection" => sig(Builtin::SetIntersection, vec![("other", set)], set_ty()),
        "difference" => sig(Builtin::SetDifference, vec![("other", set)], set_ty()),
        "symmetric_difference" => sig(
            Builtin::SetSymmetricDifference,
            vec![("other", set)],
            set_ty(),
        ),
        "is_subset" => sig(Builtin::SetIsSubset, vec![("other", set)], Ty::Bool),
        "is_superset" => sig(Builtin::SetIsSuperset, vec![("other", set)], Ty::Bool),
        "is_disjoint" => sig(Builtin::SetIsDisjoint, vec![("other", set)], Ty::Bool),
        "iter" => sig(Builtin::Iter, vec![], iter_of(t)),
        _ => return None,
    })
}

/// Whether a built-in type has the method `name` (other than `clone`, which every type has).
pub(crate) fn builtin_method_exists(ty: &Ty, name: &str) -> bool {
    let mut fresh = || Ty::Error;
    match ty {
        Ty::Str => str_sig(name).is_some(),
        Ty::Bytes => bytes_sig(name).is_some(),
        Ty::List(t) => list_sig(t, name, &mut fresh).is_some(),
        Ty::Map(k, v) => map_sig(k, v, name).is_some(),
        Ty::Set(t) => set_sig(t, name).is_some(),
        Ty::Range(_) => range_sig(name).is_some(),
        Ty::Option(t) => option_sig(t, name, &mut fresh).is_some(),
        Ty::Adt(RESULT_TYPE, a) if a.len() == 2 => {
            result_sig(&a[0], &a[1], name, &mut fresh).is_some()
        }
        Ty::Adt(ERROR_TYPE, _) => error_sig(name).is_some(),
        Ty::Adt(COROUTINE_TYPE, a) if a.len() == 1 => coroutine_sig(&a[0], name).is_some(),
        Ty::Adt(COGROUP_TYPE, _) => name == "go" || cogroup_sig(name).is_some(),
        Ty::Adt(SIGNAL_TYPE, _) => signal_sig(name).is_some(),
        Ty::Adt(TASK_TYPE, a) if a.len() == 1 => task_sig(&a[0], name).is_some(),
        Ty::Adt(CHANNEL_TYPE, a) if a.len() == 1 => channel_sig(&a[0], name).is_some(),
        Ty::Adt(SHARED_TYPE, a) if a.len() == 1 => shared_sig(&a[0], name).is_some(),
        Ty::Adt(MUTEX_TYPE, a) if a.len() == 1 => mutex_sig(&a[0], name, &mut fresh).is_some(),
        Ty::Adt(STR_BUILDER_TYPE, _) => name == "write" || str_builder_sig(name).is_some(),
        Ty::Adt(BYTES_BUILDER_TYPE, _) => bytes_builder_sig(name).is_some(),
        _ => false,
    }
}

/// The methods of a built-in type with their signatures for this receiver, for editors. A
/// method's own type parameter is shown as `U`. `clone`, which every type has, is not listed.
pub(crate) fn builtin_methods(ty: &Ty) -> Vec<crate::members::MethodSig> {
    let mut count = 0u32;
    let mut fresh = || {
        count += 1;
        Ty::Param(crate::types::ParamId(u32::MAX - count), "U".into())
    };
    let rows: Vec<(&str, Option<Sig>)> = match ty {
        Ty::Str => STR_METHODS.iter().map(|n| (*n, str_sig(n))).collect(),
        Ty::Bytes => BYTES_METHODS.iter().map(|n| (*n, bytes_sig(n))).collect(),
        Ty::List(t) => LIST_METHODS
            .iter()
            .map(|n| (*n, list_sig(t, n, &mut fresh)))
            .collect(),
        Ty::Map(k, v) => MAP_METHODS.iter().map(|n| (*n, map_sig(k, v, n))).collect(),
        Ty::Set(t) => SET_METHODS.iter().map(|n| (*n, set_sig(t, n))).collect(),
        Ty::Range(_) => RANGE_METHODS.iter().map(|n| (*n, range_sig(n))).collect(),
        Ty::Option(t) => OPTION_METHODS
            .iter()
            .map(|n| (*n, option_sig(t, n, &mut fresh)))
            .collect(),
        Ty::Adt(RESULT_TYPE, a) if a.len() == 2 => RESULT_METHODS
            .iter()
            .map(|n| (*n, result_sig(&a[0], &a[1], n, &mut fresh)))
            .collect(),
        Ty::Adt(ERROR_TYPE, _) => ERROR_METHODS.iter().map(|n| (*n, error_sig(n))).collect(),
        Ty::Adt(COROUTINE_TYPE, a) if a.len() == 1 => COROUTINE_METHODS
            .iter()
            .map(|n| (*n, coroutine_sig(&a[0], n)))
            .collect(),
        Ty::Adt(COGROUP_TYPE, _) => COGROUP_METHODS
            .iter()
            .map(|n| (*n, cogroup_sig(n)))
            .collect(),
        Ty::Adt(SIGNAL_TYPE, _) => SIGNAL_METHODS.iter().map(|n| (*n, signal_sig(n))).collect(),
        Ty::Adt(TASK_TYPE, a) if a.len() == 1 => TASK_METHODS
            .iter()
            .map(|n| (*n, task_sig(&a[0], n)))
            .collect(),
        Ty::Adt(CHANNEL_TYPE, a) if a.len() == 1 => CHANNEL_METHODS
            .iter()
            .map(|n| (*n, channel_sig(&a[0], n)))
            .collect(),
        Ty::Adt(SHARED_TYPE, a) if a.len() == 1 => SHARED_METHODS
            .iter()
            .map(|n| (*n, shared_sig(&a[0], n)))
            .collect(),
        Ty::Adt(MUTEX_TYPE, a) if a.len() == 1 => MUTEX_METHODS
            .iter()
            .map(|n| (*n, mutex_sig(&a[0], n, &mut fresh)))
            .collect(),
        Ty::Adt(STR_BUILDER_TYPE, _) => STR_BUILDER_METHODS
            .iter()
            .map(|n| (*n, str_builder_sig(n)))
            .collect(),
        Ty::Adt(BYTES_BUILDER_TYPE, _) => BYTES_BUILDER_METHODS
            .iter()
            .map(|n| (*n, bytes_builder_sig(n)))
            .collect(),
        _ => Vec::new(),
    };
    rows.into_iter()
        .filter_map(|(name, sig)| {
            let sig = sig?;
            Some(crate::members::MethodSig {
                name: name.to_string(),
                params: sig
                    .params
                    .iter()
                    .map(|(n, t)| (n.to_string(), t.clone()))
                    .collect(),
                ret: sig.ret,
            })
        })
        .collect()
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
            Ty::Bytes => (bytes_sig(&name.name), BYTES_METHODS),
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
            Ty::Adt(COROUTINE_TYPE, a) if a.len() == 1 => {
                (coroutine_sig(&a[0], &name.name), COROUTINE_METHODS)
            }
            Ty::Adt(COGROUP_TYPE, _) => (cogroup_sig(&name.name), COGROUP_METHODS),
            Ty::Adt(SIGNAL_TYPE, _) => (signal_sig(&name.name), SIGNAL_METHODS),
            Ty::Adt(TASK_TYPE, a) if a.len() == 1 => (task_sig(&a[0], &name.name), TASK_METHODS),
            Ty::Adt(CHANNEL_TYPE, a) if a.len() == 1 => {
                (channel_sig(&a[0], &name.name), CHANNEL_METHODS)
            }
            Ty::Adt(SHARED_TYPE, a) if a.len() == 1 => {
                (shared_sig(&a[0], &name.name), SHARED_METHODS)
            }
            Ty::Adt(MUTEX_TYPE, a) if a.len() == 1 => {
                (mutex_sig(&a[0], &name.name, &mut fresh), MUTEX_METHODS)
            }
            Ty::Adt(STR_BUILDER_TYPE, _) => {
                if name.name == "write" {
                    return Ok(self.check_str_builder_write(recv, args, span));
                }
                (str_builder_sig(&name.name), STR_BUILDER_METHODS)
            }
            Ty::Adt(BYTES_BUILDER_TYPE, _) => {
                (bytes_builder_sig(&name.name), BYTES_BUILDER_METHODS)
            }
            _ => return Err(recv),
        };
        // The hidden helpers of the prelude are no methods of a program's lists.
        let sig =
            sig.filter(|_| !name.name.starts_with("__") || crate::prelude::in_prelude(name.span));
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
                module: BASE,
            })
            .collect();
        let full = format!("{}.{}", self.show(&recv_ty), name.name);
        let Some(bound) = self.bind_args(&full, &params, args, span, 1) else {
            return Ok(self.error_expr_typed(span, sig.ret));
        };
        if !self.check_need(&sig.need, &name.name, name.span, span) {
            return Ok(self.error_expr_typed(span, sig.ret));
        }
        if sig.op == Builtin::ChannelSend
            && let Some(sent) = bound.first()
        {
            self.note_channel_send(&sent.expr);
            self.note_thread_boundary(&sent.expr);
        }
        if sig.op == Builtin::MutexSet
            && let Some(set) = bound.first()
        {
            self.note_thread_boundary(&set.expr);
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
                if matches!(t, Ty::Int(crate::types::IntTy::I64) | Ty::Float | Ty::Error) {
                    return true;
                }
                let shown = self.show(&t);
                self.ck.report(
                    Diagnostic::error(
                        format!("`sum` needs a list of `int` or `float`, but this is a list of `{shown}`"),
                        at,
                    )
                    .with_hint("add the elements yourself with `fold`, or map them to `int` or `float` first"),
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

    /// `b.write(v)` on a `StrBuilder`: appends the `Show` text of any value, as the hole of a
    /// string interpolation would.
    fn check_str_builder_write(&mut self, recv: Expr, args: &[ast::Arg], span: Span) -> Expr {
        let [arg] = args else {
            self.check_args_loosely(args);
            self.ck.report(Diagnostic::error(
                format!("`write` takes one value, found {} arguments", args.len()),
                span,
            ));
            return self.error_expr(span);
        };
        let text = self.check_hole(&arg.value, None);
        if text.ty.is_error() {
            return self.error_expr(span);
        }
        self.builtin_call(Builtin::StrBuilderPush, vec![recv, text], Ty::Unit, span)
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
