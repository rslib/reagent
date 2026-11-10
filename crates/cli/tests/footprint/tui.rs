//! The TUI's speed and memory targets, measured on the real binary.
//!
//! Every test drives `reagent` in a pseudo terminal against a worker it starts
//! for a scripted fake provider (the `testing` feature, so no real API), and
//! watches the terminal the way a person does: the pty's bytes go through a
//! `vt100` parser in a reader thread, which also counts the frames (one DEC
//! 2026 start each), the steps among them (the frames whose characters are all on
//! the status line, which is what a run draws on its own clock), and stamps the
//! moment a watched text showed up.
//!
//! The targets are the TUI grill's round 3, Q14: start to first frame under
//! 50 ms, an idle client under 15 MB of RSS, key to screen under 16 ms, no
//! writes and no CPU while idle, and full-speed streaming under 10% of one
//! core. The numbers are the TUI process's own: the worker is its child and is
//! never in them (its sockets and files live in their own temp folders).
//!
//! The release profile is the one that holds these budgets, so the file runs
//! with `--release`. Reads wait on generous timeouts, so a slow machine fails
//! with the number it missed instead of hanging.
//!
//! The default mode is full screen, so the cases written for inline pass
//! `--inline` and the full-screen cases run the default, on a terminal wide
//! enough for the sidebar to dock, against the same budgets: full screen costs
//! what inline costs, a scroll frame draws the rows the viewport shows, and an
//! idle full-screen client writes nothing.
#![cfg(all(feature = "testing", feature = "tui"))]

use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};
use tokio::process::{Child, Command};

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// How long a test waits for text, a frame or an exit before it fails.
const WAIT: Duration = Duration::from_secs(60);

/// The terminal the binary gets: 100 columns by 30 rows, as in `tests/e2e/tui.rs`.
const COLUMNS: u16 = 100;
const ROWS: u16 = 30;

/// The terminal the full-screen cases run on: 160 columns dock the sidebar on
/// the right, as a wide terminal does, and 30 rows is the same terminal.
const WIDE: u16 = 160;

/// How many rows of scrollback the test's parser keeps.
const SCROLLBACK: usize = 1000;

/// The bytes that start a frame: DEC 2026 synchronized output.
const SYNC_START: &[u8] = b"\x1b[?2026h";

/// The bytes that end a frame: DEC 2026 synchronized output off.
const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Start to first frame, spawn and handshake included, in a release build:
/// 5 to 20 ms measured.
const START_BUDGET: Duration = Duration::from_millis(50);

/// A debug build starts slower (23 to 65 ms measured on macOS); the release
/// budget is the target.
const DEBUG_START_BUDGET: Duration = Duration::from_millis(250);

/// The idle client's resident set size in a release build: 7.0 MiB measured
/// on Linux and 8.2 MiB on macOS.
const RSS_BUDGET: u64 = 15 * 1024 * 1024;

/// A debug binary maps far more code: 19.8 MiB idle measured on Linux, so the
/// debug budget is looser; the release budget is the target.
const DEBUG_RSS_BUDGET: u64 = 32 * 1024 * 1024;

/// A key's byte to the screen.
const KEY_BUDGET: Duration = Duration::from_millis(16);

/// How many keys the latency test times, and takes the median of.
const SAMPLES: usize = 20;

/// The lines the scroll test keeps in the transcript ring: a page and a long
/// transcript, neither of them near the ring's cap.
const SHORT_LINES: usize = 500;
const LONG_LINES: usize = 5_000;

/// How many pairs of one-line scrolls the CPU test sends, and how many it sends
/// first to warm the viewport: an up and a down each, so every key moves the
/// viewport and neither transcript runs off its top. `ps` reads a live
/// process's clock in 10 ms steps, so the count is high enough for one step to
/// be a tenth of the measurement.
const SCROLL_PAIRS: usize = 1_000;
const WARM_SCROLLS: usize = 8;

/// How many windows of scrolls each client's CPU test measures, taking the
/// smallest: a process's own clock carries the scheduler's gifts and the page
/// faults of its first seconds, and the smallest window is the work's own cost.
/// A window of [`SCROLL_PAIRS`] pairs measured 30 to 50 ms.
const SCROLL_ROUNDS: usize = 3;

/// The key typed after each window of scrolls, so the test knows the app took
/// every key of it: its frame can only be drawn after them.
const MARKS: [&str; SCROLL_ROUNDS] = ["!", "@", "#"];

/// The CPU a one-line scroll on the long transcript may add over the same scroll
/// on the short one, on top of the clock's own step: the frame draws the rows the
/// viewport shows, never the ring. The two measured 30 ms each, so this is the
/// noise of a busy machine.
const SCROLL_CPU_SLACK: f64 = 0.25;

/// A scroll of one line up and one line down, as a terminal sends them.
const SHIFT_UP: &str = "\u{1b}[1;2A";
const SHIFT_DOWN: &str = "\u{1b}[1;2B";

/// How long the app is watched for movement before it counts as quiet: longer
/// than the frame clock, so a pending frame has landed.
const QUIET: Duration = Duration::from_millis(100);

/// How long the full-screen cases wait before they start a client of their own.
/// `cargo nextest` starts every test of this file at once, and the cases that
/// hold a clock budget measure their start in the first hundred milliseconds: a
/// client this file starts on top of that is a start they did not pay for.
const SETTLE: Duration = Duration::from_millis(300);

/// The start and idle memory a full-screen run may add over the inline run it is
/// compared with, whose numbers the same test measures: the smallest of three
/// starts was 14 to 18 ms for full screen and 14 to 26 ms for inline, and the
/// idle set 8.6 against 8.4 MiB, so the margin is the spread of a start, not room
/// full screen is allowed to spend.
const START_MARGIN: Duration = Duration::from_millis(20);
const RSS_MARGIN: u64 = 2 * 1024 * 1024;

/// The two queries the app's first frame writes, and the answers a real terminal
/// gives them: the device attributes, which end the first frame's wait, and the
/// background, which picks the look's mode.
const DEVICE_QUERY: &[u8] = b"\x1b[c";
const DEVICE_ATTRIBUTES: &[u8] = b"\x1b[?62;22c";
const BACKGROUND_QUERY: &[u8] = b"\x1b]11;?\x07";
const DARK_BACKGROUND: &[u8] = b"\x1b]11;rgb:1414/1414/1414\x07";

/// The bytes of the last read kept, less one, so a query split over two reads is
/// still answered once.
const QUERY_TAIL: usize = 7;

/// How long the busy case watches a run whose tool does not come back, and the CPU
/// its bar may use while it steps on its clock: a step draws one row's changed
/// cells, so ten seconds of them are a few frames' work.
const BUSY: Duration = Duration::from_secs(10);
const BUSY_CPU_BUDGET: Duration = Duration::from_millis(100);

/// How long the still case watches a busy run with `NO_MOTION=1`: the bar holds
/// its frame, so at most one frame a second draws the elapsed time.
const STILL: Duration = Duration::from_secs(2);

/// How many clients each mode starts, whose smallest start is the one reported:
/// a start is 12 to 60 ms from run to run on a busy machine, so one sample says
/// nothing. Two missed the 50 ms budget together under load; the rounds run one
/// after another, so five adds no clients running at the same time.
const STARTS: usize = 5;

/// The CPU time an idle client may use over [`IDLE`]. `ps` reads a live
/// process's clock in 10 ms steps, so a report of 0 is under 10 ms; the budget
/// is the smallest step the clock can show.
const IDLE_CPU_BUDGET: Duration = Duration::from_millis(10);

/// How long the idle client is watched.
const IDLE: Duration = Duration::from_secs(2);

/// The deltas of the streaming test's reply, and the frame clock the client
/// coalesces them at.
const DELTAS: usize = 10_000;
const FRAME_CLOCK: Duration = Duration::from_millis(16);

/// The share of one core a stream may use, at the delta rate below.
const STREAM_CPU_SHARE: f64 = 0.10;

/// The delta rate "full speed" means: faster than a model streams. The fake
/// provider sends every delta at once, so a stream lasts a few ms and its
/// share of a core is near 100% by construction; the check is the CPU per
/// delta at this rate instead.
const FULL_SPEED_DELTAS_PER_SECOND: f64 = 1_000.0;

/// The text of the last delta, so the test knows the reply has arrived.
const LAST_DELTA: &str = "ENDOFREPLY";

/// The folders one test runs in.
struct Home {
    /// The config home and the project folder.
    root: tempfile::TempDir,
    /// The state folder, short enough for a worker's socket path.
    state: tempfile::TempDir,
}

impl Home {
    #[expect(
        clippy::disallowed_methods,
        reason = "a test makes the folders it runs in"
    )]
    fn new() -> Home {
        let root = tempfile::tempdir().expect("a temp folder");
        let state = tempfile::Builder::new()
            .prefix("tf")
            .tempdir_in("/tmp")
            .expect("a short state folder");
        // A `.git` marks the project folder, so its config file is read.
        std::fs::create_dir_all(root.path().join("work/.git")).expect("the project folder");
        Home { root, state }
    }

    fn cwd(&self) -> PathBuf {
        self.root.path().join("work")
    }

    /// Writes a provider script and returns the `--provider` value playing it.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test writes its own provider script"
    )]
    fn script(&self, name: &str, body: &str) -> String {
        let path = self.root.path().join(format!("{name}.json"));
        std::fs::write(&path, body).expect("the script");
        format!("fake:{}", path.display())
    }

    /// The binary, set up the way this home runs it.
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.state.path())
            .env("REAGENT_MODELS_DEV_URL", "http://127.0.0.1:1")
            // The terminal the cases draw on says it takes true color and has
            // motion, whatever the shell this suite runs in says.
            .env("COLORTERM", "truecolor")
            .env_remove("NO_COLOR")
            .env_remove("NO_MOTION")
            .env_remove("REAGENT_PROFILE")
            .current_dir(self.cwd());
        command
    }
}

/// The answers the test's terminal writes back, the moment it reads each query,
/// the way the terminal in front of a person does.
struct Answers {
    /// Whether the background query is still answered: the styled cases answer it
    /// with a dark background, which puts the look in rich mode.
    background: bool,
    /// Whether the device attributes query is still answered.
    attributes: bool,
    /// The last bytes of the previous read, so a query split over two reads is
    /// answered once.
    tail: Vec<u8>,
}

impl Answers {
    /// The answers to give for a terminal that says whether its background is dark.
    fn new(dark: bool) -> Answers {
        Answers {
            background: dark,
            attributes: true,
            tail: Vec::with_capacity(QUERY_TAIL),
        }
    }

    /// The bytes to write back for `chunk`, given the reads before it.
    fn read(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut joined = std::mem::take(&mut self.tail);
        joined.extend_from_slice(chunk);
        let asked = |query: &[u8]| joined.windows(query.len()).any(|window| window == query);
        let mut answer = Vec::new();
        if self.background && asked(BACKGROUND_QUERY) {
            self.background = false;
            answer.extend_from_slice(DARK_BACKGROUND);
        }
        if self.attributes && asked(DEVICE_QUERY) {
            self.attributes = false;
            answer.extend_from_slice(DEVICE_ATTRIBUTES);
        }
        let keep = joined.len().saturating_sub(QUERY_TAIL);
        self.tail = joined.split_off(keep);
        answer
    }
}

/// The terminal's state, filled by the reader thread.
struct Shared {
    parser: vt100::Parser,
    /// Every byte the terminal gave the test.
    bytes: usize,
    /// Every frame the app wrote, and the steps among them.
    frames: Frames,
    /// The text a test waits for, and when it showed.
    watch: Option<Watch>,
}

/// The text a test waits for.
struct Watch {
    needle: String,
    seen: Option<Instant>,
}

/// A UI drawn in a pseudo terminal, with the screen it drew.
#[expect(
    clippy::disallowed_types,
    reason = "a test drives a pty through the standard library"
)]
struct Tui {
    /// The binary's process.
    child: Child,
    /// Its process id, for the memory and CPU readings.
    pid: u32,
    /// The master side, where the test writes keys.
    master: std::fs::File,
    /// A handle on the terminal the binary drew on, held for the test's life.
    _slave: std::fs::File,
    /// The screen the reader thread fills.
    shared: Arc<Mutex<Shared>>,
    /// When the test started the binary.
    started: Instant,
    /// The terminal's width, so a screen read asks for its own row length.
    columns: u16,
    /// The row the status line is on, found once from the screen.
    status_row: Option<u16>,
}

impl Tui {
    /// Starts the binary in a terminal, with `args` and this home.
    fn start(home: &Home, args: &[&str]) -> Tui {
        Tui::sized(home, args, COLUMNS, ROWS)
    }

    /// Starts the binary in the styled look's terminal: one that answers the
    /// background query with a dark background, so every role is a fill.
    fn styled(home: &Home, args: &[&str], columns: u16, rows: u16) -> Tui {
        Tui::with(home, args, columns, rows, true, false)
    }

    /// Starts the binary on a styled terminal that holds the bar still, as
    /// `NO_MOTION=1` asks.
    fn still(home: &Home, args: &[&str], columns: u16, rows: u16) -> Tui {
        Tui::with(home, args, columns, rows, true, true)
    }

    /// Starts the binary in a terminal of `columns` by `rows`, with `args` and
    /// this home.
    fn sized(home: &Home, args: &[&str], columns: u16, rows: u16) -> Tui {
        Tui::with(home, args, columns, rows, false, false)
    }

    /// Starts the binary in a terminal that answers `dark`'s background query and
    /// runs with `NO_MOTION` set when `still`, of `columns` by `rows`.
    #[expect(
        clippy::disallowed_types,
        reason = "a test drives a pty through the standard library"
    )]
    fn with(home: &Home, args: &[&str], columns: u16, rows: u16, dark: bool, still: bool) -> Tui {
        let size = Winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&size), None).expect("a pseudo terminal");
        let master = std::fs::File::from(pty.master);
        let slave = std::fs::File::from(pty.slave);
        let mut command = home.command();
        if still {
            command.env("NO_MOTION", "1");
        }
        let started = Instant::now();
        let child = command
            .args(args)
            .stdin(Stdio::from(slave.try_clone().expect("a terminal handle")))
            .stdout(Stdio::from(slave.try_clone().expect("a terminal handle")))
            .stderr(Stdio::from(slave.try_clone().expect("a terminal handle")))
            .kill_on_drop(true)
            .spawn()
            .expect("the binary starts");
        let pid = child.id().expect("the binary has a process id");
        let parser = vt100::Parser::new(
            NonZeroU16::new(rows).expect("rows"),
            NonZeroU16::new(columns).expect("columns"),
            SCROLLBACK,
        );
        let shared = Arc::new(Mutex::new(Shared {
            parser,
            bytes: 0,
            frames: Frames::new(),
            watch: None,
        }));
        let mut output = master.try_clone().expect("a terminal handle");
        let mut answers_out = master.try_clone().expect("a terminal handle");
        let read = Arc::clone(&shared);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            let mut answers = Answers::new(dark);
            loop {
                let read_now = match output.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                let answer = answers.read(&buffer[..read_now]);
                if !answer.is_empty() {
                    let _ = answers_out.write_all(&answer);
                    let _ = answers_out.flush();
                }
                let mut state = read.lock().unwrap_or_else(PoisonError::into_inner);
                state.bytes += read_now;
                state.frames.push(&buffer[..read_now]);
                state.parser.process(&buffer[..read_now]);
                // The screens are read only while a test waits for one: the
                // formatting of 3,000 cells per chunk is the reader thread's
                // whole cost otherwise.
                let watching = state
                    .watch
                    .as_ref()
                    .is_some_and(|watch| watch.seen.is_none());
                if watching {
                    let text = state.parser.screen().contents();
                    if let Some(watch) = state.watch.as_mut()
                        && watch.seen.is_none()
                        && text.contains(&watch.needle)
                    {
                        watch.seen = Some(Instant::now());
                    }
                }
            }
        });
        Tui {
            child,
            pid,
            master,
            _slave: slave,
            shared,
            started,
            columns,
            status_row: None,
        }
    }

    /// Sends keys to the terminal.
    fn keys(&mut self, keys: &str) {
        self.master
            .write_all(keys.as_bytes())
            .expect("the keys reach the terminal");
        self.master.flush().expect("the keys are sent");
    }

    /// The bytes the terminal gave the test so far.
    fn bytes(&self) -> usize {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bytes
    }

    /// The frames the app wrote so far.
    fn frames(&self) -> usize {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .frames
            .count
    }

    /// The frames the app wrote so far that only stepped its clock: the bar's
    /// step, or the elapsed time's move under `NO_MOTION`.
    fn steps(&self) -> usize {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .frames
            .steps
    }

    /// Everything the terminal holds, its scrollback included.
    fn text(&self) -> String {
        let mut state = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        text_of(&mut state.parser)
    }

    /// Waits until the terminal shows `text` anywhere, and fails with the
    /// screen when it never does.
    async fn until(&self, text: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.text().contains(text) {
            assert!(
                Instant::now() < deadline,
                "the terminal never showed {text:?}:\n{}",
                self.text()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Arms the reader thread to stamp the moment `needle` shows.
    fn arm(&self, needle: &str) {
        let mut state = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        state.watch = Some(Watch {
            needle: needle.to_owned(),
            seen: None,
        });
    }

    /// Waits for the armed text to show and answers when it did.
    ///
    /// The reader thread stamps it, so the time is the byte's, not the test's
    /// polling interval.
    async fn watched(&self) -> Instant {
        let deadline = Instant::now() + WAIT;
        loop {
            let seen = {
                let state = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
                state.watch.as_ref().and_then(|watch| watch.seen)
            };
            if let Some(seen) = seen {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "the terminal never showed the armed text:\n{}",
                self.text()
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Waits for `needle` to show and answers when it did.
    async fn stamp(&self, needle: &str) -> Instant {
        self.arm(needle);
        self.watched().await
    }

    /// Waits for the status line to say `want` and answers when it did.
    ///
    /// The poll reads that one row, not the whole screen: formatting 100 cells
    /// per millisecond instead of 3,000 keeps the test from taking the reader
    /// thread's lock often enough to slow the app's frames down.
    async fn status(&mut self, want: &str) -> Instant {
        let row = match self.status_row {
            Some(row) => row,
            None => {
                let found = self.find_status_row();
                self.status_row = Some(found);
                found
            }
        };
        let started = Instant::now();
        let deadline = started + WAIT;
        loop {
            if self.status_text(row).contains(want) {
                return Instant::now();
            }
            // A binary that left cannot say anything: fail now, with what it
            // had measured and the screen it left, rather than wait out the
            // timeout.
            if let Some(code) = self.exited() {
                panic!(
                    "the binary exited {code} after {} bytes and {} frames, \
                     {:?} into the wait for {want:?}:\n{}",
                    self.bytes(),
                    self.frames(),
                    started.elapsed(),
                    self.text()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the status never said {want:?}:\n{}",
                self.text()
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// The row of the visible screen that holds the status line, which says
    /// one of the words the run uses.
    fn find_status_row(&self) -> u16 {
        let mut state = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        let rows: Vec<String> = state.parser.screen().rows(0, self.columns).collect();
        let found = rows
            .iter()
            .position(|row| row.contains("idle") || row.contains("working"));
        let row = found.unwrap_or_else(|| panic!("no status line in {rows:?}"));
        let row = u16::try_from(row).unwrap_or(0);
        // The reader thread sorts the frames by the row their characters land on,
        // and the app's frames count rows from 1. The two rows agree in full
        // screen, the mode these cases drive.
        state.frames.row = row.saturating_add(1);
        row
    }

    /// The text of row `row`.
    fn status_text(&self, row: u16) -> String {
        let state = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .parser
            .screen()
            .contents_between(row, 0, row, self.columns)
    }

    /// The binary's exit code, when it has ended.
    fn exited(&mut self) -> Option<i32> {
        let status = self.child.try_wait().expect("the binary's state")?;
        Some(status.code().unwrap_or(-1))
    }

    /// Waits for the binary to exit and returns its code.
    async fn wait(&mut self) -> i32 {
        let status = tokio::time::timeout(WAIT, self.child.wait())
            .await
            .expect("an exit in time")
            .expect("the binary's exit");
        status.code().unwrap_or(-1)
    }

    /// Waits until the app has drawn everything it has: no byte, frame or
    /// reading of the process moves for [`QUIET`].
    async fn quiet(&mut self) {
        let (mut bytes, mut frames) = (self.bytes(), self.frames());
        loop {
            tokio::time::sleep(QUIET).await;
            let (now_bytes, now_frames) = (self.bytes(), self.frames());
            if now_bytes == bytes && now_frames == frames {
                return;
            }
            (bytes, frames) = (now_bytes, now_frames);
        }
    }

    /// Scrolls a few lines up and down, so the viewport and the ring's path are
    /// warm before a window is timed.
    async fn scroll_warm(&mut self) {
        let pair = format!("{SHIFT_UP}{SHIFT_DOWN}");
        self.keys(&pair.repeat(WARM_SCROLLS));
        self.quiet().await;
    }

    /// The CPU time [`SCROLL_PAIRS`] pairs of one-line scrolls cost `name`'s
    /// transcript: the keys go out, then the key `mark`, whose frame can only be
    /// drawn after all of them.
    async fn scroll_window(&mut self, name: &str, mark: &str) -> Duration {
        let pair = format!("{SHIFT_UP}{SHIFT_DOWN}");
        let before = usage(self.pid).await.1;
        self.keys(&pair.repeat(SCROLL_PAIRS));
        self.keys(mark);
        self.until(mark).await;
        self.quiet().await;
        let window = usage(self.pid).await.1.saturating_sub(before);
        eprintln!("  {name} scroll window {mark}: {window:?}");
        window
    }

    /// Leaves the UI the way a user does, and returns its exit code.
    async fn quit(&mut self) -> i32 {
        self.until("idle").await;
        self.keys("\u{3}");
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.keys("\u{3}");
        self.wait().await
    }
}

/// The frames the app wrote, and how many of them only stepped its clock.
///
/// A frame writes every cell behind an absolute cursor move, so a frame that
/// stepped the bar or the elapsed time writes its characters on the status line
/// and leaves the terminal's cursor where the text box's edit point is without
/// writing a cell there. That is the frame a run draws on its own, between the
/// frames an event draws.
struct Frames {
    /// The bytes of the frame the last read left open, from its start.
    open: Vec<u8>,
    /// Every frame the app wrote: one DEC 2026 start each.
    count: usize,
    /// The frames whose characters were all on the status line.
    steps: usize,
    /// The status line's row in the app's own numbers, where the first row is 1;
    /// 0 until the test found it, so no frame before that counts as a step.
    row: u16,
}

impl Frames {
    fn new() -> Frames {
        Frames {
            open: Vec::new(),
            count: 0,
            steps: 0,
            row: 0,
        }
    }

    /// Adds the bytes of one read, counting the frames it completes: a marker
    /// that spans two reads is still one frame's, since the bytes of an open
    /// frame stay here until its end comes.
    fn push(&mut self, chunk: &[u8]) {
        self.open.extend_from_slice(chunk);
        let mut from = 0;
        while let Some(start) = find(&self.open[from..], SYNC_START) {
            let body = from + start + SYNC_START.len();
            let Some(end) = find(&self.open[body..], SYNC_END) else {
                break;
            };
            let stop = body + end + SYNC_END.len();
            self.count += 1;
            if self.row != 0 && only_row(&self.open[from + start..stop], self.row) {
                self.steps += 1;
            }
            from = stop;
        }
        self.open.drain(..from);
    }
}

/// Where `needle` starts in `hay`, if it is there.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|window| window == needle)
}

/// Whether every character `frame` writes lands on row `row`, counted from 1 as
/// the app's frames count: the frame changed nothing else on screen.
fn only_row(frame: &[u8], row: u16) -> bool {
    let mut at = 0;
    // The row the cursor is on; 0 until the frame moves it.
    let mut cursor = 0;
    while let Some(&byte) = frame.get(at) {
        if byte != 0x1b {
            // A character cell, or one of its bytes, on the cursor's row.
            if byte >= 0x20 && cursor != row {
                return false;
            }
            at += 1;
            continue;
        }
        // An escape sequence: a CSI is parameter bytes and one final byte, and
        // only a `H` moves the cursor to another row.
        let Some(&kind) = frame.get(at + 1) else {
            return false;
        };
        if kind != b'[' {
            at += 2;
            continue;
        }
        let mut end = at + 2;
        while frame
            .get(end)
            .is_some_and(|byte| (0x20..=0x3f).contains(byte))
        {
            end += 1;
        }
        if end >= frame.len() {
            return false;
        }
        if frame[end] == b'H' {
            cursor = frame[at + 2..end]
                .split(|byte| *byte == b';')
                .next()
                .and_then(|row| std::str::from_utf8(row).ok())
                .and_then(|row| row.parse().ok())
                .unwrap_or(1);
        }
        at = end + 1;
    }
    true
}

/// Everything a parsed terminal holds, its scrollback included.
fn text_of(parser: &mut vt100::Parser) -> String {
    let at = parser.screen().scrollback();
    let mut text = String::new();
    for rows in (0..=parser.screen().scrollback_len()).rev() {
        parser.screen_mut().set_scrollback(rows);
        text.push_str(&parser.screen().contents());
        text.push('\n');
    }
    parser.screen_mut().set_scrollback(at);
    text
}

/// The resident set size in bytes and the CPU time of `pid`.
///
/// `ps` reads a live process; on both targets its clock is 10 ms, so a CPU
/// time here is exact to that.
async fn usage(pid: u32) -> (u64, Duration) {
    let pid = pid.to_string();
    let output = Command::new("ps")
        .args(["-o", "rss=,time=", "-p", &pid])
        .output()
        .await
        .expect("ps runs");
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.trim();
    let mut fields = line.split_whitespace();
    let rss = fields
        .next()
        .and_then(|field| field.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("no rss in ps output {line:?}"));
    let time = fields
        .next()
        .unwrap_or_else(|| panic!("no time in ps output {line:?}"));
    // macOS reports KiB, and says so on Linux too.
    (rss * 1024, parse_time(time))
}

/// A `ps` time: `[DD-][HH:]MM:SS[.cc]`.
fn parse_time(text: &str) -> Duration {
    let (days, rest) = match text.split_once('-') {
        Some((days, rest)) => (days.parse::<u64>().unwrap_or(0), rest),
        None => (0, text),
    };
    let mut seconds = 0.0f64;
    for part in rest.split(':') {
        let value = part.parse::<f64>().unwrap_or(0.0);
        seconds = seconds * 60.0 + value;
    }
    Duration::from_secs_f64(days as f64 * 86_400.0 + seconds)
}

/// The script of a reply of `deltas` pieces of text.
fn stream_script(deltas: usize) -> String {
    let mut script = String::from(r#"{"replies": [{"deltas": ["#);
    for index in 0..deltas {
        if index > 0 {
            script.push(',');
        }
        script.push_str(r#""w""#);
    }
    script.push_str(&format!(r#","{LAST_DELTA}"]}}]}}"#));
    script
}

/// A spawn reaches its first frame in under 50 ms, and the idle client stays
/// under 15 MB of memory.
#[tokio::test]
async fn start_and_idle_memory_hold_their_budgets() {
    let home = Home::new();
    let provider = home.script("one", r#"{"replies": [{"text": "hello from the fake"}]}"#);
    // The best of a few starts: one start alone carries whatever else the machine
    // did in that moment, such as the page faults of a binary just linked, and
    // the best one is the start's own cost.
    let mut start = Duration::MAX;
    let mut tui = Tui::start(&home, &["--provider", &provider, "--inline"]);
    for round in 0..STARTS {
        let first = tui.stamp("idle").await;
        start = start.min(first.duration_since(tui.started));
        if round + 1 < STARTS {
            assert_eq!(tui.quit().await, 0, "a normal quit");
            tui = Tui::start(&home, &["--provider", &provider, "--inline"]);
        }
    }
    let start_budget = if cfg!(debug_assertions) {
        DEBUG_START_BUDGET
    } else {
        START_BUDGET
    };
    eprintln!("start: {start:?} to the first frame, the best of {STARTS}, budget {start_budget:?}");
    assert!(
        start < start_budget,
        "start {start:?} over {start_budget:?}"
    );

    // Idle means the app has finished its first frame and nothing has happened
    // for a second: the reader thread has taken every byte by then.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (rss, cpu) = usage(tui.pid).await;
    eprintln!(
        "rss: {rss} bytes ({:.1} MiB) when idle, budget {RSS_BUDGET} bytes; cpu {cpu:?}",
        rss as f64 / (1024.0 * 1024.0)
    );
    let budget = if cfg!(debug_assertions) {
        DEBUG_RSS_BUDGET
    } else {
        RSS_BUDGET
    };
    assert!(rss < budget, "idle rss {rss} over {budget}");

    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// A key's byte reaches the screen in under 16 ms: the median of 20 keys.
#[tokio::test]
async fn a_key_reaches_the_screen_within_a_frame() {
    let home = Home::new();
    let provider = home.script("one", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider, "--inline"]);
    tui.until("idle").await;

    // The editor's text is what a key changes on screen. A lead of ten keys
    // gives its buffer, its layout and the interned cursor style their
    // capacity, so the timed keys measure the frame and not a first growth.
    let mut typed = String::from("k");
    tui.keys(&typed);
    tui.until(&typed).await;
    for index in 0..10 {
        let key = char::from(b'a' + index);
        tui.keys(&key.to_string());
        typed.push(key);
        tui.until(&typed).await;
        tokio::time::sleep(FRAME_CLOCK).await;
    }

    let mut samples = Vec::with_capacity(SAMPLES);
    for index in 0..SAMPLES {
        let key = char::from(b'a' + u8::try_from(index).unwrap_or(0));
        // Armed before the byte goes out, so nothing can slip between.
        tui.arm(&format!("{typed}{key}"));
        let at = Instant::now();
        tui.keys(&key.to_string());
        let latency = tui.watched().await.duration_since(at);
        eprintln!("key {index}: {latency:?}");
        samples.push(latency);
        typed.push(key);
        tokio::time::sleep(FRAME_CLOCK).await;
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    eprintln!("key to screen: {median:?} median, budget {KEY_BUDGET:?}");
    assert!(
        median < KEY_BUDGET,
        "key to screen {median:?} over {KEY_BUDGET:?}"
    );
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// Two seconds with no input write nothing to the terminal, draw no frame and
/// use no CPU: no frames and no wakeups.
///
/// `ps` reads a live process's CPU at a 10 ms clock, the finest either target
/// offers for a running process (`rusage` reports a child only once it exits,
/// and then with the worker's own time folded in), so the number here is that
/// clock's: 0 means under 10 ms over the two seconds. A wakeup loop at the
/// frame clock would show about 2,000 ms, five hundred times the budget.
#[tokio::test]
async fn an_idle_client_writes_nothing_and_burns_no_cpu() {
    let home = Home::new();
    let provider = home.script("one", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider, "--inline"]);
    tui.until("idle").await;
    // Let the first frame and the handshake's last bytes arrive.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (bytes, frames, cpu) = (tui.bytes(), tui.frames(), usage(tui.pid).await.1);
    tokio::time::sleep(IDLE).await;
    let (after_bytes, after_frames, after_cpu) =
        (tui.bytes(), tui.frames(), usage(tui.pid).await.1);
    let grew = after_cpu.saturating_sub(cpu);
    eprintln!(
        "idle: {} bytes, {} frames, cpu {grew:?} over {IDLE:?} (ps' clock is 10 ms), \
         budget under {IDLE_CPU_BUDGET:?}",
        after_bytes - bytes,
        after_frames - frames
    );
    assert_eq!(after_bytes, bytes, "an idle client writes nothing");
    assert_eq!(after_frames, frames, "an idle client draws no frame");
    assert!(
        grew < IDLE_CPU_BUDGET,
        "an idle client used {grew:?} of cpu over {IDLE:?}"
    );
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// A reply of 10,000 deltas stays under 10% of one core, and draws no more than
/// one frame per frame clock.
///
/// The whole reply has to arrive before any of that means anything: a client
/// the worker cuts mid-stream has measured nothing, so a cut fails the test
/// with the numbers it had, and so does a connection that closes.
#[tokio::test]
async fn a_full_speed_stream_stays_under_a_tenth_of_a_core() {
    let home = Home::new();
    let provider = home.script("stream", &stream_script(DELTAS));
    let mut tui = Tui::start(&home, &["--provider", &provider, "--inline"]);
    tui.until("idle").await;
    // Learn the status line's row while it is still idle, so the lookup cannot
    // race the first frame of the run.
    tui.status("idle").await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // The whole reply is measured, from before the key to the frame that drew
    // its last delta: the stream is too short for `ps` to time on its own.
    let start_cpu = usage(tui.pid).await.1;
    let started = Instant::now();
    let start_frames = tui.frames();
    tui.keys("go\r");
    tui.until(LAST_DELTA).await;
    let ended = tui.status("idle").await;
    let wall = ended.duration_since(started);
    let (frames, cpu) = (tui.frames(), usage(tui.pid).await.1);

    let stream_frames = frames - start_frames;
    let stream_cpu = cpu.saturating_sub(start_cpu);
    let per_delta = stream_cpu.as_secs_f64() / DELTAS as f64;
    let share = per_delta * FULL_SPEED_DELTAS_PER_SECOND;
    // One frame per frame clock while the reply streams, plus three outside
    // it: the echo of the key, the frame that opens the run and the frame
    // that draws the end of the run.
    let allowed = wall.as_secs_f64() / FRAME_CLOCK.as_secs_f64() + 3.0;
    eprintln!(
        "stream: {DELTAS} deltas in {wall:?}, cpu {stream_cpu:?} ({:.2} us per delta, \
         {:.2}% of a core at {FULL_SPEED_DELTAS_PER_SECOND} deltas/s, budget {:.0}%), \
         {stream_frames} frames (budget {allowed:.1}), {} bytes out",
        per_delta * 1e6,
        100.0 * share,
        100.0 * STREAM_CPU_SHARE,
        tui.bytes(),
    );
    assert!(
        !tui.text().contains("cut off") && !tui.text().contains("closed the session"),
        "the worker cut the client and it did not come back:\n{}",
        tui.text()
    );
    assert!(
        share < STREAM_CPU_SHARE,
        "the stream used {:.2}% of a core at {FULL_SPEED_DELTAS_PER_SECOND} deltas/s, over {:.0}%",
        100.0 * share,
        100.0 * STREAM_CPU_SHARE
    );
    assert!(
        (stream_frames as f64) <= allowed,
        "{stream_frames} frames over {wall:?} of streaming, over {allowed:.1}"
    );
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// The budget the start and the idle memory of a mode are held to: the release
/// one, or the looser one a debug build needs.
fn start_budget() -> Duration {
    if cfg!(debug_assertions) {
        DEBUG_START_BUDGET
    } else {
        START_BUDGET
    }
}

fn rss_budget() -> u64 {
    if cfg!(debug_assertions) {
        DEBUG_RSS_BUDGET
    } else {
        RSS_BUDGET
    }
}

/// One client's start to the first frame, and its idle resident set when
/// `memory`, after a second of being idle.
async fn start_once(home: &Home, args: &[&str], columns: u16, memory: bool) -> (Duration, u64) {
    let mut tui = Tui::sized(home, args, columns, ROWS);
    let start = tui.stamp("idle").await.duration_since(tui.started);
    let mut rss = 0;
    if memory {
        // Idle means the app has finished its first frame and nothing has
        // happened for a second: the reader thread has taken every byte by then.
        tokio::time::sleep(Duration::from_secs(1)).await;
        rss = usage(tui.pid).await.0;
    }
    assert_eq!(tui.quit().await, 0, "a normal quit");
    (start, rss)
}

/// The default run, full screen, holds the start and idle memory budgets of the
/// inline run: both are measured here, and full screen may add no more than the
/// margin over inline, which draws the alternate screen and a whole terminal
/// before its first frame.
#[tokio::test]
async fn the_full_screen_run_holds_the_inline_start_and_idle_budgets() {
    let home = Home::new();
    let provider = home.script("one", r#"{"replies": [{"text": "hello from the fake"}]}"#);
    tokio::time::sleep(SETTLE).await;

    // The modes alternate, so a busy moment of the machine lands in both, and
    // each mode's start is the smallest of its rounds: a start is 12 to 60 ms
    // from run to run, and the smallest is the mode's own cost.
    let (mut inline, mut full) = (Vec::with_capacity(STARTS), Vec::with_capacity(STARTS));
    let (mut inline_rss, mut full_rss) = (0, 0);
    for round in 0..STARTS {
        let last = round + 1 == STARTS;
        let inline_args = ["--provider", &provider, "--inline"];
        let (start, rss) = start_once(&home, &inline_args, COLUMNS, last).await;
        inline.push(start);
        inline_rss = rss;
        let full_args = ["--provider", &provider];
        let (start, rss) = start_once(&home, &full_args, WIDE, last).await;
        full.push(start);
        full_rss = rss;
    }
    let inline_start = inline.into_iter().min().expect("a start per round");
    let full_start = full.into_iter().min().expect("a start per round");

    let (start_budget, rss_budget) = (start_budget(), rss_budget());
    eprintln!(
        "start: inline {inline_start:?}, full screen {full_start:?}, budget {start_budget:?} \
         (inline plus {START_MARGIN:?})"
    );
    eprintln!(
        "rss: inline {:.1} MiB, full screen {:.1} MiB, budget {:.1} MiB (inline plus {:.1} MiB)",
        inline_rss as f64 / (1024.0 * 1024.0),
        full_rss as f64 / (1024.0 * 1024.0),
        rss_budget as f64 / (1024.0 * 1024.0),
        RSS_MARGIN as f64 / (1024.0 * 1024.0)
    );
    assert!(
        full_start < start_budget,
        "full-screen start {full_start:?} over the {start_budget:?} both modes are held to"
    );
    assert!(
        full_start <= inline_start + START_MARGIN,
        "full-screen start {full_start:?} against inline's {inline_start:?}, over {START_MARGIN:?}"
    );
    assert!(
        full_rss < rss_budget,
        "full-screen idle rss {full_rss} over the {rss_budget} both modes are held to"
    );
    assert!(
        full_rss <= inline_rss + RSS_MARGIN,
        "full-screen idle rss {full_rss} against inline's {inline_rss}, over {RSS_MARGIN}"
    );
}

/// Two seconds of an idle full-screen client with the sidebar on write nothing,
/// draw no frame and use no CPU: the panels read no clock of their own, and the
/// app arms no timer while nothing is dirty.
#[tokio::test]
async fn an_idle_full_screen_client_with_the_sidebar_writes_nothing_and_burns_no_cpu() {
    let home = Home::new();
    let provider = home.script("one", r#"{"replies": [{"text": "unused"}]}"#);
    tokio::time::sleep(SETTLE).await;
    let mut tui = Tui::sized(&home, &["--provider", &provider], WIDE, ROWS);
    tui.until("idle").await;
    // The sidebar is docked on the right, its first panel drawn.
    tui.until("Context").await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (bytes, frames, cpu) = (tui.bytes(), tui.frames(), usage(tui.pid).await.1);
    tokio::time::sleep(IDLE).await;
    let (after_bytes, after_frames, after_cpu) =
        (tui.bytes(), tui.frames(), usage(tui.pid).await.1);
    let grew = after_cpu.saturating_sub(cpu);
    eprintln!(
        "idle full screen: {} bytes, {} frames, cpu {grew:?} over {IDLE:?} (ps' clock is 10 ms), \
         budget under {IDLE_CPU_BUDGET:?}",
        after_bytes - bytes,
        after_frames - frames
    );
    assert_eq!(after_bytes, bytes, "an idle client writes nothing");
    assert_eq!(after_frames, frames, "an idle client draws no frame");
    assert!(
        grew < IDLE_CPU_BUDGET,
        "an idle full-screen client used {grew:?} of cpu over {IDLE:?}"
    );
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// A key's byte reaches the screen in under 16 ms in full screen too, the median
/// of 20 keys, with the sidebar painted in every frame.
#[tokio::test]
async fn a_key_reaches_the_screen_within_a_frame_in_full_screen() {
    let home = Home::new();
    let provider = home.script("one", r#"{"replies": [{"text": "unused"}]}"#);
    tokio::time::sleep(SETTLE).await;
    let mut tui = Tui::sized(&home, &["--provider", &provider], WIDE, ROWS);
    tui.until("idle").await;
    tui.until("Context").await;

    // The editor's text is what a key changes on screen. A lead of ten keys gives
    // its buffer, its layout and the interned cursor style their capacity, so
    // the timed keys measure the frame and not a first growth.
    let mut typed = String::from("k");
    tui.keys(&typed);
    tui.until(&typed).await;
    for index in 0..10 {
        let key = char::from(b'a' + index);
        tui.keys(&key.to_string());
        typed.push(key);
        tui.until(&typed).await;
        tokio::time::sleep(FRAME_CLOCK).await;
    }

    let mut samples = Vec::with_capacity(SAMPLES);
    for index in 0..SAMPLES {
        let key = char::from(b'a' + u8::try_from(index).unwrap_or(0));
        // Armed before the byte goes out, so nothing can slip between.
        tui.arm(&format!("{typed}{key}"));
        let at = Instant::now();
        tui.keys(&key.to_string());
        let latency = tui.watched().await.duration_since(at);
        eprintln!("key {index}: {latency:?}");
        samples.push(latency);
        typed.push(key);
        tokio::time::sleep(FRAME_CLOCK).await;
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    eprintln!("full-screen key to screen: {median:?} median, budget {KEY_BUDGET:?}");
    assert!(
        median < KEY_BUDGET,
        "key to screen {median:?} over {KEY_BUDGET:?}"
    );
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// The script of a run whose tool does not come back inside the case's window: the
/// run stays on screen with no reply open, so the bar steps on its own clock.
fn busy_script() -> String {
    String::from(
        r#"{"replies": [{"calls": [
            {"name": "bash", "arguments": {"command": "sleep 11"}}
        ]}]}"#,
    )
}

/// Ten seconds of a busy run whose tool does not come back use under a tenth of a
/// second of CPU, and its bar moves the whole time.
#[tokio::test]
async fn a_busy_run_moves_its_bar_under_a_tenth_of_a_second_of_cpu() {
    let home = Home::new();
    let provider = home.script("busy", &busy_script());
    tokio::time::sleep(SETTLE).await;
    let mut tui = Tui::styled(&home, &["--provider", &provider], WIDE, ROWS);
    tui.until("idle").await;
    tui.until("Context").await;
    // Learn the status line's row while it is still idle, so the lookup cannot
    // race the first frame of the run.
    tui.status("idle").await;
    tui.keys("go\r");
    tui.status("working").await;
    // The bar's filled run is the look's busy bar on screen.
    tui.until("\u{25b0}").await;
    let before = usage(tui.pid).await.1;
    let start_frames = tui.frames();
    tokio::time::sleep(BUSY).await;
    let cpu = usage(tui.pid).await.1.saturating_sub(before);
    let steps = tui.frames() - start_frames;
    eprintln!(
        "busy: {steps} frames and {cpu:?} of cpu over {BUSY:?} (ps' clock is 10 ms), budget \
         under {BUSY_CPU_BUDGET:?}, {} bytes out",
        tui.bytes()
    );
    assert!(
        cpu < BUSY_CPU_BUDGET,
        "a busy run used {cpu:?} of cpu over {BUSY:?}, over {BUSY_CPU_BUDGET:?}"
    );
    // A step every tenth of a second: ten seconds of them.
    assert!(steps >= 50, "the bar moved {steps} times over {BUSY:?}");
    // Still busy, so the frames above are the bar's and not an end of run's.
    tui.status("working").await;
    // Esc ends the run, so the tool's process does not hold the case.
    tui.keys("\u{1b}");
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// Two seconds of a busy run with `NO_MOTION=1` draw at most one frame a second:
/// the bar holds its frame and only the elapsed time moves.
#[tokio::test]
async fn no_motion_holds_the_bar_still_and_draws_at_most_a_frame_a_second() {
    let home = Home::new();
    let provider = home.script("busy", &busy_script());
    tokio::time::sleep(SETTLE).await;
    let mut tui = Tui::still(&home, &["--provider", &provider], WIDE, ROWS);
    tui.until("idle").await;
    tui.until("Context").await;
    tui.status("idle").await;
    tui.keys("go\r");
    tui.status("working").await;
    tui.until("\u{25b1}\u{25b0}\u{25b0}\u{25b0}\u{25b1}\u{25b1}\u{25b1}\u{25b1}")
        .await;
    let frames = tui.frames();
    let steps = tui.steps();
    let bytes = tui.bytes();
    tokio::time::sleep(STILL).await;
    let drawn = tui.frames() - frames;
    let stepped = tui.steps() - steps;
    let written = tui.bytes() - bytes;
    eprintln!(
        "still: {stepped} step frames and {written} bytes over {STILL:?} ({drawn} frames in all, \
         the rest an event's), budget one step frame a second"
    );
    // The bar holds its frame, so the only frames a run draws on its own are the
    // elapsed time's, one a second: a frame an event draws is that event's, not
    // the clock's. That the window holds a step at all proves the frames the
    // clock draws are the ones counted.
    assert!(
        stepped >= 1,
        "the elapsed time never moved over {STILL:?}: {drawn} frames drew nothing"
    );
    assert!(
        stepped as u64 <= STILL.as_secs(),
        "{stepped} step frames over {STILL:?}, over one a second ({drawn} frames in all)"
    );
    tui.status("working").await;
    tui.keys("\u{1b}");
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// A scroll of one line costs the same on a transcript of 5,000 lines as on one
/// of 500: the frame draws the rows the viewport shows, so neither the ring's
/// length nor its cap is in the path.
#[tokio::test]
async fn a_one_line_scroll_costs_the_same_on_a_short_and_a_long_transcript() {
    let home = Home::new();
    tokio::time::sleep(SETTLE).await;
    // Both clients hold their transcript first, and then the windows alternate
    // between them: a busy moment of the machine lands in both, and the smallest
    // window of each is its scroll's own cost.
    let mut short = scroll_client(&home, "short", SHORT_LINES).await;
    let mut long = scroll_client(&home, "long", LONG_LINES).await;
    short.scroll_warm().await;
    long.scroll_warm().await;
    let (mut short_cpu, mut long_cpu) = (Duration::MAX, Duration::MAX);
    for mark in MARKS {
        short_cpu = short_cpu.min(short.scroll_window("short", mark).await);
        long_cpu = long_cpu.min(long.scroll_window("long", mark).await);
    }
    let over = long_cpu.saturating_sub(short_cpu);
    let slack = short_cpu.mul_f64(SCROLL_CPU_SLACK) + IDLE_CPU_BUDGET;
    eprintln!(
        "scroll of one line: {short_cpu:?} of cpu on {SHORT_LINES} lines, {long_cpu:?} on \
         {LONG_LINES}, {over:?} over, slack {slack:?} (ps' clock is 10 ms, {SCROLL_PAIRS} pairs \
         of keys each)"
    );
    assert!(
        over <= slack,
        "a scroll on {LONG_LINES} lines cost {long_cpu:?} against {short_cpu:?} on {SHORT_LINES}, \
         {over:?} over {SCROLL_PAIRS} pairs of them, over the {slack:?} slack"
    );
    assert_eq!(short.quit().await, 0, "a normal quit");
    assert_eq!(long.quit().await, 0, "a normal quit");
}

/// A full-screen client, idle and quiet, whose transcript holds the `lines` lines
/// of one reply streamed by the fake provider.
async fn scroll_client(home: &Home, name: &str, lines: usize) -> Tui {
    let provider = home.script(name, &lines_script(lines));
    let mut tui = Tui::sized(home, &["--provider", &provider], WIDE, ROWS);
    tui.until("idle").await;
    tui.status("idle").await;
    // The reply only streams once a message asks for it.
    tui.keys("go\r");
    tui.until(&format!("line {lines}")).await;
    tui.status("idle").await;
    tui.quiet().await;
    tui
}

/// The script of one reply of `lines` lines, each its own paragraph.
fn lines_script(lines: usize) -> String {
    let mut script = String::from(r#"{"replies": [{"text": ""#);
    for n in 1..=lines {
        script.push_str(&format!("line {n}\\n\\n"));
    }
    script.push_str(r#""}]}"#);
    script
}
