//! The Colibri bytecode virtual machine (vm-plan, phase 3).
//!
//! HIR from `colibri-check` is compiled to register bytecode ([`bytecode`], `compile`) and run
//! by a dispatch loop over a heap frame stack (`run`, `fiber`). The machine builds on
//! `colibri-core` and implements its `Engine` trait.
//!
//! The machine is the engine behind `colibri-runtime`. Hosts do not use
//! this crate directly; [`run_source`] is a small entry for tests. A construct the compiler has no
//! instruction for would compile to one that panics with "the bytecode VM does not support
//! <node> yet", which a checked program does not reach today.

// `deny`, not `forbid`: the module `unchecked` (register accesses of the dispatch loop, with the
// proof in `Chunk::verify_registers`), the three `unsafe` blocks of `cffi` (reads and writes of C memory, which the
// program asks for inside its own `unsafe` blocks) and the module `stacks` (the native stacks of
// host functions that wait, vm-plan §3.2) are the only places that are allowed to use it.
#![deny(unsafe_code)]

pub mod bytecode;
mod calls;
#[allow(unsafe_code)]
mod cffi;
mod compile;
mod coro;
mod embed;
mod engine;
mod fiber;
mod heap;
mod host;
mod host_async;
mod leaf;
mod load;
mod machine;
mod patterns;
mod places;
mod profile;
mod rc;
mod run;
mod safepoint;
mod source;
#[allow(unsafe_code)]
#[cfg_attr(target_family = "wasm", path = "stacks_web.rs")]
mod stacks;
#[allow(unsafe_code)]
mod unchecked;
mod unwind;

pub use compile::{CompileEnv, compile_body};
pub use coro::FiberSlab;
pub use embed::SessionUndo;
pub use fiber::VmFiber;
pub use host::{HostClassHooks, HostFault, HostImpl};
pub use machine::{DEFAULT_NATIVE_STACK_BUDGET, Machine};
pub use profile::{PROFILE_SLICE, Profile, ProfileSample, ProfileWeight};
pub use rc::{RcLine, RcProfile};
pub use source::{Options, Outcome, compile_source, run_source, run_source_with};

#[cfg(test)]
mod tests;
