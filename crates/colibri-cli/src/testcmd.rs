//! `colibri test`: finds the `test "name" { .. }` blocks of a script or package and runs each
//! one from a fresh state (ARCHITECTURE §15).

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use colibri_runtime as colibri;

use crate::leakcmd::{self, LeakMode};

/// The instruction budget of one test when `--budget` is not given.
pub const DEFAULT_BUDGET: u64 = 100_000_000;

/// What `colibri test` was asked to do.
pub struct Options {
    /// A script file, or a package folder (the current folder by default).
    pub path: PathBuf,
    /// Run only the tests whose `module::name` contains this text.
    pub filter: Option<String>,
    /// Steps one test may take; 0 is unlimited.
    pub budget: u64,
    /// Let a test's `print` output through at once instead of keeping it for failures.
    pub nocapture: bool,
    /// Report the cycles each test leaks (`--leaks`); `Error` fails the test.
    pub leaks: Option<crate::leakcmd::LeakMode>,
    /// `--locked` and `--offline` for git dependencies.
    pub deps: crate::depscmd::DepFlags,
    /// Profile the tests (`--profile`): one profile for the whole run.
    pub profile: Option<crate::profilecmd::Options>,
}

/// Runs the tests and prints the report to stdout; the exit code is 1 if the program does not
/// compile or any test fails.
pub fn run(options: &Options) -> ExitCode {
    let stderr_color = io::stderr().is_terminal();
    if let Err(code) = crate::depscmd::prepare(&options.path, options.deps) {
        return code;
    }
    let captured = colibri::SharedBuffer::new();
    let mut builder = crate::vm_builder()
        .allow_all()
        .track_leaks(options.leaks.is_some());
    if !options.nocapture {
        builder = builder.stdout(Box::new(captured.clone()));
    }
    let mut vm = crate::build_vm(builder, Vec::new());
    if let Some(profile) = &options.profile {
        crate::profilecmd::start(&mut vm, profile);
    }
    if let Err(e) = vm.load_tests(&options.path) {
        eprint!("{}", e.render(stderr_color));
        return ExitCode::from(1);
    }
    let warnings = vm.render_warnings(stderr_color);
    if !warnings.is_empty() {
        eprint!("{warnings}");
    }
    vm.set_instruction_budget((options.budget > 0).then_some(options.budget));

    // A script's tests are not run by a folder: its initialisers would run.
    let mut skipped: Vec<(String, usize)> = Vec::new();
    for t in vm.skipped_tests().into_iter().filter(|t| t.root) {
        match skipped.iter_mut().find(|(f, _)| *f == t.file) {
            Some((_, n)) => *n += 1,
            None => skipped.push((t.file, 1)),
        }
    }
    for (file, n) in &skipped {
        eprintln!(
            "skipped {n} test{} in {file}: a script's tests run with `colibri test {file}`",
            if *n == 1 { "" } else { "s" }
        );
    }

    let all: Vec<colibri::TestInfo> = vm.tests().into_iter().filter(|t| t.root).collect();
    let selected: Vec<&colibri::TestInfo> = all
        .iter()
        .filter(|t| {
            options
                .filter
                .as_ref()
                .is_none_or(|f| full_name(t).contains(f.as_str()))
        })
        .collect();
    let filtered_out = all.len() - selected.len();

    let color = io::stdout().is_terminal();
    let mut out = io::stdout().lock();
    let _ = writeln!(
        out,
        "running {} test{}",
        selected.len(),
        if selected.len() == 1 { "" } else { "s" }
    );
    let mut seen = leakcmd::Seen::default();
    let mut passed = 0usize;
    let mut failures: Vec<(String, String, String)> = Vec::new();
    for test in selected {
        let name = full_name(test);
        let result = vm.run_test(test.id);
        let output = captured.take();
        // A cycle a test leaks stays leaked, so only the ones it added are its own.
        let leaked = options
            .leaks
            .map(|mode| (mode, seen.new_leaks(&vm)))
            .filter(|(_, found)| !found.is_empty());
        if let Some((LeakMode::Warn, found)) = &leaked {
            eprint!("{}", leakcmd::render(found, LeakMode::Warn, stderr_color));
        }
        match result {
            Ok(()) if matches!(&leaked, Some((LeakMode::Error, _))) => {
                let _ = writeln!(out, "test {name} ... {}", paint("FAILED", "31", color));
                let found = leaked.map(|(_, f)| f).unwrap_or_default();
                failures.push((
                    name,
                    output,
                    leakcmd::render(&found, LeakMode::Error, false),
                ));
            }
            Ok(()) => {
                passed += 1;
                let _ = writeln!(out, "test {name} ... {}", paint("ok", "32", color));
            }
            Err(e) => {
                let _ = writeln!(out, "test {name} ... {}", paint("FAILED", "31", color));
                failures.push((name, output, describe(&e, color)));
            }
        }
        let _ = out.flush();
    }
    if !failures.is_empty() {
        let _ = writeln!(out, "\nfailures:");
        for (name, output, details) in &failures {
            let _ = writeln!(out, "\n---- {name} ----");
            if !output.is_empty() {
                let _ = write!(out, "{output}");
                if !output.ends_with('\n') {
                    let _ = writeln!(out);
                }
            }
            let _ = write!(out, "{details}");
        }
    }
    if let Some(profile) = &options.profile {
        crate::profilecmd::finish(&mut vm, profile);
    }
    let failed = failures.len();
    let mut summary = format!("\n{passed} passed, {failed} failed");
    if filtered_out > 0 {
        summary.push_str(&format!(" ({filtered_out} filtered out)"));
    }
    let _ = writeln!(out, "{summary}");
    let _ = out.flush();
    drop(vm);
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// `module::name`, what the report shows and `--filter` matches.
fn full_name(test: &colibri::TestInfo) -> String {
    format!("{}::{}", test.module, test.name)
}

/// The failure as the report prints it: the panic message with its location and stack, the
/// error a test returned, or the exhausted budget.
fn describe(error: &colibri::Error, color: bool) -> String {
    let mut text = error.render(color);
    match error {
        colibri::Error::TestReturnedErr { .. } => {
            text = format!("the test returned an error\n{text}");
        }
        colibri::Error::BudgetExhausted { .. } => {
            text.push_str(
                "hint: an endless loop? raise the limit with `--budget <steps>` (0 is unlimited)\n",
            );
        }
        _ => {}
    }
    text
}

fn paint(text: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}
