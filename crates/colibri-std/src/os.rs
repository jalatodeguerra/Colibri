//! `os`: the environment, the program's arguments, the temporary folder and `exit`.

use std::collections::BTreeMap;

use colibri_runtime::{Cap, Ctx, Error, Vm};

use crate::config::{EnvSource, State};
use crate::errors::error_kind;
use crate::{needs, plain};

error_kind!(
    "os",
    /// Why a question to the operating system failed (`os.Error.NotFound`).
    Kind {
        NotFound,
        PermissionDenied,
        Other,
    }
);

fn lookup(state: &State, name: &str) -> Option<String> {
    if let Some(v) = state.env_overrides.get(name) {
        return Some(v.clone());
    }
    match &state.env {
        EnvSource::Process => std::env::var(name).ok(),
        EnvSource::Fixed(vars) => vars.get(name).cloned(),
    }
}

fn exit(cx: &mut Ctx, code: i64) -> colibri_runtime::Never {
    let Ok(code) = i32::try_from(code) else {
        cx.raise(format!("os.exit: {code} is not an exit code"));
    };
    if let Some(state) = cx.state_mut::<State>() {
        state.exit = Some(code);
    }
    cx.raise(format!("the program exited with code {code}"));
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    let mut m = vm.std_module("os");
    needs(
        Cap::Os,
        m.register_fn_named("env", ["name"], |cx: &mut Ctx, name: String| {
            cx.state_mut::<State>().and_then(|s| lookup(s, &name))
        }),
    )?;
    // Every variable, sorted by name so that iteration order is the same on every run.
    needs(
        Cap::Os,
        m.register_fn_named("vars", [], |cx: &mut Ctx| {
            let mut all: BTreeMap<String, String> = BTreeMap::new();
            if let Some(state) = cx.state_mut::<State>() {
                match &state.env {
                    EnvSource::Process => {
                        all.extend(std::env::vars_os().filter_map(|(k, v)| {
                            Some((k.into_string().ok()?, v.into_string().ok()?))
                        }))
                    }
                    EnvSource::Fixed(vars) => all.extend(vars.clone()),
                }
                all.extend(state.env_overrides.clone());
            }
            all
        }),
    )?;
    // Changes what this program sees, never the process's own environment.
    needs(
        Cap::Os,
        m.register_fn_named(
            "set_env",
            ["name", "value"],
            |cx: &mut Ctx, name: String, value: String| {
                if let Some(state) = cx.state_mut::<State>() {
                    state.env_overrides.insert(name, value);
                }
            },
        ),
    )?;
    // The script's own arguments: the host hands them over, so they need no capability.
    plain(m.register_fn_named("args", [], |cx: &mut Ctx| {
        cx.state_mut::<State>()
            .map(|s| s.args.clone())
            .unwrap_or_default()
    }))?;
    needs(
        Cap::Os,
        m.register_fn_named("temp_dir", [], |cx: &mut Ctx| {
            let dir = cx
                .state_mut::<State>()
                .and_then(|s| s.temp_dir.clone())
                .unwrap_or_else(std::env::temp_dir);
            crate::fs::script_path(&dir)
        }),
    )?;
    needs(
        Cap::Os,
        m.register_fn_named("cwd", [], || {
            std::env::current_dir()
                .map(|p| crate::fs::script_path(&p))
                .map_err(|e| {
                    let kind = match e.kind() {
                        std::io::ErrorKind::NotFound => Kind::NotFound,
                        std::io::ErrorKind::PermissionDenied => Kind::PermissionDenied,
                        _ => Kind::Other,
                    };
                    kind.error(format!(
                        "cannot find the current folder: {}",
                        crate::fs::reason(&e)
                    ))
                })
        }),
    )?;
    // "linux", "macos", "windows", ...
    plain(m.register_fn_named("platform", [], || std::env::consts::OS.to_string()))?;
    // Ends the program as a panic does (`defer`s and `Drop` hooks run), without the panic's
    // report: the host reads the code with `colibri_std::exit_code`.
    needs(Cap::Os, m.register_fn_named("exit", ["code"], exit))?;
    Ok(())
}
