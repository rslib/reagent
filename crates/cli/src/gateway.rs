//! The `gateway` command: one long-lived process that hands the parked
//! questions of this machine to the gateway plugins, and takes their answers.
//!
//! A run parks while it waits for a question whose client is gone: it leaves one
//! file per open question in the `parked` folder of the discovery folder and
//! stops. The gateway holds that folder open with a file watcher, tells every
//! gateway plugin about each question it has not announced yet, and answers with
//! what a plugin sends back. It is the only process that may write an answer for
//! a plugin, and the token it hands out is the only thing that lets one in.
//!
//! Two things close a question without a plugin: `reagent answer`, which writes
//! the answer itself, and the deadline of `[gateway] deadline`, which writes an
//! answer of no lists, so the run goes on with the text a client shows when
//! nobody answered. Nothing expires while no gateway runs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reagent_agent::journal::record::{self, Aside};
use reagent_protocol::discovery::{self, Parked};
use reagent_script::plugin::{AnswerAsk, Plugins};
use reagent_store::Session;
use reagent_tui::watch::Watch;
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::answer::{self, Refused};

/// How the command is used.
const USAGE: &str = "usage: reagent gateway";

/// The most bytes of a plugin's name and its own `by` the journal records.
const BY_BYTES: usize = 64;

/// One question the plugins were told about, and the secret they were given.
struct Announced {
    /// The parked entry, as it was read.
    entry: Parked,
    /// The 32 hex digits only this process knows.
    token: String,
    /// When it expires, in milliseconds since the epoch, when it does.
    deadline: Option<u64>,
}

/// Runs the gateway until SIGINT or SIGTERM.
pub async fn run(argv: Vec<OsString>) -> ExitCode {
    match parse(&argv) {
        Ok(()) => {}
        Err(message) => {
            if !message.is_empty() {
                eprintln!("reagent: {message}");
            }
            println!("{USAGE}");
            return ExitCode::from(2);
        }
    }
    // Listen before anything is loaded, so a signal during the start ends the
    // gateway the same clean way.
    let stopped = stop();
    let Some(state) = crate::session::state_home() else {
        eprintln!("reagent: no state folder; set HOME or XDG_STATE_HOME");
        return ExitCode::from(2);
    };
    let lock = match lock(&state).await {
        Ok(lock) => lock,
        Err(Why::Running) => {
            eprintln!("reagent: a gateway already runs");
            return ExitCode::from(1);
        }
        Err(Why::Failed(message)) => {
            eprintln!("reagent: {message}");
            return ExitCode::from(2);
        }
    };
    let (deadline, sandbox) = match settings() {
        Ok(settings) => (settings.deadline, settings.sandbox),
        Err(message) => {
            eprintln!("reagent: {message}");
            return ExitCode::from(2);
        }
    };
    let folder = discovery::folder(&state);
    let parked = match discovery::create_parked(&folder).await {
        Ok(folder) => folder,
        Err(error) => {
            eprintln!(
                "reagent: cannot make {}: {error}",
                discovery::parked_folder(&folder).display()
            );
            return ExitCode::from(1);
        }
    };
    let Some(root) = reagent_config::global_dir().map(|dir| dir.join("plugins")) else {
        eprintln!("reagent: no config folder; set XDG_CONFIG_HOME or HOME");
        return ExitCode::from(2);
    };
    let (answers, asks) = tokio::sync::mpsc::unbounded_channel();
    let (plugins, notes) =
        Plugins::load_gateway(&root, &state.join("reagent"), sandbox, answers).await;
    for note in notes {
        eprintln!("reagent: {note}");
    }
    let Some(plugins) = plugins else {
        eprintln!("reagent: no gateway plugins in {}", root.display());
        return ExitCode::from(1);
    };
    let mut watch = match Watch::new(&parked) {
        Ok(watch) => watch,
        Err(error) => {
            eprintln!("reagent: cannot watch {}: {error}", parked.display());
            return ExitCode::from(1);
        }
    };
    serve(
        &state, &folder, &plugins, deadline, asks, &mut watch, stopped,
    )
    .await;
    drop(plugins);
    drop(lock);
    ExitCode::SUCCESS
}

/// Reads the words of the command, before the command's name.
fn parse(words: &[OsString]) -> Result<(), String> {
    for word in words {
        let word = word.to_str().ok_or_else(|| {
            format!(
                "{} is not a word this command reads",
                word.to_string_lossy()
            )
        })?;
        match word {
            "-h" | "--help" | "" => {}
            other => return Err(format!("{other} is not a flag of this command")),
        }
    }
    Ok(())
}

/// Why the gateway could not take the lock of this state folder.
enum Why {
    /// Another gateway holds it.
    Running,
    /// The lock file could not be opened or locked; the text says why.
    Failed(String),
}

/// Holds the lock of one state folder for as long as it lives.
///
/// The lock is the file system's own: the second gateway fails at once, and the
/// kernel drops the lock when this process ends, however it ends.
#[expect(clippy::disallowed_types, reason = "the file is only held, never read")]
async fn lock(state: &Path) -> Result<std::fs::File, Why> {
    let at = state.join("gateway.lock");
    match tokio::task::spawn_blocking(move || lock_blocking(&at)).await {
        Ok(lock) => lock,
        Err(_) => Err(Why::Failed("the lock task stopped".to_string())),
    }
}

/// [`lock`] on the blocking pool.
#[expect(clippy::disallowed_types, reason = "runs inside spawn_blocking")]
fn lock_blocking(at: &Path) -> Result<std::fs::File, Why> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(at)
        .map_err(|error| Why::Failed(format!("{}: {error}", at.display())))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Why::Running),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(Why::Failed(format!("{}: {error}", at.display())))
        }
    }
}

/// What the gateway reads of this machine's config.
struct Settings {
    /// How long a question waits, and `None` when it never expires.
    deadline: Option<Duration>,
    /// The helper every child process of a plugin starts through.
    sandbox: Option<PathBuf>,
}

/// The settings of this machine, from the global config file and its profile.
fn settings() -> Result<Settings, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let layers = reagent_config::load_for(&cwd, None)?;
    let merged = reagent_config::merge_all(&layers);
    let deadline = merged.gateway.deadline()?;
    let sandbox = match merged.sandbox.mode.as_deref() {
        None | Some("off") => None,
        Some("auto" | "on") => std::env::current_exe().ok(),
        Some(other) => {
            return Err(format!(
                "config: sandbox.mode must be off, auto or on, not {other:?}"
            ));
        }
    };
    Ok(Settings { deadline, sandbox })
}

/// The loop of one gateway: the folder watcher, the answers of the plugins and
/// the one deadline that comes first.
///
/// Nothing here polls: the watch and the sleep wake it, and the folder is read
/// once at the start and after each notification.
async fn serve(
    state: &Path,
    folder: &Path,
    plugins: &Plugins,
    deadline: Option<Duration>,
    mut asks: UnboundedReceiver<AnswerAsk>,
    watch: &mut Watch,
    stopped: impl Future<Output = ()>,
) {
    let mut announced: Vec<Announced> = Vec::new();
    let mut dirty = true;
    let mut stopped = std::pin::pin!(stopped);
    loop {
        for note in plugins.take_notes() {
            eprintln!("reagent: {note}");
        }
        if dirty {
            dirty = false;
            read(state, folder, plugins, deadline, &mut announced).await;
        }
        let wake = earliest(&announced);
        let timer = async move {
            match wake {
                Some(at) => match tokio::time::Instant::now().checked_add(until(at)) {
                    Some(when) => tokio::time::sleep_until(when).await,
                    None => std::future::pending::<()>().await,
                },
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = &mut stopped => break,
            () = timer => {
                expire(state, folder, plugins, &mut announced).await;
            }
            Some(ask) = asks.recv() => {
                if answered(state, folder, plugins, &mut announced, ask).await {
                    dirty = true;
                }
            }
            _ = watch.changed() => {
                dirty = true;
            }
        }
    }
}

/// Reads the parked folder: announces every open question the plugins have not
/// heard of, and closes the ones that are no longer open.
async fn read(
    state: &Path,
    folder: &Path,
    plugins: &Plugins,
    deadline: Option<Duration>,
    announced: &mut Vec<Announced>,
) {
    let open = answer::list(folder, state).await;
    // Close first: a question that ended and came back keeps its old id out of
    // the way before the new one is announced.
    let closed: Vec<String> = announced
        .iter()
        .map(|known| known.entry.qid.clone())
        .filter(|qid| !open.iter().any(|entry| entry.qid == *qid))
        .collect();
    for qid in closed {
        let Some(at) = announced.iter().position(|known| known.entry.qid == qid) else {
            continue;
        };
        let known = announced.remove(at);
        let by = closed_by(state, &known.entry.session, &qid).await;
        let how = match by.as_deref() {
            Some("deadline") => "expired",
            _ => "answered",
        };
        plugins.event(
            "question_closed",
            json!({ "qid": qid, "how": how, "by": by }),
        );
    }
    for entry in open {
        if announced.iter().any(|known| known.entry.qid == entry.qid) {
            continue;
        }
        let token = token();
        let at = deadline.map(|wait| deadline_at(entry.at, wait));
        plugins.event(
            "question_parked",
            json!({
                "qid": entry.qid,
                "session": entry.session,
                "questions": entry.questions,
                "at": entry.at,
                "deadline": at,
                "token": token,
            }),
        );
        announced.push(Announced {
            entry,
            token,
            deadline: at,
        });
    }
}

/// Writes the answer one gateway plugin asked for, and answers its promise.
///
/// Answers `true` when the announced set changed without the folder saying so,
/// which asks for a read: a question another answer closed is no longer open.
async fn answered(
    state: &Path,
    folder: &Path,
    plugins: &Plugins,
    announced: &mut Vec<Announced>,
    ask: AnswerAsk,
) -> bool {
    let Some(at) = announced
        .iter()
        .position(|known| known.entry.qid == ask.qid)
    else {
        let _ = ask.reply.send("stale".to_string());
        return false;
    };
    if ask.token != announced[at].token {
        let _ = ask.reply.send("denied".to_string());
        return false;
    }
    let entry = announced[at].entry.clone();
    if answer::counted(&entry, &ask.answers).is_err() {
        let _ = ask.reply.send("denied".to_string());
        return false;
    }
    let by = format!("gateway:{}:{}", ask.plugin, cut(&ask.by, BY_BYTES));
    let verdict = match answer::record(state, entry.clone(), &by, &ask.answers).await {
        Ok((session, cwd)) => {
            if let Err(error) = discovery::remove_parked(folder, &entry.qid).await {
                eprintln!(
                    "reagent: warning: could not remove the entry of {}: {error}",
                    entry.qid
                );
            }
            if let Err(message) = answer::start(state, &cwd, &session).await {
                eprintln!(
                    "reagent: recorded the answer, but session {session} did not start again: {message}"
                );
            }
            announced.remove(at);
            plugins.event(
                "question_closed",
                json!({ "qid": entry.qid, "how": "answered", "by": by }),
            );
            "ok"
        }
        Err(Refused::Stale) => "stale",
        // The question is still open: a client of the live session can answer
        // it, or a later try can, so the plugin must not drop it.
        Err(refused) => {
            eprintln!("reagent: {}", why(&refused));
            "busy"
        }
    };
    let _ = ask.reply.send(verdict.to_string());
    verdict == "stale"
}

/// Expires every announced question whose deadline has come.
///
/// The closing record carries no answers, so the run that goes on reads the text
/// a client shows when nobody answered. A question whose answer could not be
/// written keeps its entry and loses its deadline: the next gateway start reads
/// its deadline again.
async fn expire(state: &Path, folder: &Path, plugins: &Plugins, announced: &mut Vec<Announced>) {
    let now = now_ms();
    for at in (0..announced.len()).rev() {
        if !announced[at]
            .deadline
            .is_some_and(|deadline| deadline <= now)
        {
            continue;
        }
        let entry = announced[at].entry.clone();
        match answer::record(state, entry.clone(), "deadline", "null").await {
            Ok((session, cwd)) => {
                if let Err(error) = discovery::remove_parked(folder, &entry.qid).await {
                    eprintln!(
                        "reagent: warning: could not remove the entry of {}: {error}",
                        entry.qid
                    );
                }
                if let Err(message) = answer::start(state, &cwd, &session).await {
                    eprintln!(
                        "reagent: recorded the answer, but session {session} did not start again: {message}"
                    );
                }
                announced.remove(at);
                plugins.event(
                    "question_closed",
                    json!({ "qid": entry.qid, "how": "expired", "by": "deadline" }),
                );
            }
            Err(refused) => {
                eprintln!("reagent: {} did not expire: {}", entry.qid, why(&refused));
                announced[at].deadline = None;
            }
        }
    }
}

/// Who closed question `qid` of session `id`, from the records of its journal,
/// or `None` when there is none to read.
async fn closed_by(state: &Path, id: &str, qid: &str) -> Option<String> {
    let state = state.to_path_buf();
    let id = id.to_string();
    let qid = qid.to_string();
    tokio::task::spawn_blocking(move || {
        let dir = answer::find(&state, &id)?;
        let session = Session::open(&dir).ok()?;
        for record in session.records() {
            let record = record.ok()?;
            if let Ok(Some(Aside::Question(record::Question::Answered {
                qid: closed, by, ..
            }))) = record::decode_aside(record.kind, &record.payload)
                && closed == qid
            {
                return Some(by);
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

/// The deadline of a question parked at `at`, in milliseconds since the epoch,
/// that waits for `wait`; a wait too long to count ends at the end of time.
fn deadline_at(at: u64, wait: Duration) -> u64 {
    at.saturating_add(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX))
}

/// The deadline of the announced set that comes first, in milliseconds since
/// the epoch.
fn earliest(announced: &[Announced]) -> Option<u64> {
    announced.iter().filter_map(|known| known.deadline).min()
}

/// How long is left until `at`, in milliseconds since the epoch.
fn until(at: u64) -> Duration {
    Duration::from_millis(at.saturating_sub(now_ms()))
}

/// The milliseconds since the epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// The token of one announced question: 32 hex digits from the store's random
/// source, which is a new one after every restart.
fn token() -> String {
    format!(
        "{:016x}{:016x}",
        reagent_store::paths::random_u64(),
        reagent_store::paths::random_u64()
    )
}

/// Why an answer was not written, as one line.
fn why(refused: &Refused) -> String {
    match refused {
        Refused::Failed(message) | Refused::Live(message) => message.clone(),
        Refused::Stale => "the journal of the session no longer waits for it".to_string(),
    }
}

/// The longest prefix of `text` that is at most `max` bytes and ends on a char
/// boundary.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    &text[..text.floor_char_boundary(max)]
}

/// Answers when the gateway is told to stop: `SIGINT` or `SIGTERM`.
///
/// The handlers are set up by this call, not by the first poll, so a signal
/// that comes while the gateway starts is not lost. A handler that cannot be
/// set up leaves only the other one, and with neither the gateway runs until
/// it is killed.
fn stop() -> impl Future<Output = ()> {
    use tokio::signal::unix::{SignalKind, signal};
    let interrupt = signal(SignalKind::interrupt());
    let terminate = signal(SignalKind::terminate());
    for error in [interrupt.as_ref().err(), terminate.as_ref().err()]
        .into_iter()
        .flatten()
    {
        eprintln!("reagent: cannot listen for a signal: {error}");
    }
    async move {
        let one = async {
            match interrupt {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        let other = async {
            match terminate {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = one => {}
            () = other => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_a_deadline_too_long_to_count_never_comes() {
        let wait = Duration::from_secs(999_999_999_999 * 86_400);
        assert_eq!(deadline_at(1_000, wait), u64::MAX);
        assert_eq!(deadline_at(u64::MAX - 1, Duration::from_secs(5)), u64::MAX);
        assert_eq!(deadline_at(1_000, Duration::from_secs(2)), 3_000);
    }
}
