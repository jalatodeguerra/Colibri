//! The Colibri language server (`colibri lsp`): a minimal LSP over stdio (DESIGN §16,
//! ARCHITECTURE §14).
//!
//! It checks the package of every open document whenever the document changes, with the
//! unsaved buffers laid over the files on disk, and publishes the diagnostics of every file of
//! that package. It also answers formatting, hover, go to definition and the document outline.

mod analysis;
mod completion;
mod convert;
mod rename;
mod resolve;
mod server;
mod symbols;

pub use server::{Config, run_stdio, serve};
