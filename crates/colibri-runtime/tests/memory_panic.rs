//! A host function that panics, with `RUST_BACKTRACE` on, must not leave std's backtrace cache in
//! the account of the `Vm` that happened to panic first (phase 3 L13). Its own binary, so that
//! no other panic has run the hook before the variable is set.

use std::alloc::System;

use colibri_runtime as colibri;

#[global_allocator]
static ALLOC: colibri::CountingAlloc<System> = colibri::CountingAlloc(System);

#[test]
fn a_host_panic_with_a_backtrace_does_not_charge_the_vm_for_the_symbol_cache() {
    // SAFETY: the only test of this binary, so no other thread reads the environment meanwhile,
    // and the panic hook has not read the variable yet.
    unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    let mut vm = colibri::Vm::builder().build();
    vm.register_fn("boom", || -> i64 { panic!("a host function that fails") })
        .unwrap();
    vm.load_source("boom.colibri", "fn f() -> int => boom()\n")
        .unwrap();
    let f = vm.get_fn::<(), i64>("f").unwrap();
    let before = vm.memory_used().expect("installed");
    let err = f.call(&mut vm, ()).unwrap_err();
    assert!(matches!(err, colibri::Error::Panic { .. }), "{err:?}");
    drop(err);
    let after = vm.memory_used().expect("installed");
    eprintln!("before {before}, after {after}");
    // The cache is megabytes; what the panic itself holds is a few hundred bytes.
    assert!(after < before + 64 * 1024, "before {before}, after {after}");
}
