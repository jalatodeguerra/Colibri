//! The fixture runner after its author changed a signature: `fixture.double` takes two numbers.
//! A fake cargo copies this one instead of the first when a test builds "the next version" of the
//! bindings. It holds the same bindings marker (the manifest is the same).

use colibri_lang::bindings::Binding;
use colibri_runtime as colibri;

#[colibri::export]
pub fn double(n: i64, times: i64) -> i64 {
    n * times
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

/// The same marker as `src/main.rs`.
#[used]
static BINDINGS_MARKER: [u8; 35] = *b"\x01colibri-bindings=864995e496153e0e\x02";

fn main() -> std::process::ExitCode {
    std::hint::black_box(&BINDINGS_MARKER);
    colibri_lang::runner_main(BINDINGS)
}
