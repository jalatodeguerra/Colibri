//! Running iterators: the steps of a built-in iterator that may call user code.
//!
//! The iterator objects themselves are in `colibri_core::iter`.

use std::rc::Rc;

use colibri_check::types::{OPTION_TYPE, VARIANT_SOME};
use colibri_syntax::Span;

use crate::ctl::Eval;
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::value::Value;

pub(crate) use colibri_core::iter::IterObj;
use colibri_core::iter::Step;

/// The item inside a `T?` that `next` returned: `Some(item)` for `some(item)`.
fn item_of(next: &Value) -> Option<Option<Value>> {
    match next {
        Value::Enum(e) if e.ty == OPTION_TYPE => {
            if e.variant == VARIANT_SOME {
                e.payload.first().cloned().map(Some)
            } else {
                Some(None)
            }
        }
        _ => None,
    }
}

impl Interp {
    /// The next item of the iterator value `it` (a built-in iterator or a boxed user `Iter`),
    /// or `None` when it is exhausted. A user iterator that is a struct is advanced by value:
    /// its new state is stored back into `it`.
    pub(crate) fn iter_next(
        &mut self,
        fiber: &mut Fiber,
        it: &mut Value,
        span: Span,
    ) -> Eval<Option<Value>> {
        match it {
            Value::Iter(obj) => {
                let obj = Rc::clone(obj);
                self.native_next(fiber, &obj, span)
            }
            Value::Dyn(_) => self.user_next(fiber, it, span),
            _ => Err(self.internal(
                fiber,
                span,
                "a `for` over something that is not an iterator",
            )),
        }
    }

    /// One step of a built-in iterator.
    pub(crate) fn native_next(
        &mut self,
        fiber: &mut Fiber,
        obj: &Rc<IterObj>,
        span: Span,
    ) -> Eval<Option<Value>> {
        match obj.step() {
            Step::Item(v) => Ok(Some(v)),
            Step::End => Ok(None),
            Step::Inner(mut inner, index) => {
                // The inner iterator is out of its object while user code may run: a twin on
                // the temporaries keeps it alive on every path.
                let mark = fiber.temps.len();
                fiber.temps.push(inner.clone());
                let step = self.iter_next(fiber, &mut inner, span);
                let out = match step {
                    Ok(Some(item)) => {
                        obj.resume(inner, true);
                        Ok(Some(Value::tuple(vec![Value::Int(index), item])))
                    }
                    Ok(None) => {
                        obj.resume(inner, false);
                        Ok(None)
                    }
                    Err(ctl) => {
                        obj.resume(inner, true);
                        Err(ctl)
                    }
                };
                self.release_temps(fiber, mark);
                out
            }
        }
    }

    /// One step of a user iterator boxed in an interface value.
    fn user_next(&mut self, fiber: &mut Fiber, it: &mut Value, span: Span) -> Eval<Option<Value>> {
        let Value::Dyn(rc) = std::mem::replace(it, Value::Unit) else {
            return Err(self.internal(fiber, span, "not an interface value"));
        };
        let impl_id = rc.impl_id;
        let inner = match Rc::try_unwrap(rc) {
            Ok(d) => d.val,
            Err(rc) => rc.val.clone(),
        };
        if let Value::Iter(obj) = &inner {
            let obj = Rc::clone(obj);
            *it = Value::Dyn(Rc::new(crate::value::DynVal {
                impl_id,
                val: inner,
            }));
            return self.native_next(fiber, &obj, span);
        }
        let body = self
            .target_of(Some(impl_id), 0)
            .and_then(|f| self.fns.get(f.0 as usize).cloned());
        let Some(body) = body else {
            *it = Value::Dyn(Rc::new(crate::value::DynVal {
                impl_id,
                val: inner,
            }));
            return Err(self.internal(fiber, span, "an iterator without a `next`"));
        };
        let unchanged = (!body.mutates_self).then(|| inner.clone());
        let (result, out) = self.call_body_inout(fiber, body, vec![inner]);
        if let Some(updated) = out.or(unchanged) {
            *it = Value::Dyn(Rc::new(crate::value::DynVal {
                impl_id,
                val: updated,
            }));
        }
        let next = result?;
        let item = item_of(&next);
        self.release(next);
        match item {
            Some(item) => Ok(item),
            None => Err(self.internal(fiber, span, "`next` did not return an `Option`")),
        }
    }

    /// The iterator `v.iter()` hands out for a built-in collection or range, or `v` itself
    /// when it already is one.
    pub(crate) fn iterator_of(&mut self, v: &Value) -> Option<Value> {
        Some(Value::Iter(match v {
            Value::List(l) => IterObj::over_list(Rc::clone(l)),
            Value::Map(m) => IterObj::over_map(Rc::clone(m)),
            Value::Set(s) => IterObj::over_set(Rc::clone(s)),
            Value::Range(r) => IterObj::over_range(**r, false),
            Value::Bytes(b) => IterObj::over_bytes(Rc::clone(b)),
            Value::Iter(i) => Rc::clone(i),
            _ => return None,
        }))
    }
}
