//! The sampling profiler: a consumer of safepoints (see `colibri_core::safepoint`).
//!
//! At each safepoint the profiler reads the frames of the running fiber (it only reads) and
//! adds the weight of the sample to the stack it finds. The weight is either the steps since the
//! previous safepoint (deterministic, what the tests use) or the real time since then, so time in
//! host functions and long built-ins counts; it lands on the stack at the safepoint after the
//! call, which is the caller's line.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use colibri_core::safepoint::{Due, SafepointKind};
use colibri_syntax::Span;

use crate::bytecode::Chunk;
use crate::fiber::{FrameKind, VmFiber};
use crate::machine::Machine;

/// How many steps pass between two samples.
pub const PROFILE_SLICE: u32 = 1024;

/// What a sample weighs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileWeight {
    /// The steps since the previous sample: the same on every run of the same program.
    Steps,
    /// The nanoseconds of real time since the previous sample.
    Time,
}

/// Stacks deeper than this are cut: the first `HEAD_FRAMES` and the last `TAIL_FRAMES` frames stay.
const MAX_FRAMES: usize = 200;
const HEAD_FRAMES: usize = 40;
const TAIL_FRAMES: usize = 160;
/// What stands for the frames that were cut.
const ELIDED: &str = "[...]";

/// The stack of a sample that was taken when no frame was left to name (a host calling in).
const NO_FRAMES: &str = "[host]";
/// The position of a frame that has none (`[idle]`, `[unsampled]`, `[host]`): no file has this
/// id, so the locator finds no line for it.
const NO_POSITION: Span = Span {
    file: colibri_syntax::FileId(u32::MAX),
    start: 0,
    end: 0,
};
/// The root of the time the event loop spent sleeping (a time profile).
const IDLE: &str = "[idle]";
/// The stack of weight that was never sampled, when the program ended before any sample.
const UNSAMPLED: &str = "[unsampled]";

/// One stack with its weight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSample {
    /// The frames, the outermost first. A coroutine's first frame reads `go name`, a host
    /// function `[native: name]`, and a built-in written in the prelude `List.map` and the like.
    pub stack: Vec<String>,
    /// The line the innermost visible frame was at (`line` 0: no known position).
    pub file: String,
    pub line: u32,
    pub weight: u64,
}

/// What a profiled run found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub weight: ProfileWeight,
    /// Sorted by stack, then position, so the same run always gives the same list.
    pub samples: Vec<ProfileSample>,
}

impl Profile {
    /// The sum of all weights.
    pub fn total(&self) -> u64 {
        self.samples.iter().map(|s| s.weight).sum()
    }

    /// The stacks with their weights, one per line, `main;load;parse 1234`, sorted by stack.
    /// Stacks that differ only in the line merge.
    pub fn folded(&self) -> String {
        let mut out = String::new();
        for (stack, weight) in self.stacks() {
            out.push_str(&stack.join(";"));
            out.push(' ');
            out.push_str(&weight.to_string());
            out.push('\n');
        }
        out
    }

    /// The distinct stacks with their weights, sorted by stack.
    pub fn stacks(&self) -> Vec<(Vec<&str>, u64)> {
        let mut merged: Vec<(Vec<&str>, u64)> = Vec::new();
        for s in &self.samples {
            let stack: Vec<&str> = s.stack.iter().map(String::as_str).collect();
            match merged.last_mut() {
                Some((last, w)) if *last == stack => *w += s.weight,
                _ => merged.push((stack, s.weight)),
            }
        }
        // `samples` is sorted by stack, so equal stacks are neighbours.
        merged
    }
}

/// A frame as the profiler names it.
#[derive(Clone, Copy)]
struct Named {
    id: u32,
    /// A frame of the prelude's own code.
    hidden: bool,
}

/// The state of a profiled run.
pub(crate) struct Profiler {
    weight: ProfileWeight,
    names: Vec<String>,
    name_ids: HashMap<String, u32>,
    /// Names of chunks by address; the chunk is kept so the address stays its own.
    chunks: HashMap<*const Chunk, (Rc<Chunk>, Named)>,
    /// `go name`, by the id of `name`.
    go_names: HashMap<u32, u32>,
    /// Weights by stack, then by the position of the innermost visible frame.
    samples: HashMap<Box<[u32]>, HashMap<Span, u64>>,
    scratch: Vec<u32>,
    last_time: Instant,
    /// The scheduler's idle nanoseconds (`Scheduler::idle_nanos`) at the previous sample: the
    /// time it spent sleeping since is the program waiting, not working.
    last_idle: u64,
    /// Where the most recent sample landed, for the weight that comes after the last safepoint.
    last: Option<(Box<[u32]>, Span)>,
}

/// `__list_map` is what the prelude calls `List.map`; the other built-ins follow the same pattern.
fn builtin_name(name: &str) -> String {
    let Some(rest) = name.strip_prefix("__") else {
        return name.to_string();
    };
    match rest.split_once('_') {
        Some(("list", m)) => format!("List.{m}"),
        Some(("option", m)) => format!("Option.{m}"),
        Some(("result", m)) => format!("Result.{m}"),
        _ => rest.to_string(),
    }
}

impl Profiler {
    pub(crate) fn new(weight: ProfileWeight, idle: u64) -> Self {
        Profiler {
            weight,
            names: Vec::new(),
            name_ids: HashMap::new(),
            chunks: HashMap::new(),
            go_names: HashMap::new(),
            samples: HashMap::new(),
            scratch: Vec::new(),
            last_time: Instant::now(),
            last_idle: idle,
            last: None,
        }
    }

    /// Starts the clock of a time profile again: what came before (checking and compiling
    /// the program) is not the program's time.
    pub(crate) fn restart_clock(&mut self, idle: u64) {
        self.last_time = Instant::now();
        self.last_idle = idle;
    }

    /// The real time since the previous reading, split into the time spent working and the
    /// time the event loop spent sleeping; the sleeping goes to the `[idle]` root.
    fn take_time(&mut self, idle: u64) -> u64 {
        let now = Instant::now();
        let spent =
            u64::try_from(now.duration_since(self.last_time).as_nanos()).unwrap_or(u64::MAX);
        self.last_time = now;
        let slept = idle.saturating_sub(self.last_idle).min(spent);
        self.last_idle = idle;
        if slept > 0 {
            let id = self.intern(IDLE.to_string());
            let stack: Box<[u32]> = Box::from([id].as_slice());
            self.record(stack, NO_POSITION, slept);
        }
        spent - slept
    }

    fn intern(&mut self, name: String) -> u32 {
        if let Some(&id) = self.name_ids.get(&name) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(name.clone());
        self.name_ids.insert(name, id);
        id
    }

    fn name_of(&mut self, chunk: &Rc<Chunk>, kind: FrameKind) -> Named {
        let key = Rc::as_ptr(chunk);
        if let Some((_, named)) = self.chunks.get(&key) {
            return *named;
        }
        let raw = &chunk.body.name;
        let text = if kind == FrameKind::HostShim {
            format!("[native: {raw}]")
        } else if chunk.hidden {
            builtin_name(raw)
        } else {
            raw.clone()
        };
        let named = Named {
            id: self.intern(text),
            hidden: chunk.hidden,
        };
        self.chunks.insert(key, (Rc::clone(chunk), named));
        named
    }

    fn go_name(&mut self, id: u32) -> u32 {
        if let Some(&go) = self.go_names.get(&id) {
            return go;
        }
        let text = format!("go {}", self.names[id as usize]);
        let go = self.intern(text);
        self.go_names.insert(id, go);
        go
    }

    /// Takes the sample of a safepoint that is due.
    pub(crate) fn sample(&mut self, due: Due, fiber: &VmFiber, idle: u64) {
        let weight = match self.weight {
            ProfileWeight::Steps => due.intervals.saturating_mul(u64::from(due.every)),
            ProfileWeight::Time => self.take_time(idle),
        };
        self.scratch.clear();
        let frames = &fiber.frames;
        // A very deep stack (recursion) would cost more to read than the steps it stands for
        // and fill the output: past the limit only its first and last frames are read, with a
        // marker between them, which is the same for every sample so they still add up.
        let (head, tail_from) = if frames.len() > MAX_FRAMES {
            (HEAD_FRAMES, frames.len() - TAIL_FRAMES)
        } else {
            (frames.len(), frames.len())
        };
        let mut at: Option<Span> = None;
        let mut outer_hidden = false;
        let mut i = 0;
        while i < frames.len() {
            if i == head && head < tail_from {
                let id = self.intern(ELIDED.to_string());
                self.scratch.push(id);
                i = tail_from;
                outer_hidden = frames[i - 1].chunk.hidden;
            }
            let frame = &frames[i];
            let named = self.name_of(&frame.chunk, frame.kind);
            // The frames of a built-in's own helpers fold into the built-in's.
            if !(named.hidden && outer_hidden) {
                let mut id = named.id;
                if i == 0 && fiber.entry.is_some() {
                    id = self.go_name(id);
                }
                self.scratch.push(id);
            }
            outer_hidden = named.hidden;
            if !named.hidden {
                at = Some(frame.span());
            }
            i += 1;
        }
        if self.scratch.is_empty() {
            let id = self.intern(NO_FRAMES.to_string());
            self.scratch.push(id);
        }
        let span = at.unwrap_or(NO_POSITION);
        self.add(weight, span);
    }

    /// Adds `weight` to the stack in `scratch`.
    fn add(&mut self, weight: u64, span: Span) {
        if let Some(by_span) = self.samples.get_mut(self.scratch.as_slice()) {
            *by_span.entry(span).or_insert(0) += weight;
        } else {
            let stack: Box<[u32]> = self.scratch.as_slice().into();
            self.samples.insert(stack, HashMap::from([(span, weight)]));
        }
        match &mut self.last {
            Some((stack, at)) if **stack == *self.scratch.as_slice() => *at = span,
            last => *last = Some((self.scratch.as_slice().into(), span)),
        }
    }

    /// Adds `weight` to `stack` without making it the stack of the last sample.
    fn record(&mut self, stack: Box<[u32]>, span: Span, weight: u64) {
        *self
            .samples
            .entry(stack)
            .or_default()
            .entry(span)
            .or_insert(0) += weight;
    }

    /// The weight after the last safepoint goes to the stack of the last sample.
    fn flush(&mut self, steps: u64, idle: u64) {
        let weight = match self.weight {
            ProfileWeight::Steps => steps,
            ProfileWeight::Time => self.take_time(idle),
        };
        if weight == 0 {
            return;
        }
        let (stack, span) = match self.last.take() {
            Some(last) => last,
            None => {
                let id = self.intern(UNSAMPLED.to_string());
                (Box::from([id].as_slice()), NO_POSITION)
            }
        };
        self.scratch.clear();
        self.scratch.extend_from_slice(&stack);
        self.add(weight, span);
    }

    fn finish(
        mut self,
        steps: u64,
        idle: u64,
        locate: &dyn Fn(Span) -> Option<(String, u32)>,
    ) -> Profile {
        self.flush(steps, idle);
        let mut samples = Vec::new();
        for (stack, by_span) in &self.samples {
            let names: Vec<String> = stack
                .iter()
                .map(|&id| self.names[id as usize].clone())
                .collect();
            // Different spans on one line are one line.
            let mut lines: Vec<((String, u32), u64)> = Vec::new();
            for (span, weight) in by_span {
                let place = locate(*span).unwrap_or_default();
                match lines.iter_mut().find(|(p, _)| *p == place) {
                    Some((_, w)) => *w += weight,
                    None => lines.push((place, *weight)),
                }
            }
            for ((file, line), weight) in lines {
                samples.push(ProfileSample {
                    stack: names.clone(),
                    file,
                    line,
                    weight,
                });
            }
        }
        samples.sort_by(|a, b| (&a.stack, &a.file, a.line).cmp(&(&b.stack, &b.file, b.line)));
        Profile {
            weight: self.weight,
            samples,
        }
    }
}

impl Machine {
    /// Starts sampling every [`PROFILE_SLICE`] steps. A profile already running starts over.
    #[doc(hidden)]
    pub fn start_profile(&mut self, weight: ProfileWeight) {
        self.core.sched.measure_idle(true);
        let idle = self.core.sched.idle_nanos();
        self.profiler = Some(Box::new(Profiler::new(weight, idle)));
        let every = std::num::NonZeroU32::new(PROFILE_SLICE).expect("a slice");
        self.core.want_safepoints(SafepointKind::Profiler, every);
        // The steps before this call are not the profile's.
        self.core.take_pending_steps(SafepointKind::Profiler);
    }

    /// Stops sampling and returns what was found, with the steps (or time) since the last
    /// sample as a final one. `locate` turns a span into a file name and a 1-based line.
    #[doc(hidden)]
    pub fn finish_profile(
        &mut self,
        locate: &dyn Fn(Span) -> Option<(String, u32)>,
    ) -> Option<Profile> {
        let profiler = self.profiler.take()?;
        let steps = self.core.take_pending_steps(SafepointKind::Profiler);
        self.core.drop_safepoints(SafepointKind::Profiler);
        let idle = self.core.sched.idle_nanos();
        self.core.sched.measure_idle(false);
        Some(profiler.finish(steps, idle, locate))
    }
}
