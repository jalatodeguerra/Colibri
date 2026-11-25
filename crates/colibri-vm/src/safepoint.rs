//! Running the consumers of safepoints (see `colibri_core::safepoint`).

use colibri_core::ctl::Ctl;
use colibri_core::prelude::*;
use colibri_core::safepoint::{Due, SafepointKind};

use crate::fiber::VmFiber;
use crate::machine::Machine;

/// Says, for a consumer that is due, whether it fails (tests only).
pub type Failure = Box<dyn FnMut(Due) -> bool>;

/// What a test hangs on the safepoints to see them run: each consumer that runs, and the fiber
/// it is given.
pub type Probe = Box<dyn FnMut(Due, &VmFiber)>;

impl Machine {
    /// Runs the consumers whose interval has passed, in the fixed order: memory, debugger,
    /// profiler. The fiber's frames are in step with the running instruction (the slow paths
    /// that call this synchronise the pc first), and a consumer only reads them: the loop that
    /// called goes on with the state it cached. When one fails (the memory limit) the others
    /// that are due still run, and the first failure ends the operation.
    pub(crate) fn safepoint(&mut self, fiber: &mut VmFiber) -> Result<(), Ctl> {
        debug_assert!(
            self.core.safepoint_due,
            "a safepoint without a slice that ended"
        );
        let mut failure = None;
        for due in self.core.take_due_safepoints().into_iter().flatten() {
            if let Some(probe) = self.safepoint_probe.as_mut() {
                probe(due, fiber);
            }
            let forced = self
                .safepoint_failure
                .as_mut()
                .is_some_and(|fails| fails(due));
            let outcome: Result<(), Ctl> = if forced {
                let span = fiber.frames.last().map_or_else(
                    || colibri_syntax::Span::new(colibri_syntax::FileId(0), 0, 0),
                    |f| f.span(),
                );
                Err(self.panic(fiber, span, "a safepoint consumer failed"))
            } else {
                match due.kind {
                    // No consumer is built yet: each arrives with its own step.
                    SafepointKind::Memory | SafepointKind::Debugger => Ok(()),
                    SafepointKind::Profiler => {
                        if let Some(profiler) = self.profiler.as_mut() {
                            profiler.sample(due, fiber, self.core.sched.idle_nanos());
                        }
                        Ok(())
                    }
                }
            };
            if let Err(ctl) = outcome
                && failure.is_none()
            {
                failure = Some(ctl);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Makes the consumers fail where `fails` says so (it is asked for each one that runs): a
    /// stand-in for the memory limit. Tests only.
    #[doc(hidden)]
    pub fn set_safepoint_failure(&mut self, fails: Option<Failure>) {
        self.safepoint_failure = fails;
    }

    /// Hangs `probe` on the safepoints: it is called for each consumer that runs, in the order
    /// they run. Tests only.
    #[doc(hidden)]
    pub fn set_safepoint_probe(&mut self, probe: Option<Probe>) {
        self.safepoint_probe = probe;
    }
}
