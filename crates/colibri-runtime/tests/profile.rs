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
    let (profile, used, _) = profile_of_with(weight, slice);
    (profile, used)
}

/// The profile, the steps the program took, and how many safepoints ran in all.
fn profile_of_with(
    weight: colibri::ProfileWeight,
    slice: Option<u32>,
) -> (colibri::Profile, u64, u64) {
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
    let ran = vm.safepoints_run();
    (
        vm.finish_profile().expect("a profile was started"),
        used,
        ran,
    )
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
    // No sample was taken, so there is no stack to put the steps on.
    assert_eq!(profile.samples.len(), 1);
    assert_eq!(profile.samples[0].stack, ["[unsampled]"]);
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
fn another_consumer_with_a_smaller_slice_does_not_change_the_profile_total() {
    // The hook registers a second consumer (the debugger's kind) with a slice of 7 steps; the
    // profiler goes on with its own 1,024.
    let (profile, used, safepoints) = profile_of_with(colibri::ProfileWeight::Steps, Some(7));
    assert_eq!(profile.total(), used);
    assert!(
        safepoints > used / 14,
        "the 7-step consumer ran: {safepoints} for {used} steps"
    );
    // Each sample but the last stands for whole 1,024-step intervals.
    let mut weights: Vec<u64> = profile.samples.iter().map(|s| s.weight).collect();
    weights.sort_unstable();
    assert!(
        weights.iter().filter(|w| *w % 1024 != 0).count() <= 2,
        "{weights:?}"
    );
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
    let mut used = vm.steps_used();
    vm.run_main().unwrap();
    used += vm.steps_used();
    let profile = vm.finish_profile().unwrap();
    // The cut loses no weight.
    assert_eq!(profile.total(), used);
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

#[test]
fn a_run_that_exhausts_its_budget_still_adds_up_to_the_steps_it_took() {
    let src = "\
fn spin() {
    let mut t = 0
    defer { t += 1 }
    while true {
        t += 1
    }
}
";
    for budget in [3000, 50_000] {
        let mut vm = colibri::Vm::builder()
            .stdout(Box::new(colibri::SharedBuffer::new()))
            .instruction_budget(budget)
            .build();
        vm.start_profile(colibri::ProfileWeight::Steps);
        vm.load_source("e.colibri", src).unwrap();
        let load = vm.steps_used();
        let spin = vm.get_fn::<(), ()>("spin").unwrap();
        let err = spin.call(&mut vm, ()).unwrap_err();
        assert!(
            matches!(err, colibri::Error::BudgetExhausted { .. }),
            "{err:?}"
        );
        let used = load + vm.steps_used();
        let profile = vm.finish_profile().unwrap();
        assert!(used > budget);
        assert_eq!(profile.total(), used, "budget {budget}");
    }
}

#[test]
fn a_host_function_that_calls_back_shows_as_native_with_the_script_under_it() {
    let src = "\
fn work(i: int) -> int {
    let mut t = 0
    for j in 0..400 {
        t += j + i
    }
    return t
}
fn main() {
    each(20, fn(i: int) => work(i))
}
";
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .build();
    vm.register_fn(
        "each",
        |cx: &mut colibri::Ctx, n: i64, f: colibri::Callback<(i64,), i64>| {
            for i in 0..n {
                f.call(cx, (i,)).unwrap();
            }
        },
    )
    .unwrap();
    vm.start_profile(colibri::ProfileWeight::Steps);
    vm.load_source("n.colibri", src).unwrap();
    let mut used = vm.steps_used();
    vm.run_main().unwrap();
    used += vm.steps_used();
    let profile = vm.finish_profile().unwrap();
    assert_eq!(profile.total(), used);
    let heavy = profile
        .samples
        .iter()
        .max_by_key(|s| s.weight)
        .expect("samples");
    assert_eq!(
        heavy.stack,
        ["main", "[native: each]", "<closure in main>", "work"]
    );
}

#[test]
fn steps_after_a_native_loop_are_charged_to_the_caller_not_to_the_next_call() {
    // `xs.map` over 3000 elements runs as one native loop that charges thousands of steps at
    // once; the short function after it must not be billed for them.
    let src = "\
fn short(n: int) -> int {
    if n == 0 { return 0 }
    return short(n - 1) + 1
}
fn main() {
    let mut xs: List[int] = []
    for i in 0..3000 {
        xs.push(i)
    }
    let ys = xs.map(fn(x) => x + 1)
    print(ys.len())
    print(short(30))
}
";
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .build();
    vm.start_profile(colibri::ProfileWeight::Steps);
    vm.load_source("m.colibri", src).unwrap();
    vm.run_main().unwrap();
    let profile = vm.finish_profile().unwrap();
    let under = |name: &str| -> u64 {
        profile
            .samples
            .iter()
            .filter(|s| s.stack.iter().any(|n| n == name))
            .map(|s| s.weight)
            .sum()
    };
    assert!(vm.safepoints_run() > 4);
    assert!(
        under("short") <= 1024,
        "`short` runs about 90 steps but holds {}",
        under("short")
    );
    // The native loop runs inside the call of `map` (line 10), the frame on top of the stack.
    let at_map: u64 = profile
        .samples
        .iter()
        .filter(|s| s.line == 10)
        .map(|s| s.weight)
        .sum();
    assert!(at_map >= 4096, "{:?}", profile.samples);
}

#[test]
fn coroutines_that_the_event_loop_resumes_are_profiled_with_their_own_root() {
    let src = "\
fn busy(n: int) -> int {
    let mut t = 0
    for i in 0..n {
        t += i
    }
    return t
}
fn sleeper() {
    for k in 0..2 {
        wait(1.0)
        busy(2000)
    }
}
fn main() {
    go sleeper()
    busy(4000)
}
";
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .clock(FastClock::default())
        .build();
    vm.start_profile(colibri::ProfileWeight::Steps);
    vm.load_source("s.colibri", src).unwrap();
    vm.run_main().unwrap();
    vm.run_event_loop().unwrap();
    let profile = vm.finish_profile().unwrap();
    let under = |root: &str| -> u64 {
        profile
            .samples
            .iter()
            .filter(|s| s.stack[0] == root)
            .map(|s| s.weight)
            .sum()
    };
    let (main, sleeper) = (under("main"), under("go sleeper"));
    // 4000 iterations in `main`, 2 x 2000 in the coroutine, the same loop body: the weights
    // are about equal (a sample is off by up to one slice where the stack changes).
    assert!(main > 4000 && sleeper > 4000, "{main} {sleeper}");
    let ratio = sleeper as f64 / main as f64;
    assert!((0.7..1.4).contains(&ratio), "{main} {sleeper}");
    assert!(
        profile
            .samples
            .iter()
            .any(|s| s.stack == ["go sleeper", "busy"])
    );
    assert_eq!(profile.total(), main + sleeper);
}

/// A clock that moves by what the program sleeps and takes real time to do it.
#[derive(Default)]
struct FastClock {
    now: f64,
}

impl colibri::Clock for FastClock {
    fn now(&mut self) -> f64 {
        self.now
    }
    fn sleep(&mut self, seconds: f64) {
        self.now += seconds;
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

#[test]
fn the_time_the_event_loop_sleeps_is_idle_and_not_charged_to_the_work() {
    let src = "\
fn busy(n: int) -> int {
    let mut t = 0
    for i in 0..n {
        t += i
    }
    return t
}
fn sleeper() {
    for k in 0..2 {
        wait(30.0)
        busy(500)
    }
}
fn main() {
    go sleeper()
    busy(2000)
}
";
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(colibri::SharedBuffer::new()))
        .clock(FastClock::default())
        .build();
    vm.start_profile(colibri::ProfileWeight::Time);
    vm.load_source("i.colibri", src).unwrap();
    vm.run_main().unwrap();
    vm.run_event_loop().unwrap();
    let profile = vm.finish_profile().unwrap();
    let ms = |idle: bool| -> f64 {
        profile
            .samples
            .iter()
            .filter(|s| (s.stack[0] == "[idle]") == idle)
            .map(|s| s.weight)
            .sum::<u64>() as f64
            / 1e6
    };
    // Two sleeps of at least 150 ms are idle; the work between them is a few milliseconds.
    assert!(ms(true) >= 290.0, "idle {} ms", ms(true));
    assert!(ms(false) < 140.0, "work {} ms", ms(false));
}
