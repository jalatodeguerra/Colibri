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

use std::ops::Deref;
use std::rc::Rc;

use colibri_core::value::Value;

use crate::bytecode::{Chunk, Op};

/// The chunk the dispatch loop runs, with its code as a slice kept next to it, so that fetching
/// an instruction is one indexing and not a walk through the `Rc` and the chunk's vector.
///
/// The two cannot be set apart: `new` and `set` take the `Rc` and derive the slice from it, and
/// the only way to read the slice is `ops`, whose lifetime is that of the borrow of `self`.
pub(crate) struct Running {
    chunk: Rc<Chunk>,
    /// The code of `chunk`, with the lifetime erased. Never exposed with a longer lifetime than
    /// a borrow of `self`.
    ops: *const [Op],
}

impl Running {
    #[inline(always)]
    pub(crate) fn new(chunk: Rc<Chunk>) -> Running {
        let ops = std::ptr::from_ref::<[Op]>(&chunk.code[..]);
        Running { chunk, ops }
    }

    /// Runs `chunk` from now on.
    #[inline(always)]
    pub(crate) fn set(&mut self, chunk: Rc<Chunk>) {
        *self = Running::new(chunk);
    }

    /// The code of the chunk.
    #[inline(always)]
    pub(crate) fn ops(&self) -> &[Op] {
        // SAFETY: `ops` points into the vector of `self.chunk`, which `self` keeps alive, and
        // nothing mutates the code of a chunk inside an `Rc`. The vector is not moved: `Rc`
        // keeps it in place whatever happens to `self`.
        #[allow(unsafe_code)]
        unsafe {
            &*self.ops
        }
    }
}

impl Deref for Running {
    type Target = Chunk;
    #[inline(always)]
    fn deref(&self) -> &Chunk {
        &self.chunk
    }
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
