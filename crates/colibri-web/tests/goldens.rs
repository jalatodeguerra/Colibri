//! The programs of the web harness (`web-goldens.txt`, `web-tests/`) through the runner, natively:
//! what `js/test/run.mjs` does in node with the wasm build, without the wasm.

#[path = "support/driver.rs"]
mod driver;
mod support;

use colibri_web::State;

#[test]
fn every_web_golden_prints_what_its_expect_lines_say() {
    let mut failures = Vec::new();
    let cases = support::cases();
    assert!(cases.len() >= 8, "the harness runs a few programs");
    for (name, source) in &cases {
        let out = driver::run_pack(&support::pack_of(name, source));
        let want = support::expected_output(source);
        let (state, code) = match support::web_exit(source) {
            None => (State::Done, 0),
            Some(code) => (State::Failed, code),
        };
        let mut problems = Vec::new();
        if out.state != state || out.exit_code != code {
            problems.push(format!(
                "ended {:?} with {}, wanted {state:?} with {code}",
                out.state, out.exit_code
            ));
        }
        if out.out.stdout != want {
            problems.push(format!("printed:\n{}wanted:\n{want}", out.out.stdout));
        }
        if let Some(text) = support::web_stderr(source)
            && !out.out.stderr.contains(&text)
        {
            problems.push(format!(
                "the error stream lacks {text:?}: {}",
                out.out.stderr
            ));
        }
        if let Some(ms) = support::virtual_ms(source) {
            let ms = ms as f64;
            if out.elapsed_ms < ms || out.elapsed_ms > ms + 3.0 * driver::FRAME_MS {
                problems.push(format!(
                    "took {} ms of the page's clock, wanted about {ms}",
                    out.elapsed_ms
                ));
            }
        }
        if !problems.is_empty() {
            failures.push(format!(
                "{name}: {}\n--- stderr\n{}",
                problems.join("\n"),
                out.out.stderr
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A sweep over a list of goldens (`COLIBRI_WEB_SWEEP=file`, one path below `tests/golden` per
/// line): which of them the runner does not run as the native test does. For finding what to put
/// in `web-goldens.txt`; it checks nothing when the variable is not set.
#[test]
fn sweep() {
    let Ok(list) = std::env::var("COLIBRI_WEB_SWEEP") else {
        return;
    };
    let list = std::fs::read_to_string(list).expect("the list");
    let (mut pass, mut fail) = (0, 0);
    for line in list.lines().filter(|l| !l.trim().is_empty()) {
        let source = std::fs::read_to_string(support::golden_dir().join(line)).expect("read");
        let name = line.replace('/', "__");
        let want = support::expected_output(&source);
        let result =
            std::panic::catch_unwind(|| driver::run_pack(&support::pack_of(&name, &source)));
        match result {
            Ok(out) if out.state == State::Done && out.out.stdout == want => pass += 1,
            Ok(out) => {
                fail += 1;
                eprintln!(
                    "FAIL {line}: {:?} {}",
                    out.state,
                    out.out.stderr.lines().next().unwrap_or("")
                );
            }
            Err(_) => {
                fail += 1;
                eprintln!("CRASH {line}");
            }
        }
    }
    eprintln!("sweep: {pass} pass, {fail} fail");
}
