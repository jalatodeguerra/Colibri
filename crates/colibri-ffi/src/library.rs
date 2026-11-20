//! Finding and opening shared libraries (ARCHITECTURE "C FFI as built").
//!
//! `@link("name")` names a library the way a linker flag does (`-lname`). Opening it tries,
//! in order: the name as a path (when it contains a separator or ends in a library
//! extension), the platform's file names for it, the versioned files the system keeps
//! (`libm.so.6`), and, for the C runtime's own libraries (`c`, `m`, `dl`, `pthread`), the
//! symbols already in the process.
//!
//! "The process" (an `extern` block without `@link`) means the running program and every
//! library it has loaded. Unix has one handle for that; on Windows each loaded module is
//! asked in turn, the program first and the C runtime second.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::rc::Rc;

/// An open shared library, or the process itself.
pub struct Library {
    name: String,
    inner: libloading::Library,
    /// Whether this is the process itself rather than one library (only Windows asks: there
    /// the handle of the program finds only what the program exports).
    #[cfg(windows)]
    whole_process: bool,
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
        if let Ok(sym) = found {
            return Ok(*sym);
        }
        #[cfg(windows)]
        if self.whole_process
            && let Some(address) = loaded::symbol(symbol)
        {
            return Ok(address);
        }
        Err(format!(
            "the library `{}` has no symbol `{symbol}`",
            self.name
        ))
    }
}

/// The libraries the process has loaded, on Windows, where the handle of the program finds
/// only what the program itself exports (`dlopen(NULL)` on Unix finds all of it).
#[cfg(windows)]
mod loaded {
    use std::ffi::{CString, c_char, c_void};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetModuleHandleA(name: *const c_char) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *const c_void;
        fn K32EnumProcessModules(
            process: *mut c_void,
            modules: *mut *mut c_void,
            bytes: u32,
            needed: *mut u32,
        ) -> i32;
    }

    /// The C runtime libraries, asked before the rest: system libraries such as `ntdll`
    /// export reduced versions of some C functions under the same names.
    const C_RUNTIME: [&std::ffi::CStr; 2] = [c"ucrtbase.dll", c"msvcrt.dll"];

    fn in_module(module: *mut c_void, name: &CString) -> Option<*const c_void> {
        if module.is_null() {
            return None;
        }
        // SAFETY: `name` is a NUL-terminated string. `module` is not checked: Windows does not
        // promise that a handle to an unloaded module fails cleanly. What this relies on is that
        // `module` came from the list of modules loaded in this process a moment ago (or from
        // `GetModuleHandleA`, which does not count a reference) and that nothing unloads a
        // module while the lookup runs: this crate only unloads a library when its `Library` is
        // dropped, and that is not during a lookup. Another thread of the program calling
        // `FreeLibrary` on that very module in that moment would make the call undefined, and
        // nothing here prevents it.
        let address = unsafe { GetProcAddress(module, name.as_ptr()) };
        (!address.is_null()).then_some(address)
    }

    /// Every module loaded in the process, in load order (the program is first).
    fn modules() -> Vec<*mut c_void> {
        let slot = std::mem::size_of::<*mut c_void>();
        let mut found: Vec<*mut c_void> = vec![std::ptr::null_mut(); 256];
        for _ in 0..4 {
            let bytes = u32::try_from(found.len() * slot).unwrap_or(u32::MAX);
            let mut needed = 0u32;
            // SAFETY: `found` is `bytes` bytes of writable handles and `needed` a valid place
            // for the size the full list takes.
            let ok = unsafe {
                K32EnumProcessModules(GetCurrentProcess(), found.as_mut_ptr(), bytes, &mut needed)
            };
            if ok == 0 {
                return Vec::new();
            }
            let count = needed as usize / slot;
            if count <= found.len() {
                found.truncate(count);
                return found;
            }
            // More modules than fitted: ask again with room for all of them.
            found = vec![std::ptr::null_mut(); count + 64];
        }
        Vec::new()
    }

    /// The address of `symbol` in the first loaded module that exports it.
    pub fn symbol(symbol: &str) -> Option<*const c_void> {
        let name = CString::new(symbol).ok()?;
        for runtime in C_RUNTIME {
            // SAFETY: the name is a NUL-terminated string; the call only looks the module up.
            let module = unsafe { GetModuleHandleA(runtime.as_ptr()) };
            if let Some(address) = in_module(module, &name) {
                return Some(address);
            }
        }
        modules()
            .into_iter()
            .find_map(|module| in_module(module, &name))
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
        #[cfg(windows)]
        whole_process: true,
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
                    #[cfg(windows)]
                    whole_process: false,
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
            #[cfg(windows)]
            whole_process: true,
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
