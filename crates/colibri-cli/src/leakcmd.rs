//! `--leaks` for `colibri run` and `colibri test`: the leak detector's report (ARCHITECTURE §18).

use std::collections::HashSet;

use clap::ValueEnum;
use colibri_runtime as colibri;

/// What a leak does to the exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LeakMode {
    /// Print the report to stderr; the exit code is unchanged.
    Warn,
    /// Print it, and exit with 1 if the program would otherwise have succeeded.
    Error,
}

/// The leaks already reported, so a later look shows only the new ones (`colibri test` looks
/// after every test, and a leaked cycle stays leaked).
#[derive(Default)]
pub struct Seen(HashSet<String>);

impl Seen {
    /// The leaks of `vm` that no earlier call returned.
    pub fn new_leaks(&mut self, vm: &colibri::Vm) -> Vec<colibri::Leak> {
        vm.find_leaks()
            .into_iter()
            .filter(|l| self.0.insert(format!("{}|{}", l.path, l.sites.join(","))))
            .collect()
    }
}

/// The report of a set of leaks, ready for stderr; empty when there are none.
pub fn render(leaks: &[colibri::Leak], mode: LeakMode, color: bool) -> String {
    if leaks.is_empty() {
        return String::new();
    }
    let label = match (mode, color) {
        (LeakMode::Warn, true) => "\x1b[1;33mwarning\x1b[0m",
        (LeakMode::Warn, false) => "warning",
        (LeakMode::Error, true) => "\x1b[1;31merror\x1b[0m",
        (LeakMode::Error, false) => "error",
    };
    let n = leaks.len();
    let mut text = format!(
        "{label}: {n} leaked reference cycle{} (reference counting never frees a cycle; make one link `weak`)\n",
        if n == 1 { "" } else { "s" }
    );
    for leak in leaks {
        text.push_str(&format!("  {}\n", leak.to_string().replace('\n', "\n  ")));
    }
    text
}
