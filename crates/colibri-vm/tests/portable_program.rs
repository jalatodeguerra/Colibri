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

/// `[12]` becomes `[_]`.
fn positions_as_blanks(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '[' && chars.peek().is_some_and(|d| d.is_ascii_digit()) {
            while chars.peek().is_some_and(|d| d.is_ascii_digit()) {
                chars.next();
            }
            out.push('_');
        }
    }
    out
}
