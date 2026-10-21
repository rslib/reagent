//! A cheap fingerprint of the working folder's file state.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::process::Command;

use crate::gitignore::{self, Ignores};
use crate::walk::SKIP_DIRS;

const GIT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ENTRIES: usize = 20_000;

/// Hashes the file state under `root`; two calls differ when a file was
/// created, deleted or changed. `None` when the state cannot be read. Inside a
/// git repository only git's view is used, so a hash is never compared with
/// one from the folder walk.
pub async fn fingerprint(root: &Path) -> Option<u64> {
    match git_fingerprint(root).await {
        GitState::Hash(hash) => return Some(hash),
        GitState::Failed => return None,
        GitState::NotRepo => {}
    }
    let root = root.to_path_buf();
    let global = match gitignore::global_excludes_path() {
        Some(path) => tokio::fs::read(path).await.ok(),
        None => None,
    };
    tokio::task::spawn_blocking(move || {
        walk_fingerprint(&root, MAX_ENTRIES, SKIP_DIRS, global.as_deref())
    })
    .await
    .ok()
    .flatten()
}

enum GitState {
    Hash(u64),
    NotRepo,
    Failed,
}

#[derive(Debug, PartialEq)]
enum Toplevel {
    Repo,
    NotRepo,
    Failed,
}

/// Classifies a `git rev-parse` run: outer `None` is a timeout, inner `None`
/// is a spawn error (git missing), `Some(ok)` is the exit status.
fn classify_toplevel(run: Option<Option<bool>>) -> Toplevel {
    match run {
        None => Toplevel::Failed,
        Some(None | Some(false)) => Toplevel::NotRepo,
        Some(Some(true)) => Toplevel::Repo,
    }
}

fn mix_meta(hasher: &mut DefaultHasher, meta: &std::fs::Metadata) {
    meta.len().hash(hasher);
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok());
    mtime.hash(hasher);
}

async fn git_fingerprint(root: &Path) -> GitState {
    let top = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let top = tokio::time::timeout(GIT_TIMEOUT, top).await;
    let run = match &top {
        Err(_) => None,
        Ok(Err(_)) => Some(None),
        Ok(Ok(out)) => Some(Some(out.status.success())),
    };
    let top_path = match (classify_toplevel(run), top) {
        (Toplevel::Repo, Ok(Ok(out))) => out.stdout,
        (Toplevel::NotRepo, _) => return GitState::NotRepo,
        _ => return GitState::Failed,
    };
    match git_status_hash(root, &top_path).await {
        Some(hash) => GitState::Hash(hash),
        None => GitState::Failed,
    }
}

async fn git_status_hash(root: &Path, top: &[u8]) -> Option<u64> {
    let run = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(GIT_TIMEOUT, run).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    output.stdout.hash(&mut hasher);
    // Paths in this output are relative to the repository top level.
    let top = PathBuf::from(std::str::from_utf8(top).ok()?.trim_end_matches('\n'));
    let mut fields = output.stdout.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let status = &field[..2];
        let path = String::from_utf8_lossy(&field[3..]).into_owned();
        if status.contains(&b'R') || status.contains(&b'C') {
            // The next field is the original path of a rename or copy.
            fields.next();
        }
        if let Ok(meta) = tokio::fs::metadata(top.join(&path)).await {
            mix_meta(&mut hasher, &meta);
        }
    }
    Some(hasher.finish())
}

/// Walks `root` the way git would without a repository: directories named in
/// `skip` and entries matched by `.gitignore` files (and the `global` excludes
/// text) are left out.
#[expect(clippy::disallowed_methods, reason = "runs inside spawn_blocking")]
fn walk_fingerprint(root: &Path, cap: usize, skip: &[&str], global: Option<&[u8]>) -> Option<u64> {
    let mut ignores = Ignores::default();
    let global_tip = global.and_then(|text| ignores.add(None, b"", text));
    let mut entries: Vec<(PathBuf, u64, Option<Duration>)> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), Vec::<u8>::new(), global_tip)];
    let mut rel = Vec::<u8>::new();
    let mut seen = 0usize;
    while let Some((dir, dir_rel, parent_tip)) = stack.pop() {
        let tip = match std::fs::read(dir.join(".gitignore")) {
            Ok(text) => ignores.add(parent_tip, &dir_rel, &text),
            Err(_) => parent_tip,
        };
        for entry in std::fs::read_dir(&dir).ok()? {
            let entry = entry.ok()?;
            seen += 1;
            if seen > cap {
                return None;
            }
            let kind = entry.file_type().ok()?;
            let name = entry.file_name();
            rel.clear();
            if !dir_rel.is_empty() {
                rel.extend_from_slice(&dir_rel);
                rel.push(b'/');
            }
            rel.extend_from_slice(name.as_encoded_bytes());
            if kind.is_dir() {
                if !skip.iter().any(|s| name == *s) && !ignores.is_ignored(tip, &rel, true) {
                    stack.push((entry.path(), rel.clone(), tip));
                }
            } else if kind.is_file() && !ignores.is_ignored(tip, &rel, false) {
                let meta = entry.metadata().ok()?;
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok());
                let rel_path = entry.path().strip_prefix(root).ok()?.to_path_buf();
                entries.push((rel_path, meta.len(), mtime));
            }
        }
    }
    entries.sort();
    let mut hasher = DefaultHasher::new();
    entries.hash(&mut hasher);
    Some(hasher.finish())
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "test setup on a temp dir"
)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    async fn walk(dir: &Path) -> Option<u64> {
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || walk_fingerprint(&dir, MAX_ENTRIES, SKIP_DIRS, None))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn unchanged_folder_is_equal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();

        assert_eq!(walk(dir.path()).await, walk(dir.path()).await);
        assert!(walk(dir.path()).await.is_some());
    }

    #[tokio::test]
    async fn new_file_changes_it() {
        let dir = tempfile::tempdir().unwrap();
        let before = walk(dir.path()).await;
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();

        assert_ne!(before, walk(dir.path()).await);
    }

    #[tokio::test]
    async fn rewrite_with_new_size_changes_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let before = walk(dir.path()).await;
        std::fs::write(&file, "three").unwrap();

        assert_ne!(before, walk(dir.path()).await);
    }

    #[tokio::test]
    async fn deleting_a_file_changes_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let before = walk(dir.path()).await;
        std::fs::remove_file(&file).unwrap();

        assert_ne!(before, walk(dir.path()).await);
    }

    #[tokio::test]
    async fn skipped_dirs_do_not_count() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let before = walk(dir.path()).await;
        for name in ["target", "__pycache__"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
            std::fs::write(dir.path().join(name).join("x.bin"), "junk").unwrap();
        }

        assert_eq!(before, walk(dir.path()).await);
    }

    #[tokio::test]
    async fn gitignored_files_do_not_count_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\nscratch/\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let before = walk(dir.path()).await;

        std::fs::write(dir.path().join("noise.log"), "junk").unwrap();
        std::fs::create_dir(dir.path().join("scratch")).unwrap();
        std::fs::write(dir.path().join("scratch").join("b.txt"), "junk").unwrap();
        assert_eq!(before, walk(dir.path()).await);

        std::fs::write(dir.path().join("b.txt"), "two").unwrap();
        assert_ne!(before, walk(dir.path()).await);
    }

    #[test]
    fn caller_skip_list_and_global_rules_apply() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let base = walk_fingerprint(dir.path(), 100, &[], Some(b"*.bak\n"));
        std::fs::create_dir(dir.path().join("vendor")).unwrap();
        std::fs::write(dir.path().join("vendor").join("v.txt"), "x").unwrap();
        std::fs::write(dir.path().join("c.bak"), "x").unwrap();

        let skipping = walk_fingerprint(dir.path(), 100, &["vendor"], Some(b"*.bak\n"));
        assert_eq!(base, skipping);
        assert_ne!(
            base,
            walk_fingerprint(dir.path(), 100, &[], Some(b"*.bak\n"))
        );
    }

    #[test]
    fn toplevel_outcome_decides_the_fallback() {
        assert_eq!(classify_toplevel(Some(Some(true))), Toplevel::Repo);
        assert_eq!(classify_toplevel(Some(Some(false))), Toplevel::NotRepo);
        assert_eq!(classify_toplevel(Some(None)), Toplevel::NotRepo);
        assert_eq!(classify_toplevel(None), Toplevel::Failed);
    }

    #[test]
    fn walk_over_the_cap_is_none() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("{i}.txt")), "x").unwrap();
        }

        assert_eq!(walk_fingerprint(dir.path(), 3, SKIP_DIRS, None), None);
        assert!(walk_fingerprint(dir.path(), 5, SKIP_DIRS, None).is_some());
    }

    #[tokio::test]
    async fn public_fingerprint_works_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let before = fingerprint(dir.path()).await;
        assert!(before.is_some());
        std::fs::write(dir.path().join("b.txt"), "two").unwrap();

        assert_ne!(before, fingerprint(dir.path()).await);
    }

    #[tokio::test]
    async fn git_mode_sees_repeated_edits_and_ignores_ignored_files() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(git(root, &["init", "-q"]));
        std::fs::write(root.join(".gitignore"), "ignored.log\n").unwrap();
        std::fs::write(root.join("a.txt"), "one").unwrap();
        assert!(git(root, &["add", "."]));
        assert!(git(
            root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init"
            ]
        ));
        let clean = fingerprint(root).await;
        assert!(clean.is_some());

        std::fs::write(root.join("a.txt"), "two").unwrap();
        let first = fingerprint(root).await;
        assert_ne!(clean, first);

        std::fs::write(root.join("a.txt"), "three more").unwrap();
        let second = fingerprint(root).await;
        assert_ne!(first, second);

        std::fs::write(root.join("ignored.log"), "noise").unwrap();
        assert_eq!(second, fingerprint(root).await);
    }
}
