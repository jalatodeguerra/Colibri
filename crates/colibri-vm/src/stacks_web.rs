//! The web has no native stacks to park: corosensei does not target wasm32, so a host function
//! cannot suspend there, and a wait for a background operation blocks the thread (vm-plan
//! §3.2). Nothing in this file is unsafe, and `may_park` never holds, so only the names the VM
//! uses are here.
//!
//! Compiled for `target_family = "wasm"` (see `lib.rs`), and checked by the CI job that builds
//! the engine crates for `wasm32-unknown-unknown`. What a host function that wants to wait does
//! there: a wait for a background operation fails with `CannotStart` (the pool has no threads, see
//! `colibri_core::external`), so nothing needs to park.

use colibri_core::value::Value;

use crate::fiber::VmFiber;
use crate::host::{HostFault, HostImpl};
use crate::machine::Machine;

/// No fiber ever has a yielder here.
#[derive(Clone, Copy, Debug)]
pub(crate) enum YielderPtr {}

#[inline]
pub(crate) fn on_host_stack(_fiber: &VmFiber) -> bool {
    false
}

/// No host function parks here.
pub(crate) enum Parked {}

pub(crate) enum Step {
    Done(Result<Value, HostFault>),
    // Never built: its payload has no values.
    #[allow(dead_code)]
    Parked(Parked),
}

#[derive(Default)]
pub(crate) struct HostStacks(());

impl HostStacks {
    pub(crate) fn set_size(&mut self, _size: usize) {}
    pub(crate) fn allocated(&self) -> usize {
        0
    }
    pub(crate) fn pooled(&self) -> usize {
        0
    }
}

pub(crate) fn run(
    machine: &mut Machine,
    fiber: &mut VmFiber,
    imp: HostImpl,
    args: Vec<Value>,
) -> Step {
    Step::Done(imp(machine, fiber, args))
}

pub(crate) fn resume(_: &mut Machine, _: &mut VmFiber, parked: Parked, _: bool) -> Step {
    match parked {}
}

pub(crate) fn park(_: &mut Machine, _: &mut VmFiber) -> bool {
    unreachable!("a host function cannot park on the web")
}

// ---- the stack of the portable walks ----

/// The size of the stack a portable walk runs on (there is no separate stack here: the walk runs
/// on the caller's).
pub const WALK_STACK: usize = 64 * 1024 * 1024;
pub const SENDER_WALK_BUDGET: usize = 48 * 1024 * 1024;
pub const RECEIVER_EXTRA_BUDGET: usize = 64 * 1024;

/// Which end of a thread crossing a walk is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkRole {
    Sender,
    Receiver,
}

impl WalkRole {
    pub const fn budget(self) -> usize {
        match self {
            WalkRole::Sender => SENDER_WALK_BUDGET,
            WalkRole::Receiver => SENDER_WALK_BUDGET + RECEIVER_EXTRA_BUDGET,
        }
    }
}

/// Runs `walk` on the caller's stack, measured from here against the budget of `role`.
pub fn on_walk_stack<R>(role: WalkRole, walk: impl FnOnce(colibri_core::portable::StackGuard) -> R) -> R {
    walk(colibri_core::portable::StackGuard::here(role.budget()))
}

#[doc(hidden)]
pub fn walk_stacks_pooled() -> usize {
    0
}
