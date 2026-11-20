//! The engine-neutral runtime pieces of Colibri: values, heap objects, the deep operations on
//! them, and the state an engine shares with its host.
//!
//! The tree-walking interpreter in `colibri-interp` is the only engine today; a bytecode VM is
//! meant to share this crate. The API is workspace-internal and unstable: hosts use
//! `colibri-runtime`, which re-exports what they need.
