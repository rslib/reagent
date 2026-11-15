//! `reagent acp`: the ACP v1 agent an editor such as Zed runs over stdin and
//! stdout.
//!
//! One process serves every session of the editor. Each session is one reagent
//! worker, which this process starts or joins as a socket client, so the TUI of
//! `reagent` can join the same session while the editor drives it.

use std::ffi::OsString;
use std::process::ExitCode;

#[cfg(feature = "acp")]
mod editor;
#[cfg(feature = "acp")]
mod message;
#[cfg(feature = "acp")]
mod server;
#[cfg(feature = "acp")]
mod session;

/// Runs the agent the editor drives, with the words this process was given after
/// the `acp` word.
#[cfg(feature = "acp")]
pub(crate) fn run(argv: Vec<OsString>, runtime: &tokio::runtime::Runtime) -> ExitCode {
    runtime.block_on(server::run(argv))
}

/// The agent is part of the default build; without it the command says so.
#[cfg(not(feature = "acp"))]
pub(crate) fn run(_argv: Vec<OsString>, _runtime: &tokio::runtime::Runtime) -> ExitCode {
    eprintln!("reagent: this build has no acp");
    ExitCode::from(2)
}
