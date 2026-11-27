//! `Shared[T]` (DESIGN §14.1, ARCHITECTURE §36.6): a frozen copy that thread tasks can share.
//!
//! `Shared(x)` copies `x` once into a [`SharedInner`], an `Arc` around the portable form of the
//! value. The `Shared` the program holds is a class object of `SHARED_TYPE` that names an entry of
//! the [`SharedTable`] of its [`Core`], like a channel handle does; the handle crosses threads as
//! the `Arc` ([`SharedHandle`] is a [`CrossesAsIs`], memoised by address like every handle), so
//! sending a `Shared` copies nothing.
//!
//! `get()` gives the isolate that calls it a copy of its own, built from the shared graph without
//! consuming it ([`crate::portable::from_portable_frozen`]) and frozen: lists, maps and sets hold
//! their guard for ever, and class objects refuse field writes. The copy is cached in the isolate,
//! so a later `get()` is a pointer copy and gives the same object. The cache is keyed by the
//! address of the `Arc`, with a `Weak` kept beside it: the `Weak` keeps the address from being
//! reused, and tells the isolate that every handle is gone, so that the copy can be freed.
//!
//! A frozen copy is an ordinary value in every other way: it can be read, iterated, shown,
//! compared and cloned. A `clone()` (or a copy sent over a channel) is not frozen.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Weak};

use colibri_check::types::SHARED_TYPE;

use crate::heap::ClassObj;
use crate::portable::{CrossesAsIs, Portable};
use crate::state::Core;
use crate::value::Value;

/// The graph of a `Shared`: made once, never changed, read by every isolate that has a handle.
pub struct SharedInner {
    root: Portable,
}

// The `Arc` is what crosses threads, so this must hold without any `unsafe impl`.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<SharedInner>();
};

impl SharedInner {
    /// A `Shared` of this graph.
    pub fn new(root: Portable) -> Arc<SharedInner> {
        Arc::new(SharedInner { root })
    }

    /// The frozen graph.
    pub fn root(&self) -> &Portable {
        &self.root
    }
}

impl fmt::Debug for SharedInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Shared { .. }")
    }
}

/// A `Shared` as it crosses to another thread: the `Arc` itself. The receiver makes a handle of
/// its own isolate for it.
#[derive(Debug)]
pub struct SharedHandle(pub Arc<SharedInner>);

impl CrossesAsIs for SharedHandle {
    fn materialise(&self, core: &mut Core) -> Value {
        core.shared_handle(Arc::clone(&self.0))
    }

    fn address(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }
}

/// The copy an isolate made for one `Shared`.
struct Cached {
    /// Names the graph without keeping it alive: dead when every handle, in every isolate, is gone.
    graph: Weak<SharedInner>,
    value: Value,
}

/// The `Shared`s an engine holds a handle of, and the copies it made.
#[derive(Default)]
pub struct SharedTable {
    entries: HashMap<u64, Arc<SharedInner>>,
    next: u64,
    /// The frozen copies, by the address of the graph.
    cache: HashMap<usize, Cached>,
}

fn address(inner: &Arc<SharedInner>) -> usize {
    Arc::as_ptr(inner) as usize
}

impl Core {
    /// A new `Shared` of `root`, and its handle.
    pub fn new_shared(&mut self, root: Portable) -> Value {
        self.shared_handle(SharedInner::new(root))
    }

    /// A handle of this engine to `inner`. Each handle is an object of its own, and the table
    /// keeps the `Arc` until the last reference to the object dies ([`Core::shared_died`]).
    pub fn shared_handle(&mut self, inner: Arc<SharedInner>) -> Value {
        let id = self.shareds.next;
        self.shareds.next += 1;
        self.shareds.entries.insert(id, inner);
        Value::Obj(ClassObj::new(SHARED_TYPE, vec![Value::Int(id as i64)]))
    }

    /// The graph a handle names.
    pub fn shared_of(&self, v: &Value) -> Option<Arc<SharedInner>> {
        let (Value::Obj(o) | Value::SomeObj(o)) = v else {
            return None;
        };
        let id = shared_id_of(o)?;
        self.shareds.entries.get(&id).cloned()
    }

    /// The handle `id` died: its graph is let go of (it lives on while another isolate or handle
    /// holds it). The copies of graphs that are gone are freed by [`Core::sweep_shared`], which
    /// the engine calls next because it can release them.
    pub fn shared_died(&mut self, id: u64) {
        let Some(inner) = self.shareds.entries.remove(&id) else {
            return;
        };
        // When this was the last handle anywhere, the copies that other isolates cached are
        // garbage, and an idle worker does not look until its next task: tell the pool.
        let graph = Arc::downgrade(&inner);
        drop(inner);
        if graph.strong_count() == 0
            && let Some(spawner) = self.task_spawner()
        {
            spawner.shared_gone();
        }
    }

    /// The frozen copy this isolate made of `inner`, if it has one.
    pub fn shared_cached(&self, inner: &Arc<SharedInner>) -> Option<Value> {
        self.shareds
            .cache
            .get(&address(inner))
            .map(|c| c.value.clone())
    }

    /// Keeps `value` as this isolate's copy of `inner`.
    pub fn shared_cache(&mut self, inner: &Arc<SharedInner>, value: Value) {
        self.shareds.cache.insert(
            address(inner),
            Cached {
                graph: Arc::downgrade(inner),
                value,
            },
        );
    }

    /// Takes out the copies whose graph has no handle left anywhere, for the engine to release.
    pub fn sweep_shared(&mut self) -> Vec<Value> {
        let dead: Vec<usize> = self
            .shareds
            .cache
            .iter()
            .filter(|(_, c)| c.graph.strong_count() == 0)
            .map(|(a, _)| *a)
            .collect();
        dead.into_iter()
            .filter_map(|a| self.shareds.cache.remove(&a))
            .map(|c| c.value)
            .collect()
    }

    /// Takes out every copy (the engine is ending).
    pub fn take_shared_copies(&mut self) -> Vec<Value> {
        self.shareds.cache.drain().map(|(_, c)| c.value).collect()
    }

    /// How many handles this engine holds (for tests).
    pub fn shared_count(&self) -> usize {
        self.shareds.entries.len()
    }

    /// How many frozen copies this engine keeps (for tests).
    pub fn shared_copies(&self) -> usize {
        self.shareds.cache.len()
    }
}

/// The number inside a `Shared` handle.
pub fn shared_id_of(o: &ClassObj) -> Option<u64> {
    if o.ty() != SHARED_TYPE {
        return None;
    }
    match o.field(0) {
        Some(Value::Int(n)) => Some(n as u64),
        _ => None,
    }
}

/// Whether the graph holds a variable that a closure changes (a shared cell). Such a graph cannot
/// be frozen: the variable would be written through the closure that holds it.
pub fn holds_a_cell(root: &Portable) -> bool {
    let mut work = vec![root];
    while let Some(p) = work.pop() {
        match p {
            Portable::Cell { .. } => return true,
            Portable::Struct { fields: xs, .. }
            | Portable::Obj { fields: xs, .. }
            | Portable::Tuple { items: xs, .. }
            | Portable::List { items: xs, .. }
            | Portable::Set { items: xs, .. }
            | Portable::Enum { payload: xs, .. } => work.extend(xs.iter()),
            Portable::Map { entries, .. } => {
                for (k, v) in entries.iter() {
                    work.push(k);
                    work.push(v);
                }
            }
            Portable::Dyn { val, .. } => work.push(val),
            Portable::Closure { captures, .. } => work.extend(captures.iter().map(|c| &c.value)),
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::ListObj;
    use crate::portable::{StackGuard, Unnamed, from_portable_frozen, to_portable};

    fn core() -> Core {
        Core::new(Box::new(std::io::sink()))
    }

    fn graph() -> Arc<SharedInner> {
        let list = Value::list(vec![Value::Int(1), Value::Int(2)]);
        SharedInner::new(to_portable(&list, &Unnamed, StackGuard::here(1 << 20)).unwrap())
    }

    fn copy_of(core: &mut Core, inner: &Arc<SharedInner>) -> Value {
        match core.shared_cached(inner) {
            Some(v) => v,
            None => {
                let v = from_portable_frozen(inner.root(), core, StackGuard::here(1 << 20))
                    .expect("rebuilt");
                core.shared_cache(inner, v.clone());
                v
            }
        }
    }

    fn list_of(v: &Value) -> &std::rc::Rc<ListObj> {
        match v {
            Value::List(l) => l,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_handle_names_its_graph_and_a_second_handle_names_the_same_one() {
        let mut core = core();
        let inner = graph();
        let a = core.shared_handle(Arc::clone(&inner));
        let b = core.shared_handle(Arc::clone(&inner));
        assert_eq!(core.shared_count(), 2);
        assert!(Arc::ptr_eq(&core.shared_of(&a).unwrap(), &inner));
        assert!(Arc::ptr_eq(&core.shared_of(&b).unwrap(), &inner));
        assert!(core.shared_of(&Value::Int(3)).is_none());
        // Two handles are two objects: their ids differ.
        let (Value::Obj(x), Value::Obj(y)) = (&a, &b) else {
            panic!()
        };
        assert_ne!(shared_id_of(x), shared_id_of(y));
        assert_eq!(Arc::strong_count(&inner), 3);
        core.shared_died(shared_id_of(x).unwrap());
        core.shared_died(shared_id_of(y).unwrap());
        assert_eq!(core.shared_count(), 0);
        assert_eq!(Arc::strong_count(&inner), 1);
        for o in [x, y] {
            drop(o.take_fields());
        }
    }

    #[test]
    fn the_copy_is_made_once_and_is_the_same_object_every_time() {
        let mut core = core();
        let inner = graph();
        assert!(core.shared_cached(&inner).is_none());
        let first = copy_of(&mut core, &inner);
        let second = copy_of(&mut core, &inner);
        assert!(std::rc::Rc::ptr_eq(list_of(&first), list_of(&second)));
        assert_eq!(core.shared_copies(), 1);
        assert!(list_of(&first).guard().is_frozen());
        // Another isolate's copy is a different object of the same graph.
        let mut other = self::tests::core();
        let theirs = copy_of(&mut other, &inner);
        assert!(!std::rc::Rc::ptr_eq(list_of(&first), list_of(&theirs)));
        assert_eq!(list_of(&theirs).snapshot(), list_of(&first).snapshot());
        // A different graph has a copy of its own.
        let inner2 = graph();
        let third = copy_of(&mut core, &inner2);
        assert!(!std::rc::Rc::ptr_eq(list_of(&first), list_of(&third)));
        assert_eq!(core.shared_copies(), 2);
        for c in core
            .take_shared_copies()
            .into_iter()
            .chain(other.take_shared_copies())
        {
            drop(c);
        }
    }

    #[test]
    fn a_copy_stays_while_any_handle_lives_and_is_swept_when_the_last_one_dies() {
        let mut core = core();
        let inner = graph();
        let mine = core.shared_handle(Arc::clone(&inner));
        // Another isolate's handle, which this core knows nothing of.
        let theirs = Arc::clone(&inner);
        drop(inner);
        let inner = core.shared_of(&mine).unwrap();
        let _ = copy_of(&mut core, &inner);
        drop(inner);
        let Value::Obj(o) = &mine else { panic!() };
        core.shared_died(shared_id_of(o).unwrap());
        drop(o.take_fields());
        // This isolate has no handle left, but the graph lives: the copy is kept.
        assert!(core.sweep_shared().is_empty());
        assert_eq!(core.shared_copies(), 1);
        // The other handle dies: the next sweep finds the graph gone.
        drop(theirs);
        let dead = core.sweep_shared();
        assert_eq!(dead.len(), 1);
        assert_eq!(core.shared_copies(), 0);
        drop(dead);
    }

    #[test]
    fn a_dead_graphs_address_is_not_taken_by_a_new_graph_while_its_copy_is_kept() {
        // The cache holds a `Weak`, which keeps the allocation (not the value) alive, so a new
        // graph can never get the address of one whose copy is still in the table.
        let mut core = core();
        let first = graph();
        let _ = copy_of(&mut core, &first);
        let address = Arc::as_ptr(&first) as usize;
        drop(first);
        for _ in 0..64 {
            let next = graph();
            assert_ne!(Arc::as_ptr(&next) as usize, address);
            let _ = copy_of(&mut core, &next);
        }
        let dead = core.sweep_shared();
        // Every graph above was dropped at the end of its turn, so every copy is swept.
        assert_eq!(dead.len(), 65);
        assert_eq!(core.shared_copies(), 0);
    }

    #[test]
    fn the_scan_for_cells_finds_one_at_any_depth() {
        use crate::closure::CellObj;
        let cell = Value::Cell(CellObj::new(Value::Int(1)));
        let nested = Value::list(vec![Value::tuple(vec![Value::Int(1), cell])]);
        let p = to_portable(&nested, &Unnamed, StackGuard::here(1 << 20)).unwrap();
        assert!(holds_a_cell(&p));
        let plain = Value::list(vec![Value::tuple(vec![Value::Int(1), Value::str("x")])]);
        let p = to_portable(&plain, &Unnamed, StackGuard::here(1 << 20)).unwrap();
        assert!(!holds_a_cell(&p));
    }
}
