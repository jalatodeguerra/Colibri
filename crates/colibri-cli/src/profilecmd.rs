//! `--profile`: starts the sampling profiler of the `Vm`, and writes what it found: the folded
//! stacks, the flame graph and a short table of the heaviest functions and lines.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use colibri::{Profile, ProfileWeight};
use colibri_runtime as colibri;

use crate::flame;

/// How many rows each table has.
const TOP: usize = 20;

/// The file the flame graph goes to when `--profile` names none.
pub const DEFAULT_FILE: &str = "profile.svg";

/// What `--profile-weight` can be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Weight {
    /// Real time, so time in host functions and long built-ins counts.
    Time,
    /// Steps, the same on every run of the same program.
    Steps,
}

/// What `--profile` asked for.
#[derive(Clone, Debug)]
pub struct Options {
    /// The flame graph; the folded stacks go next to it.
    pub svg: PathBuf,
    pub weight: Weight,
}

impl Options {
    /// An empty file name means the default one (`--profile` without `=FILE`).
    pub fn new(file: &Path, weight: Weight) -> Options {
        let svg = if file.as_os_str().is_empty() {
            PathBuf::from(DEFAULT_FILE)
        } else if file.extension().is_none() {
            // `--profile=out` is `out.svg` and `out.folded`, not a file named `out`.
            file.with_file_name(format!(
                "{}.svg",
                file.file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            ))
        } else {
            file.to_path_buf()
        };
        Options { svg, weight }
    }

    /// Where the folded stacks go: the graph's name with `.folded`. If the file asked for
    /// already ends in `.folded` the graph is written beside it as `.svg`.
    pub fn folded_path(&self) -> PathBuf {
        self.svg.with_extension("folded")
    }

    fn svg_path(&self) -> PathBuf {
        if self.svg.extension().is_some_and(|e| e == "folded") {
            self.svg.with_extension("svg")
        } else {
            self.svg.clone()
        }
    }
}

/// Starts profiling in `vm`.
pub fn start(vm: &mut colibri::Vm, options: &Options) {
    vm.start_profile(match options.weight {
        Weight::Time => ProfileWeight::Time,
        Weight::Steps => ProfileWeight::Steps,
    });
}

/// Stops profiling, writes the files and prints the tables to stderr. A file that cannot be
/// written is an error line; the tables are printed anyway.
pub fn finish(vm: &mut colibri::Vm, options: &Options) {
    let Some(profile) = vm.finish_profile() else {
        return;
    };
    let folded = options.folded_path();
    let svg = options.svg_path();
    let units = units(profile.weight);
    let stacks = profile.stacks();
    let title = format!(
        "Colibri profile ({}, {})",
        match profile.weight {
            ProfileWeight::Steps => "steps",
            ProfileWeight::Time => "time",
        },
        flame_total(&profile, &units)
    );
    let mut notes = Vec::new();
    for (path, text) in [
        (&folded, profile.folded()),
        (&svg, flame::render(&title, &stacks, &units)),
    ] {
        if let Err(e) = std::fs::write(path, text) {
            eprintln!("error: cannot write {}: {e}", path.display());
        } else {
            notes.push(path.display().to_string());
        }
    }
    eprint!("{}", table(&profile, &notes));
}

fn units(weight: ProfileWeight) -> flame::Units {
    match weight {
        ProfileWeight::Steps => flame::Units {
            unit: "steps",
            divisor: 1.0,
        },
        ProfileWeight::Time => flame::Units {
            unit: "ms",
            divisor: 1e6,
        },
    }
}

fn flame_total(profile: &Profile, units: &flame::Units) -> String {
    let total = profile.total();
    if units.divisor == 1.0 {
        format!("{total} {}", units.unit)
    } else {
        format!("{:.1} {}", total as f64 / units.divisor, units.unit)
    }
}

/// The report: the heaviest functions (by their own weight and with their callees) and the
/// heaviest lines.
pub fn table(profile: &Profile, written: &[String]) -> String {
    let total = profile.total();
    let mut out = String::new();
    let units = units(profile.weight);
    let _ = writeln!(out, "profile: {}", flame_total(profile, &units));
    if !written.is_empty() {
        let _ = writeln!(out, "written: {}", written.join(", "));
    }
    if total == 0 {
        return out;
    }
    let percent = |w: u64| w as f64 / total as f64 * 100.0;

    let mut selfs: HashMap<&str, u64> = HashMap::new();
    let mut totals: HashMap<&str, u64> = HashMap::new();
    let mut lines: HashMap<(&str, u32), u64> = HashMap::new();
    for s in &profile.samples {
        if let Some(inner) = s.stack.last() {
            *selfs.entry(inner).or_insert(0) += s.weight;
        }
        // A function that is in the stack twice (recursion) counts once.
        let mut seen: Vec<&str> = Vec::new();
        for name in &s.stack {
            if !seen.contains(&name.as_str()) {
                seen.push(name);
                *totals.entry(name).or_insert(0) += s.weight;
            }
        }
        if s.line > 0 {
            *lines.entry((&s.file, s.line)).or_insert(0) += s.weight;
        }
    }
    let mut functions: Vec<(&str, u64)> = selfs.iter().map(|(n, w)| (*n, *w)).collect();
    functions.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let _ = writeln!(out, "\n  self   total  function");
    for (name, own) in functions.iter().take(TOP) {
        let _ = writeln!(
            out,
            "{:>5.1}%  {:>5.1}%  {name}",
            percent(*own),
            percent(totals[name])
        );
    }
    let mut by_line: Vec<((&str, u32), u64)> = lines.into_iter().collect();
    by_line.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    if !by_line.is_empty() {
        let _ = writeln!(out, "\n  self  line");
        for ((file, line), w) in by_line.iter().take(TOP) {
            let _ = writeln!(out, "{:>5.1}%  {file}:{line}", percent(*w));
        }
    }
    out
}
