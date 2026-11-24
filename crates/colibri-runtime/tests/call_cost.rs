//! What it costs to call a Colibri function from Rust (`Fn::call`), per engine: DESIGN §17.4
//! asks for under 100 ns. The numbers go to stderr; nothing here asserts on a time, because a
//! shared CI machine cannot promise one. Run it where the figure matters:
//! `cargo test --release -p colibri-runtime --test call_cost -- --nocapture`.

use std::time::Instant;

use colibri_runtime as colibri;

fn ns_per_call(kind: colibri::EngineKind, args: &str, body: &str, calls: u32) -> (f64, i64) {
    let mut vm = colibri::Vm::builder().engine(kind).build();
    vm.load_source("a.colibri", &format!("fn f({args}) -> int => {body}\n"))
        .unwrap();
    let f = vm.get_fn::<(i64,), i64>("f").unwrap();
    let mut sum = 0;
    for i in 0..1000 {
        sum += f.call(&mut vm, (i,)).unwrap();
    }
    let started = Instant::now();
    for i in 0..i64::from(calls) {
        sum += f.call(&mut vm, (i,)).unwrap();
    }
    let ns = started.elapsed().as_nanos() as f64 / f64::from(calls);
    (ns, sum)
}

#[test]
fn calling_a_trivial_function_from_rust_costs() {
    let calls = if cfg!(debug_assertions) {
        20_000
    } else {
        2_000_000
    };
    for (name, kind) in [
        ("tree", colibri::EngineKind::Tree),
        ("vm", colibri::EngineKind::Bytecode),
    ] {
        let (ns, sum) = ns_per_call(kind, "x: int", "x + 1", calls);
        eprintln!(
            "Fn::call of `x + 1` on the {name} engine: {ns:.1} ns per call ({calls} calls, {} build)",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
        // The answers are what the function says (and keep the loop from being optimised away).
        assert!(sum > 0);
    }
}
