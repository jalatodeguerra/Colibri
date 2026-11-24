//! One running computation: a register file and a stack of frames, both on the heap
//! (vm-plan §2.6). Colibri-to-Colibri calls never recurse on the native stack; only a call from
//! Rust into Colibri (a user `Show` from `print`, a `Drop` hook, the host) starts a nested loop.

use std::rc::Rc;

use colibri_core::fiber::FiberState;
use colibri_core::heap::GuardToken;
use colibri_core::value::Value;
use colibri_syntax::Span;

use crate::bytecode::Chunk;

/// How a frame ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameKind {
    /// Returns into the frame below, whose register `ret` receives the result.
    Colibri,
    /// Entered from Rust: returning ends the dispatch loop that was started for it.
    NativeEntry,
    /// A host function that Colibri code called: no code and no registers, only the name in
    /// traces and the body a borrowed host object is held under. Pushed and popped by the call
    /// itself, so no `Return` ever meets it.
    HostShim,
}

/// One call. A frame's registers are `regs[base .. base + chunk.nregs]`; its parameters are the
/// first `chunk.nparams` of them, which overlap the caller's argument temporaries.
#[derive(Debug)]
pub(crate) struct Frame {
    pub chunk: Rc<Chunk>,
    /// The next instruction. Kept in the loop's local while the frame runs and written back
    /// before any call out of the loop, so that stack traces see it.
    pub pc: u32,
    pub base: u32,
    /// The absolute register of the caller that receives the result.
    pub ret: u32,
    pub kind: FrameKind,
    /// A position recorded by `Engine::note_position`, valid while `pc` is unchanged.
    pub noted: Option<(u32, Span)>,
    /// How many `Engine::hold_temp` values were alive when the frame began.
    pub temps_mark: u32,
}

impl Frame {
    /// Where the frame is: the noted position if it is current, else the instruction it is
    /// executing (a caller is at the call it made).
    pub fn span(&self) -> Span {
        match self.noted {
            Some((pc, span)) if pc == self.pc => span,
            _ => self.chunk.spans[(self.pc as usize).saturating_sub(1)],
        }
    }
}

/// The stack of one computation.
#[derive(Debug)]
pub struct VmFiber {
    /// The windows of all frames, contiguous. It only grows: a register that no frame owns
    /// holds `()` or a scalar (checked when a frame pops, in debug builds).
    pub(crate) regs: Vec<Value>,
    pub(crate) frames: Vec<Frame>,
    /// How many of `frames` run a hidden chunk (the prelude's). The depth limit counts the others only, as the tree-walker has no frames for built-ins.
    pub(crate) hidden: usize,
    /// Values kept alive for `Engine::hold_temp`: what core code holds across user code.
    pub(crate) temps: Vec<Value>,
    /// The mutation guards held by running code, newest last: a `for` loop's iteration hold, a
    /// `for mut` loop's or a `mut self` call's place hold. Released by `Unhold`, in pads too.
    pub(crate) holds: Vec<GuardToken>,
    pub(crate) state: FiberState,
    /// The native stack position when the host called in, to bound native re-entry.
    pub(crate) stack_base: usize,
    /// The panic or cancellation that a frame's landing pad is running for (`Resume` hands it
    /// on), by the frame's index, lowest first. Off the frame so a call and a return do not
    /// touch it.
    pub(crate) unwinding: Vec<(u32, Box<colibri_core::ctl::Ctl>)>,
    /// How many entries from Rust (`NativeEntry` frames, a coroutine's own among them) are
    /// running a dispatch loop on this fiber. A coroutine may pause only where its own entry
    /// is the only one: a nested loop sits on the native stack, which cannot be suspended.
    pub(crate) entries: u32,
    /// The coroutine paused in an instruction that runs again when it is resumed (`join`), not
    /// in one that is over (`wait`).
    pub(crate) redo: bool,
    /// A coroutine that has not run yet: what its first resumption calls.
    pub(crate) start: Option<Box<CoStart>>,
    /// What the end of a coroutine's entry frame releases.
    pub(crate) entry: Option<CoEntry>,
}

/// The call a new coroutine begins with (`go`), kept until its first resumption.
#[derive(Debug)]
pub(crate) struct CoStart {
    pub callee: colibri_check::hir::Callee,
    pub type_args: Vec<colibri_check::types::Ty>,
    /// The values the callee owns.
    pub values: Vec<Value>,
    pub span: Span,
}

/// The entry frame of a running coroutine: its parameters are released after the frame's own
/// cleanups, and so is the function value that was called.
#[derive(Debug)]
pub(crate) struct CoEntry {
    pub nparams: usize,
    pub callee: Option<Value>,
}

impl VmFiber {
    /// Takes the failure the pad of frame `idx` is running for.
    pub(crate) fn take_unwinding(&mut self, idx: usize) -> Option<Box<colibri_core::ctl::Ctl>> {
        match self.unwinding.last() {
            Some(&(i, _)) if i as usize == idx => self.unwinding.pop().map(|(_, c)| c),
            _ => None,
        }
    }

    /// Records the failure the pad of frame `idx` is running for.
    pub(crate) fn set_unwinding(&mut self, idx: usize, ctl: Box<colibri_core::ctl::Ctl>) {
        debug_assert!(
            self.unwinding
                .last()
                .is_none_or(|&(i, _)| (i as usize) < idx)
        );
        self.unwinding.push((idx as u32, ctl));
    }

    pub fn new() -> Self {
        VmFiber {
            regs: Vec::new(),
            frames: Vec::new(),
            hidden: 0,
            temps: Vec::new(),
            holds: Vec::new(),
            state: FiberState::default(),
            stack_base: 0,
            unwinding: Vec::new(),
            entries: 0,
            redo: false,
            start: None,
            entry: None,
        }
    }

    /// The frames that count against the call-depth limit: all but the hidden ones.
    #[inline(always)]
    pub(crate) fn visible_depth(&self) -> usize {
        self.frames.len() - self.hidden
    }

    /// Empties a finished coroutine's fiber for the next one, keeping the capacity of its
    /// stacks. Everything in it was released when the coroutine ended.
    pub(crate) fn reset(&mut self) {
        debug_assert!(self.frames.is_empty() && self.temps.is_empty() && self.holds.is_empty());
        debug_assert!(self.unwinding.is_empty() && self.start.is_none() && self.entry.is_none());
        self.regs.clear();
        self.frames.clear();
        self.temps.clear();
        self.holds.clear();
        self.unwinding.clear();
        self.state = FiberState::default();
        self.stack_base = 0;
        self.hidden = 0;
        self.entries = 0;
        self.redo = false;
        self.start = None;
        self.entry = None;
    }

    /// How many calls are active.
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// How many host functions are running, one inside another, on this fiber.
    pub fn host_depth(&self) -> u32 {
        self.state.host_depth
    }

    /// Whether this fiber is the computation of a coroutine.
    pub fn is_coroutine(&self) -> bool {
        self.state.coro.is_some()
    }

    /// How many held temporaries are alive; zero whenever no code is running.
    pub fn pending_temps(&self) -> usize {
        self.temps.len()
    }

    /// The first register above every frame's window.
    pub(crate) fn top(&self) -> usize {
        self.frames
            .last()
            .map_or(0, |f| f.base as usize + f.chunk.nregs as usize)
    }
}

impl Default for VmFiber {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a value owns nothing that needs releasing.
#[inline]
pub(crate) fn is_scalar_value(v: &Value) -> bool {
    matches!(
        v,
        Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::U64(_)
            | Value::Ptr(_)
            | Value::Float(_)
            | Value::F32(_)
            | Value::Char(_)
    )
}
