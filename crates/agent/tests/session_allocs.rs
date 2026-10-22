//! Allocation counts for the session writer and the stuck policy.
//!
//! Appending a message must cost one block, the id handed back to the caller:
//! the entry is serialized from borrowed fields into a reused buffer, so the
//! message is not cloned and the line is not a fresh `String` per append. The
//! stuck policy reads the `path` argument with borrowed serde and keys its
//! counters on a hash, so a `write` of a large body copies neither the body
//! nor a key string.
//!
//! dhat allows one profiler at a time, so every measured case takes the lock
//! below: only the measured call runs inside the profiler.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dhat::{Alloc, HeapStats, Profiler};
use reagent_agent::archive::Archive;
use reagent_agent::checkpoint;
use reagent_agent::compact::{BUDGET, Compact};
use reagent_agent::policy::stuck::Stuck;
use reagent_agent::policy::{Decision, TurnPolicy, TurnView};
use reagent_agent::session::Session;
use reagent_ai::{AssistantMessage, Content, Message, Text, ToolCall, ToolResult};

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// Serializes the profiled sections so two profilers never overlap.
static MEASURE: Mutex<()> = Mutex::new(());

/// How many messages the measured append run writes.
const MESSAGES: usize = 1000;

/// The most blocks one append may allocate: the id handed back to the caller.
const BLOCKS_PER_MESSAGE: f64 = 2.0;

/// The most blocks one stuck-policy turn may allocate: the counter table and
/// little else. Parsing the arguments into a `Value` cost 11 before the fix.
const BLOCKS_PER_TURN: u64 = 4;

/// The most bytes one stuck-policy turn may allocate. A copy of the 1 MB
/// `content` argument would blow through this.
const BYTES_PER_TURN: u64 = 64 * 1024;

/// The most bytes the heap may hold at once while a compaction reads the
/// changed files. The 10 MB file is read up to the cap, not whole.
const COMPACT_PEAK_BYTES: usize = 1024 * 1024;

/// The most bytes one checkpoint may allocate. The 1 MB `content` argument of
/// the `write` must stay in place, not be parsed into a `Value`.
const CHECKPOINT_BYTES: u64 = 16 * 1024;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    MEASURE.lock().unwrap_or_else(|error| error.into_inner())
}

/// One turn: a reply with one tool call, then its result.
fn turn(history: &mut Vec<Message>, n: usize, name: &str, arguments: &str, output: Text) {
    let id = format!("call_{n}");
    history.push(Message::assistant(AssistantMessage::new(
        "fake",
        "m",
        vec![Content::ToolCall(ToolCall {
            id: Text::from(id.as_str()),
            name: Text::from(name),
            arguments: Text::from(arguments),
        })],
    )));
    history.push(Message::tool_result(id, output, false));
}

/// Sets the last reply's prompt size, which is what starts a compaction.
fn set_prompt(history: &mut [Message], tokens: u64) {
    if let Some(Message::Assistant(reply)) = history
        .iter_mut()
        .rev()
        .find(|message| matches!(message, Message::Assistant(_)))
    {
        reply.usage.input_tokens = tokens;
    }
}

/// The text of a user message, or the empty string for another kind.
fn user_text(message: &Message) -> &str {
    match message {
        Message::User { content } => content
            .iter()
            .find_map(|block| match block {
                Content::Text { text } => Some(&**text),
                _ => None,
            })
            .unwrap_or(""),
        _ => "",
    }
}

#[test]
fn perf_session_append_allocates_at_most_two_blocks_per_message() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("session.jsonl");
    let messages: Vec<Message> = (0..MESSAGES)
        .map(|n| Message::user(Arc::from(format!("message {n}"))))
        .collect();
    let runtime = runtime();

    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = Profiler::builder().testing().build();
    runtime.block_on(async {
        let mut session = Session::open(&path).await.expect("open");
        for message in &messages {
            session.append(message).await.expect("append");
        }
        session.close().await.expect("close");
    });
    let stats = HeapStats::get();
    drop(profiler);

    let per_message = stats.total_blocks as f64 / MESSAGES as f64;
    eprintln!(
        "session append: {} blocks for {MESSAGES} messages = {per_message:.3} per message, {} bytes",
        stats.total_blocks, stats.total_bytes
    );
    assert!(
        per_message <= BLOCKS_PER_MESSAGE,
        "{} blocks for {MESSAGES} messages = {per_message:.3} per message, over {BLOCKS_PER_MESSAGE}",
        stats.total_blocks
    );
}

#[test]
fn perf_session_entry_bytes_keep_the_old_format() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("session.jsonl");
    let first = Message::user(Arc::from("message 1"));
    let second = Message::user(Arc::from("message 2"));
    let runtime = runtime();

    let (a, b, text) = runtime.block_on(async {
        let mut session = Session::open(&path).await.expect("open");
        let a = session.append(&first).await.expect("append a");
        let b = session.append(&second).await.expect("append b");
        session.close().await.expect("close");
        let text = tokio::fs::read_to_string(&path).await.expect("read");
        (a, b, text)
    });

    // The old writer's bytes: `id`, then `parentId` only off the root, then the
    // message, with no spaces and one newline per entry.
    let one = serde_json::to_string(&first).expect("first json");
    let two = serde_json::to_string(&second).expect("second json");
    let expected = format!(
        "{{\"id\":\"{a}\",\"message\":{one}}}\n\
         {{\"id\":\"{b}\",\"parentId\":\"{a}\",\"message\":{two}}}\n"
    );
    assert_eq!(text, expected);

    // The id is the per-file prefix plus a four-digit hex counter.
    assert!(a.ends_with("0000"), "{a}");
    assert!(b.ends_with("0001"), "{b}");
    assert_eq!(&a[..a.len() - 4], &b[..b.len() - 4], "one prefix per file");
}

#[test]
fn perf_stuck_write_turn_does_not_copy_the_content() {
    let _guard = lock();
    let body = "x".repeat(1_000_000);
    let args = format!(r#"{{"path":"src/a.rs","content":"{body}"}}"#);
    let calls = vec![(
        ToolCall {
            id: "c".into(),
            name: "write".into(),
            arguments: Text::from(args),
        },
        ToolResult {
            call_id: "c".into(),
            content: Text::from("ok"),
            is_error: false,
        },
    )];
    let assistant = AssistantMessage::new("fake", "m", vec![Content::text("x")]);
    let view = TurnView {
        index: 1,
        elapsed: Duration::from_secs(1),
        ran_tools: true,
        assistant: &assistant,
        tools: &calls,
        changed_files: None,
    };
    let mut policy = Stuck::default();

    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = Profiler::builder().testing().build();
    let decision = policy.after_turn(&view);
    let stats = HeapStats::get();
    drop(profiler);

    eprintln!(
        "stuck write turn: {} blocks, {} bytes",
        stats.total_blocks, stats.total_bytes
    );
    assert_eq!(decision, Decision::Continue);
    assert!(
        stats.total_blocks <= BLOCKS_PER_TURN,
        "{} blocks for one write turn, over {BLOCKS_PER_TURN}",
        stats.total_blocks
    );
    assert!(
        stats.total_bytes < BYTES_PER_TURN,
        "{} bytes for one write turn, over {BYTES_PER_TURN}: the 1 MB content was copied",
        stats.total_bytes
    );
}

#[test]
fn perf_compact_reads_a_huge_changed_file_without_holding_it() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("temp dir");
    let file = "big.txt";
    let mut history = vec![Message::user(Arc::from("fix the bug"))];
    turn(
        &mut history,
        1,
        "edit",
        &format!(r#"{{"path":"{file}","old_string":"a","new_string":"b"}}"#),
        Text::from("ok"),
    );
    for n in 2..=60 {
        turn(
            &mut history,
            n,
            "bash",
            r#"{"command":"ls"}"#,
            Arc::from("x".repeat(4_000)),
        );
    }
    set_prompt(&mut history, BUDGET);
    // The expected message, built before the measured step.
    let expected = format!(
        "Current text of files changed before the last checkpoint:\n\n== {file} ==\n{}\n[cut at 20 KB; read the file for the rest]",
        "a".repeat(20 * 1024)
    );
    let runtime = runtime();
    let big = "a".repeat(10 * 1024 * 1024);
    runtime
        .block_on(tokio::fs::write(dir.path().join(file), &big))
        .expect("write the 10 MB file");
    drop(big);
    let archive = Archive::default();
    let compact = Compact::new(dir.path());

    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = Profiler::builder().testing().build();
    let done = runtime.block_on(compact.apply(&mut history, &archive));
    let stats = HeapStats::get();
    drop(profiler);

    eprintln!(
        "compaction with a 10 MB file: {} bytes in use at the peak, {} blocks, {} bytes allocated",
        stats.max_bytes, stats.total_blocks, stats.total_bytes
    );
    assert!(done.is_some(), "the history was over the budget");
    assert_eq!(
        user_text(&history[2]),
        expected,
        "the files message changed"
    );
    assert!(
        stats.max_bytes < COMPACT_PEAK_BYTES,
        "{} bytes in use at the peak, over {COMPACT_PEAK_BYTES}: the 10 MB file was held whole",
        stats.max_bytes
    );
}

#[test]
fn perf_compact_a_checkpoint_path_leaves_the_arguments_in_place() {
    let _guard = lock();
    let body = "x".repeat(1_000_000);
    let arguments = format!(r#"{{"path":"src/a.rs","content":"{body}"}}"#);
    let messages = vec![
        Message::assistant(AssistantMessage::new(
            "fake",
            "m",
            vec![Content::ToolCall(ToolCall {
                id: Text::from("c"),
                name: Text::from("write"),
                arguments: Text::from(arguments),
            })],
        )),
        Message::tool_result("c", "ok", false),
    ];
    let archive = Archive::default();

    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = Profiler::builder().testing().build();
    let made = checkpoint::build(1, &messages, &archive);
    let stats = HeapStats::get();
    drop(profiler);

    eprintln!(
        "checkpoint of a 1 MB write: {} bytes in {} blocks",
        stats.total_bytes, stats.total_blocks
    );
    assert_eq!(made.changed, vec!["src/a.rs"]);
    assert!(
        stats.total_bytes < CHECKPOINT_BYTES,
        "{} bytes for one checkpoint, over {CHECKPOINT_BYTES}: the 1 MB arguments were parsed into a Value",
        stats.total_bytes
    );
}
