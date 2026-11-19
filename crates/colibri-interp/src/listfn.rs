//! The list methods that take a closure or order the elements, and slicing (DESIGN §6.3).
//!
//! Every method here that may run user code (a closure, a user `Ord`) holds the list's
//! iteration guard for its whole duration, so a closure that changes the list panics with
//! "list modified during iteration" instead of corrupting it. No `RefCell` borrow is held
//! while user code runs: elements are read one at a time with [`ListObj::at`].
//!
//! Sorting never touches the list until it has finished: a stable bottom-up merge sort orders
//! a vector of *positions* of a snapshot with a comparison that may panic or be inconsistent,
//! and only the finished order is written back. A panic in the comparator leaves the list as
//! it was; an inconsistent comparator gives some order, never a corrupt list.

use std::cmp::Ordering;
use std::rc::Rc;

use colibri_check::hir::Builtin;
use colibri_syntax::Span;

use crate::ctl::Eval;
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::heap::ListObj;
use crate::value::Value;

/// The most elements `chunks` and `windows` build in one call.
const MAX_PIECES: usize = 1 << 27;

/// The position-wise comparison of a sort.
type Compare<'a> = dyn FnMut(&mut Interp, &mut Fiber, usize, usize) -> Eval<Ordering> + 'a;

/// Orders `0..n` stably with `cmp`, which may fail.
fn merge_order(
    this: &mut Interp,
    fiber: &mut Fiber,
    n: usize,
    cmp: &mut Compare<'_>,
) -> Eval<Vec<usize>> {
    let mut src: Vec<usize> = (0..n).collect();
    let mut dst = src.clone();
    let mut width = 1;
    while width < n {
        let mut start = 0;
        while start < n {
            let mid = (start + width).min(n);
            let end = (start + 2 * width).min(n);
            let (mut i, mut j, mut k) = (start, mid, start);
            while i < mid && j < end {
                // The right element goes first only when it is strictly smaller: stability.
                if cmp(this, fiber, src[i], src[j])? == Ordering::Greater {
                    dst[k] = src[j];
                    j += 1;
                } else {
                    dst[k] = src[i];
                    i += 1;
                }
                k += 1;
            }
            while i < mid {
                dst[k] = src[i];
                i += 1;
                k += 1;
            }
            while j < end {
                dst[k] = src[j];
                j += 1;
                k += 1;
            }
            start += 2 * width;
        }
        std::mem::swap(&mut src, &mut dst);
        width *= 2;
    }
    Ok(src)
}

/// `-`, `0` or `+` as an ordering.
fn order_of_int(v: &Value) -> Ordering {
    match v {
        Value::Int(i) => i.cmp(&0),
        _ => Ordering::Equal,
    }
}

fn is_true(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

impl Interp {
    /// Calls the function value `f` with `args`.
    fn call_fn(&mut self, fiber: &mut Fiber, f: &Value, args: Vec<Value>, span: Span) -> Eval {
        if let Some(frame) = fiber.frames.last_mut() {
            frame.span = span;
        }
        self.call_function_value(fiber, f, args, span)
    }

    /// Calls the predicate `f` on `item` and reads its answer.
    fn test(&mut self, fiber: &mut Fiber, f: &Value, item: Value, span: Span) -> Eval<bool> {
        let answer = self.call_fn(fiber, f, vec![item], span)?;
        Ok(is_true(&answer))
    }

    /// The list methods with a closure, ordering or sum; `args[0]` is the list.
    pub(crate) fn list_fn_op(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        list: &Rc<ListObj>,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        let arg = |args: &[Value], i: usize| args.get(i).cloned().unwrap_or(Value::Unit);
        match op {
            Builtin::ListMap => {
                let f = arg(args, 1);
                let _hold = list.iter_hold();
                let mut out = Vec::new();
                let mut i = 0;
                while let Some(item) = list.at(i) {
                    match self.call_fn(fiber, &f, vec![item], span) {
                        Ok(v) => out.push(v),
                        Err(ctl) => {
                            self.release_all(out);
                            return Err(ctl);
                        }
                    }
                    i += 1;
                }
                Ok(Value::list(out))
            }
            Builtin::ListFilter => {
                let f = arg(args, 1);
                let _hold = list.iter_hold();
                let mut out = Vec::new();
                let mut i = 0;
                while let Some(item) = list.at(i) {
                    match self.test(fiber, &f, item.clone(), span) {
                        Ok(true) => out.push(item),
                        Ok(false) => {}
                        Err(ctl) => {
                            self.release_all(out);
                            return Err(ctl);
                        }
                    }
                    i += 1;
                }
                Ok(Value::list(out))
            }
            Builtin::ListRetain => {
                let f = arg(args, 1);
                if let Err(v) = list.check_guard() {
                    return Err(self.violation(fiber, span, v));
                }
                let mut keep = Vec::new();
                {
                    let _hold = list.iter_hold();
                    let mut i = 0;
                    while let Some(item) = list.at(i) {
                        keep.push(self.test(fiber, &f, item, span)?);
                        i += 1;
                    }
                }
                if let Err(v) = list.check_guard() {
                    return Err(self.violation(fiber, span, v));
                }
                let items = list.snapshot();
                let kept: Vec<Value> = items
                    .iter()
                    .zip(&keep)
                    .filter(|(_, k)| **k)
                    .map(|(v, _)| v.clone())
                    .collect();
                self.release_all(items);
                let old = list.replace_all(kept);
                self.release_all(old);
                Ok(Value::Unit)
            }
            Builtin::ListAny | Builtin::ListAll => {
                let f = arg(args, 1);
                let any = op == Builtin::ListAny;
                let _hold = list.iter_hold();
                let mut i = 0;
                while let Some(item) = list.at(i) {
                    if self.test(fiber, &f, item, span)? == any {
                        return Ok(Value::Bool(any));
                    }
                    i += 1;
                }
                Ok(Value::Bool(!any))
            }
            Builtin::ListFind | Builtin::ListFindIndex => {
                let f = arg(args, 1);
                let _hold = list.iter_hold();
                let mut i = 0;
                while let Some(item) = list.at(i) {
                    if self.test(fiber, &f, item.clone(), span)? {
                        return Ok(if op == Builtin::ListFind {
                            Value::option(Some(item))
                        } else {
                            Value::option(Some(Value::Int(i as i64)))
                        });
                    }
                    i += 1;
                }
                Ok(Value::option(None))
            }
            Builtin::ListFold => {
                let (mut acc, f) = (arg(args, 1), arg(args, 2));
                let _hold = list.iter_hold();
                let mut i = 0;
                while let Some(item) = list.at(i) {
                    acc = self.call_fn(fiber, &f, vec![acc, item], span)?;
                    i += 1;
                }
                Ok(acc)
            }
            Builtin::ListForEach => {
                let f = arg(args, 1);
                let _hold = list.iter_hold();
                let mut i = 0;
                while let Some(item) = list.at(i) {
                    let v = self.call_fn(fiber, &f, vec![item], span)?;
                    self.release(v);
                    i += 1;
                }
                Ok(Value::Unit)
            }
            Builtin::ListSort | Builtin::ListSortBy | Builtin::ListSortByKey => {
                self.sort_list(fiber, op, list, arg(args, 1), true, span)
            }
            Builtin::ListSorted => {
                self.sort_list(fiber, Builtin::ListSort, list, Value::Unit, false, span)
            }
            Builtin::ListSortedBy => {
                self.sort_list(fiber, Builtin::ListSortBy, list, arg(args, 1), false, span)
            }
            Builtin::ListZip => {
                let Some(Value::List(other)) = args.get(1) else {
                    return Err(self.internal(fiber, span, "ill-typed `zip`"));
                };
                let (a, b) = (list.snapshot(), other.snapshot());
                let pairs = a
                    .iter()
                    .zip(&b)
                    .map(|(x, y)| Value::tuple(vec![x.clone(), y.clone()]))
                    .collect();
                self.release_all(a);
                self.release_all(b);
                Ok(Value::list(pairs))
            }
            Builtin::ListFlatMap => {
                let f = arg(args, 1);
                let _hold = list.iter_hold();
                let mut out = Vec::new();
                let mut i = 0;
                while let Some(item) = list.at(i) {
                    match self.call_fn(fiber, &f, vec![item], span) {
                        Ok(Value::List(inner)) => {
                            let more = inner.snapshot();
                            out.extend(more);
                            self.release(Value::List(inner));
                        }
                        Ok(other) => {
                            self.release(other);
                            self.release_all(out);
                            return Err(self.internal(fiber, span, "ill-typed `flat_map`"));
                        }
                        Err(ctl) => {
                            self.release_all(out);
                            return Err(ctl);
                        }
                    }
                    i += 1;
                }
                Ok(Value::list(out))
            }
            Builtin::ListDedup => {
                if let Err(v) = list.check_guard() {
                    return Err(self.violation(fiber, span, v));
                }
                let items = list.snapshot();
                let mut keep = vec![true; items.len()];
                {
                    let _hold = list.iter_hold();
                    for i in 1..items.len() {
                        match self.eq_deep(fiber, &items[i - 1], &items[i], span) {
                            Ok(same) => keep[i] = !same,
                            Err(ctl) => {
                                self.release_all(items);
                                return Err(ctl);
                            }
                        }
                    }
                }
                if let Err(v) = list.check_guard() {
                    self.release_all(items);
                    return Err(self.violation(fiber, span, v));
                }
                let kept: Vec<Value> = items
                    .iter()
                    .zip(&keep)
                    .filter(|(_, k)| **k)
                    .map(|(v, _)| v.clone())
                    .collect();
                self.release_all(items);
                let old = list.replace_all(kept);
                self.release_all(old);
                Ok(Value::Unit)
            }
            Builtin::ListChunks | Builtin::ListWindows => {
                let Some(Value::Int(size)) = args.get(1) else {
                    return Err(self.internal(fiber, span, "ill-typed `chunks`"));
                };
                let name = if op == Builtin::ListChunks {
                    "chunks"
                } else {
                    "windows"
                };
                let Ok(size @ 1..) = usize::try_from(*size) else {
                    return Err(self.panic(
                        fiber,
                        span,
                        format!("`{name}` needs a size of at least 1, found {size}"),
                    ));
                };
                let items = list.snapshot();
                // The number of elements in all the pieces, worked out before building any.
                let total = if op == Builtin::ListChunks {
                    items.len()
                } else {
                    (items.len() + 1)
                        .saturating_sub(size)
                        .saturating_mul(size.min(items.len()))
                };
                if total > MAX_PIECES {
                    self.release_all(items);
                    return Err(self.panic(
                        fiber,
                        span,
                        format!(
                            "`{name}` would make a list of {total} elements, which is too many"
                        ),
                    ));
                }
                if let Err(ctl) = self.charge(fiber, span, total) {
                    self.release_all(items);
                    return Err(ctl);
                }
                let out = if op == Builtin::ListChunks {
                    items
                        .chunks(size)
                        .map(|p| Value::list(p.to_vec()))
                        .collect()
                } else {
                    items
                        .windows(size)
                        .map(|p| Value::list(p.to_vec()))
                        .collect()
                };
                self.release_all(items);
                Ok(Value::list(out))
            }
            Builtin::ListMin | Builtin::ListMax => {
                let items = list.snapshot();
                let want = if op == Builtin::ListMin {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
                let mut best: Option<usize> = None;
                for i in 0..items.len() {
                    let Some(b) = best else {
                        best = Some(i);
                        continue;
                    };
                    match self.cmp_deep(fiber, &items[i], &items[b], span) {
                        Ok(Some(o)) if o == want => best = Some(i),
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            self.release_all(items);
                            return Err(self.not_ordered(fiber, span));
                        }
                        Err(ctl) => {
                            self.release_all(items);
                            return Err(ctl);
                        }
                    }
                }
                let found = best.map(|b| items[b].clone());
                self.release_all(items);
                Ok(Value::option(found))
            }
            Builtin::ListSum => {
                let items = list.snapshot();
                let mut acc = args.get(1).cloned();
                for item in &items {
                    acc = Some(match (acc, item) {
                        (Some(Value::Int(a)), Value::Int(b)) => match a.checked_add(*b) {
                            Some(s) => Value::Int(s),
                            None => return Err(self.panic(fiber, span, "integer overflow")),
                        },
                        (Some(Value::Float(a)), Value::Float(b)) => Value::Float(a + b),
                        (None, first) => first.clone(),
                        _ => return Err(self.internal(fiber, span, "ill-typed `sum`")),
                    });
                }
                Ok(acc.unwrap_or(Value::Int(0)))
            }
            Builtin::ListJoin => {
                let Some(Value::Str(sep)) = args.get(1) else {
                    return Err(self.internal(fiber, span, "ill-typed `join`"));
                };
                let parts: Vec<String> = list.snapshot().iter().map(|v| self.shown(v)).collect();
                Ok(Value::Str(Rc::from(parts.join(sep))))
            }
            Builtin::ListSlice => self.list_slice(fiber, list, args, span),
            _ => Err(self.internal(fiber, span, "ill-typed built-in call")),
        }
    }

    pub(crate) fn not_ordered(&self, fiber: &mut Fiber, span: Span) -> crate::ctl::Ctl {
        self.panic(fiber, span, "these values are not ordered (a `nan`)")
    }

    /// `sort`, `sort_by` and `sort_by_key`: a stable sort of a copy, written back whole. With
    /// `in_place` false (`sorted`, `sorted_by`) the list is left alone and the sorted copy is
    /// the result.
    fn sort_list(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        list: &Rc<ListObj>,
        f: Value,
        in_place: bool,
        span: Span,
    ) -> Eval {
        if in_place && let Err(v) = list.check_guard() {
            return Err(self.violation(fiber, span, v));
        }
        let items = list.snapshot();
        let order = {
            let _hold = list.iter_hold();
            self.order_items(fiber, op, &items, &f, span)
        };
        let order = match order {
            Ok(order) => order,
            Err(ctl) => {
                self.release_all(items);
                return Err(ctl);
            }
        };
        if in_place && let Err(v) = list.check_guard() {
            self.release_all(items);
            return Err(self.violation(fiber, span, v));
        }
        let sorted: Vec<Value> = order.iter().map(|&i| items[i].clone()).collect();
        self.release_all(items);
        if !in_place {
            return Ok(Value::list(sorted));
        }
        let old = list.replace_all(sorted);
        self.release_all(old);
        Ok(Value::Unit)
    }

    /// The sorted order of `items` as positions.
    fn order_items(
        &mut self,
        fiber: &mut Fiber,
        op: Builtin,
        items: &[Value],
        f: &Value,
        span: Span,
    ) -> Eval<Vec<usize>> {
        match op {
            Builtin::ListSort => {
                merge_order(
                    self,
                    fiber,
                    items.len(),
                    &mut |this, fiber, a, b| match this
                        .cmp_deep(fiber, &items[a], &items[b], span)?
                    {
                        Some(o) => Ok(o),
                        None => Err(this.not_ordered(fiber, span)),
                    },
                )
            }
            Builtin::ListSortBy => {
                merge_order(self, fiber, items.len(), &mut |this, fiber, a, b| {
                    let v =
                        this.call_fn(fiber, f, vec![items[a].clone(), items[b].clone()], span)?;
                    let o = order_of_int(&v);
                    this.release(v);
                    Ok(o)
                })
            }
            _ => {
                // The key of each element is computed once, in order.
                let mut keys: Vec<Value> = Vec::with_capacity(items.len());
                for item in items {
                    match self.call_fn(fiber, f, vec![item.clone()], span) {
                        Ok(k) => keys.push(k),
                        Err(ctl) => {
                            self.release_all(keys);
                            return Err(ctl);
                        }
                    }
                }
                let order =
                    merge_order(
                        self,
                        fiber,
                        items.len(),
                        &mut |this, fiber, a, b| match this
                            .cmp_deep(fiber, &keys[a], &keys[b], span)?
                        {
                            Some(o) => Ok(o),
                            None => Err(this.not_ordered(fiber, span)),
                        },
                    );
                self.release_all(keys);
                order
            }
        }
    }

    /// `xs[lo..hi]`: a copy of part of the list.
    fn list_slice(
        &mut self,
        fiber: &mut Fiber,
        list: &Rc<ListObj>,
        args: &[Value],
        span: Span,
    ) -> Eval {
        let len = list.len();
        let (lo, hi) = self.slice_bounds(fiber, args, len, "list", span)?;
        let items = list.snapshot();
        Ok(Value::list(items[lo..hi].to_vec()))
    }

    /// The `lo..hi` positions a slice expression selects out of `len`, or a panic. `args` are
    /// `[value, lo: int?, hi: int?, inclusive: bool]`.
    pub(crate) fn slice_bounds(
        &mut self,
        fiber: &mut Fiber,
        args: &[Value],
        len: usize,
        what: &str,
        span: Span,
    ) -> Eval<(usize, usize)> {
        let end_of = |v: Option<&Value>| match v {
            Some(Value::Enum(e)) => match e.payload.first() {
                Some(Value::Int(i)) => Some(*i),
                _ => None,
            },
            _ => None,
        };
        let (lo, hi) = (end_of(args.get(1)), end_of(args.get(2)));
        let inclusive = matches!(args.get(3), Some(Value::Bool(true)));
        let shown = format!(
            "{}..{}{}",
            lo.map_or(String::new(), |x| x.to_string()),
            if inclusive { "=" } else { "" },
            hi.map_or(String::new(), |x| x.to_string())
        );
        let n = len as i64;
        let start = lo.unwrap_or(0);
        let end = match (hi, inclusive) {
            (Some(h), true) => h.saturating_add(1),
            (Some(h), false) => h,
            (None, _) => n,
        };
        if start > end && start >= 0 && end >= 0 && start <= n && end <= n {
            return Err(self.panic(
                fiber,
                span,
                format!("slice {shown} starts after it ends (start {start}, end {end})"),
            ));
        }
        if start < 0 || end < 0 || end > n || start > end {
            return Err(self.panic(
                fiber,
                span,
                format!("slice {shown} out of range for {what} of length {len}"),
            ));
        }
        Ok((start as usize, end as usize))
    }
}
