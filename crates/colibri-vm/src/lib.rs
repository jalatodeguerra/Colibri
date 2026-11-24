//! The Colibri bytecode virtual machine (vm-plan, phase 3).
//!
//! HIR from `colibri-check` is compiled to register bytecode ([`bytecode`], `compile`) and run
//! by a dispatch loop over a heap frame stack (`run`, `fiber`). The machine shares
//! `colibri-core` with the tree-walking interpreter and implements its `Engine` trait.
//!
//! This is the first slice: integer, float, bool and `str` code with functions, globals,
//! control flow and `for` over ranges. Every other node compiles to an instruction that
//! panics with "the bytecode VM does not support <node> yet". Hosts do not use this crate
//! directly yet; [`run_source`] is a small entry for tests.

// `deny`, not `forbid`: the three `unsafe` blocks of `cffi` (reads and writes of C memory, which the
// program asks for inside its own `unsafe` blocks) are the only ones, and they are allowed there.
#![deny(unsafe_code)]

pub mod bytecode;
#[allow(unsafe_code)]
mod cffi;
mod compile;
mod embed;
mod engine;
mod fiber;
mod heap;
mod host;
mod load;
mod machine;
mod patterns;
mod places;
mod run;
mod source;
mod unwind;

pub use compile::{CompileEnv, compile_body};
pub use fiber::VmFiber;
pub use host::{HostClassHooks, HostFault, HostImpl};
pub use machine::{DEFAULT_NATIVE_STACK_BUDGET, Machine};
pub use source::{Options, Outcome, compile_source, run_source, run_source_with};

#[cfg(test)]
mod tests;
