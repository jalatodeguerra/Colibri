//! Host classes: Rust types whose objects scripts hold but only the host looks inside
//! (EMBEDDING §2).
//!
//! A [`HostType`] says what Colibri calls the class and describes its methods to a
//! [`TypeBuilder`]. The builder turns each Rust closure into one non-generic [`RawHostFn`]
//! whose first argument, for a method, is the object: the wrapper borrows it from the host heap
//! (shared for `&T`, exclusive for `&mut T`) for exactly the length of the call, so a call back
//! into the script that reaches the same object finds the borrow and panics with a defined
//! message instead of aliasing a `&mut`.

use std::marker::PhantomData;
use std::rc::Rc;

use colibri_interp::HostFault;

use crate::convert::{FromColibri, IntoColibri, Value};
use crate::ctx::Ctx;
use crate::handle::borrow_receiver;
use crate::host::{HostFn, Plain, RawHostFn, WithCtx, arity_fault, conversion_fault};
use crate::types::{MethodEntry, SigDesc, TypeRegistry};

/// A Rust type that scripts see as an opaque class (`@host class`).
///
/// Implement it by hand or with `#[colibri::export]` on the struct and on an `impl` block of
/// its methods. A type returned by value from a host function (or method) becomes an object the
/// script owns; [`crate::Vm::insert`] makes one the host owns.
///
/// ```
/// use colibri_runtime as colibri;
///
/// struct Counter(i64);
///
/// impl colibri::HostType for Counter {
///     const NAME: &'static str = "Counter";
///     fn describe(d: &mut colibri::TypeBuilder<Self>) {
///         d.static_fn("new", ["start"], |start: i64| Counter(start));
///         d.method_mut("bump", ["by"], |this: &mut Counter, by: i64| this.0 += by);
///         d.method("get", [], |this: &Counter| this.0);
///     }
/// }
///
/// # fn main() -> Result<(), colibri::Error> {
/// let out = colibri::SharedBuffer::new();
/// let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
/// vm.register_type::<Counter>()?;
/// vm.load_source("a.colibri", "fn main() {\n    let c = Counter.new(1)\n    c.bump(4)\n    print(c.get())\n}\n")?;
/// vm.run_main()?;
/// assert_eq!(out.contents(), "5\n");
/// # Ok(())
/// # }
/// ```
pub trait HostType: Sized + 'static {
    /// The name of the class in Colibri.
    const NAME: &'static str;
    /// Declares the methods and static functions of the class.
    fn describe(d: &mut TypeBuilder<'_, Self>);
}

/// The methods an `#[colibri::export] impl` block exports, for the [`HostType`] that
/// `#[colibri::export]` on the struct wrote. Not for hosts to implement.
#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "`{Self}` is exported as a host class, but no `#[colibri::export] impl {Self}` block gives it methods",
    label = "no exported methods",
    note = "write `#[colibri::export] impl {Self} {{ pub fn .. }}`, or `#[colibri::export(no_methods)]` on the struct for a class without methods"
)]
pub trait ExportedMethods: HostType {
    fn describe_methods(d: &mut TypeBuilder<'_, Self>);
}

/// Collects the methods of a [`HostType`]. Parameter names are given explicitly, as an array
/// with one name per parameter (not counting the receiver), because Rust closures have none.
pub struct TypeBuilder<'r, T: HostType> {
    reg: &'r mut TypeRegistry,
    methods: Vec<MethodEntry>,
    problems: Vec<String>,
    _class: PhantomData<fn(T)>,
}

impl<'r, T: HostType> TypeBuilder<'r, T> {
    pub(crate) fn new(reg: &'r mut TypeRegistry) -> Self {
        TypeBuilder {
            reg,
            methods: Vec::new(),
            problems: Vec::new(),
            _class: PhantomData,
        }
    }

    /// The methods collected, after reporting every problem to the registry.
    pub(crate) fn finish(self) -> Vec<MethodEntry> {
        self.reg.conflicts.extend(self.problems);
        self.methods
    }

    /// A function of the class that takes no object: `World.new(..)` in a script. The closure
    /// is anything [`crate::Vm::register_fn`] accepts, optionally with a leading `&mut Ctx`.
    pub fn static_fn<M, F: HostFn<M>, const N: usize>(
        &mut self,
        name: &str,
        params: [&str; N],
        f: F,
    ) -> &mut Self {
        let sig = F::sig(self.reg);
        self.add(name, &params, sig, false, f.into_raw());
        self
    }

    /// A method that reads the object (`&self`): `world.position(..)` in a script. The closure
    /// takes `&T` first (after an optional `&mut Ctx`), then the parameters.
    pub fn method<M, F: HostMethod<M, T>, const N: usize>(
        &mut self,
        name: &str,
        params: [&str; N],
        f: F,
    ) -> &mut Self {
        let sig = F::sig(self.reg);
        self.add(name, &params, sig, true, f.into_raw());
        self
    }

    /// A method that changes the object (`&mut self`). In Colibri it is a plain `self` method,
    /// like every method of a class. If a call back into the script reaches the same object
    /// while the method runs, that call panics: ``host object `World` is already in use by an
    /// enclosing call (`World.step`)``.
    pub fn method_mut<M, F: HostMethodMut<M, T>, const N: usize>(
        &mut self,
        name: &str,
        params: [&str; N],
        f: F,
    ) -> &mut Self {
        let sig = F::sig(self.reg);
        self.add(name, &params, sig, true, f.into_raw());
        self
    }

    fn add(&mut self, name: &str, params: &[&str], sig: SigDesc, has_self: bool, raw: RawHostFn) {
        let class = T::NAME;
        if name == "alive" {
            self.problems.push(format!(
                "`{class}.alive` is built into every host class and cannot be redefined"
            ));
            return;
        }
        if self.methods.iter().any(|m| m.name == name) {
            self.problems
                .push(format!("`{class}.{name}` is described twice"));
            return;
        }
        if params.len() != sig.params.len() {
            self.problems.push(format!(
                "`{class}.{name}` takes {} parameter(s) but {} name(s) were given",
                sig.params.len(),
                params.len()
            ));
            return;
        }
        self.methods.push(MethodEntry {
            name: name.to_string(),
            params: params.iter().map(ToString::to_string).collect(),
            sig,
            has_self,
            raw,
        });
    }
}

/// A closure that can be a `&self` method of `T`: `Fn(&T, ..)`, optionally after `&mut Ctx`.
pub trait HostMethod<Marker, T: HostType>: 'static {
    /// The signature, without the receiver.
    fn sig(reg: &mut TypeRegistry) -> SigDesc;
    fn into_raw(self) -> RawHostFn;
}

/// A closure that can be a `&mut self` method of `T`: `Fn(&mut T, ..)`, optionally after
/// `&mut Ctx`.
pub trait HostMethodMut<Marker, T: HostType>: 'static {
    /// The signature, without the receiver.
    fn sig(reg: &mut TypeRegistry) -> SigDesc;
    fn into_raw(self) -> RawHostFn;
}

fn missing_object(class: &str) -> HostFault {
    HostFault::Panic(format!(
        "internal error: the object is not a `{class}` of this host"
    ))
}

macro_rules! host_method {
    ($count:expr; $(($name:ident, $index:tt, $arg:tt)),*) => {
        host_method!(@one HostMethod, Plain, false, downcast_ref, Fn(&This), [], $count; $(($name, $arg)),*);
        host_method!(@one HostMethod, WithCtx, false, downcast_ref, Fn(&mut Ctx<'_>, &This), [cx], $count; $(($name, $arg)),*);
        host_method!(@one HostMethodMut, Plain, true, downcast_mut, Fn(&mut This), [], $count; $(($name, $arg)),*);
        host_method!(@one HostMethodMut, WithCtx, true, downcast_mut, Fn(&mut Ctx<'_>, &mut This), [cx], $count; $(($name, $arg)),*);
    };
    (@one $tr:ident, $marker:ident, $excl:expr, $get:ident, Fn($($lead:ty),*), [$($cx:ident)?], $count:expr; $(($name:ident, $arg:tt)),*) => {
        impl<Func, This, Ret, $($name),*> $tr<$marker<($($name,)*)>, This> for Func
        where
            This: HostType,
            Func: Fn($($lead,)* $($name),*) -> Ret + 'static,
            Ret: IntoColibri,
            $($name: FromColibri,)*
        {
            fn sig(reg: &mut TypeRegistry) -> SigDesc {
                let _ = &reg;
                SigDesc {
                    params: vec![$(<$name as FromColibri>::ty(reg)),*],
                    ret: <Ret as IntoColibri>::ty(reg),
                }
            }

            #[allow(unused_variables, unused_mut, non_snake_case)]
            fn into_raw(self) -> RawHostFn {
                Rc::new(move |cx: &mut Ctx<'_>, args: &[Value]| {
                    if args.len() != $count + 1 {
                        return Err(arity_fault($count + 1, args.len()));
                    }
                    let mut guard = borrow_receiver::<This>(cx, &args[0], $excl)?;
                    $(let $name = <$name as FromColibri>::from_colibri(&args[$arg], cx)
                        .map_err(|e| conversion_fault($arg, e))?;)*
                    let this = guard
                        .$get::<This>()
                        .ok_or_else(|| missing_object(This::NAME))?;
                    let result = host_method!(@call self, this, $excl, cx, [$($cx)?], $($name),*);
                    drop(guard);
                    Ok(result.into_colibri(cx))
                })
            }
        }
    };
    (@call $f:ident, $this:ident, $excl:expr, $cx:ident, [], $($name:ident),*) => {
        $f($this, $($name),*)
    };
    (@call $f:ident, $this:ident, $excl:expr, $cx:ident, [cx], $($name:ident),*) => {
        $f($cx, $this, $($name),*)
    };
}

host_method!(0;);
host_method!(1; (A, 0, 1));
host_method!(2; (A, 0, 1), (B, 1, 2));
host_method!(3; (A, 0, 1), (B, 1, 2), (C, 2, 3));
host_method!(4; (A, 0, 1), (B, 1, 2), (C, 2, 3), (D, 3, 4));
host_method!(5; (A, 0, 1), (B, 1, 2), (C, 2, 3), (D, 3, 4), (E, 4, 5));
host_method!(6; (A, 0, 1), (B, 1, 2), (C, 2, 3), (D, 3, 4), (E, 4, 5), (F, 5, 6));
host_method!(7; (A, 0, 1), (B, 1, 2), (C, 2, 3), (D, 3, 4), (E, 4, 5), (F, 5, 6), (G, 6, 7));
host_method!(8; (A, 0, 1), (B, 1, 2), (C, 2, 3), (D, 3, 4), (E, 4, 5), (F, 5, 6), (G, 6, 7), (H, 7, 8));
