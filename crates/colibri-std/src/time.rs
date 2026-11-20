//! `time`: a monotonic clock for timers and measuring intervals, the wall clock, and sleeping.

use colibri_runtime::{Cap, Ctx, Error, Vm};

use crate::config::State;
use crate::{needs, plain};

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    let mut m = vm.std_module("time");
    // Seconds since the program started, as a float: `dt = time.now() - last` is a frame time.
    plain(m.register_fn_named("now", [], |cx: &mut Ctx| {
        cx.state_mut::<State>()
            .map_or(0.0, |s| s.clock.monotonic_nanos() as f64 / 1e9)
    }))?;
    // The same clock in whole nanoseconds, for measuring without rounding.
    plain(m.register_fn_named("monotonic", [], |cx: &mut Ctx| {
        cx.state_mut::<State>().map_or(0, |s| {
            i64::try_from(s.clock.monotonic_nanos()).unwrap_or(i64::MAX)
        })
    }))?;
    // Wall-clock seconds since 1970, which is machine state: it needs the `os` capability.
    needs(
        Cap::Os,
        m.register_fn_named("unix", [], |cx: &mut Ctx| {
            cx.state_mut::<State>()
                .map_or(0.0, |s| s.clock.unix_seconds())
        }),
    )?;
    needs(
        Cap::Os,
        m.register_fn_named("sleep", ["seconds"], |cx: &mut Ctx, seconds: f64| {
            if !seconds.is_finite() || seconds < 0.0 {
                cx.raise(format!(
                    "time.sleep: {seconds} is not a number of seconds to wait (it must be finite and not negative)"
                ));
            }
            if let Some(state) = cx.state_mut::<State>() {
                state.clock.sleep(seconds);
            }
        }),
    )?;
    Ok(())
}
