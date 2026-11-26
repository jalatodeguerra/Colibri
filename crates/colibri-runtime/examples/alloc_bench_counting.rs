//! The benchmarks with the counting allocator installed (see `alloc_bench/common.rs`).

use std::alloc::System;

#[global_allocator]
static ALLOC: colibri_runtime::CountingAlloc<System> = colibri_runtime::CountingAlloc(System);

include!("alloc_bench/common.rs");
