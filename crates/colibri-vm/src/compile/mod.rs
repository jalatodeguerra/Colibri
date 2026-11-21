//! The compiler from HIR to register bytecode (vm-plan §2).
//!
//! One [`FnCompiler`] per body. Locals are registers `0..locals.len()` (parameters first, as in
//! the HIR); temporaries are allocated above them, stack-like, and dropped as the plan's Unit
//! invariant requires: a register that does not own a value holds `()`.
//!
//! Which temporaries are dropped when:
//!
//! * scalars need no drop and their registers are reused as soon as the operation that read
//!   them is emitted;
//! * strings (and `Bytes`) are *inert*: nothing can observe when they die, so they are dropped
//!   eagerly too, right after the operation that used them;
//! * every other value is kept alive until the end of the statement and dropped newest first
//!   (ARCHITECTURE §2 rule 2), which is what the tree-walker's temporaries stack does.

mod call;
mod cond;
mod expr;
mod place;
mod stmt;

use std::rc::Rc;

use colibri_check::hir::{Body, FnId, Storage};
use colibri_check::types::Ty;
use colibri_core::value::Value;
use colibri_syntax::Span;

use crate::bytecode::{Chunk, Op, Reg, Site};

/// What the compiler needs to know about the loaded program.
#[derive(Debug, Default, Clone)]
pub struct CompileEnv {
    /// By global id: the global is a script value, which may be used before its line has run,
    /// so reads and writes check that it is initialised.
    pub checked_globals: Vec<bool>,
}

/// Why a body did not compile.
#[derive(Debug)]
pub(crate) enum Fail {
    /// A fused compare-and-branch jumps too far for its 16-bit offset: compile again unfused.
    TooFar,
    Message(String),
}

/// Compiles one body. `fn_id` is set for a function body.
pub fn compile_body(
    body: &Rc<Body>,
    fn_id: Option<FnId>,
    env: &CompileEnv,
) -> Result<Chunk, String> {
    match FnCompiler::compile(body, fn_id, env, true) {
        Err(Fail::TooFar) => match FnCompiler::compile(body, fn_id, env, false) {
            Ok(chunk) => Ok(chunk),
            Err(Fail::TooFar) => Err("a jump is too far for the bytecode".to_string()),
            Err(Fail::Message(m)) => Err(m),
        },
        Err(Fail::Message(m)) => Err(m),
        Ok(chunk) => Ok(chunk),
    }
}

/// A jump target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Label(usize);

#[derive(Default)]
struct LabelState {
    pos: Option<usize>,
    patches: Vec<usize>,
}

/// How a temporary's value dies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// No drop is ever needed.
    Scalar,
    /// A string or bytes: dropped eagerly (right after the operation that used it).
    Inert,
    /// Anything else: dropped at the end of the statement.
    Owned,
}

pub(crate) fn kind_of(ty: &Ty) -> Kind {
    match ty {
        Ty::Int(_)
        | Ty::Float
        | Ty::F32
        | Ty::Bool
        | Ty::Char
        | Ty::Unit
        | Ty::Never
        | Ty::Ptr(_) => Kind::Scalar,
        Ty::Str | Ty::Bytes => Kind::Inert,
        _ => Kind::Owned,
    }
}

pub(crate) fn is_scalar(ty: &Ty) -> bool {
    kind_of(ty) == Kind::Scalar
}

/// A live temporary register.
#[derive(Clone, Copy)]
struct Temp {
    kind: Kind,
}

/// Where a statement's temporaries start.
#[derive(Clone, Copy)]
pub(crate) struct Mark {
    temps: usize,
    cleanups: usize,
}

/// An enclosing loop, for `break` and `continue`.
pub(crate) struct LoopCtx {
    pub id: colibri_check::hir::LoopId,
    /// How many cleanups were live when the loop began.
    pub cleanups: usize,
    pub brk: Label,
    pub cont: Label,
}

pub(crate) struct FnCompiler<'a> {
    pub env: &'a CompileEnv,
    pub body: &'a Body,
    pub fuse: bool,
    code: Vec<Op>,
    spans: Vec<Span>,
    consts: Vec<Value>,
    sites: Vec<Site>,
    nlocals: usize,
    temps: Vec<Temp>,
    max_temps: usize,
    /// Registers that hold a value to drop at an exit, innermost last: droppable temporaries
    /// and bound non-scalar locals.
    cleanups: Vec<Reg>,
    labels: Vec<LabelState>,
    pub loops: Vec<LoopCtx>,
    /// The `Step` instruction of the straight-line run being emitted.
    step_at: Option<usize>,
    failure: Option<Fail>,
}

impl<'a> FnCompiler<'a> {
    fn compile(
        body: &'a Rc<Body>,
        fn_id: Option<FnId>,
        env: &'a CompileEnv,
        fuse: bool,
    ) -> Result<Chunk, Fail> {
        let mut c = FnCompiler {
            env,
            body,
            fuse,
            code: Vec::new(),
            spans: Vec::new(),
            consts: Vec::new(),
            sites: Vec::new(),
            nlocals: body.locals.len(),
            temps: Vec::new(),
            max_temps: 0,
            cleanups: Vec::new(),
            labels: Vec::new(),
            loops: Vec::new(),
            step_at: None,
            failure: None,
        };
        c.function();
        if let Some(f) = c.failure.take() {
            return Err(f);
        }
        let nregs = c.nlocals + c.max_temps;
        if nregs >= usize::from(u16::MAX) {
            return Err(Fail::Message(format!(
                "`{}` needs more registers than the bytecode VM supports",
                body.name
            )));
        }
        Ok(Chunk {
            fn_id,
            body: Rc::clone(body),
            code: c.code.into_boxed_slice(),
            spans: c.spans.into_boxed_slice(),
            consts: c.consts.into_boxed_slice(),
            sites: c.sites.into_boxed_slice(),
            nregs: nregs as u16,
            nparams: body.param_count as u16,
        })
    }

    pub(crate) fn fail(&mut self, message: impl Into<String>) {
        if self.failure.is_none() {
            self.failure = Some(Fail::Message(message.into()));
        }
    }

    // ---- emitting ----

    pub(crate) fn emit(&mut self, op: Op, span: Span) -> usize {
        self.code.push(op);
        self.spans.push(span);
        self.code.len() - 1
    }

    pub(crate) fn add_const(&mut self, v: Value) -> u32 {
        self.consts.push(v);
        (self.consts.len() - 1) as u32
    }

    pub(crate) fn add_site(&mut self, s: Site) -> u16 {
        if self.sites.len() >= usize::from(u16::MAX) {
            self.fail("a body has more call sites than the bytecode VM supports");
            return 0;
        }
        self.sites.push(s);
        (self.sites.len() - 1) as u16
    }

    pub(crate) fn unsupported(&mut self, what: &str, span: Span) {
        let site = self.add_site(Site::Unsupported(format!(
            "the bytecode VM does not support {what} yet"
        )));
        self.emit(Op::Unsupported { site }, span);
    }

    // ---- labels and jumps ----

    pub(crate) fn new_label(&mut self) -> Label {
        self.labels.push(LabelState::default());
        Label(self.labels.len() - 1)
    }

    /// Binds `l` to the next instruction. A label is a join point, so it ends a run of
    /// statements that share a `Step`.
    pub(crate) fn bind(&mut self, l: Label) {
        self.step_at = None;
        let pos = self.code.len();
        self.labels[l.0].pos = Some(pos);
        for at in std::mem::take(&mut self.labels[l.0].patches) {
            self.set_target(at, pos);
        }
    }

    /// Emits a jump-like instruction whose offset is filled in when the label is bound.
    pub(crate) fn emit_to(&mut self, op: Op, span: Span, l: Label) {
        self.step_at = None;
        let at = self.emit(op, span);
        match self.labels[l.0].pos {
            Some(pos) => self.set_target(at, pos),
            None => self.labels[l.0].patches.push(at),
        }
    }

    fn set_target(&mut self, at: usize, target: usize) {
        let off = target as i64 - (at as i64 + 1);
        let wide = i32::try_from(off).ok();
        let narrow = i16::try_from(off).ok();
        let mut too_far = false;
        match &mut self.code[at] {
            Op::Jmp { off: o }
            | Op::JmpIf { off: o, .. }
            | Op::JmpIfNot { off: o, .. }
            | Op::Loop { off: o } => match wide {
                Some(w) => *o = w,
                None => too_far = true,
            },
            Op::JmpNotLtI { off: o, .. }
            | Op::JmpNotLeI { off: o, .. }
            | Op::JmpNotEqI { off: o, .. }
            | Op::JmpNotNeI { off: o, .. }
            | Op::JmpNotLtIK { off: o, .. }
            | Op::JmpNotLeIK { off: o, .. }
            | Op::JmpNotGtIK { off: o, .. }
            | Op::JmpNotGeIK { off: o, .. }
            | Op::JmpNotEqIK { off: o, .. }
            | Op::JmpNotNeIK { off: o, .. }
            | Op::ForRangeNext { off: o, .. }
            | Op::ForRangeNextIncl { off: o, .. } => match narrow {
                Some(n) => *o = n,
                None => too_far = true,
            },
            other => unreachable!("not a jump: {other:?}"),
        }
        if too_far && self.failure.is_none() {
            self.failure = Some(if self.fuse {
                Fail::TooFar
            } else {
                Fail::Message("a jump is too far for the bytecode".to_string())
            });
        }
    }

    pub(crate) fn jmp(&mut self, l: Label, span: Span) {
        self.emit_to(Op::Jmp { off: 0 }, span, l);
    }

    // ---- the budget ----

    /// Counts one statement in the current straight-line run, starting the run's `Step` when
    /// there is none.
    pub(crate) fn count_statement(&mut self, span: Span) {
        match self.step_at {
            Some(at) => {
                if let Op::Step { n } = &mut self.code[at] {
                    *n += 1;
                }
            }
            None => {
                let at = self.emit(Op::Step { n: 1 }, span);
                self.step_at = Some(at);
            }
        }
    }

    // ---- registers ----

    pub(crate) fn local(&self, id: colibri_check::hir::LocalId) -> Reg {
        id.0 as Reg
    }

    pub(crate) fn storage(&self, id: colibri_check::hir::LocalId) -> Storage {
        self.body.locals[id.0 as usize].storage
    }

    pub(crate) fn local_ty(&self, id: colibri_check::hir::LocalId) -> &'a Ty {
        &self.body.locals[id.0 as usize].ty
    }

    /// The register the next temporary gets.
    pub(crate) fn next_reg(&self) -> usize {
        self.nlocals + self.temps.len()
    }

    pub(crate) fn alloc(&mut self, kind: Kind) -> Reg {
        let r = self.next_reg();
        if r >= usize::from(u16::MAX) {
            self.fail("a body needs more registers than the bytecode VM supports");
            return 0;
        }
        self.temps.push(Temp { kind });
        self.max_temps = self.max_temps.max(self.temps.len());
        r as Reg
    }

    /// Registers a temporary's value for dropping at an exit. Called when the value has been
    /// written, not when the register was allocated, so that values die newest first in the
    /// order they were made (a result register is allocated before the operands that produce
    /// it, but the value comes after theirs).
    pub(crate) fn own(&mut self, r: Reg) {
        if matches!(self.temp_kind(r), Some(Kind::Inert | Kind::Owned)) {
            self.cleanups.push(r);
        }
    }

    pub(crate) fn alloc_for(&mut self, ty: &Ty) -> Reg {
        self.alloc(kind_of(ty))
    }

    fn temp_kind(&self, r: Reg) -> Option<Kind> {
        let idx = usize::from(r).checked_sub(self.nlocals)?;
        self.temps.get(idx).map(|t| t.kind)
    }

    /// Releases an operand temporary right after the instruction that used it: a scalar just
    /// frees its register, a string is dropped now, anything else lives to the statement end.
    /// Not for locals. Temporaries must be freed newest first to give their registers back.
    pub(crate) fn free(&mut self, r: Reg, span: Span) {
        let Some(kind) = self.temp_kind(r) else {
            return;
        };
        match kind {
            Kind::Scalar => {}
            Kind::Inert => {
                self.emit(Op::Drop { r }, span);
                self.forget_cleanup(r);
            }
            Kind::Owned => return,
        }
        if usize::from(r) + 1 == self.next_reg() {
            self.temps.pop();
        }
    }

    /// Frees a temporary whose value was moved out (an instruction took it): no drop.
    pub(crate) fn free_moved(&mut self, r: Reg) {
        if self.temp_kind(r).is_none() {
            return;
        }
        self.forget_cleanup(r);
        if usize::from(r) + 1 == self.next_reg() {
            self.temps.pop();
        }
    }

    fn forget_cleanup(&mut self, r: Reg) {
        if let Some(i) = self.cleanups.iter().rposition(|&c| c == r) {
            self.cleanups.remove(i);
        }
    }

    pub(crate) fn mark(&self) -> Mark {
        Mark {
            temps: self.temps.len(),
            cleanups: self.cleanups.len(),
        }
    }

    /// Ends a statement: whatever it left alive is dropped, newest first.
    pub(crate) fn end_stmt(&mut self, m: Mark, span: Span) {
        let drops: Vec<Reg> = self.cleanups[m.cleanups..].iter().rev().copied().collect();
        for r in drops {
            self.emit(Op::Drop { r }, span);
        }
        self.abandon(m);
    }

    /// Resets the compile-time state to `m` without emitting drops (the code that follows is
    /// unreachable, or the registers were consumed).
    pub(crate) fn abandon(&mut self, m: Mark) {
        self.cleanups.truncate(m.cleanups);
        self.temps.truncate(m.temps);
    }

    /// Binds a non-scalar local: it is dropped when its scope ends.
    pub(crate) fn bind_local_cleanup(&mut self, r: Reg, ty: &Ty) {
        if !is_scalar(ty) {
            self.cleanups.push(r);
        }
    }

    pub(crate) fn cleanup_depth(&self) -> usize {
        self.cleanups.len()
    }

    /// Emits the drops of every cleanup above `depth`, newest first, without forgetting them
    /// (a `break` leaves the scope on one path only). `keep` is a register that must survive
    /// (the value being returned).
    pub(crate) fn emit_drops_above(&mut self, depth: usize, keep: Option<Reg>, span: Span) {
        let drops: Vec<Reg> = self.cleanups[depth..].iter().rev().copied().collect();
        for r in drops {
            if Some(r) != keep {
                self.emit(Op::Drop { r }, span);
            }
        }
    }

    /// Ends a block scope: drops what it bound and forgets it.
    pub(crate) fn end_scope(&mut self, depth: usize, span: Span) {
        self.emit_drops_above(depth, None, span);
        self.cleanups.truncate(depth);
    }
}
