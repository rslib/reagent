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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
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

/// The program status query the binary asks, and the answer a terminal that
/// knows the protocol gives it: its states and the kinds of card it reads.
const PROGRAM_QUERY: &str = "\x1b]7501;?\x1b\\";
const PROGRAM_ANSWER: &str =
    "\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";

/// How many bytes of the read before a test keeps, so a query that one read
/// ended in the middle of is still answered.
const QUERY_TAIL: usize = 16;

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

    /// Writes the keys file of this home's config folder, which
    /// `XDG_CONFIG_HOME` points at.
    #[expect(clippy::disallowed_methods, reason = "a test writes its own keys file")]
    fn keys_toml(&self, body: &str) {
        let dir = self.root.path().join("config/reagent");
        std::fs::create_dir_all(&dir).expect("the config folder");
        std::fs::write(dir.join("keys.toml"), body).expect("the keys file");
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

    /// The binary in a terminal that calls itself `program`, with no multiplexer
    /// and no other terminal's host variable around it: the image protocol it
    /// reads is the one the test names.
    fn terminal(&self, program: &str) -> Command {
        let mut command = self.command();
        command.env("TERM_PROGRAM", program);
        for name in [
            "TMUX",
            "TMUX_PANE",
            "ZELLIJ",
            "ZELLIJ_SESSION_NAME",
            "STY",
            "HERDR_PANE_ID",
            "RMAESTRO_PANE",
            "KITTY_WINDOW_ID",
        ] {
            command.env_remove(name);
        }
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

    /// The session folders of the project folder, sorted by id.
    async fn sessions(&self) -> Vec<PathBuf> {
        let cwd = tokio::fs::canonicalize(self.cwd())
            .await
            .expect("the project folder");
        let folder = self
            .state()
            .join("reagent/sessions")
            .join(reagent_store::paths::cwd_key(&cwd));
        reagent_store::paths::session_dirs(&folder).unwrap_or_default()
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

    /// Starts the binary in a terminal that answers the program status query,
    /// the way a terminal with an inbox does.
    fn answering(home: &Home, args: &[&str]) -> Tui {
        Tui::spawn_answering(home.command().args(args), true)
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
    fn spawn(command: &mut Command) -> Tui {
        Tui::spawn_answering(command, false)
    }

    /// Runs `command` in a new pty and reads what it draws, answering the
    /// program status query when `answers` says the terminal knows it.
    #[expect(
        clippy::disallowed_types,
        reason = "a test drives a pty through the standard library"
    )]
    fn spawn_answering(command: &mut Command, answers: bool) -> Tui {
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
        let answer = answers.then(|| master.try_clone().expect("a terminal handle"));
        let parsed = Arc::clone(&screen);
        let written = Arc::clone(&bytes);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            // The last bytes of the read before, which a query may have started
            // in, and whether the query was answered.
            let mut held: Vec<u8> = Vec::new();
            let mut asked = false;
            loop {
                match output.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => {
                        parsed
                            .lock()
                            .expect("the screen lock")
                            .process(&buffer[..read]);
                        {
                            let mut bytes = written.lock().expect("the bytes lock");
                            if bytes.len() < DRAWN {
                                bytes.extend_from_slice(&buffer[..read]);
                            }
                        }
                        // A terminal that knows the program status protocol
                        // answers its query once, before the binary reports
                        // anything to it.
                        let Some(answer) = answer.as_ref() else {
                            continue;
                        };
                        held.extend_from_slice(&buffer[..read]);
                        if !asked
                            && held
                                .windows(PROGRAM_QUERY.len())
                                .any(|window| window == PROGRAM_QUERY.as_bytes())
                        {
                            (&*answer)
                                .write_all(PROGRAM_ANSWER.as_bytes())
                                .expect("the answer reaches the binary");
                            asked = true;
                        }
                        let keep = held.len().saturating_sub(QUERY_TAIL);
                        held.drain(..keep);
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

    /// How many times the bytes the binary wrote carry `sequence`.
    fn count(&self, sequence: &str) -> usize {
        let bytes = self.bytes.lock().expect("the bytes lock");
        bytes
            .windows(sequence.len())
            .filter(|window| *window == sequence.as_bytes())
            .count()
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

/// The id of a session folder: its name.
fn id_of(dir: &Path) -> String {
    dir.file_name()
        .and_then(|name| name.to_str())
        .expect("a session folder named by its id")
        .to_owned()
}

/// Opens the sessions sheet with Ctrl+X l and waits for its footer.
///
/// A key typed while the UI joins its worker can be lost, so the sheet is
/// asked for again from a screen that shows none.
async fn open_sessions(tui: &mut Tui) {
    let deadline = Instant::now() + WAIT;
    loop {
        tui.keys("\u{18}l");
        let tried = Instant::now() + Duration::from_secs(2);
        while Instant::now() < tried {
            if tui.text().contains("ctrl+d deletes") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            Instant::now() < deadline,
            "the sessions sheet never opened:\n{}",
            tui.text()
        );
        tui.keys("\u{1b}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The sessions sheet deletes a session of the folder: the first Ctrl+D asks
/// again, and the second takes the folder off the disk.
#[tokio::test]
async fn the_sessions_sheet_deletes_a_session() {
    let home = Home::new();
    let provider = home.script("delete", r#"{"replies": [{"text": "an old reply"}]}"#);
    let run = home
        .headless(&["-p", "an old turn", "--provider", &provider])
        .await;
    assert!(
        run.status.success(),
        "exit {} stderr {}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
    let saved = home.sessions().await;
    assert_eq!(saved.len(), 1, "the run saved one session");
    let old = saved.into_iter().next().expect("the saved session");
    let old_id = id_of(&old);

    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;
    open_sessions(&mut tui).await;
    // The rows read newest first, so the old session is the last one, however
    // many rows this run's own session adds above it.
    tui.keys("\u{1b}[B\u{1b}[B\u{1b}[B");
    tui.keys("\u{4}");
    tui.until("again deletes").await;
    tui.keys("\u{4}");
    tui.until("deleted").await;

    assert!(
        !old.exists(),
        "the folder of {old_id} is gone: {}",
        old.display()
    );
    let left: Vec<String> = home.sessions().await.iter().map(|dir| id_of(dir)).collect();
    assert!(
        !left.contains(&old_id),
        "the list names {old_id} no more: {left:?}"
    );

    // Esc and Ctrl+C in one read are Alt+Ctrl+C, which does not arm the quit.
    tui.keys("\u{1b}");
    tui.until_gone("ctrl+d deletes").await;
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// The sessions sheet keeps a session a fork reads: the host's refusal is a
/// note, and the parent's folder stays.
#[tokio::test]
async fn the_sessions_sheet_keeps_a_fork_parent() {
    let home = Home::new();
    let provider = home.script("fork", r#"{"replies": [{"text": "one"}, {"text": "two"}]}"#);
    let run = home.headless(&["-p", "one", "--provider", &provider]).await;
    assert!(
        run.status.success(),
        "exit {} stderr {}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
    let saved = home.sessions().await;
    assert_eq!(saved.len(), 1, "the run saved the parent");
    let parent = saved.into_iter().next().expect("the parent's folder");
    let reference = format!("{}@1", id_of(&parent));
    let run = home
        .headless(&["--fork", &reference, "-p", "two", "--provider", &provider])
        .await;
    assert!(
        run.status.success(),
        "exit {} stderr {}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
    let saved = home.sessions().await;
    assert_eq!(saved.len(), 2, "the parent and its fork");
    let fork = saved
        .into_iter()
        .find(|dir| *dir != parent)
        .expect("the fork's folder");

    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;
    open_sessions(&mut tui).await;
    // The rows read newest first: the fork is above its parent, which is the
    // last row.
    tui.keys("\u{1b}[B\u{1b}[B\u{1b}[B");
    tui.keys("\u{4}");
    tui.until("again deletes").await;
    tui.keys("\u{4}");
    tui.until("a fork reads it").await;

    assert!(
        parent.exists(),
        "the parent's folder stays: {}",
        parent.display()
    );
    assert!(fork.exists(), "the fork's folder stays: {}", fork.display());

    // Esc and Ctrl+C in one read are Alt+Ctrl+C, which does not arm the quit.
    tui.keys("\u{1b}");
    tui.until_gone("ctrl+d deletes").await;
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// A worker of another session of this home, with one client of it.
///
/// The client sends the few requests an approval test needs: `initialize`,
/// `attach`, `send` and `approve`. Nothing else answers its frames.
struct Second {
    /// The worker's process, which stops with the test.
    _child: Child,
    /// The client's connection to it.
    stream: tokio::net::UnixStream,
    /// The bytes read, of which the first `at` are used.
    buf: Vec<u8>,
    at: usize,
    /// The id of the next request the client sends.
    next: u64,
}

/// How many bytes one read of the second client asks for.
const SECOND_CHUNK: usize = 8192;

impl Second {
    /// Starts a worker of a session of its own with `provider`, attaches a
    /// client to it and answers its frames.
    async fn start(home: &Home, provider: &str) -> Second {
        let mut child = home
            .command()
            .args(["worker", "--provider", provider])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the second worker starts");
        let mut stdout = tokio::io::BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut ready = String::new();
        stdout
            .read_line(&mut ready)
            .await
            .expect("the ready line of the second worker");
        let ready: serde_json::Value =
            serde_json::from_str(ready.trim_end()).expect("the ready line is JSON");
        let socket = PathBuf::from(ready["socket"].as_str().expect("a socket path"));
        let session = ready["session"].as_str().expect("a session id").to_owned();
        let token = token_of(home, &session).await;
        let stream = tokio::net::UnixStream::connect(&socket)
            .await
            .expect("the second worker accepts a client");
        let mut second = Second {
            _child: child,
            stream,
            buf: Vec::new(),
            at: 0,
            next: 0,
        };
        let request = second
            .request(
                "initialize",
                &format!(
                    "{{\"major\":1,\"client\":{{\"name\":\"second\",\"version\":\"1\"}},\
                     \"capabilities\":[],\"token\":\"{token}\"}}"
                ),
            )
            .await;
        let answer = second.answer(request).await;
        let committed = answer["result"]["committed"].as_u64().expect("a record");
        let request = second
            .request("attach", &format!("{{\"from\":{committed}}}"))
            .await;
        second.answer(request).await;
        second
    }

    /// Sends one request and returns the id it carries.
    async fn request(&mut self, method: &str, params: &str) -> u64 {
        self.next += 1;
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"{method}\",\"params\":{params}}}\n",
            self.next
        );
        self.stream
            .write_all(line.as_bytes())
            .await
            .expect("the request reaches the worker");
        self.next
    }

    /// The next frame of the worker.
    async fn frame(&mut self) -> serde_json::Value {
        loop {
            if let Some(len) = self.buf[self.at..].iter().position(|byte| *byte == b'\n') {
                let start = self.at;
                self.at += len + 1;
                let line = self.buf[start..start + len].to_vec();
                return serde_json::from_slice(&line).expect("a frame is JSON");
            }
            if self.at > SECOND_CHUNK {
                self.buf.drain(..self.at);
                self.at = 0;
            }
            let mut chunk = [0u8; SECOND_CHUNK];
            let read = tokio::time::timeout(WAIT, self.stream.read(&mut chunk))
                .await
                .expect("a frame in time")
                .expect("the connection reads");
            assert!(read > 0, "the worker closed the connection");
            self.buf.extend_from_slice(&chunk[..read]);
        }
    }

    /// The answer to the request `id`, with the frames before it dropped.
    async fn answer(&mut self, id: u64) -> serde_json::Value {
        loop {
            let frame = self.frame().await;
            if frame["id"].as_u64() == Some(id) {
                return frame;
            }
        }
    }

    /// Sends a message and returns the id of the approval its run asks for.
    async fn asking(&mut self, text: &str) -> u64 {
        self.request(
            "send",
            &format!("{{\"text\":\"{text}\",\"mode\":\"queue\"}}"),
        )
        .await;
        loop {
            let frame = self.frame().await;
            if frame["method"] == "approval_request" {
                return frame["params"]["id"].as_u64().expect("an approval id");
            }
        }
    }

    /// Answers the approval `id` once.
    async fn answer_approval(&mut self, id: u64) {
        let request = self
            .request("approve", &format!("{{\"id\":{id},\"answer\":\"once\"}}"))
            .await;
        self.answer(request).await;
    }
}

/// The token of the worker of `session`, once its entry is in the folder.
async fn token_of(home: &Home, session: &str) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(entry) = home
            .workers()
            .await
            .into_iter()
            .find(|entry| entry.session == session)
        {
            return entry.token;
        }
        assert!(
            Instant::now() < deadline,
            "no entry of {session}: {:?}",
            home.workers().await
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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

/// A terminal that answers the program status query hears the session's status:
/// blocked on the card that waits, with the id of the session it is for, and
/// the session is cleared when the app exits.
#[tokio::test]
async fn program_status_reports_blocked_to_a_terminal_that_answers() {
    let home = Home::new();
    home.ask();
    let provider = home.script(
        "two",
        r#"{"replies": [{"calls": [{"name": "bash", "arguments": {"command": "echo hi > ran.txt"}}]},
            {"text": "the call ran"}]}"#,
    );
    let mut tui = Tui::answering(&home, &["--provider", &provider]);
    tui.until("idle").await;
    assert!(
        tui.drew(PROGRAM_QUERY),
        "the terminal is asked for its status: {}",
        tui.drawn()
    );

    tui.keys("run it\r");
    tui.until("Run this command?").await;
    // The status the inbox of the terminal lists reagent by: the session waits
    // on the card, for this session's id.
    let blocked = "\x1b]7501;state=blocked:app=reagent:kind=permission:msg=";
    let deadline = Instant::now() + WAIT;
    while !tui.drew(blocked) {
        assert!(
            Instant::now() < deadline,
            "the report of the card never went out: {}",
            tui.drawn()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tokio::time::sleep(Duration::from_millis(400)).await;
    tui.keys("y");
    tui.until("the call ran").await;
    assert_eq!(tui.quit().await, 0, "a normal quit");
    assert_eq!(
        tui.count("\x1b]7501;state=clear:app=reagent"),
        1,
        "the exit clears the entry once: {}",
        tui.drawn()
    );
    assert!(home.no_worker().await, "the worker stopped on its own");
}

/// A terminal that does not answer the program status query is told nothing at
/// all: only a terminal that asked hears a status.
#[tokio::test]
async fn program_status_says_nothing_to_a_terminal_that_does_not_answer() {
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
    tui.until("Run this command?").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    tui.keys("y");
    tui.until("the call ran").await;
    assert_eq!(tui.quit().await, 0, "a normal quit");
    assert!(
        tui.drew(PROGRAM_QUERY),
        "the terminal is asked for its status: {}",
        tui.drawn()
    );
    assert!(
        !tui.drew("\x1b]7501;state="),
        "no report goes out to a terminal that did not answer: {}",
        tui.drawn()
    );
    assert!(home.no_worker().await, "the worker stopped on its own");
}

/// The `ask` tool's card shows the question and its options, Down moves the pick,
/// Enter answers it, and the run goes on with the answer in the history.
#[tokio::test]
async fn ask_a_question_card_sends_the_pick() {
    let home = Home::new();
    let provider = home.script(
        "two",
        r#"{"replies": [{"calls": [{"name": "ask", "arguments": {"questions": [
            {"q": "Which database?", "options": [{"label": "sqlite"}, {"label": "postgres"}]}]}}]},
            {"text": "the answer came back"}]}"#,
    );
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    tui.keys("ask me\r");
    tui.until("? reagent asks").await;
    tui.until("Which database?").await;
    tui.until("1 sqlite").await;
    tui.until("2 postgres").await;
    tui.until("type your own answer...").await;

    // The card takes keys only once it has been on screen a moment.
    tokio::time::sleep(Duration::from_millis(400)).await;
    tui.keys("\u{1b}[B\r");
    tui.until("the answer came back").await;
    tui.until("? Which database? -> postgres").await;
    tui.until_gone("type your own answer...").await;

    assert_eq!(tui.quit().await, 0, "a normal quit");
    assert!(home.no_worker().await, "the worker stopped on its own");
}

/// Esc on the card skips the whole request, and the run goes on without an
/// answer.
#[tokio::test]
async fn ask_esc_skips_the_question() {
    let home = Home::new();
    let provider = home.script(
        "two",
        r#"{"replies": [{"calls": [{"name": "ask", "arguments": {"questions": [
            {"q": "Which database?", "options": [{"label": "sqlite"}, {"label": "postgres"}]}]}}]},
            {"text": "the answer came back"}]}"#,
    );
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    tui.keys("ask me\r");
    tui.until("Which database?").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    tui.keys("\u{1b}");
    tui.until("the answer came back").await;
    tui.until("? Which database? -> skipped").await;

    assert_eq!(tui.quit().await, 0, "a normal quit");
    assert!(home.no_worker().await, "the worker stopped on its own");
}

/// Another session whose run waits for an approval marks the idle TUI at once,
/// and the mark goes when that run's client answers.
#[tokio::test]
async fn approval_mark_another_session_shows_while_idle() {
    let home = Home::new();
    home.ask();
    let provider = home.script("one", r#"{"replies": [{"text": "unused"}]}"#);
    let asking = home.script(
        "asking",
        r#"{"replies": [{"calls": [{"name": "bash", "arguments": {"command": "echo hi"}}]},
            {"text": "the call ran"}]}"#,
    );
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    // The count is the sessions panel's own, drawn in the header row the panels
    // fold into at this terminal's width. No key goes to the TUI from here on.
    let mut second = Second::start(&home, &asking).await;
    let id = second.asking("run it").await;
    let started = Instant::now();
    tui.until("1 waits").await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the mark took {:?}",
        started.elapsed()
    );

    second.answer_approval(id).await;
    let started = Instant::now();
    tui.until_gone("1 waits").await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the mark took {:?} to go",
        started.elapsed()
    );
    assert!(
        !tui.rows()[0].contains("waits"),
        "the count left the header row:\n{}",
        tui.text()
    );

    assert_eq!(tui.quit().await, 0, "a normal quit");
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

/// `/tidy go` sends the body of the skill's file and the text typed after the
/// command as the message of the turn, so the model's reply follows them in the
/// transcript.
#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "a test writes the skill its project folder holds"
)]
async fn skill_command_sends_the_skill_file_to_the_model() {
    let home = Home::new();
    let provider = home.script("skill", r#"{"replies": [{"text": "a reply"}]}"#);
    let folder = home.cwd().join(".agents/skills/tidy");
    std::fs::create_dir_all(&folder).expect("the skill folder");
    std::fs::write(
        folder.join("SKILL.md"),
        "---\nname: tidy\ndescription: keep the imports tidy\n---\n\nTIDY-BODY\n",
    )
    .expect("the skill file");
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    tui.keys("/tidy go");
    tui.keys("\r");
    tui.until("TIDY-BODY").await;
    // The reply means the message reached the provider, which got the block the
    // user never typed, and the words they did follow it.
    tui.until("a reply").await;
    let rows = tui.rows();
    assert!(
        rows.iter()
            .any(|row| row.contains("<skill") && row.contains("name=\"tidy\"")),
        "the block names the skill:\n{}",
        tui.text()
    );
    assert!(
        rows.iter().any(|row| row.contains("</skill>")),
        "the block ends after the body:\n{}",
        tui.text()
    );
    assert!(
        rows.iter().any(|row| row.trim() == "go"),
        "the typed text follows the block:\n{}",
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

/// The keys file of the config folder moves a key of the real UI: the leader
/// leaves Ctrl+X, and the key after it opens the sessions sheet.
#[tokio::test]
async fn keys_file_moves_a_key_of_the_real_ui() {
    let home = Home::new();
    home.keys_toml("[keys]\nleader = \"ctrl+z\"\n[leader]\nsessions = \"s\"\n");
    let provider = home.script("keys", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider]);
    tui.until("idle").await;

    // Ctrl+X is the leader it moved away from: the file frees it, so the key
    // after it opens nothing.
    tui.keys("\u{18}s");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !tui.text().contains("Sessions"),
        "the old leader key opened the sheet:\n{}",
        tui.text()
    );

    // Ctrl+Z is the leader now, and s is the sessions key.
    tui.keys("\u{1a}s");
    tui.until("Sessions").await;
    tui.keys("\u{1b}");
    tui.until_gone("Sessions").await;

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

/// Ctrl+X t lists the words the plan of the model takes, and a pick reaches the
/// worker, whose settings notice draws the level in the Context panel.
#[tokio::test]
async fn the_level_sheet_lists_and_sets_the_levels_of_the_model() {
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
    // The window is unique to this model: the Context panel reading it is how
    // the test knows the session runs `mine/fast`, whose plan the sheet reads.
    tokio::fs::write(
        cache.join("mine.json"),
        r#"{"fetched_at":1,"models":[{"id":"fast","context_window":111000,"max_output":null,
            "reasoning":{"levels":["low","high"],"off":true,"default":"high"}}]}"#,
    )
    .await
    .expect("the cache file");

    let provider = home.script("levels", r#"{"replies": [{"text": "unused"}]}"#);
    let mut command = home.command();
    command.env("XDG_CACHE_HOME", home.root.path().join("cache"));
    command.args(["--provider", provider.as_str()]);
    let mut tui = Tui::spawn(&mut command);
    tui.until("idle").await;

    // A pick of `mine/fast` in the model sheet makes it the model the session
    // runs, so the level sheet has a plan to read.
    tui.keys("\u{18}m");
    tui.until("mine/fast").await;
    tui.keys("\r");
    panels_show(&mut tui, "0 / 111k").await;

    tui.keys("\u{18}t");
    tui.until("3 levels").await;
    let text = tui.text();
    for word in ["Level", "off", "low", "high"] {
        assert!(text.contains(word), "the sheet lists {word:?}:\n{text}");
    }

    // The selection starts on the row of the level in use, which is no row of
    // the sheet yet, so it starts on `off` and one Down lands on `low`.
    tui.keys("\u{1b}[B");
    tui.keys("\r");
    panels_show(&mut tui, "low · default").await;

    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// A session started with `--reasoning high` marks that level in the sheet,
/// though no `settings` notice has named it yet.
#[tokio::test]
async fn the_level_sheet_marks_the_level_the_session_started_with() {
    let home = Home::new();
    let provider = home.script("level", r#"{"replies": [{"text": "unused"}]}"#);
    let mut tui = Tui::start(&home, &["--provider", &provider, "--reasoning", "high"]);
    tui.until("idle").await;

    tui.keys("/level\r");
    tui.until("3 levels").await;
    let text = tui.text();
    assert!(
        text.lines()
            .any(|line| line.contains("high") && line.contains("now")),
        "the high row is marked:\n{text}"
    );

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

/// A `read` of an image shows the picture in the transcript, with the chip row
/// that names its size under it, and the call's own row above that.
#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "a test copies the picture into its own project folder"
)]
async fn picture_of_a_read_shows_in_the_transcript() {
    let home = Home::new();
    let provider = home.script(
        "picture",
        r#"{"replies": [
            {"text": "reading the picture", "calls": [
                {"name": "read", "arguments": {"path": "picture.png"}}
            ]},
            {"text": "the picture is on the screen"}
        ]}"#,
    );
    // The image crate's fixture: a gradient of 64 by 48 pixels.
    let png = include_bytes!("../../../image/tests/fixtures/gradient-64x48.png");
    std::fs::write(home.cwd().join("picture.png"), png).expect("the picture");

    let mut tui = Tui::spawn(
        home.terminal("Apple_Terminal")
            .args(["--provider", &provider]),
    );
    tui.until("idle").await;
    tui.keys("read the picture\r");
    tui.until("64x48").await;
    tui.until("the picture is on the screen").await;
    let screen = tui.text();
    assert!(
        screen.contains("▣ image 64x48"),
        "the chip row of the picture:\n{screen}"
    );
    assert!(
        screen.contains('\u{2580}'),
        "the picture draws its cells:\n{screen}"
    );

    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// In a terminal that takes the iTerm2 protocol, the image of a read goes out
/// over the picture's own cells: every half-block cell goes blank under it, and
/// no row of text around it does.
#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "a test copies the picture into its own project folder"
)]
async fn picture_native_image_covers_its_cells() {
    let home = Home::new();
    let provider = home.script(
        "picture",
        r#"{"replies": [
            {"text": "reading the picture", "calls": [
                {"name": "read", "arguments": {"path": "picture.png"}}
            ]},
            {"text": "the picture is on the screen"}
        ]}"#,
    );
    // A 16 by 8 gradient, so the whole picture fits the test's terminal.
    let png: &[u8] = b"\
        \x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x00\
        \x10\x00\x00\x00\x08\x08\x02\x00\x00\x00\x7f\x14\xe8\xc0\x00\x00\x00\xd5\x49\
        \x44\x41\x54\x78\xda\x15\xcf\x51\x01\x00\x21\x08\x04\x51\x22\x18\x81\x08\x46\
        \x20\x82\x11\x88\x60\x84\x89\x60\x04\x22\x18\x81\x08\x46\x20\x82\x11\xee\xe4\
        \x7b\xdf\x02\x22\x42\x13\x54\xe8\x82\x09\x43\x70\x61\x0a\x08\x4b\x08\x61\x0b\
        \x29\x1c\xa1\x84\x2b\x88\x28\x4d\x51\xa5\x2b\xa6\x0c\xc5\x95\xa9\xa0\x2c\x25\
        \x94\xad\xa4\x72\x94\x52\xae\xfe\xc0\x68\x86\x1a\xdd\x30\x63\x18\x6e\x4c\x03\
        \x63\x19\x61\x6c\x23\x8d\x63\x94\x71\xed\x07\x4e\x73\xd4\xe9\x8e\x39\xc3\x71\
        \x67\x3a\x38\xcb\x09\x67\x3b\xe9\x1c\xa7\x9c\xeb\x3f\x80\xf6\xf6\xd3\x5f\x29\
        \xe3\x25\x99\xbc\x59\x10\xb0\x21\xe1\x40\xc1\xfd\x5f\x93\xa0\x05\x1a\xf4\xc0\
        \x82\x11\x78\x30\xe3\x65\x57\x10\xc1\x0e\x32\x38\x41\x05\x37\x7e\x90\xb4\x44\
        \x93\x9e\x58\x32\x12\x4f\x66\xbe\xda\x95\x44\xb2\x93\x4c\x4e\x52\xc9\xcd\x1f\
        \x14\xad\xd0\xa2\x17\x56\x8c\xc2\x8b\x59\xef\x82\x55\x44\xb1\x8b\x2c\x4e\x51\
        \xc5\x2d\x3e\xc2\x81\xb4\x01\xff\x25\xbb\xd8\x00\x00\x00\x00\x49\x45\x4e\x44\
        \xae\x42\x60\x82";
    std::fs::write(home.cwd().join("picture.png"), png).expect("the picture");

    let mut tui = Tui::spawn(home.terminal("WezTerm").args(["--provider", &provider]));
    tui.until("idle").await;
    tui.keys("read the picture\r");
    tui.until("the picture is on the screen").await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let drawn = tui.bytes.lock().expect("the bytes lock").clone();
        if drawn.windows(6).any(|window| window == b"]1337;") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no image went out:\n{}",
            tui.text()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let screen = tui.text();
    assert!(
        !screen.contains('\u{2580}'),
        "every cell under the image went blank:\n{screen}"
    );
    for text in [
        "reading the picture",
        "image 16x8",
        "the picture is on the screen",
    ] {
        assert!(screen.contains(text), "{text} stays whole:\n{screen}");
    }
    assert_eq!(tui.quit().await, 0, "a normal quit");
}

/// In a terminal that takes kitty's placeholders, the image of a read goes out
/// once and the picture's rows in the ring are placeholder cells, so the
/// terminal scrolls and repaints them as it does any other text.
#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "a test copies the picture into its own project folder"
)]
async fn picture_placeholders_send_the_image_once() {
    let home = Home::new();
    let provider = home.script(
        "picture",
        r#"{"replies": [
            {"text": "reading the picture", "calls": [
                {"name": "read", "arguments": {"path": "picture.png"}}
            ]},
            {"text": "the picture is on the screen"}
        ]}"#,
    );
    let png = include_bytes!("../../../image/tests/fixtures/gradient-64x48.png");
    std::fs::write(home.cwd().join("picture.png"), png).expect("the picture");

    let mut tui = Tui::spawn(home.terminal("rio").args(["--provider", &provider]));
    tui.until("idle").await;
    tui.keys("read the picture\r");
    tui.until("the picture is on the screen").await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !tui.drew("\x1b_Ga=T,f=100,") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no image went out:\n{}",
            tui.text()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        tui.count("\x1b_Ga=T,f=100,"),
        1,
        "the terminal gets the image once"
    );
    let screen = tui.text();
    assert!(
        screen.contains('\u{10EEEE}'),
        "the picture's rows are placeholder cells:\n{screen}"
    );
    assert!(
        !screen.contains('\u{2580}'),
        "no half-block cells under them:\n{screen}"
    );
    assert_eq!(tui.quit().await, 0, "a normal quit");
    assert!(tui.drew("\x1b_Ga=d,d=I,"), "the image goes with the screen");
}
