//! Native stacks for host functions that a coroutine calls (vm-plan §3.2).
//!
//! Everything else a coroutine does lives on heap frames, so pausing it costs nothing. A host
//! function is Rust code on the native stack, and one that waits for a background operation
//! (`Ctx::run_async`) must be suspended in the middle of its Rust frames. So a call of a host
//! function from a coroutine, where the coroutine could pause, runs on a small pooled native
//! stack (`corosensei`). When the function waits, it switches back to the dispatch loop, which
//! parks the coroutine's heap frames together with that stack ([`Parked`], kept in
//! `VmFiber::pending_host`); when the operation is done the scheduler resumes the coroutine,
//! which first switches back onto the stack to let the function finish. The call then goes on
//! as if it had returned. Calls from the main fiber, from a callback, from a `Drop` hook or
//! from native code that user code runs inside never come here, and neither does a function
//! the host marked `no_await`.
//!
//! This is the one module of the VM that is not safe Rust, besides the C memory of `cffi`. The
//! host function on the stack holds `&mut Machine` and `&mut VmFiber` across the switches
//! while the rest of the program uses both. That is the discipline of native coroutine
//! stacks, and it rests on four facts:
//!
//! 1. The switch is the only way the program passes control to or from the stack, and it is a
//!    call the compiler cannot see through. Both sides give it the pointers they use before the
//!    call and use only the pointers afterwards (`black_box`), so no reference to the machine is
//!    assumed unchanged across it.
//! 2. Nothing is borrowed from the machine or the fiber across a switch by the code in this
//!    file, and the callers of [`run`], [`resume`] and [`park`] hold no borrow into either
//!    except the two references they pass.
//! 3. Neither the machine nor the fiber moves while a stack is parked: the fiber of a
//!    coroutine is a `Box<VmFiber>` that is moved in and out of its slot as a pointer, and the
//!    runtime keeps the machine in a `Box`. Every resumption checks it ([`park`] compares the
//!    pointers it is given with the ones it parked with), and aborts the process if it fails.
//! 4. A stack is never dropped while it is suspended: a parked stack is run to its end (a
//!    cancelled coroutine's host function unwinds), and the one place that would drop a
//!    suspended stack ([`Parked`]'s `Drop`, for a machine that is going away) leaks it
//!    instead.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use colibri_core::value::Value;
use corosensei::stack::{DefaultStack, Stack};
use corosensei::{Coroutine, CoroutineResult, ScopedCoroutine, Yielder};

use crate::fiber::VmFiber;
use crate::host::{HostFault, HostImpl};
use crate::machine::Machine;

/// How big a host function's stack is when the machine is not told otherwise. The stack of a
/// coroutine's own code is on the heap; this one is for host code, which can use a lot of native
/// stack (a recursive parser, a C library), more so in a debug build. The memory is reserved, not
/// committed, and the pages below `HOT_WINDOW` are given back when a stack is pooled.
pub(crate) const DEFAULT_HOST_STACK: usize = 1024 * 1024;

/// How many finished stacks the pool keeps. A server that calls one host function from each of
/// a few coroutines at a time reuses a handful. Each pooled stack is `DEFAULT_HOST_STACK` of
/// address space.
const POOL_CAP: usize = 16;

/// The part of a pooled stack, at its top, that keeps its pages. Below it, the pages a deep run
/// touched are given back when the stack is pooled.
const HOT_WINDOW: usize = 256 * 1024;

/// Every this many stacks that come back are trimmed (`trim_stack` is a system call, and most
/// host functions never get below the hot window), so the memory of a deep run is given back
/// within a few calls and a pooled stack is not trimmed at every call.
const TRIM_EVERY: usize = 32;

/// What the host function may use of its stack through nested calls into Colibri (a callback):
/// the rest is for the host function's own frames and for built-ins, which are not measured.
const USABLE_NUM: usize = 3;
const USABLE_DEN: usize = 4;

/// What a switch onto the stack tells the host function: where the machine and the fiber are,
/// and whether the coroutine was cancelled while it was parked. The pointers are made by the
/// resumer just before the switch.
pub(crate) struct Resume {
    machine: *mut Machine,
    fiber: *mut VmFiber,
    cancel: bool,
}

/// What the host function gives when it parks: the pointers it parks with, which the resumer
/// does not use and the compiler has to assume are used (fact 1).
type Parking = (*mut Machine, *mut VmFiber);

type Y = Yielder<Resume, Parking>;
type Co = Coroutine<Resume, Parking, Result<Value, HostFault>, DefaultStack>;

/// The yielder of the stack a host function is running on, kept in the fiber for [`park`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct YielderPtr(*const Y);

/// Whether `fiber` is running a host function on a stack that it may park.
#[inline]
pub(crate) fn on_host_stack(fiber: &VmFiber) -> bool {
    fiber.yielder.is_some()
}

/// What running a host function on a stack came to.
pub(crate) enum Step {
    /// The function returned.
    Done(Result<Value, HostFault>),
    /// The function parked: its stack waits for the coroutine to be resumed.
    Parked(Parked),
}

/// A host function that waited: its suspended stack, and what the resumer needs to measure
/// nested calls from it.
pub(crate) struct Parked {
    co: Option<Co>,
    /// Where the top of the stack is, and how much the nested calls of a callback may use.
    base: usize,
    budget: usize,
    /// The size the stack was mapped with, to pool it under that size when it ends.
    size: usize,
}

impl Drop for Parked {
    fn drop(&mut self) {
        if let Some(co) = self.co.take() {
            // The stack is suspended inside a host function that holds references into the
            // machine, which may be gone. Dropping it would unwind that stack and run its
            // destructors; leaking it (its mapping stays) runs nothing. This is reached only
            // for a machine that is torn down with coroutines left (`FiberSlab::clear`), after
            // which nothing uses the stack again.
            std::mem::forget(co);
        }
    }
}

/// The pool of stacks and its counters.
pub(crate) struct HostStacks {
    pool: Vec<DefaultStack>,
    /// The size every pooled stack has; a request for another size empties the pool.
    pool_size: usize,
    size: usize,
    /// How many stacks were mapped (not taken from the pool), for tests.
    allocated: usize,
    /// How many stacks came back, to trim one in `TRIM_EVERY` (see `recycle`).
    recycled: usize,
}

impl Default for HostStacks {
    fn default() -> Self {
        HostStacks {
            pool: Vec::new(),
            pool_size: DEFAULT_HOST_STACK,
            size: DEFAULT_HOST_STACK,
            allocated: 0,
            recycled: 0,
        }
    }
}

impl HostStacks {
    /// Sets the size of the stacks that later calls get.
    pub(crate) fn set_size(&mut self, size: usize) {
        self.size = size.max(16 * 1024);
    }

    /// How many stacks were mapped from the system.
    pub(crate) fn allocated(&self) -> usize {
        self.allocated
    }

    /// How many finished stacks wait in the pool.
    pub(crate) fn pooled(&self) -> usize {
        self.pool.len()
    }

    /// A stack of the current size: a pooled one, or a newly mapped one.
    fn acquire(&mut self) -> std::io::Result<DefaultStack> {
        if self.pool_size != self.size {
            self.pool.clear();
            self.pool_size = self.size;
        }
        if let Some(stack) = self.pool.pop() {
            return Ok(stack);
        }
        let stack = DefaultStack::new(self.size)?;
        self.allocated += 1;
        Ok(stack)
    }

    /// A function that ran to its end gives its stack back: pooled, or unmapped when the pool is
    /// full or the stack went deep (see `trim_stack`).
    fn recycle(&mut self, co: Co, size: usize) {
        let stack = co.into_stack();
        self.recycled += 1;
        let pool = size == self.pool_size && self.pool.len() < POOL_CAP;
        // On Windows the check is a read of two fields and decides if the stack is kept at all.
        let trim = cfg!(windows) || self.recycled.is_multiple_of(TRIM_EVERY);
        if pool && (!trim || trim_stack(&stack)) {
            self.pool.push(stack);
        }
    }
}

/// Gives the pages of a finished stack below the hot window back to the system, so that a
/// pooled stack does not keep the memory of its deepest run. Returns whether the stack may go
/// into the pool.
#[cfg(unix)]
fn trim_stack(stack: &DefaultStack) -> bool {
    let base = stack.base().get();
    let limit = stack.limit().get();
    // SAFETY: `sysconf` has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    // The guard page is the lowest page of the mapping: the advice starts above it, so that
    // the range holds only the stack's usable pages. A stack no bigger than the hot window has
    // nothing to give back, and costs no system call.
    let len = match (base - limit)
        .checked_sub(HOT_WINDOW)
        .and_then(|n| n.checked_sub(page))
    {
        Some(len) if len > 0 => len,
        _ => return true,
    };
    // Linux drops the pages at once. Where `MADV_FREE` exists they are reclaimed lazily, when
    // the system is short of memory.
    #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
    let advice = libc::MADV_FREE;
    #[cfg(not(any(target_vendor = "apple", target_os = "freebsd")))]
    let advice = libc::MADV_DONTNEED;
    // SAFETY: `[limit + page, limit + page + len)` lies inside the mapping that `DefaultStack`
    // owns (the length was computed from `base - limit`, less the hot window and the guard
    // page, so the end is `base - HOT_WINDOW`), and is page-aligned: `limit` is the start of
    // the mapping, and `base` its page-aligned end, less a multiple of every page size in use.
    // The function that used the stack ended (the caller holds the stack by value), so nothing
    // on it is live. The advice keeps the mapping and its protections: it only discards the
    // contents, which nothing reads, because a start rewrites the top of the stack and the rest
    // is written before it is read. A failure leaves the pages as they were, so a release build
    // ignores it; a debug build asserts.
    let rc = unsafe { libc::madvise((limit + page) as *mut libc::c_void, len, advice) };
    debug_assert_eq!(rc, 0, "madvise rejected the range of a host stack");
    true
}

/// On Windows the committed part of the stack grows through a chain of guard pages, which
/// decommitting would break, so a stack that went deeper than the hot window is dropped (and
/// unmapped) instead of pooled: only stacks that stayed within the window are pooled there.
#[cfg(windows)]
fn trim_stack(stack: &DefaultStack) -> bool {
    let teb = stack.teb_fields();
    teb.StackBase.saturating_sub(teb.StackLimit) <= HOT_WINDOW
}

#[cfg(not(any(unix, windows)))]
fn trim_stack(_stack: &DefaultStack) -> bool {
    true
}

/// Runs `imp(machine, fiber, args)` on a stack of its own. The function runs until it returns
/// ([`Step::Done`]) or parks ([`Step::Parked`], through [`park`]). If no stack can be mapped the
/// function runs on the caller's stack, where it cannot park.
pub(crate) fn run(
    machine: &mut Machine,
    fiber: &mut VmFiber,
    imp: HostImpl,
    args: Vec<Value>,
) -> Step {
    let stack = match machine.host_stacks.acquire() {
        Ok(stack) => stack,
        Err(_) => return Step::Done(imp(machine, fiber, args)),
    };
    let size = machine.host_stacks.size;
    let base = stack.base().get();
    let body = move |yielder: &Y, first: Resume| -> Result<Value, HostFault> {
        // SAFETY: the pointers come from `enter`, which made them from its own `&mut`
        // references immediately before the switch that started this body, and is blocked in
        // that switch. They point at a live machine and fiber (fact 3: the machine is not moved
        // while a stack exists, the fiber of a coroutine is in a box; `park` checks both at every
        // resumption). While this body runs, the two `&mut` made here are the only references
        // that are used, so they are unique then. That stops being true across a park, and what
        // this code asserts is weaker: both references stay alive across every `park`, and the
        // resumer uses the machine and the fiber through its own `&mut` meanwhile, so under the
        // aliasing rules of Rust (Stacked or Tree Borrows) this is a violation, as in the
        // tree-walker's coroutine stacks. It is sound in practice because of fact 1, and only
        // because of it: every park is the call `Yielder::suspend` in `park`, which the compiler
        // cannot see through and to which `park` hands the pointers, followed by `black_box` of
        // them, so no value read through these references is assumed to survive the call.
        // Nothing here has a name or a type that Miri could check (it cannot run corosensei).
        let (machine, fiber) = unsafe { (&mut *first.machine, &mut *first.fiber) };
        fiber.yielder = Some(YielderPtr(yielder));
        let result = catch_unwind(AssertUnwindSafe(|| imp(machine, fiber, args)));
        fiber.yielder = None;
        match result {
            Ok(result) => result,
            // A Rust panic that the host function did not catch (the runtime's trampoline
            // catches the ones it knows): it goes on to the resumer, off this stack.
            Err(payload) => resume_unwind(payload),
        }
    };
    let co: Co = Coroutine::with_stack(stack, body);
    let parked = Parked {
        co: Some(co),
        base,
        budget: size * USABLE_NUM / USABLE_DEN,
        size,
    };
    enter(machine, fiber, parked, false)
}

/// Switches back onto the parked stack of a coroutine that was resumed, to let its host
/// function go on: with `cancel` set when the coroutine was cancelled meanwhile.
pub(crate) fn resume(
    machine: &mut Machine,
    fiber: &mut VmFiber,
    parked: Parked,
    cancel: bool,
) -> Step {
    enter(machine, fiber, parked, cancel)
}

/// The switch onto the stack, from [`run`] and [`resume`], and what follows it.
fn enter(machine: &mut Machine, fiber: &mut VmFiber, mut parked: Parked, cancel: bool) -> Step {
    let Some(mut co) = parked.co.take() else {
        unreachable!("a parked stack is entered with its coroutine")
    };
    // While the host function runs, the nested calls into Colibri it makes (a callback) measure
    // their native recursion on this stack, not on the one the dispatch loop runs on.
    let saved_budget = std::mem::replace(&mut machine.native_stack_budget, parked.budget);
    let saved_base = std::mem::replace(&mut fiber.stack_base, parked.base);
    let saved_root = std::mem::replace(&mut machine.stack_root, parked.base);
    let pointers: Parking = (&mut *machine, &mut *fiber);
    // From here to the `black_box` below no reference to the machine or the fiber is used
    // (fact 2): `machine` and `fiber` are not touched, `co` is owned by this frame, and the host
    // function on its stack has the only references, made from `pointers`. A panic that the
    // function let through comes out of `resume` (corosensei carries it over): the three values
    // are put back on that path too, and the panic goes on.
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        co.resume(Resume {
            machine: pointers.0,
            fiber: pointers.1,
            cancel,
        })
    }));
    std::hint::black_box(pointers);
    machine.native_stack_budget = saved_budget;
    fiber.stack_base = saved_base;
    machine.stack_root = saved_root;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(payload) => resume_unwind(payload),
    };
    match outcome {
        CoroutineResult::Yield(_) => {
            parked.co = Some(co);
            Step::Parked(parked)
        }
        CoroutineResult::Return(result) => {
            // The function ran to its end (it returned, also when cancelled): only now is its
            // stack freed, into the pool (fact 4).
            machine.host_stacks.recycle(co, parked.size);
            Step::Done(result)
        }
    }
}

/// Parks the host function that is running on this fiber's stack: switches back to whoever
/// started or resumed it, and returns when the coroutine is resumed, with whether it was
/// cancelled. The caller has recorded in the scheduler what the coroutine waits for, and checked
/// [`on_host_stack`].
pub(crate) fn park(machine: &mut Machine, fiber: &mut VmFiber) -> bool {
    let Some(YielderPtr(yielder)) = fiber.yielder else {
        unreachable!("park is called only on a host stack")
    };
    // SAFETY: the yielder is the one `run`'s body was given, a reference into the stack of that
    // body, which is the stack running us (`fiber.yielder` is set at the start of the body and
    // cleared when it ends, and only a host function the body runs gets here). It is valid as
    // long as the body is, and the body is suspended in this very call, never dropped (fact 4).
    // The `&Y` reference is used for this one call and not kept.
    let yielder: &Y = unsafe { &*yielder };
    let pointers: Parking = (&mut *machine, &mut *fiber);
    // The resumer uses the machine and the fiber while this stack is parked. No reference into
    // either is used between this call and its return (fact 2), and afterwards they are used
    // again, so the compiler must not assume they were untouched meanwhile (`black_box`).
    let input = yielder.suspend(pointers);
    std::hint::black_box(pointers);
    if input.machine != pointers.0 || input.fiber != pointers.1 {
        // The references this stack holds point at a place the machine or the fiber left: going
        // on would be undefined behaviour (fact 3).
        eprintln!(
            "internal error: the machine or a coroutine's fiber moved while a host function was parked"
        );
        std::process::abort();
    }
    input.cancel
}

// ---- the stack of the portable walks ----

/// The size of the stack a portable walk runs on, mapped lazily like a host stack.
pub const WALK_STACK: usize = 64 * 1024 * 1024;

/// How much native stack a copy to the portable form (the sender) may use below the top of its
/// walk stack: what is left is for the frames under the walk (the allocator, the entry point).
pub const SENDER_WALK_BUDGET: usize = 48 * 1024 * 1024;

/// How much more the receiver's walk may use than the sender's. A graph the sender accepted
/// must be accepted by whoever rebuilds it, whatever the two walks' frame sizes do not share
/// (a rebuilt value is dropped by a loop, but a walk that is slightly deeper per level must
/// still fit), so the receiver is given room (carry-forward item of the L15 review).
pub const RECEIVER_EXTRA_BUDGET: usize = 64 * 1024;

/// Which end of a thread crossing a walk is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkRole {
    /// Copying a value into its portable form ([`SENDER_WALK_BUDGET`]).
    Sender,
    /// Rebuilding a value from its portable form (the sender's budget and
    /// [`RECEIVER_EXTRA_BUDGET`] more).
    Receiver,
}

impl WalkRole {
    /// The native stack a walk in this role may use.
    pub const fn budget(self) -> usize {
        match self {
            WalkRole::Sender => SENDER_WALK_BUDGET,
            WalkRole::Receiver => SENDER_WALK_BUDGET + RECEIVER_EXTRA_BUDGET,
        }
    }

    /// The native stack a walk in this role may use when it runs on the caller's own stack, of
    /// which `native` bytes are the caller's to use (an engine's `native_stack_budget`): the
    /// walk stack's budget, but never more than the caller has. The receiver keeps its extra over
    /// the sender, so the one relation holds on every stack.
    pub const fn fallback_budget(self, native: usize) -> usize {
        let own = match self {
            WalkRole::Sender => native,
            WalkRole::Receiver => native.saturating_add(RECEIVER_EXTRA_BUDGET),
        };
        if own < self.budget() {
            own
        } else {
            self.budget()
        }
    }
}

thread_local! {
    /// Makes this thread's walks behave as if no stack could be mapped (tests).
    static NO_WALK_STACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes the walks of this thread run on the caller's stack as when no walk stack can be mapped
/// (the pool is emptied too). Tests only.
#[doc(hidden)]
pub fn refuse_walk_stacks(refuse: bool) {
    NO_WALK_STACK.with(|r| r.set(refuse));
    WALK_POOL.with(|p| p.borrow_mut().clear());
}

/// How many stacks a thread keeps for walks.
const WALK_POOL_CAP: usize = 2;

thread_local! {
    /// The walk stacks of this thread: a thread task's worker walks twice per task, so the
    /// mapping is reused. The pages a deep walk touched are given back when it ends
    /// (`trim_stack`), as for host stacks.
    static WALK_POOL: std::cell::RefCell<Vec<DefaultStack>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `walk` on a pooled stack of [`WALK_STACK`] bytes and gives it the guard of the stack: a
/// copy that measures its recursion against it fails with a "too deep" error before it can pass
/// the budget of `role`, whatever stack the calling thread has. Both ends of a crossing use the
/// same size, so the receiver's extra budget decides what each accepts and not the luck of the
/// thread it runs on. If no stack can be mapped the walk runs on the caller's stack, measured from
/// here against [`WalkRole::fallback_budget`] of `native` (the native stack the caller may use),
/// so that a value that is too deep for it is refused as "too deep" and does not overflow it.
pub fn on_walk_stack<R>(
    role: WalkRole,
    native: usize,
    walk: impl FnOnce(colibri_core::portable::StackGuard) -> R,
) -> R {
    let pooled = WALK_POOL.with(|p| p.borrow_mut().pop());
    let mut stack = match pooled {
        Some(stack) => stack,
        None if NO_WALK_STACK.with(std::cell::Cell::get) => {
            return walk(colibri_core::portable::StackGuard::here(
                role.fallback_budget(native),
            ));
        }
        None => match DefaultStack::new(WALK_STACK) {
            Ok(stack) => stack,
            Err(_) => {
                return walk(colibri_core::portable::StackGuard::here(
                    role.fallback_budget(native),
                ));
            }
        },
    };
    let guard = colibri_core::portable::StackGuard::new(stack.base().get(), role.budget());
    type Outcome<R> = Result<R, Box<dyn std::any::Any + Send>>;
    let co = ScopedCoroutine::<(), (), Outcome<R>, &mut DefaultStack>::with_stack(
        &mut stack,
        move |_: &Yielder<(), ()>, ()| catch_unwind(AssertUnwindSafe(|| walk(guard))),
    );
    let outcome = co.scope(|mut co| match co.resume(()) {
        CoroutineResult::Return(outcome) => outcome,
        CoroutineResult::Yield(()) => unreachable!("a walk never yields"),
    });
    if trim_stack(&stack) {
        WALK_POOL.with(|p| {
            let mut pool = p.borrow_mut();
            if pool.len() < WALK_POOL_CAP {
                pool.push(stack);
            }
        });
    }
    match outcome {
        Ok(result) => result,
        Err(payload) => resume_unwind(payload),
    }
}

/// How many walk stacks this thread holds in its pool (for tests).
#[doc(hidden)]
pub fn walk_stacks_pooled() -> usize {
    WALK_POOL.with(|p| p.borrow().len())
}
