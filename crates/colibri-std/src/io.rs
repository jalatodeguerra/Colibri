//! `io`: standard input and error, and writing to standard output without a newline. `print`
//! stays the built-in (it goes through the same writer the `Vm` was built with).

use std::io::{BufRead, Write};

use colibri_runtime::{Cap, Ctx, Error, TypedError, Vm};

use crate::config::State;
use crate::errors::error_kind;
use crate::{needs, plain};

error_kind!(
    "io",
    /// Why reading standard input failed (`io.Error.InvalidData`).
    Kind {
        /// The input is not valid UTF-8 text.
        InvalidData,
        Other,
    }
);

/// Runs `f` on the configured input, or on the process's standard input.
fn with_input<T>(
    cx: &mut Ctx,
    f: impl FnOnce(&mut dyn BufRead) -> std::io::Result<T>,
) -> Result<T, TypedError> {
    let result = match cx.state_mut::<State>().and_then(|s| s.stdin.as_mut()) {
        Some(input) => f(input.as_mut()),
        None => f(&mut std::io::stdin().lock()),
    };
    result.map_err(|e| {
        let kind = match e.kind() {
            std::io::ErrorKind::InvalidData => Kind::InvalidData,
            _ => Kind::Other,
        };
        kind.error(format!(
            "cannot read standard input: {}",
            crate::fs::reason(&e)
        ))
    })
}

/// Writes to the configured standard error, or the process's.
fn write_stderr(cx: &mut Ctx, text: &str) {
    match cx.state_mut::<State>().and_then(|s| s.stderr.as_mut()) {
        Some(out) => {
            let _ = out.write_all(text.as_bytes());
        }
        None => {
            let _ = std::io::stderr().write_all(text.as_bytes());
        }
    }
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("io");
    // Where `print` writes, but with no newline added.
    plain(
        m.register_fn_named("write", ["text"], |cx: &mut Ctx, text: String| {
            cx.write_stdout(&text);
        }),
    )?;
    plain(
        m.register_fn_named("eprint", ["text"], |cx: &mut Ctx, text: String| {
            write_stderr(cx, &text);
        }),
    )?;
    plain(
        m.register_fn_named("eprintln", ["text"], |cx: &mut Ctx, text: String| {
            write_stderr(cx, &format!("{text}\n"));
        }),
    )?;
    // The next line without its line ending (one "\n" or "\r\n"), or `none` at the end of the input.
    needs(
        Cap::Os,
        m.register_fn_named("read_line", [], |cx: &mut Ctx| {
            with_input(cx, |input| {
                let mut line = String::new();
                if input.read_line(&mut line)? == 0 {
                    return Ok(None);
                }
                // Exactly one line ending: "\n" or "\r\n". A "\r" that is not part of one (an
                // extra one before it, or one at the end of the input) belongs to the line.
                if line.ends_with('\n') {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
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
