//! The Colibri debug adapter: the Debug Adapter Protocol over the VM's debugger (`colibri
//! debug`). See [`serve`].
//!
//! One thread reads the client's messages and sends them over a channel; the thread that owns
//! the `Vm` answers them: in `Debugger::stopped` while the program is stopped (the requests that
//! look at it, and the ones that let it go on), in `Debugger::poll` while it runs (pause, new
//! breakpoints, ending the session), and between the steps of a session (initialize, launch,
//! configurationDone). Everything the program prints goes out as `output` events, so nothing
//! but protocol messages is written to the adapter's standard output.

mod frame;
mod path;
mod server;

pub use frame::{ReadError, read_message, write_message};
pub use server::{LaunchArgs, serve};
