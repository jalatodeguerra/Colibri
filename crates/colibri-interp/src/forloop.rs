//! `for` loops over ranges, lists, maps, sets and any iterator (DESIGN §6.3, §11.6).
//!
//! A loop over a collection holds the collection's mutation guard for its whole duration, and
//! keeps the collection itself on the temporaries stack. `for mut` holds the stronger place
//! guard and writes each iteration's final element back unless it is unchanged.

use std::rc::Rc;

use colibri_check::hir::{ForBinding, ForLoop, ForSource};
use colibri_syntax::Span;

use crate::ctl::{Ctl, Eval};
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::heap::ListObj;
use crate::value::Value;
use colibri_core::prelude::*;

/// Whether two values are the very same shared object (or string), so a write-back would
/// change nothing. Scalars are never reported as unchanged.
fn same_object(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => Rc::ptr_eq(x, y),
        (Value::Bytes(x), Value::Bytes(y)) => Rc::ptr_eq(x, y),
        (Value::Struct(x), Value::Struct(y)) => Rc::ptr_eq(x, y),
        (Value::Tuple(x), Value::Tuple(y)) => Rc::ptr_eq(x, y),
        (Value::Enum(x), Value::Enum(y)) => Rc::ptr_eq(x, y),
        (Value::List(x), Value::List(y)) => Rc::ptr_eq(x, y),
        (Value::Map(x), Value::Map(y)) => Rc::ptr_eq(x, y),
        (Value::Set(x), Value::Set(y)) => Rc::ptr_eq(x, y),
        (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
        (Value::Host(x), Value::Host(y)) => x.index == y.index && x.generation == y.generation,
        _ => false,
    }
}

impl Interp {
    #[inline(never)]
    pub(crate) fn eval_for(&mut self, fiber: &mut Fiber, f: &ForLoop, span: Span) -> Eval {
        let mark = fiber.temps.len();
        let result = self.run_for(fiber, f, span);
        self.release_temps(fiber, mark);
        result.map(|()| Value::Unit)
    }

    /// Evaluates the iterated expression and keeps it alive for the whole loop.
    fn loop_source(&mut self, fiber: &mut Fiber, e: &colibri_check::hir::Expr) -> Eval {
        let v = self.eval(fiber, e)?;
        fiber.temps.push(v.clone());
        Ok(v)
    }

    fn run_for(&mut self, fiber: &mut Fiber, f: &ForLoop, span: Span) -> Eval<()> {
        match &f.source {
            ForSource::Range {
                start,
                end,
                inclusive,
            } => {
                let (Value::Int(start), Value::Int(end)) =
                    (self.eval(fiber, start)?, self.eval(fiber, end)?)
                else {
                    return Err(self.internal(
                        fiber,
                        span,
                        "a `for` range is not an integer range",
                    ));
                };
                let mut i = start;
                while !((*inclusive && i > end) || (!*inclusive && i >= end)) {
                    if !self.run_iteration(fiber, f, Value::Int(i), None, span)? {
                        break;
                    }
                    // The last value of an inclusive range must not be stepped past (it may be i64::MAX).
                    if *inclusive && i == end {
                        break;
                    }
                    i += 1;
                }
                Ok(())
            }
            ForSource::List { list, name } => {
                let Value::List(rc) = self.loop_source(fiber, list)? else {
                    return Err(self.internal(fiber, span, "a `for` over a non-list"));
                };
                let _hold = if f.places {
                    rc.place_hold(name)
                } else {
                    rc.iter_hold()
                };
                let mut i = 0;
                while let Some(item) = rc.at(i) {
                    let back = f.places.then_some((&rc, i));
                    if !self.run_iteration(fiber, f, item, back, span)? {
                        break;
                    }
                    i += 1;
                }
                Ok(())
            }
            ForSource::Map(map) => {
                let Value::Map(rc) = self.loop_source(fiber, map)? else {
                    return Err(self.internal(fiber, span, "a `for` over a non-map"));
                };
                let _hold = rc.iter_hold();
                let mut pos = 0;
                while let Some((k, v, next)) = rc.next_entry(pos) {
                    pos = next;
                    if !self.run_iteration(fiber, f, Value::tuple(vec![k, v]), None, span)? {
                        break;
                    }
                }
                Ok(())
            }
            ForSource::Set(set) => {
                let Value::Set(rc) = self.loop_source(fiber, set)? else {
                    return Err(self.internal(fiber, span, "a `for` over a non-set"));
                };
                let _hold = rc.iter_hold();
                let mut pos = 0;
                while let Some((item, next)) = rc.next_item(pos) {
                    pos = next;
                    if !self.run_iteration(fiber, f, item, None, span)? {
                        break;
                    }
                }
                Ok(())
            }
            ForSource::Iter(e) => {
                let mut it = self.eval(fiber, e)?;
                // A built-in iterator is an object, kept alive on the temporaries; a boxed
                // user iterator is advanced by value, so this loop owns it alone.
                if matches!(it, Value::Iter(_)) {
                    fiber.temps.push(it.clone());
                }
                let result = self.run_iter_loop(fiber, f, &mut it, span);
                self.release(it);
                result
            }
        }
    }

    /// The trips of a loop over an iterator value: `next` until it says `none`.
    fn run_iter_loop(
        &mut self,
        fiber: &mut Fiber,
        f: &ForLoop,
        it: &mut Value,
        span: Span,
    ) -> Eval<()> {
        while let Some(item) = self.iter_next(fiber, it, span)? {
            if !self.run_iteration(fiber, f, item, None, span)? {
                break;
            }
        }
        Ok(())
    }

    /// One trip round the loop: binds the item, runs the body, leaves the iteration's scope,
    /// and writes a changed element back (`for mut`). Returns whether the loop goes on.
    fn run_iteration(
        &mut self,
        fiber: &mut Fiber,
        f: &ForLoop,
        item: Value,
        write_back: Option<(&Rc<ListObj>, usize)>,
        span: Span,
    ) -> Eval<bool> {
        if let Err(ctl) = self.step(fiber, span) {
            self.release(item);
            return Err(ctl);
        }
        let original = write_back.map(|_| item.clone());
        let mark = Self::scope_mark(fiber);
        if let Err(ctl) = self.bind_item(fiber, &f.binding, item, span) {
            self.exit_scope(fiber, mark, Ok(Value::Unit))?;
            return Err(ctl);
        }
        let result = self.eval_block(fiber, &f.body);
        // The element's final value is taken before the iteration's bindings are released.
        let last = match (&f.binding, write_back) {
            (ForBinding::Local(local), Some(_)) => fiber
                .frames
                .last_mut()
                .and_then(|fr| fr.slots.get_mut(local.0 as usize))
                .map(|slot| crate::eval::unwrap_cell(std::mem::replace(slot, Value::Unit))),
            _ => None,
        };
        let mut result = self.exit_scope(fiber, mark, result);
        if let (Some((list, i)), Some(original), Some(last)) = (write_back, original, last) {
            if same_object(&original, &last) {
                self.release(last);
            } else {
                match list.write_back(i as i64, last) {
                    Ok(old) => self.release(old),
                    Err((_, unplaced)) => self.release(unplaced),
                }
            }
            self.release(original);
            // A hook panic the write-back raised folds here, not at some later statement end.
            result = self.fold_hook_panics(result);
        }
        match result {
            Ok(v) => {
                self.release(v);
                Ok(true)
            }
            Err(Ctl::Continue(target)) if target == f.id => Ok(true),
            Err(Ctl::Break(target, v)) if target == f.id => {
                self.release(v);
                Ok(false)
            }
            Err(other) => Err(other),
        }
    }

    /// Binds one iteration's item to the loop's binding.
    fn bind_item(
        &mut self,
        fiber: &mut Fiber,
        binding: &ForBinding,
        item: Value,
        span: Span,
    ) -> Eval<()> {
        match binding {
            ForBinding::None => self.release(item),
            ForBinding::Local(local) => self.bind_local(fiber, *local, item),
            ForBinding::Pat(pat) => {
                let mut binds = Vec::new();
                let matched = self.match_pat(pat, &item, &mut binds);
                self.release(item);
                if !matched {
                    self.release_all(binds.into_iter().map(|(_, v)| v).collect());
                    if let Some(at) = self.pat_gave_up.take() {
                        return Err(self.gave_up(fiber, at, None));
                    }
                    return Err(self.internal(fiber, span, "an irrefutable `for` pattern failed"));
                }
                self.bind_all(fiber, binds);
            }
        }
        Ok(())
    }
}
