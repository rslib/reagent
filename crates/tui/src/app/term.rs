//! The real terminal: crossterm behind the [`Terminal`] trait.
//!
//! Raw mode goes on when the terminal is made and comes off on every exit
//! path: on drop, and in the panic hook, so a panic does not leave the shell in
//! raw mode.
//!
//! Input is read with async I/O on the app's own runtime (see
//! [`crate::input`]): a key that the app's future was dropped on is not lost,
//! because the bytes read stay in the parser the terminal owns, and the select!
//! that waits for one is cancel safe.
//!
//! Output goes to a duplicate of fd 1 opened as a file, one `write_all` per
//! frame: the locking `LineWriter` of `io::stdout` splits a frame at its last
//! newline into two writes, and it blocks the runtime while it writes. The
//! file's write runs on the blocking pool instead.

use std::io::{self, Write};
use std::os::fd::AsFd;
use std::sync::Once;

use crossterm::event::Event;
use crossterm::terminal;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::Terminal;
use crate::input::{Background, Input};

/// The first frame's two queries: the background (`OSC 11` with `?`) and then
/// the primary device attributes (`CSI c`).
///
/// They go out in one write with the first frame setup, and both answers come
/// back through the parser as events that never reach the keys. The background
/// answer picks the mode ([`crate::input::Signal::Background`]); the device
/// attributes answer ([`crate::input::Signal::DeviceAttributes`]) ends the first
/// frame's wait, and every terminal gives it, so the wait is a few milliseconds
/// on a real terminal and the safety limit only covers one that answers neither.
pub(super) const QUERY: &[u8] = b"\x1b]11;?\x07\x1b[c";

/// DEC mouse capture on: the wheel, a drag and its release reach the app. Only
/// full screen takes the mouse, so inline mode keeps the terminal's own
/// selection.
pub(super) const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";

/// DEC mouse capture off: the wheel and the selection are the terminal's again.
pub(super) const MOUSE_OFF: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1006l";

/// The extended keys on: xterm's modifyOtherKeys level 1, which tmux turns
/// into `CSI u` keys, then the kitty flags 1 (disambiguate) and 4 (the shifted
/// key), so Shift+Enter is not Enter. A kitty terminal ignores the first.
pub(super) const KEYS_ON: &[u8] = b"\x1b[>4;1m\x1b[>5u";

/// The extended keys off: pop the kitty flags and reset modifyOtherKeys.
pub(super) const KEYS_OFF: &[u8] = b"\x1b[<u\x1b[>4;0m";

/// Leaving the alternate screen, the mouse capture and the extended keys, for
/// a panic: no exit path of a run leaves any of them on.
const LEAVE: &[u8] = b"\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[<u\x1b[>4;0m";

/// The terminal of a real process: the window in raw mode, its keys and its
/// output.
#[derive(Debug)]
pub struct Real {
    /// The tty and the bytes read from it, opened when the app first waits for
    /// input: opening it is async, and [`Real::new`] runs before the app's
    /// loop.
    input: Option<Input>,
    /// The terminal's output: a duplicate of fd 1, never `O_NONBLOCK`.
    out: tokio::fs::File,
    /// Where the terminal's answers to the first frame's two queries go. It
    /// exists before the tty is open, so the app can hold it from the start.
    background: Background,
}

impl Real {
    /// Puts the terminal in raw mode and starts listening for a resize.
    pub fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        install_hook();
        let out = output()?;
        Ok(Real {
            input: None,
            out,
            background: Background::default(),
        })
    }

    /// The handle the terminal's answers to the first frame's two queries arrive
    /// at.
    ///
    /// The app takes one for [`crate::app::Options::background`], so the first
    /// frame waits for the device attributes answer and a late background answer
    /// still repaints.
    pub fn background(&self) -> Background {
        self.background.clone()
    }
}

/// A duplicate of fd 1 for the app's own writes.
fn output() -> io::Result<tokio::fs::File> {
    let fd = io::stdout().as_fd().try_clone_to_owned()?;
    #[expect(
        clippy::disallowed_types,
        reason = "the terminal's own descriptor, wrapped for tokio"
    )]
    let file = std::fs::File::from(fd);
    Ok(tokio::fs::File::from_std(file))
}

impl Drop for Real {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

impl Terminal for Real {
    fn size(&self) -> (u16, u16) {
        terminal::size().unwrap_or((80, 24))
    }

    async fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        write_frame(&mut self.out, frame).await
    }

    async fn next_event(&mut self) -> io::Result<Event> {
        if self.input.is_none() {
            self.input = Some(Input::open(self.background.clone()).await?);
        }
        let input = self.input.as_mut().expect("the tty is open");
        input.next().await
    }
}

/// Writes one whole frame in one `write_all`, whatever newlines it holds, so a
/// frame is never split into two writes.
async fn write_frame(out: &mut (impl AsyncWrite + Unpin), frame: &[u8]) -> io::Result<()> {
    out.write_all(frame).await
}

/// Puts the terminal back in cooked mode, on the normal screen, before a panic
/// message is printed.
fn install_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // One small write with no newline, so the line writer of `stdout`
            // leaves it whole: a panic in full screen still comes back to the
            // scrollback before the message.
            let mut out = io::stdout();
            let _ = out.write_all(LEAVE);
            let _ = out.flush();
            let _ = terminal::disable_raw_mode();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::io;

    use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

    use super::write_frame;

    /// A writer that counts the writes it takes.
    #[derive(Debug, Default)]
    struct Counting {
        writes: usize,
        bytes: Vec<u8>,
    }

    impl AsyncWrite for Counting {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            self.writes += 1;
            let len = buf.len();
            self.bytes.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(len))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_frame_goes_out_in_one_write_whatever_its_newlines() {
        let mut out = Counting::default();
        write_frame(&mut out, b"one\ntwo\nthree\n")
            .await
            .expect("write");
        assert_eq!(out.writes, 1, "the frame takes one write, not two");
        write_frame(&mut out, b"four\n").await.expect("write");
        assert_eq!(out.writes, 2, "the next frame takes one more");
        assert_eq!(out.bytes, b"one\ntwo\nthree\nfour\n");
    }

    #[tokio::test]
    async fn a_frame_written_to_a_file_is_the_whole_frame() {
        let dir = tempfile::tempdir().expect("a temp folder");
        let path = dir.path().join("out");
        let mut out = tokio::fs::File::create(&path).await.expect("the file");
        write_frame(&mut out, b"a\nb\n").await.expect("write");
        out.flush().await.expect("flush");
        let mut bytes = Vec::new();
        tokio::fs::File::open(&path)
            .await
            .expect("the file")
            .read_to_end(&mut bytes)
            .await
            .expect("read");
        assert_eq!(bytes, b"a\nb\n");
    }
}
