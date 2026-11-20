//! `toml`: TOML text to an untyped `json.Value` or to a typed struct.
//!
//! - `toml.parse(text) -> Result[json.Value]`: the document as a tree of `json.Value` (the
//!   class of the `json` module, which `toml` declares it uses, so importing `toml` brings
//!   `json` in), with its accessors (`doc.get("server")?.get("port")?.as_int()`, `kind()`,
//!   `as_list()`, ...), `to_json()` and `equals`. Tables keep the order of the text. Integers
//!   are `int`, floats are `float` (`inf` and `nan` too), and a date or time is its text as a
//!   `string` (`1979-05-27T07:32:00Z`): `kind()` does not tell it from a string.
//! - `toml.decode[T](text) -> Result[T]`: like `json.decode[T]`, through the same decoder:
//!   field names, `@rename`, `@skip`, defaults, `Option`, enums (a variant with data is a table
//!   with one key) and `json.Value` fields work the same, and a date or time reads into a
//!   `str`. A key that is not a field of the struct is ignored.
//!
//! The `toml` crate (`parse` feature only, so no serde) reads the text; there is no way to
//! write TOML yet.

use colibri_runtime::{Ctx, Error, Vm};
use toml::de::{DeTable, DeValue};

use crate::errors::error_kind;
use crate::json::{Jv, decode_tree};
use crate::plain;

error_kind!(
    "toml",
    /// Why a toml call failed (`toml.Error.Syntax`).
    Kind {
        /// The text is not TOML.
        Syntax,
        /// The TOML is fine but is not a value of the type asked for.
        Mismatch,
        /// The type cannot be read from TOML (a class, a function).
        Unsupported,
    }
);

fn value(v: &DeValue<'_>) -> Result<Jv, String> {
    Ok(match v {
        DeValue::String(s) => Jv::Str(s.as_ref().into()),
        DeValue::Integer(i) => match i64::from_str_radix(i.as_str(), i.radix()) {
            Ok(n) => Jv::Int(n),
            Err(_) => {
                return Err(format!(
                    "toml: the integer {} does not fit in 64 bits",
                    i.as_str()
                ));
            }
        },
        DeValue::Float(f) => match f.as_str().parse::<f64>() {
            Ok(n) => Jv::Float(n),
            Err(_) => return Err(format!("toml: cannot read the float {}", f.as_str())),
        },
        DeValue::Boolean(b) => Jv::Bool(*b),
        DeValue::Datetime(d) => Jv::Str(d.to_string().into()),
        DeValue::Array(items) => Jv::List(std::sync::Arc::new(
            items
                .iter()
                .map(|item| value(item.get_ref()))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        DeValue::Table(table) => table_value(table)?,
    })
}

fn table_value(table: &DeTable<'_>) -> Result<Jv, String> {
    let mut entries = Vec::with_capacity(table.len());
    for (key, item) in table {
        entries.push((key.get_ref().to_string(), value(item.get_ref())?));
    }
    Ok(Jv::Map(std::sync::Arc::new(entries)))
}

fn read(text: &str) -> Result<Jv, colibri_runtime::TypedError> {
    let table = DeTable::parse(text).map_err(|e| Kind::Syntax.error(format!("toml: {e}")))?;
    table_value(table.get_ref()).map_err(|m| Kind::Mismatch.error(m))
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("toml");
    // `parse` returns the `json.Value` of the json module: one tree type for every format.
    m.uses("std.json")?;
    plain(m.register_fn_named("parse", ["text"], |text: String| read(&text)))?;
    m.register_fn_named("decode", ["text"], |cx: &mut Ctx, text: String| {
        let tree = read(&text)?;
        decode_tree(cx, &tree).map_err(|(unsupported, m)| {
            let m = m
                .replacen("json:", "toml:", 1)
                .replace("json works", "toml works");
            if unsupported {
                Kind::Unsupported.error(m)
            } else {
                Kind::Mismatch.error(m)
            }
        })
    })?
    .typed()?;
    Ok(())
}
