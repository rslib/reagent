//! Atomic file writes shared by the write and edit tools.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Writes `bytes` to `path` atomically: a temp file in the same folder is
/// written and then renamed over the target, so a reader never sees a
/// half-written file. A symlink is followed and the existing file mode is
/// kept. A call dropped mid-write can leave the temp file behind.
///
/// Runs inside the blocking job of the calling tool, which is why it is sync.
#[expect(clippy::disallowed_methods, reason = "runs inside spawn_blocking")]
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let target = match std::fs::canonicalize(path) {
        Ok(target) => target,
        Err(error) if error.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(error) => return Err(format!("cannot write {}: {error}", path.display())),
    };
    let tmp = temp_path(&target);
    match write_and_rename(&target, &tmp, bytes) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            Err(format!("cannot write {}: {error}", path.display()))
        }
    }
}

/// Writes `bytes` to `path` atomically, in one blocking job.
#[cfg(test)]
pub(crate) async fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let (path, bytes) = (path.to_path_buf(), bytes.to_vec());
    crate::blocking::run(move || write_atomic(&path, &bytes)).await
}

fn temp_path(target: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "runs inside spawn_blocking"
)]
fn write_and_rename(target: &Path, tmp: &Path, bytes: &[u8]) -> io::Result<()> {
    let permissions = match std::fs::metadata(target) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?;
    file.write_all(bytes)?;
    file.flush()?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)?;
    }
    drop(file);
    std::fs::rename(tmp, target)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[tokio::test]
    async fn keeps_the_mode_of_an_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.sh");
        tokio::fs::write(&path, "old").await.unwrap();
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();
        write(&path, b"new").await.unwrap();
        let mode = tokio::fs::metadata(&path)
            .await
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[tokio::test]
    async fn writes_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f.txt");
        let link = tmp.path().join("link");
        tokio::fs::write(&file, "old").await.unwrap();
        tokio::fs::symlink(&file, &link).await.unwrap();
        write(&link, b"new").await.unwrap();
        assert!(
            tokio::fs::symlink_metadata(&link)
                .await
                .unwrap()
                .is_symlink()
        );
        assert_eq!(tokio::fs::read_to_string(&file).await.unwrap(), "new");
    }

    #[tokio::test]
    async fn leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a"), b"x").await.unwrap();
        let mut entries = tokio::fs::read_dir(tmp.path()).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name());
        }
        assert_eq!(names, ["a"]);
    }
}
