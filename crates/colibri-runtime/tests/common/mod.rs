//! Shared by the test binaries that have tests the bytecode VM cannot pass yet.
//!
//! `COLIBRI_ENGINE=vm cargo test` runs every test on the VM. A test that does not pass there
//! yet wraps its body in [`vm_pending`] and is listed, with the reason, in `vm_pending.txt`
//! (`binary::test  # reason`). On the VM such a test is expected to fail: it counts as passed
//! when it does, and the run fails when it passes (so the list only shrinks). On the tree
//! engine, or when the test is not listed, the body just runs.

use std::panic::{AssertUnwindSafe, catch_unwind};

const LIST: &str = include_str!("../vm_pending.txt");

/// What a pending test returns instead of its result when its body failed.
pub trait Pending {
    fn pending() -> Self;
}

impl Pending for () {
    fn pending() {}
}

impl<E> Pending for Result<(), E> {
    fn pending() -> Self {
        Ok(())
    }
}

fn on_vm() -> bool {
    std::env::var("COLIBRI_ENGINE").is_ok_and(|v| v == "vm")
}

/// Whether `key` (`binary::test`) is on the pending list.
fn listed(key: &str) -> bool {
    LIST.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .any(|l| l == key)
}

/// Runs the body of the test named `name` (`binary::test`). Listed and on the VM: the body is
/// expected to panic, and a body that does not is an error.
pub fn vm_pending<R: Pending>(name: &str, body: impl FnOnce() -> R) -> R {
    // The test binary's own name comes first, so one list serves every binary.
    let key = format!("{}::{name}", env!("CARGO_CRATE_NAME"));
    if !on_vm() || !listed(&key) {
        return body();
    }
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(_) => panic!("{key} passes on the VM now but is listed in vm_pending.txt; remove it"),
        Err(_) => R::pending(),
    }
}
