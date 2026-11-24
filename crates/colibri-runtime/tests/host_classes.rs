//! Host classes: Rust objects that scripts hold (EMBEDDING §2, §3), written by hand with
//! `HostType` (the `export` macros have their own tests).

mod common;
use common::vm_pending;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use colibri::{Callback, Ctx, Handle, HostType, Ref, TypeBuilder};
use colibri_runtime as colibri;

type Log = Rc<RefCell<Vec<String>>>;

fn push(log: &Log, text: impl Into<String>) {
    log.borrow_mut().push(text.into());
}

/// A thing with a name whose Rust `Drop` is logged.
struct Enemy {
    name: String,
    hp: i64,
    log: Log,
}

impl Drop for Enemy {
    fn drop(&mut self) {
        push(&self.log, format!("rust drop {}", self.name));
    }
}

impl HostType for Enemy {
    const NAME: &'static str = "Enemy";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.method("name", [], |this: &Enemy| this.name.clone());
        d.method("hp", [], |this: &Enemy| this.hp);
        d.method_mut("hit", ["dmg"], |this: &mut Enemy, dmg: i64| {
            this.hp -= dmg;
            this.hp
        });
        d.method("same_name", ["other"], |this: &Enemy, other: Ref<Enemy>| {
            this.name == other.name
        });
    }
}

/// A world whose `step` calls back into the script.
struct World {
    ticks: i64,
}

impl HostType for World {
    const NAME: &'static str = "World";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.static_fn("new", [], || World { ticks: 0 });
        d.method_mut(
            "step",
            ["on_tick"],
            |cx: &mut Ctx, this: &mut World, on_tick: Callback<(), ()>| {
                this.ticks += 1;
                on_tick.call(cx, ()).map_err(|e| e.to_string())
            },
        );
        d.method("ticks", [], |this: &World| this.ticks);
        d.method(
            "watch",
            ["on_tick"],
            |cx: &mut Ctx, _this: &World, on_tick: Callback<(), ()>| {
                on_tick.call(cx, ()).map_err(|e| e.to_string())
            },
        );
        d.method_mut(
            "poke",
            ["on_tick"],
            |cx: &mut Ctx, _this: &mut World, on_tick: Callback<(), ()>| {
                on_tick.call(cx, ()).map_err(|e| e.to_string())
            },
        );
    }
}

struct Fixture {
    vm: colibri::Vm,
    out: colibri::SharedBuffer,
    log: Log,
}

fn fixture() -> Fixture {
    let out = colibri::SharedBuffer::new();
    let mut vm = colibri::Vm::builder()
        .stdout(Box::new(out.clone()))
        .trace_drops(true)
        .build();
    let log: Log = Rc::default();
    let sink = Rc::clone(&log);
    vm.register_fn("log", move |text: String| push(&sink, text))
        .unwrap();
    let maker = Rc::clone(&log);
    vm.register_fn("spawn", move |name: String| Enemy {
        name,
        hp: 10,
        log: Rc::clone(&maker),
    })
    .unwrap();
    Fixture { vm, out, log }
}

impl Fixture {
    fn run(&mut self, src: &str) -> Result<(), colibri::Error> {
        self.vm.load_source("game.colibri", src)?;
        self.vm.run_main()
    }
}

fn panic_message(result: Result<(), colibri::Error>) -> String {
    match result {
        Err(colibri::Error::Panic { message, .. }) => message,
        other => panic!("expected a panic, got {other:?}"),
    }
}

#[test]
fn a_returned_object_is_script_owned_and_dies_with_its_last_reference() {
    let mut f = fixture();
    f.run(
        r#"
fn scope() {
let e = spawn("orc")
let same = e
log("made {same.name()} with {e.hp()} hp")
}

fn main() {
scope()
log("after")
}
"#,
    )
    .unwrap();
    assert_eq!(
        *f.log.borrow(),
        ["made orc with 10 hp", "rust drop orc", "after"]
    );
    assert_eq!(f.vm.host_object_count(), 0);
}

#[test]
fn the_rust_drop_runs_after_the_class_drop_hook_that_held_it() {
    let mut f = fixture();
    f.run(
        r#"
class Holder { name: str, enemy: Enemy }

impl Drop for Holder {
fn drop(self) { log("hook {self.name} sees {self.enemy.name()}") }
}

fn scope() {
let h = Holder { name: "holder", enemy: spawn("orc") }
log("made")
}

fn main() {
scope()
log("after")
}
"#,
    )
    .unwrap();
    assert_eq!(
        *f.log.borrow(),
        ["made", "hook holder sees orc", "rust drop orc", "after"]
    );
    // The drop trace names the class object by its `name` field and the host object by type.
    assert_eq!(f.vm.take_drop_trace(), ["holder", "Enemy"]);
}

#[test]
fn a_host_object_dying_with_a_list_dies_in_element_order() {
    vm_pending(
        "a_host_object_dying_with_a_list_dies_in_element_order",
        || {
            let mut f = fixture();
            f.run(
                r#"
fn main() {
    let list = [spawn("a"), spawn("b"), spawn("c")]
    log("count {list.len()}")
    list.clear()
    log("cleared")
}
"#,
            )
            .unwrap();
            assert_eq!(
                *f.log.borrow(),
                [
                    "count 3",
                    "rust drop a",
                    "rust drop b",
                    "rust drop c",
                    "cleared"
                ]
            );
        },
    )
}

#[test]
fn a_host_owned_object_survives_every_script_reference() {
    let mut f = fixture();
    f.vm.register_type::<Enemy>().unwrap();
    let boss = f.vm.insert(Enemy {
        name: "boss".into(),
        hp: 100,
        log: Rc::clone(&f.log),
    });
    f.vm.register_fn("boss", move || boss).unwrap();
    f.run(
        r#"
fn scope() {
let b = boss()
let c = b
log("hit to {c.hit(30)}")
}

fn main() {
scope()
scope()
log("after")
}
"#,
    )
    .unwrap();
    assert_eq!(*f.log.borrow(), ["hit to 70", "hit to 40", "after"]);
    assert!(f.vm.is_alive(boss));
    assert_eq!(f.vm.get(boss).unwrap().hp, 40);
    f.vm.get_mut(boss).unwrap().hp = 5;
    assert_eq!(f.vm.get(boss).unwrap().hp, 5);
    let taken =
        f.vm.remove(boss)
            .unwrap()
            .expect("the host removes its own");
    assert_eq!(taken.name, "boss");
    // It is the host's object now: no Rust drop yet.
    assert_eq!(*f.log.borrow(), ["hit to 70", "hit to 40", "after"]);
    drop(taken);
    assert_eq!(f.log.borrow().last().unwrap(), "rust drop boss");
}

#[test]
fn a_script_that_still_holds_a_removed_object_is_told_so() {
    let mut f = fixture();
    f.vm.register_type::<Enemy>().unwrap();
    let boss = f.vm.insert(Enemy {
        name: "boss".into(),
        hp: 100,
        log: Rc::clone(&f.log),
    });
    f.vm.register_fn("boss", move || boss).unwrap();
    f.vm.load_source(
        "game.colibri",
        r#"
let kept = boss()

fn check() -> bool => kept.alive()
fn poke() -> int => kept.hp()
"#,
    )
    .unwrap();
    let check = f.vm.get_fn::<(), bool>("check").unwrap();
    assert!(check.call(&mut f.vm, ()).unwrap());
    f.vm.remove(boss).unwrap();
    assert!(!f.vm.is_alive(boss));
    assert!(!check.call(&mut f.vm, ()).unwrap());
    let poke = f.vm.get_fn::<(), i64>("poke").unwrap();
    match poke.call(&mut f.vm, ()) {
        Err(colibri::Error::Panic { message, .. }) => {
            assert_eq!(message, "`Enemy` was destroyed by the host");
        }
        other => panic!("{other:?}"),
    }
    // The host side reports it too, and a second removal finds nothing.
    assert!(matches!(
        f.vm.get(boss),
        Err(colibri::Error::HostObject(m)) if m == "`Enemy` was destroyed by the host"
    ));
    assert!(f.vm.remove(boss).unwrap().is_none());
}

#[test]
fn a_reused_slot_does_not_revive_an_old_handle() {
    let mut f = fixture();
    f.vm.register_type::<Enemy>().unwrap();
    let make = |name: &str, log: &Log| Enemy {
        name: name.into(),
        hp: 1,
        log: Rc::clone(log),
    };
    let first = f.vm.insert(make("first", &f.log));
    f.vm.remove(first).unwrap();
    let second = f.vm.insert(make("second", &f.log));
    assert_eq!(first.index(), second.index());
    assert_ne!(first.generation(), second.generation());
    assert!(!f.vm.is_alive(first));
    assert!(f.vm.is_alive(second));
    assert!(f.vm.get(first).is_err());
    assert_eq!(f.vm.get(second).unwrap().name, "second");
}

#[test]
fn an_option_of_a_stale_handle_is_none_for_the_script() {
    let mut f = fixture();
    f.vm.register_type::<Enemy>().unwrap();
    let live = f.vm.insert(Enemy {
        name: "live".into(),
        hp: 1,
        log: Rc::clone(&f.log),
    });
    let dead = f.vm.insert(Enemy {
        name: "dead".into(),
        hp: 1,
        log: Rc::clone(&f.log),
    });
    f.vm.remove(dead).unwrap();
    f.vm.register_fn("lookup", move |which: i64| -> Option<Handle<Enemy>> {
        match which {
            0 => Some(live),
            1 => Some(dead),
            _ => None,
        }
    })
    .unwrap();
    f.run(
        r#"
fn show(i: int) {
match lookup(i) {
    some(e) => log("found {e.name()}"),
    none => log("nothing for {i}"),
}
}

fn main() {
show(0)
show(1)
show(2)
}
"#,
    )
    .unwrap();
    assert_eq!(
        *f.log.borrow(),
        [
            // `remove` handed the object back and the host dropped it.
            "rust drop dead",
            "found live",
            "nothing for 1",
            "nothing for 2"
        ]
    );
}

#[test]
fn a_busy_object_gives_the_documented_panic_when_the_script_reenters_it() {
    vm_pending(
        "a_busy_object_gives_the_documented_panic_when_the_script_reenters_it",
        || {
            let mut f = fixture();
            f.vm.register_type::<World>().unwrap();
            // `step` holds the world exclusively; the callback asks it for its ticks.
            let err = f
                .run(
                    r#"
fn main() {
    let w = World.new()
    w.step(fn() { log("ticks {w.ticks()}") }).expect("step")
}
"#,
                )
                .unwrap_err();
            let message = match &err {
                colibri::Error::Panic { message, .. } => message.clone(),
                other => panic!("{other:?}"),
            };
            // The callback's panic travels out of `step` as an `err` result string the script did not
            // handle; the first line is the busy-object text.
            assert!(
                message.contains(
                    "host object `World` is already in use by an enclosing call (`World.step`)"
                ),
                "{message}"
            );
        },
    )
}

#[test]
fn reading_methods_may_nest_but_not_inside_a_writing_one() {
    vm_pending(
        "reading_methods_may_nest_but_not_inside_a_writing_one",
        || {
            let mut f = fixture();
            f.vm.register_type::<World>().unwrap();
            // `watch` is `&self`: the callback may read again.
            f.run(
                r#"
fn main() {
    let w = World.new()
    w.watch(fn() { log("ticks {w.ticks()}") }).expect("watch")
}
"#,
            )
            .unwrap();
            assert_eq!(*f.log.borrow(), ["ticks 0"]);
            // But a writing method reached through a reading one is busy, and says which call holds it.
            let mut f = fixture();
            f.vm.register_type::<World>().unwrap();
            let err = f
                .run(
                    r#"
fn main() {
    let w = World.new()
    w.watch(fn() { w.poke(fn() { }).expect("poke") }).expect("watch")
}
"#,
                )
                .unwrap_err();
            assert!(
                err.to_string().contains(
                    "host object `World` is already in use by an enclosing call (`World.watch`)"
                ),
                "{err}"
            );
        },
    )
}

#[test]
fn removing_a_borrowed_object_is_an_error_not_a_free() {
    vm_pending("removing_a_borrowed_object_is_an_error_not_a_free", || {
        let mut f = fixture();
        f.vm.register_type::<World>().unwrap();
        let world = f.vm.insert(World { ticks: 0 });
        let verdict = Rc::new(RefCell::new(String::new()));
        let seen = Rc::clone(&verdict);
        f.vm.register_fn("the_world", move || world).unwrap();
        f.vm.register_fn("remove_it", move |cx: &mut Ctx, w: Handle<World>| {
            let result = cx.remove(w);
            *seen.borrow_mut() = match result {
                Ok(_) => "removed".to_string(),
                Err(e) => e.to_string(),
            };
        })
        .unwrap();
        f.run(
            r#"
fn main() {
    let w = the_world()
    w.step(fn() { remove_it(w) })
    log("alive {w.alive()}")
}
"#,
        )
        .unwrap();
        assert_eq!(
            *verdict.borrow(),
            "cannot remove the host object `World`: it is in use by an enclosing call (`World.step`)"
        );
        assert_eq!(*f.log.borrow(), ["alive true"]);
        assert!(f.vm.is_alive(world));
        // Between calls nothing holds it, so now it can go.
        assert!(f.vm.remove(world).unwrap().is_some());
    })
}

#[test]
fn the_host_cannot_borrow_what_a_running_method_holds() {
    vm_pending("the_host_cannot_borrow_what_a_running_method_holds", || {
        let mut f = fixture();
        f.vm.register_type::<World>().unwrap();
        let world = f.vm.insert(World { ticks: 0 });
        let reply = Rc::new(RefCell::new(Vec::<String>::new()));
        let seen = Rc::clone(&reply);
        f.vm.register_fn("the_world", move || world).unwrap();
        f.vm.register_fn("peek", move |cx: &mut Ctx, w: Handle<World>| {
            seen.borrow_mut().push(match cx.get(w) {
                Ok(r) => format!("read {}", r.ticks),
                Err(e) => e.to_string(),
            });
            seen.borrow_mut().push(match cx.get_mut(w) {
                Ok(_) => "wrote".to_string(),
                Err(e) => e.to_string(),
            });
        })
        .unwrap();
        f.run(
            r#"
fn main() {
    let w = the_world()
    w.step(fn() { peek(w) })
    w.watch(fn() { peek(w) })
    peek(w)
}
"#,
        )
        .unwrap();
        let busy = |holder: &str| {
            format!("host object `World` is already in use by an enclosing call (`{holder}`)")
        };
        assert_eq!(
            *reply.borrow(),
            [
                busy("World.step"),
                busy("World.step"),
                "read 1".to_string(),
                busy("World.watch"),
                "read 1".to_string(),
                "wrote".to_string()
            ]
        );
    })
}

#[test]
fn a_rust_panic_in_a_method_releases_the_borrow() {
    struct Fragile;
    impl HostType for Fragile {
        const NAME: &'static str = "Fragile";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.static_fn("new", [], || Fragile);
            d.method_mut("snap", [], |_: &mut Fragile| -> i64 { panic!("snapped") });
            d.method("fine", [], |_: &Fragile| 1i64);
        }
    }
    let mut f = fixture();
    f.vm.register_type::<Fragile>().unwrap();
    f.vm.load_source(
        "game.colibri",
        "let f = Fragile.new()\nfn snap() { f.snap() }\nfn fine() -> int => f.fine()\n",
    )
    .unwrap();
    let snap = f.vm.get_fn::<(), ()>("snap").unwrap();
    let err = snap.call(&mut f.vm, ()).unwrap_err();
    assert!(
        err.to_string()
            .contains("host function `Fragile.snap` panicked: snapped"),
        "{err}"
    );
    // The exclusive borrow was released on the way out, so the object is usable again.
    let fine = f.vm.get_fn::<(), i64>("fine").unwrap();
    assert_eq!(fine.call(&mut f.vm, ()).unwrap(), 1);
}

#[test]
fn objects_have_identity_equality_hash_and_show() {
    vm_pending("objects_have_identity_equality_hash_and_show", || {
        let mut f = fixture();
        f.run(
            r#"
fn main() {
    let a = spawn("a")
    let b = spawn("b")
    let again = a
    print(a == again)
    print(a == b)
    print(a is again)
    print(a is b)
    print(a)
    print(b)
    print("{a} and {b}")
    let seen: Map[Enemy, str] = {a: "first", b: "second"}
    print(seen[again])
    print(seen.len())
    let list = [a, b]
    print(list)
}
"#,
        )
        .unwrap();
        assert_eq!(
            f.out.contents(),
            "true\nfalse\ntrue\nfalse\n<Enemy #0>\n<Enemy #1>\n<Enemy #0> and <Enemy #1>\nfirst\n2\n[<Enemy #0>, <Enemy #1>]\n"
        );
    })
}

#[test]
fn a_host_class_cannot_be_cloned_built_or_opened() {
    let mut f = fixture();
    for (src, wanted) in [
        (
            "fn main() {\n    let a = spawn(\"a\")\n    let b = a.clone()\n}\n",
            "`Enemy` is a host class and cannot be cloned",
        ),
        (
            "fn main() {\n    let a = Enemy { name: \"a\" }\n}\n",
            "`Enemy` is a host class and cannot be built with `{ ... }`",
        ),
        (
            "fn main() {\n    let a = spawn(\"a\")\n    print(a.hp)\n}\n",
            "the host class `Enemy` has no fields",
        ),
        (
            "fn main() {\n    let a = spawn(\"a\")\n    match a {\n        Enemy { name } => print(name),\n    }\n}\n",
            "`Enemy` is a host class and has no fields to match",
        ),
        (
            "impl Drop for Enemy {\n    fn drop(self) { }\n}\nfn main() {\n}\n",
            "`Drop` cannot be implemented for the host class `Enemy`",
        ),
        (
            "class Node { e: weak Enemy }\nfn main() {\n}\n",
            "`weak` cannot refer to the host class `Enemy`",
        ),
    ] {
        let err = f.vm.load_source("game.colibri", src).unwrap_err();
        let shown = err.render(false);
        assert!(shown.contains(wanted), "wanted `{wanted}` in:\n{shown}");
    }
}

#[test]
fn objects_cross_functions_collections_and_tuples() {
    vm_pending("objects_cross_functions_collections_and_tuples", || {
        let mut f = fixture();
        f.run(
            r#"
fn pick(es: List[Enemy], i: int) -> Enemy => es[i]

fn main() {
    let es = [spawn("a"), spawn("b")]
    let (x, y) = (pick(es, 0), pick(es, 1))
    print(x.same_name(y))
    print(x.same_name(x))
    let best = es.map(fn(e: Enemy) => e.hp()).len()
    print(best)
}
"#,
        )
        .unwrap();
        assert_eq!(f.out.contents(), "false\ntrue\n2\n");
        assert_eq!(*f.log.borrow(), ["rust drop a", "rust drop b"]);
    })
}

#[test]
fn a_dead_receiver_and_a_dead_argument_both_say_so() {
    let mut f = fixture();
    f.vm.register_type::<Enemy>().unwrap();
    let make = |name: &str, log: &Log| Enemy {
        name: name.into(),
        hp: 1,
        log: Rc::clone(log),
    };
    let a = f.vm.insert(make("a", &f.log));
    let b = f.vm.insert(make("b", &f.log));
    f.vm.register_fn("get_a", move || a).unwrap();
    f.vm.register_fn("get_b", move || b).unwrap();
    f.vm.remove(b).unwrap();
    let message = panic_message(f.run("fn main() {\n    get_a().same_name(get_b())\n}\n"));
    assert_eq!(message, "`Enemy` was destroyed by the host");
}

#[test]
fn objects_are_dropped_in_index_order_when_the_vm_goes() {
    vm_pending(
        "objects_are_dropped_in_index_order_when_the_vm_goes",
        || {
            // Order: roots (a callback's closure), module values (reverse of initialisation), then
            // whatever is left in the host heap by index. Drop hooks may still use host objects.
            let log: Log = Rc::default();
            let make = |name: &str| Enemy {
                name: name.into(),
                hp: 1,
                log: Rc::clone(&log),
            };
            let out = colibri::SharedBuffer::new();
            let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
            let sink = Rc::clone(&log);
            vm.register_fn("log", move |text: String| push(&sink, text))
                .unwrap();
            vm.register_type::<Enemy>().unwrap();
            let first = vm.insert(make("host first"));
            let second = vm.insert(make("host second"));
            vm.register_fn("first", move || first).unwrap();
            let maker = make("script owned");
            let slot = Cell::new(Some(maker));
            vm.register_fn("script_owned", move || slot.take().unwrap())
                .unwrap();
            vm.load_source(
                "game.colibri",
                r#"
class Marker { name: str, partner: Enemy }

impl Drop for Marker {
    fn drop(self) {
        log("hook {self.name} sees {self.partner.name()}")
    }
}

let early = Marker { name: "early", partner: first() }
let owned = script_owned()
let late = Marker { name: "late", partner: first() }

fn rooted() -> fn() {
    let m = Marker { name: "root", partner: first() }
    return fn() { log("closure {m.name}") }
}
"#,
            )
            .unwrap();
            let rooted = vm.get_fn::<(), Callback<(), ()>>("rooted").unwrap();
            let keep = rooted.call(&mut vm, ()).unwrap();
            keep.call(&mut vm, ()).unwrap();
            assert_eq!(*log.borrow(), ["closure root"]);
            // Keep the callback (a root) alive across the drop of the Vm.
            let _ = (second, &keep);
            drop(vm);
            assert_eq!(
                *log.borrow(),
                [
                    "closure root",
                    // 1. roots
                    "hook root sees host first",
                    // 2. module values, in reverse initialisation order: `late`, `owned`, `early`
                    "hook late sees host first",
                    "rust drop script owned",
                    "hook early sees host first",
                    // 3. what is left in the host heap, by index
                    "rust drop host first",
                    "rust drop host second",
                ]
            );
        },
    )
}

#[test]
fn a_handle_of_another_vm_is_refused() {
    let mut a = fixture();
    let mut b = fixture();
    a.vm.register_type::<Enemy>().unwrap();
    b.vm.register_type::<Enemy>().unwrap();
    let h = a.vm.insert(Enemy {
        name: "a".into(),
        hp: 1,
        log: Rc::clone(&a.log),
    });
    assert!(matches!(b.vm.get(h), Err(colibri::Error::StaleHandle)));
    assert!(matches!(b.vm.remove(h), Err(colibri::Error::StaleHandle)));
    assert!(!b.vm.is_alive(h));
}

#[test]
fn registering_a_clashing_class_is_an_error() {
    struct Other;
    impl HostType for Other {
        const NAME: &'static str = "Enemy";
        fn describe(_: &mut TypeBuilder<'_, Self>) {}
    }
    let mut f = fixture();
    f.vm.register_type::<Enemy>().unwrap();
    // The same class twice is fine; another Rust type under the same name is not.
    f.vm.register_type::<Enemy>().unwrap();
    let err = f.vm.register_type::<Other>().unwrap_err();
    assert!(
        err.to_string().contains("two different Rust types"),
        "{err}"
    );
}

#[test]
fn method_descriptions_are_checked() {
    struct Bad;
    impl HostType for Bad {
        const NAME: &'static str = "Bad";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.method("alive", [], |_: &Bad| true);
        }
    }
    struct Names;
    impl HostType for Names {
        const NAME: &'static str = "Names";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.method("one", ["a", "b"], |_: &Names, x: i64| x);
            d.method("two", [], |_: &Names| 1i64);
            d.method("two", [], |_: &Names| 2i64);
        }
    }
    let mut vm = colibri::Vm::new();
    let err = vm.register_type::<Bad>().unwrap_err();
    assert!(
        err.to_string().contains("built into every host class"),
        "{err}"
    );
    let err = vm.register_type::<Names>().unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("takes 1 parameter(s) but 2 name(s)"),
        "{text}"
    );
}

#[test]
fn a_registration_that_fails_leaves_the_vm_unchanged() {
    struct Bad;
    impl HostType for Bad {
        const NAME: &'static str = "Bad";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.method("alive", [], |_: &Bad| true);
        }
    }
    let mut vm = colibri::Vm::new();
    let before = vm.interface_text();
    assert!(vm.register_fn("make", || Bad).is_err());
    assert_eq!(vm.interface_text(), before);
}

#[test]
fn a_method_name_the_language_reserves_is_refused_at_registration() {
    struct Keyword;
    impl HostType for Keyword {
        const NAME: &'static str = "Keyword";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.method("match", [], |_: &Keyword| 1i64);
        }
    }
    let mut vm = colibri::Vm::new();
    let before = vm.interface_text();
    let err = vm.register_type::<Keyword>().unwrap_err();
    assert!(
        matches!(err, colibri::Error::Registration { .. }),
        "{err:?}"
    );
    // Nothing of the class was kept.
    assert_eq!(vm.interface_text(), before);
}

#[test]
fn the_host_cannot_remove_what_the_script_owns() {
    let mut f = fixture();
    f.vm.register_type::<Enemy>().unwrap();
    f.vm.insert_state(None::<Handle<Enemy>>);
    f.vm.register_fn("grab", |cx: &mut Ctx, e: Handle<Enemy>| {
        cx.state_mut::<Option<Handle<Enemy>>>().unwrap().replace(e);
    })
    .unwrap();
    f.vm.load_source(
        "game.colibri",
        "let kept = spawn(\"mine\")\nfn setup() {\n    grab(kept)\n}\n",
    )
    .unwrap();
    f.vm.get_fn::<(), ()>("setup")
        .unwrap()
        .call(&mut f.vm, ())
        .unwrap();
    let handle =
        f.vm.state_mut::<Option<Handle<Enemy>>>()
            .unwrap()
            .take()
            .unwrap();
    // The host can look at it, but it is the script's to free.
    assert_eq!(f.vm.get(handle).unwrap().name, "mine");
    let err = f.vm.remove(handle).err().expect("the script owns it");
    assert!(
        err.to_string()
            .contains("this `Enemy` is owned by the script"),
        "{err}"
    );
    assert!(f.vm.is_alive(handle));
    // Once the script lets go, the handle is stale.
    f.vm.shutdown();
    assert!(!f.vm.is_alive(handle));
    assert_eq!(*f.log.borrow(), ["rust drop mine"]);
}

#[test]
fn a_panic_in_the_rust_drop_of_a_script_owned_object_is_a_panic_of_the_script() {
    struct Boom;
    impl Drop for Boom {
        fn drop(&mut self) {
            panic!("boom");
        }
    }
    impl HostType for Boom {
        const NAME: &'static str = "Boom";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.static_fn("new", [], || Boom);
        }
    }
    let mut f = fixture();
    f.vm.register_type::<Boom>().unwrap();
    f.vm.load_source(
        "game.colibri",
        "fn scope() {\n    let b = Boom.new()\n}\nfn after() -> int => 7\n",
    )
    .unwrap();
    let scope = f.vm.get_fn::<(), ()>("scope").unwrap();
    let err = scope.call(&mut f.vm, ()).unwrap_err();
    assert!(
        err.to_string()
            .contains("dropping the host object `Boom` panicked: boom"),
        "{err}"
    );
    // The Vm is intact and the object is gone.
    assert_eq!(f.vm.host_object_count(), 0);
    let after = f.vm.get_fn::<(), i64>("after").unwrap();
    assert_eq!(after.call(&mut f.vm, ()).unwrap(), 7);
}

/// A class that customises how it shows and what `==` means.
struct Pt(i64, i64);

impl HostType for Pt {
    const NAME: &'static str = "Pt";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.static_fn("new", ["x", "y"], |x: i64, y: i64| Pt(x, y));
        d.method("x", [], |this: &Pt| this.0);
        d.show(|this: &Pt| format!("({}, {})", this.0, this.1));
        d.eq(|a: &Pt, b: &Pt| a.0 == b.0 && a.1 == b.1);
    }
}

/// Another class, without either.
struct Plain;

impl HostType for Plain {
    const NAME: &'static str = "Plain";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.static_fn("new", [], || Plain);
    }
}

#[test]
fn a_class_can_customise_show_and_equality() {
    vm_pending("a_class_can_customise_show_and_equality", || {
        let out = colibri::SharedBuffer::new();
        let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
        vm.register_type::<Pt>().unwrap();
        vm.register_type::<Plain>().unwrap();
        vm.load_source(
            "game.colibri",
            r#"fn main() {
    let a = Pt.new(1, 2)
    let b = Pt.new(1, 2)
    let c = Pt.new(3, 4)
    print(a)
    print("{a} {c}")
    print([a, c])
    print(some(b))
    print(a == b)
    print(a == c)
    print(a != c)
    print([a] == [b])
    print(some(a) == some(c))
    let m: Map[Pt, str] = {a: "one"}
    m[b] = "two"
    print(m.len())
    print(m.get(Pt.new(1, 2)))
    print(Set([a, b, c]).len())
    let p = Plain.new()
    let q = Plain.new()
    print(p == p)
    print(p == q)
}
"#,
        )
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
        vm.run_main().unwrap();
        let text = out.contents();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[..5],
            [
                "(1, 2)",
                "(1, 2) (3, 4)",
                "[(1, 2), (3, 4)]",
                "some((1, 2))",
                "true"
            ]
        );
        assert_eq!(
            lines[5..],
            [
                "false",
                "true",
                "true",
                "false",
                "1",
                "some(\"two\")",
                "2",
                "true",
                "false"
            ]
        );
        // Nothing is added to the module's interface for the two hooks.
        let interface = vm.interface_text();
        assert!(
            !interface.contains("show") && !interface.contains("eq"),
            "{interface}"
        );
    })
}

/// A class that compares by its first field only, and hashes by it.
struct Key(i64, i64);

impl HostType for Key {
    const NAME: &'static str = "Key";
    fn describe(d: &mut TypeBuilder<'_, Self>) {
        d.static_fn("new", ["a", "b"], |a: i64, b: i64| Key(a, b));
        d.method("second", [], |this: &Key| this.1);
        d.eq(|a: &Key, b: &Key| a.0 == b.0);
        d.hash(|this: &Key| this.0 as u64);
    }
}

#[test]
fn equal_host_objects_hash_equally_and_one_without_a_hash_is_a_clean_error() {
    vm_pending(
        "equal_host_objects_hash_equally_and_one_without_a_hash_is_a_clean_error",
        || {
            let out = colibri::SharedBuffer::new();
            let mut vm = colibri::Vm::builder().stdout(Box::new(out.clone())).build();
            vm.register_type::<Key>().unwrap();
            vm.register_type::<Pt>().unwrap();
            vm.register_type::<Plain>().unwrap();
            vm.load_source(
                "game.colibri",
                r#"fn main() {
    let a = Key.new(1, 10)
    let b = Key.new(1, 20)
    let c = Key.new(2, 10)
    print(a == b)
    print(a.hash() == b.hash())
    print(a.hash() == c.hash())
    print((a, 5).hash() == (b, 5).hash())
    let p = Plain.new()
    print(p.hash() == p.hash())
}
fn no_hash() -> int => Pt.new(1, 2).hash()
"#,
            )
            .unwrap_or_else(|e| panic!("{}", e.render(false)));
            vm.run_main().unwrap();
            assert_eq!(out.contents(), "true\ntrue\nfalse\ntrue\ntrue\n");
            let f = vm.get_fn::<(), i64>("no_hash").unwrap();
            let err = f.call(&mut vm, ()).unwrap_err().to_string();
            assert!(
                err.contains("`Pt` compares with its own `==` but gives no hash"),
                "{err}"
            );
            // Nothing is added to the module's interface for the hook.
            assert!(!vm.interface_text().contains("fn hash"));
        },
    )
}

#[test]
fn describing_hash_twice_is_a_registration_error() {
    struct Twice;
    impl HostType for Twice {
        const NAME: &'static str = "TwiceHash";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.hash(|_: &Twice| 0);
            d.hash(|_: &Twice| 0);
        }
    }
    let mut vm = colibri::Vm::builder().build();
    let err = vm.register_type::<Twice>().unwrap_err().to_string();
    assert!(err.contains("customises its `hash` twice"), "{err}");
}

#[test]
fn describing_show_or_eq_twice_is_a_registration_error() {
    struct Twice;
    impl HostType for Twice {
        const NAME: &'static str = "Twice";
        fn describe(d: &mut TypeBuilder<'_, Self>) {
            d.show(|_: &Twice| String::new());
            d.show(|_: &Twice| String::new());
            d.eq(|_: &Twice, _: &Twice| true);
            d.eq(|_: &Twice, _: &Twice| true);
        }
    }
    let mut vm = colibri::Vm::builder().build();
    let err = vm.register_type::<Twice>().unwrap_err().to_string();
    assert!(err.contains("customises its `Show` text twice"), "{err}");
}
