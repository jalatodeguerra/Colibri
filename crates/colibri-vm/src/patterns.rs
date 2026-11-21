//! The pattern tests that are not worth a place in the dispatch loop: literals that are not
//! small integers, ranges and constants (vm-plan §2.7).

use colibri_core::compare::{equal_or_give_up, take_gave_up};
use colibri_core::ctl::Ctl;
use colibri_core::pattern::{lit_matches, range_matches};
use colibri_core::prelude::*;

use crate::bytecode::{Chunk, Op, Site};
use crate::fiber::VmFiber;
use crate::machine::Machine;

impl Machine {
    /// Runs a literal, range or constant test and says whether the value matches. A constant
    /// whose comparison gave up (the values nest too deeply) is a panic, raised before the
    /// stand-in answer is used.
    #[inline(never)]
    pub(crate) fn op_test(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        op: Op,
    ) -> Result<bool, Ctl> {
        let span = chunk.spans[pc - 1];
        match op {
            Op::TestLit { r, site } => {
                let Some(Site::Lit(lit)) = chunk.sites.get(usize::from(site)) else {
                    return Err(self.internal(fiber, span, "a literal pattern without its site"));
                };
                Ok(lit_matches(lit, &fiber.regs[base + usize::from(r)]))
            }
            Op::TestRange { r, site } => {
                let Some(Site::Range { lo, hi, inclusive }) = chunk.sites.get(usize::from(site))
                else {
                    return Err(self.internal(fiber, span, "a range pattern without its site"));
                };
                Ok(range_matches(
                    lo.as_ref(),
                    hi.as_ref(),
                    *inclusive,
                    &fiber.regs[base + usize::from(r)],
                ))
            }
            Op::TestConst { r, g } => {
                let Some(constant) = self.globals.get(g as usize) else {
                    return Ok(false);
                };
                let hit = equal_or_give_up(constant, &fiber.regs[base + usize::from(r)]);
                if take_gave_up() {
                    return Err(self.panic(
                        fiber,
                        span,
                        "these values nest too deeply to compare".to_string(),
                    ));
                }
                Ok(hit)
            }
            _ => Err(self.internal(fiber, span, "not a pattern test")),
        }
    }
}
