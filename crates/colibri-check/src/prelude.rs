//! The prelude: the built-in interfaces of DESIGN §9.2, written in Colibri.
//!
//! The text is parsed once and its interfaces are declared before the user's file, so their
//! type ids are the constants of [`crate::types`] (`EQ`, `ORD`, ...). Their implementations
//! for built-in types are derived by the checker (§7.4), not written here.
//!
//! `Add`, `Sub`, `Mul` and `Div` take the right-hand type `R` and give `O`, both defaulting to
//! `Self`; `Neg` is `Self` to `Self`. The checker resolves `a + b` to the impl itself when it
//! can, see `crate::operators`.
//!
//! `Ord.cmp` returns an `int`: negative, zero or positive (ARCHITECTURE §6). `IntoError` is
//! the interface of types that convert to `Error` (DESIGN §10.1); its default `message` is the
//! type's `Show`. `Error` itself is declared as an empty class so that the checker treats it
//! as the reference type it is; it has no fields to build and its methods are built in
//! (`crate::optres`).

use std::sync::OnceLock;

use colibri_syntax::ast::File;

use crate::hir::Builtin;
use colibri_syntax::{FileId, Span};

/// The source file id of the prelude; its spans never point into a user's file, and
/// diagnostics never label them.
pub(crate) const PRELUDE_FILE: FileId = FileId(u32::MAX);

const SOURCE: &str = r#"
interface Eq {
    fn eq(self, other: Self) -> bool
}
interface Ord {
    fn cmp(self, other: Self) -> int
}
interface Hash {
    fn hash(self) -> int
}
interface Show {
    fn show(self) -> str
}
interface Clone {
    fn clone(self) -> Self
}
interface Drop {
    fn drop(self)
}
interface Iter[T] {
    fn next(mut self) -> T?
}
interface Iterable[T] {
    fn iter(self) -> Iter[T]
}
interface IntoError {
    fn message(self) -> str => "{self}"
}
class Error {}
class Coroutine[T] {}
class CoGroup {}
class Signal {}
class StrBuilder {}
class BytesBuilder {}
interface Add[R = Self, O = Self] {
    fn add(self, other: R) -> O
}
interface Sub[R = Self, O = Self] {
    fn sub(self, other: R) -> O
}
interface Mul[R = Self, O = Self] {
    fn mul(self, other: R) -> O
}
interface Div[R = Self, O = Self] {
    fn div(self, other: R) -> O
}
interface Neg {
    fn neg(self) -> Self
}
"#;

/// The built-ins that take a closure, written in Colibri (vm-plan §2.9): the bytecode VM calls
/// these instead of running the native versions, so a closure runs as ordinary bytecode and may
/// pause. The tree-walker keeps its native versions. Each function does what the native one
/// does, in the same order (a sort compares in the order of core's `merge_order`), and holds
/// the list as a `for` does, so the guards and their messages are the same. The `__` methods
/// of lists are built-ins only this text may call. Their names cannot be written in a program
/// that does not mean to.
const FN_SOURCE: &str = r#"
fn __list_map[T, U](xs: List[T], f: fn(T) -> U) -> List[U] {
    let mut out: List[U] = []
    for x in xs {
        out.push(f(x))
    }
    return out
}
fn __list_filter[T](xs: List[T], f: fn(T) -> bool) -> List[T] {
    let mut out: List[T] = []
    for x in xs {
        if f(x) {
            out.push(x)
        }
    }
    return out
}
fn __list_retain[T](xs: List[T], f: fn(T) -> bool) {
    xs.__guard()
    let mut keep: List[bool] = []
    for x in xs {
        keep.push(f(x))
    }
    xs.__keep(keep)
}
fn __list_any[T](xs: List[T], f: fn(T) -> bool) -> bool {
    for x in xs {
        if f(x) {
            return true
        }
    }
    return false
}
fn __list_all[T](xs: List[T], f: fn(T) -> bool) -> bool {
    for x in xs {
        if !f(x) {
            return false
        }
    }
    return true
}
fn __list_find[T](xs: List[T], f: fn(T) -> bool) -> T? {
    for x in xs {
        if f(x) {
            return some(x)
        }
    }
    return none
}
fn __list_find_index[T](xs: List[T], f: fn(T) -> bool) -> int? {
    let mut i = 0
    for x in xs {
        if f(x) {
            return some(i)
        }
        i += 1
    }
    return none
}
fn __list_fold[T, U](xs: List[T], init: U, f: fn(U, T) -> U) -> U {
    let mut acc = init
    for x in xs {
        acc = f(acc, x)
    }
    return acc
}
fn __list_for_each[T](xs: List[T], f: fn(T)) {
    for x in xs {
        f(x)
    }
}
fn __list_flat_map[T, U](xs: List[T], f: fn(T) -> List[U]) -> List[U] {
    let mut out: List[U] = []
    for x in xs {
        // Not `extend`: that charges a step for each element of `out`, the native one does not.
        for y in f(x) {
            out.push(y)
        }
    }
    return out
}
// The loop of core's `merge_order`: runs of `width` elements are merged, the right element going
// first only when the left is strictly greater, so the comparator is asked in the same order.
fn __merge_order[T](items: List[T], f: fn(T, T) -> int) -> List[int] {
    let n = items.len()
    let mut src: List[int] = []
    let mut dst: List[int] = []
    let mut p = 0
    while p < n {
        src.push(p)
        dst.push(p)
        p += 1
    }
    let mut width = 1
    while width < n {
        let mut start = 0
        while start < n {
            let mid = if start + width < n { start + width } else { n }
            let end = if start + 2 * width < n { start + 2 * width } else { n }
            let mut i = start
            let mut j = mid
            let mut k = start
            while i < mid && j < end {
                if f(items[src[i]], items[src[j]]) > 0 {
                    dst[k] = src[j]
                    j += 1
                } else {
                    dst[k] = src[i]
                    i += 1
                }
                k += 1
            }
            while i < mid {
                dst[k] = src[i]
                i += 1
                k += 1
            }
            while j < end {
                dst[k] = src[j]
                j += 1
                k += 1
            }
            start += 2 * width
        }
        let t = src
        src = dst
        dst = t
        width *= 2
    }
    return src
}
// The same loop, ordering by `compare` (which panics on a `nan`) with no call in between.
fn __merge_order_keys[K: Ord](items: List[K]) -> List[int] {
    let n = items.len()
    let mut src: List[int] = []
    let mut dst: List[int] = []
    let mut p = 0
    while p < n {
        src.push(p)
        dst.push(p)
        p += 1
    }
    let mut width = 1
    while width < n {
        let mut start = 0
        while start < n {
            let mid = if start + width < n { start + width } else { n }
            let end = if start + 2 * width < n { start + 2 * width } else { n }
            let mut i = start
            let mut j = mid
            let mut k = start
            while i < mid && j < end {
                if compare(items[src[i]], items[src[j]]) > 0 {
                    dst[k] = src[j]
                    j += 1
                } else {
                    dst[k] = src[i]
                    i += 1
                }
                k += 1
            }
            while i < mid {
                dst[k] = src[i]
                i += 1
                k += 1
            }
            while j < end {
                dst[k] = src[j]
                j += 1
                k += 1
            }
            start += 2 * width
        }
        let t = src
        src = dst
        dst = t
        width *= 2
    }
    return src
}
fn __sort_order[T](xs: List[T], f: fn(T, T) -> int) -> List[int] {
    let mut order: List[int] = []
    for _ in xs {
        order = __merge_order(xs, f)
        break
    }
    return order
}
fn __key_order[T, K: Ord](xs: List[T], f: fn(T) -> K) -> List[int] {
    let mut keys: List[K] = []
    for x in xs {
        keys.push(f(x))
    }
    let mut order: List[int] = []
    for _ in xs {
        order = __merge_order_keys(keys)
        break
    }
    return order
}
fn __list_sort_by[T](xs: List[T], f: fn(T, T) -> int) {
    xs.__guard()
    let order = __sort_order(xs, f)
    xs.__reorder(order)
}
fn __list_sort_by_key[T, K: Ord](xs: List[T], f: fn(T) -> K) {
    xs.__guard()
    let order = __key_order(xs, f)
    xs.__reorder(order)
}
fn __list_sorted_by[T](xs: List[T], f: fn(T, T) -> int) -> List[T] {
    let order = __sort_order(xs, f)
    return xs.__pick(order)
}
fn __option_map[T, U](o: T?, f: fn(T) -> U) -> U? {
    match o {
        some(v) => return some(f(v))
        none => return none
    }
}
fn __option_and_then[T, U](o: T?, f: fn(T) -> U?) -> U? {
    match o {
        some(v) => return f(v)
        none => return none
    }
}
fn __result_map[T, U, E](r: Result[T, E], f: fn(T) -> U) -> Result[U, E] {
    match r {
        ok(v) => return ok(f(v))
        err(e) => return err(e)
    }
}
fn __result_and_then[T, U, E](r: Result[T, E], f: fn(T) -> Result[U, E]) -> Result[U, E] {
    match r {
        ok(v) => return f(v)
        err(e) => return err(e)
    }
}
"#;

/// The closure built-ins and the prelude function that stands for each.
pub(crate) const BUILTIN_FNS: &[(Builtin, &str)] = &[
    (Builtin::ListMap, "__list_map"),
    (Builtin::ListFilter, "__list_filter"),
    (Builtin::ListRetain, "__list_retain"),
    (Builtin::ListAny, "__list_any"),
    (Builtin::ListAll, "__list_all"),
    (Builtin::ListFind, "__list_find"),
    (Builtin::ListFindIndex, "__list_find_index"),
    (Builtin::ListFold, "__list_fold"),
    (Builtin::ListForEach, "__list_for_each"),
    (Builtin::ListFlatMap, "__list_flat_map"),
    (Builtin::ListSortBy, "__list_sort_by"),
    (Builtin::ListSortByKey, "__list_sort_by_key"),
    (Builtin::ListSortedBy, "__list_sorted_by"),
    (Builtin::OptionMap, "__option_map"),
    (Builtin::OptionAndThen, "__option_and_then"),
    (Builtin::ResultMap, "__result_map"),
    (Builtin::ResultAndThen, "__result_and_then"),
];

/// The parsed prelude.
pub(crate) fn prelude() -> &'static File {
    static PARSED: OnceLock<File> = OnceLock::new();
    PARSED.get_or_init(|| {
        let text = format!("{SOURCE}{FN_SOURCE}");
        let (file, problems) = colibri_syntax::parse_file(PRELUDE_FILE, &text);
        debug_assert!(
            problems.is_empty(),
            "the prelude does not parse: {problems:?}"
        );
        file
    })
}

/// Whether `name` is a type the language or its prelude declares (`int`, `Option`, `Show`,
/// `Error`, ..): a program or a host module cannot declare another type with it.
pub fn is_reserved_type_name(name: &str) -> bool {
    use colibri_syntax::ast::{ItemKind, TopLevel};
    crate::decls::BUILTIN_TYPES.contains(&name)
        || prelude().items.iter().any(|top| match top {
            TopLevel::Item(item) => match &item.kind {
                ItemKind::Struct(s) => s.name.name == name,
                ItemKind::Enum(e) => e.name.name == name,
                ItemKind::Interface(i) => i.name.name == name,
                _ => false,
            },
            TopLevel::Stmt(_) => false,
        })
}

/// The key of a function of the prelude in the table of module-level names. The prelude's own
/// functions (`__list_map`, `__merge_order`, ..) live under a key that no program can write, so
/// a program can neither call them nor clash with them: only a use inside the prelude resolves
/// to them.
pub(crate) fn value_key(name: &str, span: Span) -> std::borrow::Cow<'_, str> {
    if name.starts_with("__") && in_prelude(span) {
        std::borrow::Cow::Owned(format!("\u{1}{name}"))
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// Whether a span belongs to the prelude.
pub(crate) fn in_prelude(span: Span) -> bool {
    span.file == PRELUDE_FILE
}

/// Whether a span is in the prelude's own code, which a program never sees: the Colibri bodies
/// of the closure built-ins. An engine reports a position there as the call of the built-in.
pub fn is_prelude_span(span: Span) -> bool {
    in_prelude(span)
}
