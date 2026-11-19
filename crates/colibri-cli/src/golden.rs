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
    /// `// panic:` message fragment.
    pub panic: Option<String>,
    /// `// drops:` expected drop order (not checked yet).
    pub drops: Vec<String>,
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
            } else if let Some(v) = comment.strip_prefix("panic:") {
                d.panic = Some(v.trim().to_string());
            } else if let Some(v) = comment.strip_prefix("drops:") {
                d.drops = v
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
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
    },
    /// Compilation failed with these error diagnostics: (1-based line, message).
    CompileErrors(Vec<(usize, String)>),
}

/// Runs a golden file through the runtime in-process, with its standard output captured:
/// load the file, then call `main` if it has one. Run it on a thread with a big stack (see
/// [`crate::on_big_stack`]) so deep recursion reaches the call-depth limit.
pub fn run_golden(path: &Path, source: &str) -> Outcome {
    let out = colibri::SharedBuffer::new();
    let mut vm = crate::vm_builder().stdout(Box::new(out.clone())).build();
    let name = path.display().to_string();
    match vm.load_source(&name, source).and_then(|()| vm.run_main()) {
        Ok(()) => Outcome::Compiled {
            stdout: out.contents(),
            panic: None,
        },
        Err(colibri::Error::Compile(report)) => Outcome::CompileErrors(
            report
                .diagnostics
                .iter()
                .filter(|d| d.is_error())
                .map(|d| {
                    let line = report.sources.line_col(d.span.file, d.span.start).0;
                    (line as usize, d.message.clone())
                })
                .collect(),
        ),
        Err(colibri::Error::Panic { message, .. }) => Outcome::Compiled {
            stdout: out.contents(),
            panic: Some(message),
        },
        Err(other) => Outcome::Compiled {
            stdout: out.contents(),
            panic: Some(format!("host error: {other}")),
        },
    }
}

/// Compares an outcome with the directives; `Err` describes every mismatch.
pub fn check(d: &Directives, outcome: &Outcome) -> Result<(), String> {
    match outcome {
        Outcome::CompileErrors(actual) => {
            let mut problems = Vec::new();
            for (line, text) in &d.errors {
                if !actual
                    .iter()
                    .any(|(l, m)| l == line && m.contains(text.as_str()))
                {
                    problems.push(format!("expected error on line {line} containing {text:?}"));
                }
            }
            for (line, msg) in actual {
                if !d
                    .errors
                    .iter()
                    .any(|(l, t)| l == line && msg.contains(t.as_str()))
                {
                    problems.push(format!("unexpected error on line {line}: {msg}"));
                }
            }
            finish(problems)
        }
        Outcome::Compiled { stdout, panic } => {
            let mut problems = Vec::new();
            for (line, text) in &d.errors {
                problems.push(format!(
                    "expected error on line {line} containing {text:?}, but it compiled"
                ));
            }
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
            match (&d.panic, panic) {
                (Some(want), Some(got)) if got.contains(want.as_str()) => {}
                (Some(want), Some(got)) => {
                    problems.push(format!("panic {got:?} does not contain {want:?}"))
                }
                (Some(want), None) => {
                    problems.push(format!("expected a panic containing {want:?}"))
                }
                (None, Some(got)) => problems.push(format!("unexpected panic: {got}")),
                (None, None) => {}
            }
            finish(problems)
        }
    }
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
        }
    }

    #[test]
    fn parses_directives() {
        let src = "print(1) // expect: 1\n// expect:\n// expect: a b \nlet x = y // error: unknown name\n// panic: boom\n// drops: a, b ,c\n// skip: later\n// plain comment\n";
        let d = parse_directives(src);
        assert_eq!(d.expect, ["1", "", "a b"]);
        assert_eq!(d.errors, [(4, "unknown name".to_string())]);
        assert_eq!(d.panic.as_deref(), Some("boom"));
        assert_eq!(d.drops, ["a", "b", "c"]);
        assert_eq!(d.skip.as_deref(), Some("later"));
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

    #[test]
    fn error_comparison() {
        let d = parse_directives("a // error: unknown\nb\nc // error: type\n");
        let ok = Outcome::CompileErrors(vec![
            (1, "unknown name `a`".into()),
            (3, "type mismatch".into()),
        ]);
        assert!(check(&d, &ok).is_ok());
        let missing = Outcome::CompileErrors(vec![(1, "unknown name".into())]);
        assert!(check(&d, &missing).is_err());
        let extra = Outcome::CompileErrors(vec![
            (1, "unknown".into()),
            (3, "type".into()),
            (2, "surprise".into()),
        ]);
        assert!(check(&d, &extra).is_err());
        assert!(check(&d, &compiled("", None)).is_err());
    }
}
