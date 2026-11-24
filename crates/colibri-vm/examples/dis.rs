fn main() {
    let src = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let only = std::env::args().nth(2);
    for (n, d) in colibri_vm::compile_source(&src).unwrap() {
        if only.as_deref().is_none_or(|o| n.contains(o)) {
            println!("== {n}\n{d}");
        }
    }
}
