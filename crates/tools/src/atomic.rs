//! Atomic file writes shared by the write and edit tools.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;

/// Writes `bytes` to `path` atomically: a temp file in the same folder is
/// written and then renamed over the target, so a reader never sees a
/// half-written file. A symlink is followed and the existing file mode is
/// kept. A call dropped mid-write can leave the temp file behind.
pub async fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let target = match tokio::fs::canonicalize(path).await {
        Ok(target) => target,
        Err(error) if error.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(error) => return Err(format!("cannot write {}: {error}", path.display())),
    };
    let tmp = temp_path(&target);
    match write_and_rename(&target, &tmp, bytes).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(format!("cannot write {}: {error}", path.display()))
        }
    }
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

async fn write_and_rename(target: &Path, tmp: &Path, bytes: &[u8]) -> io::Result<()> {
    let permissions = match tokio::fs::metadata(target).await {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
        .await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions).await?;
    }
    drop(file);
    tokio::fs::rename(tmp, target).await
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
