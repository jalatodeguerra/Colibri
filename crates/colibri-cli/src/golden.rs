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
    /// `// caps: fs, os`: the capabilities the test's `Vm` grants (the default is none). Read
    /// from the file that starts the program.
    pub caps: Vec<String>,
}

/// Parses the directives of a golden file.
pub fn parse_directives(source: &str) -> Directives {
    let mut d = Directives::default();
    for (i, line) in source.lines().enumerate() {
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
            } else if let Some(v) = comment.strip_prefix("skip:") {
                d.skip = Some(v.trim().to_string());
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
    let caps = parse_directives(source).caps;
    run_with(|vm| vm.load_source(&name, source), "", &name, &caps)
}

/// Runs a golden test that is a folder: the files in it are one program, started from
/// `main.colibri` (see `Vm::load_file`, which makes the folder its implicit package).
pub fn run_golden_package(dir: &Path) -> Outcome {
    let main = dir.join("main.colibri");
    let prefix = format!("{}/", dir.display());
    let caps = std::fs::read_to_string(&main)
        .map(|text| parse_directives(&text).caps)
        .unwrap_or_default();
    run_with(|vm| vm.load_file(&main), &prefix, "", &caps)
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

fn run_with(
    load: impl FnOnce(&mut colibri::Vm) -> Result<(), colibri::Error>,
    prefix: &str,
    blank: &str,
    caps: &[String],
) -> Outcome {
    let out = colibri::SharedBuffer::new();
    let mut builder = crate::vm_builder()
        .trace_drops(true)
        .track_leaks(true)
        .stdout(Box::new(out.clone()));
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
    colibri_std::install_with(&mut vm, hermetic_std(&temp.0))
        .expect("the standard library registers into a new Vm");
    // `import testclock` gives `testclock.advance(seconds)`: moves the fake wall clock, which
    // `wait_real` and the event loop read.
    let driver = clock.clone();
    vm.module("testclock")
        .register_fn("advance", move |seconds: f64| driver.advance(seconds))
        .expect("the test clock registers into a new Vm");
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
                let file = file.strip_prefix(prefix).unwrap_or(file);
                let file = if file == blank { "" } else { file };
                (file.to_string(), line as usize, d.message.clone())
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
            let panic = match other {
                Ok(()) => None,
                Err(colibri::Error::MainReturnedErr { message }) => {
                    main_err = Some(message);
                    None
                }
                Err(colibri::Error::Panic { message, notes, .. }) => {
                    // Notes ride along with the message so a golden file can match them.
                    Some(notes.iter().fold(message, |m, n| format!("{m}\nnote: {n}")))
                }
                Err(err) => Some(format!("host error: {err}")),
            };
            Outcome::Compiled {
                stdout: out.contents(),
                panic,
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
    /// `drops`, `panic`, `main-err` and `skip` come from `main.colibri` alone.
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
            }
        }
        d
    }

    /// Whether `main.colibri` says to skip the test.
    pub fn skip(&self) -> Option<String> {
        self.whole().skip
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
        caps: d.caps.clone(),
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
            problems.extend(compare_warnings(&warnings_wanted, warnings));
            finish(problems)
        }
        Outcome::Compiled {
            stdout,
            panic,
            main_err,
            warnings,
            drops,
            leaks,
        } => {
            let mut problems = Vec::new();
            for (file, line, text) in &errors_wanted {
                problems.push(format!(
                    "expected error on {} containing {text:?}, but it compiled",
                    at(file, *line)
                ));
            }
            problems.extend(compare_warnings(&warnings_wanted, warnings));
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

/// Every expected warning must appear, and every warning must be expected.
fn compare_warnings(expected: &[Located], actual: &[Located]) -> Vec<String> {
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
