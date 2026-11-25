//! Running the consumers of safepoints (see `colibri_core::safepoint`).

use colibri_core::ctl::Ctl;
use colibri_core::safepoint::SafepointKind;

use crate::fiber::VmFiber;
use crate::machine::Machine;

/// What a test hangs on the safepoints to see them run.
pub type Probe = Box<dyn FnMut(SafepointKind)>;

impl Machine {
    /// Runs the consumers whose interval has passed, in the fixed order: memory, debugger,
    /// profiler. The fiber's frames are in step with the running instruction (the slow paths
    /// that call this synchronise the pc first). A consumer that fails ends the operation.
    pub(crate) fn safepoint(&mut self, _fiber: &mut VmFiber) -> Result<(), Ctl> {
        let due: Vec<SafepointKind> = self.core.take_due_safepoints().collect();
        for kind in due {
            if let Some(probe) = self.safepoint_probe.as_mut() {
                probe(kind);
            }
            match kind {
                // No consumer is built yet: each arrives with its own step.
                SafepointKind::Memory | SafepointKind::Debugger | SafepointKind::Profiler => {}
            }
        }
        Ok(())
    }

    /// Hangs `probe` on the safepoints: it is called with the kind of each one that runs, in
    /// the order they run. Tests only.
    #[doc(hidden)]
    pub fn set_safepoint_probe(&mut self, probe: Option<Probe>) {
        self.safepoint_probe = probe;
    }
}
