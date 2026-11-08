//! The terminal UI: a worker for this folder and the app attached to it.
//!
//! `reagent` with no `-p` on a terminal starts the UI. The worker is this
//! binary run again as `reagent worker` with the same run flags, detached from
//! the terminal: its stdin is null, its stderr a log file, its stdout a pipe
//! that carries the ready line and then closes, and its own process group, so
//! Ctrl+C in the UI does not reach it. The UI reads the join token from the
//! worker's discovery entry and runs the app over the socket.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use reagent_protocol::discovery;
use reagent_store::paths;
use reagent_tui::app::{self, Exit, Options, Real};
use tokio::io::AsyncBufReadExt;

use crate::args::{Args, Sessions};

/// What a run with no `-p` says when no terminal is attached.
const NO_TERMINAL: &str = "a task needs -p when stdin or stdout is not a terminal";

/// How long the worker has to print its ready line.
const READY: Duration = Duration::from_secs(5);

/// A million: the unit a price is shown in.
const MILLION: f64 = 1_000_000.0;

/// The log file of the worker's stderr, beside the session store.
const LOG: &str = "worker.log";

/// The ready line a worker prints once it serves a session.
#[derive(Debug, serde::Deserialize)]
struct Ready {
    /// The session the worker serves.
    session: String,
}

/// Whether the arguments ask for the UI: a session and no task, nothing else.
///
/// A worker keeps its own path, since the UI is the process that starts one.
/// `--json` is a headless format, so it keeps the plain run even with no `-p`,
/// and `-r` and `--fork` keep it too: the UI starts a new session or continues
/// the newest one.
pub(crate) fn wanted(args: &Args) -> bool {
    args.task.is_none()
        && !args.worker
        && !args.json
        && matches!(args.sessions, Sessions::New | Sessions::Continue)
}

/// Runs the UI for `args`, or exits with the reason it could not start.
pub(crate) async fn run(args: Args) -> ExitCode {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        // A continue has a session even with no terminal, so it keeps the
        // headless path and its own errors. A plain run has nothing to do but
        // say what it needs.
        return match args.sessions {
            Sessions::Continue => crate::run::run(args).await,
            _ => {
                eprintln!("reagent: {NO_TERMINAL}");
                ExitCode::from(2)
            }
        };
    }
    // The worker resolves the folder the same way, so both key the session by
    // the same path.
    let cwd = match tokio::fs::canonicalize(&args.cwd).await {
        Ok(cwd)
            if tokio::fs::metadata(&cwd)
                .await
                .is_ok_and(|meta| meta.is_dir()) =>
        {
            cwd
        }
        _ => {
            eprintln!("reagent: --cwd {} is not a folder", args.cwd.display());
            return ExitCode::from(2);
        }
    };
    match attach(&args, &cwd).await {
        Ok(exit) => ExitCode::from(u8::try_from(exit.code).unwrap_or(1)),
        Err(message) => {
            eprintln!("reagent: {message}");
            ExitCode::from(1)
        }
    }
}

/// Starts the worker of the session, attaches to it and runs the app until it
/// ends.
///
/// A failure to start leaves the terminal as it was: raw mode goes on only once
/// the worker serves the session.
async fn attach(args: &Args, cwd: &Path) -> Result<Exit, String> {
    // The mode is chosen before the worker starts, so a config that names no
    // mode stops the start instead of leaving a worker behind.
    let config = {
        let cwd = cwd.to_path_buf();
        let profile = args.profile.clone();
        tokio::task::spawn_blocking(move || crate::args::ui_setting(&cwd, profile.as_deref()))
            .await
            .map_err(|_| "the config read failed".to_string())?
    }?;
    let term = std::env::var("TERM").ok();
    let full = full_mode(args.inline, config, term.as_deref());
    // The theme file the config names, read with the mode's config so a broken
    // file is found before a worker starts. `None` is the mode's own theme.
    let theme = {
        let cwd = cwd.to_path_buf();
        let profile = args.profile.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<std::path::PathBuf>, String> {
            let layers = reagent_config::load_for(&cwd, profile.as_deref())?;
            Ok(reagent_config::merge_all(&layers)
                .theme()
                .map(std::path::Path::to_path_buf))
        })
        .await
        .map_err(|_| "the config read failed".to_string())?
    }?;
    let prices = {
        let config = args.price;
        let provider = args.provider_name.clone();
        let model = args.model.clone();
        tokio::task::spawn_blocking(move || prices(config, provider.as_deref(), &model))
            .await
            .ok()
            .flatten()
    };
    // The keys file, which the app reads once at start: a missing one is no
    // table and no note.
    let keys = reagent_config::global_dir().map(|dir| dir.join("keys.toml"));
    let state = crate::session::state_home()
        .ok_or_else(|| "no state folder; set HOME or XDG_STATE_HOME".to_string())?;
    let given: Vec<OsString> = std::env::args_os().skip(1).collect();
    let entry = start_worker(&state, worker_flags(given.clone())).await?;
    // A continued session shows the turns it already has; a new one starts
    // empty. The worker names the session, so the folder is where the store
    // keeps it.
    let session_dir = match args.sessions {
        Sessions::Continue => Some(
            paths::sessions_root(&state)
                .join(paths::cwd_key(cwd))
                .join(&entry.session),
        ),
        _ => None,
    };
    // The terminal is made before the options name its answer handle: the app
    // asks for the background with its first frame setup, and the handle knows
    // the answer before the tty is even open.
    let terminal = Real::new().map_err(|error| format!("the terminal: {error}"))?;
    // The host answers what only this binary can: the model list, the files,
    // the logins and the worker of another session, which starts under the flags
    // of this run.
    let host = crate::tui_host::CliHost::new(
        cwd.to_path_buf(),
        args.profile.clone(),
        state.clone(),
        given,
    );
    let options = Options {
        socket: entry.socket,
        token: entry.token,
        session_dir,
        client_name: format!("reagent/{}", env!("CARGO_PKG_VERSION")),
        full,
        prices,
        state: Some(state),
        theme,
        keys,
        background: Some(terminal.background()),
        cwd: Some(cwd.to_path_buf()),
        host: Some(app::HostHandle::new(std::sync::Arc::new(host))),
    };
    // The worker is not ours to stop: it goes when its last client leaves.
    app::run(options, terminal)
        .await
        .map_err(|error| format!("the session: {error}"))
}

/// Starts a worker with `flags` and answers the discovery entry that joins it.
///
/// The worker is not ours to stop: it keeps running when this process exits,
/// and its stderr goes to the log [`reason`] reads.
pub(crate) async fn start_worker(
    state: &Path,
    flags: Vec<OsString>,
) -> Result<discovery::Entry, String> {
    let log = log_path().await;
    let exe =
        std::env::current_exe().map_err(|error| format!("could not find this binary: {error}"))?;
    let stderr = match &log {
        Some(path) => match tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .await
        {
            Ok(file) => Stdio::from(file.into_std().await),
            Err(_) => Stdio::null(),
        },
        None => Stdio::null(),
    };
    let mut command = tokio::process::Command::new(exe);
    command
        .args(flags)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
        // Its own process group, so a Ctrl+C aimed at this terminal stays here.
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start the worker: {error}"))?;
    let session = ready_line(&mut child, log.as_deref()).await?.session;
    let folder = discovery::folder(state);
    discovery::list(&folder)
        .await
        .into_iter()
        .find(|entry| entry.session == session)
        .ok_or_else(|| {
            format!(
                "the worker of {session} left no entry in {}",
                folder.display()
            )
        })
}

/// The arguments of the worker: the word that makes the binary serve the
/// session, then every word the user gave this run, unchanged, so no run flag
/// is lost on the way. The worker resolves `--cwd` the same way this run did,
/// so both name the session by the same folder.
///
/// `--inline` is the one word left out: it picks how this terminal draws, and
/// the worker has no terminal.
fn worker_flags(given: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    words(given, Choice::Keep)
}

/// The arguments of a worker for another session: the run's own words without
/// the session it picked -- `-c`, `-r`, `--fork` and `-p` with their values --
/// and `--resume <session>` in their place.
///
/// A worker serves one session, so the session the UI started is not its own.
pub(crate) fn resume_flags(
    given: impl IntoIterator<Item = OsString>,
    session: &str,
) -> Vec<OsString> {
    words(given, Choice::Resume(session))
}

/// The arguments of a worker for a new session: the run's own words without
/// the session it picked, and no session choice in their place.
///
/// A worker with no session choice starts a new one.
pub(crate) fn new_flags(given: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    words(given, Choice::New)
}

/// The arguments of a worker for a session forked from `session`: the run's own
/// words without the session it picked, and `--fork <session>` in their place.
///
/// The fork takes the newest turn of `session` as its own, so the new worker
/// starts with that history.
pub(crate) fn fork_flags(
    given: impl IntoIterator<Item = OsString>,
    session: &str,
) -> Vec<OsString> {
    words(given, Choice::Fork(session))
}

/// The session a worker is started for.
enum Choice<'a> {
    /// The session this run picked: every word reaches the worker as it is.
    Keep,
    /// The session `--resume` names.
    Resume(&'a str),
    /// The session `--fork` forks from, at its newest turn.
    Fork(&'a str),
    /// A new session, which no session choice names.
    New,
}

/// The words of a worker: the run's flags, with the session choice of `choice`
/// in place of the one this run picked.
fn words(given: impl IntoIterator<Item = OsString>, choice: Choice<'_>) -> Vec<OsString> {
    let mut flags: Vec<OsString> = vec![OsString::from("worker")];
    // A worker's own command line starts with `worker` already: that word is
    // the one above, not a second one.
    let mut given = given.into_iter().peekable();
    if given.peek().is_some_and(|word| word == "worker") {
        given.next();
    }
    // Whether the words carry a session of this run, which a worker of another
    // session drops.
    let dropped = !matches!(choice, Choice::Keep);
    // Whether the previous word was a flag whose value is the next word, which
    // is then passed on even when it reads like a flag, as `-p --inline` does.
    let mut value = false;
    // Whether the previous word was a session choice that drops the word after
    // it with it.
    let mut drop_next = false;
    for word in given {
        if std::mem::take(&mut drop_next) {
            continue;
        }
        if !std::mem::take(&mut value) {
            match word.to_str() {
                Some("--inline") => continue,
                Some(flag) if dropped && session_flag(flag) => {
                    drop_next = drop_value(flag);
                    continue;
                }
                Some(flag) => value = takes_value(flag),
                None => {}
            }
        }
        flags.push(word);
    }
    match choice {
        Choice::Keep | Choice::New => {}
        Choice::Resume(session) => flags.extend(["--resume", session].map(OsString::from)),
        Choice::Fork(session) => flags.extend(["--fork", session].map(OsString::from)),
    }
    flags
}

/// Whether `flag` is a session choice, which the worker of another session
/// drops: the session it starts or continues, or the task it runs.
fn session_flag(flag: &str) -> bool {
    matches!(
        flag,
        "-c" | "--continue" | "-p" | "--print" | "-r" | "--resume" | "--fork" | "--no-session"
    ) || attached(flag)
}

/// Whether `flag` carries the value of a session choice itself, as `-ptask`,
/// `-rref`, `--print=task`, `--resume=ref` and `--fork=ref` write it.
fn attached(flag: &str) -> bool {
    ["-p", "-r", "--print=", "--resume=", "--fork="]
        .iter()
        .any(|name| flag.len() > name.len() && flag.starts_with(name))
}

/// Whether dropping `flag` also drops the word after it, since the value sits
/// there and not in the word itself.
fn drop_value(flag: &str) -> bool {
    matches!(flag, "-p" | "--print" | "-r" | "--resume" | "--fork")
}

/// Whether `flag` reads its value from the word after it, so that word is no
/// flag of its own.
fn takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "-p" | "--print"
            | "--model"
            | "--fork"
            | "--provider"
            | "--base-url"
            | "--api-key-env"
            | "--header"
            | "--max-tokens"
            | "--reasoning"
            | "--deadline"
            | "-t"
            | "--tools"
            | "--policy"
            | "--mcp"
            | "--cwd"
            | "--profile"
    )
}

/// Whether the UI draws full screen: `--inline`, a config that says
/// `ui = "inline"`, or a terminal that cannot do full screen all keep the
/// inline UI.
fn full_mode(inline: bool, config: Option<reagent_config::Ui>, term: Option<&str>) -> bool {
    if inline || config == Some(reagent_config::Ui::Inline) {
        return false;
    }
    !matches!(term, None | Some("dumb"))
}

/// What the model of `args` costs in USD per million input and output tokens:
/// its config price, else the price of its model list, which may have come from
/// the models.dev catalogue. `None` when neither names a price.
///
/// The model list is a file, so call it in `spawn_blocking`.
fn prices(
    config: Option<reagent_config::ModelPrice>,
    provider: Option<&str>,
    model: &str,
) -> Option<(f64, f64)> {
    if let Some(price) = config {
        return Some((price.input, price.output));
    }
    let dir = crate::models::cache_dir()?;
    let cache = reagent_ai::catalog::read(&dir, provider?)?;
    let entry = cache.models.into_iter().find(|entry| entry.id == model)?;
    // The catalogue prices one token; the panels show a million of them.
    let price = entry.price?;
    Some((price.input * MILLION, price.output * MILLION))
}

/// The file the worker's stderr goes to, or `None` when there is no state
/// folder to write it in.
async fn log_path() -> Option<PathBuf> {
    let dir = reagent_config::auth::dir().ok()?;
    tokio::fs::create_dir_all(&dir).await.ok()?;
    // One log per TUI start, so a second start cannot cut the log of a worker
    // that still writes to it, or show another session's reason.
    Some(dir.join(format!("{LOG}.{}", std::process::id())))
}

/// Reads the worker's ready line, or the reason it printed none.
async fn ready_line(
    child: &mut tokio::process::Child,
    log: Option<&Path>,
) -> Result<Ready, String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "the worker has no stdout".to_string())?;
    let mut line = String::new();
    let read = tokio::time::timeout(
        READY,
        tokio::io::BufReader::new(stdout).read_line(&mut line),
    )
    .await;
    match read {
        // A worker that stops before its ready line said why on stderr.
        Ok(Ok(0)) => Err(format!("the worker stopped: {}", reason(log).await)),
        Ok(Ok(_)) => serde_json::from_str(line.trim_end())
            .map_err(|error| format!("the worker's ready line is not JSON: {error}: {line}")),
        Ok(Err(error)) => Err(format!("could not read the worker's ready line: {error}")),
        Err(_) => Err(format!(
            "the worker printed no ready line in {} seconds",
            READY.as_secs()
        )),
    }
}

/// The last line the worker logged, or a plain sentence when it logged none.
async fn reason(log: Option<&Path>) -> String {
    let Some(path) = log else {
        return "it printed no reason".to_string();
    };
    let Ok(text) = tokio::fs::read_to_string(path).await else {
        return "it printed no reason".to_string();
    };
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map_or_else(|| "it printed no reason".to_string(), str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The TUI reads the journal without the agent crate, so its copy of the
    /// base record kind must match the kind the agent writes.
    #[test]
    fn the_tui_knows_the_base_record_kind() {
        assert_eq!(
            reagent_tui::history::BASE,
            reagent_agent::journal::kinds::BASE
        );
    }

    /// A run flag that the old rebuilt list left out, such as `--reasoning`,
    /// reaches the worker with every other word, in order.
    #[test]
    fn every_run_flag_reaches_the_worker() {
        let given = [
            "--model",
            "m",
            "--reasoning",
            "high",
            "--tools",
            "-bash",
            "-c",
        ];
        let flags = worker_flags(given.iter().map(OsString::from));
        let want: Vec<OsString> = std::iter::once("worker")
            .chain(given)
            .map(OsString::from)
            .collect();
        assert_eq!(flags, want);
    }

    /// `--inline` is the UI's own flag: the worker reads the mode from the
    /// config, so the word never reaches it while every run flag does.
    #[test]
    fn the_inline_flag_stays_with_the_ui() {
        let given = ["--inline", "--model", "m", "-p", "task"];
        let flags = worker_flags(given.iter().map(OsString::from));
        let want: Vec<OsString> = ["worker", "--model", "m", "-p", "task"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(flags, want);
    }

    /// A word that follows a flag is that flag's value, even when it reads like
    /// the UI's own: `-p --inline` is a task, and must still reach the worker.
    #[test]
    fn a_task_that_reads_like_the_flag_reaches_the_worker() {
        let given = ["-p", "--inline", "--inline"];
        let flags = worker_flags(given.iter().map(OsString::from));
        let want: Vec<OsString> = ["worker", "-p", "--inline"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(flags, want);
    }

    /// A worker for another session drops the session this run picked, with the
    /// value it carries, and resumes the session it is asked for.
    #[test]
    fn the_session_of_the_run_stays_with_the_ui() {
        let given = [
            "-c",
            "--model",
            "m",
            "-r",
            "old",
            "--fork=old@2",
            "-ptask",
            "--print",
            "task",
            "--resume",
            "older",
            "--inline",
        ];
        let flags = resume_flags(given.iter().map(OsString::from), "s1");
        let want: Vec<OsString> = ["worker", "--model", "m", "--resume", "s1"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(flags, want);
    }

    /// The host is given the words of a worker's own command line, which
    /// already start with `worker`: the worker of another session still starts
    /// with one `worker` word.
    #[test]
    fn resume_flags_keep_one_worker_word() {
        let given = worker_flags(["--model", "m"].iter().map(OsString::from));
        let flags = resume_flags(given, "s1");
        let want: Vec<OsString> = ["worker", "--model", "m", "--resume", "s1"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(flags, want);
    }

    /// `--no-session` is a session choice too: the worker of another session
    /// resumes that session, so the flag would clash with `--resume`.
    #[test]
    fn no_session_does_not_reach_the_worker_of_another_session() {
        let given = ["--no-session", "--model", "m"];
        let flags = resume_flags(given.iter().map(OsString::from), "s1");
        let want: Vec<OsString> = ["worker", "--model", "m", "--resume", "s1"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(flags, want);
    }

    /// A run flag that only looks like a session choice is passed on, and a
    /// bare `-r` drops no word of its own.
    #[test]
    fn the_flags_of_the_run_reach_the_worker_of_another_session() {
        let given = ["--profile", "-p", "--tools", "-c", "-r"];
        let flags = resume_flags(given.iter().map(OsString::from), "s1");
        let want: Vec<OsString> = [
            "worker",
            "--profile",
            "-p",
            "--tools",
            "-c",
            "--resume",
            "s1",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(flags, want);
    }

    /// The worker of a new session drops the session this run picked and names
    /// no other, so it starts one of its own.
    #[test]
    fn new_flags_drop_the_session_choice() {
        let given = ["-c", "--model", "m"];
        let flags = new_flags(given.iter().map(OsString::from));
        let want: Vec<OsString> = ["worker", "--model", "m"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(flags, want);
    }

    /// The worker of a fork names the session it forks from, in the place of
    /// the session this run picked.
    #[test]
    fn fork_flags_name_the_parent() {
        let given = ["-r", "s0", "--model", "m"];
        let flags = fork_flags(given.iter().map(OsString::from), "s1");
        let want: Vec<OsString> = ["worker", "--model", "m", "--fork", "s1"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(flags, want);
    }

    /// A session choice with its value in the same word is dropped too.
    #[test]
    fn new_flags_drop_an_attached_fork() {
        let given = ["--fork=s0@2", "--model", "m"];
        let flags = new_flags(given.iter().map(OsString::from));
        let want: Vec<OsString> = ["worker", "--model", "m"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(flags, want);
    }

    /// A config price is given per million tokens, the unit the panels show, so
    /// it is passed on as written and no model list is read.
    #[test]
    fn a_config_price_is_kept_per_million() {
        let price = reagent_config::ModelPrice {
            input: 3.0,
            output: 15.0,
            cached: 0.3,
            cache_write: None,
        };
        assert_eq!(prices(Some(price), None, "m"), Some((3.0, 15.0)));
        assert_eq!(prices(None, None, "m"), None, "and no model list is read");
    }

    /// The flag, a config that says inline and a terminal that cannot draw
    /// full screen all keep the inline UI; anything else draws full screen.
    #[test]
    fn the_mode_is_full_unless_something_asks_for_inline() {
        use reagent_config::Ui;
        assert!(full_mode(false, None, Some("xterm-256color")));
        assert!(
            full_mode(false, Some(Ui::Full), Some("xterm-256color")),
            "a config that says full draws full screen"
        );
        assert!(
            !full_mode(true, Some(Ui::Full), Some("xterm-256color")),
            "the flag beats a config that says full"
        );
        assert!(!full_mode(true, None, Some("xterm-256color")));
        assert!(!full_mode(false, Some(Ui::Inline), Some("xterm-256color")));
        assert!(!full_mode(false, None, Some("dumb")), "a dumb terminal");
        assert!(!full_mode(false, None, None), "a terminal with no TERM");
    }
}
