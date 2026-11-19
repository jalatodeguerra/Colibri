//! Deep copy, `x.clone()` (DESIGN §5.3).
//!
//! The copy is made with a memo from each reference object visited to its copy, so a
//! substructure that is shared inside the cloned graph is shared in the copy, and a cycle
//! comes out as a cycle. Objects outside the graph are never reached. A weak reference whose
//! target was copied points at the copy; any other weak reference is kept as it is. Strings
//! are immutable, so the copy shares them.

use std::collections::HashMap;
use std::rc::{Rc, Weak};

use crate::heap::{ClassObj, ListObj, MapObj, SetObj};
use crate::hooks::{Overrides, Stop};
use crate::ordered::OrderedMap;
use crate::value::Value;

/// One deep copy in progress.
pub(crate) struct Cloner {
    /// Copies by the address of the original reference object.
    memo: HashMap<usize, Value>,
    stack_base: usize,
    budget: usize,
}

impl Cloner {
    /// A cloner that gives up (rather than overflow the native stack) once the recursion has
    /// used `budget` bytes below `stack_base`.
    pub(crate) fn new(stack_base: usize, budget: usize) -> Self {
        Cloner {
            memo: HashMap::new(),
            stack_base,
            budget,
        }
    }

    /// The objects made so far, to be taken apart when the copy is abandoned.
    pub(crate) fn into_copies(self) -> Vec<Value> {
        self.memo.into_values().collect()
    }

    pub(crate) fn clone_value(&mut self, v: &Value, ov: &mut dyn Overrides) -> Result<Value, Stop> {
        if crate::eval::stack_position().abs_diff(self.stack_base) > self.budget {
            return Err(Stop::TooDeep);
        }
        if matches!(v, Value::Struct(_) | Value::Enum(_) | Value::Obj(_))
            && let Some(answer) = ov.clone_value(v)
        {
            return answer;
        }
        Ok(match v {
            Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Char(_)
            | Value::Str(_) => v.clone(),
            Value::Struct(s) => Value::strukt(s.ty, self.clone_all(&s.fields, ov)?),
            Value::Tuple(items) => Value::tuple(self.clone_all(items, ov)?),
            Value::Enum(e) => Value::variant(e.ty, e.variant, self.clone_all(&e.payload, ov)?),
            Value::Weak(w) => self.clone_weak(w),
            Value::Dyn(d) => Value::Dyn(Rc::new(crate::value::DynVal {
                impl_id: d.impl_id,
                val: self.clone_value(&d.val, ov)?,
            })),
            Value::Obj(src) => {
                let key = Rc::as_ptr(src) as usize;
                if let Some(done) = self.memo.get(&key) {
                    return Ok(done.clone());
                }
                let fields = src.snapshot();
                let copy = ClassObj::new(src.ty(), vec![Value::Unit; fields.len()]);
                self.memo.insert(key, Value::Obj(Rc::clone(&copy)));
                for (i, field) in fields.iter().enumerate() {
                    let new = self.clone_value(field, ov)?;
                    let _ = copy.set_field(i, new);
                }
                Value::Obj(copy)
            }
            Value::List(src) => {
                let key = Rc::as_ptr(src) as usize;
                if let Some(done) = self.memo.get(&key) {
                    return Ok(done.clone());
                }
                let copy = ListObj::new(Vec::new());
                self.memo.insert(key, Value::List(Rc::clone(&copy)));
                for item in src.snapshot() {
                    let new = self.clone_value(&item, ov)?;
                    let _ = copy.push(new);
                }
                Value::List(copy)
            }
            Value::Map(src) => {
                let key = Rc::as_ptr(src) as usize;
                if let Some(done) = self.memo.get(&key) {
                    return Ok(done.clone());
                }
                let copy = MapObj::new(OrderedMap::new());
                self.memo.insert(key, Value::Map(Rc::clone(&copy)));
                for (k, v) in src.snapshot() {
                    let (k, v) = (self.clone_value(&k, ov)?, self.clone_value(&v, ov)?);
                    let _ = copy.insert(k, v);
                }
                Value::Map(copy)
            }
            Value::Set(src) => {
                let key = Rc::as_ptr(src) as usize;
                if let Some(done) = self.memo.get(&key) {
                    return Ok(done.clone());
                }
                let copy = SetObj::new(OrderedMap::new());
                self.memo.insert(key, Value::Set(Rc::clone(&copy)));
                for item in src.snapshot() {
                    let new = self.clone_value(&item, ov)?;
                    let _ = copy.add(new);
                }
                Value::Set(copy)
            }
        })
    }

    fn clone_all(&mut self, items: &[Value], ov: &mut dyn Overrides) -> Result<Vec<Value>, Stop> {
        items.iter().map(|v| self.clone_value(v, ov)).collect()
    }

    /// A weak reference follows its target into the copy when the target was copied.
    fn clone_weak(&self, w: &Weak<ClassObj>) -> Value {
        match self.memo.get(&(w.as_ptr() as usize)) {
            Some(Value::Obj(copy)) => Value::Weak(Rc::downgrade(copy)),
            _ => Value::Weak(w.clone()),
        }
    }
}
