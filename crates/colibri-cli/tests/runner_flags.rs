//! The project runner's `run`, `test` and `repl` take the flags `colibri`'s do (`runner.rs` copies
//! them from `main.rs` until they share a module): a flag added to one and not the other fails here.

use std::collections::BTreeSet;
use std::process::Command;

/// What a command takes: its usage after the command name (the positionals), and each option as
/// its flag names and value name (`--leaks [<MODE>]`), without the help text or defaults.
fn shape(help: &str, name: &str) -> (String, BTreeSet<String>) {
    let usage = help
        .lines()
        .find_map(|l| l.strip_prefix("Usage: "))
        .expect("a usage line");
    let usage = usage.split_once(name).expect("the name").1.trim();
    let mut options = BTreeSet::new();
    for line in help.lines() {
        let line = line.trim_start();
        if !line.starts_with('-') {
            continue;
        }
        // `-h, --help`, `--feature <NAME>`: up to the first double space (the help text).
        let head = line.split("  ").next().unwrap().trim();
        options.insert(head.to_string());
    }
    (usage.to_string(), options)
}

#[test]
fn the_runner_takes_the_flags_colibri_takes() {
    for name in ["run", "test", "repl"] {
        let out = Command::new(env!("CARGO_BIN_EXE_colibri"))
            .args([name, "--help"])
            .output()
            .unwrap();
        let colibri = String::from_utf8_lossy(&out.stdout).into_owned();
        let runner = colibri_lang::runner_command_help(name).expect("the runner has the command");
        let (c_usage, c_options) = shape(&colibri, name);
        let (r_usage, r_options) = shape(&runner, name);
        assert!(
            c_options.len() > 3,
            "{name}: read no options from\n{colibri}"
        );
        assert_eq!(c_usage, r_usage, "{name}");
        assert_eq!(c_options, r_options, "{name}");
    }
}
