//! The automation scripts in `examples/scripts`, run through the real
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
    for script in [
        "organize/organize.colibri",
        "report/report.colibri",
        "kvserver/kvserver.colibri",
        "kvserver/kvclient.colibri",
    ] {
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
                .replace('\\', "/");
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
            "other/",
            "other/README",
            "other/weird.xyz",
        ]
    );
    // The collisions kept every file's content.
    let read = |rel: &str| fs::read_to_string(root.join(rel)).unwrap();
    // The plan walks in name order, so `old/` is placed before the top-level `photo.jpg`.
    // `old/` was emptied by the moves, so it is pruned.
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
fn organize_prunes_only_the_folders_it_emptied() {
    let root = temp_dir("prune");
    touch(&root, "deep/er/a.txt", "a");
    touch(&root, "kept/b.txt", "b");
    touch(&root, "kept/.hidden", "stays");
    fs::create_dir_all(root.join("untouched")).unwrap();
    let out = run_script(&organize(), &[root.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        tree(&root),
        [
            "documents/",
            "documents/a.txt",
            "documents/b.txt",
            "kept/",
            "kept/.hidden",
            "untouched/",
        ]
    );
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

/// A server that answers every request after `delay`, from the files of `dir`; counts how many
/// requests were in flight at once. Returns the base URL and the peak.
fn serve_slowly(
    dir: PathBuf,
    delay: Duration,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener.local_addr().unwrap().port();
    let active = std::sync::Arc::new(AtomicUsize::new(0));
    let peak = std::sync::Arc::new(AtomicUsize::new(0));
    let reported = peak.clone();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let (dir, active, peak) = (dir.clone(), active.clone(), peak.clone());
            thread::spawn(move || {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let mut data = Vec::new();
                let mut chunk = [0u8; 1024];
                while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&data).into_owned();
                let name = head
                    .lines()
                    .next()
                    .and_then(|l| l.split(' ').nth(1))
                    .and_then(|p| p.strip_prefix("/data/"))
                    .unwrap_or("")
                    .to_string();
                thread::sleep(delay);
                let body = fs::read(dir.join(name)).unwrap_or_default();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (format!("http://127.0.0.1:{port}/data"), reported)
}

/// Finding 4: with a non-blocking `http` the workers' requests overlap. Four items, each answered
/// after 300 ms, `--concurrency 4`: the items take about 300 ms together, not 4 x 300 ms.
#[test]
fn report_requests_overlap_with_concurrency() {
    let dir = temp_dir("overlap");
    let north = fs::read_to_string(fixtures().join("north.json")).unwrap();
    let items = ["a.json", "b.json", "c.json", "d.json"];
    for item in items {
        fs::write(dir.join(item), &north).unwrap();
    }
    let list: Vec<String> = items.iter().map(|i| format!("\"{i}\"")).collect();
    fs::write(
        dir.join("index.json"),
        format!("{{\"title\": \"Slow\", \"items\": [{}]}}", list.join(", ")),
    )
    .unwrap();
    let delay = Duration::from_millis(300);
    let (base, peak) = serve_slowly(dir.clone(), delay);
    let url = format!("{base}/index.json");
    let mut runs = Vec::new();
    for concurrency in ["4", "1"] {
        let started = std::time::Instant::now();
        let out = run_script(
            &report(),
            &[&url, "--now", NOW, "--concurrency", concurrency],
        );
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert!(text(&out.stdout).contains("Sources: 4 read, 0 failed."));
        runs.push((
            started.elapsed(),
            peak.swap(0, std::sync::atomic::Ordering::SeqCst),
        ));
    }
    let (overlapped, peak4) = runs[0];
    let (serial, peak1) = runs[1];
    eprintln!(
        "concurrency 4: {overlapped:?} (peak {peak4}); concurrency 1: {serial:?} (peak {peak1})"
    );
    // The index (300 ms) then four items together (about 300 ms), against five in a row.
    assert_eq!(peak4, 4, "four requests were in flight at once");
    assert_eq!(peak1, 1);
    assert!(overlapped < Duration::from_millis(1000), "{overlapped:?}");
    assert!(serial >= Duration::from_millis(1500), "{serial:?}");
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- kvserver

/// A running `kvserver` and the address it printed.
struct Kv {
    child: std::process::Child,
    addr: String,
}

impl Kv {
    fn start(extra: &[&str]) -> Kv {
        use std::io::BufRead;
        let script = scripts().join("kvserver/kvserver.colibri");
        let mut child = Command::new(env!("CARGO_BIN_EXE_colibri"))
            .args(["run", script.to_str().unwrap(), "--"])
            .args(extra)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("the colibri binary runs");
        let mut first = String::new();
        std::io::BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut first)
            .unwrap();
        let addr = first
            .trim()
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("unexpected first line {first:?}"))
            .to_string();
        Kv { child, addr }
    }

    fn client(&self) -> (TcpStream, std::io::BufReader<TcpStream>) {
        let s = TcpStream::connect(&self.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let r = std::io::BufReader::new(s.try_clone().unwrap());
        (s, r)
    }
}

impl Drop for Kv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ask(c: &mut (TcpStream, std::io::BufReader<TcpStream>), request: &str) -> String {
    use std::io::BufRead;
    c.0.write_all(request.as_bytes()).unwrap();
    let mut line = String::new();
    c.1.read_line(&mut line).unwrap();
    line.trim_end().to_string()
}

#[test]
fn kvserver_serves_several_clients_and_shuts_down() {
    let mut kv = Kv::start(&[]);
    let mut a = kv.client();
    let mut b = kv.client();
    assert_eq!(ask(&mut a, "SET colour dark blue\r\n"), "OK");
    // Another client sees it at once; and `\n` alone ends a request too.
    assert_eq!(ask(&mut b, "GET colour\n"), "VALUE dark blue");
    assert_eq!(ask(&mut b, "set size 3\n"), "OK");
    assert_eq!(ask(&mut a, "KEYS\n"), "KEYS colour size");
    assert_eq!(ask(&mut a, "COUNT\n"), "2");
    assert_eq!(ask(&mut a, "DEL size\n"), "DELETED");
    assert_eq!(ask(&mut b, "DEL size\n"), "MISSING");
    assert_eq!(ask(&mut b, "GET size\n"), "MISSING");
    assert!(ask(&mut b, "FROB\n").starts_with("ERR unknown"));
    assert_eq!(ask(&mut a, "QUIT\n"), "BYE");
    // QUIT closed that connection only.
    assert_eq!(ask(&mut b, "COUNT\n"), "1");
    assert_eq!(ask(&mut b, "SHUTDOWN\n"), "BYE");
    let status = kv.child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
}

/// The Colibri client that the README shows (for systems without `nc`) talks to the server, and
/// says so when there is nobody to talk to.
#[test]
fn kvclient_talks_to_kvserver_and_reports_a_missing_server() {
    let kv = Kv::start(&[]);
    let client = scripts().join("kvserver/kvclient.colibri");
    let out = run_script(
        &client,
        &[&kv.addr, "SET a 1", "GET a", "COUNT", "SHUTDOWN"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "OK\nVALUE 1\n1\nBYE\n");
    let mut kv = kv;
    assert_eq!(kv.child.wait().unwrap().code(), Some(0));
    // The server has stopped: its port refuses (a port that was just closed).
    let out = run_script(&client, &[&kv.addr, "COUNT"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("cannot reach the server"),
        "{}",
        text(&out.stderr)
    );
    let out = run_script(&client, &[&kv.addr]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn kvserver_closes_idle_connections_and_reports_a_bad_address() {
    let kv = Kv::start(&["--idle", "1.5"]);
    let mut c = kv.client();
    assert_eq!(ask(&mut c, "COUNT\n"), "0");
    // Silent for longer than the idle time: the server closes the connection.
    use std::io::BufRead;
    let mut line = String::new();
    assert_eq!(c.1.read_line(&mut line).unwrap(), 0);
    drop(kv);

    let script = scripts().join("kvserver/kvserver.colibri");
    let out = run_script(&script, &["--addr", "not an address"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("is not an address"),
        "{}",
        text(&out.stderr)
    );
}

/// `--idle` is a deadline for each request line, and what it is given must be a number of
/// seconds that `set_timeout` takes: anything else is a usage error (exit 2), not a panic.
#[test]
fn kvserver_refuses_an_idle_time_or_a_connection_cap_that_makes_no_sense() {
    let script = scripts().join("kvserver/kvserver.colibri");
    for (flag, value) in [
        ("--idle", "-1"),
        ("--idle", "nan"),
        ("--idle", "inf"),
        ("--idle", "soon"),
        ("--max-connections", "0"),
        ("--max-connections", "-3"),
        ("--max-connections", "many"),
    ] {
        let out = run_script(&script, &[flag, value]);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{flag} {value}: {}",
            text(&out.stderr)
        );
        assert!(
            text(&out.stderr).contains("usage:") && !text(&out.stderr).contains("panic"),
            "{flag} {value}: {}",
            text(&out.stderr)
        );
        assert_eq!(text(&out.stdout), "", "{flag} {value}");
    }
}

/// With `--max-connections 2` a third connection is told so in one line and closed, and a
/// connection that leaves makes room again.
#[test]
fn kvserver_turns_away_connections_over_its_cap() {
    use std::io::BufRead;
    let kv = Kv::start(&["--max-connections", "2"]);
    let mut a = kv.client();
    let mut b = kv.client();
    // Both are being served (the answer shows the server has taken them into its table).
    assert_eq!(ask(&mut a, "SET k v\n"), "OK");
    assert_eq!(ask(&mut b, "GET k\n"), "VALUE v");
    let mut c = kv.client();
    let mut line = String::new();
    c.1.read_line(&mut line).unwrap();
    assert_eq!(line.trim_end(), "ERR too many connections");
    line.clear();
    assert_eq!(
        c.1.read_line(&mut line).unwrap(),
        0,
        "it is closed after that"
    );
    // The others are not disturbed.
    assert_eq!(ask(&mut a, "COUNT\n"), "1");
    // `a` leaves; the server notices when its coroutine ends, so ask until there is room.
    assert_eq!(ask(&mut a, "QUIT\n"), "BYE");
    let started = std::time::Instant::now();
    loop {
        let mut d = kv.client();
        let answer = ask(&mut d, "COUNT\n");
        if answer == "1" {
            break;
        }
        assert_eq!(answer, "ERR too many connections");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no room after a connection left"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(ask(&mut b, "SHUTDOWN\n"), "BYE");
}

/// A server that runs out of descriptors reports it and goes on: `accept` fails with a
/// `net.Error` while the table is full, and works again when connections have ended. Unix only,
/// because the limit is set with `ulimit -n` (a Windows process has no such limit to lower).
#[cfg(unix)]
#[test]
fn kvserver_survives_running_out_of_descriptors() {
    use std::io::BufRead;
    let script = scripts().join("kvserver/kvserver.colibri");
    let mut child = Command::new("sh")
        .args(["-c", "ulimit -n 40; exec \"$0\" run \"$1\" --"])
        .arg(env!("CARGO_BIN_EXE_colibri"))
        .arg(&script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("sh runs");
    let mut first = String::new();
    std::io::BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut first)
        .unwrap();
    let addr = first
        .trim()
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("unexpected first line {first:?}"))
        .to_string();
    let mut kv = Kv { child, addr };
    // Many more connections than the server has descriptors for. Each is served or waits in
    // the queue of the listener.
    let crowd: Vec<_> = (0..80).map(|_| kv.client()).collect();
    let mut first_ones = crowd;
    let mut a = first_ones.remove(0);
    assert_eq!(ask(&mut a, "SET k v\n"), "OK");
    // They all go away; the server accepts the queued ones, finds them closed, and is free.
    drop(first_ones);
    let mut b = kv.client();
    assert_eq!(ask(&mut b, "GET k\n"), "VALUE v");
    assert_eq!(ask(&mut b, "SHUTDOWN\n"), "BYE");
    let status = kv.child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
}

/// `os.exit` with coroutines blocked in `accept` and `read` ends the process at once: no
/// background thread keeps it alive.
#[test]
fn os_exit_with_blocked_sockets_ends_the_process() {
    let dir = temp_dir("net-exit");
    let script = dir.join("exit.colibri");
    fs::write(
        &script,
        r#"import net
import os

fn accepting(l: net.Listener) {
    let _ = l.accept()
}

fn reading(c: net.Conn) {
    let _ = c.read(1)
}

fn main() {
    let l = net.listen("127.0.0.1:0").expect("listens")
    let c = net.connect(l.addr()).expect("connects")
    let s = l.accept().expect("accepts")
    go reading(c)
    go reading(s)
    go accepting(l)
    wait(0.05)
    os.exit(3)
}
"#,
    )
    .unwrap();
    let started = std::time::Instant::now();
    let out = run_script(&script, &[]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The example scripts are formatted: `colibri fmt --check` lists nothing. If this fails, run
/// `colibri fmt examples/scripts` (and read the diff).
#[test]
fn example_scripts_are_formatted() {
    let dir = scripts();
    let out = colibri(&["fmt", "--check", dir.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "run `colibri fmt examples/scripts`:\n{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
}
