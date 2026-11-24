//! `find_fn` for optional hooks, and `Callback`: script functions the host keeps (EMBEDDING §6).

mod common;
use common::vm_pending;

use colibri_runtime as colibri;

type Hook = colibri::Callback<(i64,), i64>;

#[derive(Default)]
struct Hooks {
    all: Vec<Hook>,
    unit: Vec<colibri::Callback<(), ()>>,
}

fn vm() -> (colibri::Vm, colibri::SharedBuffer) {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    vm.insert_state(Hooks::default());
    vm.register_fn("on_hit", |cx: &mut colibri::Ctx, hook: Hook| {
        cx.state_mut::<Hooks>().unwrap().all.push(hook);
    })
    .unwrap();
    vm.register_fn(
        "on_tick",
        |cx: &mut colibri::Ctx, hook: colibri::Callback<(), ()>| {
            cx.state_mut::<Hooks>().unwrap().unit.push(hook);
        },
    )
    .unwrap();
    (vm, out)
}

const SCRIPT: &str = r#"
let mut total = 0

fn bonus(n: int) -> int => n + 1000

fn setup() {
    let base = 10
    on_hit(fn(n: int) => n * base)
    on_hit(bonus)
    on_tick(fn() {
        total += 1
        print("tick {total}")
    })
}
"#;

#[test]
fn closures_and_functions_outlive_the_call_that_registered_them() {
    let (mut vm, out) = vm();
    vm.load_source("game.colibri", SCRIPT).unwrap();
    vm.get_fn::<(), ()>("setup")
        .unwrap()
        .call(&mut vm, ())
        .unwrap();
    let hooks = vm.state_mut::<Hooks>().unwrap();
    let (all, unit) = (hooks.all.clone(), hooks.unit.clone());
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].call(&mut vm, (4,)).unwrap(), 40);
    assert_eq!(all[1].call(&mut vm, (4,)).unwrap(), 1004);
    // A closure keeps what it captured, and changes module state.
    unit[0].call(&mut vm, ()).unwrap();
    unit[0].call(&mut vm, ()).unwrap();
    assert_eq!(out.contents(), "tick 1\ntick 2\n");
    // Clones share one root.
    let again = all[0].clone();
    assert_eq!(again.call(&mut vm, (1,)).unwrap(), 10);
    assert!(format!("{again:?}").starts_with("Callback"));
}

#[test]
fn a_callback_panic_is_an_error_and_the_vm_stays_usable() {
    let (mut vm, _) = vm();
    vm.load_source(
        "game.colibri",
        "fn setup() {\n    on_hit(fn(n: int) => 100 / n)\n}\n",
    )
    .unwrap();
    vm.get_fn::<(), ()>("setup")
        .unwrap()
        .call(&mut vm, ())
        .unwrap();
    let hook = vm.state_mut::<Hooks>().unwrap().all[0].clone();
    assert_eq!(hook.call(&mut vm, (5,)).unwrap(), 20);
    let err = hook.call(&mut vm, (0,)).unwrap_err();
    assert!(
        matches!(&err, colibri::Error::Panic { message, .. } if message == "division by zero"),
        "{err:?}"
    );
    assert_eq!(hook.call(&mut vm, (50,)).unwrap(), 2);
}

#[test]
fn a_host_function_calls_a_callback_through_its_context() {
    let (mut vm, out) = vm();
    vm.register_fn("fire", |cx: &mut colibri::Ctx, n: i64| -> Vec<i64> {
        let hooks = cx.state_mut::<Hooks>().unwrap().all.clone();
        hooks.iter().map(|h| h.call(cx, (n,)).unwrap()).collect()
    })
    .unwrap();
    vm.load_source(
        "game.colibri",
        &format!("{SCRIPT}\nfn main() {{\n    setup()\n    print(fire(3))\n}}\n"),
    )
    .unwrap();
    vm.run_main().unwrap();
    assert_eq!(out.contents(), "[30, 1003]\n");
}

#[test]
fn a_callback_goes_stale_when_the_program_goes() {
    let (mut vm, _) = vm();
    vm.load_source("game.colibri", SCRIPT).unwrap();
    vm.get_fn::<(), ()>("setup")
        .unwrap()
        .call(&mut vm, ())
        .unwrap();
    let hook = vm.state_mut::<Hooks>().unwrap().all[0].clone();
    assert_eq!(hook.call(&mut vm, (1,)).unwrap(), 10);
    vm.shutdown();
    assert!(matches!(
        hook.call(&mut vm, (1,)),
        Err(colibri::Error::StaleHandle)
    ));
    // A function handle is stale too, and so is a new program's view of the old callback.
    assert!(matches!(
        vm.get_fn::<(), ()>("setup"),
        Err(colibri::Error::NotLoaded)
    ));
    vm.load_source("game.colibri", SCRIPT).unwrap();
    assert!(matches!(
        hook.call(&mut vm, (1,)),
        Err(colibri::Error::StaleHandle)
    ));
}

#[test]
fn a_callback_belongs_to_its_vm() {
    let (mut vm, _) = vm();
    vm.load_source("game.colibri", SCRIPT).unwrap();
    vm.get_fn::<(), ()>("setup")
        .unwrap()
        .call(&mut vm, ())
        .unwrap();
    let hook = vm.state_mut::<Hooks>().unwrap().all[0].clone();
    let (mut other, _) = self::vm();
    other.load_source("game.colibri", SCRIPT).unwrap();
    assert!(matches!(
        hook.call(&mut other, (1,)),
        Err(colibri::Error::StaleHandle)
    ));
    // After the Vm is gone entirely, dropping the callback is harmless.
    drop(vm);
    drop(hook);
}

#[test]
fn a_dropped_callback_releases_its_closure_at_the_next_entry() {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
    vm.insert_state(Hooks::default());
    vm.register_fn(
        "keep",
        |cx: &mut colibri::Ctx, hook: colibri::Callback<(), ()>| {
            cx.state_mut::<Hooks>().unwrap().unit.push(hook);
        },
    )
    .unwrap();
    vm.load_source(
        "game.colibri",
        r#"
class Res { label: str }

impl Drop for Res {
    fn drop(self) { print("drop {self.label}") }
}

fn setup() {
    let r = Res { label: "captured" }
    keep(fn() { print("using {r.label}") })
}

fn noop() {
}
"#,
    )
    .unwrap();
    vm.get_fn::<(), ()>("setup")
        .unwrap()
        .call(&mut vm, ())
        .unwrap();
    // The closure keeps the object alive.
    assert_eq!(out.contents(), "");
    let hook = vm.state_mut::<Hooks>().unwrap().unit.pop().unwrap();
    hook.call(&mut vm, ()).unwrap();
    assert_eq!(out.take(), "using captured\n");
    // Dropping the callback does not run the hook on the spot: the host's code is not a safe
    // place for a script's `Drop` hook.
    drop(hook);
    assert_eq!(out.contents(), "");
    // The next entry into the Vm releases it.
    vm.get_fn::<(), ()>("noop")
        .unwrap()
        .call(&mut vm, ())
        .unwrap();
    assert_eq!(out.contents(), "drop captured\n");
}

#[test]
fn a_callback_with_the_wrong_signature_is_a_compile_error() {
    let (mut vm, _) = vm();
    let err = vm
        .load_source("game.colibri", "fn f() {\n    on_hit(fn(s: str) => 1)\n}\n")
        .unwrap_err();
    let text = err.render(false);
    assert!(text.contains("fn(int) -> int"), "{text}");
    assert!(text.contains("fn(str)"), "{text}");
}

#[test]
fn find_fn_is_for_optional_hooks() {
    let (mut vm, _) = vm();
    vm.load_source(
        "game.colibri",
        "fn update(dt: float) {\n}\nfn on_death(e: int) -> int => e\n",
    )
    .unwrap();
    let update = vm.find_fn::<(f64,), ()>("update").unwrap();
    assert!(update.is_some());
    update.unwrap().call(&mut vm, (0.016,)).unwrap();
    // Not defined: no error, just nothing.
    assert!(vm.find_fn::<(i64,), ()>("on_spawn").unwrap().is_none());
    // Defined with another signature: an error, so a typo in a hook is not silent.
    let err = vm.find_fn::<(f64,), i64>("on_death").err().unwrap();
    assert!(
        matches!(err, colibri::Error::SignatureMismatch { .. }),
        "{err:?}"
    );
    // Without a program it is the usual error.
    let (empty, _) = self::vm();
    assert!(matches!(
        empty.find_fn::<(), ()>("x"),
        Err(colibri::Error::NotLoaded)
    ));
}
