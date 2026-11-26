//! The list methods that order the elements or join, zip and chunk them, and slicing
//! (DESIGN §6.3). The methods that take a closure are Colibri code in the prelude.
//!
//! Every method here that may run user code (a user `Ord`) holds the list's
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
use crate::engine::Engine;
use crate::heap::ListObj;
use crate::prelude::*;
use crate::value::Value;

/// The most elements `chunks` and `windows` build in one call.
const MAX_PIECES: usize = 1 << 27;

/// The position-wise comparison of a sort.
type Compare<'a, E> =
    dyn FnMut(&mut E, &mut <E as Engine>::Fiber, usize, usize) -> Eval<Ordering> + 'a;

/// Orders `0..n` stably with `cmp`, which may fail.
fn merge_order<E: Engine>(
    this: &mut E,
    fiber: &mut E::Fiber,
    n: usize,
    cmp: &mut Compare<'_, E>,
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

fn is_true(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

#[doc(hidden)]
pub trait ListFnOps: Engine {
    /// The list methods that order, sum, join, zip or chunk; `args[0]` is the list.
    fn list_fn_op(
        &mut self,
        fiber: &mut Self::Fiber,
        op: Builtin,
        list: &Rc<ListObj>,
        args: &mut [Value],
        span: Span,
    ) -> Eval {
        match op {
            Builtin::ListSort => self.sort_list(fiber, list, true, span),
            Builtin::ListSorted => self.sort_list(fiber, list, false, span),
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
                if let Err(ctl) = self.charge(fiber, span, total).and_then(|()| {
                    self.mem_reserve(
                        fiber,
                        span,
                        total.saturating_mul(std::mem::size_of::<Value>()),
                    )
                }) {
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
                Ok(self.core().option(found))
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
                // The buffer is sized for the strings in it (an integer or other value takes a
                // guess); with a memory limit set, that size is asked for first.
                if self.core().memory_limit().is_some() {
                    let guess = list.with(|items| {
                        items
                            .iter()
                            .map(|v| match v {
                                Value::Str(s) => s.len(),
                                _ => 8,
                            })
                            .sum::<usize>()
                            .saturating_add(sep.len().saturating_mul(items.len().saturating_sub(1)))
                    });
                    self.mem_reserve(fiber, span, guess)?;
                }
                // One buffer, and no copy of the list or per-item `String`.
                let types = &self.core().types;
                let text = list.with(|items| {
                    let guess: usize = items
                        .iter()
                        .map(|v| match v {
                            Value::Str(s) => s.len(),
                            _ => 8,
                        })
                        .sum::<usize>()
                        + sep.len() * items.len().saturating_sub(1);
                    let mut out = String::with_capacity(guess);
                    for (i, v) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(sep);
                        }
                        match v {
                            Value::Str(s) => out.push_str(s),
                            Value::Int(n) => crate::show::push_int(&mut out, *n),
                            other => out.push_str(&crate::show::show(types, other)),
                        }
                    }
                    out
                });
                Ok(Value::Str(Rc::from(text)))
            }
            Builtin::ListGuardCheck => match list.check_guard() {
                Ok(()) => Ok(Value::Unit),
                Err(v) => Err(self.violation(fiber, span, v)),
            },
            Builtin::ListKeepMask => {
                let Some(Value::List(mask)) = args.get(1) else {
                    return Err(self.internal(fiber, span, "ill-typed `retain`"));
                };
                if let Err(v) = list.check_guard() {
                    return Err(self.violation(fiber, span, v));
                }
                let keep = mask.snapshot();
                let items = list.snapshot();
                let kept: Vec<Value> = items
                    .iter()
                    .zip(&keep)
                    .filter(|(_, k)| is_true(k))
                    .map(|(v, _)| v.clone())
                    .collect();
                self.release_all(items);
                let old = list.replace_all(kept);
                self.release_all(old);
                Ok(Value::Unit)
            }
            Builtin::ListSetOrder | Builtin::ListPickOrder => {
                let Some(Value::List(order)) = args.get(1) else {
                    return Err(self.internal(fiber, span, "ill-typed sort"));
                };
                let in_place = op == Builtin::ListSetOrder;
                if in_place && let Err(v) = list.check_guard() {
                    return Err(self.violation(fiber, span, v));
                }
                let items = list.snapshot();
                let picked: Option<Vec<Value>> = order
                    .snapshot()
                    .iter()
                    .map(|i| match i {
                        Value::Int(i) => {
                            usize::try_from(*i).ok().and_then(|i| items.get(i)).cloned()
                        }
                        _ => None,
                    })
                    .collect();
                self.release_all(items);
                let Some(sorted) = picked else {
                    return Err(self.internal(fiber, span, "ill-typed sort"));
                };
                if !in_place {
                    return Ok(Value::list(sorted));
                }
                let old = list.replace_all(sorted);
                self.release_all(old);
                Ok(Value::Unit)
            }
            Builtin::ListSlice => self.list_slice(fiber, list, args, span),
            _ => Err(self.internal(fiber, span, "ill-typed built-in call")),
        }
    }

    fn not_ordered(&self, fiber: &mut Self::Fiber, span: Span) -> crate::ctl::Ctl {
        self.panic(fiber, span, "these values are not ordered (a `nan`)")
    }

    /// `sort` and `sorted`: a stable sort of a copy, written back whole. With `in_place` false
    /// (`sorted`) the list is left alone and the sorted copy is the result.
    fn sort_list(
        &mut self,
        fiber: &mut Self::Fiber,
        list: &Rc<ListObj>,
        in_place: bool,
        span: Span,
    ) -> Eval {
        if in_place && let Err(v) = list.check_guard() {
            return Err(self.violation(fiber, span, v));
        }
        let items = list.snapshot();
        let order = {
            let _hold = list.iter_hold();
            merge_order(
                self,
                fiber,
                items.len(),
                &mut |this, fiber, a, b| match this.cmp_deep(fiber, &items[a], &items[b], span)? {
                    Some(o) => Ok(o),
                    None => Err(this.not_ordered(fiber, span)),
                },
            )
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

    /// `xs[lo..hi]`: a copy of part of the list.
    fn list_slice(
        &mut self,
        fiber: &mut Self::Fiber,
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
    fn slice_bounds(
        &mut self,
        fiber: &mut Self::Fiber,
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
        self.slice_positions(fiber, (lo, hi), inclusive, len, what, span)
    }

    /// The `lo..hi` positions of a slice with these (optional) ends, or a panic.
    fn slice_positions(
        &mut self,
        fiber: &mut Self::Fiber,
        (lo, hi): (Option<i64>, Option<i64>),
        inclusive: bool,
        len: usize,
        what: &str,
        span: Span,
    ) -> Eval<(usize, usize)> {
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

impl<E: Engine> ListFnOps for E {}
