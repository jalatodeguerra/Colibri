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

use std::rc::Rc;

use colibri_core::value::Value;

use crate::bytecode::{Chunk, Op};

/// The code of `chunk` as a slice that does not borrow the `Rc`, so the dispatch loop can keep
/// it in registers and still replace its `chunk` when a call or a return changes the frame.
/// Indexing it stays bounds checked.
///
/// # Safety
/// The caller keeps `chunk` (or another `Rc` of the same chunk) alive for as long as it uses the
/// slice, and stops using the slice when it drops or replaces the last such `Rc`: `exec` reassigns
/// the slice in the same statement block as it replaces its `chunk`.
#[inline(always)]
pub(crate) unsafe fn code_of<'a>(chunk: &Rc<Chunk>) -> &'a [Op] {
    // SAFETY: the chunk's code is never mutated once the chunk is in an `Rc`, and the caller
    // keeps the chunk alive (see above).
    unsafe { &*std::ptr::from_ref::<[Op]>(&chunk.code[..]) }
}

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
