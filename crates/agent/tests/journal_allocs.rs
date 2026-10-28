//! Allocation counts for the journal writer and the stuck policy.
//!
//! Appending a message must cost at most one block, the record handed to the
//! store: the message is encoded from borrowed fields into a reused buffer and
//! copied once into the record's bytes. A huge tool result must not keep its
//! megabytes in the encoder buffer for the next small message. The stuck
//! policy reads the `path` argument with borrowed serde and keys its counters
//! on a hash, so a `write` of a large body copies neither the body nor a key
//! string.
//!
//! dhat allows one profiler at a time, so every measured case takes the shared
//! lock below as its first line: only the measured call runs inside the
//! profiler.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use dhat::{Alloc, HeapStats, Profiler};
use reagent_agent::archive::Archive;
use reagent_agent::checkpoint;
use reagent_agent::compact::{BUDGET, Compact};
use reagent_agent::journal::{Journal, MAX_BUFFER, kinds, record};
use reagent_agent::policy::stuck::Stuck;
use reagent_agent::policy::{Decision, TurnPolicy, TurnView};
use reagent_ai::{AssistantMessage, Content, Message, Text, ToolCall, ToolResult};
use reagent_store::Header;

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// Serializes the profiled sections so two profilers never overlap.
static MEASURE: Mutex<()> = Mutex::new(());

/// How many messages the measured append run writes.
const MESSAGES: usize = 1000;

/// The most blocks one append may allocate: the record handed to the store.
const BLOCKS_PER_MESSAGE: f64 = 1.1;

/// The most blocks a decode of one turn (a reply with one tool call, then its
/// result) may allocate: one per text field (provider, model, call id, name,
/// arguments; call id, result) and the reply's content list. Copying each
/// field through a `String` first cost 15.
const BLOCKS_PER_DECODED_TURN: f64 = 8.0;

/// The bytes of the tool result the buffer test appends.
const HUGE_RESULT: usize = 4 * 1024 * 1024;

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

/// Opens a journal over `dir`, as a run does.
fn journal(dir: &std::path::Path) -> Journal {
    let header = Header::new(
        "session".into(),
        reagent_store::paths::now_ms(),
        None,
        Bytes::new(),
    );
    Journal::create(dir, header).expect("create the journal")
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
fn perf_journal_append_allocates_at_most_two_blocks_per_message() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("temp dir");
    let messages: Vec<Message> = (0..MESSAGES)
        .map(|n| Message::user(Arc::from(format!("message {n}"))))
        .collect();
    let runtime = runtime();
    let mut journal = journal(dir.path());
    // The writer thread takes its one-time buffers outside the measured part.
    journal
        .message(&Message::user("warm the writer"))
        .expect("warm");
    runtime.block_on(async { journal.sync().await.expect("sync") });

    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = Profiler::builder().testing().build();
    for message in &messages {
        journal.message(message).expect("append");
    }
    let stats = HeapStats::get();
    drop(profiler);
    journal.close().expect("close");

    let per_message = stats.total_blocks as f64 / MESSAGES as f64;
    eprintln!(
        "journal append: {} blocks for {MESSAGES} messages = {per_message:.3} per message, {} bytes",
        stats.total_blocks, stats.total_bytes
    );
    assert!(
        per_message <= BLOCKS_PER_MESSAGE,
        "{} blocks for {MESSAGES} messages = {per_message:.3} per message, over {BLOCKS_PER_MESSAGE}",
        stats.total_blocks
    );
}

#[test]
fn perf_journal_buffer_after_a_huge_result_stays_small() {
    let _guard = lock();
    let dir = tempfile::tempdir().expect("temp dir");
    let mut journal = journal(dir.path());
    // Built before the measured part, so only the append is counted.
    let huge = Message::tool_result("call_1", "x".repeat(HUGE_RESULT), false);

    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = Profiler::builder().testing().build();
    journal.message(&huge).expect("append the huge result");
    let after_huge = journal.buffered_bytes();
    journal.message(&Message::user("after")).expect("append");
    let after_small = journal.buffered_bytes();
    let stats = HeapStats::get();
    drop(profiler);
    journal.close().expect("close");

    eprintln!(
        "journal buffer: {after_huge} bytes after the 4 MiB result, {after_small} after the next message, {} bytes allocated",
        stats.total_bytes
    );
    assert!(
        after_huge <= MAX_BUFFER,
        "{after_huge} bytes kept after a 4 MiB result, over {MAX_BUFFER}"
    );
    assert!(
        after_small <= MAX_BUFFER,
        "{after_small} bytes kept for the next message, over {MAX_BUFFER}"
    );
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
            images: Vec::new(),
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

#[test]
fn perf_journal_decode_copies_each_text_field_once() {
    let _guard = lock();
    let mut history = Vec::new();
    for n in 0..MESSAGES {
        turn(
            &mut history,
            n,
            "read",
            "{\"path\":\"a.rs\"}",
            Text::from("fn main() {}"),
        );
    }
    let payloads: Vec<Bytes> = history
        .iter()
        .map(|message| {
            let mut out = Vec::new();
            record::encode_message(&mut out, message);
            Bytes::from(out)
        })
        .collect();
    let mut decoded = Vec::with_capacity(payloads.len());

    let profiler = Profiler::builder().testing().build();
    for payload in &payloads {
        decoded.push(record::decode(kinds::MESSAGE, payload).expect("decode"));
    }
    let stats = HeapStats::get();
    drop(profiler);

    assert_eq!(decoded.len(), history.len());
    let per_turn = stats.total_blocks as f64 / MESSAGES as f64;
    eprintln!(
        "journal decode: {} blocks for {MESSAGES} turns = {per_turn:.3} per turn, {} bytes",
        stats.total_blocks, stats.total_bytes
    );
    assert!(
        per_turn <= BLOCKS_PER_DECODED_TURN,
        "{} blocks for {MESSAGES} turns = {per_turn:.3} per turn, over {BLOCKS_PER_DECODED_TURN}",
        stats.total_blocks
    );
}
