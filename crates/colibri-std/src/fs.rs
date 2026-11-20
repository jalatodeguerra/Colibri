//! `fs`: files and folders. Every function needs the `fs` capability and returns a `Result`
//! whose message says what was attempted and why it failed (`cannot read `x`: no such file or
//! directory`). Paths are used as given: relative ones are relative to the process's current
//! folder.

use std::io::ErrorKind;
use std::path::Path;

use colibri_runtime::{Cap, Error, TypedError, Vm};

use crate::errors::error_kind;
use crate::needs;
use crate::values::value_struct;

error_kind!(
    "fs",
    /// Why an `fs` function failed: what a script branches on (`fs.Error.NotFound`).
    Kind {
        NotFound,
        PermissionDenied,
        AlreadyExists,
        NotADirectory,
        IsADirectory,
        DirectoryNotEmpty,
        InvalidData,
        Other,
    }
);

/// What `fs.stat` reports: one look at a path, without following a final symlink
/// (`fs.stat_follow` does follow it).
/// `kind` is `"file"`, `"dir"`, `"symlink"` or `"other"`; times are seconds since the Unix
/// epoch; `created` is `none` where the platform or filesystem does not record it.
#[derive(Clone, Debug)]
pub(crate) struct Stat {
    pub kind: String,
    pub size: i64,
    pub modified: f64,
    pub created: Option<f64>,
    pub readonly: bool,
}

value_struct!(Stat {
    kind: String,
    size: i64,
    modified: f64,
    created: Option<f64>,
    readonly: bool,
});

fn unix_seconds(time: std::time::SystemTime) -> f64 {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

/// Every entry below `dir`, as paths relative to `root` with `/` separators. Symlinks are
/// listed but never entered, so a link cycle cannot loop.
fn walk_into(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let parts: Vec<_> = rel.iter().map(|p| p.to_string_lossy()).collect();
        out.push(parts.join("/"));
        if entry.file_type()?.is_dir() {
            walk_into(root, &path, out)?;
        }
    }
    Ok(())
}

/// A path the standard library hands to a script (`os.temp_dir`, `os.cwd`): scripts use `/`
/// on every system (see `path`), so on Windows `C:\Users\me` is given as `C:/Users/me`, which
/// Windows accepts everywhere. A path that comes from a script (a name it passed in) is never
/// changed.
pub(crate) fn script_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        windows_script_path(&text)
    } else {
        text.into_owned()
    }
}

/// [`script_path`] for a Windows path: backslashes become `/`, and the verbatim prefix that
/// some system calls add (`\\?\C:\x`, `\\?\UNC\server\share`) is dropped, so that `C:/x` and
/// `//server/share` come out. Pure text, so it is tested on every system.
pub(crate) fn windows_script_path(path: &str) -> String {
    let plain = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(local) = path.strip_prefix(r"\\?\") {
        local.to_string()
    } else {
        path.to_string()
    };
    plain.replace('\\', "/")
}

/// Opens a file or folder to change its times. Unix lets a read-only handle do it; Windows
/// needs a handle with `FILE_WRITE_ATTRIBUTES` access (a plain read handle gives "permission
/// denied"), and `FILE_FLAG_BACKUP_SEMANTICS` to open a folder at all. That access does not
/// need the file to be writable, nor does it change its contents.
fn open_for_times(path: &str) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::File::open(path)
    }
}

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

/// The variant of `fs.Error` an I/O error is.
pub(crate) fn kind_of(e: &std::io::Error) -> Kind {
    match e.kind() {
        ErrorKind::NotFound => Kind::NotFound,
        ErrorKind::PermissionDenied => Kind::PermissionDenied,
        ErrorKind::AlreadyExists => Kind::AlreadyExists,
        ErrorKind::NotADirectory => Kind::NotADirectory,
        ErrorKind::IsADirectory => Kind::IsADirectory,
        ErrorKind::DirectoryNotEmpty => Kind::DirectoryNotEmpty,
        ErrorKind::InvalidData => Kind::InvalidData,
        _ => Kind::Other,
    }
}

fn fail(action: &str, path: &str, e: &std::io::Error) -> TypedError {
    // Windows says "access denied" where Unix says "is a directory" when a folder is opened
    // as a file: say what is meant, so a script branches on the same variant everywhere.
    if cfg!(windows)
        && e.kind() == ErrorKind::PermissionDenied
        && matches!(action, "read" | "write" | "append to")
        && Path::new(path).is_dir()
    {
        return Kind::IsADirectory.error(format!(
            "cannot {action} `{path}`: {}",
            reason(&std::io::Error::from(ErrorKind::IsADirectory))
        ));
    }
    kind_of(e).error(format!("cannot {action} `{path}`: {}", reason(e)))
}

fn stat_of(path: &str, meta: &std::fs::Metadata) -> Result<Stat, TypedError> {
    let kind = if meta.file_type().is_symlink() {
        "symlink"
    } else if meta.is_dir() {
        "dir"
    } else if meta.is_file() {
        "file"
    } else {
        "other"
    };
    let size =
        i64::try_from(meta.len()).map_err(|_| Kind::Other.error(format!("`{path}` is too big")))?;
    let modified = meta.modified().map_err(|e| fail("look at", path, &e))?;
    Ok(Stat {
        kind: kind.to_string(),
        size,
        modified: unix_seconds(modified),
        created: meta.created().ok().map(unix_seconds),
        readonly: meta.permissions().readonly(),
    })
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
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
        Ok::<_, TypedError>(names)
    });
    // Like `mkdir -p`: also makes the parents, and is fine when the folder exists already.
    fs_fn!("create_dir", ["path"], |path: String| {
        std::fs::create_dir_all(&path).map_err(|e| fail("create the folder", &path, &e))
    });
    fs_fn!("remove", ["path"], |path: String| {
        std::fs::remove_file(&path).map_err(|e| fail("remove", &path, &e))
    });
    // Only an empty folder; `remove_tree` is the one that removes a folder with everything
    // in it, and the message of a folder that is not empty says so.
    fs_fn!("remove_dir", ["path"], |path: String| {
        std::fs::remove_dir(&path).map_err(|e| {
            if e.kind() == ErrorKind::DirectoryNotEmpty {
                Kind::DirectoryNotEmpty.error(format!(
                    "cannot remove the folder `{path}`: it is not empty \
                     (use `fs.remove_tree` to remove a folder with its contents)"
                ))
            } else {
                fail("remove the folder", &path, &e)
            }
        })
    });
    // RECURSIVE: a folder and everything in it.
    fs_fn!("remove_tree", ["path"], |path: String| {
        std::fs::remove_dir_all(&path).map_err(|e| fail("remove the folder", &path, &e))
    });
    // Everything below `path`, files and folders, relative to it, sorted. Symlinks are listed
    // and not followed.
    fs_fn!("walk", ["path"], |path: String| {
        let mut out = Vec::new();
        walk_into(Path::new(&path), Path::new(&path), &mut out)
            .map_err(|e| fail("walk", &path, &e))?;
        out.sort();
        Ok::<_, TypedError>(out)
    });
    // One look at a path; a final symlink is described, not followed (`kind` is `"symlink"`
    // and `size` is the link's own). `stat_follow` looks at what it points to.
    fs_fn!("stat", ["path"], |path: String| {
        let meta = std::fs::symlink_metadata(&path).map_err(|e| fail("look at", &path, &e))?;
        stat_of(&path, &meta)
    });
    // `stat`, but a symlink is followed: the kind, size and times are those of its target
    // (an error if the target does not exist).
    fs_fn!("stat_follow", ["path"], |path: String| {
        let meta = std::fs::metadata(&path).map_err(|e| fail("look at", &path, &e))?;
        stat_of(&path, &meta)
    });
    // Sets the modification time, in seconds since the Unix epoch.
    fs_fn!(
        "set_modified",
        ["path", "unix_seconds"],
        |path: String, seconds: f64| {
            let fail_set = |e: &std::io::Error| fail("set the time of", &path, e);
            if !seconds.is_finite() || seconds.abs() > 1e11 {
                return Err(Kind::Other.error(format!(
                    "cannot set the time of `{path}`: {seconds} is not a usable time"
                )));
            }
            let magnitude = std::time::Duration::from_secs_f64(seconds.abs());
            let time = if seconds >= 0.0 {
                std::time::UNIX_EPOCH + magnitude
            } else {
                std::time::UNIX_EPOCH - magnitude
            };
            let file = open_for_times(&path).map_err(|e| fail_set(&e))?;
            file.set_modified(time).map_err(|e| fail_set(&e))
        }
    );
    fs_fn!("copy", ["from", "to"], |from: String, to: String| {
        std::fs::copy(&from, &to)
            .map(|_| ())
            .map_err(|e| fail("copy", &from, &e))
    });
    fs_fn!("rename", ["from", "to"], |from: String, to: String| {
        std::fs::rename(&from, &to).map_err(|e| fail("rename", &from, &e))
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::windows_script_path;

    #[test]
    fn windows_paths_reach_scripts_with_forward_slashes() {
        assert_eq!(
            windows_script_path(r"C:\Users\RUNNER~1\AppData\Local\Temp"),
            "C:/Users/RUNNER~1/AppData/Local/Temp"
        );
        // Mixed separators, as a path built from a folder and a name can have.
        assert_eq!(windows_script_path(r"C:\a/b\c"), "C:/a/b/c");
        // What canonicalising adds.
        assert_eq!(windows_script_path(r"\\?\C:\x\y"), "C:/x/y");
        assert_eq!(
            windows_script_path(r"\\?\UNC\server\share\x"),
            "//server/share/x"
        );
        assert_eq!(windows_script_path(r"\\server\share"), "//server/share");
        assert_eq!(windows_script_path("relative/already"), "relative/already");
    }
}
