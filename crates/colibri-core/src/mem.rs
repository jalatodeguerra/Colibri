//! Memory accounting: a counting global allocator and the accounts it charges (ARCHITECTURE §35).
//!
//! A host that wants to know (and later limit) how much memory a `Vm` holds installs
//! [`CountingAlloc`] around its allocator. Every allocation then carries a prefix with the tag of
//! the account that was current on its thread when it was made, and `dealloc` and `realloc`
//! credit that same account, wherever and whenever they run. Each `Vm` owns one account
//! ([`MemAccount`]), and `begin_entry` and `end_entry` make it the current one of the thread.
//!
//! The allocator never fails or panics because of a limit: unwinding out of `GlobalAlloc` is not
//! allowed. It only sets the account's `over` flag, which a safepoint reads.
//!
//! # Accounts
//!
//! The accounts live in a table of leaked slots that is reached without a lock, because the
//! allocator reads it. A slot is named by a tag: its index plus one in the low 32 bits and its
//! generation in the high 32 bits, so tag 0 means "no account". A closed slot is reused only when
//! its `used` is back to 0, which means that no allocation that carries its tag is alive: a late
//! `dealloc` can never credit the next owner of the slot. The generation additionally makes a
//! stale tag (one left in a thread-local by an entry that unwound) resolve to nothing.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU32, Ordering};

/// A global allocator that counts the bytes each account holds. Wrap the allocator the program
/// uses (`System`, or another one) and install the wrapper:
///
/// ```
/// use std::alloc::System;
///
/// #[global_allocator]
/// static ALLOC: colibri_core::mem::CountingAlloc<System> =
///     colibri_core::mem::CountingAlloc(System);
/// # fn main() {}
/// ```
///
/// Each allocation is 16 bytes larger (more for an alignment above 16), and is counted with that
/// prefix.
pub struct CountingAlloc<A: GlobalAlloc>(pub A);

/// The least prefix: a `u64` tag, padded so that the user pointer keeps a 16-byte alignment.
const PREFIX: usize = 16;

/// The size of the prefix of an allocation with this alignment: the user pointer is `prefix`
/// bytes after the block start, and a multiple of the alignment from it.
#[inline]
const fn prefix_for(align: usize) -> usize {
    if align > PREFIX { align } else { PREFIX }
}

/// The layout of the block behind a user layout, and the length of its prefix. `None` when the
/// size does not fit (the allocation then fails like any too-large one).
#[inline]
fn block_layout(layout: Layout) -> Option<(Layout, usize)> {
    let prefix = prefix_for(layout.align());
    let size = layout.size().checked_add(prefix)?;
    let block = Layout::from_size_align(size, layout.align().max(PREFIX)).ok()?;
    Some((block, prefix))
}

/// Where the tag of the allocation at `user` is stored: the last 16 bytes before it.
#[inline]
unsafe fn tag_slot(user: *mut u8) -> *mut u64 {
    // SAFETY (caller): `user` is a pointer this allocator returned, so 16 bytes before it lie
    // in the same block, and the block start is 16-aligned, so this address is 8-aligned.
    unsafe { user.sub(PREFIX) as *mut u64 }
}

/// Turns a fresh block into a user pointer: writes the tag of the current account and charges it.
///
/// # Safety
/// `base` is the start of a block of `prefix + size` bytes, aligned to `max(align, 16)`, with
/// `prefix = prefix_for(align)`.
#[inline]
unsafe fn seal(base: *mut u8, prefix: usize, size: usize) -> *mut u8 {
    // SAFETY: the block is `prefix` bytes longer than the user part, so the user pointer is
    // inside it (or one past the end for a zero-sized user part, which `GlobalAlloc` forbids).
    let user = unsafe { base.add(prefix) };
    let tag = current();
    if tag != 0 {
        charge(tag, size + prefix);
    }
    // SAFETY: `tag_slot(user)` is the last 16 bytes of the prefix, inside the block.
    unsafe { tag_slot(user).write(tag) };
    user
}

// SAFETY: every method forwards to the inner allocator with a block that is `prefix` bytes larger
// and aligned at least as strictly, and hands out the pointer `prefix` bytes into it, which keeps
// the alignment the caller asked for (the prefix is a multiple of it). `dealloc` and `realloc`
// undo that offset with the same layout arithmetic, so the inner allocator always sees the pointer
// and layout it handed out.
unsafe impl<A: GlobalAlloc> GlobalAlloc for CountingAlloc<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_installed();
        let Some((block, prefix)) = block_layout(layout) else {
            return ptr::null_mut();
        };
        // SAFETY: `block` has a non-zero size.
        let base = unsafe { self.0.alloc(block) };
        if base.is_null() {
            return base;
        }
        // SAFETY: `base` is a block of the layout `seal` wants.
        unsafe { seal(base, prefix, layout.size()) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_installed();
        let Some((block, prefix)) = block_layout(layout) else {
            return ptr::null_mut();
        };
        // SAFETY: as in `alloc`. The prefix is zeroed too and then overwritten by `seal`.
        let base = unsafe { self.0.alloc_zeroed(block) };
        if base.is_null() {
            return base;
        }
        // SAFETY: as in `alloc`.
        unsafe { seal(base, prefix, layout.size()) }
    }

    #[inline]
    unsafe fn dealloc(&self, user: *mut u8, layout: Layout) {
        // The caller passes a layout `alloc` accepted, so the block layout exists.
        let Some((block, prefix)) = block_layout(layout) else {
            return;
        };
        // SAFETY: `user` came from `alloc` with this layout: its tag is in the prefix and the
        // block starts `prefix` bytes before it.
        unsafe {
            let tag = tag_slot(user).read();
            if tag != 0 {
                credit(tag, layout.size() + prefix);
            }
            self.0.dealloc(user.sub(prefix), block);
        }
    }

    #[inline]
    unsafe fn realloc(&self, user: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let Some((block, prefix)) = block_layout(layout) else {
            return ptr::null_mut();
        };
        let Some(new_block_size) = new_size.checked_add(prefix) else {
            return ptr::null_mut();
        };
        // The new block must be a valid layout too (`realloc` of the inner allocator requires it).
        if Layout::from_size_align(new_block_size, block.align()).is_err() {
            return ptr::null_mut();
        }
        // SAFETY: `user` came from `alloc` with `layout`. The prefix lies in the first bytes of
        // the block, which `realloc` keeps (it copies `min(old, new)` bytes and the prefix is
        // inside both), so the tag is still in front of the moved data. The account is the one
        // recorded: a value that grows after its entry ended still charges the entry that made it. A block with
        // no account is adopted by the current one (below).
        unsafe {
            let tag = tag_slot(user).read();
            let base = self.0.realloc(user.sub(prefix), block, new_block_size);
            if base.is_null() {
                // The old block is untouched and still counted.
                return base;
            }
            if tag != 0 {
                if new_size >= layout.size() {
                    charge(tag, new_size - layout.size());
                } else {
                    credit(tag, layout.size() - new_size);
                }
            } else {
                // A block made outside any account (a host `Vec` argument converted before the
                // entry opened, a global the host built) that is resized inside an entry joins
                // the account that is current: otherwise a script could grow it for free.
                // Nothing is credited, because tag 0 was never charged.
                let now = current();
                if now != 0 && lookup(now).is_some() {
                    charge(now, new_size + prefix);
                    tag_slot(base.add(prefix)).write(now);
                }
            }
            base.add(prefix)
        }
    }
}

// ------------------------------------------------------------------------------------------
// The thread-local current account
// ------------------------------------------------------------------------------------------

thread_local! {
    /// The tag of the account that allocations on this thread are charged to; 0 for none. A const
    /// `Cell<u64>` has no lazy initialisation and no destructor, so reading it from inside the
    /// allocator cannot allocate, register a destructor or hit a destroyed value. (On a target
    /// without native thread-locals, std falls back to OS keys and `thread_local!` may allocate on
    /// first use, which would recurse into the allocator. No target Colibri builds for is like
    /// that: Linux, macOS, Windows and wasm all have a native or plain-static path.)
    static CURRENT: Cell<u64> = const { Cell::new(0) };
}

/// The account allocations on this thread are charged to; 0 while the thread is panicking.
///
/// Allocations made while a Rust panic unwinds are deliberately not counted: std's default panic
/// hook builds a process-wide symbol cache (tens of megabytes with `RUST_BACKTRACE` set) that
/// would otherwise be charged to whichever `Vm` panicked first and stay there. Frees still credit
/// the tag they carry. Grace logic of a memory limit that runs inside a Rust unwind must not
/// expect its allocations to be counted. `panicking()` is a relaxed load of a global count, and a
/// destructor-free thread-local only when that is non-zero, so it neither allocates nor panics.
#[inline]
fn current() -> u64 {
    if std::thread::panicking() {
        return 0;
    }
    // `try_with` cannot fail for a type without a destructor, but the allocator must never panic.
    CURRENT.try_with(Cell::get).unwrap_or(0)
}

/// Makes `tag` current and returns the one it replaced.
#[inline]
fn swap_current(tag: u64) -> u64 {
    CURRENT.try_with(|c| c.replace(tag)).unwrap_or(0)
}

// ------------------------------------------------------------------------------------------
// The "installed" flag
// ------------------------------------------------------------------------------------------

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Called by every allocation: the first one records that the allocator is installed. A load
/// first, so that threads do not fight over the cache line of the flag.
#[inline]
fn note_installed() {
    if !INSTALLED.load(Ordering::Relaxed) {
        INSTALLED.store(true, Ordering::Relaxed);
    }
}

/// Whether [`CountingAlloc`] is the program's global allocator (it has served an allocation).
/// A program that has made any allocation at all knows, so this is reliable from `main` on.
pub fn installed() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

/// A byte count as the memory-limit panic words it: whole `GiB`, `MiB` or `KiB` when it is a
/// multiple of one (`64 MiB`), plain bytes otherwise (`1000 bytes`).
pub fn human_bytes(bytes: u64) -> String {
    for (unit, name) in [(1u64 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")] {
        if bytes >= unit && bytes.is_multiple_of(unit) {
            return format!("{} {name}", bytes / unit);
        }
    }
    format!("{bytes} bytes")
}

/// The text of the memory-limit panic: ``memory limit of 64 MiB exceeded``.
pub fn limit_message(limit: u64) -> String {
    format!("memory limit of {} exceeded", human_bytes(limit))
}

/// Panics with a message that says how to install the allocator, when it is not. A builder calls
/// it for a feature that needs byte counts (`memory_limit`).
#[track_caller]
pub fn require_installed(what: &str) {
    if !installed() {
        panic!(
            "{what} needs the counting allocator, which this program has not installed: add \
             `#[global_allocator] static ALLOC: colibri::CountingAlloc<std::alloc::System> = \
             colibri::CountingAlloc(std::alloc::System);` to the host (or wrap its own allocator)"
        );
    }
}

// ------------------------------------------------------------------------------------------
// The account table
// ------------------------------------------------------------------------------------------

/// One account. Everything is relaxed: the numbers are read at safepoints and by the host, and
/// need no ordering with the memory they describe.
struct Slot {
    /// Bytes allocated under this account and not freed yet (signed, so a bug shows as a
    /// negative number instead of wrapping).
    used: AtomicI64,
    /// `i64::MAX` for none.
    limit: AtomicI64,
    /// An allocation took `used` past `limit` (until `clear_over`).
    over: AtomicBool,
    /// 0 for a slot that was never handed out.
    generation: AtomicU32,
}

impl Slot {
    const fn new() -> Slot {
        Slot {
            used: AtomicI64::new(0),
            limit: AtomicI64::new(i64::MAX),
            over: AtomicBool::new(false),
            generation: AtomicU32::new(0),
        }
    }
}

/// Chunk `k` holds `FIRST << k` slots, so 28 chunks are over 4 billion slots, more than a tag's
/// 32-bit index can name.
const FIRST: usize = 16;
const CHUNKS: usize = 28;

/// The chunks, leaked once made. A pointer is stored (release) after its slots are initialised.
static TABLE: [AtomicPtr<Slot>; CHUNKS] = [const { AtomicPtr::new(ptr::null_mut()) }; CHUNKS];

/// The chunk and the offset in it of slot `index`.
#[inline]
const fn locate(index: usize) -> (usize, usize) {
    let n = index / FIRST + 1;
    let chunk = (usize::BITS - 1 - n.leading_zeros()) as usize;
    (chunk, index - FIRST * ((1usize << chunk) - 1))
}

/// The slot a tag names, if it is still the owner's: the generation must match.
#[inline]
fn lookup(tag: u64) -> Option<&'static Slot> {
    let index = (tag as u32 as usize).checked_sub(1)?;
    let (chunk, offset) = locate(index);
    let base = TABLE.get(chunk)?.load(Ordering::Acquire);
    if base.is_null() {
        return None;
    }
    // SAFETY: a chunk is leaked with `FIRST << chunk` initialised slots and `locate` keeps the
    // offset inside it; slots are never freed or moved.
    let slot = unsafe { &*base.add(offset) };
    (slot.generation.load(Ordering::Relaxed) == (tag >> 32) as u32).then_some(slot)
}

/// Charges `bytes` to the account of `tag` and raises `over` when that passes its limit.
#[inline]
fn charge(tag: u64, bytes: usize) {
    if let Some(slot) = lookup(tag) {
        let bytes = bytes as i64;
        let now = slot
            .used
            .fetch_add(bytes, Ordering::Relaxed)
            .wrapping_add(bytes);
        if now > slot.limit.load(Ordering::Relaxed) {
            slot.over.store(true, Ordering::Relaxed);
        }
    }
}

/// Gives `bytes` back to the account of `tag`.
#[inline]
fn credit(tag: u64, bytes: usize) {
    if let Some(slot) = lookup(tag) {
        slot.used.fetch_sub(bytes as i64, Ordering::Relaxed);
    }
}

/// What `open` and `close` guard: how many slots were handed out, and the closed ones that wait
/// for their allocations to die. Only account creation and closing take this lock, never the
/// allocator.
struct Registry {
    handed_out: usize,
    closed: Vec<usize>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    handed_out: 0,
    closed: Vec::new(),
});

fn slot_at(index: usize) -> &'static Slot {
    let (chunk, offset) = locate(index);
    let base = TABLE[chunk].load(Ordering::Acquire);
    // SAFETY: only called for indexes `open` handed out, whose chunk exists.
    unsafe { &*base.add(offset) }
}

/// A new account, as a tag; 0 when the table is full.
fn open_slot() -> u64 {
    let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    // A closed slot whose allocations are all gone is free again.
    let reusable = reg
        .closed
        .iter()
        .position(|&i| slot_at(i).used.load(Ordering::Acquire) == 0);
    let index = match reusable {
        Some(at) => reg.closed.swap_remove(at),
        None => {
            let index = reg.handed_out;
            let (chunk, offset) = locate(index);
            if chunk >= CHUNKS || index >= u32::MAX as usize - 1 {
                return 0;
            }
            if offset == 0 {
                let slots: Box<[Slot]> = (0..FIRST << chunk).map(|_| Slot::new()).collect();
                let base = Box::leak(slots).as_mut_ptr();
                TABLE[chunk].store(base, Ordering::Release);
            }
            reg.handed_out += 1;
            index
        }
    };
    let slot = slot_at(index);
    slot.limit.store(i64::MAX, Ordering::Relaxed);
    slot.over.store(false, Ordering::Relaxed);
    // The next generation (never 0). Storing it last makes the slot visible to the allocator
    // only once it is clean; the old tag stops resolving at the same moment.
    let generation = match slot.generation.load(Ordering::Relaxed).wrapping_add(1) {
        0 => 1,
        g => g,
    };
    slot.generation.store(generation, Ordering::Release);
    (u64::from(generation) << 32) | (index as u64 + 1)
}

/// Retires an account: its slot is reused once everything allocated under it has been freed.
fn close_slot(tag: u64) {
    if lookup(tag).is_none() {
        return;
    }
    let index = tag as u32 as usize - 1;
    let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    reg.closed.push(index);
}

// ------------------------------------------------------------------------------------------
// The account of a Vm
// ------------------------------------------------------------------------------------------

/// The account of one `Vm` (it lives in its `Core`). Inert, with no table slot, when the counting
/// allocator is not installed, so a program without it pays one comparison per entry.
pub struct MemAccount {
    /// 0 for an inert account.
    tag: u64,
}

/// What an outermost entry must undo: the account that was current before it. Dropping it
/// without [`MemAccount::leave`] (a panic unwound past the entry) restores that account too, so
/// the thread never keeps the tag of a `Vm` that is no longer running.
pub struct EntryGuard {
    prev: u64,
    armed: bool,
}

impl EntryGuard {
    /// A guard that restores nothing (an entry that is not the outermost, or an inert account).
    #[inline]
    pub const fn inert() -> EntryGuard {
        EntryGuard {
            prev: 0,
            armed: false,
        }
    }
}

impl Drop for EntryGuard {
    #[inline]
    fn drop(&mut self) {
        if self.armed {
            swap_current(self.prev);
        }
    }
}

impl MemAccount {
    /// The account of a new `Vm`: inert when the allocator is not installed.
    pub fn new() -> MemAccount {
        if installed() {
            MemAccount::open()
        } else {
            MemAccount { tag: 0 }
        }
    }

    /// A live account whether or not the allocator is installed (tests of the accounting).
    pub fn open() -> MemAccount {
        MemAccount { tag: open_slot() }
    }

    /// Whether this account counts anything.
    pub fn is_live(&self) -> bool {
        self.tag != 0
    }

    /// The tag recorded in the prefix of what is allocated under this account (0 when inert).
    pub fn tag(&self) -> u64 {
        self.tag
    }

    /// Makes this account the current one of the thread (an outermost entry began).
    #[inline]
    pub fn enter(&mut self) -> EntryGuard {
        if self.tag == 0 {
            return EntryGuard::inert();
        }
        EntryGuard {
            prev: swap_current(self.tag),
            armed: true,
        }
    }

    /// Restores the account that was current before `enter` (the outermost entry ended). A `Vm`
    /// inside a host function of another one gives the thread back to the outer one this way.
    #[inline]
    pub fn leave(&mut self, mut guard: EntryGuard) {
        if guard.armed {
            guard.armed = false;
            swap_current(guard.prev);
        }
    }

    /// Bytes charged and not yet credited; `None` for an inert account or one whose slot is gone.
    pub fn used(&self) -> Option<u64> {
        let slot = lookup(self.tag)?;
        Some(slot.used.load(Ordering::Relaxed).max(0) as u64)
    }

    /// Sets the limit in bytes (`None` for none) and re-evaluates `over` against it. Allocation
    /// is never refused: the flag is for a safepoint to read.
    pub fn set_limit(&self, limit: Option<u64>) {
        if let Some(slot) = lookup(self.tag) {
            let limit = limit.map_or(i64::MAX, |l| i64::try_from(l).unwrap_or(i64::MAX));
            slot.limit.store(limit, Ordering::Relaxed);
            let over = slot.used.load(Ordering::Relaxed) > limit;
            slot.over.store(over, Ordering::Relaxed);
        }
    }

    /// The limit in bytes, `None` when there is none or the account is inert.
    pub fn limit(&self) -> Option<u64> {
        let limit = lookup(self.tag)?.limit.load(Ordering::Relaxed);
        (limit != i64::MAX).then_some(limit.max(0) as u64)
    }

    /// Whether `extra` more bytes would take the account past its limit (what a built-in asks
    /// before it allocates in proportion to an argument). False for an inert account.
    pub fn would_exceed(&self, extra: u64) -> bool {
        let Some(slot) = lookup(self.tag) else {
            return false;
        };
        let used = slot.used.load(Ordering::Relaxed).max(0) as u64;
        let limit = slot.limit.load(Ordering::Relaxed);
        limit != i64::MAX && used.saturating_add(extra) > limit.max(0) as u64
    }

    /// Whether an allocation went past the limit since it was set or cleared.
    #[inline]
    pub fn over(&self) -> bool {
        lookup(self.tag).is_some_and(|s| s.over.load(Ordering::Relaxed))
    }

    /// Forgets that the limit was passed (the next allocation past it sets the flag again).
    pub fn clear_over(&self) {
        if let Some(slot) = lookup(self.tag) {
            slot.over.store(false, Ordering::Relaxed);
        }
    }
}

impl Default for MemAccount {
    fn default() -> Self {
        MemAccount::new()
    }
}

impl Drop for MemAccount {
    fn drop(&mut self) {
        if self.tag != 0 {
            close_slot(self.tag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_worded_in_whole_units() {
        assert_eq!(limit_message(64 << 20), "memory limit of 64 MiB exceeded");
        assert_eq!(human_bytes(1 << 30), "1 GiB");
        assert_eq!(human_bytes(3 << 10), "3 KiB");
        assert_eq!(human_bytes(1536), "1536 bytes");
        assert_eq!(human_bytes((1 << 20) + 1), "1048577 bytes");
        assert_eq!(human_bytes(0), "0 bytes");
    }
    use std::alloc::System;
    use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

    static A: CountingAlloc<System> = CountingAlloc(System);

    /// Tests that open accounts hold this shared, so the one that follows a slot through its
    /// reuse (it holds it exclusively) is not raced for that slot.
    static OPENING: RwLock<()> = RwLock::new(());

    fn shared() -> RwLockReadGuard<'static, ()> {
        OPENING.read().unwrap_or_else(|e| e.into_inner())
    }

    fn exclusive() -> RwLockWriteGuard<'static, ()> {
        OPENING.write().unwrap_or_else(|e| e.into_inner())
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).expect("a valid layout")
    }

    /// Runs `f` with `account` current, as an entry does.
    fn within<R>(account: &mut MemAccount, f: impl FnOnce() -> R) -> R {
        let guard = account.enter();
        let r = f();
        account.leave(guard);
        r
    }

    #[test]
    fn prefix_is_sixteen_bytes_and_grows_with_alignment() {
        assert_eq!(prefix_for(1), 16);
        assert_eq!(prefix_for(8), 16);
        assert_eq!(prefix_for(16), 16);
        assert_eq!(prefix_for(32), 32);
        assert_eq!(prefix_for(4096), 4096);
        let (block, prefix) = block_layout(layout(10, 64)).expect("fits");
        assert_eq!((block.size(), block.align(), prefix), (74, 64, 64));
        let (block, prefix) = block_layout(layout(10, 1)).expect("fits");
        assert_eq!((block.size(), block.align(), prefix), (26, 16, 16));
        assert!(block_layout(layout(isize::MAX as usize - 8, 1)).is_none());
    }

    #[test]
    fn locate_covers_every_index_once() {
        let mut next = (0, 0);
        for index in 0..FIRST * 20 {
            assert_eq!(locate(index), next);
            next.1 += 1;
            if next.1 == FIRST << next.0 {
                next = (next.0 + 1, 0);
            }
        }
    }

    #[test]
    fn pointers_keep_their_alignment_and_the_tag_sits_before_them() {
        let _g = shared();
        for align in [1usize, 2, 8, 16, 32, 64, 256, 4096] {
            for size in [1usize, 7, 16, 100] {
                let l = layout(size, align);
                let mut account = MemAccount::open();
                let p = within(&mut account, || unsafe { A.alloc(l) });
                assert!(!p.is_null());
                assert_eq!(p as usize % align, 0, "align {align}");
                assert_eq!(unsafe { tag_slot(p).read() }, account.tag());
                let held = (size + prefix_for(align)) as u64;
                assert_eq!(account.used(), Some(held));
                // The user bytes are all writable.
                unsafe { ptr::write_bytes(p, 0xAB, size) };
                unsafe { A.dealloc(p, l) };
                assert_eq!(account.used(), Some(0));
            }
        }
    }

    #[test]
    fn outside_an_account_nothing_is_counted() {
        let l = layout(24, 8);
        let p = unsafe { A.alloc(l) };
        assert_eq!(unsafe { tag_slot(p).read() }, 0);
        unsafe { A.dealloc(p, l) };
    }

    #[test]
    fn zeroed_allocation_is_zero_and_counted() {
        let _g = shared();
        let mut account = MemAccount::open();
        let l = layout(64, 32);
        let p = within(&mut account, || unsafe { A.alloc_zeroed(l) });
        assert!((0..64).all(|i| unsafe { *p.add(i) } == 0));
        assert_eq!(account.used(), Some(64 + 32));
        unsafe { A.dealloc(p, l) };
        assert_eq!(account.used(), Some(0));
    }

    #[test]
    fn realloc_keeps_the_data_the_tag_and_the_account() {
        let _g = shared();
        for align in [1usize, 16, 64, 1024] {
            let mut account = MemAccount::open();
            let mut other = MemAccount::open();
            let prefix = prefix_for(align);
            let small = layout(8, align);
            let p = within(&mut account, || unsafe { A.alloc(small) });
            for i in 0..8 {
                unsafe { *p.add(i) = i as u8 };
            }
            // Grow while another account is current: the first account is still charged.
            let grown = within(&mut other, || unsafe { A.realloc(p, small, 4096) });
            assert_eq!(grown as usize % align, 0);
            assert_eq!(unsafe { tag_slot(grown).read() }, account.tag());
            assert!((0..8).all(|i| unsafe { *grown.add(i) } == i as u8));
            assert_eq!(account.used(), Some(4096 + prefix as u64));
            assert_eq!(other.used(), Some(0));
            // And shrink outside any account.
            let big = layout(4096, align);
            let shrunk = unsafe { A.realloc(grown, big, 3) };
            assert!((0..3).all(|i| unsafe { *shrunk.add(i) } == i as u8));
            assert_eq!(unsafe { tag_slot(shrunk).read() }, account.tag());
            assert_eq!(account.used(), Some(3 + prefix as u64));
            unsafe { A.dealloc(shrunk, layout(3, align)) };
            assert_eq!(account.used(), Some(0));
        }
    }

    #[test]
    fn a_block_without_an_account_joins_the_current_one_when_resized() {
        let _g = shared();
        let mut account = MemAccount::open();
        for align in [1usize, 16, 64] {
            let prefix = prefix_for(align) as u64;
            let small = layout(8, align);
            // Made outside any account: tag 0, charged to nobody.
            let p = unsafe { A.alloc(small) };
            assert_eq!(unsafe { tag_slot(p).read() }, 0);
            unsafe { *p = 9 };
            let grown = within(&mut account, || unsafe { A.realloc(p, small, 1000) });
            assert_eq!(unsafe { *grown }, 9);
            assert_eq!(unsafe { tag_slot(grown).read() }, account.tag());
            assert_eq!(account.used(), Some(1000 + prefix));
            // From here on it behaves like any block of the account.
            let big = layout(1000, align);
            let more = within(&mut account, || unsafe { A.realloc(grown, big, 3000) });
            assert_eq!(account.used(), Some(3000 + prefix));
            unsafe { A.dealloc(more, layout(3000, align)) };
            assert_eq!(account.used(), Some(0));
        }
        // With no account current it stays uncharged.
        let small = layout(8, 8);
        let p = unsafe { A.alloc(small) };
        let grown = unsafe { A.realloc(p, small, 100) };
        assert_eq!(unsafe { tag_slot(grown).read() }, 0);
        unsafe { A.dealloc(grown, layout(100, 8)) };
    }

    #[test]
    fn nothing_is_charged_while_the_thread_panics() {
        let _g = shared();
        let mut account = MemAccount::open();
        let l = layout(100, 8);
        let guard = account.enter();
        // A value whose drop allocates during the unwind: not charged (a hook would do the same).
        struct AllocOnDrop(Layout, std::sync::mpsc::Sender<usize>);
        impl Drop for AllocOnDrop {
            fn drop(&mut self) {
                let p = unsafe { A.alloc(self.0) };
                self.1.send(unsafe { tag_slot(p).read() } as usize).unwrap();
                unsafe { A.dealloc(p, self.0) };
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _bomb = AllocOnDrop(l, tx);
            panic!("unwinding with an account current");
        }));
        assert!(r.is_err());
        assert_eq!(
            rx.recv().unwrap(),
            0,
            "allocated while panicking, so uncounted"
        );
        // Outside the unwind the account is current again.
        let p = unsafe { A.alloc(l) };
        assert_eq!(unsafe { tag_slot(p).read() }, account.tag());
        unsafe { A.dealloc(p, l) };
        account.leave(guard);
        assert_eq!(account.used(), Some(0));
    }

    #[test]
    fn a_dropped_entry_guard_gives_the_thread_back() {
        let _g = shared();
        let mut outer = MemAccount::open();
        let mut inner = MemAccount::open();
        let go = outer.enter();
        let before = current();
        let gi = inner.enter();
        assert_eq!(current(), inner.tag());
        // A panic unwound past the entry: the guard is dropped, `leave` never ran.
        drop(gi);
        assert_eq!(current(), before);
        outer.leave(go);
        assert_eq!(current(), 0);
    }

    #[test]
    fn a_failed_realloc_changes_nothing() {
        let _g = shared();
        let mut account = MemAccount::open();
        let l = layout(32, 8);
        let p = within(&mut account, || unsafe { A.alloc(l) });
        let before = account.used();
        // Too large for any layout: refused before the inner allocator is asked.
        assert!(unsafe { A.realloc(p, l, usize::MAX - 8) }.is_null());
        assert_eq!(account.used(), before);
        unsafe { A.dealloc(p, l) };
        assert_eq!(account.used(), Some(0));
    }

    #[test]
    fn an_oversized_request_fails_without_counting() {
        let _g = shared();
        let mut account = MemAccount::open();
        let l = layout(isize::MAX as usize - 8, 1);
        assert!(within(&mut account, || unsafe { A.alloc(l) }).is_null());
        assert_eq!(account.used(), Some(0));
    }

    #[test]
    fn the_limit_only_sets_the_flag() {
        let _g = shared();
        let mut account = MemAccount::open();
        account.set_limit(Some(100));
        assert!(!account.over());
        let l = layout(200, 8);
        let p = within(&mut account, || unsafe { A.alloc(l) });
        assert!(!p.is_null());
        assert!(account.over());
        unsafe { A.dealloc(p, l) };
        // Still set: the safepoint decides what to do with it.
        assert!(account.over());
        account.clear_over();
        assert!(!account.over());
        // Raising the limit re-evaluates it.
        let p = within(&mut account, || unsafe { A.alloc(l) });
        assert!(account.over());
        account.set_limit(Some(10_000));
        assert!(!account.over());
        account.set_limit(Some(10));
        assert!(account.over());
        account.set_limit(None);
        assert!(!account.over());
        unsafe { A.dealloc(p, l) };
    }

    #[test]
    fn nested_entries_restore_the_outer_account() {
        let _g = shared();
        let mut outer = MemAccount::open();
        let mut inner = MemAccount::open();
        let go = outer.enter();
        let a = layout(10, 1);
        let p1 = unsafe { A.alloc(a) };
        let gi = inner.enter();
        let p2 = unsafe { A.alloc(a) };
        inner.leave(gi);
        let p3 = unsafe { A.alloc(a) };
        outer.leave(go);
        assert_eq!(current(), 0);
        assert_eq!(outer.used(), Some(2 * 26));
        assert_eq!(inner.used(), Some(26));
        for p in [p1, p2, p3] {
            unsafe { A.dealloc(p, a) };
        }
        assert_eq!((outer.used(), inner.used()), (Some(0), Some(0)));
    }

    #[test]
    fn a_freed_account_is_not_confused_with_the_next() {
        let _g = exclusive();
        let mut first = MemAccount::open();
        let a = layout(500, 1);
        let kept = within(&mut first, || unsafe { A.alloc(a) });
        let old = first.tag();
        drop(first);
        // The slot still has a live allocation, so a new account gets another one.
        let mut second = MemAccount::open();
        assert_ne!(second.tag() as u32, old as u32);
        unsafe { A.dealloc(kept, a) };
        assert_eq!(second.used(), Some(0));
        let q = within(&mut second, || unsafe { A.alloc(a) });
        assert_eq!(second.used(), Some(516));
        unsafe { A.dealloc(q, a) };
        // With nothing left under it the old slot is reused, under a new generation, so the old
        // tag no longer names anything.
        let mut found = None;
        for _ in 0..64 {
            let next = MemAccount::open();
            if next.tag() as u32 == old as u32 {
                found = Some(next);
                break;
            }
            std::mem::forget(next);
        }
        let reused = found.expect("the freed slot is handed out again");
        assert_ne!(reused.tag() >> 32, old >> 32);
        assert!(lookup(old).is_none());
        assert!(lookup(reused.tag()).is_some());
    }

    #[test]
    fn a_stale_current_tag_is_harmless() {
        let _g = shared();
        let account = MemAccount::open();
        let stale = account.tag();
        drop(account);
        // An entry that unwound could leave a closed account's tag current. Allocations made
        // under it still balance out, and the slot is not reused while one is alive.
        let before = swap_current(stale);
        let l = layout(40, 8);
        let p = unsafe { A.alloc(l) };
        swap_current(before);
        let used = |tag| lookup(tag).map(|s| s.used.load(Ordering::Relaxed));
        assert_eq!(used(stale), Some(56));
        let other = MemAccount::open();
        assert_ne!(other.tag() as u32, stale as u32);
        unsafe { A.dealloc(p, l) };
        assert_eq!(used(stale), Some(0));
    }

    #[test]
    fn credits_from_another_thread_reach_the_account() {
        let _g = shared();
        let mut account = MemAccount::open();
        let l = layout(1000, 8);
        /// A pointer that may cross to another thread (the test owns the block).
        struct Block(*mut u8);
        // SAFETY: the block is only touched by the thread that frees it, after this one is done.
        unsafe impl Send for Block {}
        let block = Block(within(&mut account, || unsafe { A.alloc(l) }));
        assert_eq!(account.used(), Some(1016));
        std::thread::spawn(move || {
            let block = block;
            unsafe { A.dealloc(block.0, l) }
        })
        .join()
        .expect("the thread ends");
        assert_eq!(account.used(), Some(0));
    }

    #[test]
    fn an_inert_account_counts_nothing_and_leaves_the_thread_alone() {
        let mut inert = MemAccount { tag: 0 };
        let before = current();
        let guard = inert.enter();
        assert_eq!(current(), before);
        inert.leave(guard);
        assert_eq!(inert.used(), None);
        assert!(!inert.over());
        inert.set_limit(Some(1));
        inert.clear_over();
    }

    #[test]
    fn many_accounts_grow_the_table() {
        let _g = shared();
        let accounts: Vec<MemAccount> = (0..200).map(|_| MemAccount::open()).collect();
        let mut tags: Vec<u64> = accounts.iter().map(MemAccount::tag).collect();
        assert!(tags.iter().all(|&t| t != 0));
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), 200);
        assert!(accounts.iter().all(|a| a.used() == Some(0)));
    }
}
