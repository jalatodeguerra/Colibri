//! The portable deep copy: the form a value takes to cross to another thread (DESIGN §14.1,
//! ARCHITECTURE §36.1).
//!
//! A [`Value`] is made of `Rc`s and is `!Send` on purpose: every thread task is an isolate with its
//! own object graph. What crosses is a [`Portable`], an owned tree of boxes that is `Send` (checked
//! below at compile time, and with no `unsafe impl`). [`to_portable`] walks a graph on the sending
//! thread and [`from_portable`] rebuilds fresh `Rc`s on the receiving one.
//!
//! # What is kept
//!
//! * **Sharing and cycles.** Each object that can be changed (class object, list, map, set, byte
//!   buffer, cell) gets an id the first time it is visited (a memo by address, like
//!   [`crate::clone::Cloner`]), and every later visit is a [`Portable::Ref`]: a cycle always runs
//!   through one of them. A struct, tuple, enum, interface value or closure is a value, so its
//!   sharing is only an economy: it gets an id when something else holds it too (a reference count
//!   above 1), and a later visit of one that is complete is a `Ref`, which keeps a graph that
//!   shares a value along many paths from being copied once per path. One that is still being
//!   copied when it is reached again (a cycle through an object that holds it) is copied again, and
//!   the second copy ends at the object's `Ref`.
//! * **Weak references.** A `weak` field names its target by id. A target that is not part of the
//!   copied graph, or is already dead, comes out as an empty (cleared) reference on the other side.
//! * **Closures** keep their `FnId` and their captures, and a cell shared by two closures is one cell
//!   in the copy.
//! * **Interface values** keep their `impl_id`.
//!
//! The copy is structural: it never runs a user `impl Clone`, and nothing is moved, so the sender's
//! graph is untouched.
//!
//! # What is refused
//!
//! Host handles, iterators, and the handles of coroutines, `CoGroup`s, `Signal`s and tasks. The error
//! ([`NotSendable`]) names the path to the offender, as the checker will (`` `Job.conn` is a
//! `net.Conn`, which cannot be sent to another thread ``). Two seams are left for what comes later:
//! [`SendEnv::crosses_as_is`] lets a host type or a built-in handle cross as an `Arc` (`Shared`,
//! `Channel`, `Mutex`, a registered thread-shareable host type) through [`Portable::Handle`], and
//! [`SendEnv::refuses`] adds refused class types (`Task`).
//!
//! # Deep graphs
//!
//! Both directions recurse along the nesting of the graph and measure the native stack against a
//! [`StackGuard`], failing with [`Refused::TooDeep`] or [`RebuildError::TooDeep`] instead of
//! overflowing it, like [`crate::clone::Cloner`]. Dropping a `Portable` is iterative, so a deep one
//! can be freed on a thread with a small stack.
//!
//! # Rebuilding
//!
//! The nodes are rebuilt in the order they were copied in. An object that can be changed is created
//! empty and registered by id before its contents are built, which is what lets a back-reference
//! (`Ref`) name an object that is still being filled, and a `weak` reference name one that has not
//! been reached yet (it gets an empty object of the right shape, which the target's own node takes
//! over, or which dies at the end if the target was not part of the graph).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use colibri_check::hir::{CaptureMode, FnId, LocalId};
use colibri_check::types::{
    COGROUP_TYPE, COROUTINE_TYPE, OPTION_TYPE, SIGNAL_TYPE, TypeId, VARIANT_NONE, VARIANT_SOME,
};

use crate::closure::{CellObj, Captured, ClosureObj};
use crate::heap::{ClassObj, ListObj, MapObj, SetObj};
use crate::ordered::OrderedMap;
use crate::state::Core;
use crate::value::{
    DynVal, EnumVal, ObjFields, Payload, RangeVal, StructFields, StructVal, Value,
};

// ---- the portable form ----

/// A value that can cross to another thread.
///
/// It mirrors [`Value`]. A reference object carries the id the memo gave it, so that a later
/// [`Portable::Ref`] can name it. The objects that can be changed (byte buffers, lists, maps, sets,
/// class objects and cells) always have one: they are what a cycle runs through. A struct, tuple,
/// enum, interface value or closure has one (`Some`) only when something else holds it too.
#[derive(Debug)]
pub enum Portable {
    Unit,
    Bool(bool),
    Int(i64),
    U64(u64),
    Ptr(usize),
    Float(f64),
    F32(f32),
    Char(char),
    Str(Box<str>),
    Bytes(Box<[u8]>),
    /// The buffer of a `StrBuilder` or `BytesBuilder`.
    ByteBuf {
        id: u32,
        bytes: Box<[u8]>,
    },
    Struct {
        id: Option<u32>,
        ty: TypeId,
        fields: Box<[Portable]>,
    },
    Tuple {
        id: Option<u32>,
        items: Box<[Portable]>,
    },
    Enum {
        id: Option<u32>,
        ty: TypeId,
        variant: u32,
        payload: Box<[Portable]>,
    },
    List {
        id: u32,
        items: Box<[Portable]>,
    },
    Map {
        id: u32,
        entries: Box<[(Portable, Portable)]>,
    },
    Set {
        id: u32,
        items: Box<[Portable]>,
    },
    /// A class object.
    Obj {
        id: u32,
        ty: TypeId,
        fields: Box<[Portable]>,
    },
    /// A `weak` reference: `None` when its target is gone or was not part of the graph.
    Weak(Option<WeakRef>),
    /// An interface value.
    Dyn {
        id: Option<u32>,
        impl_id: u32,
        val: Box<Portable>,
    },
    Fn(FnId, Box<str>),
    Closure {
        id: Option<u32>,
        f: FnId,
        generation: u32,
        captures: Box<[PortableCapture]>,
    },
    /// The storage of a captured variable, shared by every closure that captured it.
    Cell {
        id: u32,
        value: Box<Portable>,
    },
    Range(RangeVal),
    /// A second visit to the reference object with this id (a shared substructure, or a cycle).
    Ref(u32),
    /// A value that crosses as it is, by reference count (`Shared`, `Channel`, `Mutex`, a host
    /// type registered thread-shareable).
    Handle(Arc<dyn CrossesAsIs>),
}

/// One captured variable of a closure.
#[derive(Debug)]
pub struct PortableCapture {
    pub inner: LocalId,
    pub mode: CaptureMode,
    pub value: Portable,
}

/// A `weak` reference to a class object of the graph: the target's id, and its shape, so that the
/// receiver can make the reference before it reaches the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WeakRef {
    pub id: u32,
    pub ty: TypeId,
    pub fields: u32,
}

/// A thread-shareable value that is not copied when it crosses: it is an `Arc` with its own
/// synchronisation, and the receiver turns it back into a `Value` of its own isolate.
pub trait CrossesAsIs: Send + Sync + fmt::Debug {
    /// The value this handle stands for on the receiving thread.
    fn into_value(&self, core: &mut Core) -> Value;
}

// A `Portable` is `Send`. This is the whole point of the type, so it is a compile-time fact, and no
// `unsafe impl Send` exists anywhere to make it so.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Portable>();
    assert_send::<NotSendable>();
};

impl Portable {
    /// Whether the node has no children to free.
    fn is_leaf(&self) -> bool {
        !matches!(
            self,
            Portable::Struct { .. }
                | Portable::Tuple { .. }
                | Portable::Enum { .. }
                | Portable::List { .. }
                | Portable::Map { .. }
                | Portable::Set { .. }
                | Portable::Obj { .. }
                | Portable::Dyn { .. }
                | Portable::Closure { .. }
                | Portable::Cell { .. }
        )
    }

    /// Moves the children of this node onto `work` (the leaves among them are dropped here).
    fn drain_into(&mut self, work: &mut Vec<Portable>) {
        fn push(work: &mut Vec<Portable>, items: &mut Box<[Portable]>) {
            work.extend(
                std::mem::take(items)
                    .into_vec()
                    .into_iter()
                    .filter(|p| !p.is_leaf()),
            );
        }
        match self {
            Portable::Struct { fields: items, .. }
            | Portable::Tuple { items, .. }
            | Portable::Enum { payload: items, .. }
            | Portable::List { items, .. }
            | Portable::Set { items, .. }
            | Portable::Obj { fields: items, .. } => push(work, items),
            Portable::Map { entries, .. } => {
                for (k, v) in std::mem::take(entries).into_vec() {
                    for p in [k, v] {
                        if !p.is_leaf() {
                            work.push(p);
                        }
                    }
                }
            }
            Portable::Dyn { val, .. }
            | Portable::Cell { value: val, .. } => {
                if !val.is_leaf() {
                    work.push(std::mem::replace(&mut **val, Portable::Unit));
                }
            }
            Portable::Closure { captures, .. } => {
                for c in std::mem::take(captures).into_vec() {
                    if !c.value.is_leaf() {
                        work.push(c.value);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Drop for Portable {
    /// Frees a deep graph with a worklist, not with one native call per level.
    fn drop(&mut self) {
        if self.is_leaf() {
            return;
        }
        let mut work = Vec::new();
        self.drain_into(&mut work);
        while let Some(mut node) = work.pop() {
            node.drain_into(&mut work);
        }
    }
}

// ---- how deep, and what the program calls things ----

/// How much native stack a copy may use, measured below a base position (see
/// [`crate::stack_position`]).
#[derive(Clone, Copy, Debug)]
pub struct StackGuard {
    base: usize,
    budget: usize,
}

impl StackGuard {
    /// A guard that gives up once the recursion has used `budget` bytes below `base`.
    pub fn new(base: usize, budget: usize) -> Self {
        StackGuard { base, budget }
    }

    /// A guard measured from the current position.
    pub fn here(budget: usize) -> Self {
        StackGuard::new(crate::stack_position(), budget)
    }

    #[inline]
    fn exceeded(&self) -> bool {
        crate::stack_position().abs_diff(self.base) > self.budget
    }
}

/// What [`to_portable`] asks of the program it runs in: names for the error path, and the two seams
/// for later steps. Every method has a default, so a copy needs no program.
pub trait SendEnv {
    /// The name of a type as the user writes it (`Job`, `net.Conn`).
    fn type_name(&self, ty: TypeId) -> String {
        format!("type#{}", ty.0)
    }

    /// The name of field `index` of a struct or class.
    fn field_name(&self, _ty: TypeId, _index: u32) -> Option<String> {
        None
    }

    /// The name of variant `variant` of an enum.
    fn variant_name(&self, _ty: TypeId, _variant: u32) -> Option<String> {
        None
    }

    /// The name of payload field `index` of a variant (`"0"` for a positional one).
    fn payload_name(&self, _ty: TypeId, _variant: u32, _index: u32) -> Option<String> {
        None
    }

    /// The name of the variable that capture number `index` of the closure body `f` holds.
    fn capture_name(&self, _f: FnId, _index: u32) -> Option<String> {
        None
    }

    /// Seam: a class type that must not cross although it looks like any other class (the `Task`
    /// handle of L18).
    fn refuses(&self, _ty: TypeId) -> Option<Refused> {
        None
    }

    /// Seam: a host object or class object that crosses as an `Arc` instead of being copied or
    /// refused (`Shared`, `Channel`, `Mutex` handles; a host type registered thread-shareable).
    /// Asked for every host object and class object before anything else is.
    fn crosses_as_is(&self, _v: &Value) -> Option<Arc<dyn CrossesAsIs>> {
        None
    }
}

/// A copy that knows no program: types are shown as `type#N`.
pub struct Unnamed;

impl SendEnv for Unnamed {}

impl SendEnv for Core {
    fn type_name(&self, ty: TypeId) -> String {
        match self.types.get(ty.0 as usize) {
            Some(def) => def.display_name().into_owned(),
            None => format!("type#{}", ty.0),
        }
    }

    fn field_name(&self, ty: TypeId, index: u32) -> Option<String> {
        use colibri_check::hir::TypeDefKind;
        match &self.types.get(ty.0 as usize)?.kind {
            TypeDefKind::Struct { fields } | TypeDefKind::Class { fields } => {
                Some(fields.get(index as usize)?.name.clone())
            }
            _ => None,
        }
    }

    fn variant_name(&self, ty: TypeId, variant: u32) -> Option<String> {
        let def = self.types.get(ty.0 as usize)?;
        Some(def.variants().get(variant as usize)?.name.clone())
    }

    fn payload_name(&self, ty: TypeId, variant: u32, index: u32) -> Option<String> {
        let def = self.types.get(ty.0 as usize)?;
        let v = def.variants().get(variant as usize)?;
        v.named
            .then(|| v.fields.get(index as usize).map(|f| f.name.clone()))
            .flatten()
    }
}

// ---- refusals ----

/// Why a value cannot cross.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A host object, by the name of its type.
    Host(String),
    Iterator,
    Coroutine,
    CoGroup,
    Signal,
    Task,
    /// The graph nests deeper than the native stack allows.
    TooDeep,
}

/// A value that cannot be sent to another thread, and where it sits in what was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotSendable {
    /// Where the offender is, as a user would write it: `Job.conn`, `Report.rows[3].conn`,
    /// `closure.handle`. Empty when the offender is the value itself.
    pub path: String,
    pub refused: Refused,
}

impl fmt::Display for NotSendable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str("this value")?;
        } else {
            write!(f, "`{}`", self.path)?;
        }
        match &self.refused {
            Refused::TooDeep => f.write_str(" is nested too deeply to send to another thread"),
            other => {
                f.write_str(" is ")?;
                match other {
                    Refused::Host(name) => write!(f, "a `{name}`")?,
                    Refused::Iterator => f.write_str("an iterator")?,
                    Refused::Coroutine => f.write_str("a `Coroutine`")?,
                    Refused::CoGroup => f.write_str("a `CoGroup`")?,
                    Refused::Signal => f.write_str("a `Signal`")?,
                    Refused::Task => f.write_str("a `Task`")?,
                    Refused::TooDeep => unreachable!(),
                }
                f.write_str(", which cannot be sent to another thread")
            }
        }
    }
}

impl std::error::Error for NotSendable {}

/// One step of the way down to a value, kept small and named only if an error needs it.
#[derive(Clone, Copy)]
enum Seg {
    /// A field of a struct or class.
    Field(TypeId, u32),
    /// A payload field of an enum variant.
    Payload(TypeId, u32, u32),
    /// An element of a tuple.
    Item(u32),
    /// An element of a list or set.
    Index(u32),
    /// The key (`true`) or the value of the n-th entry of a map.
    Entry(u32, bool),
    /// A capture of a closure.
    Capture(FnId, u32),
}

/// The most steps of a path that an error shows: a graph that is too deep has a path of thousands.
const SHOWN_STEPS: usize = 12;

fn render(env: &dyn SendEnv, path: &[Seg]) -> String {
    let mut out = String::new();
    let elided = path.len().saturating_sub(SHOWN_STEPS);
    for (n, seg) in path.iter().enumerate() {
        if elided > 0 && n >= SHOWN_STEPS / 2 && n < SHOWN_STEPS / 2 + elided {
            if n == SHOWN_STEPS / 2 {
                out.push_str(&format!("...({elided} levels)..."));
            }
            continue;
        }
        match *seg {
            Seg::Field(ty, i) => {
                if n == 0 {
                    out.push_str(&env.type_name(ty));
                }
                out.push('.');
                match env.field_name(ty, i) {
                    Some(name) => out.push_str(&name),
                    None => out.push_str(&i.to_string()),
                }
            }
            Seg::Payload(ty, v, i) => {
                if n == 0 {
                    out.push_str(&env.type_name(ty));
                    out.push('.');
                    match env.variant_name(ty, v) {
                        Some(name) => out.push_str(&name),
                        None => out.push_str(&format!("variant{v}")),
                    }
                }
                out.push('.');
                match env.payload_name(ty, v, i) {
                    Some(name) => out.push_str(&name),
                    None => out.push_str(&i.to_string()),
                }
            }
            Seg::Item(i) => out.push_str(&format!(".{i}")),
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
            Seg::Entry(i, key) => {
                out.push_str(&format!("[entry {i}].{}", if key { "key" } else { "value" }));
            }
            Seg::Capture(f, i) => {
                if n == 0 {
                    out.push_str("closure");
                }
                out.push('.');
                match env.capture_name(f, i) {
                    Some(name) => out.push_str(&name),
                    None => out.push_str(&format!("captures[{i}]")),
                }
            }
        }
    }
    out
}

// ---- Value to Portable ----

/// Copies `v` into its portable form.
///
/// The copy uses the native stack to the depth of the value's own nesting (`stack`), and fails with
/// [`Refused::TooDeep`] rather than overflow it. The sending thread's memory account pays for what
/// it allocates, like any other work of the entry it runs in.
pub fn to_portable(
    v: &Value,
    env: &dyn SendEnv,
    stack: StackGuard,
) -> Result<Portable, NotSendable> {
    let mut walk = ToPortable {
        env,
        stack,
        shared: HashMap::new(),
        mutable: HashMap::new(),
        objs: HashMap::new(),
        next: 0,
        path: Vec::new(),
    };
    walk.visit(v)
}

/// What the walk knows of a value (a struct, tuple, enum, interface value or closure) that more
/// than one thing holds.
struct SharedSlot {
    id: u32,
    /// Its copy is complete, so a later visit may refer to it.
    done: bool,
}

/// What the walk knows of a class object it has met.
struct ObjSlot {
    id: u32,
    /// Its place in the output has been written (a `weak` reference may name an object first).
    defined: bool,
}

struct ToPortable<'e> {
    env: &'e dyn SendEnv,
    stack: StackGuard,
    /// The values held more than once, by address.
    shared: HashMap<usize, SharedSlot>,
    /// The lists, maps, sets, byte buffers and cells, by address.
    mutable: HashMap<usize, u32>,
    /// The class objects, by address.
    objs: HashMap<usize, ObjSlot>,
    next: u32,
    path: Vec<Seg>,
}

impl ToPortable<'_> {
    fn fresh_id(&mut self) -> u32 {
        let id = self.next;
        self.next += 1;
        id
    }

    fn refuse(&self, refused: Refused) -> NotSendable {
        NotSendable {
            path: render(self.env, &self.path),
            refused,
        }
    }

    /// An object that can be changed: copied once, and a `Ref` to it ever after (also while its
    /// copy is under way, which is how a cycle ends).
    fn changeable(
        &mut self,
        addr: usize,
        build: impl FnOnce(&mut Self, u32) -> Result<Portable, NotSendable>,
    ) -> Result<Portable, NotSendable> {
        if let Some(&id) = self.mutable.get(&addr) {
            return Ok(Portable::Ref(id));
        }
        let id = self.fresh_id();
        self.mutable.insert(addr, id);
        build(self, id)
    }

    /// A value held by `strong` things. One that nothing else holds cannot be reached twice, so it
    /// needs no id; one that is complete is not copied again.
    fn value_node(
        &mut self,
        addr: usize,
        strong: usize,
        build: impl FnOnce(&mut Self, Option<u32>) -> Result<Portable, NotSendable>,
    ) -> Result<Portable, NotSendable> {
        if strong <= 1 {
            return build(self, None);
        }
        match self.shared.get(&addr) {
            Some(slot) if slot.done => return Ok(Portable::Ref(slot.id)),
            // Reached again through an object that it holds: copy it again (the copy ends at
            // that object).
            Some(_) => return build(self, None),
            None => {}
        }
        let id = self.fresh_id();
        self.shared.insert(addr, SharedSlot { id, done: false });
        let out = build(self, Some(id))?;
        if let Some(slot) = self.shared.get_mut(&addr) {
            slot.done = true;
        }
        Ok(out)
    }

    fn seq(
        &mut self,
        items: &[Value],
        seg: impl Fn(u32) -> Seg,
    ) -> Result<Box<[Portable]>, NotSendable> {
        let mut out = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            self.path.push(seg(i as u32));
            out.push(self.visit(item)?);
            self.path.pop();
        }
        Ok(out.into_boxed_slice())
    }

    fn visit(&mut self, v: &Value) -> Result<Portable, NotSendable> {
        if self.stack.exceeded() {
            return Err(self.refuse(Refused::TooDeep));
        }
        // Each kind that has children is a function of its own, so that the frame of the recursion
        // holds what the kind on the path needs and not what every kind needs.
        Ok(match v {
            Value::Unit => Portable::Unit,
            Value::Bool(b) => Portable::Bool(*b),
            Value::Int(i) => Portable::Int(*i),
            Value::U64(i) => Portable::U64(*i),
            Value::Ptr(p) => Portable::Ptr(*p),
            Value::Float(x) => Portable::Float(*x),
            Value::F32(x) => Portable::F32(*x),
            Value::Char(c) => Portable::Char(*c),
            Value::Str(s) => Portable::Str(Box::from(&**s)),
            Value::Bytes(b) => Portable::Bytes(Box::from(&**b)),
            Value::Range(r) => Portable::Range(**r),
            Value::Fn(f, name) => Portable::Fn(*f, Box::from(&**name)),
            Value::ByteBuf(b) => self.byte_buf(b),
            Value::Struct(s) => self.struct_node(s)?,
            Value::Tuple(items) => self.tuple_node(items)?,
            Value::Enum(e) => self.enum_node(e)?,
            Value::SomeObj(o) => self.some_obj(o)?,
            Value::Dyn(d) => self.dyn_node(d)?,
            Value::Closure(c) => self.closure_node(c)?,
            Value::List(l) => self.list_node(l)?,
            Value::Set(s) => self.set_node(s)?,
            Value::Map(m) => self.map_node(m)?,
            Value::Cell(c) => self.cell_node(c)?,
            Value::Obj(o) => self.obj(v, o)?,
            Value::Weak(w) => Portable::Weak(self.weak(w)),
            Value::Host(r) => {
                if let Some(handle) = self.env.crosses_as_is(v) {
                    return Ok(Portable::Handle(handle));
                }
                return Err(self.refuse(Refused::Host(self.env.type_name(r.ty))));
            }
            Value::Iter(_) => return Err(self.refuse(Refused::Iterator)),
        })
    }

    #[inline(never)]
    fn byte_buf(&mut self, b: &Rc<RefCell<Vec<u8>>>) -> Portable {
        let addr = Rc::as_ptr(b) as usize;
        match self.changeable(addr, |_, id| {
            Ok(Portable::ByteBuf {
                id,
                bytes: b.borrow().as_slice().into(),
            })
        }) {
            Ok(p) => p,
            Err(_) => unreachable!("a byte buffer always copies"),
        }
    }

    #[inline(never)]
    fn struct_node(&mut self, s: &Rc<StructVal>) -> Result<Portable, NotSendable> {
        self.value_node(Rc::as_ptr(s) as usize, Rc::strong_count(s), |me, id| {
            Ok(Portable::Struct {
                id,
                ty: s.ty,
                fields: me.seq(&s.fields, |i| Seg::Field(s.ty, i))?,
            })
        })
    }

    #[inline(never)]
    fn tuple_node(&mut self, items: &Rc<[Value]>) -> Result<Portable, NotSendable> {
        let addr = Rc::as_ptr(items) as *const u8 as usize;
        self.value_node(addr, Rc::strong_count(items), |me, id| {
            Ok(Portable::Tuple {
                id,
                items: me.seq(items, Seg::Item)?,
            })
        })
    }

    #[inline(never)]
    fn enum_node(&mut self, e: &Rc<EnumVal>) -> Result<Portable, NotSendable> {
        self.value_node(Rc::as_ptr(e) as usize, Rc::strong_count(e), |me, id| {
            me.enum_value(id, e.ty, e.variant, &e.payload)
        })
    }

    #[inline(never)]
    fn some_obj(&mut self, o: &Rc<ClassObj>) -> Result<Portable, NotSendable> {
        let inner = Value::Obj(Rc::clone(o));
        self.enum_value(None, OPTION_TYPE, VARIANT_SOME, std::slice::from_ref(&inner))
    }

    #[inline(never)]
    fn dyn_node(&mut self, d: &Rc<DynVal>) -> Result<Portable, NotSendable> {
        self.value_node(Rc::as_ptr(d) as usize, Rc::strong_count(d), |me, id| {
            Ok(Portable::Dyn {
                id,
                impl_id: d.impl_id,
                val: Box::new(me.visit(&d.val)?),
            })
        })
    }

    #[inline(never)]
    fn closure_node(&mut self, c: &Rc<ClosureObj>) -> Result<Portable, NotSendable> {
        self.value_node(Rc::as_ptr(c) as usize, Rc::strong_count(c), |me, id| {
            let mut captures = Vec::with_capacity(c.captures.len());
            for (i, cap) in c.captures.iter().enumerate() {
                me.path.push(Seg::Capture(c.f, i as u32));
                captures.push(PortableCapture {
                    inner: cap.inner,
                    mode: cap.mode,
                    value: me.visit(&cap.value)?,
                });
                me.path.pop();
            }
            Ok(Portable::Closure {
                id,
                f: c.f,
                generation: c.generation,
                captures: captures.into_boxed_slice(),
            })
        })
    }

    #[inline(never)]
    fn list_node(&mut self, l: &Rc<ListObj>) -> Result<Portable, NotSendable> {
        self.changeable(Rc::as_ptr(l) as usize, |me, id| {
            Ok(Portable::List {
                id,
                items: l.with(|items| me.seq(items, Seg::Index))?,
            })
        })
    }

    #[inline(never)]
    fn set_node(&mut self, s: &Rc<SetObj>) -> Result<Portable, NotSendable> {
        self.changeable(Rc::as_ptr(s) as usize, |me, id| {
            let items = s.with(|m| -> Result<Vec<Portable>, NotSendable> {
                let mut out = Vec::with_capacity(m.len());
                for (n, (item, _)) in m.iter().enumerate() {
                    me.path.push(Seg::Index(n as u32));
                    out.push(me.visit(item)?);
                    me.path.pop();
                }
                Ok(out)
            })?;
            Ok(Portable::Set {
                id,
                items: items.into_boxed_slice(),
            })
        })
    }

    #[inline(never)]
    fn map_node(&mut self, m: &Rc<MapObj>) -> Result<Portable, NotSendable> {
        self.changeable(Rc::as_ptr(m) as usize, |me, id| {
            let entries = m.with(|om| -> Result<Vec<(Portable, Portable)>, NotSendable> {
                let mut out = Vec::with_capacity(om.len());
                for (n, (k, v)) in om.iter().enumerate() {
                    me.path.push(Seg::Entry(n as u32, true));
                    let key = me.visit(k)?;
                    me.path.pop();
                    me.path.push(Seg::Entry(n as u32, false));
                    let value = me.visit(v)?;
                    me.path.pop();
                    out.push((key, value));
                }
                Ok(out)
            })?;
            Ok(Portable::Map {
                id,
                entries: entries.into_boxed_slice(),
            })
        })
    }

    #[inline(never)]
    fn cell_node(&mut self, c: &Rc<CellObj>) -> Result<Portable, NotSendable> {
        self.changeable(Rc::as_ptr(c) as usize, |me, id| {
            Ok(Portable::Cell {
                id,
                value: Box::new(c.with(|inner| me.visit(inner))?),
            })
        })
    }

    #[inline(never)]
    fn enum_value(
        &mut self,
        id: Option<u32>,
        ty: TypeId,
        variant: u32,
        payload: &[Value],
    ) -> Result<Portable, NotSendable> {
        Ok(Portable::Enum {
            id,
            ty,
            variant,
            payload: self.seq(payload, |i| Seg::Payload(ty, variant, i))?,
        })
    }

    #[inline(never)]
    fn obj(&mut self, v: &Value, o: &Rc<ClassObj>) -> Result<Portable, NotSendable> {
        if let Some(handle) = self.env.crosses_as_is(v) {
            return Ok(Portable::Handle(handle));
        }
        let ty = o.ty();
        let refused = match ty {
            COROUTINE_TYPE => Some(Refused::Coroutine),
            COGROUP_TYPE => Some(Refused::CoGroup),
            SIGNAL_TYPE => Some(Refused::Signal),
            _ => self.env.refuses(ty),
        };
        if let Some(refused) = refused {
            return Err(self.refuse(refused));
        }
        let addr = Rc::as_ptr(o) as usize;
        let id = match self.objs.get_mut(&addr) {
            Some(slot) if slot.defined => return Ok(Portable::Ref(slot.id)),
            // A `weak` reference named it before this visit gave it a place.
            Some(slot) => {
                slot.defined = true;
                slot.id
            }
            None => {
                let id = self.fresh_id();
                self.objs.insert(addr, ObjSlot { id, defined: true });
                id
            }
        };
        let fields = o.with_fields(|fields| self.seq(fields, |i| Seg::Field(ty, i)))?;
        Ok(Portable::Obj { id, ty, fields })
    }

    /// A `weak` reference names its target by id, whether or not the target has been visited yet;
    /// a target nothing else of the graph reaches ends up as a dead reference on the other side.
    fn weak(&mut self, w: &Weak<ClassObj>) -> Option<WeakRef> {
        let target = w.upgrade()?;
        let addr = Rc::as_ptr(&target) as usize;
        let id = match self.objs.get(&addr) {
            Some(slot) => slot.id,
            None => {
                let id = self.fresh_id();
                self.objs.insert(addr, ObjSlot { id, defined: false });
                id
            }
        };
        Some(WeakRef {
            id,
            ty: target.ty(),
            fields: target.with_fields(|f| f.len()) as u32,
        })
    }
}

// ---- Portable to Value ----

/// Why a portable graph could not be rebuilt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildError {
    /// The graph nests deeper than the receiving thread's native stack allows.
    TooDeep,
    /// A `Ref` names an object that the graph never defines: not a graph [`to_portable`] made.
    Malformed,
}

impl fmt::Display for RebuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RebuildError::TooDeep => f.write_str("the value is nested too deeply to receive"),
            RebuildError::Malformed => f.write_str("the value received is not a valid graph"),
        }
    }
}

impl std::error::Error for RebuildError {}

/// Rebuilds a portable value on this thread, with fresh `Rc`s, charged to the entry that is
/// running. Class objects and closures are registered with `core`'s leak detector when it is on.
pub fn from_portable(
    portable: Portable,
    core: &mut Core,
    stack: StackGuard,
) -> Result<Value, RebuildError> {
    rebuild(&portable, core, stack)
}

/// [`from_portable`] of a graph that stays where it is (a frozen `Shared` value is rebuilt once
/// on each thread that reads it).
pub fn rebuild(
    portable: &Portable,
    core: &mut Core,
    stack: StackGuard,
) -> Result<Value, RebuildError> {
    let mut b = Rebuild {
        core,
        stack,
        built: HashMap::new(),
        placeholders: HashSet::new(),
    };
    match b.value(portable) {
        Ok(v) => {
            b.finish();
            Ok(v)
        }
        Err(e) => {
            b.abandon();
            Err(e)
        }
    }
}

struct Rebuild<'c> {
    core: &'c mut Core,
    stack: StackGuard,
    /// What each id has been built as.
    built: HashMap<u32, Value>,
    /// Class objects made for a `weak` reference that came before its target; the target's own node
    /// takes them over, and any left at the end were not part of the graph.
    placeholders: HashSet<u32>,
}

impl Rebuild<'_> {
    fn register(&mut self, id: Option<u32>, v: &Value) {
        if let Some(id) = id {
            self.built.insert(id, v.clone());
        }
    }

    fn all(&mut self, items: &[Portable]) -> Result<Vec<Value>, RebuildError> {
        items.iter().map(|p| self.value(p)).collect()
    }

    fn value(&mut self, p: &Portable) -> Result<Value, RebuildError> {
        if self.stack.exceeded() {
            return Err(RebuildError::TooDeep);
        }
        Ok(match p {
            Portable::Unit => Value::Unit,
            Portable::Bool(b) => Value::Bool(*b),
            Portable::Int(i) => Value::Int(*i),
            Portable::U64(i) => Value::U64(*i),
            Portable::Ptr(a) => Value::Ptr(*a),
            Portable::Float(x) => Value::Float(*x),
            Portable::F32(x) => Value::F32(*x),
            Portable::Char(c) => Value::Char(*c),
            Portable::Str(s) => Value::Str(Rc::from(&**s)),
            Portable::Bytes(b) => Value::Bytes(Rc::from(&**b)),
            Portable::Range(r) => Value::Range(Rc::new(*r)),
            Portable::Fn(f, name) => Value::Fn(*f, Rc::from(&**name)),
            Portable::ByteBuf { id, bytes } => {
                let v = Value::ByteBuf(Rc::new(RefCell::new(bytes.to_vec())));
                self.built.insert(*id, v.clone());
                v
            }
            Portable::Struct { id, ty, fields } => {
                let mut out = StructFields::with_capacity(fields.len());
                for f in fields.iter() {
                    out.push(self.value(f)?);
                }
                let v = Value::Struct(Rc::new(StructVal {
                    ty: *ty,
                    fields: out,
                }));
                self.register(*id, &v);
                v
            }
            Portable::Tuple { id, items } => {
                let v = Value::Tuple(Rc::from(self.all(items)?));
                self.register(*id, &v);
                v
            }
            Portable::Enum {
                id,
                ty,
                variant,
                payload,
            } => {
                if id.is_none()
                    && payload.is_empty()
                    && *ty == OPTION_TYPE
                    && *variant == VARIANT_NONE
                {
                    return Ok(self.core.none());
                }
                let mut out = Payload::with_capacity(payload.len());
                for f in payload.iter() {
                    out.push(self.value(f)?);
                }
                let v = Value::Enum(Rc::new(EnumVal {
                    ty: *ty,
                    variant: *variant,
                    payload: out,
                }));
                self.register(*id, &v);
                v
            }
            Portable::Dyn { id, impl_id, val } => {
                let v = Value::Dyn(Rc::new(DynVal {
                    impl_id: *impl_id,
                    val: self.value(val)?,
                }));
                self.register(*id, &v);
                v
            }
            Portable::Closure {
                id,
                f,
                generation,
                captures,
            } => {
                let mut out = Vec::with_capacity(captures.len());
                for c in captures.iter() {
                    out.push(Captured {
                        inner: c.inner,
                        mode: c.mode,
                        value: self.value(&c.value)?,
                    });
                }
                let closure = Rc::new(ClosureObj {
                    f: *f,
                    generation: *generation,
                    captures: out,
                });
                self.core.track_closure(&closure, None);
                let v = Value::Closure(closure);
                self.register(*id, &v);
                v
            }
            Portable::List { id, items } => {
                let list = ListObj::new(Vec::with_capacity(items.len()));
                let v = Value::List(Rc::clone(&list));
                self.built.insert(*id, v.clone());
                for p in items.iter() {
                    let item = self.value(p)?;
                    let _ = list.push(item);
                }
                v
            }
            Portable::Map { id, entries } => {
                let map = MapObj::new(OrderedMap::new());
                let v = Value::Map(Rc::clone(&map));
                self.built.insert(*id, v.clone());
                for (k, e) in entries.iter() {
                    let (k, e) = (self.value(k)?, self.value(e)?);
                    let _ = map.insert(k, e);
                }
                v
            }
            Portable::Set { id, items } => {
                let set = SetObj::new(OrderedMap::new());
                let v = Value::Set(Rc::clone(&set));
                self.built.insert(*id, v.clone());
                for p in items.iter() {
                    let item = self.value(p)?;
                    let _ = set.add(item);
                }
                v
            }
            Portable::Cell { id, value } => {
                let cell = CellObj::new(Value::Unit);
                let v = Value::Cell(Rc::clone(&cell));
                self.built.insert(*id, v.clone());
                let inner = self.value(value)?;
                let _ = cell.set(inner);
                v
            }
            Portable::Obj { id, ty, fields } => {
                let obj = match self.built.get(id) {
                    // Made for a `weak` reference that came first.
                    Some(Value::Obj(o)) if self.placeholders.remove(id) => Rc::clone(o),
                    _ => {
                        let o = ClassObj::new_exact(
                            *ty,
                            ObjFields::from_elem(Value::Unit, fields.len()),
                        );
                        self.built.insert(*id, Value::Obj(Rc::clone(&o)));
                        o
                    }
                };
                self.core.track_object(&obj, None);
                for (i, p) in fields.iter().enumerate() {
                    let field = self.value(p)?;
                    let _ = obj.set_field(i, field);
                }
                Value::Obj(obj)
            }
            Portable::Weak(None) => Value::Weak(Weak::new()),
            Portable::Weak(Some(w)) => self.weak(w),
            Portable::Ref(id) => match self.built.get(id) {
                Some(v) => v.clone(),
                None => return Err(RebuildError::Malformed),
            },
            Portable::Handle(h) => h.into_value(self.core),
        })
    }

    /// A reference to a class object that may not have been built yet: it gets an empty one of the
    /// right shape, which its own node takes over.
    fn weak(&mut self, w: &WeakRef) -> Value {
        if let Some(Value::Obj(o)) = self.built.get(&w.id) {
            return Value::Weak(Rc::downgrade(o));
        }
        let obj = ClassObj::new_exact(w.ty, ObjFields::from_elem(Value::Unit, w.fields as usize));
        let weak = Rc::downgrade(&obj);
        self.built.insert(w.id, Value::Obj(obj));
        self.placeholders.insert(w.id);
        Value::Weak(weak)
    }

    /// The copy is complete. The objects made for `weak` references whose targets were not part of
    /// the graph die here, which leaves those references dead.
    fn finish(&mut self) {
        for id in std::mem::take(&mut self.placeholders) {
            if let Some(Value::Obj(o)) = self.built.remove(&id) {
                drop(o.take_fields());
            }
        }
    }

    /// The copy failed: take apart what was made so that nothing is kept alive by a cycle and no
    /// class object is dropped without being released. User `Drop` hooks do not run, because no
    /// program has seen these objects.
    fn abandon(&mut self) {
        let mut taken: Vec<Value> = Vec::new();
        for v in self.built.values() {
            match v {
                Value::Obj(o) => taken.extend(o.take_fields()),
                Value::List(l) => taken.extend(l.take_all()),
                Value::Map(m) => taken.extend(m.take_all().into_iter().flat_map(|(k, v)| [k, v])),
                Value::Set(s) => taken.extend(s.take_all()),
                Value::Cell(c) => taken.push(c.take()),
                _ => {}
            }
        }
        // Every object that can be changed is in `built`, so each has been flagged released before
        // anything it held is dropped.
        drop(taken);
        self.built.clear();
    }
}

#[cfg(test)]
mod tests;
