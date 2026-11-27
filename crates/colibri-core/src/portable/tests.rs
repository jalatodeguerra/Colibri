//! Tests of the portable deep copy (L15).
//!
//! Two graphs are compared through [`dump`]: a canonical text in which every reference object is
//! numbered by its first visit, so two graphs have the same dump exactly when they are isomorphic,
//! with the same aliasing and the same cycles.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use colibri_check::hir::{CaptureMode, FnId, LocalId, TypeDef, TypeDefKind};
use colibri_check::types::{COGROUP_TYPE, COROUTINE_TYPE, SIGNAL_TYPE, TypeId};
use colibri_syntax::{FileId, Span};

use super::*;
use crate::clone::Cloner;
use crate::hooks::Plain;
use crate::hostheap::HostRef;
use crate::iter::IterObj;

const JOB: TypeId = TypeId(30);
const NODE: TypeId = TypeId(31);
const POINT: TypeId = TypeId(32);
const CONN: TypeId = TypeId(33);
const MSG: TypeId = TypeId(34);

fn core() -> Core {
    Core::new(Box::new(std::io::sink()))
}

fn stack() -> StackGuard {
    StackGuard::here(1 << 20)
}

fn def(name: &str, module: Option<&str>, kind: TypeDefKind) -> TypeDef {
    TypeDef {
        name: name.into(),
        span: Span::new(FileId(0), 0, 0),
        params: Vec::new(),
        kind,
        module: module.map(str::to_string),
        open: false,
    }
}

fn field(name: &str) -> colibri_check::hir::FieldDef {
    colibri_check::hir::FieldDef {
        name: name.into(),
        ty: colibri_check::types::Ty::Bool,
        span: Span::new(FileId(0), 0, 0),
        weak: false,
        default: None,
        rename: None,
        skip: false,
    }
}

/// A core that knows the types of these tests (the table is indexed by `TypeId`).
fn named_core() -> Core {
    use colibri_check::hir::VariantDef;
    let mut c = core();
    let mut table: Vec<TypeDef> = (0..30)
        .map(|i| def(&format!("T{i}"), None, TypeDefKind::HostClass))
        .collect();
    table.push(def(
        "Job",
        None,
        TypeDefKind::Class {
            fields: vec![field("id"), field("conn"), field("items")],
        },
    ));
    table.push(def(
        "Node",
        None,
        TypeDefKind::Class {
            fields: vec![field("next"), field("prev")],
        },
    ));
    table.push(def(
        "Point",
        None,
        TypeDefKind::Struct {
            fields: vec![field("x"), field("handle")],
        },
    ));
    table.push(def("Conn", Some("std.net"), TypeDefKind::HostClass));
    table.push(def(
        "Msg",
        None,
        TypeDefKind::Enum {
            variants: vec![
                VariantDef {
                    name: "Work".into(),
                    span: Span::new(FileId(0), 0, 0),
                    fields: vec![field("0")],
                    named: false,
                },
                VariantDef {
                    name: "Named".into(),
                    span: Span::new(FileId(0), 0, 0),
                    fields: vec![field("conn")],
                    named: true,
                },
            ],
        },
    ));
    c.types = Rc::from(table);
    c
}

// ---- building values ----

fn obj(ty: TypeId, fields: Vec<Value>) -> Rc<ClassObj> {
    ClassObj::new(ty, fields)
}

fn host(ty: TypeId) -> Value {
    Value::Host(Rc::new(HostRef {
        ty,
        index: 0,
        generation: 1,
    }))
}

fn point(x: i64) -> Value {
    Value::strukt(POINT, vec![Value::Int(x), Value::Unit])
}

fn strs(items: &[&str]) -> Value {
    Value::list(items.iter().map(|s| Value::str(s)).collect())
}

// ---- comparing graphs ----

struct Dumper {
    ids: HashMap<usize, usize>,
    inside: HashSet<usize>,
    out: String,
    /// The address of every reference object met, for "nothing is shared with the original".
    addrs: Vec<usize>,
    /// The targets of the `weak` references met, in order: they are named by that order.
    weak_targets: Vec<usize>,
    pass: u8,
}

/// The canonical text of a graph, and the addresses of its reference objects.
fn dump_all(roots: &[&Value]) -> (String, Vec<usize>) {
    let mut d = Dumper {
        ids: HashMap::new(),
        inside: HashSet::new(),
        out: String::new(),
        addrs: Vec::new(),
        weak_targets: Vec::new(),
        pass: 1,
    };
    // The first pass finds the objects of the graph (a `weak` reference to anything else is none).
    for r in roots {
        d.value(r);
    }
    d.ids.clear();
    d.out.clear();
    d.addrs.clear();
    d.pass = 2;
    for r in roots {
        d.value(r);
        d.out.push('|');
    }
    // Which object each `weak` reference names, by the number its target got.
    for (ord, addr) in d.weak_targets.iter().enumerate() {
        let id = d
            .ids
            .get(addr)
            .expect("a weak target of the graph is in it");
        d.out.push_str(&format!(" w{ord}->#{id}"));
    }
    (d.out, d.addrs)
}

fn dump(v: &Value) -> String {
    dump_all(&[v]).0
}

impl Dumper {
    /// Numbers a reference object; `true` when it was met before.
    fn seen(&mut self, addr: usize) -> bool {
        if self.pass == 1 {
            return !self.inside.insert(addr);
        }
        if let Some(n) = self.ids.get(&addr) {
            self.out.push_str(&format!("#{n}"));
            return true;
        }
        let n = self.ids.len();
        self.ids.insert(addr, n);
        self.addrs.push(addr);
        self.out.push_str(&format!("#{n}="));
        false
    }

    fn list(&mut self, items: &[Value]) {
        for i in items {
            self.value(i);
            self.out.push(',');
        }
    }

    fn value(&mut self, v: &Value) {
        use std::fmt::Write;
        let w = self.pass == 2;
        match v {
            Value::Unit => self.out.push_str("()"),
            Value::Bool(b) => write!(self.out, "{b}").unwrap(),
            Value::Int(i) => write!(self.out, "{i}").unwrap(),
            Value::U64(i) => write!(self.out, "{i}u").unwrap(),
            Value::Ptr(p) => write!(self.out, "ptr{p}").unwrap(),
            Value::Float(x) => write!(self.out, "{:?}f", x.to_bits()).unwrap(),
            Value::F32(x) => write!(self.out, "{:?}g", x.to_bits()).unwrap(),
            Value::Char(c) => write!(self.out, "{c:?}").unwrap(),
            Value::Str(s) => write!(self.out, "{s:?}").unwrap(),
            Value::Bytes(b) => write!(self.out, "b{b:?}").unwrap(),
            Value::Range(r) => write!(self.out, "{}..{}{}", r.start, r.end, r.inclusive).unwrap(),
            Value::Fn(f, name) => write!(self.out, "fn{}:{name}", f.0).unwrap(),
            Value::ByteBuf(b) => {
                if !self.seen(Rc::as_ptr(b) as usize) && w {
                    write!(self.out, "buf{:?}", b.borrow()).unwrap();
                }
            }
            Value::Struct(s) => {
                if w {
                    write!(self.out, "S{}(", s.ty.0).unwrap();
                }
                self.list(&s.fields);
                if w {
                    self.out.push(')');
                }
            }
            Value::Tuple(items) => {
                if w {
                    self.out.push('(');
                }
                self.list(items);
                if w {
                    self.out.push(')');
                }
            }
            Value::Enum(e) => {
                if w {
                    write!(self.out, "E{}.{}(", e.ty.0, e.variant).unwrap();
                }
                self.list(&e.payload);
                if w {
                    self.out.push(')');
                }
            }
            Value::SomeObj(_) => self.value(&v.unsugar()),
            Value::Dyn(d) => {
                if w {
                    write!(self.out, "dyn{}(", d.impl_id).unwrap();
                }
                self.value(&d.val);
                if w {
                    self.out.push(')');
                }
            }
            Value::List(l) => {
                if !self.seen(Rc::as_ptr(l) as usize) {
                    self.out.push('[');
                    self.list(&l.snapshot());
                    self.out.push(']');
                }
            }
            Value::Set(s) => {
                if !self.seen(Rc::as_ptr(s) as usize) {
                    self.out.push_str("set{");
                    self.list(&s.snapshot());
                    self.out.push('}');
                }
            }
            Value::Map(m) => {
                if !self.seen(Rc::as_ptr(m) as usize) {
                    self.out.push('{');
                    for (k, v) in m.snapshot() {
                        self.value(&k);
                        self.out.push(':');
                        self.value(&v);
                        self.out.push(',');
                    }
                    self.out.push('}');
                }
            }
            Value::Obj(o) => {
                if !self.seen(Rc::as_ptr(o) as usize) {
                    write!(self.out, "Obj{}(", o.ty().0).unwrap();
                    self.list(&o.snapshot());
                    self.out.push(')');
                }
            }
            Value::Cell(c) => {
                if !self.seen(Rc::as_ptr(c) as usize) {
                    self.out.push_str("cell(");
                    self.value(&c.get());
                    self.out.push(')');
                }
            }
            // A closure is a value: its identity is not told apart (a cycle through it runs through
            // a cell or an object, which are numbered).
            Value::Closure(c) => {
                if w {
                    write!(self.out, "closure{}@{}(", c.f.0, c.generation).unwrap();
                }
                for cap in &c.captures {
                    if w {
                        write!(self.out, "{}:{:?}=", cap.inner.0, cap.mode).unwrap();
                    }
                    self.value(&cap.value);
                    if w {
                        self.out.push(',');
                    }
                }
                if w {
                    self.out.push(')');
                }
            }
            Value::Weak(wk) => {
                if !w {
                    return;
                }
                match wk.upgrade() {
                    Some(t) if self.inside.contains(&(Rc::as_ptr(&t) as usize)) => {
                        let addr = Rc::as_ptr(&t) as usize;
                        let ord = match self.weak_targets.iter().position(|a| *a == addr) {
                            Some(ord) => ord,
                            None => {
                                self.weak_targets.push(addr);
                                self.weak_targets.len() - 1
                            }
                        };
                        write!(self.out, "weak{ord}").unwrap();
                    }
                    _ => self.out.push_str("weak-none"),
                }
            }
            Value::Host(_) | Value::Iter(_) => self.out.push_str("<opaque>"),
        }
    }
}

/// Takes a graph apart so that its class objects are released and its cycles freed (a class object
/// dropped any other way trips a debug assertion).
fn dispose(roots: Vec<Value>) {
    let mut seen: HashSet<usize> = HashSet::new();
    let mut grave: Vec<Value> = Vec::new();
    let mut work = roots;
    while let Some(v) = work.pop() {
        let first = |addr: usize, seen: &mut HashSet<usize>| seen.insert(addr);
        match &v {
            Value::Obj(o) if first(Rc::as_ptr(o) as usize, &mut seen) => {
                work.extend(o.take_fields());
            }
            Value::SomeObj(o) if first(Rc::as_ptr(o) as usize, &mut seen) => {
                work.extend(o.take_fields());
            }
            Value::List(l) if first(Rc::as_ptr(l) as usize, &mut seen) => work.extend(l.take_all()),
            Value::Set(s) if first(Rc::as_ptr(s) as usize, &mut seen) => work.extend(s.take_all()),
            Value::Map(m) if first(Rc::as_ptr(m) as usize, &mut seen) => {
                work.extend(m.take_all().into_iter().flat_map(|(k, v)| [k, v]));
            }
            Value::Cell(c) if first(Rc::as_ptr(c) as usize, &mut seen) => work.push(c.take()),
            Value::Closure(c) if first(Rc::as_ptr(c) as usize, &mut seen) => {
                work.extend(c.captures.iter().map(|c| c.value.clone()));
            }
            Value::Struct(s) => work.extend(s.fields.iter().cloned()),
            Value::Tuple(t) => work.extend(t.iter().cloned()),
            Value::Enum(e) => work.extend(e.payload.iter().cloned()),
            Value::Dyn(d) => work.push(d.val.clone()),
            _ => {}
        }
        grave.push(v);
    }
    drop(grave);
}

/// `to_portable` then `from_portable` with the program's names, on this thread.
fn round_trip(core: &mut Core, v: &Value) -> Value {
    let p = to_portable(v, &*core, stack()).expect("sendable");
    from_portable(p, core, stack()).expect("rebuilt")
}

fn sendable_path(core: &Core, v: &Value) -> NotSendable {
    to_portable(v, core, stack()).expect_err("not sendable")
}

// ---- every kind of value ----

/// One value of every kind that can cross, nested so that each appears inside another.
fn everything(with_weak: bool) -> (Value, Rc<ClassObj>) {
    let target = obj(NODE, vec![Value::Int(1), Value::Unit]);
    let listed = Value::list(vec![Value::Int(1), Value::str("two"), Value::Char('3')]);
    let map = {
        let mut m = OrderedMap::new();
        m.insert(Value::str("a"), Value::Int(1));
        m.insert(Value::Int(2), Value::list(vec![Value::Float(0.5)]));
        Value::Map(MapObj::new(m))
    };
    let set = {
        let mut s = OrderedMap::new();
        s.insert(Value::Int(10), Value::Unit);
        s.insert(Value::str("x"), Value::Unit);
        Value::Set(SetObj::new(s))
    };
    let cell = CellObj::new(Value::Int(7));
    let closure = Value::Closure(Rc::new(ClosureObj {
        f: FnId(5),
        generation: 2,
        captures: vec![
            Captured {
                inner: LocalId(0),
                mode: CaptureMode::ByValue,
                value: Value::str("by value"),
            },
            Captured {
                inner: LocalId(1),
                mode: CaptureMode::Cell,
                value: Value::Cell(Rc::clone(&cell)),
            },
            Captured {
                inner: LocalId(2),
                mode: CaptureMode::Weak,
                value: if with_weak {
                    Value::Weak(Rc::downgrade(&target))
                } else {
                    Value::Weak(std::rc::Weak::new())
                },
            },
        ],
    }));
    let holder = obj(
        JOB,
        vec![
            Value::Obj(Rc::clone(&target)),
            if with_weak {
                Value::Weak(Rc::downgrade(&target))
            } else {
                Value::Weak(std::rc::Weak::new())
            },
            closure,
        ],
    );
    let root = Value::tuple(vec![
        Value::Unit,
        Value::Bool(true),
        Value::Int(-5),
        Value::U64(u64::MAX),
        Value::Ptr(0x1000),
        Value::Float(1.5),
        Value::F32(2.5),
        Value::Char('é'),
        Value::str("text"),
        Value::bytes(&[0, 1, 255]),
        Value::ByteBuf(Rc::new(RefCell::new(vec![9, 8, 7]))),
        point(3),
        Value::variant(MSG, 0, vec![Value::Int(4)]),
        Value::option(None),
        Value::option(Some(Value::Int(6))),
        Value::SomeObj(Rc::clone(&target)),
        listed,
        map,
        set,
        Value::Obj(holder),
        Value::Dyn(Rc::new(DynVal {
            impl_id: 9,
            val: point(1),
        })),
        Value::Fn(FnId(3), Rc::from("add")),
        Value::range(1, 5, true),
        Value::Cell(cell),
    ]);
    (root, target)
}

#[test]
fn every_value_kind_survives_a_round_trip() {
    let mut core = named_core();
    let (root, target) = everything(true);
    let copy = round_trip(&mut core, &root);
    let (a, addrs_a) = dump_all(&[&root]);
    let (b, addrs_b) = dump_all(&[&copy]);
    assert_eq!(a, b);
    // Nothing is shared with the original: the copy is a different graph.
    let original: HashSet<usize> = addrs_a.into_iter().collect();
    assert!(addrs_b.iter().all(|x| !original.contains(x)));
    assert!(!addrs_b.is_empty());
    // Strings and bytes are copied, not shared.
    let (Value::Tuple(x), Value::Tuple(y)) = (&root, &copy) else {
        panic!()
    };
    let (Value::Str(s), Value::Str(t)) = (&x[8], &y[8]) else {
        panic!()
    };
    assert!(!Rc::ptr_eq(s, t));
    // The sender is untouched: the original graph still dumps as it did.
    assert_eq!(dump(&root), a);
    drop(target);
    dispose(vec![root, copy]);
}

#[test]
fn a_round_trip_matches_the_structural_clone() {
    let mut core = core();
    let (root, _target) = everything(false);
    let copy = round_trip(&mut core, &root);
    let mut cloner = Cloner::new(crate::stack_position(), 1 << 20);
    let cloned = cloner.clone_value(&root, &mut Plain).expect("clone");
    assert_eq!(dump(&copy), dump(&cloned));
    dispose(vec![root, copy, cloned]);
}

#[test]
fn a_copy_never_calls_a_user_clone() {
    // `to_portable` takes no hooks at all: a type with a user `impl Clone` (an `Overrides` answer)
    // has nowhere to be called from. What is checked here is that its fields are copied as they are,
    // whatever that impl would have returned.
    let mut core = core();
    let p = point(5);
    let copy = round_trip(&mut core, &p);
    assert_eq!(dump(&copy), dump(&p));
}

#[test]
fn unshared_nodes_get_no_id_and_shared_ones_do() {
    let shared = point(1);
    let root = Value::tuple(vec![shared.clone(), shared.clone(), point(2)]);
    let p = to_portable(&root, &Unnamed, stack()).unwrap();
    let Portable::Tuple { id: None, items } = &p else {
        panic!("{p:?}")
    };
    assert!(matches!(&items[0], Portable::Struct { id: Some(_), .. }));
    assert!(matches!(&items[1], Portable::Ref(_)));
    assert!(matches!(&items[2], Portable::Struct { id: None, .. }));
    let mut c = core();
    let Value::Tuple(out) = from_portable(p, &mut c, stack()).unwrap() else {
        panic!()
    };
    let (Value::Struct(a), Value::Struct(b), Value::Struct(d)) = (&out[0], &out[1], &out[2]) else {
        panic!()
    };
    assert!(Rc::ptr_eq(a, b));
    assert!(!Rc::ptr_eq(a, d));
}

#[test]
fn a_dag_with_a_million_paths_is_copied_once_per_node() {
    // Each level holds the level below twice, so level 20 has 2^20 paths to the bottom. Without the
    // memo the portable form would have a million nodes; with it, one per level.
    let mut v = point(0);
    for _ in 0..20 {
        v = Value::tuple(vec![v.clone(), v]);
    }
    let p = to_portable(&v, &Unnamed, stack()).unwrap();
    let text = format!("{p:?}");
    assert!(text.len() < 20_000, "{} bytes of portable form", text.len());
    let mut c = core();
    let copy = from_portable(p, &mut c, stack()).unwrap();
    let mut depth = 0;
    let mut cur = copy;
    while let Value::Tuple(t) = &cur {
        assert!(
            matches!((&t[0], &t[1]), (Value::Tuple(a), Value::Tuple(b)) if Rc::ptr_eq(a, b))
                || matches!((&t[0], &t[1]), (Value::Struct(a), Value::Struct(b)) if Rc::ptr_eq(a, b))
        );
        let next = t[0].clone();
        depth += 1;
        cur = next;
    }
    assert_eq!(depth, 20);
}

// ---- cycles and sharing ----

#[test]
fn a_cycle_of_objects_comes_out_as_a_cycle() {
    let mut core = core();
    let a = obj(NODE, vec![Value::Unit, Value::Unit]);
    let b = obj(NODE, vec![Value::Obj(Rc::clone(&a)), Value::Int(2)]);
    a.set_field(0, Value::Obj(Rc::clone(&b))).unwrap();
    let root = Value::Obj(Rc::clone(&a));
    let copy = round_trip(&mut core, &root);
    assert_eq!(dump(&copy), dump(&root));
    let Value::Obj(ca) = &copy else { panic!() };
    let Some(Value::Obj(cb)) = ca.field(0) else {
        panic!()
    };
    let Some(Value::Obj(back)) = cb.field(0) else {
        panic!()
    };
    assert!(Rc::ptr_eq(ca, &back));
    assert!(!Rc::ptr_eq(ca, &a));
    dispose(vec![root, copy, Value::Obj(b)]);
}

#[test]
fn a_list_that_holds_itself_and_a_self_referencing_object() {
    let mut core = core();
    let list = ListObj::new(vec![Value::Int(1)]);
    list.push(Value::List(Rc::clone(&list))).ok().unwrap();
    let me = obj(NODE, vec![Value::Unit, Value::Int(0)]);
    me.set_field(0, Value::Obj(Rc::clone(&me))).unwrap();
    let root = Value::tuple(vec![Value::List(list), Value::Obj(me)]);
    let copy = round_trip(&mut core, &root);
    assert_eq!(dump(&copy), dump(&root));
    dispose(vec![root, copy]);
}

#[test]
fn a_recursive_closure_keeps_its_cell_cycle() {
    // `let f = fn() => f()`: the closure captures the cell that holds the closure.
    let mut core = core();
    let cell = CellObj::new(Value::Unit);
    let closure = Value::Closure(Rc::new(ClosureObj {
        f: FnId(1),
        generation: 0,
        captures: vec![Captured {
            inner: LocalId(0),
            mode: CaptureMode::Cell,
            value: Value::Cell(Rc::clone(&cell)),
        }],
    }));
    cell.set(closure.clone());
    let copy = round_trip(&mut core, &closure);
    assert_eq!(dump(&copy), dump(&closure));
    let Value::Closure(c) = &copy else { panic!() };
    let Value::Cell(cc) = &c.captures[0].value else {
        panic!()
    };
    let Value::Closure(inner) = cc.get() else {
        panic!()
    };
    // The closure inside the cell is a copy of the closure (an immutable value reached again while it
    // was being copied), but it holds the same cell.
    assert!(Rc::ptr_eq(cc, &cell_of(&Value::Closure(inner))));
    dispose(vec![closure, copy]);
}

#[test]
fn a_closure_cycle_through_an_object_is_rebuilt_in_either_entry_order() {
    // closure -> object -> struct holding the closure, entered from the closure and from the object.
    let mut core = core();
    let holder = obj(NODE, vec![Value::Unit, Value::Unit]);
    let closure = Value::Closure(Rc::new(ClosureObj {
        f: FnId(2),
        generation: 0,
        captures: vec![Captured {
            inner: LocalId(0),
            mode: CaptureMode::ByValue,
            value: Value::Obj(Rc::clone(&holder)),
        }],
    }));
    holder
        .set_field(0, Value::strukt(POINT, vec![closure.clone(), Value::Unit]))
        .unwrap();
    for root in [closure.clone(), Value::Obj(Rc::clone(&holder))] {
        let copy = round_trip(&mut core, &root);
        assert_eq!(dump(&copy), dump(&root));
        dispose(vec![copy]);
    }
    dispose(vec![closure, Value::Obj(holder)]);
}

#[test]
fn shared_objects_stay_shared_inside_the_graph_and_apart_from_the_original() {
    let mut core = core();
    let shared = obj(NODE, vec![Value::Int(1), Value::Unit]);
    let root = Value::list(vec![
        Value::Obj(Rc::clone(&shared)),
        Value::Obj(Rc::clone(&shared)),
        Value::Obj(obj(NODE, vec![Value::Int(1), Value::Unit])),
    ]);
    let copy = round_trip(&mut core, &root);
    let Value::List(l) = &copy else { panic!() };
    let items = l.snapshot();
    let (Value::Obj(a), Value::Obj(b), Value::Obj(c)) = (&items[0], &items[1], &items[2]) else {
        panic!()
    };
    assert!(Rc::ptr_eq(a, b));
    assert!(!Rc::ptr_eq(a, c));
    assert!(!Rc::ptr_eq(a, &shared));
    // A write in the copy is invisible to the original.
    a.set_field(0, Value::Int(99)).unwrap();
    assert!(matches!(shared.field(0), Some(Value::Int(1))));
    dispose(vec![root, copy]);
}

// ---- weak references ----

fn weak_to(o: &Rc<ClassObj>) -> Value {
    Value::Weak(Rc::downgrade(o))
}

#[test]
fn a_weak_reference_to_an_object_of_the_graph_is_remapped() {
    let mut core = core();
    let target = obj(NODE, vec![Value::Int(7), Value::Unit]);
    // The weak reference comes first, so the walk meets it before its target.
    let holder = obj(
        JOB,
        vec![
            weak_to(&target),
            Value::Obj(Rc::clone(&target)),
            weak_to(&target),
        ],
    );
    let root = Value::Obj(holder);
    let copy = round_trip(&mut core, &root);
    assert_eq!(dump(&copy), dump(&root));
    let Value::Obj(h) = &copy else { panic!() };
    let (Some(Value::Weak(w0)), Some(Value::Obj(t)), Some(Value::Weak(w2))) =
        (h.field(0), h.field(1), h.field(2))
    else {
        panic!()
    };
    let up = w0
        .upgrade()
        .expect("the weak reference points into the copy");
    assert!(Rc::ptr_eq(&up, &t));
    assert!(Rc::ptr_eq(&w2.upgrade().unwrap(), &t));
    assert!(!Rc::ptr_eq(&up, &target));
    drop(up);
    dispose(vec![root, copy, Value::Obj(target)]);
}

#[test]
fn a_weak_reference_to_an_object_outside_the_graph_is_cut() {
    let mut core = core();
    let outside = obj(NODE, vec![Value::Int(7), Value::Unit]);
    let holder = obj(JOB, vec![weak_to(&outside), Value::Int(1)]);
    let root = Value::Obj(holder);
    let copy = round_trip(&mut core, &root);
    let Value::Obj(h) = &copy else { panic!() };
    let Some(Value::Weak(w)) = h.field(0) else {
        panic!()
    };
    assert!(w.upgrade().is_none());
    // And it does not keep the placeholder alive or touch the original.
    assert_eq!(Rc::strong_count(&outside), 1);
    assert_eq!(Rc::weak_count(&outside), 1);
    dispose(vec![root, copy, Value::Obj(outside)]);
}

#[test]
fn a_weak_reference_to_a_dead_object_stays_dead() {
    let mut core = core();
    let gone = obj(NODE, vec![Value::Int(7), Value::Unit]);
    let w = weak_to(&gone);
    dispose(vec![Value::Obj(gone)]);
    let root = Value::tuple(vec![w, Value::Int(1)]);
    let copy = round_trip(&mut core, &root);
    let Value::Tuple(t) = &copy else { panic!() };
    let Value::Weak(w) = &t[0] else { panic!() };
    assert!(w.upgrade().is_none());
}

#[test]
fn a_weak_reference_in_a_cycle_points_at_the_copy() {
    // parent <-> child, the child's back-reference weak.
    let mut core = core();
    let parent = obj(NODE, vec![Value::Unit, Value::Unit]);
    let child = obj(NODE, vec![Value::Unit, weak_to(&parent)]);
    parent.set_field(0, Value::Obj(Rc::clone(&child))).unwrap();
    let root = Value::Obj(Rc::clone(&parent));
    let copy = round_trip(&mut core, &root);
    assert_eq!(dump(&copy), dump(&root));
    let Value::Obj(cp) = &copy else { panic!() };
    let Some(Value::Obj(cc)) = cp.field(0) else {
        panic!()
    };
    let Some(Value::Weak(back)) = cc.field(1) else {
        panic!()
    };
    assert!(Rc::ptr_eq(&back.upgrade().unwrap(), cp));
    dispose(vec![root, copy, Value::Obj(child)]);
}

// ---- closures and cells ----

fn closure_over(f: u32, cell: &Rc<CellObj>) -> Value {
    Value::Closure(Rc::new(ClosureObj {
        f: FnId(f),
        generation: 0,
        captures: vec![Captured {
            inner: LocalId(0),
            mode: CaptureMode::Cell,
            value: Value::Cell(Rc::clone(cell)),
        }],
    }))
}

fn cell_of(c: &Value) -> Rc<CellObj> {
    let Value::Closure(c) = c else { panic!() };
    let Value::Cell(cell) = &c.captures[0].value else {
        panic!()
    };
    Rc::clone(cell)
}

#[test]
fn two_closures_that_share_a_cell_still_share_it() {
    let mut core = core();
    let cell = CellObj::new(Value::Int(1));
    let root = Value::tuple(vec![closure_over(1, &cell), closure_over(2, &cell)]);
    let copy = round_trip(&mut core, &root);
    assert_eq!(dump(&copy), dump(&root));
    let Value::Tuple(t) = &copy else { panic!() };
    let (a, b) = (cell_of(&t[0]), cell_of(&t[1]));
    assert!(Rc::ptr_eq(&a, &b));
    assert!(!Rc::ptr_eq(&a, &cell));
    // A write through one is seen by the other, and not by the original.
    a.set(Value::Int(50));
    assert!(matches!(b.get(), Value::Int(50)));
    assert!(matches!(cell.get(), Value::Int(1)));
}

#[test]
fn closures_with_separate_cells_do_not_share_after_the_copy() {
    let mut core = core();
    let one = CellObj::new(Value::Int(1));
    let two = CellObj::new(Value::Int(1));
    let root = Value::tuple(vec![closure_over(1, &one), closure_over(2, &two)]);
    let copy = round_trip(&mut core, &root);
    let Value::Tuple(t) = &copy else { panic!() };
    assert!(!Rc::ptr_eq(&cell_of(&t[0]), &cell_of(&t[1])));
}

#[test]
fn a_cell_that_one_closure_holds_is_copied_with_it() {
    let mut core = core();
    let root = closure_over(1, &CellObj::new(Value::str("solo")));
    let p = to_portable(&root, &Unnamed, stack()).unwrap();
    let Portable::Closure { captures, .. } = &p else {
        panic!()
    };
    assert!(matches!(captures[0].value, Portable::Cell { .. }));
    let copy = from_portable(p, &mut core, stack()).unwrap();
    assert_eq!(dump(&copy), dump(&root));
}

#[test]
fn a_closure_keeps_its_function_id_and_generation() {
    let mut core = core();
    let cell = CellObj::new(Value::Unit);
    let Value::Closure(src) = closure_over(77, &cell) else {
        panic!()
    };
    let src = Value::Closure(Rc::new(ClosureObj {
        f: src.f,
        generation: 4,
        captures: Vec::new(),
    }));
    let Value::Closure(c) = round_trip(&mut core, &src) else {
        panic!()
    };
    assert_eq!((c.f, c.generation), (FnId(77), 4));
}

#[test]
fn an_interface_value_keeps_its_impl_id() {
    let mut core = core();
    let v = Value::Dyn(Rc::new(DynVal {
        impl_id: 42,
        val: point(8),
    }));
    let Value::Dyn(d) = round_trip(&mut core, &v) else {
        panic!()
    };
    assert_eq!(d.impl_id, 42);
}

// ---- refusals ----

#[test]
fn a_host_handle_is_refused_with_its_path() {
    let core = named_core();
    let job = Value::Obj(obj(JOB, vec![Value::Int(1), host(CONN), Value::Unit]));
    let err = sendable_path(&core, &job);
    assert_eq!(err.path, "Job.conn");
    assert_eq!(err.refused, Refused::Host("net.Conn".into()));
    assert_eq!(
        err.to_string(),
        "`Job.conn` is a `net.Conn`, which cannot be sent to another thread"
    );
    dispose(vec![job]);
}

#[test]
fn the_path_goes_through_lists_maps_tuples_enums_closures_and_cells() {
    let core = named_core();
    let conn = host(CONN);

    let in_list = Value::Obj(obj(
        JOB,
        vec![
            Value::Int(1),
            Value::Unit,
            Value::list(vec![Value::Int(1), conn.clone()]),
        ],
    ));
    assert_eq!(sendable_path(&core, &in_list).path, "Job.items[1]");

    let in_tuple = Value::tuple(vec![Value::Int(1), Value::tuple(vec![conn.clone()])]);
    assert_eq!(sendable_path(&core, &in_tuple).path, ".1.0");

    let in_struct = Value::strukt(POINT, vec![Value::Int(1), conn.clone()]);
    assert_eq!(sendable_path(&core, &in_struct).path, "Point.handle");

    let in_enum = Value::variant(MSG, 0, vec![conn.clone()]);
    assert_eq!(sendable_path(&core, &in_enum).path, "Msg.Work.0");
    let in_named = Value::variant(MSG, 1, vec![conn.clone()]);
    assert_eq!(sendable_path(&core, &in_named).path, "Msg.Named.conn");

    let mut m = OrderedMap::new();
    m.insert(Value::str("k"), Value::Int(1));
    m.insert(Value::str("j"), conn.clone());
    let in_map = Value::Map(MapObj::new(m));
    assert_eq!(sendable_path(&core, &in_map).path, "[entry 1].value");

    let captured = Value::Closure(Rc::new(ClosureObj {
        f: FnId(1),
        generation: 0,
        captures: vec![Captured {
            inner: LocalId(0),
            mode: CaptureMode::Cell,
            value: Value::Cell(CellObj::new(conn.clone())),
        }],
    }));
    assert_eq!(sendable_path(&core, &captured).path, "closure.captures[0]");

    let bare = sendable_path(&core, &conn);
    assert_eq!(bare.path, "");
    assert_eq!(
        bare.to_string(),
        "this value is a `net.Conn`, which cannot be sent to another thread"
    );

    // A field path continues through an object nested in another.
    let nested = Value::list(vec![Value::Obj(obj(
        JOB,
        vec![Value::Int(1), conn, Value::Unit],
    ))]);
    assert_eq!(sendable_path(&core, &nested).path, "[0].conn");
    dispose(vec![in_list, nested]);
}

#[test]
fn closure_captures_are_named_by_the_program_when_it_knows_them() {
    struct Names;
    impl SendEnv for Names {
        fn capture_name(&self, f: FnId, _generation: u32, index: u32) -> Option<String> {
            (f == FnId(1) && index == 0).then(|| "conn".to_string())
        }
    }
    let captured = Value::Closure(Rc::new(ClosureObj {
        f: FnId(1),
        generation: 0,
        captures: vec![Captured {
            inner: LocalId(0),
            mode: CaptureMode::ByValue,
            value: Value::Iter(IterObj::over_list(ListObj::new(Vec::new()))),
        }],
    }));
    let err = to_portable(&captured, &Names, stack()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "`closure.conn` is an iterator, which cannot be sent to another thread"
    );
}

#[test]
fn each_refused_kind_names_itself() {
    let core = named_core();
    let list = ListObj::new(vec![Value::Int(1)]);
    let iter = Value::Iter(IterObj::over_list(list));
    let kinds: Vec<(Value, Refused, &str)> = vec![
        (
            iter,
            Refused::Iterator,
            "`Job.items` is an iterator, which cannot be sent to another thread",
        ),
        (
            Value::Obj(obj(COROUTINE_TYPE, vec![])),
            Refused::Coroutine,
            "`Job.items` is a `Coroutine`, which cannot be sent to another thread",
        ),
        (
            Value::Obj(obj(COGROUP_TYPE, vec![])),
            Refused::CoGroup,
            "`Job.items` is a `CoGroup`, which cannot be sent to another thread",
        ),
        (
            Value::Obj(obj(SIGNAL_TYPE, vec![])),
            Refused::Signal,
            "`Job.items` is a `Signal`, which cannot be sent to another thread",
        ),
        (
            host(CONN),
            Refused::Host("net.Conn".into()),
            "`Job.items` is a `net.Conn`, which cannot be sent to another thread",
        ),
    ];
    for (bad, refused, message) in kinds {
        let job = Value::Obj(obj(JOB, vec![Value::Int(1), Value::Unit, bad]));
        let err = sendable_path(&core, &job);
        assert_eq!(err.refused, refused);
        assert_eq!(err.to_string(), message);
        dispose(vec![job]);
    }
}

#[test]
fn a_task_handle_is_refused_through_the_environment() {
    struct WithTask(Core);
    impl SendEnv for WithTask {
        fn type_name(&self, ty: TypeId) -> String {
            SendEnv::type_name(&self.0, ty)
        }
        fn field_name(&self, ty: TypeId, i: u32) -> Option<String> {
            self.0.field_name(ty, i)
        }
        fn refuses(&self, ty: TypeId) -> Option<Refused> {
            (ty == TypeId(40)).then_some(Refused::Task)
        }
    }
    let env = WithTask(named_core());
    let job = Value::Obj(obj(
        JOB,
        vec![
            Value::Int(1),
            Value::Unit,
            Value::Obj(obj(TypeId(40), vec![])),
        ],
    ));
    let err = to_portable(&job, &env, stack()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "`Job.items` is a `Task`, which cannot be sent to another thread"
    );
    dispose(vec![job]);
}

#[test]
fn a_refusal_deep_in_a_cyclic_graph_still_reports_and_leaves_the_sender_alone() {
    let core = named_core();
    let a = obj(NODE, vec![Value::Unit, Value::Unit]);
    let b = obj(JOB, vec![Value::Int(1), host(CONN), Value::Unit]);
    a.set_field(0, Value::Obj(Rc::clone(&b))).unwrap();
    b.set_field(2, Value::Obj(Rc::clone(&a))).unwrap();
    let root = Value::Obj(Rc::clone(&a));
    let err = sendable_path(&core, &root);
    assert_eq!(err.path, "Node.next.conn");
    dispose(vec![root, Value::Obj(b)]);
}

#[test]
fn a_registered_handle_crosses_as_it_is() {
    #[derive(Debug)]
    struct Pool(i64);
    impl CrossesAsIs for Pool {
        fn materialise(&self, _core: &mut Core) -> Value {
            Value::Int(self.0)
        }
        fn address(&self) -> usize {
            self as *const Pool as usize
        }
    }
    struct Registry(Arc<Pool>);
    impl SendEnv for Registry {
        fn crosses_as_is(&self, v: &Value) -> Option<Arc<dyn CrossesAsIs>> {
            match v {
                Value::Host(r) if r.ty == CONN => Some(self.0.clone()),
                _ => None,
            }
        }
    }
    let pool = Arc::new(Pool(41));
    let env = Registry(Arc::clone(&pool));
    let root = Value::tuple(vec![host(CONN), host(CONN), Value::Int(1)]);
    let p = to_portable(&root, &env, stack()).unwrap();
    // The first visit carries the handle; the second reaches the same thing, so it names the first
    // (one handle on the receiver, S4).
    assert!(matches!(
        &p,
        Portable::Tuple { items, .. }
            if matches!(items[0], Portable::Handle { .. }) && matches!(items[1], Portable::Ref(_))
    ));
    // The handle is the same `Arc`, not a copy of what it names.
    assert_eq!(Arc::strong_count(&pool), 3);
    let mut c = core();
    let Value::Tuple(out) = from_portable(p, &mut c, stack()).unwrap() else {
        panic!()
    };
    assert!(matches!(
        (&out[0], &out[1], &out[2]),
        (Value::Int(41), Value::Int(41), Value::Int(1))
    ));
    assert_eq!(Arc::strong_count(&pool), 2);
    // A host type the registry does not know is still refused.
    let other = to_portable(&host(TypeId(35)), &env, stack()).unwrap_err();
    assert!(matches!(other.refused, Refused::Host(_)));
}

// ---- deep graphs ----

/// `Cons(i, Cons(i-1, ... Nil))`, `depth` cells deep.
fn chain(depth: usize) -> Value {
    let mut list = Value::variant(MSG, 1, Vec::new());
    for i in 0..depth {
        list = Value::variant(MSG, 0, vec![Value::Int(i as i64), list]);
    }
    list
}

#[test]
fn a_graph_deeper_than_the_budget_is_refused_cleanly() {
    let core = named_core();
    // A million levels would overflow any native stack; the budget stops the walk long before.
    let v = chain(1_000_000);
    let err = to_portable(&v, &core, StackGuard::here(256 * 1024)).unwrap_err();
    assert_eq!(err.refused, Refused::TooDeep);
    assert!(err.to_string().contains("nested too deeply"), "{err}");
    // The path of such an error is short enough to read.
    assert!(err.path.len() < 200, "{}", err.path);
    assert!(err.path.contains("levels"), "{}", err.path);
}

/// Bytes of native stack that one level of a nested enum costs `to_portable` and `from_portable`
/// here (found by shrinking the budget until a 100-level chain no longer fits; the cost depends on
/// the platform and the profile, which is why no test assumes a depth for a fixed budget).
fn bytes_per_level() -> (usize, usize) {
    const LEVELS: usize = 100;
    let v = chain(LEVELS);
    let mut send = 1024;
    while to_portable(&v, &Unnamed, StackGuard::here(send)).is_err() {
        send += 1024;
    }
    let mut c = core();
    let mut recv = 1024;
    while from_portable(
        to_portable(&v, &Unnamed, StackGuard::here(1 << 24)).unwrap(),
        &mut c,
        StackGuard::here(recv),
    )
    .is_err()
    {
        recv += 1024;
    }
    (send / LEVELS, recv / LEVELS)
}

#[test]
fn a_deep_graph_within_the_budget_is_copied() {
    // The depth is derived from the measured frame cost and the budget, so it fits on every
    // platform: half of what the budget allows, on a thread whose stack is far larger than it.
    const STACK: usize = 64 << 20;
    const BUDGET: usize = 32 << 20;
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(|| {
            let (send, recv) = bytes_per_level();
            eprintln!(
                "native stack per nested level: to_portable {send} B, from_portable {recv} B"
            );
            let depth = (BUDGET / 2 / send.max(recv)).min(20_000);
            // A frame cost that grew past 32 KB a level would be a bug on any platform.
            assert!(
                depth >= 500,
                "only {depth} levels fit {BUDGET} bytes ({send}/{recv} B each)"
            );
            let mut core = core();
            let v = chain(depth);
            let guard = StackGuard::here(BUDGET);
            let p = to_portable(&v, &core, guard).expect("fits");
            let copy = from_portable(p, &mut core, guard).expect("fits");
            assert_eq!(dump(&copy), dump(&v));
            // The same chain is refused, cleanly, by a budget a tenth as large.
            let tight = StackGuard::here(depth * send / 10);
            let err = to_portable(&v, &core, tight).unwrap_err();
            assert_eq!(err.refused, Refused::TooDeep);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn rebuilding_stops_at_the_receivers_budget_and_leaves_nothing_behind() {
    let mut core = core();
    core.set_track_leaks(true);
    // An object cycle at the top, a list that holds itself, a cell and a deep chain below them, so
    // that the failure comes with reference objects of every kind already made.
    let a = obj(NODE, vec![Value::Unit, Value::Unit]);
    a.set_field(0, Value::Obj(Rc::clone(&a))).unwrap();
    a.set_field(1, chain(100)).unwrap();
    let list = ListObj::new(vec![
        Value::Obj(Rc::clone(&a)),
        Value::Cell(CellObj::new(Value::Int(1))),
    ]);
    list.push(Value::List(Rc::clone(&list))).ok().unwrap();
    let mut keys = OrderedMap::new();
    keys.insert(
        Value::Int(1),
        Value::Obj(obj(NODE, vec![Value::Unit, Value::Unit])),
    );
    let root = Value::tuple(vec![
        Value::List(Rc::clone(&list)),
        Value::Obj(Rc::clone(&a)),
        Value::Map(MapObj::new(keys)),
        Value::Set(SetObj::new(OrderedMap::new())),
    ]);
    let p = to_portable(&root, &Unnamed, StackGuard::here(1 << 20)).expect("the sender has room");
    let err = from_portable(p, &mut core, StackGuard::here(64 * 1024)).unwrap_err();
    assert_eq!(err, RebuildError::TooDeep);
    // What the failed copy made is gone: no cycle of it is left for the leak detector to find, and
    // none of its class objects was dropped without being released (which asserts).
    assert!(core.find_leaks(&|_| None).is_empty());
    // The same graph rebuilds where there is room.
    let p = to_portable(&root, &Unnamed, StackGuard::here(1 << 20)).unwrap();
    let ok = from_portable(p, &mut core, StackGuard::here(1 << 20)).unwrap();
    assert_eq!(dump(&ok), dump(&root));
    dispose(vec![root, ok, Value::List(list)]);
}

#[test]
fn a_ref_to_nothing_is_a_malformed_graph() {
    let mut core = core();
    let err = from_portable(Portable::Ref(3), &mut core, stack()).unwrap_err();
    assert_eq!(err, RebuildError::Malformed);
}

#[test]
fn a_very_deep_portable_is_freed_without_recursion() {
    // A million levels through every kind of node that has children, built and dropped on a thread
    // whose stack could never take a recursive drop.
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let mut p = Portable::Unit;
            for i in 0..1_000_000u32 {
                p = match i % 8 {
                    0 => Portable::Enum {
                        id: None,
                        ty: MSG,
                        variant: 0,
                        payload: vec![Portable::Int(1), p].into_boxed_slice(),
                    },
                    1 => Portable::List {
                        id: i,
                        items: vec![p].into_boxed_slice(),
                    },
                    2 => Portable::Obj {
                        id: i,
                        ty: NODE,
                        fields: vec![p].into_boxed_slice(),
                    },
                    3 => Portable::Map {
                        id: i,
                        entries: vec![(Portable::Int(1), p)].into_boxed_slice(),
                    },
                    4 => Portable::Dyn {
                        id: None,
                        impl_id: 0,
                        val: Box::new(p),
                    },
                    5 => Portable::Cell {
                        id: i,
                        value: Box::new(p),
                    },
                    6 => Portable::Closure {
                        id: None,
                        f: FnId(0),
                        generation: 0,
                        captures: vec![PortableCapture {
                            inner: LocalId(0),
                            mode: CaptureMode::ByValue,
                            value: p,
                        }]
                        .into_boxed_slice(),
                    },
                    _ => Portable::Set {
                        id: i,
                        items: vec![Portable::Int(2), p].into_boxed_slice(),
                    },
                };
            }
            drop(p);
        })
        .unwrap()
        .join()
        .unwrap();
}

// ---- another thread ----

#[test]
fn a_portable_value_crosses_to_another_thread_and_is_rebuilt_there() {
    // A graph with a cycle, a weak reference, a shared cell and a shared object.
    let mut sender = named_core();
    let parent = obj(NODE, vec![Value::Unit, Value::Unit]);
    let child = obj(NODE, vec![Value::Obj(Rc::clone(&parent)), weak_to(&parent)]);
    parent.set_field(0, Value::Obj(Rc::clone(&child))).unwrap();
    let cell = CellObj::new(Value::Int(5));
    let root = Value::tuple(vec![
        Value::Obj(Rc::clone(&parent)),
        closure_over(1, &cell),
        closure_over(2, &cell),
        strs(&["a", "b"]),
        Value::Obj(Rc::clone(&child)),
    ]);
    let expected = dump(&root);
    let portable = to_portable(&root, &sender, stack()).unwrap();

    let handle = std::thread::spawn(move || {
        let mut receiver = core();
        let rebuilt = from_portable(portable, &mut receiver, StackGuard::here(1 << 20)).unwrap();
        let text = dump(&rebuilt);
        // Mutate on this side, then send the result back.
        let Value::Tuple(items) = &rebuilt else {
            panic!()
        };
        cell_of(&items[1]).set(Value::Int(6));
        let back = to_portable(&rebuilt, &Unnamed, StackGuard::here(1 << 20)).unwrap();
        dispose(vec![rebuilt]);
        (text, back)
    });
    let (text, back) = handle.join().unwrap();
    assert_eq!(text, expected);
    let again = from_portable(back, &mut sender, stack()).unwrap();
    let Value::Tuple(items) = &again else {
        panic!()
    };
    assert!(matches!(cell_of(&items[1]).get(), Value::Int(6)));
    assert!(Rc::ptr_eq(&cell_of(&items[1]), &cell_of(&items[2])));
    // The sender's own cell never saw the other thread's write.
    assert!(matches!(cell.get(), Value::Int(5)));
    dispose(vec![root, again]);
}

// ---- the leak detector and the allocator ----

#[test]
fn rebuilt_objects_and_closures_are_known_to_the_leak_detector() {
    let mut core = core();
    core.set_track_leaks(true);
    let a = obj(NODE, vec![Value::Unit, Value::Unit]);
    let b = obj(NODE, vec![Value::Obj(Rc::clone(&a)), Value::Int(0)]);
    a.set_field(0, Value::Obj(Rc::clone(&b))).unwrap();
    let root = Value::Obj(Rc::clone(&a));
    let copy = round_trip(&mut core, &root);
    dispose(vec![root, Value::Obj(b)]);
    // Nothing leaks while the copy is held.
    assert!(core.find_leaks(&|_| None).is_empty());
    // Dropping the copy without taking it apart leaves the cycle: the detector, which saw the
    // objects the copy made, reports it.
    drop(copy);
    let leaks = core.find_leaks(&|_| None);
    assert_eq!(leaks.len(), 1, "{leaks:?}");
    assert_eq!(leaks[0].objects.len(), 2);
}

// ---- random graphs ----

/// xorshift64*: a fixed seed gives the same graphs on every platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// A random graph: scalars, structures built from earlier values (so values are shared), and
/// reference objects that are filled afterwards from anything (so there are cycles).
fn random_graph(rng: &mut Rng) -> Vec<Value> {
    let mut pool: Vec<Value> = vec![
        Value::Unit,
        Value::Bool(true),
        Value::Int(-3),
        Value::Float(2.5),
        Value::str("s"),
        Value::bytes(&[1, 2]),
        Value::Char('c'),
        Value::range(0, 3, false),
        Value::option(None),
    ];
    let mut objs = Vec::new();
    let mut lists = Vec::new();
    let mut maps = Vec::new();
    let mut sets = Vec::new();
    let mut cells = Vec::new();
    for _ in 0..1 + rng.below(8) {
        match rng.below(5) {
            0 => {
                let n = 1 + rng.below(3);
                let o = obj(TypeId(30 + rng.below(3) as u32), vec![Value::Unit; n]);
                pool.push(Value::Obj(Rc::clone(&o)));
                objs.push(o);
            }
            1 => {
                let l = ListObj::new(Vec::new());
                pool.push(Value::List(Rc::clone(&l)));
                lists.push(l);
            }
            2 => {
                let m = MapObj::new(OrderedMap::new());
                pool.push(Value::Map(Rc::clone(&m)));
                maps.push(m);
            }
            3 => {
                let s = SetObj::new(OrderedMap::new());
                pool.push(Value::Set(Rc::clone(&s)));
                sets.push(s);
            }
            _ => {
                let c = CellObj::new(Value::Unit);
                pool.push(Value::Cell(Rc::clone(&c)));
                cells.push(c);
            }
        }
    }
    for _ in 0..rng.below(14) {
        let a = rng.pick(&pool).clone();
        let b = rng.pick(&pool).clone();
        // A cell is never a value of the language: only a closure holds one.
        let plain = |v: Value| {
            if matches!(v, Value::Cell(_)) {
                Value::Unit
            } else {
                v
            }
        };
        let made = match rng.below(8) {
            0 => Value::strukt(POINT, vec![plain(a), plain(b)]),
            1 => Value::tuple(vec![plain(a), plain(b)]),
            2 => Value::variant(MSG, rng.below(2) as u32, vec![plain(a)]),
            3 => Value::option(Some(plain(a))),
            4 => Value::Dyn(Rc::new(DynVal {
                impl_id: rng.below(4) as u32,
                val: plain(a),
            })),
            5 => match (objs.is_empty(), cells.is_empty()) {
                (_, true) => Value::Unit,
                _ => {
                    let cell = rng.pick(&cells);
                    let mut captures = vec![Captured {
                        inner: LocalId(0),
                        mode: CaptureMode::Cell,
                        value: Value::Cell(Rc::clone(cell)),
                    }];
                    if rng.below(2) == 0 {
                        captures.push(Captured {
                            inner: LocalId(1),
                            mode: CaptureMode::ByValue,
                            value: plain(a),
                        });
                    }
                    if !objs.is_empty() && rng.below(2) == 0 {
                        captures.push(Captured {
                            inner: LocalId(2),
                            mode: CaptureMode::Weak,
                            value: weak_to(rng.pick(&objs)),
                        });
                    }
                    Value::Closure(Rc::new(ClosureObj {
                        f: FnId(rng.below(5) as u32),
                        generation: rng.below(2) as u32,
                        captures,
                    }))
                }
            },
            6 if !objs.is_empty() => weak_to(rng.pick(&objs)),
            _ => Value::SomeObj(match objs.is_empty() {
                true => obj(NODE, vec![Value::Unit, Value::Unit]),
                false => Rc::clone(rng.pick(&objs)),
            }),
        };
        if !matches!(made, Value::Unit) {
            pool.push(made);
        }
    }
    let plain = |v: &Value| match v {
        Value::Cell(_) => Value::Unit,
        other => other.clone(),
    };
    for o in &objs {
        for i in 0..o.snapshot().len() {
            let v = plain(rng.pick(&pool));
            o.set_field(i, v).unwrap();
        }
    }
    for l in &lists {
        for _ in 0..rng.below(4) {
            let v = plain(rng.pick(&pool));
            l.push(v).ok().unwrap();
        }
    }
    for (n, m) in maps.iter().enumerate() {
        for k in 0..rng.below(4) {
            let key = if rng.below(2) == 0 {
                Value::Int((n * 10 + k) as i64)
            } else {
                Value::str(&format!("k{n}_{k}"))
            };
            let v = plain(rng.pick(&pool));
            m.insert(key, v).ok().unwrap();
        }
    }
    for s in &sets {
        for _ in 0..rng.below(4) {
            let item = if rng.below(2) == 0 {
                Value::Int(rng.below(6) as i64)
            } else {
                Value::str(&format!("e{}", rng.below(6)))
            };
            s.add(item).ok().unwrap();
        }
    }
    for c in &cells {
        c.set(plain(rng.pick(&pool)));
    }
    // The roots: a few values of the pool, so that some of it is out of the graph (and so are
    // some `weak` targets).
    let mut roots = Vec::new();
    for _ in 0..1 + rng.below(3) {
        roots.push(plain(rng.pick(&pool)));
    }
    // Everything made is disposed of through the pool, which holds all of it.
    let root = Value::tuple(roots);
    pool.push(root.clone());
    let mut out = vec![root];
    out.extend(pool);
    out
}

#[test]
fn random_graphs_round_trip_to_an_isomorphic_copy() {
    let mut core = core();
    let mut back_refs = 0;
    let mut weaks = 0;
    let mut against_clone = 0;
    for seed in 1..=1500u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let graph = random_graph(&mut rng);
        let root = graph[0].clone();
        let copy = round_trip(&mut core, &root);
        let (a, addrs_a) = dump_all(&[&root]);
        let (b, addrs_b) = dump_all(&[&copy]);
        assert_eq!(a, b, "seed {seed}");
        let original: HashSet<usize> = addrs_a.iter().copied().collect();
        assert!(addrs_b.iter().all(|x| !original.contains(x)), "seed {seed}");
        assert_eq!(addrs_a.len(), addrs_b.len(), "seed {seed}");
        // The same graph as the structural clone (what `x.clone()` makes).
        let mut cloner = Cloner::new(crate::stack_position(), 1 << 20);
        let cloned = cloner.clone_value(&root, &mut Plain).expect("clone");
        // (The clone shares a closure and its cells with the original instead of copying them,
        // and leaves a `weak` reference to a target it had not copied yet pointing at the original,
        // so only graphs without either are compared with it.)
        if !a.contains("weak") && !a.contains("closure") {
            assert_eq!(dump(&cloned), a, "seed {seed}");
            against_clone += 1;
        }
        // The copy of a copy is the same graph again (the walk is stable).
        let twice = round_trip(&mut core, &copy);
        assert_eq!(dump(&twice), a, "seed {seed}");
        // The sender's graph is unchanged by being sent.
        assert_eq!(dump(&root), a, "seed {seed}");
        // Each object is defined once (`#n=`); every other `#n` is a reference back to it.
        let defs = a.matches("#").count() - a.matches("->#").count();
        back_refs += usize::from(defs > addrs_a.len());
        weaks += usize::from(a.contains("weak"));
        let mut all = graph;
        all.push(copy);
        all.push(twice);
        all.push(cloned);
        dispose(all);
    }
    // The generator does produce what it is meant to test.
    eprintln!("random graphs: {back_refs} with back-references, {weaks} with weak references");
    assert!(back_refs > 150, "{back_refs}");
    assert!(weaks > 80, "{weaks}");
    assert!(against_clone > 400, "{against_clone}");
}

#[test]
fn random_graphs_keep_their_aliasing_through_the_portable_form() {
    // Sending two roots in one tuple keeps what they share; sending them one at a time does not.
    let mut core = core();
    for seed in 1..=100u64 {
        let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
        let graph = random_graph(&mut rng);
        let pair = Value::tuple(vec![graph[0].clone(), graph[0].clone()]);
        let copy = round_trip(&mut core, &pair);
        let Value::Tuple(t) = &copy else { panic!() };
        assert_eq!(dump_all(&[&t[0]]).0, dump_all(&[&t[1]]).0, "seed {seed}");
        let (a, _) = dump_all(&[&pair]);
        let (b, _) = dump_all(&[&copy]);
        assert_eq!(a, b, "seed {seed}");
        let mut all = graph;
        all.push(copy);
        dispose(all);
    }
}

// ---- strings are copied once ----

#[test]
fn a_string_held_a_hundred_times_is_copied_once_and_rebuilt_as_one() {
    let text: Rc<str> = Rc::from("x".repeat(1 << 20).as_str());
    let bytes: Rc<[u8]> = Rc::from(vec![7u8; 1 << 10]);
    let mut items: Vec<Value> = (0..100).map(|_| Value::Str(Rc::clone(&text))).collect();
    items.extend((0..100).map(|_| Value::Bytes(Rc::clone(&bytes))));
    // A string nobody else holds is plain.
    items.push(Value::str("alone"));
    let root = Value::list(items);
    let p = to_portable(&root, &Unnamed, stack()).unwrap();
    let Portable::List { items, .. } = &p else {
        panic!()
    };
    let mut copied = 0usize;
    let mut refs = 0;
    for item in items.iter() {
        match item {
            Portable::SharedStr(_, s) => copied += s.len(),
            Portable::SharedBytes(_, b) => copied += b.len(),
            Portable::Str(s) => copied += s.len(),
            Portable::Ref(_) => refs += 1,
            other => panic!("{other:?}"),
        }
    }
    // One megabyte and one kilobyte, not a hundred of each.
    assert_eq!(copied, (1 << 20) + (1 << 10) + "alone".len());
    assert_eq!(refs, 198);
    let mut c = core();
    let Value::List(l) = from_portable(p, &mut c, stack()).unwrap() else {
        panic!()
    };
    let out = l.snapshot();
    let Value::Str(first) = &out[0] else { panic!() };
    assert_eq!(first.len(), 1 << 20);
    assert!(
        out[..100]
            .iter()
            .all(|v| matches!(v, Value::Str(s) if Rc::ptr_eq(s, first)))
    );
    let Value::Bytes(b0) = &out[100] else {
        panic!()
    };
    assert!(
        out[100..200]
            .iter()
            .all(|v| matches!(v, Value::Bytes(b) if Rc::ptr_eq(b, b0)))
    );
    // The receiver's strings are its own.
    assert!(!Rc::ptr_eq(first, &text));
}

// ---- what is sent can be received ----

/// A value `depth` levels deep of the given shape.
fn nested(shape: u8, depth: usize) -> Value {
    let mut v = Value::Int(0);
    for i in 0..depth {
        v = match shape {
            0 => Value::variant(MSG, 0, vec![Value::Int(i as i64), v]),
            1 => Value::list(vec![v]),
            2 => Value::Obj(obj(NODE, vec![v, Value::Unit])),
            3 => Value::tuple(vec![v, Value::Int(1)]),
            _ => Value::strukt(POINT, vec![v, Value::Unit]),
        };
    }
    v
}

#[test]
fn whatever_the_sender_accepts_the_receiver_takes_under_the_same_budget() {
    // For each shape that nests, the deepest value `to_portable` accepts under a budget must be
    // rebuilt by `from_portable` under that same budget.
    const BUDGET: usize = 256 * 1024;
    std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(|| {
            for shape in 0..5u8 {
                let mut deepest = 0;
                let mut depth = 8;
                loop {
                    let v = nested(shape, depth);
                    let sent = to_portable(&v, &Unnamed, StackGuard::here(BUDGET));
                    dispose(vec![v]);
                    if sent.is_err() {
                        break;
                    }
                    deepest = depth;
                    depth += 4;
                }
                assert!(deepest > 8, "shape {shape}: only {deepest} levels fit");
                // Two levels short of the edge: the two walks start a few hundred bytes apart (and
                // a release build inlines the calls differently), so the very last level is not
                // told apart. A receiver that cost a tenth more per level would still fail.
                let deepest = deepest - 2;
                let v = nested(shape, deepest);
                let p = to_portable(&v, &Unnamed, StackGuard::here(BUDGET)).unwrap();
                let mut c = core();
                let back = from_portable(p, &mut c, StackGuard::here(BUDGET))
                    .unwrap_or_else(|e| panic!("shape {shape}, {deepest} levels: {e}"));
                eprintln!("shape {shape}: {deepest} levels sent and received in {BUDGET} bytes");
                dispose(vec![v, back]);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn one_channel_reached_twice_in_a_value_is_one_handle_on_the_receiver() {
    // The sender holds one channel by two handle objects and one of them twice; the receiver
    // gets one object for the channel (S4), however it was reached.
    let mut sender = core();
    let inner = crate::chan::ChanInner::new(None);
    let a = sender.channel_handle(Arc::clone(&inner));
    let b = sender.channel_handle(Arc::clone(&inner));
    let root = Value::tuple(vec![a.clone(), b, a]);
    let p = to_portable(&root, &sender, stack()).unwrap();
    let Portable::Tuple { items, .. } = &p else {
        panic!("{p:?}")
    };
    assert!(matches!(items[0], Portable::Handle { .. }));
    assert!(matches!(items[1], Portable::Ref(_)));
    assert!(matches!(items[2], Portable::Ref(_)));
    let mut receiver = core();
    let Value::Tuple(out) = from_portable(p, &mut receiver, stack()).unwrap() else {
        panic!()
    };
    {
        let (Value::Obj(x), Value::Obj(y), Value::Obj(z)) = (&out[0], &out[1], &out[2]) else {
            panic!()
        };
        assert!(Rc::ptr_eq(x, y) && Rc::ptr_eq(y, z));
    }
    assert_eq!(receiver.chan_count(), 1);
    dispose(vec![root, Value::Tuple(out)]);
}
