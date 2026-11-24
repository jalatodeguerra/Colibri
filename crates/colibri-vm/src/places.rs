//! Places and `for` loops at run time: reading and writing the part of a value that a place
//! names, the mutation guard, and the steps of a loop over a collection or an iterator
//! (vm-plan §1.3, §1.5, §2.5). The rules are the tree-walker's `place.rs` and `forloop.rs`.
//!
//! A place is an anchor (a local, a cell, a global, a class field, a list element or a map
//! entry) plus a path of struct and tuple projections. The compiler evaluates the anchor's
//! operands once into registers; these instructions read and write through them. A write walks
//! the path with `Rc::make_mut`, so a struct shared with another variable is copied once, at the
//! first write. A guarded write is refused while the collection is iterated or has a place held.

use std::rc::Rc;

use colibri_check::hir::ValueProj;
use colibri_core::ctl::{Ctl, Eval};
use colibri_core::prelude::*;
use colibri_core::value::Value;
use colibri_syntax::Span;

use crate::bytecode::{AnchorKind, Chunk, Op, PlaceSite, Site};
use crate::fiber::{VmFiber, is_scalar_value};
use crate::heap::project;
use crate::machine::Machine;

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

/// The part of the owned `root` at `path`.
fn part_of(root: Value, path: &[ValueProj]) -> Option<Value> {
    if path.is_empty() {
        Some(root)
    } else {
        read_at(&root, path)
    }
}

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

impl Machine {
    /// The site word after a place instruction (`pc` is past it).
    fn place_site<'c>(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &'c Chunk,
        pc: usize,
    ) -> Result<&'c PlaceSite, Ctl> {
        if let Op::Site { site } = chunk.code[pc - 1]
            && let Some(Site::Place(p)) = chunk.sites.get(usize::from(site))
        {
            return Ok(p);
        }
        let span = chunk.spans[pc - 1];
        Err(self.internal(fiber, span, "a place instruction without its site"))
    }

    /// How a key is written in a panic message.
    fn shown_key(&self, key: &Value) -> String {
        match key {
            Value::Str(s) => format!("{s:?}"),
            other => self.shown(other),
        }
    }

    /// The instructions that read and write places, with the site word after each.
    #[inline(never)]
    pub(crate) fn op_place(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let site = self.place_site(fiber, chunk, pc)?;
        let r = |i: u16| base + usize::from(i);
        match op {
            Op::PlaceGet { d, a, b } => {
                let v = self.place_read(fiber, site, r(a), r(b), span)?;
                Self::put_reg(fiber, r(d), v);
            }
            Op::PlaceTake { d, a, .. } => {
                let mut new = Some(Value::Unit);
                match replace_at(&mut fiber.regs[r(a)], &site.path, &mut new) {
                    Some(old) => Self::put_reg(fiber, r(d), old),
                    None => return Err(self.internal(fiber, span, "a place does not exist")),
                }
            }
            Op::PlaceSet { a, b, v } => {
                let value = std::mem::replace(&mut fiber.regs[r(v)], Value::Unit);
                let old = self.place_write(fiber, site, r(a), r(b), value, span)?;
                if !is_scalar_value(&old) {
                    self.release(old);
                }
            }
            Op::PlaceHold { a } => {
                let token = match (site.anchor, &fiber.regs[r(a)]) {
                    (AnchorKind::Elem, Value::List(l)) => l.place_hold(&site.name),
                    (AnchorKind::Entry, Value::Map(m)) => m.place_hold(&site.name),
                    _ => {
                        return Err(self.internal(
                            fiber,
                            span,
                            "a hold on something that is not a collection element",
                        ));
                    }
                };
                fiber.holds.push(token);
            }
            _ => return Err(self.internal(fiber, span, "not a place instruction")),
        }
        if matches!(site.anchor, AnchorKind::Entry) && colibri_core::compare::take_gave_up() {
            let result = match op {
                Op::PlaceGet { d, .. } => Some(r(d)),
                _ => None,
            };
            return Err(self.lookup_gave_up(fiber, chunk, pc, result));
        }
        Ok(())
    }

    /// The part of the place, cloned.
    fn place_read(
        &mut self,
        fiber: &mut VmFiber,
        site: &PlaceSite,
        ra: usize,
        rb: usize,
        span: Span,
    ) -> Eval {
        let path = &site.path[..];
        let found = match site.anchor {
            AnchorKind::Local => read_at(&fiber.regs[ra], path),
            AnchorKind::Cell => match &fiber.regs[ra] {
                Value::Cell(c) => c.with(|root| read_at(root, path)),
                _ => None,
            },
            AnchorKind::Global(g) => self
                .globals
                .get(g as usize)
                .and_then(|root| read_at(root, path)),
            AnchorKind::Field(idx) => match &fiber.regs[ra] {
                Value::Obj(o) => o
                    .field(usize::from(idx))
                    .and_then(|root| part_of(root, path)),
                _ => {
                    return Err(self.internal(
                        fiber,
                        span,
                        "a field of something that is not an object",
                    ));
                }
            },
            AnchorKind::Elem => {
                let got = match (&fiber.regs[ra], &fiber.regs[rb]) {
                    (Value::List(l), Value::Int(i)) => l.get(*i),
                    _ => return Err(self.internal(fiber, span, "an ill-typed list index")),
                };
                match got {
                    Ok(root) => part_of(root, path),
                    Err(e) => return Err(self.bounds_panic(fiber, span, "index", e)),
                }
            }
            AnchorKind::Entry => {
                let got = match (&fiber.regs[ra], &fiber.regs[rb]) {
                    (Value::Map(m), key) => m.get(key).ok_or_else(|| self.shown_key(key)),
                    _ => return Err(self.internal(fiber, span, "an ill-typed map index")),
                };
                match got {
                    Ok(root) => part_of(root, path),
                    Err(key) => {
                        let message = format!("key {key} not found in map");
                        return Err(self.panic(fiber, span, message));
                    }
                }
            }
        };
        found.ok_or_else(|| self.internal(fiber, span, "a place does not exist"))
    }

    /// Stores `value` in the place and returns the value it replaced. On failure `value` is
    /// released.
    fn place_write(
        &mut self,
        fiber: &mut VmFiber,
        site: &PlaceSite,
        ra: usize,
        rb: usize,
        value: Value,
        span: Span,
    ) -> Eval {
        let mut new = Some(value);
        let outcome = self.try_write(fiber, site, ra, rb, &mut new, span);
        if let Some(unused) = new {
            self.release(unused);
        }
        outcome
    }

    fn try_write(
        &mut self,
        fiber: &mut VmFiber,
        site: &PlaceSite,
        ra: usize,
        rb: usize,
        new: &mut Option<Value>,
        span: Span,
    ) -> Eval {
        let path = &site.path[..];
        let old = match site.anchor {
            AnchorKind::Local => replace_at(&mut fiber.regs[ra], path, new),
            AnchorKind::Cell => match &fiber.regs[ra] {
                Value::Cell(c) => c.update(|root| replace_at(root, path, new)),
                _ => None,
            },
            AnchorKind::Global(g) => self
                .globals
                .get_mut(g as usize)
                .and_then(|root| replace_at(root, path, new)),
            AnchorKind::Field(idx) => match &fiber.regs[ra] {
                Value::Obj(o) => o
                    .update_field(usize::from(idx), |root| replace_at(root, path, new))
                    .flatten(),
                _ => None,
            },
            AnchorKind::Elem => {
                let done = match (&fiber.regs[ra], &fiber.regs[rb]) {
                    (Value::List(l), Value::Int(i)) => {
                        l.update(*i, site.guarded, |root| replace_at(root, path, new))
                    }
                    _ => return Err(self.internal(fiber, span, "an ill-typed list index")),
                };
                match done {
                    Ok(old) => old,
                    Err(e) => return Err(self.list_error(fiber, span, "index", e)),
                }
            }
            AnchorKind::Entry => {
                let (map, key) = match (&fiber.regs[ra], &fiber.regs[rb]) {
                    (Value::Map(m), key) => (Rc::clone(m), key.clone()),
                    _ => return Err(self.internal(fiber, span, "an ill-typed map index")),
                };
                match map.update(&key, site.guarded, |root| replace_at(root, path, new)) {
                    Err(v) => return Err(self.panic(fiber, span, v.0)),
                    Ok(Some(old)) => old,
                    Ok(None) if path.is_empty() => {
                        return self.insert_entry(fiber, &map, key, new, span);
                    }
                    Ok(None) => {
                        let message = format!("key {} not found in map", self.shown_key(&key));
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
        fiber: &mut VmFiber,
        map: &Rc<colibri_core::heap::MapObj>,
        key: Value,
        new: &mut Option<Value>,
        span: Span,
    ) -> Eval {
        let Some(value) = new.take() else {
            return Err(self.internal(fiber, span, "a place was written twice"));
        };
        match map.insert(key, value) {
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

    /// `xs[i]` and `m[k]` with an empty path: the instructions of the commonest places, which
    /// need no site (they do what `place_read` and `try_write` do for the same anchors).
    #[inline(never)]
    pub(crate) fn op_elem(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let r = |i: u16| base + usize::from(i);
        match op {
            Op::ListGetAt { d, l, i } => {
                let got = match (&fiber.regs[r(l)], &fiber.regs[r(i)]) {
                    (Value::List(l), Value::Int(i)) => l.get(*i),
                    _ => return Err(self.internal(fiber, span, "an ill-typed list index")),
                };
                match got {
                    Ok(v) => Self::put_reg(fiber, r(d), v),
                    Err(e) => return Err(self.bounds_panic(fiber, span, "index", e)),
                }
            }
            Op::ListSetAt { l, i, v } => {
                let value = std::mem::replace(&mut fiber.regs[r(v)], Value::Unit);
                let done = match (&fiber.regs[r(l)], &fiber.regs[r(i)]) {
                    (Value::List(l), Value::Int(i)) => l.assign(*i, value),
                    _ => {
                        self.release(value);
                        return Err(self.internal(fiber, span, "an ill-typed list index"));
                    }
                };
                match done {
                    Ok(old) => {
                        if !is_scalar_value(&old) {
                            self.release(old);
                        }
                    }
                    Err((e, unplaced)) => {
                        self.release(unplaced);
                        return Err(self.list_error(fiber, span, "index", e));
                    }
                }
            }
            Op::MapGetAt { d, m, k } => {
                let got = match (&fiber.regs[r(m)], &fiber.regs[r(k)]) {
                    (Value::Map(m), key) => m.get(key).ok_or_else(|| self.shown_key(key)),
                    _ => return Err(self.internal(fiber, span, "an ill-typed map index")),
                };
                match got {
                    Ok(v) => Self::put_reg(fiber, r(d), v),
                    Err(key) => {
                        let message = format!("key {key} not found in map");
                        return Err(self.panic(fiber, span, message));
                    }
                }
            }
            Op::MapSetAt { m, k, v } => {
                let value = std::mem::replace(&mut fiber.regs[r(v)], Value::Unit);
                // One lookup: `insert` checks the guard first, replaces the value of a key the
                // map has (keeping its stored key, and handing the new one back as `spare_key`,
                // a clone of the key in `k`), and adds a key it has not.
                let inserted = match (&fiber.regs[r(m)], &fiber.regs[r(k)]) {
                    (Value::Map(map), key) => map.insert(key.clone(), value),
                    _ => {
                        self.release(value);
                        return Err(self.internal(fiber, span, "an ill-typed map index"));
                    }
                };
                match inserted {
                    Ok(replaced) => {
                        if let Some(spare) = replaced.spare_key {
                            self.release(spare);
                        }
                        if let Some(old) = replaced.old
                            && !is_scalar_value(&old)
                        {
                            self.release(old);
                        }
                    }
                    Err(rejected) => {
                        let (key, value) = rejected.args;
                        self.release(key);
                        self.release(value);
                        return Err(self.panic(fiber, span, rejected.violation.0));
                    }
                }
            }
            _ => return Err(self.internal(fiber, span, "not an element instruction")),
        }
        if matches!(op, Op::MapGetAt { .. } | Op::MapSetAt { .. })
            && colibri_core::compare::take_gave_up()
        {
            let result = match op {
                Op::MapGetAt { d, .. } => Some(r(d)),
                _ => None,
            };
            return Err(self.lookup_gave_up(fiber, chunk, pc, result));
        }
        Ok(())
    }

    /// `l.<path> = s` on a plain local slot.
    #[inline(never)]
    pub(crate) fn op_set_path(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [l, s, site]: [u16; 3],
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let Some(Site::Path(path)) = chunk.sites.get(usize::from(site)) else {
            return Err(self.internal(fiber, span, "a path write without its path"));
        };
        let value = std::mem::replace(&mut fiber.regs[base + usize::from(s)], Value::Unit);
        let mut new = Some(value);
        let old = replace_at(&mut fiber.regs[base + usize::from(l)], path, &mut new);
        if let Some(unused) = new {
            self.release(unused);
        }
        match old {
            Some(old) => {
                if !is_scalar_value(&old) {
                    self.release(old);
                }
                Ok(())
            }
            None => Err(self.internal(fiber, span, "a place does not exist")),
        }
    }

    /// `d = the key the map `m` stores for the key in `k``.
    #[inline(never)]
    pub(crate) fn op_canon_key(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [d, m, k]: [u16; 3],
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let Value::Map(map) = &fiber.regs[base + usize::from(m)] else {
            return Err(self.internal(fiber, span, "an ill-typed map index"));
        };
        let map = Rc::clone(map);
        let key = fiber.regs[base + usize::from(k)].clone();
        let canon = self.canonical_key(fiber, || map.keys(), key, span)?;
        Self::put_reg(fiber, base + usize::from(d), canon);
        if colibri_core::compare::take_gave_up() {
            return Err(self.lookup_gave_up(fiber, chunk, pc, Some(base + usize::from(d))));
        }
        Ok(())
    }

    /// Takes the guard a `for` loop holds for its whole duration.
    #[inline(never)]
    pub(crate) fn op_hold(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let token = match op {
            Op::HoldIter { r } => match &fiber.regs[base + usize::from(r)] {
                Value::List(l) => l.iter_hold(),
                Value::Map(m) => m.iter_hold(),
                Value::Set(s) => s.iter_hold(),
                _ => return Err(self.internal(fiber, span, "a `for` over a non-collection")),
            },
            Op::HoldListPlace { r, site } => {
                let Some(Site::Name(name)) = chunk.sites.get(usize::from(site)) else {
                    return Err(self.internal(fiber, span, "a hold without its name"));
                };
                match &fiber.regs[base + usize::from(r)] {
                    Value::List(l) => l.place_hold(name),
                    _ => return Err(self.internal(fiber, span, "a `for mut` over a non-list")),
                }
            }
            _ => return Err(self.internal(fiber, span, "not a hold instruction")),
        };
        fiber.holds.push(token);
        Ok(())
    }

    /// The next item of a `for` loop. `Ok(true)`: it charged a step and put the item in `d`;
    /// the dispatch loop skips the jump that ends the loop. `Ok(false)`: the source is used up.
    #[inline(never)]
    pub(crate) fn op_for_next(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<bool, Ctl> {
        let span = chunk.spans[pc - 1];
        let (Op::ForListNext { d, a }
        | Op::ForListNextVar { d, a }
        | Op::ForMapNext { d, a }
        | Op::ForSetNext { d, a }
        | Op::ForIterNext { d, a }) = op
        else {
            return Err(self.internal(fiber, span, "not a loop instruction"));
        };
        let src = base + usize::from(a);
        let item = if let Op::ForIterNext { .. } = op {
            // A built-in iterator is an object; a boxed user iterator is advanced by value, so
            // it is taken out of its register while it runs and put back.
            let mut it = std::mem::replace(&mut fiber.regs[src], Value::Unit);
            let next = self.iter_next(fiber, &mut it, span);
            fiber.regs[src] = it;
            match next? {
                Some(item) => item,
                None => return Ok(false),
            }
        } else {
            let Value::Int(i) = fiber.regs[src + 1] else {
                return Err(self.internal(fiber, span, "a loop position that is not an integer"));
            };
            let (item, next) = match (&fiber.regs[src], op) {
                (Value::List(l), Op::ForListNext { .. } | Op::ForListNextVar { .. }) => {
                    match l.at(i as usize) {
                        Some(item) => (item, i + 1),
                        None => return Ok(false),
                    }
                }
                (Value::Map(m), Op::ForMapNext { .. }) => match m.next_entry(i as usize) {
                    Some((k, v, next)) => (Value::tuple(vec![k, v]), next as i64),
                    None => return Ok(false),
                },
                (Value::Set(s), Op::ForSetNext { .. }) => match s.next_item(i as usize) {
                    Some((item, next)) => (item, next as i64),
                    None => return Ok(false),
                },
                _ => {
                    return Err(self.internal(fiber, span, "a `for` over the wrong kind of value"));
                }
            };
            fiber.regs[src + 1] = Value::Int(next);
            item
        };
        // One budget step per trip, before anything is bound.
        if let Err(ctl) = self.step(fiber, span) {
            self.release(item);
            return Err(ctl);
        }
        if let Op::ForListNextVar { .. } = op {
            Self::put_reg(fiber, src + 2, item.clone());
        }
        Self::put_reg(fiber, base + usize::from(d), item);
        Ok(true)
    }

    /// The end of an iteration of `for mut`: the binding's final value goes back into the list
    /// unless it is the element that was copied out.
    #[inline(never)]
    pub(crate) fn op_for_write_back(&mut self, fiber: &mut VmFiber, base: usize, l: u16, a: u16) {
        let src = base + usize::from(a);
        let last = colibri_core::value::unwrap_cell(std::mem::replace(
            &mut fiber.regs[base + usize::from(l)],
            Value::Unit,
        ));
        let original = std::mem::replace(&mut fiber.regs[src + 2], Value::Unit);
        let index = match fiber.regs[src + 1] {
            Value::Int(next) => next - 1,
            _ => -1,
        };
        if same_object(&original, &last) {
            self.release(last);
        } else if let Value::List(list) = &fiber.regs[src] {
            let list = Rc::clone(list);
            match list.write_back(index, last) {
                Ok(old) => self.release(old),
                Err((_, unplaced)) => self.release(unplaced),
            }
        } else {
            self.release(last);
        }
        self.release(original);
    }
}
