//! `json`: `encode`, `pretty` and `decode[T]` (DESIGN §7.5).
//!
//! The parser is written by hand (about 150 lines) because the only thing `serde_json` would add
//! is a second tree to convert from, and the type descriptors are not `serde` types.
//!
//! What maps to what:
//!
//! | Colibri | JSON |
//! |---------|------|
//! | `int` and the sized integers, `u64` | a number with no fraction or exponent, range checked |
//! | `float`, `f32` | a number (`1.0` is written with its `.0`; `nan` and infinities are errors) |
//! | `bool`, `str` | `true`/`false`, a string; `char` is a string of one character |
//! | `List[T]`, tuples, `Set[T]` (encode only) | an array |
//! | `Map[str, V]` | an object, in insertion order |
//! | `T?` | `null` or the value; a missing field of a struct is `none` |
//! | `struct` | an object with the field names, in declaration order |
//! | `enum` without data | the variant name as a string |
//!
//! Not supported (a clear error, never a wrong result): enums with data, classes, generic
//! structs, functions, `Map` with keys that are not `str`. `@rename` and `@skip` wait for
//! attributes on user types. A struct field that has a default may be absent from the object
//! and takes its default; one without a default is an error (unless it is an `Option`).

use std::rc::Rc;

use colibri_runtime::reflect::{IntTy, OPTION_TYPE, Ty, TypeDef, TypeDefKind, TypeId};
use colibri_runtime::{Ctx, Error, Value, ValueKind, Vm};

use crate::plain;

/// How deep arrays and objects may nest, in the text and in a value (a list that holds itself
/// would otherwise never end).
const MAX_DEPTH: usize = 128;

// ---- the text ----

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    /// The number as written; whether it is an integer is decided by what it is read into.
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

type ParseResult<T> = Result<T, String>;

impl<'a> Parser<'a> {
    fn fail<T>(&self, message: &str) -> ParseResult<T> {
        let before = &self.text[..self.pos.min(self.text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        Err(format!(
            "json: invalid syntax at line {line}, column {column}: {message}"
        ))
    }

    fn peek(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, c: char, what: &str) -> ParseResult<()> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            self.fail(&format!("expected {what}"))
        }
    }

    fn document(mut self) -> ParseResult<Json> {
        self.skip_space();
        if self.pos >= self.text.len() {
            return self.fail("the text is empty");
        }
        let value = self.value(0)?;
        self.skip_space();
        if self.pos < self.text.len() {
            return self.fail("unexpected text after the value");
        }
        Ok(value)
    }

    fn value(&mut self, depth: usize) -> ParseResult<Json> {
        if depth > MAX_DEPTH {
            return self.fail("too deeply nested");
        }
        self.skip_space();
        match self.peek() {
            None => self.fail("the text ends where a value was expected"),
            Some('{') => self.object(depth),
            Some('[') => self.array(depth),
            Some('"') => Ok(Json::Str(self.string()?)),
            Some('t') => self.word("true", Json::Bool(true)),
            Some('f') => self.word("false", Json::Bool(false)),
            Some('n') => self.word("null", Json::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            Some(c) => self.fail(&format!("`{c}` cannot start a value")),
        }
    }

    fn word(&mut self, word: &str, value: Json) -> ParseResult<Json> {
        if self.text[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(value)
        } else {
            self.fail(&format!("expected `{word}`"))
        }
    }

    fn number(&mut self) -> ParseResult<Json> {
        let start = self.pos;
        let digits = |p: &mut Parser| {
            let from = p.pos;
            while p.peek().is_some_and(|c| c.is_ascii_digit()) {
                p.pos += 1;
            }
            p.pos - from
        };
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        match self.peek() {
            Some('0') => {
                self.pos += 1;
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    return self.fail("a number cannot start with a zero");
                }
            }
            Some(c) if c.is_ascii_digit() => {
                digits(self);
            }
            _ => return self.fail("a number needs a digit"),
        }
        if self.peek() == Some('.') {
            self.pos += 1;
            if digits(self) == 0 {
                return self.fail("a digit must follow the `.`");
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.pos += 1;
            }
            if digits(self) == 0 {
                return self.fail("a digit must follow the exponent");
            }
        }
        Ok(Json::Num(self.text[start..self.pos].to_string()))
    }

    fn hex4(&mut self) -> ParseResult<u32> {
        let digits = self.text.get(self.pos..self.pos + 4);
        match digits.and_then(|d| u32::from_str_radix(d, 16).ok()) {
            Some(v) if digits.is_some_and(|d| d.chars().all(|c| c.is_ascii_hexdigit())) => {
                self.pos += 4;
                Ok(v)
            }
            _ => self.fail("`\\u` needs four hexadecimal digits"),
        }
    }

    fn string(&mut self) -> ParseResult<String> {
        self.expect('"', "`\"`")?;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return self.fail("the string is not closed");
            };
            self.pos += c.len_utf8();
            match c {
                '"' => return Ok(out),
                '\\' => {
                    let Some(e) = self.peek() else {
                        return self.fail("the string is not closed");
                    };
                    self.pos += e.len_utf8();
                    match e {
                        '"' | '\\' | '/' => out.push(e),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let first = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&first) {
                                if !self.text[self.pos..].starts_with("\\u") {
                                    return self.fail("a high surrogate needs a low one after it");
                                }
                                self.pos += 2;
                                let second = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&second) {
                                    return self.fail("a high surrogate needs a low one after it");
                                }
                                0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                            } else {
                                first
                            };
                            match char::from_u32(code) {
                                Some(ch) => out.push(ch),
                                None => return self.fail("a lone surrogate is not a character"),
                            }
                        }
                        other => return self.fail(&format!("`\\{other}` is not an escape")),
                    }
                }
                c if (c as u32) < 0x20 => {
                    self.pos -= 1;
                    return self.fail("a control character must be escaped in a string");
                }
                c => out.push(c),
            }
        }
    }

    fn array(&mut self, depth: usize) -> ParseResult<Json> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_space();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_space();
            match self.peek() {
                Some(',') => self.pos += 1,
                Some(']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return self.fail("expected `,` or `]`"),
            }
        }
    }

    fn object(&mut self, depth: usize) -> ParseResult<Json> {
        self.pos += 1;
        let mut entries: Vec<(String, Json)> = Vec::new();
        self.skip_space();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Json::Obj(entries));
        }
        loop {
            self.skip_space();
            if self.peek() != Some('"') {
                return self.fail("expected a string for the name");
            }
            let key = self.string()?;
            if entries.iter().any(|(k, _)| *k == key) {
                return self.fail(&format!("the name `{key}` appears twice in one object"));
            }
            self.skip_space();
            self.expect(':', "`:` after the name")?;
            let value = self.value(depth + 1)?;
            entries.push((key, value));
            self.skip_space();
            match self.peek() {
                Some(',') => self.pos += 1,
                Some('}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(entries));
                }
                _ => return self.fail("expected `,` or `}`"),
            }
        }
    }
}

// ---- encoding ----

struct Encoder<'a> {
    defs: &'a [TypeDef],
    /// Spaces per level, or `None` for the compact form.
    indent: Option<usize>,
    out: String,
}

fn escape(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The name of a struct or enum, for messages.
fn type_name(defs: &[TypeDef], id: TypeId) -> String {
    defs.get(id.0 as usize)
        .map_or_else(|| "?".to_string(), |d| d.name.clone())
}

impl Encoder<'_> {
    fn newline(&mut self, level: usize) {
        if let Some(step) = self.indent {
            self.out.push('\n');
            self.out.push_str(&" ".repeat(step * level));
        }
    }

    /// Writes `items` between `open` and `close`, one per line when pretty.
    fn sequence<T>(
        &mut self,
        (open, close): (char, char),
        items: Vec<T>,
        level: usize,
        mut each: impl FnMut(&mut Self, T, usize) -> Result<(), String>,
    ) -> Result<(), String> {
        self.out.push(open);
        if items.is_empty() {
            self.out.push(close);
            return Ok(());
        }
        for (i, item) in items.into_iter().enumerate() {
            if i > 0 {
                self.out.push(',');
            }
            self.newline(level + 1);
            each(self, item, level + 1)?;
        }
        self.newline(level);
        self.out.push(close);
        Ok(())
    }

    fn key(&mut self, name: &str) {
        escape(&mut self.out, name);
        self.out.push(':');
        if self.indent.is_some() {
            self.out.push(' ');
        }
    }

    fn value(&mut self, v: &Value, level: usize) -> Result<(), String> {
        if level > MAX_DEPTH {
            return Err(
                "json: the value is nested too deeply (does a list or map hold itself?)".into(),
            );
        }
        match v.kind() {
            ValueKind::Unit => self.out.push_str("null"),
            ValueKind::Bool(b) => self.out.push_str(if b { "true" } else { "false" }),
            ValueKind::Int(i) => self.out.push_str(&i.to_string()),
            ValueKind::U64(i) => self.out.push_str(&i.to_string()),
            ValueKind::Float(f) => self.float(f, format!("{f:?}"))?,
            ValueKind::F32(f) => self.float(f64::from(f), format!("{f:?}"))?,
            ValueKind::Char(c) => escape(&mut self.out, &c.to_string()),
            ValueKind::Str(s) => escape(&mut self.out, &s),
            ValueKind::List(items) | ValueKind::Set(items) | ValueKind::Tuple(items) => {
                self.sequence(('[', ']'), items, level, |e, item, l| e.value(&item, l))?;
            }
            ValueKind::Map(entries) => {
                let mut named = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    match k.kind() {
                        ValueKind::Str(name) => named.push((name, v)),
                        _ => {
                            return Err(
                                "json: a Map becomes an object, so its keys must be str".into()
                            );
                        }
                    }
                }
                self.sequence(('{', '}'), named, level, |e, (name, v), l| {
                    e.key(&name);
                    e.value(&v, l)
                })?;
            }
            ValueKind::Struct { ty, fields } => {
                let def = self.defs.get(ty.0 as usize);
                let names: Vec<String> = def
                    .map(|d| d.fields().iter().map(|f| f.name.clone()).collect())
                    .unwrap_or_default();
                if names.len() != fields.len() {
                    return Err(format!(
                        "json: cannot encode a value of the type `{}`",
                        type_name(self.defs, ty)
                    ));
                }
                let pairs: Vec<(String, Value)> = names.into_iter().zip(fields).collect();
                self.sequence(('{', '}'), pairs, level, |e, (name, v), l| {
                    e.key(&name);
                    e.value(&v, l)
                })?;
            }
            ValueKind::Enum {
                ty,
                variant,
                payload,
            } => {
                if ty == OPTION_TYPE {
                    // `none` is null, `some(x)` is x.
                    match payload.first() {
                        Some(inner) => self.value(inner, level)?,
                        None => self.out.push_str("null"),
                    }
                } else {
                    let def = self.defs.get(ty.0 as usize);
                    let unit_only =
                        def.is_some_and(|d| d.variants().iter().all(|v| v.fields.is_empty()));
                    let name = def.and_then(|d| d.variants().get(variant as usize));
                    match (unit_only, name) {
                        (true, Some(v)) => escape(&mut self.out, &v.name),
                        _ => {
                            return Err(format!(
                                "json: cannot encode the enum `{}`: only enums whose variants carry no data are supported",
                                type_name(self.defs, ty)
                            ));
                        }
                    }
                }
            }
            ValueKind::Other => {
                return Err(
                    "json: cannot encode a class object, function or host object".to_string(),
                );
            }
            _ => return Err("json: cannot encode this value".to_string()),
        }
        Ok(())
    }

    fn float(&mut self, value: f64, text: String) -> Result<(), String> {
        if !value.is_finite() {
            return Err(format!(
                "json: cannot encode {value}: JSON has no such number"
            ));
        }
        self.out.push_str(&text);
        Ok(())
    }
}

fn encode(cx: &Ctx, value: &Value, indent: Option<usize>) -> Result<String, String> {
    let mut e = Encoder {
        defs: cx.type_defs(),
        indent,
        out: String::new(),
    };
    e.value(value, 0)?;
    Ok(e.out)
}

// ---- decoding ----

struct Decoder<'a, 'b> {
    defs: Rc<[TypeDef]>,
    /// Computes the default of a struct field that the JSON leaves out.
    cx: &'a mut Ctx<'b>,
}

fn found(json: &Json) -> String {
    match json {
        Json::Null => "null".to_string(),
        Json::Bool(b) => format!("the boolean {b}"),
        Json::Num(n) => format!("the number {n}"),
        Json::Str(s) if s.chars().count() > 20 => {
            format!(
                "a string starting `{}`",
                s.chars().take(20).collect::<String>()
            )
        }
        Json::Str(s) => format!("the string {s:?}"),
        Json::Arr(_) => "an array".to_string(),
        Json::Obj(_) => "an object".to_string(),
    }
}

impl Decoder<'_, '_> {
    fn show(&self, ty: &Ty) -> String {
        match ty {
            Ty::Int(i) => i.name().to_string(),
            Ty::Float => "float".to_string(),
            Ty::F32 => "f32".to_string(),
            Ty::Bool => "bool".to_string(),
            Ty::Char => "char".to_string(),
            Ty::Str => "str".to_string(),
            Ty::Unit => "()".to_string(),
            Ty::Tuple(items) => format!(
                "({})",
                items
                    .iter()
                    .map(|t| self.show(t))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Ty::List(t) => format!("List[{}]", self.show(t)),
            Ty::Set(t) => format!("Set[{}]", self.show(t)),
            Ty::Option(t) => format!("{}?", self.show(t)),
            Ty::Map(k, v) => format!("Map[{}, {}]", self.show(k), self.show(v)),
            Ty::Adt(id, args) | Ty::Dyn(id, args) => {
                let name = type_name(&self.defs, *id);
                if args.is_empty() {
                    name
                } else {
                    format!(
                        "{name}[{}]",
                        args.iter()
                            .map(|t| self.show(t))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            }
            Ty::Fn(..) => "a function".to_string(),
            _ => "this type".to_string(),
        }
    }

    fn mismatch<T>(&self, at: &str, expected: &str, got: &Json) -> Result<T, String> {
        Err(format!(
            "json: at `{at}`: expected {expected}, found {}",
            found(got)
        ))
    }

    fn decode(&mut self, json: &Json, ty: &Ty, at: &str) -> Result<Value, String> {
        match ty {
            Ty::Int(int) => self.int(json, *int, at),
            Ty::Float | Ty::F32 => {
                let Json::Num(text) = json else {
                    return self.mismatch(at, "a number", json);
                };
                let value: f64 = text
                    .parse()
                    .ok()
                    .filter(|v: &f64| v.is_finite())
                    .ok_or_else(|| format!("json: at `{at}`: the number {text} is out of range"))?;
                Ok(if *ty == Ty::F32 {
                    Value::f32(value as f32)
                } else {
                    Value::float(value)
                })
            }
            Ty::Bool => match json {
                Json::Bool(b) => Ok(Value::bool(*b)),
                _ => self.mismatch(at, "a bool", json),
            },
            Ty::Str => match json {
                Json::Str(s) => Ok(Value::str(s)),
                _ => self.mismatch(at, "a string", json),
            },
            Ty::Char => match json {
                Json::Str(s) if s.chars().count() == 1 => {
                    Ok(Value::char(s.chars().next().unwrap_or('\0')))
                }
                _ => self.mismatch(at, "a string of one character", json),
            },
            Ty::Unit => match json {
                Json::Null => Ok(Value::unit()),
                _ => self.mismatch(at, "null", json),
            },
            Ty::Option(inner) => match json {
                Json::Null => Ok(Value::option(None)),
                other => Ok(Value::option(Some(self.decode(other, inner, at)?))),
            },
            Ty::List(inner) => {
                let Json::Arr(items) = json else {
                    return self.mismatch(at, "an array", json);
                };
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    out.push(self.decode(item, inner, &format!("{at}[{i}]"))?);
                }
                Ok(Value::list(out))
            }
            Ty::Tuple(tys) => {
                let Json::Arr(items) = json else {
                    return self.mismatch(at, "an array", json);
                };
                if items.len() != tys.len() {
                    return Err(format!(
                        "json: at `{at}`: expected an array of {} items, found {}",
                        tys.len(),
                        items.len()
                    ));
                }
                let mut out = Vec::with_capacity(items.len());
                for (i, (item, t)) in items.iter().zip(tys).enumerate() {
                    out.push(self.decode(item, t, &format!("{at}[{i}]"))?);
                }
                Ok(Value::tuple(out))
            }
            Ty::Map(key, value) => {
                if **key != Ty::Str {
                    return Err(format!(
                        "json: at `{at}`: an object decodes to a Map with `str` keys, not `{}`",
                        self.show(key)
                    ));
                }
                let Json::Obj(entries) = json else {
                    return self.mismatch(at, "an object", json);
                };
                let mut out = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    out.push((Value::str(k), self.decode(v, value, &format!("{at}.{k}"))?));
                }
                Ok(Value::map(out))
            }
            Ty::Adt(id, args) => self.adt(json, *id, args, at),
            other => Err(format!(
                "json: at `{at}`: cannot decode a value of the type `{}`",
                self.show(other)
            )),
        }
    }

    fn int(&self, json: &Json, int: IntTy, at: &str) -> Result<Value, String> {
        let what = if int == IntTy::I64 {
            "an int".to_string()
        } else {
            format!("a `{}`", int.name())
        };
        let Json::Num(text) = json else {
            return self.mismatch(at, &what, json);
        };
        let Ok(n) = text.parse::<i128>() else {
            return self.mismatch(at, &format!("{what} (a whole number)"), json);
        };
        if n < int.min() || n > int.max() {
            return Err(format!(
                "json: at `{at}`: {n} does not fit in `{}`",
                int.name()
            ));
        }
        Ok(if int == IntTy::U64 {
            Value::u64(n as u64)
        } else {
            Value::int(n as i64)
        })
    }

    fn adt(&mut self, json: &Json, id: TypeId, args: &[Ty], at: &str) -> Result<Value, String> {
        let defs = Rc::clone(&self.defs);
        let Some(def) = defs.get(id.0 as usize) else {
            return Err(format!("json: at `{at}`: cannot decode this type"));
        };
        if !args.is_empty() {
            return Err(format!(
                "json: at `{at}`: cannot decode the generic type `{}`",
                self.show(&Ty::Adt(id, args.to_vec()))
            ));
        }
        match &def.kind {
            TypeDefKind::Struct { fields } => {
                let Json::Obj(entries) = json else {
                    return self.mismatch(at, &format!("an object for `{}`", def.name), json);
                };
                let mut values = Vec::with_capacity(fields.len());
                for field in fields {
                    let here = format!("{at}.{}", field.name);
                    match entries.iter().find(|(k, _)| *k == field.name) {
                        Some((_, v)) => values.push(self.decode(v, &field.ty, &here)?),
                        // A field with a default may be left out (DESIGN §7.5).
                        None if let Some(f) = field.default => match self.cx.call_field_default(f) {
                            Ok(v) => values.push(v),
                            Err(e) => {
                                return Err(format!(
                                    "json: at `{here}`: the default value failed: {e}"
                                ));
                            }
                        },
                        None if matches!(field.ty, Ty::Option(_)) => {
                            values.push(Value::option(None));
                        }
                        None => {
                            return Err(format!(
                                "json: at `{at}`: the object has no field `{}` (a `{}` needs it)",
                                field.name, def.name
                            ));
                        }
                    }
                }
                Ok(Value::new_struct(id, values))
            }
            TypeDefKind::Enum { variants } => {
                if variants.iter().any(|v| !v.fields.is_empty()) {
                    return Err(format!(
                        "json: at `{at}`: cannot decode the enum `{}`: only enums whose variants carry no data are supported",
                        def.name
                    ));
                }
                let Json::Str(name) = json else {
                    return self.mismatch(
                        at,
                        &format!("a string naming a `{}` variant", def.name),
                        json,
                    );
                };
                match variants.iter().position(|v| v.name == *name) {
                    Some(i) => Ok(Value::new_enum(id, i as u32, Vec::new())),
                    None => Err(format!(
                        "json: at `{at}`: `{name}` is not a variant of `{}` (expected one of: {})",
                        def.name,
                        variants
                            .iter()
                            .map(|v| v.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                }
            }
            TypeDefKind::Class { .. } | TypeDefKind::HostClass => Err(format!(
                "json: at `{at}`: `{}` is a class, and json works on structs and enums",
                def.name
            )),
            TypeDefKind::Interface { .. } => Err(format!(
                "json: at `{at}`: cannot decode an interface value (`{}`)",
                def.name
            )),
        }
    }
}

fn decode(cx: &mut Ctx, text: &str) -> Result<Value, String> {
    let Some(ty) = cx.type_args().first() else {
        return Err("json.decode: the call has no type (internal error)".to_string());
    };
    let json = Parser { text, pos: 0 }.document()?;
    let ty = ty.clone();
    Decoder {
        defs: cx.type_defs_shared(),
        cx,
    }
    .decode(&json, &ty, "$")
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("json");
    // fn encode[T](value: T) -> Result[str]
    plain(
        m.register_fn_named("encode", ["value"], |cx: &mut Ctx, value: Value| {
            encode(cx, &value, None)
        }),
    )?;
    // The same with two spaces of indentation, for files people read.
    plain(
        m.register_fn_named("pretty", ["value"], |cx: &mut Ctx, value: Value| {
            encode(cx, &value, Some(2))
        }),
    )?;
    // fn decode[T](text: str) -> Result[T]: the type comes from the call (`json.decode[Player](text)`).
    m.register_fn_named("decode", ["text"], |cx: &mut Ctx, text: String| {
        decode(cx, &text)
    })?
    .typed()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Json, String> {
        Parser { text, pos: 0 }.document()
    }

    #[test]
    fn parses_every_kind_of_value() {
        assert_eq!(parse(" null ").unwrap(), Json::Null);
        assert_eq!(
            parse("[1, -2.5e3, \"a\\n\\u00e9\\ud83d\\ude00\"]").unwrap(),
            Json::Arr(vec![
                Json::Num("1".into()),
                Json::Num("-2.5e3".into()),
                Json::Str("a\né😀".into()),
            ])
        );
        assert_eq!(
            parse("{\"a\": {\"b\": [true, false]}}").unwrap(),
            Json::Obj(vec![(
                "a".into(),
                Json::Obj(vec![(
                    "b".into(),
                    Json::Arr(vec![Json::Bool(true), Json::Bool(false)])
                )])
            )])
        );
    }

    #[test]
    fn rejects_what_json_does_not_allow() {
        for bad in [
            "",
            "[1,]",
            "{\"a\":1,}",
            "01",
            "1.",
            ".5",
            "+1",
            "'a'",
            "[1 2]",
            "{\"a\" 1}",
            "\"a\nb\"",
            "\"\\x\"",
            "\"\\ud800\"",
            "nul",
            "[1] 2",
            "{\"a\":1,\"a\":2}",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
        let deep = "[".repeat(300);
        assert!(parse(&deep).unwrap_err().contains("too deeply"));
    }

    #[test]
    fn syntax_errors_say_where() {
        let e = parse("{\n  \"a\": ?\n}").unwrap_err();
        assert!(e.contains("line 2, column 8"), "{e}");
    }

    #[test]
    fn strings_escape_what_they_must() {
        let mut out = String::new();
        escape(&mut out, "a\"b\\c\n\u{1}é");
        assert_eq!(out, "\"a\\\"b\\\\c\\n\\u0001é\"");
    }
}
