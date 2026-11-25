//! Writes the programs of the web test harness as packs: for each program of `web-goldens.txt`
//! and of `web-tests/`, `NAME.pack` (what the runner loads) and `NAME.out` (what it must print).
//!
//! ```text
//! cargo run -p colibri-web --example make_packs -- OUT_DIR
//! ```
//!
//! `js/test/run.mjs` runs `colibri.js` on them (see the notes in that file).

#[path = "../tests/support/mod.rs"]
mod support;

fn main() {
    let out = std::env::args().nth(1).expect("usage: make_packs OUT_DIR");
    let out = std::path::Path::new(&out);
    std::fs::create_dir_all(out).expect("create the output folder");
    for (name, source) in support::cases() {
        std::fs::write(out.join(format!("{name}.pack")), support::pack_of(&name, &source))
            .expect("write a pack");
        std::fs::write(out.join(format!("{name}.out")), support::expected_output(&source))
            .expect("write the expected output");
        // Programs that must also take a given time on the clock of the page: see `web-tests`.
        if let Some(ms) = support::virtual_ms(&source) {
            std::fs::write(out.join(format!("{name}.ms")), ms.to_string()).expect("write");
        }
        if let Some(code) = support::web_exit(&source) {
            std::fs::write(out.join(format!("{name}.exit")), code.to_string()).expect("write");
        }
        if let Some(text) = support::web_stderr(&source) {
            std::fs::write(out.join(format!("{name}.err")), text).expect("write");
        }
    }
}
