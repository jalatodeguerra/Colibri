//! `path`: text operations on paths. They never touch the file system, and they are the same
//! on every platform: `/` is the separator everywhere (Windows accepts it too).

use colibri_runtime::{Error, Vm};

use crate::plain;

/// The text after the last `/`, ignoring trailing slashes; `None` for `""`, `"/"` and `".."`.
fn file_name(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
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

fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') || a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// Resolves `.` and `..` and repeated slashes without looking at the disk. `a/../b` is `b`; a
/// `..` that has nothing to remove stays for a relative path and disappears for an absolute one.
fn normalize(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ if absolute => {}
                _ => parts.push(".."),
            },
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("path");
    plain(m.register_fn_named("join", ["a", "b"], |a: String, b: String| join(&a, &b)))?;
    plain(m.register_fn_named("file_name", ["path"], |path: String| {
        file_name(&path).map(str::to_string)
    }))?;
    plain(m.register_fn_named("stem", ["path"], |path: String| {
        file_name(&path).map(|n| split_ext(n).0.to_string())
    }))?;
    // The extension without its dot.
    plain(m.register_fn_named("ext", ["path"], |path: String| {
        file_name(&path)
            .and_then(|n| split_ext(n).1)
            .map(str::to_string)
    }))?;
    plain(
        m.register_fn_named("with_ext", ["path", "ext"], |path: String, ext: String| {
            let Some(name) = file_name(&path) else {
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
        }),
    )?;
    // Everything before the last name; `none` for a path that has no parent (`""`, `"/"`).
    plain(m.register_fn_named("parent", ["path"], |path: String| {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        match trimmed.rfind('/') {
            Some(0) => Some("/".to_string()),
            Some(i) => Some(trimmed[..i].trim_end_matches('/').to_string()),
            None => Some(String::new()),
        }
    }))?;
    plain(
        m.register_fn_named("is_absolute", ["path"], |path: String| {
            path.starts_with('/')
        }),
    )?;
    plain(m.register_fn_named("normalize", ["path"], |path: String| normalize(&path)))?;
    plain(m.register_fn_named("split", ["path"], |path: String| {
        path.split('/')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    }))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_and_normalizes() {
        assert_eq!(join("a", "b"), "a/b");
        assert_eq!(join("a/", "b"), "a/b");
        assert_eq!(join("a", "/b"), "/b");
        assert_eq!(join("", "b"), "b");
        assert_eq!(normalize("a/./b/../c//d"), "a/c/d");
        assert_eq!(normalize("../a"), "../a");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("a/.."), ".");
    }

    #[test]
    fn names_and_extensions() {
        assert_eq!(file_name("a/b.txt"), Some("b.txt"));
        assert_eq!(file_name("a/b/"), Some("b"));
        assert_eq!(file_name("/"), None);
        assert_eq!(split_ext("b.tar.gz"), ("b.tar", Some("gz")));
        assert_eq!(split_ext(".gitignore"), (".gitignore", None));
        assert_eq!(split_ext("b."), ("b.", None));
    }
}
