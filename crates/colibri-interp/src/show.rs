//! The `Show` form of values (DESIGN §7.4): what `print` writes and `"{x}"` interpolates.
//!
//! Scalars show as themselves, and a `str` or `char` shown on its own is not quoted. Inside a
//! struct, enum or tuple, strings and chars are quoted and escaped. The forms are
//! `Player { name: "Ana", hp: 100 }`, `Move(3)`, `Circle(radius: 2.0)`, `Quit`, `(1, "a")`
//! and `()`.

use std::borrow::Cow;

use colibri_check::hir::{TypeDef, TypeDefKind};

use crate::value::Value;

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
        Value::Struct(_) | Value::Tuple(_) | Value::Enum(_) => {
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

/// Writes a value as it appears inside another value: strings and chars quoted.
fn write_nested(out: &mut String, types: &[TypeDef], v: &Value, depth: u32) {
    if depth > MAX_SHOW_DEPTH {
        out.push_str("...");
        return;
    }
    let list = |out: &mut String, items: &[Value]| {
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            write_nested(out, types, item, depth + 1);
        }
    };
    match v {
        Value::Str(s) => {
            out.push('"');
            s.chars().for_each(|c| escape(out, c, '"'));
            out.push('"');
        }
        Value::Char(c) => {
            out.push('\'');
            escape(out, *c, '\'');
            out.push('\'');
        }
        Value::Tuple(items) => {
            out.push('(');
            list(out, items);
            if items.len() == 1 {
                out.push(',');
            }
            out.push(')');
        }
        Value::Struct(s) => {
            let Some(def) = types.get(s.ty.0 as usize) else {
                out.push_str("<struct>");
                return;
            };
            out.push_str(&def.name);
            if s.fields.is_empty() {
                out.push_str(" {}");
                return;
            }
            out.push_str(" { ");
            for (i, (field, value)) in def.fields().iter().zip(s.fields.iter()).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&field.name);
                out.push_str(": ");
                write_nested(out, types, value, depth + 1);
            }
            out.push_str(" }");
        }
        Value::Enum(e) => {
            let variant = types.get(e.ty.0 as usize).and_then(|def| match &def.kind {
                TypeDefKind::Enum { variants } => variants.get(e.variant as usize),
                TypeDefKind::Struct { .. } => None,
            });
            let Some(variant) = variant else {
                out.push_str("<enum>");
                return;
            };
            out.push_str(&variant.name);
            if e.payload.is_empty() {
                return;
            }
            out.push('(');
            if variant.named {
                for (i, (field, value)) in variant.fields.iter().zip(e.payload.iter()).enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&field.name);
                    out.push_str(": ");
                    write_nested(out, types, value, depth + 1);
                }
            } else {
                list(out, &e.payload);
            }
            out.push(')');
        }
        scalar => out.push_str(&show(types, scalar)),
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
