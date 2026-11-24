//! Unwinding: finding the landing pad of a panic, and folding failures that happen while one
//! is on its way out (vm-plan §2.4; the rules are the tree-walker's `scope.rs`).
//!
//! A panic or a cancellation is a `Ctl` that `exec` hands to [`Machine::catch`]. The chunk's
//! unwind table says which pad covers the instruction the frame is at. The pad is ordinary
//! code that drops and runs `defer`s in scope order and ends in `Resume`, which hands the
//! frame's in-flight `Ctl` to the caller's pad. A failure inside a pad is folded into the one
//! already on its way: the first panic stays the panic and later ones become notes.

use colibri_core::ctl::{Ctl, Eval, PanicInfo};
use colibri_core::prelude::*;
use colibri_core::value::Value;

use crate::fiber::{FrameKind, VmFiber, is_scalar_value};
use crate::machine::Machine;

// vm-pin: refusal (the leaks that keep a refused run silent go when no feature is refused)
/// What every refusal of an unsupported feature starts with.
pub(crate) const REFUSAL: &str = "the bytecode VM does not support";

impl Machine {
    /// Continues unwinding `ctl` from the top frame: into its pad when the instruction it is at
    /// has one (`Ok`: the dispatch loop reloads and runs the pad), else out of the frame into
    /// its caller. `Err` when the frame that began this dispatch loop is gone: the failure goes
    /// back to Rust. `skip_top` is `Resume`: the top frame's pad has run, so it is left at once.
    pub(crate) fn catch(
        &mut self,
        fiber: &mut VmFiber,
        mut ctl: Ctl,
        mut skip_top: bool,
    ) -> Result<(), Ctl> {
        // The VM's refusal of a feature is not the program's own panic: nothing the program
        // wrote (a `defer`, a `Drop` hook) runs for it, so output stops where the refusal is.
        // The cancellation of a coroutine after a refusal (at shutdown, between tests) is the same:
        // the program stopped, and nothing it wrote runs.
        let refusal = matches!(&ctl, Ctl::Panic(info) if info.message.starts_with(REFUSAL))
            || (self.refused && matches!(ctl, Ctl::Cancel));
        loop {
            let frame = fiber.frames.last_mut().expect("a frame to unwind");
            if !skip_top && !refusal {
                let at = frame.pc.saturating_sub(1);
                if let Some(pad) = frame.chunk.pad_for(at) {
                    // A failure inside a pad joins the one already on its way out.
                    let idx = fiber.frames.len() - 1;
                    let ctl = match fiber.take_unwinding(idx) {
                        Some(inflight) => match self.fold_cleanup_failure(Err(*inflight), ctl) {
                            Err(folded) => folded,
                            Ok(_) => unreachable!("a folded failure is an error"),
                        },
                        None => ctl,
                    };
                    fiber.set_unwinding(idx, Box::new(ctl));
                    let frame = fiber.frames.last_mut().expect("a frame to unwind");
                    frame.pc = pad;
                    return Ok(());
                }
            }
            skip_top = false;
            // No pad: the frame owns nothing. (A compiler bug would leave values in its window;
            // debug builds assert it, release builds release them rather than leak.)
            let frame = fiber.frames.pop().expect("a frame to unwind");
            fiber.hidden -= usize::from(frame.chunk.hidden);
            let inflight = fiber.take_unwinding(fiber.frames.len());
            let base = frame.base as usize;
            let chunk = &frame.chunk;
            let (from, to) = (base + chunk.nparams as usize, base + chunk.nregs as usize);
            for i in (from..to).rev() {
                if refusal {
                    // Leaked on purpose: releasing would run hooks.
                    std::mem::forget(std::mem::replace(&mut fiber.regs[i], Value::Unit));
                } else if !is_scalar_value(&fiber.regs[i]) {
                    debug_assert!(
                        false,
                        "register r{} of `{}` still owns {:?} after unwinding",
                        i - base,
                        chunk.body.name,
                        fiber.regs[i]
                    );
                    let v = std::mem::replace(&mut fiber.regs[i], Value::Unit);
                    self.release(v);
                }
            }
            if frame.kind == FrameKind::NativeEntry {
                self.refused |= refusal;
                if refusal {
                    // Leaked on purpose, like the registers.
                    for v in fiber.temps.drain(frame.temps_mark as usize..) {
                        std::mem::forget(v);
                    }
                    return Err(ctl);
                }
                self.release_temps(fiber, frame.temps_mark as usize);
                return Err(ctl);
            }
            // The caller continues unwinding from the call it made. (A failure in a pad that no
            // further pad covers still joins the one on its way out.)
            if let Some(inflight) = inflight {
                ctl = match self.fold_cleanup_failure(Err(*inflight), ctl) {
                    Err(folded) => folded,
                    Ok(_) => unreachable!("a folded failure is an error"),
                };
            }
        }
    }

    /// A deferred statement (or a hook fold inside a pad) ended abnormally while `result` was
    /// already on its way out.
    pub(crate) fn fold_cleanup_failure(&mut self, result: Eval, failure: Ctl) -> Eval {
        let failure = match failure {
            Ctl::Panic(info) => info,
            // A coroutine cancelled while a `defer` was pausing: the cancellation continues,
            // and a panic already on its way stays the panic.
            Ctl::Cancel => {
                return match result {
                    Err(Ctl::Panic(_) | Ctl::Cancel) => result,
                    other => {
                        self.release_outcome(other);
                        Err(Ctl::Cancel)
                    }
                };
            }
            // The checker does not allow these in a `defer`; they carry nothing to keep.
            Ctl::Break(_, v) | Ctl::Return(v) => {
                self.release(v);
                return result;
            }
            Ctl::Continue(_) => return result,
        };
        self.merge_failure(result, failure, "a deferred statement also panicked")
    }

    /// `failure` happened while `result` was on its way out. The first panic is the one
    /// reported and `failure` rides along as a note, except that running out of budget always
    /// wins (the host must see it), and a second budget failure adds nothing.
    fn merge_failure(&mut self, result: Eval, failure: Box<PanicInfo>, what: &str) -> Eval {
        match result {
            Err(Ctl::Panic(mut first)) if first.budget => {
                // The grace of an exhausted budget ran out too; nothing to add.
                let _ = failure;
                first.budget = true;
                Err(Ctl::Panic(first))
            }
            Err(Ctl::Panic(first)) if failure.budget => {
                let mut winner = failure;
                winner
                    .notes
                    .push(format!("while unwinding the panic: {}", first.message));
                Err(Ctl::Panic(winner))
            }
            Err(Ctl::Panic(mut first)) => {
                first.notes.push(format!("{what}: {}", failure.message));
                first.notes.extend(failure.notes);
                Err(Ctl::Panic(first))
            }
            other => {
                self.release_outcome(other);
                Err(Ctl::Panic(failure))
            }
        }
    }

    /// Folds the panics of `Drop` hooks that ran meanwhile into `result`: the first panic is
    /// the one reported, later ones ride along as notes (ARCHITECTURE §6).
    pub(crate) fn fold_hook_panics(&mut self, result: Eval) -> Eval {
        if self.core.hook_panics.is_empty() {
            return result;
        }
        let mut panics = std::mem::take(&mut self.core.hook_panics).into_iter();
        // A refusal that no resumer settled (a `CoGroup` died, and a member's `defer` met it)
        // is raised here, as the failure of the code whose release it was.
        if let Some((id, _)) = self.pending_refusal.take() {
            self.core.sched.forget_failure_of(id);
        }
        let mut result = result;
        for failure in &mut panics {
            result = self.merge_failure(result, Box::new(failure), "a `Drop` hook also panicked");
        }
        result
    }
}
