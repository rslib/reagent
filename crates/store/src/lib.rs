//! A durable, append-only log for sessions.
//!
//! The store knows nothing about messages: a record is a kind byte and a
//! payload, and a part file is a chain of frames. Reads block, so run them
//! inside `spawn_blocking`; the writer runs its own thread.

#![warn(missing_docs)]

pub mod codec;
pub mod fork;
pub mod frame;
pub mod list;
pub mod part;
pub mod paths;
pub mod recover;
pub mod stream;
pub mod writer;

pub use codec::{DecodeError, Decoder, Encoder, Text, Value};
pub use fork::{ForkPoint, Records, Session};
pub use frame::{Frame, FrameError};
pub use list::{Entry, Lookup, SessionList, State};
pub use part::Header;
pub use paths::{cwd_key, sessions_root};
pub use recover::{Part, Record};
pub use writer::Writer;

/// Frame kinds the store defines itself.
pub mod kind {
    /// The first frame of a part: its header.
    pub const HEADER: u8 = 1;
    /// Compressed records.
    pub const CHUNK: u8 = 2;
    /// Makes the records written before it visible.
    pub const COMMIT: u8 = 3;
    /// One entry of a folder's session list.
    pub const LIST_ENTRY: u8 = 4;
    /// A removed session in a folder's session list.
    pub const LIST_TOMBSTONE: u8 = 5;
    /// The commits of a closed part, at its end.
    pub const FOOTER: u8 = 6;
    /// The first kind a caller may use for a record of its own.
    pub const RECORD_FIRST: u8 = 16;
}

/// Files the tests write and read.
#[cfg(test)]
pub(crate) mod testutil {
    use std::io::{Read, Write};
    use std::path::Path;

    /// Writes `bytes` to `path`, creating or replacing it.
    #[expect(
        clippy::disallowed_types,
        reason = "a test writes its own fixture file"
    )]
    pub(crate) fn write(path: &Path, bytes: &[u8]) {
        let mut file = std::fs::File::create(path).expect("create fixture");
        file.write_all(bytes).expect("write fixture");
    }

    /// Reads `path` whole.
    #[expect(clippy::disallowed_types, reason = "a test reads its own fixture file")]
    pub(crate) fn read(path: &Path) -> Vec<u8> {
        let mut file = std::fs::File::open(path).expect("open fixture");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read fixture");
        bytes
    }

    /// How long a fixture file is.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test measures its own fixture file"
    )]
    pub(crate) fn len(path: &Path) -> u64 {
        std::fs::metadata(path).expect("fixture").len()
    }
}

/// Faults tests inject into the writer thread.
#[cfg(test)]
pub(crate) mod hook {
    use std::sync::{Mutex, PoisonError};

    /// A step the writer thread took, in the order it took it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Event {
        /// The zstd stream was flushed.
        Flush,
        /// A chunk frame was written.
        Write,
        /// The file was synced.
        Sync,
    }

    /// Steps of each part, so a test sees only the part it made.
    static EVENTS: Mutex<Vec<(u64, Event)>> = Mutex::new(Vec::new());
    /// Parts whose next write must fail.
    static FAULTS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

    /// Records one step of the writer thread of `part`.
    pub fn push(part: u64, event: Event) {
        EVENTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((part, event));
    }

    /// Takes the steps recorded for `part`.
    pub fn take(part: u64) -> Vec<Event> {
        let mut events = EVENTS.lock().unwrap_or_else(PoisonError::into_inner);
        let (mine, others): (Vec<_>, Vec<_>) = std::mem::take(&mut *events)
            .into_iter()
            .partition(|(id, _)| *id == part);
        *events = others;
        mine.into_iter().map(|(_, event)| event).collect()
    }

    /// Makes the next write of `part` fail.
    pub fn fail_next_write(part: u64) {
        FAULTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(part);
    }

    /// Whether a write of `part` must fail now.
    pub fn fault(part: u64) -> bool {
        let mut faults = FAULTS.lock().unwrap_or_else(PoisonError::into_inner);
        let at = faults.iter().position(|id| *id == part);
        match at {
            Some(at) => {
                faults.remove(at);
                true
            }
            None => false,
        }
    }
}
