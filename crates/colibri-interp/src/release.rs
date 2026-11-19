//! Releasing values: the one place where owned values die (ARCHITECTURE §3, DESIGN §11.2).
//!
//! Colibri values are reference counted, so "the last owner lets go" is exactly the moment an
//! object is destroyed. Every owner of a value that may hold a class object hands it to
//! [`Interp::release`] instead of dropping it: when it is the last reference, the object is
//! traced (when tracing is on) and its fields are released in declaration order, then the
//! object is freed. Lists release their elements by index, maps their entries in insertion
//! order. The work is done with an explicit worklist, so a long chain of objects cannot
//! overflow the native stack.

use std::rc::Rc;

use colibri_check::types::Ty;

use crate::eval::Interp;
use crate::heap::ClassObj;
use crate::value::Value;

/// Whether a value can own other values that need releasing.
fn is_container(v: &Value) -> bool {
    matches!(
        v,
        Value::Struct(_)
            | Value::Tuple(_)
            | Value::Enum(_)
            | Value::List(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::Obj(_)
            | Value::Host(_)
            | Value::Dyn(_)
            | Value::Closure(_)
            | Value::Cell(_)
            | Value::Iter(_)
    )
}

/// Moves the container children of `items` onto `work` so that they pop in index order.
fn push_children(work: &mut Vec<Value>, items: &mut [Value]) {
    for item in items.iter_mut().rev() {
        if is_container(item) {
            work.push(std::mem::replace(item, Value::Unit));
        }
    }
}

impl Interp {
    /// Turns drop tracing on or off (for the test harness); a trace starts empty.
    pub fn set_trace_drops(&mut self, on: bool) {
        self.trace = on.then(Vec::new);
    }

    /// The labels of the objects destroyed since tracing was turned on or last taken, in
    /// destruction order.
    pub fn take_trace(&mut self) -> Vec<String> {
        self.trace.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// An owned value dies here. If this was the last reference to a class object, the object
    /// is destroyed and everything only it owned dies with it.
    pub fn release(&mut self, value: Value) {
        if !is_container(&value) {
            return;
        }
        let mut work = vec![value];
        while let Some(v) = work.pop() {
            self.dismantle(v, &mut work);
        }
    }

    /// Releases the values of a finished collection, in order.
    pub(crate) fn release_all(&mut self, values: Vec<Value>) {
        for v in values {
            self.release(v);
        }
    }

    /// Handles one value of the worklist: when it is the last reference, queues its children
    /// (in the order they must die) and lets the shell drop.
    fn dismantle(&mut self, value: Value, work: &mut Vec<Value>) {
        match value {
            Value::Obj(rc) if Rc::strong_count(&rc) == 1 => {
                if rc.ty() == colibri_check::types::COGROUP_TYPE {
                    // The group dies: its coroutines are cancelled, newest first.
                    self.group_died(&rc);
                }
                // The `Drop` hook runs first; if it stores the object somewhere, the object
                // lives on and dies (without a second hook) when that reference does.
                if self.run_drop_hook(&rc) {
                    return;
                }
                self.trace_destroy(&rc);
                let mut fields = rc.take_fields();
                push_children(work, &mut fields);
            }
            Value::Host(rc) if Rc::strong_count(&rc) == 1 => self.free_host_object(&rc),
            Value::List(rc) if Rc::strong_count(&rc) == 1 => {
                let mut items = rc.take_all();
                push_children(work, &mut items);
            }
            Value::Map(rc) if Rc::strong_count(&rc) == 1 => {
                let mut flat: Vec<Value> = rc
                    .take_all()
                    .into_iter()
                    .flat_map(|(k, v)| [k, v])
                    .collect();
                push_children(work, &mut flat);
            }
            Value::Set(rc) if Rc::strong_count(&rc) == 1 => {
                let mut items = rc.take_all();
                push_children(work, &mut items);
            }
            Value::Closure(rc) => {
                // Dropping a closure releases what it captured, in capture order. (Not
                // `get_mut`: the leak detector's registry holds a weak reference.)
                if let Ok(mut c) = Rc::try_unwrap(rc) {
                    let mut values = c.take_values();
                    push_children(work, &mut values);
                }
            }
            Value::Cell(rc) if Rc::strong_count(&rc) == 1 => {
                let mut items = [rc.take()];
                push_children(work, &mut items);
            }
            Value::Iter(rc) if Rc::strong_count(&rc) == 1 => {
                let mut items = rc.take_inner();
                push_children(work, &mut items);
            }
            Value::Dyn(mut rc) => {
                if let Some(d) = Rc::get_mut(&mut rc)
                    && is_container(&d.val)
                {
                    work.push(std::mem::replace(&mut d.val, Value::Unit));
                }
            }
            Value::Struct(mut rc) => {
                if let Some(s) = Rc::get_mut(&mut rc) {
                    push_children(work, &mut s.fields);
                }
            }
            Value::Enum(mut rc) => {
                if let Some(e) = Rc::get_mut(&mut rc) {
                    push_children(work, &mut e.payload);
                }
            }
            Value::Tuple(mut rc) => {
                if let Some(items) = Rc::get_mut(&mut rc) {
                    push_children(work, items);
                }
            }
            _ => {}
        }
    }

    /// Records the destruction of `obj` when tracing: its `name: str` field if it has one,
    /// else its class name.
    fn trace_destroy(&mut self, obj: &ClassObj) {
        // An `Error` is plumbing, not a user object: its kept value is traced when it dies.
        if self.trace.is_none()
            || obj.ty() == colibri_check::types::ERROR_TYPE
            || obj.ty() == colibri_check::types::COROUTINE_TYPE
            || obj.ty() == colibri_check::types::COGROUP_TYPE
            || obj.ty() == colibri_check::types::SIGNAL_TYPE
        {
            return;
        }
        let label = self.types.get(obj.ty().0 as usize).map(|def| {
            let named = def
                .fields()
                .iter()
                .position(|f| f.name == "name" && f.ty == Ty::Str && !f.weak)
                .and_then(|i| obj.field(i));
            match named {
                Some(Value::Str(s)) => s.to_string(),
                _ => def.name.clone(),
            }
        });
        if let (Some(trace), Some(label)) = (self.trace.as_mut(), label) {
            trace.push(label);
        }
    }

    /// Releases the module-level values in reverse initialisation order and leaves the slots
    /// empty, like [`Interp::shutdown`] but keeping the host's roots (the program stays loaded).
    /// Used between tests: a `Drop` hook that panics here has nobody to tell.
    pub fn release_globals(&mut self) {
        self.cancel_all_coroutines();
        self.sched_forget_failures();
        let outermost = self.begin_entry();
        let order: Vec<usize> = self.inits.iter().map(|(g, _)| g.0 as usize).collect();
        for g in order.into_iter().rev() {
            if let Some(slot) = self.globals.get_mut(g) {
                let v = std::mem::replace(slot, Value::Unit);
                self.release(v);
            }
        }
        self.hook_panics.clear();
        self.end_entry(outermost, false);
    }

    /// Ends the program's life: module-level values die in reverse initialisation order
    /// (DESIGN §11.2). Safe to call twice.
    pub fn shutdown(&mut self) {
        // Coroutines unwind first: their locals may use module-level values.
        self.cancel_all_coroutines();
        let outermost = self.begin_entry();
        // Roots die first, then the module's values (EMBEDDING §3).
        self.clear_roots();
        let order: Vec<usize> = self.inits.iter().map(|(g, _)| g.0 as usize).collect();
        for g in order.into_iter().rev() {
            if let Some(slot) = self.globals.get_mut(g) {
                let v = std::mem::replace(slot, Value::Unit);
                self.release(v);
            }
        }
        // A `Drop` hook that panics while the program is shutting down has nobody to tell.
        self.hook_panics.clear();
        self.end_entry(outermost, false);
    }
}

impl Drop for Interp {
    fn drop(&mut self) {
        self.shutdown();
        // Roots and module values are gone; what is left in the host heap dies last, in
        // index order (EMBEDDING §3).
        self.drop_host_heap();
    }
}
