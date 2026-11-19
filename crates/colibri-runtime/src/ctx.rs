//! The context host code runs in: [`Ctx`] for host functions and conversions, and [`Caller`],
//! what `Fn::call` and `Callback::call` need to run Colibri code (EMBEDDING §0 A, §1).

use colibri_check::hir::{FnId, TypeDef};
use colibri_check::types::{Ty, TypeId};
use colibri_interp::{Fiber, Interp, PanicInfo};
use colibri_syntax::Span;

use crate::convert::Value;
use crate::error::{Error, Location, StackFrame};

/// What a host function (and every conversion) may use: the interpreter, the running fiber,
/// the host state. It is the same thing as [`Cx`] in the conversion traits.
///
/// A host function takes it as an optional first parameter: `|cx: &mut Ctx, x: i64| ..`.
pub struct Ctx<'a> {
    pub(crate) interp: &'a mut Interp,
    pub(crate) fiber: &'a mut Fiber,
}

/// The payload of the Rust panic [`Ctx::raise`] starts: the trampoline turns it into a Colibri
/// panic with exactly this message.
pub(crate) struct RaisedPanic(pub String);

/// The payload of the Rust panic [`Ctx::run_async`] starts when its coroutine is cancelled.
pub(crate) struct RaisedCancel;

/// The context of a conversion between Rust and Colibri values.
pub type Cx<'a> = Ctx<'a>;

/// Something that can run Colibri code: a [`crate::Vm`], or the [`Ctx`] of a host function that
/// wants to call back into the script it was called from.
pub trait Caller {
    /// The context to run in. Not for hosts to call.
    #[doc(hidden)]
    fn ctx(&mut self) -> Ctx<'_>;
}

impl Caller for Ctx<'_> {
    fn ctx(&mut self) -> Ctx<'_> {
        Ctx {
            interp: &mut *self.interp,
            fiber: &mut *self.fiber,
        }
    }
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(interp: &'a mut Interp, fiber: &'a mut Fiber) -> Self {
        Ctx { interp, fiber }
    }

    /// The host state of type `T` (see [`crate::Vm::insert_state`]), if there is one.
    ///
    /// The borrow lasts as long as `&mut self`, so a host function cannot hold it across a call
    /// back into Colibri: the borrow checker enforces it.
    pub fn state_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.interp.state_mut::<T>()
    }

    /// Stores host state of type `T`, returning the value it replaces.
    pub fn insert_state<T: 'static>(&mut self, value: T) -> Option<T> {
        self.interp.insert_state(value)
    }

    /// Takes host state of type `T` out.
    pub fn remove_state<T: 'static>(&mut self) -> Option<T> {
        self.interp.remove_state::<T>()
    }

    /// How many host functions are running, one inside another (at least 1 inside a host
    /// function).
    pub fn host_depth(&self) -> u32 {
        self.fiber.host_depth()
    }

    /// The steps the current outermost entry has used so far (see
    /// [`crate::Vm::set_instruction_budget`]).
    pub fn steps_used(&self) -> u64 {
        self.interp.steps_used()
    }

    /// Writes text to the `Vm`'s standard output, where `print` writes. It adds no newline.
    pub fn write_stdout(&mut self, text: &str) {
        self.interp.write_stdout(text);
    }

    /// Ends the running program with a Colibri panic whose message is exactly `message`, from
    /// anywhere in a host function: the script's `defer`s and `Drop` hooks run as for any panic
    /// and the host sees [`Error::Panic`]. The Rust stack unwinds to the `Vm` (no panic message
    /// is printed), so state the host function needs afterwards must be set first.
    pub fn raise(&mut self, message: impl Into<String>) -> ! {
        std::panic::resume_unwind(Box::new(RaisedPanic(message.into())))
    }

    /// Runs `work` on a background thread and returns its result, **without blocking the
    /// other coroutines**: inside a coroutine, the coroutine is suspended until the thread has
    /// finished and the scheduler resumes it on a later tick (the event loop sleeps until a
    /// timer or a completion, it does not poll). On the main fiber the call runs the event
    /// loop while it waits, so coroutines keep running. Inside a callback or a `Drop` hook it
    /// blocks the calling thread.
    ///
    /// Use it for blocking work (network, disk, a subprocess) in a host function; `work` must
    /// not touch the `Vm` or any Colibri value (it is `Send + 'static`). Other coroutines run
    /// while this call waits, so the host function must not hold a borrow of host state or
    /// host objects across it (the borrow checker enforces it for [`Ctx::state_mut`]).
    ///
    /// `work` runs on an idle thread of the `Vm`'s pool, or on a new one: there is no limit,
    /// and an operation never waits behind another (one may wait for a later one to be
    /// started). Idle threads are reused; those beyond a few end after some seconds. Work that
    /// blocks for a long time (a child process) holds its thread meanwhile.
    ///
    /// If the calling coroutine is cancelled meanwhile, the host function is unwound (its
    /// destructors run) and the result is discarded; `work` that already started runs to the
    /// end, one whose thread has not begun it never runs. A panic of `work` becomes a Colibri panic of the caller.
    pub fn run_async<R: Send + 'static>(&mut self, work: impl FnOnce() -> R + Send + 'static) -> R {
        match self.interp.await_external(self.fiber, work) {
            Ok(r) => r,
            Err(colibri_interp::ExternalError::Cancelled) => {
                std::panic::resume_unwind(Box::new(RaisedCancel))
            }
            Err(colibri_interp::ExternalError::Failed(message)) => self.raise(message),
        }
    }

    /// [`Ctx::run_async`] for work that can stop early: `work` is given a [`CancelToken`]
    /// that is cancelled when the waiting coroutine is cancelled, or when the `Vm` ends while
    /// the operation runs. The token's hooks run on the cancelling side before it goes on: an
    /// operation that holds a child process registers a hook that kills it (`process.run`
    /// does), so nothing is left running and no pool thread waits for a result nobody wants.
    pub fn run_async_cancellable<R: Send + 'static>(
        &mut self,
        work: impl FnOnce(&colibri_interp::CancelToken) -> R + Send + 'static,
    ) -> R {
        match self.interp.await_external_with(self.fiber, work) {
            Ok(r) => r,
            Err(colibri_interp::ExternalError::Cancelled) => {
                std::panic::resume_unwind(Box::new(RaisedCancel))
            }
            Err(colibri_interp::ExternalError::Failed(message)) => self.raise(message),
        }
    }

    /// The type arguments of the call this host function is running for, in the order of the
    /// function's type parameters (`fn decode[T](..)` gets one). Empty for a function that is
    /// not generic. Only a function registered with [`crate::Registration::typed`] can count
    /// on them being concrete types.
    pub fn type_args(&self) -> &[Ty] {
        self.interp.host_type_args()
    }

    /// Every struct, enum and class of the loaded program, indexed by [`TypeId`]: the type
    /// descriptors a serializer reads (DESIGN §7.5).
    pub fn type_defs(&self) -> &[TypeDef] {
        self.interp.type_defs()
    }

    /// [`Ctx::type_defs`], shared, so a serializer can read them while it calls
    /// [`Ctx::call_field_default`].
    pub fn type_defs_shared(&self) -> std::rc::Rc<[TypeDef]> {
        self.interp.type_defs_shared()
    }

    /// Computes the default value of a struct field: `f` is [`FieldDef::default`]
    /// (`reflect::FieldDef`). A panic in the default expression comes back as an error, and
    /// so does a `f` that is not a function of the loaded program or takes parameters (a
    /// default never does).
    pub fn call_field_default(&mut self, f: FnId) -> Result<Value, Error> {
        match self.interp.fn_param_count(f) {
            Some(0) => {}
            Some(n) => {
                return Err(Error::Convert(format!(
                    "function {} is not a field default: it takes {n} parameter(s)",
                    f.0
                )));
            }
            None => {
                return Err(Error::Convert(format!(
                    "function {} is not a function of the loaded program",
                    f.0
                )));
            }
        }
        let result = self.interp.call(self.fiber, f, Vec::new());
        self.interp.flush();
        match result {
            Ok(v) => Ok(Value(v)),
            Err(info) => Err(panic_error(self.interp, *info)),
        }
    }

    /// The type id of a struct, enum or class the host declared.
    pub(crate) fn host_type(&self, name: &str) -> TypeId {
        match self.interp.host_type(name) {
            Some(id) => id,
            None => panic!(
                "the type `{name}` is not part of the loaded host interface (register it with `Vm::register_type` before loading)"
            ),
        }
    }

    /// A struct value of the host type `name`, with its fields in declaration order.
    ///
    /// # Panics
    /// If `name` is not a type declared through the `TypeRegistry` (a bug in the
    /// `IntoColibri` implementation).
    pub fn new_struct(&self, name: &str, fields: Vec<Value>) -> Value {
        let ty = self.host_type(name);
        Value(colibri_interp::Value::strukt(
            ty,
            fields.into_iter().map(|v| v.0).collect(),
        ))
    }

    /// A value of the payload-free enum `name`: its `variant`th variant.
    ///
    /// # Panics
    /// As [`Ctx::new_struct`].
    pub fn new_variant(&self, name: &str, variant: u32) -> Value {
        let ty = self.host_type(name);
        Value(colibri_interp::Value::variant(ty, variant, Vec::new()))
    }

    /// Lets go of a value the host owns, so a `Drop` hook it was the last owner of can run.
    pub(crate) fn release(&mut self, value: Value) {
        self.interp.release(value.0);
    }

    /// Calls a function of the loaded program. `owner` is what the function handle says it
    /// belongs to.
    pub(crate) fn call_fn(
        &mut self,
        id: FnId,
        owner: (u64, u32),
        args: Vec<Value>,
    ) -> Result<Value, Error> {
        if self.interp.owner() != owner {
            return Err(Error::StaleHandle);
        }
        if !self.interp.is_loaded() {
            return Err(Error::NotLoaded);
        }
        let args = args.into_iter().map(|v| v.0).collect();
        let result = self.interp.call(self.fiber, id, args);
        self.interp.flush();
        match result {
            Ok(v) => Ok(Value(v)),
            Err(info) => Err(panic_error(self.interp, *info)),
        }
    }

    /// Calls a function or closure value.
    pub(crate) fn call_value(&mut self, f: &Value, args: Vec<Value>) -> Result<Value, Error> {
        let args = args.into_iter().map(|v| v.0).collect();
        let result = self.interp.call_function(self.fiber, &f.0, args);
        self.interp.flush();
        match result {
            Ok(v) => Ok(Value(v)),
            Err(info) => Err(panic_error(self.interp, *info)),
        }
    }
}

/// A panic of the interpreter as the error a host sees, with the locations looked up in the
/// program's sources.
pub(crate) fn panic_error(interp: &Interp, info: PanicInfo) -> Error {
    let sources = interp.sources();
    let locate = |span: Span| {
        if !sources.contains(span.file) {
            return Location {
                file: "<host>".to_string(),
                line: 0,
                column: 0,
            };
        }
        let (line, column) = sources.line_col(span.file, span.start);
        Location {
            file: sources.name(span.file).to_string(),
            line,
            column,
        }
    };
    let located = !info.stack.is_empty() || info.span.start > 0;
    let location = located.then(|| locate(info.span));
    let stack = info
        .stack
        .iter()
        .map(|entry| StackFrame {
            function: entry.function.clone(),
            location: locate(entry.span),
        })
        .collect();
    if info.budget {
        return Error::BudgetExhausted { location, stack };
    }
    Error::Panic {
        message: info.message,
        location,
        stack,
        notes: info.notes,
    }
}
