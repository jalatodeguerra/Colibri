//! `io`: standard input and error, and writing to standard output without a newline. `print`
//! stays the built-in (it goes through the same writer the `Vm` was built with).

use std::io::{BufRead, Write};

use colibri_runtime::{Cap, Ctx, Error, Vm};

use crate::config::State;
use crate::{needs, plain};

/// Runs `f` on the configured input, or on the process's standard input.
fn with_input<T>(
    cx: &mut Ctx,
    f: impl FnOnce(&mut dyn BufRead) -> std::io::Result<T>,
) -> Result<T, String> {
    let result = match cx.state_mut::<State>().and_then(|s| s.stdin.as_mut()) {
        Some(input) => f(input.as_mut()),
        None => f(&mut std::io::stdin().lock()),
    };
    result.map_err(|e| format!("cannot read standard input: {}", crate::fs::reason(&e)))
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("io");
    // Where `print` writes, but with no newline added.
    plain(
        m.register_fn_named("write", ["text"], |cx: &mut Ctx, text: String| {
            cx.write_stdout(&text);
        }),
    )?;
    plain(m.register_fn_named("eprint", ["text"], |text: String| {
        let _ = std::io::stderr().write_all(text.as_bytes());
    }))?;
    plain(m.register_fn_named("eprintln", ["text"], |text: String| {
        let _ = writeln!(std::io::stderr(), "{text}");
    }))?;
    // The next line without its line ending, or `none` at the end of the input.
    needs(
        Cap::Os,
        m.register_fn_named("read_line", [], |cx: &mut Ctx| {
            with_input(cx, |input| {
                let mut line = String::new();
                if input.read_line(&mut line)? == 0 {
                    return Ok(None);
                }
                while line.ends_with(['\n', '\r']) {
                    line.pop();
                }
                Ok(Some(line))
            })
        }),
    )?;
    needs(
        Cap::Os,
        m.register_fn_named("read_all", [], |cx: &mut Ctx| {
            with_input(cx, |input| {
                let mut text = String::new();
                std::io::Read::read_to_string(input, &mut text)?;
                Ok(text)
            })
        }),
    )?;
    Ok(())
}
