//! A small entry for tests: check a source text the way the runtime does and run it.

use std::cell::RefCell;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;

use colibri_check::hir::Program;
use colibri_check::package::{Entry, LoadOptions, MemoryFs};
use colibri_check::{CheckOptions, DEFAULT_STACK_BUDGET, HostEnv};
use colibri_core::prelude::*;
use colibri_syntax::SourceMap;

use crate::compile::{CompileEnv, compile_body};
use crate::fiber::VmFiber;
use crate::machine::Machine;

/// What running a program produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Everything the program printed.
    pub stdout: String,
    /// The panic message (with the notes of later panics, one `note:` line each) when the
    /// program ended in a panic, or the compile errors when it did not compile.
    pub panic: Option<String>,
    /// The stack of the panic, innermost call first: the function and the 1-based line.
    pub trace: Vec<(String, u32)>,
    /// The labels of the class objects destroyed, in order (when `Options::trace_drops`).
    pub drops: Vec<String>,
    /// The program did not compile: `panic` holds the diagnostics.
    pub compile_error: bool,
    /// The budget steps the last entry (`main`, else the module) used, grace included.
    #[doc(hidden)] // testing only, no stability promise
    pub steps: u64,
    /// How many closure built-in calls ran in a native loop (see `Options::no_leaf`).
    #[doc(hidden)] // testing only, no stability promise
    pub leaf_runs: u64,
}

/// How [`run_source_with`] runs a program.
#[derive(Debug, Clone)]
pub struct Options {
    /// The call-depth limit (the runtime's default is 10,000).
    pub max_call_depth: usize,
    /// The instruction budget of the run (`None`: unlimited).
    pub instruction_budget: Option<u64>,
    pub trace_drops: bool,
    /// Never run a closure built-in in a native loop: the prelude's code does every call, as it
    /// did before leaf closures got their own loops (for tests that compare the two).
    #[doc(hidden)] // testing only, no stability promise
    pub no_leaf: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_call_depth: 10_000,
            instruction_budget: None,
            trace_drops: false,
            no_leaf: false,
        }
    }
}

#[derive(Clone, Default)]
struct Shared(Rc<RefCell<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reads and checks one source text, as the runtime's `load_source` does for a program with
/// no host modules. `Err` holds the error diagnostics, one per line.
pub(crate) fn check_source(src: &str) -> Result<(Program, SourceMap), String> {
    let mut fs = MemoryFs::new();
    fs.insert("main.colibri", src);
    let mut sources = SourceMap::new();
    let loaded = colibri_check::package::load_with(
        &fs,
        &mut sources,
        Entry::File(Path::new("main.colibri")),
        &[],
        LoadOptions::default(),
    )
    .map_err(|e| format!("{}: {}", e.path, e.message))?;
    let mut diagnostics = loaded.diagnostics.clone();
    let mut program = None;
    if !loaded.has_errors() {
        let checked = colibri_check::check_modules_with(
            &loaded.inputs(),
            &loaded.packages,
            &HostEnv::new(),
            DEFAULT_STACK_BUDGET,
            CheckOptions::default(),
        );
        diagnostics.extend(checked.diagnostics);
        program = checked.program;
    }
    match program {
        Some(p) => Ok((p, sources)),
        None => Err(diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(|d| {
                let (line, _) = sources.line_col(d.span.file, d.span.start);
                format!("line {line}: {}", d.message)
            })
            .collect::<Vec<_>>()
            .join("\n")),
    }
}

/// Compiles every function of `src` (and the script) and returns `(name, disassembly)` in
/// program order. For tests of the compiler.
pub fn compile_source(src: &str) -> Result<Vec<(String, String)>, String> {
    let (program, _) = check_source(src)?;
    let env = CompileEnv::for_program(&program);
    let mut out = Vec::new();
    for (i, f) in program.fns.iter().enumerate() {
        let id = colibri_check::hir::FnId(i as u32);
        let chunk = compile_body(&f.body, Some(id), &env)?;
        out.push((f.name.clone(), chunk.disassemble()));
    }
    let chunk = compile_body(&program.script, None, &env)?;
    out.push(("<script>".to_string(), chunk.disassemble()));
    Ok(out)
}

/// Runs a program: the module-level values and top-level statements, then `main` if there is
/// one.
pub fn run_source(src: &str) -> Outcome {
    run_source_with(src, &Options::default())
}

pub fn run_source_with(src: &str, options: &Options) -> Outcome {
    let (program, sources) = match check_source(src) {
        Ok(ok) => ok,
        Err(errors) => {
            return Outcome {
                panic: Some(errors),
                compile_error: true,
                ..Outcome::default()
            };
        }
    };
    let out = Shared::default();
    let mut machine = Machine::new(Box::new(out.clone()), options.max_call_depth);
    machine
        .core_mut()
        .set_instruction_budget(options.instruction_budget);
    if options.trace_drops {
        machine.core_mut().set_trace_drops(true);
    }
    machine.leaf_off = options.no_leaf;
    machine.load(&program);
    let mut fiber = VmFiber::new();
    let mut failure = machine.run_module(&mut fiber).err();
    if failure.is_none()
        && let Some(main) = program.main
    {
        match machine.call(&mut fiber, main, Vec::new()) {
            Ok(value) => machine.release_in_entry(value),
            Err(info) => failure = Some(info),
        }
    }
    machine.core_mut().flush();
    let steps = machine.core().steps_used();
    let leaf_runs = machine.leaf_runs;
    machine.shutdown();
    let drops = machine.core_mut().take_trace();
    let (panic, trace) = match failure {
        Some(info) => {
            let message = info
                .notes
                .iter()
                .fold(info.message.clone(), |m, n| format!("{m}\nnote: {n}"));
            let trace = info
                .stack
                .iter()
                .map(|t| {
                    (
                        t.function.clone(),
                        sources.line_col(t.span.file, t.span.start).0,
                    )
                })
                .collect();
            (Some(message), trace)
        }
        None => (None, Vec::new()),
    };
    let stdout = String::from_utf8_lossy(&out.0.borrow()).into_owned();
    Outcome {
        stdout,
        panic,
        trace,
        drops,
        compile_error: false,
        steps,
        leaf_runs,
    }
}
