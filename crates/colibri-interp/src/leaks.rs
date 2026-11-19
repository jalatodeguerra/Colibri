//! The leak detector (ARCHITECTURE §18): finds reference cycles that nothing reaches.
//!
//! Colibri counts references, so a cycle of class objects (or of closures and the variables
//! they capture) is never freed. DESIGN §11.5 leaves finding them to a tool: this is it, a
//! synchronous *trial deletion* in the manner of Bacon and Rajan. It is a diagnostic: it
//! reports and never frees.
//!
//! **Registry.** While tracking is on, every class object and closure is entered, as a weak
//! reference with its allocation site, when it is made. With tracking off there is no registry
//! and creating an object costs one `Option` test.
//!
//! **Trial deletion.** From the registered objects the detector walks the graph of reference
//! counted nodes (class objects, lists, maps, sets, closures, cells, iterators, and the
//! structs, tuples, enums and interface boxes that hold them), cloning one handle per node
//! and recording each edge. For every node it subtracts the edges that come from other nodes
//! of the graph from the node's reference count; what is left is the number of references
//! from outside: frames, temporaries, globals, host handles, suspended coroutines, anything
//! that is not a node. A node with an outside reference is live, and so is everything it
//! reaches. The nodes left over are held only by each other: garbage. The cyclic strongly
//! connected components of that garbage are the leaks; the rest of it is *retained* by them.
//!
//! No root has to be enumerated, so a reference the detector cannot see (a Rust local, a
//! host-held value) is simply an outside reference: the detector can miss a leak but cannot
//! report a live object. `weak` references are not counted by `Rc` and are not edges.

use std::collections::HashMap;
use std::fmt;
use std::rc::{Rc, Weak};

use colibri_check::hir::{TypeDef, TypeDefKind};
use colibri_syntax::Span;

use crate::closure::ClosureObj;
use crate::eval::Interp;
use crate::heap::ClassObj;
use crate::value::Value;

/// One reported leak: a cycle that is unreachable from every root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leak {
    /// The class of every object in the cycle (`closure` for a closure), sorted.
    pub objects: Vec<String>,
    /// One cycle through it, e.g. `Node(name: "a") -> next -> Node(name: "b") -> prev ->
    /// Node(name: "a")`; a long cycle is abbreviated.
    pub path: String,
    /// Where the objects on the path were made (`file:line`), when known.
    pub sites: Vec<String>,
    /// How many more objects only this cycle keeps alive.
    pub retained: usize,
}

impl fmt::Display for Leak {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.objects.len();
        let what = if n == 1 { "object" } else { "objects" };
        write!(f, "leak: a cycle of {n} {what}: {}", self.path)?;
        if !self.sites.is_empty() {
            write!(f, "\n  allocated at {}", self.sites.join(", "))?;
        }
        if self.retained > 0 {
            write!(f, "\n  also keeps {} more alive", self.retained)?;
        }
        Ok(())
    }
}

/// The weak registry of live class objects and closures.
pub(crate) struct LeakTracker {
    entries: Vec<Entry>,
    /// The size at which dead entries are dropped; it follows the live count.
    prune_at: usize,
}

struct Entry {
    target: Target,
    span: Option<Span>,
}

enum Target {
    Obj(Weak<ClassObj>),
    Closure(Weak<ClosureObj>),
}

impl Target {
    fn alive(&self) -> bool {
        match self {
            Target::Obj(w) => w.strong_count() > 0,
            Target::Closure(w) => w.strong_count() > 0,
        }
    }
}

impl LeakTracker {
    pub(crate) fn new() -> Self {
        LeakTracker {
            entries: Vec::new(),
            prune_at: 1024,
        }
    }

    fn add(&mut self, target: Target, span: Option<Span>) {
        if self.entries.len() >= self.prune_at {
            // A dead entry keeps its allocation until the weak reference goes: drop them.
            self.entries.retain(|e| e.target.alive());
            self.prune_at = (self.entries.len() * 2).max(1024);
        }
        self.entries.push(Entry { target, span });
    }
}

impl Interp {
    /// Turns the registry on or off. Only objects made while it is on are known to the
    /// detector, so turn it on before the program loads.
    pub fn set_track_leaks(&mut self, on: bool) {
        self.leaks = on.then(|| Box::new(LeakTracker::new()));
    }

    pub fn tracks_leaks(&self) -> bool {
        self.leaks.is_some()
    }

    pub(crate) fn track_object(&mut self, obj: &Rc<ClassObj>, span: Option<Span>) {
        if let Some(t) = &mut self.leaks {
            t.add(Target::Obj(Rc::downgrade(obj)), span);
        }
    }

    pub(crate) fn track_closure(&mut self, c: &Rc<ClosureObj>, span: Option<Span>) {
        if let Some(t) = &mut self.leaks {
            t.add(Target::Closure(Rc::downgrade(c)), span);
        }
    }

    /// The cycles that nothing reaches (see the module documentation). Empty when tracking
    /// is off. Leaks come in allocation order of their oldest object.
    pub fn find_leaks(&self) -> Vec<Leak> {
        let Some(tracker) = &self.leaks else {
            return Vec::new();
        };
        let mut graph = Graph::default();
        for e in &tracker.entries {
            let value = match &e.target {
                Target::Obj(w) => w.upgrade().map(Value::Obj),
                Target::Closure(w) => w.upgrade().map(Value::Closure),
            };
            if let Some(v) = value {
                graph.intern(v, e.span);
            }
        }
        graph.explore();
        let garbage = graph.garbage();
        graph.report(self, &garbage)
    }
}

// ---- the graph ----

/// How an edge leaves its source node (enough to name it when reporting).
#[derive(Clone, Copy, Debug)]
enum Edge {
    Field(u32),
    Index(u32),
    Key(u32),
    Val(u32),
    Item(u32),
    Capture(u32),
    Payload(u32),
    /// A cell's content, an iterator's source, the value inside an interface box.
    Through,
}

struct Node {
    /// The one handle the detector holds (it is subtracted from the count).
    value: Value,
    edges: Vec<(Edge, usize)>,
    site: Option<Span>,
}

#[derive(Default)]
struct Graph {
    nodes: Vec<Node>,
    index: HashMap<usize, usize>,
}

fn key(v: &Value) -> Option<usize> {
    Some(match v {
        Value::Obj(r) => Rc::as_ptr(r) as *const u8 as usize,
        Value::List(r) => Rc::as_ptr(r) as *const u8 as usize,
        Value::Map(r) => Rc::as_ptr(r) as *const u8 as usize,
        Value::Set(r) => Rc::as_ptr(r) as *const u8 as usize,
        Value::Closure(r) => Rc::as_ptr(r) as *const u8 as usize,
        Value::Cell(r) => Rc::as_ptr(r) as *const u8 as usize,
        Value::Iter(r) => Rc::as_ptr(r) as *const u8 as usize,
        Value::Struct(r) => Rc::as_ptr(r) as *const u8 as usize,
        Value::Tuple(r) => Rc::as_ptr(r) as *const u8 as usize,
        Value::Enum(r) => Rc::as_ptr(r) as *const u8 as usize,
        Value::Dyn(r) => Rc::as_ptr(r) as *const u8 as usize,
        _ => return None,
    })
}

fn strong_count(v: &Value) -> usize {
    match v {
        Value::Obj(r) => Rc::strong_count(r),
        Value::List(r) => Rc::strong_count(r),
        Value::Map(r) => Rc::strong_count(r),
        Value::Set(r) => Rc::strong_count(r),
        Value::Closure(r) => Rc::strong_count(r),
        Value::Cell(r) => Rc::strong_count(r),
        Value::Iter(r) => Rc::strong_count(r),
        Value::Struct(r) => Rc::strong_count(r),
        Value::Tuple(r) => Rc::strong_count(r),
        Value::Enum(r) => Rc::strong_count(r),
        Value::Dyn(r) => Rc::strong_count(r),
        _ => 0,
    }
}

fn push(out: &mut Vec<(Edge, Value)>, edge: Edge, v: &Value) {
    if key(v).is_some() {
        out.push((edge, v.clone()));
    }
}

/// The nodes `v` holds a strong reference to, each as a fresh handle.
fn children(v: &Value, out: &mut Vec<(Edge, Value)>) {
    let at = |i: usize| i as u32;
    match v {
        Value::Obj(o) => o.with_fields(|fs| {
            for (i, f) in fs.iter().enumerate() {
                push(out, Edge::Field(at(i)), f);
            }
        }),
        Value::List(l) => l.with(|items| {
            for (i, f) in items.iter().enumerate() {
                push(out, Edge::Index(at(i)), f);
            }
        }),
        Value::Map(m) => m.with(|map| {
            for (i, (k, v)) in map.iter().enumerate() {
                push(out, Edge::Key(at(i)), k);
                push(out, Edge::Val(at(i)), v);
            }
        }),
        Value::Set(s) => s.with(|set| {
            for (i, (k, _)) in set.iter().enumerate() {
                push(out, Edge::Item(at(i)), k);
            }
        }),
        Value::Closure(c) => {
            for (i, cap) in c.captures.iter().enumerate() {
                push(out, Edge::Capture(at(i)), &cap.value);
            }
        }
        Value::Cell(c) => c.with(|inner| push(out, Edge::Through, inner)),
        Value::Iter(i) => {
            for inner in i.children() {
                push(out, Edge::Through, &inner);
            }
        }
        Value::Struct(s) => {
            for (i, f) in s.fields.iter().enumerate() {
                push(out, Edge::Field(at(i)), f);
            }
        }
        Value::Tuple(items) => {
            for (i, f) in items.iter().enumerate() {
                push(out, Edge::Index(at(i)), f);
            }
        }
        Value::Enum(e) => {
            for (i, f) in e.payload.iter().enumerate() {
                push(out, Edge::Payload(at(i)), f);
            }
        }
        Value::Dyn(d) => push(out, Edge::Through, &d.val),
        _ => {}
    }
}

impl Graph {
    /// The index of the node for `v`, adding it (and keeping this handle) if it is new.
    fn intern(&mut self, v: Value, site: Option<Span>) -> usize {
        let k = key(&v).expect("only nodes are interned");
        if let Some(&i) = self.index.get(&k) {
            return i;
        }
        let i = self.nodes.len();
        self.index.insert(k, i);
        self.nodes.push(Node {
            value: v,
            edges: Vec::new(),
            site,
        });
        i
    }

    /// Finds every node reachable from the registered ones and records the edges.
    fn explore(&mut self) {
        let mut scratch = Vec::new();
        let mut i = 0;
        while i < self.nodes.len() {
            scratch.clear();
            children(&self.nodes[i].value, &mut scratch);
            let mut edges = Vec::with_capacity(scratch.len());
            for (edge, v) in scratch.drain(..) {
                edges.push((edge, self.intern(v, None)));
            }
            self.nodes[i].edges = edges;
            i += 1;
        }
    }

    /// The nodes that no outside reference keeps alive, directly or through another node.
    fn garbage(&self) -> Vec<bool> {
        let n = self.nodes.len();
        let mut inside = vec![0usize; n];
        for node in &self.nodes {
            for &(_, to) in &node.edges {
                inside[to] += 1;
            }
        }
        let mut live = vec![false; n];
        let mut work = Vec::new();
        for (i, node) in self.nodes.iter().enumerate() {
            // One handle is the detector's own.
            let held = strong_count(&node.value).saturating_sub(1);
            if held > inside[i] {
                live[i] = true;
                work.push(i);
            }
        }
        while let Some(i) = work.pop() {
            for &(_, to) in &self.nodes[i].edges {
                if !live[to] {
                    live[to] = true;
                    work.push(to);
                }
            }
        }
        live.into_iter().map(|l| !l).collect()
    }

    /// The strongly connected components of the garbage that contain a cycle, each as its
    /// node indices in ascending order, ordered by their first node (Tarjan, iteratively).
    fn cycles(&self, garbage: &[bool]) -> Vec<Vec<usize>> {
        const NONE: usize = usize::MAX;
        let n = self.nodes.len();
        let mut order = vec![NONE; n];
        let mut low = vec![0usize; n];
        let mut on_stack = vec![false; n];
        let mut stack = Vec::new();
        let mut next = 0;
        let mut found = Vec::new();
        for root in 0..n {
            if !garbage[root] || order[root] != NONE {
                continue;
            }
            let mut calls = vec![(root, 0usize)];
            order[root] = next;
            low[root] = next;
            next += 1;
            stack.push(root);
            on_stack[root] = true;
            while let Some(&mut (v, ref mut at)) = calls.last_mut() {
                if let Some(&(_, w)) = self.nodes[v].edges.get(*at) {
                    *at += 1;
                    if !garbage[w] {
                        continue;
                    }
                    if order[w] == NONE {
                        order[w] = next;
                        low[w] = next;
                        next += 1;
                        stack.push(w);
                        on_stack[w] = true;
                        calls.push((w, 0));
                    } else if on_stack[w] {
                        low[v] = low[v].min(order[w]);
                    }
                    continue;
                }
                calls.pop();
                if let Some(&(parent, _)) = calls.last() {
                    low[parent] = low[parent].min(low[v]);
                }
                if low[v] == order[v] {
                    let mut comp = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    let cyclic =
                        comp.len() > 1 || self.nodes[v].edges.iter().any(|&(_, to)| to == v);
                    if cyclic {
                        comp.sort_unstable();
                        found.push(comp);
                    }
                }
            }
        }
        found.sort_by_key(|c| c[0]);
        found
    }

    fn report(&self, interp: &Interp, garbage: &[bool]) -> Vec<Leak> {
        let cycles = self.cycles(garbage);
        let mut in_cycle = vec![false; self.nodes.len()];
        for c in &cycles {
            for &i in c {
                in_cycle[i] = true;
            }
        }
        let mut claimed = vec![false; self.nodes.len()];
        let namer = Namer { interp };
        cycles
            .iter()
            .filter_map(|comp| {
                let first = comp
                    .iter()
                    .copied()
                    .find(|&i| namer.visible(&self.nodes[i].value))?;
                let mut objects: Vec<String> = comp
                    .iter()
                    .filter(|&&i| namer.visible(&self.nodes[i].value))
                    .map(|&i| namer.class(&self.nodes[i].value))
                    .collect();
                objects.sort();
                // The garbage that hangs off this cycle and is in no cycle itself.
                let mut retained = 0;
                let mut work: Vec<usize> = comp.clone();
                while let Some(i) = work.pop() {
                    for &(_, to) in &self.nodes[i].edges {
                        if garbage[to] && !in_cycle[to] && !claimed[to] {
                            claimed[to] = true;
                            if namer.visible(&self.nodes[to].value) {
                                retained += 1;
                            }
                            work.push(to);
                        }
                    }
                }
                let (path, shown) = self.path(&namer, comp, first);
                let sites = shown
                    .iter()
                    .filter_map(|&i| {
                        let span = self.nodes[i].site?;
                        let sources = &interp.host.sources;
                        let line = sources.line_col(span.file, span.start).0;
                        Some(format!(
                            "{} at {}:{line}",
                            namer.class(&self.nodes[i].value),
                            sources.name(span.file)
                        ))
                    })
                    .collect();
                Some(Leak {
                    objects,
                    path,
                    sites,
                    retained,
                })
            })
            .collect()
    }

    /// A shortest cycle through `start` inside `comp`, rendered, and the visible nodes on it.
    fn path(&self, namer: &Namer, comp: &[usize], start: usize) -> (String, Vec<usize>) {
        let mut parent: HashMap<usize, (usize, Edge)> = HashMap::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(start);
        let mut closing = None;
        'search: while let Some(v) = queue.pop_front() {
            for &(edge, to) in &self.nodes[v].edges {
                if comp.binary_search(&to).is_err() {
                    continue;
                }
                if to == start {
                    closing = Some((v, edge));
                    break 'search;
                }
                if let std::collections::hash_map::Entry::Vacant(slot) = parent.entry(to) {
                    slot.insert((v, edge));
                    queue.push_back(to);
                }
            }
        }
        let Some((last, last_edge)) = closing else {
            return (namer.describe(&self.nodes[start].value), vec![start]);
        };
        // Walk back to the start to get the hops in order.
        let mut hops = vec![(last, last_edge, start)];
        let mut at = last;
        while at != start {
            let (from, edge) = parent[&at];
            hops.push((from, edge, at));
            at = from;
        }
        hops.reverse();
        let mut groups: Vec<String> = Vec::new();
        let mut pending = String::new();
        let mut shown = vec![start];
        for &(from, edge, to) in &hops {
            let part = namer.edge(&self.nodes[from].value, edge);
            if !part.is_empty() {
                if !pending.is_empty() && !part.starts_with('[') {
                    pending.push('.');
                }
                pending.push_str(&part);
            }
            if namer.visible(&self.nodes[to].value) {
                let label = std::mem::take(&mut pending);
                let name = namer.describe(&self.nodes[to].value);
                groups.push(if label.is_empty() {
                    name
                } else {
                    format!("{label} -> {name}")
                });
                if to != start {
                    shown.push(to);
                }
            }
        }
        const KEEP: usize = 8;
        let mut parts = vec![namer.describe(&self.nodes[start].value)];
        if groups.len() > KEEP + 1 {
            parts.extend(groups[..KEEP].iter().cloned());
            parts.push(format!("... ({} more)", groups.len() - KEEP - 1));
            parts.push(groups[groups.len() - 1].clone());
        } else {
            parts.extend(groups);
        }
        shown.truncate(4);
        (parts.join(" -> "), shown)
    }
}

/// Names things for the report.
struct Namer<'a> {
    interp: &'a Interp,
}

impl Namer<'_> {
    /// Whether the node is something the program has a name for (an object or a closure);
    /// the rest (lists, cells, structs, ...) only show up inside edge labels.
    fn visible(&self, v: &Value) -> bool {
        matches!(v, Value::Obj(_) | Value::Closure(_))
    }

    fn types(&self) -> &[TypeDef] {
        &self.interp.types
    }

    fn class(&self, v: &Value) -> String {
        match v {
            Value::Obj(o) => self
                .types()
                .get(o.ty().0 as usize)
                .map_or_else(|| "object".to_string(), |d| d.name.clone()),
            _ => "closure".to_string(),
        }
    }

    /// `Node(name: "a")` when the class has a `name: str` field, else `Node`; `closure f`.
    fn describe(&self, v: &Value) -> String {
        match v {
            Value::Obj(o) => {
                let class = self.class(v);
                let named = self.types().get(o.ty().0 as usize).and_then(|d| {
                    let i = d.fields().iter().position(|f| {
                        f.name == "name" && f.ty == colibri_check::types::Ty::Str && !f.weak
                    })?;
                    match o.field(i)? {
                        Value::Str(s) => Some(s.to_string()),
                        _ => None,
                    }
                });
                match named {
                    Some(n) => format!("{class}(name: {n:?})"),
                    None => class,
                }
            }
            Value::Closure(c) => match self.interp.fns.get(c.f.0 as usize) {
                Some(body) if body.name.contains("closure") => body.name.clone(),
                Some(body) => format!("closure {}", body.name),
                None => "closure".to_string(),
            },
            _ => "?".to_string(),
        }
    }

    /// The text of one hop out of `from`.
    fn edge(&self, from: &Value, edge: Edge) -> String {
        let field_of = |ty: colibri_check::types::TypeId, i: u32| {
            self.types()
                .get(ty.0 as usize)
                .and_then(|d| d.fields().get(i as usize))
                .map_or_else(|| i.to_string(), |f| f.name.clone())
        };
        match (from, edge) {
            (Value::Obj(o), Edge::Field(i)) => field_of(o.ty(), i),
            (Value::Struct(s), Edge::Field(i)) => field_of(s.ty, i),
            (Value::List(_), Edge::Index(i)) => format!("[{i}]"),
            (Value::Tuple(_), Edge::Index(i)) => i.to_string(),
            (Value::Map(m), Edge::Val(i) | Edge::Key(i)) => {
                let shown = m.with(|map| {
                    map.iter().nth(i as usize).map(|(k, _)| match k {
                        Value::Str(s) => format!("{s:?}"),
                        other => crate::show::show(self.types(), other).into_owned(),
                    })
                });
                let key = shown.unwrap_or_else(|| format!("#{i}"));
                if matches!(edge, Edge::Key(_)) {
                    format!("[key {key}]")
                } else {
                    format!("[{key}]")
                }
            }
            (Value::Set(_), Edge::Item(i)) => format!("{{#{i}}}"),
            (Value::Closure(c), Edge::Capture(i)) => {
                let name = c.captures.get(i as usize).and_then(|cap| {
                    let body = self.interp.fns.get(c.f.0 as usize)?;
                    body.locals
                        .get(cap.inner.0 as usize)
                        .map(|l| l.name.clone())
                });
                format!("captures {}", name.unwrap_or_else(|| i.to_string()))
            }
            (Value::Enum(e), Edge::Payload(i)) => {
                let name = self
                    .types()
                    .get(e.ty.0 as usize)
                    .and_then(|d| match &d.kind {
                        TypeDefKind::Enum { variants } => {
                            variants.get(e.variant as usize).map(|v| v.name.clone())
                        }
                        _ => None,
                    });
                let name = name.unwrap_or_else(|| "variant".to_string());
                if e.ty == colibri_check::types::OPTION_TYPE {
                    // `x: T?` reads as `x`, not `x.some`.
                    String::new()
                } else if e.payload.len() > 1 {
                    format!("{name}.{i}")
                } else {
                    name
                }
            }
            _ => String::new(),
        }
    }
}
