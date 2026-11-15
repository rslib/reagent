//! The `answer` command: the questions the parked runs of this machine left, and
//! the answer to one of them.
//!
//! A run parks when the last client of its session goes while it waits for a
//! question: it ends `Parked`, leaves one file per open question in the `parked`
//! folder of the discovery folder, and stops. `reagent answer` reads those
//! files, writes the answer into the journal of the session and starts its
//! worker again with no client, so the run goes on headless.

use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use reagent_agent::journal::Journal;
use reagent_agent::journal::resume;
use reagent_protocol::discovery;
use reagent_store::Header;
use reagent_store::Session;
use reagent_store::paths;
use tokio::io::AsyncReadExt;

use crate::session::Meta;

/// How the command is used, printed for a wrong command line or `--help`.
const USAGE: &str = "usage: reagent answer [<qid>] [--json] [--no-resume]";

/// The command line of `answer`.
struct Command {
    /// The question to answer, or `None` to list the parked ones.
    qid: Option<String>,
    /// Whether the listing is the JSON array of the entries.
    json: bool,
    /// Whether the session goes on after the answer is written.
    resume: bool,
}

impl Command {
    /// Reads the words of the command, before the command's name.
    fn parse(words: &[OsString]) -> Result<Command, String> {
        let mut command = Command {
            qid: None,
            json: false,
            resume: true,
        };
        for word in words {
            let word = word.to_str().ok_or_else(|| {
                format!(
                    "{} is not a word this command reads",
                    word.to_string_lossy()
                )
            })?;
            match word {
                "--json" => command.json = true,
                "--no-resume" => command.resume = false,
                "-h" | "--help" => return Err(String::new()),
                "" => {}
                other if other.starts_with('-') => {
                    return Err(format!("{other} is not a flag of this command"));
                }
                other => {
                    if command.qid.is_some() {
                        return Err("one question at a time".to_string());
                    }
                    command.qid = Some(other.to_string());
                }
            }
        }
        Ok(command)
    }
}

/// Lists the questions the parked runs left, or answers the one named.
pub async fn run(argv: Vec<OsString>) -> ExitCode {
    let command = match Command::parse(&argv) {
        Ok(command) => command,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("reagent: {message}");
            }
            println!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let Some(state) = crate::session::state_home() else {
        eprintln!("reagent: no state folder; set HOME or XDG_STATE_HOME");
        return ExitCode::from(2);
    };
    let folder = discovery::folder(&state);
    let listed = list(&folder, &state).await;
    let Some(qid) = command.qid else {
        return show(&listed, command.json);
    };
    let Some(entry) = listed.into_iter().find(|entry| entry.qid == qid) else {
        eprintln!("reagent: no parked question {qid}; `reagent answer` lists the open ones");
        return ExitCode::from(1);
    };

    let Some(answers) = read_answers().await else {
        return ExitCode::from(2);
    };
    if let Err(message) = counted(&entry, &answers) {
        eprintln!("reagent: {message}");
        return ExitCode::from(2);
    }
    let (session, cwd) = match record(&state, entry, "cli", &answers).await {
        Ok(recorded) => recorded,
        Err(refused) => {
            eprintln!("reagent: {}", why(refused));
            return ExitCode::from(1);
        }
    };
    if let Err(error) = discovery::remove_parked(&folder, &qid).await {
        eprintln!("reagent: warning: could not remove the entry of {qid}: {error}");
    }
    if command.resume
        && let Err(message) = start(&state, &cwd, &session).await
    {
        eprintln!(
            "reagent: recorded the answer, but session {session} did not start again: {message}"
        );
        return ExitCode::from(1);
    }
    println!("{session}");
    ExitCode::SUCCESS
}

/// The file in a session folder that one answer at a time holds.
const ANSWER_LOCK: &str = "answer.lock";

/// Why an answer was not written into a journal.
pub(crate) enum Refused {
    /// The session has no folder, or its journal could not be written; the text
    /// says which.
    Failed(String),
    /// A live worker holds the session; the text says so.
    Live(String),
    /// The journal no longer waits for the question: another answer came first.
    Stale,
}

/// Every parked question whose session still waits for it.
///
/// The journal is the source of truth: `list_parked` drops the entry of a
/// question a person has answered or a run has closed. Opening the journals
/// blocks, so it runs on the blocking pool with the folder.
pub(crate) async fn list(folder: &Path, state: &Path) -> Vec<discovery::Parked> {
    let state = state.to_path_buf();
    discovery::list_parked(folder, move |session, qid| still_open(&state, session, qid)).await
}

/// Whether the journal of session `id` still waits for question `qid`.
pub(crate) fn still_open(state: &Path, id: &str, qid: &str) -> bool {
    let Some(dir) = find(state, id) else {
        return false;
    };
    let Ok(session) = Session::open(&dir) else {
        return false;
    };
    resume::parked(&session).is_ok_and(|open| open.iter().any(|question| question.qid == qid))
}

/// Why an answer was not written, as one line.
fn why(refused: Refused) -> String {
    match refused {
        Refused::Failed(message) => message,
        Refused::Live(message) => message,
        Refused::Stale => "the journal of the session no longer waits for it".to_string(),
    }
}

/// The folder of session `id` under the sessions of `state`.
///
/// A session id is random, so the folder of the working directory that holds
/// one is found by looking for it, and the folder is right whatever the working
/// directory of this process is.
#[expect(clippy::disallowed_methods, reason = "runs inside spawn_blocking")]
pub(crate) fn find(state: &Path, id: &str) -> Option<PathBuf> {
    let folders = std::fs::read_dir(paths::sessions_root(state)).ok()?;
    for folder in folders.flatten() {
        let dir = folder.path().join(id);
        if dir.is_dir() {
            return Some(dir);
        }
    }
    None
}

/// Prints the listing, or the JSON array of the entries for `--json`.
fn show(listed: &[discovery::Parked], json: bool) -> ExitCode {
    if json {
        return match serde_json::to_string(listed) {
            Ok(text) => {
                println!("{text}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("reagent: the listing is not JSON: {error}");
                ExitCode::from(1)
            }
        };
    }
    if listed.is_empty() {
        println!("no parked questions");
        return ExitCode::SUCCESS;
    }
    for entry in listed {
        println!(
            "{}  {}  {}  {}",
            entry.qid,
            entry.session,
            age(entry.at),
            entry.first()
        );
    }
    ExitCode::SUCCESS
}

/// The time since a question was parked, as the listing prints it.
fn age(at: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    let seconds = now.saturating_sub(at) / 1000;
    match seconds {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

/// The answers typed on standard input, or `None` when they could not be read.
async fn read_answers() -> Option<String> {
    let mut bytes = Vec::new();
    if let Err(error) = tokio::io::stdin().read_to_end(&mut bytes).await {
        eprintln!("reagent: cannot read the answers from stdin: {error}");
        return None;
    }
    match String::from_utf8(bytes) {
        Ok(text) => Some(text.trim().to_string()),
        Err(_) => {
            eprintln!("reagent: the answers on stdin are not UTF-8");
            None
        }
    }
}

/// Checks that the answers name one list per question of `entry`, or that they
/// skip the whole request with `null`.
pub(crate) fn counted(entry: &discovery::Parked, answers: &str) -> Result<(), String> {
    let typed: Option<Vec<Vec<String>>> = serde_json::from_str(answers)
        .map_err(|error| format!("the answers are not a JSON list of lists: {error}"))?;
    let questions = entry.questions.as_array().map_or(0, Vec::len);
    match typed {
        None => Ok(()),
        Some(lists) if lists.len() == questions => Ok(()),
        Some(lists) => Err(format!(
            "{} answers for {questions} questions of {}",
            lists.len(),
            entry.qid
        )),
    }
}

/// Writes the answer of a question into the journal of its session, and
/// returns the session and the working folder the run continues in.
///
/// The journal is opened as a resume opens it: a new part that continues the
/// session, so the run that restarts it reads the answer as a record of its
/// own. A live worker keeps the session, so the answer is refused.
pub(crate) async fn record(
    state: &Path,
    entry: discovery::Parked,
    by: &str,
    answers: &str,
) -> Result<(String, String), Refused> {
    let state = state.to_path_buf();
    let by = by.to_string();
    let answers = answers.to_string();
    let recorded = tokio::task::spawn_blocking(move || {
        let Some(dir) = find(&state, &entry.session) else {
            return Err(Refused::Failed(format!("no session {}", entry.session)));
        };
        // Held from the live check to the write, so two answers of one question
        // (this command and the gateway, say) never both pass the open check.
        let _held = hold(&dir)?;
        if live(&dir) {
            return Err(Refused::Live(format!(
                "session {} is live; answer it in its client",
                entry.session
            )));
        }
        answer(&dir, &entry.qid, &by, &answers).map(|cwd| (entry.session.clone(), cwd))
    })
    .await;
    match recorded {
        Ok(recorded) => recorded,
        Err(_) => Err(Refused::Failed("the session writer stopped".to_string())),
    }
}

/// Takes the answer lock of the session at `dir`, waiting while another answer
/// holds it; the kernel drops it when the file is dropped or the process ends.
#[expect(
    clippy::disallowed_types,
    reason = "runs inside spawn_blocking; the file is only held"
)]
fn hold(dir: &Path) -> Result<std::fs::File, Refused> {
    let at = dir.join(ANSWER_LOCK);
    let failed = |error: std::io::Error| Refused::Failed(format!("{}: {error}", at.display()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&at)
        .map_err(failed)?;
    file.lock().map_err(failed)?;
    Ok(file)
}

/// Whether a live worker holds the session at `dir`.
fn live(dir: &Path) -> bool {
    let Ok(parts) = paths::part_ids(dir) else {
        return false;
    };
    parts
        .iter()
        .any(|&part| crate::session::locked(&paths::part_path(dir, part)))
}

/// Writes the answer of question `qid` as a part that continues the session at
/// `dir`, and returns the working folder of the session.
fn answer(dir: &Path, qid: &str, by: &str, answers: &str) -> Result<String, Refused> {
    let failed = |error: std::io::Error| Refused::Failed(format!("{}: {error}", dir.display()));
    let session = Session::open(dir)
        .map_err(|error| Refused::Failed(format!("{}: {error}", dir.display())))?;
    let meta = Meta::decode(&session.header().meta);
    let session_id = session.header().session_id.clone();
    let last = session
        .records()
        .filter_map(Result::ok)
        .map(|record| record.seq)
        .max()
        .unwrap_or(0);
    let fork = session.fork_point(last);
    let resumed = resume::load(&session)
        .map_err(|error| Refused::Failed(format!("{}: {error}", dir.display())))?;
    // Another answer, from the CLI or from the gateway, may have closed this
    // question since the caller read the folder: exactly one of them writes.
    if !resumed.parked.iter().any(|(_, open)| open == qid) {
        return Err(Refused::Stale);
    }
    drop(session);
    let header = Header::new(session_id, paths::now_ms(), Some(fork), meta.encode());
    let mut journal = Journal::resumed(dir, header, resumed.recorded).map_err(failed)?;
    journal
        .question_answered(qid, by, answers)
        .map_err(failed)?;
    journal.close().map_err(failed)?;
    Ok(meta.cwd)
}

/// Starts a detached worker that continues session `session` in `cwd`, with no
/// client of its own, so the run goes on headless.
///
/// It waits for the worker's ready line, as the TUI does, so a worker that
/// stops before it serves the session gives its own reason here.
pub(crate) async fn start(state: &Path, cwd: &str, session: &str) -> Result<(), String> {
    if !Path::new(cwd).is_dir() {
        return Err(format!("the working folder of the session, {cwd}, is gone"));
    }
    let flags: Vec<OsString> = ["worker", "-r", session, "--cwd", cwd]
        .into_iter()
        .map(OsString::from)
        .collect();
    // A build without the terminal UI has no worker to start.
    #[cfg(feature = "tui")]
    {
        crate::tui::start_worker(state, flags)
            .await
            .map(|_entry| ())
    }
    #[cfg(not(feature = "tui"))]
    {
        let _ = (state, flags);
        Err("this build cannot start a session".to_string())
    }
}
