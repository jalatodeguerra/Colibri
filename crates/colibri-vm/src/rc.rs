//! Reference-count traffic by line (`--profile-rc`).
//!
//! Code compiled with `CompileEnv::count_rc` has an `Op::Count` next to each instruction that
//! copies, clones or releases a value with a reference count. This module keeps what they find:
//! a table from the position of the instruction to three counters. Without the mode there is
//! no such instruction and nothing here runs.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use colibri_syntax::Span;

use crate::bytecode::{Chunk, RcKind};
use crate::fiber::{VmFiber, is_scalar_value};
use crate::machine::Machine;

/// The traffic of one line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RcLine {
    pub file: String,
    pub line: u32,
    /// References taken: a call's arguments, a string literal, a global, a field, an element,
    /// a pattern's part, the item of a `for`, a value pushed or kept by a constructor.
    pub copies: u64,
    /// References released: a temporary or a local that dies, a value that is overwritten.
    pub drops: u64,
    /// Deep copies made by `x.clone()`.
    pub clones: u64,
}

impl RcLine {
    /// What the report ranks by: every reference taken or let go.
    pub fn traffic(&self) -> u64 {
        self.copies + self.drops + self.clones
    }
}

/// What a run with `--profile-rc` found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RcProfile {
    /// By file, then line.
    pub lines: Vec<RcLine>,
    /// Traffic at instructions that have no line (the prelude's, a file that is gone).
    pub unplaced: u64,
}

impl RcProfile {
    pub fn total(&self) -> u64 {
        self.lines.iter().map(RcLine::traffic).sum::<u64>() + self.unplaced
    }

    /// The lines, the most traffic first; equal traffic keeps the order of the source.
    pub fn top(&self) -> Vec<&RcLine> {
        let mut by: Vec<&RcLine> = self.lines.iter().collect();
        by.sort_by(|a, b| {
            b.traffic()
                .cmp(&a.traffic())
                .then_with(|| (&a.file, a.line).cmp(&(&b.file, b.line)))
        });
        by
    }
}

/// A hasher for the few `u32`s of a span: the lookup is on the path of every counted
/// instruction, where the standard hasher costs more than the counting.
#[derive(Default)]
struct SpanHasher(u64);

impl Hasher for SpanHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u32(u32::from(b));
        }
    }

    fn write_u32(&mut self, n: u32) {
        self.0 = (self.0.rotate_left(5) ^ u64::from(n)).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

/// The counters while a run is counting.
#[derive(Default)]
pub(crate) struct RcCounter {
    slots: HashMap<Span, usize, BuildHasherDefault<SpanHasher>>,
    counts: Vec<[u64; 3]>,
    /// The slot of the last count: a loop hits the same few instructions over and over.
    last: Option<(Span, usize)>,
}

impl RcCounter {
    fn add(&mut self, span: Span, kind: RcKind, n: u64) {
        let slot = match self.last {
            Some((s, slot)) if s == span => slot,
            _ => {
                let counts = &mut self.counts;
                let slot = *self.slots.entry(span).or_insert_with(|| {
                    counts.push([0; 3]);
                    counts.len() - 1
                });
                self.last = Some((span, slot));
                slot
            }
        };
        self.counts[slot][kind as usize] += n;
    }
}

impl Machine {
    /// Makes the code that is compiled from now on count its reference-count traffic. Call it
    /// before the program loads: what is already compiled has no counters.
    #[doc(hidden)]
    pub fn start_rc_profile(&mut self) {
        self.rc = Some(Box::default());
        self.env.count_rc = true;
    }

    /// Stops counting and returns the lines, with `locate` turning a span into a file name and
    /// a 1-based line (`None`: no line).
    #[doc(hidden)]
    pub fn finish_rc_profile(
        &mut self,
        locate: &dyn Fn(Span) -> Option<(String, u32)>,
    ) -> Option<RcProfile> {
        let counter = self.rc.take()?;
        self.env.count_rc = false;
        let mut by_line: HashMap<(String, u32), [u64; 3]> = HashMap::new();
        let mut out = RcProfile::default();
        for (span, slot) in counter.slots {
            let [copies, drops, clones] = counter.counts[slot];
            match locate(span) {
                Some(place) => {
                    let c = by_line.entry(place).or_default();
                    c[0] += copies;
                    c[1] += drops;
                    c[2] += clones;
                }
                None => out.unplaced += copies + drops + clones,
            }
        }
        out.lines = by_line
            .into_iter()
            .map(|((file, line), [copies, drops, clones])| RcLine {
                file,
                line,
                copies,
                drops,
                clones,
            })
            .collect();
        out.lines
            .sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
        Some(out)
    }

    /// `Op::Count`: the registers `r..r+n` that hold something with a reference count. Kept
    /// out of the dispatch loop's body: only code compiled for `--profile-rc` runs it.
    #[inline(never)]
    #[cold]
    pub(crate) fn op_count(
        &mut self,
        fiber: &VmFiber,
        chunk: &Chunk,
        base: usize,
        pc: usize,
        (r, n, kind): (u16, u16, RcKind),
    ) {
        let Some(counter) = self.rc.as_mut() else {
            return;
        };
        let from = base + usize::from(r);
        let regs = fiber.regs.get(from..from + usize::from(n)).unwrap_or(&[]);
        let hits = regs.iter().filter(|v| !is_scalar_value(v)).count() as u64;
        if hits == 0 {
            return;
        }
        // A built-in written in the prelude is charged to the call that reached it.
        let span = if chunk.hidden {
            fiber
                .frames
                .iter()
                .rev()
                .skip(1)
                .find(|f| !f.chunk.hidden)
                .map_or(chunk.spans[pc - 1], |f| f.span())
        } else {
            chunk.spans[pc - 1]
        };
        counter.add(span, kind, hits);
    }
}
