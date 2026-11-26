//! Debug mode: what a debugger needs of the machine (L10; the Debug Adapter Protocol server of
//! L11 is built on this).
//!
//! `Machine::set_debug(true)` before `load` makes the compiler emit one `Step { n: 1 }` per
//! statement (the totals are the same), a `Mark` at the start of a block's tail, a statement
//! table and the scopes of each chunk, and nothing fused or run as a leaf. A debugger is a
//! [`Debugger`] attached with [`Machine::set_debugger`]; it is called when the program stops
//! and every [`DEBUG_SLICE`] steps while it runs.
//!
//! **Breakpoints** are patches. A breakpoint is a line of a file, with an optional hit
//! condition and an optional log message; it is put into every compiled chunk that has a
//! statement on the line, and into each chunk that is compiled later (`Slot::chunk` calls
//! [`DebugShared::adopt`]). Putting it in means exchanging the statement's `Step` (or `Mark`)
//! for `Op::Breakpoint { id }`, where `id` is the index of the chunk's [`Patch`], which keeps
//! the original. The code is `Cell<Op>`, so a frame that is running the chunk fetches the
//! patch at its next instruction: a breakpoint in a loop that is running hits. On a hit the
//! machine calls the debugger with the patch still in place and then runs the original
//! (`charge!` of the `Step`), so the breakpoint hits once per execution of the statement.
//!
//! **Stepping** asks for a safepoint at every step (slice 1) and looks at the top frame there:
//! see [`Machine::step_check`]. **Pause** is an `AtomicBool` that [`PauseHandle`] sets from
//! another thread and the 1,024-step safepoint reads. **Break on panic** is a hook at the top
//! of `Machine::catch`, before any landing pad runs.
//!
//! **Inspection** never runs program code. Values are shown structurally (a user `Show` is not
//! called), and the handles of [`Machine::debug_scopes`] index a table that is cleared whenever
//! the program goes on, so no reference a debugger took outlives a stop.

use std::cell::{Cell, RefCell};
use std::num::NonZeroU32;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use colibri_check::hir::{LocalId, TypeDef, TypeDefKind};
use colibri_check::types::OPTION_TYPE;
use colibri_core::ctl::Ctl;
use colibri_core::prelude::*;
use colibri_core::safepoint::SafepointKind;
use colibri_core::value::{Value, unwrap_cell_ref};
use colibri_syntax::{FileId, Span};

use crate::bytecode::{Chunk, Op, Patch};
use crate::fiber::{Frame, FrameKind, VmFiber};
use crate::machine::Machine;

/// How many steps pass between two safepoints of an attached debugger that is not stepping:
/// how late a pause request is seen, and how often `Debugger::poll` runs.
pub const DEBUG_SLICE: u32 = 1024;

/// The longest text `summary` makes of a value.
const SUMMARY_CHARS: usize = 120;
/// How many children `debug_children` returns at most in one call.
const MAX_CHILDREN: usize = 10_000;

/// Turns spans into what a debugger shows. The runtime implements it over its source map.
pub trait Locator {
    /// The 1-based line a span starts on (`None` for a span with no source, the prelude's).
    fn line(&self, span: Span) -> Option<u32>;
    /// The name of the span's file as diagnostics show it.
    fn file(&self, span: Span) -> Option<String>;
}

/// When the hits of a breakpoint count (the hit-count condition of a conditional breakpoint).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitCondition {
    /// Only the n-th hit.
    Equals(u64),
    /// The n-th hit and every one after it.
    AtLeast(u64),
    /// Every n-th hit.
    MultipleOf(u64),
}

impl HitCondition {
    /// Whether the `hits`-th hit (1-based) counts.
    pub fn holds(self, hits: u64) -> bool {
        match self {
            HitCondition::Equals(n) => hits == n,
            HitCondition::AtLeast(n) => hits >= n,
            HitCondition::MultipleOf(n) => n != 0 && hits.is_multiple_of(n),
        }
    }
}

/// One breakpoint a debugger asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakpointSpec {
    /// The 1-based line.
    pub line: u32,
    /// With a hit condition the breakpoint counts its hits and acts only when it holds.
    pub hit: Option<HitCondition>,
    /// A logpoint: the text, in which `{name}` is replaced by the local `name` of the top
    /// frame (`{{` and `}}` are braces), is given to `Debugger::output` and the program does
    /// not stop.
    pub log: Option<String>,
}

/// What became of a breakpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakpointInfo {
    /// The number the stop reports.
    pub id: u32,
    pub line: u32,
    /// Whether a compiled statement has it now. A breakpoint in a function that has not been
    /// compiled is not verified yet and is put in when the function is compiled.
    pub verified: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// To the next line of this frame, or the caller's when this one returns; calls run.
    Over,
    /// Into the next call, else as `Over`.
    In,
    /// Until this frame has returned, to the next statement of its caller.
    Out,
}

/// How the program goes on after a stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    Continue,
    Step(StepKind),
    /// Ends the program with a panic (the `defer`s run).
    Terminate,
}

/// Why the program stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The breakpoints (by id) that acted.
    Breakpoint(Vec<u32>),
    Step,
    Pause,
    /// An uncaught panic, before anything unwinds. The message.
    Panic(String),
}

/// The debugger: called by the machine on its thread, with the program stopped.
pub trait Debugger {
    /// The program stopped. The view reads the stopped fiber and the paused coroutines and
    /// changes breakpoints; what is returned says how to go on. May block (a server waits for
    /// requests here).
    fn stopped(&mut self, view: &mut DebugView<'_>, stop: &Stop) -> Resume;

    /// Every [`DEBUG_SLICE`] steps while the program runs freely; a server answers the
    /// requests that arrived meanwhile (a new set of breakpoints) here.
    fn poll(&mut self, _view: &mut DebugView<'_>) {}

    /// The text of a logpoint.
    fn output(&mut self, _text: &str) {}
}

/// Asks a running program to stop at its next safepoint; may be sent to another thread.
#[derive(Clone, Debug, Default)]
pub struct PauseHandle(Arc<AtomicBool>);

impl PauseHandle {
    pub fn pause(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// One call on a stack, as shown (innermost first in [`Machine::debug_frames`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugFrame {
    /// The frame's index in the fiber, which `debug_scopes` takes.
    pub index: usize,
    pub name: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    /// A host function: no source.
    pub native: bool,
}

/// A coroutine, or the main computation, that can be inspected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugFiber {
    /// `None` for the computation that is not a coroutine.
    pub coroutine: Option<u64>,
    /// The function the computation began in.
    pub name: String,
    /// The one that stopped.
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugScope {
    pub name: &'static str,
    /// For `Machine::debug_children`.
    pub handle: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugVar {
    pub name: String,
    /// A one-line structural form of the value (never a user `Show`).
    pub value: String,
    pub ty: String,
    /// For `Machine::debug_children`, when the value has parts.
    pub handle: Option<u32>,
}

/// What a debugger holds a number for.
enum Handle {
    Locals {
        coroutine: Option<u64>,
        frame: usize,
    },
    Module,
    Value(Value),
}

/// What the machine does at its safepoints.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Run,
    Step(StepState),
}

/// Where a step began.
#[derive(Clone, Copy, Debug)]
struct StepState {
    kind: StepKind,
    /// The coroutine (or `None`) the step is for.
    coroutine: Option<u64>,
    /// How many frames that can be stepped in were on the stack.
    depth: usize,
    /// The file and line of the top such frame.
    line: Option<(FileId, u32)>,
}

pub(crate) struct DebugState {
    pub(crate) shared: Rc<DebugShared>,
    sink: Option<Box<dyn Debugger>>,
    mode: Mode,
    break_on_panic: bool,
    pause: PauseHandle,
    handles: Vec<Handle>,
    /// A `Resume::Terminate` is unwinding the program: nothing stops it again.
    terminating: bool,
    /// The panic that was last reported, so that it is not reported again when it passes
    /// through a native entry on its way out.
    reported: Option<(usize, Span)>,
}

struct Bp {
    id: u32,
    file: FileId,
    line: u32,
    hit: Option<HitCondition>,
    log: Option<String>,
    hits: u64,
}

/// The part of the debugger the compiler reaches: the breakpoints and the chunks to put them
/// into. In the `CompileEnv`, so `Slot::chunk` can adopt each chunk it makes.
#[derive(Default)]
pub(crate) struct DebugShared {
    bps: RefCell<Vec<Bp>>,
    next_id: Cell<u32>,
    chunks: RefCell<Vec<Weak<Chunk>>>,
    locator: RefCell<Option<Rc<dyn Locator>>>,
}

impl std::fmt::Debug for DebugShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugShared")
            .field("breakpoints", &self.bps.borrow().len())
            .finish_non_exhaustive()
    }
}

impl DebugShared {
    /// A chunk was compiled: it is listed, and gets the breakpoints that match it.
    pub(crate) fn adopt(&self, chunk: &Rc<Chunk>) {
        {
            let mut chunks = self.chunks.borrow_mut();
            if chunks.len() >= 1024 && chunks.len().is_power_of_two() {
                chunks.retain(|w| w.strong_count() > 0);
            }
            chunks.push(Rc::downgrade(chunk));
        }
        for bp in self.bps.borrow().iter() {
            self.put(chunk, bp);
        }
    }

    fn locator(&self) -> Option<Rc<dyn Locator>> {
        self.locator.borrow().clone()
    }

    /// Puts `bp` into `chunk`: at the statements that start the line, the outermost ones (the
    /// earliest in the source) when several start on it. Returns whether any was found.
    fn put(&self, chunk: &Chunk, bp: &Bp) -> bool {
        let Some(loc) = self.locator() else {
            return false;
        };
        if chunk.hidden {
            return false;
        }
        let on_line: Vec<(u32, Span)> = chunk
            .stmts
            .iter()
            .copied()
            .filter(|(_, s)| s.file == bp.file && loc.line(*s) == Some(bp.line))
            .collect();
        let Some(first) = on_line.iter().map(|(_, s)| s.start).min() else {
            return false;
        };
        for (pc, span) in on_line {
            if span.start == first {
                chunk.add_breakpoint(pc, bp.id);
            }
        }
        true
    }

    fn live_chunks(&self) -> Vec<Rc<Chunk>> {
        self.chunks
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect()
    }
}

impl Chunk {
    /// Makes the statement at `pc` (a `Step` or a `Mark`) a breakpoint of `bp`.
    pub(crate) fn add_breakpoint(&self, pc: u32, bp: u32) {
        let mut patches = self.patches.borrow_mut();
        if let Some((id, patch)) = patches.iter_mut().enumerate().find(|(_, p)| p.pc == pc) {
            if patch.bps.is_empty() {
                self.code[pc as usize].set(Op::Breakpoint { id: id as u32 });
            }
            if !patch.bps.contains(&bp) {
                patch.bps.push(bp);
            }
            return;
        }
        let original = self.code[pc as usize].get();
        // The only instructions the dispatch loop's unchecked register proof allows to be
        // exchanged: `Op::unchecked_regs_end` is 0 for them and for `Breakpoint`.
        assert!(
            matches!(original, Op::Step { .. } | Op::Mark),
            "a breakpoint replaces a Step or a Mark, not {original:?}"
        );
        patches.push(Patch {
            pc,
            original,
            bps: vec![bp],
        });
        self.code[pc as usize].set(Op::Breakpoint {
            id: (patches.len() - 1) as u32,
        });
    }

    /// Takes the breakpoints for which `gone` holds out of the chunk, restoring the
    /// instruction of each statement that has none left.
    pub(crate) fn remove_breakpoints(&self, gone: impl Fn(u32) -> bool) {
        for patch in self.patches.borrow_mut().iter_mut() {
            let had = !patch.bps.is_empty();
            patch.bps.retain(|b| !gone(*b));
            if had && patch.bps.is_empty() {
                self.code[patch.pc as usize].set(patch.original);
            }
        }
    }

    /// What the patch `id` replaced, and the breakpoints that want it.
    pub(crate) fn patch(&self, id: u32) -> Option<(Op, Vec<u32>)> {
        let patches = self.patches.borrow();
        let p = patches.get(id as usize)?;
        Some((p.original, p.bps.clone()))
    }
}

/// What a debugger sees while the program is stopped, or while it polls.
pub struct DebugView<'a> {
    pub(crate) vm: &'a mut Machine,
    pub(crate) fiber: &'a mut VmFiber,
}

impl DebugView<'_> {
    /// The computations that can be inspected: the one that stopped first, then the paused
    /// coroutines.
    pub fn fibers(&self) -> Vec<DebugFiber> {
        self.vm.debug_fibers(self.fiber)
    }

    /// The frames of a computation, innermost first (`None`: the one that stopped).
    pub fn frames(&self, coroutine: Option<u64>) -> Vec<DebugFrame> {
        self.vm.debug_frames(self.fiber, coroutine)
    }

    pub fn scopes(&mut self, coroutine: Option<u64>, frame: usize) -> Vec<DebugScope> {
        self.vm.debug_scopes(self.fiber, coroutine, frame)
    }

    pub fn children(&mut self, handle: u32, start: usize, count: usize) -> Vec<DebugVar> {
        self.vm.debug_children(self.fiber, handle, start, count)
    }

    /// Replaces the breakpoints of `file` (see [`Machine::debug_set_breakpoints`]).
    pub fn set_breakpoints(
        &mut self,
        file: FileId,
        specs: &[BreakpointSpec],
    ) -> Vec<BreakpointInfo> {
        self.vm.debug_set_breakpoints(file, specs)
    }

    pub fn break_on_panic(&mut self, on: bool) {
        self.vm.debug_break_on_panic(on);
    }

    pub fn pause_handle(&self) -> PauseHandle {
        self.vm.debug_pause_handle()
    }
}

/// Whether a frame is somewhere a step may stop: Colibri code of the program, not a host
/// function's shim and not the prelude's own code (a closure built-in written in Colibri).
fn steppable(f: &Frame) -> bool {
    f.kind != FrameKind::HostShim && !f.chunk.hidden
}

impl Machine {
    // ---- switching it on ----

    /// Turns debug mode on or off for the program that is loaded next (see the module
    /// documentation). Call it before `load`: code that is compiled already stays as it is.
    pub fn set_debug(&mut self, on: bool) {
        match (on, self.debug.is_some()) {
            (true, false) => {
                self.debug = Some(Box::new(DebugState {
                    shared: Rc::new(DebugShared::default()),
                    sink: None,
                    mode: Mode::Run,
                    break_on_panic: false,
                    pause: PauseHandle::default(),
                    handles: Vec::new(),
                    terminating: false,
                    reported: None,
                }));
            }
            (false, true) => {
                self.core.drop_safepoints(SafepointKind::Debugger);
                self.debug = None;
            }
            _ => {}
        }
        self.env.debug = self.debug.as_ref().map(|d| Rc::clone(&d.shared));
    }

    /// Whether the program was compiled for a debugger.
    pub fn debug_mode(&self) -> bool {
        self.debug.is_some()
    }

    /// Attaches `debugger` (turning debug mode on if it is not), with the locator that turns
    /// spans into lines. Before `load`.
    pub fn set_debugger(&mut self, debugger: Box<dyn Debugger>, locator: Rc<dyn Locator>) {
        self.set_debug(true);
        if let Some(d) = self.debug.as_mut() {
            *d.shared.locator.borrow_mut() = Some(locator);
            d.sink = Some(debugger);
            d.mode = Mode::Run;
        }
        self.slice_for_mode();
    }

    /// Detaches the debugger; the breakpoints are taken out of the code.
    pub fn clear_debugger(&mut self) {
        if let Some(d) = self.debug.as_mut() {
            d.sink = None;
            d.mode = Mode::Run;
        }
        self.debug_set_all_breakpoints(&[]);
        self.core.drop_safepoints(SafepointKind::Debugger);
    }

    /// Gives the debugger's safepoints their interval: every step while stepping, else
    /// [`DEBUG_SLICE`].
    fn slice_for_mode(&mut self) {
        let Some(d) = self.debug.as_ref() else {
            return;
        };
        if d.sink.is_none() {
            return;
        }
        let every = match d.mode {
            Mode::Step(_) => 1,
            Mode::Run => DEBUG_SLICE,
        };
        let every = NonZeroU32::new(every).expect("a slice");
        self.core.want_safepoints(SafepointKind::Debugger, every);
    }

    /// Replaces the locator (a new program has other files).
    pub fn set_debug_locator(&mut self, locator: Rc<dyn Locator>) {
        if let Some(d) = self.debug.as_ref() {
            *d.shared.locator.borrow_mut() = Some(locator);
        }
    }

    /// Stop at a panic that nothing handles, before anything unwinds.
    pub fn debug_break_on_panic(&mut self, on: bool) {
        if let Some(d) = self.debug.as_mut() {
            d.break_on_panic = on;
        }
    }

    /// The handle that pauses the program from another thread.
    pub fn debug_pause_handle(&self) -> PauseHandle {
        self.debug
            .as_ref()
            .map(|d| d.pause.clone())
            .unwrap_or_default()
    }

    // ---- breakpoints ----

    /// Replaces the breakpoints of `file` by `specs`, for the code that exists and the code that
    /// is compiled later. Hit counts start again. One result per spec, in order.
    pub fn debug_set_breakpoints(
        &mut self,
        file: FileId,
        specs: &[BreakpointSpec],
    ) -> Vec<BreakpointInfo> {
        let Some(d) = self.debug.as_ref() else {
            return Vec::new();
        };
        let shared = Rc::clone(&d.shared);
        let old: Vec<u32> = shared
            .bps
            .borrow()
            .iter()
            .filter(|b| b.file == file)
            .map(|b| b.id)
            .collect();
        let chunks = shared.live_chunks();
        for chunk in &chunks {
            chunk.remove_breakpoints(|b| old.contains(&b));
        }
        shared.bps.borrow_mut().retain(|b| b.file != file);
        let mut out = Vec::new();
        for spec in specs {
            let id = shared.next_id.get();
            shared.next_id.set(id + 1);
            let bp = Bp {
                id,
                file,
                line: spec.line,
                hit: spec.hit,
                log: spec.log.clone(),
                hits: 0,
            };
            let mut verified = false;
            for chunk in &chunks {
                verified |= shared.put(chunk, &bp);
            }
            shared.bps.borrow_mut().push(bp);
            out.push(BreakpointInfo {
                id,
                line: spec.line,
                verified,
            });
        }
        out
    }

    /// Takes every breakpoint out (`clear_debugger`).
    fn debug_set_all_breakpoints(&mut self, _none: &[BreakpointSpec]) {
        let Some(d) = self.debug.as_ref() else {
            return;
        };
        let shared = Rc::clone(&d.shared);
        for chunk in shared.live_chunks() {
            chunk.remove_breakpoints(|_| true);
        }
        shared.bps.borrow_mut().clear();
    }

    /// An `Op::Breakpoint { id }` of `chunk` was reached: counts the hit, writes the log
    /// messages, and stops if a breakpoint says so. The original instruction runs after.
    pub(crate) fn breakpoint_hit(
        &mut self,
        fiber: &mut VmFiber,
        chunk: &Chunk,
        id: u32,
    ) -> Result<(), Ctl> {
        let Some(d) = self.debug.as_ref() else {
            return Ok(());
        };
        if d.sink.is_none() {
            return Ok(());
        }
        let Some((_, wanted)) = chunk.patch(id) else {
            return Ok(());
        };
        let mut acted = Vec::new();
        let mut logs = Vec::new();
        {
            let mut bps = d.shared.bps.borrow_mut();
            for b in bps.iter_mut().filter(|b| wanted.contains(&b.id)) {
                b.hits += 1;
                if b.hit.is_some_and(|h| !h.holds(b.hits)) {
                    continue;
                }
                match &b.log {
                    Some(text) => logs.push(text.clone()),
                    None => acted.push(b.id),
                }
            }
        }
        for text in logs {
            let line = self.interpolate(fiber, &text);
            if let Some(mut sink) = self.debug.as_mut().and_then(|d| d.sink.take()) {
                sink.output(&line);
                if let Some(d) = self.debug.as_mut() {
                    d.sink = Some(sink);
                }
            }
        }
        if acted.is_empty() {
            return Ok(());
        }
        self.debug_stop(fiber, Stop::Breakpoint(acted))
    }

    /// `{name}` of a logpoint text, from the locals of the top frame.
    fn interpolate(&self, fiber: &VmFiber, text: &str) -> String {
        let locals = fiber
            .frames
            .len()
            .checked_sub(1)
            .map(|top| self.frame_locals(fiber, top))
            .unwrap_or_default();
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    out.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    out.push('}');
                }
                '{' => {
                    let mut name = String::new();
                    let mut closed = false;
                    for n in chars.by_ref() {
                        if n == '}' {
                            closed = true;
                            break;
                        }
                        name.push(n);
                    }
                    match locals.iter().rev().find(|(n, _, _)| *n == name.trim()) {
                        Some((_, v, _)) if closed => out.push_str(&self.shown(v)),
                        _ => {
                            out.push('{');
                            out.push_str(&name);
                            if closed {
                                out.push('}');
                            }
                        }
                    }
                }
                other => out.push(other),
            }
        }
        out
    }

    // ---- stopping ----

    /// Calls the debugger with the program stopped, and carries out what it answers.
    pub(crate) fn debug_stop(&mut self, fiber: &mut VmFiber, stop: Stop) -> Result<(), Ctl> {
        let Some(mut sink) = self.debug.as_mut().and_then(|d| d.sink.take()) else {
            return Ok(());
        };
        if let Some(d) = self.debug.as_mut() {
            d.mode = Mode::Run;
            d.handles.clear();
        }
        let resume = {
            let mut view = DebugView {
                vm: &mut *self,
                fiber: &mut *fiber,
            };
            sink.stopped(&mut view, &stop)
        };
        let step = match resume {
            Resume::Step(kind) => Some(self.step_origin(fiber, kind)),
            _ => None,
        };
        if let Some(d) = self.debug.as_mut() {
            // Nothing a debugger looked at may outlive the stop: a held reference would make
            // the program copy a value it would have changed in place.
            d.handles.clear();
            d.sink = Some(sink);
            if let Some(step) = step {
                d.mode = Mode::Step(step);
            }
        }
        self.slice_for_mode();
        match resume {
            Resume::Terminate => {
                if let Some(d) = self.debug.as_mut() {
                    d.terminating = true;
                }
                let span = fiber
                    .frames
                    .last()
                    .map_or(Span::new(FileId(0), 0, 0), |f| f.span());
                Err(self.panic(fiber, span, "the program was ended by the debugger"))
            }
            _ => Ok(()),
        }
    }

    /// Where a step that begins now begins.
    fn step_origin(&self, fiber: &VmFiber, kind: StepKind) -> StepState {
        let top = fiber.frames.iter().rev().find(|f| steppable(f));
        StepState {
            kind,
            coroutine: fiber.state.coro,
            depth: fiber.frames.iter().filter(|f| steppable(f)).count(),
            line: top.and_then(|f| self.line_of(f.span())),
        }
    }

    fn line_of(&self, span: Span) -> Option<(FileId, u32)> {
        let loc = self.debug.as_ref()?.shared.locator()?;
        Some((span.file, loc.line(span)?))
    }

    /// The debugger's safepoint: a pause request, a step that has arrived, or the poll.
    pub(crate) fn debug_safepoint(&mut self, fiber: &mut VmFiber) -> Result<(), Ctl> {
        let Some(d) = self.debug.as_mut() else {
            return Ok(());
        };
        if d.sink.is_none() {
            return Ok(());
        }
        if d.pause.0.swap(false, Ordering::AcqRel) {
            return self.debug_stop(fiber, Stop::Pause);
        }
        match d.mode {
            Mode::Step(_) => self.step_check(fiber),
            Mode::Run => {
                if let Some(mut sink) = d.sink.take() {
                    {
                        let mut view = DebugView {
                            vm: &mut *self,
                            fiber: &mut *fiber,
                        };
                        sink.poll(&mut view);
                    }
                    if let Some(d) = self.debug.as_mut() {
                        d.sink = Some(sink);
                    }
                }
                Ok(())
            }
        }
    }

    /// Stops if the step in progress has arrived. Looks at the top frame at a safepoint (every
    /// step while stepping) and at a `Mark`. A stop is at the start of a statement, except that
    /// a step into a call stops as soon as the callee's frame is there.
    pub(crate) fn step_check(&mut self, fiber: &mut VmFiber) -> Result<(), Ctl> {
        let Some(Mode::Step(from)) = self.debug.as_ref().map(|d| d.mode) else {
            return Ok(());
        };
        if from.coroutine != fiber.state.coro {
            return Ok(());
        }
        let Some(top) = fiber.frames.last() else {
            return Ok(());
        };
        if !steppable(top) {
            return Ok(());
        }
        let depth = fiber.frames.iter().filter(|f| steppable(f)).count();
        let boundary = top
            .chunk
            .stmts
            .binary_search_by_key(&top.pc.saturating_sub(1), |s| s.0)
            .is_ok();
        let line = self.line_of(top.span());
        let moved = depth < from.depth || (depth == from.depth && line != from.line);
        let stop = match from.kind {
            StepKind::In => depth > from.depth || (boundary && moved),
            StepKind::Over => boundary && moved,
            StepKind::Out => boundary && depth < from.depth,
        };
        if stop {
            self.debug_stop(fiber, Stop::Step)
        } else {
            Ok(())
        }
    }

    /// Whether `Op::Mark` has anything to do: a step is in progress.
    pub(crate) fn stepping(&self) -> bool {
        self.debug
            .as_ref()
            .is_some_and(|d| matches!(d.mode, Mode::Step(_)))
    }

    /// The hook of `Machine::catch`: a panic is about to unwind, and nothing has run yet.
    pub(crate) fn debug_panic(&mut self, fiber: &mut VmFiber, ctl: &Ctl) {
        let Some(d) = self.debug.as_ref() else {
            return;
        };
        if !d.break_on_panic || d.sink.is_none() || d.terminating {
            return;
        }
        let Ctl::Panic(info) = ctl else {
            return;
        };
        // Running out of budget is the host's doing, a refusal is the VM's, and a panic that
        // is already unwinding (in a `defer`) is a note of the one that was reported.
        if info.budget
            || info.message.starts_with(crate::unwind::REFUSAL)
            || !fiber.unwinding.is_empty()
        {
            return;
        }
        let key = (
            std::ptr::from_ref::<colibri_core::ctl::PanicInfo>(info) as usize,
            info.span,
        );
        if d.reported == Some(key) {
            return;
        }
        let message = info.message.clone();
        if let Some(d) = self.debug.as_mut() {
            d.reported = Some(key);
        }
        // The debugger cannot fail the program here, only stop it; `Terminate` lets the panic go.
        let _ = self.debug_stop(fiber, Stop::Panic(message));
    }

    // ---- inspection ----

    /// The computations: `current` (the one that stopped) first, then the coroutines that
    /// are paused.
    pub fn debug_fibers(&self, current: &VmFiber) -> Vec<DebugFiber> {
        let name_of = |f: &VmFiber| {
            f.frames
                .iter()
                .find(|fr| fr.kind != FrameKind::HostShim)
                .map_or_else(|| "main".to_string(), |fr| fr.chunk.body.name.clone())
        };
        let mut out = vec![DebugFiber {
            coroutine: current.state.coro,
            name: name_of(current),
            current: true,
        }];
        for f in self.fibers.stored() {
            out.push(DebugFiber {
                coroutine: f.state.coro,
                name: name_of(f),
                current: false,
            });
        }
        out
    }

    /// The fiber of `coroutine` (`None`, or the id of `current` itself: `current`).
    fn fiber_of<'a>(&'a self, current: &'a VmFiber, coroutine: Option<u64>) -> Option<&'a VmFiber> {
        match coroutine {
            None => Some(current),
            Some(id) if current.state.coro == Some(id) => Some(current),
            Some(id) => self.fibers.stored().find(|f| f.state.coro == Some(id)),
        }
    }

    /// The frames of a computation, innermost first, without the prelude's own frames (a
    /// closure built-in's loop). Empty for a coroutine that is not paused here.
    pub fn debug_frames(&self, current: &VmFiber, coroutine: Option<u64>) -> Vec<DebugFrame> {
        let Some(fiber) = self.fiber_of(current, coroutine) else {
            return Vec::new();
        };
        let loc = self.debug.as_ref().and_then(|d| d.shared.locator());
        let mut out = Vec::new();
        for (index, f) in fiber.frames.iter().enumerate().rev() {
            if f.chunk.hidden {
                continue;
            }
            let native = f.kind == FrameKind::HostShim;
            let (file, line) = match (&loc, native) {
                (Some(loc), false) => {
                    let span = f.span();
                    (loc.file(span), loc.line(span))
                }
                _ => (None, None),
            };
            let name = if native {
                format!("[native: {}]", f.chunk.body.name)
            } else {
                f.chunk.body.name.clone()
            };
            out.push(DebugFrame {
                index,
                name,
                file,
                line,
                native,
            });
        }
        out
    }

    /// The scopes of frame `frame` (an index of [`DebugFrame::index`]): its locals and the
    /// module's values. Their numbers are good until the program goes on.
    pub fn debug_scopes(
        &mut self,
        current: &VmFiber,
        coroutine: Option<u64>,
        frame: usize,
    ) -> Vec<DebugScope> {
        if self
            .fiber_of(current, coroutine)
            .is_none_or(|f| frame >= f.frames.len())
        {
            return Vec::new();
        }
        let Some(d) = self.debug.as_mut() else {
            return Vec::new();
        };
        d.handles.push(Handle::Locals { coroutine, frame });
        let locals = (d.handles.len() - 1) as u32;
        d.handles.push(Handle::Module);
        vec![
            DebugScope {
                name: "Locals",
                handle: locals,
            },
            DebugScope {
                name: "Module",
                handle: locals + 1,
            },
        ]
    }

    /// The parts of a scope or a value: `count` of them from `start`.
    pub fn debug_children(
        &mut self,
        current: &VmFiber,
        handle: u32,
        start: usize,
        count: usize,
    ) -> Vec<DebugVar> {
        let count = count.min(MAX_CHILDREN);
        let Some(d) = self.debug.as_ref() else {
            return Vec::new();
        };
        let named: Vec<(String, Value, Option<String>)> = match d.handles.get(handle as usize) {
            None => return Vec::new(),
            Some(Handle::Locals { coroutine, frame }) => match self.fiber_of(current, *coroutine) {
                Some(fiber) => self
                    .frame_locals(fiber, *frame)
                    .into_iter()
                    .map(|(n, v, t)| (n, v, Some(t)))
                    .collect(),
                None => Vec::new(),
            },
            Some(Handle::Module) => self.module_values(),
            Some(Handle::Value(v)) => {
                let v = v.clone();
                self.parts(&v, start, count)
            }
        };
        let skip = match self
            .debug
            .as_ref()
            .and_then(|d| d.handles.get(handle as usize))
        {
            Some(Handle::Value(_)) => 0,
            _ => start,
        };
        named
            .into_iter()
            .skip(skip)
            .take(count)
            .map(|(name, v, ty)| {
                let value = self.shown(&v);
                let ty = ty.unwrap_or_else(|| self.type_name(&v));
                let handle = self.has_parts(&v).then(|| self.new_handle(v));
                DebugVar {
                    name,
                    value,
                    ty,
                    handle,
                }
            })
            .collect()
    }

    fn new_handle(&mut self, v: Value) -> u32 {
        let d = self.debug.as_mut().expect("debug mode");
        d.handles.push(Handle::Value(v));
        (d.handles.len() - 1) as u32
    }

    /// The locals of frame `frame` that hold values at its position: the parameters and
    /// captures, and the locals of the scopes the position is inside, declared by then. A
    /// local kept in a cell is read through it.
    fn frame_locals(&self, fiber: &VmFiber, frame: usize) -> Vec<(String, Value, String)> {
        let Some(f) = fiber.frames.get(frame) else {
            return Vec::new();
        };
        if f.kind == FrameKind::HostShim {
            return Vec::new();
        }
        let chunk = &f.chunk;
        let body = &chunk.body;
        let at = f.pc.saturating_sub(1);
        let mut ids: Vec<LocalId> = (0..body.param_count).map(LocalId).collect();
        ids.extend(body.captures.iter().copied());
        let mut scopes: Vec<&crate::bytecode::ScopeInfo> = chunk
            .scopes
            .iter()
            .filter(|s| s.start <= at && at < s.end)
            .collect();
        scopes.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
        for s in scopes {
            for &(l, from) in &s.locals {
                if from <= at && !ids.contains(&l) {
                    ids.push(l);
                }
            }
        }
        let types = &self.core.types;
        ids.into_iter()
            .filter_map(|id| {
                let info = body.locals.get(id.0 as usize)?;
                if info.name.is_empty() || info.name.starts_with('$') {
                    return None;
                }
                let reg = f.base as usize + id.0 as usize;
                let v = unwrap_cell_ref(fiber.regs.get(reg)?);
                Some((info.name.clone(), v, info.ty.render(types)))
            })
            .collect()
    }

    /// The module-level values that have one (a script value whose line has not run has none).
    fn module_values(&self) -> Vec<(String, Value, Option<String>)> {
        self.globals
            .iter()
            .enumerate()
            .filter(|(i, _)| self.uninit.get(*i).is_none_or(Option::is_none))
            .filter_map(|(i, v)| {
                let name = self.global_names.get(i)?;
                (!name.is_empty() && !name.starts_with('$') && !matches!(v, Value::Fn(..)))
                    .then(|| (name.to_string(), unwrap_cell_ref(v), None))
            })
            .collect()
    }

    // ---- showing values (structurally: no user code runs) ----

    fn type_def(&self, id: colibri_check::types::TypeId) -> Option<&TypeDef> {
        self.core.types.get(id.0 as usize)
    }

    /// The name of a value's type, from the value alone.
    pub(crate) fn type_name(&self, v: &Value) -> String {
        let named = |id| {
            self.type_def(id)
                .map_or_else(|| "?".to_string(), |t| t.display_name().into_owned())
        };
        match v {
            Value::Unit => "()".into(),
            Value::Bool(_) => "bool".into(),
            Value::Int(_) => "int".into(),
            Value::U64(_) => "u64".into(),
            Value::Ptr(_) => "ptr".into(),
            Value::Float(_) => "float".into(),
            Value::F32(_) => "f32".into(),
            Value::Char(_) => "char".into(),
            Value::Str(_) => "str".into(),
            Value::Bytes(_) | Value::ByteBuf(_) => "bytes".into(),
            Value::Struct(s) => named(s.ty),
            Value::Enum(e) => named(e.ty),
            Value::Obj(o) => named(o.ty()),
            Value::SomeObj(_) => named(OPTION_TYPE),
            Value::Host(h) => named(h.ty),
            Value::Tuple(_) => "tuple".into(),
            Value::List(_) => "list".into(),
            Value::Map(_) => "map".into(),
            Value::Set(_) => "set".into(),
            Value::Weak(_) => "weak".into(),
            Value::Dyn(d) => self.type_name(&d.val),
            Value::Fn(..) => "fn".into(),
            Value::Closure(_) => "closure".into(),
            Value::Cell(c) => self.type_name(&c.get()),
            Value::Range(_) => "range".into(),
            Value::Iter(_) => "iterator".into(),
        }
    }

    /// A one-line structural form of a value, cut at [`SUMMARY_CHARS`].
    pub(crate) fn shown(&self, v: &Value) -> String {
        let mut out = String::new();
        self.summary(v, 0, &mut out);
        truncate(out)
    }

    fn summary(&self, v: &Value, depth: u32, out: &mut String) {
        use std::fmt::Write;
        let full = out.chars().count() >= SUMMARY_CHARS;
        if full {
            return;
        }
        match v {
            Value::Unit => out.push_str("()"),
            Value::Bool(b) => {
                let _ = write!(out, "{b}");
            }
            Value::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Value::U64(i) => {
                let _ = write!(out, "{i}");
            }
            Value::Ptr(p) => {
                let _ = write!(out, "0x{p:x}");
            }
            Value::Float(x) => {
                let _ = write!(out, "{x:?}");
            }
            Value::F32(x) => {
                let _ = write!(out, "{x:?}");
            }
            Value::Char(c) => {
                let _ = write!(out, "{c:?}");
            }
            Value::Str(s) => {
                let _ = write!(
                    out,
                    "{:?}",
                    s.chars().take(SUMMARY_CHARS).collect::<String>()
                );
            }
            Value::Bytes(b) => {
                let _ = write!(out, "<{} bytes>", b.len());
            }
            Value::ByteBuf(_) => out.push_str("<buffer>"),
            Value::Struct(s) => {
                let fields = self.field_names(s.ty, None);
                self.record(&self.name_of(s.ty), &fields, s.fields.iter(), depth, out);
            }
            Value::Obj(o) => {
                let fields = self.field_names(o.ty(), None);
                if depth >= 2 {
                    let _ = write!(out, "{} {{...}}", self.name_of(o.ty()));
                } else {
                    self.record(
                        &self.name_of(o.ty()),
                        &fields,
                        o.snapshot().iter(),
                        depth,
                        out,
                    );
                }
            }
            Value::Enum(e) => {
                let def = self.type_def(e.ty);
                let variant = def.and_then(|t| t.variants().get(e.variant as usize));
                let name = variant.map_or("?", |v| v.name.as_str());
                out.push_str(name);
                if !e.payload.is_empty() {
                    let names: Vec<String> = match variant {
                        Some(v) if v.named => v.fields.iter().map(|f| f.name.clone()).collect(),
                        _ => Vec::new(),
                    };
                    out.push('(');
                    for (i, p) in e.payload.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        if let Some(n) = names.get(i) {
                            let _ = write!(out, "{n}: ");
                        }
                        self.summary(p, depth + 1, out);
                    }
                    out.push(')');
                }
            }
            Value::SomeObj(o) => {
                out.push_str("some(");
                self.summary(&Value::Obj(Rc::clone(o)), depth + 1, out);
                out.push(')');
            }
            Value::Tuple(items) => {
                out.push('(');
                for (i, p) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.summary(p, depth + 1, out);
                }
                out.push(')');
            }
            Value::List(l) => {
                out.push('[');
                let n = l.len();
                for (i, p) in l.snapshot().iter().take(6).enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.summary(p, depth + 1, out);
                }
                if n > 6 {
                    let _ = write!(out, ", ... {n} items");
                }
                out.push(']');
            }
            Value::Set(s) => {
                out.push_str("Set([");
                let n = s.len();
                for (i, p) in s.snapshot().iter().take(6).enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.summary(p, depth + 1, out);
                }
                if n > 6 {
                    let _ = write!(out, ", ... {n} items");
                }
                out.push_str("])");
            }
            Value::Map(m) => {
                let n = m.len();
                if n == 0 {
                    out.push_str("{:}");
                } else {
                    out.push('{');
                    for (i, (k, p)) in m.snapshot().iter().take(4).enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        self.summary(k, depth + 1, out);
                        out.push_str(": ");
                        self.summary(p, depth + 1, out);
                    }
                    if n > 4 {
                        let _ = write!(out, ", ... {n} entries");
                    }
                    out.push('}');
                }
            }
            Value::Weak(w) => match w.upgrade() {
                Some(o) => {
                    out.push_str("weak ");
                    self.summary(&Value::Obj(o), depth + 1, out);
                }
                None => out.push_str("weak (gone)"),
            },
            Value::Host(h) => {
                let _ = write!(out, "<{} #{}>", self.name_of(h.ty), h.index);
            }
            Value::Dyn(d) => self.summary(&d.val, depth, out),
            Value::Fn(_, name) => {
                let _ = write!(out, "<fn {name}>");
            }
            Value::Closure(_) => out.push_str("<closure>"),
            Value::Cell(c) => self.summary(&c.get(), depth, out),
            Value::Range(r) => {
                let _ = write!(
                    out,
                    "{}..{}{}",
                    r.start,
                    if r.inclusive { "=" } else { "" },
                    r.end
                );
            }
            Value::Iter(_) => out.push_str("<iterator>"),
        }
    }

    fn record<'v>(
        &self,
        name: &str,
        fields: &[String],
        values: impl Iterator<Item = &'v Value>,
        depth: u32,
        out: &mut String,
    ) {
        use std::fmt::Write;
        let _ = write!(out, "{name} {{");
        for (i, p) in values.enumerate() {
            out.push_str(if i == 0 { " " } else { ", " });
            if let Some(n) = fields.get(i) {
                let _ = write!(out, "{n}: ");
            }
            self.summary(p, depth + 1, out);
        }
        out.push_str(" }");
    }

    fn name_of(&self, id: colibri_check::types::TypeId) -> String {
        self.type_def(id)
            .map_or_else(|| "?".to_string(), |t| t.display_name().into_owned())
    }

    /// The names of the fields of a struct or class, or of the payload of `variant` of an enum.
    fn field_names(&self, id: colibri_check::types::TypeId, variant: Option<u32>) -> Vec<String> {
        match self.type_def(id).map(|t| &t.kind) {
            Some(TypeDefKind::Struct { fields } | TypeDefKind::Class { fields }) => {
                fields.iter().map(|f| f.name.clone()).collect()
            }
            Some(TypeDefKind::Enum { variants }) => variant
                .and_then(|v| variants.get(v as usize))
                .map(|v| v.fields.iter().map(|f| f.name.clone()).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn has_parts(&self, v: &Value) -> bool {
        match v {
            Value::Struct(s) => !s.fields.is_empty(),
            Value::Obj(o) => !o.snapshot().is_empty(),
            Value::Enum(e) => !e.payload.is_empty(),
            Value::SomeObj(_) => true,
            Value::Tuple(t) => !t.is_empty(),
            Value::List(l) => !l.is_empty(),
            Value::Map(m) => !m.is_empty(),
            Value::Set(s) => !s.is_empty(),
            Value::Weak(w) => w.upgrade().is_some(),
            Value::Range(_) => true,
            Value::Dyn(d) => self.has_parts(&d.val),
            Value::Cell(c) => self.has_parts(&c.get()),
            _ => false,
        }
    }

    /// The parts of a value: `count` from `start` (a list or a map is paged; the rest is small).
    fn parts(&self, v: &Value, start: usize, count: usize) -> Vec<(String, Value, Option<String>)> {
        let ty_of = |id: colibri_check::types::TypeId, variant: Option<u32>, i: usize| {
            let fields = match self.type_def(id).map(|t| &t.kind) {
                Some(TypeDefKind::Struct { fields } | TypeDefKind::Class { fields }) => {
                    Some(fields.as_slice())
                }
                Some(TypeDefKind::Enum { variants }) => variant
                    .and_then(|v| variants.get(v as usize))
                    .map(|v| v.fields.as_slice()),
                _ => None,
            };
            fields
                .and_then(|f| f.get(i))
                .filter(|f| !f.ty.has_param() && !f.weak)
                .map(|f| f.ty.render(&self.core.types))
        };
        match v {
            Value::Struct(s) => {
                let names = self.field_names(s.ty, None);
                s.fields
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        (
                            names.get(i).cloned().unwrap_or(i.to_string()),
                            p.clone(),
                            ty_of(s.ty, None, i),
                        )
                    })
                    .collect()
            }
            Value::Obj(o) => {
                let names = self.field_names(o.ty(), None);
                o.snapshot()
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| {
                        (
                            names.get(i).cloned().unwrap_or(i.to_string()),
                            p,
                            ty_of(o.ty(), None, i),
                        )
                    })
                    .collect()
            }
            Value::Enum(e) => {
                let names = self.field_names(e.ty, Some(e.variant));
                e.payload
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        (
                            names.get(i).cloned().unwrap_or(i.to_string()),
                            p.clone(),
                            ty_of(e.ty, Some(e.variant), i),
                        )
                    })
                    .collect()
            }
            Value::SomeObj(o) => vec![("0".into(), Value::Obj(Rc::clone(o)), None)],
            Value::Tuple(t) => t
                .iter()
                .enumerate()
                .map(|(i, p)| (i.to_string(), p.clone(), None))
                .collect(),
            Value::List(l) => (start..start.saturating_add(count).min(l.len()))
                .filter_map(|i| Some((format!("[{i}]"), l.at(i)?, None)))
                .collect(),
            Value::Set(s) => s
                .snapshot()
                .into_iter()
                .enumerate()
                .skip(start)
                .take(count)
                .map(|(i, p)| (format!("[{i}]"), p, None))
                .collect(),
            Value::Map(m) => m
                .snapshot()
                .into_iter()
                .skip(start)
                .take(count)
                .map(|(k, p)| (self.shown(&k), p, None))
                .collect(),
            Value::Weak(w) => w
                .upgrade()
                .map(|o| vec![("target".to_string(), Value::Obj(o), None)])
                .unwrap_or_default(),
            Value::Range(r) => vec![
                ("start".into(), Value::Int(r.start), None),
                ("end".into(), Value::Int(r.end), None),
            ],
            Value::Dyn(d) => self.parts(&d.val, start, count),
            Value::Cell(c) => self.parts(&c.get(), start, count),
            _ => Vec::new(),
        }
    }
}

/// Cuts a text at [`SUMMARY_CHARS`] characters.
fn truncate(mut s: String) -> String {
    if s.chars().count() > SUMMARY_CHARS {
        let cut = s
            .char_indices()
            .nth(SUMMARY_CHARS)
            .map_or(s.len(), |(i, _)| i);
        s.truncate(cut);
        s.push_str("...");
    }
    s
}
