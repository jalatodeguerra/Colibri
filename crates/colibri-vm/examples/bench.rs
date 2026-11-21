//! Runs one Colibri source file on the bytecode VM and prints what it printed, for the
//! benchmarks (the CLI is not wired to the VM yet): `bench <file.colibri>`.

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: bench <file.colibri>");
    let src = std::fs::read_to_string(&path).expect("readable file");
    let out = colibri_vm::run_source(&src);
    print!("{}", out.stdout);
    if let Some(p) = out.panic {
        eprintln!("panic: {p}");
        std::process::exit(1);
    }
}
