//! Installs a plugin from a git repository, with the `git` command.
//!
//! `add git:<host>/<path>[@<ref>]` clones the repository at its ref into a temp
//! folder inside the staging folder, copies the files it tracks into the
//! staging folder at the same relative paths and deletes the clone. The bare
//! imports of those files are bundled through the CDN the npm source uses, by
//! the same code. The lockfile pins the commit, so an update knows what the
//! folder holds and whether the repository moved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use super::{
    FILES_LIMIT, Fetcher, Lock, Own, Pending, Plan, Staged, Summary, TOTAL_LIMIT, Update, cdn,
    check_folder, crawl_bare, folder_of, installed_of, lock, own_files, short, write_files,
    write_lock,
};

/// How long a fetch or a ref listing may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// How many lines of git's error output an error keeps.
const ERROR_LINES: usize = 8;

/// The folder inside the staging folder the clone stands in.
const CLONE: &str = ".git-fetch";

/// A `git:` source: where to clone from and which ref to fetch.
#[derive(Debug, PartialEq, Eq)]
struct Git {
    /// What `git:` names, without the ref: `github.com/owner/repo`, or
    /// `file:///srv/repo` for a local repository.
    base: String,
    /// The URL the clone fetches from.
    url: String,
    /// The ref to fetch, or `None` for the repository's default branch.
    git_ref: Option<String>,
    /// Whether the repository is a local folder.
    local: bool,
}

impl Git {
    /// The source `spec` names, or the reason it names none.
    fn parse(spec: &str) -> Result<Self, String> {
        const SHAPE: &str =
            "a git source is git:<host>/<path>[@<ref>], or git:file:///<path>[@<ref>]";
        let rest = spec
            .strip_prefix("git:")
            .ok_or_else(|| format!("{spec}: {SHAPE}"))?;
        let (base, git_ref) = split_ref(rest);
        let base = base.to_owned();
        if base.is_empty() {
            return Err(format!("{spec}: {SHAPE}"));
        }
        let git_ref = match git_ref {
            Some(text)
                if text.is_empty()
                    || text.starts_with('-')
                    || text.contains(char::is_whitespace) =>
            {
                return Err(format!("{spec}: {text:?} is not a git ref"));
            }
            Some(text) => Some(text.to_owned()),
            None => None,
        };
        let local = base.starts_with("file://");
        let url = if local {
            if !base["file://".len()..].starts_with('/') {
                return Err(format!("{spec}: a local repository is git:file:///<path>"));
            }
            base.clone()
        } else {
            if base.contains("://") {
                return Err(format!(
                    "{spec}: a git source names a host and a path, not a scheme; write git:<host>/<path>"
                ));
            }
            let (host, path) = base
                .split_once('/')
                .ok_or_else(|| format!("{spec}: {SHAPE}"))?;
            let shaped = !host.is_empty()
                && !path.is_empty()
                && host.chars().all(|letter| {
                    letter.is_ascii_alphanumeric() || matches!(letter, '.' | '-' | '_')
                });
            if !shaped {
                return Err(format!("{spec}: {SHAPE}"));
            }
            format!("https://{base}")
        };
        Ok(Self {
            base,
            url,
            git_ref,
            local,
        })
    }

    /// The last part of the repository's path, for a manifest that names no
    /// `name`.
    fn name_hint(&self) -> String {
        let last = self.base.rsplit('/').next().unwrap_or(&self.base);
        last.strip_suffix(".git").unwrap_or(last).to_owned()
    }
}

/// The source and the ref of `rest`: the ref is the text after the last `@`
/// that comes after the last `/`, so a path keeps its own `@`.
fn split_ref(rest: &str) -> (&str, Option<&str>) {
    match rest.rfind('@') {
        Some(at) if rest[..at].rfind('/').is_some_and(|slash| at > slash) => {
            (&rest[..at], Some(&rest[at + 1..]))
        }
        _ => (rest, None),
    }
}

/// One clone of a git source.
struct Cloned {
    /// The folder the clone stands in.
    dir: PathBuf,
    /// The full hash of the commit it checked out.
    commit: String,
    /// The ref it fetched: the one asked for, or the default branch's name.
    ref_name: String,
}

/// Fetches `requested` into `<plugins>/.staging-<label>-<pid>/`, ready for the
/// confirm, and refuses when the folder it installs as stands already.
pub(super) async fn stage_add(
    client: &reqwest::Client,
    requested: &str,
    plugins: &Path,
) -> Result<Pending, String> {
    let pending = stage(client, requested, plugins, "git", false).await?;
    if tokio::fs::symlink_metadata(plugins.join(&pending.name))
        .await
        .is_ok()
    {
        let name = pending.name.clone();
        pending.abort().await;
        return Err(format!(
            "{name} is installed; run reagent plugin update {name}"
        ));
    }
    Ok(pending)
}

/// Fetches the git source `old` recorded again, ready for the confirm, or says
/// that the folder already holds it.
pub(super) async fn stage_update(
    client: &reqwest::Client,
    folder: &str,
    old: &Lock,
    plugins: &Path,
) -> Result<Staged, String> {
    let pending = stage(client, &old.requested, plugins, folder, true).await?;
    if pending.lock.commit == old.commit && pending.lock.files == old.files {
        pending.abort().await;
        return Ok(Staged::UpToDate);
    }
    let installed = plugins.join(folder);
    let (version, old_permissions) = installed_of(&installed).await;
    let old_version = old
        .commit
        .as_deref()
        .map_or(version, |commit| short(commit).to_owned());
    Ok(Staged::Ready(Box::new(Update {
        pending,
        old_version,
        old_permissions,
    })))
}

/// Fetches one source into `<plugins>/.staging-<label>-<pid>/`, and writes its
/// lockfile there.
async fn stage(
    client: &reqwest::Client,
    requested: &str,
    plugins: &Path,
    label: &str,
    replace: bool,
) -> Result<Pending, String> {
    // A source this build cannot fetch is refused before it makes a folder.
    Git::parse(requested)?;
    let staging = plugins.join(format!(".staging-{label}-{}", std::process::id()));
    if tokio::fs::symlink_metadata(&staging).await.is_ok() {
        tokio::fs::remove_dir_all(&staging)
            .await
            .map_err(|error| format!("{}: {error}", staging.display()))?;
    }
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(|error| format!("{}: {error}", staging.display()))?;
    let built = build(client, requested, &staging).await;
    let (name, summary, lock) = match built {
        Ok(built) => built,
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(error);
        }
    };
    Ok(Pending {
        staging,
        folder: plugins.join(&name),
        name,
        summary,
        lock,
        replace,
    })
}

/// Fetches the source and writes it into `folder`; answers the plugin's folder
/// name, the summary and the lockfile.
async fn build(
    client: &reqwest::Client,
    requested: &str,
    folder: &Path,
) -> Result<(String, Summary, Lock), String> {
    let git = Git::parse(requested)?;
    let cloned = clone(&git, folder).await?;
    let source = format!("git:{}@{}", git.base, cloned.ref_name);
    let (files, bytes) = copy_tracked(&cloned, &git, folder).await?;
    let _ = tokio::fs::remove_dir_all(&cloned.dir).await;
    let raw = tokio::fs::read_to_string(folder.join("package.json"))
        .await
        .map_err(|error| format!("{source}: package.json: {error}"))?;
    let plugin = super::Plugin::parse(&raw, &source)?;
    let name = plugin
        .name
        .as_deref()
        .map_or_else(|| git.name_hint(), folder_of);
    check_folder(&name).map_err(|error| format!("{source}: {error}"))?;
    let mut plan = Plan {
        files: Vec::new(),
        imports: BTreeMap::new(),
        dependencies: BTreeMap::new(),
        known: BTreeMap::new(),
        bytes,
    };
    let (_, bare, _) = {
        let mut own = Own::Disk(folder);
        own_files(&mut own, &plugin.entry, &source, files.len()).await?
    };
    let mut fetcher = Fetcher::new(client, cdn()?);
    crawl_bare(&mut fetcher, &mut plan, &source, &plugin.ranges, bare).await?;
    let mut pinned = files;
    pinned.extend(write_files(&plan.files, folder).await?);
    let lock = Lock {
        version: lock::VERSION,
        requested: requested.to_owned(),
        source: source.clone(),
        commit: Some(cloned.commit.clone()),
        files: pinned,
        imports: plan.imports.clone(),
    };
    write_lock(folder, &lock).await?;
    let summary = Summary {
        source,
        version: plugin.version.clone().unwrap_or_else(|| "0.0.0".to_owned()),
        files: lock.files.len(),
        bytes: plan.bytes,
        dependencies: plan.dependencies.clone(),
        permissions: plugin.permissions.clone(),
        commit: Some(cloned.commit),
    };
    Ok((name, summary, lock))
}

/// Clones `git` into `folder/.git-fetch` and checks out its commit.
async fn clone(git: &Git, folder: &Path) -> Result<Cloned, String> {
    let dir = folder.join(CLONE);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| format!("{}: {error}", dir.display()))?;
    let what = format!("the clone of {}", git.base);
    let mut init = command(&dir, git.local);
    init.args(["init", "-q"]);
    run(init, &what, None).await?;
    let refspec = git.git_ref.as_deref().unwrap_or("HEAD");
    let mut fetch = command(&dir, git.local);
    fetch.args(["fetch", "-q", "--depth", "1", &git.url, refspec]);
    run(
        fetch,
        &format!("the fetch of {} at {refspec}", git.url),
        Some(FETCH_TIMEOUT),
    )
    .await?;
    let mut checkout = command(&dir, git.local);
    checkout.args([
        "-c",
        "advice.detachedHead=false",
        "checkout",
        "-q",
        "FETCH_HEAD",
    ]);
    run(checkout, &what, None).await?;
    let mut rev_parse = command(&dir, git.local);
    rev_parse.args(["rev-parse", "HEAD"]);
    let commit = run(rev_parse, &what, None).await?.trim().to_owned();
    if commit.is_empty() {
        return Err(format!("{what}: git named no commit"));
    }
    let ref_name = match &git.git_ref {
        Some(git_ref) => git_ref.clone(),
        None => head_ref(git, &dir).await,
    };
    Ok(Cloned {
        dir,
        commit,
        ref_name,
    })
}

/// The branch the repository's `HEAD` names, or `HEAD` when it names none.
async fn head_ref(git: &Git, dir: &Path) -> String {
    let mut command = command(dir, git.local);
    command.args(["ls-remote", "--symref", &git.url, "HEAD"]);
    let what = format!("the refs of {}", git.base);
    let Ok(listed) = run(command, &what, Some(FETCH_TIMEOUT)).await else {
        return "HEAD".to_owned();
    };
    listed
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("ref: "))
        .and_then(|line| line.split('\t').next())
        .and_then(|name| name.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .map_or_else(|| "HEAD".to_owned(), str::to_owned)
}

/// Copies the files the clone tracks into `folder`, at the same relative paths,
/// and answers the sha256 of each with the bytes they held.
async fn copy_tracked(
    cloned: &Cloned,
    git: &Git,
    folder: &Path,
) -> Result<(BTreeMap<String, String>, u64), String> {
    let mut command = command(&cloned.dir, git.local);
    command.args(["ls-files", "-s", "-z"]);
    let listed = run(command, &format!("the files of {}", git.base), None).await?;
    let paths = tracked_paths(&listed)?;
    if paths.len() > FILES_LIMIT {
        return Err(format!("{} holds more than {FILES_LIMIT} files", git.base));
    }
    let mut files = BTreeMap::new();
    let mut bytes = 0u64;
    for path in paths {
        if !lock::inside(&path) {
            return Err(format!(
                "the tracked path {path:?} leaves the plugin folder"
            ));
        }
        let body = tokio::fs::read(cloned.dir.join(&path))
            .await
            .map_err(|error| format!("{path}: {error}"))?;
        bytes += body.len() as u64;
        if bytes > TOTAL_LIMIT {
            return Err(format!("{} holds more than {TOTAL_LIMIT} bytes", git.base));
        }
        let full = folder.join(&path);
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| format!("{path}: {error}"))?;
        }
        tokio::fs::write(&full, &body)
            .await
            .map_err(|error| format!("{path}: {error}"))?;
        files.insert(path, lock::hash(&body));
    }
    Ok((files, bytes))
}

/// The paths of the files in `git ls-files -s -z` output, refusing a symbolic
/// link or a submodule.
fn tracked_paths(listed: &str) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    for entry in listed.split('\0').filter(|entry| !entry.is_empty()) {
        let (meta, path) = entry
            .split_once('\t')
            .ok_or_else(|| format!("{entry:?} is not a file git tracks"))?;
        match meta.split(' ').next().unwrap_or_default() {
            "120000" => {
                return Err(format!(
                    "{path} is a symbolic link, which a plugin may not hold"
                ));
            }
            "160000" => {
                return Err(format!(
                    "{path} is a submodule, which a plugin may not hold"
                ));
            }
            "100644" | "100755" => paths.push(path.to_owned()),
            mode => {
                return Err(format!(
                    "{path} has the git mode {mode}, which a plugin may not hold"
                ));
            }
        }
    }
    Ok(paths)
}

/// The `git` command of one install: no prompt, no system config, no hooks, and
/// a local repository allowed only for a `file://` source.
fn command(dir: &Path, local: bool) -> Command {
    let mut command = Command::new("git");
    command.arg("-c").arg("core.hooksPath=/dev/null");
    if local {
        command.arg("-c").arg("protocol.file.allow=always");
    }
    command
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

/// Runs `command`, which works for `what`, and answers its stdout, waiting at
/// most `within`.
async fn run(mut command: Command, what: &str, within: Option<Duration>) -> Result<String, String> {
    let output = match within {
        Some(within) => tokio::time::timeout(within, command.output())
            .await
            .map_err(|_| format!("{what}: no answer in {} s", within.as_secs()))?,
        None => command.output().await,
    };
    let output = output.map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => "git sources need the git command on PATH".to_owned(),
        _ => format!("{what}: {error}"),
    })?;
    if !output.status.success() {
        return Err(format!("{what}: {}", last_lines(&output.stderr)));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| format!("{what}: git wrote text that is not UTF-8"))
}

/// The last lines of git's error output, as one text.
fn last_lines(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let kept = &lines[lines.len().saturating_sub(ERROR_LINES)..];
    if kept.is_empty() {
        return "git failed with no error".to_owned();
    }
    kept.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_git_parses_a_host_and_a_path() {
        assert_eq!(
            Git::parse("git:github.com/owner/repo").expect("a source"),
            Git {
                base: "github.com/owner/repo".to_owned(),
                url: "https://github.com/owner/repo".to_owned(),
                git_ref: None,
                local: false,
            }
        );
        assert_eq!(
            Git::parse("git:github.com/owner/repo@v1.2").expect("a ref"),
            Git {
                base: "github.com/owner/repo".to_owned(),
                url: "https://github.com/owner/repo".to_owned(),
                git_ref: Some("v1.2".to_owned()),
                local: false,
            }
        );
        let other = Git::parse("git:git.example.com/team/plugin@main").expect("another host");
        assert_eq!(other.url, "https://git.example.com/team/plugin");
        assert_eq!(other.git_ref, Some("main".to_owned()));
        assert_eq!(other.name_hint(), "plugin");
        assert_eq!(
            Git::parse("git:github.com/owner/repo.git")
                .expect("a name")
                .name_hint(),
            "repo"
        );
    }

    #[test]
    fn install_git_parses_a_local_repository() {
        assert_eq!(
            Git::parse("git:file:///srv/repo@v1").expect("a local source"),
            Git {
                base: "file:///srv/repo".to_owned(),
                url: "file:///srv/repo".to_owned(),
                git_ref: Some("v1".to_owned()),
                local: true,
            }
        );
        let plain = Git::parse("git:file:///srv/repo").expect("a local source");
        assert_eq!(plain.git_ref, None);
        assert_eq!(plain.name_hint(), "repo");
        assert!(Git::parse("git:file://srv/repo").is_err());
        assert!(Git::parse("git:file://").is_err());
    }

    #[test]
    fn install_git_refuses_another_scheme() {
        for spec in [
            "git:http://example.com/x",
            "git:https://github.com/owner/repo",
            "git:ssh://git@example.com/x",
            "git:git@github.com:owner/repo",
            "git:github.com",
            "git:github.com/",
            "git:",
            "npm:demo@1.0.0",
        ] {
            let error = Git::parse(spec).unwrap_err();
            assert!(!error.is_empty(), "{spec} was refused");
        }
        let error = Git::parse("git:http://example.com/x").unwrap_err();
        assert!(error.contains("not a scheme"), "{error}");
    }

    #[test]
    fn install_git_refuses_a_ref_that_starts_with_a_dash() {
        assert!(Git::parse("git:github.com/owner/repo@-v1").is_err());
        assert!(Git::parse("git:github.com/owner/repo@").is_err());
        assert!(Git::parse("git:github.com/owner/repo@a b").is_err());
        assert!(Git::parse("git:github.com/owner/repo@--upload-pack=x").is_err());
    }

    #[test]
    fn install_git_refuses_a_symlink_and_a_submodule() {
        let file = "100644 0000000000000000000000000000000000000000 0\tindex.js\0";
        let exec = "100755 0000000000000000000000000000000000000000 0\tbin/run.js\0";
        assert_eq!(
            tracked_paths(&format!("{file}{exec}")).expect("files"),
            vec!["index.js".to_owned(), "bin/run.js".to_owned()]
        );
        let link = "120000 0000000000000000000000000000000000000000 0\tlib/link.js\0";
        let error = tracked_paths(link).unwrap_err();
        assert!(error.contains("lib/link.js"), "{error}");
        assert!(error.contains("symbolic link"), "{error}");
        let module = "160000 0000000000000000000000000000000000000000 0\tvendor/tiny\0";
        let error = tracked_paths(module).unwrap_err();
        assert!(error.contains("vendor/tiny"), "{error}");
        assert!(error.contains("submodule"), "{error}");
    }

    #[test]
    fn install_git_refuses_a_git_failure_with_its_last_lines() {
        let error = last_lines(b"fatal: repository not found\n");
        assert_eq!(error, "fatal: repository not found");
        assert!(last_lines(b"  \n\n").contains("no error"));
        let many: Vec<String> = (0..20).map(|line| format!("line {line}\n")).collect();
        let joined = last_lines(many.concat().as_bytes());
        assert_eq!(joined.lines().count(), ERROR_LINES, "{joined}");
        assert!(joined.starts_with("line 12"), "{joined}");
    }
}
