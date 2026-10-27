//! Killing the server's whole process group, needed to stop its children too.
#![allow(unsafe_code)]

use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::time::{Instant, sleep};

/// How long `stop` waits after `SIGTERM` before it sends `SIGKILL`.
pub const STOP_GRACE: Duration = Duration::from_secs(2);

/// How long `stop` waits between two checks that the group is gone.
const STOP_POLL: Duration = Duration::from_millis(20);

/// Kills the process group `pgid` with `SIGKILL` when dropped, unless disarmed.
pub struct GroupGuard {
    pgid: Option<i32>,
}

impl GroupGuard {
    /// Guards the group led by `pgid`, a child spawned with [`new_session`].
    pub fn new(pgid: i32) -> Self {
        Self { pgid: Some(pgid) }
    }

    /// Asks the group to end, waits up to [`STOP_GRACE`], then kills it.
    pub async fn stop(&mut self) {
        let Some(pgid) = self.pgid else { return };
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGTERM);
        let limit = Instant::now() + STOP_GRACE;
        while Instant::now() < limit {
            if !alive_group(pgid) {
                break;
            }
            sleep(STOP_POLL).await;
        }
        self.kill();
    }

    /// Leaves the group running, for a leader that ended and was reaped.
    pub fn disarm(&mut self) {
        self.pgid = None;
    }

    /// Kills the group now. Later calls and the drop do nothing.
    pub fn kill(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

impl std::fmt::Debug for GroupGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("GroupGuard")
            .field(&self.pgid)
            .finish()
    }
}

/// Reports whether any process of the group `pgid` exists. Signal 0 sends
/// nothing.
fn alive_group(pgid: i32) -> bool {
    killpg(Pid::from_raw(pgid), None).is_ok()
}

/// Starts the command's child in a new session, which also makes it the
/// leader of a new process group.
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
