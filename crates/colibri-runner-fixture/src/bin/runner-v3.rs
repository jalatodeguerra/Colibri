//! The fixture runner after its author changed what `double` does (not its signature): `double`
//! triples. A watch test swaps this one in for the first when "the Rust code changed".

use colibri_lang::bindings::Binding;
use colibri_runtime as colibri;

#[colibri::export]
pub fn double(n: i64) -> i64 {
    n * 3
}

#[colibri::module]
fn fixture(m: &mut colibri::Module<'_>) -> Result<(), colibri::Error> {
    m.add_fn(double::export)?;
    Ok(())
}

static BINDINGS: &[Binding] = &[Binding {
    name: "fixture",
    register: __colibri_binding::register,
}];

fn main() -> std::process::ExitCode {
    colibri_lang::runner_main(BINDINGS)
}
