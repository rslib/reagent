//! End-to-end tests for the session flags and the `session` command.
//!
//! Each test runs the binary with a scripted fake provider against a temp
//! state and config folder, so no test touches the real sessions or a network.
//! The provider is scripted, so these tests exist only with `testing` on.

#![cfg(feature = "testing")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// How long a test waits for a line of a worker before it fails.
const WAIT: Duration = Duration::from_secs(30);

/// A temp home the runs write their sessions in, and a temp working folder.
struct Home {
    root: TempDir,
}

impl Home {
    fn new() -> Home {
        Home {
            root: tempfile::tempdir().expect("a temp dir"),
        }
    }

    /// A home in a short folder, so the socket path of a worker fits the length
    /// the platform caps it at.
    fn short() -> Home {
        Home {
            root: tempfile::Builder::new()
                .prefix("rw")
                .tempdir_in("/tmp")
                .expect("a temp folder"),
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

    /// Writes a config profile that a run may name with `--profile`.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test writes its own config folder"
    )]
    fn profile(&self, name: &str) {
        let dir = self.root.path().join("config/reagent/profiles");
        std::fs::create_dir_all(&dir).expect("create the profiles folder");
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, "max_tokens = 1000\n").expect("write the profile");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("restrict the profile");
    }

    /// Runs the binary with the home isolated and the arguments given.
    async fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.cwd(), args).await
    }

    /// Runs the binary in `cwd`.
    async fn run_in(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(args, cwd)
            .stdin(Stdio::null())
            .output()
            .await
            .expect("run the binary")
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
            .env_remove("REAGENT_MODELS_DEV_URL")
            .env("REAGENT_MODELS_DEV_URL", "http://127.0.0.1:1");
        command
    }

    /// Starts a worker in `cwd` with `args` after its own word, and reads its
    /// ready line.
    async fn worker(&self, cwd: &Path, args: &[&str]) -> Worker {
        let mut words = vec!["worker"];
        words.extend_from_slice(args);
        let mut child = self
            .command(&words, cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the worker starts");
        let mut stdout = tokio::io::BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut line = String::new();
        tokio::time::timeout(WAIT, stdout.read_line(&mut line))
            .await
            .expect("a ready line in time")
            .expect("the worker writes a line");
        let ready: serde_json::Value =
            serde_json::from_str(line.trim_end()).expect("the ready line is JSON");
        Worker { child, ready }
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

    /// The ids of the sessions of a working folder.
    fn ids(&self, cwd: &Path) -> Vec<String> {
        let mut ids = self
            .sessions(cwd)
            .iter()
            .filter_map(|dir| dir.file_name()?.to_str().map(str::to_owned))
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }
}

/// The stdout of a run, as text.
fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The part header field that keeps the headless mark of a run.
const META_HEADLESS: u32 = 6;

/// The headless mark the part header of a run keeps.
fn marked_headless(header: &reagent_store::Header) -> bool {
    let mut decoder = reagent_store::Decoder::new(&header.meta);
    while let Ok(Some((field, value))) = decoder.next_field() {
        if field == META_HEADLESS {
            return value.varint() == Some(1);
        }
    }
    false
}

/// The entry a rebuilt list shows for a part, the mark from its header.
fn part_entry(
    header: &reagent_store::Header,
    _commit: Option<&reagent_store::part::Commit>,
) -> reagent_store::Entry {
    reagent_store::Entry {
        session: header.session_id.clone(),
        updated_ms: header.created_ms,
        turns: 0,
        title: String::new(),
        profile: String::new(),
        provider: String::new(),
        model: String::new(),
        part: header.part_id,
        state: reagent_store::State::Interrupted,
        headless: marked_headless(header),
    }
}

/// The sessions the list of `cwd` holds, newest first.
fn listed(home: &Home, cwd: &Path) -> Vec<reagent_store::Entry> {
    reagent_store::SessionList::open(&home.folder(cwd), part_entry)
        .expect("the session list")
        .all()
        .to_vec()
}

/// Removes the list file of `folder`, so the next read rebuilds it.
#[expect(
    clippy::disallowed_methods,
    reason = "a test removes the list file it made"
)]
fn remove_list(folder: &Path) {
    std::fs::remove_file(reagent_store::paths::list_path(folder)).expect("remove the list");
}

/// The stderr of a run, as text.
fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// One text reply for the fake provider.
fn reply(text: &str) -> serde_json::Value {
    json!([{ "text": text }])
}

/// A run without a session writes no folder.
#[tokio::test]
async fn no_session_writes_nothing() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let cwd = home.cwd();
    let output = home
        .run(&[
            "-p",
            "hello",
            "--provider",
            &format!("fake:{script}"),
            "--no-session",
        ])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    assert!(home.ids(&cwd).is_empty(), "a session was written");
}

/// A plain run is saved and listed.
#[tokio::test]
async fn a_plain_run_is_saved_and_listed() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let cwd = home.cwd();
    let provider = format!("fake:{script}");
    let output = home
        .run(&["-p", "hello world", "--provider", &provider])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    let ids = home.ids(&cwd);
    assert_eq!(ids.len(), 1, "one session was written: {ids:?}");
    let listed = home.run(&["session", "ls"]).await;
    assert!(listed.status.success(), "{}", err(&listed));
    let text = out(&listed);
    assert!(text.contains(&ids[0]), "{text}");
    assert!(text.contains("hello world"), "{text}");
    assert!(text.contains("1 turn"), "{text}");
    // A session with no folder says nothing.
    let empty = Home::new();
    let listed = empty.run(&["session", "ls"]).await;
    assert_eq!(out(&listed), "");
}

/// A `-p` run is marked headless in the list; a worker a client drives is not.
#[tokio::test]
async fn headless_session_is_marked_in_the_list() {
    let home = Home::short();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let cwd = home.cwd();
    let provider = format!("fake:{script}");
    let output = home.run(&["-p", "hello", "--provider", &provider]).await;
    assert!(output.status.success(), "{}", err(&output));
    let printed = home.ids(&cwd).remove(0);
    let entries = listed(&home, &cwd);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(entries[0].headless, "a -p run is headless");
    assert_eq!(entries[0].session, printed);

    // A worker a terminal UI starts has no task, so its session is not one a
    // headless run wrote. It keeps the session only once a client sends a
    // message.
    let worker = home.worker(&cwd, &["--provider", &provider]).await;
    let mut client = Client::initialize(&worker).await;
    let sent = client
        .request("send", r#"{"text":"go","mode":"queue"}"#)
        .await;
    assert!(client.answer(sent).await["result"].is_object());
    client
        .until(|frame| frame["method"] == "event" && frame["params"]["type"] == "run_end")
        .await;
    drop(client);
    worker.stop().await;
    let entries = listed(&home, &cwd);
    assert_eq!(entries.len(), 2, "{entries:?}");
    let marked: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.headless)
        .map(|entry| entry.session.as_str())
        .collect();
    assert_eq!(
        marked,
        vec![printed.as_str()],
        "only the -p run is headless"
    );
}

/// A list rebuilt from the part headers keeps the mark of a headless run.
#[tokio::test]
async fn headless_session_rebuilt_from_parts_keeps_the_mark() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let cwd = home.cwd();
    let output = home
        .run(&["-p", "hello", "--provider", &format!("fake:{script}")])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    let id = home.ids(&cwd).remove(0);
    assert!(listed(&home, &cwd)[0].headless, "the run marks its entry");

    remove_list(&home.folder(&cwd));
    let entries = listed(&home, &cwd);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].session, id);
    assert!(entries[0].headless, "a rebuild keeps the mark");
}

/// A run whose first request fails is still listed.
#[tokio::test]
async fn a_failed_first_request_is_listed() {
    let home = Home::new();
    let script = home.script("script.json", json!([{ "fail": "boom" }]));
    let script = script.to_string_lossy().into_owned();
    let cwd = home.cwd();
    let output = home
        .run(&["-p", "hello", "--provider", &format!("fake:{script}")])
        .await;
    assert!(!output.status.success());
    assert_eq!(home.ids(&cwd).len(), 1, "the session is listed");
}

/// Continue picks the newer session.
#[tokio::test]
async fn continue_picks_the_newer_session() {
    let home = Home::new();
    let first = home.script("one.json", reply("first"));
    let second = home.script("two.json", reply("second"));
    let (first, second) = (
        first.to_string_lossy().into_owned(),
        second.to_string_lossy().into_owned(),
    );
    let cwd = home.cwd();
    assert!(
        home.run(&["-p", "older", "--provider", &format!("fake:{first}")])
            .await
            .status
            .success()
    );
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        home.run(&["-p", "newer", "--provider", &format!("fake:{second}")])
            .await
            .status
            .success()
    );
    let ids = home.ids(&cwd);
    assert_eq!(ids.len(), 2);
    let listed = out(&home.run(&["session", "ls"]).await);
    let mut lines = listed.lines();
    let newer = lines.next().expect("the newer session").to_owned();
    let older = lines.next().expect("the older session").to_owned();
    let id_of = |line: &str| {
        line.split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    assert!(newer.contains("newer"), "{listed}");
    assert!(older.contains("older"), "{listed}");
    // Continue with a message continues the newer session.
    let output = home
        .run(&["-c", "-p", "again", "--provider", &format!("fake:{second}")])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    // No new session folder was made, and the newer session gained a turn.
    assert_eq!(home.ids(&cwd).len(), 2);
    let listed = out(&home.run(&["session", "ls"]).await);
    let line = |id: &str| {
        listed
            .lines()
            .find(|line| line.starts_with(id))
            .unwrap_or_default()
            .to_owned()
    };
    assert!(line(&id_of(&newer)).contains("2 turns"), "{listed}");
    assert!(line(&id_of(&older)).contains("1 turn"), "{listed}");
}

/// `--profile` narrows which session `-c` continues.
#[tokio::test]
async fn a_profile_narrows_continue() {
    let home = Home::new();
    for name in ["work", "home", "third"] {
        home.profile(name);
    }
    let first = home.script("one.json", reply("one"));
    let second = home.script("two.json", reply("two"));
    let (first, second) = (
        first.to_string_lossy().into_owned(),
        second.to_string_lossy().into_owned(),
    );
    let cwd = home.cwd();
    assert!(
        home.run(&[
            "-p",
            "older",
            "--profile",
            "work",
            "--provider",
            &format!("fake:{first}")
        ])
        .await
        .status
        .success()
    );
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        home.run(&[
            "-p",
            "newer",
            "--profile",
            "home",
            "--provider",
            &format!("fake:{second}")
        ])
        .await
        .status
        .success()
    );
    let listed = out(&home.run(&["session", "ls"]).await);
    let mut lines = listed.lines();
    let newer = lines.next().expect("the newer session").to_owned();
    let older = lines.next().expect("the older session").to_owned();
    let id_of = |line: &str| {
        line.split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    assert!(newer.contains("newer"), "{listed}");
    assert!(older.contains("older"), "{listed}");
    // The work profile continues its own session, though the home one is newer.
    let output = home
        .run(&[
            "--profile",
            "work",
            "-c",
            "-p",
            "again",
            "--provider",
            &format!("fake:{first}"),
        ])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    let listed = out(&home.run(&["session", "ls"]).await);
    let line = |id: &str| {
        listed
            .lines()
            .find(|line| line.starts_with(id))
            .unwrap_or_default()
            .to_owned()
    };
    assert!(line(&id_of(&older)).contains("2 turns"), "{listed}");
    assert!(line(&id_of(&newer)).contains("1 turn"), "{listed}");
    // A profile that no session used has nothing to continue.
    let output = home
        .run(&[
            "-c",
            "-p",
            "again",
            "--profile",
            "third",
            "--provider",
            &format!("fake:{second}"),
        ])
        .await;
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(
        err(&output).contains("no session to continue for profile third"),
        "{}",
        err(&output)
    );
    assert_eq!(home.ids(&cwd).len(), 2, "a continue wrote a session");
}

/// A continue with no open turn and no message says there is nothing to do.
#[tokio::test]
async fn nothing_to_continue() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    assert!(
        home.run(&["-p", "hello", "--provider", &format!("fake:{script}")])
            .await
            .status
            .success()
    );
    let output = home.run(&["-c"]).await;
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(
        err(&output).contains("nothing to continue"),
        "{}",
        err(&output)
    );
    // With no session at all there is nothing to continue either.
    let empty = Home::new();
    let output = empty.run(&["-c", "-p", "hello"]).await;
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
}

/// A resume takes a unique id prefix, and names every match of an ambiguous one.
#[tokio::test]
async fn resume_by_prefix() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let cwd = home.cwd();
    assert!(
        home.run(&["-p", "hello", "--provider", &format!("fake:{script}")])
            .await
            .status
            .success()
    );
    let ids = home.ids(&cwd);
    let prefix = &ids[0][..4];
    let output = home
        .run(&[
            "-r",
            prefix,
            "-p",
            "more",
            "--provider",
            &format!("fake:{script}"),
        ])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    // The whole id is one match; a prefix that matches nothing fails, and a
    // path to the session folder is a reference too.
    let output = home.run(&["-r", "ZZZZZZ", "-p", "more"]).await;
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(
        err(&output).contains("no session matches"),
        "{}",
        err(&output)
    );
    let dir = home.sessions(&cwd).remove(0);
    let path = dir.to_string_lossy().into_owned();
    let output = home
        .run(&[
            "-r",
            &path,
            "-p",
            "more",
            "--provider",
            &format!("fake:{script}"),
        ])
        .await;
    assert!(output.status.success(), "{}", err(&output));
}

/// A prefix that matches two sessions names both of them.
#[tokio::test]
async fn an_ambiguous_prefix_names_both() {
    let home = Home::new();
    let cwd = home.cwd();
    let folder = home.folder(&cwd);
    // Two sessions whose ids share their first four characters. Neither run
    // writes a list, so `ls` and the lookup rebuild from the part headers.
    let first = "01M5YHWTNQR6K3C80C6TXHFH0P".to_owned();
    let mut second = first[..4].to_owned();
    second.push_str(&"Z".repeat(first.len() - 4));
    for id in [first.clone(), second.clone()] {
        reagent_store::Writer::create(
            &folder.join(&id),
            reagent_store::Header {
                part_id: 1,
                session_id: id.clone(),
                created_ms: 1,
                fork: None,
                meta: Default::default(),
            },
        )
        .expect("a session")
        .close()
        .expect("close it");
    }

    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let prefix = &first[..4];
    let output = home
        .run(&[
            "-r",
            prefix,
            "-p",
            "hi",
            "--provider",
            &format!("fake:{script}"),
        ])
        .await;
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    let message = err(&output);
    assert!(message.contains("several sessions match"), "{message}");
    assert!(message.contains(&first), "{message}");
    assert!(message.contains(&second), "{message}");
}

/// A fork after a turn keeps the parent's turns and copies nothing.
#[tokio::test]
async fn fork_at_a_turn() {
    let home = Home::new();
    let calls = home.script(
        "tool.json",
        json!([
            { "calls": [{ "name": "bash", "arguments": { "command": "echo one" } }] },
            { "text": "first turn" },
            { "text": "forked" }
        ]),
    );
    let calls = calls.to_string_lossy().into_owned();
    let cwd = home.cwd();
    let provider = format!("fake:{calls}");
    assert!(
        home.run(&["-p", "one", "--provider", &provider])
            .await
            .status
            .success()
    );
    let ids = home.ids(&cwd);
    assert_eq!(ids.len(), 1);
    let parent = ids[0].clone();
    let reference = format!("{parent}@1");
    let output = home
        .run(&["--fork", &reference, "-p", "two", "--provider", &provider])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    let ids = home.ids(&cwd);
    assert_eq!(ids.len(), 2, "a new session folder: {ids:?}");
    // The parent keeps its two turns.
    let listed = home.run(&["session", "ls"]).await;
    let text = out(&listed);
    assert!(text.contains("2 turns"), "{text}");
    // The fork writes no copy of the parent's records.
    let bytes = home.bytes(&cwd);
    for (dir, size) in bytes {
        assert!(size < 8 * 1024, "{} is {size} bytes", dir.display());
    }
    // A fork of the fork works too.
    let output = home
        .run(&["--fork", &ids[0], "-p", "three", "--provider", &provider])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    assert_eq!(home.ids(&cwd).len(), 3);
}

/// The stored model is used by a resume, and `--model` wins.
#[tokio::test]
async fn the_stored_model_is_used() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let cwd = home.cwd();
    let provider = format!("fake:{script}");
    assert!(
        home.run(&[
            "-p",
            "hello",
            "--model",
            "stored-model",
            "--provider",
            &provider
        ])
        .await
        .status
        .success()
    );
    assert_eq!(home.ids(&cwd).len(), 1);
    let output = home
        .run(&["-c", "-p", "more", "--provider", &provider])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    assert!(err(&output).contains("stored-model"), "{}", err(&output));
    let output = home
        .run(&[
            "-c",
            "-p",
            "more",
            "--model",
            "flag-model",
            "--provider",
            &provider,
        ])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    assert!(err(&output).contains("flag-model"), "{}", err(&output));
}

/// `session cat` prints a header line and parseable record lines.
#[tokio::test]
async fn cat_prints_the_log() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let cwd = home.cwd();
    assert!(
        home.run(&["-p", "hello", "--provider", &format!("fake:{script}")])
            .await
            .status
            .success()
    );
    let id = home.ids(&cwd).remove(0);
    let output = home.run(&["session", "cat", &id[..4]]).await;
    assert!(output.status.success(), "{}", err(&output));
    let text = out(&output);
    let mut lines = text.lines();
    let header = lines.next().expect("a header line");
    let header: serde_json::Value = serde_json::from_str(header).expect("header JSON");
    assert_eq!(header["type"], "header");
    assert_eq!(header["session"], id);
    let mut messages = 0;
    for line in lines {
        let value: serde_json::Value = serde_json::from_str(line).expect("record JSON");
        if value["type"] == "message" {
            messages += 1;
        }
    }
    assert!(messages >= 2, "{text}");
}

/// `session rm` removes a session, and refuses while a writer holds it.
#[tokio::test]
async fn rm_removes_a_session() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let cwd = home.cwd();
    assert!(
        home.run(&["-p", "hello", "--provider", &format!("fake:{script}")])
            .await
            .status
            .success()
    );
    let ids = home.ids(&cwd);
    assert_eq!(ids.len(), 1);
    let dir = home.sessions(&cwd).remove(0);
    // A writer on the newest part makes the removal refuse. The newest part is
    // the one with the highest id, not the one written last.
    let mut parts = reagent_store::paths::part_ids(&dir).expect("the part files");
    parts.sort_unstable();
    let newest = parts.last().copied().unwrap_or(0) + 1;
    let holder = reagent_store::Writer::create(
        &dir,
        reagent_store::Header {
            part_id: newest,
            session_id: ids[0].clone(),
            created_ms: 1,
            fork: None,
            meta: Default::default(),
        },
    )
    .expect("a second part");
    let output = home.run(&["session", "rm", &ids[0][..4]]).await;
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(err(&output).contains("in use"), "{}", err(&output));
    // Closing joins the writer thread, so the lock is gone when it returns.
    holder.close().expect("close the holder");
    let output = home.run(&["session", "rm", &ids[0][..4]]).await;
    assert!(output.status.success(), "{}", err(&output));
    assert!(home.ids(&cwd).is_empty());
}

/// `session ls` in a folder with no sessions prints nothing and exits 0.
#[tokio::test]
async fn ls_with_no_sessions_prints_nothing() {
    let home = Home::new();
    let listed = home.run(&["session", "ls"]).await;
    assert!(listed.status.success(), "{}", err(&listed));
    assert_eq!(out(&listed), "");
}

/// The turns of a resumed run keep counting from the ones the session has.
#[tokio::test]
async fn turn_numbers_run_across_a_resume() {
    let home = Home::new();
    let cwd = home.cwd();
    let first = home.script(
        "one.json",
        json!([
            { "calls": [{ "name": "bash", "arguments": { "command": "echo one" } }] },
            { "text": "first turn" }
        ]),
    );
    let first = first.to_string_lossy().into_owned();
    assert!(
        home.run(&["-p", "one", "--provider", &format!("fake:{first}")])
            .await
            .status
            .success()
    );
    let second = home.script("two.json", reply("second turn"));
    let second = second.to_string_lossy().into_owned();
    let output = home
        .run(&["-c", "-p", "two", "--provider", &format!("fake:{second}")])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    let id = home.ids(&cwd).remove(0);
    // The session counts three turns, though the second run made one.
    let listed = out(&home.run(&["session", "ls"]).await);
    assert!(listed.contains("3 turns"), "{listed}");
    let log = out(&home.run(&["session", "cat", &id]).await);
    for turn in ["\"turn\":1", "\"turn\":2", "\"turn\":3"] {
        assert!(log.contains(turn), "{turn} is missing: {log}");
    }
    // The last turn is a fork point that holds every turn of the session.
    let third = home.script("three.json", reply("third turn"));
    let third = third.to_string_lossy().into_owned();
    let reference = format!("{id}@3");
    let output = home
        .run(&[
            "--fork",
            &reference,
            "-p",
            "three",
            "--provider",
            &format!("fake:{third}"),
        ])
        .await;
    assert!(output.status.success(), "{}", err(&output));
    let ids = home.ids(&cwd);
    let fork = ids
        .iter()
        .find(|other| **other != id)
        .expect("the fork is a session");
    let log = out(&home.run(&["session", "cat", fork]).await);
    for text in ["first turn", "second turn", "third turn"] {
        assert!(log.contains(text), "{text} is missing: {log}");
    }
}

/// A resume takes the provider and the model of the session, and `--model`
/// names both for the run that gives it.
#[tokio::test]
async fn a_resume_takes_the_provider_and_model_of_the_session() {
    let home = Home::new();
    let script = home.script("alpha.json", reply("alpha answered"));
    let script = script.to_string_lossy().into_owned();
    // The session ran on `alpha`. The config default is another provider with
    // no key, so a run that drops the provider of the session stops at once.
    home.config(&format!(
        "model = \"beta/gpt\"\n\
         [providers.alpha]\nwire = \"fake:{script}\"\n\
         [providers.beta]\nwire = \"openai-chat\"\n\
         base_url = \"http://127.0.0.1:2/v1\"\napi_key_env = \"BETA_KEY\"\n"
    ));
    let output = home.run(&["-p", "hello", "--model", "alpha/a"]).await;
    assert!(output.status.success(), "{}", err(&output));
    // The continue names no provider: the session keeps its own.
    let output = home.run(&["-c", "-p", "more"]).await;
    let said = err(&output);
    assert!(output.status.success(), "{said}");
    assert!(said.contains("model alpha/a"), "{said}");
    let id = home.ids(&home.cwd()).remove(0);
    let log = out(&home.run(&["session", "cat", &id]).await);
    // The part the run wrote names the provider and the model it used.
    let header = log.lines().next().expect("a header line");
    assert!(header.contains("\"provider\":\"alpha\""), "{header}");
    assert!(header.contains("\"model\":\"a\""), "{header}");
    assert!(log.contains("alpha answered"), "{log}");
    // The run that names a model writes it down for the next resume.
    let output = home.run(&["-c", "-p", "more", "--model", "alpha/b"]).await;
    assert!(output.status.success(), "{}", err(&output));
    let log = out(&home.run(&["session", "cat", &id]).await);
    assert!(log.contains("\"type\":\"model\""), "{log}");
    let output = home.run(&["-c", "-p", "more"]).await;
    let said = err(&output);
    assert!(output.status.success(), "{said}");
    assert!(said.contains("model alpha/b"), "{said}");
}

/// A path cannot name the session of another working folder.
#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "a test makes the folders it runs in"
)]
async fn a_path_cannot_leave_the_sessions_of_this_folder() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let provider = format!("fake:{script}");
    // A session of another working folder.
    let other = home.root.path().join("other");
    std::fs::create_dir_all(&other).expect("create the other working folder");
    assert!(
        home.run_in(&other, &["-p", "hello", "--provider", &provider])
            .await
            .status
            .success()
    );
    let outside = home.sessions(&other).remove(0);
    let outside = outside.to_string_lossy().into_owned();
    // Every other part of the root folder, which is no session either.
    let beside = home.root.path().join("outside");
    std::fs::create_dir_all(&beside).expect("create the folder beside the sessions");
    let cwd = home.cwd();
    assert!(
        home.run_in(
            &cwd,
            &["-p", "hello", "--provider", &format!("fake:{script}")]
        )
        .await
        .status
        .success()
    );
    let mine = home.ids(&cwd);
    // Removing the session of the other folder is refused, and it stands.
    let output = home.run_in(&cwd, &["session", "rm", &outside]).await;
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(
        err(&output).contains("not a session of this folder"),
        "{}",
        err(&output)
    );
    assert_eq!(home.sessions(&other).len(), 1, "the other session is gone");
    // A folder beside the sessions folder is refused too.
    let output = home.run_in(&cwd, &["session", "rm", "../outside"]).await;
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(
        err(&output).contains("not a session of this folder"),
        "{}",
        err(&output)
    );
    assert!(beside.is_dir(), "the folder is gone");
    // `session cat` reads nothing outside either.
    let output = home.run_in(&cwd, &["session", "cat", &outside]).await;
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(
        err(&output).contains("not a session of this folder"),
        "{}",
        err(&output)
    );
    // The session of this folder is still there.
    assert_eq!(home.ids(&cwd), mine);
    let output = home.run_in(&cwd, &["session", "cat", &mine[0][..4]]).await;
    assert!(output.status.success(), "{}", err(&output));
}

/// A resume in a folder with no sessions says no session matches.
#[tokio::test]
async fn resume_in_a_folder_with_no_sessions() {
    let home = Home::new();
    let script = home.script("script.json", reply("done"));
    let script = script.to_string_lossy().into_owned();
    let cwd = home.cwd();
    let output = home
        .run(&[
            "-r",
            "01M5YHWT",
            "-p",
            "hello",
            "--provider",
            &format!("fake:{script}"),
        ])
        .await;
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(
        err(&output).contains("no session matches 01M5YHWT"),
        "{}",
        err(&output)
    );
    assert!(home.ids(&cwd).is_empty(), "a session was written");
}

/// A worker the test started, with the ready line it printed.
struct Worker {
    child: tokio::process::Child,
    ready: serde_json::Value,
}

impl Worker {
    /// The socket the worker listens on.
    fn socket(&self) -> PathBuf {
        PathBuf::from(self.ready["socket"].as_str().expect("a socket path"))
    }

    /// The token a client initializes with.
    async fn token(&self) -> String {
        let session = self.ready["session"].as_str().expect("a session id");
        let path = self.socket().with_file_name(format!("{session}.json"));
        let bytes = tokio::fs::read(&path).await.expect("the entry file");
        let entry: serde_json::Value = serde_json::from_slice(&bytes).expect("a JSON entry");
        entry["token"].as_str().expect("a token").to_owned()
    }

    /// Waits for the worker to leave after its last client did.
    async fn stop(mut self) {
        tokio::time::timeout(WAIT, self.child.wait())
            .await
            .expect("the worker leaves when its last client does")
            .expect("the worker exits");
    }
}

/// A client of a worker: its connection and the id of its next request.
struct Client {
    lines: tokio::io::BufReader<tokio::net::UnixStream>,
    next: u64,
}

impl Client {
    /// Connects to `worker` and initializes with its token.
    async fn initialize(worker: &Worker) -> Client {
        let stream = tokio::net::UnixStream::connect(worker.socket())
            .await
            .expect("the worker accepts a client");
        let mut client = Client {
            lines: tokio::io::BufReader::new(stream),
            next: 0,
        };
        let token = worker.token().await;
        let params = format!(
            "{{\"major\":1,\"client\":{{\"name\":\"a\",\"version\":\"1\"}},\
             \"capabilities\":[],\"token\":\"{token}\"}}"
        );
        let id = client.request("initialize", &params).await;
        let answer = client.answer(id).await;
        assert!(
            answer["result"].is_object(),
            "the client initializes: {answer}"
        );
        // The frames after `initialize` come from the hub, which a client reads
        // from the record it attached at.
        let committed = answer["result"]["committed"].as_u64().unwrap_or(0);
        let attach = client
            .request("attach", &format!("{{\"from\":{committed}}}"))
            .await;
        let answer = client.answer(attach).await;
        assert!(
            answer["result"]["committed"].is_u64(),
            "the client attaches: {answer}"
        );
        client
    }

    /// Sends one request and returns the id it carries.
    async fn request(&mut self, method: &str, params: &str) -> u64 {
        self.next += 1;
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"{method}\",\"params\":{params}}}\n",
            self.next
        );
        self.lines
            .get_mut()
            .write_all(line.as_bytes())
            .await
            .expect("writes the request");
        self.next
    }

    /// The next frame of the worker.
    async fn frame(&mut self) -> serde_json::Value {
        let mut line = String::new();
        tokio::time::timeout(WAIT, self.lines.read_line(&mut line))
            .await
            .expect("a frame in time")
            .expect("the worker writes a frame");
        serde_json::from_str(line.trim_end()).expect("a JSON frame")
    }

    /// The frames up to and with the first one `done` names.
    async fn until(&mut self, done: impl Fn(&serde_json::Value) -> bool) -> Vec<serde_json::Value> {
        let mut frames = Vec::new();
        loop {
            let frame = self.frame().await;
            let end = done(&frame);
            frames.push(frame);
            if end {
                return frames;
            }
        }
    }

    /// The answer to the request `id`, whether it is a result or an error.
    async fn answer(&mut self, id: u64) -> serde_json::Value {
        self.until(|frame| frame["id"].as_u64() == Some(id))
            .await
            .pop()
            .expect("an answer")
    }

    /// The next settings notice.
    async fn settings(&mut self) -> serde_json::Value {
        self.until(|frame| frame["method"] == "settings")
            .await
            .pop()
            .expect("a settings notice")
    }
}

/// The text the deltas of `frames` spell.
fn deltas(frames: &[serde_json::Value]) -> String {
    frames
        .iter()
        .filter(|frame| frame["method"] == "event" && frame["params"]["type"] == "text_delta")
        .filter_map(|frame| frame["params"]["text"].as_str())
        .collect()
}

/// The part header field that keeps the provider of a run.
const META_PROVIDER: u32 = 2;
/// The part header field that keeps the model of a run.
const META_MODEL: u32 = 3;

/// Writes a session as a build before config provider ids wrote it: a header
/// that names `one/model-a` and a switch to `model-b` that names no config
/// provider id.
fn old_switch(home: &Home, cwd: &Path, id: &str) {
    let mut meta = Vec::new();
    let mut encoder = reagent_store::Encoder::new(&mut meta);
    encoder.str(META_PROVIDER, "one");
    encoder.str(META_MODEL, "model-a");
    let header = reagent_store::Header::new(id.to_owned(), 1, None, meta.into());
    let mut journal = reagent_agent::journal::Journal::create(&home.folder(cwd).join(id), header)
        .expect("a part");
    journal
        .message(&reagent_ai::Message::user("hello"))
        .expect("a message");
    journal
        .message(&reagent_ai::Message::assistant(
            reagent_ai::AssistantMessage::new(
                "fake",
                "model-a",
                vec![reagent_ai::Content::text("hi")],
            ),
        ))
        .expect("a reply");
    journal
        .turn_end(1, reagent_ai::Usage::default())
        .expect("a turn end");
    journal.model("fake", "model-b").expect("a switch");
    journal.close().expect("the part is closed");
}

/// A resume runs on the provider and the model the session switched to, not on
/// the ones its part header names.
#[tokio::test]
async fn resume_after_a_switch_runs_on_the_switched_provider() {
    let home = Home::short();
    let cwd = home.cwd();
    let one = home.script("one.json", reply("from one"));
    let two = home.script("two.json", reply("from two"));
    let (one, two) = (
        one.to_string_lossy().into_owned(),
        two.to_string_lossy().into_owned(),
    );
    home.config(&format!(
        "[providers.one]\nwire = \"fake:{one}\"\n\n[providers.two]\nwire = \"fake:{two}\"\n"
    ));
    let worker = home.worker(&cwd, &["--model", "one/model-a"]).await;
    let mut client = Client::initialize(&worker).await;
    // A client switches the session to the other provider.
    let set = client.request("set", r#"{"model":"two/model-b"}"#).await;
    let answer = client.answer(set).await;
    assert!(
        answer["result"].is_object(),
        "the session switches to two: {answer}"
    );
    let notice = client.settings().await;
    assert_eq!(
        notice["params"]["model"], "two/model-b",
        "the notice names the model the session switched to: {notice}"
    );
    // The turn after the switch goes to the provider it switched to, and its
    // request writes the switch down.
    let sent = client
        .request("send", r#"{"text":"go","mode":"queue"}"#)
        .await;
    assert!(client.answer(sent).await["result"].is_object());
    let run = client
        .until(|frame| frame["method"] == "event" && frame["params"]["type"] == "run_end")
        .await;
    assert_eq!(
        deltas(&run),
        "from two",
        "the turn after the switch goes to two"
    );
    // The worker leaves with its last client, which closes the part.
    drop(client);
    worker.stop().await;
    let output = home.run(&["-c", "-p", "more"]).await;
    let said = err(&output);
    assert!(output.status.success(), "{said}");
    assert!(said.contains("model two/model-b"), "{said}");
    assert!(out(&output).contains("from two"), "{}", out(&output));
}

/// A resume keeps the provider and the model its part header names when the
/// switch the journal holds names no config provider id.
#[tokio::test]
async fn resume_after_a_switch_without_one_keeps_the_start_provider() {
    let home = Home::new();
    let cwd = home.cwd();
    let one = home.script("one.json", reply("from one"));
    let two = home.script("two.json", reply("from two"));
    let (one, two) = (
        one.to_string_lossy().into_owned(),
        two.to_string_lossy().into_owned(),
    );
    home.config(&format!(
        "[providers.one]\nwire = \"fake:{one}\"\n\n[providers.two]\nwire = \"fake:{two}\"\n"
    ));
    old_switch(&home, &cwd, "01JSWITCH");
    let output = home.run(&["-c", "-p", "more"]).await;
    let said = err(&output);
    assert!(output.status.success(), "{said}");
    assert!(said.contains("model one/model-a"), "{said}");
    assert!(out(&output).contains("from one"), "{}", out(&output));
}
