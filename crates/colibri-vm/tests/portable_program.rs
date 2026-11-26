//! The portable copy against a real loaded program: the names in a refusal come from the
//! program's own type table and function bodies (L15).

use colibri_check::hir::FnId;
use colibri_core::portable::{Refused, StackGuard};
use colibri_core::release::Release;
use colibri_core::value::Value;
use colibri_syntax::{FileId, parse_file};
use colibri_vm::{Machine, VmFiber};

const SRC: &str = "
fn work() -> int { return 1 }
class Job { id: int, runner: Coroutine[int]? }
class Rows { rows: List[Job] }

fn make_closure() -> fn() -> bool {
    let job = go work()
    return fn() => job.done()
}

fn make_job() -> Job {
    return Job { id: 1, runner: some(go work()) }
}

fn make_opt() -> Job? {
    return some(Job { id: 3, runner: some(go work()) })
}

fn make_rows() -> Rows {
    let fine = Job { id: 1, runner: none }
    let bad = Job { id: 2, runner: some(go work()) }
    return Rows { rows: [fine, bad] }
}

fn make_fine() -> Rows {
    return Rows { rows: [Job { id: 1, runner: none }] }
}
";

fn load() -> (Machine, colibri_check::Program, VmFiber) {
    let (ast, _) = parse_file(FileId(0), SRC);
    let checked = colibri_check::check(&ast);
    let program = checked
        .program
        .unwrap_or_else(|| panic!("the program does not check: {:?}", checked.diagnostics));
    let mut m = Machine::new(Box::new(std::io::sink()), 200);
    m.load(&program);
    let mut fiber = VmFiber::new();
    m.run_module(&mut fiber).expect("module");
    (m, program, fiber)
}

fn call(m: &mut Machine, p: &colibri_check::Program, fiber: &mut VmFiber, name: &str) -> Value {
    let id = p
        .fns
        .iter()
        .position(|f| f.name == name)
        .unwrap_or_else(|| panic!("no function {name}"));
    m.call(fiber, FnId(id as u32), Vec::new())
        .expect("the call runs")
}

#[test]
fn refusals_in_a_real_program_name_types_fields_captures_and_elements() {
    let (mut m, p, mut fiber) = load();
    let stack = StackGuard::here(1 << 20);

    let closure = call(&mut m, &p, &mut fiber, "make_closure");
    let err = m.to_portable(&closure, stack).unwrap_err();
    assert_eq!(err.refused, Refused::Coroutine);
    assert_eq!(
        err.to_string(),
        "`closure.job` is a `Coroutine`, which cannot be sent to another thread"
    );

    // The `some` of the optional field is not a step the program writes.
    let job = call(&mut m, &p, &mut fiber, "make_job");
    let err = m.to_portable(&job, stack).unwrap_err();
    assert_eq!(err.path, "Job.runner");

    // The same when the root is itself a `some`: the path still starts at the type's name.
    let opt = call(&mut m, &p, &mut fiber, "make_opt");
    let err = m.to_portable(&opt, stack).unwrap_err();
    assert_eq!(err.path, "Job.runner");

    let rows = call(&mut m, &p, &mut fiber, "make_rows");
    let err = m.to_portable(&rows, stack).unwrap_err();
    assert_eq!(err.path, "Rows.rows[1].runner");

    // A value with nothing refused crosses.
    let fine = call(&mut m, &p, &mut fiber, "make_fine");
    assert!(m.to_portable(&fine, stack).is_ok());

    for v in [closure, job, opt, rows, fine] {
        m.release(v);
    }
    m.release_globals();
}

/// What the checker says about `task.run` of each of these, with the position of a list element
/// as `[_]`.
#[test]
fn the_checker_and_the_run_time_name_the_same_path() {
    let (mut m, p, mut fiber) = load();
    let stack = StackGuard::here(1 << 20);
    let source = format!(
        "{SRC}
fn static_closure() {{
    let job = go work()
    let t = task.run(fn() => job.done())
}}
fn static_job() {{
    let t = task.run(make_job)
}}
fn static_opt() {{
    let t = task.run(make_opt)
}}
fn static_rows() {{
    let t = task.run(make_rows)
}}
"
    );
    let (ast, _) = parse_file(FileId(0), &source);
    let checked = colibri_check::check(&ast);
    assert!(checked.program.is_none());
    let errors: Vec<&str> = checked
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(errors.len(), 4, "{errors:?}");
    for (function, static_message) in [
        ("make_closure", errors[0]),
        ("make_job", errors[1]),
        ("make_opt", errors[2]),
        ("make_rows", errors[3]),
    ] {
        let value = call(&mut m, &p, &mut fiber, function);
        let run_time = m.to_portable(&value, stack).unwrap_err().to_string();
        // A run-time path counts the elements it passed; the checker cannot.
        let digits = positions_as_blanks(&run_time);
        assert_eq!(static_message, digits, "{function}");
        m.release(value);
    }
    m.release_globals();
}

/// `[12]` becomes `[_]` and `[entry 3]` becomes `[entry _]`.
fn positions_as_blanks(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        let entry = out.ends_with("[entry ");
        if (c == '[' || entry) && chars.peek().is_some_and(|d| d.is_ascii_digit()) {
            while chars.peek().is_some_and(|d| d.is_ascii_digit()) {
                chars.next();
            }
            out.push('_');
        }
    }
    out
}

const SHAPES: &str = "
fn work() -> int { return 1 }
class Job { id: int, runner: Coroutine[int] }
enum Msg { Quit, Send(Job), Named(job: Job) }
class Node { id: int, job: Job? }

fn job() -> Job { return Job { id: 1, runner: go work() } }
fn s_enum() -> Msg { return Msg.Send(job()) }
fn s_tuple() -> (int, Job) { return (1, job()) }
fn s_map() -> Map[str, Job] { return {\"a\": job()} }
fn s_list() -> List[Job] { return [job()] }
fn s_node() -> Node { return Node { id: 1, job: some(job()) } }
fn s_msgs() -> List[Msg?] { return [some(Msg.Send(job()))] }
fn s_named() -> Msg { return Msg.Named(job: job()) }
";

/// The checker and the run time give the same message for seven shapes, up to three differences
/// that cannot be removed (the first is a trailer the checker adds to a path that starts with a
/// step, "of the result of the task"): a position in a list or a map is a number at run time and `_` in the
/// checker, and for an enum the checker names the first variant whose payload cannot be sent,
/// where the run time names the variant the value holds.
#[test]
fn seven_shapes_have_the_same_path_in_the_checker_and_at_run_time() {
    let (ast, _) = parse_file(FileId(0), SHAPES);
    let checked = colibri_check::check(&ast);
    let program = checked
        .program
        .unwrap_or_else(|| panic!("{:?}", checked.diagnostics));
    let mut m = Machine::new(Box::new(std::io::sink()), 200);
    m.load(&program);
    let mut fiber = VmFiber::new();
    m.run_module(&mut fiber).expect("module");
    let stack = StackGuard::here(1 << 20);

    let names = [
        "s_enum", "s_tuple", "s_map", "s_list", "s_node", "s_msgs", "s_named",
    ];
    let statics: String = names
        .iter()
        .map(|n| format!("fn t_{n}() {{\n    let t = task.run({n})\n}}\n"))
        .collect();
    let (ast, _) = parse_file(FileId(0), &format!("{SHAPES}{statics}"));
    let checked = colibri_check::check(&ast);
    let found: Vec<String> = checked
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.message.clone())
        .collect();
    assert_eq!(found.len(), names.len(), "{found:?}");

    let mut differing = Vec::new();
    for (name, static_message) in names.iter().zip(&found) {
        // A path that starts with a step says whose part it is.
        let static_message = static_message.replace(" of the result of the task", "");
        let id = program.fns.iter().position(|f| f.name == *name).unwrap();
        let value = m.call(&mut fiber, FnId(id as u32), Vec::new()).unwrap();
        let run_time = positions_as_blanks(&m.to_portable(&value, stack).unwrap_err().to_string());
        if run_time != static_message {
            differing.push((*name, run_time, static_message));
        }
        m.release(value);
    }
    m.release_globals();
    let want = "`Msg.Send.0.runner` is a `Coroutine`, which cannot be sent to another thread";
    let named = "`Msg.Named.job.runner` is a `Coroutine`, which cannot be sent to another thread";
    assert_eq!(
        differing,
        vec![("s_named", named.to_string(), want.to_string())]
    );
    // And the shapes that agree read as expected, with the variant named inside a path.
    assert!(
        found.contains(
            &"`.1.runner` of the result of the task is a `Coroutine`, which cannot be sent to another thread"
                .to_string()
        ),
        "{found:?}"
    );
    assert!(
        found.iter().any(|f| f.starts_with("`[_].Send.0.runner`")),
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .any(|f| f.starts_with("`[entry _].value.runner`")),
        "{found:?}"
    );
}
