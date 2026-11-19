//! The embedding facade of Colibri. Hosts depend on this crate and write
//! `use colibri_runtime as colibri;`, so the engine behind it (tree-walking
//! interpreter now, bytecode VM later) can change without touching host code.
