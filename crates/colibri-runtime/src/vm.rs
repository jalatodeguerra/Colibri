//! The virtual machine a host embeds.

use std::io::Write;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use colibri_check::hir::{FnId, Program};
use colibri_interp::{DEFAULT_NATIVE_STACK_BUDGET, Fiber, Interp, PanicInfo};
use colibri_syntax::{Diagnostic, SourceMap, parse_file};

use crate::convert::{Args, FromColibri, Type, Value};
use crate::error::{Error, Location, Report, StackFrame};
use crate::func::Fn;

/// The default limit on nested Colibri calls (DESIGN §10.4 leaves the number open).
pub const DEFAULT_MAX_CALL_DEPTH: usize = 10_000;

static NEXT_VM_ID: AtomicU64 = AtomicU64::new(0);

/// Configures a [`Vm`].
pub struct VmBuilder {
    stdout: Option<Box<dyn Write>>,
    max_call_depth: usize,
    native_stack_budget: usize,
    trace_drops: bool,
}

impl VmBuilder {
    /// Records the label of every class object as it is destroyed, for tests of drop order
    /// (see [`Vm::take_drop_trace`]). Off by default.
    #[doc(hidden)]
    pub fn trace_drops(mut self, on: bool) -> Self {
        self.trace_drops = on;
        self
    }

    /// Where `print` writes. The default is the process's standard output.
    pub fn stdout(mut self, writer: Box<dyn Write>) -> Self {
        self.stdout = Some(writer);
        self
    }

    /// How many Colibri calls may be active at once before a "stack overflow" panic. Each
    /// call also uses Rust stack (see [`VmBuilder::native_stack_budget`]): the `colibri` tool
    /// runs on a 256 MB stack for the default of 10 000.
    pub fn max_call_depth(mut self, depth: usize) -> Self {
        self.max_call_depth = depth.max(1);
        self
    }

    /// How many bytes of Rust stack the interpreter may use below the point where the host
    /// calls in, before a deep recursion panics with "stack overflow" instead of crashing the
    /// host. The default is 1 MiB, which fits a default test thread; on a thread with a
    /// bigger stack raise it to a little below that stack's size, together with
    /// [`VmBuilder::max_call_depth`].
    pub fn native_stack_budget(mut self, bytes: usize) -> Self {
        self.native_stack_budget = bytes;
        self
    }

    pub fn build(self) -> Vm {
        let out = self.stdout.unwrap_or_else(|| Box::new(std::io::stdout()));
        let mut interp = Interp::new(out, self.max_call_depth);
        interp.set_native_stack_budget(self.native_stack_budget);
        interp.set_trace_drops(self.trace_drops);
        Vm {
            sources: SourceMap::new(),
            interp,
            fiber: Fiber::new(),
            program: None,
            warnings: Vec::new(),
            id: NEXT_VM_ID.fetch_add(1, Ordering::Relaxed),
            epoch: 0,
            native_stack_budget: self.native_stack_budget,
        }
    }
}

/// A Colibri virtual machine: one loaded program, its module-level state and an output writer.
pub struct Vm {
    sources: SourceMap,
    interp: Interp,
    fiber: Fiber,
    program: Option<Rc<Program>>,
    warnings: Vec<Diagnostic>,
    /// Distinguishes this `Vm` from every other, so a handle cannot be used on the wrong one.
    id: u64,
    /// Bumped whenever the loaded program changes, which invalidates function handles.
    epoch: u32,
    native_stack_budget: usize,
}

impl Default for Vm {
    fn default() -> Self {
        Vm::new()
    }
}

impl Vm {
    pub fn builder() -> VmBuilder {
        VmBuilder {
            stdout: None,
            max_call_depth: DEFAULT_MAX_CALL_DEPTH,
            native_stack_budget: DEFAULT_NATIVE_STACK_BUDGET,
            trace_drops: false,
        }
    }

    /// A `Vm` printing to the process's standard output.
    pub fn new() -> Vm {
        Vm::builder().build()
    }

    /// Reads, checks and runs a script file: module-level values are initialised, then its
    /// top-level statements run. `fn main` is not called; use [`Vm::run_main`].
    pub fn load_file(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        self.load_source(&path.display().to_string(), &text)
    }

    /// Like [`Vm::load_file`] for source text; `name` is what diagnostics call the file.
    ///
    /// On a compile error nothing is loaded. If the top-level code panics, the program is
    /// discarded (its module state is half-built) and the `Vm` can load another.
    pub fn load_source(&mut self, name: &str, source: &str) -> Result<(), Error> {
        if self.program.is_some() {
            return Err(Error::AlreadyLoaded);
        }
        let mut sources = SourceMap::new();
        let file = sources.add_file(name, source);
        let (ast, mut diagnostics) = parse_file(file, source);
        let mut program = None;
        // A tree with syntax errors would only produce follow-on errors.
        if !diagnostics.iter().any(Diagnostic::is_error) {
            let checked = colibri_check::check_with_stack_budget(&ast, self.native_stack_budget);
            diagnostics = checked.diagnostics;
            program = checked.program;
        }
        let Some(program) = program else {
            return Err(Error::Compile(Report {
                sources,
                diagnostics,
            }));
        };
        self.sources = sources;
        self.warnings = diagnostics;
        self.epoch += 1;
        self.interp.load(&program);
        let outcome = self.interp.run_module(&mut self.fiber);
        self.interp.flush();
        match outcome {
            Ok(()) => {
                self.program = Some(Rc::new(program));
                Ok(())
            }
            Err(info) => {
                self.epoch += 1;
                self.interp.shutdown();
                Err(self.panic_error(*info))
            }
        }
    }

    /// Calls `fn main()` if the program defines it.
    pub fn run_main(&mut self) -> Result<(), Error> {
        let program = self.program.clone().ok_or(Error::NotLoaded)?;
        match program.main {
            Some(main) => self
                .call_id(main, (self.id, self.epoch), Vec::new())
                .map(|_| ()),
            None => Ok(()),
        }
    }

    /// A handle to the function `name`, if its Colibri signature matches `A -> R`.
    pub fn get_fn<A: Args, R: FromColibri>(&self, name: &str) -> Result<Fn<A, R>, Error> {
        let program = self.program.as_ref().ok_or(Error::NotLoaded)?;
        let id = program.fn_by_name(name).ok_or_else(|| Error::FnNotFound {
            name: name.to_string(),
        })?;
        let sig = &program.fns[id.0 as usize].sig;
        let params: Vec<Option<Type>> = sig.params.iter().map(Type::of).collect();
        let wanted: Vec<Type> = A::colibri_types();
        let ret = Type::of(&sig.ret);
        let matches = params.len() == wanted.len()
            && params.iter().zip(&wanted).all(|(a, b)| *a == Some(*b))
            && ret == Some(R::colibri_type());
        if !matches {
            let list: Vec<String> = wanted.iter().map(Type::to_string).collect();
            let mut rust = format!("fn({})", list.join(", "));
            if R::colibri_type() != Type::Unit {
                rust.push_str(&format!(" -> {}", R::colibri_type()));
            }
            return Err(Error::SignatureMismatch {
                name: name.to_string(),
                colibri: sig.render(&program.types),
                rust,
            });
        }
        Ok(Fn::new(id, self.id, self.epoch))
    }

    /// Warnings from the last successful load.
    pub fn warnings(&self) -> &[Diagnostic] {
        &self.warnings
    }

    /// The last warnings rendered with source snippets.
    pub fn render_warnings(&self, color: bool) -> String {
        Report {
            sources: self.sources.clone(),
            diagnostics: self.warnings.clone(),
        }
        .render(color)
    }

    /// The sources of the loaded program.
    pub fn sources(&self) -> &SourceMap {
        &self.sources
    }

    pub(crate) fn call_id(
        &mut self,
        id: FnId,
        owner: (u64, u32),
        args: Vec<Value>,
    ) -> Result<Value, Error> {
        if owner != (self.id, self.epoch) {
            return Err(Error::StaleHandle);
        }
        if self.program.is_none() {
            return Err(Error::NotLoaded);
        }
        let args = args.into_iter().map(|v| v.0).collect();
        let result = self.interp.call(&mut self.fiber, id, args);
        self.interp.flush();
        match result {
            Ok(v) => Ok(Value(v)),
            Err(info) => Err(self.panic_error(*info)),
        }
    }

    fn panic_error(&self, info: PanicInfo) -> Error {
        let locate = |span: colibri_syntax::Span| {
            let (line, column) = self.sources.line_col(span.file, span.start);
            Location {
                file: self.sources.name(span.file).to_string(),
                line,
                column,
            }
        };
        let located = !info.stack.is_empty() || info.span.start > 0;
        Error::Panic {
            message: info.message,
            location: located.then(|| locate(info.span)),
            stack: info
                .stack
                .iter()
                .map(|entry| StackFrame {
                    function: entry.function.clone(),
                    location: locate(entry.span),
                })
                .collect(),
            notes: info.notes,
        }
    }

    /// Ends the program's life: module-level values are released in reverse initialisation
    /// order (DESIGN §11.2). Done automatically when the `Vm` is dropped or loads another
    /// program; call it to see the effects (and the drop trace) at a chosen moment.
    pub fn shutdown(&mut self) {
        self.interp.shutdown();
        self.interp.flush();
    }

    /// The labels of the class objects destroyed so far, in order, when the `Vm` was built
    /// with [`VmBuilder::trace_drops`]; the trace is emptied. Test support.
    #[doc(hidden)]
    pub fn take_drop_trace(&mut self) -> Vec<String> {
        self.interp.take_trace()
    }
}
