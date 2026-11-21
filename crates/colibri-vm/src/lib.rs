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

#![forbid(unsafe_code)]

pub mod bytecode;
mod compile;
mod engine;
mod fiber;
mod load;
mod machine;
mod run;
mod source;

pub use compile::{CompileEnv, compile_body};
pub use fiber::VmFiber;
pub use machine::{DEFAULT_NATIVE_STACK_BUDGET, Machine};
pub use source::{Options, Outcome, compile_source, run_source, run_source_with};

#[cfg(test)]
mod tests;
