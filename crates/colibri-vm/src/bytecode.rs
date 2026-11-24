//! The bytecode: [`Op`], [`Chunk`], [`Site`] and a disassembler (vm-plan §2.1, §2.3).
//!
//! A chunk is the compiled form of one HIR body. Registers are addressed relative to the frame's
//! window; locals keep their HIR index (`LocalId(i)` is register `i`, parameters first) and
//! temporaries follow them. Jumps are relative to the instruction after the jump.

use std::fmt::Write;
use std::rc::Rc;

use std::cell::Cell;

use colibri_check::hir::{
    BinOp, Body, Capture, Coercion, FnId, FormatSpec, HostFnId, Intrinsic, Lit, TypeKey, UnOp,
    ValueProj,
};
use colibri_check::types::{Ty, TypeId};
use colibri_core::value::Value;
use colibri_syntax::Span;

/// A register of the running frame.
pub type Reg = u16;

/// One instruction: eight bytes (asserted below). Operands are registers (`u16`), constant
/// indices (`u32`), site indices (`u16`) and relative jumps (`i32`, or `i16` for the fused
/// compare-and-branch forms; the compiler falls back to unfused code when a jump is too far).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    // ---- loads, moves, drops ----
    /// `d = ()`.
    LoadUnit {
        d: Reg,
    },
    LoadBool {
        d: Reg,
        b: bool,
    },
    LoadInt {
        d: Reg,
        i: i32,
    },
    /// `d = consts[k]`.
    LoadK {
        d: Reg,
        k: u32,
    },
    /// `d = s.clone()` (a reference bump for heap values).
    Copy {
        d: Reg,
        s: Reg,
    },
    /// `d = take(s)`: `s` is left `()`.
    Move {
        d: Reg,
        s: Reg,
    },
    /// Releases the value in `r` and leaves `()`.
    Drop {
        r: Reg,
    },
    /// Drops a string or bytes value: nothing can observe when it dies, so no `release`.
    DropInert {
        r: Reg,
    },
    /// Releases `r+n-1` down to `r`, newest first (a run of temporaries at a statement end).
    DropN {
        r: Reg,
        n: u16,
    },

    // ---- locals and cells ----
    /// `l = take(s)`, releasing the value `l` held.
    StoreLocal {
        l: Reg,
        s: Reg,
    },
    /// `l = Cell(take(s))`: binds a local whose storage is a cell.
    NewCell {
        l: Reg,
        s: Reg,
    },
    /// `d = clone of the cell in l`.
    CellGet {
        d: Reg,
        l: Reg,
    },
    /// Writes `take(s)` into the cell in `l`, releasing the old content.
    CellSet {
        l: Reg,
        s: Reg,
    },
    /// Replaces the cell in `r` by its content (a `mut self` parameter that was captured).
    UnwrapCell {
        r: Reg,
    },

    // ---- globals ----
    GetGlobal {
        d: Reg,
        g: u32,
    },
    /// Like `GetGlobal` for a script value that may not be initialised yet.
    GetGlobalChecked {
        d: Reg,
        g: u32,
    },
    /// Panics when the script value `g` is not initialised (the use of a global as a place).
    CheckGlobal {
        g: u32,
    },
    /// `globals[g] = take(s)`, releasing the old value.
    SetGlobal {
        g: u32,
        s: Reg,
    },
    /// Stores the initial value of the script value `g` and marks it initialised.
    InitGlobal {
        g: u32,
        s: Reg,
    },
    /// Calls the initialiser body of global `g` with no arguments; the window starts at `base`.
    CallInit {
        d: Reg,
        base: Reg,
        g: u16,
    },

    // ---- integer fast path (`int`, 64 bits, checked) ----
    AddI {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    SubI {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    MulI {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    DivI {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    RemI {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    AddIK {
        d: Reg,
        a: Reg,
        i: i16,
    },
    /// `d = a * i`, checked.
    MulIK {
        d: Reg,
        a: Reg,
        i: i16,
    },
    /// `d = a / i`; the compiler only emits it for an `i` that is neither 0 nor -1.
    DivIK {
        d: Reg,
        a: Reg,
        i: i16,
    },
    /// `d = a % i`; the compiler only emits it for an `i` that is neither 0 nor -1.
    RemIK {
        d: Reg,
        a: Reg,
        i: i16,
    },
    /// `d = a == i`.
    EqIK {
        d: Reg,
        a: Reg,
        i: i16,
    },
    /// `d = a != i`.
    NeIK {
        d: Reg,
        a: Reg,
        i: i16,
    },
    NegI {
        d: Reg,
        a: Reg,
    },
    LtI {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    LeI {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    EqI {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    NeI {
        d: Reg,
        a: Reg,
        b: Reg,
    },

    // ---- float fast path ----
    AddF {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    SubF {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    MulF {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    DivF {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    NegF {
        d: Reg,
        a: Reg,
    },
    LtF {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    LeF {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    EqF {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    NeF {
        d: Reg,
        a: Reg,
        b: Reg,
    },

    // ---- generic operators (core `Operators`) ----
    Not {
        d: Reg,
        a: Reg,
    },
    /// `d = a op b` for the `BinOp` of the site named by the next instruction (`Site`), through
    /// core (operands are cloned). Four 16-bit operands do not fit one instruction.
    Binary {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    /// The operand word of the instruction before it; never executed on its own.
    Site {
        site: u16,
    },
    Unary {
        d: Reg,
        a: Reg,
        site: u16,
    },

    // ---- control ----
    Jmp {
        off: i32,
    },
    JmpIf {
        r: Reg,
        off: i32,
    },
    JmpIfNot {
        r: Reg,
        off: i32,
    },
    /// A back edge: one budget step, then a jump.
    Loop {
        off: i32,
    },
    /// Charges `n` budget steps (one per statement of a straight-line run).
    Step {
        n: u32,
    },
    /// Fused compare and branch: jumps unless the comparison holds.
    JmpNotLtI {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JmpNotLeI {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JmpNotEqI {
        a: Reg,
        b: Reg,
        off: i16,
    },
    JmpNotNeI {
        a: Reg,
        b: Reg,
        off: i16,
    },
    /// The same against a small constant.
    JmpNotLtIK {
        a: Reg,
        k: i16,
        off: i16,
    },
    JmpNotLeIK {
        a: Reg,
        k: i16,
        off: i16,
    },
    JmpNotGtIK {
        a: Reg,
        k: i16,
        off: i16,
    },
    JmpNotGeIK {
        a: Reg,
        k: i16,
        off: i16,
    },
    JmpNotEqIK {
        a: Reg,
        k: i16,
        off: i16,
    },
    JmpNotNeIK {
        a: Reg,
        k: i16,
        off: i16,
    },

    // ---- `for` over an integer range ----
    /// `i += 1`; if `i < end`, budget step and jump back by `off`. (`end` is not stepped past.)
    ForRangeNext {
        i: Reg,
        end: Reg,
        off: i16,
    },
    /// Inclusive: if `i == end` the loop is over, else `i += 1`, budget step and jump back.
    ForRangeNextIncl {
        i: Reg,
        end: Reg,
        off: i16,
    },

    // ---- calls ----
    /// Calls the function of the site with the window starting at register `base` (the
    /// arguments are `base..`); the result goes to `d`.
    Call {
        d: Reg,
        base: Reg,
        site: u16,
    },
    /// Calls a function of the prelude (`Site::Hidden`): a closure built-in written in Colibri, or a
    /// helper of one. It costs what the native built-in costs, which is all the budget the
    /// built-in takes: the site's `work` is charged here, as core charges it, and neither the
    /// call nor anything the prelude's own code does is.
    CallHidden {
        d: Reg,
        base: Reg,
        site: u16,
    },
    /// `CallValue` for a function value that sits in register `f`, outside the window: the
    /// arguments are `base..`, and nothing is copied. Only the prelude's code uses it.
    CallValueIn {
        d: Reg,
        f: Reg,
        base: Reg,
    },
    /// Calls the host function of the site (`Site::Host`) with owned copies of the arguments in
    /// `base..`, which stay where they are for the caller to drop; the result goes to `d`.
    CallHost {
        d: Reg,
        base: Reg,
        site: u16,
    },
    /// Calls the function value in `base` (a function or a closure) with the arguments
    /// `base+1..`: the callee's window starts at `base + 1`.
    CallValue {
        d: Reg,
        base: Reg,
    },
    /// Calls method `method` of an interface on the receiver in `base` (a value whose
    /// implementation is found from its runtime type, or an interface value); the arguments
    /// follow it. The site holds the inline cache.
    CallBound {
        d: Reg,
        base: Reg,
        site: u16,
    },
    /// The same for a receiver that is statically an interface value.
    CallDyn {
        d: Reg,
        base: Reg,
        site: u16,
    },
    /// A `mut self` interface call: the receiver was copied from its place into `base`; the
    /// callee's window starts at `base + 1`, where the receiver is unwrapped from an interface
    /// value (`base` keeps its shell). `Rewrap` puts the final value back.
    CallIfaceInout {
        d: Reg,
        base: Reg,
        site: u16,
    },
    /// Boxes the value in `recv` again as the interface value whose shell is in `shell` (or
    /// moves it there when the receiver was not an interface value).
    Rewrap {
        shell: Reg,
        recv: Reg,
    },
    /// The operator of an interface (`a + b` on a type parameter) on the operands `base..`.
    CallOperator {
        d: Reg,
        base: Reg,
        site: u16,
    },
    /// `d = closure of the site`: captures read from the registers of the running frame.
    MakeClosure {
        d: Reg,
        site: u16,
    },
    /// Returns the value in `s` (moved).
    Return {
        s: Reg,
    },
    ReturnUnit,

    // ---- output and strings ----
    /// Prints the value in `r` and a newline (the value stays in `r`).
    Print {
        r: Reg,
    },
    /// `d = show(r)`.
    Show {
        d: Reg,
        r: Reg,
    },
    /// `d = format(r, spec of the site)`.
    Format {
        d: Reg,
        r: Reg,
        site: u16,
    },
    /// `d = show(base) ++ show(base+1) ++ .. ++ show(base+n-1)`.
    StrConcat {
        d: Reg,
        base: Reg,
        n: u16,
    },

    // ---- intrinsics and conversions ----
    /// The intrinsic of the site on the arguments `base..base+argc`, which it consumes.
    Intrinsic {
        d: Reg,
        base: Reg,
        site: u16,
    },
    /// The built-in of the site (core `Builtins::builtin`) on references of its own to the
    /// arguments `base..base+argc`, which stay in the window for the caller to drop.
    Builtin {
        d: Reg,
        base: Reg,
        site: u16,
    },
    /// `l.push(take(v))` on the list in `l`: refused while the list is held.
    ListPush {
        l: Reg,
        v: Reg,
    },
    /// `d = l.len()`.
    ListLen {
        d: Reg,
        l: Reg,
    },
    /// `d = s.len()` (bytes of a `str`).
    StrLen {
        d: Reg,
        s: Reg,
    },
    /// `d = coerce(s)` for the coercion of the site (a clone of `s` is converted).
    Coerce {
        d: Reg,
        s: Reg,
        site: u16,
    },

    // ---- heap values ----
    /// `d = Struct { site fields }`: the fields are cloned from the registers of the site.
    NewStruct {
        d: Reg,
        site: u16,
    },
    /// `d = new class object` (tracked for the leak detector).
    NewObject {
        d: Reg,
        site: u16,
    },
    NewVariant {
        d: Reg,
        site: u16,
    },
    NewTuple {
        d: Reg,
        site: u16,
    },
    NewList {
        d: Reg,
        site: u16,
    },
    /// `d = {}`: an empty map, filled by `MapInsert`.
    NewMap {
        d: Reg,
    },
    /// Inserts the entry `k: v` into the map in `m` the way a map literal does (a key that a
    /// user `Eq` calls equal to a stored one is that key).
    MapInsert {
        m: Reg,
        k: Reg,
        v: Reg,
    },
    NewRange {
        d: Reg,
        a: Reg,
        b: Reg,
        incl: bool,
    },
    /// `d = s.<path of the site>`: a clone of the part a struct or tuple projection selects.
    GetPath {
        d: Reg,
        s: Reg,
        site: u16,
    },
    /// `d = s.<field idx>`: a clone of a field of the struct in `s` (`GetPath` of one field).
    GetField {
        d: Reg,
        s: Reg,
        idx: u16,
    },
    /// `d = field idx of the class object in o`.
    ObjGet {
        d: Reg,
        o: Reg,
        idx: u16,
    },
    /// `d = weak(take(s))`: the strong value is released.
    ToWeak {
        d: Reg,
        s: Reg,
    },
    /// `d = upgrade of the weak value in s` as `T?`.
    FromWeak {
        d: Reg,
        s: Reg,
    },

    // ---- places and the mutation guard (vm-plan §1.3, §2.5) ----
    /// `d = the part of the place that the next instruction's site names`, a clone. The anchor
    /// operands are registers: `a` is the local, object, list or map, `b` the list index or map
    /// key, as the anchor kind of the site says.
    PlaceGet {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    /// Like `PlaceGet`, but the part is moved out and `()` is left behind (a `mut self`
    /// receiver on a path of a local).
    PlaceTake {
        d: Reg,
        a: Reg,
        b: Reg,
    },
    /// Writes `take(v)` into the place the next instruction's site names, and releases what it
    /// replaced. A guarded site refuses to write while the collection is iterated or has a place
    /// held; a map entry that does not exist yet is added when the path is empty.
    PlaceSet {
        a: Reg,
        b: Reg,
        v: Reg,
    },
    /// Takes the place hold of the collection the next instruction's site names: it pushes a guard
    /// token on the fiber's hold stack.
    PlaceHold {
        a: Reg,
    },
    /// Releases the newest hold of the fiber's hold stack.
    Unhold,
    /// `d = xs[i]` for the list in `l` and the integer in `i`: a clone of the element, a panic
    /// when the index is out of bounds. (`PlaceGet` with an empty path, without the site.)
    ListGetAt {
        d: Reg,
        l: Reg,
        i: Reg,
    },
    /// `xs[i] = take(v)`: refused while the list is held, a panic out of bounds; the old element
    /// is released.
    ListSetAt {
        l: Reg,
        i: Reg,
        v: Reg,
    },
    /// `d = m[k]`: a clone of the value, a panic when the key is missing.
    MapGetAt {
        d: Reg,
        m: Reg,
        k: Reg,
    },
    /// `m[k] = take(v)`: refused while the map is held; adds the entry when the key is missing.
    MapSetAt {
        m: Reg,
        k: Reg,
        v: Reg,
    },
    /// `l.<path of the site> = take(s)` on a local that is a plain slot: the struct or tuple is
    /// made unique along the path, the old part is released.
    SetPath {
        l: Reg,
        s: Reg,
        site: u16,
    },
    /// `l.<field idx> = take(s)` on a local that holds a struct (`SetPath` of one field).
    SetField {
        l: Reg,
        s: Reg,
        idx: u16,
    },
    /// `d = the key the map in `m` stores for the key in `k``: another key when a user `Eq` says
    /// the two are equal, else a clone of `k`.
    CanonKey {
        d: Reg,
        m: Reg,
        k: Reg,
    },
    /// Holds the iteration guard of the list, map or set in `r` (a `for` loop's).
    HoldIter {
        r: Reg,
    },
    /// Holds the place guard of the list in `r`, named by the site (a `for mut` loop's).
    HoldListPlace {
        r: Reg,
        site: u16,
    },

    // ---- `for` over collections and iterators ----
    // Each loop keeps its source in `a` and its position in `a + 1`. When an item is there, the
    // instruction charges a budget step, puts the item in `d` and skips the next instruction
    // (a jump to the end of the loop); else the next instruction runs.
    ForListNext {
        d: Reg,
        a: Reg,
    },
    /// `for mut` over a list: also leaves a clone of the item in `a + 2`.
    ForListNextVar {
        d: Reg,
        a: Reg,
    },
    /// The item is the tuple `(key, value)`.
    ForMapNext {
        d: Reg,
        a: Reg,
    },
    ForSetNext {
        d: Reg,
        a: Reg,
    },
    /// The source is an iterator value (position unused).
    ForIterNext {
        d: Reg,
        a: Reg,
    },
    /// The end of an iteration of `for mut`: takes the final value of the binding `l`, and
    /// writes it into the list `a` at the index before the one in `a + 1` unless it is the
    /// element `a + 2` already is; releases both.
    ForWriteBack {
        l: Reg,
        a: Reg,
    },

    // ---- patterns (vm-plan §2.7) ----
    // A test is followed by a `Jmp` to where a failed test goes: a test that holds skips it.
    /// The enum value in `r` is the variant `v`.
    TestVariant {
        r: Reg,
        v: u32,
    },
    /// The integer in `r` equals `k` (a `u64` too, when `k` is not negative).
    TestInt {
        r: Reg,
        k: i32,
    },
    /// The value in `r` matches the literal of the site.
    TestLit {
        r: Reg,
        site: u16,
    },
    /// The value in `r` lies in the range of the site.
    TestRange {
        r: Reg,
        site: u16,
    },
    /// The value in `r` equals the module-level constant `g`. Panics when the comparison gave up
    /// (the values nest too deeply).
    TestConst {
        r: Reg,
        g: u32,
    },
    /// `d = part i of s`: a clone of the field of a struct or class object, the element of a
    /// tuple, or the payload of an enum value.
    Unpack {
        d: Reg,
        s: Reg,
        i: u16,
    },

    // ---- unwinding (vm-plan §2.4) ----
    /// Jumps over a deferred statement when the budget grace is used up.
    SkipIfDead {
        off: i32,
    },
    /// Statement end: folds the panics of `Drop` hooks that ran meanwhile into a panic.
    CheckHooks,
    /// `Drop { r }` followed by `CheckHooks`: the commonest pair at the end of a statement.
    DropCheck {
        r: Reg,
    },
    /// The same inside a landing pad: folds them into the panic on its way out.
    FoldHooks,
    /// The end of a landing pad: the frame's panic goes on to its caller.
    Resume,

    // ---- `for` over an iterator that may be a user implementation ----
    /// When the iterator in `a` is a boxed user iterator whose `next` is Colibri code, skips the
    /// next instruction; else falls through.
    IterIsUser {
        a: Reg,
    },
    /// Calls `next` of the user iterator in `a`: the receiver is unwrapped into `w` (the
    /// callee's window starts there), the interface value's shell stays in `a`, the result
    /// goes to `w - 1`.
    IterNextCall {
        a: Reg,
        w: Reg,
    },
    /// After `next` returned: the receiver goes back into the shell in `a`; when the result in
    /// `w - 1` is `some(item)` the item is in `d`, a budget step is charged and the next
    /// instruction is skipped; on `none` the loop is over.
    IterNextEnd {
        d: Reg,
        a: Reg,
        w: Reg,
    },

    /// A node the VM does not compile yet: panics with the site's message when executed. Since
    /// step 11 no program the checker accepts reaches it (every construct compiles), so no test
    /// can name a program for it; it stays for what the checker may add, and the refusal that
    /// can still happen, of a host call that can wait, is pinned by
    /// `colibri-runtime/tests/vm_refusals.rs`.
    Unsupported {
        site: u16,
    },
    /// The coroutine running `wait_until` pauses until it is next resumed, then goes on (the
    /// loop that follows runs the condition again). The check before the loop has made sure
    /// that this is a coroutine that may pause.
    SuspendReady,

    // ---- coroutines (vm-plan §2.8) ----
    /// `d = go call(..)` or `d = group.go(call(..))`: the site says what is called; the
    /// window starting at `base` holds the group (when the site has one) and the call's
    /// arguments. The new coroutine runs until it first pauses, then `d` holds its handle.
    Go {
        d: Reg,
        base: Reg,
        site: u16,
    },
    /// `wait`, `wait_real` or `wait_for` of the operand in `a`: the running coroutine pauses
    /// and goes on with the next instruction when it is resumed. On the main fiber `wait` runs
    /// the event loop instead. Resuming it with a cancel unwinds from here.
    Pause {
        a: Reg,
        kind: PauseKind,
    },
    /// `join`, `join_result` or `join_all` of the handle in `a`. A pause here runs the
    /// instruction again when the coroutine is resumed, which looks at the handle once more.
    Join {
        d: Reg,
        a: Reg,
        kind: JoinKind,
    },
}

/// What a [`Op::Pause`] waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseKind {
    /// Scheduler time: the operand is a number of seconds.
    Wait,
    /// Wall-clock time.
    WaitReal,
    /// The next `emit` of the `Signal` in the operand.
    WaitFor,
}

/// What a [`Op::Join`] waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Join,
    JoinResult,
    JoinAll,
}

/// One range of instructions and the landing pad a panic inside it goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnwindEntry {
    /// The first instruction covered.
    pub start: u32,
    /// One past the last instruction covered.
    pub end: u32,
    /// Where the pad starts.
    pub pad: u32,
}

/// Where a place starts, for the instructions that read and write places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    /// A local that is a plain slot: register `a`.
    Local,
    /// A local that lives in a cell: the cell is in register `a`.
    Cell,
    Global(u32),
    /// A field of the class object in `a`.
    Field(u16),
    /// The element of the list in `a` at the index in `b`.
    Elem,
    /// The entry of the map in `a` for the key in `b`.
    Entry,
}

/// A place: its anchor, the path of struct and tuple projections inside it, and the text that
/// guard messages call the collection.
#[derive(Debug, Clone)]
pub struct PlaceSite {
    pub anchor: AnchorKind,
    pub path: Box<[ValueProj]>,
    pub name: Rc<str>,
    /// A write from user code is refused while the collection is held; the write-back of a place
    /// that holds the guard itself is not.
    pub guarded: bool,
}

/// What an instruction needs beyond its operands.
#[derive(Debug, Clone)]
pub enum Site {
    Place(Box<PlaceSite>),
    /// A collection's name for a guard message.
    Name(Rc<str>),
    /// The registers whose values a constructor clones, in order, and what it builds.
    Struct(TypeId, Box<[Reg]>),
    Object(TypeId, Box<[Reg]>),
    Variant(TypeId, u32, Box<[Reg]>),
    Regs(Box<[Reg]>),
    Path(Box<[ValueProj]>),
    Binary(BinOp),
    Unary(UnOp),
    Call(FnId),
    /// A call of a function of the prelude; `work` is the built-in whose native version this
    /// stands for, which charges one step for each element of its receiver (`work_of`).
    Hidden {
        f: FnId,
        work: Option<colibri_check::hir::Builtin>,
    },
    /// A host function, how many arguments it takes and the type arguments of the call.
    Host(HostFnId, u16, Box<[Ty]>),
    Format(Box<FormatSpec>),
    Intrinsic {
        which: Intrinsic,
        argc: u16,
    },
    F32ToFloat,
    Coerce(Coercion),
    /// A closure: the lifted function and what it captures.
    Closure {
        f: FnId,
        captures: Box<[Capture]>,
    },
    /// An interface method call, with its inline cache.
    Iface(Box<IfaceSite>),
    Operator {
        iface: TypeId,
        rhs: Option<TypeKey>,
        argc: u16,
    },
    /// A literal pattern.
    Lit(Lit),
    /// A range pattern; a missing bound is open.
    Range {
        lo: Option<Lit>,
        hi: Option<Lit>,
        inclusive: bool,
    },
    Unsupported(String),
    /// `go`: the callee and how many values the window holds (a group first, when there is one).
    Go(Box<GoSite>),
}

/// What `go` starts.
#[derive(Debug, Clone)]
pub struct GoSite {
    pub callee: colibri_check::hir::Callee,
    pub type_args: Vec<Ty>,
    /// The values of the window, the group included.
    pub argc: u16,
    /// The first value is the `CoGroup` that owns the new coroutine.
    pub grouped: bool,
}

/// What an interface call site remembers: the implementation and method found for one kind of
/// receiver, valid for one load of the program (`epoch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcEntry {
    pub epoch: u32,
    pub key: IcKey,
    pub target: Option<FnId>,
}

/// The receiver a cache entry is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IcKey {
    /// An interface value with this implementation.
    Impl(u32),
    /// A value of this runtime type (a call through a bound).
    Type(TypeKey),
}

/// An interface method call: the cache lives in the chunk's site, so it dies with the chunk,
/// and its entry names the load of the program it was made for, so a chunk that a running frame
/// keeps across a patch never uses an entry of the old tables.
#[derive(Debug, Clone)]
pub struct IfaceSite {
    pub iface: TypeId,
    pub method: u32,
    pub argc: u16,
    pub cache: Cell<Option<IcEntry>>,
}

/// The compiled form of one body.
#[derive(Debug)]
pub struct Chunk {
    /// The function, for a function body (`None` for initialisers and the script).
    pub fn_id: Option<FnId>,
    /// The body: its name is in stack traces, and it says whether `self` is handed back.
    pub body: Rc<Body>,
    pub code: Box<[Op]>,
    /// Parallel to `code`: the span a panic at that instruction reports (for a call, the span
    /// of the call expression).
    pub spans: Box<[Span]>,
    pub consts: Box<[Value]>,
    pub sites: Box<[Site]>,
    /// Landing pads, sorted by `start`: a panic at an instruction inside `[start, end)` continues
    /// at `pad` (vm-plan §2.4). An instruction outside every range has nothing to clean up.
    pub unwind: Box<[UnwindEntry]>,
    /// Registers of one frame: the locals, then the most temporaries alive at once.
    pub nregs: u16,
    pub nparams: u16,
    /// The body is the prelude's own code (a closure built-in written in Colibri): a stack trace
    /// leaves its frame out, and a panic in it is reported at the call of the built-in.
    pub hidden: bool,
}

impl Op {
    /// One past the highest register that the dispatch loop reads or writes without a bounds
    /// check for this instruction, or 0 when its arm uses checked accesses only. `exec` indexes
    /// the register file unchecked for the instructions that return a bound here, so this is
    /// the proof obligation behind `crate::unchecked`: [`Chunk::verify_registers`] refuses a chunk
    /// whose instruction names a register at or beyond `nregs`.
    ///
    /// Exhaustive on purpose: a new instruction must be classified, and one whose `exec` arm
    /// uses `reg!` or `put!` must be given its registers here.
    pub(crate) fn unchecked_regs_end(&self) -> usize {
        let end = |rs: &[Reg]| rs.iter().map(|&r| usize::from(r) + 1).max().unwrap_or(0);
        match *self {
            Op::LoadUnit { d }
            | Op::LoadBool { d, .. }
            | Op::LoadInt { d, .. }
            | Op::LoadK { d, .. }
            | Op::GetGlobal { d, .. } => end(&[d]),
            Op::Copy { d, s }
            | Op::Move { d, s }
            | Op::NewCell { l: d, s }
            | Op::CellGet { d, l: s }
            | Op::StoreLocal { l: d, s }
            | Op::CellSet { l: d, s }
            | Op::AddIK { d, a: s, .. }
            | Op::MulIK { d, a: s, .. }
            | Op::DivIK { d, a: s, .. }
            | Op::RemIK { d, a: s, .. }
            | Op::EqIK { d, a: s, .. }
            | Op::NeIK { d, a: s, .. }
            | Op::NegI { d, a: s }
            | Op::NegF { d, a: s }
            | Op::Not { d, a: s }
            | Op::Coerce { d, s, .. }
            | Op::GetField { d, s, .. }
            | Op::ListLen { d, l: s }
            | Op::StrLen { d, s }
            | Op::ListPush { l: d, v: s }
            | Op::Unpack { d, s, .. } => end(&[d, s]),
            Op::ForListNext { d, a } => end(&[d, a.saturating_add(1)]),
            Op::ForListNextVar { d, a } => end(&[d, a.saturating_add(2)]),
            Op::Drop { r }
            | Op::DropInert { r }
            | Op::DropCheck { r }
            | Op::UnwrapCell { r }
            | Op::JmpIf { r, .. }
            | Op::JmpIfNot { r, .. }
            | Op::TestVariant { r, .. }
            | Op::TestInt { r, .. }
            | Op::Return { s: r }
            | Op::IterIsUser { a: r } => end(&[r]),
            Op::DropN { r, n } => usize::from(r) + usize::from(n),
            Op::AddI { d, a, b }
            | Op::SubI { d, a, b }
            | Op::MulI { d, a, b }
            | Op::DivI { d, a, b }
            | Op::RemI { d, a, b }
            | Op::LtI { d, a, b }
            | Op::LeI { d, a, b }
            | Op::EqI { d, a, b }
            | Op::NeI { d, a, b }
            | Op::AddF { d, a, b }
            | Op::SubF { d, a, b }
            | Op::MulF { d, a, b }
            | Op::DivF { d, a, b }
            | Op::LtF { d, a, b }
            | Op::LeF { d, a, b }
            | Op::EqF { d, a, b }
            | Op::NeF { d, a, b } => end(&[d, a, b]),
            Op::JmpNotLtI { a, b, .. }
            | Op::JmpNotLeI { a, b, .. }
            | Op::JmpNotEqI { a, b, .. }
            | Op::JmpNotNeI { a, b, .. }
            | Op::ForRangeNext { i: a, end: b, .. }
            | Op::ForRangeNextIncl { i: a, end: b, .. } => end(&[a, b]),
            Op::JmpNotLtIK { a, .. }
            | Op::JmpNotLeIK { a, .. }
            | Op::JmpNotGtIK { a, .. }
            | Op::JmpNotGeIK { a, .. }
            | Op::JmpNotEqIK { a, .. }
            | Op::JmpNotNeIK { a, .. } => end(&[a]),
            Op::CheckGlobal { .. }
            | Op::GetGlobalChecked { .. }
            | Op::SetGlobal { .. }
            | Op::InitGlobal { .. }
            | Op::CallInit { .. }
            | Op::Binary { .. }
            | Op::Site { .. }
            | Op::Unary { .. }
            | Op::Jmp { .. }
            | Op::Loop { .. }
            | Op::Step { .. }
            | Op::Call { .. }
            | Op::CallHidden { .. }
            | Op::CallValueIn { .. }
            | Op::CallHost { .. }
            | Op::CallValue { .. }
            | Op::CallBound { .. }
            | Op::CallDyn { .. }
            | Op::CallIfaceInout { .. }
            | Op::Rewrap { .. }
            | Op::CallOperator { .. }
            | Op::MakeClosure { .. }
            | Op::ReturnUnit
            | Op::Print { .. }
            | Op::Show { .. }
            | Op::Format { .. }
            | Op::StrConcat { .. }
            | Op::Intrinsic { .. }
            | Op::Builtin { .. }
            | Op::NewStruct { .. }
            | Op::NewObject { .. }
            | Op::NewVariant { .. }
            | Op::NewTuple { .. }
            | Op::NewList { .. }
            | Op::NewMap { .. }
            | Op::MapInsert { .. }
            | Op::NewRange { .. }
            | Op::GetPath { .. }
            | Op::ObjGet { .. }
            | Op::ToWeak { .. }
            | Op::FromWeak { .. }
            | Op::PlaceGet { .. }
            | Op::PlaceTake { .. }
            | Op::PlaceSet { .. }
            | Op::PlaceHold { .. }
            | Op::Unhold
            | Op::ListGetAt { .. }
            | Op::ListSetAt { .. }
            | Op::MapGetAt { .. }
            | Op::MapSetAt { .. }
            | Op::SetPath { .. }
            | Op::SetField { .. }
            | Op::CanonKey { .. }
            | Op::HoldIter { .. }
            | Op::HoldListPlace { .. }
            | Op::ForMapNext { .. }
            | Op::ForSetNext { .. }
            | Op::ForIterNext { .. }
            | Op::ForWriteBack { .. }
            | Op::TestLit { .. }
            | Op::TestRange { .. }
            | Op::TestConst { .. }
            | Op::SkipIfDead { .. }
            | Op::CheckHooks
            | Op::FoldHooks
            | Op::Resume
            | Op::IterNextCall { .. }
            | Op::IterNextEnd { .. }
            | Op::Unsupported { .. }
            | Op::SuspendReady
            | Op::Go { .. }
            | Op::Pause { .. }
            | Op::Join { .. } => 0,
        }
    }
}

impl Chunk {
    /// Whether every register that `exec` accesses without a bounds check is inside the
    /// frame's window (see [`Op::unchecked_regs_end`]). The compiler runs it on every chunk.
    pub(crate) fn verify_registers(&self) -> Result<(), String> {
        for (pc, op) in self.code.iter().enumerate() {
            if op.unchecked_regs_end() > usize::from(self.nregs) {
                return Err(format!(
                    "`{}` uses a register beyond its {} at instruction {pc}",
                    self.body.name, self.nregs
                ));
            }
        }
        Ok(())
    }

    /// The landing pad of the instruction at `pc`, if one covers it.
    pub fn pad_for(&self, pc: u32) -> Option<u32> {
        let i = self.unwind.partition_point(|e| e.start <= pc);
        let e = self.unwind.get(i.checked_sub(1)?)?;
        (pc < e.end).then_some(e.pad)
    }

    /// The instructions, one per line (`pc  op`), for tests and debugging.
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        for (pc, op) in self.code.iter().enumerate() {
            let _ = writeln!(out, "{pc:04}  {}", self.describe(pc, op));
        }
        for e in self.unwind.iter() {
            let _ = writeln!(out, "unwind [{:04}, {:04}) -> {:04}", e.start, e.end, e.pad);
        }
        out
    }

    fn describe(&self, pc: usize, op: &Op) -> String {
        let target = |off: i64| pc as i64 + 1 + off;
        match *op {
            Op::LoadUnit { d } => format!("LoadUnit r{d}"),
            Op::LoadBool { d, b } => format!("LoadBool r{d} {b}"),
            Op::LoadInt { d, i } => format!("LoadInt r{d} {i}"),
            Op::LoadK { d, k } => format!("LoadK r{d} k{k} ({})", self.const_text(k)),
            Op::Copy { d, s } => format!("Copy r{d} r{s}"),
            Op::Move { d, s } => format!("Move r{d} r{s}"),
            Op::Drop { r } => format!("Drop r{r}"),
            Op::DropN { r, n } => format!("DropN r{r} n={n}"),
            Op::DropInert { r } => format!("DropInert r{r}"),
            Op::StoreLocal { l, s } => format!("StoreLocal r{l} r{s}"),
            Op::NewCell { l, s } => format!("NewCell r{l} r{s}"),
            Op::CellGet { d, l } => format!("CellGet r{d} r{l}"),
            Op::CellSet { l, s } => format!("CellSet r{l} r{s}"),
            Op::UnwrapCell { r } => format!("UnwrapCell r{r}"),
            Op::GetGlobal { d, g } => format!("GetGlobal r{d} g{g}"),
            Op::GetGlobalChecked { d, g } => format!("GetGlobalChecked r{d} g{g}"),
            Op::CheckGlobal { g } => format!("CheckGlobal g{g}"),
            Op::SetGlobal { g, s } => format!("SetGlobal g{g} r{s}"),
            Op::InitGlobal { g, s } => format!("InitGlobal g{g} r{s}"),
            Op::CallInit { d, base, g } => format!("CallInit r{d} base=r{base} g{g}"),
            Op::AddI { d, a, b } => format!("AddI r{d} r{a} r{b}"),
            Op::SubI { d, a, b } => format!("SubI r{d} r{a} r{b}"),
            Op::MulI { d, a, b } => format!("MulI r{d} r{a} r{b}"),
            Op::DivI { d, a, b } => format!("DivI r{d} r{a} r{b}"),
            Op::RemI { d, a, b } => format!("RemI r{d} r{a} r{b}"),
            Op::AddIK { d, a, i } => format!("AddIK r{d} r{a} {i}"),
            Op::MulIK { d, a, i } => format!("MulIK r{d} r{a} {i}"),
            Op::DivIK { d, a, i } => format!("DivIK r{d} r{a} {i}"),
            Op::RemIK { d, a, i } => format!("RemIK r{d} r{a} {i}"),
            Op::EqIK { d, a, i } => format!("EqIK r{d} r{a} {i}"),
            Op::NeIK { d, a, i } => format!("NeIK r{d} r{a} {i}"),
            Op::NegI { d, a } => format!("NegI r{d} r{a}"),
            Op::LtI { d, a, b } => format!("LtI r{d} r{a} r{b}"),
            Op::LeI { d, a, b } => format!("LeI r{d} r{a} r{b}"),
            Op::EqI { d, a, b } => format!("EqI r{d} r{a} r{b}"),
            Op::NeI { d, a, b } => format!("NeI r{d} r{a} r{b}"),
            Op::AddF { d, a, b } => format!("AddF r{d} r{a} r{b}"),
            Op::SubF { d, a, b } => format!("SubF r{d} r{a} r{b}"),
            Op::MulF { d, a, b } => format!("MulF r{d} r{a} r{b}"),
            Op::DivF { d, a, b } => format!("DivF r{d} r{a} r{b}"),
            Op::NegF { d, a } => format!("NegF r{d} r{a}"),
            Op::LtF { d, a, b } => format!("LtF r{d} r{a} r{b}"),
            Op::LeF { d, a, b } => format!("LeF r{d} r{a} r{b}"),
            Op::EqF { d, a, b } => format!("EqF r{d} r{a} r{b}"),
            Op::NeF { d, a, b } => format!("NeF r{d} r{a} r{b}"),
            Op::Not { d, a } => format!("Not r{d} r{a}"),
            Op::Binary { d, a, b } => format!("Binary r{d} r{a} r{b}"),
            Op::Site { site } => format!("  site {}", self.site_text(site)),
            Op::Unary { d, a, site } => format!("Unary r{d} r{a} {}", self.site_text(site)),
            Op::Jmp { off } => format!("Jmp -> {:04}", target(off.into())),
            Op::JmpIf { r, off } => format!("JmpIf r{r} -> {:04}", target(off.into())),
            Op::JmpIfNot { r, off } => format!("JmpIfNot r{r} -> {:04}", target(off.into())),
            Op::Loop { off } => format!("Loop -> {:04}", target(off.into())),
            Op::Step { n } => format!("Step {n}"),
            Op::JmpNotLtI { a, b, off } => {
                format!("JmpNotLtI r{a} r{b} -> {:04}", target(off.into()))
            }
            Op::JmpNotLeI { a, b, off } => {
                format!("JmpNotLeI r{a} r{b} -> {:04}", target(off.into()))
            }
            Op::JmpNotEqI { a, b, off } => {
                format!("JmpNotEqI r{a} r{b} -> {:04}", target(off.into()))
            }
            Op::JmpNotNeI { a, b, off } => {
                format!("JmpNotNeI r{a} r{b} -> {:04}", target(off.into()))
            }
            Op::JmpNotLtIK { a, k, off } => {
                format!("JmpNotLtIK r{a} {k} -> {:04}", target(off.into()))
            }
            Op::JmpNotLeIK { a, k, off } => {
                format!("JmpNotLeIK r{a} {k} -> {:04}", target(off.into()))
            }
            Op::JmpNotGtIK { a, k, off } => {
                format!("JmpNotGtIK r{a} {k} -> {:04}", target(off.into()))
            }
            Op::JmpNotGeIK { a, k, off } => {
                format!("JmpNotGeIK r{a} {k} -> {:04}", target(off.into()))
            }
            Op::JmpNotEqIK { a, k, off } => {
                format!("JmpNotEqIK r{a} {k} -> {:04}", target(off.into()))
            }
            Op::JmpNotNeIK { a, k, off } => {
                format!("JmpNotNeIK r{a} {k} -> {:04}", target(off.into()))
            }
            Op::ForRangeNext { i, end, off } => {
                format!("ForRangeNext r{i} r{end} -> {:04}", target(off.into()))
            }
            Op::ForRangeNextIncl { i, end, off } => {
                format!("ForRangeNextIncl r{i} r{end} -> {:04}", target(off.into()))
            }
            Op::Call { d, base, site } => {
                format!("Call r{d} base=r{base} {}", self.site_text(site))
            }
            Op::CallHidden { d, base, site } => {
                format!("CallHidden r{d} base=r{base} {}", self.site_text(site))
            }
            Op::CallHost { d, base, site } => {
                format!("CallHost r{d} base=r{base} {}", self.site_text(site))
            }
            Op::CallValue { d, base } => format!("CallValue r{d} base=r{base}"),
            Op::CallValueIn { d, f, base } => format!("CallValueIn r{d} f=r{f} base=r{base}"),
            Op::CallBound { d, base, site } => {
                format!("CallBound r{d} base=r{base} {}", self.site_text(site))
            }
            Op::CallDyn { d, base, site } => {
                format!("CallDyn r{d} base=r{base} {}", self.site_text(site))
            }
            Op::CallIfaceInout { d, base, site } => {
                format!("CallIfaceInout r{d} base=r{base} {}", self.site_text(site))
            }
            Op::Rewrap { shell, recv } => format!("Rewrap r{shell} r{recv}"),
            Op::CallOperator { d, base, site } => {
                format!("CallOperator r{d} base=r{base} {}", self.site_text(site))
            }
            Op::MakeClosure { d, site } => format!("MakeClosure r{d} {}", self.site_text(site)),
            Op::IterIsUser { a } => format!("IterIsUser r{a}"),
            Op::IterNextCall { a, w } => format!("IterNextCall r{a} w=r{w}"),
            Op::IterNextEnd { d, a, w } => format!("IterNextEnd r{d} r{a} w=r{w}"),
            Op::Return { s } => format!("Return r{s}"),
            Op::ReturnUnit => "ReturnUnit".to_string(),
            Op::Print { r } => format!("Print r{r}"),
            Op::Show { d, r } => format!("Show r{d} r{r}"),
            Op::Format { d, r, site } => format!("Format r{d} r{r} {}", self.site_text(site)),
            Op::StrConcat { d, base, n } => format!("StrConcat r{d} base=r{base} n={n}"),
            Op::Intrinsic { d, base, site } => {
                format!("Intrinsic r{d} base=r{base} {}", self.site_text(site))
            }
            Op::Builtin { d, base, site } => {
                format!("Builtin r{d} base=r{base} {}", self.site_text(site))
            }
            Op::ListPush { l, v } => format!("ListPush r{l} r{v}"),
            Op::ListLen { d, l } => format!("ListLen r{d} r{l}"),
            Op::StrLen { d, s } => format!("StrLen r{d} r{s}"),
            Op::Coerce { d, s, site } => format!("Coerce r{d} r{s} {}", self.site_text(site)),
            Op::NewStruct { d, site } => format!("NewStruct r{d} {}", self.site_text(site)),
            Op::NewObject { d, site } => format!("NewObject r{d} {}", self.site_text(site)),
            Op::NewVariant { d, site } => format!("NewVariant r{d} {}", self.site_text(site)),
            Op::NewTuple { d, site } => format!("NewTuple r{d} {}", self.site_text(site)),
            Op::NewList { d, site } => format!("NewList r{d} {}", self.site_text(site)),
            Op::NewMap { d } => format!("NewMap r{d}"),
            Op::MapInsert { m, k, v } => format!("MapInsert r{m} r{k} r{v}"),
            Op::NewRange { d, a, b, incl } => format!("NewRange r{d} r{a} r{b} incl={incl}"),
            Op::GetPath { d, s, site } => format!("GetPath r{d} r{s} {}", self.site_text(site)),
            Op::ObjGet { d, o, idx } => format!("ObjGet r{d} r{o} .{idx}"),
            Op::ToWeak { d, s } => format!("ToWeak r{d} r{s}"),
            Op::FromWeak { d, s } => format!("FromWeak r{d} r{s}"),
            Op::ListGetAt { d, l, i } => format!("ListGetAt r{d} r{l}[r{i}]"),
            Op::ListSetAt { l, i, v } => format!("ListSetAt r{l}[r{i}] r{v}"),
            Op::MapGetAt { d, m, k } => format!("MapGetAt r{d} r{m}[r{k}]"),
            Op::MapSetAt { m, k, v } => format!("MapSetAt r{m}[r{k}] r{v}"),
            Op::PlaceGet { d, a, b } => format!("PlaceGet r{d} r{a} r{b}"),
            Op::PlaceTake { d, a, b } => format!("PlaceTake r{d} r{a} r{b}"),
            Op::PlaceSet { a, b, v } => format!("PlaceSet r{a} r{b} r{v}"),
            Op::PlaceHold { a } => format!("PlaceHold r{a}"),
            Op::Unhold => "Unhold".to_string(),
            Op::GetField { d, s, idx } => format!("GetField r{d} r{s} .{idx}"),
            Op::SetField { l, s, idx } => format!("SetField r{l} r{s} .{idx}"),
            Op::SetPath { l, s, site } => format!("SetPath r{l} r{s} {}", self.site_text(site)),
            Op::CanonKey { d, m, k } => format!("CanonKey r{d} map=r{m} r{k}"),
            Op::HoldIter { r } => format!("HoldIter r{r}"),
            Op::HoldListPlace { r, site } => {
                format!("HoldListPlace r{r} {}", self.site_text(site))
            }
            Op::ForListNext { d, a } => format!("ForListNext r{d} src=r{a}"),
            Op::ForListNextVar { d, a } => format!("ForListNextVar r{d} src=r{a}"),
            Op::ForMapNext { d, a } => format!("ForMapNext r{d} src=r{a}"),
            Op::ForSetNext { d, a } => format!("ForSetNext r{d} src=r{a}"),
            Op::ForIterNext { d, a } => format!("ForIterNext r{d} src=r{a}"),
            Op::ForWriteBack { l, a } => format!("ForWriteBack r{l} src=r{a}"),
            Op::TestVariant { r, v } => format!("TestVariant r{r} .{v}"),
            Op::TestInt { r, k } => format!("TestInt r{r} {k}"),
            Op::TestLit { r, site } => format!("TestLit r{r} {}", self.site_text(site)),
            Op::TestRange { r, site } => format!("TestRange r{r} {}", self.site_text(site)),
            Op::TestConst { r, g } => format!("TestConst r{r} g{g}"),
            Op::Unpack { d, s, i } => format!("Unpack r{d} r{s} .{i}"),
            Op::SkipIfDead { off } => format!("SkipIfDead -> {:04}", target(off.into())),
            Op::CheckHooks => "CheckHooks".to_string(),
            Op::DropCheck { r } => format!("DropCheck r{r}"),
            Op::FoldHooks => "FoldHooks".to_string(),
            Op::Resume => "Resume".to_string(),
            Op::Unsupported { site } => format!("Unsupported {}", self.site_text(site)),
            Op::SuspendReady => "SuspendReady".to_string(),
            Op::Go { d, base, site } => format!("Go r{d} base=r{base} {}", self.site_text(site)),
            Op::Pause { a, kind } => format!("Pause r{a} {kind:?}"),
            Op::Join { d, a, kind } => format!("Join r{d} r{a} {kind:?}"),
        }
    }

    fn const_text(&self, k: u32) -> String {
        match self.consts.get(k as usize) {
            Some(Value::Str(s)) => format!("{s:?}"),
            Some(Value::Float(x)) => format!("{x}"),
            Some(Value::Int(i)) => format!("{i}"),
            Some(Value::U64(u)) => format!("{u}u64"),
            Some(Value::Char(c)) => format!("{c:?}"),
            Some(Value::F32(x)) => format!("{x}f32"),
            Some(other) => format!("{other:?}"),
            None => "?".to_string(),
        }
    }

    fn site_text(&self, site: u16) -> String {
        match self.sites.get(site as usize) {
            Some(Site::Binary(op)) => format!("{op:?}"),
            Some(Site::Unary(op)) => format!("{op:?}"),
            Some(Site::Call(f)) => format!("fn#{}", f.0),
            Some(Site::Hidden { f, work }) => match work {
                Some(b) => format!("fn#{} work({b:?})", f.0),
                None => format!("fn#{}", f.0),
            },
            Some(Site::Host(id, argc, _)) => format!("host#{}/{argc}", id.0),
            Some(Site::Format(spec)) => format!("{spec:?}"),
            Some(Site::Intrinsic { which, argc }) => format!("{which:?}/{argc}"),
            Some(Site::F32ToFloat) => "F32ToFloat".to_string(),
            Some(Site::Coerce(c)) => format!("{c:?}"),
            Some(Site::Closure { f, captures }) => format!("fn#{} {captures:?}", f.0),
            Some(Site::Iface(i)) => format!("iface#{}.{}", i.iface.0, i.method),
            Some(Site::Operator { iface, .. }) => format!("operator iface#{}", iface.0),
            Some(Site::Struct(ty, regs)) => format!("struct#{} {}", ty.0, regs_text(regs)),
            Some(Site::Object(ty, regs)) => format!("class#{} {}", ty.0, regs_text(regs)),
            Some(Site::Variant(ty, v, regs)) => {
                format!("enum#{}.{v} {}", ty.0, regs_text(regs))
            }
            Some(Site::Regs(regs)) => regs_text(regs),
            Some(Site::Path(path)) => format!("{path:?}"),
            Some(Site::Place(p)) => format!(
                "{:?}{:?}{}",
                p.anchor,
                p.path,
                if p.guarded { "" } else { " unguarded" }
            ),
            Some(Site::Name(n)) => format!("{n:?}"),
            Some(Site::Lit(l)) => format!("{l:?}"),
            Some(Site::Range { lo, hi, inclusive }) => {
                format!("{lo:?}{}{hi:?}", if *inclusive { "..=" } else { ".." })
            }
            Some(Site::Unsupported(what)) => what.clone(),
            Some(Site::Go(g)) => format!(
                "{:?}{}/{}",
                g.callee,
                if g.grouped { " in group" } else { "" },
                g.argc
            ),
            None => "?".to_string(),
        }
    }
}

fn regs_text(regs: &[Reg]) -> String {
    let list: Vec<String> = regs.iter().map(|r| format!("r{r}")).collect();
    format!("[{}]", list.join(" "))
}
