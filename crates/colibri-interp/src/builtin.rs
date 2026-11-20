//! The methods and constructors of `str`, `List`, `Map` and `Set`, and `clone`.
//!
//! The checker has already typed every call, so an operand of the wrong shape is an internal
//! error, not a user error. Values removed from a collection are released here, after the
//! collection's own borrow has ended (ARCHITECTURE §3).

use std::rc::Rc;

use colibri_check::hir::Builtin;
use colibri_check::types::{BYTES_BUILDER_TYPE, STR_BUILDER_TYPE};
use colibri_syntax::Span;

use crate::ctl::{Ctl, Eval};
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::heap::{ClassObj, ListError, ListObj, MapObj, OutOfBounds, SetObj, Violation};
use crate::ordered::OrderedMap;
use crate::show::show;
use crate::value::Value;

/// The receiver of a list method.
fn list_of(v: &Value) -> Option<Rc<ListObj>> {
    match v {
        Value::List(l) => Some(Rc::clone(l)),
        _ => None,
    }
}

fn map_of(v: &Value) -> Option<Rc<MapObj>> {
    match v {
        Value::Map(m) => Some(Rc::clone(m)),
        _ => None,
    }
}

fn set_of(v: &Value) -> Option<Rc<SetObj>> {
    match v {
        Value::Set(s) => Some(Rc::clone(s)),
        _ => None,
    }
}

fn int_of(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }
}

/// How many elements a built-in looks at, for the instruction budget: the length of the
/// receiver for every operation that walks it. Constant-time operations cost nothing extra.
fn work_of(op: Builtin, recv: Option<&Value>) -> Option<usize> {
    use Builtin as B;
    let walks = matches!(
        op,
        B::Clone
            | B::ListContains
            | B::ListIndexOf
            | B::ListReverse
            | B::ListExtend
            | B::ListMap
            | B::ListFilter
            | B::ListRetain
            | B::ListAny
            | B::ListAll
            | B::ListFind
            | B::ListFindIndex
            | B::ListFold
            | B::ListForEach
            | B::ListSort
            | B::ListSortBy
            | B::ListSortByKey
            | B::ListMin
            | B::ListMax
            | B::ListSum
            | B::ListJoin
            | B::ListZip
            | B::ListFlatMap
            | B::ListSorted
            | B::ListSortedBy
            | B::ListDedup
            | B::ListChunks
            | B::ListWindows
            | B::ListSlice
            | B::SetFromList
            | B::RangeToList
            | B::SetUnion
            | B::SetIntersection
            | B::SetDifference
            | B::SetSymmetricDifference
            | B::SetIsSubset
            | B::SetIsSuperset
            | B::SetIsDisjoint
            | B::MapKeys
            | B::MapEntries
            | B::MapValues
            | B::Iter
            | B::StrChars
            | B::StrBytes
            | B::StrContains
            | B::StrSplit
            | B::StrTrim
            | B::StrToUpper
            | B::StrToLower
            | B::StrReplace
            | B::StrFind
            | B::StrRFind
            | B::StrSplitOnce
            | B::StrRSplitOnce
            | B::StrPadLeft
            | B::StrPadRight
            | B::StrSlice
            | B::StrBuilderToStr
            | B::BytesToList
            | B::BytesToStr
            | B::BytesToStrLossy
            | B::BytesFind
            | B::BytesContains
            | B::BytesSlice
            | B::BytesSliceOf
            | B::StrToBytes
    );
    if !walks {
        return None;
    }
    Some(match recv? {
        Value::List(l) => l.len(),
        Value::Str(s) => s.len(),
        Value::Bytes(b) => b.len(),
        Value::Map(m) => m.len(),
        Value::Set(s) => s.len(),
        // A range too long for `to_list` panics there, so the charge stops at the limit
        // (a saturated length would otherwise exhaust the budget first).
        // Making an iterator over a range is O(1): what it yields is charged as it is
        // consumed, not here.
        Value::Range(_) if op == B::Iter => return None,
        Value::Range(r) => {
            let cap = if op == B::RangeToList {
                crate::strfn::MAX_RANGE_LIST + 1
            } else {
                u64::MAX
            };
            usize::try_from(r.len().min(cap)).unwrap_or(usize::MAX)
        }
        Value::Obj(o) => match o.field(1) {
            Some(Value::Int(n)) => usize::try_from(n).unwrap_or(0),
            _ => return None,
        },
        _ => return None,
    })
}

/// Moves argument `i` out, leaving `()` behind.
fn take(args: &mut [Value], i: usize) -> Value {
    args.get_mut(i)
        .map_or(Value::Unit, |a| std::mem::replace(a, Value::Unit))
}

impl Interp {
    /// Runs one built-in operation. `args[0]` is the receiver, if the operation has one. The
    /// arguments are references to values the caller still owns on its temporaries, so
    /// releasing what is left of them here only drops a reference.
    pub(crate) fn builtin(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        mut args: Vec<Value>,
        span: Span,
    ) -> Eval {
        // Built-ins that look at every element cost a step for each.
        if let Some(n) = work_of(op, args.first())
            && let Err(ctl) = self.charge(fiber, span, n)
        {
            self.release_all(args);
            return Err(ctl);
        }
        let result = self.builtin_inner(fiber, op, &mut args, span);
        self.release_all(args);
        result
    }

    fn builtin_inner(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        let bad =
            |this: &Self, fiber: &mut Fiber| this.internal(fiber, span, "ill-typed built-in call");
        let recv = args.first().cloned().unwrap_or(Value::Unit);
        Ok(match op {
            Builtin::Clone => return self.clone_deep(fiber, &recv, span),
            Builtin::StrLen => match &recv {
                Value::Str(s) => Value::Int(s.len() as i64),
                _ => return Err(bad(self, fiber)),
            },
            Builtin::StrIsEmpty => match &recv {
                Value::Str(s) => Value::Bool(s.is_empty()),
                _ => return Err(bad(self, fiber)),
            },
            Builtin::OptionIsSome
            | Builtin::OptionIsNone
            | Builtin::OptionExpect
            | Builtin::OptionMap
            | Builtin::OptionAndThen
            | Builtin::ResultIsOk
            | Builtin::ResultIsErr
            | Builtin::ResultExpect
            | Builtin::ResultMap
            | Builtin::ResultAndThen
            | Builtin::ResultOk
            | Builtin::ResultErr
            | Builtin::ResultContext
            | Builtin::ErrorMessage
            | Builtin::ErrorContext
            | Builtin::ErrorCause => return self.optres_op(fiber, op, args, span),
            Builtin::CoroutineCancel | Builtin::CoroutineDone | Builtin::CoroutineJoin => {
                return self.coroutine_builtin(fiber, op, &recv, span);
            }
            Builtin::CoGroupCancel | Builtin::CoGroupCount | Builtin::CoGroupJoinAll => {
                return self.cogroup_builtin(fiber, op, &recv, span);
            }
            Builtin::SignalEmit => return self.signal_builtin(fiber, &recv, span),
            Builtin::StrBuilderNew => Value::Obj(ClassObj::new(
                STR_BUILDER_TYPE,
                vec![Value::list(Vec::new()), Value::Int(0)],
            )),
            Builtin::StrBuilderPush
            | Builtin::StrBuilderPushChar
            | Builtin::StrBuilderLen
            | Builtin::StrBuilderIsEmpty
            | Builtin::StrBuilderClear
            | Builtin::StrBuilderToStr => return self.str_builder_op(fiber, op, args, span),
            Builtin::BytesBuilderNew => Value::Obj(ClassObj::new(
                BYTES_BUILDER_TYPE,
                vec![Value::ByteBuf(Default::default())],
            )),
            Builtin::StrToBytes
            | Builtin::BytesFromList
            | Builtin::BytesEmpty
            | Builtin::BytesLen
            | Builtin::BytesIsEmpty
            | Builtin::BytesAt
            | Builtin::BytesGet
            | Builtin::BytesSlice
            | Builtin::BytesSliceOf
            | Builtin::BytesToList
            | Builtin::BytesToStr
            | Builtin::BytesToStrLossy
            | Builtin::BytesStartsWith
            | Builtin::BytesEndsWith
            | Builtin::BytesFind
            | Builtin::BytesContains => return self.bytes_op(fiber, op, args, span),
            Builtin::BytesBuilderPush
            | Builtin::BytesBuilderPushBytes
            | Builtin::BytesBuilderPushStr
            | Builtin::BytesBuilderLen
            | Builtin::BytesBuilderIsEmpty
            | Builtin::BytesBuilderClear
            | Builtin::BytesBuilderToBytes => return self.bytes_builder_op(fiber, op, args, span),
            Builtin::SetNew => Value::Set(SetObj::new(OrderedMap::new())),
            Builtin::SetFromList => {
                let Some(list) = list_of(&recv) else {
                    return Err(bad(self, fiber));
                };
                let set = SetObj::new(OrderedMap::new());
                for item in list.snapshot() {
                    let item = self.canonical_key(fiber, || set.snapshot(), item, span)?;
                    if let Ok((_, Some(spare))) = set.add(item) {
                        self.release(spare);
                    }
                }
                Value::Set(set)
            }
            Builtin::ListLen
            | Builtin::ListIsEmpty
            | Builtin::ListPush
            | Builtin::ListPop
            | Builtin::ListInsert
            | Builtin::ListRemove
            | Builtin::ListClear
            | Builtin::ListContains
            | Builtin::ListIndexOf
            | Builtin::ListFirst
            | Builtin::ListLast
            | Builtin::ListReverse
            | Builtin::ListExtend
            | Builtin::ListGet => {
                let Some(list) = list_of(&recv) else {
                    return Err(bad(self, fiber));
                };
                return self.list_op(fiber, op, &list, args, span);
            }
            Builtin::ListMap
            | Builtin::ListFilter
            | Builtin::ListRetain
            | Builtin::ListAny
            | Builtin::ListAll
            | Builtin::ListFind
            | Builtin::ListFindIndex
            | Builtin::ListFold
            | Builtin::ListForEach
            | Builtin::ListSort
            | Builtin::ListSortBy
            | Builtin::ListSortByKey
            | Builtin::ListMin
            | Builtin::ListMax
            | Builtin::ListSum
            | Builtin::ListJoin
            | Builtin::ListZip
            | Builtin::ListFlatMap
            | Builtin::ListSorted
            | Builtin::ListSortedBy
            | Builtin::ListDedup
            | Builtin::ListChunks
            | Builtin::ListWindows
            | Builtin::ListSlice => {
                let Some(list) = list_of(&recv) else {
                    return Err(bad(self, fiber));
                };
                return self.list_fn_op(fiber, op, &list, args, span);
            }
            Builtin::StrChars
            | Builtin::StrBytes
            | Builtin::StrContains
            | Builtin::StrStartsWith
            | Builtin::StrEndsWith
            | Builtin::StrSplit
            | Builtin::StrTrim
            | Builtin::StrToUpper
            | Builtin::StrToLower
            | Builtin::StrReplace
            | Builtin::StrFind
            | Builtin::StrRFind
            | Builtin::StrSplitOnce
            | Builtin::StrRSplitOnce
            | Builtin::StrPadLeft
            | Builtin::StrPadRight
            | Builtin::StrSlice => return self.str_op(fiber, op, args, span),
            Builtin::RangeContains
            | Builtin::RangeLen
            | Builtin::RangeIsEmpty
            | Builtin::RangeRev
            | Builtin::RangeToList
            | Builtin::Iter
            | Builtin::IterEnumerate => return self.range_op(fiber, op, args, span),
            Builtin::MapLen
            | Builtin::MapIsEmpty
            | Builtin::MapGet
            | Builtin::MapGetOr
            | Builtin::MapEntries
            | Builtin::MapContainsKey
            | Builtin::MapInsert
            | Builtin::MapRemove
            | Builtin::MapKeys
            | Builtin::MapValues
            | Builtin::MapClear => {
                let Some(map) = map_of(&recv) else {
                    return Err(bad(self, fiber));
                };
                return self.map_op(fiber, op, &map, args, span);
            }
            Builtin::SetLen
            | Builtin::SetIsEmpty
            | Builtin::SetAdd
            | Builtin::SetRemove
            | Builtin::SetContains
            | Builtin::SetClear
            | Builtin::SetUnion
            | Builtin::SetIntersection
            | Builtin::SetDifference
            | Builtin::SetSymmetricDifference
            | Builtin::SetIsSubset
            | Builtin::SetIsSuperset
            | Builtin::SetIsDisjoint => {
                let Some(set) = set_of(&recv) else {
                    return Err(bad(self, fiber));
                };
                return self.set_op(fiber, op, &set, args, span);
            }
        })
    }

    /// Takes a half-made copy apart without tracing its objects as destroyed.
    pub(crate) fn abandon_copies(&mut self, copies: Vec<Value>) {
        let mut pending = Vec::new();
        for copy in &copies {
            match copy {
                Value::Obj(o) => pending.extend(o.take_fields()),
                Value::List(l) => pending.extend(l.take_all()),
                Value::Map(m) => pending.extend(m.take_all().into_iter().flat_map(|(k, v)| [k, v])),
                Value::Set(s) => pending.extend(s.take_all()),
                _ => {}
            }
        }
        drop(copies);
        self.release_all(pending);
    }

    pub(crate) fn violation(&self, fiber: &mut Fiber, span: Span, v: Violation) -> Ctl {
        self.panic(fiber, span, v.0)
    }

    pub(crate) fn bounds_panic(
        &self,
        fiber: &mut Fiber,
        span: Span,
        what: &str,
        e: OutOfBounds,
    ) -> Ctl {
        self.panic(
            fiber,
            span,
            format!(
                "{what} {} out of bounds for list of length {}",
                e.index, e.len
            ),
        )
    }

    pub(crate) fn list_error(
        &self,
        fiber: &mut Fiber,
        span: Span,
        what: &str,
        e: ListError,
    ) -> Ctl {
        match e {
            ListError::Guard(v) => self.violation(fiber, span, v),
            ListError::Bounds(b) => self.bounds_panic(fiber, span, what, b),
        }
    }

    fn list_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        list: &Rc<ListObj>,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        let index = |args: &[Value]| args.get(1).and_then(int_of).unwrap_or(0);
        Ok(match op {
            Builtin::ListLen => Value::Int(list.len() as i64),
            Builtin::ListIsEmpty => Value::Bool(list.is_empty()),
            Builtin::ListPush => match list.push(take(args, 1)) {
                Ok(()) => Value::Unit,
                Err(rejected) => {
                    self.release(rejected.args);
                    return Err(self.violation(fiber, span, rejected.violation));
                }
            },
            Builtin::ListPop => match list.pop() {
                Ok(v) => Value::option(v),
                Err(v) => return Err(self.violation(fiber, span, v)),
            },
            Builtin::ListInsert => match list.insert(index(args), take(args, 2)) {
                Ok(()) => Value::Unit,
                Err((e, item)) => {
                    self.release(item);
                    return Err(self.list_error(fiber, span, "insert index", e));
                }
            },
            Builtin::ListRemove => match list.remove(index(args)) {
                Ok(v) => v,
                Err(e) => return Err(self.list_error(fiber, span, "index", e)),
            },
            Builtin::ListClear => match list.clear() {
                Ok(items) => {
                    self.release_all(items);
                    Value::Unit
                }
                Err(v) => return Err(self.violation(fiber, span, v)),
            },
            Builtin::ListContains => {
                let item = take(args, 1);
                let mut found = false;
                for x in list.snapshot() {
                    if self.eq_deep(fiber, &x, &item, span)? {
                        found = true;
                        break;
                    }
                }
                self.release(item);
                Value::Bool(found)
            }
            Builtin::ListIndexOf => {
                let item = take(args, 1);
                let mut at = None;
                for (i, x) in list.snapshot().iter().enumerate() {
                    if self.eq_deep(fiber, x, &item, span)? {
                        at = Some(i);
                        break;
                    }
                }
                self.release(item);
                Value::option(at.map(|i| Value::Int(i as i64)))
            }
            Builtin::ListFirst => Value::option(list.first()),
            Builtin::ListLast => Value::option(list.last()),
            Builtin::ListReverse => match list.reverse() {
                Ok(()) => Value::Unit,
                Err(v) => return Err(self.violation(fiber, span, v)),
            },
            Builtin::ListExtend => {
                let more = match args.get(1).and_then(list_of) {
                    Some(other) => other.snapshot(),
                    None => return Err(self.internal(fiber, span, "ill-typed built-in call")),
                };
                match list.extend(more) {
                    Ok(()) => Value::Unit,
                    Err(rejected) => {
                        self.release_all(rejected.args);
                        return Err(self.violation(fiber, span, rejected.violation));
                    }
                }
            }
            Builtin::ListGet => Value::option(list.get(index(args)).ok()),
            _ => return Err(self.internal(fiber, span, "ill-typed built-in call")),
        })
    }

    fn map_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        map: &Rc<MapObj>,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        if !matches!(
            op,
            Builtin::MapLen
                | Builtin::MapIsEmpty
                | Builtin::MapKeys
                | Builtin::MapValues
                | Builtin::MapEntries
                | Builtin::MapClear
        ) {
            let keys = if args.get(1).is_some_and(|k| self.key_needs_scan(k)) {
                map.keys()
            } else {
                Vec::new()
            };
            self.canonicalise_key(fiber, &keys, args, span)?;
        }
        Ok(match op {
            Builtin::MapLen => Value::Int(map.len() as i64),
            Builtin::MapIsEmpty => Value::Bool(map.is_empty()),
            Builtin::MapGet => Value::option(args.get(1).and_then(|k| map.get(k))),
            Builtin::MapGetOr => match args.get(1).and_then(|k| map.get(k)) {
                Some(v) => v,
                None => args.get(2).cloned().unwrap_or(Value::Unit),
            },
            Builtin::MapEntries => Value::list(
                map.snapshot()
                    .into_iter()
                    .map(|(k, v)| Value::tuple(vec![k, v]))
                    .collect(),
            ),
            Builtin::MapContainsKey => {
                Value::Bool(args.get(1).is_some_and(|k| map.contains_key(k)))
            }
            Builtin::MapInsert => match map.insert(take(args, 1), take(args, 2)) {
                Ok(replaced) => {
                    if let Some(old) = replaced.old {
                        self.release(old);
                    }
                    if let Some(spare) = replaced.spare_key {
                        self.release(spare);
                    }
                    Value::Unit
                }
                Err(rejected) => {
                    let (k, v) = rejected.args;
                    self.release(k);
                    self.release(v);
                    return Err(self.violation(fiber, span, rejected.violation));
                }
            },
            Builtin::MapRemove => {
                let Some(key) = args.get(1) else {
                    return Err(self.internal(fiber, span, "ill-typed built-in call"));
                };
                match map.remove(key) {
                    Ok(Some((k, v))) => {
                        self.release(k);
                        Value::option(Some(v))
                    }
                    Ok(None) => Value::option(None),
                    Err(v) => return Err(self.violation(fiber, span, v)),
                }
            }
            Builtin::MapKeys => Value::list(map.keys()),
            Builtin::MapValues => Value::list(map.values()),
            Builtin::MapClear => match map.clear() {
                Ok(entries) => {
                    for (k, v) in entries {
                        self.release(k);
                        self.release(v);
                    }
                    Value::Unit
                }
                Err(v) => return Err(self.violation(fiber, span, v)),
            },
            _ => return Err(self.internal(fiber, span, "ill-typed built-in call")),
        })
    }

    fn set_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        set: &Rc<SetObj>,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        if matches!(
            op,
            Builtin::SetUnion
                | Builtin::SetIntersection
                | Builtin::SetDifference
                | Builtin::SetSymmetricDifference
                | Builtin::SetIsSubset
                | Builtin::SetIsSuperset
                | Builtin::SetIsDisjoint
        ) {
            return self.set_algebra(fiber, op, set, args, span);
        }
        if !matches!(
            op,
            Builtin::SetLen | Builtin::SetIsEmpty | Builtin::SetClear
        ) {
            let keys = if args.get(1).is_some_and(|k| self.key_needs_scan(k)) {
                set.snapshot()
            } else {
                Vec::new()
            };
            self.canonicalise_key(fiber, &keys, args, span)?;
        }
        Ok(match op {
            Builtin::SetLen => Value::Int(set.len() as i64),
            Builtin::SetIsEmpty => Value::Bool(set.is_empty()),
            Builtin::SetContains => Value::Bool(args.get(1).is_some_and(|x| set.contains(x))),
            Builtin::SetAdd => match set.add(take(args, 1)) {
                Ok((_, spare)) => {
                    if let Some(spare) = spare {
                        self.release(spare);
                    }
                    Value::Unit
                }
                Err(rejected) => {
                    self.release(rejected.args);
                    return Err(self.violation(fiber, span, rejected.violation));
                }
            },
            Builtin::SetRemove => {
                let Some(item) = args.get(1) else {
                    return Err(self.internal(fiber, span, "ill-typed built-in call"));
                };
                match set.remove(item) {
                    Ok(Some(old)) => {
                        self.release(old);
                        Value::Bool(true)
                    }
                    Ok(None) => Value::Bool(false),
                    Err(v) => return Err(self.violation(fiber, span, v)),
                }
            }
            Builtin::SetClear => match set.clear() {
                Ok(items) => {
                    self.release_all(items);
                    Value::Unit
                }
                Err(v) => return Err(self.violation(fiber, span, v)),
            },
            _ => return Err(self.internal(fiber, span, "ill-typed built-in call")),
        })
    }

    /// Whether `item` is in the set whose elements are `items`: by the user-written `Eq` of
    /// the element type when `scan` (some element may have one), else by structure.
    fn set_has(
        &mut self,
        fiber: &mut Fiber,
        set: &SetObj,
        items: &[Value],
        item: &Value,
        scan: bool,
        span: Span,
    ) -> Eval<bool> {
        if !scan {
            return Ok(set.contains(item));
        }
        self.core.key_scans += 1;
        for x in items {
            // One step per comparison actually made: a user `Eq` may do anything.
            self.charge(fiber, span, 1)?;
            if self.eq_deep(fiber, x, item, span)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `union`, `intersection`, `difference`, `symmetric_difference` (new sets: the elements of
    /// the receiver in its order, then those of `other`) and `is_subset`, `is_superset`,
    /// `is_disjoint`. Both sets are held while a user `Eq` may run.
    fn set_algebra(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        set: &Rc<SetObj>,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        let Some(other) = args.get(1).and_then(set_of) else {
            return Err(self.internal(fiber, span, "ill-typed built-in call"));
        };
        self.charge(fiber, span, other.len())?;
        let (left, right) = (set.snapshot(), other.snapshot());
        let result = {
            let _hold_left = set.iter_hold();
            let _hold_right = other.iter_hold();
            self.set_algebra_inner(fiber, op, (set, &left), (&other, &right), span)
        };
        self.release_all(left);
        self.release_all(right);
        result
    }

    fn set_algebra_inner(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        (lset, left): (&SetObj, &[Value]),
        (rset, right): (&SetObj, &[Value]),
        span: Span,
    ) -> Eval {
        // Only elements that may have a user `Eq` need the scan.
        let scan = left.iter().chain(right).any(|x| self.key_needs_scan(x));
        // Which elements of `side` are (not) in `other`.
        let pick = |this: &mut Self,
                    fiber: &mut Fiber,
                    side: &[Value],
                    other: (&SetObj, &[Value]),
                    wanted: bool|
         -> Eval<Vec<Value>> {
            let mut out = Vec::new();
            for x in side {
                if this.set_has(fiber, other.0, other.1, x, scan, span)? == wanted {
                    out.push(x.clone());
                }
            }
            Ok(out)
        };
        let (l, r) = ((lset, left), (rset, right));
        let items = match op {
            Builtin::SetUnion => {
                let mut items = left.to_vec();
                items.extend(pick(self, fiber, right, l, false)?);
                items
            }
            Builtin::SetIntersection => pick(self, fiber, left, r, true)?,
            Builtin::SetDifference => pick(self, fiber, left, r, false)?,
            Builtin::SetSymmetricDifference => {
                let mut items = pick(self, fiber, left, r, false)?;
                items.extend(pick(self, fiber, right, l, false)?);
                items
            }
            Builtin::SetIsSubset => {
                return Ok(Value::Bool(pick(self, fiber, left, r, false)?.is_empty()));
            }
            Builtin::SetIsSuperset => {
                return Ok(Value::Bool(pick(self, fiber, right, l, false)?.is_empty()));
            }
            Builtin::SetIsDisjoint => {
                return Ok(Value::Bool(pick(self, fiber, left, r, true)?.is_empty()));
            }
            _ => return Err(self.internal(fiber, span, "ill-typed built-in call")),
        };
        let out = SetObj::new(OrderedMap::new());
        for item in items {
            let item = self.canonical_key(fiber, || out.snapshot(), item, span)?;
            if let Ok((_, Some(spare))) = out.add(item) {
                self.release(spare);
            }
        }
        Ok(Value::Set(out))
    }

    /// The `Show` text of a value, for panic messages.
    pub(crate) fn shown(&self, v: &Value) -> String {
        show(&self.core.types, v).into_owned()
    }
}
