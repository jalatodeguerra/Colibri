//! Iterator objects of the built-in collections, strings and ranges (DESIGN §6.3, §9.2).
//!
//! `xs.iter()`, `m.iter()`, `s.chars()`, `r.rev()` and `it.enumerate()` hand out an
//! [`IterObj`], a value of type `Iter[T]` that the engine implements itself. An iterator over
//! a collection holds the collection's iteration guard from its creation until it runs out or
//! dies, so changing the collection meanwhile panics ("list modified during iteration"),
//! exactly as during a `for` loop (§6.3, §11.6). The guard is released as soon as `next` first
//! returns `none`, so a finished iterator does not keep the collection locked.
//!
//! The state sits in a `RefCell`, but every borrow is confined to the functions here, none of
//! which runs Colibri code.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use colibri_check::types::{OPTION_TYPE, VARIANT_SOME};
use colibri_syntax::Span;

use crate::ctl::Eval;
use crate::dispatch::Dispatch;
use crate::engine::Engine;
use crate::heap::{GuardToken, ListObj, MapObj, SetObj};
use crate::protocol::Protocol;
use crate::release::Release;
use crate::value::{DynVal, RangeVal, Value};

enum State {
    List {
        list: Rc<ListObj>,
        pos: usize,
        hold: Option<GuardToken>,
    },
    Map {
        map: Rc<MapObj>,
        pos: usize,
        hold: Option<GuardToken>,
    },
    Set {
        set: Rc<SetObj>,
        pos: usize,
        hold: Option<GuardToken>,
    },
    /// The integers `lo..=hi` (already normalised), forwards or backwards.
    Range {
        lo: i64,
        hi: i64,
        rev: bool,
    },
    Chars {
        text: Rc<str>,
        pos: usize,
    },
    /// The bytes of a `Bytes`.
    Octets {
        data: Rc<[u8]>,
        pos: usize,
    },
    Bytes {
        text: Rc<str>,
        pos: usize,
    },
    /// `(index, item)` pairs of another iterator (an `Iter` value).
    Enumerate {
        inner: Value,
        index: i64,
    },
    Done,
}

/// What one step of an iterator produced.
pub enum Step {
    Item(Value),
    End,
    /// An enumerating iterator needs the next item of its inner iterator, which may be user
    /// code. The inner value is moved out for the call; give it back with [`IterObj::resume`].
    Inner(Value, i64),
}

/// A built-in iterator.
pub struct IterObj {
    state: RefCell<State>,
}

impl IterObj {
    fn new(state: State) -> Rc<IterObj> {
        Rc::new(IterObj {
            state: RefCell::new(state),
        })
    }

    pub fn over_list(list: Rc<ListObj>) -> Rc<IterObj> {
        let hold = Some(list.iter_hold());
        Self::new(State::List { list, pos: 0, hold })
    }

    pub fn over_map(map: Rc<MapObj>) -> Rc<IterObj> {
        let hold = Some(map.iter_hold());
        Self::new(State::Map { map, pos: 0, hold })
    }

    pub fn over_set(set: Rc<SetObj>) -> Rc<IterObj> {
        let hold = Some(set.iter_hold());
        Self::new(State::Set { set, pos: 0, hold })
    }

    /// The integers of `r`, counting up or (`rev`) down.
    pub fn over_range(r: RangeVal, rev: bool) -> Rc<IterObj> {
        if r.is_empty() {
            return Self::new(State::Done);
        }
        let hi = if r.inclusive { r.end } else { r.end - 1 };
        Self::new(State::Range {
            lo: r.start,
            hi,
            rev,
        })
    }

    pub fn chars(text: Rc<str>) -> Rc<IterObj> {
        Self::new(State::Chars { text, pos: 0 })
    }

    pub fn bytes(text: Rc<str>) -> Rc<IterObj> {
        Self::new(State::Bytes { text, pos: 0 })
    }

    pub fn over_bytes(data: Rc<[u8]>) -> Rc<IterObj> {
        Self::new(State::Octets { data, pos: 0 })
    }

    pub fn enumerate(inner: Value) -> Rc<IterObj> {
        Self::new(State::Enumerate { inner, index: 0 })
    }

    /// The next item, or what is needed to find it.
    pub fn step(&self) -> Step {
        let mut state = self.state.borrow_mut();
        let step = match &mut *state {
            State::List { list, pos, .. } => match list.at(*pos) {
                Some(v) => {
                    *pos += 1;
                    Step::Item(v)
                }
                None => Step::End,
            },
            State::Map { map, pos, .. } => match map.next_entry(*pos) {
                Some((k, v, next)) => {
                    *pos = next;
                    Step::Item(Value::tuple(vec![k, v]))
                }
                None => Step::End,
            },
            State::Set { set, pos, .. } => match set.next_item(*pos) {
                Some((v, next)) => {
                    *pos = next;
                    Step::Item(v)
                }
                None => Step::End,
            },
            State::Range { lo, hi, rev } => {
                let v = if *rev { *hi } else { *lo };
                if *lo == *hi {
                    *state = State::Done;
                } else if *rev {
                    *hi -= 1;
                } else {
                    *lo += 1;
                }
                Step::Item(Value::Int(v))
            }
            State::Chars { text, pos } => match text[*pos..].chars().next() {
                Some(c) => {
                    *pos += c.len_utf8();
                    Step::Item(Value::Char(c))
                }
                None => Step::End,
            },
            State::Bytes { text, pos } => match text.as_bytes().get(*pos) {
                Some(&b) => {
                    *pos += 1;
                    Step::Item(Value::Int(i64::from(b)))
                }
                None => Step::End,
            },
            State::Octets { data, pos } => match data.get(*pos) {
                Some(&b) => {
                    *pos += 1;
                    Step::Item(Value::Int(i64::from(b)))
                }
                None => Step::End,
            },
            State::Enumerate { inner, index } => {
                let taken = std::mem::replace(inner, Value::Unit);
                return Step::Inner(taken, *index);
            }
            State::Done => Step::End,
        };
        if matches!(step, Step::End) {
            // Out of items: let go of the collection's guard for good.
            *state = State::Done;
        }
        step
    }

    /// Puts an enumerating iterator's inner iterator back after a step; `produced` says
    /// whether it gave an item (otherwise this iterator is finished).
    pub fn resume(&self, inner: Value, produced: bool) {
        let mut state = self.state.borrow_mut();
        if let State::Enumerate { inner: slot, index } = &mut *state {
            *slot = inner;
            if produced {
                *index += 1;
            } else {
                *state = State::Done;
            }
        }
    }

    /// The values the iterator keeps alive (fresh handles), for the leak detector.
    pub fn children(&self) -> Vec<Value> {
        match &*self.state.borrow() {
            State::List { list, .. } => vec![Value::List(Rc::clone(list))],
            State::Map { map, .. } => vec![Value::Map(Rc::clone(map))],
            State::Set { set, .. } => vec![Value::Set(Rc::clone(set))],
            State::Enumerate { inner, .. } => vec![inner.clone()],
            _ => Vec::new(),
        }
    }

    /// Takes the values the iterator keeps alive (its collection, its inner iterator), for
    /// destruction. Only `release` calls this.
    pub fn take_inner(&self) -> Vec<Value> {
        let state = std::mem::replace(&mut *self.state.borrow_mut(), State::Done);
        match state {
            State::List { list, hold, .. } => {
                drop(hold);
                vec![Value::List(list)]
            }
            State::Map { map, hold, .. } => {
                drop(hold);
                vec![Value::Map(map)]
            }
            State::Set { set, hold, .. } => {
                drop(hold);
                vec![Value::Set(set)]
            }
            State::Enumerate { inner, .. } => vec![inner],
            _ => Vec::new(),
        }
    }
}

impl fmt::Debug for IterObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Iter")
    }
}

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

/// Running iterators: the steps of a built-in iterator that may call user code.
#[doc(hidden)]
pub trait IterOps: Engine {
    /// The next item of the iterator value `it` (a built-in iterator or a boxed user `Iter`),
    /// or `None` when it is exhausted. A user iterator that is a struct is advanced by value:
    /// its new state is stored back into `it`.
    fn iter_next(
        &mut self,
        fiber: &mut Self::Fiber,
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
    fn native_next(
        &mut self,
        fiber: &mut Self::Fiber,
        obj: &Rc<IterObj>,
        span: Span,
    ) -> Eval<Option<Value>> {
        match obj.step() {
            Step::Item(v) => Ok(Some(v)),
            Step::End => Ok(None),
            Step::Inner(mut inner, index) => {
                // The inner iterator is out of its object while user code may run: a twin on
                // the temporaries keeps it alive on every path.
                let mark = Self::temp_mark(fiber);
                Self::hold_temp(fiber, inner.clone());
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
    fn user_next(
        &mut self,
        fiber: &mut Self::Fiber,
        it: &mut Value,
        span: Span,
    ) -> Eval<Option<Value>> {
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
            *it = Value::Dyn(Rc::new(DynVal {
                impl_id,
                val: inner,
            }));
            return self.native_next(fiber, &obj, span);
        }
        let target = self
            .target_of(Some(impl_id), 0)
            .filter(|f| self.fn_span(*f).is_some());
        let Some(f) = target else {
            *it = Value::Dyn(Rc::new(DynVal {
                impl_id,
                val: inner,
            }));
            return Err(self.internal(fiber, span, "an iterator without a `next`"));
        };
        let mutates = self.fn_mutates_self(f);
        let unchanged = (!mutates).then(|| inner.clone());
        let (result, out) = self.call_fn_inout(fiber, f, vec![inner]);
        if let Some(updated) = out.or(unchanged) {
            *it = Value::Dyn(Rc::new(DynVal {
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
    fn iterator_of(&mut self, v: &Value) -> Option<Value> {
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

impl<E: Engine> IterOps for E {}
