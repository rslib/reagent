//! The sessions of a working folder: where runs are logged and what the
//! `session` command shows.
//!
//! Every call here reads or writes the store, so [`plan`], [`finish`] and
//! [`cat`] block and a caller runs them in `spawn_blocking`.

use std::io;
use std::path::{Path, PathBuf};

use reagent_agent::journal::Journal;
use reagent_agent::journal::kinds;
use reagent_agent::journal::record::{self, Record};
use reagent_agent::journal::resume::{self, Resumed};
use reagent_ai::Message;
use reagent_store::list::title;
use reagent_store::part::FIRST_SEQ;
use reagent_store::paths;
use reagent_store::{
    Decoder, Encoder, Entry, ForkPoint, Header, Lookup, Session, SessionList, State, Text, Value,
};
use tokio_util::bytes::Bytes;

use crate::args::Sessions;

/// The part header field that keeps the profile of a run.
const META_PROFILE: u32 = 1;
/// The part header field that keeps the provider of a run.
const META_PROVIDER: u32 = 2;
/// The part header field that keeps the model of a run.
const META_MODEL: u32 = 3;
/// The part header field that keeps the working folder of a run.
const META_CWD: u32 = 4;
/// The part header field that keeps the title of a session.
const META_TITLE: u32 = 5;
/// The part header field that keeps whether a headless run wrote the part.
const META_HEADLESS: u32 = 6;
/// The byte offset of the local time zone rules of this machine.
const LOCALTIME: &str = "/etc/localtime";
/// Where the zone files of a named zone live.
const ZONEINFO: &str = "/usr/share/zoneinfo";
/// The minute of a day, in seconds.
const MINUTE: i64 = 60;
/// The seconds of a day.
const DAY: i64 = 86_400;

/// What a part header keeps about the run that wrote it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    /// The profile the run used.
    pub profile: String,
    /// The provider the run used.
    pub provider: String,
    /// The model the run used.
    pub model: String,
    /// The working folder of the run.
    pub cwd: String,
    /// The first user message, cut.
    pub title: String,
    /// Whether a headless run wrote the part.
    pub headless: bool,
}

impl Meta {
    /// Writes the fields in the header of a part.
    pub fn encode(&self) -> Bytes {
        let mut out = Vec::with_capacity(
            self.profile.len() + self.provider.len() + self.model.len() + self.cwd.len(),
        );
        let mut enc = Encoder::new(&mut out);
        enc.str(META_PROFILE, &self.profile);
        enc.str(META_PROVIDER, &self.provider);
        enc.str(META_MODEL, &self.model);
        enc.str(META_CWD, &self.cwd);
        enc.str(META_TITLE, &self.title);
        // A run with a terminal UI writes no flag, so its part reads as one.
        if self.headless {
            enc.bool(META_HEADLESS, true);
        }
        Bytes::from(out)
    }

    /// Reads the fields back. A field of a newer build is skipped, and a
    /// missing one reads as empty.
    pub fn decode(bytes: &Bytes) -> Self {
        let mut meta = Meta::default();
        let mut decoder = Decoder::new(bytes);
        while let Ok(Some((field, value))) = decoder.next_field() {
            if field == META_HEADLESS {
                meta.headless = value.varint() == Some(1);
                continue;
            }
            let Some(text) = text_of(&value) else {
                continue;
            };
            match field {
                META_PROFILE => meta.profile = text,
                META_PROVIDER => meta.provider = text,
                META_MODEL => meta.model = text,
                META_CWD => meta.cwd = text,
                META_TITLE => meta.title = text,
                _ => {}
            }
        }
        meta
    }

    /// The entry a rebuilt list shows for a part.
    fn entry(&self, header: &Header) -> Entry {
        Entry {
            session: header.session_id.clone(),
            updated_ms: header.created_ms,
            turns: 0,
            title: self.title.clone(),
            profile: self.profile.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            part: header.part_id,
            state: State::Interrupted,
            headless: self.headless,
        }
    }
}

/// The text of a length-delimited field, if it is UTF-8.
fn text_of(value: &Value) -> Option<String> {
    let bytes = value.bytes()?.clone();
    let text = Text::new(bytes).ok()?;
    Some(text.as_str().to_owned())
}

/// Where a run writes its journal and what it continues from.
pub struct Plan {
    /// The folder of the cwd that lists this session.
    pub folder: PathBuf,
    /// The folder of the session the new part goes in.
    pub dir: PathBuf,
    /// The session id of the new part.
    pub id: String,
    /// The header of the new part, its fork point included.
    pub header: Header,
    /// The run this part continues, for a resume or a fork.
    pub resumed: Option<Resumed>,
}

impl Plan {
    /// The meta of the part.
    pub fn meta(&self) -> Meta {
        Meta::decode(&self.header.meta)
    }
}

/// What a run asks the store for.
pub struct Ask<'a> {
    /// What the command line chose.
    pub sessions: &'a Sessions,
    /// The working folder, absolute.
    pub cwd: &'a Path,
    /// The profile of the run, if one was given.
    pub profile: Option<&'a str>,
    /// The provider of the run.
    pub provider: &'a str,
    /// The model of the run.
    pub model: &'a str,
    /// The task text, which titles a new session.
    pub title: &'a str,
    /// Whether the session is served by a worker, which may resume a session
    /// with nothing to continue.
    pub worker: bool,
    /// Whether a headless run writes the part: a task and no terminal UI.
    pub headless: bool,
}

/// Runs the `session` command: `ls`, `cat <ref>`, `rm <ref>` or `gc`.
pub async fn command(argv: Vec<std::ffi::OsString>) -> std::process::ExitCode {
    let words: Vec<String> = argv
        .into_iter()
        .map(|word| word.to_string_lossy().into_owned())
        .collect();
    let cwd = match tokio::fs::canonicalize(std::path::Path::new(".")).await {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("reagent: {error}");
            return std::process::ExitCode::from(1);
        }
    };
    let (verb, reference) = match words.as_slice() {
        [verb] => (verb.as_str(), None),
        [verb, reference] => (verb.as_str(), Some(reference.as_str())),
        [verb, ..] => (verb.as_str(), None),
        [] => ("ls", None),
    };
    let outcome = match verb {
        "ls" => tokio::task::spawn_blocking(move || list(&cwd))
            .await
            .map_err(|_| "the session store failed".to_string())
            .and_then(|list| list)
            .map(|entries| print_list(&entries)),
        "cat" | "rm" => {
            let Some(reference) = reference else {
                eprintln!("reagent: session {verb} needs a session id or folder");
                return std::process::ExitCode::from(2);
            };
            let reference = reference.to_owned();
            let (folder, dir) =
                match tokio::task::spawn_blocking(move || locate(&cwd, &reference)).await {
                    Ok(Ok(found)) => found,
                    Ok(Err(message)) => {
                        eprintln!("reagent: {message}");
                        return std::process::ExitCode::from(1);
                    }
                    Err(_) => {
                        eprintln!("reagent: the session store failed");
                        return std::process::ExitCode::from(1);
                    }
                };
            if verb == "cat" {
                tokio::task::spawn_blocking(move || cat(&dir))
                    .await
                    .map_err(|_| "the session store failed".to_string())
                    .and_then(|lines| lines)
                    .map(|lines| {
                        let mut out = String::new();
                        for line in lines {
                            out.push_str(&line);
                            out.push('\n');
                        }
                        print!("{out}");
                    })
            } else {
                tokio::task::spawn_blocking(move || remove(&folder, &dir))
                    .await
                    .map_err(|_| "the session store failed".to_string())
                    .and_then(|removed| removed)
            }
        }
        "gc" => tokio::task::spawn_blocking(move || gc(&cwd))
            .await
            .map_err(|_| "the session store failed".to_string())
            .and_then(|line| line)
            .map(|line| println!("{line}")),
        other => {
            eprintln!("reagent: unknown session command: {other}");
            return std::process::ExitCode::from(2);
        }
    };
    match outcome {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("reagent: {message}");
            std::process::ExitCode::from(1)
        }
    }
}

/// Runs a sweep of the saved sessions at once, as `session gc` does.
///
/// Walking and removing the store blocks, so a caller runs this in
/// `spawn_blocking`.
pub fn gc(cwd: &Path) -> Result<String, String> {
    let limits = crate::args::session_limits(cwd)?;
    let root = crate::gc::root().ok_or_else(|| "no state folder".to_string())?;
    let freed = crate::gc::sweep(&root, &[], crate::gc::Limits::from(limits))?;
    Ok(format!(
        "deleted {} sessions, {} bytes",
        freed.sessions, freed.bytes
    ))
}

/// The root of the state folder, or `None` when the machine names none.
pub(crate) fn state_home() -> Option<PathBuf> {
    reagent_config::auth::dir()
        .ok()
        .and_then(|dir| dir.parent().map(Path::to_path_buf))
}

/// The folder of the sessions of `cwd`, or `None` when sessions are off.
fn folder_of(cwd: &Path) -> Option<PathBuf> {
    Some(paths::sessions_root(&state_home()?).join(paths::cwd_key(cwd)))
}

/// Reads the sessions of a folder, newest first.
///
/// Printing blocks, so a caller runs this in `spawn_blocking`.
pub fn list(cwd: &Path) -> Result<Vec<Entry>, String> {
    let Some(folder) = folder_of(cwd) else {
        return Ok(Vec::new());
    };
    // A folder no run has saved a session in yet has no sessions.
    if !std::fs::exists(&folder).map_err(|error| error.to_string())? {
        return Ok(Vec::new());
    }
    let list = SessionList::open(&folder, entry_of).map_err(|error| error.to_string())?;
    Ok(list.all().to_vec())
}

/// The entry a rebuilt list shows for one part.
pub(crate) fn entry_of(header: &Header, _commit: Option<&reagent_store::part::Commit>) -> Entry {
    Meta::decode(&header.meta).entry(header)
}

/// Resolves what a run does with the store.
///
/// Returns `None` when the run writes no session at all.
pub fn plan(ask: &Ask<'_>) -> Result<Option<Plan>, String> {
    if matches!(ask.sessions, Sessions::None) {
        return Ok(None);
    }
    let Some(folder) = folder_of(ask.cwd) else {
        eprintln!("reagent: warning: no state folder; this run is not saved");
        return Ok(None);
    };
    match ask.sessions {
        Sessions::None => Ok(None),
        Sessions::New => Ok(Some(create(&folder, ask, None, None)?)),
        Sessions::Continue => {
            let dir = continued(&folder, ask)?;
            resume_into(&folder, &dir, ask)
        }
        Sessions::Resume(Some(reference)) => {
            let dir = resolve(&folder, reference)?;
            resume_into(&folder, &dir, ask)
        }
        // A resume with no reference only lists; the caller has done that.
        Sessions::Resume(None) => Ok(None),
        Sessions::Fork { reference, turn } => {
            let parent = resolve(&folder, reference)?;
            let session = Session::open(&parent).map_err(|error| error.to_string())?;
            let (resumed, seq) = match turn {
                Some(turn) => resume::load_to(&session, *turn)
                    .map_err(|error| format!("session {reference}: {error}"))?,
                None => resume::load_tail(&session)
                    .map_err(|error| format!("session {reference}: {error}"))?,
            };
            let parent_meta = Meta::decode(&session.header().meta);
            let fork = Some(session.fork_point(seq));
            Ok(Some(create(
                &folder,
                ask,
                fork,
                Some((resumed, parent_meta)),
            )?))
        }
    }
}

/// The provider and model the session a run continues ran with.
pub struct Stored {
    /// The config provider id the session last switched to, or the provider
    /// name the newest part was planned with.
    pub provider: String,
    /// The model the session last switched to, or the model the newest part
    /// was planned with.
    pub model: String,
}

/// The provider and model of the session a run continues.
///
/// A switch the journal recorded names the config provider id it moved to,
/// which the header of the part does not, so the last switch wins and the
/// header answers only for a journal that holds none.
/// A run reads this before it plans the new part, so the header and the list
/// entry of that part name the provider and model the run actually uses.
/// Reading a session blocks, so a caller runs this in `spawn_blocking`.
pub fn stored(ask: &Ask<'_>) -> Result<Option<Stored>, String> {
    let Some(folder) = folder_of(ask.cwd) else {
        return Ok(None);
    };
    // A run that starts a session, or one that names no session it can reach,
    // takes nothing: `plan` reports what the reference got wrong.
    let dir = match ask.sessions {
        Sessions::Continue => continued(&folder, ask).ok(),
        Sessions::Resume(Some(reference)) => resolve(&folder, reference).ok(),
        _ => None,
    };
    let Some(dir) = dir else {
        return Ok(None);
    };
    let session = Session::open(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let switched = switched(&session).map_err(|error| format!("{}: {error}", dir.display()))?;
    let meta = Meta::decode(&session.header().meta);
    Ok(Some(match switched {
        Some((provider, model)) => Stored { provider, model },
        None => Stored {
            provider: meta.provider,
            model: meta.model,
        },
    }))
}

/// The config provider id and model of the last switch of `session`, from the
/// records a resume continues from.
fn switched(session: &Session) -> io::Result<Option<(String, String)>> {
    let from = session.last_base().unwrap_or(FIRST_SEQ);
    let mut found = None;
    for record in session.records_from(from) {
        let record = record?;
        if record.kind != kinds::MODEL {
            continue;
        }
        // A record of a build before the config provider id carries none, and
        // names no provider a resume could build.
        if let Record::Model(change) = record::decode(record.kind, &record.payload)?
            && let Some(config) = change.config
        {
            found = Some((config.to_string(), change.model));
        }
    }
    Ok(found)
}

/// The folder of the session a continue takes: the newest of the folder, or
/// of the profile when one was given.
fn continued(folder: &Path, ask: &Ask<'_>) -> Result<PathBuf, String> {
    let list = if std::fs::exists(folder).map_err(|error| error.to_string())? {
        Some(SessionList::open(folder, entry_of).map_err(|error| error.to_string())?)
    } else {
        None
    };
    let found = list.as_ref().and_then(|list| match ask.profile {
        Some(profile) => list.newest_of(profile),
        None => list.newest(),
    });
    match found {
        Some(entry) => Ok(folder.join(&entry.session)),
        None => Err(match ask.profile {
            Some(profile) => format!("no session to continue for profile {profile}"),
            None => "no session to continue in this folder".into(),
        }),
    }
}

/// Starts a new part in a new session folder.
fn create(
    folder: &Path,
    ask: &Ask<'_>,
    fork: Option<ForkPoint>,
    parent: Option<(Resumed, Meta)>,
) -> Result<Plan, String> {
    let id = paths::new_session_id();
    let dir = folder.join(&id);
    let (resumed, title) = match parent {
        Some((resumed, meta)) if !meta.title.is_empty() => (Some(resumed), meta.title),
        Some((resumed, _)) => (Some(resumed), title(ask.title)),
        None => (None, title(ask.title)),
    };
    let meta = Meta {
        profile: ask.profile.unwrap_or_default().to_owned(),
        provider: ask.provider.to_owned(),
        model: ask.model.to_owned(),
        cwd: ask.cwd.to_string_lossy().into_owned(),
        title,
        headless: ask.headless,
    };
    let header = Header::new(id.clone(), paths::now_ms(), fork, meta.encode());
    Ok(Plan {
        folder: folder.to_path_buf(),
        dir,
        id,
        header,
        resumed,
    })
}

/// Reads a session folder and plans a new part in it.
fn resume_into(folder: &Path, dir: &Path, ask: &Ask<'_>) -> Result<Option<Plan>, String> {
    let session = Session::open(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let (resumed, seq) =
        resume::load_tail(&session).map_err(|error| format!("{}: {error}", dir.display()))?;
    // A run with no open turn and no message has nothing to do, unless it is
    // a worker that serves the session to its clients.
    if !resumed.open_turn && ask.title.is_empty() && !ask.worker {
        return Err("nothing to continue; give a message with -p".into());
    }
    let parent = Meta::decode(&session.header().meta);
    let id = session.header().session_id.clone();
    let meta = Meta {
        profile: ask
            .profile
            .map(str::to_owned)
            .filter(|profile| !profile.is_empty())
            .unwrap_or(parent.profile),
        provider: ask.provider.to_owned(),
        model: ask.model.to_owned(),
        cwd: ask.cwd.to_string_lossy().into_owned(),
        title: if parent.title.is_empty() {
            title(ask.title)
        } else {
            parent.title
        },
        headless: ask.headless,
    };
    let header = Header::new(
        id.clone(),
        paths::now_ms(),
        Some(session.fork_point(seq)),
        meta.encode(),
    );
    Ok(Some(Plan {
        folder: folder.to_path_buf(),
        dir: dir.to_path_buf(),
        id,
        header,
        resumed: Some(resumed),
    }))
}

/// The folder of the session `reference` names: an id prefix first, then a
/// path inside the sessions of this folder.
fn resolve(folder: &Path, reference: &str) -> Result<PathBuf, String> {
    // An id prefix wins over a path, so a folder named like a prefix still
    // picks the session it names.
    if std::fs::exists(folder).map_err(|error| error.to_string())? {
        let list = SessionList::open(folder, entry_of).map_err(|error| error.to_string())?;
        match list.by_prefix(reference) {
            Lookup::One(entry) => return Ok(folder.join(&entry.session)),
            Lookup::Many(entries) => {
                let names = entries
                    .iter()
                    .map(|entry| entry.session.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!("several sessions match {reference}: {names}"));
            }
            Lookup::None => {}
        }
    }
    inside(folder, reference)
}

/// The folder a path names, when it is a session of `folder`.
///
/// A session folder is a direct child of the sessions folder of the working
/// folder it was made in, so a path cannot name the session of another folder.
#[expect(
    clippy::disallowed_methods,
    reason = "runs inside spawn_blocking, once per reference"
)]
fn inside(folder: &Path, reference: &str) -> Result<PathBuf, String> {
    let not_here = || format!("not a session of this folder: {reference}");
    if !Path::new(reference).is_dir() {
        return Err(format!("no session matches {reference}"));
    }
    // The real folders, so a `..` or a link cannot leave the sessions folder.
    let dir = std::fs::canonicalize(reference).map_err(|error| error.to_string())?;
    let Ok(folder) = std::fs::canonicalize(folder) else {
        return Err(not_here());
    };
    if dir.parent() != Some(folder.as_path()) {
        return Err(not_here());
    }
    Ok(dir)
}

/// What a worker records when its session commits a record: the entry to
/// write, and the turns the session carried before this run.
pub struct Listing {
    /// The session the worker serves.
    pub plan: Plan,
    /// The turns the session had before this run.
    pub turns: u64,
}

/// Records an entry for the part before its first request.
pub fn started(plan: &Plan, turns: u64) -> io::Result<()> {
    let entry = entry(plan, State::Open, turns);
    SessionList::open(&plan.folder, entry_of)?.record(entry)
}

/// Records how a part ended.
pub fn finish(plan: &Plan, state: State, turns: u64) -> io::Result<()> {
    let entry = entry(plan, state, turns);
    SessionList::open(&plan.folder, entry_of)?.record(entry)
}

/// The list entry of a part.
fn entry(plan: &Plan, state: State, turns: u64) -> Entry {
    let meta = plan.meta();
    Entry {
        session: plan.id.clone(),
        updated_ms: paths::now_ms(),
        turns,
        title: meta.title,
        profile: meta.profile,
        provider: meta.provider,
        model: meta.model,
        part: plan.header.part_id,
        state,
        headless: meta.headless,
    }
}

/// Reads every record of a session, in log order, as JSON lines.
///
/// The first line is the header of the part the session ends in, then one
/// line per record the chain shows.
pub fn cat(dir: &Path) -> Result<Vec<String>, String> {
    let session = Session::open(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let mut lines = vec![header_line(session.header())?];
    for record in session.records() {
        let record = record.map_err(|error| error.to_string())?;
        lines.push(record_line(&record)?);
    }
    Ok(lines)
}

/// The header line of a part.
fn header_line(header: &Header) -> Result<String, String> {
    let meta = Meta::decode(&header.meta);
    let fork = match &header.fork {
        Some(fork) => serde_json::json!({
            "part": fork.part.to_string(),
            "seq": fork.seq,
        }),
        None => serde_json::Value::Null,
    };
    let line = serde_json::json!({
        "type": "header",
        "session": header.session_id,
        "part": header.part_id.to_string(),
        "fork": fork,
        "profile": meta.profile,
        "provider": meta.provider,
        "model": meta.model,
        "cwd": meta.cwd,
        "title": meta.title,
    });
    serde_json::to_string(&line).map_err(|error| error.to_string())
}

/// The line of one record.
fn record_line(part: &reagent_store::Record) -> Result<String, String> {
    let decoded = record::decode(part.kind, &part.payload).map_err(|error| error.to_string())?;
    let mut line = match decoded {
        Record::Message(message) => message_line(&message)?,
        Record::CallStart(call) => serde_json::json!({
            "type": "call_start",
            "id": call.id,
            "name": call.name,
            "class": class_name(call.class),
        }),
        Record::TurnEnd(end) => serde_json::json!({
            "type": "turn_end",
            "turn": end.turn,
            "usage": serde_json::to_value(end.usage).map_err(|error| error.to_string())?,
        }),
        Record::Base(state) => serde_json::json!({
            "type": "base",
            "provider": state.provider,
            "model": state.model,
            "compaction": state.compaction,
            "touched": state.touched,
        }),
        Record::Model(change) => serde_json::json!({
            "type": "model",
            "provider": change.provider,
            "model": change.model,
        }),
        Record::Unknown(kind) => serde_json::json!({
            "type": "unknown",
            "kind": kind,
        }),
    };
    if let Some(map) = line.as_object_mut() {
        map.insert("seq".into(), serde_json::json!(part.seq));
    }
    serde_json::to_string(&line).map_err(|error| error.to_string())
}

/// A message record: the message's own fields, with `type` first.
fn message_line(message: &Message) -> Result<serde_json::Value, String> {
    let value = serde_json::to_value(message).map_err(|error| error.to_string())?;
    let mut line = serde_json::json!({ "type": "message" });
    if let (Some(line), serde_json::Value::Object(fields)) = (line.as_object_mut(), value) {
        line.extend(fields);
    }
    Ok(line)
}

/// The name of an effect class.
fn class_name(class: reagent_agent::tool::EffectClass) -> &'static str {
    match class {
        reagent_agent::tool::EffectClass::Read => "read",
        reagent_agent::tool::EffectClass::LocalReversible => "local_reversible",
        reagent_agent::tool::EffectClass::LocalIrreversible => "local_irreversible",
        reagent_agent::tool::EffectClass::External => "external",
    }
}

/// The session folder `reference` names, for a command that changes it.
pub fn locate(cwd: &Path, reference: &str) -> Result<(PathBuf, PathBuf), String> {
    let folder = folder_of(cwd).ok_or_else(|| "no state folder".to_string())?;
    let dir = resolve(&folder, reference)?;
    Ok((folder, dir))
}

/// Removes a session, unless a writer holds its newest part.
///
/// Taking the lock blocks, so a caller runs this in `spawn_blocking`.
#[expect(
    clippy::disallowed_methods,
    reason = "runs inside spawn_blocking, once per remove"
)]
pub fn remove(folder: &Path, dir: &Path) -> Result<(), String> {
    let session = Session::open(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let id = session.header().session_id.clone();
    // Part ids are random, so any part may be the one a writer holds.
    let parts = paths::part_ids(dir).map_err(|error| error.to_string())?;
    if parts
        .iter()
        .any(|&part| locked(&paths::part_path(dir, part)))
    {
        return Err(format!("session {id} is in use"));
    }
    std::fs::remove_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    SessionList::open(folder, entry_of)
        .and_then(|mut list| list.remove(&id))
        .map_err(|error| error.to_string())
}

/// Whether another writer holds the part file at `path`.
#[expect(
    clippy::disallowed_types,
    reason = "runs inside spawn_blocking, once per remove or sweep"
)]
pub(crate) fn locked(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// The local time of `ms` as `2026-10-27 14:03`, or the UTC time when the
/// zone rules of this machine cannot be read.
pub fn stamp(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let offset = local_offset(secs).unwrap_or(0);
    let zone = secs + offset;
    let days = zone.div_euclid(DAY);
    let rest = zone.rem_euclid(DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rest / 3600,
        (rest % 3600) / MINUTE
    )
}

/// The offset of the local zone from UTC at `secs`, when it can be read.
#[expect(
    clippy::disallowed_methods,
    reason = "runs inside spawn_blocking, for one line of ls"
)]
fn local_offset(secs: i64) -> Option<i64> {
    let named = std::env::var("TZ").ok();
    let path = match named.as_deref().filter(|zone| !zone.is_empty()) {
        Some(zone) if zone.starts_with('/') => PathBuf::from(zone),
        Some(zone) => Path::new(ZONEINFO).join(zone.trim_start_matches(':')),
        None => PathBuf::from(LOCALTIME),
    };
    offset_from_tzif(&std::fs::read(path).ok()?, secs)
}

/// Reads the offset of a TZif file at `secs`.
fn offset_from_tzif(bytes: &[u8], secs: i64) -> Option<i64> {
    let header = |at: usize| -> Option<(u32, u32, u32, u32, u32, u32)> {
        let field = |index: usize| -> Option<u32> {
            let at = at + 20 + index * 4;
            let bytes = bytes.get(at..at + 4)?;
            Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        };
        Some((
            field(0)?,
            field(1)?,
            field(3)?,
            field(4)?,
            field(5)?,
            field(2)?,
        ))
    };
    if bytes.get(0..4)? != b"TZif" {
        return None;
    }
    let version = *bytes.get(4)?;
    // A version 2 or later file keeps its 64-bit times in a second block.
    let mut at = 0;
    let mut wide = false;
    if version >= b'2' {
        let (isutcnt, isstdcnt, timecnt, typecnt, charcnt, leapcnt) = header(0)?;
        let size = (timecnt as usize) * 5
            + (typecnt as usize) * 6
            + charcnt as usize
            + (leapcnt as usize) * 8
            + isstdcnt as usize
            + isutcnt as usize;
        at = 44 + size;
        if bytes.get(at..at + 4)? != b"TZif" {
            return None;
        }
        wide = true;
    }
    let (_isutcnt, _isstdcnt, timecnt, typecnt, _charcnt, _leapcnt) = header(at)?;
    let mut at = at + 44;
    let step = if wide { 8 } else { 4 };
    let count = timecnt as usize;
    let times = bytes.get(at..at + count * step)?;
    let transitions: Vec<i64> = (0..count)
        .filter_map(|index| {
            let bytes = times.get(index * step..(index + 1) * step)?;
            Some(if wide {
                let mut wide = [0u8; 8];
                wide.copy_from_slice(bytes);
                i64::from_be_bytes(wide)
            } else {
                let mut narrow = [0u8; 4];
                narrow.copy_from_slice(bytes);
                i64::from(i32::from_be_bytes(narrow))
            })
        })
        .collect();
    at += count * step;
    let indices = bytes.get(at..at + count)?;
    at += count;
    let types = bytes.get(at..at + (typecnt as usize) * 6)?;
    let offsets: Vec<i64> = (0..typecnt as usize)
        .filter_map(|index| {
            let bytes = types.get(index * 6..index * 6 + 6)?;
            Some(i64::from(i32::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3],
            ])))
        })
        .collect();
    let which = transitions
        .iter()
        .rposition(|time| *time <= secs)
        .and_then(|index| indices.get(index))
        .map(|index| usize::from(*index))?;
    offsets.get(which).copied()
}

/// The year, month and day of a count of days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_fraction = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_fraction + 2) / 5 + 1;
    let month = if month_fraction < 10 {
        month_fraction + 3
    } else {
        month_fraction - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// One session of a run, as the `ls` command prints it.
pub fn ls_line(entry: &Entry) -> String {
    let noun = if entry.turns == 1 { "turn" } else { "turns" };
    format!(
        "{}  {}  {} {noun}  {}",
        entry.session,
        stamp(entry.updated_ms),
        entry.turns,
        entry.title
    )
}

/// Prints the sessions of a working folder, newest first.
pub fn print_list(entries: &[Entry]) {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&ls_line(entry));
        out.push('\n');
    }
    print!("{out}");
}

/// The state a run ended in, as a list entry keeps it.
pub fn state_of(done: bool) -> State {
    if done {
        State::Done
    } else {
        State::Interrupted
    }
}

/// Closes a part's journal and records how the run ended.
///
/// Closing blocks on a sync, so a caller runs this in `spawn_blocking`.
pub fn close(journal: Journal, plan: &Plan, state: State, turns: u64) -> Result<(), String> {
    journal.close().map_err(|error| error.to_string())?;
    finish(plan, state, turns).map_err(|error| error.to_string())
}

/// Closes a part's journal and takes what the run added back out of the store,
/// for a worker session that committed no record: no list names it, and a
/// session it resumed keeps its folder, its earlier parts and its entry.
///
/// Closing blocks on a sync and the removal on the disk, so a caller runs this
/// in `spawn_blocking`.
#[expect(
    clippy::disallowed_methods,
    reason = "runs inside spawn_blocking, once per discarded session"
)]
pub fn discard(journal: Journal, plan: &Plan) -> Result<(), String> {
    let part = paths::part_path(&plan.dir, journal.part_id());
    journal.close().map_err(|error| error.to_string())?;
    std::fs::remove_file(&part).map_err(|error| format!("{}: {error}", part.display()))?;
    // A part that was the folder's only one leaves no session behind.
    let left =
        paths::part_ids(&plan.dir).map_err(|error| format!("{}: {error}", plan.dir.display()))?;
    if left.is_empty() {
        std::fs::remove_dir(&plan.dir)
            .map_err(|error| format!("{}: {error}", plan.dir.display()))?;
    }
    Ok(())
}

/// How many turns a session has before a run adds its own.
pub fn carried(resumed: Option<&Resumed>) -> u64 {
    resumed.map_or(0, |resumed| resumed.turns)
}
