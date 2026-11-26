// Shared by `alloc_bench_plain` and `alloc_bench_counting`: runs the benchmark programs of
// `benchmarks/` in-process and prints the best of five wall times for each, in milliseconds.
//
//     cargo run --release -p colibri-runtime --example alloc_bench_plain -- benchmarks/fib.colibri
//     cargo run --release -p colibri-runtime --example alloc_bench_counting -- benchmarks/fib.colibri
//
// The two binaries differ only in the global allocator, so the difference is the cost of the
// counting allocator and of the accounting at the entries of a `Vm`.

use colibri_runtime as colibri;

fn main() {
    let probe = colibri::Vm::builder().build();
    let which = if probe.memory_used().is_some() { "counting" } else { "plain" };
    println!("{:<10} {:>10} {:>14}", "program", which, "memory_used");
    for path in std::env::args().skip(1) {
        let name = std::path::Path::new(&path)
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let mut best = u128::MAX;
        let mut used = None;
        for _ in 0..5 {
            let out = colibri::SharedBuffer::new();
            let mut vm = colibri::Vm::builder().stdout(Box::new(out)).build();
            vm.load_file(&path).expect("the benchmark loads");
            let start = std::time::Instant::now();
            vm.run_main().expect("the benchmark runs");
            best = best.min(start.elapsed().as_micros());
            used = vm.memory_used();
        }
        let used = used.map_or_else(|| "-".to_string(), |u| u.to_string());
        println!("{name:<10} {:>10.1} {used:>14}", best as f64 / 1000.0);
    }
}
