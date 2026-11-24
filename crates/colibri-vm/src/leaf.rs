//! Leaf closures: native loops for the list built-ins that take one.
//!
//! A *leaf* body has no captures, no calls, no allocation, no drops and no loops: it only reads
//! its parameters (scalars, or the scalar fields of a struct), computes with `int`, `float` and
//! `bool`, and returns. It cannot pause, release anything or observe the budget except through
//! its own `Step`s, so running it for each element of a list from native code is
//! unobservable, provided the run is abandoned and the generic path (the prelude's loop in
//! bytecode) takes over, from the start, on anything unusual: an overflow or a division by zero
//! in the body, an element of another shape than the body expects, a budget that would run out,
//! the call limit. Every panic, trace and budget failure is then the generic path's.
//!
//! [`Leaf::build`] runs once, when a chunk is compiled. It checks the instructions against the
//! leaf set, infers a scalar kind for every register (two registers a `Copy` joins share one),
//! and translates the body to [`Ins`], a small register program over raw 64-bit slots (floats as
//! their bits) with no value tags.

use std::rc::Rc;

use colibri_check::hir::Builtin;
use colibri_core::heap::ListObj;
use colibri_core::value::{StructVal, Value};

use crate::bytecode::{Chunk, Op};
use crate::fiber::VmFiber;
use crate::machine::Machine;

/// Registers a leaf program may use.
const MAX_REGS: usize = 256;

/// The scalar kinds of the typed form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum K {
    Int,
    Float,
    Bool,
    /// A struct parameter whose scalar fields the body reads.
    Struct,
}

/// What a parameter must hold for the program to run, and how it is loaded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Param {
    /// Not used by an operation: any value.
    Any,
    Int,
    Float,
    Bool,
    Struct,
}

/// What the program returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ret {
    Int,
    Float,
    Bool,
    /// The parameter itself, unchanged.
    Param(u8),
    /// A field of the struct in a parameter, whatever its type, unchanged.
    Field {
        p: u8,
        idx: u16,
    },
}

/// One instruction of the typed form. Registers hold raw 64-bit slots.
#[derive(Clone, Copy, Debug)]
enum Ins {
    Const {
        d: u8,
        v: i64,
    },
    Mov {
        d: u8,
        a: u8,
    },
    AddI {
        d: u8,
        a: u8,
        b: u8,
    },
    SubI {
        d: u8,
        a: u8,
        b: u8,
    },
    MulI {
        d: u8,
        a: u8,
        b: u8,
    },
    DivI {
        d: u8,
        a: u8,
        b: u8,
    },
    RemI {
        d: u8,
        a: u8,
        b: u8,
    },
    AddIK {
        d: u8,
        a: u8,
        k: i64,
    },
    MulIK {
        d: u8,
        a: u8,
        k: i64,
    },
    DivIK {
        d: u8,
        a: u8,
        k: i64,
    },
    RemIK {
        d: u8,
        a: u8,
        k: i64,
    },
    EqIK {
        d: u8,
        a: u8,
        k: i64,
    },
    NeIK {
        d: u8,
        a: u8,
        k: i64,
    },
    NegI {
        d: u8,
        a: u8,
    },
    LtI {
        d: u8,
        a: u8,
        b: u8,
    },
    LeI {
        d: u8,
        a: u8,
        b: u8,
    },
    EqI {
        d: u8,
        a: u8,
        b: u8,
    },
    NeI {
        d: u8,
        a: u8,
        b: u8,
    },
    AddF {
        d: u8,
        a: u8,
        b: u8,
    },
    SubF {
        d: u8,
        a: u8,
        b: u8,
    },
    MulF {
        d: u8,
        a: u8,
        b: u8,
    },
    DivF {
        d: u8,
        a: u8,
        b: u8,
    },
    NegF {
        d: u8,
        a: u8,
    },
    LtF {
        d: u8,
        a: u8,
        b: u8,
    },
    LeF {
        d: u8,
        a: u8,
        b: u8,
    },
    EqF {
        d: u8,
        a: u8,
        b: u8,
    },
    NeF {
        d: u8,
        a: u8,
        b: u8,
    },
    Not {
        d: u8,
        a: u8,
    },
    /// `d = field idx of the struct in parameter p`, as the kind the body uses it as.
    FieldI {
        d: u8,
        p: u8,
        idx: u16,
    },
    FieldF {
        d: u8,
        p: u8,
        idx: u16,
    },
    FieldB {
        d: u8,
        p: u8,
        idx: u16,
    },
    Jmp {
        to: u16,
    },
    JmpIf {
        r: u8,
        to: u16,
    },
    JmpIfNot {
        r: u8,
        to: u16,
    },
    JNotLtI {
        a: u8,
        b: u8,
        to: u16,
    },
    JNotLeI {
        a: u8,
        b: u8,
        to: u16,
    },
    JNotEqI {
        a: u8,
        b: u8,
        to: u16,
    },
    JNotNeI {
        a: u8,
        b: u8,
        to: u16,
    },
    JNotLtK {
        a: u8,
        k: i64,
        to: u16,
    },
    JNotLeK {
        a: u8,
        k: i64,
        to: u16,
    },
    JNotGtK {
        a: u8,
        k: i64,
        to: u16,
    },
    JNotGeK {
        a: u8,
        k: i64,
        to: u16,
    },
    JNotEqK {
        a: u8,
        k: i64,
        to: u16,
    },
    JNotNeK {
        a: u8,
        k: i64,
        to: u16,
    },
    /// A budget `Step` inside the body.
    Step {
        n: u32,
    },
    Ret {
        s: u8,
    },
}

/// The typed form of a leaf body.
#[derive(Debug)]
pub(crate) struct Leaf {
    code: Box<[Ins]>,
    nparams: u8,
    params: [Param; 2],
    ret: Ret,
    /// What each call charges before the body runs: the call, and the body's opening `Step`.
    steps: u32,
    /// The most the `Step`s inside the body charge on any path.
    max_body_steps: u32,
}

/// Raw register file: a `u8` indexes it without a bounds check.
type Regs = [i64; MAX_REGS];

#[inline(always)]
fn fl(x: i64) -> f64 {
    f64::from_bits(x as u64)
}

#[inline(always)]
fn bits(x: f64) -> i64 {
    x.to_bits() as i64
}

/// Registers that must hold the same kind, with the kind once known.
struct Kinds {
    parent: Vec<usize>,
    kind: Vec<Option<K>>,
}

impl Kinds {
    fn new(n: usize) -> Kinds {
        Kinds {
            parent: (0..n).collect(),
            kind: vec![None; n],
        }
    }

    fn find(&mut self, mut r: usize) -> usize {
        while self.parent[r] != r {
            self.parent[r] = self.parent[self.parent[r]];
            r = self.parent[r];
        }
        r
    }

    /// Joins two registers; `false` when their kinds disagree.
    fn union(&mut self, a: usize, b: usize) -> bool {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return true;
        }
        match (self.kind[a], self.kind[b]) {
            (Some(x), Some(y)) if x != y => return false,
            (None, Some(y)) => self.kind[a] = Some(y),
            _ => {}
        }
        self.parent[b] = a;
        true
    }

    /// The register holds `k`; `false` when it holds another.
    fn set(&mut self, r: usize, k: K) -> bool {
        let r = self.find(r);
        match self.kind[r] {
            Some(x) => x == k,
            None => {
                self.kind[r] = Some(k);
                true
            }
        }
    }

    fn get(&mut self, r: usize) -> Option<K> {
        let r = self.find(r);
        self.kind[r]
    }
}

impl Leaf {
    /// The typed form of the body of `chunk`, when it is a leaf.
    pub(crate) fn build(chunk: &Chunk) -> Option<Leaf> {
        let nparams = usize::from(chunk.nparams);
        if chunk.hidden
            || !chunk.body.captures.is_empty()
            || !(1..=2).contains(&nparams)
            || usize::from(chunk.nregs) > MAX_REGS
            || !chunk.unwind.is_empty()
        {
            return None;
        }
        let (steps, start) = match chunk.code.first() {
            Some(Op::Step { n }) => (1 + *n, 1),
            _ => (1, 0),
        };
        let body = &chunk.code[start..];
        if body.len() > 4096 {
            return None;
        }
        let reach = reachable(body)?;
        let mut kinds = Kinds::new(MAX_REGS);
        let reg =
            |r: u16| -> Option<usize> { (usize::from(r) < MAX_REGS).then_some(usize::from(r)) };
        // Pass 1: the kinds. Every operation says what it needs of its operands and gives.
        let mut returns = Vec::new();
        for (j, op) in body.iter().enumerate() {
            if !reach[j] {
                continue;
            }
            let ok = match *op {
                Op::LoadBool { d, .. } => kinds.set(reg(d)?, K::Bool),
                Op::LoadInt { d, .. } => kinds.set(reg(d)?, K::Int),
                Op::LoadK { d, k } => {
                    let kind = match chunk.consts.get(k as usize)? {
                        Value::Int(_) => K::Int,
                        Value::Float(_) => K::Float,
                        Value::Bool(_) => K::Bool,
                        _ => return None,
                    };
                    kinds.set(reg(d)?, kind)
                }
                Op::Copy { d, s } | Op::Move { d, s } => kinds.union(reg(d)?, reg(s)?),
                Op::AddI { d, a, b }
                | Op::SubI { d, a, b }
                | Op::MulI { d, a, b }
                | Op::DivI { d, a, b }
                | Op::RemI { d, a, b } => {
                    kinds.set(reg(d)?, K::Int)
                        && kinds.set(reg(a)?, K::Int)
                        && kinds.set(reg(b)?, K::Int)
                }
                Op::AddIK { d, a, .. }
                | Op::MulIK { d, a, .. }
                | Op::DivIK { d, a, .. }
                | Op::RemIK { d, a, .. }
                | Op::NegI { d, a } => kinds.set(reg(d)?, K::Int) && kinds.set(reg(a)?, K::Int),
                Op::EqIK { d, a, .. } | Op::NeIK { d, a, .. } => {
                    kinds.set(reg(d)?, K::Bool) && kinds.set(reg(a)?, K::Int)
                }
                Op::LtI { d, a, b }
                | Op::LeI { d, a, b }
                | Op::EqI { d, a, b }
                | Op::NeI { d, a, b } => {
                    kinds.set(reg(d)?, K::Bool)
                        && kinds.set(reg(a)?, K::Int)
                        && kinds.set(reg(b)?, K::Int)
                }
                Op::AddF { d, a, b }
                | Op::SubF { d, a, b }
                | Op::MulF { d, a, b }
                | Op::DivF { d, a, b } => {
                    kinds.set(reg(d)?, K::Float)
                        && kinds.set(reg(a)?, K::Float)
                        && kinds.set(reg(b)?, K::Float)
                }
                Op::NegF { d, a } => kinds.set(reg(d)?, K::Float) && kinds.set(reg(a)?, K::Float),
                Op::LtF { d, a, b }
                | Op::LeF { d, a, b }
                | Op::EqF { d, a, b }
                | Op::NeF { d, a, b } => {
                    kinds.set(reg(d)?, K::Bool)
                        && kinds.set(reg(a)?, K::Float)
                        && kinds.set(reg(b)?, K::Float)
                }
                Op::Not { d, a } => kinds.set(reg(d)?, K::Bool) && kinds.set(reg(a)?, K::Bool),
                Op::GetField { s, .. } => kinds.set(reg(s)?, K::Struct),
                Op::Jmp { off } => forward(j, i64::from(off), body.len()).is_some(),
                Op::JmpIf { r, off } | Op::JmpIfNot { r, off } => {
                    kinds.set(reg(r)?, K::Bool) && forward(j, i64::from(off), body.len()).is_some()
                }
                Op::JmpNotLtI { a, b, off }
                | Op::JmpNotLeI { a, b, off }
                | Op::JmpNotEqI { a, b, off }
                | Op::JmpNotNeI { a, b, off } => {
                    kinds.set(reg(a)?, K::Int)
                        && kinds.set(reg(b)?, K::Int)
                        && forward(j, i64::from(off), body.len()).is_some()
                }
                Op::JmpNotLtIK { a, off, .. }
                | Op::JmpNotLeIK { a, off, .. }
                | Op::JmpNotGtIK { a, off, .. }
                | Op::JmpNotGeIK { a, off, .. }
                | Op::JmpNotEqIK { a, off, .. }
                | Op::JmpNotNeIK { a, off, .. } => {
                    kinds.set(reg(a)?, K::Int) && forward(j, i64::from(off), body.len()).is_some()
                }
                Op::Step { .. } => true,
                Op::Return { s } => {
                    returns.push(reg(s)?);
                    true
                }
                _ => false,
            };
            if !ok {
                return None;
            }
        }
        let first = *returns.first()?;
        for &r in &returns[1..] {
            if !kinds.union(first, r) {
                return None;
            }
        }
        // The parameters: the kind the body needs of each, none when it only passes it on.
        let mut params = [Param::Any; 2];
        for (p, slot) in params.iter_mut().enumerate().take(nparams) {
            *slot = match kinds.get(p) {
                None => Param::Any,
                Some(K::Int) => Param::Int,
                Some(K::Float) => Param::Float,
                Some(K::Bool) => Param::Bool,
                Some(K::Struct) => Param::Struct,
            };
        }
        // The parameter that a struct register stands for.
        let struct_param = |kinds: &mut Kinds, r: usize| -> Option<u8> {
            let root = kinds.find(r);
            let mut found = None;
            for p in 0..nparams {
                if kinds.find(p) == root {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(p as u8);
                }
            }
            found
        };
        // What is returned: a computed scalar, or the one parameter the returned register stands
        // for.
        let ret = match kinds.get(first) {
            Some(K::Int) => Ret::Int,
            Some(K::Float) => Ret::Float,
            Some(K::Bool) => Ret::Bool,
            Some(K::Struct) => return None,
            None => {
                let root = kinds.find(first);
                let mut found = None;
                for p in 0..nparams {
                    if kinds.find(p) == root {
                        if found.is_some() {
                            return None;
                        }
                        found = Some(p as u8);
                    }
                }
                // Or a field that is only passed on: the one `GetField` that fills the register.
                let mut field = None;
                for (j, op) in body.iter().enumerate() {
                    if let Op::GetField { d, s, idx } = *op
                        && reach[j]
                        && kinds.find(usize::from(d)) == root
                    {
                        let at = (struct_param(&mut kinds, usize::from(s))?, idx);
                        if field.is_some_and(|f| f != at) {
                            return None;
                        }
                        field = Some(at);
                    }
                }
                match (found, field) {
                    (Some(p), None) => Ret::Param(p),
                    (None, Some((p, idx))) => Ret::Field { p, idx },
                    _ => return None,
                }
            }
        };
        // Pass 2: the typed form.
        let mut code = Vec::with_capacity(body.len());
        let r8 = |r: u16| r as u8;
        for (j, op) in body.iter().enumerate() {
            let to = |off: i64| forward(j, off, body.len()).map(|t| t as u16);
            if !reach[j] {
                // Never run (the `ReturnUnit` that ends a body whose paths all return).
                code.push(Ins::Ret { s: 0 });
                continue;
            }
            let ins = match *op {
                Op::LoadBool { d, b } => Ins::Const {
                    d: r8(d),
                    v: i64::from(b),
                },
                Op::LoadInt { d, i } => Ins::Const {
                    d: r8(d),
                    v: i64::from(i),
                },
                Op::LoadK { d, k } => Ins::Const {
                    d: r8(d),
                    v: match chunk.consts.get(k as usize)? {
                        Value::Int(i) => *i,
                        Value::Float(f) => bits(*f),
                        Value::Bool(b) => i64::from(*b),
                        _ => return None,
                    },
                },
                Op::Copy { d, s } | Op::Move { d, s } => Ins::Mov { d: r8(d), a: r8(s) },
                Op::AddI { d, a, b } => Ins::AddI {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::SubI { d, a, b } => Ins::SubI {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::MulI { d, a, b } => Ins::MulI {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::DivI { d, a, b } => Ins::DivI {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::RemI { d, a, b } => Ins::RemI {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::AddIK { d, a, i } => Ins::AddIK {
                    d: r8(d),
                    a: r8(a),
                    k: i64::from(i),
                },
                Op::MulIK { d, a, i } => Ins::MulIK {
                    d: r8(d),
                    a: r8(a),
                    k: i64::from(i),
                },
                // The compiler emits these only for a constant that is neither 0 nor -1.
                Op::DivIK { d, a, i } if i != 0 && i != -1 => Ins::DivIK {
                    d: r8(d),
                    a: r8(a),
                    k: i64::from(i),
                },
                Op::RemIK { d, a, i } if i != 0 && i != -1 => Ins::RemIK {
                    d: r8(d),
                    a: r8(a),
                    k: i64::from(i),
                },
                Op::EqIK { d, a, i } => Ins::EqIK {
                    d: r8(d),
                    a: r8(a),
                    k: i64::from(i),
                },
                Op::NeIK { d, a, i } => Ins::NeIK {
                    d: r8(d),
                    a: r8(a),
                    k: i64::from(i),
                },
                Op::NegI { d, a } => Ins::NegI { d: r8(d), a: r8(a) },
                Op::LtI { d, a, b } => Ins::LtI {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::LeI { d, a, b } => Ins::LeI {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::EqI { d, a, b } => Ins::EqI {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::NeI { d, a, b } => Ins::NeI {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::AddF { d, a, b } => Ins::AddF {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::SubF { d, a, b } => Ins::SubF {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::MulF { d, a, b } => Ins::MulF {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::DivF { d, a, b } => Ins::DivF {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::NegF { d, a } => Ins::NegF { d: r8(d), a: r8(a) },
                Op::LtF { d, a, b } => Ins::LtF {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::LeF { d, a, b } => Ins::LeF {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::EqF { d, a, b } => Ins::EqF {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::NeF { d, a, b } => Ins::NeF {
                    d: r8(d),
                    a: r8(a),
                    b: r8(b),
                },
                Op::Not { d, a } => Ins::Not { d: r8(d), a: r8(a) },
                Op::GetField { d, s, idx } => {
                    let p = struct_param(&mut kinds, usize::from(s))?;
                    match kinds.get(usize::from(d)) {
                        Some(K::Int) => Ins::FieldI { d: r8(d), p, idx },
                        Some(K::Float) => Ins::FieldF { d: r8(d), p, idx },
                        Some(K::Bool) => Ins::FieldB { d: r8(d), p, idx },
                        Some(K::Struct) => return None,
                        // Only passed on to the result (`Ret::Field`): nothing to load.
                        None => Ins::Const { d: r8(d), v: 0 },
                    }
                }
                Op::Jmp { off } => Ins::Jmp {
                    to: to(i64::from(off))?,
                },
                Op::JmpIf { r, off } => Ins::JmpIf {
                    r: r8(r),
                    to: to(i64::from(off))?,
                },
                Op::JmpIfNot { r, off } => Ins::JmpIfNot {
                    r: r8(r),
                    to: to(i64::from(off))?,
                },
                Op::JmpNotLtI { a, b, off } => Ins::JNotLtI {
                    a: r8(a),
                    b: r8(b),
                    to: to(i64::from(off))?,
                },
                Op::JmpNotLeI { a, b, off } => Ins::JNotLeI {
                    a: r8(a),
                    b: r8(b),
                    to: to(i64::from(off))?,
                },
                Op::JmpNotEqI { a, b, off } => Ins::JNotEqI {
                    a: r8(a),
                    b: r8(b),
                    to: to(i64::from(off))?,
                },
                Op::JmpNotNeI { a, b, off } => Ins::JNotNeI {
                    a: r8(a),
                    b: r8(b),
                    to: to(i64::from(off))?,
                },
                Op::JmpNotLtIK { a, k, off } => Ins::JNotLtK {
                    a: r8(a),
                    k: i64::from(k),
                    to: to(i64::from(off))?,
                },
                Op::JmpNotLeIK { a, k, off } => Ins::JNotLeK {
                    a: r8(a),
                    k: i64::from(k),
                    to: to(i64::from(off))?,
                },
                Op::JmpNotGtIK { a, k, off } => Ins::JNotGtK {
                    a: r8(a),
                    k: i64::from(k),
                    to: to(i64::from(off))?,
                },
                Op::JmpNotGeIK { a, k, off } => Ins::JNotGeK {
                    a: r8(a),
                    k: i64::from(k),
                    to: to(i64::from(off))?,
                },
                Op::JmpNotEqIK { a, k, off } => Ins::JNotEqK {
                    a: r8(a),
                    k: i64::from(k),
                    to: to(i64::from(off))?,
                },
                Op::JmpNotNeIK { a, k, off } => Ins::JNotNeK {
                    a: r8(a),
                    k: i64::from(k),
                    to: to(i64::from(off))?,
                },
                Op::Step { n } => Ins::Step { n },
                Op::Return { s } => Ins::Ret { s: r8(s) },
                _ => return None,
            };
            code.push(ins);
        }
        // The most the body's own steps can charge: the heaviest path of a forward-only program.
        let mut heaviest = vec![0u32; code.len() + 1];
        for j in (0..code.len()).rev() {
            let here = match code[j] {
                Ins::Step { n } => n,
                _ => 0,
            };
            let onward = match code[j] {
                Ins::Ret { .. } => 0,
                Ins::Jmp { to } => heaviest[usize::from(to)],
                Ins::JmpIf { to, .. }
                | Ins::JmpIfNot { to, .. }
                | Ins::JNotLtI { to, .. }
                | Ins::JNotLeI { to, .. }
                | Ins::JNotEqI { to, .. }
                | Ins::JNotNeI { to, .. }
                | Ins::JNotLtK { to, .. }
                | Ins::JNotLeK { to, .. }
                | Ins::JNotGtK { to, .. }
                | Ins::JNotGeK { to, .. }
                | Ins::JNotEqK { to, .. }
                | Ins::JNotNeK { to, .. } => heaviest[usize::from(to)].max(heaviest[j + 1]),
                _ => heaviest[j + 1],
            };
            heaviest[j] = here.checked_add(onward)?;
        }
        Some(Leaf {
            code: code.into_boxed_slice(),
            nparams: nparams as u8,
            params,
            ret,
            steps,
            max_body_steps: heaviest[0],
        })
    }

    /// Runs the body on the loaded registers. `None` when something is unusual: an overflow, a
    /// division by zero. `extra` counts the body's own steps.
    #[inline(always)]
    fn run(&self, r: &mut Regs, st: [Option<&StructVal>; 2], extra: &mut u64) -> Option<i64> {
        let code = &*self.code;
        let mut pc = 0usize;
        loop {
            let ins = *code.get(pc)?;
            pc += 1;
            match ins {
                Ins::Const { d, v } => r[usize::from(d)] = v,
                Ins::Mov { d, a } => r[usize::from(d)] = r[usize::from(a)],
                Ins::AddI { d, a, b } => {
                    r[usize::from(d)] = r[usize::from(a)].checked_add(r[usize::from(b)])?;
                }
                Ins::SubI { d, a, b } => {
                    r[usize::from(d)] = r[usize::from(a)].checked_sub(r[usize::from(b)])?;
                }
                Ins::MulI { d, a, b } => {
                    r[usize::from(d)] = r[usize::from(a)].checked_mul(r[usize::from(b)])?;
                }
                Ins::DivI { d, a, b } => {
                    r[usize::from(d)] = r[usize::from(a)].checked_div(r[usize::from(b)])?;
                }
                Ins::RemI { d, a, b } => {
                    r[usize::from(d)] = r[usize::from(a)].checked_rem(r[usize::from(b)])?;
                }
                Ins::AddIK { d, a, k } => r[usize::from(d)] = r[usize::from(a)].checked_add(k)?,
                Ins::MulIK { d, a, k } => r[usize::from(d)] = r[usize::from(a)].checked_mul(k)?,
                Ins::DivIK { d, a, k } => r[usize::from(d)] = r[usize::from(a)].wrapping_div(k),
                Ins::RemIK { d, a, k } => r[usize::from(d)] = r[usize::from(a)].wrapping_rem(k),
                Ins::EqIK { d, a, k } => r[usize::from(d)] = i64::from(r[usize::from(a)] == k),
                Ins::NeIK { d, a, k } => r[usize::from(d)] = i64::from(r[usize::from(a)] != k),
                Ins::NegI { d, a } => r[usize::from(d)] = r[usize::from(a)].checked_neg()?,
                Ins::LtI { d, a, b } => {
                    r[usize::from(d)] = i64::from(r[usize::from(a)] < r[usize::from(b)]);
                }
                Ins::LeI { d, a, b } => {
                    r[usize::from(d)] = i64::from(r[usize::from(a)] <= r[usize::from(b)]);
                }
                Ins::EqI { d, a, b } => {
                    r[usize::from(d)] = i64::from(r[usize::from(a)] == r[usize::from(b)]);
                }
                Ins::NeI { d, a, b } => {
                    r[usize::from(d)] = i64::from(r[usize::from(a)] != r[usize::from(b)]);
                }
                Ins::AddF { d, a, b } => {
                    r[usize::from(d)] = bits(fl(r[usize::from(a)]) + fl(r[usize::from(b)]));
                }
                Ins::SubF { d, a, b } => {
                    r[usize::from(d)] = bits(fl(r[usize::from(a)]) - fl(r[usize::from(b)]));
                }
                Ins::MulF { d, a, b } => {
                    r[usize::from(d)] = bits(fl(r[usize::from(a)]) * fl(r[usize::from(b)]));
                }
                Ins::DivF { d, a, b } => {
                    r[usize::from(d)] = bits(fl(r[usize::from(a)]) / fl(r[usize::from(b)]));
                }
                Ins::NegF { d, a } => r[usize::from(d)] = bits(-fl(r[usize::from(a)])),
                Ins::LtF { d, a, b } => {
                    r[usize::from(d)] = i64::from(fl(r[usize::from(a)]) < fl(r[usize::from(b)]));
                }
                Ins::LeF { d, a, b } => {
                    r[usize::from(d)] = i64::from(fl(r[usize::from(a)]) <= fl(r[usize::from(b)]));
                }
                Ins::EqF { d, a, b } => {
                    r[usize::from(d)] = i64::from(fl(r[usize::from(a)]) == fl(r[usize::from(b)]));
                }
                Ins::NeF { d, a, b } => {
                    r[usize::from(d)] = i64::from(fl(r[usize::from(a)]) != fl(r[usize::from(b)]));
                }
                Ins::Not { d, a } => r[usize::from(d)] = r[usize::from(a)] ^ 1,
                Ins::FieldI { d, p, idx } => {
                    let Some(Value::Int(x)) = st[usize::from(p)]?.fields.get(usize::from(idx))
                    else {
                        return None;
                    };
                    r[usize::from(d)] = *x;
                }
                Ins::FieldF { d, p, idx } => {
                    let Some(Value::Float(x)) = st[usize::from(p)]?.fields.get(usize::from(idx))
                    else {
                        return None;
                    };
                    r[usize::from(d)] = bits(*x);
                }
                Ins::FieldB { d, p, idx } => {
                    let Some(Value::Bool(x)) = st[usize::from(p)]?.fields.get(usize::from(idx))
                    else {
                        return None;
                    };
                    r[usize::from(d)] = i64::from(*x);
                }
                Ins::Jmp { to } => pc = usize::from(to),
                Ins::JmpIf { r: c, to } => {
                    if r[usize::from(c)] != 0 {
                        pc = usize::from(to);
                    }
                }
                Ins::JmpIfNot { r: c, to } => {
                    if r[usize::from(c)] == 0 {
                        pc = usize::from(to);
                    }
                }
                Ins::JNotLtI { a, b, to } => {
                    if r[usize::from(a)] >= r[usize::from(b)] {
                        pc = usize::from(to);
                    }
                }
                Ins::JNotLeI { a, b, to } => {
                    if r[usize::from(a)] > r[usize::from(b)] {
                        pc = usize::from(to);
                    }
                }
                Ins::JNotEqI { a, b, to } => {
                    if r[usize::from(a)] != r[usize::from(b)] {
                        pc = usize::from(to);
                    }
                }
                Ins::JNotNeI { a, b, to } => {
                    if r[usize::from(a)] == r[usize::from(b)] {
                        pc = usize::from(to);
                    }
                }
                Ins::JNotLtK { a, k, to } => {
                    if r[usize::from(a)] >= k {
                        pc = usize::from(to);
                    }
                }
                Ins::JNotLeK { a, k, to } => {
                    if r[usize::from(a)] > k {
                        pc = usize::from(to);
                    }
                }
                Ins::JNotGtK { a, k, to } => {
                    if r[usize::from(a)] <= k {
                        pc = usize::from(to);
                    }
                }
                Ins::JNotGeK { a, k, to } => {
                    if r[usize::from(a)] < k {
                        pc = usize::from(to);
                    }
                }
                Ins::JNotEqK { a, k, to } => {
                    if r[usize::from(a)] != k {
                        pc = usize::from(to);
                    }
                }
                Ins::JNotNeK { a, k, to } => {
                    if r[usize::from(a)] == k {
                        pc = usize::from(to);
                    }
                }
                Ins::Step { n } => *extra += u64::from(n),
                Ins::Ret { s } => return Some(r[usize::from(s)]),
            }
        }
    }

    /// Loads parameter `p` from the element: `None` when the element is not what the body reads.
    #[inline(always)]
    fn load<'a>(&self, p: usize, v: &'a Value, r: &mut Regs) -> Option<Option<&'a StructVal>> {
        match (self.params[p], v) {
            (Param::Any, _) => {}
            (Param::Int, Value::Int(x)) => r[p] = *x,
            (Param::Float, Value::Float(x)) => r[p] = bits(*x),
            (Param::Bool, Value::Bool(x)) => r[p] = i64::from(*x),
            (Param::Struct, Value::Struct(s)) => return Some(Some(s)),
            _ => return None,
        }
        Some(None)
    }

    /// The result as a value.
    #[inline(always)]
    fn boxed(&self, raw: i64, args: [&Value; 2], st: [Option<&StructVal>; 2]) -> Option<Value> {
        Some(match self.ret {
            Ret::Int => Value::Int(raw),
            Ret::Float => Value::Float(fl(raw)),
            Ret::Bool => Value::Bool(raw != 0),
            Ret::Param(p) => args[usize::from(p)].dup(),
            Ret::Field { p, idx } => st[usize::from(p)]?.fields.get(usize::from(idx))?.dup(),
        })
    }

    /// The truth of a predicate's result.
    #[inline(always)]
    fn truth(&self, raw: i64, arg: &Value, st: [Option<&StructVal>; 2]) -> Option<bool> {
        match self.ret {
            Ret::Bool => Some(raw != 0),
            Ret::Param(0) => match arg {
                Value::Bool(b) => Some(*b),
                _ => None,
            },
            Ret::Field { p: 0, idx } => match st[0]?.fields.get(usize::from(idx))? {
                Value::Bool(b) => Some(*b),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Which instructions of a forward-only body can run, from the first. `None` when a jump goes
/// back or out, or when the last reachable instruction falls off the end.
fn reachable(body: &[Op]) -> Option<Vec<bool>> {
    let mut reach = vec![false; body.len() + 1];
    reach[0] = true;
    for (j, op) in body.iter().enumerate() {
        if !reach[j] {
            continue;
        }
        let jump = |off: i64| forward(j, off, body.len());
        match *op {
            Op::Return { .. } | Op::ReturnUnit => {}
            Op::Jmp { off } => reach[jump(i64::from(off))?] = true,
            Op::JmpIf { off, .. } | Op::JmpIfNot { off, .. } => {
                reach[jump(i64::from(off))?] = true;
                reach[j + 1] = true;
            }
            Op::JmpNotLtI { off, .. }
            | Op::JmpNotLeI { off, .. }
            | Op::JmpNotEqI { off, .. }
            | Op::JmpNotNeI { off, .. }
            | Op::JmpNotLtIK { off, .. }
            | Op::JmpNotLeIK { off, .. }
            | Op::JmpNotGtIK { off, .. }
            | Op::JmpNotGeIK { off, .. }
            | Op::JmpNotEqIK { off, .. }
            | Op::JmpNotNeIK { off, .. } => {
                reach[jump(i64::from(off))?] = true;
                reach[j + 1] = true;
            }
            _ => reach[j + 1] = true,
        }
    }
    // Falling off the end, or a `ReturnUnit` that can run, is no leaf (the closure returns `()`).
    let unit = body
        .iter()
        .zip(&reach)
        .any(|(op, &r)| r && matches!(op, Op::ReturnUnit));
    (!reach[body.len()] && !unit).then_some(reach)
}

/// The index, in a body of `len` instructions, that a jump at `j` by `off` reaches, when it
/// goes forward and stays inside.
fn forward(j: usize, off: i64, len: usize) -> Option<usize> {
    let to = j as i64 + 1 + off;
    (off >= 0 && to >= 0 && (to as usize) < len).then_some(to as usize)
}

impl Machine {
    /// The closure built-ins of lists, run by a native loop when the closure is a leaf. `true`
    /// when the result is in register `d`; `false` leaves everything as it was (nothing is
    /// charged, nothing is held), and the caller runs the prelude's function, which makes every
    /// failure what it has always been. Called after the built-in's own charge.
    #[inline(never)]
    pub(crate) fn leaf_hof(
        &mut self,
        fiber: &mut VmFiber,
        base: usize,
        a: u16,
        d: u16,
        which: Builtin,
    ) -> bool {
        if self.leaf_off {
            return false;
        }
        // A Drop hook that panicked earlier in this expression is folded in at the first closure
        // call of the generic path; the loop here would run past it.
        if !self.core.hook_panics.is_empty() {
            return false;
        }
        let at = base + usize::from(a);
        let fold = which == Builtin::ListFold;
        let (Some(Value::List(list)), Some(Value::Closure(c))) = (
            fiber.regs.get(at),
            fiber.regs.get(at + 1 + usize::from(fold)),
        ) else {
            return false;
        };
        // The body the generic path would call for this closure (a closure made before a hot
        // reload gets the replacement's, a body that does not compile is its failure).
        let Ok(chunk) = self.closure_chunk(c) else {
            return false;
        };
        let Some(leaf) = chunk.leaf.as_ref() else {
            return false;
        };
        if usize::from(leaf.nparams) != 1 + usize::from(fold) {
            return false;
        }
        // The closure's frame would be one deeper than the call: the generic path refuses it.
        if fiber.visible_depth() >= self.max_depth || fiber.frames.len() >= 2 * self.max_depth + 64
        {
            return false;
        }
        let n = list.len() as u64;
        let per = u64::from(leaf.steps) + u64::from(leaf.max_body_steps);
        let left = u64::try_from(self.core.budget.left).unwrap_or(0);
        if n.checked_mul(per).is_none_or(|need| left < need) {
            return false;
        }
        let list = Rc::clone(list);
        let init = if fold {
            fiber.regs.get(at + 1).cloned()
        } else {
            None
        };
        let Some((result, calls, extra)) = Self::leaf_loop(&list, leaf, which, init) else {
            return false;
        };
        let charged = calls
            .saturating_mul(u64::from(leaf.steps))
            .saturating_add(extra);
        self.core.budget.left -= i64::try_from(charged).unwrap_or(i64::MAX);
        self.leaf_runs += 1;
        let value = match result {
            LeafResult::Value(v) => v,
            LeafResult::Option(v) => self.core.option(v),
        };
        Self::put_reg(fiber, base + usize::from(d), value);
        true
    }

    /// Runs `leaf` over the elements of `list` for `which`: the result, how many calls it made,
    /// and what the bodies' own steps charged. `None` when anything is unusual.
    fn leaf_loop(
        list: &Rc<ListObj>,
        leaf: &Leaf,
        which: Builtin,
        init: Option<Value>,
    ) -> Option<(LeafResult, u64, u64)> {
        // The loop holds the list as the prelude's `for` does.
        let _hold = list.iter_hold();
        let mut r: Regs = [0; MAX_REGS];
        let mut extra = 0u64;
        list.with(|items| {
            let mut calls = 0u64;
            let result = match which {
                Builtin::ListMap => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        let st = leaf.load(0, item, &mut r)?;
                        let raw = leaf.run(&mut r, [st, None], &mut extra)?;
                        out.push(leaf.boxed(raw, [item, item], [st, None])?);
                    }
                    calls = items.len() as u64;
                    LeafResult::Value(Value::List(ListObj::new(out)))
                }
                Builtin::ListFilter => {
                    let mut out = Vec::new();
                    for item in items {
                        let st = leaf.load(0, item, &mut r)?;
                        let raw = leaf.run(&mut r, [st, None], &mut extra)?;
                        if leaf.truth(raw, item, [st, None])? {
                            out.push(item.dup());
                        }
                    }
                    calls = items.len() as u64;
                    LeafResult::Value(Value::List(ListObj::new(out)))
                }
                Builtin::ListAny | Builtin::ListAll => {
                    let any = which == Builtin::ListAny;
                    let mut found = !any;
                    for item in items {
                        let st = leaf.load(0, item, &mut r)?;
                        let raw = leaf.run(&mut r, [st, None], &mut extra)?;
                        calls += 1;
                        if leaf.truth(raw, item, [st, None])? == any {
                            found = any;
                            break;
                        }
                    }
                    LeafResult::Value(Value::Bool(found))
                }
                Builtin::ListFind | Builtin::ListFindIndex => {
                    let mut hit = None;
                    for (i, item) in items.iter().enumerate() {
                        let st = leaf.load(0, item, &mut r)?;
                        let raw = leaf.run(&mut r, [st, None], &mut extra)?;
                        calls += 1;
                        if leaf.truth(raw, item, [st, None])? {
                            hit = Some(if which == Builtin::ListFind {
                                item.dup()
                            } else {
                                Value::Int(i as i64)
                            });
                            break;
                        }
                    }
                    LeafResult::Option(hit)
                }
                Builtin::ListFold => {
                    // The accumulator is a raw slot of the kind the body returns.
                    let init = init?;
                    let mut acc = match (leaf.ret, &init) {
                        (Ret::Int, Value::Int(x)) => *x,
                        (Ret::Float, Value::Float(x)) => bits(*x),
                        (Ret::Bool, Value::Bool(x)) => i64::from(*x),
                        _ => return None,
                    };
                    if !matches!(
                        (leaf.params[0], leaf.ret),
                        (Param::Any, _)
                            | (Param::Int, Ret::Int)
                            | (Param::Float, Ret::Float)
                            | (Param::Bool, Ret::Bool)
                    ) {
                        return None;
                    }
                    for item in items {
                        r[0] = acc;
                        let st = leaf.load(1, item, &mut r)?;
                        acc = leaf.run(&mut r, [None, st], &mut extra)?;
                    }
                    calls = items.len() as u64;
                    LeafResult::Value(match leaf.ret {
                        Ret::Int => Value::Int(acc),
                        Ret::Float => Value::Float(fl(acc)),
                        _ => Value::Bool(acc != 0),
                    })
                }
                _ => return None,
            };
            Some((result, calls, extra))
        })
    }
}

enum LeafResult {
    Value(Value),
    Option(Option<Value>),
}
