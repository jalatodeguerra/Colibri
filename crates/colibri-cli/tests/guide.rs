//! The user guide in `guide/`: every ```colibri sample in it is checked, formatted and, when it
//! has an expected output right after it, run; and the standard library reference in
//! `guide/std` must be what `colibri doc --std` writes.
//!
//! The fence of a sample says what to do with it, as words after `colibri`, separated by commas:
//!
//! - ```` ```colibri ````: the sample must pass `colibri check` and `colibri fmt --check`.
//! - ```` ```output ```` right after it: the sample is run, and its standard output must be this.
//! - `no-run`: the sample is checked but never run (it serves for ever, or reads a terminal).
//! - `test`: the sample is run with `colibri test`, and the `output` block is what that prints.
//! - `error`: the sample must fail `colibri check`, and each line of the `output` block after it
//!   must be part of the diagnostics.
//! - `pkg=NAME` and `file=PATH`: the sample is a file of a package that all the blocks of this
//!   page with the same `pkg=NAME` make together (a ```` ```toml ```` block can be `colibri.toml`).
//!   The package is checked as a whole, and an `output` block after one of its blocks is what
//!   `colibri run` on the package prints.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// How long one `colibri` process may take before the test kills it.
const LIMIT: Duration = Duration::from_secs(120);

fn guide() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guide")
}

/// One fenced block of a page.
#[derive(Debug, PartialEq)]
struct Block {
    /// The page, relative to `guide/`.
    page: String,
    /// The line of the opening fence, from 1.
    line: usize,
    /// The first word of the fence: `colibri`, `output`, `toml`, `console`, ...
    lang: String,
    /// The other words.
    words: Vec<String>,
    code: String,
}

impl Block {
    fn has(&self, word: &str) -> bool {
        self.words.iter().any(|w| w == word)
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.words
            .iter()
            .find_map(|w| w.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
    }

    fn place(&self) -> String {
        format!("{}:{}", self.page, self.line)
    }
}

/// The fenced blocks of a Markdown text, in order. Only fences that start a line count.
fn blocks_of(page: &str, text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    // The line, the language, the other words, the code and the indentation of the fence: a
    // fence inside a list item is indented, and is a sample all the same.
    let mut open: Option<(usize, String, Vec<String>, String, usize)> = None;
    for (i, line) in text.replace("\r\n", "\n").lines().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        match &mut open {
            None => {
                if let Some(info) = trimmed.strip_prefix("```") {
                    let mut words = info
                        .split([',', ' '])
                        .filter(|w| !w.is_empty())
                        .map(str::to_string);
                    let lang = words.next().unwrap_or_default();
                    open = Some((i + 1, lang, words.collect(), String::new(), indent));
                }
            }
            Some((start, lang, words, code, fence_indent)) => {
                if trimmed.trim_end() == "```" {
                    out.push(Block {
                        page: page.to_string(),
                        line: *start,
                        lang: std::mem::take(lang),
                        words: std::mem::take(words),
                        code: std::mem::take(code),
                    });
                    open = None;
                } else {
                    let strip = line.len() - line.trim_start_matches(' ').len();
                    code.push_str(&line[strip.min(*fence_indent)..]);
                    code.push('\n');
                }
            }
        }
    }
    assert!(open.is_none(), "{page}: a fence is never closed");
    out
}

/// The languages a fence may name. Anything else (a misspelt `colibri`, a fence with no
/// language) is a mistake that would let a sample escape the checks.
const LANGUAGES: &[&str] = &["colibri", "output", "toml", "sh", "console", "text"];

/// What is wrong with the fences of a page, as messages with their places.
fn fence_problems(page: &str, blocks: &[Block]) -> Vec<String> {
    let mut problems = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        let place = b.place();
        if !LANGUAGES.contains(&b.lang.as_str()) {
            problems.push(format!(
                "{place}: the fence language `{}` is not one of {LANGUAGES:?}",
                b.lang
            ));
            continue;
        }
        for w in &b.words {
            let known = match b.lang.as_str() {
                "colibri" => ["no-run", "test", "error"].contains(&w.as_str()),
                _ => false,
            } || w.starts_with("pkg=") && (b.lang == "colibri" || b.lang == "toml")
                || w.starts_with("file=") && (b.lang == "colibri" || b.lang == "toml");
            if !known {
                problems.push(format!("{place}: the fence word `{w}` means nothing here"));
            }
        }
        if b.value("pkg").is_some() != b.value("file").is_some() {
            problems.push(format!("{place}: `pkg=` and `file=` go together"));
        }
        if b.lang == "output" {
            let before = i.checked_sub(1).map(|j| &blocks[j]);
            match before {
                Some(p) if p.lang == "colibri" && !p.has("no-run") => {}
                Some(p) if p.lang == "colibri" => problems.push(format!(
                    "{place}: an `output` block after a `no-run` sample is never compared"
                )),
                _ => problems.push(format!(
                    "{place}: an `output` block must follow a colibri sample"
                )),
            }
        }
    }
    let _ = page;
    problems
}

/// The Markdown pages of `guide/` itself (the reference in `guide/std` is generated and has
/// its own test).
fn pages() -> Vec<(String, String)> {
    let mut pages = Vec::new();
    for entry in std::fs::read_dir(guide()).expect("guide/ exists") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "md") {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let text = std::fs::read_to_string(&path).unwrap();
            pages.push((name, text));
        }
    }
    pages.sort();
    pages
}

/// A child process that is killed when it goes out of scope, so a test that panics or times
/// out leaves nothing running.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs `colibri args` in `dir` with no stdin, and gives up on it after [`LIMIT`].
fn colibri(dir: &Path, args: &[&str]) -> Ran {
    let child = Command::new(env!("CARGO_BIN_EXE_colibri"))
        .args(args)
        .current_dir(dir)
        // The `http` client follows the proxy variables and does not exempt loopback, so a
        // proxy in the environment would take the guide's loopback requests away from the
        // machine. The samples must not depend on whose machine they run on.
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("NO_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .env_remove("no_proxy")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the colibri binary runs");
    let mut guard = Guard(child);
    let mut out = guard.0.stdout.take().unwrap();
    let mut err = guard.0.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out.read_to_end(&mut s);
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = err.read_to_end(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = guard.0.try_wait().unwrap() {
            break Some(status);
        }
        if start.elapsed() > LIMIT {
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if status.is_none() {
        // The pipes close when the child dies, which lets the readers finish.
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
    Ran {
        code: status.map(|s| s.code().unwrap_or(-1)),
        stdout: String::from_utf8_lossy(&out.join().unwrap()).replace("\r\n", "\n"),
        stderr: String::from_utf8_lossy(&err.join().unwrap()).replace("\r\n", "\n"),
    }
}

/// A fresh folder for the samples of one test, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new(test: &str) -> Scratch {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("colibri-guide-{}-{test}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What to do with one sample after its files are written.
struct Job {
    block_place: String,
    /// The file or folder that `check`, `fmt` and `run` take.
    target: PathBuf,
    /// The folder to run in.
    cwd: PathBuf,
    no_run: bool,
    test: bool,
    error: bool,
    /// The `output` block that follows, if there is one.
    expected: Option<String>,
}

/// Writes the samples of one page and says what to do with each. A package's blocks are written
/// first, so each package is one job, made when its last block is met.
fn plan(root: &Path, page: &str, blocks: &[Block]) -> Vec<Job> {
    let mut jobs = Vec::new();
    let mut packages: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (i, b) in blocks.iter().enumerate() {
        let next = blocks.get(i + 1).filter(|n| n.lang == "output");
        let sample = b.lang == "colibri";
        let in_package = b.value("pkg");
        if !sample && !(in_package.is_some() && b.value("file").is_some()) {
            continue;
        }
        let stem = page.trim_end_matches(".md");
        if let Some(pkg) = in_package {
            let dir = packages
                .entry(pkg.to_string())
                .or_insert_with(|| root.join(format!("{stem}-pkg-{pkg}")))
                .clone();
            let file = b
                .value("file")
                .unwrap_or_else(|| panic!("{}: a pkg block needs file=PATH", b.place()));
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &b.code).unwrap();
            if sample {
                // Each package block is checked with the package; an output block after it
                // is what the package prints.
                jobs.push(Job {
                    block_place: b.place(),
                    target: dir.clone(),
                    cwd: dir,
                    no_run: b.has("no-run") || next.is_none(),
                    test: b.has("test"),
                    error: b.has("error"),
                    expected: next.map(|n| n.code.clone()),
                });
            }
            continue;
        }
        let dir = root.join(format!("{stem}-{}", b.line));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.colibri");
        std::fs::write(&path, &b.code).unwrap();
        jobs.push(Job {
            block_place: b.place(),
            target: path,
            cwd: dir,
            no_run: b.has("no-run") || next.is_none(),
            test: b.has("test"),
            error: b.has("error"),
            expected: next.map(|n| n.code.clone()),
        });
    }
    jobs
}

fn s(p: &Path) -> &str {
    p.to_str().expect("a temporary path is text")
}

/// Every problem found, with the page and line of the sample it is about.
fn check_all(test: &str, run: bool) -> Vec<String> {
    let scratch = Scratch::new(test);
    let mut problems = Vec::new();
    let mut count = 0;
    for (page, text) in pages() {
        let blocks = blocks_of(&page, &text);
        for job in plan(&scratch.0, &page, &blocks) {
            count += 1;
            let place = &job.block_place;
            let target = s(&job.target);
            if !run {
                let fmt = colibri(&job.cwd, &["fmt", "--check", target]);
                if fmt.code != Some(0) {
                    problems.push(format!(
                        "{place}: `colibri fmt --check` says\n{}{}",
                        fmt.stdout, fmt.stderr
                    ));
                }
                let check = colibri(&job.cwd, &["check", target]);
                if job.error {
                    if check.code != Some(1) {
                        problems.push(format!(
                            "{place}: expected a compile error, got {:?}",
                            check.code
                        ));
                    }
                    for line in job.expected.iter().flat_map(|e| e.lines()) {
                        if !check.stderr.contains(line) {
                            problems.push(format!(
                                "{place}: the diagnostics lack `{line}`:\n{}",
                                check.stderr
                            ));
                        }
                    }
                } else if check.code != Some(0) || !check.stderr.is_empty() {
                    problems.push(format!("{place}: `colibri check` says\n{}", check.stderr));
                }
                continue;
            }
            if job.no_run || job.error {
                continue;
            }
            let args: Vec<&str> = if job.test {
                vec!["test", target]
            } else {
                vec!["run", target]
            };
            let ran = colibri(&job.cwd, &args);
            let want = job.expected.as_deref().unwrap_or("");
            if !ran.stderr.is_empty() {
                problems.push(format!("{place}: it wrote to stderr:\n{}", ran.stderr));
            }
            if ran.code != Some(0) {
                problems.push(format!(
                    "{place}: `colibri {}` ended with {:?}\n{}{}",
                    args[0], ran.code, ran.stdout, ran.stderr
                ));
            } else if ran.stdout != want {
                problems.push(format!(
                    "{place}: the output is not what the guide says.\n--- guide\n{want}--- got\n{}",
                    ran.stdout
                ));
            }
        }
    }
    assert!(count > 0, "no samples found in guide/");
    problems
}

#[test]
fn guide_samples_check_and_are_formatted() {
    let problems = check_all("check", false);
    assert!(problems.is_empty(), "\n{}", problems.join("\n\n"));
}

#[test]
fn guide_samples_print_the_output_the_guide_shows() {
    let problems = check_all("run", true);
    assert!(problems.is_empty(), "\n{}", problems.join("\n\n"));
}

#[test]
fn the_guide_has_its_pages_and_samples() {
    let pages = pages();
    let names: Vec<&str> = pages.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"tutorial.md"), "{names:?}");
    assert!(names.contains(&"tour.md"), "{names:?}");
    assert!(names.contains(&"builtins.md"), "{names:?}");
    for (name, text) in &pages {
        let blocks = blocks_of(name, text);
        let samples = blocks.iter().filter(|b| b.lang == "colibri").count();
        if name != "README.md" {
            assert!(samples >= 10, "{name} has only {samples} colibri samples");
        }
        let problems = fence_problems(name, &blocks);
        assert!(problems.is_empty(), "\n{}", problems.join("\n"));
    }
}

#[test]
fn the_fence_words_are_read() {
    let md = "intro\n```colibri,no-run\nfn main() {}\n```\n\n```output\nhi\n```\n```toml,pkg=a,file=colibri.toml\n[package]\n```\n";
    let blocks = blocks_of("x.md", md);
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0].lang, "colibri");
    assert!(blocks[0].has("no-run"));
    assert_eq!(blocks[0].line, 2);
    assert_eq!(blocks[0].code, "fn main() {}\n");
    assert_eq!(blocks[1].lang, "output");
    assert_eq!(blocks[2].value("pkg"), Some("a"));
    assert_eq!(blocks[2].value("file"), Some("colibri.toml"));
}

/// The reference in `guide/std` is what `colibri doc --std` writes: a signature that changes, a
/// module that is added or a page that is edited by hand fails here until the pages are written
/// again with `colibri doc --std guide/std`.
#[test]
fn the_std_reference_is_what_the_generator_writes() {
    let dir = guide().join("std");
    let pages = colibri_lang::doccmd::std_pages();
    let mut stale = Vec::new();
    for (name, text) in &pages {
        let found = std::fs::read_to_string(dir.join(name))
            .map(|t| t.replace("\r\n", "\n"))
            .ok();
        if found.as_deref() != Some(text.as_str()) {
            stale.push(name.clone());
        }
    }
    stale.extend(colibri_lang::doccmd::extra_files(&dir, &pages));
    assert!(
        stale.is_empty(),
        "guide/std is out of date ({}): run `colibri doc --std guide/std`",
        stale.join(", ")
    );
}

/// The command itself: it writes the same pages, and `--check` tells a current folder from a
/// stale one.
#[test]
fn colibri_doc_writes_and_checks_the_reference() {
    let scratch = Scratch::new("doc");
    let dir = scratch.0.join("std");
    let check = |expect: i32| {
        let ran = colibri(&scratch.0, &["doc", "--std", s(&dir), "--check"]);
        assert_eq!(ran.code, Some(expect), "{}", ran.stderr);
        ran
    };
    check(1);
    let ran = colibri(&scratch.0, &["doc", "--std", s(&dir)]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    check(0);
    for (name, text) in colibri_lang::doccmd::std_pages() {
        assert_eq!(
            std::fs::read_to_string(dir.join(&name)).unwrap(),
            text,
            "{name}"
        );
    }
    std::fs::write(dir.join("fs.md"), "edited by hand\n").unwrap();
    let ran = check(1);
    assert!(ran.stderr.contains("fs.md"), "{}", ran.stderr);
}

#[test]
fn the_fence_rules_catch_what_would_escape_the_checks() {
    let check = |md: &str| fence_problems("x.md", &blocks_of("x.md", md));
    assert!(
        check(
            "```colibri
fn main() {}
```
"
        )
        .is_empty()
    );
    // An indented fence in a list item is a sample, and is read.
    let blocks = blocks_of(
        "x.md",
        "- item

  ```colibri
  let x = 1
  ```
",
    );
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].code, "let x = 1\n");
    // A misspelt language, a fence with no language, an unknown word.
    assert_eq!(
        check(
            "```Colibri
let x = 1
```
"
        )
        .len(),
        1
    );
    assert_eq!(
        check(
            "```colibri-
let x = 1
```
"
        )
        .len(),
        1
    );
    assert_eq!(
        check(
            "```
let x = 1
```
"
        )
        .len(),
        1
    );
    assert_eq!(
        check(
            "```colibri,norun
let x = 1
```
"
        )
        .len(),
        1
    );
    assert_eq!(
        check(
            "```colibri,pkg=a
let x = 1
```
"
        )
        .len(),
        1
    );
    // An output after a `no-run` sample, or after nothing.
    assert_eq!(
        check(
            "```colibri,no-run
let x = 1
```
```output
1
```
"
        )
        .len(),
        1
    );
    assert_eq!(
        check(
            "```output
1
```
"
        )
        .len(),
        1
    );
    assert!(
        check(
            "```colibri,error
let x = 1
```
```output
m
```
"
        )
        .is_empty()
    );
}

/// `colibri doc --std` writes only files it generated: a folder with a hand-written page is
/// refused whole, and nothing in it changes.
#[test]
fn colibri_doc_refuses_a_folder_with_pages_it_did_not_write() {
    let scratch = Scratch::new("doc-refuse");
    let dir = scratch.0.join("guide");
    std::fs::create_dir_all(&dir).unwrap();
    let mine = dir.join("tutorial.md");
    std::fs::write(&mine, "# my tutorial\n").unwrap();
    let readme = dir.join("README.md");
    std::fs::write(&readme, "# my readme\n").unwrap();
    let ran = colibri(&scratch.0, &["doc", "--std", s(&dir)]);
    assert_eq!(ran.code, Some(1), "{}", ran.stderr);
    assert!(ran.stderr.contains("tutorial.md"), "{}", ran.stderr);
    assert!(ran.stderr.contains("README.md"), "{}", ran.stderr);
    assert_eq!(std::fs::read_to_string(&mine).unwrap(), "# my tutorial\n");
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "# my readme\n");
    // Nothing was created beside them either.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    // A marked page that is no longer one of the modules is removed, an unmarked one is not
    // touched, and a dotfile is left alone.
    let empty = scratch.0.join("std");
    let ran = colibri(&scratch.0, &["doc", "--std", s(&empty)]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    std::fs::write(empty.join(".DS_Store"), "x").unwrap();
    let old = std::fs::read_to_string(empty.join("fs.md")).unwrap();
    std::fs::write(empty.join("gone.md"), old).unwrap();
    let ran = colibri(&scratch.0, &["doc", "--std", s(&empty)]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert!(!empty.join("gone.md").exists());
    assert!(empty.join(".DS_Store").exists());
}
