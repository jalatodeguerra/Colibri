//! Runs the example with `scripts/game.colibri` and prints its log.

use std::path::PathBuf;
use std::process::ExitCode;

use colibri_example_host_game::{DEFAULT_TICKS, Game, scripts_dir};

fn main() -> ExitCode {
    let ticks = match std::env::args().nth(1) {
        Some(arg) => match arg.parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("usage: host_game [ticks]  (default {DEFAULT_TICKS})");
                return ExitCode::from(2);
            }
        },
        None => DEFAULT_TICKS,
    };
    let dir: PathBuf = scripts_dir();
    let mut game = match Game::new() {
        Ok(game) => game,
        Err(e) => {
            eprintln!("{}", e.render(false));
            return ExitCode::FAILURE;
        }
    };
    // The editors and `colibri check --interface` read this file; keep it current.
    if let Err(e) = game.write_interface(dir.join("host.cbi")) {
        eprintln!("warning: could not write host.cbi: {}", e.render(false));
    }
    if let Err(e) = game.load_file(dir.join("game.colibri")) {
        eprintln!("{}", e.render(false));
        return ExitCode::FAILURE;
    }
    for line in game.run(ticks) {
        println!("{line}");
    }
    ExitCode::SUCCESS
}
