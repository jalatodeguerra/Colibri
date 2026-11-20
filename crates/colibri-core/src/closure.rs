//! Closure objects and variable cells (ARCHITECTURE §3).
//!
//! A closure is a lifted function plus the variables it captured. A *by-value* capture holds a
//! copy of the value, a *cell* capture shares a [`CellObj`] with the frame that owns the
//! variable (and with every other closure that captured it), and a *weak* capture holds a weak
//! reference to a class object. Like the other heap objects these die only through
//! the engine's `release`, so what a closure captured dies when the closure does.

use std::cell::RefCell;
use std::fmt;

use colibri_check::hir::{CaptureMode, FnId, LocalId};

use crate::value::Value;

/// A variable that lives in a shared cell: written by whoever holds the cell, seen by all.
pub struct CellObj {
    value: RefCell<Value>,
}

impl CellObj {
    #[inline]
    pub fn new(value: Value) -> std::rc::Rc<CellObj> {
        std::rc::Rc::new(CellObj {
            value: RefCell::new(value),
        })
    }

    /// The current value (a reference bump for shared values).
    #[inline]
    pub fn get(&self) -> Value {
        self.value.borrow().clone()
    }

    /// Reads the value in place.
    pub fn with<R>(&self, f: impl FnOnce(&Value) -> R) -> R {
        f(&self.value.borrow())
    }

    /// Stores a value and returns the old one, to be released by the caller.
    #[inline]
    pub fn set(&self, v: Value) -> Value {
        std::mem::replace(&mut *self.value.borrow_mut(), v)
    }

    /// Takes the value out, leaving `()`. Only `release` and a moving receiver do this.
    #[inline]
    pub fn take(&self) -> Value {
        self.set(Value::Unit)
    }

    /// Changes the value in place with `f`, which must not run Colibri code.
    pub fn update<R>(&self, f: impl FnOnce(&mut Value) -> R) -> R {
        f(&mut self.value.borrow_mut())
    }
}

impl fmt::Debug for CellObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cell")
    }
}

/// One captured variable.
#[derive(Debug)]
pub struct Captured {
    /// The local of the closure's body that holds it while the closure runs.
    pub inner: LocalId,
    pub mode: CaptureMode,
    /// The copy (`ByValue`), the shared [`Value::Cell`] (`Cell`) or the [`Value::Weak`] (`Weak`).
    pub value: Value,
}

/// A closure: the lifted function and its captured variables.
#[derive(Debug)]
pub struct ClosureObj {
    pub f: FnId,
    /// How many times the program was extended (a session extension) when the closure
    /// was made: `f` is an id of that generation of the function table.
    pub generation: u32,
    pub captures: Vec<Captured>,
}

impl ClosureObj {
    /// Moves the captured values out, for destruction. Only `release` calls this.
    pub(crate) fn take_values(&mut self) -> Vec<Value> {
        self.captures
            .iter_mut()
            .map(|c| std::mem::replace(&mut c.value, Value::Unit))
            .collect()
    }
}
