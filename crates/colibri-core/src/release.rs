//! Releasing values: the one place where owned values die (ARCHITECTURE §3, DESIGN §11.2).
//!
//! Colibri values are reference counted, so "the last owner lets go" is exactly the moment an
//! object is destroyed. Every owner of a value that may hold a class object hands it to
//! [`Release::release`] instead of dropping it: when it is the last reference, the object is
//! traced (when tracing is on) and its fields are released in declaration order, then the
//! object is freed. Lists release their elements by index, maps their entries in insertion
//! order. The work is done with an explicit worklist, so a long chain of objects cannot
//! overflow the native stack.

use std::rc::Rc;

use colibri_check::types::Ty;

use crate::coro::Sched;
use crate::ctl::{Ctl, Eval, PanicInfo};
use crate::engine::Engine;
use crate::heap::ClassObj;
use crate::host::RootHandle;
use crate::state::Core;
use crate::value::Value;

/// Hooks may nest this deep (a hook that drops an object whose hook drops another, ...).
const MAX_HOOK_DEPTH: u32 = 64;

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

impl Core {
    /// Turns drop tracing on or off (for the test harness); a trace starts empty.
    pub fn set_trace_drops(&mut self, on: bool) {
        self.trace = on.then(Vec::new);
    }

    /// The labels of the objects destroyed since tracing was turned on or last taken, in
    /// destruction order.
    pub fn take_trace(&mut self) -> Vec<String> {
        self.trace.as_mut().map(std::mem::take).unwrap_or_default()
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
            || obj.ty() == colibri_check::types::STR_BUILDER_TYPE
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
}

/// Releasing values and running `Drop` hooks, for any [`Engine`].
#[doc(hidden)]
pub trait Release: Engine {
    /// An owned value dies here. If this was the last reference to a class object, the object
    /// is destroyed and everything only it owned dies with it.
    fn release(&mut self, value: Value) {
        if !is_container(&value) {
            return;
        }
        let mut work = vec![value];
        while let Some(v) = work.pop() {
            self.dismantle(v, &mut work);
        }
    }

    /// Releases the values of a finished collection, in order.
    fn release_all(&mut self, values: Vec<Value>) {
        for v in values {
            self.release(v);
        }
    }

    /// Handles one value of the worklist: when it is the last reference, queues its children
    /// (in the order they must die) and lets the shell drop.
    #[doc(hidden)]
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
                self.core_mut().trace_destroy(&rc);
                let mut fields = rc.take_fields();
                push_children(work, &mut fields);
            }
            Value::Host(rc) if Rc::strong_count(&rc) == 1 => self.core_mut().free_host_object(&rc),
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

    // ---- the values the host keeps ----

    /// Lets go of a root; the value is released (a `Drop` hook may run). A stale handle is
    /// ignored.
    fn unroot(&mut self, handle: RootHandle) {
        if let Some(value) = self.core_mut().take_root(handle) {
            self.release_in_entry(value);
        }
    }

    /// Releases the roots whose last holder let go. Called at every entry from the host.
    fn drain_root_drops(&mut self) {
        let pending = self.core().host.drop_queue.take();
        for handle in pending {
            self.unroot(handle);
        }
    }

    /// Every root dies at once; the handles made so far become stale.
    fn clear_roots(&mut self) {
        for value in self.core_mut().take_all_roots() {
            self.release(value);
        }
    }

    /// Releases a value outside any call, with a budget of its own for the `Drop` hooks it
    /// may run.
    fn release_in_entry(&mut self, value: Value) {
        let outer = self.core_mut().begin_entry();
        self.release(value);
        self.core_mut().end_entry(outer, false);
    }

    // ---- the outcome of a step ----

    /// Releases the value an evaluation produced, when it produced one.
    fn release_outcome(&mut self, result: Eval) {
        match result {
            Ok(v) | Err(Ctl::Return(v) | Ctl::Break(_, v)) => self.release(v),
            Err(_) => {}
        }
    }

    /// Takes a half-made copy apart without tracing its objects as destroyed.
    fn abandon_copies(&mut self, copies: Vec<Value>) {
        let mut pending = Vec::new();
        for copy in &copies {
            match copy {
                Value::Obj(o) => pending.extend(o.take_fields()),
                Value::List(l) => pending.extend(l.take_all()),
                Value::Map(m) => pending.extend(m.take_all().into_iter().flat_map(|(k, v)| [k, v])),
                Value::Set(s) => pending.extend(s.take_all()),
                _ => {}
            }
        }
        drop(copies);
        self.release_all(pending);
    }

    // ---- Drop hooks ----

    /// Runs the `Drop` hook of a class object that is about to die. Returns whether the hook
    /// kept the object alive (resurrection): then it is not destroyed, and its hook never runs
    /// again (ARCHITECTURE §6).
    fn run_drop_hook(&mut self, obj: &Rc<ClassObj>) -> bool {
        let Some(f) = self.core().dispatch.drop_hook(obj.ty()) else {
            return false;
        };
        if obj.mark_hooked() {
            return false;
        }
        let Some(span) = self.fn_span(f) else {
            return false;
        };
        let core = self.core_mut();
        if core.hook_depth >= MAX_HOOK_DEPTH {
            core.hook_panics.push(PanicInfo {
                message: "Drop hooks are nested too deeply".to_string(),
                span,
                stack: Vec::new(),
                notes: Vec::new(),
                budget: false,
            });
            return false;
        }
        // Past the grace of an exhausted budget, hooks no longer run (the object is released
        // plainly).
        if core.budget_is_dead() {
            return false;
        }
        core.hook_depth += 1;
        let result = self.call_detached(f, vec![Value::Obj(Rc::clone(obj))]);
        let core = self.core_mut();
        core.hook_depth -= 1;
        match result {
            Ok(v) | Err(Ctl::Return(v)) => self.release(v),
            Err(Ctl::Panic(info)) => self.core_mut().hook_panics.push(*info),
            Err(_) => {}
        }
        Rc::strong_count(obj) > 1
    }
}

impl<E: Engine> Release for E {}
