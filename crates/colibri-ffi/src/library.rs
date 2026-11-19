//! Finding and opening shared libraries (ARCHITECTURE "C FFI as built").
//!
//! `@link("name")` names a library the way a linker flag does (`-lname`). Opening it tries,
//! in order: the name as a path (when it contains a separator or ends in a library
//! extension), the platform's file names for it, the versioned files the system keeps
//! (`libm.so.6`), and, for the C runtime's own libraries (`c`, `m`, `dl`, `pthread`), the
//! symbols already in the process.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::rc::Rc;

/// An open shared library, or the process itself.
pub struct Library {
    name: String,
    inner: libloading::Library,
}

impl Library {
    /// The name it was opened by.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The address of a symbol.
    pub fn symbol(&self, symbol: &str) -> Result<*const c_void, String> {
        // SAFETY: only the address is read; calling it is `Function::call`'s business.
        let found: Result<libloading::Symbol<'_, *const c_void>, _> =
            unsafe { self.inner.get(symbol.as_bytes()) };
        match found {
            Ok(sym) => Ok(*sym),
            Err(_) => Err(format!(
                "the library `{}` has no symbol `{symbol}`",
                self.name
            )),
        }
    }
}

/// The libraries a program opened, so each is loaded once.
#[derive(Default)]
pub struct Libraries {
    open: HashMap<String, Rc<Library>>,
}

impl Libraries {
    pub fn new() -> Self {
        Self::default()
    }

    /// The library `name` (see the module comment), or the process itself for `None`.
    pub fn get(&mut self, name: Option<&str>) -> Result<Rc<Library>, String> {
        let key = name.unwrap_or("").to_string();
        if let Some(lib) = self.open.get(&key) {
            return Ok(Rc::clone(lib));
        }
        let lib = Rc::new(match name {
            Some(name) => open(name)?,
            None => process()?,
        });
        self.open.insert(key, Rc::clone(&lib));
        Ok(lib)
    }
}

/// The symbols of the running program and the libraries it already loaded (the C library).
fn process() -> Result<Library, String> {
    #[cfg(unix)]
    let inner: libloading::Library = libloading::os::unix::Library::this().into();
    #[cfg(windows)]
    let inner: libloading::Library = libloading::os::windows::Library::this()
        .map_err(|e| format!("cannot look in the running program: {e}"))?
        .into();
    Ok(Library {
        name: "the running program".to_string(),
        inner,
    })
}

/// The libraries of the C runtime: on most systems they are part of the process already.
fn is_runtime_lib(name: &str) -> bool {
    matches!(name, "c" | "m" | "dl" | "pthread" | "rt" | "util")
}

fn open(name: &str) -> Result<Library, String> {
    let mut tried: Vec<String> = Vec::new();
    for candidate in candidates(name) {
        // SAFETY: loading a library runs its initialisers. That is what `@link` asks for, and
        // the package declared the `ffi` capability to be allowed to ask.
        match unsafe { libloading::Library::new(&candidate) } {
            Ok(inner) => {
                return Ok(Library {
                    name: name.to_string(),
                    inner,
                });
            }
            Err(_) => tried.push(candidate),
        }
    }
    if is_runtime_lib(name)
        && let Ok(lib) = process()
    {
        return Ok(Library {
            name: name.to_string(),
            inner: lib.inner,
        });
    }
    tried.dedup();
    let shown: Vec<String> = tried.iter().map(|t| format!("`{t}`")).collect();
    Err(format!(
        "cannot load the library `{name}` (tried {})",
        shown.join(", ")
    ))
}

const LIB_EXTENSIONS: [&str; 3] = [".so", ".dylib", ".dll"];

/// The file names to try for `@link("name")`, most likely first.
fn candidates(name: &str) -> Vec<String> {
    let looks_like_path = name.contains('/')
        || name.contains('\\')
        || LIB_EXTENSIONS.iter().any(|e| name.contains(e))
        || Path::new(name).extension().is_some_and(|e| e == "so");
    if looks_like_path {
        return vec![name.to_string()];
    }
    let mut out = Vec::new();
    if cfg!(target_os = "macos") {
        out.push(format!("lib{name}.dylib"));
        for dir in ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"] {
            out.push(format!("{dir}/lib{name}.dylib"));
        }
    } else if cfg!(windows) {
        out.push(format!("{name}.dll"));
        out.push(format!("lib{name}.dll"));
        if matches!(name, "c" | "m") {
            out.push("ucrtbase.dll".to_string());
            out.push("msvcrt.dll".to_string());
        }
    } else {
        out.push(format!("lib{name}.so"));
        // The development symlink `libz.so` is often missing, and `libm.so` is a linker
        // script that `dlopen` rejects; the runtime files carry a version.
        match name {
            "m" => out.push("libm.so.6".to_string()),
            "c" => out.push("libc.so.6".to_string()),
            "dl" => out.push("libdl.so.2".to_string()),
            "pthread" => out.push("libpthread.so.0".to_string()),
            _ => {}
        }
        out.extend(versioned_files(name));
    }
    out
}

/// `lib{name}.so.N...` in the usual library folders, the highest version first.
#[cfg(unix)]
fn versioned_files(name: &str) -> Vec<String> {
    let prefix = format!("lib{name}.so.");
    let mut found: Vec<(Vec<u64>, String)> = Vec::new();
    for dir in [
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/usr/local/lib",
        "/lib/x86_64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
        "/lib/aarch64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
    ] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            if let Some(version) = file.strip_prefix(&prefix) {
                let key: Vec<u64> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
                found.push((key, format!("{dir}/{file}")));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, path)| path).collect()
}

#[cfg(not(unix))]
fn versioned_files(_name: &str) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_used_as_it_is() {
        assert_eq!(candidates("/opt/x/libfoo.so"), ["/opt/x/libfoo.so"]);
        assert_eq!(candidates("./foo.dll"), ["./foo.dll"]);
    }

    #[test]
    fn a_name_gets_platform_files() {
        let c = candidates("z");
        assert!(c.iter().any(|f| f.contains("libz") || f.contains("z.dll")));
    }

    #[test]
    fn a_missing_library_is_an_error_that_lists_what_was_tried() {
        let e = open("definitely_not_a_library_xyz")
            .err()
            .unwrap_or_default();
        assert!(e.contains("cannot load the library `definitely_not_a_library_xyz`"));
    }
}
