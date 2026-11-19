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

use colibri_check::hir::{FieldDef, TypeDef, TypeDefKind};

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
        Value::Float(x) => Cow::Owned(show_float(*x)),
        Value::Char(c) => Cow::Owned(c.to_string()),
        Value::Str(s) => Cow::Borrowed(s),
        Value::Struct(_)
        | Value::Tuple(_)
        | Value::Enum(_)
        | Value::List(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::Obj(_)
        | Value::Weak(_) => {
            let mut out = String::new();
            write_nested(&mut out, types, v, 0);
            Cow::Owned(out)
        }
    }
}

/// The `Show` form of a float.
///
/// Floats print in the shortest form that reads back as the same number, and always look like
/// floats: `1.5`, `2.0`, `-0.0`, `1e-7`, `1e16` (Rust's `{:?}` rules: exponent form below
/// `1e-4` and from `1e16` up), plus `inf`, `-inf` and `nan`.
pub fn show_float(x: f64) -> String {
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
}

/// Writes a value as it appears inside another value: strings and chars quoted.
fn write_nested(out: &mut String, types: &[TypeDef], v: &Value, depth: u32) {
    let mut w = Writer {
        types,
        out: std::mem::take(out),
        visiting: Vec::new(),
    };
    w.value(v, depth);
    *out = w.out;
}

impl Writer<'_> {
    fn list(&mut self, items: &[Value], depth: u32) {
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.value(item, depth + 1);
        }
    }

    fn value(&mut self, v: &Value, depth: u32) {
        if depth > MAX_SHOW_DEPTH {
            self.out.push_str("...");
            return;
        }
        let types = self.types;
        match v {
            Value::Str(s) => {
                self.out.push('"');
                s.chars().for_each(|c| escape(&mut self.out, c, '"'));
                self.out.push('"');
            }
            Value::Char(c) => {
                self.out.push('\'');
                escape(&mut self.out, *c, '\'');
                self.out.push('\'');
            }
            Value::Tuple(items) => {
                self.out.push('(');
                self.list(items, depth);
                if items.len() == 1 {
                    self.out.push(',');
                }
                self.out.push(')');
            }
            Value::Struct(s) => {
                let Some(def) = types.get(s.ty.0 as usize) else {
                    self.out.push_str("<struct>");
                    return;
                };
                self.record(&def.name, def.fields(), &s.fields, depth);
            }
            Value::Enum(e) => {
                let variant = types.get(e.ty.0 as usize).and_then(|def| match &def.kind {
                    TypeDefKind::Enum { variants } => variants.get(e.variant as usize),
                    _ => None,
                });
                let Some(variant) = variant else {
                    self.out.push_str("<enum>");
                    return;
                };
                self.out.push_str(&variant.name);
                if e.payload.is_empty() {
                    return;
                }
                self.out.push('(');
                if variant.named {
                    for (i, (field, value)) in
                        variant.fields.iter().zip(e.payload.iter()).enumerate()
                    {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.out.push_str(&field.name);
                        self.out.push_str(": ");
                        self.value(value, depth + 1);
                    }
                } else {
                    self.list(&e.payload, depth);
                }
                self.out.push(')');
            }
            Value::List(l) => {
                self.out.push('[');
                self.list(&l.snapshot(), depth);
                self.out.push(']');
            }
            Value::Map(m) => {
                if m.is_empty() {
                    self.out.push_str("{:}");
                    return;
                }
                self.out.push('{');
                for (i, (k, v)) in m.snapshot().iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.value(k, depth + 1);
                    self.out.push_str(": ");
                    self.value(v, depth + 1);
                }
                self.out.push('}');
            }
            Value::Set(s) => {
                self.out.push_str("Set([");
                self.list(&s.snapshot(), depth);
                self.out.push_str("])");
            }
            Value::Obj(obj) => self.object(obj, depth),
            Value::Weak(w) => match w.upgrade() {
                Some(obj) => {
                    self.out.push_str("some(");
                    self.object(&obj, depth + 1);
                    self.out.push(')');
                }
                None => self.out.push_str("none"),
            },
            scalar => self.out.push_str(&show(types, scalar)),
        }
    }

    /// `Name { field: value, .. }` for a struct or a class.
    fn record(&mut self, name: &str, fields: &[FieldDef], values: &[Value], depth: u32) {
        self.out.push_str(name);
        if values.is_empty() {
            self.out.push_str(" {}");
            return;
        }
        self.out.push_str(" { ");
        for (i, (field, value)) in fields.iter().zip(values.iter()).enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.out.push_str(&field.name);
            self.out.push_str(": ");
            self.value(value, depth + 1);
        }
        self.out.push_str(" }");
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
                kind: TypeDefKind::Struct {
                    fields: vec![field("name"), field("hp")],
                },
            },
            TypeDef {
                name: "Shape".into(),
                span: SP,
                kind: TypeDefKind::Enum {
                    variants: vec![
                        variant("Circle", vec![field("radius")], true),
                        variant("Move", vec![field("0")], false),
                        variant("Quit", vec![], false),
                    ],
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
}
