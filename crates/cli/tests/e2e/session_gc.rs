//! End-to-end tests for deleting saved sessions by age and by size.
//!
//! Each test runs the binary with a scripted fake provider against a temp
//! state and config folder, then ages the sessions it made by setting their
//! part files' modification times, so no test touches the real sessions.

#![cfg(feature = "testing")]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, SystemTime};

use serde_json::json;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");
/// The seconds of a day, which is how far a test backdates a file.
const DAY: u64 = 86_400;
/// The bytes of a KiB.
const KIB: u64 = 1024;
/// Characters of a text a session cannot compress away: 600 KiB of it or more
/// reach the part file.
const NOISE: usize = 1_000_000;

/// A temp home with a temp state folder, a temp config folder and a temp
/// working folder.
struct Home {
    root: TempDir,
}

impl Home {
    fn new() -> Home {
        Home {
            root: tempfile::tempdir().expect("a temp dir"),
        }
    }

    /// The folder the sessions of a run are written under.
    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    /// The working folder a run sees.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test makes the folder it runs in"
    )]
    fn cwd(&self) -> PathBuf {
        let cwd = self.root.path().join("work");
        std::fs::create_dir_all(&cwd).expect("create the working folder");
        cwd
    }

    /// Writes a fake provider script and returns its path.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test writes its own provider script"
    )]
    fn script(&self, name: &str, replies: serde_json::Value) -> PathBuf {
        let path = self.root.path().join(name);
        std::fs::write(&path, json!({ "replies": replies }).to_string()).expect("write script");
        path
    }

    /// Writes the config file of the isolated home.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test writes its own config folder"
    )]
    fn config(&self, text: &str) {
        let dir = self.root.path().join("config/reagent");
        std::fs::create_dir_all(&dir).expect("create the config folder");
        std::fs::write(dir.join("config.toml"), text).expect("write the config");
    }

    /// The folder that holds the sessions of a working folder.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test names the session folder of its own temp folder"
    )]
    fn folder(&self, cwd: &Path) -> PathBuf {
        let cwd = std::fs::canonicalize(cwd).expect("canonical working folder");
        self.state()
            .join("reagent/sessions")
            .join(reagent_store::paths::cwd_key(&cwd))
    }

    /// The stamp file that says when a run last swept the sessions.
    fn stamp(&self) -> PathBuf {
        self.state().join("reagent/sessions/gc.stamp")
    }

    /// A command for one run of the binary in `cwd`.
    fn command(&self, args: &[&str], cwd: &Path) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(cwd)
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.state())
            .env_remove("REAGENT_PROFILE")
            .env_remove("REAGENT_MODELS_DEV_URL")
            .env("REAGENT_MODELS_DEV_URL", "http://127.0.0.1:1");
        command
    }

    /// Runs the binary in the working folder.
    async fn run(&self, args: &[&str]) -> Output {
        let cwd = self.cwd();
        self.run_in(&cwd, args).await
    }

    /// Runs the binary in `cwd`.
    async fn run_in(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(args, cwd)
            .stdin(Stdio::null())
            .output()
            .await
            .expect("run the binary")
    }

    /// Runs the binary with `input` piped to its stdin.
    async fn run_stdin(&self, cwd: &Path, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command(args, cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the binary");
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(input.as_bytes())
            .await
            .expect("write the task");
        drop(child.stdin.take());
        child.wait_with_output().await.expect("the run completes")
    }

    /// Runs one saved session in `cwd` and returns its id.
    async fn session(&self, provider: &str, task: &str) -> String {
        let cwd = self.cwd();
        let before = self.ids(&cwd);
        let output = self
            .run_in(&cwd, &["-p", task, "--provider", provider])
            .await;
        assert!(output.status.success(), "{}", err(&output));
        let ids = self.ids(&cwd);
        let fresh: Vec<&String> = ids.iter().filter(|id| !before.contains(id)).collect();
        assert_eq!(fresh.len(), 1, "one session was written: {ids:?}");
        fresh[0].clone()
    }

    /// Runs one saved session with a text of its own as the task.
    async fn big_session(&self, provider: &str, task: &str) -> String {
        let cwd = self.cwd();
        let before = self.ids(&cwd);
        let output = self
            .run_stdin(&cwd, &["-p", "-", "--provider", provider], task)
            .await;
        assert!(output.status.success(), "{}", err(&output));
        let ids = self.ids(&cwd);
        let fresh: Vec<&String> = ids.iter().filter(|id| !before.contains(id)).collect();
        assert_eq!(fresh.len(), 1, "one session was written: {ids:?}");
        fresh[0].clone()
    }

    /// The session folders of a working folder.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test lists the session folder it made"
    )]
    fn sessions(&self, cwd: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(self.folder(cwd))
            .map(|dir| {
                dir.filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    /// The ids of the sessions of a working folder, oldest first.
    fn ids(&self, cwd: &Path) -> Vec<String> {
        let mut ids = self
            .sessions(cwd)
            .iter()
            .filter_map(|dir| dir.file_name()?.to_str().map(str::to_owned))
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    /// The bytes each session folder holds, with its folder.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test weighs the session folders it made"
    )]
    fn bytes(&self, cwd: &Path) -> Vec<(PathBuf, u64)> {
        self.sessions(cwd)
            .into_iter()
            .map(|dir| {
                let size = std::fs::read_dir(&dir)
                    .map(|parts| {
                        parts
                            .filter_map(|part| part.ok())
                            .map(|part| std::fs::metadata(part.path()).map_or(0, |meta| meta.len()))
                            .sum::<u64>()
                    })
                    .unwrap_or_default();
                (dir, size)
            })
            .collect()
    }

    /// Sets the part files of a session back `days` days.
    #[expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "a test backdates the session it made"
    )]
    fn age(&self, cwd: &Path, id: &str, days: u64) {
        let dir = self.folder(cwd).join(id);
        let old = SystemTime::now() - Duration::from_secs(days * DAY);
        for entry in std::fs::read_dir(&dir).expect("the session folder") {
            let path = entry.expect("a part file").path();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open a part file")
                .set_modified(old)
                .expect("backdate a part file");
        }
    }

    /// Sets the stamp of the last sweep back, or makes it when it is missing.
    #[expect(
        clippy::disallowed_types,
        reason = "a test backdates the stamp of its own state folder"
    )]
    fn backdate_stamp(&self, age: Duration) {
        let path = self.stamp();
        let file = std::fs::File::options()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .expect("open the stamp");
        file.set_modified(SystemTime::now() - age)
            .expect("backdate the stamp");
    }

    /// How long ago the stamp of the last sweep was written, in seconds.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test reads the stamp of its own state folder"
    )]
    fn stamp_age(&self) -> u64 {
        std::fs::metadata(self.stamp())
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|stamped| SystemTime::now().duration_since(stamped).ok())
            .map_or(0, |age| age.as_secs())
    }

    /// Runs `session gc` in the working folder and returns its output.
    async fn gc(&self) -> Output {
        self.run(&["session", "gc"]).await
    }
}

/// The stdout of a run, as text.
fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a run, as text.
fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The bytes the `deleted <n> sessions, <bytes> bytes` line of gc names.
fn freed_bytes(text: &str) -> u64 {
    let line = text.trim();
    line.rsplit_once(' ')
        .and_then(|(head, _)| head.rsplit_once(' '))
        .and_then(|(_, bytes)| bytes.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("gc printed {line:?}"))
}

/// One text reply for the fake provider.
fn reply(text: &str) -> serde_json::Value {
    json!([{ "text": text }])
}

/// A text of [`NOISE`] characters a compressor cannot shrink: the part file of
/// a session that holds it takes more than half a MiB.
fn noise() -> String {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut text = String::with_capacity(NOISE);
    for _ in 0..NOISE {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let byte = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
            [(state % 62) as usize];
        text.push(char::from(byte));
    }
    text
}

/// A session older than `keep_days` is deleted, and one written today stays.
#[tokio::test]
async fn an_old_session_is_deleted_and_a_new_one_is_kept() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let old = home.session(&provider, "old").await;
    let fresh = home.session(&provider, "fresh").await;
    home.age(&cwd, &old, 31);

    let output = home.gc().await;
    assert!(output.status.success(), "{}", err(&output));
    assert!(
        out(&output).starts_with("deleted 1 sessions, "),
        "{}",
        out(&output)
    );
    assert!(freed_bytes(&out(&output)) > 0, "{}", out(&output));
    assert_eq!(home.ids(&cwd), vec![fresh.clone()], "the old one stayed");
    let listed = home.run(&["session", "ls"]).await;
    assert!(listed.status.success(), "{}", err(&listed));
    let text = out(&listed);
    assert!(text.contains(&fresh), "{text}");
    assert!(!text.contains(&old), "{text}");
}

/// `session rm` refuses a session a writer holds, whichever part it holds:
/// part ids are random, so the held part can sort first.
#[tokio::test]
async fn rm_refuses_a_session_whose_first_sorted_part_is_held() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let id = home.session(&provider, "held").await;
    let dir = home.folder(&cwd).join(&id);
    let mut parts = reagent_store::paths::part_ids(&dir).expect("the part files");
    parts.sort_unstable();
    let held = parts.first().copied().unwrap_or(2).max(2) - 1;
    let holder = reagent_store::Writer::create(
        &dir,
        reagent_store::Header {
            part_id: held,
            session_id: id.clone(),
            created_ms: 1,
            fork: None,
            meta: Default::default(),
        },
    )
    .expect("a second part");
    let output = home.run(&["session", "rm", &id]).await;
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert_eq!(home.ids(&cwd), vec![id.clone()], "a session in use stays");
    holder.close().expect("close the holder");
}

/// A continued session points its new part at its own older one; that is not
/// a fork parent, so the session still goes when it is old.
#[tokio::test]
async fn an_old_continued_session_is_deleted() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let id = home.session(&provider, "first").await;
    let output = home
        .run(&["-c", "-p", "second", "--provider", &provider])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    assert_eq!(
        home.ids(&cwd),
        vec![id.clone()],
        "the continue kept one session"
    );
    home.age(&cwd, &id, 31);

    let output = home.gc().await;
    assert!(output.status.success(), "{}", err(&output));
    assert!(
        out(&output).starts_with("deleted 1 sessions, "),
        "{}",
        out(&output)
    );
    assert!(home.ids(&cwd).is_empty(), "the continued session stayed");
}

/// `keep_days = 0` turns the age rule off.
#[tokio::test]
async fn an_age_limit_of_zero_keeps_a_very_old_session() {
    let home = Home::new();
    home.config("[session]\nkeep_days = 0\n");
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let old = home.session(&provider, "old").await;
    home.age(&cwd, &old, 400);

    let output = home.gc().await;
    assert!(output.status.success(), "{}", err(&output));
    assert_eq!(
        out(&output).trim(),
        "deleted 0 sessions, 0 bytes",
        "{}",
        out(&output)
    );
    assert_eq!(home.ids(&cwd), vec![old], "the session of any age stays");
}

/// Over the size cap, the least recently written sessions go first.
#[tokio::test]
async fn the_size_cap_deletes_the_oldest_sessions() {
    let home = Home::new();
    home.config("[session]\nmax_mb = 1\n");
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let text = noise();
    let first = home.big_session(&provider, &text).await;
    let second = home.big_session(&provider, &text).await;
    let third = home.big_session(&provider, &text).await;
    for (dir, size) in home.bytes(&cwd) {
        assert!(size > 600 * KIB, "{} is only {size} bytes", dir.display());
    }

    let output = home.gc().await;
    assert!(output.status.success(), "{}", err(&output));
    assert!(
        out(&output).starts_with("deleted 2 sessions, "),
        "{}",
        out(&output)
    );
    assert!(freed_bytes(&out(&output)) > 1024 * KIB, "{}", out(&output));
    assert_eq!(
        home.ids(&cwd),
        vec![third],
        "the newest session stays, the two older ones went ({first}, {second})"
    );
}

/// Under the size cap nothing is deleted.
#[tokio::test]
async fn under_the_size_cap_nothing_is_deleted() {
    let home = Home::new();
    home.config("[session]\nmax_mb = 1\n");
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let text = noise();
    let kept = home.big_session(&provider, &text).await;
    for (dir, size) in home.bytes(&cwd) {
        assert!(size > 600 * KIB, "{} is only {size} bytes", dir.display());
        assert!(size < 1024 * KIB, "{} is {size} bytes", dir.display());
    }

    let output = home.gc().await;
    assert!(output.status.success(), "{}", err(&output));
    assert_eq!(
        out(&output).trim(),
        "deleted 0 sessions, 0 bytes",
        "{}",
        out(&output)
    );
    assert_eq!(home.ids(&cwd), vec![kept]);
}

/// A session a writer holds open is never deleted.
#[tokio::test]
async fn a_session_in_use_is_kept() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let id = home.session(&provider, "held").await;
    let dir = home.folder(&cwd).join(&id);
    let mut parts = reagent_store::paths::part_ids(&dir).expect("the part files");
    parts.sort_unstable();
    // Part ids are random, so the part a writer holds can sort first.
    let held = parts.first().copied().unwrap_or(2).max(2) - 1;
    let holder = reagent_store::Writer::create(
        &dir,
        reagent_store::Header {
            part_id: held,
            session_id: id.clone(),
            created_ms: 1,
            fork: None,
            meta: Default::default(),
        },
    )
    .expect("a second part");
    // The lock is what keeps the session, not its age: every part of it,
    // the one the writer holds included, is a month old.
    home.age(&cwd, &id, 31);

    let output = home.gc().await;
    assert!(output.status.success(), "{}", err(&output));
    assert_eq!(
        out(&output).trim(),
        "deleted 0 sessions, 0 bytes",
        "{}",
        out(&output)
    );
    assert_eq!(home.ids(&cwd), vec![id.clone()], "a session in use stays");

    // Closing joins the writer thread, so the lock is gone when it returns.
    holder.close().expect("close the holder");
    home.age(&cwd, &id, 31);
    let output = home.gc().await;
    assert!(output.status.success(), "{}", err(&output));
    assert!(
        out(&output).starts_with("deleted 1 sessions, "),
        "{}",
        out(&output)
    );
    assert!(home.ids(&cwd).is_empty(), "the session went");
}

/// A fork parent stays while the fork that reads it stays.
#[tokio::test]
async fn a_fork_parent_of_a_fresh_fork_is_kept() {
    let home = Home::new();
    let script = home.script(
        "script.json",
        json!([
            { "calls": [{ "name": "bash", "arguments": { "command": "echo one" } }] },
            { "text": "first turn" },
            { "text": "forked" },
            { "text": "resumed" }
        ]),
    );
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let parent = home.session(&provider, "one").await;
    let reference = format!("{parent}@1");
    let before = home.ids(&cwd);
    let forked = home
        .run(&["--fork", &reference, "-p", "two", "--provider", &provider])
        .await;
    assert!(forked.status.success(), "{}", err(&forked));
    let fork = home
        .ids(&cwd)
        .into_iter()
        .find(|id| !before.contains(id))
        .expect("the fork");
    home.age(&cwd, &parent, 40);

    let output = home.gc().await;
    assert!(output.status.success(), "{}", err(&output));
    assert_eq!(
        out(&output).trim(),
        "deleted 0 sessions, 0 bytes",
        "{}",
        out(&output)
    );
    assert_eq!(home.ids(&cwd), {
        let mut ids = vec![parent.clone(), fork.clone()];
        ids.sort();
        ids
    });

    // The fork still reads the turns the parent wrote.
    let resumed = home
        .run(&["--resume", &fork, "-p", "go", "--provider", &provider])
        .await;
    assert!(resumed.status.success(), "{}", err(&resumed));
    let log = home.run(&["session", "cat", &fork]).await;
    assert!(log.status.success(), "{}", err(&log));
    let text = out(&log);
    assert!(text.contains("first turn"), "{text}");
    assert!(text.contains("go"), "{text}");
}

/// The first saved run of a day sweeps the sessions.
#[tokio::test]
async fn the_first_run_of_the_day_deletes_an_old_session() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let old = home.session(&provider, "old").await;
    home.age(&cwd, &old, 31);
    home.backdate_stamp(Duration::from_secs(25 * 3600));

    let fresh = home.session(&provider, "fresh").await;
    // The sweep runs after the output, so the run's own session is what the
    // listing shows next to nothing else.
    assert_eq!(home.ids(&cwd), vec![fresh], "the old session stayed");
}

/// A second saved run within a day does not sweep again.
#[tokio::test]
async fn a_second_run_within_a_day_keeps_an_old_session() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let old = home.session(&provider, "old").await;
    assert!(home.stamp().exists(), "a saved run writes the stamp");
    home.age(&cwd, &old, 31);
    home.backdate_stamp(Duration::from_secs(3600));
    let stamped = home.stamp_age();

    let fresh = home.session(&provider, "fresh").await;
    let ids = home.ids(&cwd);
    assert!(ids.contains(&old), "the old session went: {ids:?}");
    assert!(ids.contains(&fresh), "{ids:?}");
    assert!(
        home.stamp_age() >= stamped,
        "a run that skipped the sweep touched the stamp"
    );
}

/// A run that saves no session does not sweep.
#[tokio::test]
async fn a_run_without_a_session_keeps_an_old_session() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let old = home.session(&provider, "old").await;
    home.age(&cwd, &old, 31);
    home.backdate_stamp(Duration::from_secs(25 * 3600));

    let output = home
        .run(&["--no-session", "-p", "hi", "--provider", &provider])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    assert_eq!(home.ids(&cwd), vec![old], "the old session stayed");
}

/// `session gc` with nothing to delete says so, and touches nothing else.
#[tokio::test]
async fn gc_with_nothing_to_delete_says_so() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    let cwd = home.cwd();
    let kept = home.session(&provider, "kept").await;
    // Files gc must leave alone: one in the working folder, one beside the
    // state folder.
    let inside = cwd.join("notes.txt");
    let outside = home.root.path().join("beside.txt");
    tokio::fs::write(&inside, b"keep me").await.expect("a file");
    tokio::fs::write(&outside, b"keep me")
        .await
        .expect("a file");

    let output = home.gc().await;
    assert!(output.status.success(), "{}", err(&output));
    assert_eq!(
        out(&output).trim(),
        "deleted 0 sessions, 0 bytes",
        "{}",
        out(&output)
    );
    assert_eq!(home.ids(&cwd), vec![kept]);
    assert!(inside.exists(), "a file of the working folder went");
    assert!(outside.exists(), "a file beside the state folder went");
}
