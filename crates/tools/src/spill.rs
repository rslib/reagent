//! The folder that holds this run's spill files, and the sweep that removes the
//! folders of runs that are gone.
//!
//! Parallel runs share one root, so a run deletes only what it can prove dead: a
//! run's folder carries a lock its process holds for its whole life, and the OS
//! drops that lock however the process ends.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use tokio::sync::OnceCell;

/// The lock file every run folder carries.
const LOCK: &str = "lock";
/// A lock file this fresh may belong to a run that has not locked it yet.
const GRACE: Duration = Duration::from_secs(60);
/// How old a loose spill file of the versions before the run folders is.
const LOOSE_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// How many taken folder names a run skips before it gives up.
const FOLDER_TRIES: u32 = 64;
/// How many times a run tries its new lock, which another run's sweep may hold
/// for a moment.
const LOCK_TRIES: u32 = 100;

/// This process's run folder, made by the first spill that succeeds in making
/// it; a failure is tried again by the next spill.
static RUN: OnceCell<Run> = OnceCell::const_new();
/// The number in the name of the next run folder.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// One run's folder and the lock that says the run is live.
#[allow(clippy::disallowed_types, reason = "tokio's File takes no lock")]
struct Run {
    folder: PathBuf,
    /// Held for the whole process life, so the OS drops the lock however the
    /// process ends, a `kill -9` included.
    _lock: std::fs::File,
}

/// The root that holds every run's folder.
fn root() -> PathBuf {
    std::env::temp_dir().join("reagent-spill")
}

/// This run's spill folder, made on its first spill: a run that spills nothing
/// makes nothing.
pub(crate) async fn folder() -> Result<&'static Path, String> {
    folder_in(&root(), &RUN).await
}

/// Deletes this run's folder and those of runs that are gone, best effort: a
/// folder the sweep cannot prove dead stays.
///
/// Call it once the runtime has stopped: it uses plain blocking file calls. A
/// run that spilled nothing does nothing here, and the next run that spills
/// sweeps what a crash left.
pub fn cleanup() {
    if let Some(run) = RUN.get()
        && let Some(root) = run.folder.parent()
    {
        cleanup_in(root, Some(run));
    }
}

async fn folder_in<'c>(root: &Path, cell: &'c OnceCell<Run>) -> Result<&'c Path, String> {
    let run = cell.get_or_try_init(|| make(root)).await?;
    Ok(&run.folder)
}

/// Makes a fresh folder under `root` and locks it, skipping the names a dead run
/// with this process id left.
async fn make(root: &Path) -> Result<Run, String> {
    tokio::fs::create_dir_all(root)
        .await
        .map_err(|error| error.to_string())?;
    let mut tries = 0;
    let folder = loop {
        let folder = root.join(format!(
            "run-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match tokio::fs::create_dir(&folder).await {
            Ok(()) => break folder,
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists && tries + 1 < FOLDER_TRIES =>
            {
                tries += 1;
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(folder.join(LOCK))
        .await
        .map_err(|error| error.to_string())?
        .into_std()
        .await;
    // A sweep that finds the new lock file tries it before its grace check,
    // so the lock can be held for a moment by a run that is not its owner.
    let mut tries = 0;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if tries + 1 < LOCK_TRIES => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(Run {
        folder,
        _lock: file,
    })
}

// The sweep runs after the runtime has stopped, when nothing else runs: one
// blocking call per entry costs less than a hop to tokio's blocking pool.
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
fn cleanup_in(root: &Path, own: Option<&Run>) {
    if let Some(run) = own {
        // Unix removes a folder while this process holds its lock, and the lock
        // goes with the process either way.
        let _ = std::fs::remove_dir_all(&run.folder);
    }
    sweep(root);
}

/// Deletes the run folders that are gone and the loose spill files of the
/// versions before them.
#[allow(clippy::disallowed_methods)]
fn sweep(root: &Path) {
    let Ok(names) = std::fs::read_dir(root) else {
        return;
    };
    for entry in names.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let path = entry.path();
        if name.starts_with("run-") {
            let is_folder = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_folder && stale(&path) {
                let _ = std::fs::remove_dir_all(&path);
            }
        } else if name.starts_with("bash-")
            && name.ends_with(".log")
            && age(&path).is_some_and(|age| age > LOOSE_AGE)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Whether a run folder's run is gone: nothing holds its lock and the lock is
/// past the grace, or there is no lock file and the folder is past it.
#[allow(clippy::disallowed_types)]
fn stale(folder: &Path) -> bool {
    let file = match std::fs::OpenOptions::new()
        .write(true)
        .open(folder.join(LOCK))
    {
        Ok(file) => file,
        // A run dies between making its folder and its lock file, so the
        // folder's own age has to say whether it is gone.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return age(folder).is_some_and(|age| age > GRACE);
        }
        // A lock this process cannot open may belong to a run that is live.
        Err(_) => return false,
    };
    // A live run holds its lock, so a lock that locks means the run is gone.
    if file.try_lock().is_err() {
        return false;
    }
    file.metadata()
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > GRACE)
}

/// How long ago `path` was last written, when it says.
#[allow(clippy::disallowed_methods)]
fn age(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use tokio::fs;
    use tokio::sync::OnceCell;

    use super::*;

    /// Makes `root/name` and, when `age` is given, a lock file that old inside.
    async fn folder(root: &Path, name: &str, age: Option<Duration>) -> PathBuf {
        let folder = root.join(name);
        fs::create_dir_all(&folder).await.unwrap();
        if let Some(age) = age {
            let file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(folder.join(LOCK))
                .await
                .unwrap()
                .into_std()
                .await;
            file.set_modified(SystemTime::now() - age).unwrap();
        }
        folder
    }

    /// Sets the modification time of `path`, a folder included, `age` back.
    async fn backdate(path: &Path, age: Duration) {
        fs::OpenOptions::new()
            .read(true)
            .open(path)
            .await
            .unwrap()
            .into_std()
            .await
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    /// A run that died leaves its folder and its lock file behind.
    #[tokio::test]
    async fn a_dead_run_folder_is_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let dead = folder(tmp.path(), "run-1-0", Some(Duration::from_secs(120))).await;
        fs::write(dead.join("bash-1-0.log"), b"old").await.unwrap();
        cleanup_in(tmp.path(), None);
        assert!(
            !fs::try_exists(&dead).await.unwrap(),
            "a dead run's folder stays"
        );
    }

    /// A live run holds its lock, so its folder stays however old it is.
    #[tokio::test]
    async fn a_folder_with_a_held_lock_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let age = Duration::from_secs(2 * 24 * 60 * 60);
        let live = folder(tmp.path(), "run-1-0", Some(age)).await;
        fs::write(live.join("bash-1-0.log"), b"kept").await.unwrap();
        let lock = fs::OpenOptions::new()
            .write(true)
            .truncate(false)
            .open(live.join(LOCK))
            .await
            .unwrap()
            .into_std()
            .await;
        lock.try_lock().unwrap();
        cleanup_in(tmp.path(), None);
        assert!(
            fs::try_exists(live.join("bash-1-0.log")).await.unwrap(),
            "a live run's spill is deleted"
        );
    }

    /// A run between making its lock file and locking it is left alone.
    #[tokio::test]
    async fn a_fresh_lock_file_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let young = folder(tmp.path(), "run-1-0", Some(Duration::from_secs(10))).await;
        cleanup_in(tmp.path(), None);
        assert!(
            fs::try_exists(&young).await.unwrap(),
            "a folder made a moment ago is deleted"
        );
    }

    /// A run that died before its lock file leaves only the folder's age to
    /// say it is gone.
    #[tokio::test]
    async fn a_folder_without_a_lock_file_is_deleted_when_old() {
        let tmp = tempfile::tempdir().unwrap();
        let dead = folder(tmp.path(), "run-1-0", None).await;
        let young = folder(tmp.path(), "run-1-1", None).await;
        backdate(&dead, Duration::from_secs(120)).await;
        cleanup_in(tmp.path(), None);
        assert!(
            !fs::try_exists(&dead).await.unwrap(),
            "an old folder with no lock stays"
        );
        assert!(
            fs::try_exists(&young).await.unwrap(),
            "a fresh folder with no lock is deleted"
        );
    }

    /// The spill files that the versions before the run folders left in the root
    /// go after a day, and only then.
    #[tokio::test]
    async fn a_loose_spill_file_of_an_old_version_is_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("bash-1-0.log");
        let fresh = tmp.path().join("bash-1-1.log");
        fs::write(&old, b"old").await.unwrap();
        fs::write(&fresh, b"fresh").await.unwrap();
        backdate(&old, Duration::from_secs(2 * 24 * 60 * 60)).await;
        backdate(&fresh, Duration::from_secs(60 * 60)).await;
        cleanup_in(tmp.path(), None);
        assert!(
            !fs::try_exists(&old).await.unwrap(),
            "an old loose spill stays"
        );
        assert!(
            fs::try_exists(&fresh).await.unwrap(),
            "a fresh loose spill is deleted"
        );
    }

    /// The run's own folder goes at once, its lock held or not.
    #[tokio::test]
    async fn the_runs_own_folder_is_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let cell = OnceCell::const_new();
        let own = folder_in(tmp.path(), &cell).await.unwrap();
        fs::write(own.join("bash-1-0.log"), b"x").await.unwrap();
        cleanup_in(tmp.path(), cell.get());
        assert!(
            !fs::try_exists(&own).await.unwrap(),
            "the run's own folder stays"
        );
    }

    /// The folder is made once, and a second opener cannot lock it.
    #[tokio::test]
    async fn the_run_folder_is_made_once_and_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let cell = OnceCell::const_new();
        let first = folder_in(tmp.path(), &cell).await.unwrap();
        let second = folder_in(tmp.path(), &cell).await.unwrap();
        assert_eq!(first, second, "a second spill makes a second folder");
        assert!(
            first.starts_with(tmp.path()),
            "the folder is outside the root"
        );
        assert!(
            first
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("run-"),
            "the folder is not a run folder: {}",
            first.display()
        );
        let other = fs::OpenOptions::new()
            .write(true)
            .truncate(false)
            .open(first.join(LOCK))
            .await
            .unwrap()
            .into_std()
            .await;
        assert!(other.try_lock().is_err(), "the run's lock is free");
    }

    #[tokio::test]
    async fn a_failed_run_folder_is_tried_again() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        // A file where the root should be: the folder cannot be made.
        tokio::fs::write(&root, b"").await.unwrap();
        let cell = OnceCell::const_new();
        assert!(folder_in(&root, &cell).await.is_err());

        tokio::fs::remove_file(&root).await.unwrap();
        let folder = folder_in(&root, &cell)
            .await
            .expect("the second spill makes it");
        assert!(folder.starts_with(&root));
    }
}
