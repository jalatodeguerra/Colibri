//! The `Show` form of values (DESIGN §7.4): what `print` writes and `"{x}"` interpolates.
//!
//! Scalars show as themselves, and a `str` or `char` shown on its own is not quoted. Inside a
//! struct, enum or tuple, strings and chars are quoted and escaped. The forms are
//! `Player { name: "Ana", hp: 100 }`, `Move(3)`, `Circle(radius: 2.0)`, `Quit`, `(1, "a")`
//! and `()`. A class object shows like a struct; an object that is already being shown (a
//! cycle) is `Node {...}`. Collections show as `[1, 2]`, `{"a": 1}` (`{:}` when empty) and
//! `Set([1, 2])`; `Option` is the prelude enum, so `some(3)` and `none`.

use std::borrow::Cow;

use std::rc::Rc;

use colibri_check::hir::{FieldDef, TypeDef, TypeDefKind, VariantDef};
use colibri_check::types::ERROR_TYPE;

use crate::hooks::{Overrides, Plain, Stop};
use crate::value::{ClassObj, Value};

/// How deep a value is shown before the rest is elided as `...`, so showing a huge nested
/// value cannot overflow the Rust stack.
const MAX_SHOW_DEPTH: u32 = 1_000;

/// The `Show` form of a value. `types` names the user-defined types a composite value may
/// contain; scalars need none.
pub fn show<'v>(types: &[TypeDef], v: &'v Value) -> Cow<'v, str> {
    match v {
        Value::Unit => Cow::Borrowed("()"),
        Value::Bool(true) => Cow::Borrowed("true"),
        Value::Bool(false) => Cow::Borrowed("false"),
        Value::Int(i) => Cow::Owned(i.to_string()),
        Value::U64(i) => Cow::Owned(i.to_string()),
        Value::Ptr(p) => Cow::Owned(format!("0x{p:x}")),
        Value::Float(x) => Cow::Owned(show_float(*x)),
        Value::F32(x) => Cow::Owned(show_f32(*x)),
        Value::Char(c) => Cow::Owned(c.to_string()),
        Value::Str(s) => Cow::Borrowed(s),
        Value::Bytes(b) => Cow::Owned(show_bytes(b)),
        Value::ByteBuf(_) => Cow::Borrowed("<buffer>"),
        Value::Struct(_)
        | Value::Tuple(_)
        | Value::Enum(_)
        | Value::SomeObj(_)
        | Value::List(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::Obj(_)
        | Value::Weak(_) => {
            let mut out = String::new();
            write_nested(&mut out, types, v, 0, &mut Plain);
            Cow::Owned(out)
        }
        Value::Dyn(d) => show(types, &d.val),
        Value::Fn(_, name) => Cow::Owned(format!("<fn {name}>")),
        Value::Closure(_) => Cow::Borrowed("<closure>"),
        Value::Host(r) => {
            let name = types
                .get(r.ty.0 as usize)
                .map_or("host object", |t| t.name.as_str());
            Cow::Owned(format!("<{name} #{}>", r.index))
        }
        Value::Cell(_) => Cow::Borrowed("<cell>"),
        Value::Iter(_) => Cow::Borrowed("<iterator>"),
        Value::Range(r) => Cow::Owned(format!(
            "{}..{}{}",
            r.start,
            if r.inclusive { "=" } else { "" },
            r.end
        )),
    }
}

/// The decimal digits of `i` in `buf`, without going through `core::fmt`.
#[inline]
pub fn int_text(buf: &mut [u8; 20], i: i64) -> &str {
    let mut n = i.unsigned_abs();
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    if i < 0 {
        // `i64::MIN` has 19 digits, so there is always a byte left for the sign.
        at -= 1;
        buf[at] = b'-';
    }
    // The bytes are ASCII digits and a sign.
    std::str::from_utf8(&buf[at..]).unwrap_or("")
}

/// Appends the `Show` form of an integer to `out`.
#[inline]
pub fn push_int(out: &mut String, i: i64) {
    out.push_str(int_text(&mut [0; 20], i));
}

/// The `Show` form of a value, where a user-written `Show` (found through `ov`) replaces the
/// structural form of the values it covers, also nested inside others.
pub(crate) fn show_with(
    types: &[TypeDef],
    v: &Value,
    ov: &mut dyn Overrides,
) -> Result<String, Stop> {
    if let Value::Dyn(d) = v {
        return show_with(types, &d.val, ov);
    }
    if !matches!(
        v,
        Value::Struct(_)
            | Value::Tuple(_)
            | Value::Enum(_)
            | Value::SomeObj(_)
            | Value::List(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::Obj(_)
            | Value::Host(_)
            | Value::Weak(_)
    ) {
        return Ok(show(types, v).into_owned());
    }
    let mut w = Writer {
        types,
        out: String::new(),
        visiting: Vec::new(),
        ov,
        stop: None,
    };
    w.value(v, 0);
    match w.stop {
        Some(stop) => Err(stop),
        None => Ok(w.out),
    }
}

/// The `Show` form of a float.
///
/// Floats print in the shortest form that reads back as the same number, and always look like
/// floats: `1.5`, `2.0`, `-0.0`, `1e-7`, `1e16` (Rust's `{:?}` rules: exponent form below
/// `1e-4` and from `1e16` up), plus `inf`, `-inf` and `nan`.
pub(crate) fn show_float(x: f64) -> String {
    if x.is_nan() {
        "nan".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "inf" } else { "-inf" }.to_string()
    } else {
        format!("{x:?}")
    }
}

/// An `f32` shows as the shortest text that reads back as the same `f32` (`0.1`, not
/// `0.10000000149011612`), in the same style as [`show_float`].
pub(crate) fn show_f32(x: f32) -> String {
    if x.is_nan() {
        "nan".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "inf" } else { "-inf" }.to_string()
    } else {
        format!("{x:?}")
    }
}

/// Builds the text of one value. `visiting` holds the class objects being written right now,
/// so a cycle shows an object that is already open as `Name {...}`.
struct Writer<'t> {
    types: &'t [TypeDef],
    out: String,
    visiting: Vec<usize>,
    ov: &'t mut dyn Overrides,
    /// A user-written `show` that failed; the rest of the text is not written.
    stop: Option<Stop>,
}

/// Writes a value as it appears inside another value: strings and chars quoted.
fn write_nested(
    out: &mut String,
    types: &[TypeDef],
    v: &Value,
    depth: u32,
    ov: &mut dyn Overrides,
) {
    let mut w = Writer {
        types,
        out: std::mem::take(out),
        visiting: Vec::new(),
        ov,
        stop: None,
    };
    w.value(v, depth);
    *out = w.out;
}

impl<'t> Writer<'t> {
    fn list(&mut self, items: &[Value], depth: u32) {
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.value(item, depth + 1);
        }
    }

    fn value(&mut self, v: &Value, depth: u32) {
        // A `dyn` shows as the value it holds, at the same depth: peeled here, in a loop, so
        // that it costs no frame of its own.
        let mut v = v;
        while let Value::Dyn(d) = v {
            v = &d.val;
        }
        if let Value::SomeObj(_) = v {
            return self.value(&v.unsugar(), depth);
        }
        if depth > MAX_SHOW_DEPTH || self.stop.is_some() {
            self.out.push_str("...");
            return;
        }
        if matches!(
            v,
            Value::Struct(_) | Value::Enum(_) | Value::Obj(_) | Value::Host(_)
        ) && self.overridden(v)
        {
            return;
        }
        match v {
            Value::Str(_) | Value::Bytes(_) | Value::Char(_) => self.leaf(v),
            Value::Tuple(items) => self.tuple(items, depth),
            Value::Struct(s) => self.structure(s, depth),
            Value::Enum(e) => self.variant(e, depth),
            Value::List(l) => self.list_value(l, depth),
            Value::Map(m) => self.map(m, depth),
            Value::Set(s) => self.set_value(s, depth),
            Value::Obj(obj) if obj.ty() == ERROR_TYPE => self.error(obj, depth),
            Value::Obj(obj) => {
                if !self.leaf_object(obj) {
                    self.object(obj, depth);
                }
            }
            Value::Weak(w) => self.weak(w, depth),
            _ => self.leaf(v),
        }
    }

    fn tuple(&mut self, items: &[Value], depth: u32) {
        self.out.push('(');
        self.list(items, depth);
        if items.len() == 1 {
            self.out.push(',');
        }
        self.out.push(')');
    }

    fn structure(&mut self, s: &crate::value::StructVal, depth: u32) {
        match self.types.get(s.ty.0 as usize) {
            Some(def) => self.record(&def.name, def.fields(), &s.fields, depth),
            None => self.out.push_str("<struct>"),
        }
    }

    fn list_value(&mut self, l: &crate::value::ListObj, depth: u32) {
        let items = l.snapshot();
        self.out.push('[');
        self.list(&items, depth);
        self.out.push(']');
    }

    fn set_value(&mut self, s: &crate::value::SetObj, depth: u32) {
        let items = s.snapshot();
        self.out.push_str("Set([");
        self.list(&items, depth);
        self.out.push_str("])");
    }

    fn variant(&mut self, e: &crate::value::EnumVal, depth: u32) {
        let Some(variant) = self.variant_def(e) else {
            self.out.push_str("<enum>");
            return;
        };
        self.out.push_str(&variant.name);
        if e.payload.is_empty() {
            return;
        }
        self.out.push('(');
        if variant.named {
            self.named_payload(&variant.fields, &e.payload, depth);
        } else {
            self.list(&e.payload, depth);
        }
        self.out.push(')');
    }

    fn variant_def(&self, e: &crate::value::EnumVal) -> Option<&'t VariantDef> {
        let def = self.types.get(e.ty.0 as usize)?;
        match &def.kind {
            TypeDefKind::Enum { variants } => variants.get(e.variant as usize),
            _ => None,
        }
    }

    fn named_payload(&mut self, fields: &[FieldDef], payload: &[Value], depth: u32) {
        for i in 0..fields.len().min(payload.len()) {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.out.push_str(&fields[i].name);
            self.out.push_str(": ");
            self.value(&payload[i], depth + 1);
        }
    }

    fn map(&mut self, m: &crate::value::MapObj, depth: u32) {
        if m.is_empty() {
            self.out.push_str("{:}");
            return;
        }
        self.out.push('{');
        let entries = m.snapshot();
        for (i, (key, value)) in entries.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.value(key, depth + 1);
            self.out.push_str(": ");
            self.value(value, depth + 1);
        }
        self.out.push('}');
    }

    fn weak(&mut self, w: &std::rc::Weak<ClassObj>, depth: u32) {
        match w.upgrade() {
            Some(obj) => {
                self.out.push_str("some(");
                self.object(&obj, depth + 1);
                self.out.push(')');
            }
            None => self.out.push_str("none"),
        }
    }

    /// Asks the host's `show` override for a value. Kept out of line, with the other arms that
    /// need temporaries, so that the frame of the recursive `value` stays small: a value can
    /// nest 1,000 deep, and a test thread has only 2 MB of stack.
    #[inline(never)]
    fn overridden(&mut self, v: &Value) -> bool {
        let Some(answer) = self.ov.show(v) else {
            return false;
        };
        match answer {
            Ok(text) => self.out.push_str(&text),
            Err(stop) => self.stop = Some(stop),
        }
        true
    }

    /// The values that hold no other value to show: text, bytes, characters, scalars. Out of
    /// line for the reason given at `overridden`.
    #[inline(never)]
    fn leaf(&mut self, v: &Value) {
        match v {
            Value::Str(s) => {
                self.out.push('"');
                s.chars().for_each(|c| escape(&mut self.out, c, '"'));
                self.out.push('"');
            }
            Value::Bytes(b) => self.out.push_str(&show_bytes(b)),
            Value::Char(c) => {
                self.out.push('\'');
                escape(&mut self.out, *c, '\'');
                self.out.push('\'');
            }
            scalar => self.out.push_str(&show(self.types, scalar)),
        }
    }

    /// The runtime objects that show as text rather than as fields. Says whether `obj` was one.
    /// Out of line for the reason given at `overridden`.
    #[inline(never)]
    fn leaf_object(&mut self, obj: &Rc<ClassObj>) -> bool {
        let ty = obj.ty();
        if ty == colibri_check::types::COROUTINE_TYPE {
            let name = match obj.field(3) {
                Some(Value::Str(s)) => s.to_string(),
                _ => "?".to_string(),
            };
            self.out.push_str(&format!("<coroutine {name}>"));
        } else if ty == colibri_check::types::COGROUP_TYPE {
            self.out.push_str("<CoGroup>");
        } else if ty == colibri_check::types::STR_BUILDER_TYPE {
            self.out.push_str(&crate::builders::builder_text(obj));
        } else if ty == colibri_check::types::BYTES_BUILDER_TYPE {
            let bytes = crate::builders::builder_bytes(obj);
            self.out.push_str(&show_bytes(&bytes));
        } else if ty == colibri_check::types::SIGNAL_TYPE {
            self.out.push_str("<Signal>");
        } else if ty == colibri_check::types::TASK_TYPE {
            self.out.push_str("<Task>");
        } else if ty == colibri_check::types::CHANNEL_TYPE {
            self.out.push_str("<Channel>");
        } else if ty == colibri_check::types::SHARED_TYPE {
            self.out.push_str("<Shared>");
        } else {
            return false;
        }
        true
    }

    /// `Name { field: value, .. }` for a struct or a class.
    fn record(&mut self, name: &str, fields: &[FieldDef], values: &[Value], depth: u32) {
        self.out.push_str(name);
        if values.is_empty() {
            self.out.push_str(" {}");
            return;
        }
        self.out.push_str(" { ");
        for i in 0..fields.len().min(values.len()) {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.out.push_str(&fields[i].name);
            self.out.push_str(": ");
            self.value(&values[i], depth + 1);
        }
        self.out.push_str(" }");
    }

    /// An `Error` shows as its whole chain: the context messages, outermost first, then the
    /// message of the value it keeps, separated by `: ` (DESIGN §10.2).
    fn error(&mut self, obj: &Rc<ClassObj>, depth: u32) {
        let mut cur = Rc::clone(obj);
        loop {
            match crate::error::link(&cur) {
                crate::error::Link::Context(message, cause) => {
                    self.out.push_str(&message);
                    self.out.push_str(": ");
                    cur = cause;
                }
                crate::error::Link::Root(_, _, Some(message)) => {
                    self.out.push_str(&message);
                    return;
                }
                crate::error::Link::Root(value, impl_id, None) => {
                    match self.ov.error_message(&value, impl_id) {
                        Some(Ok(text)) => self.out.push_str(&text),
                        Some(Err(stop)) => self.stop = Some(stop),
                        // A string is its own message; anything else shows as it would nested.
                        None => match &value {
                            Value::Str(s) => self.out.push_str(s),
                            other => self.value(other, depth + 1),
                        },
                    }
                    return;
                }
            }
        }
    }

    /// A class object: its fields like a struct's, unless it is already being written, which
    /// is a cycle and shows as `Name {...}`.
    fn object(&mut self, obj: &Rc<ClassObj>, depth: u32) {
        let Some(def) = self.types.get(obj.ty().0 as usize) else {
            self.out.push_str("<object>");
            return;
        };
        let address = Rc::as_ptr(obj) as usize;
        if self.visiting.contains(&address) {
            self.out.push_str(&def.name);
            self.out.push_str(" {...}");
            return;
        }
        self.visiting.push(address);
        self.record(&def.name, def.fields(), &obj.snapshot(), depth);
        self.visiting.pop();
    }
}

/// Writes `c` escaped for a literal delimited by `quote`.
fn escape(out: &mut String, c: char, quote: char) {
    match c {
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\0' => out.push_str("\\0"),
        c if c == quote => {
            out.push('\\');
            out.push(c);
        }
        c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
        c => out.push(c),
    }
}

/// How a `Bytes` shows (`print`, `"{b}"`): `b"hello\x00\xff"`, with printable ASCII as it is,
/// `\\`, `\"`, `\n`, `\r` and `\t` escaped, and every other byte as `\xNN`.
fn show_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 3);
    out.push_str("b\"");
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(char::from(b)),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use colibri_check::hir::{FieldDef, VariantDef};
    use colibri_check::types::{Ty, TypeId};
    use colibri_syntax::{FileId, Span};

    use super::*;

    const SP: Span = Span {
        file: FileId(0),
        start: 0,
        end: 0,
    };

    fn field(name: &str) -> FieldDef {
        FieldDef {
            name: name.into(),
            ty: Ty::INT,
            span: SP,
            weak: false,
            default: None,
            rename: None,
            skip: false,
        }
    }

    /// `struct Player { name, hp }` and `enum Shape { Circle(radius), Move(0), Quit }`.
    fn types() -> Vec<TypeDef> {
        let variant = |name: &str, fields: Vec<FieldDef>, named| VariantDef {
            name: name.into(),
            span: SP,
            fields,
            named,
        };
        vec![
            TypeDef {
                name: "Player".into(),
                span: SP,
                params: Vec::new(),
                module: None,
                open: false,
                kind: TypeDefKind::Struct {
                    fields: vec![field("name"), field("hp")],
                },
            },
            TypeDef {
                name: "Shape".into(),
                span: SP,
                params: Vec::new(),
                module: None,
                open: false,
                kind: TypeDefKind::Enum {
                    variants: vec![
                        variant("Circle", vec![field("radius")], true),
                        variant("Move", vec![field("0")], false),
                        variant("Quit", vec![], false),
                    ],
                },
            },
            TypeDef {
                name: "Node".into(),
                span: SP,
                params: Vec::new(),
                module: None,
                open: false,
                kind: TypeDefKind::Struct {
                    fields: vec![field("next")],
                },
            },
        ]
    }

    #[test]
    fn floats_always_look_like_floats() {
        assert_eq!(show_float(1.5), "1.5");
        assert_eq!(show_float(2.0), "2.0");
        assert_eq!(show_float(-0.0), "-0.0");
        assert_eq!(show_float(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(show_float(1e-9), "1e-9");
        assert_eq!(show_float(1e16), "1e16");
        assert_eq!(show_float(123456789.0), "123456789.0");
        assert_eq!(show_float(f64::INFINITY), "inf");
        assert_eq!(show_float(f64::NEG_INFINITY), "-inf");
        assert_eq!(show_float(f64::NAN), "nan");
    }

    #[test]
    fn show_of_each_scalar() {
        assert_eq!(show(&[], &Value::Unit), "()");
        assert_eq!(show(&[], &Value::Bool(true)), "true");
        assert_eq!(show(&[], &Value::Int(-7)), "-7");
        assert_eq!(show(&[], &Value::Char('x')), "x");
        assert_eq!(show(&[], &Value::str("a b")), "a b");
    }

    #[test]
    fn composites_quote_their_strings_and_chars() {
        let ts = types();
        let p = Value::strukt(TypeId(0), vec![Value::str("Ana"), Value::Int(100)]);
        assert_eq!(show(&ts, &p), r#"Player { name: "Ana", hp: 100 }"#);
        let t = Value::tuple(vec![Value::Int(1), Value::str("a"), Value::Char('c')]);
        assert_eq!(show(&ts, &t), r#"(1, "a", 'c')"#);
        assert_eq!(show(&ts, &Value::tuple(vec![Value::Int(1)])), "(1,)");
    }

    #[test]
    fn enums_show_their_payload() {
        let ts = types();
        let circle = Value::variant(TypeId(1), 0, vec![Value::Float(2.0)]);
        assert_eq!(show(&ts, &circle), "Circle(radius: 2.0)");
        let mv = Value::variant(TypeId(1), 1, vec![Value::Int(3)]);
        assert_eq!(show(&ts, &mv), "Move(3)");
        assert_eq!(show(&ts, &Value::variant(TypeId(1), 2, vec![])), "Quit");
    }

    #[test]
    fn escapes_inside_composites() {
        let t = Value::tuple(vec![Value::str("a\"b\\c\nd"), Value::Char('\'')]);
        assert_eq!(show(&[], &t), r#"("a\"b\\c\nd", '\'')"#);
    }

    #[test]
    fn very_deep_values_are_elided_not_recursed() {
        let mut v = Value::Unit;
        for _ in 0..100_000 {
            v = Value::variant(TypeId(1), 1, vec![v]);
        }
        let text = show(&types(), &v);
        assert!(text.contains("..."));
    }

    /// The same deep show on a thread with half of a test thread's stack. A debug build has
    /// no frame reuse, so every temporary in an arm of `Writer::value` and of the functions
    /// it calls adds to the stack used by each of the 1,000 levels. A new arm that grows
    /// those frames overflowed the 2 MB test stack only on a build with debug info, which
    /// the local setup does not use. Running on 1 MB makes that show up here, with a margin
    /// of about two against the figure measured when this test was written (544 bytes a level
    /// with debug info, so about 540 KB).
    #[test]
    fn very_deep_values_are_elided_within_one_megabyte_of_stack() {
        let worker = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(|| {
                let mut v = Value::Unit;
                for _ in 0..100_000 {
                    v = Value::variant(TypeId(1), 1, vec![v]);
                }
                let text = show(&types(), &v);
                assert!(text.contains("..."));
            })
            .expect("a thread starts");
        worker.join().expect("the show stays within the stack");
    }

    /// Shows a chain of 100,000 values made by `make` on a thread with a 1 MB stack. The chain
    /// is leaked: dropping it is not what is measured here, and a chain of 100,000 structs
    /// would drop recursively.
    fn deep_on_one_megabyte(make: fn(Value) -> Value) {
        let worker = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || {
                let mut v = Value::Unit;
                for _ in 0..100_000 {
                    v = make(v);
                }
                let text = show(&types(), &v);
                assert!(text.contains("..."));
                std::mem::forget(v);
            })
            .expect("a thread starts");
        worker.join().expect("the show stays within the stack");
    }

    /// The other recursive paths of `Writer`, which the guard above does not run through, so
    /// that a frame that grows in `record`, `object` or `named_payload` shows up here too.
    /// Bytes per level with debug info, as measured when this was written: a struct 560, a
    /// class object 688, a `dyn` holding a struct 560, a named variant 544. The worst is 688 KB
    /// at depth 1,000, which leaves the 1 MB of these tests a margin of 1.5 on the target that
    /// was measured; other targets have room above that, up to the 2 MB of a test thread.
    #[test]
    fn very_deep_structs_are_elided_within_one_megabyte_of_stack() {
        deep_on_one_megabyte(|v| Value::strukt(TypeId(2), vec![v]));
    }

    #[test]
    fn very_deep_class_objects_are_elided_within_one_megabyte_of_stack() {
        deep_on_one_megabyte(|v| Value::Obj(ClassObj::new(TypeId(2), vec![v])));
    }

    #[test]
    fn very_deep_dyn_structs_are_elided_within_one_megabyte_of_stack() {
        deep_on_one_megabyte(|v| {
            Value::Dyn(Rc::new(crate::value::DynVal {
                impl_id: 0,
                val: Value::strukt(TypeId(2), vec![v]),
            }))
        });
    }

    #[test]
    fn very_deep_named_variants_are_elided_within_one_megabyte_of_stack() {
        deep_on_one_megabyte(|v| Value::variant(TypeId(1), 0, vec![v]));
    }

    #[test]
    fn int_text_matches_to_string_at_the_edges() {
        let mut cases = vec![0, 1, -1, 9, 10, -10, 99, 100, 12345, i64::MAX, i64::MIN];
        cases.extend((0..19).flat_map(|p| [10i64.pow(p), -(10i64.pow(p)), 10i64.pow(p) - 1]));
        for i in cases {
            assert_eq!(int_text(&mut [0; 20], i), i.to_string(), "{i}");
            let mut out = String::from("<");
            push_int(&mut out, i);
            assert_eq!(out, format!("<{i}"));
        }
    }
}
