//! The global allocator of the `colibri` binary: mimalloc, with its plain entry points.
//!
//! The `mimalloc` crate's `GlobalAlloc` always calls the aligned entry points, which cost about
//! twice the plain ones on every allocation. mimalloc's plain `mi_malloc` already returns memory
//! aligned to 16 bytes, which covers every type the interpreter allocates (`Value` is 8-aligned),
//! so only a layout asking for more takes the aligned path. This is the only unsafe code of the
//! crate.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout};
use std::ffi::c_void;

use libmimalloc_sys::{
    mi_free, mi_malloc, mi_malloc_aligned, mi_realloc, mi_realloc_aligned, mi_zalloc,
    mi_zalloc_aligned,
};

/// What `mi_malloc` guarantees on a 64-bit target (`MI_MAX_ALIGN_SIZE`).
const PLAIN_ALIGN: usize = 16;

pub struct Mimalloc;

// SAFETY: every method forwards to the mimalloc function of the same meaning. A block from
// `mi_malloc`, `mi_zalloc` or the aligned variants is freed by `mi_free`, and `mi_realloc` and
// `mi_realloc_aligned` accept a block from either family, as long as the alignment asked for
// stays the one the block was allocated with (the `GlobalAlloc` contract: `realloc` keeps
// the layout's alignment).
unsafe impl GlobalAlloc for Mimalloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: a plain allocation; `size` and `align` come from a valid `Layout`.
        unsafe {
            if layout.align() <= PLAIN_ALIGN {
                mi_malloc(layout.size()).cast()
            } else {
                mi_malloc_aligned(layout.size(), layout.align()).cast()
            }
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as in `alloc`.
        unsafe {
            if layout.align() <= PLAIN_ALIGN {
                mi_zalloc(layout.size()).cast()
            } else {
                mi_zalloc_aligned(layout.size(), layout.align()).cast()
            }
        }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: `ptr` came from this allocator, so from one of the mimalloc functions above.
        unsafe { mi_free(ptr.cast::<c_void>()) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr` came from this allocator with `layout`, whose alignment decides the
        // family; the new block keeps it.
        unsafe {
            if layout.align() <= PLAIN_ALIGN {
                mi_realloc(ptr.cast::<c_void>(), new_size).cast()
            } else {
                mi_realloc_aligned(ptr.cast::<c_void>(), new_size, layout.align()).cast()
            }
        }
    }
}
