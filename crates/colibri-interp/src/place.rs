//! Reading and writing places (ARCHITECTURE §4, DESIGN §11.6).
//!
//! A place is an anchor plus a path of struct-field and tuple-element projections. The anchor
//! is a local, a module-level value, a field of a class object, an element of a list or an
//! entry of a map. Collections and objects are references, so an anchor names the *object*
//! and the path continues through value types inside it.
//!
//! The operands of an anchor (the list and its index, the map and its key, the object) are
//! evaluated exactly once into a [`Resolved`] anchor, kept alive on the temporaries stack;
//! reading and writing are then separate steps, so `xs[f()].hp -= 1` calls `f` once and the
//! bounds are checked when the access happens. A write walks the path with `Rc::make_mut`, so
//! a struct shared with another variable is copied exactly once, at the first write.
//!
//! A write from user code is *guarded*: it is refused while the collection is being iterated
//! or has a live place. The write-back of a `var self` call or a `for var` iteration is not,
//! because that place holds the guard itself.

use std::rc::Rc;

use colibri_check::hir::{Anchor, GlobalId, LocalId, Place, ValueProj};
use colibri_syntax::Span;

use crate::ctl::Eval;
use crate::eval::Interp;
use crate::fiber::Fiber;
use crate::heap::{ClassObj, GuardToken, ListObj, MapObj};
use crate::value::Value;

/// The part of `v` a projection selects.
pub(crate) fn project(v: &Value, proj: ValueProj) -> Option<&Value> {
    match (v, proj) {
        (Value::Struct(s), ValueProj::Field(i)) => s.fields.get(i.0 as usize),
        (Value::Tuple(items), ValueProj::TupleIdx(i)) => items.get(i as usize),
        _ => None,
    }
}

/// The part of `v` a projection selects, made uniquely owned so it can be changed.
fn project_mut(v: &mut Value, proj: ValueProj) -> Option<&mut Value> {
    match (v, proj) {
        (Value::Struct(s), ValueProj::Field(i)) => Rc::make_mut(s).fields.get_mut(i.0 as usize),
        (Value::Tuple(items), ValueProj::TupleIdx(i)) => Rc::make_mut(items).get_mut(i as usize),
        _ => None,
    }
}

/// Replaces the part of `root` at `path` with the value in `new` (leaving `None` behind) and
/// returns what was there. `None` without touching `new` when the path does not exist.
fn replace_at(root: &mut Value, path: &[ValueProj], new: &mut Option<Value>) -> Option<Value> {
    let slot = path.iter().try_fold(root, |v, &p| project_mut(v, p))?;
    let value = new.take()?;
    Some(std::mem::replace(slot, value))
}

/// The part of `root` at `path`, cloned.
fn read_at(root: &Value, path: &[ValueProj]) -> Option<Value> {
    path.iter().try_fold(root, |v, &p| project(v, p)).cloned()
}

/// An anchor whose operands have been evaluated.
pub(crate) enum Resolved {
    Local(LocalId),
    Global(GlobalId),
    Field(Rc<ClassObj>, usize),
    Elem(Rc<ListObj>, i64),
    Entry(Rc<MapObj>, Value),
}

/// A live place guard on a collection, released when dropped.
pub(crate) type PlaceHold = Option<GuardToken>;

impl Interp {
    /// Evaluates the operands of `place`'s anchor once. The values stay on the temporaries
    /// stack until the statement ends, so they outlive every use of the result.
    pub(crate) fn resolve_anchor(
        &mut self,
        fiber: &mut Fiber,
        place: &Place,
        span: Span,
    ) -> Eval<Resolved> {
        match &place.anchor {
            Anchor::Local(id) => Ok(Resolved::Local(*id)),
            Anchor::Global(id) => Ok(Resolved::Global(*id)),
            Anchor::ClassField(obj, idx) => {
                let v = self.eval(fiber, obj)?;
                fiber.temps.push(v.clone());
                match v {
                    Value::Obj(o) => Ok(Resolved::Field(o, idx.0 as usize)),
                    _ => Err(self.internal(
                        fiber,
                        span,
                        "a field of something that is not an object",
                    )),
                }
            }
            Anchor::ListElem(list, index, _) => {
                let lv = self.eval(fiber, list)?;
                fiber.temps.push(lv.clone());
                let iv = self.eval(fiber, index)?;
                match (lv, iv) {
                    (Value::List(l), Value::Int(i)) => Ok(Resolved::Elem(l, i)),
                    _ => Err(self.internal(fiber, span, "an ill-typed list index")),
                }
            }
            Anchor::MapEntry(map, key, _) => {
                let mv = self.eval(fiber, map)?;
                fiber.temps.push(mv.clone());
                let kv = self.eval(fiber, key)?;
                fiber.temps.push(kv.clone());
                match mv {
                    Value::Map(m) => Ok(Resolved::Entry(m, kv)),
                    _ => Err(self.internal(fiber, span, "an ill-typed map index")),
                }
            }
        }
    }

    /// The part of the place a resolved anchor and path name, cloned.
    pub(crate) fn get_resolved(
        &mut self,
        fiber: &mut Fiber,
        r: &Resolved,
        path: &[ValueProj],
        span: Span,
    ) -> Eval {
        let found = match r {
            Resolved::Local(id) => fiber
                .frames
                .last()
                .and_then(|f| f.slots.get(id.0 as usize))
                .and_then(|root| read_at(root, path)),
            Resolved::Global(id) => self
                .globals
                .get(id.0 as usize)
                .and_then(|root| read_at(root, path)),
            Resolved::Field(obj, idx) => obj.field(*idx).and_then(|root| read_at(&root, path)),
            Resolved::Elem(list, i) => match list.get(*i) {
                Ok(root) => read_at(&root, path),
                Err(e) => return Err(self.bounds_panic(fiber, span, "index", e)),
            },
            Resolved::Entry(map, key) => match map.get(key) {
                Some(root) => read_at(&root, path),
                None => {
                    let message = format!("key {} not found in map", self.shown_key(key));
                    return Err(self.panic(fiber, span, message));
                }
            },
        };
        found.ok_or_else(|| self.internal(fiber, span, "a place does not exist"))
    }

    /// How a key is written in a panic message.
    fn shown_key(&self, key: &Value) -> String {
        match key {
            Value::Str(s) => format!("{s:?}"),
            other => self.shown(other),
        }
    }

    /// Stores `value` in the place and returns the value it replaced. `guarded` says whether
    /// the collection's mutation guard applies (a write from user code) or not (the
    /// write-back of a place that holds the guard). On failure `value` is released.
    pub(crate) fn write_resolved(
        &mut self,
        fiber: &mut Fiber,
        r: &Resolved,
        path: &[ValueProj],
        value: Value,
        guarded: bool,
        span: Span,
    ) -> Eval {
        let mut new = Some(value);
        let outcome = self.try_write(fiber, r, path, &mut new, guarded, span);
        if let Some(unused) = new {
            self.release(unused);
        }
        outcome
    }

    fn try_write(
        &mut self,
        fiber: &mut Fiber,
        r: &Resolved,
        path: &[ValueProj],
        new: &mut Option<Value>,
        guarded: bool,
        span: Span,
    ) -> Eval {
        let old = match r {
            Resolved::Local(id) => fiber
                .frames
                .last_mut()
                .and_then(|f| f.slots.get_mut(id.0 as usize))
                .and_then(|root| replace_at(root, path, new)),
            Resolved::Global(id) => self
                .globals
                .get_mut(id.0 as usize)
                .and_then(|root| replace_at(root, path, new)),
            Resolved::Field(obj, idx) => obj
                .update_field(*idx, |root| replace_at(root, path, new))
                .flatten(),
            Resolved::Elem(list, i) => {
                match list.update(*i, guarded, |root| replace_at(root, path, new)) {
                    Ok(old) => old,
                    Err(e) => return Err(self.list_error(fiber, span, "index", e)),
                }
            }
            Resolved::Entry(map, key) => {
                match map.update(key, guarded, |root| replace_at(root, path, new)) {
                    Err(v) => return Err(self.panic(fiber, span, v.0)),
                    Ok(Some(old)) => old,
                    Ok(None) if path.is_empty() => {
                        return self.insert_entry(fiber, map, key, new, span);
                    }
                    Ok(None) => {
                        let message = format!("key {} not found in map", self.shown_key(key));
                        return Err(self.panic(fiber, span, message));
                    }
                }
            }
        };
        old.ok_or_else(|| self.internal(fiber, span, "a place does not exist"))
    }

    /// `m[k] = v` for a key the map does not have yet: adds the entry. Nothing is replaced.
    fn insert_entry(
        &mut self,
        fiber: &mut Fiber,
        map: &Rc<MapObj>,
        key: &Value,
        new: &mut Option<Value>,
        span: Span,
    ) -> Eval {
        let Some(value) = new.take() else {
            return Err(self.internal(fiber, span, "a place was written twice"));
        };
        match map.insert(key.clone(), value) {
            Ok(replaced) => {
                if let Some(spare) = replaced.spare_key {
                    self.release(spare);
                }
                Ok(replaced.old.unwrap_or(Value::Unit))
            }
            Err(rejected) => {
                let (k, v) = rejected.args;
                self.release(k);
                self.release(v);
                Err(self.panic(fiber, span, rejected.violation.0))
            }
        }
    }

    /// The value at `place`; for a struct or tuple that is a pointer bump, not a deep copy.
    pub(crate) fn read_place(&mut self, fiber: &mut Fiber, place: &Place, span: Span) -> Eval {
        let r = self.resolve_anchor(fiber, place, span)?;
        self.get_resolved(fiber, &r, &place.path, span)
    }

    /// Holds the mutation guard of the collection a resolved anchor is an element of, for the
    /// life of the returned token (a `var self` call or a `for var` loop). Anchors that are
    /// not collection elements need no guard.
    pub(crate) fn hold_place(&self, r: &Resolved, place: &Place) -> PlaceHold {
        let name = match &place.anchor {
            Anchor::ListElem(_, _, name) | Anchor::MapEntry(_, _, name) => name.as_str(),
            _ => return None,
        };
        match r {
            Resolved::Elem(list, _) => Some(list.place_hold(name)),
            Resolved::Entry(map, _) => Some(map.place_hold(name)),
            _ => None,
        }
    }

    /// The value of a `var self` receiver of a local or module-level place, handed to the
    /// callee. A local is moved out (nothing else can see it meanwhile, and the callee then
    /// owns it uniquely, so it changes in place); anything else is copied in, and the callee's
    /// final value replaces the original at write-back (ARCHITECTURE §4).
    pub(crate) fn take_receiver(
        &mut self,
        fiber: &mut Fiber,
        r: &Resolved,
        path: &[ValueProj],
        span: Span,
    ) -> Eval {
        if matches!(r, Resolved::Local(_)) {
            return self.write_resolved(fiber, r, path, Value::Unit, false, span);
        }
        self.get_resolved(fiber, r, path, span)
    }
}
