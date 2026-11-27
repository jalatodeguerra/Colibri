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

use crate::coro::SchedInternal;
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
            | Value::SomeObj(_)
            | Value::Host(_)
            | Value::Dyn(_)
            | Value::Closure(_)
            | Value::Cell(_)
            | Value::Iter(_)
    )
}

/// Whether the value is a counted reference to something that another reference keeps alive:
/// dropping it cannot run a hook, trace anything or release a child.
#[inline]
fn is_shared(v: &Value) -> bool {
    match v {
        Value::Struct(rc) => Rc::strong_count(rc) > 1,
        Value::Tuple(rc) => Rc::strong_count(rc) > 1,
        Value::Enum(rc) => Rc::strong_count(rc) > 1,
        Value::List(rc) => Rc::strong_count(rc) > 1,
        Value::Map(rc) => Rc::strong_count(rc) > 1,
        Value::Set(rc) => Rc::strong_count(rc) > 1,
        Value::Obj(rc) | Value::SomeObj(rc) => Rc::strong_count(rc) > 1,
        Value::Host(rc) => Rc::strong_count(rc) > 1,
        Value::Dyn(rc) => Rc::strong_count(rc) > 1,
        Value::Closure(rc) => Rc::strong_count(rc) > 1,
        Value::Cell(rc) => Rc::strong_count(rc) > 1,
        Value::Iter(rc) => Rc::strong_count(rc) > 1,
        _ => false,
    }
}

/// Moves the container children of `items` onto `work` so that they pop in index order.
#[inline]
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
    #[inline]
    fn trace_destroy(&mut self, obj: &ClassObj) {
        if self.trace.is_some() {
            self.trace_destroy_traced(obj);
        }
    }

    #[inline(never)]
    fn trace_destroy_traced(&mut self, obj: &ClassObj) {
        // An `Error` is plumbing, not a user object: its kept value is traced when it dies.
        if self.trace.is_none()
            || obj.ty() == colibri_check::types::ERROR_TYPE
            || obj.ty() == colibri_check::types::COROUTINE_TYPE
            || obj.ty() == colibri_check::types::COGROUP_TYPE
            || obj.ty() == colibri_check::types::SIGNAL_TYPE
            || obj.ty() == colibri_check::types::TASK_TYPE
            || obj.ty() == colibri_check::types::CHANNEL_TYPE
            || obj.ty() == colibri_check::types::SHARED_TYPE
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
        // A reference that is not the last one only lets the count go down.
        if is_shared(&value) {
            return;
        }
        self.release_last(value);
    }

    /// [`Release::release`] of a container that is its last reference. A separate function so
    /// that the early exits of `release` keep a short prologue.
    #[doc(hidden)]
    #[inline(never)]
    fn release_last(&mut self, value: Value) {
        // A struct, tuple or enum that owns nothing but scalars and strings (a point, a pair)
        // has nothing to release: it is freed with its last reference, no worklist.
        match &value {
            Value::Struct(rc) if !rc.fields.iter().any(is_container) => return,
            Value::Tuple(rc) if !rc.iter().any(is_container) => return,
            Value::Enum(rc) if !rc.payload.iter().any(is_container) => return,
            // A collection of scalars and strings is freed whole: no entry has anything to
            // release, so the worklist (and the `Vec` that would collect the entries) is skipped.
            Value::List(rc) if rc.with(|xs| !xs.iter().any(is_container)) => return,
            Value::Map(rc)
                if rc.with(|m| !m.iter().any(|(k, v)| is_container(k) || is_container(v))) =>
            {
                return;
            }
            Value::Set(rc) if rc.with(|m| !m.iter().any(|(k, _)| is_container(k))) => return,
            _ => {}
        }
        // The worklist allocates on its first push, so a container with no container
        // children (a leaf) is taken apart without allocating (a map still collects its
        // entries into one `Vec`). `is_shared` only saves the call: an unshared-check inside
        // `dismantle` would do nothing for such a value, so no test can tell it apart.
        let mut work = std::mem::take(&mut self.core_mut().release_work);
        self.dismantle(value, &mut work);
        while let Some(v) = work.pop() {
            self.dismantle(v, &mut work);
        }
        // A hook that ran meanwhile may have left a list of its own; the last one stays.
        if work.capacity() <= 1 << 12 {
            self.core_mut().release_work = work;
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
        // A `some(obj)` without its box dies as the object does: the box it stands for would
        // only have handed the object on.
        let value = match value {
            Value::SomeObj(rc) => Value::Obj(rc),
            other => other,
        };
        match value {
            Value::Obj(rc) if Rc::strong_count(&rc) == 1 => {
                if rc.ty() == colibri_check::types::COGROUP_TYPE {
                    // The group dies: its coroutines are cancelled, newest first.
                    self.group_died(&rc);
                } else if let Some(op) = crate::taskrun::task_op_of(&rc) {
                    // The handle of a thread task dies: the task is detached.
                    self.core_mut().task_died(op);
                } else if let Some(id) = crate::chan::chan_id_of(&rc) {
                    // A channel handle dies: the channel lives on while another handle holds it,
                    // and a channel is closed only by `close()`.
                    self.core_mut().chan_died(id);
                } else if let Some(id) = crate::shared::shared_id_of(&rc) {
                    // A `Shared` handle dies: the graph lives on while another handle holds it.
                    // This isolate's frozen copies of graphs that no handle holds any more die
                    // with it (the last handle may be in another isolate: the next sweep, at a
                    // `get` or at the start of a task, finds those).
                    self.core_mut().shared_died(id);
                    let dead = self.core_mut().sweep_shared();
                    self.release_all(dead);
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
        if let Ok(v) = result {
            self.release(v);
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
                debug_serial: 0,
                memory: None,
            });
            return false;
        }
        // Past the grace of an exhausted budget, hooks no longer run (the object is released
        // plainly).
        if core.cleanups_are_dead() {
            return false;
        }
        core.hook_depth += 1;
        let result = self.call_detached(f, vec![Value::Obj(Rc::clone(obj))]);
        let core = self.core_mut();
        core.hook_depth -= 1;
        match result {
            Ok(v) => self.release(v),
            Err(Ctl::Panic(info)) => self.core_mut().hook_panics.push(*info),
            Err(_) => {}
        }
        Rc::strong_count(obj) > 1
    }
}

impl<E: Engine> Release for E {}

#[cfg(test)]
mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    use colibri_check::hir::FnId;
    use colibri_check::types::TypeId;
    use colibri_syntax::Span;

    use super::*;
    use crate::ctl::TraceEntry;
    use crate::fiber::FiberState;
    use crate::host::{HostFault, HostHook};
    use crate::sched::EngineSlot;
    use crate::value::{ObjFields, StructFields, StructInline, exact, one};

    thread_local! {
        static ALLOCS: Cell<usize> = const { Cell::new(0) };
    }

    /// Counts the allocations of the thread that runs the test, so that tests running side
    /// by side do not see each other's.
    struct Counting;

    // SAFETY: every call is forwarded to the system allocator unchanged.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ALLOCS.with(|c| c.set(c.get() + 1));
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            ALLOCS.with(|c| c.set(c.get() + 1));
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    // This counts the allocations of every test in this crate's unit-test binary; another
    // allocator for these tests would need to replace it.
    #[global_allocator]
    static COUNTING: Counting = Counting;

    /// How many allocations `f` makes on this thread.
    fn allocations(f: impl FnOnce()) -> usize {
        let before = ALLOCS.with(Cell::get);
        f();
        ALLOCS.with(Cell::get) - before
    }

    /// An engine with nothing to run: enough for `release`, which only calls a hook for an
    /// object whose class has one.
    struct Bare {
        core: Core,
    }

    impl Bare {
        fn new() -> Bare {
            Bare {
                core: Core::new(Box::new(std::io::sink())),
            }
        }

        /// What a worker does at the start of a task.
        fn free_dead_copies_for_test(&mut self) {
            for copy in self.core.sweep_shared() {
                self.release(copy);
            }
        }
    }

    impl Engine for Bare {
        type Fiber = FiberState;
        fn core(&self) -> &Core {
            &self.core
        }
        fn core_mut(&mut self) -> &mut Core {
            &mut self.core
        }
        fn fiber_state(fiber: &FiberState) -> &FiberState {
            fiber
        }
        fn fiber_state_mut(fiber: &mut FiberState) -> &mut FiberState {
            fiber
        }
        fn note_position(_: &mut FiberState, _: Span) {}
        fn position(_: &FiberState) -> Option<Span> {
            None
        }
        fn stack_trace(&self, _: &FiberState) -> Vec<TraceEntry> {
            Vec::new()
        }
        fn stack_base(_: &FiberState) -> usize {
            0
        }
        fn native_stack_budget(&self) -> usize {
            usize::MAX
        }
        fn call_fn_inout(
            &mut self,
            _: &mut FiberState,
            _: FnId,
            _: Vec<Value>,
        ) -> (Eval, Option<Value>) {
            unreachable!("no function runs in these tests")
        }
        fn call_value(&mut self, _: &mut FiberState, _: &Value, _: Vec<Value>, _: Span) -> Eval {
            unreachable!("no function runs in these tests")
        }
        fn call_detached(&mut self, _: FnId, _: Vec<Value>) -> Eval {
            unreachable!("no function runs in these tests")
        }
        fn fn_span(&self, _: FnId) -> Option<Span> {
            None
        }
        fn fn_mutates_self(&self, _: FnId) -> bool {
            false
        }
        fn call_host_hook(
            &mut self,
            _: &mut FiberState,
            _: TypeId,
            _: HostHook,
            _: Vec<Value>,
        ) -> Result<Value, HostFault> {
            unreachable!("no host hook runs in these tests")
        }
        fn temp_mark(_: &FiberState) -> usize {
            0
        }
        fn hold_temp(_: &mut FiberState, _: Value) {}
        fn release_temps(&mut self, _: &mut FiberState, _: usize) {}
        fn resume_stack(&mut self, _: u64, _: EngineSlot, _: bool) -> Resumed {
            Resumed::Gone
        }
        fn task_builtin(
            &mut self,
            _: &mut FiberState,
            _: colibri_check::hir::Builtin,
            _: &Value,
            _: Span,
        ) -> Eval {
            unreachable!("no task runs in these tests")
        }
        fn channel_builtin(
            &mut self,
            _: &mut FiberState,
            _: colibri_check::hir::Builtin,
            _: &mut [Value],
            _: Span,
        ) -> Eval {
            unreachable!("no channel is used in these tests")
        }
        fn shared_get(&mut self, _: &mut FiberState, _: &Value, _: Span) -> Eval {
            unreachable!("no shared value is used in these tests")
        }
        fn channel_next(&mut self, _: &mut FiberState, _: &Value, _: Span) -> Eval<Option<Value>> {
            unreachable!("no channel is used in these tests")
        }
    }

    use crate::engine::Resumed;

    /// One of every container the worklist knows, each holding other containers so that the
    /// last-reference path would have work to do.
    fn containers() -> Vec<Value> {
        let inner = Value::strukt(TypeId(0), vec![Value::Int(1)]);
        vec![
            Value::strukt(TypeId(0), vec![Value::Int(1), inner.clone()]),
            Value::variant(TypeId(0), 1, one(inner.clone())),
            Value::tuple(vec![inner.clone(), Value::Int(2)]),
            Value::list(vec![inner.clone()]),
            Value::Obj(ClassObj::new(TypeId(1), ObjFields::from_elem(inner, 2))),
        ]
    }

    #[test]
    fn releasing_a_shared_container_does_not_allocate() {
        let mut engine = Bare::new();
        for kept in containers() {
            let extra = kept.clone();
            let n = allocations(|| engine.release(extra));
            assert_eq!(n, 0, "releasing a shared {kept:?} allocated");
            // The last reference still goes through the worklist.
            engine.release(kept);
        }
    }

    #[test]
    fn releasing_a_leaf_container_does_not_allocate() {
        let mut engine = Bare::new();
        let leaves = vec![
            Value::strukt(TypeId(0), vec![Value::Int(1), Value::Float(2.0)]),
            Value::variant(TypeId(0), 1, one(Value::Int(1))),
            Value::tuple(vec![Value::Int(1), Value::str("a")]),
            Value::list(vec![Value::Int(1), Value::Int(2)]),
            Value::Obj(ClassObj::new(
                TypeId(1),
                ObjFields::from_elem(Value::Bool(true), 3),
            )),
        ];
        for leaf in leaves {
            let n = allocations(|| engine.release(leaf));
            assert_eq!(n, 0, "releasing a leaf allocated");
        }
    }

    /// A class `name` with no fields, as type `id`.
    fn class(name: &str) -> colibri_check::hir::TypeDef {
        colibri_check::hir::TypeDef {
            name: name.to_string(),
            span: Span {
                file: colibri_syntax::FileId(0),
                start: 0,
                end: 0,
            },
            params: Vec::new(),
            kind: colibri_check::hir::TypeDefKind::Class { fields: Vec::new() },
            module: None,
            open: false,
        }
    }

    fn object(ty: u32) -> Value {
        Value::Obj(ClassObj::new(TypeId(ty), ObjFields::new()))
    }

    /// The last reference to a struct dies, and the objects it owned die in field order, the
    /// ones in nested values first-come: the trace shows it. A release that never took the
    /// value apart would leave the objects to the plain drop (which the debug check in
    /// `ClassObj` rejects) and the trace empty.
    #[test]
    fn the_last_reference_releases_the_children_in_order() {
        let mut engine = Bare::new();
        engine.core.types = Rc::from(vec![class("A"), class("B"), class("C")]);
        engine.core.set_trace_drops(true);
        let outer = Value::strukt(
            TypeId(0),
            vec![
                Value::tuple(vec![object(0), Value::Int(1)]),
                object(1),
                Value::variant(TypeId(0), 0, one(object(2))),
            ],
        );
        engine.release(outer);
        assert_eq!(engine.core.take_trace(), ["A", "B", "C"]);
    }

    /// A reference that is not the last one destroys nothing; the last one does.
    #[test]
    fn a_shared_reference_destroys_nothing() {
        let mut engine = Bare::new();
        engine.core.types = Rc::from(vec![class("A")]);
        engine.core.set_trace_drops(true);
        let kept = Value::tuple(vec![object(0)]);
        engine.release(kept.clone());
        assert!(engine.core.take_trace().is_empty());
        engine.release(kept);
        assert_eq!(engine.core.take_trace(), ["A"]);
    }

    /// The last reference to a channel handle lets go of the channel's entry in the table, and
    /// not before; the channel itself lives on while another handle holds it.
    #[test]
    fn the_last_reference_to_a_channel_handle_lets_go_of_its_table_entry() {
        let mut engine = Bare::new();
        let inner = crate::chan::ChanInner::new(None);
        let first = engine.core.channel_handle(std::sync::Arc::clone(&inner));
        let second = engine.core.channel_handle(std::sync::Arc::clone(&inner));
        assert_eq!(engine.core.chan_count(), 2);
        engine.release(first.clone());
        assert_eq!(engine.core.chan_count(), 2);
        engine.release(first);
        assert_eq!(engine.core.chan_count(), 1);
        assert_eq!(std::sync::Arc::strong_count(&inner), 2);
        engine.release(second);
        assert_eq!(engine.core.chan_count(), 0);
        assert_eq!(std::sync::Arc::strong_count(&inner), 1);
    }

    /// The last handle of a `Shared` in this engine lets go of its graph, and the engine's copy of
    /// the graph goes with it when no handle of it is left anywhere, and not before.
    #[test]
    fn the_last_handle_to_a_shared_frees_the_copy_and_not_before() {
        use crate::portable::{StackGuard, Unnamed, from_portable_frozen, to_portable};
        let mut engine = Bare::new();
        let root = to_portable(&Value::list(vec![Value::Int(1)]), &Unnamed, StackGuard::here(1 << 20))
            .unwrap();
        let inner = crate::shared::SharedInner::new(root);
        let first = engine.core.shared_handle(std::sync::Arc::clone(&inner));
        let second = engine.core.shared_handle(std::sync::Arc::clone(&inner));
        let copy = from_portable_frozen(inner.root(), &mut engine.core, StackGuard::here(1 << 20))
            .unwrap();
        engine.core.shared_cache(&inner, copy);
        assert_eq!((engine.core.shared_count(), engine.core.shared_copies()), (2, 1));
        // Another isolate holds the graph too: the copy stays after both of this one's handles.
        let theirs = std::sync::Arc::clone(&inner);
        drop(inner);
        engine.release(first);
        assert_eq!((engine.core.shared_count(), engine.core.shared_copies()), (1, 1));
        engine.release(second);
        assert_eq!((engine.core.shared_count(), engine.core.shared_copies()), (0, 1));
        // The other isolate's handle dies: whoever looks next finds the graph gone.
        drop(theirs);
        engine.free_dead_copies_for_test();
        assert_eq!(engine.core.shared_copies(), 0);
        // With no other holder, the last handle's death frees the copy by itself.
        let root = to_portable(&Value::list(vec![Value::Int(2)]), &Unnamed, StackGuard::here(1 << 20))
            .unwrap();
        let alone = crate::shared::SharedInner::new(root);
        let handle = engine.core.shared_handle(std::sync::Arc::clone(&alone));
        let copy = from_portable_frozen(alone.root(), &mut engine.core, StackGuard::here(1 << 20))
            .unwrap();
        engine.core.shared_cache(&alone, copy);
        drop(alone);
        assert_eq!(engine.core.shared_copies(), 1);
        engine.release(handle);
        assert_eq!((engine.core.shared_count(), engine.core.shared_copies()), (0, 0));
    }

    /// A string builder of a frozen copy can be asked for its text any number of times without
    /// being rewritten: its pieces stay as they were, which `to_str` does to an ordinary builder.
    #[test]
    fn a_frozen_string_builder_is_read_without_being_rewritten() {
        use crate::strfn::StrOps;
        let mut engine = Bare::new();
        let chunks = crate::heap::ListObj::new(vec![Value::str("a"), Value::str("b")]);
        let builder = ClassObj::new_exact(
            colibri_check::types::STR_BUILDER_TYPE,
            ObjFields::from(vec![Value::List(Rc::clone(&chunks)), Value::Int(2)]),
        );
        let args = [Value::Obj(Rc::clone(&builder))];
        let (mut fiber, span) = (FiberState::default(), Span::new(colibri_syntax::FileId(0), 0, 0));
        let to_str = colibri_check::hir::Builtin::StrBuilderToStr;
        // An ordinary builder joins its pieces into one the first time it is asked.
        let text = engine.str_builder_op(&mut fiber, to_str, &args, span).unwrap();
        assert_eq!(text, Value::str("ab"));
        assert_eq!(chunks.len(), 1);
        // A frozen one does not.
        let chunks = crate::heap::ListObj::new(vec![Value::str("a"), Value::str("b")]);
        builder.set_field(0, Value::List(Rc::clone(&chunks))).unwrap();
        builder.freeze();
        chunks.freeze();
        for _ in 0..3 {
            let text = engine.str_builder_op(&mut fiber, to_str, &args, span).unwrap();
            assert_eq!(text, Value::str("ab"));
            assert_eq!(chunks.len(), 2);
        }
        // And it cannot be added to.
        let push = colibri_check::hir::Builtin::StrBuilderPush;
        let with = [args[0].clone(), Value::str("c")];
        assert!(engine.str_builder_op(&mut fiber, push, &with, span).is_err());
        assert_eq!(chunks.len(), 2);
        drop(builder.take_fields());
        drop(chunks.take_all());
    }

    #[test]
    fn a_small_struct_is_one_allocation() {
        let inline = StructFields::new().inline_size();
        let n = allocations(|| {
            let mut fields = StructFields::new();
            fields.extend((0..inline).map(|i| Value::Int(i as i64)));
            drop(Value::strukt(TypeId(0), fields));
        });
        assert_eq!(n, 1);
    }

    /// A value with more fields than the inline storage holds keeps no spare room, however the
    /// fields were collected.
    #[test]
    fn a_struct_past_the_inline_size_has_no_spare_capacity() {
        let len = StructFields::new().inline_size() + 1;
        let items = || (0..len).map(|i| Value::Int(i as i64));
        let mut roomy = Vec::with_capacity(64);
        roomy.extend(items());
        let from_vec = Value::strukt(TypeId(0), roomy);
        let from_exact = Value::strukt(TypeId(0), exact::<StructInline>(items()));
        let mut grown = StructFields::new();
        grown.extend(items());
        let from_grown = Value::strukt(TypeId(0), grown);
        for v in [from_vec, from_exact, from_grown] {
            let Value::Struct(s) = &v else { unreachable!() };
            assert_eq!(s.fields.len(), len);
            assert_eq!(s.fields.capacity(), len);
        }
    }
}
