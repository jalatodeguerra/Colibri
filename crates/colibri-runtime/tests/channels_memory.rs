//! Memory and channels (phase 3 L19, ARCHITECTURE §35.1, §36.5): a value that does not fit the
//! memory limit when it is copied to a channel, or when it is rebuilt from one, is the memory
//! limit of the program that sends or receives it, as for `task.run`. This binary installs the
//! counting allocator, as every host that counts memory must.

use std::alloc::System;
use std::io::Write;
use std::sync::{Arc, Mutex};

use colibri_runtime as colibri;

#[global_allocator]
static ALLOC: colibri::CountingAlloc<System> = colibri::CountingAlloc(System);

const MIB: u64 = 1 << 20;

#[derive(Clone, Default)]
struct Out(Arc<Mutex<Vec<u8>>>);

impl Out {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn vm_with(limit: u64, source: &str) -> (colibri::Vm, Out) {
    let out = Out::default();
    let mut vm = colibri::Vm::builder()
        .memory_limit(limit)
        .stdout_shared(Box::new(out.clone()))
        .build();
    vm.load_source("channels_memory.colibri", source).unwrap();
    (vm, out)
}

const BIG_LIST: &str = r#"
fn big() -> List[int] {
    let mut items: List[int] = []
    for i in 0..200_000 {
        items.push(i)
    }
    return items
}
"#;

/// The program of both tests: a list of 200,000 integers, copied to a channel and taken from it.
fn program() -> String {
    format!(
        r#"{BIG_LIST}
fn main() {{
    let ch = Channel[List[int]](capacity: 4)
    let items = big()
    print("made")
    ch.send(items.clone())
    print("sent")
    let b = ch.recv().expect("queued")
    print("received {{b.len()}}")
}}
"#
    )
}

#[test]
fn a_value_too_big_to_copy_into_a_channel_is_the_senders_memory_limit() {
    // The list and its copy do not fit in 8 MiB: the send is refused as the memory limit.
    let (mut vm, out) = vm_with(8 * MIB, &program());
    let err = vm.run_main().unwrap_err();
    assert!(
        matches!(err, colibri::Error::MemoryLimit { limit, .. } if limit == 8 * MIB),
        "{err:?}"
    );
    assert_eq!(out.text(), "made\n");
}

#[test]
fn a_value_that_does_not_fit_when_it_is_received_is_the_receivers_memory_limit() {
    // In 14 MiB the copy goes into the channel, and the receiver cannot hold a second one next to
    // it: the receive is refused as the memory limit, not as a panic about channels.
    let (mut vm, out) = vm_with(14 * MIB, &program());
    let err = vm.run_main().unwrap_err();
    assert!(
        matches!(err, colibri::Error::MemoryLimit { limit, .. } if limit == 14 * MIB),
        "{err:?}"
    );
    assert_eq!(out.text(), "made\nsent\n");
}

#[test]
fn the_same_program_runs_when_the_limit_has_room() {
    let (mut vm, out) = vm_with(64 * MIB, &program());
    vm.run_main().unwrap();
    assert_eq!(out.text(), "made\nsent\nreceived 200000\n");
    // What the channel held was freed with it.
    drop(vm);
}

#[test]
fn the_clone_that_send_was_given_is_not_made_twice_unless_the_program_has_a_clone_of_its_own() {
    // The same send fits in 14 MiB as the walk's own copy, and does not fit once a user `Clone`
    // in the program means the clone has to be made as well as the copy.
    let own = format!(
        "struct Unrelated {{ n: int }}\nimpl Clone for Unrelated {{\n    fn clone(self) -> Unrelated => Unrelated {{ n: self.n }}\n}}\n{}",
        program()
    );
    let (mut vm, out) = vm_with(14 * MIB, &own);
    let err = vm.run_main().unwrap_err();
    assert!(matches!(err, colibri::Error::MemoryLimit { .. }), "{err:?}");
    assert_eq!(out.text(), "made\n");
}
