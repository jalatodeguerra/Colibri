//! `colibri build --web`: write a folder that a static file server can serve, with the program
//! running in the page.
//!
//! The folder holds four files:
//!
//! - `index.html`: a page with one element for the program's output, that loads `colibri.js`;
//! - `colibri.js`: the loop around the runner (`crates/colibri-web/js/colibri.js`, embedded);
//! - `colibri.wasm`: the runner, `crates/colibri-web` built for `wasm32-unknown-unknown`;
//! - `app.pack`: the program as a pack with no runner in front (sources, dependencies, assets).
//!
//! The program is checked first as for any build, but for the web: `os` is `"web"` (so `@when(os
//! == "web")` picks the files), the standard library is the subset a page has, and an `import` of
//! a module a page does not have, `task.run`, and `task.tick` from `main` are compile errors.
//!
//! **The runner.** `colibri.wasm` is built by the release pipeline and compiled into `colibri`
//! from the file `COLIBRI_WEB_RUNNER` names when `colibri` is built (see `build.rs`), compressed:
//! raw it is 3.6 MB, which would take `colibri` near its 15 MB size gate, gzipped it is 1.1 MB.
//! A `colibri` built without it says so and builds nothing; `--web-runner FILE` names a runner
//! to use instead (a `colibri.wasm` just built from `crates/colibri-web`, or the pipeline's
//! gzip file).
//!
//! The files are written as `colibri build` writes its executable: each to a new file next to its
//! place, then moved there, the page last, so no file is ever half written (a running page that
//! reloads meanwhile gets a whole old file or a whole new one). Other files in the folder are
//! never touched.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use colibri_runtime::{self as colibri, RecordingFs};

use crate::buildcmd::{clobbers, create_temp, fail, make_pack, program_name};
use crate::depscmd::{self, DepFlags};
use crate::pack::{Pack, PackDisk};

/// The loop around the runner: what the page imports.
const COLIBRI_JS: &str = include_str!("../../colibri-web/js/colibri.js");

/// The runner, gzipped, compiled in by `build.rs` when `COLIBRI_WEB_RUNNER` named a file.
#[cfg(web_runner)]
const EMBEDDED_RUNNER: Option<&[u8]> = Some(include_bytes!(env!("COLIBRI_WEB_RUNNER_FILE")));
#[cfg(not(web_runner))]
const EMBEDDED_RUNNER: Option<&[u8]> = None;

/// What the page says of itself in a comment: a folder whose `index.html` has it is a web build's.
const PAGE_MARKER: &str = "written by colibri build --web";

/// The files of the folder, in the order they are moved into place (the page last).
pub const FILES: [&str; 4] = ["app.pack", "colibri.wasm", "colibri.js", "index.html"];

/// What `colibri build --web` was asked for.
pub struct Options<'a> {
    /// A script, or a package folder.
    pub path: &'a Path,
    /// The folder to write; by default `NAME-web` in the current folder.
    pub out: Option<&'a Path>,
    pub deps: DepFlags,
    /// A runner (wasm, or gzip of it) to use instead of the one inside this `colibri`.
    pub runner: Option<&'a Path>,
}

/// Writes the folder, printing problems to stderr.
pub fn build(options: &Options<'_>) -> ExitCode {
    match try_build(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

/// The page: the output element is `colibri-output`, which `runInPage` fills.
fn index_html(title: &str) -> String {
    format!(
        r#"<!doctype html>
<!-- {MARKER} -->
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<title>{}</title>
<style>
  body {{ margin: 1rem; font: 16px/1.4 system-ui, sans-serif; }}
  #colibri-output {{ margin: 0; font: 14px/1.4 ui-monospace, monospace; white-space: pre-wrap; overflow-wrap: anywhere; }}
</style>
</head>
<body>
<pre id="colibri-output"></pre>
<noscript>This program needs JavaScript.</noscript>
<script type="module">
  import {{ runInPage }} from "./colibri.js";
  runInPage({{ wasm: "colibri.wasm", pack: "app.pack" }});
</script>
</body>
</html>
"#,
        escape_html(title),
        MARKER = PAGE_MARKER
    )
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The bytes of `colibri.wasm`: the file `--web-runner` names, or the one inside this `colibri`;
/// either may be gzipped.
fn runner_wasm(given: Option<&Path>) -> Result<Vec<u8>, String> {
    let (bytes, what): (std::borrow::Cow<'_, [u8]>, String) = match given {
        Some(path) => {
            let bytes = fs::read(path)
                .map_err(|e| format!("cannot read the web runner `{}`: {e}", path.display()))?;
            (bytes.into(), format!("the web runner `{}`", path.display()))
        }
        None => {
            let Some(bytes) = EMBEDDED_RUNNER else {
                return Err("this colibri was built without the web runner; build `crates/colibri-web` first \
                     (`cargo build --profile web --target wasm32-unknown-unknown -p colibri-web`), \
                     then build colibri with `COLIBRI_WEB_RUNNER` set to the absolute path of the \
                     result (gzipped, `gzip -9n`), or pass `--web-runner FILE`"
                    .to_string());
            };
            (bytes.into(), "the web runner of this colibri".to_string())
        }
    };
    let wasm = if crate::inflate::is_gzip(&bytes) {
        crate::inflate::gunzip(&bytes).map_err(|e| format!("{what}: {e}"))?
    } else {
        bytes.into_owned()
    };
    if !wasm.starts_with(b"\0asm") {
        return Err(format!("{what} is not a WebAssembly module"));
    }
    Ok(wasm)
}

fn try_build(options: &Options<'_>) -> Result<(), ExitCode> {
    let path = options.path;
    // Before anything is checked: a build that cannot finish says so first.
    let wasm = runner_wasm(options.runner).map_err(fail)?;
    depscmd::prepare(path, options.deps)?;
    let recorder = RecordingFs::new(&colibri::DiskFs);
    crate::checkcmd::check_on(&recorder, path, &[], false, &mut colibri_std::web_interface)?;
    let reads = recorder.into_files();
    let out = match options.out {
        Some(out) => out.to_path_buf(),
        None => PathBuf::from(format!("{}-web", program_name(path))),
    };
    if out.exists() && !out.is_dir() {
        return Err(fail(format!(
            "the output `{}` exists and is not a folder: choose a folder with `-o`",
            out.display()
        )));
    }
    let pack = make_pack(path, reads, &out)?;
    refuse_to_overwrite_the_program(&out, &pack, path)?;
    // A page that was not written by this command is somebody's own: it is not replaced.
    let page = out.join("index.html");
    if page.is_file() && !fs::read_to_string(&page).is_ok_and(|t| t.contains(PAGE_MARKER)) {
        return Err(fail(format!(
            "`{}` exists and is not a page written by `colibri build --web`: choose another folder with `-o`, or move it away",
            page.display()
        )));
    }
    let html = index_html(&pack.name);
    let cannot = |e: std::io::Error| fail(format!("cannot write in `{}`: {e}", out.display()));
    fs::create_dir_all(&out).map_err(cannot)?;
    let written = write_files(&out, &pack, &wasm, &html).map_err(fail)?;
    let sources = pack
        .files
        .iter()
        .filter(|f| f.path.ends_with(".colibri"))
        .count();
    let assets = match pack.assets.len() {
        0 => String::new(),
        1 => ", 1 asset".to_string(),
        n => format!(", {n} assets"),
    };
    eprintln!(
        "built {} ({sources} source file{}{assets}; colibri.wasm {} bytes, app.pack {} bytes): \
         serve it with a static file server, a page opened from disk cannot load its files",
        out.display(),
        if sources == 1 { "" } else { "s" },
        wasm.len(),
        written
    );
    Ok(())
}

/// Fails if a file of the folder would be a file of the program: a source, a dependency or an
/// asset. (The folder may be the package's own: `index.html` is then written beside the sources.)
fn refuse_to_overwrite_the_program(out: &Path, pack: &Pack, entry: &Path) -> Result<(), ExitCode> {
    let root = crate::assets::package_root(entry);
    for name in FILES {
        let target = out.join(name);
        if let Some(source) = clobbers(&target, pack, entry) {
            return Err(fail(format!(
                "the output `{}` is the source file `{source}`: choose another folder with `-o`",
                target.display()
            )));
        }
        let Ok(target) = fs::canonicalize(&target) else {
            continue;
        };
        for asset in &pack.assets {
            if fs::canonicalize(root.join(&asset.path)).is_ok_and(|a| a == target) {
                return Err(fail(format!(
                    "the output `{}` is the asset `{}`: choose another folder with `-o`",
                    target.display(),
                    asset.path
                )));
            }
        }
    }
    Ok(())
}

/// Writes the four files, each to a new file in `out` first. Returns the size of the pack.
fn write_files(out: &Path, pack: &Pack, wasm: &[u8], html: &str) -> Result<u64, String> {
    let mut made: Vec<(PathBuf, PathBuf)> = Vec::new();
    let result = (|| {
        let mut pack_len = 0;
        for name in FILES {
            let target = out.join(name);
            let cannot = |e: std::io::Error| format!("cannot write `{}`: {e}", target.display());
            let (mut file, tmp) = create_temp(&target).map_err(cannot)?;
            made.push((tmp, target.clone()));
            match name {
                "app.pack" => {
                    let mut buffered = std::io::BufWriter::new(&mut file);
                    pack.write_to(&mut buffered, 0)?;
                    buffered.flush().map_err(cannot)?;
                }
                "colibri.wasm" => file.write_all(wasm).map_err(cannot)?,
                "colibri.js" => file.write_all(COLIBRI_JS.as_bytes()).map_err(cannot)?,
                _ => file.write_all(html.as_bytes()).map_err(cannot)?,
            }
            file.sync_all().map_err(cannot)?;
            if name == "app.pack" {
                pack_len = file.metadata().map_err(cannot)?.len();
            }
        }
        for (tmp, target) in &made {
            fs::rename(tmp, target)
                .map_err(|e| format!("cannot write `{}`: {e}", target.display()))?;
        }
        Ok(pack_len)
    })();
    // What was not moved into place is removed (a moved file's temporary name is gone).
    for (tmp, _) in &made {
        let _ = fs::remove_file(tmp);
    }
    result
}
