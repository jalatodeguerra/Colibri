//! `Shared[T]` (DESIGN §14.1, ARCHITECTURE §36.6): the engine's half.
//!
//! The graph, the handle and the per-isolate cache are [`colibri_core::shared`]; this is what
//! needs the machine: copying the value into its frozen graph and rebuilding a frozen copy from
//! it, both on a walk stack.

use std::ops::Range;

use colibri_core::ctl::Eval;
use colibri_core::portable::{RebuildError, Refused, from_portable_frozen};
use colibri_core::prelude::*;
use colibri_core::shared::holds_a_cell;
use colibri_core::value::Value;
use colibri_syntax::Span;

use crate::fiber::VmFiber;
use crate::machine::Machine;
use crate::stacks::{WalkRole, on_walk_stack};

impl Machine {
    /// `Shared(x)`: copies `x` once into a graph that nothing will change, and gives its handle.
    pub(crate) fn new_shared(
        &mut self,
        fiber: &mut VmFiber,
        args: Range<usize>,
        span: Span,
    ) -> Eval {
        // The argument is the intrinsic's: taken out of its register, released after the copy.
        let value = std::mem::replace(&mut fiber.regs[args.start], Value::Unit);
        let budget = self.native_stack_budget;
        let copied = on_walk_stack(WalkRole::Sender, budget, |guard| {
            self.to_portable(&value, guard)
        });
        self.release(value);
        let root = match copied {
            Ok(root) => root,
            Err(e) if e.refused == Refused::MemoryLimit => {
                return Err(self.memory_exceeded(fiber, span));
            }
            Err(e) => return Err(self.panic(fiber, span, e.to_string())),
        };
        if holds_a_cell(&root) {
            return Err(self.panic(
                fiber,
                span,
                "a `Shared` cannot hold a closure that captures a `let mut` variable: its copy is frozen",
            ));
        }
        Ok(self.core.new_shared(root))
    }

    /// `s.get()`: this isolate's frozen copy of the value, built on the first call and kept
    /// after it (the same object every time, for as long as a handle of the `Shared` lives).
    pub(crate) fn shared_copy(&mut self, fiber: &mut VmFiber, recv: &Value, span: Span) -> Eval {
        let Some(inner) = self.core.shared_of(recv) else {
            return Err(self.internal(fiber, span, "`get` of something that is not a `Shared`"));
        };
        if let Some(copy) = self.core.shared_cached(&inner) {
            return Ok(copy);
        }
        // The copies of graphs that are gone go first: a program that makes and drops many
        // `Shared`s holds one frozen copy at a time, not all of them.
        self.free_dead_shared_copies();
        let budget = self.native_stack_budget;
        let built = on_walk_stack(WalkRole::Receiver, budget, |guard| {
            from_portable_frozen(inner.root(), &mut self.core, guard)
        });
        match built {
            Ok(copy) => {
                self.core.shared_cache(&inner, copy.clone());
                Ok(copy)
            }
            Err(RebuildError::MemoryLimit) => Err(self.memory_exceeded(fiber, span)),
            Err(e) => Err(self.panic(
                fiber,
                span,
                format!("a `Shared` value cannot be rebuilt here: {e}"),
            )),
        }
    }

    /// Frees this isolate's copies of the `Shared`s that no handle holds any more, in any isolate.
    pub fn free_dead_shared_copies(&mut self) {
        let dead = self.core.sweep_shared();
        for copy in dead {
            self.release(copy);
        }
    }

    /// Frees every frozen copy this isolate keeps (it is ending).
    pub(crate) fn free_shared_copies(&mut self) {
        for copy in self.core.take_shared_copies() {
            self.release(copy);
        }
    }
}
