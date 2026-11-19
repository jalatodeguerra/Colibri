//! The phase 2 milestone: the automation scripts in `examples/scripts`, run through the real
//! `colibri` binary. The organiser works on a temporary tree; the report generator talks to a
//! hand-written HTTP server on 127.0.0.1 that serves the fixtures (nothing leaves the machine).

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, SystemTime};

fn scripts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/scripts")
}

fn colibri(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_colibri"))
        .args(args)
        .output()
        .expect("the colibri binary runs")
}

/// `colibri run <script> -- <args...>`.
fn run_script(script: &Path, args: &[&str]) -> Output {
    let mut all = vec!["run", script.to_str().unwrap(), "--"];
    all.extend_from_slice(args);
    colibri(&all)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A fresh, empty temporary folder for one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-scripts-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn both_scripts_check() {
    for script in ["organize/organize.colibri", "report/report.colibri"] {
        let path = scripts().join(script);
        let out = colibri(&["check", path.to_str().unwrap()]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    }
}

// ---------------------------------------------------------------- organize

fn organize() -> PathBuf {
    scripts().join("organize/organize.colibri")
}

fn touch(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Every file and folder under `root`, as sorted relative paths (folders end with `/`).
fn tree(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.push(format!("{rel}/"));
                walk(&path, root, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// The plan is in name order (`fs.list` sorts by bytes: `README` before `SHOT.PNG` before
/// `backup.tar.gz`), and `old/` comes before the top-level `photo.jpg`.
const EXPECTED_DRY_RUN: &str = "\
README -> other/README
SHOT.PNG -> images/SHOT.PNG
backup.tar.gz -> archives/backup.tar.gz
main.rs -> code/main.rs
notes.txt -> documents/notes.txt
old/photo (1).jpg -> images/photo (1).jpg
old/photo.jpg -> images/photo.jpg
paper.pdf -> documents/paper.pdf
photo.jpg -> images/photo (2).jpg
weird.xyz -> other/weird.xyz
dry run: nothing was moved
folder         files     bytes
archives           1         7
code               1        12
documents          2        13
images             4        19
other              2        13
total             10        64
";

fn sample_tree(name: &str) -> PathBuf {
    let root = temp_dir(name);
    touch(&root, "photo.jpg", "jpeg");
    touch(&root, "SHOT.PNG", "png!");
    touch(&root, "notes.txt", "some notes");
    touch(&root, "paper.pdf", "pdf");
    touch(&root, "backup.tar.gz", "archive");
    touch(&root, "main.rs", "fn main() {}");
    touch(&root, "README", "no extension");
    touch(&root, "weird.xyz", "?");
    touch(&root, ".hidden.jpg", "left alone");
    // A file in a subfolder with the same name as one at the top: a collision.
    touch(&root, "old/photo.jpg", "older");
    touch(&root, "old/photo (1).jpg", "oldest");
    root
}

#[test]
fn organize_dry_run_prints_the_plan_and_changes_nothing() {
    let root = sample_tree("dry");
    let before = tree(&root);
    let out = run_script(&organize(), &[root.to_str().unwrap(), "--dry-run"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let got = text(&out.stdout);
    assert_eq!(got, EXPECTED_DRY_RUN, "{got}");
    assert_eq!(tree(&root), before);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn organize_moves_files_into_categories_and_resolves_collisions() {
    let root = sample_tree("move");
    let out = run_script(&organize(), &[root.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        tree(&root),
        [
            ".hidden.jpg",
            "archives/",
            "archives/backup.tar.gz",
            "code/",
            "code/main.rs",
            "documents/",
            "documents/notes.txt",
            "documents/paper.pdf",
            "images/",
            "images/SHOT.PNG",
            "images/photo (1).jpg",
            "images/photo (2).jpg",
            "images/photo.jpg",
            "old/",
            "other/",
            "other/README",
            "other/weird.xyz",
        ]
    );
    // The collisions kept every file's content.
    let read = |rel: &str| fs::read_to_string(root.join(rel)).unwrap();
    // The plan walks in name order, so `old/` is placed before the top-level `photo.jpg`.
    assert_eq!(read("images/photo (1).jpg"), "oldest");
    assert_eq!(read("images/photo.jpg"), "older");
    assert_eq!(read("images/photo (2).jpg"), "jpeg");
    // A second run finds everything in place.
    let again = run_script(&organize(), &[root.to_str().unwrap()]);
    assert_eq!(again.status.code(), Some(0));
    assert_eq!(text(&again.stdout), "nothing to do\n");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn organize_match_only_touches_matching_names() {
    let root = sample_tree("match");
    let out = run_script(
        &organize(),
        &[root.to_str().unwrap(), "--match", r"\.(pdf|txt)$"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let t = tree(&root);
    assert!(t.contains(&"documents/paper.pdf".to_string()), "{t:?}");
    assert!(t.contains(&"documents/notes.txt".to_string()), "{t:?}");
    assert!(t.contains(&"photo.jpg".to_string()), "{t:?}");
    assert!(!t.contains(&"images/".to_string()), "{t:?}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn organize_reads_a_rules_file() {
    let root = sample_tree("rules");
    let rules = scripts().join("organize/rules.example.json");
    let out = run_script(
        &organize(),
        &[root.to_str().unwrap(), "--rules", rules.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let t = tree(&root);
    for want in [
        "photos/photo.jpg",
        "photos/SHOT.PNG",
        "paperwork/notes.txt",
        "paperwork/paper.pdf",
        "source/main.rs",
        // `tar.gz` is not in these rules.
        "other/backup.tar.gz",
    ] {
        assert!(t.contains(&want.to_string()), "{want} missing from {t:?}");
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn organize_by_date_uses_the_modification_time() {
    let root = temp_dir("date");
    touch(&root, "a.txt", "a");
    touch(&root, "b.txt", "b");
    touch(&root, "c.txt", "c");
    let set = |name: &str, unix: u64| {
        let file = fs::File::options()
            .write(true)
            .open(root.join(name))
            .unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(unix))
            .unwrap();
    };
    set("a.txt", 1_615_000_000); // 2021-03-06
    set("b.txt", 1_700_000_000); // 2023-11-14
    set("c.txt", 1_615_100_000); // 2021-03-07
    let out = run_script(&organize(), &[root.to_str().unwrap(), "--by", "date"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        tree(&root),
        [
            "2021/",
            "2021/03/",
            "2021/03/a.txt",
            "2021/03/c.txt",
            "2023/",
            "2023/11/",
            "2023/11/b.txt",
        ]
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn organize_exit_codes() {
    let root = temp_dir("codes");
    let dir = root.to_str().unwrap();
    // 2: the command line is wrong.
    for args in [
        vec![],
        vec![dir, "--by", "size"],
        vec!["/definitely/not/here"],
        vec![dir, "--match", "("],
    ] {
        let out = run_script(&organize(), &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stderr).contains("usage: colibri run organize.colibri"));
    }
    // 1: the rules file is unreadable or not JSON of the right shape.
    let missing = run_script(&organize(), &[dir, "--rules", "/no/such/rules.json"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(text(&missing.stderr).contains("reading the rules"));
    let bad = root.join("bad.json");
    fs::write(&bad, r#"{"images": "jpg"}"#).unwrap();
    let out = run_script(&organize(), &[dir, "--rules", bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(
        err.contains("the rules file") && err.contains("$.images"),
        "{err}"
    );
    let _ = fs::remove_dir_all(&root);
}

// ------------------------------------------------------------------ report

fn report() -> PathBuf {
    scripts().join("report/report.colibri")
}

fn fixtures() -> PathBuf {
    scripts().join("report/fixtures")
}

/// Serves the fixture folder on 127.0.0.1 (`GET /<file>`, 404 for anything else). Returns the
/// base URL and the log of requested paths. It lives until the process ends.
fn serve() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener.local_addr().unwrap().port();
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = log.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = seen.clone();
            thread::spawn(move || handle(stream, &seen));
        }
    });
    (format!("http://127.0.0.1:{port}/data"), log)
}

fn handle(mut stream: TcpStream, seen: &std::sync::Mutex<Vec<String>>) {
    let mut data = Vec::new();
    let mut chunk = [0u8; 1024];
    while !data.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        data.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&data).into_owned();
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .unwrap_or("/")
        .to_string();
    seen.lock().unwrap().push(path.clone());
    let file = path.strip_prefix("/data/").filter(|f| !f.contains('/'));
    let (status, body) = match file.and_then(|f| fs::read(fixtures().join(f)).ok()) {
        Some(body) => ("200 OK", body),
        None => ("404 Not Found", b"not found".to_vec()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

const NOW: &str = "2024-05-01T12:00:00Z";

fn expected_report() -> String {
    fs::read_to_string(scripts().join("report/expected.md")).expect("report/expected.md")
}

#[test]
fn report_over_http_matches_the_golden_file() {
    let (base, log) = serve();
    let dir = temp_dir("http");
    let out_file = dir.join("report.md");
    let url = format!("{base}/index.json");
    let out = run_script(
        &report(),
        &[
            &url,
            "--out",
            out_file.to_str().unwrap(),
            "--now",
            NOW,
            "--concurrency",
            "3",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).starts_with("wrote "),
        "{}",
        text(&out.stdout)
    );
    let got = fs::read_to_string(&out_file).unwrap();
    if std::env::var_os("UPDATE_EXPECTED").is_some() {
        fs::write(scripts().join("report/expected.md"), &got).unwrap();
    }
    assert_eq!(
        got,
        expected_report(),
        "run with UPDATE_EXPECTED=1 to rewrite it"
    );
    // The index and each of the three items were fetched once.
    let mut seen = log.lock().unwrap().clone();
    seen.sort();
    assert_eq!(
        seen,
        [
            "/data/index.json",
            "/data/north.json",
            "/data/south.json",
            "/data/west.json"
        ]
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn report_from_files_is_the_same_report() {
    let index = fixtures().join("index.json");
    for concurrency in ["1", "4"] {
        let out = run_script(
            &report(),
            &[
                index.to_str().unwrap(),
                "--now",
                NOW,
                "--concurrency",
                concurrency,
            ],
        );
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        // Without `--out` the report goes to stdout, `print` adding one newline.
        assert_eq!(text(&out.stdout), format!("{}\n", expected_report()));
    }
}

#[test]
fn report_lists_failed_sources_and_exits_1() {
    let (base, _) = serve();
    let url = format!("{base}/index_with_missing.json");
    let out = run_script(&report(), &[&url, "--now", NOW]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("Sources: 2 read, 1 failed."), "{stdout}");
    assert!(stdout.contains("## Failed sources"), "{stdout}");
    assert!(stdout.contains("gone.json"), "{stdout}");
    assert!(stdout.contains("HTTP status 404"), "{stdout}");
    assert!(text(&out.stderr).contains("1 of 3 sources failed"));
}

#[test]
fn report_exit_codes() {
    // 1: the index itself cannot be read.
    let out = run_script(&report(), &["/no/such/index.json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("reading the index"),
        "{}",
        text(&out.stderr)
    );
    // 2: bad command line.
    let index = fixtures().join("index.json");
    let index = index.to_str().unwrap();
    for args in [
        vec![],
        vec![index, "--concurrency", "0"],
        vec![index, "--concurrency", "many"],
        vec![index, "--now", "yesterday"],
    ] {
        let out = run_script(&report(), &args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            text(&out.stderr)
        );
        assert!(text(&out.stderr).contains("usage: colibri run report.colibri"));
    }
}

#[test]
fn report_git_flag_adds_the_commit_inside_a_repository() {
    let index = fixtures().join("index.json");
    let inside = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output();
    let Ok(inside) = inside else { return };
    if !inside.status.success() {
        return; // not a checkout (a source tarball): nothing to compare with
    }
    let out = run_script(&report(), &[index.to_str().unwrap(), "--now", NOW, "--git"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let sha = text(&inside.stdout).trim().to_string();
    assert!(
        text(&out.stdout).contains(&format!("Generated {NOW}, commit `{sha}`")),
        "{}",
        text(&out.stdout)
    );
    // Outside a repository the flag is silently ignored.
    let dir = temp_dir("nogit");
    let out = Command::new(env!("CARGO_BIN_EXE_colibri"))
        .current_dir(&dir)
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap())
        .args([
            "run",
            report().to_str().unwrap(),
            "--",
            index.to_str().unwrap(),
            "--now",
            NOW,
            "--git",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        !text(&out.stdout).contains("commit"),
        "{}",
        text(&out.stdout)
    );
    let _ = fs::remove_dir_all(&dir);
}
