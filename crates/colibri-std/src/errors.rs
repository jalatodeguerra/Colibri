//! The typed errors of the standard library (DESIGN §10.2): each module that can fail declares
//! `pub enum Error` (`fs.Error`, `http.Error`, ..) and its functions return the erased
//! `Error` of the prelude with a variant of that enum underneath. A script tells them apart
//! with `e.is[fs.Error]()` and `e.as[fs.Error]()`, also after `?` and `.context(..)`; `{e}`
//! prints the same message as before.

/// Declares the Rust side of a module's `Error` enum: the variants (in the order of the
/// interface), `register` to declare the enum in the module, and `error` to make the
/// [`colibri_runtime::TypedError`] a function returns.
macro_rules! error_kind {
    ($module:literal, $(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            const VARIANTS: &'static [&'static str] = &[$(stringify!($variant)),+];

            /// Declares `Error` as an enum of this module (call it first in `register`).
            pub(crate) fn declare(vm: &mut colibri_runtime::Vm) -> Result<(), colibri_runtime::Error> {
                vm.std_module($module).register_module_enum("Error", Self::VARIANTS)
            }

            /// An error of this kind that shows as `message`.
            pub(crate) fn error(self, message: impl Into<String>) -> colibri_runtime::TypedError {
                colibri_runtime::TypedError::new(
                    concat!("std.", $module),
                    "Error",
                    Self::VARIANTS[self as usize],
                    message,
                )
            }
        }
    };
}

pub(crate) use error_kind;
