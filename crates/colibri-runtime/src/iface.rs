//! Interface (`.cbi`) text (EMBEDDING §4): printing it from what the host registered, reading
//! it back, and comparing it with a file.
//!
//! One path serves everyone: a live `Vm` prints the canonical text from its registrations and
//! parses it into the `HostEnv` the checker uses; `Vm::write_interface` writes the same text;
//! `colibri check --interface engine.cbi` parses a file of it. The text is the contract.

use std::path::Path;

use colibri_check::HostEnv;
use colibri_check::host::{DEFAULT_MODULE, body_hash, header_line};

use crate::cap::Cap;
use crate::error::Error;
use crate::host::RawHostFn;
use crate::types::{SigDesc, TypeDecl, TypeRegistry};

/// A registered host function.
pub(crate) struct HostFnEntry {
    /// The host module the function is registered in (`host` is the default one).
    pub module: String,
    pub name: String,
    pub sig: SigDesc,
    pub raw: RawHostFn,
    pub requires: Option<Cap>,
    /// Its parameter names are placeholders (`a`, `b`, ..), so scripts cannot name arguments.
    pub positional: bool,
    /// The real parameter names, when the host gave them (then `positional` is false).
    pub names: Option<Vec<String>>,
}

/// The canonical text of an interface.
pub(crate) struct Rendered {
    pub text: String,
    pub hash: String,
}

/// The first line of every interface file.
use colibri_check::host::GENERATED;

/// The placeholder name of the `i`th parameter of a closure.
fn param_name(i: usize) -> char {
    char::from(b'a' + (i as u8 % 26))
}

/// Prints the canonical interface of the types and functions of one module: the types in the
/// order they were described (which is their type id order), then the functions in
/// registration order (which is their `HostFnId` order).
pub(crate) fn render(reg: &TypeRegistry, fns: &[HostFnEntry], module: &str) -> Rendered {
    let mut body = String::new();
    for (i, decl) in reg.decls.iter().enumerate() {
        if reg.decl_modules[i].as_deref().unwrap_or(DEFAULT_MODULE) != module {
            continue;
        }
        match decl {
            TypeDecl::Struct { name, fields } => {
                if fields.is_empty() {
                    body.push_str(&format!("struct {name} {{}}\n"));
                } else {
                    let fields: Vec<String> =
                        fields.iter().map(|(n, t)| format!("{n}: {t}")).collect();
                    body.push_str(&format!("struct {name} {{ {} }}\n", fields.join(", ")));
                }
            }
            TypeDecl::Enum { name, variants } => {
                body.push_str(&format!("enum {name} {{ {} }}\n", variants.join(", ")));
            }
            TypeDecl::Class { name } => {
                body.push_str(&format!("@host class {name} {{}}\n"));
                let methods = reg
                    .classes
                    .iter()
                    .find(|c| c.name == *name)
                    .map(|c| c.methods.as_slice())
                    .unwrap_or_default();
                if !methods.is_empty() {
                    body.push_str(&format!("impl {name} {{\n"));
                    for m in methods {
                        let mut params: Vec<String> = Vec::new();
                        if m.has_self {
                            params.push("self".to_string());
                        }
                        params.extend(
                            m.params
                                .iter()
                                .zip(&m.sig.params)
                                .map(|(n, t)| format!("{n}: {t}")),
                        );
                        body.push_str(&format!("    fn {}({})", m.name, params.join(", ")));
                        if m.sig.ret != crate::types::TypeDesc::Unit {
                            body.push_str(&format!(" -> {}", m.sig.ret));
                        }
                        body.push('\n');
                    }
                    body.push_str("}\n");
                }
            }
        }
    }
    let mut caps: Vec<Cap> = Vec::new();
    for f in fns.iter().filter(|f| f.module == module) {
        let mut line = String::new();
        if let Some(cap) = f.requires {
            line.push_str(&format!("@requires({cap}) "));
            if !caps.contains(&cap) {
                caps.push(cap);
            }
        }
        if f.positional {
            line.push_str("@positional ");
        }
        let params: Vec<String> = f
            .sig
            .params
            .iter()
            .enumerate()
            .map(|(i, t)| match &f.names {
                Some(names) => format!("{}: {t}", names[i]),
                None => format!("{}: {t}", param_name(i)),
            })
            .collect();
        line.push_str(&format!("fn {}({})", f.name, params.join(", ")));
        if f.sig.ret != crate::types::TypeDesc::Unit {
            line.push_str(&format!(" -> {}", f.sig.ret));
        }
        body.push_str(&line);
        body.push('\n');
    }
    caps.sort();
    let names: Vec<String> = caps.iter().map(|c| c.name().to_string()).collect();
    let hash = body_hash(&body);
    let text = format!(
        "{GENERATED}\n{}\n{body}",
        header_line(module, &hash, &names)
    );
    Rendered { text, hash }
}

/// The hash a file's header declares, if it has a readable one.
pub(crate) fn declared_hash(text: &str) -> Option<String> {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("@interface("))?;
    let rest = &line[line.find("hash")?..];
    let start = rest.find('"')? + 1;
    let end = start + rest[start..].find('"')?;
    Some(rest[start..end].to_string())
}

/// Writes `text` to `path` unless the file already holds exactly that. Whether it wrote.
pub(crate) fn write_if_changed(path: &Path, text: &str) -> Result<bool, Error> {
    if std::fs::read_to_string(path).is_ok_and(|current| current == text) {
        return Ok(false);
    }
    std::fs::write(path, text).map_err(|e| Error::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    Ok(true)
}

/// An interface file read for checking (`colibri check --interface engine.cbi`).
#[derive(Clone, Debug)]
pub struct Interface {
    pub(crate) env: HostEnv,
    path: String,
    hash: String,
}

impl Interface {
    /// Parses interface text; `path` names it in messages.
    pub fn from_text(path: &str, text: &str) -> Result<Interface, Error> {
        let env = HostEnv::from_text(text).map_err(|message| Error::Interface {
            path: path.to_string(),
            message,
        })?;
        let hash = env
            .headers()
            .next()
            .map(|h| h.hash.clone())
            .unwrap_or_default();
        Ok(Interface {
            env,
            path: path.to_string(),
            hash,
        })
    }

    /// Reads and parses an interface file.
    pub fn read(path: impl AsRef<Path>) -> Result<Interface, Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        Interface::from_text(&path.display().to_string(), &text)
    }

    /// The canonical text, printed back from what was parsed. For a file the host generated
    /// this is the file's text exactly.
    pub fn canonical_text(&self) -> String {
        self.env.canonical_texts().join("")
    }

    /// The hash in the file's header.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Where the interface came from.
    pub fn path(&self) -> &str {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TypeDesc;

    fn entry(name: &str, params: Vec<TypeDesc>, ret: TypeDesc, cap: Option<Cap>) -> HostFnEntry {
        HostFnEntry {
            module: DEFAULT_MODULE.to_string(),
            name: name.to_string(),
            sig: SigDesc { params, ret },
            raw: std::rc::Rc::new(|_, _| unreachable!()),
            requires: cap,
            positional: true,
            names: None,
        }
    }

    #[test]
    fn prints_the_documented_shape() {
        let mut reg = TypeRegistry::new();
        let vec3 = reg.define_struct(
            "Vec3",
            vec![
                ("x", TypeDesc::F32),
                ("y", TypeDesc::F32),
                ("z", TypeDesc::F32),
            ],
        );
        let fns = vec![
            entry("spawn", vec![TypeDesc::F32, TypeDesc::F32], vec3, None),
            entry(
                "read_save",
                vec![TypeDesc::Str],
                TypeDesc::Result(Box::new(TypeDesc::Str)),
                Some(Cap::Fs),
            ),
        ];
        let out = render(&reg, &fns, DEFAULT_MODULE);
        let lines: Vec<&str> = out.text.lines().collect();
        assert_eq!(lines[0], GENERATED);
        assert!(
            lines[1].starts_with("@interface(format = 1, module = \"host\", hash = \""),
            "{}",
            lines[1]
        );
        assert!(lines[1].ends_with("caps = [\"fs\"])"));
        assert_eq!(lines[2], "struct Vec3 { x: f32, y: f32, z: f32 }");
        assert_eq!(lines[3], "@positional fn spawn(a: f32, b: f32) -> Vec3");
        assert_eq!(
            lines[4],
            "@requires(fs) @positional fn read_save(a: str) -> Result[str]"
        );
        assert_eq!(declared_hash(&out.text), Some(out.hash.clone()));
        // The checker reads what is printed.
        assert!(HostEnv::from_text(&out.text).is_ok());
    }

    #[test]
    fn writing_happens_only_when_the_text_changed() {
        let dir = std::env::temp_dir().join(format!("colibri-iface-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.cbi");
        assert!(write_if_changed(&path, "one").unwrap());
        assert!(!write_if_changed(&path, "one").unwrap());
        assert!(write_if_changed(&path, "two").unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
