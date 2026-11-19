//! `fs`: files and folders. Every function needs the `fs` capability and returns a `Result`
//! whose message says what was attempted and why it failed (`cannot read `x`: no such file or
//! directory`). Paths are used as given: relative ones are relative to the process's current
//! folder.

use std::io::ErrorKind;
use std::path::Path;

use colibri_runtime::{Cap, Error, Vm};

use crate::needs;

/// Why an operation failed, worded the same on every platform for the common cases.
pub(crate) fn reason(e: &std::io::Error) -> String {
    match e.kind() {
        ErrorKind::NotFound => "no such file or directory".to_string(),
        ErrorKind::PermissionDenied => "permission denied".to_string(),
        ErrorKind::AlreadyExists => "already exists".to_string(),
        ErrorKind::NotADirectory => "not a directory".to_string(),
        ErrorKind::IsADirectory => "is a directory".to_string(),
        ErrorKind::DirectoryNotEmpty => "directory not empty".to_string(),
        ErrorKind::InvalidData => "the data is not valid UTF-8 text".to_string(),
        _ => {
            // "Broken pipe (os error 32)" -> "broken pipe"
            let text = e.to_string();
            let text = text.split(" (os error").next().unwrap_or(&text);
            let mut chars = text.chars();
            match chars.next() {
                Some(first) => first.to_lowercase().chain(chars).collect(),
                None => "unknown error".to_string(),
            }
        }
    }
}

fn fail(action: &str, path: &str, e: &std::io::Error) -> String {
    format!("cannot {action} `{path}`: {}", reason(e))
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("fs");
    macro_rules! fs_fn {
        ($name:literal, [$($p:literal),*], $f:expr) => {
            needs(Cap::Fs, m.register_fn_named($name, [$($p),*], $f))?;
        };
    }
    fs_fn!("read", ["path"], |path: String| {
        std::fs::read_to_string(&path).map_err(|e| fail("read", &path, &e))
    });
    fs_fn!("read_bytes", ["path"], |path: String| {
        std::fs::read(&path).map_err(|e| fail("read", &path, &e))
    });
    fs_fn!("write", ["path", "text"], |path: String, text: String| {
        std::fs::write(&path, text).map_err(|e| fail("write", &path, &e))
    });
    fs_fn!(
        "write_bytes",
        ["path", "bytes"],
        |path: String, bytes: Vec<u8>| {
            std::fs::write(&path, bytes).map_err(|e| fail("write", &path, &e))
        }
    );
    fs_fn!("append", ["path", "text"], |path: String, text: String| {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .map_err(|e| fail("append to", &path, &e))
    });
    fs_fn!("exists", ["path"], |path: String| Path::new(&path).exists());
    fs_fn!("is_file", ["path"], |path: String| Path::new(&path)
        .is_file());
    fs_fn!("is_dir", ["path"], |path: String| Path::new(&path).is_dir());
    // The names in the folder (not paths), sorted, so the order never depends on the platform.
    fs_fn!("list", ["path"], |path: String| {
        let entries = std::fs::read_dir(&path).map_err(|e| fail("list", &path, &e))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| fail("list", &path, &e))?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok::<_, String>(names)
    });
    // Like `mkdir -p`: also makes the parents, and is fine when the folder exists already.
    fs_fn!("create_dir", ["path"], |path: String| {
        std::fs::create_dir_all(&path).map_err(|e| fail("create the folder", &path, &e))
    });
    fs_fn!("remove", ["path"], |path: String| {
        std::fs::remove_file(&path).map_err(|e| fail("remove", &path, &e))
    });
    // A folder and everything in it.
    fs_fn!("remove_dir", ["path"], |path: String| {
        std::fs::remove_dir_all(&path).map_err(|e| fail("remove the folder", &path, &e))
    });
    fs_fn!("copy", ["from", "to"], |from: String, to: String| {
        std::fs::copy(&from, &to)
            .map(|_| ())
            .map_err(|e| fail("copy", &from, &e))
    });
    fs_fn!("rename", ["from", "to"], |from: String, to: String| {
        std::fs::rename(&from, &to).map_err(|e| fail("rename", &from, &e))
    });
    fs_fn!("size", ["path"], |path: String| {
        std::fs::metadata(&path)
            .map_err(|e| fail("look at", &path, &e))
            .and_then(|meta| i64::try_from(meta.len()).map_err(|_| format!("`{path}` is too big")))
    });
    // Last modification, in seconds since the Unix epoch (like `time.unix()`).
    fs_fn!("modified", ["path"], |path: String| {
        let meta = std::fs::metadata(&path).map_err(|e| fail("look at", &path, &e))?;
        let time = meta.modified().map_err(|e| fail("look at", &path, &e))?;
        Ok::<_, String>(match time.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs_f64(),
            Err(e) => -e.duration().as_secs_f64(),
        })
    });
    Ok(())
}
