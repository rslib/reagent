use std::future::Future;
use std::io;

use crossterm::event::Event;

/// The seam the TUI writes through: the terminal size, one write per frame and
/// the input events. The real terminal wraps crossterm; tests use a `vt100`
/// parser.
///
/// Every method returns its own future, so the trait is not object safe and a
/// frame write, a key wait or a wake costs no allocation.
pub trait Terminal {
    /// Terminal size as `(columns, rows)`.
    fn size(&self) -> (u16, u16);

    /// Write one whole frame as exactly one write call. A slow pty must not
    /// block the runtime, so this is a future, not a blocking call.
    fn write_frame(&mut self, frame: &[u8]) -> impl Future<Output = io::Result<()>>;

    /// The next input event. The app task fills this in; until then it never
    /// resolves.
    fn next_event(&mut self) -> impl Future<Output = io::Result<Event>> {
        std::future::pending()
    }
}
