use colibri_runtime as colibri;
use colibri::tasks::{Portable, TaskFailure};

#[test]
fn smoke() {
    let mut vm = colibri::Vm::builder().worker_threads(2).build();
    vm.load_source("a.colibri", "const K = 5\nlet mut hits = 0\nfn add(a: int, b: int) -> int { return a + b + K }\nfn peek() -> int { return hits }\nfn bump() { hits += 1 }\n").unwrap();
    let r = vm.submit_portable("add", vec![Portable::Int(1), Portable::Int(2)]).unwrap().wait();
    eprintln!("{r:?}");
    let r = vm.submit_portable("peek", vec![]).unwrap().wait();
    eprintln!("{r:?}");
    let r = vm.submit_portable("bump", vec![]).unwrap().wait();
    eprintln!("{r:?}");
    let _ = TaskFailure::Cancelled;
}
