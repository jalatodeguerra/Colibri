//! `path`: text operations on paths. They never touch the file system, and they are the same
//! on every platform but for the Windows roots described below: `/` is the separator everywhere (Windows accepts it too). Paths the
//! standard library hands to a script (`os.temp_dir`, `os.cwd`, `fs.walk`) use `/` as well.
//! A path that comes from outside (a command-line argument, a file's content) may be spelled
//! the system's way; `path.from_native` turns it into the `/` form (on Unix it changes
//! nothing, since `\` is an ordinary character in a Unix name).
//!
//! Roots: `/` on every system. On Windows, and only there, `C:/` (a drive) and `//host/share/`
//! (a share, which `from_native` makes of `\\host\share\`) are roots as well: they are absolute,
//! `join` with such a second part gives that part, `normalize` keeps the root and never climbs
//! above it, `parent("C:/x")` is `C:/` and a root has no parent and no `file_name`, and `split`
//! starts with the root (`["C:/", "x"]`). A bare `C:` or `C:x` is relative to the drive's current
//! folder: not absolute, and `join("C:", "x")` is `C:x`. On Unix `C:` is a legal file name and
//! nothing of this applies. Each function is written with a `windows` flag so that the tests
//! cover both behaviours on every system; the module passes `cfg!(windows)`.

use colibri_runtime::{Error, Vm};

use crate::plain;

/// How many bytes at the start of `path` are its root (0 for a relative path). On every system
/// `/` is a root. On Windows, and only there, `C:/` (a drive letter, a colon and a slash) and
/// `//host/share` with an optional slash after it are roots too; a bare `C:` or `C:x` is
/// relative to the drive's current folder and has none, and `//host` without a share is just
/// `/` followed by a name. (On Unix `C:` is an ordinary file name.)
fn root_len(path: &str, windows: bool) -> usize {
    if windows {
        let b = path.as_bytes();
        if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/' {
            return 3;
        }
        if let Some(rest) = path.strip_prefix("//") {
            let mut parts = rest.splitn(3, '/');
            if let (Some(host), Some(share)) = (parts.next(), parts.next()) {
                if !host.is_empty() && !share.is_empty() {
                    let len = 2 + host.len() + 1 + share.len();
                    return if path[len..].starts_with('/') {
                        len + 1
                    } else {
                        len
                    };
                }
            }
        }
    }
    usize::from(path.starts_with('/'))
}

/// The root of `path` in its normal spelling: `/`, `C:/` or `//host/share/`.
fn root_of(path: &str, windows: bool) -> String {
    let mut root = path[..root_len(path, windows)].to_string();
    if !root.ends_with('/') && !root.is_empty() {
        root.push('/');
    }
    root
}

fn is_absolute(path: &str, windows: bool) -> bool {
    root_len(path, windows) > 0
}

/// A bare drive such as `C:` (Windows only): the folder the drive is currently in.
fn is_bare_drive(path: &str, windows: bool) -> bool {
    let b = path.as_bytes();
    windows && b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// The text after the last `/`, ignoring trailing slashes; `None` for `""`, a root (`/`, and on
/// Windows `C:/` and `//host/share`) and `".."`.
fn file_name(path: &str, windows: bool) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.len() <= root_len(path, windows) {
        return None;
    }
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    (!name.is_empty() && name != "..").then_some(name)
}

/// Splits a name at its last dot: `("a", Some("txt"))`. A leading dot (`.gitignore`) is part of
/// the stem.
fn split_ext(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], Some(&name[i + 1..])),
        _ => (name, None),
    }
}

/// `b` when it is absolute (a root of its own), else `b` below `a`.
fn join(a: &str, b: &str, windows: bool) -> String {
    if is_absolute(b, windows) || a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else if a.ends_with('/') || is_bare_drive(a, windows) {
        // `C:` and `x` is `C:x`, the drive-relative path, not `C:/x`.
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// Resolves `.` and `..` and repeated slashes without looking at the disk. `a/../b` is `b`; a
/// `..` that has nothing to remove stays for a relative path and disappears for an absolute one
/// (a root, whichever it is, is never climbed above).
fn normalize(path: &str, windows: bool) -> String {
    let root_len = root_len(path, windows);
    let mut parts: Vec<&str> = Vec::new();
    for part in path[root_len..].split('/') {
        match part {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ if root_len > 0 => {}
                _ => parts.push(".."),
            },
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    match (root_len > 0, joined.is_empty()) {
        (true, _) => format!("{}{joined}", root_of(path, windows)),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

/// Everything before the last name; `None` for a path that has no parent (`""`, a root). The
/// parent of `C:/x` is the root `C:/`.
fn parent(path: &str, windows: bool) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    let root_len = root_len(path, windows);
    if trimmed.len() <= root_len {
        return None;
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(i) => {
            let dir = trimmed[..i].trim_end_matches('/');
            // Only with a root of more than one byte can trimming the slashes cut into it.
            if windows && dir.len() < root_len {
                Some(root_of(path, windows))
            } else {
                Some(dir.to_string())
            }
        }
        None => Some(String::new()),
    }
}

/// The root (on Windows a drive or share root, as `C:/` or `//host/share/`, but not a plain
/// `/`) and then the names, without empty pieces. The root is kept there because without it
/// `C:/x` and `D:/x` would be told apart by nothing.
fn split(path: &str, windows: bool) -> Vec<String> {
    let root_len = root_len(path, windows);
    let mut parts = Vec::new();
    if root_len > 1 {
        parts.push(root_of(path, windows));
    }
    parts.extend(
        path[root_len..]
            .split('/')
            .filter(|p| !p.is_empty())
            .map(str::to_string),
    );
    parts
}

fn with_ext(path: String, ext: &str, windows: bool) -> String {
    let Some(name) = file_name(&path, windows) else {
        return path;
    };
    let trimmed = path.trim_end_matches('/');
    let dir = &trimmed[..trimmed.len() - name.len()];
    let ext = ext.trim_start_matches('.');
    let stem = split_ext(name).0;
    if ext.is_empty() {
        format!("{dir}{stem}")
    } else {
        format!("{dir}{stem}.{ext}")
    }
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("path");
    let windows = cfg!(windows);
    plain(
        m.register_fn_named("join", ["a", "b"], move |a: String, b: String| {
            join(&a, &b, windows)
        }),
    )?;
    plain(
        m.register_fn_named("file_name", ["path"], move |path: String| {
            file_name(&path, windows).map(str::to_string)
        }),
    )?;
    plain(m.register_fn_named("stem", ["path"], move |path: String| {
        file_name(&path, windows).map(|n| split_ext(n).0.to_string())
    }))?;
    // The extension without its dot.
    plain(m.register_fn_named("ext", ["path"], move |path: String| {
        file_name(&path, windows)
            .and_then(|n| split_ext(n).1)
            .map(str::to_string)
    }))?;
    plain(m.register_fn_named(
        "with_ext",
        ["path", "ext"],
        move |path: String, ext: String| with_ext(path, &ext, windows),
    ))?;
    // Everything before the last name; `none` for a path that has no parent (`""`, a root).
    plain(
        m.register_fn_named("parent", ["path"], move |path: String| {
            parent(&path, windows)
        }),
    )?;
    plain(
        m.register_fn_named("is_absolute", ["path"], move |path: String| {
            is_absolute(&path, windows)
        }),
    )?;
    // A path as the system spells it, in the form the other functions understand: on Windows
    // `C:\data\a.json` becomes `C:/data/a.json` (and `\\?\C:\x` becomes `C:/x`), elsewhere
    // the text is unchanged.
    plain(
        m.register_fn_named("from_native", ["path"], |path: String| {
            if cfg!(windows) {
                crate::fs::windows_script_path(&path)
            } else {
                path
            }
        }),
    )?;
    plain(
        m.register_fn_named("normalize", ["path"], move |path: String| {
            normalize(&path, windows)
        }),
    )?;
    plain(m.register_fn_named("split", ["path"], move |path: String| split(&path, windows)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIX: bool = false;
    const WIN: bool = true;

    #[test]
    fn joins_and_normalizes() {
        for w in [UNIX, WIN] {
            assert_eq!(join("a", "b", w), "a/b");
            assert_eq!(join("a/", "b", w), "a/b");
            assert_eq!(join("a", "/b", w), "/b");
            assert_eq!(join("", "b", w), "b");
            assert_eq!(join("a", "", w), "a");
            assert_eq!(normalize("a/./b/../c//d", w), "a/c/d");
            assert_eq!(normalize("../a", w), "../a");
            assert_eq!(normalize("/..", w), "/");
            assert_eq!(normalize("a/..", w), ".");
        }
    }

    #[test]
    fn names_and_extensions() {
        for w in [UNIX, WIN] {
            assert_eq!(file_name("a/b.txt", w), Some("b.txt"));
            assert_eq!(file_name("a/b/", w), Some("b"));
            assert_eq!(file_name("/", w), None);
            assert_eq!(file_name("", w), None);
            assert_eq!(file_name("a/..", w), None);
        }
        assert_eq!(split_ext("b.tar.gz"), ("b.tar", Some("gz")));
        assert_eq!(split_ext(".gitignore"), (".gitignore", None));
        assert_eq!(split_ext("b."), ("b.", None));
    }

    #[test]
    fn a_drive_or_share_is_a_root_only_on_windows() {
        for p in [
            "C:/",
            "C:/Users/me",
            "c:/x",
            "//server/share",
            "//server/share/f",
        ] {
            assert!(is_absolute(p, WIN), "{p}");
        }
        // Drive-relative, not rooted; and not even that on Unix.
        for p in [
            "C:",
            "C:x",
            "C:x/y",
            "1:/x",
            "//server",
            "//server/",
            "///x/y",
        ] {
            assert_eq!(is_absolute(p, WIN), p.starts_with('/'), "{p}");
        }
        for p in ["C:/", "C:/Users/me", "C:", "x"] {
            assert!(!is_absolute(p, UNIX), "{p}");
        }
        assert!(is_absolute("//server/share", UNIX));
    }

    #[test]
    fn join_with_an_absolute_second_part_gives_it() {
        assert_eq!(join("C:/a", "D:/b", WIN), "D:/b");
        assert_eq!(join("C:/a", "//h/s/f", WIN), "//h/s/f");
        assert_eq!(join("C:/a", "/b", WIN), "/b");
        assert_eq!(join("C:/", "b", WIN), "C:/b");
        assert_eq!(join("//h/s", "b", WIN), "//h/s/b");
        // A bare drive stays drive-relative.
        assert_eq!(join("C:", "x", WIN), "C:x");
        assert_eq!(join("C:x", "y", WIN), "C:x/y");
        // On Unix `C:/b` is an ordinary relative path.
        assert_eq!(join("C:/a", "D:/b", UNIX), "C:/a/D:/b");
        assert_eq!(join("C:", "x", UNIX), "C:/x");
    }

    #[test]
    fn normalize_keeps_the_root_and_stays_below_it() {
        assert_eq!(normalize("C:/../x", WIN), "C:/x");
        assert_eq!(normalize("C:/a/../../x", WIN), "C:/x");
        assert_eq!(normalize("C://a/./b/", WIN), "C:/a/b");
        assert_eq!(normalize("C:/", WIN), "C:/");
        assert_eq!(normalize("C:/..", WIN), "C:/");
        assert_eq!(normalize("//server/share/f", WIN), "//server/share/f");
        assert_eq!(normalize("//server/share", WIN), "//server/share/");
        assert_eq!(normalize("//server/share/../../x", WIN), "//server/share/x");
        assert_eq!(normalize("/a/..", WIN), "/");
        // Drive-relative paths are relative.
        assert_eq!(normalize("C:x/../y", WIN), "y");
        assert_eq!(normalize("C:", WIN), "C:");
        // Unix: `C:` is a name and `//` is one slash.
        assert_eq!(normalize("C:/../x", UNIX), "x");
        assert_eq!(normalize("//server/share/f", UNIX), "/server/share/f");
        assert_eq!(normalize("C:/", UNIX), "C:");
    }

    #[test]
    fn parents_and_names_of_roots() {
        assert_eq!(parent("C:/x", WIN).as_deref(), Some("C:/"));
        assert_eq!(parent("C:/x/", WIN).as_deref(), Some("C:/"));
        assert_eq!(parent("C:/x/y", WIN).as_deref(), Some("C:/x"));
        assert_eq!(parent("C:///y", WIN).as_deref(), Some("C:/"));
        assert_eq!(parent("C:/", WIN), None);
        assert_eq!(parent("/", WIN), None);
        assert_eq!(parent("//h/s/f", WIN).as_deref(), Some("//h/s/"));
        assert_eq!(parent("//h/s/f/g", WIN).as_deref(), Some("//h/s/f"));
        assert_eq!(parent("//h/s/", WIN), None);
        assert_eq!(parent("//h/s", WIN), None);
        assert_eq!(parent("C:x", WIN).as_deref(), Some(""));
        assert_eq!(parent("/x", WIN).as_deref(), Some("/"));
        assert_eq!(file_name("C:/", WIN), None);
        assert_eq!(file_name("//h/s", WIN), None);
        assert_eq!(file_name("//h/s/", WIN), None);
        assert_eq!(file_name("//h/s/f.txt", WIN), Some("f.txt"));
        assert_eq!(file_name("C:/x/", WIN), Some("x"));
        assert_eq!(file_name("C:x", WIN), Some("C:x"));
        // Unix, unchanged.
        assert_eq!(parent("C:/x", UNIX).as_deref(), Some("C:"));
        assert_eq!(parent("C:", UNIX).as_deref(), Some(""));
        assert_eq!(parent("/x", UNIX).as_deref(), Some("/"));
        assert_eq!(parent("/", UNIX), None);
        assert_eq!(parent("//x", UNIX).as_deref(), Some(""));
        assert_eq!(file_name("C:/", UNIX), Some("C:"));
    }

    #[test]
    fn split_starts_with_a_windows_root() {
        assert_eq!(split("C:/a/b", WIN), ["C:/", "a", "b"]);
        assert_eq!(split("C:/", WIN), ["C:/"]);
        assert_eq!(split("//h/s/a//b/", WIN), ["//h/s/", "a", "b"]);
        assert_eq!(split("//h/s", WIN), ["//h/s/"]);
        assert_eq!(split("/a/b", WIN), ["a", "b"]);
        assert_eq!(split("C:x/b", WIN), ["C:x", "b"]);
        assert_eq!(split("C:/a/b", UNIX), ["C:", "a", "b"]);
        assert_eq!(split("//h/s/a", UNIX), ["h", "s", "a"]);
    }

    #[test]
    fn with_ext_keeps_the_root() {
        assert_eq!(with_ext("C:/a/b.txt".into(), "md", WIN), "C:/a/b.md");
        assert_eq!(with_ext("C:/".into(), "md", WIN), "C:/");
        assert_eq!(with_ext("//h/s".into(), "md", WIN), "//h/s");
        assert_eq!(with_ext("//h/s/f".into(), ".md", WIN), "//h/s/f.md");
        // Unix: `C:` is the name (the old behaviour: the trailing slash goes).
        assert_eq!(with_ext("C:/".into(), "md", UNIX), "C:.md");
    }
}
