//! End-to-end tests of `colibri run --profile` and `colibri test --profile`. They weigh by
//! steps, which gives the same numbers on every run, and assert on the shape of the profile:
//! which frames exist and where the weight is, never on exact counts.

use std::path::PathBuf;
use std::process::Output;

fn colibri_command() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_colibri"))
}

fn colibri(args: &[&str]) -> Output {
    colibri_command()
        .args(args)
        .output()
        .expect("the colibri binary runs")
}

/// A fresh folder for one test, with `files` in it.
fn folder(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-profile-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (path, text) in files {
        std::fs::write(dir.join(path), text).unwrap();
    }
    dir
}

fn text(out: &[u8]) -> String {
    String::from_utf8_lossy(out).into_owned()
}

/// `fib(18)` under `main`, a coroutine that loops, and a built-in written in the prelude
/// (`List.map`) that calls a closure.
const PROGRAM: &str = "\
fn fib(n: int) -> int {
    if n < 2 { return n }
    return fib(n - 1) + fib(n - 2)
}

fn spin(n: int) -> int {
    let mut t = 0
    for i in 0..n {
        t += i
    }
    return t
}

fn main() {
    let c = go spin(5000)
    let xs = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    let mut total = 0
    for i in 0..300 {
        let ys = xs.map(fn(x) => x * i)
        total += ys.len()
    }
    print(fib(18))
    print(total)
    print(c.join())
}
";

struct Profiled {
    dir: PathBuf,
    folded: String,
    svg: String,
    stderr: String,
}

/// Runs the program with `--profile=prof.svg --profile-weight=steps` and reads the outputs.
fn profiled(name: &str) -> Profiled {
    let dir = folder(name, &[("p.colibri", PROGRAM)]);
    let svg_path = dir.join("prof.svg");
    let out = colibri(&[
        "run",
        &format!("--profile={}", svg_path.display()),
        "--profile-weight=steps",
        dir.join("p.colibri").to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "2584\n3000\n12497500\n");
    Profiled {
        folded: std::fs::read_to_string(dir.join("prof.folded")).unwrap(),
        svg: std::fs::read_to_string(&svg_path).unwrap(),
        stderr: text(&out.stderr),
        dir,
    }
}

/// `(frames, weight)` of each folded line.
fn lines(folded: &str) -> Vec<(Vec<&str>, u64)> {
    folded
        .lines()
        .map(|l| {
            let (stack, weight) = l.rsplit_once(' ').expect("a stack and a weight");
            (stack.split(';').collect(), weight.parse().expect("a number"))
        })
        .collect()
}

#[test]
fn the_folded_stacks_have_the_shape_of_the_program() {
    let p = profiled("shape");
    let lines = lines(&p.folded);
    let total: u64 = lines.iter().map(|(_, w)| w).sum();
    assert!(total > 20_000, "a program of some size: {total}");

    // The frames that exist: `main` and the coroutine are the roots.
    let names: std::collections::BTreeSet<&str> = lines
        .iter()
        .flat_map(|(s, _)| s.iter().copied())
        .collect();
    let expected = ["<closure in main>", "List.map", "fib", "go spin", "main"];
    assert_eq!(names.into_iter().collect::<Vec<_>>(), expected);
    assert!(lines
        .iter()
        .all(|(s, _)| s[0] == "main" || s[0] == "go spin"));

    // `fib` holds most of the weight of `main`'s stacks, and under `main` only.
    let under_fib: u64 = lines
        .iter()
        .filter(|(s, _)| s.contains(&"fib"))
        .map(|(_, w)| w)
        .sum();
    let under_main: u64 = lines
        .iter()
        .filter(|(s, _)| s[0] == "main")
        .map(|(_, w)| w)
        .sum();
    assert!(under_fib * 2 > under_main, "{under_fib} of {under_main}");
    assert!(lines
        .iter()
        .filter(|(s, _)| s.contains(&"fib"))
        .all(|(s, _)| s[..2] == ["main", "fib"]));

    // The hidden prelude function that `map` runs is named as the built-in, and calls the closure.
    assert!(
        lines
            .iter()
            .any(|(s, _)| s.windows(2).any(|w| w == ["List.map", "<closure in main>"])),
        "{}",
        p.folded
    );
    assert!(!p.folded.contains("__"), "{}", p.folded);
    // The coroutine's frames hang under their own root.
    assert!(lines.iter().any(|(s, _)| s == &["go spin"]));
}

#[test]
fn the_table_on_stderr_lists_functions_and_lines() {
    let p = profiled("table");
    let err = &p.stderr;
    assert!(err.contains("self") && err.contains("total"), "{err}");
    let functions: Vec<&str> = err
        .lines()
        .skip_while(|l| !l.contains("function"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .collect();
    assert!(functions[0].ends_with("fib"), "fib is the heaviest: {err}");
    assert!(err.contains("p.colibri:3"), "{err}");
    assert!(p.dir.join("prof.svg").exists());
}

#[test]
fn the_flame_graph_is_well_formed_xml_with_a_group_for_each_frame() {
    let p = profiled("svg");
    let groups = check_xml(&p.svg);
    // One `<g>` per distinct frame of the folded stacks, and one for the root.
    let mut frames: std::collections::BTreeSet<Vec<&str>> = std::collections::BTreeSet::new();
    for (stack, _) in lines(&p.folded) {
        for end in 1..=stack.len() {
            frames.insert(stack[..end].to_vec());
        }
    }
    assert_eq!(groups, frames.len() + 1);
    // A name with `<` in it is escaped.
    assert!(p.svg.contains("&lt;closure in main&gt;"));
    assert!(p.svg.contains("<title>all ("), "the root frame");
}

/// A small well-formedness check of the SVG: tags nest and match, attributes are quoted, text
/// has no bare `<` and only the five entities. Returns how many `<g>` elements there are.
fn check_xml(doc: &str) -> usize {
    let mut open: Vec<String> = Vec::new();
    let mut groups = 0;
    let mut rest = doc;
    let mut roots = 0;
    while let Some(at) = rest.find('<') {
        let (before, tail) = rest.split_at(at);
        check_text(before);
        if open.is_empty() {
            assert!(before.trim().is_empty(), "text outside the root: {before:?}");
        }
        if let Some(body) = tail.strip_prefix("<?") {
            let end = body.find("?>").expect("a declaration ends");
            rest = &body[end + 2..];
            continue;
        }
        let end = tail.find('>').expect("a tag ends");
        let tag = &tail[1..end];
        rest = &tail[end + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            assert_eq!(open.pop().as_deref(), Some(name.trim()), "closing {name}");
            continue;
        }
        let selfclosing = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let name_end = tag.find(char::is_whitespace).unwrap_or(tag.len());
        let name = &tag[..name_end];
        assert!(!name.is_empty() && !name.contains('"'), "{tag}");
        check_attributes(&tag[name_end..]);
        if name == "g" {
            groups += 1;
        }
        if open.is_empty() {
            roots += 1;
        }
        if !selfclosing {
            open.push(name.to_string());
        }
    }
    check_text(rest);
    assert!(open.is_empty(), "unclosed: {open:?}");
    assert_eq!(roots, 1, "one root element");
    groups
}

fn check_text(text: &str) {
    assert!(!text.contains('<'), "{text:?}");
    let mut chars = text.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '&' {
            let rest = &text[i..];
            let end = rest.find(';').expect("an entity ends");
            assert!(
                ["&amp", "&lt", "&gt", "&quot", "&apos"].contains(&&rest[..end]),
                "entity {:?}",
                &rest[..=end]
            );
        }
    }
}

/// `name="value"` pairs, separated by space; values have no raw `<` or `&`.
fn check_attributes(mut attrs: &str) {
    loop {
        attrs = attrs.trim_start();
        if attrs.is_empty() {
            return;
        }
        let eq = attrs.find('=').expect("an attribute has a value");
        let name = &attrs[..eq];
        assert!(!name.is_empty() && !name.contains(char::is_whitespace), "{attrs}");
        let value = attrs[eq + 1..].strip_prefix('"').expect("a quoted value");
        let end = value.find('"').expect("the value ends");
        check_text(&value[..end]);
        assert!(!value[..end].contains('<'));
        attrs = &value[end + 1..];
    }
}

#[test]
fn a_step_profile_is_the_same_on_every_run() {
    let a = profiled("same-a");
    let b = profiled("same-b");
    assert_eq!(a.folded, b.folded);
    assert_eq!(a.svg, b.svg);
    // The same rows in the table, too.
    let rows = |p: &Profiled| {
        p.stderr
            .lines()
            .filter(|l| !l.starts_with("written"))
            .collect::<Vec<_>>()
            .join("\n")
            .replace(p.dir.to_str().unwrap(), "DIR")
    };
    assert_eq!(rows(&a), rows(&b));
}

#[test]
fn a_time_profile_writes_the_same_files_with_weights_in_nanoseconds() {
    let dir = folder("time", &[("p.colibri", PROGRAM)]);
    let svg = dir.join("t.svg");
    let out = colibri(&[
        "run",
        &format!("--profile={}", svg.display()),
        dir.join("p.colibri").to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let folded = std::fs::read_to_string(dir.join("t.folded")).unwrap();
    let lines = lines(&folded);
    assert!(lines.iter().any(|(s, w)| s.contains(&"fib") && *w > 0));
    assert!(text(&out.stderr).contains("ms"));
    check_xml(&std::fs::read_to_string(&svg).unwrap());
}

#[test]
fn profile_without_a_file_writes_profile_svg_and_profile_folded_in_the_current_folder() {
    let dir = folder("default", &[("p.colibri", PROGRAM)]);
    let out = colibri_command()
        .current_dir(&dir)
        .args(["run", "--profile", "--profile-weight=steps", "p.colibri"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(dir.join("profile.svg").exists());
    assert!(dir.join("profile.folded").exists());
}

#[test]
fn a_profile_weight_without_profile_is_refused_and_a_panicking_program_is_still_profiled() {
    let dir = folder("misc", &[("p.colibri", PROGRAM), ("boom.colibri", BOOM)]);
    let p = dir.join("p.colibri");
    let out = colibri(&["run", "--profile-weight=steps", p.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("--profile"));

    let boom = dir.join("boom.colibri");
    let svg = dir.join("boom.svg");
    let out = colibri(&[
        "run",
        &format!("--profile={}", svg.display()),
        "--profile-weight=steps",
        boom.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(101));
    let folded = std::fs::read_to_string(dir.join("boom.folded")).unwrap();
    assert!(folded.lines().all(|l| l.starts_with("main")), "{folded}");
    assert!(folded.contains("main;deep;deep"), "{folded}");
}

const BOOM: &str = "\
fn deep(n: int) -> int {
    if n == 0 { panic(\"boom\") }
    return deep(n - 1) + 1
}
fn main() {
    let mut t = 0
    for i in 0..2000 {
        t += i
    }
    print(deep(30) + t)
}
";

#[test]
fn test_profile_covers_every_test() {
    let dir = folder(
        "tests",
        &[(
            "t.colibri",
            "fn fib(n: int) -> int {\n    if n < 2 { return n }\n    return fib(n - 1) + fib(n - 2)\n}\nfn count(n: int) -> int {\n    let mut t = 0\n    for i in 0..n {\n        t += i\n    }\n    return t\n}\ntest \"fib\" {\n    assert_eq(fib(15), 610)\n}\ntest \"count\" {\n    assert_eq(count(4000), 7998000)\n}\n",
        )],
    );
    let svg = dir.join("tp.svg");
    let out = colibri(&[
        "test",
        &format!("--profile={}", svg.display()),
        "--profile-weight=steps",
        dir.join("t.colibri").to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}{}", text(&out.stdout), text(&out.stderr));
    assert!(text(&out.stdout).contains("2 passed"));
    let folded = std::fs::read_to_string(dir.join("tp.folded")).unwrap();
    let lines = lines(&folded);
    assert!(lines.iter().any(|(s, _)| s.contains(&"fib")), "{folded}");
    assert!(lines.iter().any(|(s, _)| s.contains(&"count")), "{folded}");
    check_xml(&std::fs::read_to_string(&svg).unwrap());
}
