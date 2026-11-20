//! `json.Value`: a JSON document whose shape the program does not know in advance.
//!
//! It is an opaque host class (a Colibri `enum` with payloads cannot be declared by a host
//! module), so scripts do not `match` on it: they ask it. `v.get("name")` and `v.at(0)` walk
//! down and give `none` when the step does not exist, `v.as_str()`, `v.as_int()`, ... read a
//! scalar as an option, and `v.kind()` says what it is. The tree is immutable and shares its
//! children, so `get` and `at` cost nothing proportional to the size of the subtree.
//!
//! A value nests as deeply as a program builds it (the constructors do not stop at a depth),
//! so nothing here walks a tree by recursion without a bound: dropping and `equals` are
//! iterative (`==` is `equals`), `Show` writes `...` below `MAX_DEPTH` levels, and encoding or
//! converting stops with an error there.
//!
//! Equality and printing: `a == b` is the structural comparison (same kind and content, maps
//! equal whatever the order of their keys, an `int` and a `float` never equal since `1` and
//! `1.0` are different JSON texts and different kinds, a `nan` equal to itself), the same as
//! `a.equals(b)`. `print(v)` and `"{v}"` give the compact JSON text (`Jv::show`). Both are hooks
//! of the host class (`TypeBuilder::show`, `eq` and `hash`: `a == b` gives `a.hash() == b.hash()`). Used as a `Map` key or a `Set` element a
//! value is found by `==` (a scan of the keys, see `HostType` hooks in EMBEDDING §2).
//! `v.to_json()` is the same text as a `Result` (an error for a `nan` or an infinity).
//!
//! Numbers: an integer literal (no fraction, no exponent) that fits an `i64` is an `int` and
//! survives a round trip exactly. An integer literal outside that range is kept as its digits
//! (`kind()` is `"int"`, `as_int()` is `none`, `as_float()` converts, `json.encode` writes the
//! digits back unchanged). Every other number is a `float`.

use std::collections::HashMap;
use std::sync::Arc;

use colibri_runtime::{
    Cx, Error, FromColibri, HostType, IntoColibri, Ref, TypeBuilder, TypeDesc, TypeRegistry, Value,
    ValueKind,
};

use super::{Json, MAX_DEPTH};

/// The most bytes `Show` writes for one value.
const SHOW_LIMIT: usize = 16 << 20;

/// A JSON value. Called `Value` in scripts (`json.Value`).
#[derive(Clone)]
pub(crate) enum Jv {
    Null,
    Bool(bool),
    Int(i64),
    /// An integer literal that does not fit an `i64`: its digits, as written.
    Big(Arc<str>),
    Float(f64),
    Str(Arc<str>),
    List(Arc<Vec<Jv>>),
    /// The entries in insertion order; the keys are distinct.
    Map(Arc<Vec<(String, Jv)>>),
}

/// Why a tree could not become a `Jv`: the message, and whether the content is something JSON
/// cannot hold at all (a `nan`, an infinity) as opposed to a number out of range.
pub(crate) struct JvError {
    pub(crate) unsupported: bool,
    pub(crate) message: String,
}

impl Drop for Jv {
    /// Takes a deep tree apart with a loop: the default drop would recurse once per level and
    /// overflow the stack on a list nested a hundred thousand deep.
    fn drop(&mut self) {
        let mut pending: Vec<Jv> = Vec::new();
        take_children(self, &mut pending);
        while let Some(mut next) = pending.pop() {
            take_children(&mut next, &mut pending);
            // `next` has no children now, so this drop is shallow.
        }
    }
}

/// Moves the children out of `jv` when this is the last holder of them.
fn take_children(jv: &mut Jv, into: &mut Vec<Jv>) {
    match jv {
        Jv::List(items) => {
            if let Some(items) = Arc::get_mut(items) {
                into.append(items);
            }
        }
        Jv::Map(entries) => {
            if let Some(entries) = Arc::get_mut(entries) {
                into.extend(entries.drain(..).map(|(_, v)| v));
            }
        }
        _ => {}
    }
}

impl std::fmt::Debug for Jv {
    /// The kind only: a derived `Debug` would recurse as deep as the value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Jv::{}", self.kind())
    }
}

impl PartialEq for Jv {
    /// Structural equality (see the module docs), with a loop and an explicit stack.
    fn eq(&self, other: &Jv) -> bool {
        let mut stack: Vec<(&Jv, &Jv)> = vec![(self, other)];
        while let Some((a, b)) = stack.pop() {
            match (a, b) {
                (Jv::Null, Jv::Null) => {}
                (Jv::Bool(x), Jv::Bool(y)) if x == y => {}
                (Jv::Int(x), Jv::Int(y)) if x == y => {}
                (Jv::Big(x), Jv::Big(y)) if x == y => {}
                (Jv::Float(x), Jv::Float(y)) if x == y || (x.is_nan() && y.is_nan()) => {}
                (Jv::Str(x), Jv::Str(y)) if x == y => {}
                (Jv::List(x), Jv::List(y)) => {
                    if Arc::ptr_eq(x, y) {
                        continue;
                    }
                    if x.len() != y.len() {
                        return false;
                    }
                    stack.extend(x.iter().zip(y.iter()));
                }
                (Jv::Map(x), Jv::Map(y)) => {
                    if Arc::ptr_eq(x, y) {
                        continue;
                    }
                    if x.len() != y.len() {
                        return false;
                    }
                    // The keys are distinct, so equal lengths and every key of `x` found in
                    // `y` make the key sets equal.
                    let index: HashMap<&str, &Jv> =
                        y.iter().map(|(k, v)| (k.as_str(), v)).collect();
                    for (k, v) in x.iter() {
                        match index.get(k.as_str()) {
                            Some(w) => stack.push((v, w)),
                            None => return false,
                        }
                    }
                }
                _ => return false,
            }
        }
        true
    }
}

impl Jv {
    /// A number that is the same for values that are `==` (`hash()` of a script must agree with
    /// `==`): the sum of one number per node of the tree, each made from the node's path (the
    /// keys and indexes from the root) and what it holds. A sum does not depend on the order
    /// of the keys of an object, which `==` ignores. A `-0.0` hashes as `0.0` and every `nan`
    /// alike, as `==` treats them. Iterative, like `==`.
    pub(crate) fn hash64(&self) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        fn node(path: u64, tag: u8, f: impl FnOnce(&mut DefaultHasher)) -> u64 {
            let mut h = DefaultHasher::new();
            path.hash(&mut h);
            tag.hash(&mut h);
            f(&mut h);
            h.finish()
        }
        fn step(path: u64, key: impl FnOnce(&mut DefaultHasher)) -> u64 {
            let mut h = DefaultHasher::new();
            path.hash(&mut h);
            key(&mut h);
            h.finish()
        }
        let mut total = 0u64;
        let mut stack: Vec<(&Jv, u64)> = vec![(self, 0)];
        while let Some((jv, path)) = stack.pop() {
            let part = match jv {
                Jv::Null => node(path, 0, |_| {}),
                Jv::Bool(b) => node(path, 1, |h| b.hash(h)),
                Jv::Int(i) => node(path, 2, |h| i.hash(h)),
                Jv::Big(digits) => node(path, 3, |h| digits.hash(h)),
                Jv::Float(x) => node(path, 4, |h| {
                    let bits = if x.is_nan() {
                        f64::NAN.to_bits()
                    } else if *x == 0.0 {
                        0
                    } else {
                        x.to_bits()
                    };
                    bits.hash(h)
                }),
                Jv::Str(s) => node(path, 5, |h| s.hash(h)),
                Jv::List(items) => {
                    for (i, item) in items.iter().enumerate() {
                        stack.push((item, step(path, |h| i.hash(h))));
                    }
                    node(path, 6, |h| items.len().hash(h))
                }
                Jv::Map(entries) => {
                    for (key, value) in entries.iter() {
                        stack.push((value, step(path, |h| key.hash(h))));
                    }
                    node(path, 7, |h| entries.len().hash(h))
                }
            };
            total = total.wrapping_add(part);
        }
        total
    }

    /// The value of a parsed document.
    pub(crate) fn from_json(json: &Json) -> Result<Jv, JvError> {
        Ok(match json {
            Json::Null => Jv::Null,
            Json::Bool(b) => Jv::Bool(*b),
            Json::Num(text) => Jv::number(text)?,
            Json::Str(s) => Jv::Str(s.as_str().into()),
            Json::Arr(items) => {
                Jv::List(Arc::new(items.iter().map(Jv::from_json).collect::<Result<
                    Vec<_>,
                    _,
                >>(
                )?))
            }
            Json::Obj(entries) => Jv::Map(Arc::new(
                entries
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), Jv::from_json(v)?)))
                    .collect::<Result<Vec<_>, JvError>>()?,
            )),
        })
    }

    /// A number from its text: an optional `-` and digits is an integer, anything else must be
    /// a JSON number with a fraction or exponent that is a finite float. Text such as `inf` or
    /// `NaN` is never stored (`Big` is written back as it is).
    fn number(text: &str) -> Result<Jv, JvError> {
        let unsigned = text.strip_prefix('-').unwrap_or(text);
        if !unsigned.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(JvError {
                unsupported: true,
                message: format!("json: the number {text} cannot be written as JSON"),
            });
        }
        if unsigned.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(match text.parse::<i64>() {
                Ok(i) => Jv::Int(i),
                Err(_) => Jv::Big(text.into()),
            });
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Jv::Float(f)),
            _ => Err(JvError {
                unsupported: false,
                message: format!("json: the number {text} is out of range"),
            }),
        }
    }

    /// The document as the parser's tree, for the decoder. A non-finite float becomes the text
    /// `NaN` or `inf`, which no type reads as a number (and `from_json` refuses), so it is an
    /// error only where it is used. Deeper than `MAX_DEPTH` is an error.
    pub(crate) fn to_json(&self) -> Result<Json, String> {
        self.to_json_at(0)
    }

    fn to_json_at(&self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err("json: the value is nested too deeply".into());
        }
        Ok(match self {
            Jv::Null => Json::Null,
            Jv::Bool(b) => Json::Bool(*b),
            Jv::Int(i) => Json::Num(i.to_string()),
            Jv::Big(digits) => Json::Num(digits.to_string()),
            Jv::Float(f) => Json::Num(format!("{f:?}")),
            Jv::Str(s) => Json::Str(s.to_string()),
            Jv::List(items) => Json::Arr(
                items
                    .iter()
                    .map(|v| v.to_json_at(depth + 1))
                    .collect::<Result<_, _>>()?,
            ),
            Jv::Map(entries) => Json::Obj(
                entries
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), v.to_json_at(depth + 1)?)))
                    .collect::<Result<_, String>>()?,
            ),
        })
    }

    /// The name `kind()` gives it.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Jv::Null => "null",
            Jv::Bool(_) => "bool",
            Jv::Int(_) | Jv::Big(_) => "int",
            Jv::Float(_) => "float",
            Jv::Str(_) => "string",
            Jv::List(_) => "list",
            Jv::Map(_) => "map",
        }
    }

    /// The `Show` text: compact JSON, as `json.encode` writes it, except that a `nan` or an
    /// infinity (which no JSON can hold) shows as `nan`, `inf` or `-inf`. A value nested
    /// deeper than `MAX_DEPTH` shows as `...` from there down, and the text stops with `...`
    /// after `SHOW_LIMIT` bytes (a value that shares its children can be tiny in memory and
    /// huge as text), so showing never overflows the native stack or runs out of memory.
    pub(crate) fn show(&self) -> String {
        let mut out = String::new();
        self.show_into(&mut out, 0);
        out
    }

    fn show_into(&self, out: &mut String, depth: usize) {
        if out.len() > SHOW_LIMIT {
            return;
        }
        if depth > MAX_DEPTH {
            out.push_str("...");
            return;
        }
        match self {
            Jv::Null => out.push_str("null"),
            Jv::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Jv::Int(i) => out.push_str(&i.to_string()),
            Jv::Big(digits) => out.push_str(digits),
            Jv::Float(f) if f.is_nan() => out.push_str("nan"),
            Jv::Float(f) if f.is_infinite() => {
                out.push_str(if *f > 0.0 { "inf" } else { "-inf" });
            }
            Jv::Float(f) => out.push_str(&format!("{f:?}")),
            Jv::Str(s) => super::escape(out, s),
            Jv::List(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.show_into(out, depth + 1);
                    if out.len() > SHOW_LIMIT {
                        break;
                    }
                }
                out.push(']');
            }
            Jv::Map(entries) => {
                out.push('{');
                for (i, (key, item)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    super::escape(out, key);
                    out.push(':');
                    item.show_into(out, depth + 1);
                    if out.len() > SHOW_LIMIT {
                        break;
                    }
                }
                out.push('}');
            }
        }
        if depth == 0 && out.len() > SHOW_LIMIT {
            out.push_str("...");
        }
    }
}

/// A class whose objects wrap a [`Jv`] and answer its questions: `json.Value`, which every
/// format module (`toml`) shares through `HostModule::uses`.
pub(crate) trait Node: HostType + FromColibri + Clone {
    fn from_jv(jv: Jv) -> Self;
    fn jv(&self) -> &Jv;
}

impl Node for Jv {
    fn from_jv(jv: Jv) -> Self {
        jv
    }
    fn jv(&self) -> &Jv {
        self
    }
}

/// The entries of a `Map[str, Value]` in insertion order (the standard conversions of a Rust
/// map do not keep it).
pub(crate) struct Entries<T>(pub Vec<(String, T)>);

impl<T: Node> IntoColibri for Entries<T> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Map(
            Box::new(TypeDesc::Str),
            Box::new(<T as IntoColibri>::ty(reg)),
        )
    }
    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        let entries = self
            .0
            .into_iter()
            .map(|(k, v)| (Value::str(&k), v.into_colibri(cx)))
            .collect();
        Value::map(entries)
    }
}

impl<T: Node + FromColibri> FromColibri for Entries<T> {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        TypeDesc::Map(
            Box::new(TypeDesc::Str),
            Box::new(<T as FromColibri>::ty(reg)),
        )
    }
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error> {
        let ValueKind::Map(entries) = value.kind() else {
            return Err(Error::Convert("expected a Map[str, Value]".into()));
        };
        let mut out = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            let ValueKind::Str(key) = k.kind() else {
                return Err(Error::Convert("expected a Map with str keys".into()));
            };
            out.push((key, T::from_colibri(&v, cx)?));
        }
        Ok(Entries(out))
    }
}

/// Reads a `json.Value` object out of a script value (the object stays where it is).
impl FromColibri for Jv {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        reg.define_host_type::<Jv>()
    }
    fn from_colibri(value: &Value, cx: &mut Cx<'_>) -> Result<Self, Error> {
        Ok((*Ref::<Jv>::from_colibri(value, cx)?).clone())
    }
}

impl HostType for Jv {
    const NAME: &'static str = "Value";

    fn describe(d: &mut TypeBuilder<'_, Self>) {
        // Constructors: `json.Value.str("x")`.
        d.static_fn("null", [], || Jv::Null);
        d.static_fn("bool", ["value"], Jv::Bool);
        d.static_fn("int", ["value"], Jv::Int);
        // A `nan` or infinity is accepted here and refused when it is encoded, as with a
        // plain `float`.
        d.static_fn("float", ["value"], Jv::Float);
        d.static_fn("str", ["value"], |value: String| Jv::Str(value.into()));
        d.static_fn("list", ["items"], |items: Vec<Jv>| {
            Jv::List(Arc::new(items))
        });
        d.static_fn("map", ["entries"], |entries: Entries<Jv>| {
            Jv::Map(Arc::new(entries.0))
        });
        // `print(v)` and `"{v}"` give the compact JSON text, `a == b` is `equals`: a host
        // class can customise both (`TypeBuilder::show` and `eq`).
        d.show(|this: &Jv| this.show());
        d.eq(|a: &Jv, b: &Jv| a == b);
        d.hash(|this: &Jv| this.hash64());
        // The compact JSON text, as `json.encode(v)` writes it.
        d.method("to_json", [], |this: &Jv| {
            super::encode_jv(this).map_err(|m| super::Kind::Unsupported.error(m))
        });
        describe_accessors(d);
    }
}

/// The questions a node answers (see the module docs).
pub(crate) fn describe_accessors<T: Node>(d: &mut TypeBuilder<'_, T>) {
    // What it is: "null", "bool", "int", "float", "string", "list" or "map".
    d.method("kind", [], |this: &T| this.jv().kind().to_string());
    d.method("is_null", [], |this: &T| matches!(this.jv(), Jv::Null));
    // Structural equality, what `==` is too: see the module docs.
    d.method("equals", ["other"], |this: &T, other: T| {
        this.jv() == other.jv()
    });
    // The number of items of a list or entries of a map; 0 for anything else.
    d.method("len", [], |this: &T| match this.jv() {
        Jv::List(items) => items.len() as i64,
        Jv::Map(entries) => entries.len() as i64,
        _ => 0,
    });

    // Walking down: `none` when this is not a map (or list) or the step does not exist.
    d.method("get", ["key"], |this: &T, key: String| match this.jv() {
        Jv::Map(entries) => entries
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| T::from_jv(v.clone())),
        _ => None,
    });
    d.method("at", ["index"], |this: &T, index: i64| match this.jv() {
        Jv::List(items) => usize::try_from(index)
            .ok()
            .and_then(|i| items.get(i))
            .map(|v| T::from_jv(v.clone())),
        _ => None,
    });
    d.method("keys", [], |this: &T| match this.jv() {
        Jv::Map(entries) => entries.iter().map(|(k, _)| k.clone()).collect(),
        _ => Vec::new(),
    });

    // Reading a scalar or a container out as an option.
    d.method("as_str", [], |this: &T| match this.jv() {
        Jv::Str(s) => Some(s.to_string()),
        _ => None,
    });
    d.method("as_int", [], |this: &T| match this.jv() {
        Jv::Int(i) => Some(*i),
        _ => None,
    });
    // An int converts (a digit string beyond `i64` too); that may lose low digits.
    d.method("as_float", [], |this: &T| match this.jv() {
        Jv::Float(f) => Some(*f),
        Jv::Int(i) => Some(*i as f64),
        Jv::Big(text) => text.parse::<f64>().ok(),
        _ => None,
    });
    d.method("as_bool", [], |this: &T| match this.jv() {
        Jv::Bool(b) => Some(*b),
        _ => None,
    });
    d.method("as_list", [], |this: &T| match this.jv() {
        Jv::List(items) => Some(
            items
                .iter()
                .map(|v| T::from_jv(v.clone()))
                .collect::<Vec<T>>(),
        ),
        _ => None,
    });
    d.method("as_map", [], |this: &T| match this.jv() {
        Jv::Map(entries) => Some(Entries(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), T::from_jv(v.clone())))
                .collect(),
        )),
        _ => None,
    });
}
