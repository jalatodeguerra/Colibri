//! The only unchecked indexing of the bytecode VM: the register accesses of the dispatch loop.
//!
//! `exec` reads and writes the registers of the running frame several times per instruction, and
//! a bounds check on each is a fifth of its work. The accesses are sound because of an invariant
//! the machine keeps and the compiler proves, not because of anything in this file; the callers
//! (the `rd!` and `wr!` macros of `run.rs`) state it, and `Op::unchecked_regs_end` and
//! `Chunk::verify_registers` are its proof obligation:
//!
//! 1. the register file holds the whole window of the running frame (`base + nregs` registers);
//! 2. every register an instruction accesses through these functions is below `nregs`.
//!
//! Debug builds (the test suites) check the index, so a violation fails there with a panic
//! instead of reading out of bounds.

use colibri_core::value::Value;

/// The register at `at`.
///
/// # Safety
/// `at < regs.len()`.
#[inline(always)]
pub(crate) unsafe fn get(regs: &[Value], at: usize) -> &Value {
    if cfg!(debug_assertions) {
        &regs[at]
    } else {
        // SAFETY: the caller guarantees `at < regs.len()`.
        unsafe { regs.get_unchecked(at) }
    }
}

/// The register at `at`, to write.
///
/// # Safety
/// `at < regs.len()`.
#[inline(always)]
pub(crate) unsafe fn get_mut(regs: &mut [Value], at: usize) -> &mut Value {
    if cfg!(debug_assertions) {
        &mut regs[at]
    } else {
        // SAFETY: the caller guarantees `at < regs.len()`.
        unsafe { regs.get_unchecked_mut(at) }
    }
}
