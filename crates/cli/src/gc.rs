//! Deleting the saved sessions that are too old or too big.
//!
//! Every call here walks, reads and removes session folders, so a caller runs
//! it in `spawn_blocking`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use reagent_config::SessionSettings;
use reagent_store::Part;
use reagent_store::SessionList;
use reagent_store::paths;

/// Milliseconds of a day.
const DAY_MS: u64 = 86_400_000;
/// The stamp file that says when a run last swept the sessions.
pub const STAMP_FILE: &str = "gc.stamp";
/// How long a stamp holds off the next sweep of a run.
const SWEEP_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// Bytes of a MiB.
const MIB: u64 = 1024 * 1024;

/// What one sweep deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Freed {
    /// Sessions removed.
    pub sessions: u64,
    /// Bytes their part files took.
    pub bytes: u64,
}

/// The limits a sweep works to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Days a session is kept after its last write; 0 keeps every age.
    pub keep_days: u64,
    /// MiB the sessions of every folder may take; 0 means no cap.
    pub max_mb: u64,
}

impl From<SessionSettings> for Limits {
    fn from(settings: SessionSettings) -> Self {
        Limits {
            keep_days: settings.keep_days(),
            max_mb: settings.max_mb(),
        }
    }
}

/// One session folder as a sweep sees it.
#[derive(Debug)]
struct Found {
    /// The folder of the working folder that lists the session.
    folder: PathBuf,
    /// The session folder itself.
    dir: PathBuf,
    /// The session id its newest part names.
    id: String,
    /// Bytes its part files take.
    bytes: u64,
    /// The modification time of the part file written last, in ms.
    written: u64,
    /// Whether a writer holds its newest part.
    locked: bool,
    /// The part ids it owns.
    parts: Vec<u64>,
    /// The part ids it reads before its own records.
    forks: Vec<u64>,
}

/// The folder that holds the session folder of every working folder.
pub fn root() -> Option<PathBuf> {
    Some(paths::sessions_root(&crate::session::state_home()?))
}

/// Runs one sweep under `root`: the sessions `keep` names and the ones a
/// writer holds always stay, then `limits` decides.
///
/// Walking the store blocks, so a caller runs this in `spawn_blocking`.
pub fn sweep(root: &Path, keep: &[PathBuf], limits: Limits) -> Result<Freed, String> {
    let mut found = scan(root)?;
    let mut freed = Freed::default();
    // The folders a session was taken from: one left with no session goes.
    let mut emptied: Vec<PathBuf> = Vec::new();
    // Age: a session nothing holds goes once its newest part is older than
    // the limit, oldest first.
    if limits.keep_days != 0 {
        let limit = limits.keep_days.saturating_mul(DAY_MS);
        let now = paths::now_ms();
        let old = |session: &Found| now.saturating_sub(session.written) > limit;
        while let Some(at) = victim(&found, keep, old) {
            let session = found.remove(at);
            emptied.push(session.folder.clone());
            remove(&session, &mut freed)?;
        }
    }
    // Size: the least recently written sessions go until the rest fit.
    let cap = limits.max_mb.saturating_mul(MIB);
    if cap != 0 {
        let mut total: u64 = found.iter().map(|session| session.bytes).sum();
        while total > cap {
            let Some(at) = victim(&found, keep, |_| true) else {
                break;
            };
            let session = found.remove(at);
            total = total.saturating_sub(session.bytes);
            emptied.push(session.folder.clone());
            remove(&session, &mut freed)?;
        }
    }
    prune(&emptied)?;
    Ok(freed)
}

/// Runs a sweep after a saved run, unless one ran in the last day.
///
/// The stamp is touched before the sweep, so a second run that ends at the
/// same moment finds it fresh and does not sweep again. Walking the store
/// blocks, so a caller runs this in `spawn_blocking`.
pub fn after_run(keep: &Path, limits: Limits) -> Result<Option<Freed>, String> {
    let Some(root) = root() else {
        return Ok(None);
    };
    if !due(&root)? {
        return Ok(None);
    }
    sweep(&root, &[keep.to_path_buf()], limits).map(Some)
}

/// Whether a sweep is due, touching the stamp first when it is.
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "runs inside spawn_blocking, once per saved run"
)]
fn due(root: &Path) -> Result<bool, String> {
    let path = root.join(STAMP_FILE);
    let fresh = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|stamped| SystemTime::now().duration_since(stamped).ok())
        .is_some_and(|age| age < SWEEP_EVERY);
    if fresh {
        return Ok(false);
    }
    std::fs::create_dir_all(root).map_err(|error| format!("{}: {error}", root.display()))?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.set_modified(SystemTime::now())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(true)
}

/// Every session folder under `root`.
fn scan(root: &Path) -> Result<Vec<Found>, String> {
    let mut found = Vec::new();
    for folder in folders(root)? {
        let dirs = paths::session_dirs(&folder).map_err(|error| named(&folder, error))?;
        for dir in dirs {
            match inspect(&folder, &dir) {
                Ok(session) => found.push(session),
                // Another sweep or a `session rm` took it first.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(named(&dir, error)),
            }
        }
    }
    Ok(found)
}

/// The folders of the working folders under `root`.
#[expect(
    clippy::disallowed_methods,
    reason = "runs inside spawn_blocking, one folder listing per sweep and prune"
)]
fn folders(root: &Path) -> Result<Vec<PathBuf>, String> {
    let names = match std::fs::read_dir(root) {
        Ok(names) => names,
        // No run ever saved a session on this machine.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(named(root, error)),
    };
    let mut folders = Vec::new();
    for entry in names {
        let entry = entry.map_err(|error| named(root, error))?;
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            folders.push(entry.path());
        }
    }
    folders.sort();
    Ok(folders)
}

/// What the sweep needs to know about the session in `dir`.
#[expect(
    clippy::disallowed_methods,
    reason = "runs inside spawn_blocking, once per session of a sweep"
)]
fn inspect(folder: &Path, dir: &Path) -> std::io::Result<Found> {
    let mut found = Found {
        folder: folder.to_path_buf(),
        dir: dir.to_path_buf(),
        id: name_of(dir),
        bytes: 0,
        written: 0,
        locked: false,
        parts: Vec::new(),
        forks: Vec::new(),
    };
    for part in paths::part_ids(dir)? {
        let path = paths::part_path(dir, part);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        found.bytes += meta.len();
        if let Ok(modified) = meta.modified() {
            found.written = found.written.max(ms(modified));
        }
        // Part ids are random, so any part may be the one a writer holds,
        // and a writer may hold a part whose header is not on the disk yet.
        found.locked |= crate::session::locked(&path);
        // A run that died before its header was written leaves a part with no
        // header, which says nothing about the session.
        if let Ok(part) = Part::open(&path) {
            found.id = part.header().session_id.clone();
            found.parts.push(part.part_id());
            if let Some(fork) = part.header().fork {
                found.forks.push(fork.part);
            }
        }
    }
    // A continued run points its part at an older part of the same session;
    // only a part of another session makes that session a fork parent.
    let own = found.parts.clone();
    found.forks.retain(|part| !own.contains(part));
    if found.written == 0
        && let Ok(modified) = std::fs::metadata(dir).and_then(|meta| meta.modified())
    {
        found.written = ms(modified);
    }
    Ok(found)
}

/// The session to delete next: the least recently written one that `eligible`
/// allows and that no session staying behind forks from.
fn victim(found: &[Found], keep: &[PathBuf], eligible: impl Fn(&Found) -> bool) -> Option<usize> {
    // Every part a session reads from another session: the session that owns
    // one of them is a fork parent, and deleting it would break the fork.
    let needed: HashSet<u64> = found
        .iter()
        .flat_map(|session| session.forks.iter().copied())
        .collect();
    found
        .iter()
        .enumerate()
        .filter(|(_, session)| !keep.contains(&session.dir))
        .filter(|(_, session)| !session.locked)
        .filter(|(_, session)| eligible(session))
        .filter(|(_, session)| !session.parts.iter().any(|part| needed.contains(part)))
        .min_by_key(|(_, session)| session.written)
        .map(|(at, _)| at)
}

/// Removes one session folder and writes the tombstone of its list.
#[expect(
    clippy::disallowed_methods,
    reason = "runs inside spawn_blocking, once per removed session"
)]
fn remove(session: &Found, freed: &mut Freed) -> Result<(), String> {
    match std::fs::remove_dir_all(&session.dir) {
        Ok(()) => {}
        // Another sweep or a `session rm` took it first.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(named(&session.dir, error)),
    }
    let mut list = SessionList::open(&session.folder, crate::session::entry_of)
        .map_err(|error| named(&session.folder, error))?;
    list.remove(&session.id)
        .map_err(|error| named(&session.folder, error))?;
    freed.sessions += 1;
    freed.bytes += session.bytes;
    Ok(())
}

/// Removes the folder of a working folder, with its list, once the sweep took
/// its last session out of it.
#[expect(
    clippy::disallowed_methods,
    reason = "runs inside spawn_blocking, once per sweep"
)]
fn prune(emptied: &[PathBuf]) -> Result<(), String> {
    let mut folders: Vec<&Path> = emptied.iter().map(PathBuf::as_path).collect();
    folders.sort_unstable();
    folders.dedup();
    for folder in folders {
        let empty = paths::session_dirs(folder)
            .map_err(|error| named(folder, error))?
            .is_empty();
        if empty {
            // Only the list and then the empty folder: a run that makes a new
            // session here at the same moment makes the folder removal fail,
            // and its session stays.
            match std::fs::remove_file(paths::list_path(folder)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(named(folder, error)),
            }
            let _ = std::fs::remove_dir(folder);
        }
    }
    Ok(())
}

/// The name of a folder, as a session id reads it.
fn name_of(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Milliseconds since the Unix epoch of `time`.
fn ms(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// An error that names the file it happened on.
fn named(path: &Path, error: std::io::Error) -> String {
    format!("{}: {error}", path.display())
}
