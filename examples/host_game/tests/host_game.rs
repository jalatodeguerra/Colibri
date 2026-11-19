//! The example runs in-process: its log is compared with a golden file, the committed
//! `host.cbi` must be current, and a misbehaving script must not take the host down.

use std::fs;

use colibri_example_host_game::{DEFAULT_TICKS, Game, STEP_BUDGET, scripts_dir};

fn game_with_scripts() -> Game {
    let mut game = Game::new().expect("the host API registers");
    game.load_file(scripts_dir().join("game.colibri"))
        .unwrap_or_else(|e| panic!("{}", e.render(false)));
    game
}

#[test]
fn the_log_matches_the_golden_file() {
    let log = game_with_scripts().run(DEFAULT_TICKS).join("\n") + "\n";
    let path = scripts_dir().join("../tests/expected.txt");
    if std::env::var_os("UPDATE_EXPECTED").is_some() {
        fs::write(&path, &log).unwrap();
    }
    let expected = fs::read_to_string(&path).expect("tests/expected.txt");
    assert_eq!(log, expected, "run with UPDATE_EXPECTED=1 to rewrite it");
}

#[test]
fn the_run_is_deterministic() {
    let a = game_with_scripts().run(300);
    let b = game_with_scripts().run(300);
    assert_eq!(a, b);
}

#[test]
fn the_committed_interface_is_current() {
    let game = Game::new().unwrap();
    let on_disk = fs::read_to_string(scripts_dir().join("host.cbi")).expect("scripts/host.cbi");
    assert_eq!(
        on_disk,
        game.interface_text(),
        "host.cbi is stale: run `cargo run -p colibri-example-host-game`"
    );
}

#[test]
fn a_panic_in_update_is_logged_and_the_game_goes_on() {
    let mut game = Game::new().unwrap();
    game.load_source(
        "panicky.colibri",
        r#"
var ticks = 0

fn update(dt: f32) {
    ticks += 1
    if ticks == 5 {
        panic("boom at tick {ticks}")
    }
    if ticks == 8 {
        log("still running at tick {ticks}")
    }
}
"#,
    )
    .unwrap();
    let log = game.run(10);
    let errors: Vec<&String> = log.iter().filter(|l| l.contains("script error")).collect();
    assert_eq!(errors.len(), 1, "{log:#?}");
    assert!(
        errors[0].starts_with("[0005] script error in update: ")
            && errors[0].contains("boom at tick 5"),
        "{}",
        errors[0]
    );
    // Ticks 1-4 and 6-10 ran: the script kept its state and the loop kept its clock.
    assert!(
        log.iter()
            .any(|l| l == "[0008] script: still running at tick 8"),
        "{log:#?}"
    );
    assert!(log.last().unwrap().starts_with("summary: ticks=10 "));
    assert!(log.last().unwrap().contains("script_errors=1"));
}

#[test]
fn a_runaway_script_runs_out_of_budget_and_the_game_goes_on() {
    let mut game = Game::new().unwrap();
    game.load_source(
        "spin.colibri",
        "fn update(dt: f32) {\n    while true {\n    }\n}\n",
    )
    .unwrap();
    let log = game.run(3);
    let errors = log
        .iter()
        .filter(|l| l.contains("instruction budget"))
        .count();
    assert_eq!(errors, 3, "{log:#?}");
    let max_steps: u64 = log
        .last()
        .unwrap()
        .rsplit("max_steps=")
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(max_steps >= STEP_BUDGET, "{log:#?}");
}

#[test]
fn a_script_without_on_hit_is_skipped_by_the_hook_lookup() {
    let mut game = Game::new().unwrap();
    game.load_source(
        "no_hook.colibri",
        r#"
fn init() {
    spawn_enemy(Vec2 { x: 3.0, y: 0.0 })
}

fn update(dt: f32) {}
"#,
    )
    .unwrap();
    let log = game.run(60);
    // The turret still fires (the host logs it), nobody despawns the target, no error.
    assert_eq!(
        log.iter().filter(|l| l.contains("host: hit #1")).count(),
        2,
        "{log:#?}"
    );
    assert!(log.iter().all(|l| !l.contains("script error")), "{log:#?}");
    assert!(
        log.last()
            .unwrap()
            .contains("hits=2 score=0 script_errors=0"),
        "{log:#?}"
    );
}

#[test]
fn a_script_without_update_is_a_load_error() {
    let mut game = Game::new().unwrap();
    let err = game
        .load_source("empty.colibri", "fn init() {}\n")
        .unwrap_err();
    assert!(err.to_string().contains("update"), "{err}");
}
