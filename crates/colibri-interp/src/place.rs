//! Reading and writing places: an anchor (a local slot or a module-level value) plus a path
//! of struct-field and tuple-element projections (ARCHITECTURE §4).
//!
//! A write walks the path with `Rc::make_mut`, so a struct that is shared with another
//! variable is copied exactly once, at the first write, and a uniquely owned one is changed in
//! place.

use std::rc::Rc;

use colibri_check::hir::{Anchor, Place, ValueProj};
use colibri_syntax::Span;

use crate::ctl::Eval;
use crate::eval::Interp;
use crate::fiber::Fiber;
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

impl Interp {
    fn slot<'f>(&'f self, fiber: &'f Fiber, anchor: &Anchor) -> Option<&'f Value> {
        match anchor {
            Anchor::Local(id) => fiber.frames.last()?.slots.get(id.0 as usize),
            Anchor::Global(id) => self.globals.get(id.0 as usize),
            _ => None,
        }
    }

    fn slot_mut<'f>(&'f mut self, fiber: &'f mut Fiber, anchor: &Anchor) -> Option<&'f mut Value> {
        match anchor {
            Anchor::Local(id) => fiber.frames.last_mut()?.slots.get_mut(id.0 as usize),
            Anchor::Global(id) => self.globals.get_mut(id.0 as usize),
            _ => None,
        }
    }

    /// The value at `place`; for a struct or tuple that is a pointer bump, not a deep copy.
    pub(crate) fn read_place(&mut self, fiber: &mut Fiber, place: &Place, span: Span) -> Eval {
        let found = self.slot(fiber, &place.anchor).and_then(|root| {
            place
                .path
                .iter()
                .try_fold(root, |value, &proj| project(value, proj))
        });
        match found {
            Some(v) => Ok(v.clone()),
            None => Err(self.internal(fiber, span, "a place does not exist")),
        }
    }

    /// Stores `value` at `place` and returns the value it replaced.
    pub(crate) fn write_place(
        &mut self,
        fiber: &mut Fiber,
        place: &Place,
        value: Value,
        span: Span,
    ) -> Eval {
        let slot = self.slot_mut(fiber, &place.anchor).and_then(|root| {
            place
                .path
                .iter()
                .try_fold(root, |value, &proj| project_mut(value, proj))
        });
        match slot {
            Some(slot) => Ok(std::mem::replace(slot, value)),
            None => Err(self.internal(fiber, span, "a place does not exist")),
        }
    }

    /// The value of a `var self` receiver, handed to the callee. A local is moved out (nothing
    /// else can see it meanwhile, and the callee then owns it uniquely, so it changes in
    /// place); a module-level value is copied, so the callee reading the same variable still
    /// sees the old value until the write-back.
    pub(crate) fn take_place(&mut self, fiber: &mut Fiber, place: &Place, span: Span) -> Eval {
        if matches!(place.anchor, Anchor::Global(_)) {
            return self.read_place(fiber, place, span);
        }
        self.write_place(fiber, place, Value::Unit, span)
    }
}
