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
    /// `// skip:` reason; the file is not run.
    pub skip: Option<String>,
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
            } else if let Some(v) = comment.strip_prefix("skip:") {
                d.skip = Some(v.trim().to_string());
            } else {
                rest = &rest[pos + 2..];
                continue;
            }
            break;
        }
    }
    d
}

/// What running a golden file produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The program compiled and ran; `panic` is the panic message if it ended in one.
    Compiled {
        stdout: String,
        panic: Option<String>,
        /// The error chain if `main` returned `err(e)` (`Error::MainReturnedErr`).
        main_err: Option<String>,
        /// The compile warnings: (1-based line, message).
        warnings: Vec<(usize, String)>,
        /// The labels of the class objects destroyed, in order, up to and including the
        /// release of the module-level values after `main` (DESIGN §11.2).
        drops: Vec<String>,
    },
    /// Compilation failed with these error diagnostics: (1-based line, message).
    CompileErrors {
        errors: Vec<(usize, String)>,
        warnings: Vec<(usize, String)>,
    },
}

/// Runs a golden file through the runtime in-process, with its standard output captured:
/// load the file, then call `main` if it has one. Run it on a thread with a big stack (see
/// [`crate::on_big_stack`]) so deep recursion reaches the call-depth limit.
pub fn run_golden(path: &Path, source: &str) -> Outcome {
    let out = colibri::SharedBuffer::new();
    let mut vm = crate::vm_builder()
        .trace_drops(true)
        .stdout(Box::new(out.clone()))
        .build();
    let name = path.display().to_string();
    let result = vm.load_source(&name, source).and_then(|()| vm.run_main());
    // Module-level values die after `main` returns (or after a panic), in reverse order.
    vm.shutdown();
    let drops = vm.take_drop_trace();
    let located = |sources: &colibri::SourceMap, diags: &[colibri::Diagnostic], errors: bool| {
        diags
            .iter()
            .filter(|d| d.is_error() == errors)
            .map(|d| {
                let line = sources.line_col(d.span.file, d.span.start).0;
                (line as usize, d.message.clone())
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
            }
        }
    }
}

/// Compares an outcome with the directives; `Err` describes every mismatch.
pub fn check(d: &Directives, outcome: &Outcome) -> Result<(), String> {
    match outcome {
        Outcome::CompileErrors { errors, warnings } => {
            let mut problems = Vec::new();
            for (line, text) in &d.errors {
                if !errors
                    .iter()
                    .any(|(l, m)| l == line && m.contains(text.as_str()))
                {
                    problems.push(format!("expected error on line {line} containing {text:?}"));
                }
            }
            for (line, msg) in errors {
                if !d
                    .errors
                    .iter()
                    .any(|(l, t)| l == line && msg.contains(t.as_str()))
                {
                    problems.push(format!("unexpected error on line {line}: {msg}"));
                }
            }
            problems.extend(compare_warnings(&d.warnings, warnings));
            finish(problems)
        }
        Outcome::Compiled {
            stdout,
            panic,
            main_err,
            warnings,
            drops,
        } => {
            let mut problems = Vec::new();
            for (line, text) in &d.errors {
                problems.push(format!(
                    "expected error on line {line} containing {text:?}, but it compiled"
                ));
            }
            problems.extend(compare_warnings(&d.warnings, warnings));
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
fn compare_warnings(expected: &[(usize, String)], actual: &[(usize, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (line, text) in expected {
        if !actual
            .iter()
            .any(|(l, m)| l == line && m.contains(text.as_str()))
        {
            problems.push(format!(
                "expected warning on line {line} containing {text:?}"
            ));
        }
    }
    for (line, msg) in actual {
        if !expected
            .iter()
            .any(|(l, t)| l == line && msg.contains(t.as_str()))
        {
            problems.push(format!("unexpected warning on line {line}: {msg}"));
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
        }
    }

    fn dropped(labels: &[&str]) -> Outcome {
        Outcome::Compiled {
            stdout: String::new(),
            panic: None,
            main_err: None,
            warnings: Vec::new(),
            drops: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn compiled_with(warnings: &[(usize, &str)]) -> Outcome {
        Outcome::Compiled {
            stdout: String::new(),
            panic: None,
            main_err: None,
            warnings: warnings.iter().map(|&(l, m)| (l, m.to_string())).collect(),
            drops: Vec::new(),
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
            errors: errors.iter().map(|&(l, m)| (l, m.to_string())).collect(),
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
            errors: vec![(1, "bad thing".into())],
            warnings: vec![(2, "odd thing".into())],
        };
        assert!(check(&d, &both).is_ok());
        let no_warning = errors(&[(1, "bad thing")]);
        assert!(check(&d, &no_warning).is_err());
    }
}
