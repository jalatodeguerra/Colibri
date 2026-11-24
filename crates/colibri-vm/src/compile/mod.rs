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
//!
//! What has to die when control leaves is one compile-time stack of [`Item`]s: registers to
//! drop, registered `defer`s, a hook-folding mark at every statement and scope, and `mut self`
//! receivers to write back. A normal exit (a block's end, `break`, `continue`, `return`) emits
//! the items above its depth, newest first. A panic does the same from a landing pad: every
//! instruction is tagged with the stack node it runs under, and the unwind table maps a range
//! of instructions to the pad of its node (`unwind.rs` of the runtime side, vm-plan §2.4).

mod call;
mod cond;
mod expr;
mod host;
mod pattern;
mod place;
mod stmt;

use std::collections::HashMap;
use std::rc::Rc;

use colibri_check::hir::{Body, Builtin, FnId, Program, Stmt, Storage};
use colibri_check::types::Ty;
use colibri_core::value::Value;
use colibri_syntax::Span;

use crate::bytecode::{Chunk, Op, Reg, Site, UnwindEntry};

/// What the compiler needs to know about the loaded program.
#[derive(Debug, Default, Clone)]
pub struct CompileEnv {
    /// By global id: the global is a script value, which may be used before its line has run,
    /// so reads and writes check that it is initialised.
    pub checked_globals: Vec<bool>,
    /// By function id: the function is the `Drop` hook of a class. A hook runs while the panics
    /// of earlier hooks may be pending, and the tree-walker folds them at its next statement
    /// end, so every statement of a hook checks.
    pub hook_fns: Vec<bool>,
    /// By function id: the name a function value of it shows.
    pub fn_names: Vec<Rc<str>>,
    /// By type id: the name of the type, for messages.
    pub type_names: Vec<String>,
    /// The built-ins that take a closure and the function written in Colibri that stands for
    /// each: a call of the built-in is compiled as a call of that function.
    pub builtin_fns: Vec<(Builtin, FnId)>,
    /// By type id: a struct or enum with no `Drop` hook whose fields are all release-inert
    /// (type parameters count as inert; a use checks its arguments). See [`CompileEnv::inert`].
    pub type_inert: Vec<bool>,
}

impl CompileEnv {
    /// Whether releasing a value of the type can never be observed (DESIGN §11.2: no `Drop`, no
    /// weak reference anywhere in it): scalars, strings, and structs, tuples, enums and
    /// collections of such types. A class, a closure, an interface value or a type parameter is
    /// not inert. Such a value may be released earlier or later than the lexical rule says.
    pub(crate) fn inert(&self, ty: &Ty) -> bool {
        self.inert_in(ty, false)
    }

    /// `inert`, where `params` says whether a type parameter counts as inert.
    fn inert_in(&self, ty: &Ty, params: bool) -> bool {
        match ty {
            Ty::Int(_)
            | Ty::Float
            | Ty::F32
            | Ty::Bool
            | Ty::Char
            | Ty::Str
            | Ty::Bytes
            | Ty::Unit
            | Ty::Never
            | Ty::Ptr(_) => true,
            Ty::Tuple(ts) => ts.iter().all(|t| self.inert_in(t, params)),
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Range(t) => self.inert_in(t, params),
            Ty::Map(k, v) => self.inert_in(k, params) && self.inert_in(v, params),
            Ty::Adt(t, args) => {
                self.type_inert.get(t.0 as usize).copied().unwrap_or(false)
                    && args.iter().all(|a| self.inert_in(a, params))
            }
            Ty::Param(..) => params,
            Ty::Fn(..) | Ty::Dyn(..) | Ty::Infer(_) | Ty::Error => false,
        }
    }

    /// The function that does what the built-in `b` does, if it is written in Colibri.
    pub(crate) fn builtin_fn(&self, b: Builtin) -> Option<FnId> {
        self.builtin_fns
            .iter()
            .find(|(x, _)| *x == b)
            .map(|(_, f)| *f)
    }
}

impl CompileEnv {
    /// What the compiler needs from a checked program.
    pub fn for_program(program: &Program) -> CompileEnv {
        let mut checked_globals = vec![false; program.globals.len()];
        for g in &program.init_order[program.script_from.min(program.init_order.len())..] {
            checked_globals[g.0 as usize] = true;
        }
        let mut hook_fns = vec![false; program.fns.len()];
        for imp in &program.impls {
            if imp.iface == colibri_check::types::DROP
                && !imp.derived
                && matches!(imp.key, colibri_check::hir::TypeKey::Adt(_))
                && let Some(Some(f)) = imp.methods.first()
                && let Some(slot) = hook_fns.get_mut(f.0 as usize)
            {
                *slot = true;
            }
        }
        let type_inert = inert_types(program);
        CompileEnv {
            type_inert,
            checked_globals,
            hook_fns,
            type_names: program.types.iter().map(|t| t.name.clone()).collect(),
            builtin_fns: program.builtin_fns.clone(),
            fn_names: program
                .fns
                .iter()
                .map(|f| Rc::from(f.body.name.as_str()))
                .collect(),
        }
    }
}

/// Which types of the program are release-inert: the greatest fixed point of "a struct or enum
/// without a `Drop` hook whose field types are inert" (a recursive enum is inert when its other
/// parts are).
///
/// The lending `for mut` relies on this being conservative: a type that can run user code when
/// it is cloned, moved or released (a hook, a class, a closure, an interface value, or any new
/// kind with a finaliser) must stay out of the set, or a lent slot could be seen empty.
fn inert_types(program: &Program) -> Vec<bool> {
    use colibri_check::hir::TypeDefKind;
    let mut hooked = vec![false; program.types.len()];
    for imp in &program.impls {
        if imp.iface == colibri_check::types::DROP
            && let colibri_check::hir::TypeKey::Adt(t) = imp.key
            && let Some(h) = hooked.get_mut(t.0 as usize)
        {
            *h = true;
        }
    }
    let mut inert: Vec<bool> = program
        .types
        .iter()
        .zip(&hooked)
        .map(|(t, &h)| {
            !h && matches!(
                t.kind,
                TypeDefKind::Struct { .. } | TypeDefKind::Enum { .. }
            )
        })
        .collect();
    loop {
        let env = CompileEnv {
            type_inert: inert.clone(),
            ..CompileEnv::default()
        };
        let mut changed = false;
        for (i, t) in program.types.iter().enumerate() {
            if !inert[i] {
                continue;
            }
            let fields: Box<dyn Iterator<Item = &colibri_check::hir::FieldDef>> = match &t.kind {
                TypeDefKind::Struct { fields } => Box::new(fields.iter()),
                TypeDefKind::Enum { variants } => {
                    Box::new(variants.iter().flat_map(|v| v.fields.iter()))
                }
                _ => Box::new(std::iter::empty()),
            };
            for f in fields {
                // A type parameter stands for whatever the use gives, which the use checks.
                if !f.weak && env.inert_in(&f.ty, true) {
                    continue;
                }
                inert[i] = false;
                changed = true;
                break;
            }
        }
        if !changed {
            return inert;
        }
    }
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

/// Something that has to happen when control leaves the code that registered it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Item {
    /// A register that owns a value to drop: a temporary or a bound local.
    Reg(Reg),
    /// A register that owns a string or bytes value: dropping it cannot run a `Drop` hook.
    Plain(Reg),
    /// A `defer` statement (index into the compiler's list), run at the exit.
    Defer(u32),
    /// The end of a statement or scope, where `Drop` hook panics fold into the outcome. The
    /// payload is how many releases had been emitted when the group began.
    Fold(u32),
    /// A `mut self` receiver moved into a call window: written back into its local.
    Recv { l: Reg, r: Reg },
    /// A mutation guard held on the fiber's hold stack, released at the exit.
    Hold,
    /// A `mut self` receiver copied out of a place: written back (unguarded) through the place
    /// whose operands are in `a` and `b`, with the site naming it.
    Place { site: u16, a: Reg, b: Reg, r: Reg },
    /// The binding of a `for mut` iteration: its final value goes back into the list.
    ForBack { l: Reg, a: Reg },
    /// `ForBack` of a loop that may lend the slot of the element.
    ForBackLend { l: Reg, a: Reg },
    /// The receiver of a `mut self` interface call, unwrapped from its interface value in
    /// `shell`: boxed again before it is written back.
    Rewrap { shell: Reg, recv: Reg },
}

/// The stack of items as a path in a trie, so equal stacks share one node and the landing pad
/// of a node is built once. Node 0 is the empty stack.
struct Trie {
    nodes: Vec<(u32, Item)>,
    index: HashMap<(u32, Item), u32>,
}

impl Trie {
    fn new() -> Trie {
        Trie {
            nodes: vec![(0, Item::Fold(0))],
            index: HashMap::new(),
        }
    }

    fn child(&mut self, parent: u32, item: Item) -> u32 {
        if let Some(&n) = self.index.get(&(parent, item)) {
            return n;
        }
        let n = self.nodes.len() as u32;
        self.nodes.push((parent, item));
        self.index.insert((parent, item), n);
        n
    }

    /// The items of the stack a node stands for, bottom first.
    fn items(&self, mut node: u32) -> Vec<Item> {
        let mut out = Vec::new();
        while node != 0 {
            let (parent, item) = self.nodes[node as usize];
            out.push(item);
            node = parent;
        }
        out.reverse();
        out
    }
}

/// The compile-time stack: `nodes[i]` is the trie node of `items[..=i]`.
#[derive(Default)]
struct Stack {
    items: Vec<Item>,
    nodes: Vec<u32>,
}

impl Stack {
    fn top(&self) -> u32 {
        self.nodes.last().copied().unwrap_or(0)
    }
}

/// Instructions that cannot raise a panic, so no landing pad covers them. (A `Drop` runs hooks,
/// but their panics wait for the statement end.)
fn cannot_fail(op: &Op) -> bool {
    matches!(
        op,
        Op::Drop { .. }
            | Op::DropN { .. }
            | Op::DropInert { .. }
            | Op::Move { .. }
            | Op::Copy { .. }
            | Op::LoadUnit { .. }
            | Op::LoadBool { .. }
            | Op::LoadInt { .. }
            | Op::LoadK { .. }
            | Op::Jmp { .. }
            | Op::UnwrapCell { .. }
            | Op::Unhold
            | Op::Rewrap { .. }
            | Op::ForWriteBack { .. }
            | Op::ForWriteBackLend { .. }
            | Op::SkipIfDead { .. }
            | Op::FoldHooks
            | Op::Resume
            | Op::Return { .. }
            | Op::ReturnUnit
    )
}

/// Whether `items` holds the register `r`.
fn holds(items: &[Item], r: Reg) -> bool {
    items
        .iter()
        .any(|&i| i == Item::Reg(r) || i == Item::Plain(r))
}

/// A registered `defer`.
struct DeferInfo {
    stmt: Rc<Stmt>,
    /// How many temporaries were live when it was registered: its code uses the registers
    /// above them.
    temps: usize,
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
    /// How many items were on the stack when the loop began.
    pub cleanups: usize,
    pub brk: Label,
    pub cont: Label,
}

/// The state of an instruction that no pad covers.
const NO_STATE: u32 = u32::MAX;

pub(crate) struct FnCompiler<'a> {
    pub env: &'a CompileEnv,
    pub body: &'a Body,
    pub fuse: bool,
    code: Vec<Op>,
    spans: Vec<Span>,
    /// Parallel to `code`: the trie node the instruction runs under.
    states: Vec<u32>,
    consts: Vec<Value>,
    sites: Vec<Site>,
    nlocals: usize,
    temps: Vec<Temp>,
    max_temps: usize,
    stack: Stack,
    trie: Trie,
    defers: Vec<DeferInfo>,
    /// Instructions run under this node instead of the stack's (an exit sequence in progress).
    state_override: Option<u32>,
    /// Compiling a landing pad: the empty stack is a state of its own there.
    in_pad: bool,
    /// Every statement ends with a hook check (a `Drop` hook, a deferred statement).
    always_check: bool,
    /// How many instructions that can release a value (and so run a `Drop` hook) were emitted.
    releases: u32,
    labels: Vec<LabelState>,
    bound_at: usize,
    pub loops: Vec<LoopCtx>,
    /// The `Step` instruction of the straight-line run being emitted.
    step_at: Option<usize>,
    /// The body is the prelude's own code: it takes no budget steps (the call that reaches it
    /// charges what the native built-in did, see `Op::CallHidden`).
    pub(crate) free: bool,
    failure: Option<Fail>,
    /// The pad of each node that has one, once the pads are emitted.
    pad_labels: HashMap<u32, Label>,
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
            states: Vec::new(),
            consts: Vec::new(),
            sites: Vec::new(),
            nlocals: body.locals.len(),
            temps: Vec::new(),
            max_temps: 0,
            stack: Stack::default(),
            trie: Trie::new(),
            defers: Vec::new(),
            state_override: None,
            in_pad: false,
            always_check: fn_id
                .is_some_and(|f| env.hook_fns.get(f.0 as usize).copied() == Some(true)),
            releases: 0,
            labels: Vec::new(),
            bound_at: usize::MAX,
            loops: Vec::new(),
            step_at: None,
            free: colibri_check::is_prelude_span(body.span),
            failure: None,
            pad_labels: HashMap::new(),
        };
        c.function();
        c.emit_pads();
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
        let unwind = c.unwind_table();
        let chunk = Chunk {
            fn_id,
            body: Rc::clone(body),
            code: c.code.into_boxed_slice(),
            spans: c.spans.into_boxed_slice(),
            consts: c.consts.into_boxed_slice(),
            sites: c.sites.into_boxed_slice(),
            unwind,
            nregs: nregs as u16,
            nparams: body.param_count as u16,
            hidden: colibri_check::is_prelude_span(body.span),
            leaf: None,
        };
        let mut chunk = chunk;
        chunk.leaf = crate::leaf::Leaf::build(&chunk);
        // The dispatch loop indexes registers unchecked; this is what makes that sound.
        chunk.verify_registers().map_err(Fail::Message)?;
        Ok(chunk)
    }

    pub(crate) fn fail(&mut self, message: impl Into<String>) {
        if self.failure.is_none() {
            self.failure = Some(Fail::Message(message.into()));
        }
    }

    // ---- emitting ----

    pub(crate) fn emit(&mut self, op: Op, span: Span) -> usize {
        let node = self.state_override.unwrap_or_else(|| self.stack.top());
        // Without a pad or a receiver to hand back, the empty stack has nothing to run.
        let state = if (node == 0 && !self.in_pad && !self.root_is_real()) || cannot_fail(&op) {
            NO_STATE
        } else {
            node
        };
        self.code.push(op);
        self.spans.push(span);
        self.states.push(state);
        self.code.len() - 1
    }

    /// Emits `CheckHooks`; right after a `Drop` that nothing jumps past, the two become one
    /// `DropCheck` (the drop cannot fail, so only the check's landing pad counts).
    fn emit_check_hooks(&mut self, span: Span) {
        if let Some(&Op::Drop { r }) = self.code.last()
            && self.bound_at != self.code.len()
        {
            let at = self.emit(Op::DropCheck { r }, span);
            // The pair is one instruction: the `Drop` it replaces is dropped from the tables.
            self.code.swap_remove(at - 1);
            self.spans.swap_remove(at - 1);
            self.states.swap_remove(at - 1);
        } else {
            self.emit(Op::CheckHooks, span);
        }
    }

    /// Whether a panic with nothing to clean up still has to run the end of the function: a
    /// `mut self` parameter in a cell is unwrapped before it goes back to the caller.
    fn root_is_real(&self) -> bool {
        self.body.mutates_self
            && self
                .body
                .locals
                .first()
                .is_some_and(|l| l.storage == Storage::Cell)
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
        self.bound_at = pos;
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
            | Op::SkipIfDead { off: o }
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

    /// The jump back to the head of a loop: it charges one budget step, except in the prelude's
    /// own code, which charges nothing.
    pub(crate) fn back_edge(&mut self, head: Label, span: Span) {
        let op = if self.free {
            Op::Jmp { off: 0 }
        } else {
            Op::Loop { off: 0 }
        };
        self.emit_to(op, span, head);
    }

    pub(crate) fn jmp(&mut self, l: Label, span: Span) {
        self.emit_to(Op::Jmp { off: 0 }, span, l);
    }

    // ---- the budget ----

    /// Counts one statement in the current straight-line run, starting the run's `Step` when
    /// there is none.
    pub(crate) fn count_statement(&mut self, span: Span) {
        if self.free {
            return;
        }
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
        match self.temp_kind(r) {
            Some(Kind::Owned) => self.push_item(Item::Reg(r)),
            Some(Kind::Inert) => self.push_item(Item::Plain(r)),
            _ => {}
        }
    }

    pub(crate) fn alloc_for(&mut self, ty: &Ty) -> Reg {
        self.alloc(kind_of(ty))
    }

    pub(crate) fn temp_kind(&self, r: Reg) -> Option<Kind> {
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
                self.emit(Op::DropInert { r }, span);
                self.forget_cleanup(r);
            }
            Kind::Owned => return,
        }
        if usize::from(r) + 1 == self.next_reg() {
            self.temps.pop();
        }
    }

    /// Drops the value a register owns right now (it has a cleanup item), and forgets the item.
    /// A register without one holds nothing to drop.
    pub(crate) fn drop_now(&mut self, r: Reg) {
        let Some(i) = self
            .stack
            .items
            .iter()
            .rposition(|&c| c == Item::Reg(r) || c == Item::Plain(r))
        else {
            return;
        };
        let op = match self.stack.items[i] {
            Item::Plain(_) => Op::DropInert { r },
            _ => {
                self.releases += 1;
                Op::Drop { r }
            }
        };
        self.emit(op, self.body.span);
        self.forget_cleanup(r);
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

    // ---- the item stack ----

    pub(crate) fn push_item(&mut self, item: Item) {
        let node = self.trie.child(self.stack.top(), item);
        self.stack.items.push(item);
        self.stack.nodes.push(node);
    }

    /// Registers `items` at position `at` of the stack, below what is there: they outlive it.
    pub(crate) fn insert_items(&mut self, at: usize, items: &[Item]) {
        let tail = self.stack.items.split_off(at);
        self.stack.nodes.truncate(at);
        for &item in items.iter().chain(tail.iter()) {
            self.push_item(item);
        }
    }

    pub(crate) fn truncate_stack(&mut self, len: usize) {
        self.stack.items.truncate(len);
        self.stack.nodes.truncate(len);
    }

    /// Removes the newest item that is the register `r`, wherever it sits.
    fn forget_cleanup(&mut self, r: Reg) {
        let Some(i) = self
            .stack
            .items
            .iter()
            .rposition(|&c| c == Item::Reg(r) || c == Item::Plain(r))
        else {
            return;
        };
        self.stack.items.remove(i);
        self.stack.nodes.truncate(i);
        for j in i..self.stack.items.len() {
            let item = self.stack.items[j];
            let node = self.trie.child(self.stack.top(), item);
            self.stack.nodes.push(node);
        }
    }

    /// The node of `items` followed by `extra`.
    fn node_of(&mut self, items: &[Item], extra: Option<Item>) -> u32 {
        let mut node = 0;
        for &item in items.iter().chain(extra.iter()) {
            node = self.trie.child(node, item);
        }
        node
    }

    pub(crate) fn mark(&self) -> Mark {
        Mark {
            temps: self.temps.len(),
            cleanups: self.stack.items.len(),
        }
    }

    /// Opens a statement: whatever it leaves alive is dropped at its end, newest first, and the
    /// panics of `Drop` hooks that ran meanwhile fold in there.
    pub(crate) fn begin_group(&mut self) -> Mark {
        let m = self.mark();
        self.push_fold();
        m
    }

    /// Marks a group end on the stack (a statement's or a scope's).
    pub(crate) fn push_fold(&mut self) {
        self.push_item(Item::Fold(self.releases));
    }

    /// An instruction that can release a value of type `ty` was emitted: if it is a class
    /// object (or holds one), a `Drop` hook may run and panic.
    pub(crate) fn note_release(&mut self, ty: &Ty) {
        if kind_of(ty) == Kind::Owned {
            self.releases += 1;
        }
    }

    /// Ends a statement or tail: the items above `m` run, newest first.
    pub(crate) fn end_group(&mut self, m: Mark, span: Span) {
        self.emit_exit(m.cleanups, None, span);
        self.abandon(m);
    }

    /// [`Self::end_group`] for a `let`: the new local `l` is not on the stack yet, but a hook
    /// panic at the statement end must drop it with the rest.
    pub(crate) fn end_group_keeping(&mut self, m: Mark, l: Reg, span: Span) {
        self.emit_exit(m.cleanups, Some(l), span);
        self.abandon(m);
    }

    /// Resets the compile-time state to `m` without emitting drops (the code that follows is
    /// unreachable, or the registers were consumed).
    pub(crate) fn abandon(&mut self, m: Mark) {
        self.truncate_stack(m.cleanups);
        self.temps.truncate(m.temps);
    }

    /// Binds a non-scalar local: it is dropped when its scope ends.
    pub(crate) fn bind_local_cleanup(&mut self, r: Reg, ty: &Ty) {
        match kind_of(ty) {
            Kind::Scalar => {}
            Kind::Inert => self.push_item(Item::Plain(r)),
            Kind::Owned => self.push_item(Item::Reg(r)),
        }
    }

    pub(crate) fn cleanup_depth(&self) -> usize {
        self.stack.items.len()
    }

    /// Whether the group that began when `releases` stood at `start` can have released a
    /// value, so a hook may have panicked in it.
    fn may_have_released(&self, start: u32) -> bool {
        // The prelude's code checks like any other: `fold` releases its old accumulator in the
        // prelude's frame, where the native one released it in the closure's.
        self.always_check || self.releases != start
    }

    /// Emits what leaving down to `depth` does: the registers above it dropped, defers run,
    /// receivers written back, hook panics folded, newest first, without forgetting any of it
    /// (a `break` leaves the scope on one path only). `keep` is a register that must survive
    /// (the value being returned or the value of a block); a panic raised on the way drops it.
    pub(crate) fn emit_exit(&mut self, depth: usize, keep: Option<Reg>, span: Span) {
        let items = self.stack.items.clone();
        let mut i = items.len();
        while i > depth {
            i -= 1;
            // What a panic raised by the code of item `i` still has to clean up: everything
            // below it, and the value being kept.
            let below = |c: &mut Self| {
                let extra = keep.filter(|&k| !holds(&items[..i], k)).map(Item::Reg);
                c.node_of(&items[..i], extra)
            };
            match items[i] {
                Item::Reg(r) | Item::Plain(r) if Some(r) == keep => {}
                Item::Plain(r) => {
                    self.emit(Op::DropInert { r }, span);
                }
                Item::Reg(r) => {
                    // A run of consecutive registers is one instruction.
                    let mut lo = r;
                    while i > depth
                        && let Item::Reg(q) = items[i - 1]
                        && Some(q) != keep
                        && q + 1 == lo
                    {
                        lo = q;
                        i -= 1;
                    }
                    self.releases += 1;
                    if lo == r {
                        self.emit(Op::Drop { r }, span);
                    } else {
                        self.emit(
                            Op::DropN {
                                r: lo,
                                n: r - lo + 1,
                            },
                            span,
                        );
                    }
                }
                Item::Defer(idx) => {
                    let node = below(self);
                    let mut prefix = items[..i].to_vec();
                    prefix.extend(keep.filter(|&k| !holds(&items[..i], k)).map(Item::Reg));
                    self.emit_defer(idx, &prefix, node, span, self.temps.len());
                }
                Item::Fold(start) => {
                    let redundant = matches!(
                        self.code.last(),
                        Some(Op::CheckHooks | Op::DropCheck { .. })
                    ) && self.bound_at != self.code.len();
                    if self.may_have_released(start) && !redundant {
                        let node = below(self);
                        self.state_override = Some(node);
                        self.emit_check_hooks(span);
                        self.state_override = None;
                    }
                }
                Item::Recv { l, r } => {
                    self.emit(Op::Move { d: l, s: r }, span);
                }
                Item::Hold => {
                    self.emit(Op::Unhold, span);
                }
                Item::Place { site, a, b, r } => {
                    let node = below(self);
                    self.state_override = Some(node);
                    self.emit(Op::PlaceSet { a, b, v: r }, span);
                    self.emit(Op::Site { site }, span);
                    self.state_override = None;
                    self.releases += 1;
                }
                Item::ForBack { l, a } => {
                    self.emit(Op::ForWriteBack { l, a }, span);
                    self.releases += 1;
                }
                Item::ForBackLend { l, a } => {
                    self.emit(Op::ForWriteBackLend { l, a }, span);
                    self.releases += 1;
                }
                Item::Rewrap { shell, recv } => {
                    self.emit(Op::Rewrap { shell, recv }, span);
                }
            }
        }
    }

    /// Ends a block scope: runs what it registered and forgets it. `keep` holds the value of
    /// the block.
    pub(crate) fn end_scope(&mut self, depth: usize, keep: Option<Reg>, span: Span) {
        self.emit_exit(depth, keep, span);
        self.truncate_stack(depth);
    }

    // ---- defer ----

    /// Whether a `defer` is registered where the code being emitted is.
    pub(crate) fn has_defer(&self) -> bool {
        self.stack.items.iter().any(|i| matches!(i, Item::Defer(_)))
    }

    /// Registers a `defer` statement; it runs at every exit of the scope it is in.
    pub(crate) fn register_defer(&mut self, stmt: &Rc<Stmt>) {
        let idx = self.defers.len() as u32;
        self.defers.push(DeferInfo {
            stmt: Rc::clone(stmt),
            temps: self.temps.len(),
        });
        self.push_item(Item::Defer(idx));
    }

    /// The code of a deferred statement, skipped past the grace of an exhausted budget. It runs
    /// with `prefix` on the stack (what is still to be cleaned up if it panics) and, as a
    /// statement, folds hook panics at its end.
    fn emit_defer(&mut self, idx: u32, prefix: &[Item], node: u32, span: Span, floor: usize) {
        let stmt = Rc::clone(&self.defers[idx as usize].stmt);
        // Its registers start above the temporaries live where it was registered, and above any
        // the exit it runs for keeps alive (`floor`).
        let temps = self.defers[idx as usize].temps.max(floor);
        let skip = self.new_label();
        self.state_override = Some(node);
        self.emit_to(Op::SkipIfDead { off: 0 }, span, skip);
        self.state_override = None;
        // Its own statements see the stack as it is once the defer has been taken off.
        let mut stack = Stack::default();
        for &item in prefix {
            let n = self.trie.child(stack.top(), item);
            stack.items.push(item);
            stack.nodes.push(n);
        }
        let saved_stack = std::mem::replace(&mut self.stack, stack);
        let dummy = Temp { kind: Kind::Scalar };
        let saved_temps = std::mem::replace(&mut self.temps, vec![dummy; temps]);
        let saved_check = std::mem::replace(&mut self.always_check, true);
        self.step_at = None;
        self.stmt(&stmt);
        self.step_at = None;
        self.always_check = saved_check;
        self.temps = saved_temps;
        self.stack = saved_stack;
        self.bind(skip);
    }

    // ---- landing pads and the unwind table ----

    /// Emits the landing pads after the body: one per stack node that an instruction runs
    /// under, each doing the work of its top item and then jumping to the pad of the node
    /// below, down to the end that resumes the panic in the caller.
    fn emit_pads(&mut self) {
        if self.failure.is_some() {
            return;
        }
        let has_release = self.releases > 0 || !self.defers.is_empty();
        let mut pads: HashMap<u32, Label> = HashMap::new();
        let mut emitted: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut work: Vec<u32> = Vec::new();
        let mut scanned = 0;
        self.in_pad = true;
        let mut root_needed = self.root_is_real();
        loop {
            while scanned < self.states.len() {
                let st = self.states[scanned];
                scanned += 1;
                if st == NO_STATE || pads.contains_key(&st) {
                    continue;
                }
                if !self.pad_needed(st, has_release) {
                    self.states[scanned - 1] = NO_STATE;
                    continue;
                }
                let l = self.new_label();
                pads.insert(st, l);
                work.push(st);
            }
            let Some(n) = work.pop() else { break };
            if emitted.contains(&n) {
                continue;
            }
            let mut cur = n;
            loop {
                if emitted.contains(&cur) || cur == 0 {
                    // The rest is emitted elsewhere (or is the end): jump to it.
                    root_needed |= cur == 0;
                    let span = self.body.span;
                    self.state_override = Some(NO_STATE);
                    self.emit_to(Op::Jmp { off: 0 }, span, pads[&cur]);
                    self.state_override = None;
                    break;
                }
                emitted.insert(cur);
                let (parent, item) = self.trie.nodes[cur as usize];
                let label = match pads.get(&cur) {
                    Some(&l) => l,
                    None => {
                        let l = self.new_label();
                        pads.insert(cur, l);
                        l
                    }
                };
                self.bind(label);
                let span = self.body.span;
                self.state_override = Some(parent);
                match item {
                    Item::Reg(r) | Item::Plain(r) => {
                        self.emit(Op::Drop { r }, span);
                    }
                    Item::Defer(idx) => {
                        let prefix = self.trie.items(parent);
                        self.emit_defer(idx, &prefix, parent, span, 0);
                    }
                    Item::Fold(_) => {
                        // Nothing in this body can release a value, so no hook ran in it.
                        if has_release || self.always_check {
                            self.emit(Op::FoldHooks, span);
                        }
                    }
                    Item::Recv { l, r } => {
                        self.emit(Op::Move { d: l, s: r }, span);
                    }
                    Item::Hold => {
                        self.emit(Op::Unhold, span);
                    }
                    Item::Place { site, a, b, r } => {
                        self.emit(Op::PlaceSet { a, b, v: r }, span);
                        self.emit(Op::Site { site }, span);
                    }
                    Item::ForBack { l, a } => {
                        self.emit(Op::ForWriteBack { l, a }, span);
                    }
                    Item::ForBackLend { l, a } => {
                        self.emit(Op::ForWriteBackLend { l, a }, span);
                    }
                    Item::Rewrap { shell, recv } => {
                        self.emit(Op::Rewrap { shell, recv }, span);
                    }
                }
                self.state_override = None;
                // The pad of the node below follows directly.
                if let std::collections::hash_map::Entry::Vacant(e) = pads.entry(parent) {
                    e.insert(self.new_label());
                }
                cur = parent;
            }
        }
        if root_needed || pads.contains_key(&0) {
            let l = match pads.get(&0) {
                Some(&l) => l,
                None => self.new_label(),
            };
            pads.insert(0, l);
            self.bind(l);
            let span = self.body.span;
            if self.root_is_real() {
                self.emit(Op::UnwrapCell { r: 0 }, span);
            }
            self.emit(Op::Resume, span);
        }
        self.pad_labels = pads;
    }

    /// Whether an instruction under node `n` needs a landing pad: not when nothing it could
    /// have to run is more than a hook fold in a body that never releases anything.
    fn pad_needed(&self, n: u32, has_release: bool) -> bool {
        if n == 0 || has_release || self.root_is_real() {
            return true;
        }
        let mut node = n;
        while node != 0 {
            let (parent, item) = self.trie.nodes[node as usize];
            if !matches!(item, Item::Fold(_)) {
                return true;
            }
            node = parent;
        }
        false
    }

    /// The unwind table: runs of instructions under the same node, with the pc of its pad.
    fn unwind_table(&self) -> Box<[UnwindEntry]> {
        let mut out: Vec<UnwindEntry> = Vec::new();
        let mut pc = 0;
        while pc < self.states.len() {
            let st = self.states[pc];
            if st == NO_STATE {
                pc += 1;
                continue;
            }
            let start = pc;
            while pc < self.states.len() && self.states[pc] == st {
                pc += 1;
            }
            let pad = self.labels[self.pad_labels[&st].0].pos.unwrap_or(0) as u32;
            out.push(UnwindEntry {
                start: start as u32,
                end: pc as u32,
                pad,
            });
        }
        out.into_boxed_slice()
    }
}
