//! `#[colibri::export]`: attribute macros that export Rust items to Colibri (EMBEDDING §2).
//!
//! The runtime crate re-exports [`export`] as `colibri::export`; use it from there. What the
//! macro writes uses only paths below `colibri_runtime::__private`, so an engine other than the
//! VM can provide the same names.
//!
//! * `#[colibri::export(value)]` on a struct with only `pub` fields, all convertible, and
//!   `Copy`: a Colibri `struct`, converted field by field each time it crosses. On an enum
//!   without payloads: a Colibri `enum`.
//! * `#[colibri::export]` on a struct: an opaque host class (`HostType`). Its methods come from
//!   `#[colibri::export] impl Name { ... }`; write `#[colibri::export(no_methods)]` for a class
//!   without any.
//! * `#[colibri::export] impl Name { ... }`: every `pub fn` is exported. `&self` and `&mut self`
//!   methods are methods, functions without a receiver are static functions (constructors).
//!   Parameter names are the Rust ones, so scripts may pass arguments by name. A first
//!   parameter `&mut Ctx` after the receiver gives the function the host context.
//! * `#[colibri::export] fn name(..)`: a function, registered with `name::register(&mut vm)`.
//!   The macro writes a module of that name next to the function.
//!
//! * `#[colibri::module] fn name(m: &mut colibri::Module<'_>) -> Result<(), colibri::Error>`: the
//!   registration function of a binding crate. The function stays, and a hidden
//!   `__colibri_binding` module next to it (put the function at the crate root) has `NAME`, the
//!   Rust name, and `register(vm, import_name)`, which a generated runner calls with the name
//!   the manifest chose. Inside, `m.add_class::<T>()?` registers a class or value type and
//!   `m.add_fn(f::export)?` an exported function.
//!
//! An option `crate = path` names the runtime crate when it is not `colibri_runtime`:
//! `#[colibri::export(value, crate = my_engine::colibri)]`.

mod expand;

use proc_macro::TokenStream;

/// Exports a Rust item to Colibri. See the [crate documentation](crate).
#[proc_macro_attribute]
pub fn export(args: TokenStream, item: TokenStream) -> TokenStream {
    expand::export(args.into(), item.into()).into()
}

/// Marks the registration function of a binding crate. See the [crate documentation](crate).
#[proc_macro_attribute]
pub fn module(args: TokenStream, item: TokenStream) -> TokenStream {
    expand::module(args.into(), item.into()).into()
}
