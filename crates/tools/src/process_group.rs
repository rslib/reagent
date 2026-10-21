//! Killing a whole process group, needed to stop `bash -c` and its children.
#![allow(unsafe_code)]

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

/// Kills the process group `pgid` with `SIGKILL` when dropped, unless disarmed.
///
/// The agent loop drops a tool future on cancel, so the kill must live in
/// `Drop`. `kill_on_drop` alone stops only the shell, not its children.
pub struct GroupGuard {
    pgid: Option<i32>,
}

impl GroupGuard {
    /// Guards the group led by `pgid`, a child spawned with [`new_session`].
    pub fn new(pgid: i32) -> Self {
        Self { pgid: Some(pgid) }
    }

    /// Kills the group now. Later calls and the drop do nothing.
    pub fn kill(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
        }
    }

    /// Leaves the group running. Call it once the leader is reaped, because
    /// its id can then be reused by an unrelated group.
    pub fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Reports whether the process `pid` exists. Signal 0 sends nothing.
#[cfg(test)]
pub fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(Pid::from_raw(pid), None).is_ok()
}

/// Starts the command's child in a new session, which also makes it the
/// leader of a new process group.
///
/// A new session has no controlling terminal, so a prompt that opens
/// /dev/tty (ssh, sudo, git credentials) fails instead of waiting.
pub fn new_session(command: &mut tokio::process::Command) {
    // SAFETY: the hook runs in the forked child before exec and calls only
    // setsid, which is async-signal-safe and allocates nothing.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(drop)
                .map_err(std::io::Error::from)
        });
    }
}
