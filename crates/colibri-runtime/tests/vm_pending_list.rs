//! `vm_pending.txt` stays honest: every entry names a test that exists and wraps its body in
//! `vm_pending`, and carries a reason.

use std::fs;
use std::path::Path;

#[test]
fn every_entry_names_a_wrapped_test_and_gives_a_reason() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let list = fs::read_to_string(dir.join("vm_pending.txt")).expect("the list exists");
    let mut problems = Vec::new();
    let mut seen = Vec::new();
    for line in list.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, reason)) = line.split_once('#') else {
            problems.push(format!("no reason: {line}"));
            continue;
        };
        let (key, reason) = (key.trim(), reason.trim());
        if reason.is_empty() {
            problems.push(format!("no reason: {key}"));
        }
        if seen.contains(&key) {
            problems.push(format!("listed twice: {key}"));
        }
        seen.push(key);
        let Some((binary, test)) = key.split_once("::") else {
            problems.push(format!("not `binary::test`: {key}"));
            continue;
        };
        let name = test.rsplit("::").next().unwrap_or(test);
        let Ok(source) = fs::read_to_string(dir.join(format!("{binary}.rs"))) else {
            problems.push(format!("no test binary `{binary}`: {key}"));
            continue;
        };
        if !source.contains(&format!("fn {name}(")) {
            problems.push(format!("no test `{name}` in {binary}.rs: {key}"));
        }
        // rustfmt may break the call over lines.
        let squeezed: String = source.split_whitespace().collect();
        if !squeezed.contains(&format!("vm_pending(\"{test}\",")) {
            problems.push(format!(
                "`{name}` does not wrap its body in vm_pending: {key}"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
