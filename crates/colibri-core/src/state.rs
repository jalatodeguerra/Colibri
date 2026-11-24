//! [`Core`]: the state that every engine of Colibri shares.

use std::io::Write;
use std::rc::Rc;

use colibri_check::hir::{FnId, TypeDef};

use crate::budget::Budget;
use crate::closure::ClosureObj;
use crate::ctl::PanicInfo;
use crate::dispatch::Tables;
use crate::host::HostSide;
use crate::leaks::LeakTracker;
use crate::sched::Scheduler;
use crate::value::Value;

/// What an engine and the engine-neutral runtime code share: the output writer, the drop trace,
/// the `Drop` hooks' panics, the instruction budget, and the tables of the loaded program.
///
/// An engine owns one `Core` and reaches the fields directly (`self.core.budget`), so that the
/// borrows of two fields stay disjoint. The fields are internals: hosts go through
/// `colibri-runtime`.
#[doc(hidden)]
pub struct Core {
    /// Where `print` writes.
    pub out: Box<dyn Write>,
    /// The labels of destroyed class objects, when tracing is on (`set_trace_drops`).
    pub trace: Option<Vec<String>>,
    /// Panics in `Drop` hooks that no running statement has picked up yet.
    pub hook_panics: Vec<PanicInfo>,
    /// How many `Drop` hooks are running, one inside another.
    pub hook_depth: u32,
    /// The instruction budget (EMBEDDING §5).
    pub budget: Budget,
    /// How many times a map or set key was looked up by scanning the stored keys.
    pub key_scans: u64,
    /// The user-defined types, which `Show` and drop tracing need for names.
    pub types: Rc<[TypeDef]>,
    /// Interface implementations and the user-written methods that replace derived behaviour.
    pub dispatch: Tables,
    /// For each extension of the program (`session_extend`), how the function ids of the
    /// generation before map to the one after (`u32::MAX`: the function is gone).
    pub closure_maps: Vec<Vec<u32>>,
    /// What the embedding program attached: state, roots, host types and the host heap.
    pub host: HostSide,
    /// The registry of the leak detector, when it is on (ARCHITECTURE §18).
    pub leaks: Option<Box<LeakTracker>>,
    /// The coroutine scheduler (ARCHITECTURE §16). Last, so it drops last: dropping it cancels
    /// the background operations that are still pending, which is the last thing that happens
    /// to an engine.
    pub sched: Scheduler,
    /// The prelude's `none`, shared by every `T?` that has no value: enum values are
    /// immutable and nothing can tell two empty enum values apart, so one is enough.
    none: Value,
}

impl Core {
    /// Writes text to the program's standard output, where `print` writes (without a newline).
    pub fn write_stdout(&mut self, text: &str) {
        let _ = self.out.write_all(text.as_bytes());
    }

    /// Writes buffered output through to the writer.
    pub fn flush(&mut self) {
        let _ = self.out.flush();
    }

    /// A core with nothing loaded, writing program output to `out`.
    pub fn new(out: Box<dyn Write>) -> Core {
        Core {
            out,
            trace: None,
            hook_panics: Vec::new(),
            hook_depth: 0,
            budget: Budget::new(),
            key_scans: 0,
            types: Rc::from(Vec::new()),
            dispatch: Tables::default(),
            closure_maps: Vec::new(),
            host: HostSide::default(),
            leaks: None,
            sched: Scheduler::new(),
            none: Value::option(None),
        }
    }

    /// `none`, without an allocation.
    #[inline]
    pub fn none(&self) -> Value {
        self.none.clone()
    }

    /// The prelude `Option`: the shared `none`, or `some(v)`.
    #[inline]
    pub fn option(&self, inner: Option<Value>) -> Value {
        match inner {
            Some(v) => Value::option(Some(v)),
            None => self.none(),
        }
    }

    /// How many times the program was extended since it was loaded.
    #[inline]
    pub fn generation(&self) -> u32 {
        self.closure_maps.len() as u32
    }

    /// A session entry that extended the program (its map is `closure_maps[maps_len]`) failed
    /// and the old code tables came back. The generations are not given up, because a closure
    /// the entry made or stored carries the generation it saw: a map back to the old numbering is
    /// added after the entry's map (`failed_fns` is the size of the entry's function table).
    /// A closure made in code that survives finds its old body again; one made in code that
    /// only the entry had finds none (`closure_fn` says so) instead of a function that happens
    /// to have its number.
    pub fn retire_extension_closures(&mut self, maps_len: usize, failed_fns: usize) {
        self.closure_maps.truncate(maps_len + 1);
        let Some(forward) = self.closure_maps.get_mut(maps_len) else {
            return;
        };
        let mut back = vec![u32::MAX; failed_fns];
        for (old, new) in forward.iter_mut().enumerate() {
            if *new == u32::MAX {
                // The entry's program has no counterpart of this old function, but the restored
                // one does: a slot past the entry's table, which no closure of the entry holds,
                // so going forward and back is the identity on every old id.
                *new = back.len() as u32;
                back.push(old as u32);
            } else if let Some(slot) = back.get_mut(*new as usize) {
                *slot = old as u32;
            }
        }
        self.closure_maps.push(back);
    }

    /// The function id a closure made in an earlier generation has now.
    #[inline]
    pub fn closure_fn(&self, c: &ClosureObj) -> Option<FnId> {
        let mut id = c.f.0;
        for map in self
            .closure_maps
            .get(c.generation as usize..)
            .unwrap_or(&[])
        {
            id = *map.get(id as usize)?;
            if id == u32::MAX {
                return None;
            }
        }
        Some(FnId(id))
    }
}
