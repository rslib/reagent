//! A parallel walk of a folder by git's ignore rules, for the search tools.
//!
//! Each directory is read in its own blocking task, so on a network file
//! system the round trips of many directories overlap. Listing needs no
//! `stat`: the file type comes with the directory entry.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::gitignore::{self, Chain};

/// Folders that are never entered, ignored or not.
pub(crate) const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".venv",
    "venv",
    ".tox",
    ".cache",
    "dist",
    "build",
];

/// Extra folder names to skip, on top of [`SKIP_DIRS`]. A name matches a
/// folder of that exact name at any depth. Clones share one list.
#[derive(Debug, Clone, Default)]
pub(crate) struct Skip(Arc<[Box<str>]>);

impl Skip {
    /// Builds a skip list of the given extra folder names.
    pub(crate) fn new(names: impl IntoIterator<Item = String>) -> Self {
        Self(names.into_iter().map(String::into_boxed_str).collect())
    }

    /// Whether a folder with this name is never entered.
    pub(crate) fn is_skipped(&self, name: &OsStr) -> bool {
        SKIP_DIRS.iter().any(|skip| name == OsStr::new(skip))
            || self.0.iter().any(|skip| name == OsStr::new(&**skip))
    }
}

/// The most directories read at once. It hides network latency without
/// flooding a metadata server that other users share.
const MAX_TASKS: usize = 16;

/// The walk stops listing after this many files.
pub(crate) const MAX_FILES: usize = 200_000;

/// One file the walk found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct File {
    /// The path relative to the walk root, with `/` between parts.
    pub rel: String,
    /// The path to open.
    pub path: PathBuf,
}

/// The files of a walk, sorted by relative path.
#[derive(Debug, Default)]
pub(crate) struct Listing {
    pub files: Vec<File>,
    /// The walk stopped at [`MAX_FILES`].
    pub truncated: bool,
}

/// The run was cancelled during the walk.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Cancelled;

struct Dir {
    skip: Skip,
    path: PathBuf,
    rel: Vec<u8>,
    ignores: Option<Arc<Chain>>,
}

#[derive(Default)]
struct DirOutput {
    files: Vec<File>,
    dirs: Vec<Dir>,
}

/// Lists the files under `root` that git would not ignore: entries named in
/// [`SKIP_DIRS`] or `skip` and entries that a `.gitignore` or the global excludes
/// match are left out, and ignored folders are never entered. Symbolic links
/// are skipped, as ripgrep does by default.
pub(crate) async fn walk(
    root: &Path,
    skip: &Skip,
    cancel: &CancellationToken,
) -> Result<Listing, Cancelled> {
    let global = match gitignore::global_excludes_path() {
        Some(path) => tokio::fs::read(path).await.ok(),
        None => None,
    };
    let mut ignores = global.and_then(|text| Chain::add(None, b"", &text));
    for (above, text) in ancestor_ignores(root).await {
        ignores = Chain::add_above(ignores, &above, &text);
    }
    let mut queue = VecDeque::from([Dir {
        skip: skip.clone(),
        path: root.to_path_buf(),
        rel: Vec::new(),
        ignores,
    }]);
    let mut tasks = JoinSet::new();
    let mut listing = Listing::default();
    loop {
        while tasks.len() < MAX_TASKS
            && let Some(dir) = queue.pop_front()
        {
            tasks.spawn_blocking(move || read_dir(dir));
        }
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tasks.abort_all();
                return Err(Cancelled);
            }
            next = tasks.join_next() => next,
        };
        let Some(joined) = next else {
            break;
        };
        // A task that panicked lost one folder; the rest of the walk stands.
        let Ok(output) = joined else {
            continue;
        };
        listing.files.extend(output.files);
        queue.extend(output.dirs);
        // Past the cap, which files were kept depends on task timing. Trees
        // that large are rare, and the output says the walk stopped.
        if listing.files.len() >= MAX_FILES {
            tasks.abort_all();
            listing.truncated = true;
            listing.files.truncate(MAX_FILES);
            break;
        }
    }
    listing.files.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    Ok(listing)
}

/// The `.gitignore` files of the folders above `root`, up to the folder that
/// holds `.git`, outermost first, each with `root`'s path relative to its
/// folder. Git applies them to a walk of a subfolder too. Outside a
/// repository there are none.
async fn ancestor_ignores(root: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let root = tokio::fs::canonicalize(root)
        .await
        .unwrap_or_else(|_| root.to_path_buf());
    if tokio::fs::try_exists(root.join(".git"))
        .await
        .unwrap_or(false)
    {
        return Vec::new();
    }
    let mut found = Vec::new();
    for dir in root.ancestors().skip(1) {
        if let Ok(text) = tokio::fs::read(dir.join(".gitignore")).await {
            found.push((dir.to_path_buf(), text));
        }
        if tokio::fs::try_exists(dir.join(".git"))
            .await
            .unwrap_or(false)
        {
            return found
                .into_iter()
                .rev()
                .filter_map(|(dir, text)| {
                    let above = root.strip_prefix(&dir).ok()?;
                    Some((above.as_os_str().as_encoded_bytes().to_vec(), text))
                })
                .collect();
        }
    }
    Vec::new()
}

#[expect(clippy::disallowed_methods, reason = "runs inside spawn_blocking")]
fn read_dir(dir: Dir) -> DirOutput {
    let mut output = DirOutput::default();
    let ignores = match std::fs::read(dir.path.join(".gitignore")) {
        Ok(text) => Chain::add(dir.ignores, &dir.rel, &text),
        Err(_) => dir.ignores,
    };
    let Ok(entries) = std::fs::read_dir(&dir.path) else {
        return output;
    };
    let mut rel = Vec::new();
    for entry in entries.flatten() {
        // The type comes from the directory entry; std calls lstat only
        // when the file system does not report it.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        rel.clear();
        if !dir.rel.is_empty() {
            rel.extend_from_slice(&dir.rel);
            rel.push(b'/');
        }
        rel.extend_from_slice(name.as_encoded_bytes());
        let ignored = |is_dir| {
            ignores
                .as_deref()
                .is_some_and(|chain| chain.is_ignored(&rel, is_dir))
        };
        if kind.is_dir() {
            if !dir.skip.is_skipped(&name) && !ignored(true) {
                output.dirs.push(Dir {
                    skip: dir.skip.clone(),
                    path: entry.path(),
                    rel: rel.clone(),
                    ignores: ignores.clone(),
                });
            }
        } else if kind.is_file() && !ignored(false) {
            output.files.push(File {
                rel: String::from_utf8_lossy(&rel).into_owned(),
                path: entry.path(),
            });
        }
    }
    output
}

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn names(listing: &Listing) -> Vec<&str> {
        listing.files.iter().map(|f| f.rel.as_str()).collect()
    }

    async fn list(root: &Path) -> Listing {
        walk(root, &Skip::default(), &CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn ignored_entries_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.rs", "");
        write(tmp.path(), "target/b.rs", "");
        write(tmp.path(), "c.log", "");
        write(tmp.path(), ".gitignore", "*.log\n");
        assert_eq!(names(&list(tmp.path()).await), [".gitignore", "a.rs"]);
    }

    #[tokio::test]
    async fn configured_skip_folders_are_not_entered() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "data/x.rs", "");
        write(tmp.path(), "src/a.rs", "");
        write(tmp.path(), "src/data/y.rs", "");
        let skip = Skip::new(["data".to_string()]);
        let listing = walk(tmp.path(), &skip, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(names(&listing), ["src/a.rs"]);
    }

    #[tokio::test]
    async fn an_ignored_folder_is_not_entered() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".gitignore", "gen/\n");
        write(tmp.path(), "gen/.gitignore", "!keep.rs\n");
        write(tmp.path(), "gen/keep.rs", "");
        write(tmp.path(), "src/lib.rs", "");
        assert_eq!(names(&list(tmp.path()).await), [".gitignore", "src/lib.rs"]);
    }

    #[tokio::test]
    async fn nested_rules_apply_below_their_folder() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a/.gitignore", "*.tmp\n");
        write(tmp.path(), "a/x.tmp", "");
        write(tmp.path(), "b/y.tmp", "");
        assert_eq!(names(&list(tmp.path()).await), ["a/.gitignore", "b/y.tmp"]);
    }

    #[tokio::test]
    async fn the_walk_is_sorted_and_the_same_each_time() {
        let tmp = tempfile::tempdir().unwrap();
        for dir in 0..30 {
            for file in 0..10 {
                write(tmp.path(), &format!("d{dir}/f{file}.txt"), "");
            }
        }
        let first = list(tmp.path()).await;
        let second = list(tmp.path()).await;
        assert_eq!(first.files.len(), 300);
        assert_eq!(first.files, second.files);
        let mut sorted = names(&first);
        sorted.sort_unstable();
        assert_eq!(names(&first), sorted);
        assert!(!first.truncated);
    }

    #[tokio::test]
    async fn a_cancelled_walk_returns_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        for dir in 0..50 {
            write(tmp.path(), &format!("d{dir}/f.txt"), "");
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        let started = std::time::Instant::now();
        assert_eq!(
            walk(tmp.path(), &Skip::default(), &cancel)
                .await
                .unwrap_err(),
            Cancelled
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
    }

    #[tokio::test]
    async fn rules_above_a_subfolder_walk_apply_inside_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        write(tmp.path(), ".gitignore", "*.gen\n");
        write(tmp.path(), "sub/.gitignore", "deep/*.tmp\n");
        write(tmp.path(), "sub/x.gen", "");
        write(tmp.path(), "sub/b.txt", "");
        write(tmp.path(), "sub/deep/y.tmp", "");
        write(tmp.path(), "sub/deep/z.txt", "");
        assert_eq!(names(&list(&tmp.path().join("sub/deep")).await), ["z.txt"]);
        assert_eq!(
            names(&list(&tmp.path().join("sub")).await),
            [".gitignore", "b.txt", "deep/z.txt"]
        );
    }

    #[tokio::test]
    async fn rules_of_an_outer_repository_skip_a_nested_one() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        write(tmp.path(), ".gitignore", "*.gen\n");
        std::fs::create_dir_all(tmp.path().join("inner/.git")).unwrap();
        write(tmp.path(), "inner/x.gen", "");
        assert_eq!(names(&list(&tmp.path().join("inner")).await), ["x.gen"]);
    }

    #[tokio::test]
    async fn rules_above_the_walk_root_are_not_used_outside_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".gitignore", "*.gen\n");
        write(tmp.path(), "sub/x.gen", "");
        assert_eq!(names(&list(&tmp.path().join("sub")).await), ["x.gen"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symbolic_links_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "real/a.rs", "");
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("loop")).unwrap();
        assert_eq!(names(&list(tmp.path()).await), ["real/a.rs"]);
    }
}
