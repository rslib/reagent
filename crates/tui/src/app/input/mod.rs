//! The terminal's input: the tty read with async I/O, and the parser that turns
//! its bytes into events.
//!
//! The tty is read on the app's own runtime: an [`AsyncFd`] over a read-only
//! descriptor of the terminal opened `O_NONBLOCK` on a file description of its
//! own, so a wait for a key is a future and no thread blocks on one.
//!
//! Every wake reads the tty until it would block, into a buffer the parser owns
//! and parses in place. That is the point of the design: a pty holds only a few
//! kilobytes (4 KiB on Linux), and a reader that stops after one chunk leaves
//! the rest of a paste or of a fast key repeat in the tty while it waits for a
//! readiness edge that the bytes already there never make. Draining to
//! `WouldBlock` means no byte is left behind a wait.
//!
//! A window change is a `SIGWINCH` the app turns into [`Event::Resize`]: the
//! size is read when the signal comes, never polled.

#![allow(unsafe_code)]

mod parse;

pub use parse::{Parser, Signal};

use std::ffi::{CStr, OsStr};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crossterm::event::Event;
use tokio::io::unix::AsyncFd;
use tokio::signal::unix::{Signal as SignalEvent, SignalKind};

use crate::theme::Rgb;

/// The default size a window change falls back to, as the terminal's own
/// `size` does when the ioctl fails.
const SIZE: (u16, u16) = (80, 24);

/// The flags the reader opens the terminal with: a read that finds nothing
/// returns instead of blocking, the descriptor leaves no copy in a child, and it
/// cannot become the controlling terminal.
const FLAGS: i32 = libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY;

/// The tty's descriptor, read through the reactor.
///
/// A read of it happens only once the reactor says the tty is readable and its
/// `O_NONBLOCK` makes a read that finds nothing return at once instead of
/// blocking, which is why it is not a `tokio::fs::File`: a read of one of those
/// goes to the blocking pool, and a pool thread per key is the thread this fix
/// takes away.
#[expect(clippy::disallowed_types, reason = "the tty's own descriptor")]
type Tty = std::fs::File;

/// The terminal's answers to the two queries the first frame sends: the
/// background (`OSC 11`), which picks the mode, and the device attributes
/// (`CSI c`), which ends the first frame's wait. The reader, which sees the
/// bytes, fills them in, and the app, which draws, waits for them.
///
/// One handle both records and reads: the app takes one before the tty is even
/// open, so an answer that arrives late still reaches the frame that repaints
/// for it.
#[derive(Clone, Debug, Default)]
pub struct Background {
    /// The shared answers, behind an `Arc` so both sides hold the same one.
    answer: Arc<Answer>,
}

/// The color once it arrived, whether the device attributes arrived, and the
/// waiter either answer wakes.
#[derive(Debug)]
struct Answer {
    /// The color packed as `0x1_00RRGGBB`, or [`NONE`] until the terminal
    /// answers. An atomic, so the reader never blocks the draw and no lock can
    /// be poisoned by a panic.
    color: AtomicU32,
    /// Whether the terminal answered the device attributes query, the answer
    /// that ends the first frame's wait.
    attributes: AtomicBool,
    /// Wakes the first frame's wait for the device attributes and the loop's
    /// for a color that came late.
    ready: tokio::sync::Notify,
}

impl Default for Answer {
    fn default() -> Answer {
        Answer {
            color: AtomicU32::new(NONE),
            attributes: AtomicBool::new(false),
            ready: tokio::sync::Notify::new(),
        }
    }
}

/// The color a terminal that has not answered yet holds. No packed color can
/// equal it: a packed color has the bit above its three channels set and no
/// others.
const NONE: u32 = u32::MAX;

impl Background {
    /// The color the terminal answered with, `None` until it answers.
    pub fn color(&self) -> Option<Rgb> {
        let packed = self.answer.color.load(Ordering::Acquire);
        (packed != NONE).then(|| Rgb::new((packed >> 16) as u8, (packed >> 8) as u8, packed as u8))
    }

    /// Whether the terminal answered the device attributes query, the answer
    /// whose arrival ends the first frame's wait.
    pub fn attributes(&self) -> bool {
        self.answer.attributes.load(Ordering::Acquire)
    }

    /// Records the answer and wakes a frame that waits for it. The first answer
    /// is the one kept: a terminal answers one query once.
    pub fn record(&self, color: Rgb) {
        let packed =
            (1 << 24) | (u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b);
        if self
            .answer
            .color
            .compare_exchange(NONE, packed, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.answer.ready.notify_waiters();
    }

    /// Records the device attributes answer and wakes the first frame's wait.
    /// The first answer is the one kept: a terminal answers one query once.
    pub fn record_attributes(&self) {
        if self.answer.attributes.swap(true, Ordering::AcqRel) {
            return;
        }
        self.answer.ready.notify_waiters();
    }

    /// Waits until the terminal answered the background query, and returns at
    /// once when it already did.
    pub async fn wait(&self) {
        self.until(|answer| answer.color().is_some()).await;
    }

    /// Waits until the terminal answered the device attributes query, and
    /// returns at once when it already did.
    pub async fn wait_attributes(&self) {
        self.until(Background::attributes).await;
    }

    /// Waits until `done` holds for the answers recorded so far, and registers
    /// the waiter before it looks, so an answer that lands between the two
    /// wakes this wait instead of being missed. An answer of the other query
    /// wakes it too, and it waits again.
    async fn until(&self, done: fn(&Background) -> bool) {
        loop {
            let ready = self.answer.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            if done(self) {
                return;
            }
            ready.as_mut().await;
        }
    }
}

impl PartialEq for Background {
    /// Two handles are equal when they hold the same answer, so an
    /// [`Options`](crate::app::Options) compares as the value it names.
    fn eq(&self, other: &Self) -> bool {
        self.color() == other.color()
    }
}

/// The terminal's input: the tty, the window change signal, and the bytes read
/// and not yet parsed.
#[derive(Debug)]
pub struct Input {
    /// The tty, read-only and non-blocking, on its own file description.
    tty: AsyncFd<Tty>,
    /// The window change signal.
    resize: SignalEvent,
    /// The bytes read and not yet parsed.
    parser: Parser,
    /// Where the answers to the first frame's two queries go.
    background: Background,
}

impl Input {
    /// Opens the terminal for reading, starts listening for a resize, and
    /// records the answers to the first frame's queries into `background`.
    pub async fn open(background: Background) -> io::Result<Input> {
        let file = open_tty().await?;
        // SAFETY: `file` owns its descriptor and `AsyncFd` owns `file`, so the
        // descriptor stays open and unchanged for as long as it is registered.
        let tty = unsafe { AsyncFd::register(file) }.map_err(io::Error::from)?;
        Ok(Input {
            tty,
            resize: tokio::signal::unix::signal(SignalKind::window_change())?,
            parser: Parser::new(),
            background,
        })
    }

    /// The next event: the next whole event in the bytes already read, else the
    /// bytes the tty has, else a window change.
    ///
    /// The answers to the first frame's two queries are not events: they go into
    /// the [`Background`] handle and wake the frame that waits for them, so no
    /// byte of either is ever read as a key.
    ///
    /// This is cancel safe. The bytes read stay in the parser and the events
    /// parsed out of them stay in `self`, so a key is not lost when the future
    /// is dropped, which the app does whenever another of its branches is ready.
    pub async fn next(&mut self) -> io::Result<Event> {
        loop {
            while let Some(signal) = self.parser.next_event() {
                match signal {
                    Signal::Event(event) => return Ok(event),
                    Signal::Background(color) => self.background.record(color),
                    Signal::DeviceAttributes => self.background.record_attributes(),
                }
            }
            let Input {
                tty,
                resize,
                parser,
                ..
            } = self;
            tokio::select! {
                filled = fill(tty, parser) => filled?,
                _ = resize.recv() => {
                    let (columns, rows) = crossterm::terminal::size().unwrap_or(SIZE);
                    return Ok(Event::Resize(columns, rows));
                }
            }
        }
    }
}

/// Reads the bytes the tty has, and returns once it would block.
///
/// `try_io` clears the readiness when the last read returns `WouldBlock`, and
/// the kernel sets it again the moment bytes arrive: that is why this drains
/// the tty instead of waiting for another readiness.
async fn fill(tty: &mut AsyncFd<Tty>, parser: &mut Parser) -> io::Result<()> {
    let mut ready = tty.readable().await?;
    loop {
        match ready.try_io(|tty| read(tty, parser)) {
            Ok(Ok(0)) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the terminal closed its input",
                ));
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(error)) => return Err(error),
            // No byte is left in the tty: the readiness is cleared for the next
            // wait, and this wake is done.
            Err(_would_block) => return Ok(()),
        }
    }
}

/// Reads one chunk of the tty into the room the parser has for it, and answers
/// how many bytes it read.
fn read(tty: &AsyncFd<Tty>, parser: &mut Parser) -> io::Result<usize> {
    let room = parser.tail();
    let mut file = tty.get_ref();
    let read = file.read(room)?;
    let full = read == room.len();
    parser.commit(read, full);
    Ok(read)
}

/// Opens the tty for reading: the terminal of fd 0, 1 or 2 by its own device
/// path, else `/dev/tty`.
///
/// The terminal is opened read-only, on a file description of its own, so the
/// `O_NONBLOCK` the reader needs belongs to that descriptor alone. fd 0 and fd 1
/// share their file description with the shell that started reagent, and a
/// non-blocking stdin would leave the shell reading, and this process writing,
/// `EAGAIN` once reagent exits; a duplicate of fd 0 shares that description, so
/// its `O_NONBLOCK` would be stdin's, and stdout's too whenever the two are dups
/// of one open, which is how a pty is handed to a child.
///
/// The device path comes first because macOS kqueue refuses `/dev/tty` with
/// `EINVAL`: it is an alias that kqueue cannot watch, while the device it names
/// (such as `/dev/ttys051`) works. `/dev/tty` is the last resort, for a process
/// whose three standard descriptors are all redirected away from its terminal.
async fn open_tty() -> io::Result<Tty> {
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if let Ok(path) = tty_path(fd) {
            return open(path).await;
        }
    }
    open("/dev/tty").await
}

/// Opens `path` read-only, with the reader's flags, as a descriptor the reactor
/// waits on.
async fn open(path: impl AsRef<Path>) -> io::Result<Tty> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FLAGS)
        .open(path)
        .await?;
    Ok(file.into_std().await)
}

/// The device path of the terminal behind `fd`, as `ttyname` names it; an
/// error when `fd` is no terminal.
fn tty_path(fd: libc::c_int) -> io::Result<PathBuf> {
    let mut name = [0 as libc::c_char; libc::PATH_MAX as usize];
    // SAFETY: `name` is a buffer of the length the call is given, so
    // `ttyname_r` writes no more than that into it; a closed or non-terminal
    // `fd` makes it return an error, not write.
    let found = unsafe { libc::ttyname_r(fd, name.as_mut_ptr(), name.len()) };
    if found != 0 {
        return Err(io::Error::from_raw_os_error(found));
    }
    // SAFETY: a successful `ttyname_r` wrote a NUL-terminated path into the
    // buffer, which `from_ptr` reads up to that NUL and no further.
    let name = unsafe { CStr::from_ptr(name.as_ptr()) };
    Ok(PathBuf::from(OsStr::from_bytes(name.to_bytes())))
}
