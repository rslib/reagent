//! Tests of the app: a fake worker on a socket and a `vt100` terminal.

use std::borrow::Cow;
use std::io;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use reagent_ai::record::{MESSAGE, encode_message};
use reagent_ai::{AssistantMessage, Content, Message, Text, ToolCall};
use reagent_protocol::lines::Lines;
use reagent_protocol::message::{self, AnswerKind, Mode, Request};
use reagent_store::{Header, Session, Writer};
use serde_json::{Value, json};
use tokio::net::UnixListener;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use unicode_width::UnicodeWidthStr;

use super::sheet::Mark;
use super::{App, Exit, HostHandle, Options, run};
use crate::client::{Frame, Initialized};
use crate::host::{
    FakeHost, Files, Host, HostFuture, LoginStep, Model, Models, Opened, Provider, Target,
};
use crate::mouse::{Cell, Selection};
use crate::panels::{ContextPanel, SessionRow};
use crate::screen::Terminal;

/// How long a test waits for a frame before it fails.
const WAIT: Duration = Duration::from_secs(10);
/// The rows of the test terminal.
const ROWS: u16 = 24;
/// The columns of the test terminal.
const COLUMNS: u16 = 80;
/// How many rows the `vt100` parser keeps behind the screen.
const SCROLLBACK: usize = 500;
/// The join token the fake worker takes.
const TOKEN: &str = "token";

/// A worker that answers the handshake and sends what a test scripts.
struct Fake {
    /// Held for the test, to remove the socket's folder at the end; a worker a
    /// test drives itself owns no folder.
    _dir: Option<tempfile::TempDir>,
    socket: PathBuf,
    /// The raw line of every request the client sent, the handshake's first.
    sent: mpsc::UnboundedReceiver<String>,
    /// The lines the client has to read.
    out: mpsc::UnboundedSender<Vec<u8>>,
}

impl Fake {
    /// Starts a worker on a socket in `/tmp`, where paths are short enough.
    fn start(session: &str, committed: u64) -> Fake {
        Fake::start_at(session, committed, committed)
    }

    /// Starts a worker whose attach answer names another record than its
    /// handshake, as when a record is committed between the two.
    /// Starts a worker whose `initialize` answer names a model and its window.
    fn naming(session: &str, model: &str, window: u64) -> Fake {
        Fake::start_named(session, 0, 0, Some(model.to_owned()), Some(window))
    }

    fn start_at(session: &str, committed: u64, attached: u64) -> Fake {
        Fake::start_named(session, committed, attached, None, None)
    }

    /// Starts a worker whose handshake answers `committed` and `attached`, and
    /// whose `initialize` answer names `model` and its `window` when they are
    /// known.
    fn start_named(
        session: &str,
        committed: u64,
        attached: u64,
        model: Option<String>,
        window: Option<u64>,
    ) -> Fake {
        let dir = tempfile::Builder::new()
            .prefix("tui")
            .tempdir_in("/tmp")
            .expect("a temp folder in /tmp");
        let socket = dir.path().join("s.sock");
        let listener = UnixListener::bind(&socket).expect("bind the socket");
        let (sent, sent_rx) = mpsc::unbounded_channel();
        let (out, queue) = mpsc::unbounded_channel();
        let answers = out.clone();
        let session = session.to_owned();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (read, write) = stream.into_split();
            tokio::spawn(write_lines(write, queue));
            let mut answers = Answers {
                out: Some(answers),
                sent,
                session,
                committed,
                attached,
                running: false,
                catch_up: Vec::new(),
                after: Vec::new(),
                close: false,
                model: model.clone(),
                window,
            };
            answers.read(read).await;
        });
        Fake {
            _dir: Some(dir),
            socket,
            sent: sent_rx,
            out,
        }
    }

    /// A worker a test drives itself on a socket it already bound: the app
    /// needs only the socket, since `Cutting` answers its connections.
    fn at(socket: PathBuf) -> Fake {
        Fake {
            _dir: None,
            socket,
            sent: mpsc::unbounded_channel().1,
            out: mpsc::unbounded_channel().0,
        }
    }

    /// Sends one notification line to the app.
    fn notify(&self, method: &str, params: &Value) {
        let mut line = Vec::new();
        message::notify(
            &mut line,
            method,
            &serde_json::to_vec(params).expect("a body"),
        );
        let _ = self.out.send(line);
    }

    /// Sends one event line to the app.
    fn event(&self, body: Value) {
        self.notify("event", &body);
    }

    /// The next request the app sent, as a line, once the handshake is past.
    async fn next_line(&mut self) -> String {
        loop {
            let line = timeout(WAIT, self.sent.recv())
                .await
                .expect("a request before the wait ran out")
                .expect("the fake worker");
            let (_, request) = message::decode(line.as_bytes()).expect("decode");
            if matches!(request, Request::Initialize { .. } | Request::Attach { .. }) {
                continue;
            }
            return line;
        }
    }

    /// The next request, the handshake's included.
    async fn next_any(&mut self) -> String {
        timeout(WAIT, self.sent.recv())
            .await
            .expect("a request before the wait ran out")
            .expect("the fake worker")
    }

    /// The next message the app sent, and how it joined the run.
    async fn expect_send(&mut self) -> (String, Mode) {
        let line = self.next_line().await;
        match message::decode(line.as_bytes()).expect("decode").1 {
            Request::Send { text, mode } => (text.into_owned(), mode),
            other => panic!("expected a send, got {other:?}"),
        }
    }

    /// The next answer the app sent.
    async fn expect_approve(&mut self) -> (u64, AnswerKind, Option<String>) {
        let line = self.next_line().await;
        match message::decode(line.as_bytes()).expect("decode").1 {
            Request::Approve { id, answer, reason } => {
                (id, answer, reason.map(|text| text.into_owned()))
            }
            other => panic!("expected an approve, got {other:?}"),
        }
    }

    /// The next settings request, as its model, its level and its mode.
    async fn expect_set(&mut self) -> (Option<String>, Option<String>, Option<String>) {
        let line = self.next_line().await;
        match message::decode(line.as_bytes()).expect("decode").1 {
            Request::Set { model, level, mode } => (
                model.map(std::borrow::Cow::into_owned),
                level.map(std::borrow::Cow::into_owned),
                mode.map(std::borrow::Cow::into_owned),
            ),
            other => panic!("expected a set, got {other:?}"),
        }
    }

    /// The next request, by the method's name.
    async fn expect_method(&mut self) -> &'static str {
        name_of(&self.next_any().await)
    }
}

/// The name of the method the request line `line` carries.
fn name_of(line: &str) -> &'static str {
    let (_, request) = message::decode(line.as_bytes()).expect("decode");
    match request {
        Request::Initialize { .. } => "initialize",
        Request::Attach { .. } => "attach",
        Request::Send { .. } => "send",
        Request::Approve { .. } => "approve",
        Request::Cancel => "cancel",
        Request::Detach => "detach",
        Request::Set { .. } => "set",
        Request::Compact { .. } => "compact",
    }
}

/// Writes every line the fake hands it, in order.
async fn write_lines(
    mut write: tokio::net::unix::OwnedWriteHalf,
    mut queue: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    use tokio::io::AsyncWriteExt;
    while let Some(line) = queue.recv().await {
        if write.write_all(&line).await.is_err() {
            return;
        }
    }
}

/// The reading side of the fake: it answers the handshake and records the
/// lines the app sent.
struct Answers {
    /// `None` once the connection closed.
    out: Option<mpsc::UnboundedSender<Vec<u8>>>,
    sent: mpsc::UnboundedSender<String>,
    session: String,
    committed: u64,
    attached: u64,
    /// Whether the answer to `attach` says a run is open.
    running: bool,
    /// The frames written before the `attach` answer: the one catch-up frame a
    /// worker sends a client that fell behind.
    catch_up: Vec<u8>,
    /// The frames written once the `attach` answer went out.
    after: Vec<u8>,
    /// Whether the connection closes once those frames went out, as one a cut
    /// found in the middle of a frame does.
    close: bool,
    /// The model the `initialize` answer names, when it names one.
    model: Option<String>,
    /// The context window the `initialize` answer names, when it names one.
    window: Option<u64>,
}

impl Answers {
    /// Writes one line to the client.
    fn write(&self, line: Vec<u8>) {
        if let Some(out) = &self.out {
            let _ = out.send(line);
        }
    }

    async fn read(&mut self, mut read: OwnedReadHalf) {
        let mut lines = Lines::new();
        loop {
            match lines.fill(&mut read).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            while let Some(Ok(line)) = lines.next() {
                let raw = String::from_utf8_lossy(line).into_owned();
                let Ok((id, request)) = message::decode(line) else {
                    continue;
                };
                let attaches = matches!(&request, Request::Attach { .. });
                let body = match request {
                    Request::Attach { .. } => {
                        json!({ "committed": self.attached, "running": self.running })
                    }
                    _ => {
                        let mut body = json!({
                            "major": message::MAJOR,
                            "version": "1",
                            "session": self.session,
                            "client": 7,
                            "capabilities": [],
                            "committed": self.committed,
                        });
                        if let Some(model) = &self.model {
                            body["model"] = json!(model);
                        }
                        if let Some(window) = self.window {
                            body["context_window"] = json!(window);
                        }
                        body
                    }
                };
                if attaches && !self.catch_up.is_empty() {
                    let catch_up = std::mem::take(&mut self.catch_up);
                    self.write(catch_up);
                }
                let mut answer = Vec::new();
                message::result(&mut answer, id.unwrap_or(0), &body);
                self.write(answer);
                if attaches && !self.after.is_empty() {
                    let after = std::mem::take(&mut self.after);
                    self.write(after);
                }
                if attaches && self.close {
                    self.out = None;
                }
                let _ = self.sent.send(raw);
            }
        }
    }
}

/// One connection of a worker that a test cuts.
struct Link {
    /// The record the answers to `initialize` and `attach` name.
    committed: u64,
    /// Whether the answer to `attach` says a run is open.
    running: bool,
    /// The frames written before the `attach` answer.
    catch_up: Vec<u8>,
    /// The frames written once the `attach` answer went out.
    after: Vec<u8>,
    /// Whether the connection closes once those frames went out.
    close: bool,
    /// The lines the worker writes itself.
    out: mpsc::UnboundedSender<Vec<u8>>,
    /// The lines the writer takes from `out`.
    queue: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl Link {
    /// A connection that names `committed`, writes `catch_up` before its
    /// `attach` answer and `after` once it sent it, and answers the sender the
    /// test writes more lines with.
    fn new(
        committed: u64,
        catch_up: Vec<u8>,
        after: Vec<u8>,
    ) -> (Link, mpsc::UnboundedSender<Vec<u8>>) {
        let (out, queue) = mpsc::unbounded_channel();
        (
            Link {
                committed,
                running: false,
                catch_up,
                after,
                close: false,
                out: out.clone(),
                queue,
            },
            out,
        )
    }

    /// Closes the connection once the frames after its `attach` answer went
    /// out, as a cut found in the middle of a frame does: it says nothing.
    fn closes(mut self) -> Link {
        self.close = true;
        self
    }

    /// Has its `attach` answer say whether a run is open.
    fn running(mut self, running: bool) -> Link {
        self.running = running;
        self
    }
}

/// A worker that a test cuts: it answers the connections it was given, in
/// order, so a client that reconnects finds the next one on the same socket.
struct Cutting {
    /// Held for the test, to remove the socket's folder at the end.
    _dir: tempfile::TempDir,
    socket: PathBuf,
    /// The raw line of every request the client sent, from every connection.
    sent: mpsc::UnboundedReceiver<String>,
}

impl Cutting {
    /// Binds a socket in `/tmp` and serves `links` in order.
    fn start(session: &str, links: Vec<Link>) -> Cutting {
        let dir = tempfile::Builder::new()
            .prefix("tui")
            .tempdir_in("/tmp")
            .expect("a temp folder in /tmp");
        let socket = dir.path().join("s.sock");
        let listener = UnixListener::bind(&socket).expect("bind the socket");
        let (sent, sent_rx) = mpsc::unbounded_channel();
        let session = session.to_owned();
        tokio::spawn(async move {
            let mut links = links.into_iter();
            while let Ok((stream, _)) = listener.accept().await {
                let Some(link) = links.next() else {
                    return;
                };
                let (read, write) = stream.into_split();
                tokio::spawn(write_lines(write, link.queue));
                let mut answers = Answers {
                    out: Some(link.out),
                    sent: sent.clone(),
                    session: session.clone(),
                    committed: link.committed,
                    attached: link.committed,
                    running: link.running,
                    catch_up: link.catch_up,
                    after: link.after,
                    close: link.close,
                    model: None,
                    window: None,
                };
                tokio::spawn(async move { answers.read(read).await });
            }
        });
        Cutting {
            _dir: dir,
            socket,
            sent: sent_rx,
        }
    }

    /// The worker the harness holds: the app reaches it by its socket only.
    fn fake(&self) -> Fake {
        Fake::at(self.socket.clone())
    }

    /// The next request the app sent, by the method's name, the handshake's
    /// included.
    async fn expect_method(&mut self) -> &'static str {
        let line = timeout(WAIT, self.sent.recv())
            .await
            .expect("a request before the wait ran out")
            .expect("the fake worker");
        name_of(&line)
    }
}

/// One event line, as the worker writes it.
fn event(body: Value) -> Vec<u8> {
    let mut line = Vec::new();
    message::notify(
        &mut line,
        "event",
        &serde_json::to_vec(&body).expect("a body"),
    );
    line
}

/// The line of a `closing` notice.
fn closing(reason: &str) -> Vec<u8> {
    let mut line = Vec::new();
    message::notify(
        &mut line,
        "closing",
        &serde_json::to_vec(&json!({ "reason": reason })).expect("a body"),
    );
    line
}

/// A terminal for the app: every frame collects into one buffer, and the test
/// feeds it keys.
struct TestTerminal {
    /// Every byte the app wrote, in order.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// The keys the test presses.
    keys: mpsc::UnboundedReceiver<Event>,
    /// The size the app sees.
    size: (u16, u16),
    /// How many times the app asked for the size.
    sizes: Arc<std::sync::atomic::AtomicUsize>,
}

impl Terminal for TestTerminal {
    fn size(&self) -> (u16, u16) {
        self.sizes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.size
    }

    async fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        self.bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(frame);
        Ok(())
    }

    async fn next_event(&mut self) -> io::Result<Event> {
        match self.keys.recv().await {
            Some(event) => Ok(event),
            None => std::future::pending().await,
        }
    }
}

/// A copy of the app that a test drives.
struct Harness {
    fake: Fake,
    bytes: Arc<Mutex<Vec<u8>>>,
    keys: mpsc::UnboundedSender<Event>,
    /// How many times the app asked the terminal for its size.
    sizes: Arc<std::sync::atomic::AtomicUsize>,
    handle: JoinHandle<io::Result<Exit>>,
}

impl Harness {
    /// Starts the app against `fake`, with `options` when a test overrides
    /// them.
    fn start(options: Options, fake: Fake) -> Harness {
        Harness::sized(options, fake, (COLUMNS, ROWS))
    }

    /// Starts the app on a `columns` by `rows` terminal.
    fn sized(mut options: Options, fake: Fake, (columns, rows): (u16, u16)) -> Harness {
        options.socket = fake.socket.clone();
        let (keys, events) = mpsc::unbounded_channel();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sizes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let terminal = TestTerminal {
            bytes: Arc::clone(&bytes),
            keys: events,
            size: (columns, rows),
            sizes: Arc::clone(&sizes),
        };
        Harness {
            fake,
            bytes,
            keys,
            sizes,
            handle: tokio::spawn(run(options, terminal)),
        }
    }

    /// Starts the app for a new session.
    fn new_session(fake: Fake) -> Harness {
        Harness::start(options(), fake)
    }

    /// Types `text` one key at a time.
    fn type_text(&self, text: &str) {
        for c in text.chars() {
            self.press(KeyCode::Char(c));
        }
    }

    /// Presses one key.
    fn press(&self, code: KeyCode) {
        self.send(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Sends one key event.
    fn send(&self, key: KeyEvent) {
        let _ = self.keys.send(Event::Key(key));
    }

    /// Sends one mouse event at `column` of `row`.
    fn mouse(&self, kind: MouseEventKind, column: u16, row: u16) {
        let _ = self.keys.send(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }

    /// Sends one bracketed paste of `text`.
    fn paste(&self, text: &str) {
        let _ = self.keys.send(Event::Paste(text.to_owned()));
    }
}

/// The options of a test that starts a new session.
fn options() -> Options {
    Options {
        socket: PathBuf::new(),
        token: TOKEN.to_owned(),
        session_dir: None,
        client_name: "probe".to_owned(),
        ..Options::default()
    }
}

/// Waits until an approval box on screen takes its answer keys.
async fn armed() {
    tokio::time::sleep(super::APPROVAL_ARMED + Duration::from_millis(50)).await;
}

/// The options of a test that draws full screen.
fn full() -> Options {
    Options {
        full: true,
        ..options()
    }
}

/// The terminal's text: the scrollback and the screen, row by row.
fn text(bytes: &Arc<Mutex<Vec<u8>>>) -> String {
    drawn_at(bytes, 0, ROWS, COLUMNS)
}

/// The text the bytes of the terminal after `from` draw on a `rows` by
/// `columns` screen, row by row.
///
/// A resize draws for another size, so a test reads the frames behind it on a
/// screen of that size: full screen repaints every cell after a resize.
fn drawn_at(bytes: &Arc<Mutex<Vec<u8>>>, from: usize, rows: u16, columns: u16) -> String {
    let mut parser = vt100::Parser::new(nonzero(rows), nonzero(columns), SCROLLBACK);
    let held = bytes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    parser.process(&held[from.min(held.len())..]);
    parser
        .screen()
        .rows(0, columns)
        .collect::<Vec<String>>()
        .join("\n")
}

/// Everything the app wrote, read as text, for the sequences a full-screen
/// test looks for.
fn raw(bytes: &Arc<Mutex<Vec<u8>>>) -> String {
    let held = bytes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    String::from_utf8_lossy(&held).into_owned()
}

/// How many bytes the app wrote so far.
fn written(bytes: &Arc<Mutex<Vec<u8>>>) -> usize {
    bytes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

/// Whether one row of the screen is exactly `line`, once the two blank columns
/// a full-screen transcript keeps at its left edge are left out.
fn shows_row(screen: &str, line: &str) -> bool {
    screen.lines().any(|row| {
        let row = row.trim_end();
        row == line || row.strip_prefix("  ") == Some(line)
    })
}

/// Waits until one row of the screen is exactly `line`.
async fn wait_row(bytes: &Arc<Mutex<Vec<u8>>>, line: &str) {
    let start = tokio::time::Instant::now();
    loop {
        let screen = text(bytes);
        if shows_row(&screen, line) {
            return;
        }
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for a row of {line:?}, and the screen shows:\n{screen}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Waits until one row of the `rows` by `columns` screen the bytes after
/// `from` draw is exactly `line`.
async fn wait_row_at(
    bytes: &Arc<Mutex<Vec<u8>>>,
    from: usize,
    rows: u16,
    columns: u16,
    line: &str,
) {
    let start = tokio::time::Instant::now();
    loop {
        let screen = drawn_at(bytes, from, rows, columns);
        if shows_row(&screen, line) {
            return;
        }
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for a row of {line:?} on a {columns} by {rows} screen, \
             which shows:\n{screen}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Waits until no row of the screen is exactly `line`.
async fn wait_no_row(bytes: &Arc<Mutex<Vec<u8>>>, line: &str) {
    let start = tokio::time::Instant::now();
    loop {
        let screen = text(bytes);
        if !shows_row(&screen, line) {
            return;
        }
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for the row of {line:?} to go away, and the screen shows:\n{screen}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Everything the terminal still holds, the rows scrolled off the screen first.
fn whole(bytes: &Arc<Mutex<Vec<u8>>>) -> String {
    let mut parser = vt100::Parser::new(nonzero(ROWS), nonzero(COLUMNS), SCROLLBACK);
    let held = bytes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    parser.process(&held);
    let screen = parser.screen_mut();
    let mut rows = Vec::new();
    // The oldest window first: each step back by one row drops that row from the
    // top, which is the row this collects.
    for at in (1..=screen.scrollback_len()).rev() {
        screen.set_scrollback(at);
        if let Some(row) = screen.rows(0, COLUMNS).next() {
            rows.push(row);
        }
    }
    screen.set_scrollback(0);
    rows.extend(screen.rows(0, COLUMNS));
    rows.join("\n")
}

/// Waits until the screen shows `needle`.
/// Waits until `needle` has been drawn at least once: it is in the bytes the
/// terminal got, though a later frame may have drawn over it. A status note
/// lasts only until the next status change, so on a busy machine the final
/// screen may never show it.
async fn wait_drawn(bytes: &Arc<Mutex<Vec<u8>>>, needle: &str) {
    let start = tokio::time::Instant::now();
    loop {
        let drawn = {
            let bytes = bytes.lock().expect("the frame bytes");
            bytes
                .windows(needle.len())
                .any(|window| window == needle.as_bytes())
        };
        if drawn {
            return;
        }
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for {needle:?} to be drawn, the screen shows:\n{}",
            text(bytes)
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn wait_text(bytes: &Arc<Mutex<Vec<u8>>>, needle: &str) {
    let start = tokio::time::Instant::now();
    loop {
        let screen = text(bytes);
        if screen.contains(needle) {
            return;
        }
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for {needle:?} on the screen, which shows:\n{screen}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// The screen cell of the `≡` a folded paste chip starts with, as `(row,
/// column)`.
fn chip_cell(screen: &str) -> (u16, u16) {
    cell_of(screen, "≡")
}

/// The screen cell of the first `needle`, as `(row, column)`.
fn cell_of(screen: &str, needle: &str) -> (u16, u16) {
    for (row, line) in screen.lines().enumerate() {
        if let Some(at) = line.find(needle) {
            let column = UnicodeWidthStr::width(&line[..at]) as u16;
            return (row as u16, column);
        }
    }
    panic!("no {needle:?} on:\n{screen}");
}

/// Waits until the screen no longer shows `needle`.
async fn wait_gone(bytes: &Arc<Mutex<Vec<u8>>>, needle: &str) {
    let start = tokio::time::Instant::now();
    loop {
        let screen = text(bytes);
        if !screen.contains(needle) {
            return;
        }
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for {needle:?} to go away, and the screen still shows:\n{screen}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// A width for the parser's `NonZeroU16`.
fn nonzero(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value).unwrap_or(NonZeroU16::MIN)
}

/// The text of a tool result with lines `1..=count`.
fn numbered(count: usize) -> String {
    (1..=count)
        .map(|n| format!("line {n}"))
        .collect::<Vec<String>>()
        .join("\n")
}

/// Writes one journal of two turns into `dir` and answers its last record.
fn write_journal(dir: &Path) -> u64 {
    let header = Header::new(
        "sess-hist".to_owned(),
        reagent_store::paths::now_ms(),
        None,
        Bytes::new(),
    );
    let mut writer = Writer::create(dir, header).expect("the writer");
    let messages = vec![
        Message::user("hello from the user"),
        Message::assistant(AssistantMessage::new(
            "fake",
            "fake-model",
            vec![Content::Text {
                text: Text::from("and the reply"),
            }],
        )),
        Message::user("a second question"),
        Message::assistant(AssistantMessage::new(
            "fake",
            "fake-model",
            vec![Content::Text {
                text: Text::from("a second reply"),
            }],
        )),
    ];
    for message in &messages {
        let mut payload = Vec::new();
        encode_message(&mut payload, message);
        writer
            .append(MESSAGE, Bytes::from(payload), false)
            .expect("append a message");
    }
    writer.commit().expect("commit");
    writer.close().expect("close");
    let session = Session::open(dir).expect("open the journal");
    session
        .records()
        .filter_map(Result::ok)
        .map(|record| record.seq)
        .max()
        .unwrap_or(0)
}

#[tokio::test]
async fn a_new_session_shows_the_editor_and_the_idle_status() {
    let mut harness = Harness::new_session(Fake::start("sess-abcd1234", 0));
    wait_text(&harness.bytes, "idle").await;
    // A new session shows its box and an idle line: the id is the sidebar's, and
    // inline mode has no sidebar.
    wait_text(&harness.bytes, "Ask anything").await;
    assert_eq!(harness.fake.expect_method().await, "initialize");
    assert_eq!(harness.fake.expect_method().await, "attach");
}

#[tokio::test]
async fn typing_and_enter_sends_the_text() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("hi");
    harness.press(KeyCode::Enter);
    let (text, mode) = harness.fake.expect_send().await;
    assert_eq!(text, "hi");
    assert_eq!(mode, Mode::Queue, "an idle send starts the next run");
}

/// An idle send starts the run: no queued row waits above the box, since the
/// message is not behind a run.
#[tokio::test]
async fn an_idle_send_shows_no_queued_row() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("hi");
    harness.press(KeyCode::Enter);
    let (text_sent, _) = harness.fake.expect_send().await;
    assert_eq!(text_sent, "hi");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let screen = text(&harness.bytes);
    assert!(!screen.contains("↳"), "no queued row:\n{screen}");
}

/// The worker's join notice for this very client names no other client: one
/// other client makes two in all, not three.
#[tokio::test]
async fn the_own_join_notice_is_not_another_client() {
    let harness = Harness::start(full(), Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    // The fake worker's initialize answer names this client 7.
    harness
        .fake
        .notify("client_joined", &json!({ "client": 7, "name": "self" }));
    harness
        .fake
        .notify("client_joined", &json!({ "client": 9, "name": "tmux:2" }));
    wait_text(&harness.bytes, "2 clients").await;
    let screen = text(&harness.bytes);
    assert!(!screen.contains("3 clients"), "own join counted:\n{screen}");
}

/// A bracketed paste of 142 lines through the app's input folds to one chip in
/// the frame, Tab opens it, Tab folds it back, and Enter sends every line.
#[tokio::test]
async fn a_long_paste_folds_to_a_chip_that_tab_opens_and_enter_sends() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    let pasted: String = (1..=142)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    harness.paste(&pasted);
    // The folded chip draws its first line and the 141 it holds back.
    wait_text(&harness.bytes, "≡ line 1").await;
    wait_text(&harness.bytes, "+141").await;
    // Tab opens it: the chip's label goes and its own lines show.
    harness.press(KeyCode::Tab);
    wait_gone(&harness.bytes, "≡ line 1").await;
    wait_text(&harness.bytes, "line 2").await;
    // Tab folds it back to one row.
    harness.press(KeyCode::Tab);
    wait_text(&harness.bytes, "≡ line 1").await;
    // Enter sends all 142 lines, not the chip's label.
    harness.press(KeyCode::Enter);
    let (sent, mode) = harness.fake.expect_send().await;
    assert_eq!(sent, pasted);
    assert_eq!(mode, Mode::Queue);
}

/// A click on a folded paste chip selects it and opens it, the same as Tab.
#[tokio::test]
async fn a_click_on_a_paste_chip_opens_it() {
    let harness = Harness::start(full(), Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    let pasted: String = (1..=142)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    harness.paste(&pasted);
    wait_text(&harness.bytes, "≡ line 1").await;
    let (row, column) = chip_cell(&text(&harness.bytes));
    harness.mouse(MouseEventKind::Down(MouseButton::Left), column, row);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), column, row);
    wait_gone(&harness.bytes, "≡ line 1").await;
    wait_text(&harness.bytes, "line 2").await;
}

/// A click on a finished tool's row opens it, and a second click folds it.
#[tokio::test]
async fn click_folds_a_tool_row_open_and_shut() {
    let harness = Harness::start(full(), Fake::start("sess-fold", 0));
    wait_text(&harness.bytes, "idle").await;
    // A reply above the block, long enough that a narrower terminal gives each
    // of its lines more than one row, and that the block sits in a transcript
    // taller than the live area it is drawn over.
    let reply = "the reply wraps at the width of the screen ".repeat(30);
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": reply }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "tool_start", "id": "call-1", "name": "edit" }));
    harness.fake.event(json!({
        "type": "tool_call_ready",
        "id": "call-1",
        "name": "edit",
        "arguments": json!({
            "path": "src/paint.rs",
            "old_string": "",
            "new_string": numbered(8),
        })
        .to_string(),
    }));
    harness.fake.event(json!({
        "type": "tool_result",
        "id": "call-1",
        "is_error": false,
        "bytes": 8,
    }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    wait_text(&harness.bytes, "src/paint.rs").await;
    let screen = text(&harness.bytes);
    // The folded block shows four diff lines and folds the rest away.
    assert!(
        screen.contains("line 4") && !screen.contains("line 8"),
        "the block starts folded:\n{screen}"
    );

    let (row, column) = cell_of(&screen, "src/paint.rs");
    harness.mouse(MouseEventKind::Down(MouseButton::Left), column, row);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), column, row);
    wait_text(&harness.bytes, "line 8").await;

    let (row, column) = cell_of(&text(&harness.bytes), "src/paint.rs");
    harness.mouse(MouseEventKind::Down(MouseButton::Left), column, row);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), column, row);
    wait_gone(&harness.bytes, "line 8").await;

    // A narrower terminal gives the reply's lines more rows each: the click
    // counts them to find the block.
    let from = written(&harness.bytes);
    harness
        .keys
        .send(Event::Resize(60, ROWS))
        .expect("the keys");
    let screen = wait_at(&harness.bytes, from, ROWS, 60, "src/paint.rs").await;
    let (row, column) = cell_of(&screen, "src/paint.rs");
    harness.mouse(MouseEventKind::Down(MouseButton::Left), column, row);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), column, row);
    wait_at(&harness.bytes, from, ROWS, 60, "line 8").await;
}

/// A click on reply text folds no block, and the drag selection still works
/// after it.
#[tokio::test]
async fn click_folds_leaves_a_plain_text_click_alone() {
    let harness = Harness::start(full(), Fake::start("sess-plain", 0));
    wait_text(&harness.bytes, "idle").await;
    // A reply that wraps over rows, with a tool block under it: a click counts
    // the rows of the lines above it to find the block it landed on.
    let reply = "the reply wraps at the width of the screen ".repeat(12);
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": reply }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "tool_start", "id": "call-1", "name": "write" }));
    harness.fake.event(json!({
        "type": "tool_call_ready",
        "id": "call-1",
        "name": "write",
        "arguments": json!({ "path": "src/paint.rs", "content": numbered(8) }).to_string(),
    }));
    harness.fake.event(json!({
        "type": "tool_result",
        "id": "call-1",
        "is_error": false,
        "bytes": 8,
    }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    wait_text(&harness.bytes, "src/paint.rs").await;
    let screen = text(&harness.bytes);
    let (block, _) = cell_of(&screen, "src/paint.rs");
    let rows: Vec<&str> = screen.lines().collect();
    let row = rows
        .get(..usize::from(block))
        .and_then(|above| above.iter().rposition(|line| line.contains("wraps")))
        .unwrap_or_else(|| panic!("a reply row above the block:\n{screen}")) as u16;

    harness.mouse(MouseEventKind::Down(MouseButton::Left), 4, row);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), 4, row);
    // A key draws a frame: the tool block is still folded to its first lines.
    harness.type_text("x");
    wait_text(&harness.bytes, "x").await;
    let screen = text(&harness.bytes);
    assert!(
        !screen.contains("line 8"),
        "a click on the reply folded no block:\n{screen}"
    );

    // The click left the drag selection as it was.
    harness.mouse(MouseEventKind::Down(MouseButton::Left), 0, row - 2);
    harness.mouse(MouseEventKind::Drag(MouseButton::Left), 20, row);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), 20, row);
    let start = tokio::time::Instant::now();
    loop {
        let drawn = raw(&harness.bytes);
        if drawn.contains("\u{1b}]52;c;") {
            let copied = copied(&drawn);
            assert!(
                copied.contains("wraps"),
                "the drag copies the reply's lines: {copied:?}"
            );
            break;
        }
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for the copy of the selection"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// A click on a panel's title folds the panel to that row, and it stays
/// folded while the sidebar has room for it.
#[tokio::test]
async fn panel_folds_on_a_title_click_and_stays_folded() {
    let harness = Harness::sized(full(), Fake::start("sess-panel", 0), (160, ROWS));
    wait_at(&harness.bytes, 0, ROWS, 160, "▾ Changes").await;
    // A write call gives the panel a row of its own to fold away.
    harness
        .fake
        .event(json!({ "type": "tool_start", "id": "call-1", "name": "write" }));
    harness.fake.event(json!({
        "type": "tool_call_ready",
        "id": "call-1",
        "name": "write",
        "arguments": json!({ "path": "notes.rs", "content": "one\ntwo" }).to_string(),
    }));
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, "notes.rs").await;
    assert!(
        sidebar_rows(&screen).any(|row| row.contains("notes.rs")),
        "the panel shows the path:\n{screen}"
    );

    let (row, column) = cell_of(&screen, "▾ Changes");
    harness.mouse(MouseEventKind::Down(MouseButton::Left), column, row);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), column, row);
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, "▸ Changes").await;
    assert!(
        !sidebar_rows(&screen).any(|row| row.contains("notes.rs")),
        "the panel folded to its title row:\n{screen}"
    );
    assert!(
        column_of(&screen, "▾ Context").is_some(),
        "the panels under it still draw:\n{screen}"
    );

    // A later frame, with room for the panel, leaves it folded.
    harness.type_text("x");
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, "x").await;
    assert!(
        screen.contains("▸ Changes"),
        "the panel stays folded as the user left it:\n{screen}"
    );
}

/// Enter on the focused panel folds it, and the panels under it keep their
/// rows.
#[tokio::test]
async fn panel_folds_on_enter_when_focused() {
    let harness = Harness::sized(full(), Fake::start("sess-enter", 0), (160, ROWS));
    wait_at(&harness.bytes, 0, ROWS, 160, "▾ Context").await;
    harness.send(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    harness.press(KeyCode::Tab);
    harness.press(KeyCode::Enter);
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, "▸ Context").await;
    assert!(
        screen.contains("▾ Changes"),
        "the other panels stay open:\n{screen}"
    );
}

/// An app in full screen at 160 columns whose sessions panel lists forty
/// sessions, `row0` being this session, with the host that records the ones it
/// opens.
fn paged(host: FakeHost) -> (App, Arc<FakeHost>) {
    let init = Initialized {
        session: "sess-row0".to_owned(),
        ..Initialized::default()
    };
    let mut app = App::new(&init, (160, 40), true);
    app.screen.resize(160, 40);
    app.reflow();
    let host = Arc::new(host);
    let handle: Arc<dyn Host> = Arc::clone(&host) as Arc<dyn Host>;
    app.host = Some(HostHandle::new(handle));
    let now = reagent_store::paths::now_ms() / 1000;
    app.sessions = (0..40)
        .map(|index| SessionRow {
            id: Arc::from(format!("sess-row{index}")),
            title: Arc::from(format!("row{index}")),
            updated: now - index as u64 * 60,
            live: false,
            waiting: false,
            current: index == 0,
            headless: false,
        })
        .collect();
    app.set_sessions();
    (app, host)
}

/// The screen of the app after one frame it drew.
fn painted(app: &mut App) -> String {
    app.paint();
    screen_text(app)
}

/// Sends a press and a release on one cell: a click.
fn click_cell(app: &mut App, row: u16, column: u16) {
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }
}

/// The cell of the sidebar the app draws `needle` on, as `(row, column)`.
fn sidebar_cell(app: &mut App, needle: &str) -> (u16, u16) {
    let screen = painted(app);
    let (row, column) = cell_of(&screen, needle);
    assert!(
        column >= 120,
        "{needle:?} is drawn in the sidebar at 160 columns:\n{screen}"
    );
    (row, column)
}

/// A click on the row of a session the panel lists asks the host for that
/// session's worker.
#[test]
fn session_row_click_opens_that_session() {
    let (mut app, host) = paged(FakeHost::new().with_open(Ok(Opened {
        socket: PathBuf::from("/tmp/other.sock"),
        token: TOKEN.to_owned(),
        session_dir: None,
    })));
    let (row, column) = sidebar_cell(&mut app, "row3");
    click_cell(&mut app, row, column);
    assert_eq!(host.opened(), [Target::Session("sess-row3".to_owned())]);
    assert!(
        app.waits.open.is_some(),
        "the open of that session is in flight"
    );
}

/// A click on the row of this session asks the host for nothing: the app is
/// already there.
#[test]
fn session_row_click_on_the_current_session_does_nothing() {
    let (mut app, host) = paged(FakeHost::new().with_open(Ok(Opened {
        socket: PathBuf::from("/tmp/other.sock"),
        token: TOKEN.to_owned(),
        session_dir: None,
    })));
    let (row, column) = sidebar_cell(&mut app, "row0");
    click_cell(&mut app, row, column);
    assert!(host.opened().is_empty(), "no session was asked for");
    assert!(app.waits.open.is_none(), "no open is in flight");
}

/// A click on the footer's `›` turns the page of the sessions the panel shows.
#[test]
fn session_page_turns_on_an_arrow_click() {
    let (mut app, _host) = paged(FakeHost::new());
    let screen = painted(&mut app);
    assert!(screen.contains("1–8 of 40"), "{screen}");
    let (row, column) = cell_of(&screen, "›");
    click_cell(&mut app, row, column);
    let screen = painted(&mut app);
    assert!(screen.contains("9–16 of 40"), "{screen}");
    assert!(
        screen.contains("row8"),
        "the next page starts there:\n{screen}"
    );
}

/// Right turns the page of the sessions while the sidebar has the keys and the
/// Sessions panel has the focus; Left turns it back.
#[test]
fn session_page_turns_on_right_when_focused() {
    let (mut app, _host) = paged(FakeHost::new());
    let (title, _) = sidebar_cell(&mut app, "Sessions");
    let panel = app.panel_at(title).expect("the sessions panel");
    app.sidebar.set_active(true);
    while app.sidebar.focus() != panel {
        assert!(app.sidebar.focus_next(), "the panel is there to focus");
    }
    app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    let screen = painted(&mut app);
    assert!(screen.contains("9–16 of 40"), "{screen}");
    app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    let screen = painted(&mut app);
    assert!(screen.contains("1–8 of 40"), "{screen}");
}

/// The sidebar's columns of a 160 column screen, row by row.
fn sidebar_rows(screen: &str) -> impl Iterator<Item = &str> {
    screen.lines().filter_map(|row| row.get(120..))
}

#[tokio::test]
async fn a_streamed_reply_renders_as_markdown_and_is_committed() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    harness.fake.event(json!({
        "type": "thinking_delta",
        "text": "the user wants a title",
    }));
    wait_text(&harness.bytes, "thought").await;
    harness.fake.event(json!({
        "type": "text_delta",
        "text": "# Title\n\nhello *world*\n",
    }));
    wait_text(&harness.bytes, "hello world").await;
    let live = text(&harness.bytes);
    assert!(
        !live.contains("# Title") && !live.contains("*world*"),
        "the markers are drawn, not printed:\n{live}"
    );
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    harness
        .fake
        .event(json!({ "type": "run_end", "reason": "end_turn" }));
    wait_text(&harness.bytes, "idle").await;
    wait_text(&harness.bytes, "Title").await;
    wait_text(&harness.bytes, "hello world").await;
    // It was printed into the scrollback: the row survives the empty live area
    // that follows the run.
    let after = text(&harness.bytes);
    assert!(
        after.contains("Title") && after.contains("hello world"),
        "the reply stays after the run ends:\n{after}"
    );
    // The live area follows its content: the empty editor and the status line
    // sit right under the last committed line, and nothing is drawn below them.
    let rows: Vec<&str> = after.lines().collect();
    let reply = rows
        .iter()
        .rposition(|row| row.contains("hello world"))
        .expect("the reply's row");
    assert!(
        rows[reply + 1].trim().is_empty() && rows[reply + 2].contains("tokens"),
        "the turn's rule is under the reply and a blank row:\n{after}"
    );
    let box_ = rows
        .iter()
        .position(|row| row.contains("Ask anything"))
        .expect("the text box draws");
    assert!(
        box_ > reply + 1,
        "the empty editor is under the rule:\n{after}"
    );
    let status = rows
        .iter()
        .rposition(|row| !row.trim().is_empty())
        .expect("a status row");
    assert!(
        rows[status].contains("idle"),
        "the status is the last row:\n{after}"
    );
    assert!(
        rows[status + 1..].iter().all(|row| row.trim().is_empty()),
        "the rows below the status are empty, with no blank band above it:\n{after}"
    );
}

#[tokio::test]
async fn a_tool_call_shows_its_lines_and_folds_the_rest() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({
        "type": "tool_start",
        "id": "call_1",
        "name": "Bash",
    }));
    harness.fake.event(json!({
        "type": "tool_call_ready",
        "id": "call_1",
        "name": "Bash",
        "arguments": "{\"command\":\"seq 30\"}",
    }));
    harness.fake.event(json!({
        "type": "tool_result",
        "id": "call_1",
        "is_error": true,
        "bytes": 200,
        "error": numbered(30),
    }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    wait_text(&harness.bytes, "seq 30").await;
    let screen = text(&harness.bytes);
    assert!(screen.contains("Bash"), "the call's row:\n{screen}");
    assert!(
        screen.contains("line 1"),
        "the first result line, at the right edge of the row:\n{screen}"
    );
    assert!(
        !screen.contains("line 10"),
        "the rest of the result is folded into the one row:\n{screen}"
    );
    assert!(!screen.contains("line 20"), "the rest is folded:\n{screen}");
}

#[tokio::test]
async fn a_save_rule_answers_save() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.notify(
        "approval_request",
        &json!({
            "id": 5,
            "tool": "Bash",
            "call": "cargo test --all",
            "rule": "Bash(cargo test:*)",
            "save": ["Bash(cargo test:*)"],
        }),
    );
    wait_text(&harness.bytes, "cargo test --all").await;
    wait_text(&harness.bytes, "Bash(cargo test:*)").await;
    wait_text(&harness.bytes, "Always here").await;
    wait_text(&harness.bytes, "waits for you").await;
    armed().await;
    harness.press(KeyCode::Char('a'));
    let (id, answer, reason) = harness.fake.expect_approve().await;
    assert_eq!(id, 5);
    assert_eq!(answer, AnswerKind::Save);
    assert_eq!(reason, None);
    wait_gone(&harness.bytes, "Allow once").await;
}

#[tokio::test]
async fn a_reason_refuses_with_it() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.notify(
        "approval_request",
        &json!({
            "id": 6,
            "tool": "Bash",
            "call": "rm -rf build",
            "rule": "default",
        }),
    );
    wait_text(&harness.bytes, "rm -rf build").await;
    // No save rule was offered, so the card does not offer one.
    let card = text(&harness.bytes);
    assert!(
        !card.contains("saves the rule"),
        "a save is offered only when a rule is:\n{card}"
    );
    armed().await;
    // Tell it instead opens the note the refusal goes with.
    harness.press(KeyCode::Char('t'));
    wait_text(&harness.bytes, "your note").await;
    harness.type_text("too risky");
    wait_text(&harness.bytes, "too risky").await;
    harness.press(KeyCode::Enter);
    let (id, answer, reason) = harness.fake.expect_approve().await;
    assert_eq!(id, 6);
    assert_eq!(answer, AnswerKind::Refuse);
    assert_eq!(reason.as_deref(), Some("too risky"));
}

#[tokio::test]
async fn escape_refuses_without_a_reason() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.notify(
        "approval_request",
        &json!({
            "id": 7,
            "tool": "Bash",
            "call": "make",
            "rule": "default",
        }),
    );
    wait_text(&harness.bytes, "make").await;
    armed().await;
    // The spec's answer: Esc in the box refuses at once, with no reason and no
    // reason field.
    harness.press(KeyCode::Esc);
    let (id, answer, reason) = harness.fake.expect_approve().await;
    assert_eq!(id, 7);
    assert_eq!(answer, AnswerKind::Refuse);
    assert_eq!(reason, None);
}

#[tokio::test]
async fn another_client_answers_and_the_box_closes() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.notify(
        "approval_request",
        &json!({
            "id": 8,
            "tool": "Bash",
            "call": "git status",
            "rule": "default",
        }),
    );
    wait_text(&harness.bytes, "Allow once").await;
    harness.fake.notify(
        "approval_resolved",
        &json!({ "id": 8, "client": 2, "answer": "once" }),
    );
    wait_gone(&harness.bytes, "Allow once").await;
    wait_text(&harness.bytes, "idle").await;
}

#[tokio::test]
async fn escape_in_the_note_leaves_no_refusal_for_another_clients_answer() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.notify(
        "approval_request",
        &json!({
            "id": 9,
            "tool": "Bash",
            "call": "make",
            "rule": "default",
        }),
    );
    wait_text(&harness.bytes, "Allow once").await;
    armed().await;
    // Tell it instead opens the note, and a line of it is typed in.
    harness.press(KeyCode::Char('t'));
    wait_text(&harness.bytes, "your note").await;
    harness.type_text("wait for me");
    wait_text(&harness.bytes, "wait for me").await;
    // Esc in the note goes back to the choices: the card is still on screen and
    // this client gave no answer.
    harness.press(KeyCode::Esc);
    wait_text(&harness.bytes, "Allow once").await;
    // Another client allows the call, and the box closes on that answer.
    harness.fake.notify(
        "approval_resolved",
        &json!({ "id": 9, "client": 2, "answer": "once" }),
    );
    wait_gone(&harness.bytes, "Allow once").await;
    wait_text(&harness.bytes, "idle").await;
    let screen = text(&harness.bytes);
    assert!(
        !screen.contains("\u{2717} denied"),
        "the history holds no refusal this client never gave:\n{screen}"
    );
}

#[tokio::test]
async fn enter_steers_while_a_run_streams() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("start");
    harness.press(KeyCode::Enter);
    let (text, mode) = harness.fake.expect_send().await;
    assert_eq!(text, "start");
    assert_eq!(mode, Mode::Queue);
    wait_text(&harness.bytes, "working").await;
    // A turn ends after every provider call, a tool's too: the run still goes.
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "tool_use" }));
    harness.type_text("use nextest");
    harness.press(KeyCode::Enter);
    let (text, mode) = harness.fake.expect_send().await;
    assert_eq!(text, "use nextest");
    assert_eq!(mode, Mode::Steer);
}

#[tokio::test]
async fn two_ctrl_c_presses_detach_and_end_the_session() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    assert_eq!(harness.fake.expect_method().await, "initialize");
    assert_eq!(harness.fake.expect_method().await, "attach");
    harness.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    wait_text(&harness.bytes, "Ctrl+C again to quit").await;
    harness.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(harness.fake.expect_method().await, "detach");
    let exit = timeout(WAIT, harness.handle)
        .await
        .expect("the app ends")
        .expect("the app does not panic")
        .expect("the app does not fail");
    assert_eq!(exit, Exit { code: 0 });
}

#[tokio::test]
async fn a_continued_session_shows_its_history() {
    let journal = tempfile::tempdir().expect("a temp folder");
    let committed = write_journal(journal.path());
    let fake = Fake::start("sess-hist", committed);
    let mut options = options();
    options.session_dir = Some(journal.path().to_path_buf());
    let harness = Harness::start(options, fake);
    wait_text(&harness.bytes, "hello from the user").await;
    wait_text(&harness.bytes, "and the reply").await;
    wait_text(&harness.bytes, "a second question").await;
    wait_text(&harness.bytes, "a second reply").await;
    wait_text(&harness.bytes, "idle").await;
    // The history is printed before the live area, which sits right under it.
    let screen = text(&harness.bytes);
    let rows: Vec<&str> = screen.lines().collect();
    let last = rows
        .iter()
        .rposition(|row| !row.trim().is_empty())
        .expect("a row with text");
    assert!(
        rows[last].contains("idle"),
        "the status is the last row with text:\n{screen}"
    );
    assert!(
        rows[last - 1].trim().is_empty(),
        "the empty editor is right under the history:\n{screen}"
    );
}

#[tokio::test]
async fn a_word_longer_than_the_row_wraps() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    let word = "x".repeat(200);
    harness.fake.event(json!({
        "type": "user_message",
        "client": 1,
        "mode": "queue",
        "text": word,
    }));
    wait_text(&harness.bytes, "xxxx").await;
    // The live frame, not the stream: an earlier frame held a shorter row.
    let screen = whole(&harness.bytes);
    // Only the rows of the user turn hold the word: the box's footer and the
    // status line carry an `x` of their own (`alt+enter`, `ctrl+x`).
    assert_eq!(
        screen
            .lines()
            .filter(|row| row.contains("xxxx"))
            .map(|row| row.matches('x').count())
            .sum::<usize>(),
        200,
        "the whole word is on the screen:\n{screen}"
    );
    assert!(
        screen
            .lines()
            .all(|row| unicode_width::UnicodeWidthStr::width(row) <= usize::from(COLUMNS)),
        "no row runs past the terminal:\n{screen}"
    );
    assert!(
        screen.lines().filter(|row| row.contains("xxxx")).count() >= 3,
        "the word takes several rows:\n{screen}"
    );
}

/// Writes `messages` into a journal in `dir` and answers its newest record.
fn write_messages(dir: &Path, messages: &[Message]) -> u64 {
    let header = Header::new(
        "sess-hist".to_owned(),
        reagent_store::paths::now_ms(),
        None,
        Bytes::new(),
    );
    let mut writer = Writer::create(dir, header).expect("the writer");
    for message in messages {
        let mut payload = Vec::new();
        encode_message(&mut payload, message);
        writer
            .append(MESSAGE, Bytes::from(payload), false)
            .expect("append a message");
    }
    writer.commit().expect("commit");
    writer.close().expect("close");
    Session::open(dir)
        .expect("open the journal")
        .records()
        .filter_map(Result::ok)
        .map(|record| record.seq)
        .max()
        .unwrap_or(0)
}

/// A run of tool rows keeps one blank row above it and one below, in the ring
/// and in the live area: the text on each side of it stands one blank row away,
/// and never two.
#[tokio::test]
async fn air_text_then_calls_then_text_has_one_blank_row_each_side() {
    let harness = Harness::start(full(), Fake::start("sess-air", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": "the reply before the call" }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "tool_start", "id": "call-1", "name": "read" }));
    harness.fake.event(json!({
        "type": "tool_call_ready",
        "id": "call-1",
        "name": "read",
        "arguments": json!({ "path": "src/air.rs" }).to_string(),
    }));
    harness.fake.event(json!({
        "type": "tool_result",
        "id": "call-1",
        "is_error": true,
        "error": "no such file",
    }));
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": "the reply under the call" }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    wait_text(&harness.bytes, "the reply under the call").await;
    let screen = text(&harness.bytes);
    let rows: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let call = rows
        .iter()
        .position(|row| row.contains("src/air.rs"))
        .unwrap_or_else(|| panic!("the call's row:\n{screen}"));
    assert!(
        rows[call - 1].is_empty(),
        "a blank row above the call:\n{screen}"
    );
    assert!(
        !rows[call - 2].is_empty(),
        "one blank row above the call, not two:\n{screen}"
    );
    let under = rows
        .iter()
        .position(|row| row.contains("the reply under the call"))
        .unwrap_or_else(|| panic!("the reply under the call:\n{screen}"));
    assert!(
        rows[under - 1].is_empty(),
        "a blank row below the call:\n{screen}"
    );
    assert!(
        !rows[under - 2].is_empty(),
        "one blank row below the call, not two:\n{screen}"
    );
}

/// Only the newest edit shows the lines of its diff: the block of an older one
/// draws again without them when a newer edit arrives.
#[tokio::test]
async fn newest_diff_a_second_edit_folds_the_first() {
    let harness = Harness::start(full(), Fake::start("sess-newest", 0));
    wait_text(&harness.bytes, "idle").await;
    for (id, path, mark) in [
        ("call-1", "src/alpha.rs", "alpha"),
        ("call-2", "src/beta.rs", "beta"),
    ] {
        let content = format!("{mark} 1\n{mark} 2");
        harness.fake.event(json!({ "type": "turn_start" }));
        harness
            .fake
            .event(json!({ "type": "tool_start", "id": id, "name": "edit" }));
        harness.fake.event(json!({
            "type": "tool_call_ready",
            "id": id,
            "name": "edit",
            "arguments": json!({
                "path": path,
                "old_string": "",
                "new_string": content,
            })
            .to_string(),
        }));
        harness.fake.event(json!({
            "type": "tool_result",
            "id": id,
            "is_error": false,
            "bytes": 2,
        }));
        harness
            .fake
            .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    }
    wait_text(&harness.bytes, "beta 2").await;
    let screen = text(&harness.bytes);
    assert!(
        screen.contains("src/beta.rs") && screen.contains("beta 1") && screen.contains("beta 2"),
        "the newest edit shows its diff:\n{screen}"
    );
    assert!(
        screen.contains("src/alpha.rs"),
        "the earlier edit keeps its row:\n{screen}"
    );
    assert!(
        !screen.contains("alpha 1") && !screen.contains("alpha 2"),
        "the earlier edit lost the lines of its diff:\n{screen}"
    );
}

#[tokio::test]
async fn two_tool_calls_in_a_turn_both_print_their_result() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    for (id, arguments) in [
        ("call_1", "{\"command\":\"one\"}"),
        ("call_2", "{\"command\":\"two\"}"),
    ] {
        harness
            .fake
            .event(json!({ "type": "tool_start", "id": id, "name": "Bash" }));
        harness.fake.event(json!({
            "type": "tool_call_ready",
            "id": id,
            "name": "Bash",
            "arguments": arguments,
        }));
    }
    harness.fake.event(json!({
        "type": "tool_result",
        "id": "call_1",
        "bytes": 9,
        "error": "first out",
    }));
    harness.fake.event(json!({
        "type": "tool_result",
        "id": "call_2",
        "bytes": 10,
        "error": "second out",
    }));
    wait_text(&harness.bytes, "first out").await;
    wait_text(&harness.bytes, "second out").await;
    // The run still goes, so its calls keep their own rows: the end of the run
    // folds them under the run's one row.
    let screen = text(&harness.bytes);
    let first = screen.find("first out").expect("the first result");
    let second = screen.find("second out").expect("the second result");
    assert!(first < second, "the results keep the call order:\n{screen}");
    assert!(
        screen.contains("command") || screen.contains("one"),
        "both calls show their line:\n{screen}"
    );
    harness
        .fake
        .event(json!({ "type": "run_end", "reason": "end_turn" }));
    wait_text(&harness.bytes, "idle").await;
}

#[tokio::test]
async fn a_tool_result_shows_the_output_the_journal_recorded() {
    let journal = tempfile::tempdir().expect("a temp folder");
    let newest = write_messages(
        journal.path(),
        &[
            Message::user("run it"),
            Message::ToolResult(reagent_ai::ToolResult {
                call_id: "call_1".into(),
                content: (1..=12)
                    .map(|line| {
                        if line == 1 {
                            "the recorded output".to_owned()
                        } else {
                            format!("line {line}")
                        }
                    })
                    .collect::<Vec<String>>()
                    .join("\n")
                    .into(),
                is_error: false,
                images: Vec::new(),
            }),
        ],
    );
    // The handshake and the attach see the journal before the result's record,
    // so the result reaches the app through the committed record only.
    let fake = Fake::start_at("sess-journal", newest - 1, newest - 1);
    let mut options = options();
    options.session_dir = Some(journal.path().to_path_buf());
    let harness = Harness::start(options, fake);
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({
        "type": "tool_start",
        "id": "call_1",
        "name": "Bash",
    }));
    harness.fake.event(json!({
        "type": "tool_call_ready",
        "id": "call_1",
        "name": "Bash",
        "arguments": "{\"command\":\"run it\"}",
    }));
    harness.fake.event(json!({
        "type": "tool_result",
        "id": "call_1",
        "bytes": 29,
    }));
    harness
        .fake
        .event(json!({ "type": "committed", "record": newest }));
    wait_text(&harness.bytes, "12 lines").await;
    let screen = text(&harness.bytes);
    assert!(
        !screen.contains("(29 bytes)"),
        "the row shows what the text the journal recorded says, not its size:\n{screen}"
    );
    assert!(
        !screen.contains("the recorded output") && !screen.contains("line 2"),
        "the output's text stays behind the row:\n{screen}"
    );
}

#[tokio::test]
async fn a_tool_result_shows_the_tail_its_frame_carries() {
    // The journal holds one line for the call and the frame ten more before its
    // tail: the row counts what the frame says, so the journal was not read.
    let journal = tempfile::tempdir().expect("a temp folder");
    let newest = write_messages(
        journal.path(),
        &[
            Message::user("run it"),
            Message::ToolResult(reagent_ai::ToolResult {
                call_id: "call_1".into(),
                content: "the recorded output".into(),
                is_error: false,
                images: Vec::new(),
            }),
        ],
    );
    let fake = Fake::start_at("sess-tail", newest - 1, newest - 1);
    let mut options = options();
    options.session_dir = Some(journal.path().to_path_buf());
    let harness = Harness::start(options, fake);
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({
        "type": "tool_start",
        "id": "call_1",
        "name": "Bash",
    }));
    harness.fake.event(json!({
        "type": "tool_call_ready",
        "id": "call_1",
        "name": "Bash",
        "arguments": "{\"command\":\"run it\"}",
    }));
    harness.fake.event(json!({
        "type": "tool_result",
        "id": "call_1",
        "bytes": 200,
        "output": "the eleventh line\nthe twelfth line\n",
        "earlier": 10,
    }));
    harness
        .fake
        .event(json!({ "type": "committed", "record": newest }));
    wait_text(&harness.bytes, "12 lines").await;
    let screen = text(&harness.bytes);
    assert!(
        !screen.contains("1 line") && !screen.contains("(200 bytes)"),
        "the row counts the tail the frame carried, not the journal's one line:\n{screen}"
    );
}

#[tokio::test]
async fn a_run_started_elsewhere_shows_running_and_esc_cancels_it() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    // No user message of this client: the run started elsewhere, or the message
    // it runs was queued.
    harness
        .fake
        .event(json!({ "type": "turn_start", "turn": 1 }));
    wait_text(&harness.bytes, "working").await;
    assert_eq!(harness.fake.expect_method().await, "initialize");
    assert_eq!(harness.fake.expect_method().await, "attach");
    harness.press(KeyCode::Esc);
    assert_eq!(harness.fake.expect_method().await, "cancel");
    harness
        .fake
        .event(json!({ "type": "run_end", "reason": "end_turn" }));
    wait_text(&harness.bytes, "idle").await;
    // A queued run starts after the first one and sets the run going again.
    harness
        .fake
        .event(json!({ "type": "turn_start", "turn": 2 }));
    wait_text(&harness.bytes, "working").await;
}

#[tokio::test]
async fn a_record_committed_between_the_handshake_and_the_attach_shows() {
    let journal = tempfile::tempdir().expect("a temp folder");
    let newest = write_messages(
        journal.path(),
        &[
            Message::user("the first question"),
            Message::assistant(AssistantMessage::new(
                "fake",
                "fake-model",
                vec![Content::Text {
                    text: Text::from("the first reply"),
                }],
            )),
            Message::user("the second question"),
            Message::assistant(AssistantMessage::new(
                "fake",
                "fake-model",
                vec![Content::Text {
                    text: Text::from("the second reply"),
                }],
            )),
        ],
    );
    // The handshake names an older record than the attach answer: the records
    // in between are in neither the replay nor a read up to the handshake.
    let fake = Fake::start_at("sess-hist", newest - 2, newest);
    let mut options = options();
    options.session_dir = Some(journal.path().to_path_buf());
    let harness = Harness::start(options, fake);
    wait_text(&harness.bytes, "the second question").await;
    wait_text(&harness.bytes, "the second reply").await;
}

#[tokio::test]
async fn a_stray_answer_key_goes_to_the_editor_and_a_repeat_waits() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    for id in [11, 12] {
        harness.fake.notify(
            "approval_request",
            &json!({
                "id": id,
                "tool": "Bash",
                "call": "make",
                "rule": "default",
            }),
        );
    }
    wait_text(&harness.bytes, "Allow once").await;
    // A key the user was already typing is text, even after the box is armed.
    harness.type_text("hi");
    tokio::time::sleep(super::APPROVAL_ARMED + Duration::from_millis(50)).await;
    harness.press(KeyCode::Char('y'));
    wait_text(&harness.bytes, "hiy").await;
    wait_text(&harness.bytes, "Allow once").await;
    // The editor now holds text, so the box takes no key at all.
    harness.press(KeyCode::Char('y'));
    wait_text(&harness.bytes, "hiyy").await;
    assert!(
        text(&harness.bytes).contains("Allow once"),
        "no answer went out"
    );
}

#[tokio::test]
async fn a_key_repeat_does_not_answer_the_next_box() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    for id in [21, 22] {
        harness.fake.notify(
            "approval_request",
            &json!({
                "id": id,
                "tool": "Bash",
                "call": "make",
                "rule": "default",
            }),
        );
    }
    wait_text(&harness.bytes, "Allow once").await;
    armed().await;
    harness.press(KeyCode::Char('y'));
    let (id, answer, _) = harness.fake.expect_approve().await;
    assert_eq!(id, 21);
    assert_eq!(answer, AnswerKind::Once);
    // The next box is on screen at once, and a repeated key lands too early to
    // answer it: it becomes a letter of the editor instead.
    harness.press(KeyCode::Char('y'));
    tokio::time::sleep(Duration::from_millis(100)).await;
    wait_text(&harness.bytes, "Allow once").await;
    let screen = text(&harness.bytes);
    assert!(
        screen
            .lines()
            .any(|row| row.trim_start_matches('\u{258e}').trim() == "y"),
        "the repeat went to the editor's text row:\n{screen}"
    );
    harness.press(KeyCode::Backspace);
    armed().await;
    harness.press(KeyCode::Char('y'));
    let (id, _, _) = harness.fake.expect_approve().await;
    assert_eq!(id, 22, "a key after the wait answers the next box");
}

#[tokio::test]
async fn a_long_call_is_cut_with_a_marker() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    let call = "x".repeat(200);
    harness.fake.notify(
        "approval_request",
        &json!({
            "id": 31,
            "tool": "Bash",
            "call": call,
            "rule": "default",
        }),
    );
    wait_text(&harness.bytes, "\u{2026}").await;
    let screen = text(&harness.bytes);
    assert!(
        screen
            .lines()
            .all(|row| unicode_width::UnicodeWidthStr::width(row) <= usize::from(COLUMNS)),
        "the box stays inside the terminal:\n{screen}"
    );
}

#[tokio::test]
async fn a_reply_with_escape_sequences_keeps_the_screen() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({
        "type": "text_delta",
        "text": "alpha\n\n\u{1b}[2Jbeta\tgamma\n",
    }));
    harness
        .fake
        .event(json!({ "type": "run_end", "reason": "end_turn" }));
    wait_text(&harness.bytes, "beta gamma").await;
    let screen = text(&harness.bytes);
    assert!(
        screen.contains("alpha"),
        "the control sequence cleared no line:\n{screen}"
    );
    assert!(
        screen.contains("idle"),
        "the live area is still drawn:\n{screen}"
    );
}

#[tokio::test]
async fn typing_a_key_clears_the_ctrl_c_hint() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    wait_text(&harness.bytes, "Ctrl+C again to quit").await;
    harness.type_text("x");
    wait_gone(&harness.bytes, "Ctrl+C again to quit").await;
}

#[tokio::test]
async fn a_closing_with_another_reason_says_why_and_exits_with_one() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness
        .fake
        .notify("closing", &json!({ "reason": "shutdown" }));
    wait_text(&harness.bytes, "the worker closed the session: shutdown").await;
    let exit = timeout(WAIT, harness.handle)
        .await
        .expect("the app ends")
        .expect("the app does not panic")
        .expect("the app does not fail");
    assert_eq!(exit, Exit { code: 1 });
}

#[tokio::test]
async fn a_cut_client_resyncs_and_shows_the_reply_once() {
    // The first connection streams part of a reply, then cuts the app with the
    // reason `behind`. The second names a later record, and sends the open
    // reply whole as one catch-up frame before its attach answer, then a live
    // delta, as the worker does.
    let (first, talk) = Link::new(1, Vec::new(), Vec::new());
    let catch_up: Vec<u8> = [
        event(json!({ "type": "turn_start" })),
        event(json!({ "type": "text_delta", "text": "The quick " })),
        event(json!({ "type": "text_delta", "text": "brown fox" })),
    ]
    .concat();
    let (second, _) = Link::new(
        2,
        catch_up,
        event(json!({ "type": "text_delta", "text": " jumps" })),
    );
    let mut worker = Cutting::start("sess-cut", vec![first, second]);
    let harness = Harness::new_session(worker.fake());
    wait_text(&harness.bytes, "idle").await;
    assert_eq!(worker.expect_method().await, "initialize");
    assert_eq!(worker.expect_method().await, "attach");

    for delta in ["The quick ", "brown fox"] {
        let _ = talk.send(event(json!({ "type": "text_delta", "text": delta })));
    }
    wait_text(&harness.bytes, "The quick brown fox").await;
    // The worker cut this client off mid-reply, and closed its connection.
    let _ = talk.send(closing("behind"));
    drop(talk);

    // The app resyncs by itself: same socket, same token, one more handshake.
    assert_eq!(worker.expect_method().await, "initialize");
    assert_eq!(worker.expect_method().await, "attach");
    wait_text(&harness.bytes, "The quick brown fox jumps").await;
    wait_drawn(&harness.bytes, "resynced").await;

    let screen = text(&harness.bytes);
    assert_eq!(
        screen.matches("brown fox").count(),
        1,
        "the reply shows once, in order:\n{screen}"
    );
    assert!(
        !screen.contains("cut off by the worker"),
        "a resync says nothing in the transcript:\n{screen}"
    );
    assert!(!harness.handle.is_finished(), "the app keeps running");
}

#[tokio::test]
async fn a_resync_whose_attach_answer_says_no_run_is_open_shows_idle() {
    // The run ends while this client is cut, in a record the worker commits:
    // the frames it replays hold no `run_end`, so the answer to `attach` is
    // what says the run is over.
    let (first, talk) = Link::new(1, Vec::new(), Vec::new());
    let (second, _) = Link::new(2, Vec::new(), Vec::new());
    let second = second.running(false);
    let mut worker = Cutting::start("sess-open", vec![first, second]);
    let harness = Harness::new_session(worker.fake());
    wait_text(&harness.bytes, "idle").await;
    assert_eq!(worker.expect_method().await, "initialize");
    assert_eq!(worker.expect_method().await, "attach");

    let _ = talk.send(event(json!({ "type": "turn_start" })));
    wait_text(&harness.bytes, "working").await;
    // Cut mid-run, and the run ended before this client came back.
    let _ = talk.send(closing("behind"));
    drop(talk);

    assert_eq!(worker.expect_method().await, "initialize");
    assert_eq!(worker.expect_method().await, "attach");
    wait_drawn(&harness.bytes, "resynced").await;
    let screen = text(&harness.bytes);
    let status = screen
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .expect("a status line");
    assert!(
        status.contains("idle"),
        "the run that ended while the client was away left the status {status:?}:\n{screen}"
    );
    assert!(!harness.handle.is_finished(), "the app keeps running");
}

#[tokio::test]
async fn a_resync_with_nothing_to_replay_keeps_the_reply() {
    // The worker committed the reply before this client came back, so its
    // catch-up frame is empty: the text on screen stays, once.
    let part = [
        event(json!({ "type": "turn_start" })),
        event(json!({ "type": "text_delta", "text": "The quick " })),
        event(json!({ "type": "text_delta", "text": "brown fox" })),
    ]
    .concat();
    let (first, _) = Link::new(1, Vec::new(), part);
    let (second, _) = Link::new(2, Vec::new(), Vec::new());
    let mut worker = Cutting::start("sess-cut", vec![first.closes(), second]);
    let harness = Harness::new_session(worker.fake());
    for _ in 0..2 {
        assert_eq!(worker.expect_method().await, "initialize");
        assert_eq!(worker.expect_method().await, "attach");
    }
    wait_drawn(&harness.bytes, "resynced").await;
    let screen = text(&harness.bytes);
    assert_eq!(
        screen.matches("brown fox").count(),
        1,
        "the reply shows once, in order:\n{screen}"
    );
    assert!(!harness.handle.is_finished(), "the app keeps running");
}

/// A journal of one message and the reply that answers it, in `dir`, with the
/// number of its newest record.
fn journal_of(dir: &Path, message: &str, reply: &str) -> (PathBuf, u64) {
    let path = dir.join("sess-cut");
    let newest = write_messages(
        &path,
        &[
            Message::user(message),
            Message::assistant(AssistantMessage::new(
                "fake",
                "fake-model",
                vec![Content::Text {
                    text: Text::from(reply),
                }],
            )),
        ],
    );
    (path, newest)
}

#[tokio::test]
async fn a_resync_draws_the_message_the_journal_holds_once() {
    // The message and the reply reached this client as live frames before the
    // worker cut it. By the time it comes back the journal holds both of them,
    // so the read of the records it missed finds messages that are already on
    // screen, and draws neither of them a second time.
    let dir = tempfile::Builder::new()
        .prefix("tui")
        .tempdir_in("/tmp")
        .expect("a temp folder in /tmp");
    let (journal, newest) = journal_of(dir.path(), "go", "the quick brown fox");
    let (first, talk) = Link::new(0, Vec::new(), Vec::new());
    let (second, _) = Link::new(newest, Vec::new(), Vec::new());
    let mut worker = Cutting::start("sess-cut", vec![first, second.running(false)]);
    let mut options = options();
    options.session_dir = Some(journal);
    let harness = Harness::start(options, worker.fake());
    wait_text(&harness.bytes, "idle").await;
    assert_eq!(worker.expect_method().await, "initialize");
    assert_eq!(worker.expect_method().await, "attach");

    let _ = talk.send(event(json!({ "type": "turn_start" })));
    let _ = talk.send(event(json!({
        "type": "user_message",
        "client": 1,
        "mode": "queue",
        "text": "go",
    })));
    let _ = talk.send(event(json!({
        "type": "text_delta",
        "text": "the quick brown fox",
    })));
    wait_text(&harness.bytes, "brown fox").await;
    // The run ended while this client was away, so no frame says so.
    let _ = talk.send(closing("behind"));
    drop(talk);

    assert_eq!(worker.expect_method().await, "initialize");
    assert_eq!(worker.expect_method().await, "attach");
    wait_drawn(&harness.bytes, "resynced").await;
    let screen = whole(&harness.bytes);
    assert_eq!(
        screen.matches("› go").count(),
        1,
        "the message the journal holds is on screen once:\n{screen}"
    );
    assert_eq!(
        screen.matches("brown fox").count(),
        1,
        "the finished reply stands in for the one that streamed here:\n{screen}"
    );
    assert!(!harness.handle.is_finished(), "the app keeps running");
}

#[tokio::test]
async fn a_resync_that_commits_a_reply_that_filled_the_screen_keeps_the_editor_under_it() {
    // The reply the worker replays needs every row of the screen, so the live
    // area takes them. When the record that holds it commits, the rows the area
    // gives back leave the editor and the status on the screen instead of above
    // it.
    let dir = tempfile::Builder::new()
        .prefix("tui")
        .tempdir_in("/tmp")
        .expect("a temp folder in /tmp");
    let reply = format!("{}the-end", "w".repeat(2_000));
    let (journal, newest) = journal_of(dir.path(), "go", &reply);
    let (first, talk) = Link::new(0, Vec::new(), Vec::new());
    let (second, _) = Link::new(newest, Vec::new(), Vec::new());
    let mut worker = Cutting::start("sess-cut", vec![first, second.running(false)]);
    let mut options = options();
    options.session_dir = Some(journal);
    let harness = Harness::start(options, worker.fake());
    wait_text(&harness.bytes, "idle").await;
    assert_eq!(worker.expect_method().await, "initialize");
    assert_eq!(worker.expect_method().await, "attach");

    let _ = talk.send(event(json!({
        "type": "user_message",
        "client": 1,
        "mode": "queue",
        "text": "go",
    })));
    let _ = talk.send(event(json!({
        "type": "text_delta",
        "text": "w".repeat(2_000),
    })));
    wait_text(&harness.bytes, "wwww").await;
    let _ = talk.send(closing("behind"));
    drop(talk);

    assert_eq!(worker.expect_method().await, "initialize");
    assert_eq!(worker.expect_method().await, "attach");
    wait_drawn(&harness.bytes, "resynced").await;
    wait_text(&harness.bytes, "the-end").await;
    wait_text(&harness.bytes, "idle").await;
    let screen = text(&harness.bytes);
    let rows: Vec<&str> = screen.lines().collect();
    let last = usize::from(ROWS) - 1;
    assert!(
        rows[last].contains("idle"),
        "the status line is on the last row:\n{screen}"
    );
    assert!(
        rows[last - 1].trim().is_empty(),
        "the editor is the row right above it:\n{screen}"
    );
    let all = whole(&harness.bytes);
    assert_eq!(
        all.matches("› go").count(),
        1,
        "the message shows once:\n{all}"
    );
    assert_eq!(
        all.matches("the-end").count(),
        1,
        "the reply shows once:\n{all}"
    );
    assert!(!harness.handle.is_finished(), "the app keeps running");
}

#[tokio::test]
async fn a_connection_that_closes_resyncs_when_the_worker_is_there() {
    // A cut found in the middle of a frame says nothing: the worker just closes
    // the connection. The same socket answers again, so the app resyncs as it
    // does after a notice.
    let part = [
        event(json!({ "type": "turn_start" })),
        event(json!({ "type": "text_delta", "text": "The quick " })),
        event(json!({ "type": "text_delta", "text": "brown fox" })),
    ]
    .concat();
    let (first, _) = Link::new(1, Vec::new(), part.clone());
    let (second, _) = Link::new(
        2,
        part,
        event(json!({ "type": "text_delta", "text": " jumps" })),
    );
    let mut worker = Cutting::start("sess-cut", vec![first.closes(), second]);
    let harness = Harness::new_session(worker.fake());
    for _ in 0..2 {
        assert_eq!(worker.expect_method().await, "initialize");
        assert_eq!(worker.expect_method().await, "attach");
    }
    wait_text(&harness.bytes, "The quick brown fox jumps").await;
    wait_drawn(&harness.bytes, "resynced").await;
    let screen = text(&harness.bytes);
    assert_eq!(
        screen.matches("brown fox").count(),
        1,
        "the reply shows once, in order:\n{screen}"
    );
    assert!(!harness.handle.is_finished(), "the app keeps running");
}

#[tokio::test]
async fn a_connection_that_closes_with_no_worker_left_says_the_session_ended() {
    // The reconnection finds nothing behind the socket: the session ended.
    let (first, _) = Link::new(1, Vec::new(), Vec::new());
    let worker = Cutting::start("sess-cut", vec![first.closes()]);
    let harness = Harness::new_session(worker.fake());
    wait_text(&harness.bytes, "the session ended").await;
    let exit = timeout(WAIT, harness.handle)
        .await
        .expect("the app ends")
        .expect("the app does not panic")
        .expect("the app does not fail");
    assert_eq!(exit, Exit { code: 0 });
}

#[tokio::test]
async fn six_cuts_in_a_row_end_the_app_with_one() {
    let links = (0..6)
        .map(|_| Link::new(0, Vec::new(), closing("behind")).0)
        .collect::<Vec<Link>>();
    let mut worker = Cutting::start("sess-cut", links);
    let harness = Harness::new_session(worker.fake());
    // Five cuts resync; the sixth is one too many, and the app stops there.
    for _ in 0..6 {
        assert_eq!(worker.expect_method().await, "initialize");
        assert_eq!(worker.expect_method().await, "attach");
    }
    wait_text(
        &harness.bytes,
        "cut off by the worker again and again: run reagent -c to continue",
    )
    .await;
    let exit = timeout(WAIT, harness.handle)
        .await
        .expect("the app ends")
        .expect("the app does not panic")
        .expect("the app does not fail");
    assert_eq!(exit, Exit { code: 1 });
}

#[tokio::test]
async fn an_event_that_changes_nothing_writes_no_frame() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    let before = harness
        .bytes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    harness
        .fake
        .event(json!({ "type": "committed", "record": 0 }));
    tokio::time::sleep(super::FRAME * 3).await;
    let after = harness
        .bytes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    assert_eq!(after, before, "a frame that draws nothing is not written");
}

#[tokio::test]
async fn the_cursor_after_exit_sits_under_the_last_committed_line() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({
        "type": "text_delta",
        "text": "the last line\n",
    }));
    harness
        .fake
        .event(json!({ "type": "run_end", "reason": "end_turn" }));
    wait_text(&harness.bytes, "the last line").await;
    wait_text(&harness.bytes, "idle").await;
    harness.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    harness.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    let _ = timeout(WAIT, harness.handle)
        .await
        .expect("the app ends")
        .expect("the app does not panic")
        .expect("the app does not fail");

    let mut parser = vt100::Parser::new(nonzero(ROWS), nonzero(COLUMNS), SCROLLBACK);
    let held = harness
        .bytes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    parser.process(&held);
    let rows: Vec<String> = parser.screen().rows(0, COLUMNS).collect();
    let last = rows
        .iter()
        .rposition(|row| !row.trim().is_empty())
        .expect("the transcript");
    assert!(
        rows[last].contains("tokens") && rows[last - 2].contains("the last line"),
        "the run's rule closes the transcript under its reply: {rows:?}"
    );
    let (row, column) = parser.screen().cursor_position();
    assert_eq!(column, 0, "the cursor starts the line");
    assert_eq!(
        usize::from(row),
        rows.len() - ROWS as usize + last + 1,
        "the cursor sits on the first live row, right under the transcript:\n{rows:?}"
    );
}

/// A terminal whose input fails at once.
struct BrokenTerminal {
    size: (u16, u16),
}

impl Terminal for BrokenTerminal {
    fn size(&self) -> (u16, u16) {
        self.size
    }

    async fn write_frame(&mut self, _frame: &[u8]) -> io::Result<()> {
        Ok(())
    }

    async fn next_event(&mut self) -> io::Result<Event> {
        Err(io::Error::other("the input thread stopped"))
    }
}

#[tokio::test]
async fn a_terminal_read_error_ends_the_app() {
    let fake = Fake::start("sess", 0);
    let mut options = options();
    options.socket = fake.socket.clone();
    let terminal = BrokenTerminal {
        size: (COLUMNS, ROWS),
    };
    let exit = timeout(WAIT, run(options, terminal))
        .await
        .expect("the app ends instead of spinning on the failed read")
        .expect("the app does not fail");
    assert_eq!(exit, Exit { code: 0 });
}

#[test]
fn a_signal_ends_the_app() {
    let mut app = super::App::new(&super::Initialized::default(), (COLUMNS, ROWS), false);
    super::signal_end(&mut app);
    assert!(app.exit, "a terminate or a hangup ends the app");
}

#[tokio::test]
async fn the_signal_watcher_fires_on_no_signal_of_its_own() {
    // A real SIGTERM would reach every app in a test process that runs its
    // tests in parallel threads, so the watcher is only checked to stay quiet:
    // the handler it calls is tested above.
    let mut signals = super::Signals::new();
    assert!(
        timeout(Duration::from_millis(50), signals.recv())
            .await
            .is_err(),
        "the watcher fires only on a signal"
    );
}

#[test]
fn the_start_of_a_long_history_stays_quick() {
    // What the app did before the ring: every message of 5,000 rendered at
    // start, then the newest 200 only.
    let messages: Vec<Message> = (0..5_000)
        .map(|index| {
            if index % 2 == 0 {
                Message::user(format!("question {index} of the session, a sentence long"))
            } else {
                Message::assistant(AssistantMessage::new(
                    "fake",
                    "fake-model",
                    vec![Content::Text {
                        text: Text::from(format!(
                            "answer {index}: a paragraph that the markdown renderer lays out \
                             over a handful of rows of the eighty-column terminal."
                        )),
                    }],
                ))
            }
        })
        .collect();
    let mut app = super::App::new(&super::Initialized::default(), (COLUMNS, ROWS), false);

    let started = std::time::Instant::now();
    let all = super::Tail {
        messages: messages.iter().cloned().collect(),
        checkpoints: Vec::new(),
        calls: Vec::new(),
        omitted: 0,
    };
    app.history(&all);
    eprintln!(
        "history before the ring: {:?} for {} messages",
        started.elapsed(),
        all.messages.len()
    );

    let mut app = super::App::new(&super::Initialized::default(), (COLUMNS, ROWS), false);
    let started = std::time::Instant::now();
    let tail = super::Tail {
        messages: messages
            .into_iter()
            .skip(5_000 - crate::history::KEEP)
            .collect(),
        checkpoints: Vec::new(),
        calls: Vec::new(),
        omitted: 5_000 - crate::history::KEEP,
    };
    app.history(&tail);
    eprintln!(
        "history with the ring: {:?} for {} messages",
        started.elapsed(),
        tail.messages.len()
    );
}

#[tokio::test]
async fn the_terminal_size_is_read_once_not_per_draw() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    for step in 0..5 {
        harness.fake.event(json!({
            "type": "text_delta",
            "text": format!("line {step}\n"),
        }));
        wait_text(&harness.bytes, &format!("line {step}")).await;
    }
    assert_eq!(
        harness.sizes.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the size comes from the startup and the resize events, not from a draw"
    );
}

#[test]
fn a_history_that_left_messages_out_shows_one_dim_line() {
    let mut app = super::App::new(&super::Initialized::default(), (COLUMNS, ROWS), false);
    let tail = super::Tail {
        messages: std::collections::VecDeque::from([
            Message::user("the last question"),
            Message::assistant(AssistantMessage::new(
                "fake",
                "fake-model",
                vec![Content::Text {
                    text: Text::from("the last reply"),
                }],
            )),
        ]),
        checkpoints: Vec::new(),
        calls: Vec::new(),
        omitted: 50,
    };
    app.history(&tail);
    app.commit();
    let mut out = Vec::new();
    app.screen.diff(&mut out);
    let written = String::from_utf8_lossy(&out).into_owned();
    assert!(
        written.contains("... 50 earlier messages"),
        "one dim line says what was left out: {written:?}"
    );
    assert!(
        written.contains("›")
            && written.contains("the last question")
            && written.contains("the last reply"),
        "the tail of the history renders: {written:?}"
    );
}

/// The tail of a session whose reply called `edit` on `src/a.rs`, and the
/// result that answered it, failed when `is_error`.
fn edit_tail(is_error: bool) -> super::Tail {
    let call = ToolCall {
        id: Text::from("call-1"),
        name: Text::from("edit"),
        arguments: Text::from(
            r#"{"path":"src/a.rs","old_string":"one\n","new_string":"one\ntwo\n"}"#,
        ),
    };
    super::Tail {
        messages: std::collections::VecDeque::from([
            Message::assistant(AssistantMessage::new(
                "fake",
                "fake-model",
                vec![Content::ToolCall(call.clone())],
            )),
            Message::ToolResult(reagent_ai::ToolResult {
                call_id: Text::from("call-1"),
                content: Text::from(if is_error {
                    "the file is locked"
                } else {
                    "two lines are in src/a.rs"
                }),
                is_error,
                images: Vec::new(),
            }),
        ]),
        checkpoints: Vec::new(),
        calls: vec![call],
        omitted: 0,
    }
}

#[test]
fn history_tool_row_has_its_name_and_target() {
    let mut app = super::App::new(&super::Initialized::default(), (COLUMNS, ROWS), true);
    app.history(&edit_tail(false));
    app.commit();
    let screen = painted(&mut app);
    assert!(
        screen.lines().any(|row| row.contains("← Edit src/a.rs")),
        "the card keeps the call's name and its target:\n{screen}"
    );
    assert!(
        !screen.contains('✗'),
        "a successful result draws no cross:\n{screen}"
    );
}

#[test]
fn history_tool_row_failed_only_on_an_error_result() {
    let mut app = super::App::new(&super::Initialized::default(), (COLUMNS, ROWS), true);
    app.history(&edit_tail(false));
    app.commit();
    let screen = painted(&mut app);
    assert!(
        !screen.contains(" error ") && !screen.contains('✗'),
        "a result that is no error draws no chip:\n{screen}"
    );

    let mut app = super::App::new(&super::Initialized::default(), (COLUMNS, ROWS), true);
    app.history(&edit_tail(true));
    app.commit();
    let screen = painted(&mut app);
    assert!(
        screen.lines().any(|row| row.contains("✗ ← Edit src/a.rs")),
        "a failed result keeps the call's title behind the cross:\n{screen}"
    );
    assert!(
        screen.contains("the file is locked"),
        "the body of the card is the error:\n{screen}"
    );
}

/// The text at the right edge of the row of `name`, called with `arguments`,
/// after it printed `output`.
fn right_of(name: &str, arguments: &str, output: &str) -> String {
    let mut tool = super::Tool::new("call-1", name);
    tool.set_call(name, arguments, COLUMNS);
    tool.set_output(output, COLUMNS);
    tool.right[0]
        .as_ref()
        .map_or_else(String::new, |span| span.text.clone())
}

#[test]
fn short_result_of_a_read_is_its_line_count() {
    let lines = "let x = 1;\n".repeat(12);
    assert_eq!(
        right_of("read", r#"{"path":"src/a.rs"}"#, &lines),
        "12 lines"
    );
    assert_eq!(
        right_of("read", r#"{"path":"src/a.rs"}"#, "1\tfn main() {}\n"),
        "1 line"
    );
}

#[test]
fn short_result_of_a_grep_counts_matches() {
    let matches = "src/a.rs:1:fn one\nsrc/b.rs:2:fn two\nsrc/c.rs:3:fn three\n";
    assert_eq!(
        right_of("grep", r#"{"pattern":"fn"}"#, matches),
        "3 matches"
    );
    assert_eq!(
        right_of("grep", r#"{"pattern":"zzz"}"#, ""),
        "no match",
        "a search that found nothing says so"
    );
    assert_eq!(
        right_of("find", r#"{"pattern":"*.rs"}"#, "src/a.rs\nsrc/b.rs\n"),
        "2 files"
    );
}

#[test]
fn short_result_of_a_failed_bash_is_its_exit() {
    let failed = "error: one test failed\nexit code: 1";
    assert_eq!(
        right_of("bash", r#"{"command":"cargo test"}"#, failed),
        "exit 1"
    );
    let ran = "hi\nexit code: 0";
    assert_eq!(
        right_of("bash", r#"{"command":"echo hi"}"#, ran),
        "1 line",
        "a command that succeeded says how much it printed"
    );
}

#[test]
fn short_result_never_shows_raw_output() {
    let output = "the first line of the output\nand the second line of it\n";
    for (name, arguments) in [
        ("read", r#"{"path":"src/a.rs"}"#),
        ("grep", r#"{"pattern":"fn"}"#),
        ("find", r#"{"pattern":"*.rs"}"#),
        ("bash", r#"{"command":"cargo build"}"#),
        ("edit", r#"{"path":"src/a.rs"}"#),
        ("write", r#"{"path":"src/a.rs"}"#),
    ] {
        let right = right_of(name, arguments, output);
        assert!(
            !right.contains("the output"),
            "{name} keeps the output out of its row: {right:?}"
        );
        assert!(
            UnicodeWidthStr::width(right.as_str()) <= 24,
            "{name} keeps its result short: {right:?}"
        );
    }
}

/// A `read` call whose result was `output`, as the transcript holds it.
fn with_output(output: &str) -> super::Tool {
    let mut tool = super::Tool::new("call-1", "read");
    tool.set_call("read", r#"{"path":"src/a.rs"}"#, COLUMNS);
    tool.set_output(output, COLUMNS);
    tool
}

/// 16 KiB of 100-byte lines: more than a call's row keeps of its output.
fn long_output() -> String {
    format!("{}\n", "x".repeat(99)).repeat(164)
}

#[test]
fn output_tail_keeps_at_most_4_kib_from_a_line_start() {
    let output = long_output();
    let tool = with_output(&output);
    let text = tool.text.as_deref().expect("the output it kept");
    assert!(
        text.len() <= 4096,
        "the call keeps at most 4 KiB of its output, it kept {}",
        text.len()
    );
    assert!(
        output.ends_with(text),
        "the tail ends where the output ends"
    );
    let at = output.len() - text.len();
    assert_eq!(
        output.as_bytes()[at - 1],
        b'\n',
        "the tail starts at the start of a line"
    );
}

#[test]
fn output_tail_keeps_the_whole_line_count_in_the_right_span() {
    let output = long_output();
    let tool = with_output(&output);
    assert_eq!(tool.lines, 164, "the count is of the whole output");
    assert_eq!(
        right_of("read", r#"{"path":"src/a.rs"}"#, &output),
        "164 lines",
        "the row names the lines the whole output has"
    );
}

#[test]
fn output_tail_counts_the_lines_before_the_tail() {
    let output = long_output();
    let tool = with_output(&output);
    let kept = tool.text.as_deref().expect("the output it kept");
    let lines = kept.lines().count() as u64;
    assert_eq!(tool.earlier, 164 - lines, "the lines the cut takes off");
    assert_eq!(
        tool.earlier + lines,
        tool.lines,
        "the lines cut off and the lines kept are the whole output"
    );
}

#[test]
fn output_tail_keeps_a_short_output_whole() {
    let output = "let x = 1;\nlet y = 2;\n";
    let tool = with_output(output);
    assert_eq!(
        tool.text.as_deref(),
        Some(output),
        "an output under the bound is kept whole"
    );
    assert_eq!(tool.earlier, 0, "nothing was cut off");
    assert_eq!(tool.lines, 2);
}

#[test]
fn output_tail_starts_at_a_char_boundary_without_a_line_start() {
    let output = "é".repeat(4096);
    let tool = with_output(&output);
    assert_eq!(
        tool.text.as_deref(),
        Some("é".repeat(2048).as_str()),
        "one long line keeps a whole number of characters"
    );
    assert_eq!(tool.earlier, 0, "no line was cut off before the tail");
}

/// Full screen draws in the alternate screen, captures the mouse and lays the
/// transcript out over the terminal, with the editor and the status line as its
/// last two rows.
#[tokio::test]
async fn full_screen_enters_the_alternate_screen_and_captures_the_mouse() {
    let harness = Harness::start(full(), Fake::start("sess-abcd1234", 0));
    wait_text(&harness.bytes, "idle").await;
    let drawn = raw(&harness.bytes);
    assert!(
        drawn.starts_with("\u{1b}[?1049h"),
        "the run starts on the alternate screen: {:?}",
        &drawn[..drawn.len().min(40)]
    );
    assert!(
        drawn.contains("\u{1b}[?1000h"),
        "the mouse is captured, so a wheel reaches the app"
    );
    let screen = text(&harness.bytes);
    let rows: Vec<&str> = screen.lines().collect();
    assert_eq!(
        rows.len(),
        usize::from(ROWS),
        "the whole terminal: {screen:?}"
    );
    assert!(
        rows[usize::from(ROWS) - 1].contains("idle"),
        "the status line is the last row:\n{screen}"
    );
    assert!(
        rows[usize::from(ROWS) - 2].trim().is_empty(),
        "the empty editor sits right above it:\n{screen}"
    );
}

/// `count` lines, each its own paragraph, so the markdown renderer gives every
/// one of them its own row.
fn paragraphs(count: usize) -> String {
    let mut text = String::with_capacity(count * 9);
    for n in 1..=count {
        text.push_str("line ");
        text.push_str(&n.to_string());
        text.push_str("\n\n");
    }
    text
}

/// A terminal that refuses the frame that enters the alternate screen keeps the
/// inline mode, which draws on the normal screen, and says why.
#[tokio::test]
async fn a_terminal_that_refuses_the_alternate_screen_falls_back_to_inline() {
    /// A terminal that takes no frame at all.
    struct Refuses;

    impl Terminal for Refuses {
        fn size(&self) -> (u16, u16) {
            (COLUMNS, ROWS)
        }

        async fn write_frame(&mut self, _frame: &[u8]) -> io::Result<()> {
            Err(io::Error::other("no alternate screen"))
        }

        async fn next_event(&mut self) -> io::Result<Event> {
            std::future::pending().await
        }
    }

    let mut app = super::App::new(&super::Initialized::default(), (COLUMNS, ROWS), true);
    assert!(app.screen.is_full(), "the mode the start chose");
    app.enter(&mut Refuses).await;
    assert!(
        !app.screen.is_full(),
        "a terminal that refuses the switch keeps the inline mode"
    );
    let mut out = Vec::new();
    app.commit();
    app.paint();
    app.screen.diff(&mut out);
    let drawn = String::from_utf8_lossy(&out).into_owned();
    assert!(
        drawn.contains("full screen: no alternate screen"),
        "the note says why it fell back: {drawn:?}"
    );
}

/// A terminal that refuses the alternate screen at 160 columns keeps the inline
/// mode, and the sidebar the full-screen layout docked goes with it: inline mode
/// draws no sidebar, so none of its cells stay on screen.
#[tokio::test]
async fn a_refused_full_screen_drops_the_docked_sidebar() {
    /// A terminal that takes no frame at all.
    struct Refuses;

    impl Terminal for Refuses {
        fn size(&self) -> (u16, u16) {
            (160, ROWS)
        }

        async fn write_frame(&mut self, _frame: &[u8]) -> io::Result<()> {
            Err(io::Error::other("no alternate screen"))
        }

        async fn next_event(&mut self) -> io::Result<Event> {
            std::future::pending().await
        }
    }

    let mut app = super::App::new(&super::Initialized::default(), (160, ROWS), true);
    assert!(
        app.aside.rect().is_some(),
        "160 columns dock the sidebar before the switch"
    );
    app.enter(&mut Refuses).await;
    assert!(
        app.aside.rect().is_none(),
        "the inline mode the fallback keeps has no sidebar"
    );
    let mut out = Vec::new();
    app.commit();
    app.paint();
    app.screen.diff(&mut out);
    let drawn = String::from_utf8_lossy(&out).into_owned();
    assert!(
        !drawn.contains('\u{25be}'),
        "no panel title draws in inline mode: {drawn:?}"
    );
    assert_eq!(
        app.text_width(),
        160,
        "the inline rows lay out at the whole terminal's width"
    );
}
#[tokio::test]
async fn five_thousand_lines_page_up_one_page_and_back() {
    let harness = Harness::start(full(), Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": paragraphs(5_000) }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    harness
        .fake
        .event(json!({ "type": "run_end", "reason": "end_turn" }));
    wait_text(&harness.bytes, "idle").await;
    // The viewport follows the newest line, which is the row above the editor.
    wait_row(&harness.bytes, "line 5000").await;
    assert!(
        shows_row(&text(&harness.bytes), "line 4994"),
        "the viewport is at the bottom:\n{}",
        text(&harness.bytes)
    );

    // A page is the rows the transcript shows: the terminal's 24 minus the
    // editor and the status line.
    harness.press(KeyCode::PageUp);
    wait_row(&harness.bytes, "line 4987").await;
    wait_no_row(&harness.bytes, "line 5000").await;
    let page = text(&harness.bytes);
    assert!(
        !shows_row(&page, "line 5000") && !shows_row(&page, "line 4994"),
        "one page up shows the page above, not the bottom:\n{page}"
    );

    harness.press(KeyCode::PageDown);
    wait_row(&harness.bytes, "line 5000").await;
    assert!(
        shows_row(&text(&harness.bytes), "line 4994"),
        "page down is back at the bottom:\n{}",
        text(&harness.bytes)
    );
}

/// Shift+Up moves the viewport one line and Shift+Down gives it back, and output
/// that arrives while the viewport is up leaves it where it is until End.
#[tokio::test]
async fn output_while_scrolled_up_keeps_the_view() {
    let harness = Harness::start(full(), Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": paragraphs(100) }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    harness
        .fake
        .event(json!({ "type": "run_end", "reason": "end_turn" }));
    wait_text(&harness.bytes, "idle").await;
    wait_row(&harness.bytes, "line 100").await;
    let settled = text(&harness.bytes);
    assert!(
        shows_row(&settled, "line 94"),
        "the bottom page:\n{settled}"
    );

    // One row up with Shift+Up, one row back with Shift+Down.
    harness.send(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
    // One row up: line 93 takes the top row, and the newest row, the turn's
    // rule, leaves the view.
    wait_gone(&harness.bytes, "0 tokens").await;
    let up = text(&harness.bytes);
    assert!(
        shows_row(&up, "line 93") && shows_row(&up, "line 99"),
        "the viewport moved off the newest row only:\n{up}"
    );
    harness.send(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
    wait_text(&harness.bytes, "0 tokens").await;

    // A page up, then output arrives: the viewport does not move.
    harness.press(KeyCode::PageUp);
    wait_row(&harness.bytes, "line 87").await;
    let scrolled = text(&harness.bytes);
    harness.fake.event(json!({ "type": "turn_start" }));
    harness.fake.event(json!({
        "type": "text_delta",
        "text": "extra output",
    }));
    // The reply streams into the live row under the transcript, which a scrolled
    // viewport keeps: it shows up there and moves no page. The row is read off
    // the drawn screen: a frame writes the cells it changed, not its text.
    wait_row(&harness.bytes, "extra output").await;
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    harness
        .fake
        .event(json!({ "type": "run_end", "reason": "end_turn" }));
    // The line it leaves is committed into the ring at the bottom, out of sight.
    wait_gone(&harness.bytes, "extra output").await;
    let after = text(&harness.bytes);
    assert!(
        !after.contains("extra output") && shows_row(&after, "line 87"),
        "new output does not move a viewport that was scrolled up:\n{after}"
    );
    // The transcript rows are the ones that were there: nothing above them
    // moved. The rows the viewport shows are the ones the reply's lines hold;
    // the live row and the text box under them are the frame's, not its.
    let transcript = |screen: &str| {
        screen
            .lines()
            .filter(|row| row.starts_with("line "))
            .map(str::to_owned)
            .collect::<Vec<String>>()
    };
    assert_eq!(
        transcript(&after),
        transcript(&scrolled),
        "the transcript keeps the rows it showed:\n{after}"
    );

    // End, with nothing typed, goes back to the bottom and follows again.
    harness.press(KeyCode::End);
    wait_text(&harness.bytes, "extra output").await;
    assert!(
        shows_row(&text(&harness.bytes), "line 100"),
        "the bottom shows the newest lines:\n{}",
        text(&harness.bytes)
    );
}

/// Both modes ask the terminal for its extended keys, so Shift+Enter arrives
/// apart from Enter, and hand the plain keys back on a quit.
#[tokio::test]
async fn a_run_asks_for_the_extended_keys_and_gives_them_back() {
    for options in [options(), full()] {
        let full = options.full;
        let harness = Harness::start(options, Fake::start("sess", 0));
        wait_text(&harness.bytes, "idle").await;
        assert!(
            raw(&harness.bytes).contains("\u{1b}[>4;1m\u{1b}[>5u"),
            "full {full}: modifyOtherKeys and the kitty keys go on"
        );
        harness.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        harness.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        timeout(WAIT, harness.handle)
            .await
            .expect("the app ends")
            .expect("the app does not panic")
            .expect("the app does not fail");
        let drawn = raw(&harness.bytes);
        let on = drawn.rfind("\u{1b}[>5u").expect("the keys went on");
        assert!(
            drawn[on..].contains("\u{1b}[<u\u{1b}[>4;0m"),
            "full {full}: the exit gives the plain keys back"
        );
    }
}

/// A quit leaves the alternate screen and the mouse behind, and prints one
/// line that names the session instead of the transcript.
#[tokio::test]
async fn quit_leaves_the_alternate_screen_with_one_line() {
    let harness = Harness::start(full(), Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": paragraphs(60) }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    harness
        .fake
        .event(json!({ "type": "run_end", "reason": "end_turn" }));
    wait_row(&harness.bytes, "line 60").await;
    harness.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    harness.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    let exit = timeout(WAIT, harness.handle)
        .await
        .expect("the app ends")
        .expect("the app does not panic")
        .expect("the app does not fail");
    assert_eq!(exit, Exit { code: 0 });

    let drawn = raw(&harness.bytes);
    let left = drawn
        .rfind("\u{1b}[?1049l")
        .expect("the run leaves the alternate screen");
    let print = &drawn[left..];
    assert!(
        print.contains("\u{1b}[?1000l"),
        "the mouse capture is released with it"
    );
    // Everything after the sequence that leaves the alternate screen goes to the
    // shell: one line that names the session, and none of the transcript.
    assert!(
        print.contains("session SESS") && print.contains("reagent -c to continue"),
        "the exit line names the session: {print:?}"
    );
    assert!(
        !print.contains("line 60") && !print.contains("line 1\r"),
        "no transcript is printed: {print:?}"
    );
}

/// A resize lays the transcript out again on the new terminal, and no line of it
/// is lost.
#[tokio::test]
async fn a_resize_lays_the_transcript_out_again() {
    let harness = Harness::start(full(), Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": paragraphs(40) }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    harness
        .fake
        .event(json!({ "type": "run_end", "reason": "end_turn" }));
    wait_row(&harness.bytes, "line 40").await;

    // A smaller terminal: the transcript keeps a few of the 12 rows, and it
    // follows its last line again.
    let from = written(&harness.bytes);
    harness.keys.send(Event::Resize(60, 12)).expect("the keys");
    wait_row_at(&harness.bytes, from, 12, 60, "line 40").await;
    let small = drawn_at(&harness.bytes, from, 12, 60);
    let rows: Vec<&str> = small.lines().collect();
    assert_eq!(rows.len(), 12, "the new size:\n{small}");
    assert!(
        rows[11].contains("idle"),
        "the status line is the last row again:\n{small}"
    );
    assert!(
        shows_row(&small, "line 40"),
        "the viewport follows at the new size:\n{small}"
    );
    assert!(
        !shows_row(&small, "line 39"),
        "one of the 40 lines shows:\n{small}"
    );

    // The lines above the viewport are still there: one page up reaches them.
    harness.press(KeyCode::PageUp);
    wait_row_at(&harness.bytes, from, 12, 60, "line 38").await;
    assert!(
        !shows_row(&drawn_at(&harness.bytes, from, 12, 60), "line 40"),
        "the page above is where the ring's earlier lines are"
    );
}

/// The column `needle` starts at on the first row of `screen` that holds it:
/// characters, not bytes, so a `│` or a `▾` before it counts one column.
fn column_of(screen: &str, needle: &str) -> Option<usize> {
    screen
        .lines()
        .find_map(|row| row.find(needle).map(|at| row[..at].chars().count()))
}

/// Waits until the `rows` by `columns` screen the bytes after `from` draw holds
/// `needle`, and answers that screen.
async fn wait_at(
    bytes: &Arc<Mutex<Vec<u8>>>,
    from: usize,
    rows: u16,
    columns: u16,
    needle: &str,
) -> String {
    let start = tokio::time::Instant::now();
    loop {
        let screen = drawn_at(bytes, from, rows, columns);
        if screen.contains(needle) {
            return screen;
        }
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for {needle:?} on a {columns} by {rows} screen, which shows:\n{screen}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Waits until the `rows` by `columns` screen the bytes after `from` draw no
/// longer holds `needle`, and answers that screen.
async fn wait_gone_at(
    bytes: &Arc<Mutex<Vec<u8>>>,
    from: usize,
    rows: u16,
    columns: u16,
    needle: &str,
) -> String {
    let start = tokio::time::Instant::now();
    loop {
        let screen = drawn_at(bytes, from, rows, columns);
        if !screen.contains(needle) {
            return screen;
        }
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for {needle:?} to go away on a {columns} by {rows} screen, \
             which shows:\n{screen}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// At 160 columns the sidebar docks on the right with 40 columns and the
/// transcript has the other 120.
#[tokio::test]
async fn a_wide_full_screen_docks_the_sidebar_on_the_right() {
    let harness = Harness::sized(full(), Fake::start("sess-side", 0), (160, ROWS));
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, "▾ Context").await;
    // The line that sets the sidebar off holds column 120 and the panels start
    // in the column after it.
    assert_eq!(
        column_of(&screen, "│"),
        Some(120),
        "the sidebar's line starts where the transcript's 120 end:\n{screen}"
    );
    assert_eq!(
        column_of(&screen, "▾ Context"),
        Some(121),
        "the sidebar's 40 columns start where the transcript's 120 end:\n{screen}"
    );

    // A reply wider than the transcript's 120 columns is cut there: the sidebar
    // keeps its columns.
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": "x".repeat(130) }));
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, "xxxxx").await;
    let row = screen
        .lines()
        .find(|row| row.contains("xxxxx"))
        .expect("the reply's row");
    assert_eq!(
        row.trim_start().chars().take_while(|c| *c == 'x').count(),
        116,
        "the transcript's text fills the sidebar's 120 columns, less the margin:\n{screen}"
    );
}

/// At 160 columns with the sidebar docked, the transcript's 120 columns wrap a
/// reply line wider than them instead of losing its cells under the sidebar.
#[tokio::test]
async fn a_wide_reply_wraps_inside_the_main_area_of_a_docked_sidebar() {
    let harness = Harness::sized(full(), Fake::start("sess-wrap", 0), (160, ROWS));
    wait_at(&harness.bytes, 0, ROWS, 160, "idle").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    let reply = "x".repeat(150);
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": reply }));
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, &"x".repeat(30)).await;
    // The main area's 120 columns of each row, the sidebar's columns cut off.
    let main: Vec<String> = screen
        .lines()
        .map(|row| row.chars().take(120).collect())
        .collect();
    let rows: Vec<&str> = main.iter().map(|row| row.trim_end()).collect();
    // Only the transcript's rows hold the reply: the status line's hint has an
    // `x` of its own.
    assert_eq!(
        rows.iter()
            .filter(|row| !row.trim().is_empty() && row.trim().chars().all(|c| c == 'x'))
            .map(|row| row.matches('x').count())
            .sum::<usize>(),
        150,
        "every cell of the reply shows:\n{screen}"
    );
    let first = rows
        .iter()
        .position(|row| row.trim_start().starts_with("xxxx"))
        .expect("the reply's row");
    assert_eq!(
        rows[first]
            .trim_start()
            .chars()
            .take(116)
            .collect::<String>(),
        "x".repeat(116),
        "the first row is the main area's 120 columns less the margin:\n{screen}"
    );
    assert_eq!(
        screen
            .lines()
            .nth(first)
            .and_then(|row| row.chars().nth(120)),
        Some('\u{2502}'),
        "the sidebar's line holds the column after the main area's last:\n{screen}"
    );
    assert_eq!(
        rows[first + 1]
            .trim_start()
            .chars()
            .take(30)
            .collect::<String>(),
        "x".repeat(30),
        "the other 34 cells wrap into the main area under it:\n{screen}"
    );
}

/// A line of text wider than the main area wraps inside it, so the editor's
/// cursor past column 120 never hides under a docked sidebar.
#[tokio::test]
async fn an_editor_line_past_the_main_width_wraps_inside_it() {
    let harness = Harness::sized(full(), Fake::start("sess-edit", 0), (160, ROWS));
    wait_at(&harness.bytes, 0, ROWS, 160, "idle").await;
    harness.type_text(&"a".repeat(125));
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, &"a".repeat(5)).await;
    let rows: Vec<&str> = screen.lines().map(str::trim_end).collect();
    // The box's bar and its two padding cells take three columns of the main
    // area, so its text wraps at 117.
    let typed = |row: &str| {
        row.trim_start_matches('\u{258e}')
            .trim_start()
            .starts_with('a')
    };
    assert_eq!(
        rows.iter()
            .filter(|row| typed(row))
            .map(|row| row.matches('a').count())
            .sum::<usize>(),
        125,
        "every char the editor holds shows:\n{screen}"
    );
    let first = rows
        .iter()
        .position(|row| typed(row))
        .expect("the editor's row");
    assert_eq!(
        rows[first].matches('a').count(),
        117,
        "the editor's first row is the box's 117 text columns:\n{screen}"
    );
    assert_eq!(
        rows[first + 1].matches('a').count(),
        8,
        "the 8 chars past its width wrap to its next row, where the cursor is:\n{screen}"
    );
    assert!(
        rows[first].chars().nth(120) == Some('\u{2502}')
            && rows[first + 1].chars().nth(120) == Some('\u{2502}'),
        "no char of the editor reaches the sidebar's columns:\n{screen}"
    );
}

/// The approval box lays out at the main area's width, so its cut marker shows
/// beside a docked sidebar instead of under it.
#[tokio::test]
async fn an_approval_box_fits_the_main_area_of_a_docked_sidebar() {
    let harness = Harness::sized(full(), Fake::start("sess-box", 0), (160, ROWS));
    wait_at(&harness.bytes, 0, ROWS, 160, "idle").await;
    harness.fake.notify(
        "approval_request",
        &json!({
            "id": 41,
            "tool": "Bash",
            "call": "x".repeat(200),
            "rule": "default",
        }),
    );
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, "Run this command?").await;
    assert!(
        screen.contains('\u{2026}'),
        "the card's cut marker shows inside the main area:\n{screen}"
    );
    let rows: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let marker = rows
        .iter()
        .find(|row| row.contains('\u{2026}'))
        .expect("the call's row");
    // The cut marker holds the main area's last column, and the sidebar's line
    // the one after it: no row of the card runs under the sidebar.
    assert_eq!(
        marker.chars().position(|c| c == '\u{2026}'),
        Some(119),
        "the card's rows end at the main area's width:\n{screen}"
    );
    let title = rows
        .iter()
        .position(|row| row.contains("Run this command?"))
        .expect("the card's title row");
    assert!(
        rows[title].starts_with('\u{258e}') && rows[title].chars().nth(120) == Some('\u{2502}'),
        "the card's title row starts at its bar and ends before the sidebar:\n{screen}"
    );
}

/// Ctrl+X then b hides the sidebar, and Ctrl+B still moves the editor's cursor
/// back one char afterwards.
#[tokio::test]
async fn ctrl_x_b_hides_the_sidebar_and_ctrl_b_moves_the_cursor_back() {
    let harness = Harness::sized(full(), Fake::start("sess-b", 0), (160, ROWS));
    wait_at(&harness.bytes, 0, ROWS, 160, "▾ Context").await;

    harness.send(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    harness.press(KeyCode::Char('b'));
    let screen = wait_gone_at(&harness.bytes, 0, ROWS, 160, "▾ Context").await;
    assert!(
        column_of(&screen, "▾ Context").is_none(),
        "Ctrl+X b hides the sidebar:\n{screen}"
    );

    // The editor still reads Ctrl+B as one grapheme back.
    harness.type_text("ac");
    harness.send(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    harness.press(KeyCode::Char('b'));
    wait_at(&harness.bytes, 0, ROWS, 160, "abc").await;
}

/// The note of the Changes panel's row for `path`, once it drew.
fn note_of(app: &mut super::App, path: &str) -> String {
    let panel = app
        .sidebar
        .get_mut::<crate::panels::ChangesPanel>()
        .expect("the changes panel");
    let mut out = Vec::new();
    crate::slot::Panel::draw(panel, crate::slot::Size::new(40, ROWS), &mut out);
    out.iter()
        .find_map(|component| match component {
            crate::slot::Component::Item { text, note, .. } if text.as_ref() == path => {
                Some(note.as_str().to_owned())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("a row for {path} in {out:?}"))
}

/// A session longer than the 200 messages the app renders keeps the tool calls
/// of the messages it leaves out: the Changes panel names the file an old
/// `write` call wrote, though its message is not drawn.
#[tokio::test]
async fn a_long_history_feeds_the_panels_the_calls_it_leaves_out() {
    let journal = tempfile::tempdir().expect("a temp folder");
    let dir = journal.path().join("sess-old");
    let mut messages = vec![Message::assistant(AssistantMessage::new(
        "fake",
        "fake-model",
        vec![Content::ToolCall(ToolCall {
            id: Text::from("call-old"),
            name: Text::from("write"),
            arguments: Text::from(r#"{"path":"old/notes.rs","content":"one\ntwo"}"#),
        })],
    ))];
    for index in 0..250 {
        messages.push(Message::user(format!("message {index}")));
    }
    let committed = write_messages(&dir, &messages);
    let fake = Fake::start("sess-old", committed);
    let mut options = full();
    options.session_dir = Some(dir);
    let harness = Harness::sized(options, fake, (160, ROWS));
    // The first 51 of the 251 messages are left out: the call of the oldest one
    // with them, whose path the changes panel names anyway.
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, "idle").await;
    assert!(
        column_of(&screen, "old/notes.rs").is_some_and(|at| at >= 120),
        "the changes panel names the file the omitted call wrote:\n{screen}"
    );
}

/// A tool call a live frame drew does not reach the panels a second time when
/// the journal's record of the same reply arrives: an `edit` adds its lines
/// again to the row of its file, which a repeat would double.
#[tokio::test]
async fn a_journal_record_does_not_feed_a_live_call_to_the_panels_again() {
    let arguments = r#"{"path":"src/main.rs","old_string":"one\n","new_string":"one\ntwo\n"}"#;
    let reply = AssistantMessage::new(
        "fake",
        "fake-model",
        vec![Content::ToolCall(ToolCall {
            id: Text::from("call-1"),
            name: Text::from("edit"),
            arguments: Text::from(arguments),
        })],
    );
    let mut app = super::App::new(&super::Initialized::default(), (160, ROWS), true);
    app.tool_start("call-1", "edit");
    app.tool_ready("call-1", "edit", arguments);
    let once = note_of(&mut app, "src/main.rs");
    assert!(
        !once.is_empty(),
        "the live frame puts the call in the panel"
    );
    app.finished(&reply);
    assert_eq!(
        note_of(&mut app, "src/main.rs"),
        once,
        "the journal's record of the same call leaves the panel as it was"
    );
}

/// A drag that runs into a docked sidebar copies the transcript's text of the
/// rows it covers and reverses no sidebar cell.
#[tokio::test]
async fn a_drag_into_the_sidebar_copies_only_the_transcript() {
    let harness = Harness::sized(full(), Fake::start("sess-drag", 0), (160, ROWS));
    wait_at(&harness.bytes, 0, ROWS, 160, "idle").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    harness.fake.event(json!({
        "type": "text_delta",
        "text": "alpha\n\nbravo\n\ncharlie"
    }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    wait_at(&harness.bytes, 0, ROWS, 160, "charlie").await;

    let screen = wait_at(&harness.bytes, 0, ROWS, 160, "alpha").await;
    // The main area's 120 columns of each row, the sidebar's columns cut off.
    let rows: Vec<&str> = screen
        .lines()
        .map(|row| row.get(..120).unwrap_or_default().trim())
        .collect();
    let first = rows
        .iter()
        .position(|row| *row == "alpha")
        .unwrap_or_else(|| panic!("the first line in {rows:?}")) as u16;
    assert_eq!(
        rows[usize::from(first) + 4],
        "charlie",
        "the third line, after two blank rows"
    );
    let last = first + 4;

    // A drag from the first line into the sidebar's columns.
    harness.mouse(MouseEventKind::Down(MouseButton::Left), 0, first);
    harness.mouse(MouseEventKind::Drag(MouseButton::Left), 150, last);
    let start = tokio::time::Instant::now();
    loop {
        let reversed = reversed_cells(&harness.bytes, ROWS, 160);
        if reversed.contains(&(0, first)) {
            assert!(
                reversed.iter().all(|(column, _)| *column < 120),
                "the drag reverses no sidebar cell: {reversed:?}"
            );
            break;
        }
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for the drag's frame, which shows:\n{}",
            drawn_at(&harness.bytes, 0, ROWS, 160)
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    harness.mouse(MouseEventKind::Up(MouseButton::Left), 150, last);
    let start = tokio::time::Instant::now();
    while !raw(&harness.bytes).contains("\u{1b}]52;c;") {
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for the copy of the selection"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(
        copied(&raw(&harness.bytes)),
        "  alpha\n\n  bravo\n\n  charlie",
        "the copy holds the transcript's three lines, the blank rows between them and no sidebar text"
    );
}

/// The cells of a `rows` by `columns` screen the frame after the last full
/// repaint drew reversed, column and row.
fn reversed_cells(bytes: &Arc<Mutex<Vec<u8>>>, rows: u16, columns: u16) -> Vec<(u16, u16)> {
    let mut parser = vt100::Parser::new(nonzero(rows), nonzero(columns), SCROLLBACK);
    let held = bytes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    parser.process(&held);
    let screen = parser.screen();
    let mut cells = Vec::new();
    for row in 0..rows {
        for column in 0..columns {
            if screen.cell(row, column).is_some_and(vt100::Cell::inverse) {
                cells.push((column, row));
            }
        }
    }
    cells
}

/// The text the last OSC 52 sequence in `drawn` carries.
fn copied(drawn: &str) -> String {
    let open = "\u{1b}]52;c;";
    let at = drawn.rfind(open).expect("a copy") + open.len();
    let end = at + drawn[at..].find('\u{7}').expect("the end of the sequence");
    let mut bytes = Vec::new();
    for group in drawn.as_bytes()[at..end].chunks(4) {
        let mut bits = 0u32;
        let mut count: u32 = 0;
        for (index, byte) in group.iter().enumerate() {
            let value = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => continue,
            };
            bits |= u32::from(value) << (18 - 6 * index as u32);
            count += 1;
        }
        for index in 0..count.saturating_sub(1) {
            bytes.push((bits >> (16 - 8 * index)) as u8);
        }
    }
    String::from_utf8(bytes).expect("the copy is UTF-8")
}

/// A drag over two lines and its release write one OSC 52 sequence holding
/// them.
#[tokio::test]
async fn a_drag_over_two_lines_then_release_writes_one_osc_52_sequence() {
    let harness = Harness::start(full(), Fake::start("sess-copy", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    harness.fake.event(json!({
        "type": "text_delta",
        "text": "alpha bravo  \ncharlie delta"
    }));
    harness
        .fake
        .event(json!({ "type": "turn_end", "reason": "end_turn" }));
    harness
        .fake
        .event(json!({ "type": "run_end", "reason": "end_turn" }));
    wait_row(&harness.bytes, "charlie delta").await;
    wait_row(&harness.bytes, "alpha bravo").await;

    let screen = text(&harness.bytes);
    let rows: Vec<&str> = screen.lines().collect();
    let first = rows
        .iter()
        .position(|row| row.trim() == "alpha bravo")
        .expect("the first line") as u16;
    let second = rows
        .iter()
        .position(|row| row.trim() == "charlie delta")
        .expect("the second line") as u16;
    assert_eq!(second, first + 1, "the second line follows it:\n{screen}");

    let mut want = Vec::new();
    let mut scratch = String::new();
    let lines = ["  alpha bravo", "  charlie delta"];
    Selection::new(Cell::new(0, first), Cell::new(20, second)).copy(
        |row| lines[usize::from(row - first)],
        &mut scratch,
        &mut want,
    );
    assert_eq!(scratch, "  alpha bravo\n  charlie delta");

    harness.mouse(MouseEventKind::Down(MouseButton::Left), 0, first);
    harness.mouse(MouseEventKind::Drag(MouseButton::Left), 20, second);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), 20, second);

    let start = tokio::time::Instant::now();
    while !raw(&harness.bytes).contains("\u{1b}]52;c;") {
        assert!(
            start.elapsed() < WAIT,
            "waited {WAIT:?} for the copy of the selection"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let drawn = raw(&harness.bytes);
    let sequence = String::from_utf8(want).expect("the OSC 52 sequence is UTF-8");
    assert_eq!(
        drawn.matches("\u{1b}]52;c;").count(),
        1,
        "the release writes one copy"
    );
    assert!(
        drawn.contains(&sequence),
        "the copy holds the two lines: wanted {sequence:?}"
    );
}

/// The window of the `initialize` answer and the usage of the last request show
/// in the context panel.
#[tokio::test]
async fn the_window_and_the_usage_show_in_the_context_panel() {
    let harness = Harness::sized(
        full(),
        Fake::naming("sess-ctx", "a-model", 200_000),
        (160, ROWS),
    );
    harness
        .fake
        .event(json!({ "type": "usage", "input": 50_000, "output": 1_000 }));
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, "50k / 200k").await;
    assert!(
        column_of(&screen, "50k / 200k").is_some_and(|at| at >= 120),
        "the tokens show in the sidebar:\n{screen}"
    );
}

/// A `write` tool call shows the path it wrote in the changes panel.
#[tokio::test]
async fn a_write_tool_call_shows_its_path_in_the_changes_panel() {
    let harness = Harness::sized(full(), Fake::start("sess-write", 0), (160, ROWS));
    wait_at(&harness.bytes, 0, ROWS, 160, "▾ Changes").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "tool_start", "id": "call-1", "name": "write" }));
    harness.fake.event(json!({
        "type": "tool_call_ready",
        "id": "call-1",
        "name": "write",
        "arguments": "{\"path\":\"src/main.rs\",\"content\":\"one\\ntwo\"}",
    }));
    let screen = wait_at(&harness.bytes, 0, ROWS, 160, "src/main.rs").await;
    // The write's card names the path too, so the sidebar's row is any row
    // that holds it past the transcript's columns.
    assert!(
        screen.lines().any(|row| row
            .match_indices("src/main.rs")
            .any(|(at, _)| row[..at].chars().count() >= 120)),
        "the path shows in the sidebar:\n{screen}"
    );
}

/// At 100 columns the sidebar is off until Ctrl+X b, and then it is an overlay
/// over the right of the transcript.
#[tokio::test]
async fn a_narrow_full_screen_overlays_the_sidebar_only_when_toggled() {
    let harness = Harness::sized(full(), Fake::start("sess-over", 0), (100, ROWS));
    wait_at(&harness.bytes, 0, ROWS, 100, "idle").await;
    let screen = drawn_at(&harness.bytes, 0, ROWS, 100);
    assert!(
        column_of(&screen, "▾ Context").is_none(),
        "the sidebar is off under 120 columns:\n{screen}"
    );

    harness.send(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    harness.press(KeyCode::Tab);
    let screen = wait_at(&harness.bytes, 0, ROWS, 100, "▾ Context").await;
    assert_eq!(
        column_of(&screen, "│"),
        Some(72),
        "the overlay covers the last 28 columns:\n{screen}"
    );
    assert_eq!(
        column_of(&screen, "▾ Context"),
        Some(73),
        "the overlay's panels start one column past its line:\n{screen}"
    );
}

/// At 50 columns there is no sidebar, toggled or not.
#[tokio::test]
async fn a_terminal_too_narrow_for_an_overlay_has_no_sidebar() {
    let harness = Harness::sized(full(), Fake::start("sess-tiny", 0), (50, ROWS));
    wait_at(&harness.bytes, 0, ROWS, 50, "idle").await;
    harness.send(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    harness.press(KeyCode::Char('b'));
    // A key that draws once the toggle went through: the screen must not hold a
    // sidebar on it.
    harness.type_text("z");
    let screen = wait_at(&harness.bytes, 0, ROWS, 50, "z").await;
    assert!(
        column_of(&screen, "Context").is_none() && column_of(&screen, "Sessions").is_none(),
        "the sidebar is off under 60 columns:\n{screen}"
    );
}

/// The painted rows of an app the test drives key by key, trailing blanks
/// trimmed.
fn screen_text(app: &App) -> String {
    let grid = app.screen.back();
    (0..app.screen.rows())
        .map(|y| {
            let mut line = String::new();
            for x in 0..grid.width() {
                match grid.cell(x, y).char() {
                    Some(c) => line.push(c),
                    None => line.push(' '),
                }
            }
            let keep = line.trim_end().len();
            line.truncate(keep);
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An app in the inline mode with no worker behind it.
fn bare() -> App {
    App::new(&Initialized::default(), (COLUMNS, ROWS), true)
}

/// A host that answers the files of a fake and counts the reads.
struct CountingHost {
    inner: FakeHost,
    asked: Arc<AtomicUsize>,
}

impl Host for CountingHost {
    fn models(&self) -> HostFuture<Models> {
        self.inner.models()
    }

    fn files(&self) -> HostFuture<Files> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.inner.files()
    }

    fn logins(&self) -> &[&'static str] {
        self.inner.logins()
    }

    fn login(&self, provider: &str) -> mpsc::Receiver<LoginStep> {
        self.inner.login(provider)
    }

    fn open(&self, target: Target) -> HostFuture<Result<Opened, String>> {
        self.inner.open(target)
    }
}

/// `/mo` opens the command list with `/model` above `/mode`, both above the
/// text box.
#[tokio::test]
async fn command_list_opens_on_slash_and_filters() {
    let harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("/mo");
    wait_text(&harness.bytes, "/model").await;
    let screen = text(&harness.bytes);
    let model = screen
        .lines()
        .position(|line| line.split_whitespace().any(|word| word == "/model"))
        .expect("the model row");
    let mode = screen
        .lines()
        .position(|line| line.split_whitespace().any(|word| word == "/mode"))
        .expect("the mode row");
    let box_ = screen
        .lines()
        .position(|line| line.contains('▎') && line.contains("/mo"))
        .expect("the text box");
    assert!(
        model < mode && mode < box_,
        "the rows draw above the box in table order:\n{screen}"
    );
}

/// A query no command matches closes the list, so Enter sends the line.
#[tokio::test]
async fn command_list_closes_without_a_match_and_enter_sends() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("/zz");
    wait_text(&harness.bytes, "/zz").await;
    let screen = text(&harness.bytes);
    assert!(!screen.contains("choose"), "no list is left:\n{screen}");
    harness.press(KeyCode::Enter);
    let (text, mode) = harness.fake.expect_send().await;
    assert_eq!(text, "/zz");
    assert_eq!(mode, Mode::Queue);
}

/// Tab completes `/lev` to `/level `, and Enter runs the level it holds.
#[tokio::test]
async fn command_tab_completes_and_enter_runs_level() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("/lev");
    harness.press(KeyCode::Tab);
    wait_text(&harness.bytes, "/level").await;
    harness.type_text("low");
    harness.press(KeyCode::Enter);
    let (model, level, mode) = harness.fake.expect_set().await;
    assert_eq!(model, None);
    assert_eq!(level.as_deref(), Some("low"));
    assert_eq!(mode, None);
}

/// `/quit` leaves the session, as Ctrl+C twice does.
#[tokio::test]
async fn command_quit_detaches() {
    let mut harness = Harness::new_session(Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("/quit");
    harness.press(KeyCode::Enter);
    assert_eq!(harness.fake.expect_method().await, "initialize");
    assert_eq!(harness.fake.expect_method().await, "attach");
    assert_eq!(harness.fake.expect_method().await, "detach");
}

/// `/cost` lines the session's tokens and cost into the transcript once.
#[test]
fn command_cost_writes_one_line() {
    let mut app = bare();
    app.frame(Frame::Usage {
        input: 1_000,
        output: 500,
        cost: Some(0.42),
    });
    app.editor.insert("/cost");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.pending.len(), 1, "one line");
    match &app.pending[0] {
        super::Item::Plain { lines, .. } => {
            assert_eq!(lines, &["1500 tokens · $0.42".to_owned()]);
        }
        other => panic!("expected one plain line, got {other:?}"),
    }
}

/// The rules the app queued for the transcript, in the order it queued them.
fn rules(app: &App) -> Vec<super::Usage> {
    app.pending
        .iter()
        .filter_map(|item| match item {
            super::Item::Rule(usage) => Some(*usage),
            _ => None,
        })
        .collect()
}

/// A run of three provider calls draws one rule, at its end, with the tokens
/// and the cost of all three.
#[test]
fn one_rule_per_run_after_three_steps() {
    let mut app = bare();
    for step in 0..3 {
        app.frame(Frame::TurnStart);
        app.frame(Frame::Usage {
            input: 1_000,
            output: 100,
            cost: Some(0.01),
        });
        app.frame(Frame::TurnEnd);
        assert!(
            rules(&app).is_empty(),
            "the step {step} of the run ended, and no rule came with it"
        );
    }
    app.frame(Frame::RunEnd);
    let rules = rules(&app);
    assert_eq!(rules.len(), 1, "the run draws one rule");
    assert_eq!(rules[0].tokens, 3_300, "the rule sums the run's tokens");
    assert_eq!(rules[0].cents, 3, "and what the run cost");
}

/// A run that is cancelled draws its rule too, with the tokens of the calls it
/// made before it stopped.
#[test]
fn one_rule_per_run_on_a_cancelled_run() {
    let mut app = bare();
    app.frame(Frame::TurnStart);
    app.frame(Frame::ThinkingDelta(Cow::Borrowed("checking the tree")));
    app.frame(Frame::Usage {
        input: 200,
        output: 50,
        cost: Some(0.02),
    });
    // A run the user cancels stops mid-thought: the worker ends it with its
    // `run_end` and no turn end of its own.
    app.frame(Frame::RunEnd);
    let rules = rules(&app);
    assert_eq!(rules.len(), 1, "a cancelled run draws one rule");
    assert_eq!(rules[0].tokens, 250, "with the tokens of the call it made");
    assert_eq!(rules[0].cents, 2, "and what it cost");
    assert!(
        matches!(app.pending.last(), Some(super::Item::Rule(_))),
        "the rule comes under the entries of the run"
    );
}

/// The rows the notes the app queued draw, one per plain line.
fn note_rows(app: &App) -> Vec<String> {
    let mut rows = Vec::new();
    for item in &app.pending {
        match item {
            super::Item::Plain { lines, dim } => {
                assert!(!dim, "a note is not dim: {item:?}");
                rows.extend(lines.iter().cloned());
            }
            other => panic!("expected plain lines, got {other:?}"),
        }
    }
    rows
}

/// A note of 150 characters of words, thirteen of ten characters and a last of
/// seven, drops nothing: it wraps into 3 rows of at most the 60 columns of the
/// app, at the spaces between its words.
#[test]
fn note_wraps_a_long_error_at_word_boundaries() {
    let mut words: Vec<String> = (0..13).map(|i| format!("word{i:06}")).collect();
    words.push("seven77".to_owned());
    let text = words.join(" ");
    assert_eq!(text.chars().count(), 150, "the note is 150 characters");
    let mut app = App::new(&Initialized::default(), (60, ROWS), false);
    assert_eq!(app.transcript_width(), 60, "the app wraps at 60 columns");
    app.note(text);
    let rows = note_rows(&app);
    assert_eq!(rows.len(), 3, "3 rows: {rows:?}");
    for row in &rows {
        assert!(
            UnicodeWidthStr::width(row.as_str()) <= 60,
            "a row of at most 60 columns: {row:?}"
        );
    }
    let kept: Vec<&str> = rows.iter().flat_map(|row| row.split(' ')).collect();
    assert_eq!(kept, words.iter().map(String::as_str).collect::<Vec<_>>());
}

/// A word longer than a row splits at the row's end.
#[test]
fn note_wraps_a_word_longer_than_a_row() {
    let mut app = App::new(&Initialized::default(), (60, ROWS), false);
    app.note("a".repeat(70));
    let rows = note_rows(&app);
    assert_eq!(rows, ["a".repeat(60), "a".repeat(10)], "a split word");
}

/// A note that fits a row stays one row.
#[test]
fn note_wraps_keeps_a_short_note_on_one_row() {
    let mut app = App::new(&Initialized::default(), (60, ROWS), false);
    app.note("open: no such file".to_owned());
    assert_eq!(note_rows(&app), ["open: no such file".to_owned()]);
}

/// The palette filters with the keys it takes, and Esc gives the draft back.
#[test]
fn palette_keeps_the_draft() {
    let mut app = bare();
    for c in "fix it".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    palette(&mut app);
    for c in "he".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.list.is_none(), "Esc closes the palette");
    let (before, after) = app.editor.chunks();
    assert_eq!((before, after), ("fix it", ""));
}

/// Enter on the palette runs the command it picked.
#[test]
fn palette_runs_the_selected_command() {
    let mut app = bare();
    palette(&mut app);
    for c in "quit".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.exit, "the picked command quits");
    assert!(matches!(app.commands.front(), Some(super::Command::Detach)));
}

/// Ctrl+X draws the which-key list and arms no timer; the next key clears it.
#[test]
fn which_key_shows_on_ctrl_x_and_arms_no_timer() {
    let mut app = bare();
    app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(app.keys.leader_pending());
    assert_eq!(app.next_tick(), None, "no timer");
    assert!(app.deadline.is_none(), "no frame is due for it");
    app.paint();
    let screen = screen_text(&app);
    for needle in ["/ commands", "b sidebar", "l sessions", "m model"] {
        assert!(screen.contains(needle), "{needle:?} draws:\n{screen}");
    }
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.keys.leader_pending(), "the next key clears it");
    app.paint();
    let screen = screen_text(&app);
    assert!(!screen.contains("b sidebar"), "the list is gone:\n{screen}");
}

/// Tab on a file row inserts the path in place of the `@word`.
#[tokio::test]
async fn file_list_inserts_the_path() {
    let host = FakeHost::new().with_files(vec![
        "crates/tui/src/app/sidebar.rs",
        "crates/tui/src/app/mod.rs",
        "README.md",
    ]);
    let options = Options {
        host: Some(HostHandle::new(Arc::new(host))),
        ..options()
    };
    let harness = Harness::start(options, Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("look at @app/sid");
    wait_text(&harness.bytes, "sidebar.rs").await;
    harness.press(KeyCode::Tab);
    wait_text(&harness.bytes, "look at crates/tui/src/app/sidebar.rs").await;
}

/// A host whose paths arrive only when the test opens the gate, so the keys of
/// an `@` word come before them.
struct GatedHost {
    inner: FakeHost,
    gate: Arc<tokio::sync::Notify>,
}

impl Host for GatedHost {
    fn models(&self) -> HostFuture<Models> {
        self.inner.models()
    }

    fn files(&self) -> HostFuture<Files> {
        let gate = Arc::clone(&self.gate);
        let files = self.inner.files();
        Box::pin(async move {
            gate.notified().await;
            files.await
        })
    }

    fn logins(&self) -> &[&'static str] {
        self.inner.logins()
    }

    fn login(&self, provider: &str) -> mpsc::Receiver<LoginStep> {
        self.inner.login(provider)
    }

    fn open(&self, target: Target) -> HostFuture<Result<Opened, String>> {
        self.inner.open(target)
    }
}

/// Paths that arrive after the `@` word was typed fill the list that waited
/// for them.
#[tokio::test]
async fn file_list_fills_when_the_paths_arrive_after_the_keys() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let host = GatedHost {
        inner: FakeHost::new().with_files(vec!["crates/tui/src/app/sidebar.rs", "README.md"]),
        gate: Arc::clone(&gate),
    };
    let options = Options {
        host: Some(HostHandle::new(Arc::new(host))),
        ..options()
    };
    let harness = Harness::start(options, Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("look at @app/sid");
    wait_text(&harness.bytes, "reading files").await;
    gate.notify_one();
    wait_text(&harness.bytes, "sidebar.rs").await;
    harness.press(KeyCode::Tab);
    wait_text(&harness.bytes, "look at crates/tui/src/app/sidebar.rs").await;
}

/// The host reads the paths once, however many `@` words a draft holds.
#[tokio::test]
async fn file_list_reads_files_once() {
    let asked = Arc::new(AtomicUsize::new(0));
    let host = CountingHost {
        inner: FakeHost::new().with_files(vec![
            "crates/tui/src/app/sidebar.rs",
            "crates/tui/src/app/mod.rs",
        ]),
        asked: Arc::clone(&asked),
    };
    let options = Options {
        host: Some(HostHandle::new(Arc::new(host))),
        ..options()
    };
    let harness = Harness::start(options, Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("@app @app");
    wait_text(&harness.bytes, "sidebar.rs").await;
    assert_eq!(asked.load(Ordering::SeqCst), 1, "one read for the session");
}

/// A model list of one provider with two models.
fn two_models() -> Models {
    Models {
        recent: Vec::new(),
        providers: vec![Provider {
            name: Arc::from("anthropic"),
            note: Arc::from("logged in"),
            models: vec![
                Model {
                    id: Arc::from("anthropic/opus"),
                    window: Some(200_000),
                    prices: Some((15.0, 75.0)),
                },
                Model {
                    id: Arc::from("anthropic/sonnet"),
                    window: Some(200_000),
                    prices: Some((3.0, 15.0)),
                },
            ],
        }],
    }
}

/// Esc over a sheet gives the text box and its draft back.
#[tokio::test]
async fn sheet_esc_gives_the_draft_back() {
    let host = FakeHost::new().with_models(two_models());
    let options = Options {
        host: Some(HostHandle::new(Arc::new(host))),
        ..full()
    };
    let harness = Harness::start(options, Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("fix the test");
    wait_text(&harness.bytes, "fix the test").await;
    harness.send(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    harness.press(KeyCode::Char('m'));
    wait_text(&harness.bytes, "context").await;
    harness.press(KeyCode::Esc);
    wait_text(&harness.bytes, "fix the test").await;
    let screen = text(&harness.bytes);
    assert!(
        screen
            .lines()
            .any(|line| line.contains('▎') && line.contains("fix the test")),
        "the box draws the draft:\n{screen}"
    );
    assert!(!screen.contains("context"), "the sheet left:\n{screen}");
}

/// Enter on the model sheet sends the pick as the settings of the session.
#[tokio::test]
async fn model_sheet_enter_sets_the_model() {
    let host = FakeHost::new().with_models(two_models());
    let options = Options {
        host: Some(HostHandle::new(Arc::new(host))),
        ..full()
    };
    let mut harness = Harness::start(options, Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.send(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    harness.press(KeyCode::Char('m'));
    wait_text(&harness.bytes, "context").await;
    harness.press(KeyCode::Down);
    harness.press(KeyCode::Enter);
    let (model, level, mode) = harness.fake.expect_set().await;
    assert_eq!(model.as_deref(), Some("anthropic/sonnet"));
    assert_eq!((level, mode), (None, None));
    wait_text(&harness.bytes, "model: anthropic/sonnet").await;
    assert!(
        !text(&harness.bytes).contains("context"),
        "the sheet closed"
    );
}

/// The filter of the model sheet takes the typed keys.
#[tokio::test]
async fn model_sheet_filters_by_typing() {
    let host = FakeHost::new().with_models(two_models());
    let options = Options {
        host: Some(HostHandle::new(Arc::new(host))),
        ..full()
    };
    let harness = Harness::start(options, Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.send(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    harness.press(KeyCode::Char('m'));
    wait_text(&harness.bytes, "anthropic/sonnet").await;
    harness.type_text("opus");
    wait_text(&harness.bytes, "\u{203a} opus").await;
    let screen = text(&harness.bytes);
    assert!(
        !screen.contains("sonnet"),
        "the filter left only the matching row:\n{screen}"
    );
}

/// The Context panel's cost row as it stands.
fn cost_of(app: &mut App) -> String {
    app.sidebar
        .get_mut::<ContextPanel>()
        .expect("a context panel")
        .cost()
        .as_str()
        .to_owned()
}

/// A pick of a model with prices prices the turns after it at its own.
#[test]
fn model_sheet_pick_takes_the_new_prices() {
    let mut app = bare();
    app.set_prices((15.0, 75.0));
    app.models = Some(two_models());
    app.open_models();
    // The second row is `anthropic/sonnet`, at $3 and $15 a million.
    app.sheet.as_mut().expect("the model sheet").sheet.step(1);
    app.pick_sheet();
    app.frame(Frame::Usage {
        input: 1_000_000,
        output: 0,
        cost: None,
    });
    assert_eq!(cost_of(&mut app), "$3.00");
}

/// A pick of a model without prices keeps the ones the session started with.
#[test]
fn model_sheet_pick_without_prices_keeps_the_old_ones() {
    let mut app = bare();
    app.set_prices((15.0, 75.0));
    let mut models = two_models();
    models.providers[0].models[1].prices = None;
    app.models = Some(models);
    app.open_models();
    app.sheet.as_mut().expect("the model sheet").sheet.step(1);
    app.pick_sheet();
    app.frame(Frame::Usage {
        input: 1_000_000,
        output: 0,
        cost: None,
    });
    assert_eq!(cost_of(&mut app), "$15.00");
}

/// The sheet marks the model the worker's `settings` notice names, as the
/// Context panel shows it.
#[test]
fn model_sheet_marks_the_model_the_settings_notice_names() {
    let mut app = bare();
    app.frame(Frame::Settings {
        model: "anthropic/sonnet".into(),
        context_window: Some(200_000),
        level: None,
        mode: None,
    });
    app.models = Some(two_models());
    app.open_models();
    let sheet = &app.sheet.as_ref().expect("the model sheet").sheet;
    let row = sheet
        .rows
        .iter()
        .find(|row| &*row.name == "anthropic/sonnet")
        .expect("the sonnet row");
    assert_eq!(row.mark, Mark::Current);
    assert_eq!(&*row.right, "now");
}

/// The sessions sheet shows the preview beside its rows only where the width
/// leaves room for it.
#[test]
fn sessions_sheet_shows_a_preview_at_120_columns_and_none_at_80() {
    let session = |id: &str, title: &str| SessionRow {
        id: Arc::from(id),
        title: Arc::from(title),
        updated: 1_000,
        live: false,
        waiting: false,
        current: false,
        headless: false,
    };
    for (columns, preview) in [(120, true), (80, false)] {
        let mut app = App::new(&Initialized::default(), (columns, ROWS), true);
        app.sessions = vec![session("aaaa1111", "first"), session("bbbb2222", "second")];
        // The preview needs a wide main area, so the panels fold into the
        // header first.
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        app.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        app.paint();
        let screen = screen_text(&app);
        assert!(screen.contains("Sessions"), "the sheet draws:\n{screen}");
        assert!(screen.contains("first"), "the rows draw:\n{screen}");
        assert_eq!(
            screen.contains("enter starts it"),
            preview,
            "the preview at {columns} columns:\n{screen}"
        );
    }
}

/// Enter on another session asks the host for its worker, and the app ends its
/// loop with that worker to connect to.
#[tokio::test]
async fn sessions_sheet_enter_opens_another_session() {
    use std::task::Poll;

    let second = Fake::start("second", 0);
    let opened = Opened {
        socket: second.socket,
        token: TOKEN.to_owned(),
        session_dir: None,
    };
    let host = Arc::new(FakeHost::new().with_open(Ok(opened.clone())));
    let mut app = bare();
    app.host = Some(HostHandle::new(host.clone()));
    app.sessions = vec![SessionRow {
        id: Arc::from("bbbb2222"),
        title: Arc::from("second"),
        updated: 1_000,
        live: false,
        waiting: false,
        current: false,
        headless: false,
    }];
    app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    app.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(host.opened(), [Target::Session("bbbb2222".to_owned())]);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    let future = app.waits.open.as_mut().expect("the open is in flight");
    let Poll::Ready(answer) = future.as_mut().poll(&mut context) else {
        panic!("the fake host answers at once");
    };
    app.opened(answer);
    assert_eq!(app.switch.as_ref(), Some(&opened), "the app switches to it");
}

/// `/new` asks the host for a worker on a new session, and the app ends its
/// loop with that worker to connect to.
#[tokio::test]
async fn new_command_opens_a_new_session() {
    use std::task::Poll;

    let fresh = Fake::start("fresh", 0);
    let opened = Opened {
        socket: fresh.socket,
        token: TOKEN.to_owned(),
        session_dir: None,
    };
    let host = Arc::new(FakeHost::new().with_open(Ok(opened.clone())));
    let init = Initialized {
        session: "aaaa1111".to_owned(),
        ..Initialized::default()
    };
    let mut app = App::new(&init, (COLUMNS, ROWS), true);
    app.host = Some(HostHandle::new(host.clone()));
    for c in "/new".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(host.opened(), [Target::New]);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    let future = app.waits.open.as_mut().expect("the open is in flight");
    let Poll::Ready(answer) = future.as_mut().poll(&mut context) else {
        panic!("the fake host answers at once");
    };
    app.opened(answer);
    assert_eq!(app.switch.as_ref(), Some(&opened), "the app switches to it");
}

/// `/fork` asks the host for a worker on a session forked from this one, and
/// the app ends its loop with that worker to connect to.
#[tokio::test]
async fn fork_command_forks_this_session() {
    use std::task::Poll;

    let fresh = Fake::start("forked", 0);
    let opened = Opened {
        socket: fresh.socket,
        token: TOKEN.to_owned(),
        session_dir: None,
    };
    let host = Arc::new(FakeHost::new().with_open(Ok(opened.clone())));
    let init = Initialized {
        session: "aaaa1111".to_owned(),
        ..Initialized::default()
    };
    let mut app = App::new(&init, (COLUMNS, ROWS), true);
    app.host = Some(HostHandle::new(host.clone()));
    for c in "/fork".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(host.opened(), [Target::Fork("aaaa1111".to_owned())]);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    let future = app.waits.open.as_mut().expect("the open is in flight");
    let Poll::Ready(answer) = future.as_mut().poll(&mut context) else {
        panic!("the fake host answers at once");
    };
    app.opened(answer);
    assert_eq!(app.switch.as_ref(), Some(&opened), "the app switches to it");
}

/// `/fork` during a run asks the host for nothing: the fork would copy the
/// calls the run has not answered yet.
#[test]
fn fork_command_waits_for_the_turn_to_end() {
    let host = Arc::new(FakeHost::new());
    let init = Initialized {
        session: "aaaa1111".to_owned(),
        ..Initialized::default()
    };
    let mut app = App::new(&init, (COLUMNS, ROWS), true);
    app.host = Some(HostHandle::new(host.clone()));
    app.running = true;
    for c in "/fork".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(host.opened().is_empty(), "no fork was asked for");
    assert!(app.waits.open.is_none());
    let rows = note_rows(&app);
    assert!(
        rows.first()
            .is_some_and(|row| row.starts_with("fork: wait for the turn to end")),
        "the reason is a note: {rows:?}"
    );
}

/// A worker `/new` cannot start leaves this session as it was, with the reason
/// as a note.
#[tokio::test]
async fn new_command_keeps_the_session_when_the_worker_cannot_start() {
    use std::task::Poll;

    let host = FakeHost::new().with_open(Err("no worker".to_owned()));
    let init = Initialized {
        session: "aaaa1111".to_owned(),
        ..Initialized::default()
    };
    let mut app = App::new(&init, (COLUMNS, ROWS), true);
    app.host = Some(HostHandle::new(Arc::new(host)));
    for c in "/new".chars() {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    let future = app.waits.open.as_mut().expect("the open is in flight");
    let Poll::Ready(answer) = future.as_mut().poll(&mut context) else {
        panic!("the fake host answers at once");
    };
    app.opened(answer);
    assert!(app.switch.is_none(), "the app stays on this session");
    assert_eq!(app.session, "aaaa1111");
    let rows = note_rows(&app);
    assert!(
        rows.first().is_some_and(|row| row.starts_with("open:")),
        "the reason is a note: {rows:?}"
    );
}

/// The login sheet shows the lines its steps left.
#[tokio::test]
async fn login_sheet_shows_the_steps() {
    let host = FakeHost::new().with_login(
        "anthropic",
        vec![
            LoginStep::Open {
                url: Arc::from("https://example.test/open"),
            },
            LoginStep::Code {
                page: Arc::from("https://example.test/code"),
                code: Arc::from("abcd-1234"),
            },
            LoginStep::Done,
        ],
    );
    let options = Options {
        host: Some(HostHandle::new(Arc::new(host))),
        ..full()
    };
    let harness = Harness::start(options, Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("/login");
    harness.press(KeyCode::Enter);
    wait_text(&harness.bytes, "Log in").await;
    harness.press(KeyCode::Enter);
    wait_text(&harness.bytes, "https://example.test/open").await;
    wait_text(&harness.bytes, "abcd-1234").await;
    wait_text(&harness.bytes, "Logged in to anthropic").await;
}

/// `?` in an empty box opens the key help, and is not typed.
#[test]
fn help_sheet_opens_on_question_mark_in_an_empty_box() {
    let mut app = bare();
    app.key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
    assert!(app.sheet.is_some(), "the key help opens");
    assert!(app.editor.is_empty(), "the `?` is not typed");
    app.paint();
    let screen = screen_text(&app);
    assert!(
        screen.contains("text box"),
        "the key groups draw:\n{screen}"
    );
}

/// Tab on the palette of an empty draft completes the pick, as it does with a
/// draft: no fold of the transcript takes it first.
#[test]
fn palette_tab_completes_on_an_empty_draft() {
    let mut app = bare();
    palette(&mut app);
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(
        app.list.is_none(),
        "Tab takes the pick and closes the palette"
    );
    let (before, _) = app.editor.chunks();
    assert!(
        before.starts_with('/') && before.ends_with(' '),
        "the pick is in the box: {before:?}"
    );
}

/// A sheet opened while the sidebar has the keys takes them: Esc closes the
/// sheet, not the sidebar's focus.
#[test]
fn sheet_takes_the_keys_from_a_focused_sidebar() {
    let mut app = bare();
    app.screen.resize(COLUMNS, ROWS);
    app.reflow();
    app.sidebar.set_active(true);
    app.key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
    assert!(app.sheet.is_some(), "the key sheet opens");
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.sheet.is_none(), "Esc closes the sheet");
}

/// A key that comes while the host still reads the paths does not lose the
/// read: the paths still fill the list when they come.
#[tokio::test]
async fn file_list_keeps_the_read_across_a_key() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let host = GatedHost {
        inner: FakeHost::new().with_files(vec!["crates/tui/src/app/sidebar.rs", "README.md"]),
        gate: Arc::clone(&gate),
    };
    let options = Options {
        host: Some(HostHandle::new(Arc::new(host))),
        ..options()
    };
    let harness = Harness::start(options, Fake::start("sess", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("@sid");
    wait_text(&harness.bytes, "reading files").await;
    harness.press(KeyCode::Down);
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.notify_one();
    wait_text(&harness.bytes, "sidebar.rs").await;
}

/// Writes a journal of one user message for session `id` into `dir`.
fn write_session(dir: &Path, id: &str) {
    let header = Header::new(
        id.to_owned(),
        reagent_store::paths::now_ms(),
        None,
        Bytes::new(),
    );
    let mut writer = Writer::create(dir, header).expect("the writer");
    let mut payload = Vec::new();
    encode_message(&mut payload, &Message::user("the other session's task"));
    writer
        .append(MESSAGE, Bytes::from(payload), false)
        .expect("append a message");
    writer.commit().expect("commit");
    writer.close().expect("close");
}

/// The options of a full-screen app whose folder holds one other session,
/// with `host` to open it.
fn with_other_session(state: &Path, host: FakeHost) -> Options {
    let folder = state.join("folder");
    write_session(&folder.join("other"), "other");
    Options {
        full: true,
        state: Some(state.to_path_buf()),
        session_dir: Some(folder.join("sess")),
        host: Some(HostHandle::new(Arc::new(host))),
        ..options()
    }
}

/// Opens the sessions sheet once the folder's list, read off the runtime,
/// holds the other session: until then the sheet shows none.
async fn open_the_other_session(harness: &Harness) {
    let start = tokio::time::Instant::now();
    loop {
        harness.send(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        harness.press(KeyCode::Char('l'));
        wait_text(&harness.bytes, "Sessions").await;
        if text(&harness.bytes).contains("1 in this folder") {
            return;
        }
        assert!(
            start.elapsed() < WAIT,
            "the folder's list never held the other session"
        );
        harness.press(KeyCode::Esc);
        wait_gone(&harness.bytes, "Sessions").await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A session whose worker cannot be reached leaves a note and the session the
/// app has, still running.
#[tokio::test]
async fn sessions_sheet_keeps_the_session_when_the_other_cannot_be_reached() {
    let state = tempfile::tempdir().expect("a state folder");
    let host = FakeHost::new().with_open(Ok(Opened {
        socket: state.path().join("no-worker.sock"),
        token: TOKEN.to_owned(),
        session_dir: None,
    }));
    let harness = Harness::start(
        with_other_session(state.path(), host),
        Fake::start("sess", 0),
    );
    wait_text(&harness.bytes, "idle").await;
    open_the_other_session(&harness).await;
    harness.press(KeyCode::Enter);
    wait_text(&harness.bytes, "open:").await;
    assert!(!harness.handle.is_finished(), "the app keeps running");
    harness.type_text("still here");
    wait_text(&harness.bytes, "still here").await;
}

/// A switch to another session and a quit there leave the alternate screen.
#[tokio::test]
async fn sessions_sheet_switch_then_quit_leaves_the_alternate_screen() {
    let state = tempfile::tempdir().expect("a state folder");
    let mut second = Fake::start("other", 0);
    let host = FakeHost::new().with_open(Ok(Opened {
        socket: second.socket.clone(),
        token: TOKEN.to_owned(),
        session_dir: None,
    }));
    let harness = Harness::start(
        with_other_session(state.path(), host),
        Fake::start("sess", 0),
    );
    wait_text(&harness.bytes, "idle").await;
    open_the_other_session(&harness).await;
    harness.press(KeyCode::Enter);
    assert_eq!(second.expect_method().await, "initialize");
    assert_eq!(second.expect_method().await, "attach");
    harness.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    harness.send(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(second.expect_method().await, "detach");
    timeout(WAIT, harness.handle)
        .await
        .expect("the app ends")
        .expect("the app does not panic")
        .expect("the app does not fail");
    let bytes = harness.bytes.lock().expect("the bytes").clone();
    let on = b"\x1b[?1049h";
    let off = b"\x1b[?1049l";
    let last_on = bytes
        .windows(on.len())
        .rposition(|window| window == on)
        .expect("the app entered the alternate screen");
    assert!(
        bytes[last_on..]
            .windows(off.len())
            .any(|window| window == off),
        "the last session leaves the alternate screen"
    );
}

/// The rows of `screen` that hold only `c` once their blanks are trimmed: the
/// rows of a reply of one repeated character.
fn only_rows(screen: &str, c: char) -> Vec<&str> {
    screen
        .lines()
        .filter(|row| !row.trim().is_empty() && row.trim().chars().all(|other| other == c))
        .collect()
}

/// Full screen keeps two blank columns at each side of the transcript: a reply
/// line longer than the terminal wraps between them.
#[tokio::test]
async fn transcript_margin_keeps_two_columns_each_side() {
    let harness = Harness::start(full(), Fake::start("sess-margin", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": "y".repeat(100) }));
    let screen = wait_at(&harness.bytes, 0, ROWS, COLUMNS, &"y".repeat(70)).await;
    let reply = only_rows(&screen, 'y');
    assert_eq!(reply.len(), 2, "the reply takes two rows:\n{screen}");
    for row in &reply {
        assert!(
            row.starts_with("  ") && !row.starts_with("   "),
            "two blank columns on the left: {row:?}"
        );
        assert!(
            row.trim_end().chars().count() <= usize::from(COLUMNS) - 2,
            "two blank columns on the right: {row:?}"
        );
    }
    assert_eq!(
        reply[0].matches('y').count(),
        usize::from(COLUMNS) - 4,
        "the text wraps at the width between the margins:\n{screen}"
    );
    assert_eq!(
        reply[1].matches('y').count(),
        24,
        "the rest of the line:\n{screen}"
    );
}

/// The margin is the transcript's: the text box of full screen still starts in
/// column 0.
#[tokio::test]
async fn transcript_margin_keeps_the_text_box_full_width() {
    let harness = Harness::start(full(), Fake::start("sess-margin-box", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.type_text("hi");
    let screen = wait_at(&harness.bytes, 0, ROWS, COLUMNS, "hi").await;
    assert_eq!(
        column_of(&screen, "\u{258e}"),
        Some(0),
        "the text box's bar holds column 0, the margin left out:\n{screen}"
    );
}

/// Inline mode has no main area to keep a margin in: its rows start at column 0
/// and wrap at the terminal's columns.
#[tokio::test]
async fn transcript_margin_leaves_inline_mode_alone() {
    let harness = Harness::new_session(Fake::start("sess-margin-inline", 0));
    wait_text(&harness.bytes, "idle").await;
    harness.fake.event(json!({ "type": "turn_start" }));
    harness
        .fake
        .event(json!({ "type": "text_delta", "text": "y".repeat(100) }));
    wait_text(&harness.bytes, &"y".repeat(70)).await;
    let screen = text(&harness.bytes);
    let reply = only_rows(&screen, 'y');
    assert_eq!(reply.len(), 2, "the reply takes two rows:\n{screen}");
    assert_eq!(
        reply[0].matches('y').count(),
        usize::from(COLUMNS),
        "an inline row wraps at the terminal's columns and starts at column 0:\n{screen}"
    );
    assert_eq!(
        reply[1].matches('y').count(),
        20,
        "the rest of the line:\n{screen}"
    );
}

/// The key the map sends for a character with a modifier.
fn chord(c: char, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), modifiers)
}

/// Type the text one key at a time, as the keyboard sends it.
fn type_keys(app: &mut App, text: &str) {
    for c in text.chars() {
        app.key(chord(c, KeyModifiers::NONE));
    }
}

/// The text box text with `|` at the cursor.
fn box_text(app: &App) -> String {
    let (before, after) = app.editor.chunks();
    let mut out = String::with_capacity(before.len() + after.len() + 1);
    out.push_str(before);
    out.push('|');
    out.push_str(after);
    out
}

#[test]
fn readline_move_the_keys_of_the_text_box_move_the_cursor() {
    let mut app = bare();
    for c in "cargo test --all".chars() {
        app.key(chord(c, KeyModifiers::NONE));
    }
    app.key(chord('b', KeyModifiers::ALT));
    app.key(chord('b', KeyModifiers::ALT));
    assert_eq!(box_text(&app), "cargo |test --all");
    app.key(chord('f', KeyModifiers::ALT));
    assert_eq!(box_text(&app), "cargo test| --all");
    app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "cargo |test --all", "Ctrl+Left is a word");
    app.key(chord('f', KeyModifiers::CONTROL));
    app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
    assert_eq!(box_text(&app), "cargo |test --all", "Alt+Left is a word");
    app.key(chord('l', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "cargo |test --all", "Ctrl+L edits nothing");
}

#[test]
fn readline_move_ctrl_d_on_an_empty_box_arms_the_quit() {
    let mut app = bare();
    app.key(chord('d', KeyModifiers::CONTROL));
    assert!(app.armed, "the first Ctrl+D asks for another");
    app.key(chord('c', KeyModifiers::CONTROL));
    assert!(app.exit, "Ctrl+C after Ctrl+D quits");
}

#[test]
fn readline_move_ctrl_d_twice_on_an_empty_box_quits() {
    let mut app = bare();
    app.key(chord('d', KeyModifiers::CONTROL));
    app.key(chord('d', KeyModifiers::CONTROL));
    assert!(app.exit);
}

#[test]
fn readline_move_ctrl_d_with_text_deletes_it() {
    let mut app = bare();
    type_keys(&mut app, "ab");
    app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    app.key(chord('d', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "|b");
    assert!(!app.armed, "Ctrl+D with text is no quit");
}

#[test]
fn readline_move_ctrl_l_redraws_the_whole_screen() {
    let mut app = bare();
    type_keys(&mut app, "hello");
    let mut quiet = Vec::new();
    app.paint();
    app.screen.diff(&mut quiet);
    let mut again = Vec::new();
    app.paint();
    app.screen.diff(&mut again);
    app.key(chord('l', KeyModifiers::CONTROL));
    let mut full = Vec::new();
    app.paint();
    app.screen.diff(&mut full);
    assert!(
        full.len() >= again.len(),
        "Ctrl+L writes the rows the frame before it wrote: {} against {}",
        full.len(),
        again.len()
    );
    assert_eq!(box_text(&app), "hello|", "Ctrl+L edits nothing");
}

#[test]
fn readline_undo_arrives_as_ctrl_seven() {
    let mut app = bare();
    type_keys(&mut app, "ab cd");
    assert_eq!(box_text(&app), "ab cd|");
    app.key(chord('7', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "|");
}

#[test]
fn readline_undo_through_the_leader_ctrl_x_ctrl_u() {
    let mut app = bare();
    type_keys(&mut app, "ab");
    app.key(chord('x', KeyModifiers::CONTROL));
    app.key(chord('u', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "|");
}

#[test]
fn readline_move_the_mark_through_the_leader() {
    let mut app = bare();
    type_keys(&mut app, "abc");
    app.key(chord(' ', KeyModifiers::CONTROL));
    app.key(chord('a', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "|abc");
    app.key(chord('x', KeyModifiers::CONTROL));
    app.key(chord('x', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "abc|");
}

#[test]
fn readline_arg_quoted_insert_takes_a_tab_and_a_newline() {
    let mut app = bare();
    app.key(chord('v', KeyModifiers::CONTROL));
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(box_text(&app), "\t|", "a quoted Tab is a tab");
    app.key(chord('q', KeyModifiers::CONTROL));
    app.key(chord('j', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "\t\n|", "a quoted Ctrl+J is a line break");
    app.key(chord('v', KeyModifiers::CONTROL));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(box_text(&app), "\t\n\n|", "a quoted Enter is a line break");
}

#[test]
fn readline_kill_the_keys_kill_and_yank() {
    let mut app = bare();
    type_keys(&mut app, "one two three");
    app.key(chord('w', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "one two |", "Ctrl+W kills the word before");
    app.key(chord('y', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "one two three|", "Ctrl+Y puts it back");
    app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(box_text(&app), "one two |", "Alt+Backspace kills a word");
    app.key(chord('a', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "|one two ", "Ctrl+A takes the start");
}

#[test]
fn readline_arg_the_digits_before_a_key_repeat_it() {
    let mut app = bare();
    type_keys(&mut app, "abcdef");
    app.key(chord('3', KeyModifiers::ALT));
    app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(box_text(&app), "abc|");
    app.key(chord('-', KeyModifiers::ALT));
    app.key(chord('f', KeyModifiers::ALT));
    assert_eq!(box_text(&app), "|abc", "a negative argument goes backward");
}

#[test]
fn readline_arg_a_line_break_drops_the_argument() {
    let mut app = bare();
    app.key(chord('3', KeyModifiers::ALT));
    app.key(chord('j', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "\n|", "a line break does not repeat");
    type_keys(&mut app, "x");
    assert_eq!(box_text(&app), "\nx|", "the argument did not reach the x");
}

#[test]
fn readline_arg_a_move_drops_the_argument() {
    let mut app = bare();
    type_keys(&mut app, "ab");
    app.key(chord('3', KeyModifiers::ALT));
    app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(box_text(&app), "|ab");
    type_keys(&mut app, "x");
    assert_eq!(box_text(&app), "x|ab", "Home spent the argument");
}

#[test]
fn readline_move_ctrl_d_with_text_clears_the_armed_quit() {
    let mut app = bare();
    type_keys(&mut app, "ab");
    app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    app.key(chord('c', KeyModifiers::CONTROL));
    assert!(app.armed, "the first Ctrl+C asks for another");
    app.key(chord('d', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "|b", "Ctrl+D with text deletes it");
    assert!(!app.exit);
    app.key(chord('c', KeyModifiers::CONTROL));
    assert!(!app.exit, "Ctrl+D with text dropped the arming");
}

/// The sent messages the history keys walk, oldest first.
fn with_history(app: &mut App, entries: &[&str]) {
    app.history = entries.iter().map(|entry| (*entry).to_owned()).collect();
}

/// The screen row under the first row that holds `needle`.
fn row_under<'a>(screen: &'a str, needle: &str) -> Option<&'a str> {
    let mut rows = screen.lines();
    while let Some(row) = rows.next() {
        if row.contains(needle) {
            return rows.next();
        }
    }
    None
}

#[test]
fn readline_history_ctrl_p_and_ctrl_n_walk_the_sent_messages() {
    let mut app = bare();
    with_history(&mut app, &["cargo build", "git status"]);
    type_keys(&mut app, "wip");
    app.key(chord('p', KeyModifiers::CONTROL));
    assert_eq!(
        box_text(&app),
        "git status|",
        "Ctrl+P is the newest message"
    );
    app.key(chord('p', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "cargo build|", "and then the one before");
    app.key(chord('n', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "git status|");
    app.key(chord('n', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "wip|", "Ctrl+N gives the draft back");
}

#[test]
fn readline_history_alt_less_and_alt_greater_go_to_the_ends() {
    let mut app = bare();
    with_history(&mut app, &["cargo build", "git status", "ls"]);
    type_keys(&mut app, "wip");
    app.key(chord('p', KeyModifiers::CONTROL));
    app.key(chord('<', KeyModifiers::ALT));
    assert_eq!(
        box_text(&app),
        "cargo build|",
        "Alt+< is the oldest message"
    );
    app.key(chord('>', KeyModifiers::ALT));
    assert_eq!(box_text(&app), "wip|", "Alt+> is the draft");
}

#[test]
fn readline_history_alt_period_takes_the_last_word_and_walks_back() {
    let mut app = bare();
    with_history(&mut app, &["cargo build", "git status"]);
    app.key(chord('.', KeyModifiers::ALT));
    assert_eq!(box_text(&app), "status|", "the last word of the newest");
    app.key(chord('.', KeyModifiers::ALT));
    assert_eq!(box_text(&app), "build|", "the second Alt+. walks back");
    type_keys(&mut app, "x");
    assert_eq!(box_text(&app), "buildx|", "a typed key ends the walk");
}

#[test]
fn readline_history_ctrl_r_finds_an_older_message_and_draws_its_prompt() {
    let mut app = bare();
    with_history(&mut app, &["cargo build", "git status"]);
    type_keys(&mut app, "wip");
    app.key(chord('r', KeyModifiers::CONTROL));
    assert_eq!(
        box_text(&app),
        "git status|",
        "Ctrl+R shows the newest message first"
    );
    type_keys(&mut app, "car");
    assert_eq!(box_text(&app), "|cargo build", "the cursor is at the match");
    let screen = painted(&mut app);
    let prompt = screen
        .lines()
        .find(|row| row.contains("i-search"))
        .unwrap_or_else(|| panic!("the prompt draws:\n{screen}"));
    assert!(
        prompt.contains("(reverse-i-search)`car':"),
        "the prompt names the query: {prompt:?}"
    );
    let matched = row_under(&screen, "i-search")
        .unwrap_or_else(|| panic!("a row under the prompt:\n{screen}"));
    assert!(
        matched.contains("cargo build"),
        "the match draws under the prompt: {matched:?}"
    );
}

#[test]
fn readline_history_ctrl_r_again_finds_an_older_match() {
    let mut app = bare();
    with_history(&mut app, &["cargo build", "cargo test", "git status"]);
    app.key(chord('r', KeyModifiers::CONTROL));
    type_keys(&mut app, "cargo");
    assert_eq!(box_text(&app), "|cargo test", "the newest match");
    app.key(chord('r', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "|cargo build", "Ctrl+R again walks back");
    app.key(chord('s', KeyModifiers::CONTROL));
    assert_eq!(box_text(&app), "|cargo test", "Ctrl+S walks forward again");
}

#[test]
fn readline_history_ctrl_g_gives_the_draft_back() {
    let mut app = bare();
    with_history(&mut app, &["cargo build", "git status"]);
    type_keys(&mut app, "wip");
    app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    app.key(chord('r', KeyModifiers::CONTROL));
    type_keys(&mut app, "ca");
    assert_eq!(box_text(&app), "|cargo build");
    app.key(chord('g', KeyModifiers::CONTROL));
    assert_eq!(
        box_text(&app),
        "wi|p",
        "Ctrl+G gives the draft and its point back"
    );
    assert!(!app.editor.searching());
    let screen = painted(&mut app);
    assert!(
        !screen.contains("i-search"),
        "the prompt is gone:\n{screen}"
    );
}

#[test]
fn readline_history_esc_keeps_the_match_and_cancels_no_run() {
    let mut app = bare();
    with_history(&mut app, &["cargo build"]);
    app.running = true;
    app.key(chord('r', KeyModifiers::CONTROL));
    type_keys(&mut app, "car");
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(box_text(&app), "|cargo build", "the match stays in the box");
    assert!(!app.editor.searching());
    assert!(app.running, "Esc ends the search, not the run");
    assert!(app.commands.is_empty(), "no cancel reached the worker");
}

#[test]
fn readline_history_enter_sends_the_match() {
    let mut app = bare();
    with_history(&mut app, &["cargo build"]);
    app.key(chord('r', KeyModifiers::CONTROL));
    type_keys(&mut app, "car");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.editor.is_empty(), "the match left the box");
    match app.commands.front() {
        Some(super::Command::Send { text, .. }) => assert_eq!(text, "cargo build"),
        other => panic!("the match is sent: {other:?}"),
    }
}

#[test]
fn readline_history_a_query_that_matches_nothing_says_so() {
    let mut app = bare();
    with_history(&mut app, &["cargo build", "git status"]);
    app.key(chord('r', KeyModifiers::CONTROL));
    type_keys(&mut app, "zz");
    let screen = painted(&mut app);
    assert!(
        screen.contains("(failed reverse-i-search)`zz':"),
        "the prompt says the query failed:\n{screen}"
    );
    assert_eq!(
        box_text(&app),
        "git status|",
        "a failed query keeps the match shown"
    );
}

/// Opens the command palette through its leader key.
fn palette(app: &mut App) {
    app.key(chord('x', KeyModifiers::CONTROL));
    app.key(chord('/', KeyModifiers::NONE));
}

#[test]
fn palette_opens_on_ctrl_x_slash_and_esc_gives_the_draft_back() {
    let mut app = bare();
    type_keys(&mut app, "fix it");
    palette(&mut app);
    assert!(app.list.is_some(), "ctrl+x / opens the commands");
    type_keys(&mut app, "he");
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.list.is_none(), "Esc closes the palette");
    assert_eq!(box_text(&app), "fix it|", "the draft stayed in the box");
}

#[test]
fn palette_ctrl_p_no_longer_opens_the_commands() {
    let mut app = bare();
    app.key(chord('p', KeyModifiers::CONTROL));
    assert!(app.list.is_none(), "Ctrl+P opens no list");
    with_history(&mut app, &["cargo build"]);
    app.key(chord('p', KeyModifiers::CONTROL));
    assert!(app.list.is_none(), "Ctrl+P is the history now");
    assert_eq!(box_text(&app), "cargo build|");
}

#[test]
fn readline_history_a_paste_ends_the_search_and_keeps_the_match() {
    let mut app = bare();
    with_history(&mut app, &["cargo build"]);
    app.key(chord('r', KeyModifiers::CONTROL));
    type_keys(&mut app, "d");
    assert_eq!(box_text(&app), "cargo buil|d", "the match and its cursor");
    app.paste(" --all".to_owned());
    assert!(!app.editor.searching(), "the paste ended the search");
    assert_eq!(
        box_text(&app),
        "cargo buil --all|d",
        "the paste landed where the cursor of the match was"
    );
    let screen = painted(&mut app);
    assert!(
        !screen.contains("i-search"),
        "the prompt is gone:\n{screen}"
    );
}

#[test]
fn readline_history_search_gives_the_box_the_rows_the_text_takes() {
    let mut app = bare();
    let long = format!("{} car", "word ".repeat(200));
    with_history(&mut app, &[long.as_str()]);
    app.key(chord('r', KeyModifiers::CONTROL));
    type_keys(&mut app, "car");
    let screen = painted(&mut app);
    let rows: Vec<&str> = screen.lines().collect();
    let footer = rows
        .iter()
        .rposition(|row| row.contains("reagent"))
        .unwrap_or_else(|| panic!("the footer draws:\n{screen}"));
    assert_eq!(
        rows.len() - 1 - footer,
        2,
        "the gap row and the status line, and no blank row under the box:\n{screen}"
    );
}

/// `/compact` with words after it sends them as the note of the request.
#[test]
fn compact_command_sends_the_note_the_line_carries() {
    let mut app = bare();
    app.editor.insert("/compact keep the tool calls");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match app.commands.pop_front() {
        Some(super::Command::Compact { note }) => {
            assert_eq!(note.as_deref(), Some("keep the tool calls"))
        }
        other => panic!("expected a compact request, got {other:?}"),
    }
}

/// `/compact` while a turn runs sends nothing and says why.
#[test]
fn compact_command_waits_for_the_turn_to_end() {
    let mut app = bare();
    app.running = true;
    app.editor.insert("/compact");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.commands.is_empty(), "a turn in the way holds it back");
    let rows = note_rows(&app);
    assert!(
        rows.iter()
            .any(|row| row.starts_with("compact: wait for the turn to end")),
        "the note says why: {rows:?}"
    );
}

/// `/compact` with nothing after it sends a request without a note.
#[test]
fn compact_command_without_text_sends_no_note() {
    let mut app = bare();
    app.editor.insert("/compact");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match app.commands.pop_front() {
        Some(super::Command::Compact { note }) => assert_eq!(note, None),
        other => panic!("expected a compact request, got {other:?}"),
    }
}

/// The `compacted` frame the worker sends for one compaction.
fn compacted() -> Frame<'static> {
    Frame::Compacted {
        before: 142_000,
        after: 31_000,
        replaced: 64,
    }
}

/// A live `compacted` event draws one dim row with what it took off.
#[test]
fn compaction_row_draws_the_row_text() {
    let mut app = bare();
    app.frame(compacted());
    app.commit();
    let screen = painted(&mut app);
    assert!(
        screen.contains("compacted · 142k → 31.0k tokens · 64 messages"),
        "the row draws what it replaced:\n{screen}"
    );
}

/// Opening the row draws the checkpoint text the journal recorded, read once
/// from the base that wrote it.
#[tokio::test]
async fn compaction_row_opening_it_shows_the_checkpoint_text() {
    let dir = tempfile::Builder::new()
        .prefix("tui")
        .tempdir_in("/tmp")
        .expect("a temp folder in /tmp");
    let path = dir.path().join("sess-base");
    let header = Header::new(
        "sess-base".to_owned(),
        reagent_store::paths::now_ms(),
        None,
        Bytes::new(),
    );
    let mut writer = Writer::create(&path, header).expect("the writer");
    writer
        .append(crate::history::BASE, Bytes::new(), true)
        .expect("append the base");
    let mut payload = Vec::new();
    encode_message(
        &mut payload,
        &Message::user(format!("{}the tool calls it kept", crate::history::HEADER)),
    );
    writer
        .append(MESSAGE, Bytes::from(payload), false)
        .expect("append the checkpoint");
    writer.commit().expect("commit");
    writer.close().expect("close");
    let newest = Session::open(&path)
        .expect("open the journal")
        .records()
        .filter_map(Result::ok)
        .map(|record| record.seq)
        .max()
        .expect("the newest record");

    // The event arrives before the base its checkpoint is written in, so the
    // row notes the record it starts reading from.
    let mut app = bare();
    app.journal = Some(path);
    app.upto = newest - 2;
    app.frame(compacted());
    app.upto = newest;
    app.commit();
    let screen = painted(&mut app);
    assert!(
        !screen.contains("the tool calls it kept"),
        "the row is shut:\n{screen}"
    );

    let fold = app.checkpoints[0].0;
    app.folds.toggle(fold);
    app.reopen(fold);
    painted(&mut app);
    let (fold, from) = app.wanted.take().expect("the open row wants its text");
    app.read_checkpoint(fold, from).await;
    let screen = painted(&mut app);
    assert!(
        screen.contains("the tool calls it kept"),
        "the checkpoint text draws under the row:\n{screen}"
    );
}

/// A compaction with nothing to fold in notes what the session said, without
/// the mark of a failed turn.
#[test]
fn compact_command_with_nothing_to_compact_notes_it() {
    let mut app = bare();
    app.frame(Frame::Error(Cow::Borrowed("nothing to compact")));
    let rows = note_rows(&app);
    assert!(
        rows.iter().any(|row| row.starts_with("nothing to compact")),
        "the note is the session's own words: {rows:?}"
    );
}
