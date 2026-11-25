//! Golden-test support: directive parsing and result comparison.
//!
//! Directives are recognised anywhere a `//` comment starts with the keyword, so a string
//! literal that contains `// expect:` on a line would be misread. Golden files avoid that.

use std::path::Path;

use colibri_runtime as colibri;

/// Directives parsed from one golden file.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Directives {
    /// `// expect:` lines in order; they form the whole expected stdout.
    pub expect: Vec<String>,
    /// `// error:` at the end of a source line: (1-based line, text).
    pub errors: Vec<(usize, String)>,
    /// `// warning:` at the end of a source line: a compile warning on that line whose message
    /// contains the text. Warnings are compared exactly: one that is not listed fails the file.
    pub warnings: Vec<(usize, String)>,
    /// `// panic:` message fragments; the program must panic and the message (with the notes
    /// of later panics, see `colibri_runtime::Error::Panic`) must contain every one.
    pub panic: Vec<String>,
    /// `// main-err:` message fragments; `main` must return `err(e)` and the printed error
    /// chain (`error: ...` on stderr, exit code 1 in the CLI) must contain every one.
    pub main_err: Vec<String>,
    /// `// drops:` the labels of the class objects destroyed, in order, until the program ends
    /// (module-level values included). Compared only when the directive is present;
    /// `// drops:` with nothing after it expects that no object is destroyed.
    pub drops: Option<Vec<String>>,
    /// `// leaks: Node, Node, closure x2`: the objects the leak detector finds in reference
    /// cycles that nothing reaches once the program has ended, as class names (`closure` for a
    /// closure), `name xN` standing for N of them. Compared as a multiset. Without the
    /// directive a golden test must not leak at all.
    pub leaks: Vec<String>,
    /// `// skip:` reason; the file is not run.
    pub skip: Option<String>,
    /// `// only: unix - why`: the file is run only on that kind of system (`unix` or
    /// `windows`) and is reported as not run (an ignored test, counted in the summary) on
    /// the others. Use it only where the behaviour under test is itself system-specific (a
    /// C function that one system does not have, a shell that is not there), never to hide
    /// a failure; where a golden mixes portable and system-specific parts, split it so the
    /// portable part runs everywhere. The reason after the platform is mandatory.
    pub only: Option<Only>,
    /// Directives that are written wrongly (an `// only:` without a reason, an unknown
    /// platform); the test fails with these, so nothing is silently dropped.
    pub malformed: Vec<String>,
    /// `// caps: fs, os`: the capabilities the test's `Vm` grants (the default is none). Read
    /// from the file that starts the program.
    pub caps: Vec<String>,
    /// `// features: net, audio`: the feature flags the test's `Vm` turns on for `@when`.
    /// Read from the file that starts the program.
    pub features: Vec<String>,
    /// `// allow: unused` in the comment lines at the top of the file: unused-variable,
    /// unused-import and unreachable-code warnings in this file need not be listed. Every other file must list them
    /// as `// warning:` or have none.
    pub allow_unused: bool,
}

/// A system a golden file is restricted to (`// only:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Unix,
    Windows,
}

impl Platform {
    /// The system this test binary was built for.
    pub fn current() -> Platform {
        if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Unix
        }
    }
}

/// The value of an `// only:` directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Only {
    pub platform: Platform,
    pub reason: String,
}

impl Directives {
    /// Why this file is not run on this system (`// skip:` on every system, `// only:` on
    /// the others), if it is not.
    pub fn not_run_here(&self) -> Option<String> {
        // A file with a malformed directive is run, so that the failure is reported (it would
        // otherwise be ignored for good, here or on the other system).
        if !self.malformed.is_empty() {
            return None;
        }
        if let Some(reason) = &self.skip {
            return Some(reason.clone());
        }
        match &self.only {
            Some(only) if only.platform != Platform::current() => {
                Some(format!("only on {:?}: {}", only.platform, only.reason))
            }
            _ => None,
        }
    }
}

/// Reads the value of `// only: unix - reason`.
fn parse_only(value: &str) -> Result<Only, String> {
    let value = value.trim();
    let (word, rest) = value.split_once(char::is_whitespace).unwrap_or((value, ""));
    let platform = match word.trim_end_matches([',', ':', '-']) {
        "unix" => Platform::Unix,
        "windows" => Platform::Windows,
        other => {
            return Err(format!(
                "`// only:` names the system `{other}`; write `unix` or `windows`, then a reason"
            ));
        }
    };
    let reason = rest.trim_start_matches([' ', '-', ':', ',']).trim();
    if reason.is_empty() {
        return Err(format!(
            "`// only: {word}` needs a reason after the system (`// only: {word} - why`)"
        ));
    }
    Ok(Only {
        platform,
        reason: reason.to_string(),
    })
}

/// Parses the directives of a golden file.
pub fn parse_directives(source: &str) -> Directives {
    let mut d = Directives::default();
    // Still in the leading run of blank and comment lines (where `// allow:` is read).
    let mut header = true;
    for (i, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if header && !trimmed.is_empty() && !trimmed.starts_with("//") {
            header = false;
        }
        // The first `//` followed by a known keyword wins, which lets a directive trail code.
        let mut rest = line;
        while let Some(pos) = rest.find("//") {
            let comment = rest[pos + 2..].trim_start();
            if let Some(v) = comment.strip_prefix("expect:") {
                d.expect
                    .push(v.strip_prefix(' ').unwrap_or(v).trim_end().to_string());
            } else if let Some(v) = comment.strip_prefix("error:") {
                d.errors.push((i + 1, v.trim().to_string()));
            } else if let Some(v) = comment.strip_prefix("warning:") {
                d.warnings.push((i + 1, v.trim().to_string()));
            } else if let Some(v) = comment.strip_prefix("panic:") {
                d.panic.push(v.trim().to_string());
            } else if let Some(v) = comment.strip_prefix("main-err:") {
                d.main_err.push(v.trim().to_string());
            } else if let Some(v) = comment.strip_prefix("drops:") {
                d.drops = Some(
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            } else if let Some(v) = comment.strip_prefix("leaks:") {
                d.leaks.extend(parse_leaks(v));
            } else if let Some(v) = comment.strip_prefix("allow:") {
                if header && v.trim() == "unused" {
                    d.allow_unused = true;
                }
            } else if let Some(v) = comment.strip_prefix("skip:") {
                d.skip = Some(v.trim().to_string());
            } else if let Some(v) = comment.strip_prefix("only:") {
                match parse_only(v) {
                    Ok(only) => d.only = Some(only),
                    Err(problem) => d.malformed.push(format!("line {}: {problem}", i + 1)),
                }
            } else if let Some(v) = comment.strip_prefix("features:") {
                // Names are separated by commas or spaces; each is a feature name as in
                // `colibri.toml`, so `// features: a b` is two features and a name that is not
                // one is a failure of the test.
                for name in v.split([',', ' ', '\t']).filter(|s| !s.is_empty()) {
                    match colibri::feature_name_problem(name) {
                        Some((problem, _)) => d
                            .malformed
                            .push(format!("line {}: `// features:` {problem}", i + 1)),
                        None => d.features.push(name.to_string()),
                    }
                }
            } else if let Some(v) = comment.strip_prefix("caps:") {
                d.caps.extend(
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            } else {
                rest = &rest[pos + 2..];
                continue;
            }
            break;
        }
    }
    d
}

/// The objects a `// leaks:` directive names: `Node, Node` or `Node x2`.
fn parse_leaks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let counted = item.rsplit_once(" x").and_then(|(name, n)| {
            let n = n.trim().parse::<usize>().ok()?;
            Some((name.trim(), n))
        });
        match counted {
            Some((name, n)) => out.extend(std::iter::repeat_n(name.to_string(), n)),
            None => out.push(item.to_string()),
        }
    }
    out
}

/// A position in a golden test: the file (relative to the test's folder; empty for a test that
/// is one file), the 1-based line and the message.
pub type Located = (String, usize, String);

/// What running a golden file produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The program compiled and ran; `panic` is the panic message if it ended in one.
    Compiled {
        stdout: String,
        panic: Option<String>,
        /// Where the panic happened and the call stack, innermost first, as `line` and
        /// `function line` strings (columns and files are left out). Empty without a panic.
        trace: Vec<String>,
        /// The error chain if `main` returned `err(e)` (`Error::MainReturnedErr`).
        main_err: Option<String>,
        /// The compile warnings.
        warnings: Vec<Located>,
        /// The labels of the class objects destroyed, in order, up to and including the
        /// release of the module-level values after `main` (DESIGN §11.2).
        drops: Vec<String>,
        /// The reference cycles left once the program ended and its module-level values
        /// were released (`Vm::find_leaks`).
        leaks: Vec<colibri::Leak>,
    },
    /// Compilation failed with these error diagnostics.
    CompileErrors {
        errors: Vec<Located>,
        warnings: Vec<Located>,
    },
}

/// Runs a golden file through the runtime in-process, with its standard output captured:
/// load the file, then call `main` if it has one. Run it on a thread with a big stack (see
/// [`crate::on_big_stack`]) so deep recursion reaches the call-depth limit.
pub fn run_golden(path: &Path, source: &str) -> Outcome {
    let name = path.display().to_string();
    let d = parse_directives(source);
    run_with(
        |vm| vm.load_source(&name, source),
        None,
        &name,
        &d.caps,
        &d.features,
    )
}

/// Runs a golden test that is a folder: the files in it are one program, started from
/// `main.colibri` (see `Vm::load_file`, which makes the folder its implicit package).
pub fn run_golden_package(dir: &Path) -> Outcome {
    let main = dir.join("main.colibri");
    let d = std::fs::read_to_string(&main)
        .map(|text| parse_directives(&text))
        .unwrap_or_default();
    run_with(
        |vm| vm.load_file(&main),
        Some(dir),
        "",
        &d.caps,
        &d.features,
    )
}

/// The standard library's view of the world in a golden test: a clock that only moves when the
/// program sleeps, a fixed environment, fixed arguments and input, a seeded generator and a
/// temporary folder of the test's own (made before it runs, removed after).
fn hermetic_std(temp: &Path) -> colibri_std::StdConfig {
    use std::collections::BTreeMap;
    colibri_std::StdConfig {
        clock: Box::new(colibri_std::ManualClock::new(1_700_000_000.0)),
        args: vec!["alpha".to_string(), "beta".to_string()],
        env: colibri_std::EnvSource::Fixed(BTreeMap::from([(
            "COLIBRI_GOLDEN".to_string(),
            "yes".to_string(),
        )])),
        temp_dir: Some(temp.to_path_buf()),
        seed: Some(1),
        stdin: Some(Box::new(std::io::Cursor::new("first line\nsecond line\n"))),
        // +05:30, so that local time and UTC differ in every golden that uses `datetime`.
        local_offset: Some(19_800),
    }
}

/// A new empty folder under the system's temporary folder, removed on drop.
struct TempFolder(std::path::PathBuf);

impl TempFolder {
    fn new() -> TempFolder {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "colibri-golden-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        TempFolder(dir)
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A clock that only moves when told to (by `sleep` or `testclock.advance`).
#[derive(Clone, Default)]
struct FakeClock(std::rc::Rc<std::cell::Cell<f64>>);

impl FakeClock {
    fn advance(&self, seconds: f64) {
        self.0.set(self.0.get() + seconds);
    }
}

impl colibri::Clock for FakeClock {
    fn now(&mut self) -> f64 {
        self.0.get()
    }
    fn sleep(&mut self, seconds: f64) {
        self.advance(seconds);
    }
}

/// How a source file is named in a golden test's results: relative to the test's folder and
/// with `/` on every system (the resolver builds the names of the files it reaches with the
/// system's own separator, and the folder itself may be spelled with both).
fn relative_name(dir: &Path, name: &str) -> String {
    match Path::new(name).strip_prefix(dir) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => name.to_string(),
    }
}

/// `COLIBRI_TEST_COUNT_RC=1` runs every golden with the reference-count counters compiled in
/// (`--profile-rc`), which must change nothing a golden checks; `0` or unset leaves them out.
/// Anything else is a mistake in the CI file and fails the run.
fn count_rc_forced() -> bool {
    match std::env::var("COLIBRI_TEST_COUNT_RC") {
        Err(_) => false,
        Ok(v) => match v.as_str() {
            "1" => true,
            "0" => false,
            _ => panic!("COLIBRI_TEST_COUNT_RC must be 0 or 1, not {v:?}"),
        },
    }
}

fn run_with(
    load: impl FnOnce(&mut colibri::Vm) -> Result<(), colibri::Error>,
    dir: Option<&Path>,
    blank: &str,
    caps: &[String],
    features: &[String],
) -> Outcome {
    let out = colibri::SharedBuffer::new();
    // A runaway program fails its test instead of hanging the run. In a debug build the VM
    // compiles the whole program at load, so a function that cannot be compiled fails its test
    // even if the run never calls it.
    let mut builder = crate::vm_builder()
        .instruction_budget(2_000_000_000)
        .compile_all_at_load(cfg!(debug_assertions))
        .trace_drops(true)
        .track_leaks(true)
        .stdout(Box::new(out.clone()));
    for feature in features {
        builder = builder.feature(feature);
    }
    for cap in caps {
        match colibri::Cap::ALL.into_iter().find(|c| c.name() == cap) {
            Some(cap) => builder = builder.allow(cap),
            None => panic!("`// caps:` names an unknown capability `{cap}`"),
        }
    }
    let temp = TempFolder::new();
    // A fake clock that never sleeps: the event loop jumps to the next due wake.
    let clock = FakeClock::default();
    let mut vm = builder.clock(clock.clone()).build();
    if count_rc_forced() {
        vm.start_rc_profile();
    }
    colibri_std::install_with(&mut vm, hermetic_std(&temp.0))
        .expect("the standard library registers into a new Vm");
    // `import testclock` gives `testclock.advance(seconds)`: moves the fake wall clock, which
    // `wait_real` and the event loop read.
    let driver = clock.clone();
    vm.module("testclock")
        .register_fn("advance", move |seconds: f64| driver.advance(seconds))
        .expect("the test clock registers into a new Vm")
        // It only moves a fake clock: the VM may call it beside running coroutines.
        .no_await();
    // After `main`, the program's coroutines run until none is left (what `colibri run` does).
    let result = load(&mut vm)
        .and_then(|()| vm.run_main())
        .and_then(|()| vm.run_event_loop());
    // Module-level values die after `main` returns (or after a panic), in reverse order.
    vm.shutdown();
    let drops = vm.take_drop_trace();
    let leaks = vm.find_leaks();
    let located = |sources: &colibri::SourceMap, diags: &[colibri::Diagnostic], errors: bool| {
        diags
            .iter()
            .filter(|d| d.is_error() == errors)
            .map(|d| {
                let line = sources.line_col(d.span.file, d.span.start).0;
                let file = sources.name(d.span.file);
                let file = match dir {
                    Some(dir) => relative_name(dir, file),
                    None => file.to_string(),
                };
                let file = if file == blank { String::new() } else { file };
                (file, line as usize, d.message.clone())
            })
            .collect::<Vec<_>>()
    };
    match result {
        Err(colibri::Error::Compile(report)) => Outcome::CompileErrors {
            errors: located(&report.sources, &report.diagnostics, true),
            warnings: located(&report.sources, &report.diagnostics, false),
        },
        other => {
            let mut main_err = None;
            let mut trace = Vec::new();
            let panic = match other {
                Ok(()) => None,
                Err(colibri::Error::MainReturnedErr { message }) => {
                    main_err = Some(message);
                    None
                }
                Err(colibri::Error::Panic {
                    message,
                    notes,
                    location,
                    stack,
                }) => {
                    trace.extend(location.iter().map(|l| format!("at line {}", l.line)));
                    trace.extend(
                        stack
                            .iter()
                            .map(|f| format!("{} line {}", f.function, f.location.line)),
                    );
                    // Notes ride along with the message so a golden file can match them.
                    Some(notes.iter().fold(message, |m, n| format!("{m}\nnote: {n}")))
                }
                Err(err) => Some(format!("host error: {err}")),
            };
            Outcome::Compiled {
                stdout: out.contents(),
                panic,
                trace,
                main_err,
                warnings: located(vm.sources(), vm.warnings(), false),
                drops,
                leaks,
            }
        }
    }
}

/// The directives of every file of a golden test that is a folder, `main.colibri` first and the
/// others in path order (the paths are relative to the folder, with `/`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PackageDirectives {
    pub files: Vec<(String, Directives)>,
}

impl PackageDirectives {
    /// Parses `(relative path, source)` pairs.
    pub fn parse(files: &[(String, String)]) -> PackageDirectives {
        let mut parsed: Vec<(String, Directives)> = files
            .iter()
            .map(|(path, source)| (path.clone(), parse_directives(source)))
            .collect();
        parsed.sort_by(|a, b| (a.0 != "main.colibri", &a.0).cmp(&(b.0 != "main.colibri", &b.0)));
        PackageDirectives { files: parsed }
    }

    /// The directives that apply to the run as a whole. `expect` lines of every file in order;
    /// `drops`, `panic`, `main-err`, `skip`, `only`, `caps` and `features` come from `main.colibri` alone.
    fn whole(&self) -> Directives {
        let mut d = Directives::default();
        for (path, files) in &self.files {
            d.expect.extend(files.expect.iter().cloned());
            if path == "main.colibri" || path.is_empty() {
                d.panic = files.panic.clone();
                d.main_err = files.main_err.clone();
                d.drops = files.drops.clone();
                d.leaks = files.leaks.clone();
                d.skip = files.skip.clone();
                d.only = files.only.clone();
            }
            if path == "main.colibri" || path.is_empty() {
                d.caps = files.caps.clone();
                d.features = files.features.clone();
            } else {
                // Every directive that is about the run as a whole counts in main.colibri
                // alone: say so instead of ignoring it.
                for (name, present) in [
                    ("only", files.only.is_some()),
                    ("caps", !files.caps.is_empty()),
                    ("features", !files.features.is_empty()),
                ] {
                    if present {
                        d.malformed.push(format!(
                            "{path}: `// {name}:` counts in main.colibri alone; it would be ignored here"
                        ));
                    }
                }
            }
            d.malformed.extend(files.malformed.iter().map(|m| {
                if path.is_empty() {
                    m.clone()
                } else {
                    format!("{path}: {m}")
                }
            }));
        }
        d
    }

    /// Why `main.colibri` keeps the test from running on this system (`// skip:` or
    /// `// only:`), if it does.
    pub fn not_run_here(&self) -> Option<String> {
        self.whole().not_run_here()
    }
}

/// Compares an outcome with the directives of a test that is one file; `Err` describes every
/// mismatch.
pub fn check(d: &Directives, outcome: &Outcome) -> Result<(), String> {
    let single = Directives {
        expect: d.expect.clone(),
        errors: d.errors.clone(),
        warnings: d.warnings.clone(),
        panic: d.panic.clone(),
        main_err: d.main_err.clone(),
        drops: d.drops.clone(),
        leaks: d.leaks.clone(),
        skip: d.skip.clone(),
        only: d.only.clone(),
        malformed: d.malformed.clone(),
        caps: d.caps.clone(),
        features: d.features.clone(),
        allow_unused: d.allow_unused,
    };
    check_package(
        &PackageDirectives {
            files: vec![(String::new(), single)],
        },
        outcome,
    )
}

/// Compares an outcome with the directives of every file of a test; `Err` describes every
/// mismatch.
pub fn check_package(p: &PackageDirectives, outcome: &Outcome) -> Result<(), String> {
    let d = p.whole();
    if !d.malformed.is_empty() {
        return Err(d.malformed.join("\n"));
    }
    let errors_wanted: Vec<Located> = p
        .files
        .iter()
        .flat_map(|(path, f)| f.errors.iter().map(|(l, t)| (path.clone(), *l, t.clone())))
        .collect();
    let warnings_wanted: Vec<Located> = p
        .files
        .iter()
        .flat_map(|(path, f)| {
            f.warnings
                .iter()
                .map(|(l, t)| (path.clone(), *l, t.clone()))
        })
        .collect();
    let at = |file: &str, line: usize| {
        if file.is_empty() {
            format!("line {line}")
        } else {
            format!("{file}:{line}")
        }
    };
    let allow_unused: Vec<&str> = p
        .files
        .iter()
        .filter(|(_, f)| f.allow_unused)
        .map(|(path, _)| path.as_str())
        .collect();
    match outcome {
        Outcome::CompileErrors { errors, warnings } => {
            let mut problems = Vec::new();
            for (file, line, text) in &errors_wanted {
                if !errors
                    .iter()
                    .any(|(f, l, m)| f == file && l == line && m.contains(text.as_str()))
                {
                    problems.push(format!(
                        "expected error on {} containing {text:?}",
                        at(file, *line)
                    ));
                }
            }
            for (file, line, msg) in errors {
                if !errors_wanted
                    .iter()
                    .any(|(f, l, t)| f == file && l == line && msg.contains(t.as_str()))
                {
                    problems.push(format!("unexpected error on {}: {msg}", at(file, *line)));
                }
            }
            problems.extend(compare_warnings(&warnings_wanted, &allow_unused, warnings));
            finish(problems)
        }
        Outcome::Compiled {
            stdout,
            panic,
            main_err,
            warnings,
            drops,
            leaks,
            ..
        } => {
            let mut problems = Vec::new();
            for (file, line, text) in &errors_wanted {
                problems.push(format!(
                    "expected error on {} containing {text:?}, but it compiled",
                    at(file, *line)
                ));
            }
            problems.extend(compare_warnings(&warnings_wanted, &allow_unused, warnings));
            let expected = d
                .expect
                .iter()
                .map(|l| format!("{l}\n"))
                .collect::<String>();
            if *stdout != expected {
                problems.push(format!(
                    "stdout mismatch\n--- expected\n{expected}--- actual\n{stdout}"
                ));
            }
            if let Some(want) = &d.drops
                && want != drops
            {
                problems.push(format!(
                    "drop order mismatch\n--- expected\n{}\n--- actual\n{}",
                    want.join(", "),
                    drops.join(", ")
                ));
            }
            let mut found: Vec<&str> = leaks
                .iter()
                .flat_map(|l| l.objects.iter().map(String::as_str))
                .collect();
            found.sort_unstable();
            let mut wanted: Vec<&str> = d.leaks.iter().map(String::as_str).collect();
            wanted.sort_unstable();
            if found != wanted {
                let reports: String = leaks.iter().map(|l| format!("\n{l}")).collect();
                problems.push(format!(
                    "leak mismatch: expected [{}], found [{}]{reports}",
                    wanted.join(", "),
                    found.join(", ")
                ));
            }
            match (d.main_err.is_empty(), main_err) {
                (false, Some(got)) => {
                    for want in d.main_err.iter().filter(|w| !got.contains(w.as_str())) {
                        problems.push(format!("main error {got:?} does not contain {want:?}"));
                    }
                }
                (false, None) => problems.push(format!(
                    "expected `main` to return an error containing {:?}",
                    d.main_err
                )),
                (true, Some(got)) => {
                    problems.push(format!("`main` returned an unexpected error: {got}"))
                }
                (true, None) => {}
            }
            match (d.panic.is_empty(), panic) {
                (false, Some(got)) => {
                    for want in d.panic.iter().filter(|w| !got.contains(w.as_str())) {
                        problems.push(format!("panic {got:?} does not contain {want:?}"));
                    }
                }
                (false, None) => {
                    problems.push(format!("expected a panic containing {:?}", d.panic))
                }
                (true, Some(got)) => problems.push(format!("unexpected panic: {got}")),
                (true, None) => {}
            }
            finish(problems)
        }
    }
}

/// Every expected warning must appear, and every warning must be expected. The unused-code
/// warnings (`unused variable`, `unused import`) of a file with `// allow: unused` are
/// tolerated when unlisted; a `// warning: unused variable` directive there must still match.
fn is_unused_code_lint(message: &str) -> bool {
    message.starts_with("unused variable")
        || message.starts_with("unused import")
        || message.starts_with("unreachable code")
}

fn compare_warnings(
    expected: &[Located],
    allow_unused: &[&str],
    actual: &[Located],
) -> Vec<String> {
    let at = |file: &str, line: usize| {
        if file.is_empty() {
            format!("line {line}")
        } else {
            format!("{file}:{line}")
        }
    };
    let mut problems = Vec::new();
    for (file, line, text) in expected {
        if !actual
            .iter()
            .any(|(f, l, m)| f == file && l == line && m.contains(text.as_str()))
        {
            problems.push(format!(
                "expected warning on {} containing {text:?}",
                at(file, *line)
            ));
        }
    }
    for (file, line, msg) in actual {
        if is_unused_code_lint(msg) && allow_unused.contains(&file.as_str()) {
            continue;
        }
        if !expected
            .iter()
            .any(|(f, l, t)| f == file && l == line && msg.contains(t.as_str()))
        {
            problems.push(format!("unexpected warning on {}: {msg}", at(file, *line)));
        }
    }
    problems
}

fn finish(problems: Vec<String>) -> Result<(), String> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled(stdout: &str, panic: Option<&str>) -> Outcome {
        Outcome::Compiled {
            stdout: stdout.into(),
            panic: panic.map(Into::into),
            trace: Vec::new(),
            main_err: None,
            warnings: Vec::new(),
            drops: Vec::new(),
            leaks: Vec::new(),
        }
    }

    fn dropped(labels: &[&str]) -> Outcome {
        Outcome::Compiled {
            stdout: String::new(),
            panic: None,
            trace: Vec::new(),
            main_err: None,
            warnings: Vec::new(),
            drops: labels.iter().map(|s| s.to_string()).collect(),
            leaks: Vec::new(),
        }
    }

    fn compiled_with(warnings: &[(usize, &str)]) -> Outcome {
        Outcome::Compiled {
            stdout: String::new(),
            panic: None,
            trace: Vec::new(),
            main_err: None,
            warnings: warnings
                .iter()
                .map(|&(l, m)| (String::new(), l, m.to_string()))
                .collect(),
            drops: Vec::new(),
            leaks: Vec::new(),
        }
    }

    #[test]
    fn parses_directives() {
        let src = "print(1) // expect: 1\n// expect:\n// expect: a b \nlet x = y // error: unknown name\nlet x = 2 // warning: shadows\n// panic: boom\n// main-err: bad\n// drops: a, b ,c\n// skip: later\n// plain comment\n";
        let d = parse_directives(src);
        assert_eq!(d.expect, ["1", "", "a b"]);
        assert_eq!(d.errors, [(4, "unknown name".to_string())]);
        assert_eq!(d.warnings, [(5, "shadows".to_string())]);
        assert_eq!(d.panic, ["boom"]);
        assert_eq!(d.main_err, ["bad"]);
        assert_eq!(d.drops, Some(vec!["a".into(), "b".into(), "c".into()]));
        assert_eq!(parse_directives("// drops:\n").drops, Some(Vec::new()));
        assert_eq!(parse_directives("print(1)\n").drops, None);
        assert_eq!(d.skip.as_deref(), Some("later"));
    }

    #[test]
    fn only_names_a_system_and_needs_a_reason() {
        let d = parse_directives("// only: unix - sh is not on Windows\nfn main() {}\n");
        let only = d.only.expect("parsed");
        assert_eq!(only.platform, Platform::Unix);
        assert_eq!(only.reason, "sh is not on Windows");
        let d = parse_directives("// only: windows: cmd\n");
        assert_eq!(d.only.expect("parsed").platform, Platform::Windows);
        for bad in ["// only: unix\n", "// only:\n", "// only: plan9 - no\n"] {
            let d = parse_directives(bad);
            assert!(d.only.is_none() && d.malformed.len() == 1, "{bad}");
            let outcome = compiled("", None);
            assert!(check(&d, &outcome).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_malformed_only_is_never_hidden_by_a_skip() {
        let d = parse_directives("// skip: later\n// only: unix\nfn main() {}\n");
        assert_eq!(d.malformed.len(), 1);
        // Run, not ignored, so that the check reports it.
        assert_eq!(d.not_run_here(), None);
        let outcome = Outcome::CompileErrors {
            errors: Vec::new(),
            warnings: Vec::new(),
        };
        assert!(check(&d, &outcome).is_err_and(|e| e.contains("needs a reason")));
    }

    #[test]
    fn an_only_outside_main_colibri_is_a_failure_not_silence() {
        let files = vec![
            ("main.colibri".to_string(), "fn main() {}\n".to_string()),
            ("m.colibri".to_string(), "// only: unix - sh\n".to_string()),
        ];
        let p = PackageDirectives::parse(&files);
        assert_eq!(p.not_run_here(), None);
        let malformed = p.whole().malformed;
        assert_eq!(malformed.len(), 1, "{malformed:?}");
        assert!(malformed[0].starts_with("m.colibri:") && malformed[0].contains("main.colibri"));
        let files = vec![(
            "main.colibri".to_string(),
            "// only: unix - sh\nfn main() {}\n".to_string(),
        )];
        assert!(
            PackageDirectives::parse(&files)
                .whole()
                .malformed
                .is_empty()
        );
    }

    #[test]
    fn caps_and_features_outside_main_colibri_are_a_failure_like_only() {
        for directive in ["// caps: fs", "// features: fast"] {
            let files = vec![
                ("main.colibri".to_string(), "fn main() {}\n".to_string()),
                ("m.colibri".to_string(), format!("{directive}\n")),
            ];
            let malformed = PackageDirectives::parse(&files).whole().malformed;
            assert_eq!(malformed.len(), 1, "{directive}: {malformed:?}");
            assert!(
                malformed[0].starts_with("m.colibri:") && malformed[0].contains("main.colibri"),
                "{malformed:?}"
            );
        }
        // In main.colibri they count, and reach the run.
        let files = vec![(
            "main.colibri".to_string(),
            "// caps: fs\n// features: fast\nfn main() {}\n".to_string(),
        )];
        let d = PackageDirectives::parse(&files).whole();
        assert!(d.malformed.is_empty(), "{:?}", d.malformed);
        assert_eq!(
            (d.caps, d.features),
            (vec!["fs".into()], vec!["fast".into()])
        );
    }

    #[test]
    fn feature_names_are_split_on_commas_or_spaces_and_checked() {
        let d = parse_directives("// features: a b, c,d\n");
        assert_eq!(d.features, ["a", "b", "c", "d"]);
        assert!(d.malformed.is_empty());
        for bad in ["a-b", "debug", "os", "1x!"] {
            let d = parse_directives(&format!("// features: fast {bad}\n"));
            assert_eq!(d.features, ["fast"], "{bad}");
            assert_eq!(d.malformed.len(), 1, "{bad}: {:?}", d.malformed);
            assert!(d.malformed[0].starts_with("line 1:"), "{:?}", d.malformed);
        }
    }

    #[test]
    fn only_skips_on_the_other_system_but_not_on_this_one() {
        let other = if cfg!(windows) { "unix" } else { "windows" };
        let here = if cfg!(windows) { "windows" } else { "unix" };
        let d = parse_directives(&format!("// only: {other} - because\n"));
        assert!(d.not_run_here().is_some_and(|r| r.contains("because")));
        let d = parse_directives(&format!("// only: {here} - because\n"));
        assert!(d.not_run_here().is_none());
        assert!(
            parse_directives("// skip: later\n")
                .not_run_here()
                .is_some()
        );
    }

    #[test]
    fn names_are_relative_to_the_folder_with_slashes() {
        let dir = Path::new("a/b/../../golden/mod");
        assert_eq!(
            relative_name(dir, "a/b/../../golden/mod/main.colibri"),
            "main.colibri"
        );
        assert_eq!(
            relative_name(dir, "a/b/../../golden/mod/sub/x.colibri"),
            "sub/x.colibri"
        );
        assert_eq!(
            relative_name(dir, "elsewhere/x.colibri"),
            "elsewhere/x.colibri"
        );
        // The resolver builds the names it reaches with the system's separator, so on Windows
        // the same folder is spelled with both.
        #[cfg(windows)]
        assert_eq!(
            relative_name(dir, r"a/b/../../golden\mod\sub\x.colibri"),
            "sub/x.colibri"
        );
    }

    #[test]
    fn leaks_directive_lists_classes_and_repeats() {
        let d = parse_directives("// leaks: Node, closure x2, Pair\n// leaks: Node\n");
        assert_eq!(d.leaks, ["Node", "closure", "closure", "Pair", "Node"]);
        assert!(parse_directives("print(1)\n").leaks.is_empty());
    }

    #[test]
    fn drops_are_compared_only_when_the_directive_is_present() {
        let d = parse_directives("// drops: a, b\n");
        assert!(check(&d, &dropped(&["a", "b"])).is_ok());
        assert!(check(&d, &dropped(&["b", "a"])).is_err());
        assert!(check(&d, &dropped(&["a"])).is_err());
        assert!(check(&d, &dropped(&[])).is_err());
        // Without the directive nothing is compared.
        assert!(check(&parse_directives(""), &dropped(&["a", "b"])).is_ok());
        // An empty directive expects that nothing is destroyed.
        let none = parse_directives("// drops:\n");
        assert!(check(&none, &dropped(&[])).is_ok());
        assert!(check(&none, &dropped(&["a"])).is_err());
    }

    #[test]
    fn every_panic_fragment_must_be_in_the_message() {
        let d = parse_directives("// panic: first\n// panic: note: second\n");
        assert_eq!(d.panic, ["first", "note: second"]);
        assert!(check(&d, &compiled("", Some("first\nnote: second"))).is_ok());
        assert!(check(&d, &compiled("", Some("first"))).is_err());
    }

    #[test]
    fn stdout_comparison() {
        let d = parse_directives("// expect: hi\n// expect:\n");
        assert!(check(&d, &compiled("hi\n\n", None)).is_ok());
        assert!(check(&d, &compiled("hi\n", None)).is_err());
    }

    #[test]
    fn panic_comparison() {
        let d = parse_directives("// panic: index out\n");
        assert!(check(&d, &compiled("", Some("index out of range"))).is_ok());
        assert!(check(&d, &compiled("", Some("other"))).is_err());
        assert!(check(&d, &compiled("", None)).is_err());
        assert!(check(&parse_directives(""), &compiled("", Some("x"))).is_err());
    }

    fn errors(errors: &[(usize, &str)]) -> Outcome {
        Outcome::CompileErrors {
            errors: errors
                .iter()
                .map(|&(l, m)| (String::new(), l, m.to_string()))
                .collect(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn error_comparison() {
        let d = parse_directives("a // error: unknown\nb\nc // error: type\n");
        let ok = errors(&[(1, "unknown name `a`"), (3, "type mismatch")]);
        assert!(check(&d, &ok).is_ok());
        let missing = errors(&[(1, "unknown name")]);
        assert!(check(&d, &missing).is_err());
        let extra = errors(&[(1, "unknown"), (3, "type"), (2, "surprise")]);
        assert!(check(&d, &extra).is_err());
        assert!(check(&d, &compiled("", None)).is_err());
    }

    #[test]
    fn warnings_must_match_exactly_on_a_compiled_program() {
        let d = parse_directives("let x = 1\nlet x = 2 // warning: shadows\n");
        assert!(check(&d, &compiled_with(&[(2, "`x` shadows an earlier binding")])).is_ok());
        // Missing, on the wrong line, and unexpected warnings all fail.
        assert!(check(&d, &compiled_with(&[])).is_err());
        assert!(check(&d, &compiled_with(&[(1, "`x` shadows")])).is_err());
        let extra = compiled_with(&[(2, "shadows"), (3, "something else")]);
        assert!(check(&d, &extra).is_err());
        // A file with no warning directive must not warn.
        assert!(check(&parse_directives(""), &compiled_with(&[(1, "w")])).is_err());
    }

    #[test]
    fn unlisted_unused_warnings_need_the_allow_directive() {
        let outcome = compiled_with(&[(2, "unused variable `a`")]);
        let plain = parse_directives("fn main() {\n    let a = 1\n}\n");
        assert!(check(&plain, &outcome).is_err());
        let allowed = parse_directives("// allow: unused\nfn main() {\n    let a = 1\n}\n");
        assert!(allowed.allow_unused);
        assert!(check(&allowed, &compiled_with(&[(3, "unused variable `a`")])).is_ok());
        // Only in the comment lines at the top.
        let late = parse_directives("fn main() {}\n// allow: unused\n");
        assert!(!late.allow_unused);
        // Other warnings are never tolerated.
        let shadow = compiled_with(&[(2, "shadowed")]);
        assert!(check(&allowed, &shadow).is_err());
    }

    #[test]
    fn warnings_are_also_compared_when_compilation_fails() {
        let d = parse_directives("a // error: bad\nb // warning: odd\n");
        let both = Outcome::CompileErrors {
            errors: vec![(String::new(), 1, "bad thing".into())],
            warnings: vec![(String::new(), 2, "odd thing".into())],
        };
        assert!(check(&d, &both).is_ok());
        let no_warning = errors(&[(1, "bad thing")]);
        assert!(check(&d, &no_warning).is_err());
    }
}
