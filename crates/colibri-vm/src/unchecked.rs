//! The only unchecked code of the bytecode VM: the register accesses of the dispatch loop, and the
//! append of ASCII digits to a string (`push_int`).
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

/// Appends the decimal form of `x`, as `show` writes an integer (without the formatting
/// machinery, which is most of the cost of building a short string).
pub(crate) fn push_int(out: &mut String, x: i64) {
    const PAIRS: &[u8; 200] = b"0001020304050607080910111213141516171819\
2021222324252627282930313233343536373839\
4041424344454647484950515253545556575859\
6061626364656667686970717273747576777879\
8081828384858687888990919293949596979899";
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut n = x.unsigned_abs();
    while n >= 100 {
        let r = (n % 100) as usize * 2;
        n /= 100;
        at -= 2;
        digits[at] = PAIRS[r];
        digits[at + 1] = PAIRS[r + 1];
    }
    if n >= 10 {
        let r = n as usize * 2;
        at -= 2;
        digits[at] = PAIRS[r];
        digits[at + 1] = PAIRS[r + 1];
    } else {
        at -= 1;
        digits[at] = b'0' + n as u8;
    }
    if x < 0 {
        out.push('-');
    }
    // SAFETY: the buffer holds ASCII digits only.
    unsafe { push_ascii(out, &digits[at..]) }
}

/// Appends ASCII bytes to a string without the UTF-8 check.
///
/// # Safety
/// Every byte of `bytes` is below 0x80.
#[inline(always)]
unsafe fn push_ascii(out: &mut String, bytes: &[u8]) {
    if cfg!(debug_assertions) {
        out.push_str(std::str::from_utf8(bytes).expect("ASCII digits"));
    } else {
        // SAFETY: ASCII bytes are valid UTF-8 on their own, so the string stays valid.
        unsafe { out.as_mut_vec() }.extend_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::push_int;

    #[test]
    fn integers_are_written_as_show_writes_them() {
        for x in [
            0,
            1,
            9,
            10,
            11,
            99,
            100,
            101,
            999,
            1000,
            12_345,
            -1,
            -10,
            -100,
            -4_611_686_018_427_387_904,
            i64::MAX,
            i64::MIN,
        ] {
            let mut out = String::from(">");
            push_int(&mut out, x);
            assert_eq!(out, format!(">{x}"));
        }
    }
}
