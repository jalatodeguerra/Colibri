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
}

impl Core {
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
        }
    }

    /// How many times the program was extended since it was loaded.
    #[inline]
    pub fn generation(&self) -> u32 {
        self.closure_maps.len() as u32
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
