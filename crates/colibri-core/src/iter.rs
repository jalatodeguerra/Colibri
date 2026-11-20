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

use crate::heap::{GuardToken, ListObj, MapObj, SetObj};
use crate::value::{RangeVal, Value};

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
