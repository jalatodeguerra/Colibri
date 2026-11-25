//! The sampling profiler of the `Vm` (`start_profile`, `finish_profile`).

use colibri_runtime as colibri;

const PROGRAM: &str = "\
fn fib(n: int) -> int {
    if n < 2 { return n }
    return fib(n - 1) + fib(n - 2)
}
fn busy(n: int) -> int {
    let mut t = 0
    for i in 0..n {
        t += i
    }
    return t
}
fn main() {
    let c = go busy(3000)
    let ys = [1, 2, 3].map(fn(x) => x + fib(8))
    print(ys.len() + fib(12))
    print(c.join())
}
";

fn profile_of(weight: colibri::ProfileWeight, slice: Option<u32>) -> (colibri::Profile, u64) {
    let mut b = colibri::Vm::builder().stdout(Box::new(colibri::SharedBuffer::new()));
    if let Some(n) = slice {
        b = b.safepoint_slice(n);
    }
    let mut vm = b.build();
    vm.start_profile(weight);
    // `steps_used` is the total of the last entry from the host: add the entries up.
    vm.load_source("p.colibri", PROGRAM).unwrap();
    let mut used = vm.steps_used();
    vm.run_main().unwrap();
    used += vm.steps_used();
    // Nothing is left for the event loop: `c.join()` ran the coroutine inside `main`.
    vm.run_event_loop().unwrap();
    (vm.finish_profile().expect("a profile was started"), used)
}

#[test]
fn the_weights_of_a_step_profile_add_up_to_the_steps_the_program_took() {
    let (profile, used) = profile_of(colibri::ProfileWeight::Steps, None);
    assert!(used > 5_000, "the program is long enough: {used}");
    // The steps after the last sample are a sample of their own: nothing is lost.
    assert_eq!(profile.total(), used);
    assert!(profile.samples.len() >= 3, "{:?}", profile.samples);
}

#[test]
fn a_program_shorter_than_a_slice_still_has_all_its_steps_in_the_profile() {
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .build();
    vm.start_profile(colibri::ProfileWeight::Steps);
    vm.load_source("s.colibri", "fn main() {\n    print(1 + 2)\n}\n")
        .unwrap();
    let mut used = vm.steps_used();
    vm.run_main().unwrap();
    used += vm.steps_used();
    let profile = vm.finish_profile().unwrap();
    assert!(used > 0 && used < 1024, "{used}");
    assert_eq!(profile.total(), used);
}

#[test]
fn the_stacks_name_functions_a_coroutine_and_the_built_in() {
    let (profile, _) = profile_of(colibri::ProfileWeight::Steps, None);
    let names: std::collections::BTreeSet<&str> = profile
        .samples
        .iter()
        .flat_map(|s| s.stack.iter().map(String::as_str))
        .collect();
    assert!(names.contains("fib"), "{names:?}");
    assert!(names.contains("go busy"), "{names:?}");
    assert!(names.contains("List.map"), "{names:?}");
    assert!(
        !names.iter().any(|n| n.starts_with("__")),
        "the prelude's own names are folded: {names:?}"
    );
    // The coroutine's frames start at `go busy`, and nothing else is a root but `main`.
    assert!(
        profile
            .samples
            .iter()
            .all(|s| matches!(s.stack[0].as_str(), "main" | "go busy")),
        "{names:?}"
    );
}

#[test]
fn the_profile_is_the_same_on_every_run_and_the_lines_are_real() {
    let (a, _) = profile_of(colibri::ProfileWeight::Steps, None);
    let (b, _) = profile_of(colibri::ProfileWeight::Steps, None);
    assert_eq!(a, b);
    let fib_lines: Vec<u32> = a
        .samples
        .iter()
        .filter(|s| s.stack.last().map(String::as_str) == Some("fib"))
        .map(|s| s.line)
        .collect();
    assert!(!fib_lines.is_empty());
    // `fib` is the first three lines of the program.
    assert!(
        fib_lines.iter().all(|l| (1..=4).contains(l)),
        "{fib_lines:?}"
    );
    assert!(
        a.samples
            .iter()
            .all(|s| s.file == "p.colibri" || s.line == 0)
    );
}

#[test]
fn another_consumer_asking_for_a_smaller_slice_does_not_change_the_profile_total() {
    let (profile, used) = profile_of(colibri::ProfileWeight::Steps, Some(7));
    assert_eq!(profile.total(), used);
}

#[test]
fn a_time_profile_weighs_in_nanoseconds_and_finishing_twice_gives_none() {
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .build();
    assert!(vm.finish_profile().is_none());
    vm.start_profile(colibri::ProfileWeight::Time);
    vm.load_source("p.colibri", PROGRAM).unwrap();
    vm.run_main().unwrap();
    vm.run_event_loop().unwrap();
    let profile = vm.finish_profile().unwrap();
    assert_eq!(profile.weight, colibri::ProfileWeight::Time);
    assert!(profile.total() > 0);
    assert!(vm.finish_profile().is_none());
    // The profiler is off again: no safepoints run any more.
    let before = vm.safepoints_run();
    vm.run_main().unwrap();
    assert_eq!(vm.safepoints_run(), before);
}

#[test]
fn the_helpers_a_built_in_calls_fold_into_the_built_in() {
    // `sort_by` runs prelude helpers that call each other (`__sort_order`, `__merge_order`):
    // the profile shows one `List.sort_by` frame, then the user's closure.
    let src = "\
fn main() {
    let mut xs: List[int] = []
    for i in 0..600 {
        xs.push((i * 7919) % 601)
    }
    xs.sort_by(fn(a, b) => a - b)
    print(xs[0])
}
";
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .build();
    vm.start_profile(colibri::ProfileWeight::Steps);
    vm.load_source("s.colibri", src).unwrap();
    vm.run_main().unwrap();
    let profile = vm.finish_profile().unwrap();
    let sorting: Vec<&Vec<String>> = profile
        .samples
        .iter()
        .map(|s| &s.stack)
        .filter(|s| s.iter().any(|n| n == "List.sort_by"))
        .collect();
    assert!(!sorting.is_empty(), "{:?}", profile.samples);
    for stack in sorting {
        assert_eq!(stack[..2], ["main", "List.sort_by"], "{stack:?}");
        assert!(
            stack[2..].iter().all(|n| n == "<closure in main>"),
            "the helpers fold away: {stack:?}"
        );
    }
}

#[test]
fn a_very_deep_stack_keeps_its_first_and_last_frames_with_a_marker_between() {
    let src = "\
fn down(n: int) -> int {
    if n == 0 { return 0 }
    return down(n - 1) + 1
}
fn main() {
    let mut t = 0
    for i in 0..20 {
        t += down(900)
    }
    print(t)
}
";
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .build();
    vm.start_profile(colibri::ProfileWeight::Steps);
    vm.load_source("d.colibri", src).unwrap();
    vm.run_main().unwrap();
    let used = vm.steps_used();
    let profile = vm.finish_profile().unwrap();
    // The cut loses no weight.
    assert!(profile.total() >= used && profile.total() <= used + 1);
    let longest = profile.samples.iter().map(|s| s.stack.len()).max().unwrap();
    assert_eq!(longest, 201, "40 first, the marker, 160 last");
    let cut = profile
        .samples
        .iter()
        .find(|s| s.stack.len() == 201)
        .expect("a deep sample");
    assert_eq!(cut.stack[0], "main");
    assert_eq!(cut.stack[40], "[...]");
    assert!(cut.stack[41..].iter().all(|n| n == "down"));
    assert!(cut.line > 0, "the position is the innermost frame's");
}
