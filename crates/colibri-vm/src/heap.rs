//! The instructions that build and read heap values: structs, class objects, enum values,
//! tuples, lists, maps, ranges, projections and weak references. Out of line, like the other
//! cold arms of the dispatch loop.

use std::rc::{Rc, Weak};

use colibri_check::hir::ValueProj;
use colibri_core::ctl::Ctl;
use colibri_core::heap::{ClassObj, MapObj};
use colibri_core::ordered::OrderedMap;
use colibri_core::prelude::*;
use colibri_core::value::{ObjInline, PayloadInline, SmallVec, StructInline, Value};

use crate::bytecode::{Chunk, Op, Site};
use crate::fiber::VmFiber;
use crate::machine::Machine;

/// The part of `v` a projection selects.
pub(crate) fn project(v: &Value, proj: ValueProj) -> Option<&Value> {
    match (v, proj) {
        (Value::Struct(s), ValueProj::Field(i)) => s.fields.get(i.0 as usize),
        (Value::Tuple(items), ValueProj::TupleIdx(i)) => items.get(i as usize),
        _ => None,
    }
}

impl Machine {
    /// Clones the values of `regs` (a reference bump each) into inline storage of exactly their
    /// number.
    #[inline]
    fn clone_regs<A: colibri_core::value::Array<Item = Value>>(
        fiber: &VmFiber,
        base: usize,
        regs: &[u16],
    ) -> SmallVec<A> {
        let mut out = SmallVec::with_capacity(regs.len());
        for &r in regs {
            out.push(fiber.regs[base + usize::from(r)].clone());
        }
        out
    }

    #[inline(never)]
    pub(crate) fn op_new(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let (d, value) = match op {
            Op::NewStruct { d, site } => {
                let Site::Struct(ty, regs) = &chunk.sites[usize::from(site)] else {
                    return Err(self.internal(fiber, span, "a struct without its site"));
                };
                let fields = Self::clone_regs::<StructInline>(fiber, base, regs);
                (d, Value::strukt(*ty, fields))
            }
            Op::NewObject { d, site } => {
                let Site::Object(ty, regs) = &chunk.sites[usize::from(site)] else {
                    return Err(self.internal(fiber, span, "an object without its site"));
                };
                let fields = Self::clone_regs::<ObjInline>(fiber, base, regs);
                let obj = ClassObj::new(*ty, fields);
                self.core.track_object(&obj, Some(span));
                (d, Value::Obj(obj))
            }
            Op::NewVariant { d, site } => {
                let Site::Variant(ty, variant, regs) = &chunk.sites[usize::from(site)] else {
                    return Err(self.internal(fiber, span, "an enum value without its site"));
                };
                if regs.is_empty()
                    && *ty == colibri_check::types::OPTION_TYPE
                    && *variant == colibri_check::types::VARIANT_NONE
                {
                    (d, self.core.none())
                } else {
                    let payload = Self::clone_regs::<PayloadInline>(fiber, base, regs);
                    (d, Value::variant(*ty, *variant, payload))
                }
            }
            Op::NewTuple { d, site } | Op::NewList { d, site } => {
                let Site::Regs(regs) = &chunk.sites[usize::from(site)] else {
                    return Err(self.internal(fiber, span, "a collection without its site"));
                };
                let items: Vec<Value> = regs
                    .iter()
                    .map(|&r| fiber.regs[base + usize::from(r)].clone())
                    .collect();
                if matches!(op, Op::NewTuple { .. }) {
                    (d, Value::tuple(items))
                } else {
                    (d, Value::list(items))
                }
            }
            Op::NewMap { d } => (d, Value::Map(MapObj::new(OrderedMap::new()))),
            Op::NewRange { d, a, b, incl } => {
                let (Value::Int(x), Value::Int(y)) = (
                    &fiber.regs[base + usize::from(a)],
                    &fiber.regs[base + usize::from(b)],
                ) else {
                    return Err(self.internal(fiber, span, "a range of non-integers"));
                };
                (d, Value::range(*x, *y, incl))
            }
            Op::GetPath { d, s, site } => {
                let Site::Path(path) = &chunk.sites[usize::from(site)] else {
                    return Err(self.internal(fiber, span, "a projection without its path"));
                };
                let root = &fiber.regs[base + usize::from(s)];
                match path.iter().try_fold(root, |v, &p| project(v, p)) {
                    Some(part) => (d, part.clone()),
                    None => return Err(self.internal(fiber, span, "ill-typed projection")),
                }
            }
            Op::ObjGet { d, o, idx } => match &fiber.regs[base + usize::from(o)] {
                Value::Obj(obj) => match obj.field(usize::from(idx)) {
                    Some(v) => (d, v),
                    None => return Err(self.internal(fiber, span, "a place does not exist")),
                },
                _ => {
                    return Err(self.internal(
                        fiber,
                        span,
                        "a field of something that is not an object",
                    ));
                }
            },
            Op::FromWeak { d, s } => match &fiber.regs[base + usize::from(s)] {
                Value::Weak(w) => (d, self.core.option(w.upgrade().map(Value::Obj))),
                _ => return Err(self.internal(fiber, span, "a weak read of a strong value")),
            },
            _ => return Err(self.internal(fiber, span, "not a construction instruction")),
        };
        Self::put_reg(fiber, base + usize::from(d), value);
        Ok(())
    }

    /// A strong reference (or an `Option` of one) stored weakly. The strong value dies at once
    /// if this was its only owner.
    #[inline(never)]
    pub(crate) fn op_to_weak(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        d: u16,
        s: u16,
    ) -> Result<(), Ctl> {
        let v = std::mem::replace(&mut fiber.regs[base + usize::from(s)], Value::Unit);
        let weak = match &v {
            Value::Obj(rc) => Some(Rc::downgrade(rc)),
            Value::Enum(e) => match e.payload.first() {
                Some(Value::Obj(rc)) => Some(Rc::downgrade(rc)),
                None => Some(Weak::new()),
                _ => None,
            },
            _ => None,
        };
        self.release(v);
        match weak {
            Some(w) => {
                Self::put_reg(fiber, base + usize::from(d), Value::Weak(w));
                Ok(())
            }
            None => {
                let span = chunk.spans[pc - 1];
                Err(self.internal(fiber, span, "a weak reference to a non-object"))
            }
        }
    }

    /// One entry of a map literal: a key that a user `Eq` calls equal to a stored one is that
    /// key, and a repeated key keeps its first position and the last value.
    #[inline(never)]
    pub(crate) fn op_map_insert(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        [m, k, v]: [u16; 3],
    ) -> Result<(), Ctl> {
        let span = chunk.spans[pc - 1];
        let Value::Map(map) = &fiber.regs[base + usize::from(m)] else {
            return Err(self.internal(fiber, span, "a map literal without its map"));
        };
        let map = Rc::clone(map);
        let key = fiber.regs[base + usize::from(k)].clone();
        let value = fiber.regs[base + usize::from(v)].clone();
        let key = self.canonical_key(fiber, || map.keys(), key, span)?;
        let inserted = map.insert(key, value);
        match inserted {
            Ok(replaced) => {
                if let Some(old) = replaced.old {
                    self.release(old);
                }
                if let Some(spare) = replaced.spare_key {
                    self.release(spare);
                }
            }
            Err(_) => return Err(self.internal(fiber, span, "a map literal on a held map")),
        }
        if colibri_core::compare::take_gave_up() {
            return Err(self.lookup_gave_up(fiber, chunk, pc, None));
        }
        Ok(())
    }
}
