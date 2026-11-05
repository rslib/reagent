//! End-to-end tests of the terminal UI: the real binary inside a pseudo
//! terminal, against a worker it started for a scripted fake provider.
//!
//! Each test has its own config home, state folder and project folder, so no
//! test reads the real config or writes a session outside its own temp folder,
//! and the state folder sits under `/tmp` because the worker's socket path has
//! a platform length limit. The tests drive the binary, so they exist only with
//! the `testing` and `tui` features.
#![cfg(all(feature = "testing", feature = "tui"))]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};
use nix::sys::termios::{self, InputFlags, LocalFlags, Termios};
use reagent_protocol::discovery;
use tokio::process::{Child, Command};

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// What a run with no `-p` says when no terminal is attached.
const NO_TERMINAL: &str = "a task needs -p when stdin or stdout is not a terminal";

/// How long a test waits for text or an exit before it fails.
const WAIT: Duration = Duration::from_secs(30);

/// The terminal the binary gets: 100 columns by 30 rows.
const COLUMNS: u16 = 100;
const ROWS: u16 = 30;

/// How many rows of scrollback the test's parser keeps.
const SCROLLBACK: usize = 1000;

/// How many bytes of what the binary wrote a test keeps, for the sequences it
/// looks for.
const DRAWN: usize = 1 << 20;

/// The folders one test runs in.
struct Home {
    /// The config home and the project folder.
    root: tempfile::TempDir,
    /// The state folder, short enough for a socket path.
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
            .prefix("rw")
            .tempdir_in("/tmp")
            .expect("a short state folder");
        // A `.git` marks the project folder, so its config file is read.
        std::fs::create_dir_all(root.path().join("work/.git")).expect("the project folder");
        Home { root, state }
    }

    fn state(&self) -> &Path {
        self.state.path()
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

    /// Asks a person before every `bash` call of the project folder.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test writes its own project config"
    )]
    fn ask(&self) {
        let dir = self.cwd().join(".reagent");
        std::fs::create_dir_all(&dir).expect("the config folder");
        std::fs::write(dir.join("config.toml"), "[permissions]\nask = [\"Bash\"]\n")
            .expect("the rules");
    }

    /// Writes the global config file of this home, which `XDG_CONFIG_HOME`
    /// points at.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test writes its own global config"
    )]
    fn config(&self, body: &str) {
        let dir = self.root.path().join("config/reagent");
        std::fs::create_dir_all(&dir).expect("the config folder");
        std::fs::write(dir.join("config.toml"), body).expect("the global config");
    }

    /// The binary, set up the way this home runs it.
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.state.path())
            .env("REAGENT_MODELS_DEV_URL", "http://127.0.0.1:1")
            .env_remove("REAGENT_PROFILE")
            .current_dir(self.cwd());
        command
    }

    /// Runs the binary headless and returns what it printed.
    async fn headless(&self, args: &[&str]) -> std::process::Output {
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .expect("the binary runs")
    }

    /// The workers still running for this home.
    async fn workers(&self) -> Vec<discovery::Entry> {
        discovery::list(&discovery::folder(self.state())).await
    }

    /// Waits for every worker of this home to stop, and says whether it did.
    async fn no_worker(&self) -> bool {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.workers().await.is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }
}

/// A UI drawn in a pseudo terminal, with the screen it drew.
#[expect(
    clippy::disallowed_types,
    reason = "a test drives a pty through the standard library"
)]
struct Tui {
    /// The binary's process.
    child: Child,
    /// The master side, where the test writes keys.
    master: std::fs::File,
    /// A handle on the terminal the binary drew on, for its modes.
    slave: std::fs::File,
    /// The parsed screen, filled by a reader thread.
    screen: Arc<Mutex<vt100::Parser>>,
    /// Every byte the binary wrote, for the sequences a test looks for.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Tui {
    /// Starts the binary in a terminal, with `args` and this home.
    fn start(home: &Home, args: &[&str]) -> Tui {
        Tui::spawn(home.command().args(args))
    }

    /// Starts the binary under `script`, which gives it a terminal of its own
    /// as its controlling terminal, the way a shell does: then `/dev/tty`
    /// opens, which it never does for a child handed the test's pty.
    fn start_controlling(home: &Home, args: &[&str]) -> Tui {
        let command = home.command();
        let mut script = Command::new("script");
        if cfg!(target_os = "macos") {
            script.args(["-q", "/dev/null", BIN]).args(args);
        } else {
            let line = std::iter::once(BIN)
                .chain(args.iter().copied())
                .map(|word| format!("'{}'", word.replace('\'', "'\\''")))
                .collect::<Vec<_>>()
                .join(" ");
            script.args(["-qec", &line, "/dev/null"]);
        }
        let std = command.as_std();
        for (key, value) in std.get_envs() {
            match value {
                Some(value) => script.env(key, value),
                None => script.env_remove(key),
            };
        }
        if let Some(cwd) = std.get_current_dir() {
            script.current_dir(cwd);
        }
        Tui::spawn(&mut script)
    }

    /// Runs `command` in a new pty and reads what it draws.
    #[expect(
        clippy::disallowed_types,
        reason = "a test drives a pty through the standard library"
    )]
    fn spawn(command: &mut Command) -> Tui {
        let size = Winsize {
            ws_row: ROWS,
            ws_col: COLUMNS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&size), None).expect("a pseudo terminal");
        let master = std::fs::File::from(pty.master);
        let slave = std::fs::File::from(pty.slave);
        let child = command
            .stdin(Stdio::from(slave.try_clone().expect("a terminal handle")))
            .stdout(Stdio::from(slave.try_clone().expect("a terminal handle")))
            .stderr(Stdio::from(slave.try_clone().expect("a terminal handle")))
            .kill_on_drop(true)
            .spawn()
            .expect("the binary starts");
        let size = vt100::Parser::new(
            std::num::NonZeroU16::new(ROWS).expect("rows"),
            std::num::NonZeroU16::new(COLUMNS).expect("columns"),
            SCROLLBACK,
        );
        let screen = Arc::new(Mutex::new(size));
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let mut output = master.try_clone().expect("a terminal handle");
        let parsed = Arc::clone(&screen);
        let written = Arc::clone(&bytes);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match output.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => {
                        parsed
                            .lock()
                            .expect("the screen lock")
                            .process(&buffer[..read]);
                        let mut bytes = written.lock().expect("the bytes lock");
                        if bytes.len() < DRAWN {
                            bytes.extend_from_slice(&buffer[..read]);
                        }
                    }
                }
            }
        });
        Tui {
            child,
            master,
            slave,
            screen,
            bytes,
        }
    }

    /// Sends keys to the terminal.
    fn keys(&mut self, keys: &str) {
        self.master
            .write_all(keys.as_bytes())
            .expect("the keys reach the terminal");
        self.master.flush().expect("the keys are sent");
    }

    /// The terminal's modes, as the binary left them.
    fn modes(&self) -> Termios {
        termios::tcgetattr(&self.slave).expect("the terminal's modes")
    }

    /// The row and column of the editor's cursor: the terminal's own cursor,
    /// shown, and no cell painted reversed as a second one.
    fn cursor(&self) -> (u16, u16) {
        let mut parser = self.screen.lock().expect("the screen lock");
        let screen = parser.screen();
        let reversed = (0..ROWS)
            .flat_map(|row| (0..COLUMNS).map(move |column| (row, column)))
            .find(|&(row, column)| screen.cell(row, column).is_some_and(|cell| cell.inverse()));
        if screen.hide_cursor() || reversed.is_some() {
            let text = text_of(&mut parser);
            panic!("no single terminal cursor (reversed cell {reversed:?}):\n{text}");
        }
        screen.cursor_position()
    }

    /// Everything the terminal holds, its scrollback included.
    fn text(&self) -> String {
        text_of(&mut self.screen.lock().expect("the screen lock"))
    }

    /// The rows the terminal shows now, the top row first.
    fn rows(&self) -> Vec<String> {
        let parser = self.screen.lock().expect("the screen lock");
        parser.screen().rows(0, COLUMNS).collect()
    }

    /// Whether the screen the binary draws on is the alternate one.
    fn alternate(&self) -> bool {
        self.screen
            .lock()
            .expect("the screen lock")
            .screen()
            .alternate_screen()
    }

    /// Whether one of the frames the binary wrote carries `sequence`.
    fn drew(&self, sequence: &str) -> bool {
        let bytes = self.bytes.lock().expect("the bytes lock");
        bytes
            .windows(sequence.len())
            .any(|window| window == sequence.as_bytes())
    }

    /// The start of what the binary wrote, for a test that says what it got
    /// instead of the sequence it looked for.
    fn drawn(&self) -> String {
        let bytes = self.bytes.lock().expect("the bytes lock");
        String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).into_owned()
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until the terminal no longer shows `text` anywhere, and fails with
    /// the screen when it still does.
    async fn until_gone(&self, text: &str) {
        let deadline = Instant::now() + WAIT;
        while self.text().contains(text) {
            assert!(
                Instant::now() < deadline,
                "the terminal still showed {text:?}:\n{}",
                self.text()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits for the binary to exit and returns its code.
    async fn wait(&mut self) -> i32 {
        let status = tokio::time::timeout(WAIT, self.child.wait())
            .await
            .expect("an exit in time")
            .expect("the binary's exit");
        status.code().unwrap_or(-1)
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

/// A run with no task and a terminal shows the editor and the idle status, a
/// typed message reaches the worker, and Ctrl+C twice leaves the terminal as it
/// found it while the worker stops on its own.
#[tokio::test]
async fn reagent_starts_a_new_session_with_an_editor() {
    let home = Home::new();
    let provider = home.script("one", r#"{"replies": [{"text": "hello from the fake"}]}"#);
    let started = Instant::now();
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;
    println!("start to first frame: {:?}", started.elapsed());

    // The status says the session is idle, the Context panel names the session
    // (the panels open with Ctrl+X b on a terminal under 120 columns), and the
    // terminal is in raw mode, as an editor needs.
    let worker = home.workers().await.pop().expect("a worker of this run");
    let session = &worker.session[..worker.session.len().min(8)];
    tui.until("idle").await;
    tui.keys("\u{18}b");
    tui.until(session).await;
    tui.keys("\u{18}b");
    // The cursor is hidden while a frame draws, so the check waits for the
    // frame that closes the panels.
    tui.until_gone(session).await;
    let modes = tui.modes();
    assert!(
        !modes.local_flags.contains(LocalFlags::ICANON),
        "the terminal is in raw mode: {modes:?}"
    );
    assert!(
        !modes.local_flags.contains(LocalFlags::ECHO),
        "the terminal does not echo: {modes:?}"
    );

    // The editor's cursor sits at the first text column of the box (after its
    // bar and its padding), and what is typed lands under it.
    assert_eq!(tui.cursor().1, 3, "the editor's cursor starts at column 3");
    tui.keys("hi");
    // "hi" is also in the footer's "high", so wait for the cursor to move.
    let deadline = Instant::now() + Duration::from_secs(10);
    while tui.cursor().1 != 5 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (typed_row, column) = tui.cursor();
    assert_eq!(column, 5, "the cursor follows the typed text");
    assert!(
        tui.rows()[usize::from(typed_row)]
            .chars()
            .skip(3)
            .collect::<String>()
            .starts_with("hi"),
        "the typed text is the editor's row:\n{}",
        tui.text()
    );

    // Enter sends it, and the model's reply lands as text.
    tui.keys("\r");
    tui.until("› hi").await;
    tui.until("hello from the fake").await;

    // Ctrl+C twice quits, and the terminal goes back to cooked mode.
    assert_eq!(tui.quit().await, 0, "a normal quit");
    let modes = tui.modes();
    assert!(
        modes.local_flags.contains(LocalFlags::ICANON),
        "the terminal is back in canonical mode: {modes:?}"
    );
    assert!(
        modes.local_flags.contains(LocalFlags::ECHO),
        "the terminal echoes again: {modes:?}"
    );
    // The worker is not killed with the UI: it stops itself once its last
    // client left, which leaves no entry behind.
    assert!(
        home.no_worker().await,
        "the worker stopped on its own: {:?}",
        home.workers().await
    );
}

/// Waits until a worker of another session than `parent` serves this home, and
/// answers its session id. The worker of the parent session stops once the UI
/// leaves it, so the two are listed together only for a moment.
async fn other_session(home: &Home, parent: &str) -> String {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Some(session) = home
            .workers()
            .await
            .into_iter()
            .map(|entry| entry.session)
            .find(|session| session != parent)
        {
            return session;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "no worker of another session than {parent}: {:?}",
        home.workers().await
    );
}

/// Waits until the worker of `parent` leaves: it stops once its last client,
/// the UI, has left it for another session.
async fn left(home: &Home, parent: &str) {
    let deadline = Instant::now() + WAIT;
    while home
        .workers()
        .await
        .iter()
        .any(|entry| entry.session == parent)
    {
        assert!(
            Instant::now() < deadline,
            "the worker of {parent} never left: {:?}",
            home.workers().await
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Opens the panels with Ctrl+X b until the Context panel shows `text`.
///
/// A key typed while the UI joins another worker can be lost, so the key is
/// sent again, and panels that opened without `text` are closed first.
async fn panels_show(tui: &mut Tui, text: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        tui.keys("\u{18}b");
        let tried = Instant::now() + Duration::from_secs(1);
        while Instant::now() < tried {
            if tui.text().contains(text) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            Instant::now() < deadline,
            "the panels never showed {text:?}:\n{}",
            tui.text()
        );
        if tui.text().contains("Context") {
            tui.keys("\u{18}b");
            tui.until_gone("Context").await;
        }
    }
}

/// `/fork` starts a worker on a session forked from this one, and the UI
/// switches to it with the turns of the session it came from.
#[tokio::test]
async fn fork_command_carries_the_history() {
    let home = Home::new();
    let provider = home.script(
        "fork",
        r#"{"replies": [{"text": "hello from the fake"}, {"text": "another reply"}]}"#,
    );
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;
    tui.keys("hi\r");
    tui.until("hello from the fake").await;
    // `/fork` waits for the turn to end, and the reply shows before it does.
    tui.until("idle").await;
    let parent = home
        .workers()
        .await
        .pop()
        .expect("a worker of this run")
        .session;

    // The `/` list opens above the box, and Enter runs the row it picked.
    tui.keys("/fork\r");
    let forked = other_session(&home, &parent).await;
    assert_ne!(forked, parent, "the fork is a session of its own");

    // The Context panel names the session the UI switched to, once the panels
    // open with Ctrl+X b on a terminal under 120 columns.
    left(&home, &parent).await;
    panels_show(&mut tui, &forked[..forked.len().min(8)]).await;
    tui.until("hello from the fake").await;

    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// `/new` starts a worker on a new session in this folder, and the UI switches
/// to it, with none of the turns of the session it left.
#[tokio::test]
async fn new_command_starts_an_empty_session() {
    let home = Home::new();
    let provider = home.script(
        "new",
        r#"{"replies": [{"text": "hello from the fake"}, {"text": "another reply"}]}"#,
    );
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;
    tui.keys("hi\r");
    tui.until("hello from the fake").await;
    tui.until("idle").await;
    let parent = home
        .workers()
        .await
        .pop()
        .expect("a worker of this run")
        .session;

    tui.keys("/new\r");
    let fresh = other_session(&home, &parent).await;
    assert_ne!(fresh, parent, "the new session is one of its own");

    // The Context panel names the session the UI switched to, and none of the
    // earlier turns are on its screen.
    left(&home, &parent).await;
    panels_show(&mut tui, &fresh[..fresh.len().min(8)]).await;
    tui.until("idle").await;
    assert!(
        !tui.text().contains("hello from the fake"),
        "the new session is empty:\n{}",
        tui.text()
    );

    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// A `bash` call the project config asks about shows a box, and `y` runs it.
#[tokio::test]
async fn an_approval_box_runs_the_call_on_y() {
    let home = Home::new();
    home.ask();
    let provider = home.script(
        "two",
        r#"{"replies": [{"calls": [{"name": "bash", "arguments": {"command": "echo hi > ran.txt"}}]},
            {"text": "the call ran"}]}"#,
    );
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    tui.keys("run it\r");
    tui.until("waits for you").await;
    tui.until("Run this command?").await;
    tui.until("echo hi > ran.txt").await;
    // The card names the rule that asked: the project config asks on `Bash`.
    tui.until("asked by Bash").await;

    // A box answers keys only once it has been on screen a moment, so a key
    // already being typed runs no tool.
    tokio::time::sleep(Duration::from_millis(400)).await;
    tui.keys("y");
    tui.until("the call ran").await;
    let ran = tokio::fs::read_to_string(home.cwd().join("ran.txt"))
        .await
        .expect("the call ran");
    assert_eq!(ran, "hi\n", "the answer ran the call once");

    assert_eq!(tui.quit().await, 0, "a normal quit");
    assert!(home.no_worker().await, "the worker stopped on its own");
}

/// `reagent -c` shows the turns the session already has above the editor.
#[tokio::test]
async fn continue_shows_the_earlier_turns_above_the_editor() {
    let home = Home::new();
    let first = home.script("first", r#"{"replies": [{"text": "first reply"}]}"#);
    let second = home.script("second", r#"{"replies": [{"text": "second reply"}]}"#);
    for (first, provider) in [
        (["-p", "first turn"].as_slice(), &first),
        (["-c", "-p", "second turn"].as_slice(), &second),
    ] {
        let mut flags = first.to_vec();
        flags.extend(["--provider", provider.as_str()]);
        let run = home.headless(&flags).await;
        assert!(
            run.status.success(),
            "exit {} stderr {}",
            run.status,
            String::from_utf8_lossy(&run.stderr)
        );
    }

    let provider = home.script("third", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["-c", "--provider", &provider]);
    tui.until("idle").await;
    for text in [
        "› first turn",
        "first reply",
        "› second turn",
        "second reply",
    ] {
        tui.until(text).await;
    }

    // The history sits above the editor: every earlier turn ends above the
    // editor's cursor row, and the status is the last row of all.
    let rows = tui.rows();
    let (cursor_row, _) = tui.cursor();
    for text in [
        "› first turn",
        "first reply",
        "› second turn",
        "second reply",
    ] {
        let row = rows
            .iter()
            .position(|row| row.contains(text))
            .unwrap_or_else(|| panic!("no row of {text:?}:\n{}", tui.text()));
        assert!(
            row < usize::from(cursor_row),
            "{text:?} is above the editor, at row {row}: {}",
            tui.text()
        );
    }
    let last = rows
        .iter()
        .rev()
        .find(|row| !row.trim().is_empty())
        .expect("the status row");
    assert!(last.contains("idle"), "the status is last: {last:?}");

    assert_eq!(tui.quit().await, 0, "a normal quit");
    assert!(home.no_worker().await, "the worker stopped on its own");
}

/// A run with no flag and no `ui` draws full screen: it enters the alternate
/// screen with the mouse captured, and a quit leaves both behind with one line
/// that names the session on the normal screen.
#[tokio::test]
async fn the_default_run_enters_the_alternate_screen() {
    let home = Home::new();
    let provider = home.script("full", r#"{"replies": [{"text": "full screen reply"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;
    assert!(
        tui.drew("\u{1b}[?1049h"),
        "the run enters the alternate screen, and wrote \n{}",
        tui.drawn()
    );
    assert!(
        tui.drew("\u{1b}[?1000h"),
        "the run captures the mouse:\n{}",
        tui.drawn()
    );
    assert!(
        tui.alternate(),
        "the screen it draws on is the alternate one"
    );

    // The workspace works on that screen too.
    tui.keys("hi\r");
    tui.until("› hi").await;
    tui.until("full screen reply").await;

    assert_eq!(tui.quit().await, 0, "a normal quit");
    assert!(
        !tui.alternate(),
        "a quit leaves the alternate screen:\n{}",
        tui.drawn()
    );
    assert!(
        tui.drew("\u{1b}[?1000l"),
        "a quit releases the mouse, and wrote\n{}",
        tui.drawn()
    );
    assert!(home.no_worker().await, "the worker stopped on its own");
}

/// A full-screen run ends with one line that names its session and the command
/// that continues it, and the transcript stays off the shell's screen.
#[tokio::test]
async fn quitting_full_screen_leaves_one_line() {
    let home = Home::new();
    let provider = home.script("one-line", r#"{"replies": [{"text": "a vanished reply"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;
    let worker = home.workers().await.pop().expect("a worker of this run");
    let id: String = worker
        .session
        .chars()
        .take(8)
        .collect::<String>()
        .to_uppercase();

    tui.keys("hi\r");
    tui.until("a vanished reply").await;
    assert_eq!(tui.quit().await, 0, "a normal quit");

    let line = format!("reagent: session {id} · reagent -c to continue");
    tui.until(&line).await;
    assert!(
        !tui.text().contains("a vanished reply"),
        "the transcript stays out of the shell's scrollback:\n{}",
        tui.text()
    );
    assert!(
        !tui.alternate(),
        "the line lands on the normal screen:\n{}",
        tui.drawn()
    );
    assert!(home.no_worker().await, "the worker stopped on its own");
}

/// `--inline` keeps the UI inline, on the normal screen, with a workspace as it
/// has always been: its lines stay in the scrollback and no exit line follows.
#[tokio::test]
async fn the_inline_flag_keeps_the_terminal_out_of_full_screen() {
    let home = Home::new();
    let provider = home.script("inline", r#"{"replies": [{"text": "inline reply"}]}"#);
    let mut tui = Tui::start(&home, &["--inline", "--provider", &provider]);
    tui.until("idle").await;
    assert!(!tui.alternate(), "--inline draws inline: \n{}", tui.text());
    assert!(
        !tui.drew("\u{1b}[?1049h"),
        "--inline never enters the alternate screen:\n{}",
        tui.drawn()
    );

    // The workspace still works: a typed message reaches the worker.
    tui.keys("hi\r");
    tui.until("› hi").await;
    tui.until("inline reply").await;

    assert_eq!(tui.quit().await, 0, "a normal quit");
    tui.until("inline reply").await;
    assert!(
        !tui.text().contains("· reagent -c to continue"),
        "inline mode prints no exit line:\n{}",
        tui.text()
    );
    assert!(home.no_worker().await, "the worker stopped on its own");
}

/// A config that says `ui = "inline"` keeps the UI inline too, with no flag.
#[tokio::test]
async fn a_config_that_says_inline_keeps_the_terminal_out_of_full_screen() {
    let home = Home::new();
    home.config("ui = \"inline\"\n");
    let provider = home.script("config", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;
    assert!(
        !tui.alternate(),
        "a config that says inline draws inline:\n{}",
        tui.text()
    );
    assert_eq!(tui.quit().await, 0, "a normal quit");
    assert!(home.no_worker().await, "the worker stopped on its own");
}

/// A config that names no mode stops the start, says which key is wrong, and
/// leaves no worker behind.
#[tokio::test]
async fn a_bogus_ui_setting_stops_the_start_and_names_the_key() {
    let home = Home::new();
    home.config("ui = \"bogus\"\n");
    let provider = home.script("bogus", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    let code = tui.wait().await;
    // The reason goes to stderr, which is the terminal this test reads.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let text = tui.text();
    assert_ne!(code, 0, "a config with no mode fails the start:\n{text}");
    assert!(
        text.contains("ui must be full or inline"),
        "the reason names the key:\n{text}"
    );
    assert!(
        home.workers().await.is_empty(),
        "no worker started for a config that stops the UI"
    );
}

/// A run with no `-p` and no terminal on stdout exits 2 and says a task needs
/// one.
#[tokio::test]
async fn no_terminal_needs_a_task() {
    let home = Home::new();
    let run = home
        .command()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .output()
        .await
        .expect("the binary runs");
    assert_eq!(run.status.code(), Some(2), "a usage error");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains(NO_TERMINAL), "says why: {stderr}");
    assert!(
        home.workers().await.is_empty(),
        "no worker started for a run with no terminal"
    );
}

/// A burst of keys larger than the terminal's input buffer reaches the app in
/// full: the input reader drains the terminal each time it wakes, so no key
/// waits for the next one to arrive. On Linux the pty holds 4 KiB, and an
/// edge-triggered reader that stops after one chunk hangs with the writer.
#[tokio::test]
async fn a_burst_of_keys_larger_than_the_terminal_buffer_arrives_in_full() {
    let home = Home::new();
    let provider = home.script("burst", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;
    // 2,000 pairs of one-line scrolls are 24 KB of input, then a mark the
    // editor shows only once every key before it was read.
    let burst = "\u{1b}[1;2A\u{1b}[1;2B".repeat(2_000) + "burst-end";
    let mut master = tui.master.try_clone().expect("a second handle");
    let writer = std::thread::spawn(move || {
        master
            .write_all(burst.as_bytes())
            .expect("the keys reach the terminal");
    });
    tui.until("burst-end").await;
    writer.join().expect("the writer finished");
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// In a terminal that is the process's controlling terminal, as a shell gives
/// one, the UI reads its keys: macOS kqueue refuses `/dev/tty`, so the reader
/// opens the terminal by its device path.
#[tokio::test]
async fn a_controlling_terminal_reads_its_keys() {
    let home = Home::new();
    let provider = home.script("ctty", r#"{"replies": [{"text": "a reply"}]}"#);
    let mut tui = Tui::start_controlling(&home, &["--inline", "--provider", &provider]);
    tui.until("idle").await;
    assert!(
        !tui.text().contains("input:"),
        "the reader opened the terminal:\n{}",
        tui.text()
    );
    tui.keys("hi\r");
    tui.until("a reply").await;
    tui.keys("\u{3}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    tui.keys("\u{3}");
    tui.wait().await;
}

/// `/mo` opens the slash list above the text box with `/model` and `/mode`, and
/// Esc closes the list while the box keeps the draft.
#[tokio::test]
async fn the_slash_list_shows_the_model_command() {
    let home = Home::new();
    let provider = home.script("slash", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    tui.keys("/mo");
    tui.until("/model").await;
    let rows = tui.rows();
    let box_row = rows
        .iter()
        .position(|row| row.contains('▎') && row.contains("/mo"))
        .unwrap_or_else(|| panic!("the text box holds the draft:\n{}", tui.text()));
    for command in ["/model", "/mode"] {
        let row = rows
            .iter()
            .position(|row| row.contains(command))
            .unwrap_or_else(|| panic!("no row of {command:?}:\n{}", tui.text()));
        assert!(
            row < box_row,
            "{command:?} draws above the text box:\n{}",
            tui.text()
        );
    }

    // Esc closes the list, and the draft stays in the text box.
    tui.keys("\u{1b}");
    tui.until_gone("/mode").await;
    let rows = tui.rows();
    assert!(
        rows.iter()
            .any(|row| row.contains('▎') && row.contains("/mo")),
        "the text box still holds `/mo`:\n{}",
        tui.text()
    );

    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// Ctrl+X shows the which-key list of the leader keys, and Esc clears it.
#[tokio::test]
async fn ctrl_x_shows_the_leader_keys() {
    let home = Home::new();
    let provider = home.script("leader", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    tui.keys("\u{18}");
    tui.until("l sessions").await;
    let text = tui.text();
    for key in ["b sidebar", "l sessions", "m model"] {
        assert!(text.contains(key), "{key:?} is listed:\n{text}");
    }

    tui.keys("\u{1b}");
    tui.until_gone("l sessions").await;

    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// `?` on an empty box opens the key sheet, Esc gives the box back, and a `?`
/// typed after a draft is a `?` in the box.
#[tokio::test]
async fn question_mark_opens_the_key_sheet_and_esc_gives_the_draft_back() {
    let home = Home::new();
    let provider = home.script("keys", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    tui.keys("?");
    tui.until("send or steer").await;
    assert!(
        tui.text().contains("Keys"),
        "the sheet is the key sheet:\n{}",
        tui.text()
    );

    tui.keys("\u{1b}");
    tui.until_gone("send or steer").await;
    tui.keys("hi?");
    tui.until("hi?").await;
    assert!(
        !tui.text().contains("send or steer"),
        "a `?` after a draft is typed, not the sheet:\n{}",
        tui.text()
    );

    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// Ctrl+X m lists the models of a provider the config names, read from the
/// models cache of the home, without the network.
#[tokio::test]
async fn the_model_sheet_lists_a_configured_provider() {
    let home = Home::new();
    home.config(
        "[providers.mine]\nwire = \"openai-chat\"\nbase_url = \"http://127.0.0.1:1\"\n\
         api_key = \"sk-mine-test\"\n",
    );
    tokio::fs::set_permissions(
        home.root.path().join("config/reagent/config.toml"),
        std::fs::Permissions::from_mode(0o600),
    )
    .await
    .expect("the config is private");
    let cache = home.root.path().join("cache/reagent/models");
    tokio::fs::create_dir_all(&cache)
        .await
        .expect("the cache folder");
    tokio::fs::write(
        cache.join("mine.json"),
        r#"{"fetched_at":1,"models":[{"id":"fast","context_window":200000,"max_output":null}]}"#,
    )
    .await
    .expect("the cache file");

    let provider = home.script("models", r#"{"replies": [{"text": "unused"}]}"#);
    let mut command = home.command();
    command.env("XDG_CACHE_HOME", home.root.path().join("cache"));
    command.args(["--provider", provider.as_str()]);
    let mut tui = Tui::spawn(&mut command);
    tui.until("idle").await;

    tui.keys("\u{18}m");
    tui.until("mine/fast").await;
    assert!(
        tui.text().contains("Model"),
        "the model sheet is on screen:\n{}",
        tui.text()
    );

    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// `/level low` reaches the worker, whose settings notice draws the level in
/// the Context panel.
#[tokio::test]
async fn level_command_reaches_the_worker() {
    let home = Home::new();
    let provider = home.script("level", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    tui.keys("/level low\r");
    // The panels open with Ctrl+X b on a terminal under 120 columns, and the
    // Context panel names the settings the worker applied.
    tui.keys("\u{18}b");
    tui.until("low · default").await;

    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// Alt+b and Alt+f move the text box by word: the ESC of a meta key that
/// arrives in one read is the meta prefix, not a cancel.
#[tokio::test]
async fn readline_meta_keys_move_by_word() {
    let home = Home::new();
    let provider = home.script("meta", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    tui.keys("cargo test --all");
    tui.until("cargo test --all").await;
    tui.keys("\u{1b}b\u{1b}b\u{1b}f");
    tui.keys("|");
    tui.until("cargo test| --all").await;

    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// Ctrl+R finds a sent message above the box, and Ctrl+G gives the box back.
#[tokio::test]
async fn readline_ctrl_r_finds_a_sent_message() {
    let home = Home::new();
    let provider = home.script("search", r#"{"replies": [{"text": "a reply"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    tui.keys("hi\r");
    tui.until("a reply").await;
    tui.until("idle").await;
    tui.keys("\u{12}");
    tui.until("(reverse-i-search)").await;
    tui.keys("h");
    tui.until("(reverse-i-search)`h':").await;
    let rows = tui.rows();
    let at = rows
        .iter()
        .position(|row| row.contains("(reverse-i-search)`h':"))
        .unwrap_or_else(|| panic!("no prompt row:\n{}", tui.text()));
    let matched = rows.get(at + 1).expect("a row under the prompt");
    assert!(
        matched.contains("hi"),
        "the match draws under the prompt: {matched:?}"
    );

    tui.keys("\u{7}");
    tui.until_gone("(reverse-i-search)").await;
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// Ctrl+Q and Ctrl+S reach the app: raw mode clears the terminal's flow
/// control, so neither is held back by the tty.
#[tokio::test]
async fn readline_ctrl_q_and_ctrl_s_reach_the_app() {
    let home = Home::new();
    let provider = home.script("flow", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    let modes = tui.modes();
    assert!(
        !modes.input_flags.contains(InputFlags::IXON),
        "raw mode clears the flow control: {modes:?}"
    );
    // Ctrl+Q quotes the next key, so `?` in an empty box is text, not the key
    // that opens the help sheet: the cursor moves one cell to the right.
    tui.keys("\u{11}?");
    let deadline = Instant::now() + WAIT;
    while tui.cursor().1 != 4 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        tui.cursor().1,
        4,
        "the quoted `?` went into the box:\n{}",
        tui.text()
    );
    assert!(
        !tui.text().contains("text box"),
        "no sheet opened:\n{}",
        tui.text()
    );

    // Ctrl+S opens the forward search, which fails with no message sent yet.
    tui.keys("\u{13}");
    tui.until("(failed i-search)").await;
    tui.keys("\u{7}");
    tui.until_gone("(failed i-search)").await;

    assert_eq!(tui.quit().await, 0, "a normal quit");
}
