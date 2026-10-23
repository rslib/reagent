//! The environment block a run puts at the top of its first user message.
//!
//! The block says what the run can see: the OS, the CPU count and the memory,
//! the login shell, the working folder and its git branch, the languages git
//! tracks there, and which of their tools are on `PATH`. The model then starts
//! from what it has instead of spending a turn finding out.
//!
//! No shell runs and no tool is executed: `git ls-files -z` is read as a stream
//! and counted by extension without keeping the names, and a tool is a file
//! with the executable bit set somewhere on `PATH`, found by one blocking scan.
//! Git runs with the repo's own `core.fsmonitor` program off, so opening a repo
//! never runs the code its config names. The whole probe shares one [`TIMEOUT`]
//! and never fails a run: on an error or a timeout the lines that are ready are
//! kept and the rest are left out.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout_at};

/// The line that opens the block.
const HEADER: &str = "[environment]";

/// How long the whole probe may take before the ready lines are taken.
const TIMEOUT: Duration = Duration::from_secs(2);

/// Bytes in one GiB.
const GIB: u64 = 1 << 30;

/// One chunk of `git ls-files -z` stdout. A tracked path is far shorter, so a
/// name crosses a chunk boundary only sometimes.
const CHUNK: usize = 64 * 1024;

/// Folders whose files say nothing about the languages a project is written
/// in: vendored copies, build output and checked-in dependencies.
const SKIP_FOLDERS: [&[u8]; 7] = [
    b"vendor",
    b"third_party",
    b"node_modules",
    b"dist",
    b"build",
    b"target",
    b".git",
];

/// The source table: the extension, the language it is written in, and the
/// tools that run it. One const, so a new language is one row and not a new
/// match. Any one tool of a row found on `PATH` counts the language as usable.
const SOURCES: &[(&str, &str, &[&str])] = &[
    ("rs", "Rust", &["cargo", "rustc"]),
    ("go", "Go", &["go"]),
    ("ts", "TypeScript", &["node", "bun", "deno"]),
    ("tsx", "TypeScript", &["node", "bun", "deno"]),
    ("mts", "TypeScript", &["node", "bun", "deno"]),
    ("cts", "TypeScript", &["node", "bun", "deno"]),
    ("js", "JavaScript", &["node", "bun", "deno"]),
    ("jsx", "JavaScript", &["node", "bun", "deno"]),
    ("mjs", "JavaScript", &["node", "bun", "deno"]),
    ("cjs", "JavaScript", &["node", "bun", "deno"]),
    ("py", "Python", &["python3", "python"]),
    ("c", "C", &["cc", "gcc", "clang"]),
    ("h", "C", &["cc", "gcc", "clang"]),
    ("cc", "C++", &["c++", "g++", "clang++"]),
    ("cpp", "C++", &["c++", "g++", "clang++"]),
    ("cxx", "C++", &["c++", "g++", "clang++"]),
    ("hpp", "C++", &["c++", "g++", "clang++"]),
    ("hh", "C++", &["c++", "g++", "clang++"]),
    ("hxx", "C++", &["c++", "g++", "clang++"]),
    ("java", "Java", &["java", "javac"]),
    ("kt", "Kotlin", &["kotlinc", "kotlin"]),
    ("kts", "Kotlin", &["kotlinc", "kotlin"]),
    ("swift", "Swift", &["swift"]),
    ("rb", "Ruby", &["ruby"]),
    ("php", "PHP", &["php"]),
    ("cs", "C#", &["dotnet"]),
    ("sh", "Shell", &["bash", "sh"]),
    ("bash", "Shell", &["bash", "sh"]),
    ("zsh", "Shell", &["bash", "sh"]),
    ("lua", "Lua", &["lua"]),
    ("zig", "Zig", &["zig"]),
    ("scala", "Scala", &["scala", "sbt"]),
    ("ex", "Elixir", &["elixir"]),
    ("exs", "Elixir", &["elixir"]),
    ("hs", "Haskell", &["ghc", "cabal", "stack"]),
    ("ml", "OCaml", &["ocaml", "dune"]),
    ("mli", "OCaml", &["ocaml", "dune"]),
    ("dart", "Dart", &["dart"]),
    ("jl", "Julia", &["julia"]),
    ("R", "R", &["Rscript"]),
    ("r", "R", &["Rscript"]),
    ("nix", "Nix", &["nix"]),
];

/// A language and the tools that run it, as [`SOURCES`] groups them.
type Language = (&'static str, &'static [&'static str]);

/// A probe running in the background, so it overlaps the provider setup.
pub struct Probe {
    task: Option<JoinHandle<String>>,
}

impl Probe {
    /// The block, once the probe is done or [`TIMEOUT`] has passed.
    pub async fn block(mut self) -> String {
        match self.task.take() {
            Some(task) => task.await.unwrap_or_default(),
            None => String::new(),
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // A probe the run never awaits must not outlive it.
        if let Some(task) = self.task.as_ref() {
            task.abort();
        }
    }
}

/// Starts [`probe`] as a background task, to be awaited just before the first
/// message is built.
pub fn start(cwd: &Path) -> Probe {
    let cwd = cwd.to_owned();
    Probe {
        task: Some(tokio::spawn(async move { probe(&cwd).await })),
    }
}

/// The first user message: the task, then the block when there is one.
pub fn first_message(task: String, block: &str) -> String {
    let block = block.trim_end();
    if block.is_empty() {
        task
    } else {
        format!("{task}\n\n{block}")
    }
}

/// The environment block for `cwd`, or the lines that were ready within
/// [`TIMEOUT`].
pub async fn probe(cwd: &Path) -> String {
    let deadline = Instant::now() + TIMEOUT;
    // Outside a repo nothing runs git at all.
    let git_dir = timeout_at(deadline, git_dir(cwd)).await.ok().flatten();
    // Joined, not spawned: the parts are I/O waits, which the current-thread
    // runtime overlaps anyway, and a joined future is dropped with the probe
    // instead of outliving the run that dropped it.
    let (memory, branch, ranked) = tokio::join!(
        timeout_at(deadline, total_memory()),
        timeout_at(deadline, branch(git_dir.as_deref())),
        timeout_at(deadline, async {
            match git_dir {
                Some(_) => ranked(cwd).await,
                None => Vec::new(),
            }
        }),
    );
    let memory = memory.ok().flatten();
    let branch = branch.ok().flatten();
    let ranked = ranked.unwrap_or_default();
    // The `PATH` scan is the last part and shares the one deadline, so a
    // `PATH` on a slow mount cannot stretch the probe either.
    let tools = if ranked.is_empty() {
        None
    } else {
        let path = std::env::var_os("PATH");
        timeout_at(deadline, tools_line(&ranked, path.as_deref()))
            .await
            .ok()
    };
    render(cwd, memory, branch.as_deref(), &ranked, tools.as_deref())
}

/// The block's text. Plain code, kept out of the async probe so its state
/// machine holds only the waits.
#[inline(never)]
fn render(
    cwd: &Path,
    memory: Option<u64>,
    branch: Option<&str>,
    ranked: &[Language],
    tools: Option<&str>,
) -> String {
    let mut block = String::with_capacity(256);
    block.push_str(HEADER);
    block.push('\n');
    block.push_str(&os_line(memory));
    block.push('\n');
    if let Some(shell) = shell() {
        block.push_str("shell: ");
        block.push_str(&shell);
        block.push('\n');
    }
    block.push_str("cwd: ");
    block.push_str(&cwd.to_string_lossy());
    if let Some(branch) = branch {
        block.push_str(" (git: ");
        block.push_str(branch);
        block.push(')');
    }
    if !ranked.is_empty() {
        block.push_str("\nlanguages: ");
        block.push_str(&language_line(ranked));
    }
    if let Some(tools) = tools {
        block.push('\n');
        block.push_str(tools);
    }
    block
}

/// The login shell from the environment, `None` when it is unset.
fn shell() -> Option<String> {
    let shell = std::env::var_os("SHELL")?;
    let shell = shell.to_string_lossy().into_owned();
    (!shell.is_empty()).then_some(shell)
}

/// Total memory in whole GiB, rounded to the nearest.
fn giga(bytes: u64) -> u64 {
    bytes.saturating_add(GIB / 2) / GIB
}

/// The arch the way a tool prints it in `uname -m`, with `aarch64` spelled
/// `arm64`.
fn arch(machine: &str) -> &str {
    match machine {
        "aarch64" => "arm64",
        other => other,
    }
}

/// The `os:` line. The memory part is left out when it is unknown.
fn os_line(memory: Option<u64>) -> String {
    let cpu = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let head = format!(
        "os: {} {}, {cpu} cpu",
        std::env::consts::OS,
        arch(std::env::consts::ARCH)
    );
    match memory {
        Some(bytes) => format!("{head}, {}G mem", giga(bytes)),
        None => head,
    }
}

/// The `MemTotal:` line of `/proc/meminfo`, in bytes.
#[cfg(any(target_os = "linux", test))]
fn meminfo_total(text: &str) -> Option<u64> {
    let line = text.lines().find(|line| line.starts_with("MemTotal:"))?;
    let field = "MemTotal:";
    let kib: u64 = line[field.len()..]
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib.saturating_mul(1024))
}

/// The `languages:` line: the named languages, most files first, the first
/// marked ` (most)`.
fn language_line(languages: &[Language]) -> String {
    let mut line = String::new();
    for (index, (name, _)) in languages.iter().enumerate() {
        if index > 0 {
            line.push_str(", ");
        }
        line.push_str(name);
        if index == 0 {
            line.push_str(" (most)");
        }
    }
    line
}

/// The `tools: ...; missing: ...` line for `languages`, looking for their tools
/// in the folders of `path`.
async fn tools_line(languages: &[Language], path: Option<&OsStr>) -> String {
    let dirs: Vec<PathBuf> = path.into_iter().flat_map(std::env::split_paths).collect();
    // Every tool of every listed language, once, in group order.
    let mut wanted: Vec<&'static str> = Vec::new();
    for (_, tools) in languages {
        for tool in *tools {
            if !wanted.contains(tool) {
                wanted.push(tool);
            }
        }
    }
    let found = find_tools(dirs, wanted).await;
    let mut missing: Vec<String> = Vec::new();
    for (name, tools) in languages {
        if !tools.iter().any(|tool| found.contains(tool)) {
            missing.push(format!("{name} ({})", tools.join("/")));
        }
    }
    let found = if found.is_empty() {
        "none".to_string()
    } else {
        found.join(", ")
    };
    let missing = if missing.is_empty() {
        "none".to_string()
    } else {
        missing.join(", ")
    };
    format!("tools: {found}; missing: {missing}")
}

/// The tools of `wanted` that are executable files in one of `dirs`, in the
/// order given.
async fn find_tools(dirs: Vec<PathBuf>, wanted: Vec<&'static str>) -> Vec<&'static str> {
    // The whole scan is one blocking hop: the folders cross the tools a couple
    // of hundred times, and a hop per pair costs a blocking-pool thread and a
    // wakeup for a few `stat` calls.
    let scan = tokio::task::spawn_blocking(move || {
        wanted
            .into_iter()
            .filter(|tool| on_path(&dirs, tool))
            .collect::<Vec<_>>()
    });
    scan.await.unwrap_or_default()
}

/// Whether `tool` is an executable regular file in one of `dirs`.
#[expect(clippy::disallowed_methods, reason = "runs on the blocking pool")]
fn on_path(dirs: &[PathBuf], tool: &str) -> bool {
    dirs.iter().any(|dir| {
        std::fs::metadata(dir.join(tool))
            .is_ok_and(|metadata| metadata.is_file() && executable(&metadata))
    })
}

/// Whether the mode bits say this file can be run.
fn executable(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

/// Total memory in bytes, or `None` when it cannot be read.
#[cfg(target_os = "linux")]
async fn total_memory() -> Option<u64> {
    let text = read_text(Path::new("/proc/meminfo")).await?;
    meminfo_total(&text)
}

/// Total memory in bytes, or `None` when it cannot be read. One `sysctl` run,
/// never a free-memory figure that is stale before the next turn.
#[cfg(target_os = "macos")]
async fn total_memory() -> Option<u64> {
    let output = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Total memory in bytes, or `None` when it cannot be read.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn total_memory() -> Option<u64> {
    None
}

/// The git folder of the repo `cwd` is in: the nearest `.git` folder, or the
/// folder a `.git` file names (a worktree or a submodule). `None` outside git.
async fn git_dir(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let dot_git = dir.join(".git");
        let Ok(metadata) = tokio::fs::metadata(&dot_git).await else {
            continue;
        };
        if metadata.is_dir() {
            return Some(dot_git);
        }
        let text = read_text(&dot_git).await?;
        let target = text.strip_prefix("gitdir:")?.trim();
        return Some(dir.join(target));
    }
    None
}

/// The branch a `HEAD` file names: the branch of `ref: refs/heads/<name>`,
/// unborn or not, or `detached` for a commit id.
fn head_branch(head: &str) -> Option<String> {
    let head = head.trim();
    match head.strip_prefix("ref:") {
        Some(target) => {
            let target = target.trim();
            Some(
                target
                    .strip_prefix("refs/heads/")
                    .unwrap_or(target)
                    .to_string(),
            )
        }
        None if !head.is_empty() && head.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            Some("detached".to_string())
        }
        None => None,
    }
}

/// The branch of the repo in `git_dir`, read from its `HEAD` file: no git
/// process, so the probe runs one, `ls-files`, at most.
async fn branch(git_dir: Option<&Path>) -> Option<String> {
    let head = read_text(&git_dir?.join("HEAD")).await?;
    head_branch(&head)
}

/// A small text file, or `None`: one reader for every file the probe reads, so
/// the reading code is in the binary once.
async fn read_text(path: &Path) -> Option<String> {
    tokio::fs::read_to_string(path).await.ok()
}

/// A `git` command in `cwd`, with its stdin and stderr out of the way.
fn git(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        // `core.fsmonitor` names a program git runs on the commands below, so
        // a repo's own config would get code run on every default run.
        // `--no-optional-locks` keeps the probe off the index lock a parallel
        // git command may hold,. `ls-files` reads the index only, so it never prompts.
        .args(["-c", "core.fsmonitor=false", "--no-optional-locks"])
        .arg("-C")
        .arg(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

/// The languages of the files git tracks in `cwd`, counted in one pass over
/// `git ls-files -z` without holding the names.
async fn ranked(cwd: &Path) -> Vec<Language> {
    let mut command = git(cwd);
    command.args(["ls-files", "-z"]).stdout(Stdio::piped());
    let Ok(mut child) = command.spawn() else {
        return Vec::new();
    };
    let Some(mut stdout) = child.stdout.take() else {
        return Vec::new();
    };
    let mut counts = Counts::new();
    let mut chunk = vec![0u8; CHUNK];
    loop {
        match stdout.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => counts.feed(&chunk[..read]),
            Err(_) => break,
        }
    }
    // A stream cut short still names a file in its tail.
    counts.flush();
    drop(stdout);
    let _ = child.wait().await;
    counts.rank()
}

/// The per-extension counts of one `git ls-files -z` stream.
struct Counts {
    /// The bytes after the last NUL of the previous chunk.
    rest: Vec<u8>,
    /// Files per [`SOURCES`] row.
    files: Vec<u32>,
}

impl Counts {
    fn new() -> Self {
        Counts {
            rest: Vec::new(),
            files: vec![0; SOURCES.len()],
        }
    }

    /// Counts every complete NUL-separated name in `bytes` and keeps the tail,
    /// which the next chunk completes.
    fn feed(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while let Some(end) = rest.iter().position(|&byte| byte == 0) {
            let name = &rest[..end];
            if self.rest.is_empty() {
                count_path(name, &mut self.files);
            } else {
                self.rest.extend_from_slice(name);
                count_path(&self.rest, &mut self.files);
                self.rest.clear();
            }
            rest = &rest[end + 1..];
        }
        self.rest.extend_from_slice(rest);
    }

    /// Counts a last name that the stream did not NUL-terminate.
    fn flush(&mut self) {
        if self.rest.is_empty() {
            return;
        }
        let rest = std::mem::take(&mut self.rest);
        count_path(&rest, &mut self.files);
    }

    /// The counted languages with at least 10% of the files, at most 3, most
    /// files first and ties by name.
    fn rank(&self) -> Vec<Language> {
        let mut total: u32 = 0;
        let mut languages: Vec<(&'static str, &'static [&'static str], u32)> = Vec::new();
        for (row, &(_, name, tools)) in SOURCES.iter().enumerate() {
            let files = self.files[row];
            if files == 0 {
                continue;
            }
            total += files;
            match languages.iter_mut().find(|language| language.0 == name) {
                Some(language) => language.2 += files,
                None => languages.push((name, tools, files)),
            }
        }
        let all = u64::from(total);
        // The 3 most common, ties by name, kept by hand: a sort of the rows
        // would add a sort's code for at most a few dozen entries.
        let mut top: Vec<(&'static str, &'static [&'static str], u32)> = Vec::with_capacity(4);
        for language in languages {
            if u64::from(language.2) * 10 < all {
                continue;
            }
            let at = top
                .iter()
                .position(|kept| (language.2, kept.0) > (kept.2, language.0))
                .unwrap_or(top.len());
            top.insert(at, language);
            top.truncate(3);
        }
        top.into_iter()
            .map(|(name, tools, _)| (name, tools))
            .collect()
    }
}

/// Counts one tracked path, skipping the folders and the minified files that
/// say nothing about the languages.
fn count_path(path: &[u8], files: &mut [u32]) {
    if path.ends_with(b".min.js") || path.ends_with(b".min.css") {
        return;
    }
    let mut name = path;
    for component in path.split(|&byte| byte == b'/') {
        if SKIP_FOLDERS.contains(&component) {
            return;
        }
        name = component;
    }
    let Some(dot) = name.iter().rposition(|&byte| byte == b'.') else {
        return;
    };
    let extension = &name[dot + 1..];
    if let Some(row) = SOURCES
        .iter()
        .position(|(known, _, _)| known.as_bytes() == extension)
    {
        files[row] += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `count` tracked names with `ext`.
    fn source(count: usize, ext: &str) -> Vec<String> {
        (0..count).map(|i| format!("src/f{i}.{ext}")).collect()
    }

    /// `count` tracked names under `folder`.
    fn under(folder: &str, count: usize, ext: &str) -> Vec<String> {
        (0..count).map(|i| format!("{folder}/f{i}.{ext}")).collect()
    }

    /// A `git ls-files -z` stream of `names`.
    fn stream(names: &[String]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for name in names {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    /// The ranked languages of a path stream, fed in two chunks so a name that
    /// crosses a chunk boundary is joined.
    fn counted(bytes: &[u8]) -> Vec<Language> {
        let mut counts = Counts::new();
        let split = bytes.len() / 2;
        counts.feed(&bytes[..split]);
        counts.feed(&bytes[split..]);
        counts.flush();
        counts.rank()
    }

    /// The `languages:` line of a path stream.
    fn languages(names: &[String]) -> String {
        language_line(&counted(&stream(names)))
    }

    #[test]
    fn snapshot_counts_go_and_typescript() {
        let mut names = source(60, "go");
        names.extend(source(35, "ts"));
        names.extend(source(5, "md"));
        assert_eq!(languages(&names), "Go (most), TypeScript");
    }

    #[test]
    fn snapshot_skips_vendored_files() {
        let mut names = source(50, "rs");
        names.extend(under("vendor/github.com/x", 500, "js"));
        assert_eq!(languages(&names), "Rust (most)");
    }

    #[test]
    fn snapshot_skips_minified_files() {
        let mut names = source(10, "ts");
        names.extend((0..10).map(|i| format!("static/app{i}.min.js")));
        assert_eq!(languages(&names), "TypeScript (most)");
    }

    #[test]
    fn snapshot_lists_at_most_three_languages() {
        let mut names = source(30, "rs");
        names.extend(source(25, "ts"));
        names.extend(source(25, "go"));
        names.extend(source(20, "py"));
        assert_eq!(languages(&names), "Rust (most), Go, TypeScript");
    }

    #[test]
    fn snapshot_drops_languages_under_ten_percent() {
        let mut names = source(90, "rs");
        names.extend(source(5, "py"));
        names.extend(source(5, "md"));
        assert_eq!(languages(&names), "Rust (most)");
    }

    #[test]
    fn snapshot_breaks_ties_by_name() {
        let mut names = source(10, "ts");
        names.extend(source(10, "go"));
        assert_eq!(languages(&names), "Go (most), TypeScript");
    }

    #[test]
    fn snapshot_counts_no_language_out_of_nothing() {
        assert_eq!(languages(&[]), "");
        assert_eq!(languages(&source(4, "md")), "");
    }

    /// A folder of fake tools with the given mode bits.
    #[cfg(unix)]
    async fn fake_tools(tools: &[(&str, u32)]) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, mode) in tools {
            let path = dir.path().join(name);
            tokio::fs::write(&path, "#!/bin/sh\n")
                .await
                .expect("write tool");
            let mut permissions = tokio::fs::metadata(&path)
                .await
                .expect("tool metadata")
                .permissions();
            permissions.set_mode(*mode);
            tokio::fs::set_permissions(&path, permissions)
                .await
                .expect("chmod tool");
        }
        dir
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_tools_line_names_what_is_missing() {
        let dir = fake_tools(&[("node", 0o755)]).await;
        let path = std::env::join_paths([dir.path()]).expect("join path");
        let languages: [Language; 2] = [("Go", &["go"]), ("TypeScript", &["node", "bun"])];
        assert_eq!(
            tools_line(&languages, Some(&path)).await,
            "tools: node; missing: Go (go)"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_tools_line_says_none_missing() {
        let dir = fake_tools(&[("go", 0o755), ("bun", 0o755)]).await;
        let path = std::env::join_paths([dir.path()]).expect("join path");
        let languages: [Language; 2] = [("Go", &["go"]), ("TypeScript", &["node", "bun"])];
        assert_eq!(
            tools_line(&languages, Some(&path)).await,
            "tools: go, bun; missing: none"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_tools_line_ignores_a_plain_file() {
        let dir = fake_tools(&[("go", 0o644)]).await;
        let path = std::env::join_paths([dir.path()]).expect("join path");
        let languages: [Language; 1] = [("Go", &["go"])];
        assert_eq!(
            tools_line(&languages, Some(&path)).await,
            "tools: none; missing: Go (go)"
        );
    }

    #[test]
    fn snapshot_memory_is_whole_gigabytes() {
        assert_eq!(giga(34_359_738_368), 32);
        assert_eq!(giga(33_015_035_904), 31);
        assert_eq!(giga(0), 0);
        assert!(os_line(Some(34_359_738_368)).ends_with(", 32G mem"));
        assert!(!os_line(None).contains("mem"));
    }

    #[test]
    fn snapshot_reads_mem_total() {
        let text = "MemTotal:       16384000 kB\nMemFree: 10 kB\n";
        assert_eq!(meminfo_total(text), Some(16_777_216_000));
        assert_eq!(meminfo_total("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn snapshot_spells_the_arch_out() {
        assert_eq!(arch("aarch64"), "arm64");
        assert_eq!(arch("x86_64"), "x86_64");
    }

    /// Runs git in `dir` with the user config out of the way.
    async fn git_run(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .await
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn snapshot_block_names_the_repo_and_its_branch() {
        let dir = tempfile::tempdir().expect("temp dir");
        git_run(dir.path(), &["init", "-b", "trunk"]).await;
        tokio::fs::create_dir_all(dir.path().join("src"))
            .await
            .expect("create src");
        for name in source(12, "go") {
            tokio::fs::write(dir.path().join(&name), "package main\n")
                .await
                .expect("write source");
        }
        git_run(dir.path(), &["add", "-A"]).await;
        git_run(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "commit",
                "-m",
                "init",
            ],
        )
        .await;
        let cwd = tokio::fs::canonicalize(dir.path())
            .await
            .expect("canonical");
        let block = probe(&cwd).await;
        assert!(block.starts_with("[environment]\nos: "), "{block}");
        assert!(
            block.contains(&format!("cwd: {} (git: trunk)", cwd.display())),
            "{block}"
        );
        assert!(block.contains("languages: Go (most)"), "{block}");
        assert!(block.contains("\ntools: "), "{block}");
        assert!(!block.ends_with('\n'), "{block:?}");
    }

    /// A script that creates `marker` when it runs, made executable.
    #[cfg(unix)]
    async fn marker_script(path: &Path, marker: &Path) {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::write(path, format!("#!/bin/sh\ntouch {}\n", marker.display()))
            .await
            .expect("write script");
        let mut permissions = tokio::fs::metadata(path)
            .await
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        tokio::fs::set_permissions(path, permissions)
            .await
            .expect("chmod script");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_probe_never_runs_the_repo_fsmonitor_program() {
        // A repo can name a program in `core.fsmonitor` and git runs it on the
        // commands the probe uses, so a cloned repo would run code on every
        // default run. The probe turns it off.
        let dir = tempfile::tempdir().expect("temp dir");
        git_run(dir.path(), &["init", "-b", "trunk"]).await;
        tokio::fs::write(dir.path().join("main.rs"), "fn main() {}\n")
            .await
            .expect("write source");
        git_run(dir.path(), &["add", "-A"]).await;
        let marker = dir.path().join("fsmonitor-ran");
        let script = dir.path().join("fsmonitor.sh");
        marker_script(&script, &marker).await;
        git_run(
            dir.path(),
            &[
                "config",
                "core.fsmonitor",
                script.to_str().expect("script path"),
            ],
        )
        .await;
        let cwd = tokio::fs::canonicalize(dir.path())
            .await
            .expect("canonical");
        let block = probe(&cwd).await;
        assert!(block.contains("(git: trunk)"), "{block}");
        assert!(
            !tokio::fs::try_exists(&marker).await.unwrap_or(false),
            "the probe ran the repo's core.fsmonitor program"
        );
    }

    #[tokio::test]
    async fn snapshot_block_names_a_branch_with_no_commit() {
        // A fresh repo has no commit, so `rev-parse` names nothing, but its
        // `HEAD` is still a symbolic ref to the branch it will commit to.
        let dir = tempfile::tempdir().expect("temp dir");
        git_run(dir.path(), &["init", "-b", "trunk"]).await;
        let cwd = tokio::fs::canonicalize(dir.path())
            .await
            .expect("canonical");
        let block = probe(&cwd).await;
        assert!(
            block.contains(&format!("cwd: {} (git: trunk)", cwd.display())),
            "{block}"
        );
        assert!(!block.contains("languages:"), "{block}");
    }

    #[tokio::test]
    async fn snapshot_block_calls_a_detached_head_detached() {
        let dir = tempfile::tempdir().expect("temp dir");
        git_run(dir.path(), &["init", "-b", "trunk"]).await;
        tokio::fs::write(dir.path().join("main.rs"), "fn main() {}\n")
            .await
            .expect("write source");
        git_run(dir.path(), &["add", "-A"]).await;
        git_run(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "commit",
                "-m",
                "init",
            ],
        )
        .await;
        git_run(dir.path(), &["checkout", "--detach"]).await;
        let cwd = tokio::fs::canonicalize(dir.path())
            .await
            .expect("canonical");
        let block = probe(&cwd).await;
        assert!(
            block.contains(&format!("cwd: {} (git: detached)", cwd.display())),
            "{block}"
        );
    }

    #[tokio::test]
    async fn snapshot_block_outside_git_has_no_languages() {
        let dir = tempfile::tempdir().expect("temp dir");
        tokio::fs::write(dir.path().join("main.rs"), "fn main() {}\n")
            .await
            .expect("write source");
        let cwd = tokio::fs::canonicalize(dir.path())
            .await
            .expect("canonical");
        let block = probe(&cwd).await;
        assert!(
            block.contains(&format!("cwd: {}", cwd.display())),
            "{block}"
        );
        assert!(!block.contains("(git:"), "{block}");
        assert!(!block.contains("languages:"), "{block}");
        assert!(!block.contains("tools:"), "{block}");
    }

    #[tokio::test]
    async fn snapshot_probe_keeps_the_ready_lines() {
        // A folder that does not exist has no git and no sources, but the os
        // line is still there: the probe never fails a run.
        let dir = std::env::temp_dir().join("reagent-snapshot-absent-folder");
        let block = probe(&dir).await;
        assert!(block.starts_with("[environment]\nos: "), "{block}");
        assert!(
            block.contains(&format!("cwd: {}", dir.display())),
            "{block}"
        );
    }

    #[test]
    fn snapshot_the_os_line_puts_the_arch_next_to_the_os() {
        let line = os_line(Some(48 * GIB));
        let expected = format!(
            "os: {} {}, ",
            std::env::consts::OS,
            arch(std::env::consts::ARCH)
        );
        assert!(line.starts_with(&expected), "{line}");
        assert!(line.ends_with(" cpu, 48G mem"), "{line}");
        assert!(!os_line(None).contains("mem"));
    }

    #[test]
    fn snapshot_head_names_a_branch_or_detached() {
        assert_eq!(
            head_branch("ref: refs/heads/trunk\n").as_deref(),
            Some("trunk")
        );
        assert_eq!(
            head_branch("ref: refs/heads/feat/x\n").as_deref(),
            Some("feat/x")
        );
        assert_eq!(
            head_branch("1f2e3d4c5b6a79881f2e3d4c5b6a79881f2e3d4c\n").as_deref(),
            Some("detached")
        );
        assert_eq!(head_branch(""), None);
        assert_eq!(head_branch("garbage"), None);
    }

    #[tokio::test]
    async fn snapshot_a_worktree_git_file_finds_its_git_folder() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let real = tmp.path().join("real-git");
        tokio::fs::create_dir_all(&real).await.expect("git folder");
        tokio::fs::write(real.join("HEAD"), "ref: refs/heads/wt\n")
            .await
            .expect("HEAD");
        let work = tmp.path().join("work");
        tokio::fs::create_dir_all(work.join("sub"))
            .await
            .expect("work folder");
        tokio::fs::write(work.join(".git"), format!("gitdir: {}\n", real.display()))
            .await
            .expect(".git file");
        let dir = git_dir(&work.join("sub"))
            .await
            .expect("finds the git folder");
        assert_eq!(branch(Some(&dir)).await.as_deref(), Some("wt"));
        assert_eq!(git_dir(tmp.path()).await, None);
    }
}
