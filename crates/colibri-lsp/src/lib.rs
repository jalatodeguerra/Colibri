//! The Colibri language server (`colibri lsp`): a minimal LSP over stdio (DESIGN §16,
//! ARCHITECTURE §14).
//!
//! It checks the package of every open document after the document changes (once the edits
//! pause for [`DEBOUNCE`], or at once when a request needs the result), with the unsaved
//! buffers laid over the files on disk, and publishes the diagnostics of every file of that
//! package. It also answers formatting, hover, go to definition, completion, references, rename and the document
//! outline, and answers a request the client cancelled before it started with `RequestCancelled`.

mod analysis;
mod completion;
mod convert;
mod rename;
mod resolve;
mod server;
mod symbols;

pub use server::{Config, DEBOUNCE, EXIT_WAIT, run_stdio, serve};
