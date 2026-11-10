//! The TUI app: transcript, approval box, editor and status line, driven by
//! terminal input and worker frames.
//!
//! The painting of a frame is in [`paint`], one module per region: the frame
//! order, the transcript and its live entries, the approval card, the text box
//! and the status line.
//!
//! The transcript is an inline one: finished entries are printed once into the
//! terminal's scrollback and only the entries that still grow stay in the live
//! area, above the approval box, the editor and the status line. The live area
//! follows its content: its first row sits right under the last committed line
//! and the rows it gives back are erased, so a closed box or a committed reply
//! leaves no band. It grows past the bottom of the screen by taking rows from
//! the transcript, which moves into the scrollback.
//!
//! In the full-screen mode the same finished entries go into the transcript
//! ring of [`Screen::full`] instead: the main area of [`crate::layout`] holds the
//! rows the viewport shows, the live rows, the box, the editor and the status
//! line follow under them, and PageUp, PageDown, Shift+Up and Shift+Down scroll
//! the ring. A resize lays the whole terminal out again.
//!
//! Nothing allocates per steady frame or per delta: a text delta goes straight
//! into its reply's [`Markdown`] buffer, every item keeps its finished lines,
//! the frame bytes and the commit runs are reused, and the status line is
//! rebuilt only when it changes. One `tokio::select!` waits on the terminal,
//! the socket, the signals and, only while something is dirty, one frame tick.

mod commands;
mod fold;
mod full;
mod list;
mod mouse;
mod paint;
mod sheet;
mod sidebar;
mod term;

#[cfg(test)]
mod tests;

pub use term::Real;

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use reagent_ai::{AssistantMessage, Content, Message, ToolCall, ToolResult};
use reagent_protocol::message::{AnswerKind, Mode, PluginCommand};
use serde::Deserialize;

use crate::client::{Approval, Client, Frame, Initialized, Next};
use crate::editor::{Editor, IMAGE_LABEL};
use crate::highlight::{self, Lang};
use crate::history::{self, RecentMessages, Tail};
use crate::host::{Files, Host, HostFuture, LoginStep, Models, Opened, Target};
use crate::input::Background;
use crate::keyfile::{self, Foot, Section};
use crate::keys::{self, Action, Choice, EditKey, HINTS, Keys, Leader, Overlay, quoted_char};
use crate::layout::{self, Rect};
use crate::markdown::Markdown;
use crate::mouse::Mouse;
use crate::panels::{ContextPanel, SessionRow, SessionsPanel};
use crate::picture::{self, Picture, Source as PictureSource, StyleBudget};
use crate::picture_native::{self, Native, Protocol};
use crate::screen::{Color, SYNC_PAIR, Screen, Style, Terminal};
use crate::slot::{Sidebar, Update};
use crate::theme::{Depth, Emphasis, Overrides, Role, Theme};
use crate::watch::Watch;

use self::fold::Folds;
use self::list::{List, Source};
use self::paint::{
    Block, CompactedRow, Diff, DiffKind, Entry, GroupRow, LineSink, SPANS, Span, SpanRole, State,
    ToolRow, Usage, pane_width,
};
use self::sheet::Sheet;
use self::sheet::login::Login;
use self::sheet::{PreviewRow, level, model, sessions};

/// How long a frame waits for the frames that stream in behind it.
pub const FRAME: Duration = Duration::from_millis(16);

/// The id of the Sessions panel, whose rows open a session and whose footer
/// turns its page.
const SESSIONS: &str = "sessions";

/// What the transcript says when the turn limit ended a run.
const TURN_LIMIT_LINE: &str =
    "stopped at the turn limit; the task may not be done; send a message to go on";

/// How many rows the editor shows before it scrolls.
pub const EDITOR_ROWS: u16 = 6;

/// How long an approval box is on screen before its keys answer it, so a key
/// the user was already typing, or a key repeat, runs no tool.
pub const APPROVAL_ARMED: Duration = Duration::from_millis(300);

/// How many bytes of the model's reasoning are kept for its preview line.
const THINKING_MAX: usize = 4096;

/// How many bytes of a call's output the transcript keeps for its row.
const OUTPUT_KEEP: usize = 4096;

/// How long a panel that wants a tick waits for the next one.
const TICK: Duration = Duration::from_secs(1);

/// How many times a client that the worker cut for falling behind reconnects
/// within [`RESYNC_WINDOW`] before it gives up on a worker that keeps cutting
/// it.
const RESYNCS: usize = 5;

/// The window the resyncs are counted in.
const RESYNC_WINDOW: Duration = Duration::from_secs(10);

/// The columns the sessions sheet needs before it shows the preview beside its
/// rows; the draw splits the sheet at the same width.
const PREVIEW_COLUMNS: u16 = 100;

/// The blank columns the transcript keeps at each side of the main area in full
/// screen. The text box, the cards, the lists and the status line keep the whole
/// width.
const MARGIN: u16 = 2;

/// The permission modes `/mode` takes, as the worker names them.
const MODES: [&str; 4] = ["default", "accept-edits", "bypass", "auto"];

/// How many user messages the app remembers drawing from a live frame before
/// the journal read that commits them arrives.
const UNWRITTEN: usize = 32;

/// How many tool call ids the app remembers handing to the panels, so a journal
/// record of a call a live frame already drew does not reach them twice.
const FED_CALLS: usize = 64;

/// What the app needs to reach a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    /// The worker's socket.
    pub socket: PathBuf,
    /// The join token the worker printed.
    pub token: String,
    /// The folder of the session to read the earlier turns from, when
    /// continuing one.
    pub session_dir: Option<PathBuf>,
    /// The name this client reports to the worker.
    pub client_name: String,
    /// Whether the UI draws full screen: the mode the start chose, `false` for
    /// the inline one.
    pub full: bool,
    /// The price of the run's model in USD per million input and output
    /// tokens, or `None` when neither the config nor the model list prices it.
    pub prices: Option<(f64, f64)>,
    /// The state folder, which holds the session list and the discovery folder
    /// of the workers.
    pub state: Option<PathBuf>,
    /// The theme file the config's `theme` names, read once at start, or `None`
    /// for the theme the terminal's mode gives.
    pub theme: Option<PathBuf>,
    /// The keys file of the config folder, read once at start, or `None` for the
    /// table's own keys.
    pub keys: Option<PathBuf>,
    /// Where the terminal's answers to the first frame's two queries arrive, for
    /// a terminal that sends them; `None` draws in the mode the terminal's own.
    pub background: Option<Background>,
    /// The folder the session runs in, which the docked sidebar shows under
    /// the title; `None` shows no folder.
    pub cwd: Option<PathBuf>,
    /// The binary's file and model host, for the lists that read it; `None`
    /// leaves the file list closed.
    pub host: Option<HostHandle>,
}

/// The binary's [`Host`], shared by the app with the frames that read it.
///
/// The app compares two handles by the host they point at, so a test can give
/// it one and a restart gives it another.
#[derive(Clone)]
pub struct HostHandle(Arc<dyn Host>);

impl HostHandle {
    /// Wraps `host` for the app.
    pub fn new(host: Arc<dyn Host>) -> Self {
        HostHandle(host)
    }

    /// The host behind the handle.
    fn get(&self) -> &dyn Host {
        &*self.0
    }
}

impl std::fmt::Debug for HostHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostHandle(..)")
    }
}

impl PartialEq for HostHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// How the session ended, for the caller's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    /// The process exit code the caller should use.
    pub code: i32,
}

impl Exit {
    /// A session that ended the way the user asked.
    pub fn ok() -> Self {
        Exit { code: 0 }
    }
}

/// The look a session the app switches to draws with: its theme mode, the
/// depth, the theme file's colors and whether it draws color at all.
/// What the session the app switches to keeps from the last one: the look it
/// drew with, and the keys table it read at its start, which is read once for
/// the whole run.
struct Carried {
    /// The mode the last session drew in, its depth, its overrides and whether
    /// the terminal reported a color.
    look: (crate::theme::Mode, Depth, Overrides, bool),
    /// The keys the last session read.
    keys: Option<crate::keyfile::KeyFile>,
}

/// The session a sheet opened: its worker, the client already connected to it
/// and the look it draws with.
struct Switch {
    opened: Opened,
    carried: Carried,
    client: Client,
    init: Initialized,
}

/// Runs the app against the worker behind `options` until the user quits or
/// the session ends, and connects to the worker of another session when a
/// sheet opens one.
pub async fn run(mut options: Options, mut terminal: impl Terminal) -> io::Result<Exit> {
    let mut carried: Option<(Carried, Client, Initialized)> = None;
    loop {
        let (exit, next) = run_session(&mut options, &mut terminal, carried.take()).await?;
        let Some(next) = next else {
            return Ok(exit);
        };
        // The last session left the alternate screen the way a quit does; the
        // next one enters it again, with the look the last one had.
        carried = Some((next.carried, next.client, next.init));
        options.socket = next.opened.socket;
        options.token = next.opened.token;
        options.session_dir = next.opened.session_dir;
    }
}

/// Runs one session: the app's setup, its frames and its exit, and the worker
/// of another session when a sheet picked one.
async fn run_session(
    options: &mut Options,
    terminal: &mut impl Terminal,
    carried: Option<(Carried, Client, Initialized)>,
) -> io::Result<(Exit, Option<Switch>)> {
    let (carried, (mut client, init)) = match carried {
        Some((carried, client, init)) => (Some(carried), (client, init)),
        None => (
            None,
            Client::connect(&options.socket, &options.token, &options.client_name).await?,
        ),
    };
    let mut app = App::new(&init, terminal.size(), options.full);
    app.cell = terminal.cell();
    // A session the app switched to draws with the look the last one had, and
    // reads the keys the last one read.
    let looked = carried.is_some();
    if let Some(carried) = carried {
        let (mode, depth, overrides, color) = carried.look;
        app.look = Look::resolve(mode, depth, overrides, color, &mut app.screen);
        if let Some(file) = carried.keys {
            app.set_keyfile(file);
        }
    }
    app.state = options.state.clone();
    // The discovery folder of the live workers: a change there is a session
    // that started, ended or waits for an approval. A folder the watch cannot
    // open leaves every read of it as it was.
    let mut watch = app
        .state
        .as_deref()
        .and_then(|state| Watch::new(&reagent_protocol::discovery::folder(state)).ok());
    app.host = options.host.clone();
    if let Some(cwd) = &options.cwd {
        app.place = sidebar::shown(cwd, std::env::var("HOME").ok().as_deref());
    }
    if let Some(prices) = options.prices {
        app.set_prices(prices);
    }
    if let Some(model) = &init.model {
        app.panels_update(&Update::Model {
            name: model.as_ref(),
            window: init.context_window.unwrap_or(0),
        });
    }
    app.journal = match &options.session_dir {
        Some(dir) => Some(dir.clone()),
        None => session_folder(&init.session).await,
    };
    // The folder's sessions and the live workers, read while the journal is.
    app.read_sessions();
    // The attach answer names the record the worker had committed when it
    // attached, which can be later than the handshake's: a record committed
    // between the two is in neither the replay nor a read up to the handshake.
    let attached = client.attach(init.committed).await?;
    if let Some(dir) = app.journal.clone() {
        let room = app.room();
        match history::read(dir, attached.committed, room).await {
            Ok(mut tail) => {
                app.placeholder_tail(&mut tail).await;
                app.history(&mut tail);
            }
            Err(error) => app.note(format!("history: {error}")),
        }
    }
    app.read = attached.committed;
    app.drawn = attached.committed;
    app.pictured = attached.committed;
    app.picture_read = attached.committed;
    // The run may have ended while this client was away, and its `run_end` is in
    // a record the worker did not replay: the answer says whether one is open.
    app.set_running(attached.running);
    // The journal is read, so the worker replays the frames after the record it
    // committed: nothing between the handshake and now is lost.
    //
    // The alternate screen, when the mode draws full screen, goes up before the
    // first frame, and every exit path below writes the bytes that leave it. A
    // session the app switches to enters it again but only inherits the look:
    // the queries would answer for a terminal that has not changed.
    let mut background: Option<Background> = None;
    app.enter(terminal).await;
    if !looked {
        // The theme file, then the first frame's two queries: the frame waits only
        // for the device attributes answer, which every terminal gives, so it is
        // drawn in the mode the terminal can show within a few milliseconds, and a
        // late background answer draws one more frame in rich mode.
        background = options.background.take();
        // The keys file, read once for the whole run: a session the app
        // switches to keeps the table and its notes show once.
        app.read_keys(options.keys.take()).await;
        if let Some(color) = app
            .start(terminal, options.theme.take(), background.as_ref())
            .await
        {
            app.answered(color);
            background = None;
        }
    }
    let mut broken = app.draw(terminal).await.err();
    let mut next: Option<Switch> = None;
    let mut signals = Signals::new();
    while broken.is_none() {
        let until = app.deadline;
        tokio::select! {
            event = terminal.next_event() => {
                match event {
                    Ok(Event::Key(key)) => app.key(key),
                    Ok(Event::Mouse(mouse)) => app.mouse(mouse),
                    Ok(Event::Resize(columns, rows)) => {
                        app.cell = terminal.cell();
                        app.resize(columns, rows);
                    }
                    Ok(Event::Paste(text)) => app.paste(text),
                    Ok(Event::FocusGained) => app.focused(),
                    Ok(_) => {}
                    Err(error) => {
                        // The input side is gone, so every later read would fail
                        // at once: end the app instead of spinning on it.
                        app.note(format!("input: {error}"));
                        app.exit = true;
                    }
                }
            }
            // The answer to the background query, when it came late (the device
            // attributes answer ended the first frame's wait without it): rich
            // mode over it, one frame. No terminal asked, or it answered, and
            // this branch is a future that never resolves.
            () = App::late_answer(background.as_ref()) => {
                if let Some(color) = background.as_ref().and_then(Background::color) {
                    app.answered(color);
                    background = None;
                }
            }
            files = files_answer(&mut app.files_job.0) => {
                app.files = Some(files.paths);
                app.files_ready = true;
            }
            (id, exchange) = preview_answer(&mut app.preview_job) => app.exchange_arrived(id, exchange),
            models = models_answer(&mut app.waits.models) => app.models_arrived(models),
            step = login_answer(&mut app.waits.login) => app.login_step(step),
            opened = open_answer(&mut app.waits.open) => app.opened(opened),
            deleted = delete_answer(&mut app.waits.delete) => app.deleted(deleted.0, deleted.1, deleted.2),
            (request, image) = image_answer(&mut app.image_job) => app.image_pasted(request, image),
            found = read_answer(&mut app.reading) => app.sessions_arrived(found).await,
            changed = watch_answer(&mut watch) => match changed {
                Ok(()) => {
                    tokio::time::sleep(FOLDER_SETTLE).await;
                    app.read_sessions();
                }
                // A watch that stops working costs the live marks, which the
                // turns that end and the frames that arrive read as before.
                Err(error) => {
                    app.note(format!("folder: {error}"));
                    watch = None;
                }
            },
            frame = client.next_frame() => {
                match frame {
                    Ok(Next::Frame(frame)) => app.frame(frame),
                    Ok(Next::More) => {}
                    // A worker that cuts a client in the middle of a frame says
                    // nothing and closes the connection, so the app finds out
                    // what happened by connecting again.
                    Ok(Next::End) => app.ended(),
                    Err(error) => {
                        // The connection is gone, and the terminal still has to
                        // come back: the run ends by the same path as a quit.
                        app.note(format!("connection: {error}"));
                        app.code = 1;
                        app.exit = true;
                    }
                }
            }
            () = wait(until) => if let Err(error) = app.timer(terminal).await {
                broken = Some(error);
                break;
            },
            () = signals.recv() => signal_end(&mut app),
        }
        if std::mem::take(&mut app.files_ready) {
            app.files_arrived();
        }
        app.catch_up().await;
        app.flush(&mut client).await;
        // A sheet that opened another session: the app connects to its worker
        // first, so a worker it cannot reach leaves a note and this session
        // as it was. Then this worker is detached the way a quit detaches it,
        // and the loop leaves by the quit's path.
        if let Some(opened) = app.switch.take() {
            match Client::connect(&opened.socket, &opened.token, &options.client_name).await {
                Ok((next_client, next_init)) => {
                    let look = (
                        app.look.mode(),
                        app.look.depth,
                        app.look.overrides,
                        app.look.color,
                    );
                    let _ = client.detach().await;
                    next = Some(Switch {
                        opened,
                        carried: Carried {
                            look,
                            keys: app.keyfile.take(),
                        },
                        client: next_client,
                        init: next_init,
                    });
                    break;
                }
                Err(error) => {
                    app.note(format!("open: {error}"));
                    app.dirty = true;
                }
            }
        }
        if app.exit {
            break;
        }
        if let Some(why) = app.resync.take() {
            match app.reconnect(client, options).await {
                Ok(next) => client = next,
                Err(_) => {
                    // No worker behind the socket any more: the session is over,
                    // or the cut left nothing to rejoin.
                    let (note, code) = match why {
                        Resync::Behind => (
                            "cut off by the worker: run reagent -c to continue".to_owned(),
                            1,
                        ),
                        Resync::Closed => ("the session ended".to_owned(), 0),
                    };
                    app.note(note);
                    app.code = code;
                    break;
                }
            }
        }
        if app.needs_draw()
            && !terminal.input_ready()
            && let Err(error) = app.draw(terminal).await
        {
            broken = Some(error);
            break;
        }
    }
    // Every exit path, a switch to another session too, leaves the alternate
    // screen and prints the transcript before the report, and a write the
    // terminal refused is reported after it.
    let closed = app.finish(terminal).await;
    match broken {
        Some(error) => Err(error),
        None => {
            closed?;
            Ok((Exit { code: app.code }, next))
        }
    }
}

/// Fills the PNG payload of `picture` for a terminal that draws placeholders,
/// or drops its source when the image makes no PNG: the picture then keeps its
/// half-block cells.
async fn placeholder_picture(picture: &mut Picture) {
    let Some(source) = picture.source.as_ref() else {
        return;
    };
    if source.png {
        return;
    }
    let data = Arc::clone(&source.data);
    let made = tokio::task::spawn_blocking(move || picture_native::png_payload(&data))
        .await
        .ok()
        .flatten();
    let bytes = made.as_deref().map_or(0, picture::decoded_bytes);
    picture.source = made.map(|png| PictureSource {
        data: png.into(),
        bytes,
        png: true,
    });
}

/// The first image id of the run: one of the 24 bits a placeholder's color
/// names, taken from the process id so two runs in one terminal keep their
/// images apart.
fn first_image_id() -> u32 {
    1 + (std::process::id().wrapping_mul(2_654_435_761) & 0x00FF_FFFF) % 0x00FF_0000
}

/// The folder of the session `id`, whichever folder under the sessions root
/// holds it.
async fn session_folder(id: &str) -> Option<PathBuf> {
    let home = state_home()?;
    let root = reagent_store::paths::sessions_root(&home);
    let mut folders = tokio::fs::read_dir(&root).await.ok()?;
    while let Ok(Some(entry)) = folders.next_entry().await {
        let folder = entry.path().join(id);
        if tokio::fs::metadata(&folder)
            .await
            .is_ok_and(|meta| meta.is_dir())
        {
            return Some(folder);
        }
    }
    None
}

/// The unix seconds now, for the ages the sessions sheet shows.
fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The state folder the store keeps sessions under, found the way the cli
/// finds it.
fn state_home() -> Option<PathBuf> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        let state = PathBuf::from(state);
        if state.is_absolute() {
            return Some(state);
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
}

/// A terminate or a hangup ends the app, so the loop leaves by the same path as
/// a quit: the terminal is restored and the live area erased.
fn signal_end(app: &mut App) {
    app.exit = true;
}

/// How long the status line says what a mouse copy took.
const COPIED_FOR: Duration = Duration::from_secs(3);

/// How long a change of the discovery folder is left to settle: a worker
/// writes its entry under a temp name and renames it in, and one read takes
/// both.
const FOLDER_SETTLE: Duration = Duration::from_millis(50);

/// Waits until `at`, or forever when no frame is due.
async fn wait(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Waits for the paths the host is reading, and empties `slot` once they came;
/// stays pending while no read is in flight.
async fn files_answer(slot: &mut Option<HostFuture<Files>>) -> Files {
    // The job stays in its slot while it runs: another branch of the loop's
    // `select!` that fires first drops only this wait, not the job.
    let answer = match slot.as_mut() {
        Some(files) => files.await,
        None => std::future::pending().await,
    };
    *slot = None;
    answer
}

/// Waits for the messages the sessions sheet is reading, with the id of the
/// row it is for; stays pending while no read is in flight.
async fn preview_answer(
    slot: &mut Option<(
        Arc<str>,
        tokio::task::JoinHandle<io::Result<RecentMessages>>,
    )>,
) -> (Arc<str>, io::Result<RecentMessages>) {
    // The job stays in its slot while it runs, as the files' job does.
    let wait = match slot.as_mut() {
        Some((_, job)) => job.await,
        None => std::future::pending().await,
    };
    let answer = wait.unwrap_or_else(|error| Err(io::Error::other(error.to_string())));
    match slot.take() {
        Some((id, _)) => (id, answer),
        None => std::future::pending().await,
    }
}

/// Waits for the model list the host is reading; stays pending while no read is
/// in flight.
async fn models_answer(slot: &mut Option<HostFuture<Models>>) -> Models {
    // The job stays in its slot while it runs: another branch of the loop's
    // `select!` that fires first drops only this wait, not the job.
    let answer = match slot.as_mut() {
        Some(models) => models.await,
        None => std::future::pending().await,
    };
    *slot = None;
    answer
}

/// Waits for the next step of the login that runs; answers `None` when the
/// channel closed, and stays pending while no login runs.
async fn login_answer(
    slot: &mut Option<tokio::sync::mpsc::Receiver<LoginStep>>,
) -> Option<LoginStep> {
    match slot.as_mut() {
        Some(steps) => steps.recv().await,
        None => std::future::pending().await,
    }
}

/// The answer to the image paste in flight, with the request it ran for, and
/// never when none is in flight.
async fn image_answer(
    slot: &mut Option<ImageJob>,
) -> (ImageRequest, Result<Option<Pasted>, String>) {
    // The job stays in its slot while it runs, as the files' job does.
    let result = match slot.as_mut() {
        Some(job) => (&mut job.handle).await,
        None => std::future::pending().await,
    };
    let result = result.unwrap_or_else(|error| Err(format!("the image job stopped: {error}")));
    match slot.take() {
        Some(job) => (job.request, result),
        None => std::future::pending().await,
    }
}

/// The answer to the delete in flight, with the id and the title it started
/// with, and never when no delete is in flight.
async fn delete_answer(slot: &mut Option<Deleting>) -> (Arc<str>, Arc<str>, Result<(), String>) {
    // The job stays in its slot while it runs, so a further Ctrl+D sees it.
    let answer = match slot.as_mut() {
        Some(deleting) => (&mut deleting.job).await,
        None => std::future::pending().await,
    };
    match slot.take() {
        Some(deleting) => (deleting.id, deleting.title, answer),
        None => std::future::pending().await,
    }
}

/// Waits for the worker of the session the sheet picked; stays pending while no
/// open is in flight.
async fn open_answer(
    slot: &mut Option<HostFuture<Result<Opened, String>>>,
) -> Result<Opened, String> {
    // The job stays in its slot while it runs, as the files' job does.
    let answer = match slot.as_mut() {
        Some(open) => open.await,
        None => std::future::pending().await,
    };
    *slot = None;
    answer
}

/// Waits for the read of the folder's sessions, and empties `slot` once it
/// came; stays pending while no read is in flight.
async fn read_answer(
    slot: &mut Option<tokio::task::JoinHandle<Vec<reagent_store::list::Entry>>>,
) -> Vec<reagent_store::list::Entry> {
    // The job stays in its slot while it runs: another branch of the loop's
    // `select!` that fires first drops only this wait, not the job.
    let answer = match slot.as_mut() {
        Some(reading) => reading.await,
        None => std::future::pending().await,
    };
    *slot = None;
    answer.unwrap_or_default()
}

/// Waits for a change of the discovery folder; stays pending while no watch of
/// it runs.
async fn watch_answer(watch: &mut Option<Watch>) -> io::Result<()> {
    match watch.as_mut() {
        Some(watch) => watch.changed().await,
        None => std::future::pending().await,
    }
}

/// The signals that end the run: the terminal is restored and the app exits.
#[cfg(unix)]
#[derive(Debug, Default)]
struct Signals {
    term: Option<tokio::signal::unix::Signal>,
    hangup: Option<tokio::signal::unix::Signal>,
}

#[cfg(unix)]
impl Signals {
    /// Takes the two handlers.
    ///
    /// A process runs one signal driver, so a runtime that cannot have one (a
    /// test process with several runtimes, say) keeps going without them
    /// rather than failing to start.
    fn new() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        Signals {
            term: signal(SignalKind::terminate()).ok(),
            hangup: signal(SignalKind::hangup()).ok(),
        }
    }

    /// Waits for a terminate or a hangup.
    async fn recv(&mut self) {
        match (&mut self.term, &mut self.hangup) {
            (Some(term), Some(hangup)) => tokio::select! {
                _ = term.recv() => {}
                _ = hangup.recv() => {}
            },
            (Some(term), None) => {
                term.recv().await;
            }
            (None, Some(hangup)) => {
                hangup.recv().await;
            }
            (None, None) => std::future::pending().await,
        }
    }
}

#[cfg(not(unix))]
#[derive(Debug, Default)]
struct Signals;

#[cfg(not(unix))]
impl Signals {
    fn new() -> Self {
        Signals
    }

    async fn recv(&mut self) {
        std::future::pending().await
    }
}

/// What one key or one approval asks the client to send.
#[derive(Debug)]
enum Command {
    /// A message: a steer while a run goes, a queue that starts the next.
    Send { text: String, mode: Mode },
    /// Stop the current turn.
    Cancel,
    /// Answer an approval request.
    Approve {
        id: u64,
        answer: AnswerKind,
        reason: Option<String>,
    },
    /// Answer a question request: one list of picks or own text per question,
    /// and none at all to skip them.
    Answer {
        id: u64,
        answers: Option<Vec<Vec<String>>>,
    },
    /// Set the model, the reasoning level or the permission mode.
    Set {
        model: Option<String>,
        level: Option<String>,
        mode: Option<String>,
    },
    /// Compact the history into a checkpoint, with the user's note in it.
    Compact { note: Option<String> },
    /// Run one command a JS plugin offers, with the rest of the line as its
    /// argument.
    Plugin { name: String, args: String },
    /// Leave the session.
    Detach,
}

/// One entry of the transcript.
#[derive(Debug)]
enum Item {
    /// A streamed reply, rendered as markdown: its finished blocks are printed
    /// once `done` counts them, its live block stays in the live area.
    Reply { md: Box<Markdown>, done: usize },
    /// A tool call: one row with its target and result, and the diff under an
    /// edit.
    Tool(Box<Tool>),
    /// The model's reasoning: one dim row with the time it took, and its newest
    /// line while the block is open.
    Thought(Box<Thinking>),
    /// A bare line of the model's reasoning, for a caller that holds the text
    /// alone: its row draws the line with no time.
    #[allow(
        dead_code,
        reason = "the status line's preview test holds a line alone"
    )]
    Thinking(String),
    /// A run of tool calls, in one row until it opens.
    Group(Box<Group>),
    /// A user turn: `›` in the accent, the message bold.
    User { text: Arc<str> },
    /// The dim rule a turn ends with.
    Rule(Usage),
    /// A compaction: one dim row, open to the checkpoint text it wrote.
    Compacted(Box<Compacted>),
    /// A message that steers the run, above the text box.
    Steer { text: Arc<str>, queued: bool },
    /// Plain lines that never change again.
    Plain { lines: Vec<String>, dim: bool },
    /// One image of the session: its half-block cells and the chip under them.
    Picture(Box<Picture>),
    /// The one line an approval answer leaves, colored by the answer: `ok` for
    /// the add role of an allowed call, the error role for a refused one.
    Answer { line: String, ok: bool },
}

impl Item {
    /// Freezes the item: a reply commits its last block, so every line it holds
    /// is finished, and a tool call settles with the tail its open row draws.
    fn finish(&mut self) {
        match self {
            Item::Reply { md, .. } => md.finish(),
            Item::Tool(tool) => tool.finish(),
            Item::Thought(think) => think.finish(),
            Item::Thinking(_) => {}
            Item::Group(group) => {
                for member in &mut group.members {
                    member.finish();
                }
            }
            _ => {}
        }
    }

    /// The number the fold state knows the item's block by, `0` for a block the
    /// fold keys do not open.
    fn fold_id(&self) -> u64 {
        match self {
            Item::Tool(tool) => tool.fold,
            Item::Thought(think) => think.fold,
            Item::Thinking(_) => 0,
            Item::Group(group) => group.fold,
            Item::Compacted(row) => row.fold,
            _ => 0,
        }
    }
}

/// One compaction of the history: the tokens it took off, the messages its
/// checkpoint replaced, and the checkpoint text once the row is opened.
#[derive(Debug)]
struct Compacted {
    /// The prompt tokens before and after it, which a journal row leaves out.
    tokens: Option<(u64, u64)>,
    /// How many messages the checkpoint stands in for.
    messages: u32,
    /// The number the fold state knows the row by.
    fold: u64,
    /// The checkpoint text, read when the row opens.
    text: Option<Arc<str>>,
}

impl Compacted {
    /// The row the block draws: the tokens it took off and the messages its
    /// checkpoint stands in for, and its text once the journal read it.
    fn row(&self) -> CompactedRow<'_> {
        CompactedRow {
            tokens: self.tokens,
            messages: self.messages,
            text: self.text.as_deref(),
        }
    }
}

/// The model's reasoning of one turn: the newest line of it, and the time it
/// took.
#[derive(Debug)]
struct Thinking {
    /// The newest line of the reasoning.
    text: String,
    /// When the reasoning started, for the row's `thought for 4s`.
    since: Instant,
    /// The seconds it took, once it is over.
    seconds: u32,
    /// The number the fold state knows the row by.
    fold: u64,
}

impl Thinking {
    /// Freezes the time the reasoning took.
    fn finish(&mut self) {
        self.seconds = self.since.elapsed().as_secs().min(u64::from(u32::MAX)) as u32;
    }
}

/// One tool name of a group's row, with the number of calls that made it.
#[derive(Debug)]
struct Counted {
    /// The tool's name, as its first call wrote it.
    name: String,
    /// How many calls of the run made it.
    count: u32,
}

/// One member of a run: a call, or the reasoning that came after the run's first
/// call.
#[derive(Debug)]
enum Member {
    /// One tool call of the run.
    Call(Box<Tool>),
    /// The reasoning the run held between its calls.
    Thought(Box<Thinking>),
}

impl Member {
    /// Freezes the member: a call settles with the tail its open row draws.
    fn finish(&mut self) {
        match self {
            Member::Call(tool) => tool.finish(),
            Member::Thought(think) => think.finish(),
        }
    }
}

/// A run of tool calls, in one row until it opens: its calls and the thoughts
/// that came after its first call, in the order they came.
///
/// A run lasts from its first call until reply text starts, a user message
/// arrives or the run ends, so the calls of several turns meet here. What the
/// run did is counted as its members join, so a frame that draws the row adds
/// nothing up.
#[derive(Debug)]
struct Group {
    /// The members of the run, in the order they came.
    members: Vec<Member>,
    /// How many calls the run holds: the summary counts them and no thought.
    calls: u32,
    /// Whether the run still goes, which draws one row per call and no summary.
    going: bool,
    /// The tool names with their counts, in the order of their first call.
    names: Vec<Counted>,
    /// How many of the calls failed.
    failed: u32,
    /// Whether the run holds an edit with a diff.
    edits: bool,
    /// The lines its edits added.
    added: u64,
    /// The lines its edits removed.
    removed: u64,
    /// The language every one of its edits shares, when they share one.
    language: Option<Lang>,
    /// Whether its edits named two languages.
    mixed: bool,
    /// The lines the calls read.
    lines: u64,
    /// The number the fold state knows the row by.
    fold: u64,
}

impl Group {
    /// The run a sequence of finished `tools` draws, with the number the fold
    /// state knows it by.
    fn new(tools: Vec<Tool>, fold: u64) -> Self {
        let mut group = Group::empty(fold);
        group.extend(tools.into_iter().map(|tool| Member::Call(Box::new(tool))));
        group
    }

    /// A run that holds no member yet, drawn folded from its first frame.
    fn empty(fold: u64) -> Self {
        Group {
            members: Vec::new(),
            calls: 0,
            going: false,
            names: Vec::new(),
            failed: 0,
            edits: false,
            added: 0,
            removed: 0,
            language: None,
            mixed: false,
            lines: 0,
            fold,
        }
    }

    /// Adds `members` to the run in their order, counting what its calls did.
    fn extend(&mut self, members: impl IntoIterator<Item = Member>) {
        for member in members {
            if let Member::Call(tool) = &member {
                self.count(tool);
            }
            self.members.push(member);
        }
    }

    /// Counts what one call of the run did.
    fn count(&mut self, tool: &Tool) {
        match self
            .names
            .iter_mut()
            .find(|counted| counted.name == tool.name)
        {
            Some(counted) => counted.count = counted.count.saturating_add(1),
            None => self.names.push(Counted {
                name: tool.name.clone(),
                count: 1,
            }),
        }
        self.calls = self.calls.saturating_add(1);
        if tool.state == State::Failed {
            self.failed += 1;
        }
        self.lines += tool.lines;
        if tool.diff.is_empty() {
            return;
        }
        self.edits = true;
        self.added += tool.added;
        self.removed += tool.removed;
        match (self.language, tool.language) {
            (Some(first), Some(lang)) if first != lang => {
                self.mixed = true;
                self.language = None;
            }
            (None, Some(lang)) if !self.mixed => self.language = Some(lang),
            _ => {}
        }
    }

    /// The newest call of the run that made a diff, which draws its lines under
    /// the run's row while the run is folded.
    fn newest_edit(&self) -> Option<&Tool> {
        self.members.iter().rev().find_map(|member| match member {
            Member::Call(tool) if !tool.diff.is_empty() => Some(&**tool),
            _ => None,
        })
    }

    /// The row the block draws: the names of the calls with their counts, what
    /// the run did, and the members themselves, which the frame draws in their
    /// order while the group is open.
    fn row(&self) -> GroupRow<'_> {
        GroupRow {
            names: &self.names,
            failed: self.failed,
            edits: self.edits,
            added: self.added,
            removed: self.removed,
            language: self.language.map(|lang| lang.name()),
            lines: self.lines,
            calls: self.calls,
            going: self.going,
            members: self,
            newest: self.newest_edit(),
        }
    }
}

/// One span of a tool row's result, owned by the tool.
#[derive(Debug)]
struct OwnedSpan {
    /// The text, with the spaces between it and the span before it.
    text: String,
    /// The role it draws in.
    role: SpanRole,
}

/// One line of an edit's diff, owned by the tool.
#[derive(Debug)]
pub(crate) struct OwnedDiff {
    /// The line number in the file.
    pub(crate) number: u32,
    /// What the line did.
    pub(crate) kind: DiffKind,
    /// The line's text, without its `+` or `-`.
    pub(crate) text: String,
    /// The tokenizer's spans of the text, empty while its language is unknown.
    pub(crate) tokens: Vec<highlight::Span>,
}

impl OwnedDiff {
    /// The line as the row under a call draws it, borrowed in place: a frame
    /// that opens a diff copies none of its lines.
    pub(crate) fn line(&self) -> Diff<'_> {
        Diff {
            number: self.number,
            kind: self.kind,
            text: &self.text,
            tokens: &self.tokens,
        }
    }
}

/// One tool call of the transcript.
#[derive(Debug)]
struct Tool {
    /// The call id the frames name.
    id: String,
    /// The tool name.
    name: String,
    /// Its target: the path, the command, the pattern.
    target: String,
    /// What the call did.
    state: State,
    /// The result at the right edge of its row.
    right: [Option<OwnedSpan>; SPANS],
    /// The diff of an edit, under its row.
    diff: Vec<OwnedDiff>,
    /// The tail of the output the journal recorded for the call, when it read
    /// it: its last [`OUTPUT_KEEP`] bytes, from the start of a line.
    text: Option<String>,
    /// The output a failed call's frame carried, when it had one.
    error: Option<String>,
    /// The UTF-8 length of the output the model receives.
    bytes: usize,
    /// Whether the result arrived.
    done: bool,
    /// The lines of the result behind the right span, counted for a group.
    lines: u64,
    /// The lines the cut of the tail took off the output.
    earlier: u64,
    /// The lines the edit of the call adds, 0 for a call that edits nothing.
    added: u64,
    /// The lines it removes.
    removed: u64,
    /// The language of the file it edits, when this build knows it.
    language: Option<Lang>,
    /// The number the fold state knows the row by.
    fold: u64,
}

impl Tool {
    /// A call that just started.
    fn new(id: &str, name: &str) -> Self {
        Tool {
            id: id.to_owned(),
            name: name.to_owned(),
            target: String::new(),
            state: State::Running,
            right: [None, None, None],
            diff: Vec::new(),
            text: None,
            error: None,
            bytes: 0,
            done: false,
            lines: 0,
            earlier: 0,
            added: 0,
            removed: 0,
            language: None,
            fold: 0,
        }
    }

    /// Takes the call's arguments: the target its row shows, the language of
    /// the file it edits, and its diff.
    fn set_call(&mut self, name: &str, arguments: &str, width: u16) {
        self.name.clear();
        self.name.push_str(name);
        let call = Call::parse(name, arguments);
        let language = call.edit().and_then(|_| lang_of(&call.target));
        self.target.clear();
        self.target.push_str(call.target.as_ref());
        cut_in_place(&mut self.target, usize::from(width));
        self.right = [None, None, None];
        self.diff.clear();
        self.added = 0;
        self.removed = 0;
        self.language = language;
        if let Some(call) = call.edit() {
            self.added = call.added;
            self.removed = call.removed;
            let mut buf = [0u8; 16];
            let mut at = 0;
            if let Some(language) = language {
                self.right[0] = Some(OwnedSpan {
                    text: language.name().to_owned(),
                    role: SpanRole::Dim,
                });
                at = 1;
            }
            self.right[at] = Some(OwnedSpan {
                text: if at == 0 {
                    format!("+{}", digits(&mut buf, call.added))
                } else {
                    format!("  +{}", digits(&mut buf, call.added))
                },
                role: SpanRole::Add,
            });
            self.right[at + 1] = Some(OwnedSpan {
                text: format!(" -{}", call.removed),
                role: SpanRole::Delete,
            });
            self.tokenize(call, language);
        }
    }

    /// Fills the diff with the lines of `call`, each line's tokens kept beside
    /// its text: the state of a block comment or a string carries down the old
    /// lines and the new lines apart, and a context line moves both.
    fn tokenize(&mut self, call: &Edit<'_>, language: Option<Lang>) {
        let mut old = highlight::State::default();
        let mut new = highlight::State::default();
        let mut spans = Vec::new();
        for (number, kind, text) in call.diff().take(DIFF_MAX) {
            let mut tokens = Vec::new();
            if let Some(language) = language {
                match kind {
                    DiffKind::Delete => highlight::tokenize(&language, text, &mut old, &mut spans),
                    DiffKind::Add => highlight::tokenize(&language, text, &mut new, &mut spans),
                    DiffKind::Context => {
                        highlight::tokenize(&language, text, &mut old, &mut spans);
                        new = old;
                    }
                }
                tokens.extend_from_slice(&spans);
            }
            self.diff.push(OwnedDiff {
                number,
                kind,
                text: text.to_owned(),
                tokens,
            });
        }
    }

    /// The result arrived: the row's right span says what it did, and a failed
    /// call shows its chip.
    fn set_result(&mut self, bytes: usize, error: Option<&str>, width: u16) {
        self.done = true;
        self.bytes = bytes;
        if let Some(error) = error
            && !error.is_empty()
        {
            self.error = Some(error.to_owned());
        }
        self.state = if self.error.is_some() {
            State::Failed
        } else {
            State::Done
        };
        if self.diff.is_empty() || self.error.is_some() {
            self.summary(width);
        }
    }

    /// Takes the output the journal recorded for the call: its tail is what
    /// the row keeps, and the whole of it counts the lines.
    fn set_output(&mut self, text: &str, width: u16) {
        let (tail, earlier) = tail_of(text);
        self.set_tail(tail, earlier, width);
    }

    /// Takes the tail of the output a frame carried and the lines the cut took
    /// off before it, which together count the lines of the whole output.
    fn set_tail(&mut self, tail: &str, earlier: u64, width: u16) {
        self.lines = earlier + tail.lines().count() as u64;
        self.earlier = earlier;
        self.text = Some(tail.to_owned());
        if self.diff.is_empty() || self.error.is_some() {
            self.summary(width);
        }
    }

    /// The right span of a call that is not an edit: a short result of what the
    /// call and its output say, or the first line of the error it carries.
    fn summary(&mut self, width: u16) {
        let room = RESULT_MAX.min(usize::from(width.saturating_sub(RIGHT_MAX)));
        let mut buf = [0u8; 16];
        let mut span = String::with_capacity(RESULT_MAX);
        let role = if let Some(error) = self.error.as_deref() {
            // A failed call shows what went wrong, not what it asked for.
            self.right = [None, None, None];
            span.push_str(error.lines().next().unwrap_or_default());
            SpanRole::Text
        } else if self.text.is_some() {
            self.result(&mut span, &mut buf);
            SpanRole::Dim
        } else {
            self.right[0] = None;
            return;
        };
        cut_in_place(&mut span, room);
        self.right[0] = (!span.is_empty()).then_some(OwnedSpan { text: span, role });
    }

    /// Writes what the call and its output say: `written`, `12 lines`,
    /// `3 matches`, `2 files` or `exit 1`.
    fn result(&self, span: &mut String, buf: &mut [u8; 16]) {
        match self.name.as_str() {
            "edit" | "write" => span.push_str("written"),
            "grep" => found(span, buf, self.lines, ("match", "matches")),
            "find" | "glob" | "ls" => found(span, buf, self.lines, ("file", "files")),
            "bash" => match self.exit() {
                Some(code) => {
                    span.push_str("exit ");
                    span.push_str(digits(buf, code));
                }
                None => counted(span, buf, self.printed()),
            },
            _ => counted(span, buf, self.lines),
        }
    }

    /// The code a `bash` call failed with, when its output ends with one: the
    /// tool writes `exit code: 7` as its last line.
    fn exit(&self) -> Option<u64> {
        let last = self.text.as_deref()?.lines().last()?;
        let code: u64 = last.strip_prefix("exit code: ")?.trim().parse().ok()?;
        (code != 0).then_some(code)
    }

    /// The lines a `bash` call printed, without the line it ends with for its
    /// exit code.
    fn printed(&self) -> u64 {
        let exit = self
            .text
            .as_deref()
            .and_then(|text| text.lines().last())
            .is_some_and(|last| last.starts_with("exit code: "));
        self.lines.saturating_sub(u64::from(exit))
    }

    /// Whether the result arrived and its text is known, so it can be printed.
    fn settled(&self) -> bool {
        self.done && (self.text.is_some() || self.error.is_some())
    }

    /// Settles the call: its state, and the tail of the output it printed, which
    /// its open row draws. The tail is at most [`OUTPUT_KEEP`] bytes.
    fn finish(&mut self) {
        if self.state == State::Running {
            self.state = if self.error.is_some() {
                State::Failed
            } else {
                State::Done
            };
        }
    }

    /// The row the block draws, its spans in the caller's scratch and its diff
    /// borrowed from the call: a frame that draws a call copies neither.
    fn row<'a, 's>(&'a self, spans: &'s mut [Span<'a>; SPANS]) -> ToolRow<'s>
    where
        'a: 's,
    {
        ToolRow {
            name: &self.name,
            target: &self.target,
            state: self.state,
            right: self.filled(spans),
            diff: &self.diff,
            error: self.error.as_deref(),
            output: self.text.as_deref(),
            earlier: self.earlier,
        }
    }

    /// Fills `spans` with the parts of the row's right edge, and answers the
    /// slice of them the row draws.
    fn filled<'a, 's>(&'a self, spans: &'s mut [Span<'a>; SPANS]) -> &'s [Span<'s>]
    where
        'a: 's,
    {
        let mut len = 0;
        for span in self.right.iter().flatten() {
            spans[len] = Span {
                text: &span.text,
                role: span.role,
            };
            len += 1;
        }
        &spans[..len]
    }
}

/// A tool call that waits for an answer, with what its card shows.
#[derive(Debug)]
struct ApprovalBox {
    /// The id the answer names.
    id: u64,
    /// The tool the call names.
    tool: String,
    /// One line that shows the call.
    call: String,
    /// The rule that asked.
    rule: String,
    /// The rules an `always` answer saves: none, and the card offers no save.
    saves: Vec<String>,
    /// When the box went on screen: its keys answer it only after
    /// [`APPROVAL_ARMED`], so a key already being typed or a key repeat runs
    /// nothing.
    shown: std::time::Instant,
    /// The reason typed so far, when the answer turned into a refusal.
    reason: Option<String>,
}

impl ApprovalBox {
    /// A box for one request.
    ///
    /// The width is the frame's, which the box no longer needs to lay its rows
    /// out: the card cuts each row as it draws it.
    fn new(request: Approval<'_>, _width: u16) -> Self {
        ApprovalBox {
            id: request.id,
            tool: request.tool.into_owned(),
            call: request.call.into_owned(),
            rule: request.rule.into_owned(),
            saves: Vec::new(),
            shown: std::time::Instant::now(),
            reason: None,
        }
    }

    /// The same box, with the rules the request says `always` would save: none,
    /// and the card offers no save answer.
    fn saving(mut self, saves: Vec<Cow<'_, str>>) -> Self {
        self.saves = saves.into_iter().map(Cow::into_owned).collect();
        self
    }

    /// Whether the box takes answer keys: it has been on screen for
    /// [`APPROVAL_ARMED`] and the user is not typing.
    fn ready(&self, now: std::time::Instant, typing: bool) -> bool {
        !typing && now.duration_since(self.shown) >= APPROVAL_ARMED
    }

    /// Whether the reason field is open.
    fn refuses(&self) -> bool {
        self.reason.is_some()
    }
}

/// The question card's box: the questions of one `ask` request, the pick and
/// the checked rows of each, the answer typed for each, and when the card went
/// on screen.
#[derive(Debug)]
struct QuestionBox {
    /// The id the answer names.
    id: u64,
    /// The questions, in the order the model asked them.
    asks: Vec<Ask>,
    /// When the card went on screen: its keys answer it only after
    /// [`APPROVAL_ARMED`], so a key already being typed or a key repeat answers
    /// nothing.
    shown: std::time::Instant,
    /// The question the card shows.
    at: usize,
    /// The row each question has picked: one of its options, or the answer
    /// field after them.
    picks: Vec<usize>,
    /// Which options each question has checked, one flag per option.
    checked: Vec<Vec<bool>>,
    /// The answer field of each question: an editor with the text box's keys,
    /// made when the first key reaches it.
    fields: Vec<Option<Editor>>,
}

/// One question of a card, owned for as long as the card is on screen.
#[derive(Debug)]
struct Ask {
    /// The question text.
    q: String,
    /// The answers offered, in the order the card numbers them.
    options: Vec<AskOption>,
    /// Whether several options may be checked.
    multi: bool,
}

/// One answer a question offers.
#[derive(Debug)]
struct AskOption {
    /// The label an answer reads back.
    label: String,
    /// The short note the card draws after the label.
    note: Option<String>,
}

impl QuestionBox {
    /// The box for the request `id`, with the answers of the frame it came in.
    fn new(id: u64, questions: &[crate::client::Question<'_>], shown: std::time::Instant) -> Self {
        let asks: Vec<Ask> = questions
            .iter()
            .map(|question| Ask {
                q: question.q.as_ref().to_owned(),
                options: question
                    .options
                    .iter()
                    .map(|option| AskOption {
                        label: option.label.as_ref().to_owned(),
                        note: option.note.as_deref().map(str::to_owned),
                    })
                    .collect(),
                multi: question.multi,
            })
            .collect();
        let picks = vec![0; asks.len()];
        let checked = asks
            .iter()
            .map(|ask| vec![false; ask.options.len()])
            .collect();
        let fields = asks.iter().map(|_| None).collect();
        QuestionBox {
            id,
            asks,
            shown,
            at: 0,
            picks,
            checked,
            fields,
        }
    }

    /// Whether the card takes answer keys: it has been on screen for
    /// [`APPROVAL_ARMED`] and the user is not typing in the text box.
    fn ready(&self, now: std::time::Instant, typing: bool) -> bool {
        !typing && now.duration_since(self.shown) >= APPROVAL_ARMED
    }

    /// The question the card shows, and the row it has picked.
    fn current(&self) -> Option<(&Ask, usize)> {
        let ask = self.asks.get(self.at)?;
        let pick = self.picks.get(self.at).copied().unwrap_or(0);
        Some((ask, pick))
    }

    /// The row of the answer field: after the options.
    fn own_row(ask: &Ask) -> usize {
        ask.options.len()
    }

    /// Whether the answer field of the question on screen is picked, so the
    /// keys type into it.
    fn typing(&self) -> bool {
        self.current()
            .is_some_and(|(ask, pick)| pick == Self::own_row(ask))
    }

    /// The answer field of the question on screen, when a key reached it.
    fn field(&self) -> Option<&Editor> {
        self.fields.get(self.at).and_then(Option::as_ref)
    }

    /// The answer field of the question on screen, made empty on first use.
    fn field_mut(&mut self) -> Option<&mut Editor> {
        let slot = self.fields.get_mut(self.at)?;
        Some(slot.get_or_insert_with(Editor::new))
    }

    /// Whether the question on screen has typed text in its field.
    fn has_text(&self) -> bool {
        self.field().is_some_and(|field| !field.is_empty())
    }

    /// Picks the row `row` of the question on screen, when it has one.
    fn pick(&mut self, row: usize) {
        let Some((ask, _)) = self.current() else {
            return;
        };
        if row > Self::own_row(ask) {
            return;
        }
        let at = self.at;
        if let Some(pick) = self.picks.get_mut(at) {
            *pick = row;
        }
    }

    /// Moves the pick of the question on screen by `step` rows, within the
    /// options and the answer field.
    fn step(&mut self, step: isize) {
        let Some((ask, pick)) = self.current() else {
            return;
        };
        let last = ask.options.len() as isize;
        let row = (pick as isize + step).clamp(0, last) as usize;
        self.pick(row);
    }

    /// Toggles the checked state of the option the question on screen has
    /// picked.
    fn toggle(&mut self) {
        let Some((_, pick)) = self.current() else {
            return;
        };
        let at = self.at;
        if let Some(set) = self.checked.get_mut(at).and_then(|set| set.get_mut(pick)) {
            *set = !*set;
        }
    }

    /// Picks the answer field of the question on screen and types `c` into it.
    fn start_typing(&mut self, c: char) {
        let Some((ask, _)) = self.current() else {
            return;
        };
        let own = Self::own_row(ask);
        self.pick(own);
        if let Some(field) = self.field_mut() {
            field.type_char(c);
        }
    }

    /// Empties the answer field of the question on screen.
    fn clear_field(&mut self) {
        if let Some(slot) = self.fields.get_mut(self.at) {
            *slot = None;
        }
    }

    /// The answer of every question, one list each, in the order the model
    /// asked them: the checked labels and any typed text of a question that
    /// takes several, and the picked label or the typed text of one that takes
    /// a single answer.
    fn answers(&self) -> Vec<Vec<String>> {
        let mut answers = Vec::with_capacity(self.asks.len());
        for (at, ask) in self.asks.iter().enumerate() {
            let typed = self
                .fields
                .get(at)
                .and_then(Option::as_ref)
                .map(|field| {
                    let (before, after) = field.chunks();
                    let mut text = String::with_capacity(before.len() + after.len());
                    text.push_str(before);
                    text.push_str(after);
                    text
                })
                .filter(|text| !text.trim().is_empty());
            let own = Self::own_row(ask);
            let mut answer: Vec<String> = Vec::new();
            if ask.multi {
                let set = self.checked.get(at);
                for (row, option) in ask.options.iter().enumerate() {
                    let on = set.and_then(|set| set.get(row)).copied().unwrap_or(false);
                    if on {
                        answer.push(option.label.clone());
                    }
                }
                answer.extend(typed);
            } else {
                let pick = self.picks.get(at).copied().unwrap_or(0);
                if pick == own {
                    answer.extend(typed);
                } else if let Some(option) = ask.options.get(pick) {
                    answer.push(option.label.clone());
                }
            }
            answers.push(answer);
        }
        answers
    }
}

/// The theme of the mode the app is in: every role resolved once, and its id in
/// the screen's style table.
///
/// The regions draw with [`App::style`], which is one array index: no color
/// math and no style lookup per cell. `paint::theme` resolves it and resolves it
/// again when the terminal answers the background query or a theme file arrives.
#[derive(Clone, Copy, Debug)]
struct Look {
    /// The style of every role, for the mode, the depth and the theme file.
    theme: Theme,
    /// The id of every role and emphasis in the screen's style table.
    ids: [[u32; Emphasis::ALL.len()]; Role::ALL.len()],
    /// The depth a mixed fill is mapped to: true color, 256 or 16.
    depth: Depth,
    /// The theme file's colors, which move a role after the mode has had its say.
    overrides: Overrides,
    /// Draw color at all: false under `NO_COLOR`, which beats every mode.
    color: bool,
}

/// How many diff lines a block keeps before its row says how many the tab
/// opens.
const DIFF_MAX: usize = 40;

/// The columns a tool row leaves for its own text, so the result at the right
/// edge cannot crowd it out.
const RIGHT_MAX: u16 = 40;

/// The columns a call's result may take at the right edge of its row.
const RESULT_MAX: usize = 24;

/// Runs one key that needs the editor alone, the same in the text box and in
/// an answer field. Answers false for Up or Down at the edge of the text and
/// for a key that needs the app (the history, its search, an image).
fn edit_text(editor: &mut Editor, key: EditKey, width: u16) -> bool {
    match key {
        EditKey::Insert(c) => editor.type_char(c),
        EditKey::Left => editor.backward_char(),
        EditKey::Right => editor.forward_char(),
        EditKey::ForwardWord => editor.forward_word(),
        EditKey::BackwardWord => editor.backward_word(),
        EditKey::DeleteChar | EditKey::Delete => editor.delete_char(),
        EditKey::KillWordForward => editor.kill_word(),
        EditKey::KillWordBackward => editor.backward_kill_word_at(Instant::now()),
        EditKey::DeleteSpace => editor.delete_horizontal_space(),
        EditKey::Transpose => editor.transpose_chars(),
        EditKey::TransposeWords => editor.transpose_words(),
        EditKey::Upcase => editor.upcase_word(),
        EditKey::Downcase => editor.downcase_word(),
        EditKey::Capitalize => editor.capitalize_word(),
        EditKey::Yank => editor.yank(),
        EditKey::YankPop => editor.yank_pop(),
        EditKey::Undo => editor.undo(),
        EditKey::Mark => editor.set_mark(),
        EditKey::Exchange => editor.exchange_point_and_mark(),
        EditKey::Quote => editor.quote_next(),
        EditKey::CharSearch => editor.search_char(true),
        EditKey::CharSearchBackward => editor.search_char(false),
        EditKey::Digit(digit) => editor.digit(digit),
        EditKey::NegativeArgument => editor.negative_argument(),
        EditKey::NextKey(key) => editor.next_key(key),
        EditKey::AbortSearch => editor.search_abort(),
        EditKey::Up => return editor.up(width),
        EditKey::Down => return editor.down(width),
        EditKey::Home => editor.home(width),
        EditKey::End => editor.end(width),
        EditKey::Backspace => editor.backward_delete_char(),
        EditKey::LineStart => editor.beginning_of_line(),
        EditKey::LineEnd => editor.end_of_line(),
        EditKey::KillEnd => editor.kill_line(),
        EditKey::KillStart => editor.unix_line_discard(),
        EditKey::KillWord => editor.unix_word_rubout(),
        EditKey::PasteImage
        | EditKey::SearchBackward
        | EditKey::SearchForward
        | EditKey::HistoryOldest
        | EditKey::HistoryNewest
        | EditKey::YankLastArg => return false,
    }
    true
}

/// What a tool call's arguments say about the work, read borrowed: nothing is
/// copied out of the frame's buffer to show a row.
struct Call<'a> {
    /// The path, the command, the pattern, or the arguments cut short.
    target: Cow<'a, str>,
    /// The edit the call makes, when it makes one.
    edit: Option<Edit<'a>>,
}

/// The diff one `edit` or `write` call makes.
struct Edit<'a> {
    /// The lines it adds.
    added: u64,
    /// The lines it removes.
    removed: u64,
    /// The lines it removes.
    old: Option<Cow<'a, str>>,
    /// The lines it adds.
    new: Option<Cow<'a, str>>,
}

/// The arguments of a call, borrowed from the frame.
#[derive(Default, Deserialize)]
struct CallArgs<'a> {
    /// `bash`: the command line.
    #[serde(default, borrow)]
    command: Option<Cow<'a, str>>,
    /// `read`, `edit`, `write`: the file.
    #[serde(default, borrow)]
    path: Option<Cow<'a, str>>,
    /// `grep`, `find`: the pattern.
    #[serde(default, borrow)]
    pattern: Option<Cow<'a, str>>,
    /// `edit`: the text it replaces.
    #[serde(default, borrow)]
    old_string: Option<Cow<'a, str>>,
    /// `edit`: the text it writes.
    #[serde(default, borrow)]
    new_string: Option<Cow<'a, str>>,
    /// `write`: the whole content.
    #[serde(default, borrow)]
    content: Option<Cow<'a, str>>,
    /// `ask`: the questions; the row shows the first.
    #[serde(default, borrow)]
    questions: Option<Vec<AskArg<'a>>>,
}

/// One question of an `ask` call, borrowed from the frame.
#[derive(Deserialize)]
struct AskArg<'a> {
    /// The question text.
    #[serde(borrow)]
    q: Cow<'a, str>,
}

impl<'a> Call<'a> {
    /// The call `name` makes with `arguments`.
    fn parse(name: &str, arguments: &'a str) -> Self {
        let args: CallArgs<'a> = serde_json::from_str(arguments).unwrap_or_default();
        let target = match name {
            "bash" => args.command.unwrap_or_else(|| short(arguments)),
            "grep" | "find" => args.pattern.unwrap_or_else(|| short(arguments)),
            "ask" => args
                .questions
                .and_then(|questions| questions.into_iter().next())
                .map(|first| first.q)
                .unwrap_or_else(|| short(arguments)),
            _ => args
                .path
                .or(args.pattern)
                .unwrap_or_else(|| short(arguments)),
        };
        let edit = match name {
            "edit" => Some(Edit {
                added: count(args.new_string.as_deref()),
                removed: count(args.old_string.as_deref()),
                old: args.old_string,
                new: args.new_string,
            }),
            "write" => args.content.map(|content| Edit {
                added: count(Some(&content)),
                removed: 0,
                old: None,
                new: Some(content),
            }),
            _ => None,
        };
        Call { target, edit }
    }

    /// The edit, when the call makes one.
    fn edit(&self) -> Option<&Edit<'a>> {
        self.edit.as_ref()
    }
}

impl Edit<'_> {
    /// The diff lines of the edit, oldest first: what it removes, then what it
    /// writes.
    fn diff(&self) -> impl Iterator<Item = (u32, DiffKind, &str)> {
        let old = self
            .old
            .as_deref()
            .unwrap_or_default()
            .lines()
            .map(|text| (DiffKind::Delete, text));
        let new = self
            .new
            .as_deref()
            .unwrap_or_default()
            .lines()
            .map(|text| (DiffKind::Add, text));
        old.chain(new)
            .enumerate()
            .map(|(index, (kind, text))| (index as u32 + 1, kind, text))
    }
}

/// The language a diff is tokenized in, from the extension of the file it
/// edits: the tokenizer's own fence tags, and the C++ extensions it leaves out.
fn lang_of(path: &str) -> Option<Lang> {
    let extension = std::path::Path::new(path).extension()?.to_str()?;
    let tag = match extension {
        "hpp" | "cc" => "cpp",
        tag => tag,
    };
    Lang::from_tag(tag)
}

/// The lines of `text`.
fn count(text: Option<&str>) -> u64 {
    text.map_or(0, |text| text.lines().count() as u64)
}

/// The first line of `arguments`, cut to a row, for a call whose shape this
/// build does not know.
fn short(arguments: &str) -> Cow<'_, str> {
    let line = arguments.lines().next().unwrap_or_default();
    if line.len() > usize::from(RIGHT_MAX) {
        let mut cut = line.to_owned();
        cut_in_place(&mut cut, usize::from(RIGHT_MAX));
        Cow::Owned(cut)
    } else {
        Cow::Borrowed(line)
    }
}

/// Writes the lines a call printed: `12 lines`, or `1 line`.
fn counted(span: &mut String, buf: &mut [u8; 16], count: u64) {
    span.push_str(digits(buf, count));
    span.push_str(if count == 1 { " line" } else { " lines" });
}

/// Writes what a search found: `3 matches`, `1 file`, and `no match` for none.
fn found(span: &mut String, buf: &mut [u8; 16], count: u64, words: (&str, &str)) {
    if count == 0 {
        span.push_str("no match");
        return;
    }
    span.push_str(digits(buf, count));
    span.push(' ');
    span.push_str(if count == 1 { words.0 } else { words.1 });
}

/// The decimal digits of `n` in a buffer, for a row that prints a count.
fn digits(buf: &mut [u8; 16], mut n: u64) -> &str {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[at..]).unwrap_or_default()
}

/// The host's read of the paths, while it is in flight.
#[derive(Default)]
struct FilesJob(Option<HostFuture<Files>>);

impl std::fmt::Debug for FilesJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FilesJob(..)")
    }
}

/// The futures the open sheet waits on: the model list, the steps of a login
/// and the worker of another session. The loop polls them beside the socket.
#[derive(Default)]
struct Waits {
    /// The host's read of the model list, while it is in flight.
    models: Option<HostFuture<Models>>,
    /// What the list the host answers is for.
    after_models: AfterModels,
    /// The steps of the login now running, while one runs.
    login: Option<tokio::sync::mpsc::Receiver<LoginStep>>,
    /// The host's start of another session's worker, while it is in flight.
    open: Option<HostFuture<Result<Opened, String>>>,
    /// The host's delete of a session, while it is in flight.
    delete: Option<Deleting>,
}

/// A session the sessions sheet armed and the host is deleting: the id it acts
/// on, the title its row showed, and the host's answer when it comes.
struct Deleting {
    /// The session the second Ctrl+D named, whatever the sheet shows later.
    id: Arc<str>,
    /// The title of the row the sheet armed, for the note the answer writes.
    title: Arc<str>,
    /// The host's answer.
    job: HostFuture<Result<(), String>>,
}

impl std::fmt::Debug for Deleting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Deleting(..)")
    }
}

impl std::fmt::Debug for Waits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Waits(..)")
    }
}

/// The sheet in place of the text box, while one is open.
struct Open {
    /// What the pick of the sheet does.
    kind: SheetKind,
    /// The rows, the filter and the selection.
    sheet: Sheet,
}

impl std::fmt::Debug for Open {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Open(..)")
    }
}

impl Open {
    /// Names the keys a keys file moved in the words of the sheet: its hint,
    /// when it has one that names a key, and its footer, from the words the
    /// file printed once when it was read.
    fn name_keys(&mut self, file: Option<&keyfile::KeyFile>) {
        let (hint, footer) = match &self.kind {
            SheetKind::Model(_) => (None, Foot::Model),
            SheetKind::Level => (None, Foot::Level),
            SheetKind::Sessions(_) => (Some(Foot::SessionsHint), Foot::Sessions),
            SheetKind::Login(login) => (Some(Foot::LoginHint), login.foot()),
            SheetKind::Help => (None, Foot::Help),
        };
        self.sheet.moved(file, hint, footer);
    }

    /// Clears what a first Ctrl+D armed: any other key of the sheet does.
    fn disarm(&mut self) {
        if let SheetKind::Sessions(state) = &mut self.kind {
            state.armed = None;
        }
        self.sheet.confirm = None;
    }
}

/// What the app does with the model list when the host answers. One value, so
/// the last key wins while the list is on its way.
#[derive(Debug, Default)]
enum AfterModels {
    /// Opens the model sheet.
    #[default]
    ModelSheet,
    /// Opens the level sheet.
    LevelSheet,
    /// Judges a word of `/level` against the plan of the current model.
    LevelWord(String),
}

/// The rows a sessions sheet was built from, so a delete names a row of the
/// disk and not an index into a list that may have changed under it.
struct Sessions {
    /// The rows on screen, in the order the sheet lists them.
    rows: Vec<SessionRow>,
    /// The id of the row a first Ctrl+D armed.
    armed: Option<Arc<str>>,
    /// What the sheet read and built for the rows, by id, kept while the sheet
    /// is open: a row read once draws at once when a key comes back to it.
    reads: HashMap<Arc<str>, Preview>,
}

/// What the sessions sheet read and built for one row of the folder.
#[derive(Debug)]
struct Preview {
    /// The newest messages with text the row's journal held.
    messages: RecentMessages,
    /// The rows built from them, and the pane width they were built at.
    built: Option<(u16, Arc<Vec<PreviewRow>>)>,
}

/// The preview rows of a row of the sheet: the ones the cache holds for this
/// pane width, so a key that comes back to a row builds none, and the messages
/// it holds built again when the width changed.
fn preview_rows(
    state: &mut Sessions,
    id: &Arc<str>,
    pane: u16,
    theme: &Theme,
) -> Option<Arc<Vec<PreviewRow>>> {
    let read = state.reads.get_mut(id)?;
    if let Some((width, rows)) = &read.built
        && *width == pane
    {
        return Some(Arc::clone(rows));
    }
    let rows = Arc::new(sessions::preview_rows(&read.messages, pane, theme));
    read.built = Some((pane, Arc::clone(&rows)));
    Some(rows)
}

/// What a sheet is for.
enum SheetKind {
    /// The model list the sheet was built from.
    Model(Models),
    /// The rows of the reasoning levels of the current model.
    Level,
    /// The sessions of the folder, and the row a delete armed.
    Sessions(Sessions),
    /// The providers and the lines of the login now running.
    Login(Login),
    /// Every key, grouped.
    Help,
}

/// The word after the `@` the cursor ends, when it ends one.
fn at_word(before: &str) -> Option<&str> {
    let start = before
        .rfind(|c: char| c.is_whitespace())
        .map_or(0, |at| at + 1);
    before[start..].strip_prefix('@')
}

/// The app's state.
#[derive(Debug)]
struct App {
    /// The live area and the diff that writes it.
    screen: Screen,
    /// The theme of the mode the app is in: every role's style and the id it is
    /// interned as in the screen's table. `paint::theme` resolves it.
    look: Look,
    /// The region the transcript and the live rows lay out in: the whole
    /// terminal until a sidebar takes its columns.
    main: Rect,
    /// The message editor.
    editor: Editor,
    /// The key map.
    keys: Keys,
    /// The table the keys file moved, and the keys it shows; `None` for an app
    /// that read no file, which does no work per key.
    keyfile: Option<crate::keyfile::KeyFile>,
    /// The bytes of the frame being written, reused.
    frame: Vec<u8>,
    /// The message text of the send this key made, reused.
    session: String,
    /// The id the worker gave this client: its own join notice is not another
    /// client.
    client: u64,
    /// The finished entries not printed yet, oldest first.
    pending: Vec<Item>,
    /// The entries that still grow, in transcript order: a reply that streams,
    /// the tool calls that wait for their result.
    live: Vec<Item>,
    /// The runs of a commit, and the text they point at, reused.
    block_lines: LineSink,
    /// The blocks of a commit that the ring keeps, with the item each was drawn
    /// from, reused.
    finished: Vec<crate::screen::CommitBlock>,
    /// The blocks the user opened and folded.
    folds: Folds,
    /// The block of the edit that shows its diff, `0` until an edit makes one:
    /// a newer edit draws again the block that showed one before it.
    diff_block: u64,
    /// Whether the last line of the transcript on the screen is a blank row, so
    /// the block that follows it draws none of its own.
    blank: bool,
    /// The number the fold state knows the block of the run of calls now going
    /// by, `0` while no run is going: its calls and thoughts join it until reply
    /// text, a user message or the run's end closes it.
    run: u64,
    /// When the run now going started, for its rule's seconds.
    run_since: Option<Instant>,
    /// The tokens the run used, from the usage events of its calls.
    run_tokens: u64,
    /// What the run cost, in cents, from the usage events of its calls.
    run_cents: u32,
    /// The tokens the session used, from every usage event.
    tokens: u64,
    /// What the session cost, in cents, from every usage event.
    cents: u32,
    /// The approval box on screen.
    approval: Option<ApprovalBox>,
    /// Where the note of the card on screen ends, when it is open: the row and
    /// column the frame ends the cursor on, so the note is typed where it shows.
    card_cursor: Option<(u16, u16)>,
    /// The question card on screen, and the ones that wait behind it.
    question: Option<QuestionBox>,
    questions: std::collections::VecDeque<QuestionBox>,
    /// The approval boxes that wait for their turn, oldest first.
    queue: VecDeque<ApprovalBox>,
    /// The user messages of the session, oldest first, for Up and Down.
    history: Vec<String>,
    /// What the client has to send for this key.
    commands: VecDeque<Command>,
    /// The command table the `/` list and the palette open with, built once.
    slash: Arc<[list::Command]>,
    /// The list above the text box, when one is open.
    list: Option<List>,
    /// The which-key list Ctrl+X draws, built once.
    leader_list: List,
    /// The paths of the project, once the host answered.
    files: Option<Arc<[Arc<str>]>>,
    /// The host's read of the paths, while it is in flight.
    files_job: FilesJob,
    /// Whether the files arrived since the loop last looked.
    files_ready: bool,
    /// The file and model host, when the binary gave one.
    host: Option<HostHandle>,
    /// The sheet in place of the text box, when one is open.
    sheet: Option<Open>,
    /// The futures the open sheet waits on.
    waits: Waits,
    /// The newest messages the sessions sheet is reading, with the id of the
    /// row it is for, while the read runs: one read at a time, so holding the
    /// down key reads the row the user stops on and the one in flight.
    preview_job: Option<(
        Arc<str>,
        tokio::task::JoinHandle<io::Result<RecentMessages>>,
    )>,
    /// The model list the host answered, kept for the next opening.
    models: Option<Models>,
    /// The reasoning level the session runs, as its `settings` frame names it.
    level: Option<Arc<str>>,
    /// The worker of the session the sheet picked, when the app switches to it.
    switch: Option<Opened>,
    /// The query of the list for this key, reused.
    query: String,
    /// The visible rows of the editor, reused.
    rows: Vec<(usize, usize)>,
    /// The status line, rebuilt only when it changes.
    status: String,
    /// Whether the status line is out of date.
    status_dirty: bool,
    /// Whether a run is going.
    running: bool,
    /// Whether the second Ctrl+C quits.
    armed: bool,
    /// How many rows the editor showed last.
    editor_rows: u16,
    /// The lines the last mouse copy took, for the status line.
    copied: usize,
    /// When the status line stops saying so; `None` while it does not.
    copied_until: Option<Instant>,
    /// How many rows of the live entries the last frame's live area cut from
    /// the top, so a click on a live block counts from the rows it drew.
    live_skip: u16,
    /// The screen row the text box starts on last frame, so a click can find
    /// the paste chips it drew.
    editor_top: u16,
    /// Whether the editor's row count must be measured again.
    editor_dirty: bool,
    /// Whether something changed since the last frame.
    dirty: bool,
    /// When the frame that waits for more changes is due.
    deadline: Option<tokio::time::Instant>,
    /// Whether the loop ends after this frame.
    exit: bool,
    /// The process exit code the caller uses.
    code: i32,
    /// The folder of the session's journal, when it is known.
    journal: Option<PathBuf>,
    /// The folder the session runs in, as the docked sidebar shows it.
    place: Arc<str>,
    /// The newest journal record the app read.
    read: u64,
    /// The newest journal record the app drew.
    drawn: u64,
    /// The newest journal record whose picture of an image the app drew: no path
    /// draws the pictures of that record or of an older one again.
    pictured: u64,
    /// The newest journal record the app read for pictures: the next read starts
    /// after it, so a record whose message names no image costs one read.
    picture_read: u64,
    /// Whether a live frame named an image whose bytes a record that has not
    /// committed yet holds. It stays set until the run ends or a read finds a
    /// picture, since the record that commits before the one holding the image
    /// carries nothing.
    pictures: bool,
    /// Whether a committed record waits for the read of the pictures it holds.
    pictures_wait: bool,
    /// The color pairs the pictures of this transcript may still take.
    budget: StyleBudget,
    /// The user messages the app drew from live frames, oldest first: the record
    /// that commits each of them follows the frame, so a journal read matches
    /// them in order instead of drawing them twice.
    unwritten: VecDeque<String>,
    /// The ids of the tool calls the panels took, oldest first: the journal
    /// record of a call and the live frame that drew it carry one id, and a cut
    /// or a resumed session reads the call again.
    fed: VecDeque<String>,
    /// The newest journal record the worker committed.
    upto: u64,
    /// The compaction rows whose checkpoint text is not on screen yet, each with
    /// the record the read of it starts after.
    checkpoints: Vec<(u64, u64)>,
    /// The compaction row the app reads the checkpoint text of next.
    wanted: Option<(u64, u64)>,
    /// Whether a tool result came without the tail of its output, so the
    /// journal still holds the text to read.
    unfilled: bool,
    /// Why the connection ended, when the next pass reconnects after the worker
    /// cut this client off.
    resync: Option<Resync>,
    /// Whether the status line says `resynced`, until it is next rebuilt.
    resynced: bool,
    /// Whether the open reply still waits for the worker's replay after a
    /// resync, which replaces it from its start.
    awaits_replay: bool,
    /// When the resyncs happened, oldest first, so a worker that keeps cutting
    /// this client does not make it spin.
    cuts: VecDeque<Instant>,
    /// The sidebar: its panels and the block they draw in.
    sidebar: Sidebar,
    /// The width the sidebar was asked for: `None` follows the terminal's.
    sidebar_on: Option<bool>,
    /// Where the sidebar sits in the last layout, off when it is hidden.
    aside: layout::Sidebar,
    /// The screen row of each panel's title line, from the last draw.
    panel_rows: Vec<(u16, usize)>,
    /// The mouse: the press a drag follows, and the cells it selects.
    mouse: Mouse,
    /// Whether a drag that started in the transcript is selecting.
    selecting: bool,
    /// The copy of the last release: the OSC 52 sequence the next frame writes.
    clipboard: Vec<u8>,
    /// The plain text of that copy, the scratch the next one is built in.
    clip: String,
    /// The state folder the folder's sessions and the live workers are under.
    state: Option<PathBuf>,
    /// The sessions the panel shows, with the current one marked.
    sessions: Vec<SessionRow>,
    /// The job that reads the folder's sessions, while it runs.
    reading: Option<tokio::task::JoinHandle<Vec<reagent_store::list::Entry>>>,
    /// Whether a read was asked for while one ran and runs after it.
    reread: bool,
    /// The sessions whose live worker says its run waits for an approval, as
    /// the discovery folder last read them.
    waiting: Vec<Arc<str>>,
    /// When the next tick of a panel that wants one is due.
    tick: Option<Instant>,
    /// The image paste in flight, and the ones that wait behind it.
    image_job: Option<ImageJob>,
    /// The image pastes that wait for the one in flight, oldest first.
    image_queue: VecDeque<ImageRequest>,
    /// The terminal's own image protocol, decided at the first picture of the
    /// run and kept for the rest of it.
    native: Option<Native>,
    /// The size of one cell in pixels, read at start and on each resize; `None`
    /// when the terminal reports none, and pictures size for half-blocks.
    cell: Option<crate::picture::CellShape>,
    /// Whether the protocol of the terminal was decided.
    native_decided: bool,
    /// When the pictures that moved or were lost go out.
    picture_deadline: Option<tokio::time::Instant>,
    /// The id the next picture of a placeholder terminal takes.
    image_id: u32,
    /// The ids of the images the terminal holds, whose placeholders go with the
    /// screen.
    image_ids: Vec<u32>,
    /// The image sequences of the pictures going out, reused.
    images: Vec<u8>,
}

/// Where the bytes of a pasted image come from.
#[derive(Debug, Clone)]
enum ImageSource {
    /// The local clipboard.
    Clipboard,
    /// A file a dropped path names.
    File(PathBuf),
}

/// One image paste the app runs, or waits to run behind another.
#[derive(Debug, Clone)]
struct ImageRequest {
    /// Where the bytes come from.
    source: ImageSource,
    /// The paste's own text, for a dropped path whose file cannot be read: it
    /// goes in the box as a normal paste instead.
    fallback: Option<String>,
    /// Whether a clipboard with no image says nothing: a paste of no text asked.
    quiet: bool,
}

impl ImageRequest {
    /// The clipboard's image; `quiet` says that no image there is no note.
    fn clipboard(quiet: bool) -> Self {
        ImageRequest {
            source: ImageSource::Clipboard,
            fallback: None,
            quiet,
        }
    }

    /// The image of the file `path` names, and the paste to fall back on.
    fn file(path: PathBuf, text: String) -> Self {
        ImageRequest {
            source: ImageSource::File(path),
            fallback: Some(text),
            quiet: false,
        }
    }
}

/// The image paste in flight: the request it runs and the handle of its answer.
#[derive(Debug)]
struct ImageJob {
    /// The request the job runs for.
    request: ImageRequest,
    /// The job's answer, when it ends.
    handle: tokio::task::JoinHandle<Result<Option<Pasted>, String>>,
}

/// The image a job wrote, with the number of its chip.
#[derive(Debug, Clone)]
struct Pasted {
    /// The path the chip's text quotes.
    path: String,
    /// The number in the chip's label.
    number: u32,
}

/// How many bytes of a pasted image the app takes: the clipboard's cap.
const IMAGE_CAP: usize = crate::clipboard::CAP;

/// The words of an image over the cap, as the clipboard and the file both say
/// them.
const IMAGE_CAP_TEXT: &str = "the image is over 64 MiB";

/// The note of a clipboard that holds no image.
const IMAGE_NONE: &str = "no image in the clipboard";

/// The note of an image paste in a session with no folder.
const IMAGE_NO_FOLDER: &str = "this session has no folder for images";

/// The folder of a session's pasted images.
const PASTED: &str = "pasted";

/// The kinds a path can name to be a dropped image, in lower case.
const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "tif", "heic", "avif",
];

/// Where this app's own image pastes land: the word a chip stands for.
const PASTED_WORD: &str = "/pasted/image-";

/// Writes the image of `source` into the `pasted` folder of the session at
/// `folder`, and answers the file it wrote and the number of its chip.
///
/// The folder and every file are the user's own alone, and a new number takes a
/// file rather than write over one that is already there. All of it is the work
/// of one blocking job: no frame waits for it.
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "runs inside spawn_blocking"
)]
fn save_image(folder: &Path, source: &ImageSource) -> Result<Option<Pasted>, String> {
    let (bytes, ext) = match source {
        ImageSource::Clipboard => match crate::clipboard::read_image()? {
            Some(image) => (image.bytes, image.ext.to_owned()),
            None => return Ok(None),
        },
        ImageSource::File(path) => {
            let size = std::fs::metadata(path)
                .map_err(|error| error.to_string())?
                .len();
            if size > IMAGE_CAP as u64 {
                return Err(IMAGE_CAP_TEXT.to_owned());
            }
            let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
            let ext = path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if ext.is_empty() {
                return Err("the image path names no kind".to_owned());
            }
            (bytes, ext)
        }
    };
    let folder = folder.join(PASTED);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(&folder)
        .map_err(|error| format!("the pasted folder: {error}"))?;
    // The number follows the files the folder holds, so the second image of a
    // session is not the second of another one.
    let mut number = 1;
    let entries =
        std::fs::read_dir(&folder).map_err(|error| format!("the pasted folder: {error}"))?;
    for entry in entries {
        entry.map_err(|error| format!("the pasted folder: {error}"))?;
        number += 1;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let (path, mut file) = loop {
        let path = folder.join(format!("image-{number}.{ext}"));
        // A chip quotes its path, so a name no quote can close is a name no chip
        // draws.
        match path.to_str() {
            Some(name) if !name.contains('"') => {}
            _ => return Err("the image path holds a quote".to_owned()),
        }
        match options.open(&path) {
            Ok(file) => break (path, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => number += 1,
            Err(error) => return Err(format!("the image file: {error}")),
        }
    };
    file.write_all(&bytes)
        .map_err(|error| format!("the image file: {error}"))?;
    let path = path
        .to_str()
        .ok_or_else(|| "the image path is no text".to_owned())?
        .to_owned();
    Ok(Some(Pasted { path, number }))
}

/// The path an image paste names, when its text is one path to one image.
///
/// A terminal sends a dropped file as a plain path, a quoted one, or one with
/// its spaces escaped, so all three are one path here. The text trims one
/// trailing line break and the spaces around it, and it names no image unless it
/// starts at `/` or `~/`, ends in a kind [`IMAGE_EXTS`] knows, and holds no line
/// break, no quote and no unescaped space that would make it two paths.
fn image_path(text: &str) -> Option<PathBuf> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.trim_matches(' ');
    if text.is_empty() {
        return None;
    }
    let raw = if let Some(inner) = text
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        Cow::Borrowed(inner)
    } else if let Some(inner) = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        Cow::Borrowed(inner)
    } else {
        let mut path = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match c {
                // Terminals also escape brackets and the like in a dropped path.
                '\\' => path.push(chars.next()?),
                ' ' | '"' | '\n' | '\r' => return None,
                c => path.push(c),
            }
        }
        Cow::Owned(path)
    };
    if raw.contains('"') || raw.contains(['\n', '\r']) {
        return None;
    }
    let ext = raw.rsplit_once('.')?.1;
    if !IMAGE_EXTS.iter().any(|kind| kind.eq_ignore_ascii_case(ext)) {
        return None;
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        let home = std::env::var_os("HOME")?;
        return Some(PathBuf::from(home).join(rest));
    }
    if !raw.starts_with('/') {
        return None;
    }
    Some(PathBuf::from(raw.as_ref()))
}

/// The text of a user message as a transcript row draws it: an image chip's
/// `@"<path>"` word is the label of its number, and the space in front of it goes
/// with it when the text before it ends in one, so the row reads as the box did.
///
/// The message the worker gets, the journal and the history keep the real text.
fn shown_user_text(text: &str) -> Cow<'_, str> {
    if !text.contains(PASTED_WORD) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(at) = rest.find(" @\"") else {
            out.push_str(rest);
            return Cow::Owned(out);
        };
        let quoted = &rest[at + 3..];
        let Some(end) = quoted.find('"') else {
            out.push_str(rest);
            return Cow::Owned(out);
        };
        let Some(number) = pasted_number(&quoted[..end]) else {
            out.push_str(&rest[..at + 3]);
            rest = &rest[at + 3..];
            continue;
        };
        let before = &rest[..at];
        out.push_str(before);
        if !before.is_empty() && !before.ends_with(' ') {
            out.push(' ');
        }
        out.push_str(IMAGE_LABEL);
        out.push_str(number);
        out.push(']');
        rest = &rest[at + 3 + end + 1..];
    }
}

/// The number of the image a `/pasted/image-<n>.<ext>` path names, when the path
/// is one of this app's own pastes.
fn pasted_number(path: &str) -> Option<&str> {
    if !path.contains(PASTED_WORD) {
        return None;
    }
    let name = path.rsplit('/').next()?;
    let name = name.strip_prefix("image-")?;
    let (number, ext) = name.split_once('.')?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if !IMAGE_EXTS.iter().any(|kind| kind.eq_ignore_ascii_case(ext)) {
        return None;
    }
    Some(number)
}

/// What ended this client's connection, and so what a resync falls back to when
/// the worker is not there any more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resync {
    /// The worker named the reason: this client fell behind.
    Behind,
    /// The connection closed without a word, as a cut in the middle of a frame
    /// does.
    Closed,
}

impl App {
    /// The app for a session whose handshake answered `init`, in the mode the
    /// start chose.
    fn new(init: &Initialized, (columns, rows): (u16, u16), full: bool) -> Self {
        let columns = columns.max(1);
        let rows = rows.max(1);
        let mut screen = if full {
            Screen::full(columns, rows)
        } else {
            Screen::inline(columns, rows, 1)
        };
        // The mode is the terminal's until it answers the background query:
        // the 16 colors, its own background showing through, fills only for the
        // selection. `NO_COLOR` makes it the no-color theme at once.
        let look = Look::resolve(
            crate::theme::Mode::Terminal,
            Depth::from_env(),
            Overrides::default(),
            crate::theme::Flags::from_env().color,
            &mut screen,
        );
        let mut app = App {
            screen,
            look,
            main: layout::layout(columns, rows, false).main,
            editor: Editor::new(),
            keys: Keys::new(),
            keyfile: None,
            frame: Vec::new(),
            session: init.session.clone(),
            client: init.client,
            pending: Vec::new(),
            live: Vec::new(),
            copied: 0,
            copied_until: None,
            live_skip: 0,
            block_lines: LineSink::default(),
            finished: Vec::new(),
            folds: Folds::new(),
            diff_block: 0,
            blank: false,
            run: 0,
            run_since: None,
            run_tokens: 0,
            run_cents: 0,
            tokens: 0,
            cents: 0,
            approval: None,
            card_cursor: None,
            question: None,
            questions: VecDeque::new(),
            queue: VecDeque::new(),
            history: Vec::new(),
            commands: VecDeque::new(),
            slash: commands::table(None),
            list: None,
            leader_list: List::leader(),
            files: None,
            files_job: FilesJob::default(),
            files_ready: false,
            host: None,
            sheet: None,
            waits: Waits::default(),
            preview_job: None,
            models: None,
            level: init.level.clone(),
            switch: None,
            query: String::new(),
            rows: Vec::new(),
            status: String::new(),
            status_dirty: true,
            running: false,
            armed: false,
            editor_rows: 1,
            editor_top: 0,
            editor_dirty: true,
            dirty: true,
            deadline: None,
            exit: false,
            code: 0,
            journal: None,
            place: Arc::from(""),
            read: 0,
            drawn: 0,
            pictured: 0,
            picture_read: 0,
            pictures: false,
            pictures_wait: false,
            budget: StyleBudget::new(),
            unwritten: VecDeque::new(),
            fed: VecDeque::new(),
            upto: 0,
            checkpoints: Vec::new(),
            wanted: None,
            unfilled: false,
            resync: None,
            resynced: false,
            awaits_replay: false,
            cuts: VecDeque::new(),
            sidebar: App::sidebar(),
            sidebar_on: None,
            aside: layout::Sidebar::Off,
            panel_rows: Vec::new(),
            mouse: Mouse::new(),
            selecting: false,
            clipboard: Vec::new(),
            clip: String::new(),
            state: None,
            sessions: Vec::new(),
            reading: None,
            reread: false,
            waiting: Vec::new(),
            tick: None,
            image_job: None,
            image_queue: VecDeque::new(),
            native: None,
            cell: None,
            native_decided: false,
            picture_deadline: None,
            image_id: first_image_id(),
            image_ids: Vec::new(),
            images: Vec::new(),
        };
        // The sidebar the start asks for, and the main area it leaves.
        app.reflow();
        app
    }

    /// Takes the messages read from the journal as finished entries.
    fn history(&mut self, tail: &mut Tail) {
        if tail.omitted > 0 {
            self.push_plain(vec![format!("... {} earlier messages", tail.omitted)], true);
        }
        // The calls the messages the app leaves out carry reach the panels too:
        // the Changes panel names the files the oldest turns touched.
        for call in &tail.calls {
            self.feed_call(call);
        }
        // A result's row takes its name and its target from the call that asked
        // for it: the one the tail kept, or the one of the reply just replayed.
        let mut calls: Vec<&ToolCall> = Vec::new();
        let mut run: Vec<Tool> = Vec::new();
        for (index, message) in tail.messages.iter().enumerate() {
            for (_, row) in tail.checkpoints.iter().filter(|(at, _)| *at == index) {
                self.close_group(&mut run);
                self.checkpoint_row(row);
            }
            match message {
                Message::User { content } => {
                    self.close_group(&mut run);
                    let text = self.user_message(content);
                    if !text.is_empty() {
                        self.history.push(text);
                    }
                    if let Some(pictures) = tail.pictures.get_mut(index) {
                        self.pictures_of(pictures.drain(..));
                    }
                }
                Message::Assistant(reply) => {
                    calls.extend(calls_of(&reply.content));
                    // The text of a reply ends the run of calls before it, and a
                    // turn that asked for calls alone keeps it going.
                    if reply
                        .content
                        .iter()
                        .any(|block| matches!(block, Content::Text { text } if !text.is_empty()))
                    {
                        self.close_group(&mut run);
                    }
                    self.finished(reply);
                }
                Message::ToolResult(result) => {
                    let call = tail
                        .calls
                        .iter()
                        .chain(calls.iter().copied())
                        .find(|call| call.id == result.call_id);
                    run.push(self.replayed(result, call));
                    // A picture stands under the row of the call that returned
                    // it, so the run of rows before it closes first.
                    if let Some(pictures) = tail.pictures.get_mut(index)
                        && !pictures.is_empty()
                    {
                        self.close_group(&mut run);
                        self.pictures_of(pictures.drain(..));
                    }
                }
            }
        }
        self.close_group(&mut run);
        self.replayed_diff();
        self.dirty = true;
    }

    /// Adds the row a compaction of the journal reads as: its checkpoint text is
    /// the journal's, so the row opens without a read of its own.
    fn checkpoint_row(&mut self, row: &history::Checkpoint) {
        let fold = self.folds.add();
        self.pending.push(Item::Compacted(Box::new(Compacted {
            tokens: None,
            messages: row.messages,
            fold,
            text: Some(Arc::clone(&row.text)),
        })));
    }

    /// Marks the newest edit of the journal replay as the one that shows its
    /// diff: the messages came in their order, and the block that holds it is
    /// still waiting for the commit.
    fn replayed_diff(&mut self) {
        let mut found = None;
        for item in &self.pending {
            match item {
                Item::Tool(tool) if !tool.diff.is_empty() => found = Some((tool.fold, tool.fold)),
                Item::Group(group) => {
                    if let Some(edit) = group.newest_edit() {
                        found = Some((edit.fold, group.fold));
                    }
                }
                _ => {}
            }
        }
        if let Some((call, block)) = found {
            self.folds.show_diff(call);
            self.diff_block = block;
        }
    }

    /// The row a journal tool result draws: its call gives the name and the
    /// target, and only a failed result takes the error path.
    fn replayed(&mut self, result: &ToolResult, call: Option<&ToolCall>) -> Tool {
        let width = self.transcript_width();
        let mut tool = Tool::new(&result.call_id, "");
        if let Some(call) = call {
            tool.set_call(&call.name, &call.arguments, width);
        }
        if result.is_error {
            let error = (!result.content.is_empty()).then_some(result.content.as_ref());
            tool.set_result(result.content.len(), error, width);
        } else {
            tool.set_output(&result.content, width);
        }
        tool.finish();
        tool
    }

    /// Adds a finished reply, drawn whole: it never streams again.
    fn finished(&mut self, reply: &AssistantMessage) {
        let width = self.transcript_width();
        let mut md = Markdown::new(width);
        md.set_theme(&self.look.theme);
        let mut first = true;
        for block in &reply.content {
            if let Content::Text { text } = block {
                if !first {
                    md.push("\n");
                }
                md.push(text);
                first = false;
            }
        }
        // The reply carries the calls of its turn: the panels take the ones no
        // live frame of this client brought.
        for block in &reply.content {
            if let Content::ToolCall(call) = block {
                self.feed_call(call);
            }
        }
        if first {
            return;
        }
        md.finish();
        self.pending.push(Item::Reply {
            md: Box::new(md),
            done: 0,
        });
        self.dirty = true;
    }

    /// Adds a user message as a finished turn, and answers the text it held.
    fn user_message(&mut self, content: &[Content]) -> String {
        let text = user_text(content);
        self.push_user(&text);
        text
    }

    /// Adds a user turn: the block wraps the message at the width of the main
    /// area, so the text stays in the buffer this makes for it.
    fn push_user(&mut self, text: &str) {
        // A chip's word reads as its label on the row; the history and the
        // journal keep the text the worker gets.
        let text: Arc<str> = Arc::from(shown_user_text(text).as_ref());
        self.freeze_all();
        self.pending.push(Item::User { text });
        self.dirty = true;
    }

    /// Notes a user message the app drew from a live frame, so a journal record
    /// that commits it is not drawn a second time. The queue keeps the newest
    /// messages only: a read that arrives long after follows it.
    fn drew_live(&mut self, text: &str) {
        if self.unwritten.len() == UNWRITTEN {
            self.unwritten.pop_front();
        }
        self.unwritten.push_back(text.to_owned());
    }

    /// Whether the journal's user message is one the app already drew from a
    /// live frame, which it takes off the queue when it is.
    fn already_drawn(&mut self, content: &[Content]) -> bool {
        let text = user_text(content);
        if self.unwritten.front().is_none_or(|drawn| drawn != &text) {
            return false;
        }
        self.unwritten.pop_front();
        true
    }

    /// Whether the reply a journal record holds is already on screen: the open
    /// reply of that turn, or the finished one still on its way out.
    fn shows(&self, reply: &AssistantMessage) -> bool {
        let text = reply_text(reply);
        self.live
            .iter()
            .chain(self.pending.iter())
            .any(|item| match item {
                Item::Reply { md, .. } => md.text() == text,
                _ => false,
            })
    }

    /// Adds a finished entry of plain lines.
    fn push_plain(&mut self, lines: Vec<String>, dim: bool) {
        // A note inside a run leaves the run going: it draws above the calls
        // that follow it.
        self.freeze_text();
        self.pending.push(Item::Plain { lines, dim });
        self.dirty = true;
    }

    /// Adds a note, such as an error, over the rows its text wraps into.
    fn note(&mut self, text: String) {
        let width = usize::from(self.transcript_width());
        self.push_plain(wrap_note(&text, width), false);
    }

    /// The keys the frames show: the file's table, or `None` for the table's
    /// own, which every draw falls back to.
    pub(crate) fn labels(&self) -> Option<&keyfile::Labels> {
        self.keyfile.as_ref().map(keyfile::KeyFile::labels)
    }

    /// The keys table `text` answers, for the tests of the frames that draw it.
    #[cfg(test)]
    pub(crate) fn read_keys_text(&mut self, text: &str) {
        let (file, _) = keyfile::KeyFile::parse(text);
        self.set_keyfile(file);
    }

    /// Reads the keys file once, before the first frame.
    ///
    /// A missing file is no note and no table, a file that cannot be read is one
    /// note, and a line the file gets wrong is one note too: the other lines
    /// apply either way.
    async fn read_keys(&mut self, path: Option<PathBuf>) {
        let Some(path) = path else {
            return;
        };
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                self.note(format!("keys: {}: {error}", path.display()));
                return;
            }
        };
        let (file, notes) = crate::keyfile::KeyFile::parse(&text);
        for note in notes {
            self.note(format!("keys: {note} ({})", path.display()));
        }
        self.set_keyfile(file);
    }

    /// Takes the table a keys file answers: the frames read it, and the
    /// which-key list prints its own copy of the leader's keys here, once, so a
    /// frame draws no text it has to build.
    fn set_keyfile(&mut self, file: keyfile::KeyFile) {
        let labels = file.labels();
        let grid: Arc<[Arc<str>]> = HINTS
            .iter()
            .enumerate()
            .filter(|(_, hint)| hint.group == keys::Group::Leader)
            .map(|(index, _)| Arc::from(labels.hint(index)))
            .collect::<Vec<_>>()
            .into();
        let title: Arc<str> = Arc::from(labels.leader());
        self.leader_list.set_keys(list::LeaderKeys { title, grid });
        self.leader_list.set_labels(Some(labels));
        self.slash = commands::table(Some(labels));
        self.keyfile = Some(file);
    }

    /// Moves the first `count` live entries to the transcript, freezing their
    /// lines: the calls and thoughts of the run now going join its one block,
    /// and a run of calls with no run going folds into one row.
    fn flush_live(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let run = self.run;
        let rest = self.live.split_off(count);
        let mut members: Vec<Member> = Vec::new();
        let mut group: Vec<Tool> = Vec::new();
        for mut item in std::mem::replace(&mut self.live, rest) {
            item.finish();
            match item {
                Item::Tool(tool) if run != 0 => members.push(Member::Call(tool)),
                Item::Thought(think) if run != 0 => members.push(Member::Thought(think)),
                Item::Tool(tool) => group.push(*tool),
                item => {
                    self.close_group(&mut group);
                    self.pending.push(item);
                }
            }
        }
        self.close_group(&mut group);
        self.join_run(members);
        self.dirty = true;
    }

    /// Adds the run of calls the live area gave up as one row, or as its own row
    /// when the run is one call.
    fn close_group(&mut self, group: &mut Vec<Tool>) {
        if group.is_empty() {
            return;
        }
        let mut tools = std::mem::take(group);
        if tools.len() == 1 {
            for tool in tools {
                if self.folds.diff() == tool.fold {
                    // The call keeps its own block, and the diff under it.
                    self.diff_block = tool.fold;
                }
                self.pending.push(Item::Tool(Box::new(tool)));
            }
            return;
        }
        // A call the transcript read back has no number yet: it takes one here,
        // so its row opens inside the run as a live call's row does.
        for tool in &mut tools {
            if tool.fold == 0 {
                tool.fold = self.folds.add();
            }
        }
        let fold = self.folds.add();
        self.pending
            .push(Item::Group(Box::new(Group::new(tools, fold))));
    }

    /// Adds the members the live area gave up to the run's one block: the group
    /// the transcript holds or the one the ring holds, drawn again where it
    /// stands with them.
    fn join_run(&mut self, mut members: Vec<Member>) {
        if members.is_empty() {
            return;
        }
        let fold = self.run;
        // A group the transcript has not printed yet takes them where it stands.
        for item in &mut self.pending {
            if let Item::Group(group) = item
                && group.fold == fold
            {
                group.going = true;
                group.extend(members.drain(..));
                self.dirty = true;
                return;
            }
        }
        // The ring holds the block: the group comes back, and is drawn again.
        let drawn = self.edit_block(fold, |group| {
            group.going = true;
            group.extend(members.drain(..));
        });
        if drawn {
            return;
        }
        if members.is_empty() {
            return;
        }
        // The ring no longer holds the block: the rest of the run draws a new one.
        self.run = self.folds.add();
        let mut group = Group::empty(self.run);
        group.going = true;
        group.extend(members);
        if group
            .newest_edit()
            .is_some_and(|edit| edit.fold == self.folds.diff())
        {
            // The newest edit shows its diff under the new block now.
            self.diff_block = self.run;
        }
        self.pending.push(Item::Group(Box::new(group)));
        self.dirty = true;
    }

    /// Ends the run of calls now going: its block takes the live area's members
    /// and draws folded from here on.
    fn end_run(&mut self) {
        if self.run == 0 {
            return;
        }
        let count = self.run_prefix();
        self.flush_live(count);
        self.close_run();
    }

    /// Folds the block of the run now going into its one row, and answers that no
    /// run goes any more.
    fn close_run(&mut self) {
        let fold = std::mem::take(&mut self.run);
        if fold == 0 {
            return;
        }
        // A block the transcript has not printed yet draws folded when it does.
        for item in &mut self.pending {
            if let Item::Group(group) = item
                && group.fold == fold
            {
                group.going = false;
                self.dirty = true;
                return;
            }
        }
        // The ring holds the block: it draws folded where it stands.
        self.edit_block(fold, |group| group.going = false);
        self.dirty = true;
    }

    /// Adds the dim rule a run ends with: its time, its tokens and its cost.
    fn rule(&mut self) {
        let seconds = self.run_since.take().map_or(0, |since| {
            since.elapsed().as_secs().min(u64::from(u32::MAX)) as u32
        });
        self.pending.push(Item::Rule(Usage {
            seconds,
            tokens: self.run_tokens,
            cents: self.run_cents,
        }));
        self.run_tokens = 0;
        self.run_cents = 0;
    }

    /// Starts the run now going, whose rule sums every call of it: a run that
    /// took no turn start of its own, because this client was away when it
    /// began, starts with its next one.
    fn run_start(&mut self) {
        if self.run_since.is_none() {
            self.run_since = Some(Instant::now());
            self.run_tokens = 0;
            self.run_cents = 0;
        }
    }

    /// Freezes every live entry: nothing in it grows again, and the run of calls
    /// now going ends.
    fn freeze_all(&mut self) {
        self.end_run();
        let count = self.live.len();
        self.flush_live(count);
    }

    /// Freezes the replies and the reasoning of the live area and leaves the
    /// tool calls that wait for their result: a call that started keeps its
    /// place until its own result arrives, and the reasoning of a run that still
    /// goes keeps its order there.
    fn freeze_text(&mut self) {
        self.settle_tools();
        let live = std::mem::take(&mut self.live);
        let mut kept = Vec::with_capacity(live.len());
        for mut item in live {
            if matches!(item, Item::Tool(_)) || self.run != 0 && matches!(item, Item::Thought(_)) {
                kept.push(item);
            } else {
                item.finish();
                self.pending.push(item);
            }
        }
        self.live = kept;
        // Whatever the live area held of a reply is finished now.
        self.awaits_replay = false;
    }

    /// Takes one frame of the worker.
    fn frame(&mut self, frame: Frame<'_>) {
        if matches!(frame, Frame::Unknown) {
            return;
        }
        match frame {
            Frame::TurnStart => {
                self.running();
                self.run_start();
            }
            Frame::TextDelta(text) => {
                self.running();
                let bytes = text.len();
                self.text(&text);
                self.panels_update(&Update::Streamed { bytes });
            }
            Frame::ThinkingDelta(text) => {
                self.running();
                let bytes = text.len();
                self.thinking(&text);
                self.panels_update(&Update::Streamed { bytes });
            }
            Frame::ToolStart { id, name } => {
                self.running();
                self.tool_start(&id, &name);
            }
            Frame::ToolCallReady {
                id,
                name,
                arguments,
            } => self.tool_ready(&id, &name, &arguments),
            Frame::ToolResult {
                id,
                bytes,
                error,
                output,
                earlier,
                images,
                ..
            } => {
                let tail = output.as_deref().map(|output| (output, earlier));
                self.tool_result(&id, bytes, tail, error.as_deref());
                // The bytes of an image are in the journal, and the record that
                // holds them comes next.
                self.pictures = self.pictures || images > 0;
            }
            Frame::TurnEnd => self.turn_end(false),
            Frame::RunEnd { at_limit } => {
                if at_limit {
                    self.note(TURN_LIMIT_LINE.to_string());
                }
                // The run ends with every record that carries its images, so no
                // commit of it asks for another read.
                self.pictures = false;
                self.turn_end(true);
            }
            Frame::UserMessage { text, images } => {
                self.push_user(&text);
                // The images of the message, pasted or attached with `@`, are in
                // the journal: the frame carries only their count.
                self.pictures = self.pictures || images > 0;
                // The message the worker echoes is the one a steer row held, so
                // the row goes now that the turn has it.
                self.live.retain(|item| !matches!(item, Item::Steer { .. }));
                self.drew_live(text.as_ref());
                self.history.push(text.into_owned());
                // A message starts the run it steers or the one it queues.
                self.running();
            }
            Frame::Error(message) => {
                // The session answering that a compaction has nothing to fold in
                // is not a failed turn.
                let message = match message.as_ref() {
                    "nothing to compact" => message.into_owned(),
                    _ => format!("error: {message}"),
                };
                self.note(message);
            }
            Frame::Compacted {
                before,
                after,
                replaced,
            } => self.compacted(before, after, replaced),
            Frame::Retry { attempt, error } => {
                let message = format!("retry {attempt}: {error}");
                self.note(message);
            }
            Frame::Committed(record) => {
                self.upto = self.upto.max(record);
                // A live call carries its own tail, so the records it settled
                // through hold nothing this client needs: reading them would
                // make a call cost the length of the run.
                if !self.unfilled {
                    self.read = self.read.max(self.upto);
                }
                // A record can commit before the one that holds the image a frame
                // named, so every commit while the flag is set asks for a read.
                if self.pictures {
                    self.pictures_wait = true;
                }
            }
            Frame::ApprovalRequest { request, saves } => self.approval_request(request, saves),
            Frame::ApprovalResolved { id } => self.resolved(id),
            Frame::QuestionRequest { id, questions } => self.question_request(id, &questions),
            Frame::QuestionResolved { id } => self.question_resolved(id),
            Frame::Closing(reason) => self.closing(&reason),
            Frame::ClientJoined { id, .. } if id == self.client => {}
            Frame::ClientJoined { id, name } => self.client_joined(id, &name),
            Frame::ClientLeft { id } => self.client_left(id),
            Frame::PluginCommands(commands) => self.plugin_commands(&commands),
            Frame::PluginPanel { id, title, lines } => {
                self.plugin_panel(&id, &title, &lines);
            }
            Frame::Note(text) => self.note(text.into_owned()),
            Frame::Settings {
                model,
                context_window,
                level,
                mode,
            } => {
                self.panels_update(&Update::Model {
                    name: &model,
                    window: context_window.unwrap_or(0),
                });
                self.set_settings(level.as_deref(), mode.as_deref());
                self.level = level.as_deref().map(Arc::from);
            }
            Frame::Usage {
                input,
                cached,
                output,
                cost,
            } => {
                self.run_tokens = self.run_tokens.saturating_add(input).saturating_add(output);
                self.tokens = self.tokens.saturating_add(input).saturating_add(output);
                if let Some(cost) = cost {
                    let cents = (cost.max(0.0) * 100.0).round() as u32;
                    self.run_cents = self.run_cents.saturating_add(cents);
                    self.cents = self.cents.saturating_add(cents);
                }
                self.panels_update(&Update::Usage {
                    input,
                    cached,
                    output,
                });
            }
            Frame::Unknown => {}
        }
        self.touch();
    }

    /// Marks the run as going, so Enter steers and Esc cancels.
    fn running(&mut self) {
        self.set_running(true);
        self.panels_update(&Update::Busy(true));
    }

    /// Takes the run state the worker's answer to `attach` names: the `run_end`
    /// of a run that ended while this client was away is in no frame it gets,
    /// and neither is the start of one that began while it was away.
    fn set_running(&mut self, running: bool) {
        if self.running != running {
            self.running = running;
            self.status_dirty = true;
        }
    }

    /// The worker cut this client off: the reason `behind` is a client that fell
    /// behind, which resyncs by itself, and ends the app only when it happens
    /// too often; every other reason names itself and ends it.
    fn closing(&mut self, reason: &str) {
        if reason == "behind" {
            self.cut(Resync::Behind);
            return;
        }
        let text = format!("the worker closed the session: {reason}");
        self.note(text);
        self.code = 1;
        self.exit = true;
    }

    /// The connection closed without a word: the app reconnects and finds out
    /// whether the session is still there.
    fn ended(&mut self) {
        self.cut(Resync::Closed);
    }

    /// Takes the cut as one the app resyncs from, and names what it is; past
    /// [`RESYNCS`] resyncs within [`RESYNC_WINDOW`] the app gives up instead, so
    /// a worker that keeps cutting this client does not make it spin.
    fn cut(&mut self, why: Resync) {
        let now = Instant::now();
        while self
            .cuts
            .front()
            .is_some_and(|at| now.duration_since(*at) > RESYNC_WINDOW)
        {
            self.cuts.pop_front();
        }
        if self.cuts.len() == RESYNCS {
            self.note(
                "cut off by the worker again and again: run reagent -c to continue".to_owned(),
            );
            self.code = 1;
            self.exit = true;
            return;
        }
        self.cuts.push_back(now);
        self.resync = Some(why);
        self.dirty = true;
    }

    /// Makes the entries that still grow stale: a tool call and the open reply
    /// are replayed by the worker from their start, so they leave the live area
    /// here instead of being drawn twice. The transcript printed so far stays as
    /// it is.
    fn stale(&mut self) {
        self.live.retain(|item| !matches!(item, Item::Tool(_)));
        // The reply goes when its replay arrives, since a worker that committed
        // it before this client came back replays nothing.
        self.awaits_replay = true;
        self.dirty = true;
    }

    /// Drops the reply on screen, so that the copy the journal holds takes its
    /// place instead of following it.
    fn drop_open_reply(&mut self) {
        self.live.retain(|item| {
            !matches!(
                item,
                Item::Reply { .. } | Item::Thought(_) | Item::Thinking(_)
            )
        });
        self.awaits_replay = false;
    }

    /// Asks for a frame, at most one per [`FRAME`], since frames arrive in
    /// bursts behind a streamed reply.
    ///
    /// A deadline further out than that is pulled in: the bar's next step and a
    /// panel's tick are a tenth and a second away, and a delta or a key must
    /// land on the next frame, not on the next step.
    fn touch(&mut self) {
        self.dirty = true;
        let at = tokio::time::Instant::now() + FRAME;
        if self.deadline.is_none_or(|deadline| deadline > at) {
            self.deadline = Some(at);
        }
        self.arm_tick();
    }

    /// Arms the next tick, and only when a shown panel asked for one: the
    /// sidebar has no timer of its own.
    fn arm_tick(&mut self) {
        if self.aside.rect().is_none() || !self.sidebar.wants_tick() {
            self.tick = None;
            return;
        }
        // A tick already armed keeps the instant it was armed for: frames come
        // one after another while a run streams, and a frame that pushed the
        // tick out again would keep a panel that wants one from ever getting it.
        let now = Instant::now();
        let at = match self.tick {
            Some(at) if at > now => at,
            _ => now + TICK,
        };
        self.tick = Some(at);
        let due = tokio::time::Instant::from_std(at);
        if self.deadline.is_none_or(|deadline| due < deadline) {
            self.deadline = Some(due);
        }
    }

    /// What a due deadline was: one tick of the panels that want one, and the
    /// images of the pictures that waited out their settle time.
    async fn timer(&mut self, terminal: &mut impl Terminal) -> io::Result<()> {
        self.deadline = None;
        if self.tick.is_some_and(|at| at <= Instant::now()) {
            self.tick = None;
            self.panels_update(&Update::Tick);
        }
        match self.picture_deadline {
            Some(at) if at <= tokio::time::Instant::now() => self.settle_pictures(terminal).await,
            Some(at) => {
                if self.deadline.is_none_or(|deadline| deadline > at) {
                    self.deadline = Some(at);
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// A piece of the reply's text: it lands in the live reply's buffer.
    fn text(&mut self, delta: &str) {
        if self.awaits_replay {
            // This is the worker's replay of the open reply from its start: the
            // text that came before the cut goes, so it is drawn once.
            self.drop_open_reply();
        }
        let width = self.transcript_width();
        if !matches!(self.live.last(), Some(Item::Reply { .. })) {
            // A run of calls ends where the text of the next reply starts.
            self.end_run();
            let mut md = Box::new(Markdown::new(width));
            md.set_theme(&self.look.theme);
            self.live.push(Item::Reply { md, done: 0 });
        }
        if let Some(Item::Reply { md, .. }) = self.live.last_mut() {
            md.set_width(width);
            md.push(delta);
        }
    }

    /// A piece of the model's reasoning: one dim row of its time, and its newest
    /// line while the block is open.
    fn thinking(&mut self, delta: &str) {
        if !matches!(self.live.last(), Some(Item::Thought(_))) {
            // The reasoning before the first call of a run is its own block, and
            // the reasoning after it fills its place in the run.
            self.freeze_text();
            let fold = self.folds.add();
            self.live.push(Item::Thought(Box::new(Thinking {
                text: String::new(),
                since: Instant::now(),
                seconds: 0,
                fold,
            })));
        }
        let width = usize::from(self.transcript_width());
        if let Some(Item::Thought(think)) = self.live.last_mut() {
            think.text.push_str(delta);
            if let Some(at) = think.text.rfind('\n') {
                think.text.drain(..=at);
            }
            if think.text.len() > THINKING_MAX {
                let cut = think.text.len() - THINKING_MAX;
                let at = think.text.ceil_char_boundary(cut);
                think.text.drain(..at);
            }
            cut_in_place(&mut think.text, width);
        }
    }

    /// A tool call started: it gets its own entry, so two calls in one turn
    /// keep their results apart. The first call of a run opens its block.
    fn tool_start(&mut self, id: &str, name: &str) {
        self.freeze_text();
        if self.run == 0 {
            self.run = self.folds.add();
        }
        let mut tool = Tool::new(id, name);
        tool.fold = self.folds.add();
        self.live.push(Item::Tool(Box::new(tool)));
    }

    /// A tool call's arguments are complete.
    fn tool_ready(&mut self, id: &str, name: &str, arguments: &str) {
        let width = self.transcript_width();
        let mut edit = None;
        if let Some(tool) = self.tool(id) {
            tool.set_call(name, arguments, width);
            if !tool.diff.is_empty() {
                edit = Some(tool.fold);
            }
        }
        if let Some(call) = edit {
            let block = if self.run == 0 { call } else { self.run };
            self.newest_diff(call, block);
        }
        if self.note_call(id) {
            self.panels_update(&Update::ToolCall { name, arguments });
        }
    }

    /// Hands one tool call to the panels unless they took it already.
    fn feed_call(&mut self, call: &ToolCall) {
        if !self.note_call(&call.id) {
            return;
        }
        self.panels_update(&Update::ToolCall {
            name: call.name.as_ref(),
            arguments: call.arguments.as_ref(),
        });
    }

    /// Whether this is the first time the client shows the call `id`, which it
    /// then remembers. The queue keeps the newest ids only: a repeat of a call
    /// follows the frame that drew it by a read, not by a whole session.
    fn note_call(&mut self, id: &str) -> bool {
        if self.fed.iter().any(|seen| seen == id) {
            return false;
        }
        if self.fed.len() == FED_CALLS {
            self.fed.pop_front();
        }
        self.fed.push_back(id.to_owned());
        true
    }

    /// The live tool call `id`.
    fn tool(&mut self, id: &str) -> Option<&mut Tool> {
        self.live.iter_mut().find_map(|item| match item {
            Item::Tool(tool) if tool.id == id => Some(tool.as_mut()),
            _ => None,
        })
    }

    /// A tool call produced its result: it shows the tail its frame carried,
    /// the text the journal recorded, or its size while neither is there.
    ///
    /// A frame without a tail leaves the text to the journal, which is what a
    /// worker from before the tail sends.
    fn tool_result(
        &mut self,
        id: &str,
        bytes: usize,
        output: Option<(&str, u64)>,
        error: Option<&str>,
    ) {
        let width = self.transcript_width();
        if let Some(tool) = self.tool(id) {
            if let Some((tail, earlier)) = output {
                tool.set_tail(tail, earlier, width);
            }
            tool.set_result(bytes, error, width);
        }
        self.unfilled |= output.is_none();
        self.settle_tools();
    }

    /// Moves the calls of the run that have settled, and the thoughts before
    /// them, out of the live area and into the run's one block.
    fn settle_tools(&mut self) {
        // Inline mode prints a run once, at its end, so its calls stay live.
        if self.run == 0 || !self.screen.is_full() {
            return;
        }
        let count = self.run_prefix();
        self.flush_live(count);
    }

    /// How many live entries the run of calls now going gives up: its calls that
    /// settled and its thoughts, up to the call that still waits for its result.
    fn run_prefix(&self) -> usize {
        self.live
            .iter()
            .position(|item| match item {
                Item::Tool(tool) => !tool.settled(),
                Item::Thought(_) => self.run == 0,
                _ => true,
            })
            .unwrap_or(self.live.len())
    }

    /// Takes the output text the journal recorded for the call `id`.
    fn fill(&mut self, id: &str, text: &str) {
        let width = self.transcript_width();
        if let Some(tool) = self.tool(id) {
            tool.set_output(text, width);
        }
        self.settle_tools();
    }

    /// Reads the journal records the worker committed since the last read, so a
    /// tool result shows its output text.
    async fn catch_up(&mut self) {
        if let Some((fold, from)) = self.wanted.take() {
            self.read_checkpoint(fold, from).await;
        }
        self.take_sessions().await;
        self.read_pictures().await;
        if self.upto <= self.read {
            return;
        }
        if self.journal.is_none() {
            // A new session's folder appears with its first record, which comes
            // after this client attached.
            self.journal = session_folder(&self.session).await;
            if self.journal.is_none() {
                return;
            }
        }
        let Some(dir) = self.journal.take() else {
            return;
        };
        let results = history::results(dir.clone(), self.read, self.upto).await;
        self.journal = Some(dir);
        match results {
            Ok(results) => {
                self.read = self.upto;
                self.unfilled = false;
                for (id, text) in &results {
                    self.fill(id, text);
                }
            }
            Err(error) => self.note(format!("journal: {error}")),
        }
    }

    /// Reconnects after the worker cut this client off for falling behind.
    ///
    /// The transcript stays on screen and the open reply goes stale, since the
    /// worker replays it whole. The journal records the worker committed while
    /// this client was away are read and applied, so a reply that finished and a
    /// tool result that landed are not lost, and then this client attaches from
    /// the record it read: the worker's catch-up frame carries the rest of the
    /// open reply from its start, exactly once.
    async fn reconnect(&mut self, cut: Client, options: &Options) -> io::Result<Client> {
        // The new connection comes first: a worker left with no client stops as
        // soon as its run ends, so the dial has to win the race with that exit.
        let (mut client, init) =
            Client::connect(&options.socket, &options.token, &options.client_name).await?;
        drop(cut);
        self.stale();
        self.rejoined(&init);
        // The answer to `attach` can name a later record than the handshake, as
        // a record may commit between the two, and it says whether a run is open
        // now.
        self.missed(init.committed).await;
        let attached = client.attach(self.read).await?;
        self.missed(attached.committed).await;
        self.upto = self.upto.max(attached.committed);
        self.set_running(attached.running);
        // The status line says so once, until the next frame changes it.
        self.resynced = true;
        self.status_dirty = true;
        self.touch();
        Ok(client)
    }

    /// Takes what a new handshake says: another client may have set the level
    /// while this one was away.
    fn rejoined(&mut self, init: &Initialized) {
        self.level.clone_from(&init.level);
    }

    /// Reads the messages the journal recorded after the last record this app
    /// read and up to `upto`, and applies them.
    async fn missed(&mut self, upto: u64) {
        if upto <= self.read {
            return;
        }
        if self.journal.is_none() {
            // A new session's folder appears with its first record, which comes
            // after this client attached.
            self.journal = session_folder(&self.session).await;
            if self.journal.is_none() {
                return;
            }
        }
        let Some(dir) = self.journal.take() else {
            return;
        };
        let room = self.room();
        let mut messages = history::since(dir.clone(), self.read, upto, room).await;
        self.journal = Some(dir);
        match messages {
            Ok(ref mut messages) => {
                self.read = upto;
                self.unfilled = false;
                self.apply(messages);
            }
            Err(error) => self.note(format!("journal: {error}")),
        }
    }

    /// Takes the messages the journal recorded while this client was cut: a
    /// finished reply takes the place of the stale open one, and a tool result
    /// its call.
    ///
    /// Every message arrives with the record that holds it. A record the app
    /// drew already, or a user message a live frame drew before the record
    /// committed it, is not drawn again: a cut does not repeat the transcript.
    fn apply(&mut self, messages: &mut [(u64, Message, Vec<Picture>)]) {
        if messages.is_empty() {
            return;
        }
        // A result whose call is not in the live area takes the name and the
        // target of the call the record of this catch-up holds.
        let mut calls: Vec<&ToolCall> = Vec::new();
        for (record, message, pictures) in messages.iter_mut() {
            if *record > self.drawn {
                self.drawn = *record;
                match message {
                    Message::User { content } => {
                        if !self.already_drawn(content) {
                            let text = self.user_message(content);
                            if !text.is_empty() {
                                self.history.push(text);
                            }
                        }
                    }
                    Message::Assistant(reply) => {
                        calls.extend(calls_of(&reply.content));
                        if self.shows(reply) {
                            // The record holds what the app drew of that reply, whole:
                            // the turn is over, so it leaves the live area for the
                            // transcript instead of being drawn a second time.
                            self.freeze_text();
                        } else {
                            // The reply this client was streaming is the one the
                            // record finished, so it goes before the finished copy
                            // is drawn.
                            self.drop_open_reply();
                            self.finished(reply);
                        }
                        // The reply this client was streaming is finished in the
                        // journal, so that turn is over. A run that goes on says so
                        // with its next frame.
                        self.running = false;
                        self.status_dirty = true;
                    }
                    Message::ToolResult(result) => {
                        if self.tool(&result.call_id).is_some() {
                            self.fill(&result.call_id, &result.content);
                        } else {
                            let call = calls.iter().find(|call| call.id == result.call_id).copied();
                            let tool = self.replayed(result, call);
                            self.pending.push(Item::Tool(Box::new(tool)));
                        }
                    }
                }
            }
            // A record the app drew live may still hold an image whose picture
            // no path drew yet.
            if *record > self.pictured {
                self.pictured = *record;
                self.pictures_of(pictures.drain(..));
            }
            // The record was read here, so the pictures read skips it.
            self.picture_read = self.picture_read.max(*record);
        }
        self.dirty = true;
    }

    /// Reads the record a live frame said holds an image, and puts its pictures
    /// under the turn or the call that came with it.
    ///
    /// A frame carries no image bytes, so the picture of one costs a read of the
    /// journal, and only a frame that named an image costs that read. The images
    /// are decoded in the read's own blocking job. The read starts after the last
    /// record read for its own sake, so a record that holds no image is read once.
    async fn read_pictures(&mut self) {
        if !self.pictures_wait || self.upto <= self.picture_read {
            return;
        }
        self.pictures_wait = false;
        if self.journal.is_none() {
            self.journal = session_folder(&self.session).await;
            if self.journal.is_none() {
                return;
            }
        }
        let Some(dir) = self.journal.take() else {
            return;
        };
        let room = self.room();
        let messages = history::since(dir.clone(), self.picture_read, self.upto, room).await;
        self.journal = Some(dir);
        match messages {
            Ok(mut messages) => {
                self.picture_read = self.upto;
                self.placeholder_pictures(&mut messages).await;
                if self.pictures_into(&mut messages) {
                    self.pictures = false;
                }
            }
            Err(error) => self.note(format!("journal: {error}")),
        }
    }

    /// Decides the terminal's own image protocol once per run, at the first
    /// picture of a full-screen one, and gives it to the screen.
    ///
    /// Inline mode never asks: its lines are the terminal's scrollback and no
    /// image sits on them.
    async fn native_pictures(&mut self) {
        if self.native_decided || !self.screen.is_full() {
            return;
        }
        self.native_decided = true;
        let tmux = std::env::var("TMUX").ok().filter(|value| !value.is_empty());
        let display = match tmux {
            Some(_) => picture_native::tmux_display().await,
            None => None,
        };
        let native = picture_native::detect(|name| std::env::var(name).ok(), display.as_deref());
        self.native = native;
        self.screen.set_native(native);
    }

    /// The PNG payload of every picture of `messages` whose source the journal
    /// gave, inside a terminal that draws placeholders: a picture whose image
    /// makes no PNG keeps its half-block cells.
    async fn placeholder_pictures(&mut self, messages: &mut [(u64, Message, Vec<Picture>)]) {
        if messages.iter().all(|(_, _, pictures)| pictures.is_empty()) {
            return;
        }
        self.native_pictures().await;
        if !self.placeholders() {
            return;
        }
        for (_, _, pictures) in messages.iter_mut() {
            for picture in pictures.iter_mut() {
                placeholder_picture(picture).await;
            }
        }
    }

    /// The pictures a resume read from the journal, made ready the same way as
    /// the ones a live frame names.
    async fn placeholder_tail(&mut self, tail: &mut history::Tail) {
        if tail.pictures.iter().all(Vec::is_empty) {
            return;
        }
        self.native_pictures().await;
        if !self.placeholders() {
            return;
        }
        for pictures in &mut tail.pictures {
            for picture in pictures.iter_mut() {
                placeholder_picture(picture).await;
            }
        }
    }

    /// Whether the terminal draws a picture as placeholders.
    fn placeholders(&self) -> bool {
        self.native
            .is_some_and(|native| native.protocol == Protocol::Placeholders)
    }

    /// Puts the pictures the journal records `messages` hold under the rows the
    /// app drew for them, answers whether it drew one, and skips a record whose
    /// pictures it drew already.
    fn pictures_into(&mut self, messages: &mut [(u64, Message, Vec<Picture>)]) -> bool {
        let mut drawn = false;
        for (record, _, pictures) in messages.iter_mut() {
            if pictures.is_empty() || *record <= self.pictured {
                continue;
            }
            self.pictured = *record;
            self.pictures_of(pictures.drain(..));
            drawn = true;
        }
        drawn
    }

    /// Adds the pictures of one record as finished entries, in the order of its
    /// images.
    fn pictures_of(&mut self, pictures: impl Iterator<Item = Picture>) {
        for picture in pictures {
            self.pending.push(Item::Picture(Box::new(picture)));
        }
        self.dirty = true;
    }

    /// A provider call ended, or the whole run did.
    ///
    /// A turn ends after every call, the ones that stop for a tool included, so
    /// only the run's end makes the session idle again; a queued run sets it
    /// back to running with its own turn start. One run draws one rule, at its
    /// end, with the time, the tokens and the cost of all its calls; a run that
    /// ends in an error or a cancel has its rule too.
    ///
    /// A turn end between the calls of one run leaves it going: only the run's
    /// end folds its calls into the one row.
    fn turn_end(&mut self, done: bool) {
        if done {
            self.freeze_all();
            self.rule();
            self.running = false;
            self.status_dirty = true;
        } else {
            self.freeze_text();
        }
        self.panels_update(&Update::Busy(!done));
        // The sessions of the folder move with every turn the worker ends.
        self.read_sessions();
    }

    /// The worker asks for an answer: opens a card for one request, with the
    /// rules an `always` answer saves.
    fn approval_request(&mut self, request: Approval<'_>, saves: Vec<Cow<'_, str>>) {
        let width = self.text_width();
        let boxed = ApprovalBox::new(request, width).saving(saves);
        if self.approval.is_none() && self.question.is_none() {
            self.approval = Some(boxed);
        } else {
            self.queue.push_back(boxed);
        }
        self.status_dirty = true;
        // The current session waits on the answer.
        self.set_sessions();
    }

    /// A question request of the `ask` tool: its card goes on screen, or waits
    /// behind the card that is open.
    fn question_request(&mut self, id: u64, questions: &[crate::client::Question<'_>]) {
        let boxed = QuestionBox::new(id, questions, std::time::Instant::now());
        if self.question.is_none() && self.approval.is_none() {
            self.question = Some(boxed);
        } else {
            self.questions.push_back(boxed);
        }
        self.status_dirty = true;
        self.dirty = true;
        self.set_sessions();
    }

    /// A question request got its answer, here or in another client.
    fn question_resolved(&mut self, id: u64) {
        if self.question.as_ref().is_some_and(|open| open.id == id) {
            self.question = None;
            self.next_card();
            self.set_sessions();
            return;
        }
        self.questions.retain(|waiting| waiting.id != id);
    }

    /// An approval request got its answer, here or in another client.
    fn resolved(&mut self, id: u64) {
        if self.approval.as_ref().is_some_and(|open| open.id == id) {
            self.approval = None;
            self.next_card();
            self.set_sessions();
            return;
        }
        self.queue.retain(|waiting| waiting.id != id);
    }

    /// Shows the next card that waits, if one waits: an open card keeps the
    /// screen, so a question and an approval never show at once.
    fn next_card(&mut self) {
        if self.approval.is_some() || self.question.is_some() {
            return;
        }
        if !self.queue.is_empty() {
            self.next_approval();
            return;
        }
        if let Some(mut open) = self.questions.pop_front() {
            open.shown = std::time::Instant::now();
            self.question = Some(open);
            self.status_dirty = true;
            self.dirty = true;
            self.set_sessions();
        }
    }

    /// Shows the next waiting box, if one waits.
    fn next_approval(&mut self) {
        self.approval = self.queue.pop_front();
        if let Some(open) = self.approval.as_mut() {
            // A box just went on screen: its keys answer it only after a while.
            open.shown = std::time::Instant::now();
        }
        self.status_dirty = true;
        // The current session waits while one box is still open.
        self.set_sessions();
    }

    /// The sections a key press is read in, for what is on screen now.
    fn key_chain(&self, overlay: Overlay, asking: bool) -> &'static [Section] {
        if self.keys.leader_pending() {
            keyfile::LEADER_CHAIN
        } else if asking {
            keyfile::ASK_CHAIN
        } else if overlay == Overlay::Approval {
            keyfile::CARD_CHAIN
        } else if overlay == Overlay::Sheet {
            keyfile::SHEET_CHAIN
        } else if overlay == Overlay::List {
            keyfile::LIST_CHAIN
        } else if self.sidebar.active() {
            keyfile::SIDEBAR_CHAIN
        } else {
            keyfile::KEYS_CHAIN
        }
    }

    /// Takes one key press.
    fn key(&mut self, event: KeyEvent) {
        let now = std::time::Instant::now();
        // A quoted insert and a character search take the next key as a
        // character, whatever the map would do with it.
        if self.editor.wants_key() && event.kind != KeyEventKind::Release {
            self.deadline = None;
            self.dirty = true;
            return self.edit(EditKey::NextKey(quoted_char(&event)));
        }
        let refusing = self.approval.as_ref().is_some_and(ApprovalBox::refuses);
        let armed = self
            .approval
            .as_ref()
            .is_some_and(|open| open.ready(now, !self.editor.is_empty()));
        // A question card takes its keys once it has been on screen a moment,
        // and a typed answer takes them whatever the box holds.
        let asking = self
            .question
            .as_ref()
            .is_some_and(|open| open.typing() || open.ready(now, !self.editor.is_empty()));
        let overlay = match (&self.approval, refusing) {
            (Some(_), true) => Overlay::Popup,
            (Some(_), false) if armed => Overlay::Approval,
            (Some(_), false) => Overlay::None,
            (None, _) if self.sheet.is_some() => Overlay::Sheet,
            (None, _) if self.list.is_some() => Overlay::List,
            (None, _) => Overlay::None,
        };
        // A key the file moved reads as the first key of its action, before the
        // map or any raw reader sees it, and a key it swallowed goes no further.
        let event = match &self.keyfile {
            Some(file) => match file.translate(self.key_chain(overlay, asking), event) {
                Some(event) => event,
                None => return,
            },
            None => event,
        };
        // Ctrl+G is the Emacs cancel: it does what Esc does, except that a history
        // search gives the draft back on it and keeps the match on Esc.
        let event = if crate::keys::is_ctrl_g(&event) && !self.editor.searching() {
            KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                ..event
            }
        } else {
            event
        };
        let mut action = self.keys.action(event, overlay, now);
        // Ctrl+D on an empty box is the quit key, as Ctrl+C is: the map does not
        // see the text, so the app gives the key to the quit here. A Ctrl+D that
        // edits the box is not the quit, so a pending arming goes with it.
        if action == Action::Edit(EditKey::DeleteChar) {
            if self.editor.is_empty() && overlay == Overlay::None {
                action = self.keys.quit(now);
            } else {
                self.keys.disarm();
            }
        }
        // A key draws at once, whatever a frame in front of it armed.
        self.deadline = None;
        self.dirty = true;
        // The incremental search holds the keys of the box while it is open: a
        // printable key and Backspace edit its query, Ctrl+R and Ctrl+S step it,
        // Ctrl+G aborts it and Esc ends it. Enter ends it and sends the match,
        // and every other key ends it and then runs as usual.
        if self.editor.searching() && overlay == Overlay::None {
            let taken = self.search_key(action);
            self.editor_dirty = true;
            if taken {
                return;
            }
        }
        if let Some(leader) = self.keys.take_leader() {
            return self.leader(leader);
        }
        // The question card owns the keys while it is open, before the sidebar,
        // the scroll keys and the fold keys behind it see them.
        if asking {
            match action {
                Action::Quit => return self.quit(),
                Action::QuitArmed => self.armed = true,
                _ => return self.question_key(action),
            }
        }
        // An open list or sheet owns Tab, the arrows and Esc, so the sidebar,
        // the scroll keys and the fold keys behind it do not see them.
        let covered = self.list.is_some() || self.sheet.is_some();
        // Left and Right turn the page of the Sessions panel while it has the
        // focus, which the sidebar's own keys leave to the editor.
        if !covered && self.sessions_page_key(event) {
            return;
        }
        // A focused sidebar takes the keys it owns before the editor sees them.
        if !covered && self.sidebar_key(event) {
            return;
        }
        if !covered && self.scrolls(event) {
            return;
        }
        if !covered && self.fold_key(event) {
            return;
        }
        // The open sheet takes the keys the editor would: its filter is typed
        // into it and Esc gives the draft back.
        if self.sheet_key(action) {
            return;
        }
        if refusing {
            match action {
                Action::Send => return self.refuse(true),
                // Esc in the note goes back to the choices: only Esc on the
                // card itself refuses it.
                Action::Cancel => return self.close_note(),
                Action::Edit(key) => return self.reason_key(key),
                Action::Quit => return self.quit(),
                Action::QuitArmed => self.armed = true,
                _ => return,
            }
        }
        match action {
            Action::Send => {
                if !self.run_line() {
                    self.send(None);
                }
            }
            Action::Queue => self.send(Some(Mode::Queue)),
            Action::NewLine => {
                // A line break is one line break whatever the argument is.
                self.editor.clear_argument();
                self.edit(EditKey::Insert('\n'));
            }
            Action::Cancel => {
                if armed {
                    // Esc in the box refuses it at once, with no reason.
                    self.refuse(false);
                } else if self.running {
                    self.commands.push_back(Command::Cancel);
                }
            }
            Action::Quit => self.quit(),
            Action::QuitArmed => {
                self.armed = true;
                self.status_dirty = true;
            }
            Action::Approval(key) => self.answer(key),
            Action::Palette => self.toggle_palette(),
            Action::Redraw => self.screen.redraw(),
            Action::Move(delta) => {
                if let Some(list) = self.list.as_mut() {
                    list.step(delta);
                }
            }
            Action::Complete => self.complete(),
            Action::Pick => self.pick(),
            Action::Close => self.list = None,
            Action::Edit(key) => self.edit(key),
            Action::Delete | Action::Ignore => {}
        }
    }

    /// Takes one key of the incremental search, and answers whether the search
    /// took it: a key that ends the search runs as usual after it.
    fn search_key(&mut self, action: Action) -> bool {
        let (editor, history) = (&mut self.editor, &self.history);
        match action {
            Action::Edit(EditKey::Insert(c)) => editor.search_push(c, history),
            Action::Edit(EditKey::Backspace) => editor.search_pop(history),
            Action::Edit(EditKey::SearchBackward) => editor.search_history(false, history),
            Action::Edit(EditKey::SearchForward) => editor.search_history(true, history),
            Action::Edit(EditKey::AbortSearch) => editor.search_abort(),
            Action::Cancel => editor.search_end(),
            _ => {
                editor.search_end();
                return false;
            }
        }
        true
    }

    /// One key after Ctrl+X: the two sheets it opens, and the sidebar's own.
    fn leader(&mut self, leader: Leader) {
        match leader {
            Leader::Model => self.open_models(),
            Leader::Level => self.open_levels(),
            Leader::Sessions => self.open_sessions(),
            Leader::Commands => self.toggle_palette(),
            other => self.sidebar_leader(other),
        }
    }

    /// Takes one key of the open sheet, and answers whether it was one: printable
    /// keys and Backspace filter it, Up and Down step it, Enter picks and Esc
    /// gives the text box and its draft back.
    fn sheet_key(&mut self, action: Action) -> bool {
        let Some(open) = self.sheet.as_mut() else {
            return false;
        };
        // Any key but the delete itself clears what a first Ctrl+D armed, so an
        // armed row can never turn into another row.
        let deleting = action == Action::Delete;
        if !deleting {
            open.disarm();
        }
        match action {
            Action::Edit(EditKey::Insert(c)) => {
                open.sheet.push(c);
                self.sheet_preview();
            }
            Action::Edit(EditKey::Backspace) => {
                open.sheet.pop();
                self.sheet_preview();
            }
            Action::Move(delta) => {
                open.sheet.step(delta);
                self.sheet_preview();
            }
            Action::Pick => self.pick_sheet(),
            Action::Close => self.sheet = None,
            Action::Delete => self.delete_session(),
            _ => return false,
        }
        self.dirty = true;
        true
    }

    /// Ctrl+D in the sessions sheet: the first press arms the row on screen, the
    /// second deletes the id it armed.
    fn delete_session(&mut self) {
        if self.waits.delete.is_some() {
            return;
        }
        // A worker that is starting holds no lock yet, so the store's check
        // cannot see it.
        if self.waits.open.is_some() {
            self.note("wait: a worker is starting".to_owned());
            return;
        }
        let Some(open) = self.sheet.as_mut() else {
            return;
        };
        let SheetKind::Sessions(state) = &mut open.kind else {
            return;
        };
        // The row on screen, found in the rows the sheet was built from: the
        // folder's list may have moved since.
        let Some(id) = sessions::picked(&open.sheet, &state.rows) else {
            return;
        };
        let Some(row) = state.rows.iter().find(|row| row.id == id) else {
            return;
        };
        let (current, live) = (row.current, row.live);
        let title: Arc<str> = Arc::from(sessions::name(row));
        if state.armed.as_deref() == Some(&*id) {
            state.armed = None;
            open.sheet.confirm = None;
            self.start_delete(id, title);
            return;
        }
        if current {
            self.note("this is the current session".to_owned());
            return;
        }
        if live {
            self.note("a worker runs this session".to_owned());
            return;
        }
        // The prompt is built here, once, and drawn every frame until a key
        // clears it: the key is the one a keys file moved the delete to.
        let key = match &self.keyfile {
            Some(file) => file.labels().delete(),
            None => "ctrl+d",
        };
        let confirm: Arc<str> = Arc::from(format!("{key} again deletes \"{title}\""));
        open.sheet.confirm = Some(confirm);
        state.armed = Some(id);
    }

    /// Starts the host's delete of the session the sheet armed.
    fn start_delete(&mut self, id: Arc<str>, title: Arc<str>) {
        let Some(host) = self.host.as_ref() else {
            self.note("no delete without the binary's host".to_owned());
            return;
        };
        self.waits.delete = Some(Deleting {
            job: host.get().delete(&id),
            id,
            title,
        });
    }

    /// The host's answer to a delete: the row goes, or the sheet notes why not.
    /// The answer names the id it started with, whatever the sheet shows now.
    fn deleted(&mut self, id: Arc<str>, title: Arc<str>, answer: Result<(), String>) {
        match answer {
            Ok(()) => {
                self.forget_session(&id);
                self.note(format!("deleted \"{title}\""));
            }
            Err(text) => self.note(text),
        }
        self.dirty = true;
    }

    /// Takes the session `id` out of the folder's list, and builds the sessions
    /// sheet again from the rows that are left when it is still open.
    fn forget_session(&mut self, id: &str) {
        if let Some(at) = self.sessions.iter().position(|row| &*row.id == id) {
            self.sessions.remove(at);
        }
        self.set_sessions();
        let Some(open) = self.sheet.as_mut() else {
            return;
        };
        let SheetKind::Sessions(state) = &mut open.kind else {
            return;
        };
        state.rows = self.sessions.clone();
        state.armed = None;
        let sheet = sessions::rebuilt(&state.rows, now_seconds(), &open.sheet);
        open.sheet = sheet;
        open.name_keys(self.keyfile.as_ref());
        self.sheet_preview();
    }

    /// The keys of the transcript rows: Tab folds or opens the finished block
    /// it acts on, else the newest block of the live area, and Ctrl+O opens
    /// every block. A Tab on a paste chip opens or folds the chip instead.
    /// Answers whether the key was one of them.
    fn fold_key(&mut self, event: KeyEvent) -> bool {
        // An open approval card owns the keys: Tab on its note, and Tab on the
        // card itself, is not a key of the transcript behind it.
        if self.approval.is_some() {
            return false;
        }
        match (event.code, event.modifiers) {
            (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
                self.folds.open_all();
                self.reopen_all();
                true
            }
            (KeyCode::Tab, modifiers) if modifiers.is_empty() => {
                // The tab of a chip at the edit point opens it or folds it, and
                // an open chip folds even with the edit point off it.
                if self.editor.toggle_paste() {
                    self.editor_dirty = true;
                    return true;
                }
                if self.editor.is_empty() {
                    if !self.toggle_finished() {
                        self.folds.toggle_at(None);
                        self.refold();
                    }
                    return true;
                }
                // With no run a queued message sends at once, and Tab must not
                // send by accident: only a run leaves Tab to the queue.
                !self.running
            }
            _ => false,
        }
    }

    /// Left and Right turn the page of the sessions while the sidebar has the
    /// keys and the Sessions panel has the focus, which the sidebar's own keys
    /// leave to the editor. Answers whether the key was the panel's.
    fn sessions_page_key(&mut self, event: KeyEvent) -> bool {
        let delta = match event.code {
            KeyCode::Left => -1,
            KeyCode::Right => 1,
            _ => return false,
        };
        if !self.sidebar.active() {
            return false;
        }
        let focused = self
            .sidebar
            .focused()
            .is_some_and(|panel| panel.id() == SESSIONS);
        if !focused {
            return false;
        }
        if let Some(panel) = self.sidebar.get_mut::<SessionsPanel>()
            && panel.page(delta)
        {
            self.touch();
        }
        true
    }

    /// Takes one bracketed paste: the editor keeps the bytes and folds a paste
    /// of more than three lines into one chip at the edit point. The text moves
    /// into the editor, so the paste is held once.
    ///
    /// A paste of no text is a copy of an image, and a paste of the path of one
    /// is a file a terminal dropped: both go to the folder of the session as a
    /// chip. Every other paste is text and reads no file.
    fn paste(&mut self, text: String) {
        self.deadline = None;
        self.dirty = true;
        if let Some(field) = self
            .question
            .as_mut()
            .filter(|open| open.typing())
            .and_then(QuestionBox::field_mut)
        {
            field.insert(&text);
            return;
        }
        // A paste is no key of the search: it ends the search and keeps the
        // match in the box, the way every other key does.
        self.editor.search_end();
        if text.is_empty() {
            // The clipboard's image, with nothing to say when it holds none.
            self.request_image(ImageRequest::clipboard(true));
            return;
        }
        if let Some(path) = image_path(&text) {
            // A file it cannot read is the paste it would have been: the word is
            // still the path, and the worker reads it when the message goes.
            self.request_image(ImageRequest::file(path, text));
            return;
        }
        self.editor.paste(text);
        self.editor_dirty = true;
    }

    /// Ctrl+V: the image the local clipboard holds, as a chip in the box.
    fn paste_image(&mut self) {
        self.request_image(ImageRequest::clipboard(false));
    }

    /// Starts an image paste, or puts it behind the one that runs.
    fn request_image(&mut self, request: ImageRequest) {
        if self.image_job.is_some() {
            self.image_queue.push_back(request);
            return;
        }
        self.spawn_image(request);
    }

    /// Runs `request` on a blocking job of its own, so no frame waits for a
    /// pasteboard or a file.
    fn spawn_image(&mut self, request: ImageRequest) {
        let Some(folder) = self.journal.clone() else {
            self.image_pasted(request, Err(IMAGE_NO_FOLDER.to_owned()));
            return;
        };
        let source = request.source.clone();
        let handle = tokio::task::spawn_blocking(move || save_image(&folder, &source));
        self.image_job = Some(ImageJob { request, handle });
    }

    /// Takes what an image job answered: the chip of the image it wrote, the
    /// note of a clipboard with none, or the paste a dropped path falls back to.
    fn image_pasted(&mut self, request: ImageRequest, result: Result<Option<Pasted>, String>) {
        self.image_job = None;
        match result {
            Ok(Some(pasted)) => {
                self.editor.paste_image(&pasted.path, pasted.number);
                self.editor_dirty = true;
                self.dirty = true;
            }
            Ok(None) => match &request.source {
                ImageSource::Clipboard if !request.quiet => self.note(IMAGE_NONE.to_owned()),
                ImageSource::Clipboard => {}
                ImageSource::File(_) => self.paste_fallback(request.fallback),
            },
            Err(message) => match &request.source {
                // An empty paste only asked: a terminal may send one with no image in mind.
                ImageSource::Clipboard if request.quiet => {}
                ImageSource::Clipboard => self.note(format!("image: {message}")),
                ImageSource::File(_) => self.paste_fallback(request.fallback),
            },
        }
        self.next_image();
    }

    /// Puts a dropped path's text in the box as the paste it would have been: its
    /// bytes could not be read, and no note says so.
    fn paste_fallback(&mut self, text: Option<String>) {
        let Some(text) = text else {
            return;
        };
        self.editor.paste(text);
        self.editor_dirty = true;
        self.dirty = true;
    }

    /// Starts the image paste that waited behind the one that ended.
    fn next_image(&mut self) {
        if self.image_job.is_some() {
            return;
        }
        if let Some(request) = self.image_queue.pop_front() {
            self.spawn_image(request);
        }
    }

    /// Sends the editor's text: a steer while a run goes, else a queue that
    /// starts one.
    fn send(&mut self, mode: Option<Mode>) {
        self.dirty = true;
        if self.armed {
            self.armed = false;
            self.status_dirty = true;
        }
        if self.editor.is_empty() {
            return;
        }
        let text = self.editor.take();
        let mode = mode.unwrap_or(if self.running {
            Mode::Steer
        } else {
            Mode::Queue
        });
        // While a run goes, the row above the box holds the message until the
        // worker echoes it; an idle send starts the run and needs no row.
        if self.running {
            self.steer_row(&text, mode == Mode::Queue);
        }
        self.commands.push_back(Command::Send { text, mode });
        self.running = true;
        self.status_dirty = true;
        self.editor_dirty = true;
    }

    /// Adds the row that holds the message the run takes next: a steer that
    /// lands after the tool now running, or a queued turn.
    fn steer_row(&mut self, text: &str, queued: bool) {
        let text: Arc<str> = Arc::from(shown_user_text(text).as_ref());
        self.live.retain(|item| !matches!(item, Item::Steer { .. }));
        self.live.push(Item::Steer { text, queued });
    }

    /// Sends the answer one of the box's keys names.
    fn answer(&mut self, key: char) {
        match key {
            'y' => self.approve(AnswerKind::Once),
            'a' if self
                .approval
                .as_ref()
                .is_some_and(|open| !open.saves.is_empty()) =>
            {
                self.approve(AnswerKind::Save);
            }
            // Enter asks for the note that goes with a refusal: the card opens
            // its text area and the keys type into it.
            'n' => {
                if let Some(open) = self.approval.as_mut()
                    && !open.refuses()
                {
                    open.reason = Some(String::new());
                }
            }
            _ => {}
        }
    }

    /// One key of the question card, which holds every key while it is open.
    fn question_key(&mut self, action: Action) {
        self.dirty = true;
        let typing = self.question.as_ref().is_some_and(QuestionBox::typing);
        match action {
            Action::Edit(key) if typing => self.field_key(key),
            Action::Edit(EditKey::Up) => self.step_question(-1),
            Action::Edit(EditKey::Down) => self.step_question(1),
            Action::Edit(EditKey::Insert(c)) => self.question_char(c),
            Action::NewLine if typing => {
                if let Some(field) = self.question.as_mut().and_then(QuestionBox::field_mut) {
                    field.new_line();
                }
            }
            // Enter answers the question on screen, and the last one sends.
            Action::Send => self.answer_question(),
            // Tab shows the next question.
            Action::Queue => self.next_question(),
            Action::Cancel => self.close_question(),
            _ => {}
        }
    }

    /// One key of the answer field, read as the text box reads it: Up and Down
    /// past its first or its last row move the pick instead.
    fn field_key(&mut self, key: EditKey) {
        let width = self.field_columns();
        let Some(field) = self.question.as_mut().and_then(QuestionBox::field_mut) else {
            return;
        };
        let moved = edit_text(field, key, width);
        if !matches!(key, EditKey::Digit(_) | EditKey::NegativeArgument) {
            field.clear_argument();
        }
        if !moved {
            match key {
                EditKey::Up => self.step_question(-1),
                EditKey::Down => self.step_question(1),
                _ => {}
            }
        }
    }

    /// The columns the answer field wraps its text at.
    pub(crate) fn field_columns(&self) -> u16 {
        paint::field_columns(self.text_width())
    }

    /// One typed character of a question whose field is not picked: a digit
    /// picks that option, a space checks an option of a question that takes
    /// several, and any other character starts an answer of its own.
    fn question_char(&mut self, c: char) {
        let Some(open) = self.question.as_mut() else {
            return;
        };
        if c == ' ' && open.current().is_some_and(|(ask, _)| ask.multi) {
            open.toggle();
            return;
        }
        if let Some(digit) = c.to_digit(10).filter(|digit| (1..=9).contains(digit)) {
            open.pick(digit as usize - 1);
            return;
        }
        if c != ' ' {
            open.start_typing(c);
        }
    }

    /// Moves the pick of the question on screen by `step` rows.
    fn step_question(&mut self, step: isize) {
        let Some(open) = self.question.as_mut() else {
            return;
        };
        open.step(step);
    }

    /// Shows the question after the one on screen, and the first after the last.
    fn next_question(&mut self) {
        let Some(open) = self.question.as_mut() else {
            return;
        };
        if open.asks.len() > 1 {
            open.at = (open.at + 1) % open.asks.len();
            self.status_dirty = true;
        }
    }

    /// Enter on the card: an empty answer field takes nothing, any question but
    /// the last moves on, and the last sends every answer.
    fn answer_question(&mut self) {
        let Some(open) = self.question.as_mut() else {
            return;
        };
        if open.typing() && !open.has_text() {
            return;
        }
        if open.at + 1 < open.asks.len() {
            open.at += 1;
            self.status_dirty = true;
            return;
        }
        let Some(open) = self.question.take() else {
            return;
        };
        let answers = open.answers();
        self.question_answered(&open, &answers);
        self.commands.push_back(Command::Answer {
            id: open.id,
            answers: Some(answers),
        });
        self.next_card();
        self.set_sessions();
        self.status_dirty = true;
    }

    /// Esc on the card: a field with text loses its text, and anything else
    /// skips the whole request.
    fn close_question(&mut self) {
        let Some(open) = self.question.as_mut() else {
            return;
        };
        if open.typing() && open.has_text() {
            open.clear_field();
            return;
        }
        let Some(open) = self.question.take() else {
            return;
        };
        self.question_skipped(&open);
        self.commands.push_back(Command::Answer {
            id: open.id,
            answers: None,
        });
        self.next_card();
        self.set_sessions();
        self.status_dirty = true;
    }

    /// Answers the open request and shows the next one.
    fn approve(&mut self, answer: AnswerKind) {
        let Some(open) = self.approval.take() else {
            return;
        };
        self.commands.push_back(Command::Approve {
            id: open.id,
            answer,
            reason: None,
        });
        self.next_approval();
    }

    /// Refuses the open request, with the reason when the user typed one.
    fn refuse(&mut self, with_reason: bool) {
        let Some(open) = self.approval.take() else {
            return;
        };
        let reason = match (with_reason, open.reason.as_ref()) {
            (true, Some(text)) if !text.is_empty() => Some(text.clone()),
            _ => None,
        };
        self.commands.push_back(Command::Approve {
            id: open.id,
            answer: AnswerKind::Refuse,
            reason,
        });
        self.next_approval();
    }

    /// One key of the reason field.
    fn reason_key(&mut self, key: EditKey) {
        let Some(open) = self.approval.as_mut() else {
            return;
        };
        let Some(reason) = open.reason.as_mut() else {
            return;
        };
        match key {
            EditKey::Insert(c) => reason.push(c),
            EditKey::Backspace => {
                reason.pop();
            }
            _ => {}
        }
    }

    /// Goes back from the note to the row of choices: Esc in the note undoes the
    /// Tell answer, so the card waits for another one instead of refusing it.
    fn close_note(&mut self) {
        let Some(open) = self.approval.as_mut() else {
            return;
        };
        if open.reason.take().is_some() {
            open.shown = std::time::Instant::now();
            self.keys.card_mut().pick_to(Choice::Tell);
        }
    }

    /// One key of the editor. The argument the digit keys typed belongs to one
    /// command: a key that did not read it drops it, so Alt+3 Home and then a
    /// typed x gives "x" and not "xxx".
    fn edit(&mut self, key: EditKey) {
        self.edit_key(key);
        if !matches!(key, EditKey::Digit(_) | EditKey::NegativeArgument) {
            self.editor.clear_argument();
        }
    }

    /// Runs one key of the editor, which may read the argument it was given.
    fn edit_key(&mut self, key: EditKey) {
        let width = self.editor_columns();
        if self.armed {
            // The hint goes away with the next key, so the status line is
            // rebuilt for it.
            self.armed = false;
            self.status_dirty = true;
        }
        self.dirty = true;
        // `?` in an empty box, with no list open, is the key that opens the key
        // help: it is not typed.
        if key == EditKey::Insert('?')
            && self.editor.is_empty()
            && self.list.is_none()
            && self.sheet.is_none()
        {
            self.open_help();
            return;
        }
        // The palette filters as it is typed: its query takes the keys, and the
        // draft in the box stays as it was.
        if self.list.as_ref().is_some_and(List::is_palette) {
            let hit = match key {
                EditKey::Insert(c) => {
                    if let Some(list) = self.list.as_mut() {
                        list.push(c);
                    }
                    true
                }
                EditKey::Backspace => {
                    if let Some(list) = self.list.as_mut() {
                        list.pop();
                    }
                    true
                }
                _ => false,
            };
            if hit {
                return;
            }
        }
        match key {
            EditKey::PasteImage => self.paste_image(),
            EditKey::SearchBackward => self.start_search(false),
            EditKey::SearchForward => self.start_search(true),
            EditKey::HistoryOldest => {
                self.editor.history_oldest(&self.history);
            }
            EditKey::HistoryNewest => {
                self.editor.history_newest();
            }
            EditKey::YankLastArg => self.editor.yank_last_arg(&self.history),
            EditKey::Up => {
                if !self.editor.up(width) {
                    let (editor, history) = (&mut self.editor, &self.history);
                    editor.history_up(history);
                }
            }
            EditKey::Down => {
                if !self.editor.down(width) {
                    let (editor, history) = (&mut self.editor, &self.history);
                    editor.history_down(history);
                }
            }
            _ => {
                edit_text(&mut self.editor, key, width);
            }
        }
        self.editor_dirty = true;
        self.refresh_list();
    }

    /// Ctrl+R and Ctrl+S: open the search over the history, with the lists above
    /// the box closed.
    fn start_search(&mut self, forward: bool) {
        self.list = None;
        let (editor, history) = (&mut self.editor, &self.history);
        editor.search_history(forward, history);
    }

    /// Opens, keeps or closes the list above the text box from the text box and
    /// the word at the cursor: a line that starts with `/` opens the commands,
    /// a word that starts with `@` opens the files, and anything else closes a
    /// list that is not the palette.
    fn refresh_list(&mut self) {
        if self.list.as_ref().is_some_and(List::is_palette) {
            return;
        }
        // A search shows a message of the session, which is no query of a list.
        if self.editor.searching() {
            return;
        }
        let (before, after) = self.editor.chunks();
        let word = at_word(before);
        let command =
            after.is_empty() && before.starts_with('/') && !before.contains([' ', '\n', '\t']);
        self.query.clear();
        if let Some(word) = word {
            self.query.push_str(word);
            self.open_files();
        } else if command {
            self.query.push_str(before);
            self.open_commands();
        } else {
            self.list = None;
        }
    }

    /// Opens the `/` list, or keeps it, with the query of this key.
    fn open_commands(&mut self) {
        if !matches!(
            self.list.as_ref().map(List::source),
            Some(Source::Commands(_))
        ) {
            let mut list = List::commands(Arc::clone(&self.slash), false);
            list.set_labels(self.labels());
            self.list = Some(list);
        }
        if let Some(list) = self.list.as_mut() {
            list.set_query(&self.query);
        }
        if self.list.as_ref().is_some_and(|list| list.hits() == 0) {
            self.list = None;
        }
    }

    /// Opens the file list, or keeps it, with the query of this key; the first
    /// open asks the host for the paths.
    fn open_files(&mut self) {
        if self.files.is_none() {
            let Some(host) = self.host.as_ref() else {
                self.list = None;
                return;
            };
            if self.files_job.0.is_none() {
                self.files_job.0 = Some(host.get().files());
            }
        }
        if !matches!(self.list.as_ref().map(List::source), Some(Source::Files(_))) {
            let files = self.files.clone().unwrap_or_default();
            let mut list = List::files(files);
            list.set_labels(self.labels());
            self.list = Some(list);
        }
        if let Some(list) = self.list.as_mut() {
            list.set_query(&self.query);
        }
        if self.list.as_ref().is_some_and(|list| list.hits() == 0) && !self.files_pending() {
            self.list = None;
        }
    }

    /// Whether the host is still reading the paths.
    fn files_pending(&self) -> bool {
        self.files.is_none() && self.files_job.0.is_some()
    }

    /// Fills the file list with the paths the host answered: the list that
    /// waited for them was built empty, so it is built again from the paths.
    fn files_arrived(&mut self) {
        if !matches!(self.list.as_ref().map(List::source), Some(Source::Files(_))) {
            return;
        }
        self.list = None;
        self.refresh_list();
        self.dirty = true;
    }

    /// Opens the palette, or closes it when it is open; the draft stays.
    fn toggle_palette(&mut self) {
        if self.list.as_ref().is_some_and(List::is_palette) {
            self.list = None;
        } else {
            let mut list = List::commands(Arc::clone(&self.slash), true);
            list.set_labels(self.labels());
            self.list = Some(list);
        }
        self.dirty = true;
    }

    /// Takes the pick of the open sheet: a model goes to the worker as the next
    /// turn's, its prices go to the Context panel's cost, a session is opened, a
    /// provider's login starts and the key help picks nothing.
    fn pick_sheet(&mut self) {
        // What the pick is, read off the sheet before any of it changes.
        enum Pick {
            Model(Arc<str>),
            Level(Arc<str>),
            Session(Arc<str>),
            Login(&'static str),
            None,
        }
        let pick = match self.sheet.as_ref() {
            None => Pick::None,
            Some(open) => match &open.kind {
                SheetKind::Model(_) => model::picked(&open.sheet).map_or(Pick::None, Pick::Model),
                SheetKind::Level => level::picked(&open.sheet).map_or(Pick::None, Pick::Level),
                SheetKind::Sessions(state) => {
                    sessions::picked(&open.sheet, &state.rows).map_or(Pick::None, Pick::Session)
                }
                SheetKind::Login(login) => {
                    login.picked(&open.sheet).map_or(Pick::None, Pick::Login)
                }
                SheetKind::Help => Pick::None,
            },
        };
        match pick {
            Pick::Model(id) => {
                // The list stays for the next opening: the pick only names one
                // of its rows.
                if let Some(open) = self.sheet.take()
                    && let SheetKind::Model(models) = open.kind
                {
                    if let Some(prices) = prices_of(&models, &id) {
                        self.set_prices(prices);
                    }
                    self.models = Some(models);
                }
                self.commands.push_back(Command::Set {
                    model: Some(id.to_string()),
                    level: None,
                    mode: None,
                });
                self.note(format!("model: {id} from the next turn"));
            }
            Pick::Level(word) => {
                self.sheet = None;
                self.commands.push_back(Command::Set {
                    model: None,
                    level: Some(word.to_string()),
                    mode: None,
                });
                self.note(format!("level: {word} from the next turn"));
            }
            Pick::Session(id) => {
                if id.as_ref() == self.session {
                    self.sheet = None;
                    return;
                }
                self.request_open(Target::Session(id.to_string()));
            }
            Pick::Login(provider) => self.start_login(provider),
            Pick::None => {}
        }
        self.dirty = true;
    }

    /// The model the Context panel names, for the sheet's `now` mark.
    fn current_model(&mut self) -> Option<String> {
        let name = self
            .sidebar
            .get_mut::<ContextPanel>()?
            .model_name()
            .as_str()
            .to_owned();
        Some(name)
    }

    /// `/level <word>`: a word no model can make known waits for the list when
    /// this app has not read one, because the plan of the model may name it.
    fn level_word(&mut self, word: &str) {
        if !is_level_word(word)
            && self.models.is_none()
            && let Some(host) = self.host.as_ref()
        {
            if self.waits.models.is_none() {
                self.waits.models = Some(host.get().models());
            }
            self.waits.after_models = AfterModels::LevelWord(word.to_owned());
            self.dirty = true;
            return;
        }
        self.name_level(word);
    }

    /// Sends `word` when it is a named level, an off word, or a word the plan of
    /// the current model lists without a place on the scale, and otherwise
    /// names the words the command takes.
    fn name_level(&mut self, word: &str) {
        let current = self.current_model();
        let listed = plan_of(self.models.as_ref(), current.as_deref())
            .is_some_and(|plan| plan.takes_unknown(word));
        if is_level_word(word) || listed {
            self.commands.push_back(Command::Set {
                model: None,
                level: Some(word.to_owned()),
                mode: None,
            });
        } else {
            self.note("usage: /level low|medium|high|off".to_owned());
        }
        self.dirty = true;
    }

    /// Opens the model sheet, asking the host for the list when this app has
    /// not read one yet.
    fn open_models(&mut self) {
        self.waits.after_models = AfterModels::ModelSheet;
        if self.models.is_some() {
            self.show_models();
            return;
        }
        let Some(host) = self.host.as_ref() else {
            self.note("no model list without the binary's host".to_owned());
            return;
        };
        if self.waits.models.is_none() {
            self.waits.models = Some(host.get().models());
        }
        self.dirty = true;
    }

    /// The model list arrived: the sheet that waited for it opens on it.
    fn models_arrived(&mut self, models: Models) {
        self.models = Some(models);
        match std::mem::take(&mut self.waits.after_models) {
            AfterModels::ModelSheet => self.show_models(),
            AfterModels::LevelSheet => self.show_levels(),
            AfterModels::LevelWord(word) => self.name_level(&word),
        }
    }

    /// Opens the level sheet, asking the host for the model list when this app
    /// has not read one yet, so the rows are the words of the current model.
    fn open_levels(&mut self) {
        if self.models.is_some() || self.host.is_none() {
            self.show_levels();
            return;
        }
        let Some(host) = self.host.as_ref() else {
            return;
        };
        if self.waits.models.is_none() {
            self.waits.models = Some(host.get().models());
        }
        self.waits.after_models = AfterModels::LevelSheet;
        self.dirty = true;
    }

    /// Opens the level sheet over the plan of the model the app holds.
    fn show_levels(&mut self) {
        let current = self.current_model();
        let sheet = {
            let plan = plan_of(self.models.as_ref(), current.as_deref());
            level::level_sheet(plan, self.level.as_deref())
        };
        self.show_sheet(Open {
            kind: SheetKind::Level,
            sheet,
        });
        self.dirty = true;
    }

    /// Opens the model sheet over the list the app holds.
    fn show_models(&mut self) {
        let Some(models) = self.models.take() else {
            return;
        };
        let current = self.current_model();
        let sheet = model::model_sheet(&models, current.as_deref());
        self.show_sheet(Open {
            kind: SheetKind::Model(models),
            sheet,
        });
        self.dirty = true;
    }

    /// Opens the sessions sheet over the sessions of the folder.
    fn open_sessions(&mut self) {
        let sheet = sessions::sessions_sheet(&self.sessions, now_seconds());
        // The rows the sheet shows, kept so a delete names a row of the disk
        // however the folder's list changes under it.
        let state = Sessions {
            rows: self.sessions.clone(),
            armed: None,
            reads: HashMap::new(),
        };
        self.show_sheet(Open {
            kind: SheetKind::Sessions(state),
            sheet,
        });
        self.sheet_preview();
        self.dirty = true;
    }

    /// Keeps the sessions sheet's preview on the row it picked, computed only
    /// where the draw shows it: the lines and the message rows at once, from
    /// the messages the sheet already read, and a background read when the row
    /// has none and no other row's read is in flight.
    fn sheet_preview(&mut self) {
        let width = self.text_width();
        if width < PREVIEW_COLUMNS {
            return;
        }
        let pane = pane_width(width.min(self.screen.columns()));
        let theme = self.look.theme;
        let Some(open) = self.sheet.as_mut() else {
            return;
        };
        let Open { sheet, kind } = open;
        let SheetKind::Sessions(state) = kind else {
            return;
        };
        let Some((index, _)) = sheet.selected() else {
            return;
        };
        let Some(row) = state.rows.get(index) else {
            return;
        };
        let id = Arc::clone(&row.id);
        // The answer to the read in flight draws the row on screen and starts
        // the next read then, so a key that moves the selection waits for
        // nothing and reads only the row the user stops on. The read is a job
        // beside the loop, so it needs the loop's runtime; an app built without
        // one, as a test builds it, starts no job.
        if !state.reads.contains_key(&id)
            && self.preview_job.is_none()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let reading = Arc::clone(&id);
            let job = runtime.spawn(async move {
                let Some(dir) = session_folder(&reading).await else {
                    return Ok(RecentMessages::default());
                };
                history::recent_messages(dir).await
            });
            self.preview_job = Some((Arc::clone(&id), job));
        }
        let lines = sessions::preview(row, now_seconds());
        let rows = preview_rows(state, &id, pane, &theme);
        sheet.set_preview(lines);
        sheet.set_preview_rows(rows);
    }

    /// The answer to the sessions sheet's read of a row's messages: cached for
    /// the row, then the preview drawn again, which builds its rows and starts
    /// the read of the row on screen when it has none. A sheet that closed
    /// meanwhile drops it.
    fn exchange_arrived(&mut self, id: Arc<str>, answer: io::Result<RecentMessages>) {
        let Some(open) = self.sheet.as_mut() else {
            return;
        };
        let SheetKind::Sessions(state) = &mut open.kind else {
            return;
        };
        // A folder that is gone, or a journal that does not read, is no
        // message: the row draws alone and is not read again.
        let messages = answer.unwrap_or_default();
        state.reads.insert(
            id,
            Preview {
                messages,
                built: None,
            },
        );
        self.sheet_preview();
        self.dirty = true;
    }

    /// Drops the preview rows built in the old colors and draws the row on
    /// screen in the new ones.
    fn preview_restyled(&mut self) {
        if let Some(Open {
            kind: SheetKind::Sessions(state),
            ..
        }) = self.sheet.as_mut()
        {
            for read in state.reads.values_mut() {
                read.built = None;
            }
        }
        self.sheet_preview();
    }

    /// Opens the login sheet over the providers the host can log in to.
    fn open_login(&mut self) {
        let Some(host) = self.host.as_ref() else {
            self.note("no logins without the binary's host".to_owned());
            return;
        };
        let login = Login::new(host.get().logins());
        let sheet = login.sheet();
        self.show_sheet(Open {
            kind: SheetKind::Login(login),
            sheet,
        });
        self.dirty = true;
    }

    /// Puts `open` in place of the text box; it takes the keys a focused
    /// sidebar had.
    fn show_sheet(&mut self, mut open: Open) {
        self.sidebar.set_active(false);
        open.name_keys(self.keyfile.as_ref());
        self.sheet = Some(open);
    }

    /// Opens the key help sheet.
    fn open_help(&mut self) {
        self.show_sheet(Open {
            kind: SheetKind::Help,
            sheet: sheet::help::help_sheet(self.labels()),
        });
        self.dirty = true;
    }

    /// Starts a login for `provider`: the host runs it and its steps rebuild the
    /// sheet.
    fn start_login(&mut self, provider: &'static str) {
        let Some(open) = self.sheet.as_mut() else {
            return;
        };
        let SheetKind::Login(login) = &mut open.kind else {
            return;
        };
        login.start(provider);
        open.sheet = login.sheet();
        open.name_keys(self.keyfile.as_ref());
        self.waits.login = self.host.as_ref().map(|host| host.get().login(provider));
    }

    /// Takes one step of the login that runs, and rebuilds the sheet for it. A
    /// login that ended drops the model list, so the provider it added shows in
    /// the next one.
    fn login_step(&mut self, step: Option<LoginStep>) {
        let Some(step) = step else {
            self.waits.login = None;
            return;
        };
        let Some(open) = self.sheet.as_mut() else {
            self.waits.login = None;
            return;
        };
        let SheetKind::Login(login) = &mut open.kind else {
            self.waits.login = None;
            return;
        };
        login.step(&step);
        open.sheet = login.sheet();
        if login.finished().is_some() {
            self.models = None;
            self.waits.login = None;
        }
        open.name_keys(self.keyfile.as_ref());
        self.dirty = true;
    }

    /// Asks the host for the worker `target` names.
    fn request_open(&mut self, target: Target) {
        let Some(host) = self.host.as_ref() else {
            self.note("no worker without the binary's host".to_owned());
            return;
        };
        // The session may be the one that is going away.
        if self.waits.delete.is_some() {
            self.note("wait: a session is being deleted".to_owned());
            self.dirty = true;
            return;
        }
        if self.waits.open.is_none() {
            self.waits.open = Some(host.get().open(target));
        } else {
            self.note("a worker is already starting".to_owned());
        }
        self.dirty = true;
    }

    /// Adds the one dim row a compaction draws, and notes the row so the read of
    /// its checkpoint text starts from the record the worker is at now.
    fn compacted(&mut self, before: u64, after: u64, replaced: u32) {
        self.freeze_text();
        let fold = self.folds.add();
        self.pending.push(Item::Compacted(Box::new(Compacted {
            tokens: Some((before, after)),
            messages: replaced,
            fold,
            text: None,
        })));
        self.checkpoints.push((fold, self.upto));
        self.dirty = true;
    }

    /// Asks the session to compact its history into a checkpoint, once no turn
    /// runs: a compaction mid-turn would replace the messages the reply it waits
    /// for answers.
    fn compact(&mut self, note: Option<&str>) {
        if self.running {
            self.note("compact: wait for the turn to end".to_owned());
            return;
        }
        let note = note.filter(|note| !note.is_empty()).map(str::to_owned);
        self.commands.push_back(Command::Compact { note });
    }

    /// Notes the compaction row the user opened, whose checkpoint text is not on
    /// screen yet: a frame notes it, and the read runs on the next turn of the
    /// loop, never on every frame.
    fn wants_checkpoint(&mut self) {
        if self.wanted.is_some() {
            return;
        }
        self.wanted = self
            .checkpoints
            .iter()
            .find(|(fold, _)| self.folds.is_open(*fold))
            .copied();
    }

    /// Reads the checkpoint text of the compaction row `fold` from the base that
    /// recorded it, since the base commits after the event the row came with.
    async fn read_checkpoint(&mut self, fold: u64, from: u64) {
        let Some(dir) = self.journal.clone() else {
            return;
        };
        let upto = self.upto;
        let Ok(found) = history::checkpoint(dir, from, upto).await else {
            return;
        };
        let Some(text) = found else {
            // The base is not on disk yet: the row reads it when it opens again.
            return;
        };
        self.checkpoints.retain(|(row, _)| *row != fold);
        self.fill_checkpoint(fold, text);
        self.dirty = true;
    }

    /// Forks this session, once no run is going.
    ///
    /// A fork mid-turn would copy the calls the parent has not answered yet,
    /// and the fork's worker would run them a second time.
    fn fork(&mut self) {
        if self.running {
            self.note("fork: wait for the turn to end".to_owned());
            return;
        }
        self.request_open(Target::Fork(self.session.clone()));
    }

    /// The worker `request_open` asked for answered: the loop ends and the run
    /// connects to it. An error is one line, and the session and any sheet stay.
    fn opened(&mut self, answer: Result<Opened, String>) {
        match answer {
            Ok(opened) => {
                self.sheet = None;
                self.switch = Some(opened);
            }
            Err(why) => self.note(format!("open: {why}")),
        }
        self.dirty = true;
    }

    /// Takes the commands the JS plugins of the session offer: each one joins
    /// the `/` list and the palette after the built-in rows, under the plugin
    /// group and with no key of its own.
    ///
    /// A name a built-in already has is skipped, since a built-in keeps its
    /// meaning, and the note says which one. A second frame, from a reattach,
    /// replaces the plugin rows rather than adding them again.
    fn plugin_commands(&mut self, commands: &[PluginCommand<'_>]) {
        let labels = self.keyfile.as_ref().map(keyfile::KeyFile::labels);
        let mut rows: Vec<list::Command> = commands::table(labels).to_vec();
        let builtins = rows.len();
        for command in commands {
            let name = format!("/{}", command.name);
            if rows[..builtins].iter().any(|row| row.name == name) {
                let note =
                    format!("plugin command {name} skipped: a built-in command has that name");
                self.note(note);
                continue;
            }
            rows.push(list::Command {
                name: Cow::Owned(name),
                words: Cow::Owned(command.description.to_string()),
                key: Cow::Borrowed(""),
                group: commands::PLUGIN_GROUP,
            });
        }
        self.slash = Arc::from(rows);
        self.dirty = true;
    }

    /// Takes the picked row of the open list: a file inserts its path, a
    /// command puts its name in the box.
    fn complete(&mut self) {
        if let Some(path) = self.list.as_ref().and_then(List::path) {
            let path = Arc::clone(path);
            self.complete_file(&path);
            return;
        }
        let Some(name) = self
            .list
            .as_ref()
            .and_then(List::command)
            .map(|command| command.name.clone())
        else {
            return;
        };
        self.list = None;
        self.editor.clear();
        self.editor.insert(&name);
        self.editor.insert(" ");
        self.editor_dirty = true;
        self.dirty = true;
    }

    /// Takes the picked row: a file does what Tab does, a command runs.
    fn pick(&mut self) {
        if let Some(path) = self.list.as_ref().and_then(List::path) {
            let path = Arc::clone(path);
            self.complete_file(&path);
            return;
        }
        let index = self.list.as_ref().and_then(List::command).and_then(|pick| {
            self.slash
                .iter()
                .position(|command| command.name == pick.name)
        });
        if let Some(index) = index {
            self.run_command(index, None);
        }
    }

    /// Replaces the word after the `@` at the cursor with `path` and one space.
    fn complete_file(&mut self, path: &str) {
        let len = {
            let (before, _) = self.editor.chunks();
            let start = before
                .rfind(|c: char| c.is_whitespace())
                .map_or(0, |at| at + 1);
            if before[start..].strip_prefix('@').is_none() {
                return;
            }
            before[start + 1..].chars().count()
        };
        for _ in 0..len {
            self.editor.backspace();
        }
        self.editor.insert(path);
        self.editor.insert(" ");
        self.list = None;
        self.editor_dirty = true;
        self.dirty = true;
    }

    /// Runs the command line the box holds whole, and answers whether it did;
    /// a line that names no command is sent as a message.
    fn run_line(&mut self) -> bool {
        let (index, plugin) = {
            let (before, after) = self.editor.chunks();
            if !after.is_empty() {
                return false;
            }
            let mut parts = before.split_ascii_whitespace();
            let Some(name) = parts.next() else {
                return false;
            };
            let Some(index) = self.slash.iter().position(|command| command.name == name) else {
                return false;
            };
            let plugin = self.slash[index].group == commands::PLUGIN_GROUP;
            self.query.clear();
            if plugin {
                // A plugin command takes the rest of the line as it was typed,
                // after the name and one space.
                let at = before.find(name).unwrap_or(0) + name.len();
                let rest = &before[at..];
                self.query.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            } else if name == "/compact" {
                // The note of a compaction is the rest of the line, as it was typed.
                let at = before.find(name).unwrap_or(0) + name.len();
                self.query.push_str(before[at..].trim());
            } else if let Some(arg) = parts.next() {
                if parts.next().is_some() {
                    return false;
                }
                self.query.push_str(arg);
            }
            (index, plugin)
        };
        let arg = std::mem::take(&mut self.query);
        let text = if plugin || !arg.is_empty() {
            Some(arg.as_str())
        } else {
            None
        };
        self.run_command(index, text);
        self.query = arg;
        true
    }

    /// Runs the command at `index` of the table, with `arg` from the line.
    fn run_command(&mut self, index: usize, arg: Option<&str>) {
        self.list = None;
        self.editor.clear();
        self.editor_dirty = true;
        self.dirty = true;
        let Some(command) = self.slash.get(index) else {
            return;
        };
        let name = command.name.clone();
        if command.group == commands::PLUGIN_GROUP {
            // The rest of the line is the plugin command's own argument.
            let name = name.strip_prefix('/').unwrap_or(&name).to_owned();
            let args = arg.unwrap_or_default().to_owned();
            self.commands.push_back(Command::Plugin { name, args });
            return;
        }
        match &*name {
            "/quit" => self.quit(),
            "/level" => match arg {
                None => self.open_levels(),
                Some(word) => self.level_word(word),
            },
            "/mode" => match arg {
                Some(mode) if MODES.contains(&mode) => {
                    self.commands.push_back(Command::Set {
                        model: None,
                        level: None,
                        mode: Some(mode.to_owned()),
                    });
                }
                _ => self.note("usage: /mode default|accept-edits|bypass|auto".to_owned()),
            },
            "/cost" => {
                let line = format!(
                    "{} tokens · ${}.{:02}",
                    self.tokens,
                    self.cents / 100,
                    self.cents % 100
                );
                self.note(line);
            }
            "/model" => self.open_models(),
            "/sessions" => self.open_sessions(),
            "/new" => self.request_open(Target::New),
            "/fork" => self.fork(),
            "/compact" => self.compact(arg),
            "/login" => self.open_login(),
            "/help" => self.open_help(),
            _ => self.note(format!("unknown command {name}")),
        }
    }

    /// Leaves the session.
    fn quit(&mut self) {
        self.commands.push_back(Command::Detach);
        self.exit = true;
    }

    /// Sends what the last key asked for.
    async fn flush(&mut self, client: &mut Client) {
        while let Some(command) = self.commands.pop_front() {
            let sent = match command {
                Command::Send { text, mode } => client.send(&text, mode).await,
                Command::Cancel => client.cancel().await,
                Command::Approve { id, answer, reason } => {
                    client.approve(id, answer, reason.as_deref()).await
                }
                Command::Answer { id, answers } => client.answer(id, answers.as_deref()).await,
                Command::Set { model, level, mode } => {
                    client
                        .set(model.as_deref(), level.as_deref(), mode.as_deref())
                        .await
                }
                Command::Detach => client.detach().await,
                Command::Compact { note } => client.compact(note.as_deref()).await,
                Command::Plugin { name, args } => client.command(&name, &args).await,
            };
            if sent.is_err() {
                self.exit = true;
                self.commands.clear();
                return;
            }
        }
    }

    /// Whether a frame is due now.
    fn needs_draw(&self) -> bool {
        self.dirty && self.deadline.is_none()
    }

    /// Draws one frame and writes it as one write.
    ///
    /// The live area follows its content, so a frame may scroll the transcript
    /// and print finished lines as well; both go into the same write. A frame
    /// that changed nothing is not written at all.
    async fn draw(&mut self, terminal: &mut impl Terminal) -> io::Result<()> {
        self.dirty = false;
        self.deadline = None;
        let want = self.content_rows();
        self.screen.fit(want);
        self.commit();
        self.paint();
        // The fills the regions sit on go under their text, after the regions
        // drew it: the text box's rows take the input fill.
        self.paint_input_fill();
        // A box or a panel over the transcript covers the images on it.
        self.screen.block_pictures(self.covers_transcript());
        self.screen.diff(&mut self.frame);
        // The copy of a release goes out in the same write as the frame that
        // drops its highlight.
        if !self.clipboard.is_empty() {
            self.frame.append(&mut self.clipboard);
        }
        self.arm_tick();
        self.arm_pictures();
        if self.frame.len() == SYNC_PAIR {
            return Ok(());
        }
        terminal.write_frame(&self.frame).await
    }

    /// Whether a box or a panel covers the rows of the transcript this frame:
    /// an image under one of them would show through it.
    fn covers_transcript(&self) -> bool {
        self.approval.is_some() || self.sheet.is_some() || self.list.is_some()
    }

    /// Arms the wait before the images of the pictures go out, when one of them
    /// waits.
    ///
    /// Every frame that still wants an image pushes the wait out again, so a
    /// scroll draws the half-block cells until it stops.
    fn arm_pictures(&mut self) {
        if self.screen.pictures_due() {
            self.arm_settle(picture_native::MOVE_SETTLE);
        } else if let Some(at) = self.picture_deadline
            && self.deadline.is_none_or(|deadline| deadline > at)
        {
            // A settle a resize or a focus-in armed survives the frame.
            self.deadline = Some(at);
        }
    }

    /// Arms the settle wait for a terminal that draws images.
    fn arm_settle(&mut self, wait: Duration) {
        let due = tokio::time::Instant::now() + wait;
        self.picture_deadline = Some(due);
        if self.deadline.is_none_or(|deadline| deadline > due) {
            self.deadline = Some(due);
        }
    }

    /// The terminal reported a focus-in: it drew its window again, so every
    /// image on the screen is gone and goes out once the wait is over.
    fn focused(&mut self) {
        self.screen.pictures_lost();
        if self.native.is_some() {
            self.arm_settle(picture_native::SETTLE);
        } else {
            self.arm_pictures();
        }
    }

    /// Draws the images of the pictures that waited out their settle time and
    /// writes them.
    async fn settle_pictures(&mut self, terminal: &mut impl Terminal) -> io::Result<()> {
        self.picture_deadline = None;
        if !self.screen.pictures_due() {
            return Ok(());
        }
        let mut images = std::mem::take(&mut self.images);
        self.screen.draw_pictures(&mut images);
        let written = if images.is_empty() {
            Ok(())
        } else {
            terminal.write_frame(&images).await
        };
        self.images = images;
        written
    }

    /// The width the box, the cards, the lists and the status line lay out in:
    /// the columns the sidebar leaves in the main area while it is docked, and
    /// the whole terminal's while it is off or an overlay, since an overlay
    /// covers text for as long as it is open and gives the columns back after.
    ///
    /// Inline mode has no main area of its own: it draws on the normal screen,
    /// so it uses the terminal's columns. The rows of the transcript come in
    /// from this width by the margin at each side; see [`App::transcript_width`].
    fn text_width(&self) -> u16 {
        if self.screen.is_full() {
            self.main.width.max(1)
        } else {
            self.screen.columns()
        }
    }

    /// The columns the rows of the transcript wrap at: the main area without the
    /// margin at each side of a full-screen frame, and the whole terminal's
    /// inline, which keeps no margin.
    fn transcript_width(&self) -> u16 {
        self.text_width()
            .saturating_sub(self.margin().saturating_mul(2))
            .max(1)
    }

    /// The blank columns a row of the transcript keeps at each side of the main
    /// area: [`MARGIN`] in full screen, and none inline. A narrow main area
    /// keeps a margin only where its rows have columns to spare.
    fn margin(&self) -> u16 {
        if self.screen.is_full() {
            MARGIN.min(self.text_width() / (MARGIN * 2))
        } else {
            0
        }
    }

    /// The columns a picture fits into: the transcript's width without the two
    /// columns its rows are indented by. Before the first layout has set the
    /// main area, the screen's own width stands in for it, so the pictures of
    /// the replay fit the terminal the app was started on.
    fn room(&self) -> crate::picture::Room {
        let width = if self.main.width == 0 {
            self.screen.columns().saturating_sub(4)
        } else {
            self.transcript_width()
        };
        crate::picture::Room {
            columns: u32::from(width.saturating_sub(2)).max(1),
            cell: self.cell.unwrap_or(crate::picture::CellShape::HALF_BLOCK),
        }
    }

    /// The interned style of every cell of `picture`, its columns and its rows:
    /// nothing when its cells are missing, when the terminal draws sixteen
    /// colors, when the theme draws none, or when the transcript has no color
    /// pairs left for pictures. A picture with no cells draws only its chip.
    fn picture_cells(&mut self, picture: &Picture) -> (Vec<u32>, u16, u16) {
        let Some(cells) = &picture.cells else {
            return (Vec::new(), 0, 0);
        };
        if self.look.depth == Depth::Ansi16 || self.look.mode() == crate::theme::Mode::NoColor {
            return (Vec::new(), 0, 0);
        }
        if !self.budget.take(cells) {
            return (Vec::new(), 0, 0);
        }
        let columns = u16::try_from(cells.columns).unwrap_or(u16::MAX);
        let rows = u16::try_from(cells.rows).unwrap_or(u16::MAX);
        let mut ids = Vec::with_capacity(usize::from(columns) * usize::from(rows));
        for row in 0..cells.rows {
            for column in 0..cells.columns {
                let Some((top, low)) = cells.get(column, row) else {
                    break;
                };
                ids.push(self.picture_id(top, low));
            }
        }
        (ids, columns, rows)
    }

    /// The id of one cell of a picture: `▀` in the top pixel's color over the low
    /// one's, mapped to the palette the terminal draws.
    fn picture_id(&mut self, top: [u8; 3], low: [u8; 3]) -> u32 {
        self.screen.intern(Style {
            fg: self.look.depth.color(Color::Rgb(top[0], top[1], top[2])),
            bg: self.look.depth.color(Color::Rgb(low[0], low[1], low[2])),
            ..Style::default()
        })
    }

    /// Sends the image of `picture` to a terminal that draws placeholders, once,
    /// and answers the style of its rows with the columns and the rows they
    /// take; `None` for a picture that draws as half-block cells.
    ///
    /// The style is the image's own: its foreground is the color that names the
    /// image, which the palette must not change, and it takes nothing from the
    /// style budget.
    fn placeholder_cells(&mut self, picture: &Picture) -> Option<(u32, u16, u16)> {
        let native = self.native?;
        if native.protocol != Protocol::Placeholders {
            return None;
        }
        let source = picture.source.as_ref()?;
        let cells = picture.cells.as_ref()?;
        let (Ok(columns), Ok(rows)) = (u16::try_from(cells.columns), u16::try_from(cells.rows))
        else {
            return None;
        };
        let id = self.next_image_id();
        let mut images = std::mem::take(&mut self.images);
        images.clear();
        picture_native::transmit(&mut images, id, columns, rows, &source.data, native.tmux);
        self.screen.send_images(&images);
        self.images = images;
        self.image_ids.push(id);
        let (r, g, b) = picture_native::id_color(id);
        let style = self.screen.intern(Style {
            fg: Color::Rgb(r, g, b),
            ..Style::default()
        });
        Some((style, columns, rows))
    }

    /// Takes the id of the next image the terminal draws: one of the 24 bits a
    /// placeholder names, and after zero.
    fn next_image_id(&mut self) -> u32 {
        let id = self.image_id;
        self.image_id = match id.checked_add(1) {
            Some(next) if next <= 0x00FF_FFFF => next,
            _ => 1,
        };
        id
    }

    /// Appends the command that frees every image the terminal holds, for the
    /// exit write that closes the screen.
    fn delete_images(&mut self) {
        let tmux = self.native.is_some_and(|native| native.tmux);
        let mut images = std::mem::take(&mut self.images);
        images.clear();
        for id in std::mem::take(&mut self.image_ids) {
            picture_native::delete(&mut images, id, tmux);
        }
        self.screen.send_images(&images);
        self.images = images;
    }

    /// Takes a new terminal size from the event the terminal sent.
    fn resize(&mut self, columns: u16, rows: u16) {
        let (columns, rows) = (columns.max(1), rows.max(1));
        // A terminal draws an iTerm2 image at the cells it was told, so the
        // images go out again once the new layout has settled.
        if self.native.is_some() {
            self.arm_settle(picture_native::SETTLE);
        }
        self.screen.resize(columns, rows);
        self.reflow();
        self.editor_dirty = true;
        self.touch();
    }

    /// Prints what was left live, leaves the alternate screen when the mode drew
    /// full screen, and erases the live area. Both modes leave bracketed paste
    /// with the extended keys.
    async fn finish(&mut self, terminal: &mut impl Terminal) -> io::Result<()> {
        self.freeze_all();
        self.commit();
        // A kitty image outlives the screen that named it, so every image the
        // run sent is freed in the write that closes it.
        self.delete_images();
        self.screen.close(&mut self.frame);
        // Focus reports go off on every exit path, full screen or not.
        self.frame.extend_from_slice(term::FOCUS_OFF);
        if self.screen.is_full() && !self.frame.is_empty() {
            // The mouse capture goes with the exit write, after the print that
            // leaves the alternate screen, so no exit path leaves it on.
            self.frame.extend_from_slice(term::MOUSE_OFF);
        }
        self.frame.extend_from_slice(term::PASTE_OFF);
        self.frame.extend_from_slice(term::KEYS_OFF);
        terminal.write_frame(&self.frame).await
    }
}

/// The style of the dim text: prefixes, notes and tool results.
///
/// It is exactly [`crate::theme::Role::Dim`] with no color, the style the theme
/// resolves in terminal and no-color mode, which a test in `paint::theme` holds
/// it to. A region takes its ids from [`App::style`] as it is restyled.
const DIM: Style = Style {
    fg: Color::Default,
    bg: Color::Default,
    bold: false,
    dim: true,
    italic: false,
    underline: false,
    strikethrough: false,
    reverse: false,
    link: 0,
};

/// The style of the cursor.
///
/// It is exactly [`crate::theme::Role::Selection`] with no color; a region takes
/// the id from [`App::style`] as it is restyled.
const REVERSE: Style = Style {
    fg: Color::Default,
    bg: Color::Default,
    bold: false,
    dim: false,
    italic: false,
    underline: false,
    strikethrough: false,
    reverse: true,
    link: 0,
};

/// The prices of the model `id` in `models`, from the provider that lists it;
/// a recent id no provider lists has none.
fn prices_of(models: &Models, id: &str) -> Option<(f64, f64)> {
    models
        .providers
        .iter()
        .flat_map(|provider| &provider.models)
        .find(|model| &*model.id == id)
        .and_then(|model| model.prices)
}

/// Whether `word` is a level on any model: a named level or an off word. A
/// budget is not a level word.
fn is_level_word(word: &str) -> bool {
    let off = match reagent_ai::Reasoning::parse(word) {
        reagent_ai::Reasoning::Budget(_) => false,
        level => level.is_off(),
    };
    off || reagent_ai::levels::Level::parse(word).is_some()
}

/// The reasoning plan of the model `id` in `models`, from the provider that
/// lists it, and `None` when no row names that model or its plan is unknown.
fn plan_of<'a>(
    models: Option<&'a Models>,
    id: Option<&str>,
) -> Option<&'a reagent_ai::levels::Reasoning> {
    let id = id?;
    models?
        .providers
        .iter()
        .flat_map(|provider| &provider.models)
        .find(|model| &*model.id == id)?
        .reasoning
        .as_ref()
}

/// The part of `text` a call keeps: its last [`OUTPUT_KEEP`] bytes from the
/// start of a line, and the count of the lines before them.
///
/// A cut inside a line moves forward to the start of the next one, and a line
/// with no start in the kept part starts at a character boundary.
fn tail_of(text: &str) -> (&str, u64) {
    if text.len() <= OUTPUT_KEEP {
        return (text, 0);
    }
    let mut at = text.len() - OUTPUT_KEEP;
    if !text.is_char_boundary(at) {
        at = text.ceil_char_boundary(at);
    }
    if !text[..at].ends_with('\n')
        && let Some(end) = text[at..].find('\n')
    {
        at += end + 1;
    }
    (&text[at..], text[..at].matches('\n').count() as u64)
}

/// Cuts `text` to `width` columns, on a character boundary.
fn cut_in_place(text: &mut String, width: usize) {
    if width == 0 {
        text.clear();
        return;
    }
    let mut used = 0;
    for (at, c) in text.char_indices() {
        let next = used + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if next > width {
            text.truncate(at);
            return;
        }
        used = next;
    }
}

/// The rows `text` wraps into at `width` columns: a new row at an explicit
/// newline and before the word that would pass the width, the space between two
/// words dropped, and a word longer than a row cut at the row's end.
fn wrap_note(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for word in line.split(' ').filter(|word| !word.is_empty()) {
            let mut rest = word;
            let mut cells = columns(rest);
            if !row.is_empty() {
                if used + 1 + cells <= width {
                    row.push(' ');
                    row.push_str(rest);
                    used += 1 + cells;
                    continue;
                }
                rows.push(std::mem::take(&mut row));
            }
            while cells > width {
                let fits = fit(rest, width);
                row.push_str(&rest[..fits]);
                rows.push(std::mem::take(&mut row));
                rest = &rest[fits..];
                cells = columns(rest);
            }
            row.push_str(rest);
            used = cells;
        }
        rows.push(row);
    }
    rows
}

/// The columns `text` takes in a row.
fn columns(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// The bytes of `text` that fit `width` columns, and one character at least so
/// a row always advances.
fn fit(text: &str, width: usize) -> usize {
    let mut used = 0;
    for (at, c) in text.char_indices() {
        let next = used + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if next > width && at > 0 {
            return at;
        }
        used = next;
    }
    text.len()
}

/// The tool calls a message's content holds, oldest first.
fn calls_of(content: &[Content]) -> impl Iterator<Item = &ToolCall> {
    content.iter().filter_map(|block| match block {
        Content::ToolCall(call) => Some(call),
        _ => None,
    })
}

/// The text of a user message, the way the transcript draws it.
fn user_text(content: &[Content]) -> String {
    let mut text = String::new();
    for block in content {
        let Content::Text { text: block } = block else {
            continue;
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(block);
    }
    text
}

/// The text of a finished reply, the way the transcript draws it.
fn reply_text(reply: &AssistantMessage) -> String {
    let mut text = String::new();
    for block in &reply.content {
        let Content::Text { text: block } = block else {
            continue;
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(block);
    }
    text
}

/// The keys that decide what Tab folds, and the ticks a frame arms, checked
/// where the state they change lives.
#[cfg(test)]
mod key_tests {
    use super::*;

    /// The columns and rows of the test terminal.
    const COLUMNS: u16 = 80;
    const ROWS: u16 = 24;

    /// An app in full screen, its sidebar docked and its transcript empty.
    fn app() -> App {
        App::new(&Initialized::default(), (COLUMNS, ROWS), true)
    }

    /// A panel that asks for a tick each second it is shown, the one reason a
    /// frame arms the panels' timer.
    struct Ticker;

    impl crate::slot::Panel for Ticker {
        fn id(&self) -> &str {
            "ticker"
        }

        fn title(&self) -> &crate::slot::Text {
            static TITLE: crate::slot::Text = crate::slot::Text::Empty;
            &TITLE
        }

        fn priority(&self) -> crate::slot::Priority {
            crate::slot::Priority::default()
        }

        fn min_width(&self) -> u16 {
            1
        }

        fn update(&mut self, _update: &Update<'_>) -> bool {
            false
        }

        fn draw(&self, _size: crate::slot::Size, _out: &mut Vec<crate::slot::Component>) {}

        fn wants_tick(&self) -> bool {
            true
        }
    }

    #[test]
    fn tab_while_the_note_is_open_folds_no_transcript_block() {
        let mut app = app();
        let block = app.folds.add();
        app.approval = Some(ApprovalBox::new(
            Approval {
                id: 1,
                tool: "bash".into(),
                call: "rm -rf target".into(),
                rule: "outside the project".into(),
                save: false,
            },
            COLUMNS,
        ));
        // The note is open: the card owns the keys, and Tab is a key of the
        // transcript behind it only while no card waits.
        app.approval.as_mut().expect("the card").reason = Some(String::new());
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(
            !app.folds.is_open(block),
            "the block behind the card stays folded"
        );

        // With no card on screen the same key still folds the newest block.
        app.approval = None;
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(app.folds.is_open(block), "and Tab folds it with no card");
    }

    #[test]
    fn a_key_pulls_the_next_frame_in_past_a_far_tick() {
        let mut app = app();
        // The bar's next step, or a panel's tick, is armed a second out; a key
        // must draw on the next frame all the same.
        app.deadline = Some(tokio::time::Instant::now() + Duration::from_secs(1));
        app.touch();
        let deadline = app.deadline.expect("a frame is asked for");
        assert!(
            deadline <= tokio::time::Instant::now() + FRAME,
            "the key draws on the next frame, not on the next tick"
        );
    }

    #[test]
    fn a_frame_keeps_the_tick_a_panel_asked_for() {
        // Wide enough for the docked sidebar, which is where a panel's tick is
        // armed: the panels have no timer of their own.
        let mut app = App::new(&Initialized::default(), (140, ROWS), true);
        app.sidebar.add(Ticker);
        app.deadline = None;
        app.arm_tick();
        let armed = app.tick.expect("a panel that wants a tick arms one");
        assert_eq!(
            app.deadline,
            Some(tokio::time::Instant::from_std(armed)),
            "the loop waits on the tick"
        );
        // Frames of a busy app come one after another: a frame that draws the
        // status line must keep the instant the tick was armed for, or a panel
        // that wants one never gets it.
        app.touch();
        app.touch();
        assert_eq!(app.tick, Some(armed), "the armed tick stays where it was");
        assert!(
            app.deadline
                .is_some_and(|deadline| deadline <= tokio::time::Instant::from_std(armed)),
            "and the loop still reaches it: {:?}",
            app.deadline
        );
    }
}
