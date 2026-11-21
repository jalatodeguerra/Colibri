//! Calls of host functions and the C functions of `extern` blocks (vm-plan §1.10).

use colibri_check::hir::{Arg, Call, Expr, HostFnId};

use super::FnCompiler;
use crate::bytecode::{Op, Reg, Site};

impl FnCompiler<'_> {
    /// A call of host function `id`. The arguments are evaluated into a window like those of a
    /// call of a Colibri function and stay there for the caller to drop at the end of its
    /// statement (the tree-walker keeps them on its temporaries too); the host function gets
    /// references of its own, which it releases.
    pub(super) fn call_host_fn(&mut self, id: HostFnId, c: &Call, e: &Expr, dst: Option<Reg>) {
        let mut args = Vec::new();
        for a in &c.args {
            match a {
                Arg::Value(v) => args.push(v),
                // The tree-walker drops the receiver of a host call on the floor; the checker
                // does not produce one.
                Arg::Place(_) => return self.unsupported("`var self` host functions", e.span),
            }
        }
        let Ok(argc) = u16::try_from(args.len()) else {
            return self.fail("a call with too many arguments");
        };
        let d = match dst {
            Some(d) => d,
            None => self.alloc_for(&e.ty),
        };
        let w = self.eval_window(0, &args, false);
        let site = self.add_site(Site::Host(id, argc, c.type_args.clone().into_boxed_slice()));
        self.emit(
            Op::CallHost {
                d,
                base: w.base,
                site,
            },
            e.span,
        );
        self.free_window(w, e.span);
        if dst.is_none() {
            self.own(d);
            self.free(d, e.span);
        }
    }
}
