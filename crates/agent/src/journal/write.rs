//! Tests of the records the loop writes: their order, the sync before a
//! side-effect call runs, the commit after a result and the base after a
//! compaction.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use reagent_ai::fake::FakeProvider;
use reagent_ai::{AssistantMessage, Content, Message, StopReason, StreamEvent, Text, ToolSpec};
use reagent_store::{Header, Session};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{Agent, AgentConfig};
use crate::compact::Compact;
use crate::event::Event;
use crate::journal::Journal;
use crate::journal::record::{self, Record};
use crate::tool::{Registry, Tool, ToolOutput};

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
