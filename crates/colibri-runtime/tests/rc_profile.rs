//! The reference-count profile of the `Vm` (`start_rc_profile`, `finish_rc_profile`): which
//! line copies, clones and drops what, and that counting changes nothing else.

use colibri_runtime as colibri;

/// What a run printed, how many steps it took, how it ended, the cycles it leaked, and what it
/// counted (when it was counting).
struct Run {
    stdout: String,
    steps: u64,
    ended: Result<(), String>,
    leaks: usize,
    profile: Option<colibri::RcProfile>,
}

fn run(src: &str, counting: bool) -> Run {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .track_leaks(true)
        .build();
    if counting {
        vm.start_rc_profile();
    }
    vm.load_source("p.colibri", src).unwrap();
    let mut steps = vm.steps_used();
    let ended = vm.run_main();
    steps += vm.steps_used();
    let ended = ended.and_then(|()| vm.run_event_loop());
    steps += vm.steps_used();
    let profile = vm.finish_rc_profile();
    vm.shutdown();
    Run {
        stdout: out.take(),
        steps,
        ended: ended.map_err(|e| e.render(false)),
        leaks: vm.find_leaks().len(),
        profile,
    }
}

/// `(copies, drops, clones)` of a line.
fn row(profile: &colibri::RcProfile, line: u32) -> (u64, u64, u64) {
    profile
        .lines
        .iter()
        .find(|l| l.line == line)
        .map_or((0, 0, 0), |l| (l.copies, l.drops, l.clones))
}

fn counted(src: &str) -> colibri::RcProfile {
    let r = run(src, true);
    assert_eq!(r.ended, Ok(()));
    r.profile.expect("counting was started")
}

#[test]
fn a_list_passed_to_a_call_is_one_copy_and_one_drop_on_the_calling_line() {
    let p = counted(
        "\
fn size(xs: List[int]) -> int {
    return xs.len()
}
fn main() {
    let xs = [1, 2, 3]
    print(size(xs))
}
",
    );
    assert_eq!(row(&p, 6), (1, 1, 0), "{p:?}");
    // `xs` itself dies when `main` ends, which is charged to the line of `main`.
    assert_eq!(row(&p, 4), (0, 1, 0), "{p:?}");
    assert_eq!(p.total(), 3);
}

#[test]
fn clone_counts_as_a_clone_and_its_copy_and_drops() {
    let p = counted(
        "\
fn main() {
    let xs = [1, 2, 3]
    let ys = xs.clone()
    print(ys.len())
}
",
    );
    // The receiver is copied into the call, and what the call returns is a deep copy.
    assert_eq!(row(&p, 3), (1, 1, 1), "{p:?}");
    assert_eq!(row(&p, 1), (0, 2, 0), "{p:?}");
}

#[test]
fn literals_and_push_are_copies() {
    let p = counted(
        "\
fn main() {
    let mut names: List[str] = []
    for i in 0..4 {
        names.push(\"n\")
    }
    print(names.len())
}
",
    );
    // Each trip loads the literal; the push keeps it, so nothing is dropped there.
    assert_eq!(row(&p, 4), (4, 0, 0), "{p:?}");
}

#[test]
fn a_constructor_keeps_a_copy_of_each_value() {
    let p = counted(
        "\
struct P { name: str, tags: List[str] }
fn main() {
    let tags = [\"a\"]
    let p = P { name: \"n\", tags: tags }
    print(p.name.len())
}
",
    );
    // Two loads and a copy of `tags` to build the struct, and a copy of each of its two
    // values into it; the temporaries die at the end of the statement.
    assert_eq!(row(&p, 4), (4, 2, 0), "{p:?}");
    // The field read on the last line is a copy, and so is the temporary it is dropped from.
    assert_eq!(row(&p, 5), (1, 1, 0), "{p:?}");
}

#[test]
fn a_for_loop_takes_a_reference_to_each_item_and_drops_it() {
    let p = counted(
        "\
fn main() {
    let xs = [\"a\", \"b\", \"c\"]
    let mut n = 0
    for s in xs {
        n += s.len()
    }
    print(n)
}
",
    );
    // The line of the `for`: a copy of the list, an item a trip, and a drop for the items and
    // the list.
    assert_eq!(row(&p, 4), (4, 4, 0), "{p:?}");
}

#[test]
fn a_scalar_is_never_counted() {
    // `T` is not known to be a scalar when the function is compiled, so its copies and its
    // clone have a `Count`; the values are integers and nothing is counted.
    let p = counted(
        "\
fn twice[T](x: T) -> T {
    let y = x.clone()
    return y
}
fn main() {
    let mut t = 0
    for i in 0..10 {
        t += twice(i)
    }
    print(t)
}
",
    );
    assert_eq!(p.total(), 0, "{p:?}");
}

#[test]
fn a_builtin_written_in_the_prelude_is_charged_to_the_line_that_called_it() {
    let p = counted(
        "\
fn main() {
    let xs = [\"a\", \"b\", \"c\"]
    let ys = xs.map(fn(s) => s + \"!\")
    print(ys.len())
}
",
    );
    assert_eq!(
        p.unplaced, 0,
        "the prelude's own lines are not a place: {p:?}"
    );
    assert!(
        p.lines.iter().all(|l| (1..=4).contains(&l.line)),
        "only lines of the program: {p:?}"
    );
    // The map moves every item through the prelude's `map` and the closure.
    let (copies, drops, _) = row(&p, 3);
    assert!(copies >= 6 && drops >= 6, "{p:?}");
}

#[test]
fn nothing_is_counted_unless_it_was_started() {
    let src = "fn main() {\n    let xs = [1, 2]\n    print(xs.len())\n}\n";
    let r = run(src, false);
    assert!(r.profile.is_none());
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .build();
    vm.load_source("p.colibri", src).unwrap();
    vm.run_main().unwrap();
    assert!(vm.finish_rc_profile().is_none());
    // Counting that was started is over once the profile is taken.
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .build();
    vm.start_rc_profile();
    vm.load_source("p.colibri", src).unwrap();
    vm.run_main().unwrap();
    assert!(vm.finish_rc_profile().is_some());
    assert!(vm.finish_rc_profile().is_none());
    vm.run_main().unwrap();
}

/// Programs with lists, strings, closures, a class with a `Drop` hook, a coroutine, a cycle that
/// leaks and a panic.
const CORPUS: &[&str] = &[
    "fn main() {\n    let mut xs: List[str] = []\n    for i in 0..50 {\n        xs.push(\"v{i}\")\n    }\n    let ys = xs.map(fn(s) => s + \"!\")\n    print(ys.join(\",\"))\n}\n",
    "class Noisy {\n    name: str\n}\nimpl Drop for Noisy {\n    fn drop(self) {\n        print(\"drop {self.name}\")\n    }\n}\nfn main() {\n    let a = Noisy { name: \"a\" }\n    let xs = [Noisy { name: \"b\" }, Noisy { name: \"c\" }]\n    let b = a\n    print(xs.len())\n}\n",
    "fn spin(n: int) -> int {\n    let mut t = 0\n    for i in 0..n {\n        t += i\n    }\n    return t\n}\nfn main() {\n    let c = go spin(3000)\n    let d = [1, 2, 3].map(fn(x) => x + 1)\n    print(d.len() + c.join())\n}\n",
    "class Node {\n    name: str\n    next: Node? = none\n}\nfn main() {\n    let a = Node { name: \"a\" }\n    let b = Node { name: \"b\" }\n    a.next = some(b)\n    b.next = some(a)\n    print(\"made\")\n}\n",
    "fn main() {\n    let xs = [1, 2, 3]\n    let ys = xs.clone()\n    print(ys[7])\n}\n",
    "struct P { name: str, tags: List[str] }\nfn show(p: P) -> str {\n    return p.name + p.tags.join(\"+\")\n}\nfn main() {\n    let p = P { name: \"n\", tags: [\"a\", \"b\"] }\n    let q = p\n    print(show(q))\n    let m = {\"k\": [1, 2], \"j\": [3]}\n    for (k, v) in m {\n        print(\"{k} {v.len()}\")\n    }\n}\n",
];

#[test]
fn counting_changes_neither_the_output_nor_the_steps_nor_the_end_nor_the_leaks() {
    for (i, src) in CORPUS.iter().enumerate() {
        let plain = run(src, false);
        let counting = run(src, true);
        assert_eq!(counting.stdout, plain.stdout, "program {i}");
        assert_eq!(counting.steps, plain.steps, "program {i}");
        assert_eq!(counting.ended, plain.ended, "program {i}");
        assert_eq!(counting.leaks, plain.leaks, "program {i}");
        assert!(plain.steps > 0, "program {i}");
        let found = counting.profile.expect("counted");
        assert!(found.total() > 0, "program {i} has reference-count traffic");
    }
    // The corpus has what it claims to have.
    assert!(run(CORPUS[1], false).stdout.contains("drop"));
    assert!(run(CORPUS[3], false).leaks > 0, "a cycle leaks");
    assert!(run(CORPUS[4], false).ended.is_err(), "a panic");
}

#[test]
fn assigning_to_a_local_drops_the_value_it_held() {
    let p = counted(
        "\
fn main() {
    let mut xs = [1]
    xs = [2, 3]
    print(xs.len())
}
",
    );
    assert_eq!(row(&p, 3), (0, 1, 0), "{p:?}");
    assert_eq!(row(&p, 1), (0, 1, 0), "{p:?}");
}

#[test]
fn a_push_of_a_value_that_is_kept_is_a_copy() {
    let p = counted(
        "\
fn main() {
    let mut names: List[str] = []
    let n = \"n\"
    for i in 0..3 {
        names.push(n)
    }
    print(names.len())
}
",
    );
    // The value stays in `n`, so each push takes a reference; the literal is one more.
    assert_eq!(row(&p, 5), (3, 0, 0), "{p:?}");
    assert_eq!(row(&p, 3), (1, 0, 0), "{p:?}");
}

#[test]
fn the_cleanup_of_a_panic_is_not_counted() {
    let p = counted_to_the_end(
        "\
fn main() {
    let xs = [\"a\"]
    let ys = [\"b\"]
    print(xs[5])
}
",
    );
    // Each list is a literal and a copy of it into the list, and the literal is dropped. The
    // drops of `xs` and `ys` that the panic runs on the way out are not on any line.
    assert_eq!(row(&p, 2), (2, 1, 0), "{p:?}");
    assert_eq!(row(&p, 3), (2, 1, 0), "{p:?}");
    assert_eq!(row(&p, 1), (0, 0, 0), "{p:?}");
    assert_eq!(p.lines.len(), 2, "no other line has traffic: {p:?}");
    assert_eq!(p.total(), 6, "{p:?}");
}

/// Like [`counted`], for a program that panics.
fn counted_to_the_end(src: &str) -> colibri::RcProfile {
    let r = run(src, true);
    assert!(r.ended.is_err());
    r.profile.expect("counting was started")
}

#[test]
fn the_items_of_a_map_and_of_an_iterator_are_counted_like_those_of_a_list() {
    let p = counted(
        "\
fn main() {
    let m = {\"k\": \"v\", \"j\": \"w\"}
    let mut n = 0
    for k in m.keys() {
        n += k.len()
    }
    for (k, v) in m {
        n += k.len() + v.len()
    }
    print(n)
}
",
    );
    assert_eq!(row(&p, 4), (3, 4, 0), "{p:?}");
    assert_eq!(row(&p, 7), (7, 7, 0), "{p:?}");
}

fn tree(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("colibri-rc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (path, text) in files {
        std::fs::write(dir.join(path), text).unwrap();
    }
    dir
}

const RELOADABLE: &str = "\
fn pick(xs: List[str]) -> int {
    return xs.len()
}
fn run() -> int {
    let xs = [\"a\"]
    return pick(xs)
}
";

/// Counting started, `RELOADABLE` loaded, then `edited` written over it and reloaded; what the
/// reload did and the profile after one call of `run`.
fn reloaded(name: &str, edited: &str) -> (colibri::ReloadOutcome, colibri::RcProfile) {
    let dir = tree(name, &[("a.colibri", RELOADABLE)]);
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .build();
    vm.start_rc_profile();
    vm.load_file(dir.join("a.colibri")).unwrap();
    std::fs::write(dir.join("a.colibri"), edited).unwrap();
    let outcome = vm.reload_changed().unwrap();
    let run_fn = vm.get_fn::<(), i64>("run").unwrap();
    run_fn.call(&mut vm, ()).unwrap();
    (outcome, vm.finish_rc_profile().unwrap())
}

#[test]
fn a_body_that_a_reload_patches_in_is_counted() {
    let (outcome, p) = reloaded(
        "patch",
        &RELOADABLE.replace("return pick(xs)", "return pick(xs) + pick(xs)"),
    );
    assert!(matches!(
        outcome,
        colibri::ReloadOutcome::Patched { functions: 1 }
    ));
    // Two calls, each copying the list and dropping the copy, and `return` drops `xs`.
    assert_eq!(row(&p, 6), (2, 3, 0), "{p:?}");
}

#[test]
fn a_program_that_a_reload_restarts_is_counted() {
    let (outcome, p) = reloaded("restart", &format!("{RELOADABLE}struct S {{ a: int }}\n"));
    assert!(matches!(outcome, colibri::ReloadOutcome::Restarted { .. }));
    assert_eq!(row(&p, 6), (1, 2, 0), "{p:?}");
}

#[test]
fn the_items_a_user_iterator_hands_out_are_counted() {
    let p = counted(
        "\
struct Names {
    n: int
}

impl Iter[str] for Names {
    fn next(mut self) -> str? {
        if self.n <= 0 {
            return none
        }
        self.n -= 1
        return some(\"n\")
    }
}

fn main() {
    let mut t = 0
    for s in (Names { n: 3 }) {
        t += s.len()
    }
    print(t)
}
",
    );
    // An item a trip, and what each trip and the end of the loop release.
    assert_eq!(row(&p, 17), (3, 5, 0), "{p:?}");
}
