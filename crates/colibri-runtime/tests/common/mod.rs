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
    /// Whether this result is a failure (a body that returns `Err` rather than panicking).
    fn is_failure(&self) -> bool;
}

impl Pending for () {
    fn pending() {}
    fn is_failure(&self) -> bool {
        false
    }
}

impl<E> Pending for Result<(), E> {
    fn pending() -> Self {
        Ok(())
    }
    fn is_failure(&self) -> bool {
        self.is_err()
    }
}

fn on_vm() -> bool {
    std::env::var("COLIBRI_ENGINE").is_ok_and(|v| v == "vm")
}

/// The reason listed for `key` (`binary::test`), if it is on the pending list.
fn listed(key: &str) -> Option<&'static str> {
    LIST.lines().find_map(|l| {
        let (name, reason) = l.split_once('#')?;
        (name.trim() == key).then_some(reason.trim())
    })
}

/// The text of a caught panic.
fn text_of(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}

/// Runs the body of the test named `name` (`binary::test`). Listed and on the VM: the body is
/// expected to panic with the VM's refusal of the listed feature (`the bytecode VM does not
/// support X yet`), and anything else is an error: a body that passes, or fails for another
/// reason such as a crash inside the VM or a wrong answer.
pub fn vm_pending<R: Pending>(name: &str, body: impl FnOnce() -> R) -> R {
    // The test binary's own name comes first, so one list serves every binary.
    let key = format!("{}::{name}", env!("CARGO_CRATE_NAME"));
    let reason = match listed(&key) {
        Some(reason) if on_vm() => reason,
        _ => return body(),
    };
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(r) if r.is_failure() => R::pending(),
        Ok(_) => panic!("{key} passes on the VM now but is listed in vm_pending.txt; remove it"),
        Err(payload) => {
            // `enum values (test wrapper)`: the part in brackets is a note.
            let feature = reason.split(" (").next().unwrap_or(reason);
            let refusal = format!("the bytecode VM does not support {feature}");
            let text = text_of(&*payload);
            assert!(
                text.contains(&refusal),
                "{key} is listed as pending ({reason}) but failed another way: {text}"
            );
            R::pending()
        }
    }
}
