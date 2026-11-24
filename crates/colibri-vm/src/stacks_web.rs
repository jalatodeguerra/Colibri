//! The web has no native stacks to park: corosensei does not target wasm32, so a host function
//! cannot suspend there, and a wait for a background operation blocks the thread (vm-plan
//! §3.2). Nothing in this file is unsafe, and `may_park` never holds, so only the names the VM
//! uses are here. (Not built by the repository's CI: there is no wasm32 build yet.)

use colibri_core::value::Value;

use crate::fiber::VmFiber;
use crate::host::{HostFault, HostImpl};
use crate::machine::Machine;

pub(crate) const DEFAULT_HOST_STACK: usize = 1024 * 1024;

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
    Parked(Parked),
}

#[derive(Default)]
pub(crate) struct HostStacks;

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
