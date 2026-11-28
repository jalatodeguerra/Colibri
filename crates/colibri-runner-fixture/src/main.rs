//! A project runner built without cargo at test time: what a generated `src/main.rs` is, with one
//! test binding (`import fixture`). The tests of the runner (and of the steps that delegate to
//! one) run this binary instead of building a runner.

use colibri_lang::bindings::Binding;
use colibri_runtime as colibri;

#[colibri::export]
pub struct Counter {
    n: i64,
}

#[colibri::export]
impl Counter {
    pub fn new(start: i64) -> Counter {
        Counter { n: start }
    }

    pub fn bump(&mut self, by: i64) -> i64 {
        self.n += by;
        self.n
    }
}

#[colibri::export]
pub fn double(n: i64) -> i64 {
    n * 2
}

#[colibri::export]
pub fn shout(text: &str) -> String {
    text.to_uppercase()
}

/// A value of `std.json`'s `Value`, passed through: a binding may use a standard type.
struct JsonValue(colibri::Value);

impl colibri::FromColibri for JsonValue {
    fn ty(reg: &mut colibri::TypeRegistry) -> colibri::TypeDesc {
        reg.foreign_type("std.json", "Value")
    }
    fn from_colibri(v: &colibri::Value, _cx: &mut colibri::Cx<'_>) -> Result<Self, colibri::Error> {
        Ok(JsonValue(v.clone()))
    }
}

impl colibri::IntoColibri for JsonValue {
    fn ty(reg: &mut colibri::TypeRegistry) -> colibri::TypeDesc {
        reg.foreign_type("std.json", "Value")
    }
    fn into_colibri(self, _cx: &mut colibri::Cx<'_>) -> colibri::Value {
        self.0
    }
}

#[colibri::module]
fn fixture(m: &mut colibri::Module<'_>) -> Result<(), colibri::Error> {
    m.uses("std.json")?;
    m.register_fn("same", |j: JsonValue| j)?;
    m.add_class::<Counter>()?;
    m.add_fn(double::export)?;
    m.add_fn(shout::export)?;
    Ok(())
}

static BINDINGS: &[Binding] = &[Binding {
    name: "fixture",
    register: __colibri_binding::register,
}];

/// What a generated runner holds for these bindings (`bindings_marker`): the hash of the manifest
/// line `fixture = { binding = "colibri-fixture", version = "0.1" }` that the delegation tests'
/// package declares, so `--runner` accepts this binary for that package and refuses it for others.
/// (The test `the_fixture_marker_is_the_one_of_its_package` says the value if it is out of date.)
#[used]
static BINDINGS_MARKER: [u8; 35] = *b"\x01colibri-bindings=864995e496153e0e\x02";

fn main() -> std::process::ExitCode {
    std::hint::black_box(&BINDINGS_MARKER);
    colibri_lang::runner_main(BINDINGS)
}
