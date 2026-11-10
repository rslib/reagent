//! Allocation counts for the TUI app's frames.
//!
//! The app runs against a fake worker on a unix socket and a terminal written
//! here that counts its `write_frame` calls and keeps the bytes, so a number is
//! the app's own work: one key that grows the editor by a character redraws and
//! writes its frame, a streamed delta lands in the live reply's buffer, and
//! every frame leaves in one write. Once the buffers have their capacity none
//! of it allocates, and a delta that does not finish a block costs the same
//! however long the reply behind it is.
//!
//! The full-screen cases run the app the same way, on a terminal wide enough for
//! the sidebar to dock, and reach the transcript ring through [`Screen`]: the
//! sidebar is painted in the frame, a scroll frame costs the same however many
//! lines the ring stores, and a line pushed into a ring past its cap allocates
//! nothing.
//!
//! The styled cases run the same app in the styled look the brief lays out: the
//! terminal answers the background query with a dark background, so every role
//! is a fill mixed from it, and a frame draws the sidebar, the text box, the
//! busy bar, a paste chip, the approval card and the committed rows of a run in
//! that look, over a code block of a reply and over an edit's diff opened side by
//! side. Each of them allocates nothing once its buffers have their
//! capacity.
//!
//! The summary cases end twenty runs of nine calls each and draw the row every
//! run folded into: a steady frame over those rows and a page up and back over
//! them allocate nothing, and neither does a steady frame over a run that still
//! goes, whose calls are its own rows. The blocks one more settled call costs do
//! not follow the length of the run it joins.
//!
//! The open-output cases write a session whose journal holds the tool result of
//! every call they settle, and settle a call from a result that carries the tail
//! of its output, as the worker writes it, so a call draws its tail when its row
//! is open and reads no record: a steady frame over ten open rows, each under the
//! count of the lines it leaves out, allocates nothing, and one more call with
//! output costs the same after five calls as after fifty.
//!
//! The card cases settle the calls whose tools draw as a card, so ten `bash`
//! cards, three `edit` cards and five plain rows are on screen at once: a steady
//! frame over them allocates nothing, and neither does one over a run that folded
//! with the red card of its failed first call under its summary row. One more
//! `bash` card with output costs the same blocks after five calls as after fifty,
//! and a result that carries no tail still fills from the journal.
//!
//! The list and sheet cases keep a list above the text box or a sheet in its
//! place and type into its query: the command list, the palette, the file list,
//! the key help and the which-key list are each built once, so a key that only
//! refilters them and the frame it draws cost no heap block.
//!
//! dhat allows one profiler per process, so every test locks [`SERIAL`] for its
//! whole body: a test that ran beside a profiler would be counted in its
//! numbers.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use dhat::{HeapStats, Profiler};
use reagent_ai::record::{MESSAGE, encode_message};
use reagent_ai::types::ImageType;
use reagent_ai::{Content, Image, Message, ToolResult};
use reagent_protocol::lines::Lines;
use reagent_protocol::message::{self, Request};
use reagent_store::{Header, Session, Writer};
use reagent_tui::app::{Exit, HostHandle, Options, run};
use reagent_tui::host::FakeHost;
use reagent_tui::input::{Background, Parser};
use reagent_tui::screen::{Run, SYNC_END, SYNC_START, Screen, TRANSCRIPT_RING_LINES, Terminal};
use reagent_tui::theme::Rgb;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::SERIAL;

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The runtime a test runs the app on: one thread, as the app's own runtime is.
///
/// A test takes [`SERIAL`] first and `block_on` its body, so no lock is held
/// across an await point.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

/// How many times a measurement runs. dhat counts the blocks of every thread of
/// the process, so one sample can carry a block that is not the work's; the
/// smallest of several is the work's own count.
const SAMPLES: usize = 3;

/// How long the harness waits for a frame before it fails.
const WAIT: Duration = Duration::from_secs(30);

/// The terminal a test draws on: narrow for the steady frame, and as many rows
/// as a real one.
const COLUMNS: u16 = 80;
const ROWS: u16 = 24;

/// The rows of the screen of the steady cards case: the last `bash` card, the
/// plain rows of the reads above it and the text box under it.
const CARD_SCREEN: u16 = 40;

/// The columns the streaming test lays the reply out at: wide enough that a run
/// of deltas stays on the rows it already has.
const WIDE: u16 = 2000;

/// The terminal of the full-screen cases: at 160 columns `layout` docks the 40
/// column sidebar on the right and leaves 120 for the transcript, and 24 rows is
/// a real terminal.
const SIDEBAR: u16 = 160;

/// The lines the full-screen scroll test keeps in the ring: one page and a long
/// transcript.
const SHORT_LINES: usize = 500;
const LONG_LINES: usize = 5_000;

/// The lines the ring test pushes before it measures, so the ring is past its
/// cap [`TRANSCRIPT_RING_LINES`] twice over and its byte buffer and run list have
/// grown and compacted at least once.
const RING_LINES: usize = 2 * TRANSCRIPT_RING_LINES;

/// One stored line of the ring, the same text every time so the ring's byte
/// buffer grows by a fixed number of bytes per line.
const LINE: &str = "a stored line of the transcript";

/// The delta of the streaming test: a word and the space after it, which no
/// markdown construct can start or close, so the open block is never re-parsed.
const DELTA: &str = "word ";

/// The deltas that warm the reply's text, rows and pieces before a measurement,
/// so the row the next delta grows into has already been taken.
const WARM: usize = 2000;

/// The lead of the steady test: enough keys that the editor's gap buffer, the
/// rows of its layout and the status line all have their capacity.
const KEYS: usize = 64;

/// How many frames and keys a harness can hold without the app blocking on it.
const FRAMES: usize = 16_384;
const PRESSES: usize = 64;

/// The paths the file list case's host answers with: enough that the list's
/// hits, its marks and its window have their capacity after the warm-up.
const FILES: usize = 2_000;

/// The bytes the terminal keeps, in total. A frame that would pass it is
/// counted and dropped, so the bookkeeping never grows under a measurement.
const BYTES: usize = 8 << 20;

/// The blocks one steady frame costs: none. A key clears the app's deadline,
/// so the frame is drawn in the same wake and arms no timer; the editor's gap
/// buffer, the rows of its layout, the frame bytes and the interned styles all
/// have their capacity after the warm-up.
const STEADY_BLOCKS: u64 = 0;

/// The blocks one streamed delta and its frame cost, at most. 1 measured: the
/// frame timer a delta arms (`tokio::time::sleep_until` allocates its entry
/// once per armed frame), and it does not follow the reply.
const DELTA_BLOCKS: u64 = 2;

/// The blocks a second call queued behind an open approval card costs: the app
/// keeps its own copy of the call, its rule and the rules it saves, measured at 21.
/// It is the request's data, not a frame's work, and the card's own redraw adds
/// nothing to it.
const QUEUED_BLOCKS: u64 = 21;

/// The messages the history search case sends before it measures: enough that
/// the search walks a history and not the empty box.
const HISTORY: usize = 50;

/// What those messages start with, so the typed key of the search finds them.
const QUERY: &str = "cargo";

/// The blocks a key of the search costs, at most: 0 measured on a history of
/// [`HISTORY`] messages. The query, the prompt, the rows of the match and the gap
/// buffer all have their capacity after the warm-up.
const SEARCH_BLOCKS: u64 = 0;

/// The background the styled cases' terminal answers the app's `OSC 11` query
/// with: the dark one a terminal in a dark theme reports, which puts the look in
/// rich mode and every role on a fill mixed from it.
const DARK: Rgb = Rgb::new(0x14, 0x14, 0x14);

/// The two ways a frame writes a background color: true color, and the palette
/// index a terminal with fewer colors takes it at.
const FILLS: [&str; 2] = ["\u{1b}[48;2;", "\u{1b}[48;5;"];

/// The word the status line shows while a run is on screen.
const WORKING: &str = "working";

/// The lines the paste case folds into one chip, its chip saying how many the
/// tab would open.
const PASTE_LINES: usize = 142;

/// The glyph a folded paste chip opens with, and the count it holds back.
const CHIP: &str = "\u{2261}";
const CHIP_MORE: &str = "\u{2026} +";

/// The rows the committed-rows case puts in the transcript before the end of the
/// turn: enough paragraphs that the viewport can scroll to the rows under them.
const PARAGRAPHS: usize = 30;

/// The diff the committed-rows case shows under its tool row: six lines folded
/// to the four a folded edit shows.
const OLD: &str = "one\ntwo";
const NEW: &str = "three\nfour\nfive\nsix";

/// The Rust lines of the block the code-block case streams: more than the rows a
/// 24 row viewport shows, so the frame draws code and not the block's start.
const CODE_LINES: usize = 40;

/// The runs the summary cases end into one row each: enough that the page of the
/// transcript the viewport shows holds several of those rows.
const RUNS: usize = 20;

/// The calls of one of those runs: its first [`BASH_CALLS`] of them `bash` and
/// the rest `edit` with a small diff, so its row reads `bash 5 · edit 4`.
const CALLS: usize = 9;
const BASH_CALLS: usize = 5;

/// The calls the two runs of the settled-call case hold before the call it
/// measures: the shortest and the longest run its number is taken in.
const SHORT_RUN: usize = 5;
const LONG_RUN: usize = 50;

/// The calls the open-output steady case settles in one run: ten rows that each
/// draw the tail of their output once the key opens them.
const OUTPUT_CALLS: usize = 10;

/// The lines each call of the open-output cases prints: more than the row of a
/// call shows, so the row above them counts the ones it leaves out.
const OUTPUT_LINES: usize = 40;

/// The lines of a card's output its body shows while the card is folded, which
/// the count row under them leaves out the rest of.
const CARD_BODY: usize = 10;

/// The calls of the cards case: the `edit` cards with a small diff that follow
/// its `bash` cards, the plain `read` rows after them, and all of them.
const EDIT_CARDS: usize = 3;
const READ_ROWS: usize = 5;
const CARDS: usize = OUTPUT_CALLS + EDIT_CARDS + READ_ROWS + 1;

/// The calls the measured card of the card cases is settled after: the warm-up
/// in four calls, so the measured call joins a run whose buffers grew for the
/// cards before it. A card with its padding and the blank rows around it is tall
/// enough that two calls left a growth of the ring inside the run of five.
const CARD_WARM: usize = 4;

/// The calls the busy-bar case starts and never settles: a call that still runs
/// stays in the live area, whose rows the frame draws again on every step. Each
/// of them asks for an edit, since the diff a call in progress carries is what
/// the frame builds for it.
const RUNNING: usize = 3;

/// The bytes one step of the bar writes, at most: the filled run of its eight
/// cells, the elapsed time when the step crosses a second, and the block's two
/// markers. A step that redrew the row or the screen would not fit.
const BAR_STEP_BYTES: usize = 96;

/// How long [`Harness::settle`] lets the app draw before it fails: a run on
/// screen steps the bar every 100 ms for as long as it runs, so settling ends on
/// the steps alone rather than on a quiet window.
const SETTLE_WAIT: Duration = Duration::from_secs(10);

/// The blocks one key, one typed character or one mouse move costs the parser
/// that runs per key: none.
const INPUT_BLOCKS: u64 = 0;

/// How many bytes of the terminal's input the parser case feeds at a time: one
/// pty read of Linux, so no read is bigger than the terminal's own buffer.
const READS: usize = 4096;

/// How many events that input holds: 4,000 one-line scroll keys, 4,000 typed
/// characters and 1,000 mouse moves.
const INPUT_EVENTS: usize = 9_000;

/// Every byte the app wrote and how it wrote it.
#[derive(Debug)]
struct Counts {
    /// How many `write_frame` calls the app made.
    writes: usize,
    /// Where each frame starts in `bytes`.
    starts: Vec<usize>,
    /// The bytes of every frame, in order, up to [`BYTES`].
    bytes: Vec<u8>,
}

impl Counts {
    /// Empty counts whose buffers already have the capacity they need.
    fn new() -> Counts {
        Counts {
            writes: 0,
            starts: Vec::with_capacity(FRAMES),
            bytes: Vec::with_capacity(BYTES),
        }
    }

    /// Forgets the frames written so far, so the next count starts at zero.
    fn clear(&mut self) {
        self.writes = 0;
        self.starts.clear();
        self.bytes.clear();
    }

    /// The bytes of each frame written since the last [`Counts::clear`].
    fn frames(&self) -> Vec<&[u8]> {
        (0..self.starts.len())
            .map(|index| {
                let start = self.starts[index];
                let end = self
                    .starts
                    .get(index + 1)
                    .copied()
                    .unwrap_or(self.bytes.len());
                &self.bytes[start..end]
            })
            .collect()
    }
}

/// The terminal the app draws into: it counts the frames, keeps their bytes and
/// hands the test a ticket per frame, holding how many bytes that frame wrote.
/// Nothing in it allocates once its buffers have their capacity, so a measured
/// frame is the app's work alone.
struct Counting {
    counts: Arc<Mutex<Counts>>,
    /// The keys the test presses.
    keys: mpsc::Receiver<Event>,
    /// One ticket per frame the app wrote, carrying the frame's bytes.
    drawn: mpsc::Sender<usize>,
    size: (u16, u16),
}

impl Terminal for Counting {
    fn size(&self) -> (u16, u16) {
        self.size
    }

    async fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        counts.writes += 1;
        if counts.starts.len() < counts.starts.capacity() {
            let at = counts.bytes.len();
            counts.starts.push(at);
            if counts.bytes.len() + frame.len() <= counts.bytes.capacity() {
                counts.bytes.extend_from_slice(frame);
            }
        }
        drop(counts);
        // The app draws one last frame on its way out, after the test's side of
        // the ticket channel may be gone: that frame is nobody's measurement.
        let _ = self.drawn.try_send(frame.len());
        Ok(())
    }

    async fn next_event(&mut self) -> io::Result<Event> {
        match self.keys.recv().await {
            Some(event) => Ok(event),
            None => std::future::pending().await,
        }
    }
}

/// The look the app draws in: the plain one of a terminal that never answered
/// the background query, or the styled one of a terminal that answered it with
/// [`DARK`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Look {
    Terminal,
    Styled,
}

impl Look {
    /// The answer the app's background query gets from this terminal, if it
    /// gives one.
    fn answer(self) -> Option<Background> {
        match self {
            Look::Terminal => None,
            Look::Styled => {
                // Both answers before the app asks, as a terminal that answered
                // the queries the first frame writes one write earlier has.
                let answer = Background::default();
                answer.record(DARK);
                answer.record_attributes();
                Some(answer)
            }
        }
    }
}

/// The app, the fake worker it talks to and the terminal it draws on.
struct Harness {
    /// Held for the test, to remove the socket's folder at the end.
    _dir: tempfile::TempDir,
    counts: Arc<Mutex<Counts>>,
    keys: mpsc::Sender<Event>,
    drawn: mpsc::Receiver<usize>,
    /// The fake worker's side of the socket: it writes the frames the app reads
    /// and stays open, so the app never sees the session end.
    out: OwnedWriteHalf,
    /// The reading side, drained by a task for the same reason: the app writes its
    /// answers here, and a half nobody reads fills up and stops the app's writes.
    _read: JoinHandle<()>,
    _handle: JoinHandle<io::Result<Exit>>,
}

impl Harness {
    /// Starts the app on a socket in `/tmp`, where paths are short enough, and
    /// answers its handshake, in the mode the terminal `size` draws: full screen
    /// when `full`, inline otherwise.
    async fn start(size: (u16, u16), full: bool) -> Harness {
        Harness::with(size, full, Look::Terminal, false, None).await
    }

    /// Starts the app on a terminal that answered the background query with
    /// [`DARK`]: the styled look's rich mode, every role on a fill.
    async fn styled(size: (u16, u16), full: bool) -> Harness {
        Harness::with(size, full, Look::Styled, false, None).await
    }

    /// Starts the app styled, with a run on screen that keeps it busy, so the
    /// bar steps on its own clock without a key or a delta to draw a frame.
    async fn busy(size: (u16, u16), full: bool) -> Harness {
        Harness::with(size, full, Look::Styled, true, None).await
    }

    /// Starts the app styled, with `host` answering the lists that read it: the
    /// file list of `@`.
    async fn with_host(size: (u16, u16), full: bool, host: HostHandle) -> Harness {
        Harness::with(size, full, Look::Styled, false, Some(host)).await
    }

    /// Starts the app the way `look` says and answers its handshake, telling it a
    /// run is on screen when `running`, with `host` for the lists that read one.
    async fn with(
        size: (u16, u16),
        full: bool,
        look: Look,
        running: bool,
        host: Option<HostHandle>,
    ) -> Harness {
        Harness::build(size, full, look, running, host, 0, false).await
    }

    /// Starts the app styled on a session whose journal holds the tool result of
    /// each of `records` calls, so a call the worker settles reads the text of
    /// its output from the journal and draws its tail.
    async fn journaled(size: (u16, u16), records: usize) -> Harness {
        Harness::build(size, true, Look::Styled, false, None, records, false).await
    }

    /// Starts the app styled on a session whose journal holds a user message
    /// with one image, and the tool results that push it above the viewport.
    async fn pictured(size: (u16, u16)) -> Harness {
        Harness::build(size, true, Look::Styled, false, None, 0, true).await
    }

    /// Starts the app the way `with` does, on a session folder whose journal
    /// holds `records` tool results when they are more than none, or the picture
    /// of `pictured` when `pictured` says so.
    async fn build(
        size: (u16, u16),
        full: bool,
        look: Look,
        running: bool,
        host: Option<HostHandle>,
        records: usize,
        pictured: bool,
    ) -> Harness {
        let dir = tempfile::Builder::new()
            .prefix("fa")
            .tempdir_in("/tmp")
            .expect("a temp folder in /tmp");
        let socket: PathBuf = dir.path().join("s.sock");
        let listener = UnixListener::bind(&socket).expect("bind the socket");
        let (keys, presses) = mpsc::channel(PRESSES);
        let (drawn, frames) = mpsc::channel(FRAMES);
        let counts = Arc::new(Mutex::new(Counts::new()));
        let terminal = Counting {
            counts: Arc::clone(&counts),
            keys: presses,
            drawn,
            size,
        };
        let session_dir = if pictured {
            journal_pictured(dir.path())
        } else if records == 0 {
            // A folder that holds no session: the history read fails, which
            // costs the app one note, and the harness stays out of the real
            // state folder.
            dir.path().join("no-session")
        } else {
            journal(dir.path(), records)
        };
        let options = Options {
            socket,
            token: "token".to_owned(),
            session_dir: Some(session_dir),
            client_name: "frame-allocs".to_owned(),
            full,
            background: look.answer(),
            host,
            ..Options::default()
        };
        let handle = tokio::spawn(run(options, terminal));
        let (stream, _) = listener.accept().await.expect("the app connects");
        let (mut read, mut out) = stream.into_split();
        let mut lines = Lines::new();
        for _ in 0..2 {
            let (id, request) = next_request(&mut lines, &mut read).await;
            let body = match request {
                Kind::Attach if running => json!({"committed": 0, "running": true}),
                Kind::Attach => json!({"committed": 0}),
                Kind::Initialize => json!({
                    "major": message::MAJOR,
                    "version": "1",
                    "session": "frame-allocs",
                    "client": 1,
                    "capabilities": [],
                    "committed": 0,
                }),
            };
            let mut answer = Vec::new();
            message::result(&mut answer, id, &body);
            out.write_all(&answer).await.expect("the handshake answer");
        }
        // The app keeps writing its answers to this side: draining them keeps the
        // socket open and its buffer empty, so the app never sees the session end.
        let reader = tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            while read.read(&mut buffer).await.is_ok_and(|read| read > 0) {}
        });
        Harness {
            _dir: dir,
            counts,
            keys,
            drawn: frames,
            out,
            _read: reader,
            _handle: handle,
        }
    }

    /// Presses one character.
    fn press(&self, c: char) {
        self.send(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }

    /// Presses the Backspace key, which takes one char off the box or the open
    /// query.
    fn backspace(&self) {
        self.send(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    }

    /// Presses Esc.
    fn escape(&self) {
        self.send(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    }

    /// Presses Ctrl and `c`, one of the app's own keys.
    fn control(&self, c: char) {
        self.send(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    /// Presses one key, with the modifiers it needs: Shift+Up scrolls the
    /// transcript one line.
    fn send(&self, key: KeyEvent) {
        self.keys
            .try_send(Event::Key(key))
            .expect("the app takes the key");
    }

    /// Pastes `text` into the box, the way a terminal hands over a paste.
    fn paste(&self, text: String) {
        self.keys
            .try_send(Event::Paste(text))
            .expect("the app takes the paste");
    }

    /// Waits for the next frame the app writes.
    ///
    /// This takes no timer on purpose: a timeout inside a measurement would
    /// count the timer's own block. The tests run the same path with
    /// [`Harness::frame`] before they measure, so a key that draws nothing
    /// fails there instead of hanging here.
    async fn next_frame(&mut self) {
        self.drawn.recv().await.expect("the app still draws");
    }

    /// Waits for the next frame, failing when the app writes none.
    async fn frame(&mut self) {
        let got = tokio::time::timeout(WAIT, self.drawn.recv()).await;
        assert!(
            matches!(got, Ok(Some(_))),
            "the app wrote no frame within {WAIT:?}"
        );
    }

    /// Forgets the tickets of the frames written so far.
    fn drain(&mut self) {
        while self.drawn.try_recv().is_ok() {}
    }

    /// Writes one line of the worker into the socket.
    async fn write(&mut self, line: &[u8]) {
        self.out
            .write_all(line)
            .await
            .expect("the fake worker writes");
    }

    /// Waits until the app has drawn everything it has and writes no more than
    /// the steps of its clock: it drains the frames until a quiet window has no
    /// frame but a step in it.
    ///
    /// A run on screen steps its bar every 100 ms for as long as it runs, so a
    /// quiet window alone never comes while one is on screen. A step is a frame
    /// of its own and no bigger than [`BAR_STEP_BYTES`], so the window runs from
    /// the last frame that drew something and a step leaves it running.
    async fn settle(&mut self) {
        let quiet = Duration::from_millis(100);
        let deadline = tokio::time::sleep(quiet);
        tokio::pin!(deadline);
        let at = tokio::time::Instant::now();
        loop {
            tokio::select! {
                drawn = self.drawn.recv() => match drawn {
                    // A step of the bar or of the elapsed time: the frame it drew
                    // is the clock's, not the reply's, so it does not hold the
                    // window open.
                    Some(bytes) if bytes <= BAR_STEP_BYTES => {}
                    Some(_) => deadline.as_mut().reset(tokio::time::Instant::now() + quiet),
                    None => return,
                },
                () = &mut deadline => return,
            }
            assert!(
                at.elapsed() < SETTLE_WAIT,
                "the app kept drawing for {SETTLE_WAIT:?} and never settled"
            );
        }
    }

    /// The counts of the frames written so far.
    fn counts(&self) -> MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets the frames of the warm-up, so a count covers one test's own.
    fn reset(&mut self) {
        self.drain();
        self.counts().clear();
    }
}

/// Which handshake request the app just sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Initialize,
    Attach,
}

/// The next request the app sends, and whether it is the attach.
async fn next_request(lines: &mut Lines, read: &mut OwnedReadHalf) -> (u64, Kind) {
    loop {
        if let Some(Ok(line)) = lines.next() {
            let (id, request) = message::decode(line).expect("decode a request");
            let kind = match request {
                Request::Attach { .. } => Kind::Attach,
                _ => Kind::Initialize,
            };
            return (id.unwrap_or(0), kind);
        }
        let read = lines.fill(read).await.expect("a request");
        assert!(read > 0, "the app closed the socket");
    }
}

/// One event line of `type`, carrying `text`, as the worker would write it.
fn event(kind: &str, text: &str) -> Vec<u8> {
    notify(json!({"type": kind, "text": text}))
}

/// One event line of the worker's, from the `body` the worker would write.
fn notify(body: serde_json::Value) -> Vec<u8> {
    notice("event", body)
}

/// One line of `method` the worker sends as a notice, from the `body` it writes.
fn notice(method: &str, body: serde_json::Value) -> Vec<u8> {
    let params = serde_json::to_vec(&body).expect("the body");
    let mut line = Vec::new();
    message::notify(&mut line, method, &params);
    line
}

/// `count` lines, each its own paragraph, so the markdown renderer gives every
/// one of them its own row.
fn paragraphs(count: usize) -> String {
    let mut text = String::with_capacity(count * 12);
    for n in 1..=count {
        text.push_str("line ");
        text.push_str(&n.to_string());
        text.push_str("\n\n");
    }
    text
}

/// The reply the code-block case streams: one fenced Rust block of
/// [`CODE_LINES`] lines, every line of it a function with a type, a call, a
/// string and a number, so a frame that draws the block draws its code.
fn rust_block() -> String {
    let mut code = String::with_capacity(CODE_LINES * 64);
    code.push_str("```rust\n");
    for n in 0..CODE_LINES {
        code.push_str(&format!(
            "fn line_{n}(width: usize) -> usize {{ let s = \"text\"; width + {n} }}\n"
        ));
    }
    code.push_str("```\n");
    code
}

/// The end of the turn and the end of the run of a reply, so the app stores the
/// whole reply and starts the next one with an empty buffer.
fn run_end() -> Vec<u8> {
    let mut lines = turn_end();
    lines.extend(notify(json!({"type": "run_end", "reason": "end_turn"})));
    lines
}

/// The end of the turn alone: the live area goes into the ring, and the run stays
/// open.
fn turn_end() -> Vec<u8> {
    notify(json!({"type": "turn_end", "reason": "end_turn"}))
}

/// Whether the frames written so far drew line `number` of the transcript: the
/// diff writes a changed number in its own run of cells, and a whole row comes
/// out as one string.
fn drew(written: &str, number: usize) -> bool {
    written.contains(&format!("line {number}")) || written.contains(&format!("{number}\u{1b}"))
}

/// The blocks `body` allocates once, the smallest of [`SAMPLES`] runs.
///
/// The body has to be a macro argument: an async block borrows the harness, so
/// a closure could not hand it to a function that owns the loop.
macro_rules! blocks {
    ($body:expr) => {{
        let mut least = u64::MAX;
        for _ in 0..SAMPLES {
            let profiler = Profiler::builder().testing().build();
            $body.await;
            let stats = HeapStats::get();
            drop(profiler);
            least = least.min(stats.total_blocks);
        }
        least
    }};
}

/// A key that grows the editor by one character redraws the live area, diffs it
/// and writes the frame in no heap block: the editor's gap buffer, the rows of
/// its layout, the frame bytes and the interned styles all have their capacity
/// after the warm-up.
#[test]
fn perf_steady_frame_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        // Inline: the mode this case was written for.
        let mut harness = Harness::start((COLUMNS, ROWS), false).await;
        harness.frame().await;
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        // The same key and frame once outside the profiler, so a path that stopped
        // drawing fails here and not on the wait inside the measurement.
        harness.press('b');
        harness.next_frame().await;

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one key and the frame it draws");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one steady frame, over {STEADY_BLOCKS}"
        );
        // The extended keys request, the first frame, the key of every lead, the
        // one before the measurement and one per sample.
        let expected = 3 + SAMPLES;
        assert_eq!(
            harness.counts().writes,
            KEYS + expected,
            "every key draws exactly one frame"
        );
        drop(harness.out);
    });
}

/// A delta that does not finish a block appends to the open paragraph, so its
/// cost is the delta, not the reply behind it: the same delta into a reply with
/// 200 finished paragraphs costs what it costs into a reply with none.
#[test]
fn perf_streamed_delta_cost_does_not_follow_the_reply() {
    let _serial = serial();
    runtime().block_on(async {
        let short = delta_blocks(0).await;
        let long = delta_blocks(200).await;
        eprintln!(
            "blocks: {short} per delta after a short reply, {long} after a 200 paragraph one"
        );
        assert!(
            long <= DELTA_BLOCKS,
            "{long} blocks per delta after a long reply, over {DELTA_BLOCKS}"
        );
        assert!(
            long <= short + 2,
            "{long} blocks per delta after a long reply against {short} after a short one: \
         the cost follows the reply"
        );
    });
}

/// The blocks one delta into an open paragraph and the frame it draws cost,
/// after a reply whose `paragraphs` finished blocks are already on screen.
async fn delta_blocks(paragraphs: usize) -> u64 {
    // Inline: the mode this case was written for.
    let mut harness = Harness::start((WIDE, ROWS), false).await;
    harness.frame().await;
    if paragraphs > 0 {
        // A finished block: the frozen path is warm as well as the open one.
        let mut reply = String::new();
        for index in 0..paragraphs {
            reply.push_str(&format!("Paragraph {index} of the finished reply.\n\n"));
        }
        harness.write(&event("text_delta", &reply)).await;
    }
    let warm = event("text_delta", &DELTA.repeat(WARM));
    harness.write(&warm).await;
    harness.settle().await;
    harness.reset();

    let delta = event("text_delta", DELTA);
    // The same delta and frame once outside the profiler, so a path that
    // stopped drawing fails here and not on the wait inside the measurement.
    harness.write(&delta).await;
    harness.next_frame().await;
    harness.reset();

    let measured = blocks!(async {
        harness.write(&delta).await;
        harness.next_frame().await;
    });
    assert!(harness.counts().writes >= SAMPLES, "a frame per delta");
    drop(harness.out);
    measured
}

/// One frame is one `write_frame` call, and its bytes are one DEC 2026 block:
/// the app never splits a frame over two writes.
#[test]
fn perf_each_frame_is_one_write() {
    let _serial = serial();
    runtime().block_on(async {
        // Inline: the mode this case was written for.
        let mut harness = Harness::start((COLUMNS, ROWS), false).await;
        harness.frame().await;
        for _ in 0..8 {
            harness.press('a');
            harness.frame().await;
        }
        harness.reset();

        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        let counts = harness.counts();
        eprintln!(
            "writes: {} for {KEYS} keys, {} bytes in {} frames",
            counts.writes,
            counts.bytes.len(),
            counts.starts.len()
        );
        assert_eq!(counts.writes, KEYS, "one write per frame");
        assert_eq!(counts.starts.len(), KEYS, "one frame per key");
        for (index, frame) in counts.frames().iter().enumerate() {
            assert!(
                frame.starts_with(SYNC_START),
                "frame {index} does not start with DEC 2026: {frame:?}"
            );
            assert!(
                frame.ends_with(SYNC_END),
                "frame {index} does not end with DEC 2026: {frame:?}"
            );
            assert_eq!(
                frame
                    .windows(SYNC_START.len())
                    .filter(|window| *window == SYNC_START)
                    .count(),
                1,
                "frame {index} is one DEC 2026 block: {frame:?}"
            );
        }
        drop(counts);
        drop(harness.out);
    });
}

/// A key that grows the editor in full screen draws the transcript, the sidebar
/// and the status line, diffs them and writes the frame in no heap block: the
/// ring's rows, the panels' components, the sidebar's lines, the frame bytes and
/// the interned styles all have their capacity after the warm-up.
#[test]
fn perf_full_screen_steady_frame_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::start((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        // The same key and frame once outside the profiler, so a path that stopped
        // drawing fails here and not on the wait inside the measurement.
        harness.press('b');
        harness.next_frame().await;

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one full-screen frame with the sidebar");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one steady full-screen frame, over {STEADY_BLOCKS}"
        );
        // The frame that enters the alternate screen, the first draw, the key of
        // every lead, the one before the measurement and one per sample.
        let expected = 3 + SAMPLES;
        assert_eq!(
            harness.counts().writes,
            KEYS + expected,
            "every key draws exactly one frame"
        );
        drop(harness.out);
    });
}

/// A delta that does not finish a block appends to the open paragraph in full
/// screen too, so its cost is the delta and the rows it redraws, not the reply
/// behind it: the same delta into a reply with 200 finished paragraphs costs what
/// it costs into a reply with none.
#[test]
fn perf_full_screen_streamed_delta_cost_does_not_follow_the_reply() {
    let _serial = serial();
    runtime().block_on(async {
        let short = full_delta_blocks(0).await;
        let long = full_delta_blocks(200).await;
        eprintln!(
            "blocks: {short} per full-screen delta after a short reply, {long} after a \
             200 paragraph one"
        );
        assert!(
            long <= DELTA_BLOCKS,
            "{long} blocks per full-screen delta after a long reply, over {DELTA_BLOCKS}"
        );
        assert!(
            long <= short + 2,
            "{long} blocks per full-screen delta after a long reply against {short} after a \
             short one: the cost follows the reply"
        );
    });
}

/// The blocks one delta into an open paragraph of a full-screen run and the frame
/// it draws cost, after a reply whose `paragraphs` finished blocks are already in
/// the ring. The terminal is [`WIDE`] for the reason the inline test's is.
async fn full_delta_blocks(paragraphs: usize) -> u64 {
    let mut harness = Harness::start((WIDE, ROWS), true).await;
    harness.frame().await;
    if paragraphs > 0 {
        let mut reply = String::new();
        for index in 0..paragraphs {
            reply.push_str(&format!("Paragraph {index} of the finished reply.\n\n"));
        }
        harness.write(&event("text_delta", &reply)).await;
    }
    let warm = event("text_delta", &DELTA.repeat(WARM));
    harness.write(&warm).await;
    harness.settle().await;
    harness.reset();

    let delta = event("text_delta", DELTA);
    harness.write(&delta).await;
    harness.next_frame().await;
    harness.reset();

    let measured = blocks!(async {
        harness.write(&delta).await;
        harness.next_frame().await;
    });
    assert!(harness.counts().writes >= SAMPLES, "a frame per delta");
    drop(harness.out);
    measured
}

/// A line pushed into a warm transcript ring allocates nothing, past the ring's
/// cap as well as under it: the text buffer, the run list and the line queue have
/// their capacity, and the compaction that drops the oldest line shifts them
/// without a block.
#[test]
fn perf_full_screen_committed_line_into_a_full_ring_allocates_nothing() {
    let _serial = serial();
    // The block is only here because `blocks!` awaits: the commit itself is a
    // synchronous push into the ring.
    runtime().block_on(async {
        let mut screen = Screen::full(SIDEBAR, ROWS);
        let runs = [Run::plain(LINE)];
        // One line of one run, held in a slice so nothing here is built per push.
        let span = 0..runs.len();
        let lines = std::slice::from_ref(&span);
        for _ in 0..RING_LINES {
            screen.commit(&runs, lines);
        }
        assert_eq!(
            screen.transcript().ring_len(),
            TRANSCRIPT_RING_LINES,
            "the ring holds its cap and no more"
        );
        assert_eq!(
            screen.transcript().dropped(),
            (RING_LINES - TRANSCRIPT_RING_LINES) as u64,
            "every line past the cap left the ring"
        );
        // The same commit once outside the profiler, so the path is warm and the
        // measurement is not the first line after a compaction.
        screen.commit(&runs, lines);

        let measured = blocks!(async {
            screen.commit(&runs, lines);
        });
        eprintln!("blocks: {measured} for one line into a ring of {RING_LINES} stored lines");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one line into a warm ring, over {STEADY_BLOCKS}"
        );
    });
}

/// One Shift+Up frame draws the rows the viewport shows and nothing else, so a
/// scroll of one line allocates no block and costs the same on a transcript of
/// 500 lines as on one of 5,000.
#[test]
fn perf_full_screen_scroll_frame_does_not_follow_the_ring() {
    let _serial = serial();
    runtime().block_on(async {
        let short = scroll_blocks(SHORT_LINES).await;
        let long = scroll_blocks(LONG_LINES).await;
        eprintln!(
            "blocks: {short} for one line of scroll on {SHORT_LINES} stored lines, {long} on \
             {LONG_LINES}"
        );
        assert_eq!(
            long, STEADY_BLOCKS,
            "{long} blocks for one line of scroll on {LONG_LINES} stored lines, over \
             {STEADY_BLOCKS}"
        );
        assert_eq!(
            short, STEADY_BLOCKS,
            "{short} blocks for one line of scroll on {SHORT_LINES} stored lines, over \
             {STEADY_BLOCKS}"
        );
        assert_eq!(
            long, short,
            "{long} blocks for one line of scroll on {LONG_LINES} lines against {short} on \
             {SHORT_LINES}: the cost follows the ring"
        );
    });
}

/// The blocks one Shift+Up frame of a full-screen run costs, its ring holding the
/// `lines` lines of one reply.
async fn scroll_blocks(lines: usize) -> u64 {
    let mut harness = Harness::start((SIDEBAR, ROWS), true).await;
    harness.frame().await;
    harness
        .write(&event("text_delta", &paragraphs(lines)))
        .await;
    harness.write(&run_end()).await;
    // A quiet window can pass under load before the first frame of a long reply
    // lands, so settle until the last line is on screen: then the ring holds all.
    let at = tokio::time::Instant::now();
    loop {
        harness.settle().await;
        let written = screen_text(&harness.counts().bytes);
        if drew(&written, lines) {
            break;
        }
        assert!(
            at.elapsed() < SETTLE_WAIT,
            "the last of {lines} lines never reached the transcript ring: {written:?}"
        );
    }
    let up = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);
    for _ in 0..4 {
        harness.send(up);
        harness.frame().await;
    }
    harness.settle().await;
    harness.reset();

    // The same scroll and frame once outside the profiler, so a key that draws
    // nothing fails here and not on the wait inside the measurement.
    harness.send(up);
    harness.next_frame().await;
    harness.reset();

    let measured = blocks!(async {
        harness.send(up);
        harness.next_frame().await;
    });
    assert!(harness.counts().writes >= SAMPLES, "a frame per scroll");
    drop(harness.out);
    measured
}

/// One full-screen frame is one `write_frame` call and its bytes are one DEC 2026
/// block, with the sidebar the wide terminal docks painted in the frame that
/// first draws it.
#[test]
fn perf_full_screen_frame_is_one_write() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::start((SIDEBAR, ROWS), true).await;
        // The frame that enters the alternate screen, then the first draw of the
        // whole terminal, which is where the sidebar shows.
        harness.frame().await;
        harness.frame().await;
        let painted = screen_text(&harness.counts().bytes);
        assert!(
            painted.contains("Context"),
            "the docked sidebar is not in the first frame: {painted:?}"
        );
        for _ in 0..8 {
            harness.press('a');
            harness.frame().await;
        }
        harness.reset();

        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        let counts = harness.counts();
        eprintln!(
            "writes: {} for {KEYS} full-screen frames, {} bytes in {} frames",
            counts.writes,
            counts.bytes.len(),
            counts.starts.len()
        );
        assert_eq!(counts.writes, KEYS, "one write per frame");
        assert_eq!(counts.starts.len(), KEYS, "one frame per key");
        for (index, frame) in counts.frames().iter().enumerate() {
            let text = screen_text(frame);
            assert!(
                frame.starts_with(SYNC_START),
                "frame {index} does not start with DEC 2026: {text:?}"
            );
            assert!(
                frame.ends_with(SYNC_END),
                "frame {index} does not end with DEC 2026: {text:?}"
            );
            assert_eq!(
                frame
                    .windows(SYNC_START.len())
                    .filter(|window| *window == SYNC_START)
                    .count(),
                1,
                "frame {index} is one DEC 2026 block: {text:?}"
            );
        }
        drop(counts);
        drop(harness.out);
    });
}

/// A key that grows the editor in the styled look, with the sidebar docked,
/// redraws the transcript, the sidebar and the status line, diffs them and writes
/// the frame in no heap block: the fills, the sidebar's lines, the panel's
/// components, the frame bytes and the interned styles all have their capacity
/// after the warm-up.
#[test]
fn perf_styled_full_screen_steady_frame_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        harness.frame().await;
        let painted = screen_text(&harness.counts().bytes);
        assert!(
            painted.contains("Context"),
            "the docked sidebar is not in the first frame: {painted:?}"
        );
        assert!(
            filled(&painted),
            "the frame drew no role fill, so the styled look is not on: {painted:?}"
        );
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        // The same key and frame once outside the profiler, so a path that stopped
        // drawing fails here and not on the wait inside the measurement.
        harness.press('b');
        harness.next_frame().await;

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one styled full-screen frame with the sidebar");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one styled full-screen frame, over {STEADY_BLOCKS}"
        );
        drop(harness.out);
    });
}

/// The bytes a frame that shows the cursor writes before the block's end: the
/// edit point's cell and the cursor turned on.
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

/// The bytes a frame that shows the cursor writes before its cells: the cursor
/// turned off, so it never trails the text it belongs to.
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";

/// One key typed into the text box draws its frame in no heap block, and the
/// frame ends with the cursor sequences inside its one write: the app moves the
/// cursor to the edit point and shows it in the same DEC 2026 block the cells
/// went out in, so the terminal never paints a cursor the frame has left behind.
#[test]
fn perf_styled_key_allocates_nothing_and_ends_its_frame_with_the_cursor() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        harness.press('b');
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        let counts = harness.counts();
        eprintln!(
            "blocks: {measured}, writes: {} for one key typed into the text box",
            counts.writes
        );
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one key and the frame it draws, over {STEADY_BLOCKS}"
        );
        assert_eq!(counts.writes, SAMPLES, "one write per key and sample");
        assert_eq!(counts.starts.len(), SAMPLES, "one frame per key and sample");
        let frames = counts.frames();
        let frame = frames.last().expect("the last sample's frame");
        let shown = screen_text(frame);
        assert!(
            frame.starts_with(SYNC_START) && frame.ends_with(SYNC_END),
            "the frame is not one DEC 2026 block: {shown:?}"
        );
        assert!(
            frame.windows(HIDE_CURSOR.len()).any(|w| w == HIDE_CURSOR),
            "the frame drew its cells with the cursor shown: {shown:?}"
        );
        let body = frame
            .strip_suffix(SYNC_END)
            .and_then(|body| body.strip_suffix(SHOW_CURSOR))
            .unwrap_or_else(|| panic!("the frame does not end with the cursor on: {shown:?}"));
        assert!(
            body.ends_with(b"H"),
            "the cursor is not moved to its cell right before it is shown: {shown:?}"
        );
        drop(counts);
        drop(harness.out);
    });
}

/// One step of the busy bar draws the row again in no heap block and writes a few
/// bytes: the run's bar steps on its own clock, so no key and no delta is in the
/// measurement, and the frame redraws the cells the step changed.
#[test]
fn perf_styled_busy_bar_step_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::busy((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        // The bar steps on its own clock, so the frames come with no key: four
        // steps first, so the bar, the elapsed time and the row they redraw have
        // been through every change a step makes.
        for _ in 0..4 {
            harness.frame().await;
        }
        let painted = screen_text(&harness.counts().bytes);
        assert!(
            painted.contains(WORKING),
            "the run is not on screen, so no bar steps: {painted:?}"
        );
        harness.reset();
        // One step outside the profiler first, so a bar that stopped moving fails
        // here and not on the wait inside the measurement.
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.next_frame().await;
        });
        let counts = harness.counts();
        let step = counts.frames().iter().map(|frame| frame.len()).max();
        eprintln!(
            "blocks: {measured}, bytes: {} for one step of the busy bar",
            step.unwrap_or(0)
        );
        assert!(
            counts
                .frames()
                .first()
                .is_some_and(|frame| frame.starts_with(SYNC_START)),
            "the step is not a frame of its own"
        );
        drop(counts);
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one step of the busy bar, over {STEADY_BLOCKS}"
        );
        let step = step.unwrap_or(0);
        assert!(
            step <= BAR_STEP_BYTES,
            "one step of the bar wrote {step} bytes, over {BAR_STEP_BYTES}"
        );
        drop(harness.out);
    });
}

/// A frame with a paste of [`PASTE_LINES`] lines folded into one chip draws the
/// chip, its first line and the count it holds back in no heap block: the chip is
/// drawn from the ranges of its one buffer, so no frame copies the paste.
#[test]
fn perf_styled_paste_chip_frame_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        harness.paste(pasted(PASTE_LINES));
        harness.frame().await;
        harness.settle().await;
        let painted = screen_text(&harness.counts().bytes);
        assert!(
            painted.contains(CHIP) && painted.contains(CHIP_MORE),
            "the paste is not folded into a chip: {painted:?}"
        );
        let held = format!("{CHIP_MORE}{}", PASTE_LINES - 1);
        assert!(
            painted.contains(&held),
            "the chip does not say how many lines it holds back ({held}): {painted:?}"
        );
        for _ in 0..2 {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        harness.reset();
        harness.press('a');
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('a');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one frame with a folded {PASTE_LINES} line paste chip");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for a frame with a folded chip, over {STEADY_BLOCKS}"
        );
        drop(harness.out);
    });
}

/// A frame with the approval card open draws its box, its row of answers, the call
/// it shows and the status line's wait in no heap block: the card keeps the buffers
/// of its call, its rule and its answers, so the open that follows a closed one
/// allocates nothing.
#[test]
fn perf_styled_approval_card_frame_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        // The card once outside the profiler, and closed again: its text, its rule
        // and its row of answers have their capacity when the measured one opens.
        harness.write(&approval(5, "cargo test --workspace")).await;
        harness.frame().await;
        harness.settle().await;
        let painted = screen_text(&harness.counts().bytes);
        assert!(
            painted.contains("Allow once") && painted.contains("cargo test --workspace"),
            "the card is not on screen: {painted:?}"
        );
        assert!(
            painted.contains("waits for you"),
            "the status line does not show the card waiting: {painted:?}"
        );
        harness.reset();

        // The second call queues behind the first, so the open card redraws the
        // count of the calls that wait. Its row of answers answers no key without
        // changing it (`Action::Ignore`), so this is the card's own redraw.
        let measured = blocks!(async {
            harness
                .write(&approval(6, "cargo test --workspace --all-targets"))
                .await;
            harness.frame().await;
        });
        eprintln!("blocks: {measured} for one frame with the approval card open");
        // Every block of the measured frame is the app's own copy of the request it
        // queued: the call, the rule and the saves it shows, 21 of them. The frame
        // that redraws the open card adds none.
        assert!(
            measured <= QUEUED_BLOCKS,
            "{measured} blocks for a frame with the card open, over {QUEUED_BLOCKS}"
        );

        // A redraw of the open card alone: Right moves the selection to the next
        // answer, which changes the card and nothing else, and costs no block.
        // A card takes keys only after a moment on screen (a key already being
        // typed must not answer it), so the measure waits that moment out first.
        tokio::time::sleep(Duration::from_millis(600)).await;
        harness.reset();
        let moved = blocks!(async {
            harness.send(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
            harness.frame().await;
        });
        eprintln!("blocks: {moved} for a redraw of the open card");
        assert_eq!(moved, 0, "a redraw of the open card allocates nothing");
        drop(harness.out);
    });
}

/// A scroll frame over a viewport of committed rows — a tool's row, the diff under
/// its edit, and the rule a turn ends with — allocates no heap
/// block: the frame draws the rows it shows, from the styles it interned and the
/// ranges the rows keep, and never from the transcript behind them.
#[test]
fn perf_styled_committed_row_frame_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        harness
            .write(&event("text_delta", &paragraphs(PARAGRAPHS)))
            .await;
        harness.frame().await;
        // The call on its own first, so its row is a live one: the diff under it is
        // drawn while it is the newest block.
        harness.write(&tool_call()).await;
        harness.frame().await;
        harness.write(&tool_end()).await;
        harness.settle().await;
        let painted = screen_text(&harness.counts().bytes);
        assert!(
            painted.contains("src/app.rs") && painted.contains("+4"),
            "the tool row is not on screen with the edit it made: {painted:?}"
        );
        assert!(
            painted.contains("tokens \u{b7} "),
            "the turn rule is not on screen: {painted:?}"
        );
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT);
        for _ in 0..2 {
            harness.send(up);
            harness.frame().await;
        }
        harness.settle().await;
        harness.reset();
        // The pair outside the profiler first, so a viewport that stopped moving
        // fails here and not on the wait inside the measurement.
        harness.send(up);
        harness.next_frame().await;
        harness.send(down);
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.send(up);
            harness.next_frame().await;
            harness.send(down);
            harness.next_frame().await;
        });
        let counts = harness.counts();
        let painted = screen_text(&counts.bytes);
        eprintln!(
            "blocks: {measured} for a scroll up and back over the committed rows, {} bytes in {} \
             frames",
            counts.bytes.len(),
            counts.starts.len()
        );
        drop(counts);
        // The rows are on screen before the measurement, asserted above; the pair
        // scrolls one line up and back, so each frame draws the viewport it shows
        // from the ring.
        let _ = painted;
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for the two scroll frames, over {STEADY_BLOCKS} for the pair"
        );
        drop(harness.out);
    });
}

/// A key on a viewport that shows a code block allocates no heap block: the
/// ring keeps the rows the block was committed with, their text and the styles
/// they were interned at, so the frame writes the cells it shows and finds no
/// token again.
#[test]
fn perf_steady_frame_with_a_code_block_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        harness.write(&event("text_delta", &rust_block())).await;
        harness.write(&run_end()).await;
        harness.settle().await;
        let painted = screen_text(&harness.counts().bytes);
        assert!(
            painted.contains("\"text\""),
            "the code block is not on screen: {painted:?}"
        );
        assert!(
            filled(&painted),
            "the frame drew no role fill, so the styled look is not on: {painted:?}"
        );
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        // The same key and frame once outside the profiler, so a path that stopped
        // drawing fails here and not on the wait inside the measurement.
        harness.press('b');
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one styled frame over a code block");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one frame over a code block, over {STEADY_BLOCKS}"
        );
        assert_eq!(harness.counts().writes, SAMPLES, "one write per sample");
        drop(harness.out);
    });
}

/// A key on a viewport that shows an edit's diff opened, its old lines at the
/// left and its new ones at the right, allocates no heap block: the ring holds
/// the rows the reopened block was drawn into, with their text and the styles
/// they were interned at.
#[test]
fn perf_steady_frame_with_an_open_side_by_side_diff_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        harness.write(&tool_start()).await;
        harness.write(&tool_call()).await;
        harness.frame().await;
        // The result and the end of the run: the live area goes into the ring, and
        // the edit is the newest finished block, which is the one Tab opens.
        harness.write(&tool_result()).await;
        harness.write(&run_end()).await;
        harness.settle().await;
        let folded = screen_text(&harness.counts().bytes);
        assert!(
            folded.contains("2 more lines \u{b7} tab opens") && !folded.contains("six"),
            "the diff is not on screen folded to four lines: {folded:?}"
        );
        assert!(
            filled(&folded),
            "the frame drew no role fill, so the styled look is not on: {folded:?}"
        );
        // Tab with an empty box opens the block, so the frame draws every line of
        // its diff, its old lines at the left and its new ones at the right.
        harness.send(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        harness.frame().await;
        harness.settle().await;
        let opened = {
            let counts = harness.counts();
            let frame = counts.frames().last().copied().unwrap_or_default();
            screen_text(frame)
        };
        assert!(
            opened.contains("five")
                && opened.contains("six")
                && opened.matches('\u{2502}').count() >= 2,
            "the diff did not open into two halves: {opened:?}"
        );
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        harness.press('b');
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one styled frame over an open side-by-side diff");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one frame over an open diff, over {STEADY_BLOCKS}"
        );
        assert_eq!(harness.counts().writes, SAMPLES, "one write per sample");
        drop(harness.out);
    });
}

/// A key on a viewport that shows the summary rows of ended runs allocates no
/// heap block: a run of calls folds into one row when it ends, drawn from the
/// members the ring keeps, so the frame writes the cells it shows and reaches no
/// call of the runs behind them.
#[test]
fn perf_steady_frame_over_summary_rows_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        harness.write(&summary_runs()).await;
        harness.settle().await;
        let painted = screen_text(&harness.counts().bytes);
        assert!(
            filled(&painted),
            "the frame drew no role fill, so the styled look is not on: {painted:?}"
        );
        let shown = words(&painted);
        assert!(
            shown.contains("\u{2717}bash5\u{b7}edit4"),
            "no summary row of a run of {CALLS} calls is on screen: {shown:?}"
        );
        assert!(
            shown.contains("1failed"),
            "the summary row does not count the call that failed: {shown:?}"
        );
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        // The same key and frame once outside the profiler, so a path that stopped
        // drawing fails here and not on the wait inside the measurement.
        harness.press('b');
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one styled frame over {RUNS} summary rows");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one frame over the summary rows, over {STEADY_BLOCKS}"
        );
        assert_eq!(harness.counts().writes, SAMPLES, "one write per sample");
        drop(harness.out);
    });
}

/// A key on a viewport that shows ten settled calls with their output tails open
/// allocates no heap block: the ring holds the rows the open block was drawn into,
/// each call's tail and the count of the lines over it, so the frame writes the
/// cells it shows and reaches no output of the calls behind them.
#[test]
fn perf_steady_frame_over_open_output_rows_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::journaled((SIDEBAR, ROWS), OUTPUT_CALLS).await;
        harness.frame().await;
        harness.write(&output_run()).await;
        harness.settle().await;
        // Ctrl+O opens every block: each call of the run draws its row, the count
        // of the lines over it and the tail of its output under that.
        harness.control('o');
        harness.frame().await;
        harness.settle().await;
        let painted = words(&screen_text(&harness.counts().bytes));
        assert!(
            painted.contains(&format!("part_{}_{}", OUTPUT_CALLS - 1, OUTPUT_LINES - 1)),
            "the tail of the newest call's output is not on screen: {painted:?}"
        );
        assert!(
            !painted.contains("morelines"),
            "an open card counts no lines it leaves out: {painted:?}"
        );
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        // The same key and frame once outside the profiler, so a path that stopped
        // drawing fails here and not on the wait inside the measurement.
        harness.press('b');
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one styled frame over {OUTPUT_CALLS} open output rows");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one frame over the open output rows, over {STEADY_BLOCKS}"
        );
        assert_eq!(harness.counts().writes, SAMPLES, "one write per sample");
        drop(harness.out);
    });
}

/// A key on a viewport that shows the cards of ten settled `bash` calls, three
/// `edit` calls and five plain rows allocates no heap block: the ring holds the
/// rows of every card, its bar, its title, the body of its diff or of the tail
/// of its output and the count of the lines it leaves out, so the frame writes
/// the cells it shows and reaches no call behind them.
#[test]
fn perf_steady_frame_over_cards_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        // The cards and the blank rows between them are taller than the other
        // cases' rows, so the screen is too: the plain rows of the reads sit
        // above the last card.
        let mut harness = Harness::journaled((SIDEBAR, CARD_SCREEN), CARDS).await;
        harness.frame().await;
        harness.write(&card_run()).await;
        harness.settle().await;
        let painted = words(&screen_text(&harness.counts().bytes));
        assert!(
            painted.contains(&format!("$cargotest-ppart_{}", CARDS - 1)),
            "no card draws the title of a `bash` call: {painted:?}"
        );
        assert!(
            painted.contains(&format!(
                "{}morelines\u{b7}tabopens",
                OUTPUT_LINES - CARD_BODY
            )),
            "no folded card counts the lines its tab opens: {painted:?}"
        );
        assert!(
            painted.contains("\u{2192}Read"),
            "no plain row of a read is on screen: {painted:?}"
        );
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        // The same key and frame once outside the profiler, so a path that stopped
        // drawing fails here and not on the wait inside the measurement.
        harness.press('b');
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one styled frame over {CARDS} cards and rows");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one frame over the cards, over {STEADY_BLOCKS}"
        );
        // Under load a journal read of the cards can land late and draw once more
        // inside a sample (4 writes against 3); the blocks are the least sample's.
        assert!(harness.counts().writes >= SAMPLES, "a frame per sample");
        drop(harness.out);
    });
}

/// A key on a viewport that shows a run folded into one summary row, with the
/// red card of its failed first call under the row, allocates no heap block: the
/// ring holds the card of the failure under the run's row, so the frame writes
/// the cells it shows and reaches neither the call nor the run's other calls.
#[test]
fn perf_steady_frame_over_a_folded_run_with_a_failed_card_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        harness.write(&failed_run()).await;
        harness.settle().await;
        let painted = words(&screen_text(&harness.counts().bytes));
        assert!(
            painted.contains("\u{2717}$cargotest-ppart_0"),
            "the red card of the failed call is not under the summary row: {painted:?}"
        );
        assert!(
            painted.contains("exitcode:1"),
            "the card of the failed call draws no error: {painted:?}"
        );
        assert!(
            painted.contains("bash4\u{b7}edit4"),
            "the run did not fold into its summary row: {painted:?}"
        );
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        harness.press('b');
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one styled frame over a folded run and its red card");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one frame over the folded run's failed card, over \
             {STEADY_BLOCKS}"
        );
        assert_eq!(harness.counts().writes, SAMPLES, "one write per sample");
        drop(harness.out);
    });
}

/// One more `bash` card with output costs the same blocks after five calls as
/// after fifty: the call's card is drawn from the bytes of its own output and
/// the rows of its own diff, so its cost does not follow the calls of the run
/// before it.
#[test]
fn perf_card_call_cost_does_not_follow_the_calls() {
    let _serial = serial();
    runtime().block_on(async {
        let short = output_call_blocks(SHORT_RUN).await;
        let long = output_call_blocks(LONG_RUN).await;
        eprintln!(
            "blocks: {short} per settled card after {SHORT_RUN} calls, {long} after {LONG_RUN}"
        );
        assert_eq!(
            long, short,
            "{long} blocks for one settled card after {LONG_RUN} calls against {short} after \
             {SHORT_RUN}: the cost follows the calls"
        );
    });
}

/// A page up and the page down after it over those summary rows allocate no heap
/// block: the scroll draws the rows the ring already holds, so a frame costs what
/// the viewport shows and not the calls each of the runs folded away.
#[test]
fn perf_scroll_frame_over_summary_rows_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        harness.write(&summary_runs()).await;
        harness.settle().await;
        let shown = words(&screen_text(&harness.counts().bytes));
        assert!(
            shown.contains("\u{2717}bash5\u{b7}edit4"),
            "no summary row of a run of {CALLS} calls is on screen: {shown:?}"
        );
        let up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        for _ in 0..2 {
            harness.send(up);
            harness.frame().await;
            harness.send(down);
            harness.frame().await;
        }
        harness.settle().await;
        harness.reset();
        // The pair outside the profiler first, so a viewport that stopped moving
        // fails here and not on the wait inside the measurement.
        harness.send(up);
        harness.next_frame().await;
        harness.send(down);
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.send(up);
            harness.next_frame().await;
            harness.send(down);
            harness.next_frame().await;
        });
        let counts = harness.counts();
        eprintln!(
            "blocks: {measured} for a page up and back over {RUNS} summary rows, {} bytes in {} \
             frames",
            counts.bytes.len(),
            counts.starts.len()
        );
        drop(counts);
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for the two scroll frames, over {STEADY_BLOCKS} for the pair"
        );
        drop(harness.out);
    });
}

/// A key on a viewport that shows a run still going allocates no heap block: the
/// run's calls are its own rows, held by the block the ring keeps open until the
/// run ends, so the frame draws the cells the key changed and no row of the run.
///
/// Each result of the run fails, which is what settles a call without a journal
/// to fill its output text from; a call left unsettled stays in the live area,
/// where the frame lays its row out again with every key.
#[test]
fn perf_steady_frame_while_a_run_is_open_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        // One call a turn, and no end of the run: the rows of the run stay on
        // screen as its own.
        let mut lines = Vec::new();
        for call in 0..CALLS {
            lines.extend(summary_call(0, call, true));
            lines.extend(turn_end());
        }
        harness.write(&lines).await;
        harness.settle().await;
        let shown = words(&screen_text(&harness.counts().bytes));
        assert!(
            shown.contains(&format!("part_{}", CALLS - 1)),
            "the newest call of the open run is not on screen: {shown:?}"
        );
        assert!(
            !shown.contains("bash5\u{b7}edit4"),
            "the run folded into one row while it still goes: {shown:?}"
        );
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
        }
        harness.settle().await;
        harness.press('b');
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one styled frame while a run of {CALLS} calls goes");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one frame while a run goes, over {STEADY_BLOCKS}"
        );
        drop(harness.out);
    });
}

/// A call that still runs draws its row again on every step of the busy bar in
/// no heap block: the live rows build their spans and the lines of their diff
/// without a buffer of their own, so a long call costs the same at its hundredth
/// step as at its first.
#[test]
fn perf_busy_frame_while_calls_run_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::busy((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        let mut lines = Vec::new();
        for call in 0..RUNNING {
            lines.extend(running_call(call));
        }
        harness.write(&lines).await;
        harness.settle().await;
        let painted = words(&screen_text(&harness.counts().bytes));
        assert!(
            painted.contains("part_0"),
            "the first call still running is not on screen: {painted:?}"
        );
        // The bar steps on its own clock, so the frames come with no key: four
        // steps first, so every row of the calls has been drawn again.
        for _ in 0..4 {
            harness.frame().await;
        }
        harness.reset();
        // One step outside the profiler first, so a bar that stopped moving fails
        // here and not on the wait inside the measurement.
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one step of the bar over {RUNNING} calls still running");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for one step over {RUNNING} calls still running, over \
             {STEADY_BLOCKS}"
        );
        drop(harness.out);
    });
}

/// One more settled call of a run costs the same after five calls as after
/// fifty: the call redraws its block and the rows it changed, so its blocks are
/// the call's and do not follow the length of the run it joins.
#[test]
fn perf_settled_call_cost_does_not_follow_the_run() {
    let _serial = serial();
    runtime().block_on(async {
        let short = settled_call_blocks(SHORT_RUN).await;
        let long = settled_call_blocks(LONG_RUN).await;
        eprintln!(
            "blocks: {short} per settled call after {SHORT_RUN} calls, {long} after {LONG_RUN}"
        );
        assert_eq!(
            long, short,
            "{long} blocks for one settled call after {LONG_RUN} calls against {short} after \
             {SHORT_RUN}: the cost follows the run"
        );
    });
}

/// One more call with output costs the same after five calls as after fifty: the
/// output the app reads from the journal is the record the worker just committed,
/// and filling it costs the call's own bytes, so its blocks do not follow the
/// calls of the run before it.
#[test]
fn perf_output_call_cost_does_not_follow_the_calls() {
    let _serial = serial();
    runtime().block_on(async {
        let short = output_call_blocks(SHORT_RUN).await;
        let long = output_call_blocks(LONG_RUN).await;
        eprintln!(
            "blocks: {short} per call with output after {SHORT_RUN} calls, {long} after \
             {LONG_RUN}"
        );
        assert_eq!(
            long, short,
            "{long} blocks for one call with output after {LONG_RUN} calls against {short} \
             after {SHORT_RUN}: the cost follows the calls"
        );
    });
}

/// A result with no tail of its own still fills from the journal: the app reads
/// the record the worker committed for the call and draws the count of the lines
/// its row folds away.
#[test]
fn perf_output_call_cost_without_a_tail_reads_the_journal_once() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::journaled((SIDEBAR, ROWS), SHORT_RUN).await;
        harness.frame().await;
        harness.write(&tail_less_call(0)).await;
        harness.settle().await;
        let painted = words(&screen_text(&harness.counts().bytes));
        assert!(
            painted.contains(&format!("{}morelines", OUTPUT_LINES - CARD_BODY)),
            "a result with no tail of its own never filled from the journal: {painted:?}"
        );
        drop(harness.out);
    });
}

/// A streamed delta keeps its bound in the styled look: it lands in the live
/// rows' buffer and redraws the rows it grew, so its cost is the delta, not the
/// fills behind it or the reply above it.
#[test]
fn perf_styled_streamed_delta_keeps_its_bound() {
    let _serial = serial();
    runtime().block_on(async {
        let short = styled_delta_blocks(0).await;
        let long = styled_delta_blocks(200).await;
        eprintln!(
            "blocks: {short} per styled delta after a short reply, {long} after a 200 \
             paragraph one"
        );
        assert!(
            long <= DELTA_BLOCKS,
            "{long} blocks per styled delta after a long reply, over {DELTA_BLOCKS}"
        );
        assert!(
            long <= short + 2,
            "{long} blocks per styled delta after a long reply against {short} after a short \
             one: the cost follows the reply"
        );
    });
}

/// The blocks one delta into an open paragraph of a styled run and the frame it
/// draws cost, after a reply whose `paragraphs` finished blocks are already on
/// screen. The terminal is [`WIDE`] for the reason the inline test's is.
async fn styled_delta_blocks(paragraphs: usize) -> u64 {
    // Full screen on a terminal wide enough that a run of deltas stays on the rows
    // it already has, as the inline case is.
    let mut harness = Harness::styled((WIDE, ROWS), true).await;
    harness.frame().await;
    if paragraphs > 0 {
        let mut reply = String::new();
        for index in 0..paragraphs {
            reply.push_str(&format!("Paragraph {index} of the finished reply.\n\n"));
        }
        harness.write(&event("text_delta", &reply)).await;
    }
    let warm = event("text_delta", &DELTA.repeat(WARM));
    harness.write(&warm).await;
    harness.settle().await;
    harness.reset();

    let delta = event("text_delta", DELTA);
    harness.write(&delta).await;
    harness.next_frame().await;
    harness.reset();

    let measured = blocks!(async {
        harness.write(&delta).await;
        harness.next_frame().await;
    });
    assert!(harness.counts().writes >= SAMPLES, "a frame per delta");
    drop(harness.out);
    measured
}

/// Whether a frame wrote a background color: the fill the styled look draws its
/// roles over.
fn filled(written: &str) -> bool {
    FILLS.iter().any(|fill| written.contains(fill))
}

/// `count` lines, one per row, as a terminal hands over a paste: the first line
/// of them names the row it starts on.
fn pasted(count: usize) -> String {
    let mut text = String::with_capacity(count * 12);
    for n in 1..=count {
        text.push_str("pasted line ");
        text.push_str(&n.to_string());
        text.push('\n');
    }
    text
}

/// The start of that call, which gives its row a fold of its own: the arguments
/// of [`tool_call`] come after it.
fn tool_start() -> Vec<u8> {
    notify(json!({"type": "tool_start", "id": "call_1", "name": "edit"}))
}

/// The call that edits a file, whose row the transcript draws with the diff under
/// it folded to its four lines.
fn tool_call() -> Vec<u8> {
    notify(json!({
        "type": "tool_call_ready",
        "id": "call_1",
        "name": "edit",
        "arguments": format!(
            r#"{{"path": "src/app.rs", "old_string": {OLD:?}, "new_string": {NEW:?}}}"#
        ),
    }))
}

/// The end of that call and of the turn it was in: its result, the tokens the turn
/// used, and the rule the turn ends with.
fn tool_end() -> Vec<u8> {
    let mut lines = tool_result();
    lines.extend(notify(
        json!({"type": "usage", "input": 1200, "output": 340, "cost": 0.03}),
    ));
    lines.extend(run_end());
    lines
}

/// The result of that call: the row, and the diff under it, land in the ring.
fn tool_result() -> Vec<u8> {
    notify(json!({"type": "tool_result", "id": "call_1", "bytes": 2}))
}

/// The end of the run alone, after the end of the turn it ran: the rows of its
/// calls fold into one summary row.
fn run_end_only() -> Vec<u8> {
    notify(json!({"type": "run_end", "reason": "end_turn"}))
}

/// The text `written` leaves on a terminal: its escape sequences taken out, so a
/// row's wording can be read without the styles between its words.
fn plain(written: &str) -> String {
    let mut text = String::with_capacity(written.len());
    let mut rest = written;
    while let Some(at) = rest.find('\u{1b}') {
        text.push_str(&rest[..at]);
        let mut chars = rest[at..].chars();
        chars.next();
        match chars.next() {
            // A control sequence runs to its final byte.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // A string runs to the bell that ends it, or to the escape after it.
            Some(']') => {
                for c in chars.by_ref() {
                    if c == '\u{7}' {
                        break;
                    }
                }
            }
            _ => {}
        }
        rest = chars.as_str();
    }
    text.push_str(rest);
    text
}

/// The words of a frame's text with its spaces gone: a cell that only holds a
/// space is the blank one a frame never writes, so the words of a row run
/// together in the bytes of the frames, and a row's wording is read without the
/// spaces between its parts.
/// The text a frame wrote.
fn screen_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn words(written: &str) -> String {
    plain(written)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// The lines of the `call`th call of the busy-bar case: an `edit` started and
/// given its arguments, with no result, so it still runs and its row carries the
/// lines of the diff the frame builds.
fn running_call(call: usize) -> Vec<u8> {
    let id = format!("running_{call}");
    let mut lines = notify(json!({"type": "tool_start", "id": id, "name": "edit"}));
    lines.extend(notify(json!({
        "type": "tool_call_ready",
        "id": id,
        "name": "edit",
        "arguments": format!(
            r#"{{"path": "src/part_{call}.rs", "old_string": {OLD:?}, "new_string": {NEW:?}}}"#
        ),
    })));
    lines
}

/// The lines of the `call`th call of the `run`th run of the summary cases: a
/// `bash` call for the first [`BASH_CALLS`] of them, an `edit` with a small diff
/// for the rest, and a result that fails when `failed` is set.
///
/// A call settles when its result carries output text or an error, and this
/// harness has no journal to fill the text from, so a call settles here only
/// when its result fails.
fn summary_call(run: usize, call: usize, failed: bool) -> Vec<u8> {
    let id = format!("call_{run}_{call}");
    let (name, arguments) = if call < BASH_CALLS {
        (
            "bash",
            format!(r#"{{"command": "cargo test -p part_{call}"}}"#),
        )
    } else {
        (
            "edit",
            format!(
                r#"{{"path": "src/part_{call}.rs", "old_string": {OLD:?}, "new_string": {NEW:?}}}"#
            ),
        )
    };
    let mut lines = notify(json!({"type": "tool_start", "id": id, "name": name}));
    lines.extend(notify(json!({
        "type": "tool_call_ready",
        "id": id,
        "name": name,
        "arguments": arguments,
    })));
    let result = if failed {
        json!({
            "type": "tool_result",
            "id": id,
            "is_error": true,
            "bytes": 2,
            "error": "exit code: 1",
        })
    } else {
        json!({"type": "tool_result", "id": id, "bytes": 2})
    };
    lines.extend(notify(result));
    lines
}

/// The lines of the `call`th call of the open-output cases: a `bash` call, its
/// result with the tail of its output, and the event that names the record the
/// journal holds that output in.
///
/// The result carries the tail of the call's output and the count of the lines
/// before it, as the worker writes them, so the call settles on its result and
/// reads no record. The output of a call is under the bytes the worker keeps,
/// so its tail is the whole of it and no line comes before it.
fn output_call(call: usize) -> Vec<u8> {
    let id = format!("call_{call}");
    let mut lines = call_start(&id, "bash", &bash_arguments(call));
    lines.extend(tailed_result(&id, call));
    lines.extend(committed(call + 1));
    lines
}

/// The same call of the open-output cases with a result that carries no tail of
/// its own: the app reads the text of its output from the record the worker
/// committed for it.
fn tail_less_call(call: usize) -> Vec<u8> {
    let id = format!("call_{call}");
    let mut lines = call_start(&id, "bash", &bash_arguments(call));
    lines.extend(tail_less_result(&id, call));
    lines.extend(committed(call + 1));
    lines
}

/// The arguments of a `bash` call that runs the test of part `call`.
fn bash_arguments(call: usize) -> String {
    format!(r#"{{"command": "cargo test -p part_{call}"}}"#)
}

/// The lines that start a call named `id` of `name` with its arguments: the
/// start and the event that carries them.
fn call_start(id: &str, name: &str, arguments: &str) -> Vec<u8> {
    let mut lines = notify(json!({"type": "tool_start", "id": id, "name": name}));
    lines.extend(notify(json!({
        "type": "tool_call_ready",
        "id": id,
        "name": name,
        "arguments": arguments,
    })));
    lines
}

/// The result line of the call `call`, named `id`, with the tail of the output
/// the journal records for it: the whole of that text, since it is under the
/// bytes the worker keeps, and no line before it.
fn tailed_result(id: &str, call: usize) -> Vec<u8> {
    let text = output(call);
    notify(json!({
        "type": "tool_result",
        "id": id,
        "bytes": text.len(),
        "output": text,
        "earlier": 0,
    }))
}

/// The result line of the call `call`, named `id`, with no tail of its own: the
/// app reads the text of its output from the record the worker committed.
fn tail_less_result(id: &str, call: usize) -> Vec<u8> {
    notify(json!({
        "type": "tool_result",
        "id": id,
        "bytes": output(call).len(),
    }))
}

/// The event that hands the app the record `record`: it reads the tool results
/// of the journal up to it and fills each call the records name.
fn committed(record: usize) -> Vec<u8> {
    notify(json!({"type": "committed", "record": record}))
}

/// [`OUTPUT_CALLS`] calls with output in one run, ended, so their rows fold into
/// one summary row the open key brings back with the tail of each call's output
/// under its row.
fn output_run() -> Vec<u8> {
    let mut lines = Vec::new();
    for call in 0..OUTPUT_CALLS {
        lines.extend(output_call(call));
    }
    lines.extend(turn_end());
    lines.extend(run_end_only());
    lines
}

/// The output of the `call`th call of the open-output cases: [`OUTPUT_LINES`]
/// lines, as a test run prints them, each naming the call it came from.
fn output(call: usize) -> String {
    let mut text = String::with_capacity(OUTPUT_LINES * 24);
    for line in 0..OUTPUT_LINES {
        text.push_str("test part_");
        text.push_str(&call.to_string());
        text.push('_');
        text.push_str(&line.to_string());
        text.push_str(" ... ok\n");
    }
    text
}

/// A session folder under `root` whose journal holds the tool result of each of
/// `records` calls, and that folder.
///
/// The results go in order from the first record, so the `call`th of them is
/// record `call + 1`, and each is appended as a base: a point a reader starts
/// decoding at, so the app reads the result the worker just committed rather than
/// the calls before it.
/// The 64 by 48 fixture PNG: the image of the journal [`journal_pictured`]
/// writes, the one the app draws as a picture.
const PICTURE: &[u8] = include_bytes!("../../../image/tests/fixtures/gradient-64x48.png");

/// How many tool results [`journal_pictured`] writes after the picture, so the
/// picture sits above the viewport of a full-screen app from its first frame.
const PICTURE_RECORDS: usize = 20;

/// The base64 text of `bytes`, as a journal holds an image.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        for (slot, byte) in buffer.iter_mut().zip(chunk) {
            *slot = *byte;
        }
        let value =
            (u32::from(buffer[0]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[2]);
        for index in 0..4 {
            if index > chunk.len() {
                out.push('=');
                continue;
            }
            let six = (value >> (18 - 6 * index)) & 0x3F;
            out.push(char::from(ALPHABET[six as usize]));
        }
    }
    out
}

/// A session whose journal holds a user message with one image, then
/// [`PICTURE_RECORDS`] tool results: the picture is in the ring and above the
/// viewport of the first frame.
fn journal_pictured(root: &Path) -> PathBuf {
    let name = "sess-picture";
    let dir = root.join(name);
    let header = Header::new(
        name.to_owned(),
        reagent_store::paths::now_ms(),
        None,
        Bytes::new(),
    );
    let mut writer = Writer::create(&dir, header).expect("the writer");
    let mut payload = Vec::new();
    encode_message(
        &mut payload,
        &Message::User {
            content: vec![
                Content::Text {
                    text: "look at this".into(),
                },
                Content::Image(Image {
                    media_type: ImageType::Png,
                    width: 64,
                    height: 48,
                    data: base64(PICTURE).into(),
                }),
            ],
        },
    );
    writer
        .append(MESSAGE, Bytes::from(payload), true)
        .expect("append the image");
    for call in 0..PICTURE_RECORDS {
        let mut payload = Vec::new();
        encode_message(
            &mut payload,
            &Message::ToolResult(ToolResult {
                call_id: format!("call_{call}").into(),
                content: output(call).into(),
                is_error: false,
                images: Vec::new(),
            }),
        );
        writer
            .append(MESSAGE, Bytes::from(payload), true)
            .expect("append a result");
    }
    writer.commit().expect("commit");
    writer.close().expect("close");
    dir
}

fn journal(root: &Path, records: usize) -> PathBuf {
    let name = "sess-output";
    let dir = root.join(name);
    let header = Header::new(
        name.to_owned(),
        reagent_store::paths::now_ms(),
        None,
        Bytes::new(),
    );
    let mut writer = Writer::create(&dir, header).expect("the writer");
    for call in 0..records {
        let mut payload = Vec::new();
        encode_message(
            &mut payload,
            &Message::ToolResult(ToolResult {
                call_id: format!("call_{call}").into(),
                content: output(call).into(),
                is_error: false,
                images: Vec::new(),
            }),
        );
        writer
            .append(MESSAGE, Bytes::from(payload), true)
            .expect("append a result");
    }
    writer.commit().expect("commit");
    writer.close().expect("close");
    let newest = Session::open(&dir)
        .expect("open the journal")
        .records()
        .filter_map(Result::ok)
        .map(|record| record.seq)
        .max()
        .unwrap_or(0);
    assert_eq!(
        newest, records as u64,
        "one record per call, from the first"
    );
    dir
}

/// [`OUTPUT_CALLS`] `bash` cards with the tail of their output, [`EDIT_CARDS`]
/// `edit` cards with their diff and [`READ_ROWS`] plain `read` rows, in one run
/// that does not end: its calls are its own rows, so the viewport draws the
/// cards themselves.
fn card_run() -> Vec<u8> {
    let mut lines = Vec::new();
    for call in 0..OUTPUT_CALLS {
        lines.extend(output_call(call));
    }
    for call in OUTPUT_CALLS..OUTPUT_CALLS + EDIT_CARDS {
        lines.extend(card_call(
            call,
            "edit",
            &format!(
                r#"{{"path": "src/part_{call}.rs", "old_string": {OLD:?}, "new_string": {NEW:?}}}"#
            ),
        ));
    }
    for call in OUTPUT_CALLS + EDIT_CARDS..CARDS - 1 {
        lines.extend(card_call(call, "read", r#"{"path": "src/app.rs"}"#));
    }
    // The newest call is a `bash` card, so the page the run ends on draws one.
    lines.extend(output_call(CARDS - 1));
    lines
}

/// The lines of the `call`th call of the cards case: a call of `name` with its
/// arguments, a result with the tail of its output and the event that names the
/// record of the journal it settles with.
fn card_call(call: usize, name: &str, arguments: &str) -> Vec<u8> {
    let id = format!("call_{call}");
    let mut lines = call_start(&id, name, arguments);
    lines.extend(tailed_result(&id, call));
    lines.extend(committed(call + 1));
    lines
}

/// One run of [`CALLS`] calls whose first call failed, ended: its calls fold
/// into one summary row and the red card of the failure stays under it.
fn failed_run() -> Vec<u8> {
    let mut lines = Vec::new();
    for call in 0..CALLS {
        lines.extend(summary_call(0, call, call == 0));
    }
    lines.extend(turn_end());
    lines.extend(run_end_only());
    lines
}

/// [`RUNS`] runs of [`CALLS`] calls, each with the text of its reply before them
/// and after them and its last call failed, ended into one summary row at the
/// end of its run.
///
/// The failed call is the last of the run: it settles as it arrives, and one
/// settled call before the text of its run would leave the calls after it in the
/// live area, where the run's end no longer folds them.
fn summary_runs() -> Vec<u8> {
    let mut lines = Vec::new();
    for run in 0..RUNS {
        lines.extend(event(
            "text_delta",
            &format!("Run {run} of the summary cases.\n\n"),
        ));
        for call in 0..CALLS {
            lines.extend(summary_call(run, call, call == CALLS - 1));
        }
        lines.extend(event("text_delta", &format!("Run {run} ended.\n\n")));
        lines.extend(turn_end());
        lines.extend(run_end_only());
    }
    lines
}

/// The blocks one more settled call of a run and its frames cost, in a run that
/// already holds `calls` of them and does not end: a run that still goes draws
/// its calls as its own rows, so every call before the measured one is in the
/// block it redraws.
async fn settled_call_blocks(calls: usize) -> u64 {
    let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
    harness.frame().await;
    for call in 0..calls {
        harness.write(&summary_call(0, call, true)).await;
    }
    harness.settle().await;
    // The call and its frame twice outside the profiler, so a path that stopped
    // drawing fails here and not on the wait inside the measurement, and so the
    // ring has taken all the blocks it grows for: the call measured is one whose
    // room it already has.
    for warm in 0..2 {
        harness.write(&summary_call(0, calls + warm, true)).await;
        harness.next_frame().await;
        harness.settle().await;
    }
    harness.reset();

    let measured = blocks!(async {
        harness.write(&summary_call(0, calls + 1, true)).await;
        harness.next_frame().await;
        // Under load the app can read the call's lines in two parts and draw
        // twice; the second frame must not land in the next sample (88 against 44).
        harness.settle().await;
    });
    eprintln!("blocks: {measured} for one more settled call of a run of {calls}");
    drop(harness.out);
    measured
}

/// The blocks one more call with output and the frame it draws cost, in a run
/// that already holds `calls` of them and does not end: the app reads the output
/// of the committed result from the journal and fills the call's row with it.
async fn output_call_blocks(calls: usize) -> u64 {
    // The journal is the same size for both runs, so the read of the measured
    // call's record costs the same in both: the store walks the part from its
    // start, so a longer journal would make the read itself the difference.
    let mut harness = Harness::journaled((SIDEBAR, ROWS), LONG_RUN + CARD_WARM + SAMPLES + 1).await;
    harness.frame().await;
    for call in 0..calls {
        harness.write(&output_call(call)).await;
    }
    harness.settle().await;
    // The call and its frame twice outside the profiler, so a path that stopped
    // drawing fails here and not on the wait inside the measurement, and so the
    // ring has taken all the blocks its rows and its bytes grow for: a card's
    // body and the count row under it make the ring taller than a plain row did.
    for warm in 0..CARD_WARM {
        harness.write(&output_call(calls + warm)).await;
        harness.next_frame().await;
        harness.settle().await;
    }
    harness.reset();

    // Every sample takes the next call of the run, whose record the app has not
    // read yet: a call already on screen reads no journal.
    let mut next = calls + CARD_WARM;
    let measured = blocks!(async {
        harness.write(&output_call(next)).await;
        next += 1;
        harness.next_frame().await;
        // The read of the record lands after the first frame or before it, so a
        // sample that stops here can leave out its blocks (136 against 163).
        harness.settle().await;
    });
    eprintln!("blocks: {measured} for one more call with output of a run of {calls}");
    drop(harness.out);
    measured
}

/// The approval request the worker sends for a call that waits for a person: the
/// card the styled look draws over the transcript.
fn approval(id: u64, call: &str) -> Vec<u8> {
    notice(
        "approval_request",
        json!({
            "id": id,
            "tool": "Bash",
            "call": call,
            "rule": "default",
            "save": ["Bash(cargo test:*)"],
        }),
    )
}

/// The bytes a terminal sends: 4,000 one-line scroll keys, 4,000 typed
/// characters and 1,000 mouse moves, which is more than the input buffer of one
/// read and more than a pty holds.
fn terminal_input() -> Vec<u8> {
    let mut input = Vec::with_capacity(64 * 1024);
    for _ in 0..4_000 {
        input.extend_from_slice(b"\x1b[1;2A");
    }
    input.resize(input.len() + 4_000, b'a');
    for _ in 0..1_000 {
        input.extend_from_slice(b"\x1b[<35;20;10M");
    }
    input
}

/// Feeds `input` to `parser` in [`READS`] byte reads and answers how many events
/// came out of it.
///
/// Nothing here allocates: every read is copied into the parser's own room for
/// it, which is the size it needs once the warm-up has run, and an event is
/// handed out of it by value.
fn parse(parser: &mut Parser, input: &[u8]) -> usize {
    let mut events = 0;
    let mut rest = input;
    while !rest.is_empty() {
        let (piece, tail) = rest.split_at(rest.len().min(READS));
        rest = tail;
        let room = parser.tail();
        room[..piece.len()].copy_from_slice(piece);
        // Every read but the last one has the bytes behind it that still come,
        // as a read of a full buffer does.
        parser.commit(piece.len(), !rest.is_empty());
        while parser.next_event().is_some() {
            events += 1;
        }
    }
    events
}

/// The parser runs per key and per mouse move, so it works on the bytes of the
/// read that brought them: 4,000 scroll keys, 4,000 typed characters and 1,000
/// mouse moves, a 52 KB burst fed in 4,096 byte reads, allocate no block at all.
///
/// The one allocation input makes is the `String` of an `Event::Paste`, which
/// its type requires, once per paste; no paste is in this feed.
#[test]
fn perf_input_parsing_allocates_nothing() {
    let _serial = serial();
    let input = terminal_input();
    let mut parser = Parser::new();
    // Outside the profiler, so a parser that reads this input as anything but
    // one event per key fails here and not on a count inside the measurement.
    assert_eq!(
        parse(&mut parser, &input),
        INPUT_EVENTS,
        "one event per key and per move"
    );

    // `blocks!` awaits its body: the parse itself is not async, and the
    // runtime is built outside the profiler.
    let measured = runtime().block_on(async { blocks!(async { parse(&mut parser, &input) }) });
    eprintln!(
        "blocks: {measured} for {INPUT_EVENTS} keys and moves in {} bytes of reads of {READS}",
        input.len()
    );
    assert_eq!(
        measured, INPUT_BLOCKS,
        "{measured} blocks for {INPUT_EVENTS} keys and moves, over {INPUT_BLOCKS}"
    );
}

/// A key typed into the `/` list and the key after it refilter the names it
/// already matched, and the frame follows the rows the query picks: the query
/// buffer, the matcher's hits and marks and the frame bytes all have their
/// capacity after the warm-up, so a key costs no heap block.
#[test]
fn perf_command_list_key_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        harness.press('/');
        harness.frame().await;
        harness.press('m');
        harness.frame().await;
        for _ in 0..KEYS {
            harness.press('o');
            harness.frame().await;
            harness.backspace();
            harness.frame().await;
        }
        harness.press('o');
        harness.next_frame().await;
        harness.backspace();
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('o');
            harness.next_frame().await;
            harness.backspace();
            harness.next_frame().await;
        });
        let counts = harness.counts();
        eprintln!(
            "blocks: {measured}, writes: {} for one key typed into the / list and the key after it",
            counts.writes
        );
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for two keys of the / list and their frames, over {STEADY_BLOCKS}"
        );
        assert_eq!(counts.writes, SAMPLES * 2, "one write per key and sample");
        assert_eq!(counts.starts.len(), SAMPLES * 2, "one frame per key");
        drop(counts);
        drop(harness.out);
    });
}

/// A key typed into the palette's own query and the Backspace after it refilter
/// the commands in no heap block: the palette keeps its list, its query and its
/// matcher between keys.
#[test]
fn perf_palette_key_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        harness.control('x');
        harness.press('/');
        harness.frame().await;
        for _ in 0..KEYS {
            harness.press('m');
            harness.frame().await;
            harness.backspace();
            harness.frame().await;
        }
        harness.press('m');
        harness.next_frame().await;
        harness.backspace();
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('m');
            harness.next_frame().await;
            harness.backspace();
            harness.next_frame().await;
        });
        let counts = harness.counts();
        eprintln!(
            "blocks: {measured}, writes: {} for one key typed into the palette and the key after it",
            counts.writes
        );
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for two keys of the palette and their frames, over {STEADY_BLOCKS}"
        );
        assert_eq!(counts.writes, SAMPLES * 2, "one write per key and sample");
        assert_eq!(counts.starts.len(), SAMPLES * 2, "one frame per key");
        drop(counts);
        drop(harness.out);
    });
}

/// A typed key and its frame while the history search is open, over a history of
/// [`HISTORY`] messages with a match in the newest one, rewrite the query and draw
/// its prompt on the box's first row in no heap block.
#[test]
fn perf_search_frame_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        // The history the search walks: fifty sent messages, every one of them
        // a phrase the query finds.
        for n in 0..HISTORY {
            harness
                .write(&event("user_message", &format!("{QUERY} {n}")))
                .await;
            harness.write(&run_end()).await;
        }
        harness.settle().await;
        harness.control('r');
        harness.frame().await;
        let painted = screen_text(&harness.counts().bytes);
        assert!(
            painted.contains("(reverse-i-search)")
                && painted.contains(&format!("{QUERY} {}", HISTORY - 1)),
            "the search never drew its prompt over the newest message's match: {painted:?}"
        );
        // The warm-up types the whole query and takes it back, so the query and
        // the rows of its match have their capacity.
        for _ in 0..KEYS {
            for c in QUERY.chars() {
                harness.press(c);
                harness.frame().await;
            }
            for _ in QUERY.chars() {
                harness.backspace();
                harness.frame().await;
            }
        }
        harness.reset();

        let measured = blocks!(async {
            for c in QUERY.chars() {
                harness.press(c);
                harness.next_frame().await;
            }
        });
        let counts = harness.counts();
        let keys = QUERY.len() as u64;
        eprintln!(
            "blocks: {measured}, writes: {} for the {keys} keys of the query and their frames",
            counts.writes
        );
        assert_eq!(
            measured,
            keys * SEARCH_BLOCKS,
            "{measured} blocks for the {keys} keys of the query and their frames, over {SEARCH_BLOCKS} a key"
        );
        assert_eq!(
            counts.writes,
            SAMPLES * keys as usize,
            "one write per key and sample"
        );
        assert_eq!(
            counts.starts.len(),
            SAMPLES * keys as usize,
            "one frame per key"
        );
        drop(counts);
        drop(harness.out);
    });
}

/// A key typed into the file list and the Backspace after it refilter the
/// [`FILES`] paths the host answered with, and the frame follows the window of
/// them, in no heap block: the list is built once, when the paths arrive, and
/// every key after that only rewrites its query.
#[test]
fn perf_file_list_key_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let paths: Vec<String> = (0..FILES)
            .map(|at| format!("crates/tui/src/app/list{at:04}.rs"))
            .collect();
        let names: Vec<&str> = paths.iter().map(String::as_str).collect();
        let host = HostHandle::new(Arc::new(FakeHost::new().with_files(names)));
        let mut harness = Harness::with_host((SIDEBAR, ROWS), true, host).await;
        harness.frame().await;
        harness.press('@');
        // The list draws the wait for the paths, then the paths: the second
        // frame is the one the keys after it filter.
        harness.settle().await;
        let painted = screen_text(&harness.counts().bytes);
        assert!(
            painted.contains(&format!("{FILES} files")),
            "the file list never got the {FILES} paths: {painted:?}"
        );
        for _ in 0..KEYS {
            harness.press('a');
            harness.frame().await;
            harness.backspace();
            harness.frame().await;
        }
        harness.press('a');
        harness.next_frame().await;
        harness.backspace();
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('a');
            harness.next_frame().await;
            harness.backspace();
            harness.next_frame().await;
        });
        let counts = harness.counts();
        eprintln!(
            "blocks: {measured}, writes: {} for one key typed into the file list and the key after it",
            counts.writes
        );
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for two keys of the {FILES} path file list and their frames, \
             over {STEADY_BLOCKS}"
        );
        assert_eq!(counts.writes, SAMPLES * 2, "one write per key and sample");
        assert_eq!(counts.starts.len(), SAMPLES * 2, "one frame per key");
        drop(counts);
        drop(harness.out);
    });
}

/// A key typed into the key help's filter and the Backspace after it refilter
/// the rows the sheet built when it opened, and the frame follows them, in no
/// heap block: the rows, the filter, the matcher and the shown indices keep
/// their buffers between keys.
#[test]
fn perf_sheet_filter_key_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        harness.press('?');
        // The frame that switches to the alternate screen, then the one the
        // sheet is drawn in.
        harness.frame().await;
        harness.frame().await;
        let painted = screen_text(&harness.counts().bytes);
        assert!(
            painted.contains("type to filter"),
            "the key help is not on screen: {painted:?}"
        );
        for _ in 0..KEYS {
            harness.press('c');
            harness.frame().await;
            harness.backspace();
            harness.frame().await;
        }
        harness.press('c');
        harness.next_frame().await;
        harness.backspace();
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('c');
            harness.next_frame().await;
            harness.backspace();
            harness.next_frame().await;
        });
        let counts = harness.counts();
        eprintln!(
            "blocks: {measured}, writes: {} for one key typed into the key help filter and the key after it",
            counts.writes
        );
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for two keys of the key help filter and their frames, \
             over {STEADY_BLOCKS}"
        );
        assert_eq!(counts.writes, SAMPLES * 2, "one write per key and sample");
        assert_eq!(counts.starts.len(), SAMPLES * 2, "one frame per key");
        drop(counts);
        drop(harness.out);
    });
}

/// Ctrl+X draws the which-key list and the Esc after it takes it away, in no
/// heap block: the list is built once with the app, and its frame reads the key
/// map it holds.
#[test]
fn perf_which_key_frame_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::styled((SIDEBAR, ROWS), true).await;
        harness.frame().await;
        for _ in 0..KEYS {
            harness.control('x');
            harness.frame().await;
            harness.escape();
            harness.frame().await;
        }
        harness.control('x');
        harness.next_frame().await;
        harness.escape();
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.control('x');
            harness.next_frame().await;
            harness.escape();
            harness.next_frame().await;
        });
        let counts = harness.counts();
        eprintln!(
            "blocks: {measured}, writes: {} for the which-key frame of ctrl+x and the esc after it",
            counts.writes
        );
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for the which-key frame and the one that closes it, \
             over {STEADY_BLOCKS}"
        );
        assert_eq!(counts.writes, SAMPLES * 2, "one write per key and sample");
        assert_eq!(counts.starts.len(), SAMPLES * 2, "one frame per key");
        let frames = counts.frames();
        let frame = frames.first().expect("the first sample's which-key frame");
        let shown = screen_text(frame);
        assert!(
            shown.contains("ctrl+x") && shown.contains("esc cancels"),
            "the which-key list is not in the frame ctrl+x draws: {shown:?}"
        );
        drop(counts);
        drop(harness.out);
    });
}

/// A full-screen frame with one picture registered above the viewport allocates
/// nothing: the ring draws the half-block cells it holds, and no image goes out
/// for a picture the frame does not show whole.
#[test]
fn perf_picture_out_of_view_allocates_nothing() {
    let _serial = serial();
    runtime().block_on(async {
        let mut harness = Harness::pictured((SIDEBAR, ROWS)).await;
        harness.frame().await;
        harness.settle().await;
        // The same key and frame once outside the profiler.
        harness.press('b');
        harness.next_frame().await;
        harness.reset();

        let measured = blocks!(async {
            harness.press('b');
            harness.next_frame().await;
        });
        eprintln!("blocks: {measured} for one frame with a picture out of view");
        assert_eq!(
            measured, STEADY_BLOCKS,
            "{measured} blocks for a frame with a picture out of view"
        );
        drop(harness.out);
    });
}
