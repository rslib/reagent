//! The discovery folder: one entry file per running worker.

use std::ffi::OsStr;
use std::fmt;
use std::fs::Permissions;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::{Pid, getuid};
use serde::Deserialize;
use serde::Serialize;
use tokio::fs;

use crate::message::MAJOR;

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// The longest socket path this platform accepts, without its NUL.
pub const MAX_SOCKET_PATH: usize = if cfg!(target_os = "linux") { 107 } else { 103 };

/// `<state>/reagent/workers/v<MAJOR>`, where a worker writes its entry.
pub fn folder(state: &Path) -> PathBuf {
    state
        .join("reagent")
        .join("workers")
        .join(format!("v{MAJOR}"))
}

/// What the run of a worker is doing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// No run is open.
    #[default]
    Idle,
    /// A run is open and needs nobody.
    Working,
    /// A run waits for a person to answer a request.
    Blocked,
}

/// The request a run waits for a person to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitKind {
    /// An approval of a tool call.
    Approval,
    /// An answer to the questions of an `ask` call.
    Question,
}

/// The request of a run that waits for a person.
///
/// The ids of the approvals and of the questions are counted apart, so the kind
/// is what names the request with the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wait {
    pub kind: WaitKind,
    pub id: u64,
}

/// One running worker, as a client finds it. `Debug` hides the token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub pid: u32,
    pub instance: String,
    pub major: u32,
    pub token: String,
    pub socket: PathBuf,
    pub session: String,
    pub cwd: PathBuf,
    pub started_ms: u64,
    /// True while the worker's run waits for an approval.
    #[serde(default)]
    pub waiting: bool,
    /// What the run is doing, which an entry written before this field reads
    /// as idle.
    #[serde(default)]
    pub state: State,
    /// The request that waits for a person, when one waits.
    #[serde(default)]
    pub wait: Option<Wait>,
    /// The terminal that started the worker, from `HC_TERMINAL_ID`.
    #[serde(default, rename = "hc_terminal_id")]
    pub terminal: Option<String>,
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("pid", &self.pid)
            .field("instance", &self.instance)
            .field("major", &self.major)
            .field("token", &"<hidden>")
            .field("socket", &self.socket)
            .field("session", &self.session)
            .field("cwd", &self.cwd)
            .field("started_ms", &self.started_ms)
            .field("waiting", &self.waiting)
            .field("state", &self.state)
            .field("wait", &self.wait)
            .field("terminal", &self.terminal)
            .finish()
    }
}

/// Writes the folder and the entry file for one worker.
///
/// The folder is 0700 and the entry 0600. The entry is written to a temp file
/// and renamed, so a client never reads half of one. The file calls run as one
/// job on the blocking pool.
pub async fn write(folder: &Path, entry: &Entry) -> io::Result<()> {
    let (folder, entry) = (folder.to_path_buf(), entry.clone());
    tokio::task::spawn_blocking(move || write_blocking(&folder, &entry))
        .await
        .map_err(io::Error::other)?
}

/// [`write`] for a caller already on the blocking pool.
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "runs inside spawn_blocking"
)]
pub fn write_blocking(folder: &Path, entry: &Entry) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(folder)?;
    let found = std::fs::symlink_metadata(folder)?;
    if !found.is_dir() || found.uid() != getuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a folder of this user", folder.display()),
        ));
    }
    std::fs::set_permissions(folder, Permissions::from_mode(DIR_MODE))?;
    let name = format!("{}.json", entry.session);
    let temp = folder.join(format!("{name}.tmp-{}", entry.pid));
    let bytes = serde_json::to_vec(entry).map_err(io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(&temp)?;
    file.write_all(&bytes)?;
    file.set_permissions(Permissions::from_mode(FILE_MODE))?;
    drop(file);
    std::fs::rename(&temp, folder.join(name))
}

/// Removes the entry file of `session`; a missing one is not an error.
pub async fn remove(folder: &Path, session: &str) -> io::Result<()> {
    match fs::remove_file(folder.join(format!("{session}.json"))).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Every live entry, and drops the files of workers whose pid is gone.
pub async fn list(folder: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(mut dir) = fs::read_dir(folder).await else {
        return out;
    };
    while let Ok(Some(item)) = dir.next_entry().await {
        let path = item.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        let Ok(bytes) = fs::read(&path).await else {
            continue;
        };
        let Ok(entry) = serde_json::from_slice::<Entry>(&bytes) else {
            continue;
        };
        if !alive(entry.pid) {
            let _ = fs::remove_file(&path).await;
            continue;
        }
        out.push(entry);
    }
    out
}

/// `<folder>/<session>.sock`, refused when the platform cannot bind it.
pub fn socket_path(folder: &Path, session: &str) -> Result<PathBuf, PathTooLong> {
    let path = folder.join(format!("{session}.sock"));
    let len = path.as_os_str().as_bytes().len();
    if len > MAX_SOCKET_PATH {
        return Err(PathTooLong {
            len,
            limit: MAX_SOCKET_PATH,
        });
    }
    Ok(path)
}

fn alive(pid: u32) -> bool {
    // Pid 0 and a negative pid name process groups, not one worker.
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => true,
    }
}

/// A socket path over the platform's `sun_path` limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathTooLong {
    pub len: usize,
    pub limit: usize,
}

impl fmt::Display for PathTooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "socket path of {} bytes, the limit is {}",
            self.len, self.limit
        )
    }
}

impl std::error::Error for PathTooLong {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(session: &str, pid: u32, folder: &Path) -> Entry {
        Entry {
            pid,
            instance: "an-instance".to_string(),
            major: MAJOR,
            token: "0123456789abcdef0123456789abcdef".to_string(),
            socket: folder.join(format!("{session}.sock")),
            session: session.to_string(),
            cwd: PathBuf::from("/tmp"),
            started_ms: 7,
            waiting: false,
            state: State::Idle,
            wait: None,
            terminal: None,
        }
    }

    async fn mode(path: &Path) -> u32 {
        fs::metadata(path)
            .await
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    async fn names(folder: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut dir = fs::read_dir(folder).await.expect("reads");
        while let Some(item) = dir.next_entry().await.expect("entry") {
            out.push(item.file_name().to_string_lossy().into_owned());
        }
        out.sort();
        out
    }

    #[test]
    fn the_folder_names_the_major() {
        assert_eq!(
            folder(Path::new("/var/state")),
            PathBuf::from("/var/state/reagent/workers/v1")
        );
    }

    #[tokio::test]
    async fn write_sets_the_modes_and_leaves_no_temp_file() {
        let tmp = TempDir::new().expect("temp");
        let folder = folder(tmp.path());
        let entry = entry("s1", std::process::id(), &folder);
        write(&folder, &entry).await.expect("writes");
        assert_eq!(mode(&folder).await, 0o700);
        let file = folder.join("s1.json");
        assert_eq!(mode(&file).await, 0o600);
        assert_eq!(names(&folder).await, vec!["s1.json".to_string()]);
        let text = fs::read_to_string(&file).await.expect("reads");
        let back: Entry = serde_json::from_str(&text).expect("parses");
        assert_eq!(back, entry);
    }

    #[tokio::test]
    async fn a_looser_folder_is_tightened() {
        let tmp = TempDir::new().expect("temp");
        let folder = folder(tmp.path());
        fs::create_dir_all(&folder).await.expect("creates");
        fs::set_permissions(&folder, Permissions::from_mode(0o755))
            .await
            .expect("chmods");
        let entry = entry("s1", std::process::id(), &folder);
        write(&folder, &entry).await.expect("writes");
        assert_eq!(mode(&folder).await, 0o700);
    }

    #[tokio::test]
    async fn a_new_folder_is_made_0700() {
        let tmp = TempDir::new().expect("temp");
        let folder = folder(tmp.path());
        write(&folder, &entry("s1", std::process::id(), &folder))
            .await
            .expect("writes");
        assert_eq!(mode(&folder).await, 0o700);
        assert_eq!(mode(folder.parent().expect("parent")).await, 0o700);
    }

    #[tokio::test]
    async fn a_symlinked_folder_is_refused() {
        let tmp = TempDir::new().expect("temp");
        let real = tmp.path().join("real");
        fs::create_dir(&real).await.expect("creates");
        let folder = folder(tmp.path());
        fs::create_dir_all(folder.parent().expect("parent"))
            .await
            .expect("creates");
        fs::symlink(&real, &folder).await.expect("links");
        let error = write(&folder, &entry("s1", std::process::id(), &folder))
            .await
            .expect_err("refuses a link");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn pid_zero_and_group_pids_are_not_alive() {
        assert!(!alive(0));
        assert!(!alive(u32::MAX));
    }

    #[test]
    fn debug_hides_the_token() {
        let shown = format!("{:?}", entry("s1", 1, Path::new("/f")));
        assert!(!shown.contains("0123456789abcdef"), "{shown}");
    }

    #[tokio::test]
    async fn remove_takes_the_entry_away() {
        let tmp = TempDir::new().expect("temp");
        let folder = folder(tmp.path());
        let entry = entry("s1", std::process::id(), &folder);
        write(&folder, &entry).await.expect("writes");
        remove(&folder, "s1").await.expect("removes");
        assert!(names(&folder).await.is_empty());
        remove(&folder, "s1")
            .await
            .expect("a missing entry is fine");
    }

    #[tokio::test]
    async fn list_keeps_the_current_process_and_drops_a_dead_pid() {
        let tmp = TempDir::new().expect("temp");
        let folder = folder(tmp.path());
        let live = entry("live", std::process::id(), &folder);
        write(&folder, &live).await.expect("writes");

        let mut child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawns");
        let dead = entry("dead", child.id().expect("a pid"), &folder);
        child.wait().await.expect("waits");
        write(&folder, &dead).await.expect("writes");

        let listed = list(&folder).await;
        assert_eq!(listed, vec![live]);
        assert_eq!(names(&folder).await, vec!["live.json".to_string()]);
    }

    #[tokio::test]
    async fn list_skips_a_bad_file() {
        let tmp = TempDir::new().expect("temp");
        let folder = folder(tmp.path());
        fs::create_dir_all(&folder).await.expect("creates");
        fs::write(folder.join("bad.json"), b"not json")
            .await
            .expect("writes");
        fs::write(folder.join("note.txt"), b"{}")
            .await
            .expect("writes");
        assert!(list(&folder).await.is_empty());
        let left = names(&folder).await;
        assert_eq!(left, vec!["bad.json".to_string(), "note.txt".to_string()]);
    }

    /// An entry an older worker wrote, with no `waiting` field, reads as one
    /// whose run waits for nothing.
    #[tokio::test]
    async fn approval_mark_an_entry_without_the_field_reads_not_waiting() {
        let tmp = TempDir::new().expect("temp");
        let folder = folder(tmp.path());
        fs::create_dir_all(&folder).await.expect("creates");
        let old = format!(
            concat!(
                r#"{{"pid":{},"instance":"an-instance","major":{},"token":"{}","#,
                r#""socket":"/tmp/s1.sock","session":"s1","cwd":"/tmp","started_ms":7}}"#
            ),
            std::process::id(),
            MAJOR,
            "0123456789abcdef0123456789abcdef",
        );
        fs::write(folder.join("s1.json"), old)
            .await
            .expect("writes");
        let listed = list(&folder).await;
        assert_eq!(listed.len(), 1, "the entry of a live pid is listed");
        assert!(!listed[0].waiting);
    }

    #[tokio::test]
    async fn approval_mark_round_trips() {
        let tmp = TempDir::new().expect("temp");
        let folder = folder(tmp.path());
        let mut entry = entry("s1", std::process::id(), &folder);
        entry.waiting = true;
        write(&folder, &entry).await.expect("writes");
        assert_eq!(list(&folder).await, vec![entry]);
    }

    /// A supervisor reads the state of a run from the entry, so an entry an
    /// older worker wrote, with none of the three fields, still reads: the run
    /// is idle and waits for nothing.
    #[tokio::test]
    async fn state_an_entry_without_the_fields_reads_idle() {
        let tmp = TempDir::new().expect("temp");
        let folder = folder(tmp.path());
        fs::create_dir_all(&folder).await.expect("creates");
        let old = format!(
            concat!(
                r#"{{"pid":{},"instance":"an-instance","major":{},"token":"{}","#,
                r#""socket":"/tmp/s1.sock","session":"s1","cwd":"/tmp","started_ms":7}}"#
            ),
            std::process::id(),
            MAJOR,
            "0123456789abcdef0123456789abcdef",
        );
        fs::write(folder.join("s1.json"), old)
            .await
            .expect("writes");
        let listed = list(&folder).await;
        assert_eq!(listed.len(), 1, "the entry of a live pid is listed");
        assert_eq!(listed[0].state, State::Idle);
        assert_eq!(listed[0].wait, None);
        assert_eq!(listed[0].terminal, None);
    }

    /// A key of a newer worker is not what drops an entry: `list` keeps the
    /// ones it parses, and a field it does not know is one of them.
    #[tokio::test]
    async fn state_an_entry_with_an_unknown_key_reads() {
        let tmp = TempDir::new().expect("temp");
        let folder = folder(tmp.path());
        fs::create_dir_all(&folder).await.expect("creates");
        let mut entry = serde_json::to_value(entry("s1", std::process::id(), &folder))
            .expect("the entry is JSON");
        entry["from_a_newer_worker"] = serde_json::json!({ "a": 1 });
        fs::write(
            folder.join("s1.json"),
            serde_json::to_vec(&entry).expect("the entry is JSON"),
        )
        .await
        .expect("writes");
        let listed = list(&folder).await;
        assert_eq!(listed.len(), 1, "the entry is listed: {listed:?}");
        assert_eq!(listed[0].session, "s1");
    }

    #[tokio::test]
    async fn state_round_trips_with_the_wait_and_the_terminal() {
        let tmp = TempDir::new().expect("temp");
        let folder = folder(tmp.path());
        let mut entry = entry("s1", std::process::id(), &folder);
        entry.state = State::Blocked;
        entry.wait = Some(Wait {
            kind: WaitKind::Question,
            id: 3,
        });
        entry.terminal = Some("t9".to_string());
        write(&folder, &entry).await.expect("writes");
        let text = fs::read_to_string(folder.join("s1.json"))
            .await
            .expect("reads");
        assert!(text.contains(r#""state":"blocked""#), "{text}");
        assert!(
            text.contains(r#""wait":{"kind":"question","id":3}"#),
            "{text}"
        );
        assert!(text.contains(r#""hc_terminal_id":"t9""#), "{text}");
        assert_eq!(list(&folder).await, vec![entry]);
    }

    #[test]
    fn socket_path_refuses_a_long_folder() {
        let long = PathBuf::from(format!("/tmp/{}", "d".repeat(MAX_SOCKET_PATH)));
        let err = socket_path(&long, "s").expect_err("too long");
        assert_eq!(err.limit, MAX_SOCKET_PATH);
        assert!(err.len > MAX_SOCKET_PATH);
        let short = PathBuf::from("/tmp/w");
        assert_eq!(
            socket_path(&short, "s").expect("short enough"),
            short.join("s.sock")
        );
    }
}
