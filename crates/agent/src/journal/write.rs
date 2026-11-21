//! Tests of the records the loop writes: their order, the sync before a
//! side-effect call runs, the commit after a result and the base after a
//! compaction, the wait and the answer of a call that asks a client, a
//! settings change, the commit events and what a resume makes of them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use reagent_ai::fake::FakeProvider;
use reagent_ai::{
    AssistantMessage, Content, Message, StopReason, StreamEvent, Text, ToolCall, ToolSpec,
};
use reagent_config::permissions::PermissionMode;
use reagent_store::{Header, Session};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{Agent, AgentConfig};
use crate::compact::Compact;
use crate::event::{Event, TurnEndReason};
use crate::journal::Journal;
use crate::journal::record::{self, ApprovalKind, Aside, Record};
use crate::journal::resume::{self, PendingCall};
use crate::live::{self, Command, SendMode, Settings};
use crate::tool::{EffectClass, Registry, Tool, ToolOutput};

/// How often a test asks whether the writer thread wrote its line.
const VISIBLE_POLLS: usize = 500;

/// How long a test waits between two looks at the journal.
const VISIBLE_WAIT: std::time::Duration = std::time::Duration::from_millis(2);

fn config() -> AgentConfig {
    AgentConfig {
        model: "fake-model".into(),
        max_tokens: 128,
        system: None,
        reasoning: None,
        cache_key: None,
        max_turns: None,
        images: true,
    }
}

/// One reply that calls several tools, in order, and stops for them.
fn tool_replies(calls: &[(&str, &str, &str)]) -> Vec<StreamEvent> {
    let mut events = Vec::with_capacity(calls.len() * 3 + 1);
    for (call_id, name, args) in calls {
        events.push(StreamEvent::ToolCallStart {
            id: Text::from(*call_id),
            name: Text::from(*name),
        });
        events.push(StreamEvent::ToolCallDelta {
            id: Text::from(*call_id),
            partial_json: Text::from(*args),
        });
        events.push(StreamEvent::ToolCallEnd {
            id: Text::from(*call_id),
        });
    }
    events.push(StreamEvent::Stop(StopReason::ToolUse));
    events
}

/// One reply that only says `text`.
fn text_reply(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(Text::from(text)),
        StreamEvent::Stop(StopReason::EndTurn),
    ]
}

/// Opens a journal over `dir`, as a run does.
fn journal(dir: &Path) -> Journal {
    let header = Header::new(
        "session".into(),
        reagent_store::paths::now_ms(),
        None,
        Bytes::new(),
    );
    Journal::create(dir, header).expect("create the journal")
}

/// Every record of the newest part, decoded, read without closing the journal.
fn read_records(dir: &Path) -> Vec<Record> {
    let session = Session::open(dir).expect("open the session");
    session
        .records()
        .map(|record| {
            let record = record.expect("read a record");
            record::decode(record.kind, &record.payload).expect("decode a record")
        })
        .collect()
}

/// Whether the last commit of `dir` holds a call start for `id`.
fn call_start_on_disk(dir: &Path, id: &str) -> bool {
    let Ok(session) = Session::open(dir) else {
        return false;
    };
    session.records().filter_map(Result::ok).any(|record| {
        matches!(
            record::decode(record.kind, &record.payload),
            Ok(Record::CallStart(call)) if call.id == id
        )
    })
}

#[derive(Deserialize)]
struct ProbeArgs {
    id: String,
}

/// A tool that notes whether its own call start was on the disk when it ran.
struct Probe {
    spec: ToolSpec,
    dir: PathBuf,
    read_only: bool,
    seen: Arc<Mutex<Vec<(String, bool)>>>,
}

impl Probe {
    fn new(
        name: &str,
        read_only: bool,
        dir: &Path,
        seen: &Arc<Mutex<Vec<(String, bool)>>>,
    ) -> Self {
        Self {
            spec: ToolSpec {
                name: name.into(),
                description: "A tool that looks at the journal.".into(),
                input_schema: json!({"type": "object"}),
            },
            dir: dir.to_path_buf(),
            read_only,
            seen: Arc::clone(seen),
        }
    }
}

impl Tool for Probe {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let Ok(arguments) = serde_json::from_str::<ProbeArgs>(args) else {
                return ToolOutput::error("invalid arguments: id");
            };
            let dir = self.dir.clone();
            let id = arguments.id;
            let name = self.spec.name.clone();
            let seen = Arc::clone(&self.seen);
            let running = id.clone();
            // Reading the part blocks, so it runs off the async thread.
            let on_disk = tokio::task::spawn_blocking(move || call_start_on_disk(&dir, &running))
                .await
                .unwrap_or(false);
            seen.lock().expect("the seen lock").push((name, on_disk));
            ToolOutput::ok(format!("ran {id}"))
        })
    }
}

/// A short label for each record, so a test can name the order.
fn label(record: &Record) -> &'static str {
    match record {
        Record::Base(_) => "base",
        Record::CallStart(_) => "call_start",
        Record::TurnEnd(_) => "turn_end",
        Record::Model(_) => "model",
        Record::Unknown(_) => "unknown",
        Record::Message(Message::User { .. }) => "user",
        Record::Message(Message::ToolResult(_)) => "result",
        Record::Message(Message::Assistant(reply))
            if reply.stop_reason == Some(StopReason::ToolUse) =>
        {
            "assistant_call"
        }
        Record::Message(Message::Assistant(_)) => "assistant_text",
    }
}

#[tokio::test]
async fn a_side_effect_call_is_on_the_disk_before_it_runs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::new();
    registry.add(Probe::new("peek", true, dir.path(), &seen));
    registry.add(Probe::new("poke", false, dir.path(), &seen));
    let provider = FakeProvider::new(vec![
        tool_replies(&[
            ("call_1", "peek", r#"{"id":"call_1"}"#),
            ("call_2", "poke", r#"{"id":"call_2"}"#),
        ]),
        text_reply("done"),
    ]);
    let (tx, mut rx) = mpsc::channel(64);
    let mut history = vec![Message::user("hello")];
    let mut journal = journal(dir.path());

    Agent::new(&provider, &registry, config(), CancellationToken::new())
        .run(&mut history, Some(&mut journal), &tx)
        .await;
    drop(tx);
    journal.close().expect("close the journal");
    let _ = rx.try_recv();

    // The sync returned before the side-effect call ran: its start is on the
    // disk when it looks. The read has no start, so it may run again.
    let seen = seen.lock().expect("the seen lock").clone();
    assert_eq!(
        seen,
        [("peek".to_owned(), false), ("poke".to_owned(), true)]
    );

    let records = read_records(dir.path());
    let order: Vec<&str> = records.iter().map(label).collect();
    assert_eq!(
        order,
        [
            "base",
            "user",
            "assistant_call",
            "call_start",
            "result",
            "result",
            "turn_end",
            "assistant_text",
            "turn_end",
        ]
    );
    let starts: Vec<&Record> = records
        .iter()
        .filter(|record| matches!(record, Record::CallStart(_)))
        .collect();
    assert_eq!(starts.len(), 1, "only the call that is not a read");
    assert!(matches!(
        starts[0],
        Record::CallStart(call) if call.id == "call_2"
    ));
    let base = records
        .iter()
        .find_map(|record| match record {
            Record::Base(state) => Some(state),
            _ => None,
        })
        .expect("a base at the start");
    assert_eq!(base.provider, "fake");
    assert_eq!(base.model, "fake-model");
    assert_eq!(base.compaction, 0);
}

#[tokio::test]
async fn a_turn_end_carries_the_usage_of_its_reply() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut registry = Registry::new();
    registry.add(Probe::new(
        "peek",
        true,
        dir.path(),
        &Arc::new(Mutex::new(Vec::new())),
    ));
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let (tx, _rx) = mpsc::channel(64);
    let mut history = vec![Message::user("hello")];
    let mut journal = journal(dir.path());

    Agent::new(&provider, &registry, config(), CancellationToken::new())
        .run(&mut history, Some(&mut journal), &tx)
        .await;
    journal.close().expect("close the journal");

    let records = read_records(dir.path());
    let turns: Vec<u64> = records
        .iter()
        .filter_map(|record| match record {
            Record::TurnEnd(end) => Some(end.turn),
            _ => None,
        })
        .collect();
    assert_eq!(turns, [1]);
}

#[tokio::test]
async fn a_commit_makes_a_record_visible_without_a_sync() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut journal = journal(dir.path());
    let first = Message::user("first");
    let second = Message::user("second");
    let only_first = vec![first.clone()];

    // A write on its own stays behind the last commit.
    journal.message(&first).expect("append the first");
    assert_eq!(read_now(dir.path()).await, Vec::<Message>::new());

    // The commit puts it in front of a reader, with no fdatasync.
    journal.commit().expect("commit");
    assert_eq!(wait_visible(dir.path(), 1).await, only_first);

    // The write after a commit waits for the next one.
    journal.message(&second).expect("append the second");
    assert_eq!(read_now(dir.path()).await, only_first);

    journal.close().expect("close the journal");
    assert_eq!(wait_visible(dir.path(), 2).await, [first, second]);
}

/// The messages a reader sees at once, with the writer thread's frame as it is.
async fn read_now(dir: &Path) -> Vec<Message> {
    let dir = dir.to_path_buf();
    let records = tokio::task::spawn_blocking(move || read_records(&dir))
        .await
        .expect("read the journal");
    records
        .iter()
        .filter_map(|record| match record {
            Record::Message(message) => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// Pupils the reader until it sees `want` messages, so a test never races the
/// writer thread's turn.
async fn wait_visible(dir: &Path, want: usize) -> Vec<Message> {
    let mut seen = Vec::new();
    for _ in 0..VISIBLE_POLLS {
        seen = read_now(dir).await;
        if seen.len() >= want {
            return seen;
        }
        tokio::time::sleep(VISIBLE_WAIT).await;
    }
    seen
}

#[tokio::test]
async fn a_base_after_compaction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let archive = tempfile::tempdir().expect("archive dir");
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let registry = Registry::new();
    let (tx, mut rx) = mpsc::channel(64);
    // A history the compaction can shorten: the newest reply's usage is under
    // the budget and the tail after it starts at an assistant message.
    let mut history = vec![
        Message::user("hello"),
        Message::assistant(AssistantMessage::new(
            "fake",
            "fake-model",
            vec![Content::text("hi")],
        )),
        Message::user("again"),
        Message::assistant(AssistantMessage::new(
            "fake",
            "fake-model",
            vec![Content::text("x".repeat(100_000))],
        )),
        Message::tool_result("call_0", "ok", false),
    ];
    let mut journal = journal(dir.path());

    Agent::new(&provider, &registry, config(), CancellationToken::new())
        .with_compact(Compact::with_budget(0, archive.path()))
        .run(&mut history, Some(&mut journal), &tx)
        .await;
    drop(tx);
    journal.close().expect("close the journal");
    let events: Vec<Event> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Compacted { .. })),
        "{events:?}"
    );

    let records = read_records(dir.path());
    let bases: Vec<&crate::journal::state::State> = records
        .iter()
        .filter_map(|record| match record {
            Record::Base(state) => Some(state),
            _ => None,
        })
        .collect();
    assert_eq!(bases.len(), 2, "the start of the run and the compaction");
    assert_eq!(bases[0].compaction, 0);
    assert_eq!(bases[1].compaction, 1);
    // The compacted history is recorded after its base, so a resume at the
    // base rebuilds the history the run continues with.
    let labels: Vec<&str> = records.iter().map(label).collect();
    let second_base = labels
        .iter()
        .rposition(|label| *label == "base")
        .expect("the compaction base");
    assert!(labels[second_base + 1..].contains(&"user"), "{labels:?}");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::TurnEnd(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_resumed_run_writes_no_base_and_no_copy() {
    let dir = tempfile::tempdir().expect("temp dir");
    let registry = Registry::new();
    let mut history = vec![Message::user("hello")];
    {
        let provider = FakeProvider::new(vec![text_reply("hi")]);
        let (tx, _rx) = mpsc::channel(64);
        let mut journal = journal(dir.path());
        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, Some(&mut journal), &tx)
            .await;
        journal.close().expect("close the first part");
    }
    let parent = Session::open(dir.path()).expect("open the session");
    let last = parent.records().count() as u64;
    let fork = parent.fork_point(last);
    let header = Header::new(
        "session".into(),
        reagent_store::paths::now_ms() + 1,
        Some(fork),
        Bytes::new(),
    );
    let recorded = history.len();
    history.push(Message::user("next"));
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let (tx, _rx) = mpsc::channel(64);
    let mut journal = Journal::resumed(dir.path(), header, recorded).expect("resumed journal");
    let part = journal.part_id();
    Agent::new(&provider, &registry, config(), CancellationToken::new())
        .run(&mut history, Some(&mut journal), &tx)
        .await;
    journal.close().expect("close the second part");

    let records = read_records(dir.path());
    let bases = records
        .iter()
        .filter(|record| matches!(record, Record::Base(_)))
        .count();
    assert_eq!(bases, 1, "only the new session's start has a base");
    let messages = records
        .iter()
        .filter(|record| matches!(record, Record::Message(_)))
        .count();
    assert_eq!(messages, 4, "hello, hi, next and done, each once");
    let child = tokio::fs::metadata(reagent_store::paths::part_path(dir.path(), part))
        .await
        .expect("child part")
        .len();
    assert!(
        child < 8 * 1024,
        "the child part holds no copy: {child} bytes"
    );
}

/// A tool that may change the world and counts how often it ran.
struct Poke {
    spec: ToolSpec,
    runs: Arc<Mutex<usize>>,
}

impl Tool for Poke {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run<'a>(
        &'a self,
        _args: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            *self.runs.lock().expect("the runs lock") += 1;
            ToolOutput::ok("poked")
        })
    }
}

/// A registry whose only tool writes to the world and counts its runs.
fn poke_registry(runs: &Arc<Mutex<usize>>) -> Registry {
    let mut registry = Registry::new();
    registry.add(Poke {
        spec: ToolSpec {
            name: "poke".into(),
            description: "Changes the world.".into(),
            input_schema: json!({"type": "object"}),
        },
        runs: Arc::clone(runs),
    });
    registry
}

/// A tool that may change the world and never finishes, so a test can look at
/// the journal while it runs.
struct Hanging {
    spec: ToolSpec,
    started: Arc<tokio::sync::Notify>,
}

impl Tool for Hanging {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run<'a>(
        &'a self,
        _args: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            self.started.notify_one();
            std::future::pending().await
        })
    }
}

/// A guard that asks a client about every call.
fn asking_guard() -> crate::guard::Guard {
    let layer = reagent_config::Layer {
        path: PathBuf::from("/c/config.toml"),
        settings: reagent_config::Settings {
            permissions: reagent_config::permissions::Lists {
                default: Some("ask".into()),
                ..Default::default()
            },
            ..reagent_config::Settings::default()
        },
    };
    let rules = reagent_config::permissions::Rules::from_layers(&[layer], Path::new("/work"), None)
        .expect("the rules build");
    crate::guard::Guard::new(rules)
}

/// One assistant reply that calls one tool, as the model sent it.
fn call_reply(id: &str, name: &str, args: &str) -> Message {
    let call = Content::ToolCall(ToolCall {
        id: Text::from(id),
        name: Text::from(name),
        arguments: Text::from(args),
    });
    let mut reply = AssistantMessage::new("fake", "fake-model", vec![call]);
    reply.stop_reason = Some(StopReason::ToolUse);
    Message::assistant(reply)
}

/// The approval records of `dir`, in order: the call id and the kind.
fn approvals(dir: &Path) -> Vec<(String, ApprovalKind)> {
    let Ok(session) = Session::open(dir) else {
        return Vec::new();
    };
    session
        .records()
        .filter_map(Result::ok)
        .filter_map(
            |record| match record::decode_aside(record.kind, &record.payload) {
                Ok(Some(Aside::Approval(approval))) => Some((approval.id, approval.kind)),
                _ => None,
            },
        )
        .collect()
}

/// The calls whose results are on the disk, in the order they were written.
fn result_ids(dir: &Path) -> Vec<String> {
    read_records(dir)
        .iter()
        .filter_map(|record| match record {
            Record::Message(Message::ToolResult(result)) => Some(result.call_id.to_string()),
            _ => None,
        })
        .collect()
}

/// The record each commit named, in order.
fn commits(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Committed { record } => Some(*record),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_result_is_on_the_disk_before_the_batch_ends() {
    let dir = tempfile::tempdir().expect("temp dir");
    let started = Arc::new(tokio::sync::Notify::new());
    let mut registry = Registry::new();
    registry.add(Probe::new(
        "peek",
        true,
        dir.path(),
        &Arc::new(Mutex::new(Vec::new())),
    ));
    registry.add(Hanging {
        spec: ToolSpec {
            name: "hang".into(),
            description: "Never finishes.".into(),
            input_schema: json!({"type": "object"}),
        },
        started: Arc::clone(&started),
    });
    // A read and a call that writes: the read runs first, then the write.
    let provider = FakeProvider::new(vec![tool_replies(&[
        ("call_1", "peek", r#"{"id":"call_1"}"#),
        ("call_2", "hang", "{}"),
    ])]);
    let (tx, _rx) = mpsc::channel(64);
    let mut history = vec![Message::user("hello")];
    let mut journal = journal(dir.path());
    let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new());
    {
        let run = agent.run(&mut history, Some(&mut journal), &tx);
        tokio::pin!(run);
        tokio::select! {
            () = started.notified() => {}
            () = &mut run => panic!("the run ended before the call that writes started"),
        }
        // The loop takes the note of the read while its next call waits, so the
        // read's result is committed while the write still runs.
        let mut seen = Vec::new();
        for _ in 0..VISIBLE_POLLS {
            seen = result_ids(dir.path());
            if !seen.is_empty() {
                break;
            }
            tokio::select! {
                () = tokio::time::sleep(VISIBLE_WAIT) => {}
                () = &mut run => panic!("the run ended while the call that writes ran"),
            }
        }
        assert_eq!(
            seen,
            ["call_1"],
            "the read is committed while the write runs"
        );
    }
    // The process dies with the write still running: the read's result stays.
    drop(agent);
    journal.close().expect("close the journal");
    assert_eq!(result_ids(dir.path()), ["call_1"]);
}

#[tokio::test]
async fn commit_events_rise_and_are_one_per_commit() {
    let dir = tempfile::tempdir().expect("temp dir");
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let registry = Registry::new();
    let (tx, mut inbox) = live::channel();
    let (events, mut rx) = mpsc::channel(64);
    let mut history = Vec::new();
    let mut journal = journal(dir.path());
    let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new());

    let session = live::run_live(
        &mut agent,
        &mut history,
        Some(&mut journal),
        &events,
        &mut inbox,
    );
    let client = async {
        tx.send(Command::Send {
            message: 1,
            client: 1,
            text: Text::from("hello"),
            blocks: Vec::new(),
            mode: SendMode::Steer,
        })
        .await
        .expect("the session is open");
        drop(tx);
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            let end = matches!(event, Event::RunEnd(_));
            seen.push(event);
            if end {
                break;
            }
        }
        seen
    };
    let (_, seen) = tokio::join!(session, client);
    journal.close().expect("close the journal");

    // The start of the run (a base and the history) is one commit, and the end
    // of the turn (its message and its turn end) is the other.
    let named = commits(&seen);
    assert_eq!(named.len(), 2, "one event per commit: {seen:?}");
    assert!(
        named.windows(2).all(|pair| pair[0] < pair[1]),
        "a commit never names a smaller record: {named:?}"
    );
    let held = read_records(dir.path()).len() as u64;
    assert_eq!(
        named.last().copied(),
        Some(held),
        "the last event names the last record of the part"
    );
}

#[tokio::test]
async fn commit_event_names_a_record_that_is_readable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let registry = Registry::new();
    let (tx, mut inbox) = live::channel();
    let (events, mut rx) = mpsc::channel(64);
    let mut history = Vec::new();
    let mut journal = journal(dir.path());
    let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new());

    let session = live::run_live(
        &mut agent,
        &mut history,
        Some(&mut journal),
        &events,
        &mut inbox,
    );
    let client = async {
        tx.send(Command::Send {
            message: 1,
            client: 1,
            text: Text::from("hello"),
            blocks: Vec::new(),
            mode: SendMode::Steer,
        })
        .await
        .expect("the session is open");
        drop(tx);
        let mut checked = 0;
        while let Some(event) = rx.recv().await {
            match event {
                Event::Committed { record } => {
                    let dir = dir.path().to_path_buf();
                    let held = tokio::task::spawn_blocking(move || read_records(&dir).len())
                        .await
                        .expect("read the journal") as u64;
                    assert!(held >= record, "{held} records for a commit of {record}");
                    checked += 1;
                }
                Event::RunEnd(_) => break,
                _ => {}
            }
        }
        checked
    };
    let (_, checked) = tokio::join!(session, client);
    journal.close().expect("close the journal");
    assert_eq!(checked, 2, "both commits of the run were read");
}

#[tokio::test]
async fn a_run_without_an_inbox_sends_no_commit_event() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut registry = Registry::new();
    registry.add(Probe::new(
        "peek",
        true,
        dir.path(),
        &Arc::new(Mutex::new(Vec::new())),
    ));
    let provider = FakeProvider::new(vec![
        tool_replies(&[("call_1", "peek", r#"{"id":"call_1"}"#)]),
        text_reply("done"),
    ]);
    let (tx, mut rx) = mpsc::channel(64);
    let mut history = vec![Message::user("hello")];
    let mut journal = journal(dir.path());

    Agent::new(&provider, &registry, config(), CancellationToken::new())
        .run(&mut history, Some(&mut journal), &tx)
        .await;
    drop(tx);
    journal.close().expect("close the journal");
    let events: Vec<Event> = std::iter::from_fn(|| rx.try_recv().ok()).collect();

    assert!(
        commits(&events).is_empty(),
        "a headless run names no record: {events:?}"
    );
    // The records are there all the same: only the events are held back.
    assert!(read_records(dir.path()).len() > 1);
}

#[tokio::test]
async fn a_set_of_the_level_and_the_mode_is_in_the_journal_and_a_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let registry = Registry::new();
    let (tx, mut inbox) = live::channel();
    let (events, mut rx) = mpsc::channel(64);
    let mut history = Vec::new();
    let mut journal = journal(dir.path());
    let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new())
        .with_guard(asking_guard());

    let session = live::run_live(
        &mut agent,
        &mut history,
        Some(&mut journal),
        &events,
        &mut inbox,
    );
    let client = async {
        tx.send(Command::Set {
            client: 1,
            settings: Settings {
                level: Some(Text::from("high")),
                mode: Some(PermissionMode::AcceptEdits),
                ..Settings::default()
            },
        })
        .await
        .expect("the session is open");
        tx.send(Command::Send {
            message: 1,
            client: 1,
            text: Text::from("hello"),
            blocks: Vec::new(),
            mode: SendMode::Steer,
        })
        .await
        .expect("the session is open");
        drop(tx);
        while let Some(event) = rx.recv().await {
            if matches!(event, Event::RunEnd(_)) {
                break;
            }
        }
    };
    tokio::join!(session, client);
    journal.close().expect("close the journal");

    let parent = Session::open(dir.path()).expect("open the session");
    let resumed = resume::load(&parent).expect("resume");
    drop(parent);
    assert_eq!(
        resumed.level.as_deref(),
        Some("high"),
        "a resume carries the level"
    );
    assert_eq!(
        resumed.mode,
        Some(PermissionMode::AcceptEdits),
        "a resume carries the mode"
    );
    // The model keeps its own record, which this change did not touch.
    assert_eq!(resumed.model, "fake-model");
}

#[tokio::test]
async fn a_settings_event_names_what_the_journal_recorded() {
    let dir = tempfile::tempdir().expect("temp dir");
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let registry = Registry::new();
    let (tx, mut inbox) = live::channel();
    let (events, mut rx) = mpsc::channel(64);
    let mut history = Vec::new();
    let mut journal = journal(dir.path());
    let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new())
        .with_guard(asking_guard());

    let session = live::run_live(
        &mut agent,
        &mut history,
        Some(&mut journal),
        &events,
        &mut inbox,
    );
    let client = async {
        tx.send(Command::Set {
            client: 1,
            settings: Settings {
                level: Some(Text::from("high")),
                mode: Some(PermissionMode::AcceptEdits),
                ..Settings::default()
            },
        })
        .await
        .expect("the session is open");
        tx.send(Command::Send {
            message: 1,
            client: 1,
            text: Text::from("hello"),
            blocks: Vec::new(),
            mode: SendMode::Steer,
        })
        .await
        .expect("the session is open");
        drop(tx);
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            let ended = matches!(event, Event::RunEnd(_));
            seen.push(event);
            if ended {
                break;
            }
        }
        seen
    };
    let (_, seen) = tokio::join!(session, client);
    journal.close().expect("close the journal");

    // One event per `set`, naming exactly what the session applied.
    let settings: Vec<&Event> = seen
        .iter()
        .filter(|event| matches!(event, Event::Settings { .. }))
        .collect();
    assert_eq!(
        settings,
        [&Event::Settings {
            model: None,
            level: Some(Text::from("high")),
            mode: Some(PermissionMode::AcceptEdits),
        }],
        "the event names the level and the mode the session applied: {seen:?}"
    );

    // The writer takes the event as a notice, not as a record: the journal holds
    // the one settings record the `set` itself made (in the base the run opens
    // with, which leaves the flush nothing to write), naming exactly what the
    // event names, and none from the event itself.
    let mut recorded = Vec::new();
    for record in Session::open(dir.path())
        .expect("open the session")
        .records()
    {
        let record = record.expect("a record reads");
        if record.kind != crate::journal::kinds::SETTINGS {
            continue;
        }
        match record::decode_aside(record.kind, &record.payload).expect("the record decodes") {
            Some(Aside::Settings(settings)) => recorded.push(settings),
            _ => panic!("a settings kind decodes as a settings record"),
        }
    }
    assert_eq!(
        recorded.len(),
        1,
        "the base, and nothing for the flush or the event: {recorded:?}"
    );
    for settings in &recorded {
        assert_eq!(settings.level.as_deref(), Some("high"), "{settings:?}");
        assert_eq!(
            settings.mode.as_deref(),
            Some("accept-edits"),
            "{settings:?}"
        );
    }
}

#[tokio::test]
async fn a_resume_of_a_waiting_approval_asks_the_same_question() {
    let dir = tempfile::tempdir().expect("temp dir");
    let runs = Arc::new(Mutex::new(0usize));
    let registry = poke_registry(&runs);
    let provider = FakeProvider::new(vec![tool_replies(&[("call_1", "poke", "{}")])]);
    let (tx, mut inbox) = live::channel();
    let (events, mut rx) = mpsc::channel(64);
    let mut history = Vec::new();
    let mut journal = journal(dir.path());
    let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new())
        .with_guard(asking_guard());
    tx.send(Command::Send {
        message: 1,
        client: 1,
        text: Text::from("hello"),
        blocks: Vec::new(),
        mode: SendMode::Steer,
    })
    .await
    .expect("the session is open");

    // The call waits for a client, and the process dies before any answer.
    {
        let run = agent.run_with_inbox(&mut history, Some(&mut journal), &events, &mut inbox);
        tokio::pin!(run);
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Some(Event::ApprovalRequest(request)) => {
                        assert_eq!(request.tool.as_ref(), "poke");
                        break;
                    }
                    Some(_) => {}
                    None => panic!("the events ended before the call asked"),
                },
                () = &mut run => panic!("the run ended before the call asked"),
            }
        }
    }
    assert_eq!(
        *runs.lock().expect("the runs lock"),
        0,
        "the call never ran"
    );
    assert_eq!(
        approvals(dir.path()),
        [("call_1".to_owned(), ApprovalKind::Wait)],
        "the wait is on the disk with no answer"
    );

    // A resume finds the call pending: its last approval record is a wait,
    // which it asks again under the id the wait carries, with no answer yet.
    let parent = Session::open(dir.path()).expect("open the session");
    let resumed = resume::load(&parent).expect("resume");
    drop(parent);
    let held = resumed.pending.first().and_then(|call| call.held.clone());
    let qid = held
        .as_ref()
        .map(|held| held.qid.clone())
        .unwrap_or_default();
    assert_eq!(qid.len(), 32, "the wait carries an id: {held:?}");
    assert_eq!(
        resumed.pending,
        [PendingCall {
            id: "call_1".into(),
            name: "poke".into(),
            class: Some(EffectClass::LocalReversible),
            qid: None,
            held: Some(resume::Held {
                qid: qid.clone(),
                given: None,
            }),
        }]
    );
    assert_eq!(
        resumed.parked,
        [("call_1".to_owned(), qid)],
        "a later process may answer it"
    );
    assert!(
        !resumed
            .history
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_))),
        "a call that never ran gets no interrupted result"
    );

    // The rerun in a live session asks the same question, and the call does
    // not run before an answer arrives; the answer reaches the disk before the
    // call runs.
    let (events, mut rx) = mpsc::channel(64);
    let (tx, mut inbox) = live::channel();
    let provider = FakeProvider::new(Vec::new());
    let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new())
        .with_guard(asking_guard());
    let mut history = resumed.history.clone();
    let mut open = Some(&mut journal);
    let reason = {
        let run = agent.run_pending_with_inbox(
            &mut history,
            &mut open,
            &events,
            &resumed.pending,
            &mut inbox,
        );
        tokio::pin!(run);
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Some(Event::ApprovalRequest(request)) => {
                        assert_eq!(request.tool.as_ref(), "poke", "the same question");
                        assert_eq!(
                            *runs.lock().expect("the runs lock"),
                            0,
                            "no answer came, so the call still has not run"
                        );
                        tx.send(Command::Approve {
                            client: 1,
                            id: request.id,
                            answer: crate::guard::Answer::Once,
                        })
                        .await
                        .expect("the session is open");
                    }
                    Some(_) => {}
                    None => panic!("the events ended before the rerun ended"),
                },
                reason = &mut run => break reason,
            }
        }
    };
    assert_eq!(reason, TurnEndReason::EndTurn);
    assert_eq!(*runs.lock().expect("the runs lock"), 1, "the answer ran it");
    journal.close().expect("close the journal");
    assert_eq!(
        approvals(dir.path()),
        [
            // The second ask waits under the id of the first wait, which is
            // on the disk already, so it writes no second one.
            ("call_1".to_owned(), ApprovalKind::Wait),
            ("call_1".to_owned(), ApprovalKind::Run),
        ],
        "the answer of the second ask is on the disk"
    );
}

#[tokio::test]
async fn a_resumed_edit_does_not_run_again_and_its_result_says_so() {
    let dir = tempfile::tempdir().expect("temp dir");
    let runs = Arc::new(Mutex::new(0usize));
    let registry = poke_registry(&runs);
    let (tx, _rx) = mpsc::channel(64);
    let mut journal = journal(dir.path());
    let user = Message::user("hello");
    let reply = call_reply("call_1", "poke", "{}");
    journal.message(&user).expect("append");
    journal.message(&reply).expect("append");
    journal
        .call_start("call_1", "poke", EffectClass::LocalReversible)
        .expect("call start");
    journal.commit().expect("commit");
    journal.close().expect("close the journal");

    let parent = Session::open(dir.path()).expect("open the session");
    let resumed = resume::load(&parent).expect("resume");
    drop(parent);
    assert!(
        resumed.pending.is_empty(),
        "a call that may have run is never rerun"
    );

    // The next request carries the interrupted result and runs nothing.
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let mut history = resumed.history.clone();
    Agent::new(&provider, &registry, config(), CancellationToken::new())
        .run(&mut history, None, &tx)
        .await;

    let result = history
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("the interrupted result");
    assert_eq!(result.call_id.as_ref(), "call_1");
    assert!(result.is_error);
    assert!(
        result.content.starts_with("interrupted"),
        "{}",
        result.content
    );
    assert_eq!(
        *runs.lock().expect("the runs lock"),
        0,
        "the edit did not run again"
    );
    assert_eq!(
        provider.seen()[0].messages,
        3,
        "the request carries the interrupted result"
    );
}

#[tokio::test]
async fn a_resumed_read_runs_again_and_its_result_reaches_the_next_request() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut registry = Registry::new();
    registry.add(Probe::new(
        "peek",
        true,
        dir.path(),
        &Arc::new(Mutex::new(Vec::new())),
    ));
    let (tx, _rx) = mpsc::channel(64);
    let mut journal = journal(dir.path());
    let reply = call_reply("call_1", "peek", r#"{"id":"call_1"}"#);
    journal.message(&Message::user("hello")).expect("append");
    journal.message(&reply).expect("append");
    journal.commit().expect("commit");
    journal.close().expect("close the journal");

    // A read writes no call start, so a resume cannot tell whether it ran: it
    // runs again.
    let parent = Session::open(dir.path()).expect("open the session");
    let resumed = resume::load(&parent).expect("resume");
    drop(parent);
    assert_eq!(
        resumed.pending,
        [PendingCall {
            id: "call_1".into(),
            name: "peek".into(),
            class: None,
            qid: None,
            held: None,
        }]
    );

    let mut history = resumed.history.clone();
    let provider = FakeProvider::new(Vec::new());
    let mut open = None;
    let reason = Agent::new(&provider, &registry, config(), CancellationToken::new())
        .run_pending(&mut history, &mut open, &tx, &resumed.pending)
        .await;
    assert_eq!(reason, TurnEndReason::EndTurn);
    assert_eq!(
        history.last(),
        Some(&Message::tool_result("call_1", "ran call_1", false)),
        "the rerun answered the call"
    );

    // The next request carries that result.
    let provider = FakeProvider::new(vec![text_reply("done")]);
    Agent::new(&provider, &registry, config(), CancellationToken::new())
        .run(&mut history, None, &tx)
        .await;
    assert_eq!(provider.seen()[0].messages, 3);
}

#[tokio::test]
async fn a_set_level_and_mode_survive_a_compaction_base_on_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    let archive = tempfile::tempdir().expect("archive dir");
    let provider = FakeProvider::new(vec![text_reply("done")]);
    let registry = Registry::new();
    let (tx, mut inbox) = live::channel();
    let (events, mut rx) = mpsc::channel(64);
    // A history the compaction can shorten, as in `a_base_after_compaction`.
    let mut history = vec![
        Message::user("hello"),
        Message::assistant(AssistantMessage::new(
            "fake",
            "fake-model",
            vec![Content::text("hi")],
        )),
        Message::user("again"),
        Message::assistant(AssistantMessage::new(
            "fake",
            "fake-model",
            vec![Content::text("x".repeat(100_000))],
        )),
        Message::tool_result("call_0", "ok", false),
    ];
    let mut journal = journal(dir.path());
    let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new())
        .with_guard(asking_guard())
        .with_compact(Compact::with_budget(0, archive.path()));

    let session = live::run_live(
        &mut agent,
        &mut history,
        Some(&mut journal),
        &events,
        &mut inbox,
    );
    let client = async {
        tx.send(Command::Set {
            client: 1,
            settings: Settings {
                level: Some(Text::from("high")),
                mode: Some(PermissionMode::AcceptEdits),
                ..Settings::default()
            },
        })
        .await
        .expect("the session is open");
        tx.send(Command::Send {
            message: 1,
            client: 1,
            text: Text::from("go"),
            blocks: Vec::new(),
            mode: SendMode::Steer,
        })
        .await
        .expect("the session is open");
        drop(tx);
        let mut compacted = false;
        while let Some(event) = rx.recv().await {
            compacted |= matches!(event, Event::Compacted { .. });
            if matches!(event, Event::RunEnd(_)) {
                break;
            }
        }
        compacted
    };
    let ((), compacted) = tokio::join!(session, client);
    journal.close().expect("close the journal");
    assert!(compacted, "the run compacted");

    // A resume reads from the compaction's base, so the settings follow it.
    let parent = Session::open(dir.path()).expect("open the session");
    let resumed = resume::load(&parent).expect("resume");
    drop(parent);
    assert_eq!(
        resumed.state.compaction, 1,
        "the resume starts at the compaction"
    );
    assert_eq!(resumed.level.as_deref(), Some("high"));
    assert_eq!(resumed.mode, Some(PermissionMode::AcceptEdits));
}

#[tokio::test]
async fn a_tool_result_commit_event_comes_before_its_result_event() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut registry = Registry::new();
    registry.add(Probe::new(
        "peek",
        true,
        dir.path(),
        &Arc::new(Mutex::new(Vec::new())),
    ));
    let provider = FakeProvider::new(vec![
        tool_replies(&[("call_1", "peek", r#"{"id":"call_1"}"#)]),
        text_reply("done"),
    ]);
    let (tx, mut inbox) = live::channel();
    let (events, mut rx) = mpsc::channel(64);
    let mut history = Vec::new();
    let mut journal = journal(dir.path());
    let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new());

    let session = live::run_live(
        &mut agent,
        &mut history,
        Some(&mut journal),
        &events,
        &mut inbox,
    );
    let client = async {
        tx.send(Command::Send {
            message: 1,
            client: 1,
            text: Text::from("hello"),
            blocks: Vec::new(),
            mode: SendMode::Steer,
        })
        .await
        .expect("the session is open");
        drop(tx);
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            let end = matches!(event, Event::RunEnd(_));
            seen.push(event);
            if end {
                break;
            }
        }
        seen
    };
    let (_, seen) = tokio::join!(session, client);
    journal.close().expect("close the journal");

    let named = commits(&seen);
    assert!(
        named.windows(2).all(|pair| pair[0] < pair[1]),
        "a commit never names a smaller record: {named:?}"
    );
    // The result is committed, and named, while the batch still runs, before
    // the loop sends its result event.
    let result_at = seen
        .iter()
        .position(|event| matches!(event, Event::ToolResult { .. }))
        .expect("a result event");
    let ready_at = seen
        .iter()
        .position(|event| matches!(event, Event::ToolCallReady { .. }))
        .expect("a call event");
    assert!(
        seen[ready_at..result_at]
            .iter()
            .any(|event| matches!(event, Event::Committed { .. })),
        "{seen:?}"
    );
    assert_eq!(
        named.last().copied(),
        Some(read_records(dir.path()).len() as u64),
        "the last event names the last record of the part"
    );
}
