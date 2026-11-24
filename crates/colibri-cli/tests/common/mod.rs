//! Shared by the test binaries that have tests the bytecode VM cannot pass yet.
//!
//! These tests run the `colibri` binary, which inherits `COLIBRI_ENGINE`, so `COLIBRI_ENGINE=vm
//! cargo test` runs the programs they spawn on the VM. A test that does not pass there
//! yet wraps its body in [`vm_pending`] and is listed, with the reason, in `vm_pending.txt`
//! (`binary::test  # reason`). On the VM such a test is expected to fail: it counts as passed
//! when it does, and the run fails when it passes (so the list only shrinks). On the tree
//! engine, or when the test is not listed, the body just runs.

#![allow(dead_code)]

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

/// What the VM prints when it cannot run a program yet.
const REFUSAL: &str = "the bytecode VM does not support";

/// Fails at once, with the text of `stderr`, when the VM refused the program. A test that
/// spawns `colibri` calls this on what the child printed, so the failure of a pending test
/// always carries the refusal (an exit code or an output that is merely wrong would not), and
/// the reason in `vm_pending.txt` is checked against it.
pub fn no_refusal(stderr: &str) {
    assert!(
        !stderr.contains(REFUSAL),
        "the program was refused: {stderr}"
    );
}

/// `out` itself, once its stderr is known not to be a refusal (see [`no_refusal`]).
pub fn checked(out: std::process::Output) -> std::process::Output {
    no_refusal(&String::from_utf8_lossy(&out.stderr));
    out
}

/// Refusals that server children printed on stderr, with the name of the test thread that
/// started the child: a server can start, listen, and refuse only when the first client comes,
/// so the test fails on a reset connection and the refusal is not in its own failure text.
static REFUSED: std::sync::Mutex<Vec<(Option<String>, String)>> = std::sync::Mutex::new(Vec::new());

/// The refusals printed so far by children of the test on this thread.
fn refusals_of_this_test() -> String {
    let me = std::thread::current().name().map(String::from);
    let all = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    all.iter()
        .filter(|(owner, _)| *owner == me)
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a server child has printed on stderr so far, read on a thread so the pipe never fills.
pub struct Tap(std::sync::Arc<(std::sync::Mutex<String>, std::sync::atomic::AtomicBool)>);

/// Starts reading the piped stderr of `child`.
pub fn tap_stderr(child: &mut std::process::Child) -> Tap {
    use std::io::Read;
    let shared = std::sync::Arc::new((
        std::sync::Mutex::new(String::new()),
        std::sync::atomic::AtomicBool::new(false),
    ));
    let mut err = child.stderr.take().expect("stderr is piped");
    let mine = std::sync::Arc::clone(&shared);
    let owner = std::thread::current().name().map(String::from);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = err.read(&mut buf) {
            if n == 0 {
                break;
            }
            let chunk = String::from_utf8_lossy(&buf[..n]);
            if chunk.contains(REFUSAL) {
                REFUSED
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((owner.clone(), chunk.to_string()));
            }
            mine.0.lock().unwrap().push_str(&chunk);
        }
        mine.1.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    Tap(shared)
}

impl Tap {
    /// The stderr so far; once the child has ended, waits a moment for the rest.
    pub fn text(&self) -> String {
        for _ in 0..100 {
            if self.0.1.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.0.0.lock().unwrap().clone()
    }

    /// The address a server announced as `listening on ADDR`. Anything else is a failure to
    /// start, and the failure shows what the server said on stderr.
    pub fn listening_addr(&self, first: &str) -> String {
        match first.trim().strip_prefix("listening on ") {
            Some(addr) => addr.to_string(),
            None => {
                let err = self.text();
                no_refusal(&err);
                panic!("unexpected first line {first:?}; stderr: {err}")
            }
        }
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
/// expected to fail with the VM's refusal of the listed feature (`the bytecode VM does not
/// support X yet`, on the stderr of the spawned program), and one that passes is an error (so
/// the list only shrinks). The spawn helpers of the tests pass what the program printed to
/// [`no_refusal`], so a refusal ends the test at once with its text in the panic.
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
            let refusal = format!("{REFUSAL} {feature} yet");
            let mut text = text_of(&*payload);
            // A server child may refuse a moment after the test saw its connection fail.
            for _ in 0..100 {
                if text.contains(&refusal) {
                    break;
                }
                text = format!("{}\n{}", text_of(&*payload), refusals_of_this_test());
                if text.contains(&refusal) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(
                text.contains(&refusal),
                "{key} is listed as pending ({reason}) but failed another way: {text}"
            );
            R::pending()
        }
    }
}
