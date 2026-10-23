//! Allocation count for the agent loop's per-delta and per-read paths.
//!
//! A streamed answer arrives one small text delta at a time, so the loop's cost
//! per delta is what a long answer pays: mapping the stream event, sending it on
//! the channel and appending it to the assembler. The scripted reply and the
//! provider are built before the profiler starts, and a reply with no delta is
//! measured too, so only what each extra delta costs is counted: the loop's own
//! work, not the test fixture or the per-run setup.
//!
//! A tool turn has the same shape: the same scripted session runs with a read
//! tool that reports nothing, with one that reports a fresh range every call and
//! with one that reports the same range every call, so the blocks a read adds
//! and the blocks a note adds are counted against a loop that does neither.
//!
//! dhat allows one profiler at a time, so every measured case takes the lock
//! below: only the measured run happens inside the profiler.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use dhat::{Alloc, HeapStats, Profiler};
use futures_util::future::BoxFuture;
use reagent_agent::agent_loop::{Agent, AgentConfig};
use reagent_agent::tool::{Effects, Identity, Registry, Seen, Tool, ToolOutput};
use reagent_ai::fake::FakeProvider;
use reagent_ai::{Message, StopReason, StreamEvent, Text, ToolSpec};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// Serializes the profiled sections so two profilers never overlap.
static MEASURE: Mutex<()> = Mutex::new(());

/// How many text deltas the measured reply streams.
const DELTAS: usize = 1000;

/// The most blocks one delta may allocate: the loop maps the event, which is a
/// refcounted `Text` clone, sends it on the channel and appends it to the
/// assembler's text, so nothing per delta is left to allocate.
const BLOCKS_PER_DELTA: f64 = 0.1;

/// The most bytes one delta may allocate. What is left is the tokio channel,
/// which allocates a block of 32 slots when a send crosses into it, so a delta
/// costs one 32nd of a block, near 40 bytes.
const BYTES_PER_DELTA: f64 = 64.0;

fn lock() -> std::sync::MutexGuard<'static, ()> {
    MEASURE.lock().unwrap_or_else(|error| error.into_inner())
}

/// A reply of `deltas` one-character text deltas and a normal end.
fn reply(deltas: usize) -> Vec<StreamEvent> {
    let mut events: Vec<StreamEvent> = (0..deltas)
        .map(|_| StreamEvent::TextDelta(Text::from("x")))
        .collect();
    events.push(StreamEvent::Stop(StopReason::EndTurn));
    events
}

fn config() -> AgentConfig {
    AgentConfig {
        model: "fake-model".into(),
        max_tokens: 128,
        system: None,
        reasoning: None,
        cache_key: None,
        max_turns: None,
    }
}

/// What one measured run allocated, and what the receiver got.
struct Measured {
    blocks: u64,
    bytes: u64,
    deltas: usize,
    text_bytes: usize,
}

/// Runs one streamed answer of `deltas` deltas through the loop and measures
/// it, with the receiver drained in the same runtime.
fn measure(deltas: usize) -> Measured {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let provider = FakeProvider::new(vec![reply(deltas)]);
    let registry = Registry::new();
    let (tx, mut rx) = mpsc::channel(256);
    let mut history = vec![Message::user("go")];
    let agent = Agent::new(&provider, &registry, config(), CancellationToken::new());
    let mut agent = agent;

    let profiler = Profiler::builder().testing().build();
    let text_bytes = runtime.block_on(async {
        let run = async {
            agent.run(&mut history, None, &tx).await;
            drop(tx);
        };
        let drain = async {
            let mut received = 0;
            let mut text_bytes = 0;
            while let Some(event) = rx.recv().await {
                if let reagent_agent::event::Event::TextDelta(text) = event {
                    received += 1;
                    text_bytes += text.len();
                }
            }
            (received, text_bytes)
        };
        let ((), seen) = tokio::join!(run, drain);
        seen
    });
    let stats = HeapStats::get();
    drop(profiler);

    Measured {
        blocks: stats.total_blocks,
        bytes: stats.total_bytes,
        deltas: text_bytes.0,
        text_bytes: text_bytes.1,
    }
}

#[test]
fn one_delta_costs_no_block() {
    let _lock = lock();
    // One unmeasured run first, so lazy one-time setup is not counted.
    measure(2);
    let base = measure(1);
    let long = measure(DELTAS);

    // The reply reaches the receiver whole and in one piece.
    assert_eq!(base.deltas, 1);
    assert_eq!(long.deltas, DELTAS);
    assert_eq!(long.text_bytes, DELTAS);

    let extra = (DELTAS - 1) as f64;
    let blocks = (long.blocks - base.blocks) as f64 / extra;
    let bytes = (long.bytes - base.bytes) as f64 / extra;
    eprintln!("per delta: {blocks:.3} blocks, {bytes:.1} bytes");
    eprintln!(
        "per run: {} blocks, {} bytes at {DELTAS} deltas",
        long.blocks, long.bytes
    );
    assert!(
        blocks <= BLOCKS_PER_DELTA,
        "{blocks:.3} blocks per delta, over {BLOCKS_PER_DELTA}"
    );
    assert!(
        bytes <= BYTES_PER_DELTA,
        "{bytes:.1} bytes per delta, over {BYTES_PER_DELTA}"
    );
}

/// Read turns in a measured re-read session.
const READ_TURNS: usize = 100;

/// The most blocks one fresh read may add over the same loop whose tool reports
/// no lines, in tenths: 20 measured, the path text and the boxed `Seen`. The box
/// keeps every other tool result small as it moves through the batch; unboxed,
/// that move cost the tool-turn bench 1.9% of its instructions. An argument
/// parse, an effects list or a canonicalize per call would put it over.
const FRESH_BLOCKS_PER_READ_X10: u64 = 22;

/// The most blocks one noted re-read may add over a fresh one, in tenths: 19
/// measured, which is 193 blocks over 100 turns: the buffer the noted text is
/// built in and the `Arc<str>` it becomes. A copy of the content into one more
/// buffer would put it over.
const NOTED_BLOCKS_PER_READ_X10: u64 = 22;

/// What a scripted read reports about the lines it showed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lines {
    /// Nothing, the way a tool result looked before the note existed.
    No,
    /// A range no earlier call showed, so no read is a re-read.
    Fresh,
    /// The same range every call, so every read but the first is a re-read.
    Same,
}

/// A read tool that reports one line of a file.
struct Reads {
    spec: ToolSpec,
    key: Identity,
    next: AtomicU64,
    lines: Lines,
}

impl Reads {
    fn seen(&self, start: u64) -> Seen {
        Seen {
            path: Text::from("f.txt"),
            key: self.key.clone(),
            start,
            end: start,
            size: 4,
            modified: None,
        }
    }
}

impl Tool for Reads {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn read_only(&self) -> bool {
        true
    }

    fn effects(&self, _args: &str) -> Effects {
        // The real read tool's effects cost a path and a list, so a loop that
        // asks a read-only tool for them shows up in these numbers.
        Effects {
            reads: vec![std::path::PathBuf::from("f.txt")],
            writes: Vec::new(),
        }
    }

    fn run<'a>(
        &'a self,
        _args: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let output = ToolOutput::ok("1\ta\n");
            let start = match self.lines {
                Lines::No => return output,
                Lines::Same => 1,
                Lines::Fresh => self.next.fetch_add(1, Ordering::Relaxed),
            };
            output.with_seen(self.seen(start))
        })
    }
}

fn read_spec() -> ToolSpec {
    ToolSpec {
        name: "read".into(),
        description: "Read lines of a file.".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }
}

/// One reply that calls the read tool, and stops for it.
fn read_turn(index: usize) -> Vec<StreamEvent> {
    let id = Text::from(format!("call_{index}"));
    vec![
        StreamEvent::ToolCallStart {
            id: id.clone(),
            name: Text::from("read"),
        },
        StreamEvent::ToolCallDelta {
            id: id.clone(),
            partial_json: Text::from("{}"),
        },
        StreamEvent::ToolCallEnd { id },
        StreamEvent::Stop(StopReason::ToolUse),
    ]
}

/// The scripted replies: `turns` read turns, then a text answer.
fn read_script(turns: usize) -> Vec<Vec<StreamEvent>> {
    let mut replies: Vec<Vec<StreamEvent>> = (0..turns).map(read_turn).collect();
    replies.push(vec![
        StreamEvent::TextDelta(Text::from("done")),
        StreamEvent::Stop(StopReason::EndTurn),
    ]);
    replies
}

/// What one measured read session allocated, and how many results it noted.
struct ReadMeasure {
    blocks: u64,
    noted: usize,
}

/// Runs a scripted session of `turns` read calls under a fresh profiler, with
/// the provider, the registry and the history built before it starts.
fn measure_reads(
    runtime: &tokio::runtime::Runtime,
    key: &Identity,
    turns: usize,
    lines: Lines,
) -> ReadMeasure {
    let provider = FakeProvider::new(read_script(turns));
    let mut registry = Registry::new();
    registry.add(Reads {
        spec: read_spec(),
        key: key.clone(),
        next: AtomicU64::new(1),
        lines,
    });
    let (tx, mut rx) = mpsc::channel(256);
    let mut history = vec![Message::user("go")];
    let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new());

    let profiler = Profiler::builder().testing().build();
    runtime.block_on(async {
        let run = async {
            agent.run(&mut history, None, &tx).await;
            drop(tx);
        };
        let drain = async { while rx.recv().await.is_some() {} };
        tokio::join!(run, drain);
    });
    let stats = HeapStats::get();
    // Counted after the snapshot, so this scan is not in the numbers.
    let noted = history
        .iter()
        .filter(|message| {
            matches!(message, Message::ToolResult(result) if result.content.contains("[Note:"))
        })
        .count();
    drop(profiler);

    assert_eq!(
        history.len(),
        2 + 2 * turns,
        "the run made every read turn and stopped at the text answer"
    );
    ReadMeasure {
        blocks: stats.total_blocks,
        noted,
    }
}

/// A read the ledger has never seen costs nothing per call but its path text
/// and its boxed `Seen`.
#[test]
fn reread_a_fresh_read_costs_two_blocks() {
    let _lock = lock();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let tmp = tempfile::tempdir().expect("temp dir");
    let key = runtime.block_on(async {
        let path = tmp.path().join("f.txt");
        tokio::fs::write(&path, "a\n").await.expect("the file");
        let metadata = tokio::fs::metadata(&path).await.expect("metadata");
        Identity::new(&metadata, &path).await
    });

    // One unmeasured run first, so lazy one-time setup is not counted.
    measure_reads(&runtime, &key, 2, Lines::No);
    let base = measure_reads(&runtime, &key, READ_TURNS, Lines::No);
    let fresh = measure_reads(&runtime, &key, READ_TURNS, Lines::Fresh);

    assert_eq!(base.noted, 0);
    assert_eq!(fresh.noted, 0, "a fresh range is never noted");
    let extra = fresh.blocks.saturating_sub(base.blocks);
    let per_read_x10 = extra * 10 / READ_TURNS as u64;
    eprintln!(
        "blocks: {base} with no lines reported, {fresh} with a fresh read each turn, \
         {per_read_x10}/10 per read",
        base = base.blocks,
        fresh = fresh.blocks,
    );
    assert!(
        per_read_x10 <= FRESH_BLOCKS_PER_READ_X10,
        "{extra} blocks over {READ_TURNS} read turns, {per_read_x10}/10 per read over \
         {FRESH_BLOCKS_PER_READ_X10}/10"
    );
}

/// A noted re-read costs the noted text and nothing else.
#[test]
fn reread_a_noted_read_adds_two_blocks() {
    let _lock = lock();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let tmp = tempfile::tempdir().expect("temp dir");
    let key = runtime.block_on(async {
        let path = tmp.path().join("f.txt");
        tokio::fs::write(&path, "a\n").await.expect("the file");
        let metadata = tokio::fs::metadata(&path).await.expect("metadata");
        Identity::new(&metadata, &path).await
    });

    // One unmeasured run first, so lazy one-time setup is not counted.
    measure_reads(&runtime, &key, 2, Lines::No);
    let fresh = measure_reads(&runtime, &key, READ_TURNS, Lines::Fresh);
    let noted = measure_reads(&runtime, &key, READ_TURNS, Lines::Same);

    assert_eq!(
        noted.noted,
        READ_TURNS - 1,
        "every read but the first is a re-read"
    );
    let extra = noted.blocks.saturating_sub(fresh.blocks);
    let per_read_x10 = extra * 10 / READ_TURNS as u64;
    eprintln!(
        "blocks: {} with a fresh read each turn, {} with a noted one, {per_read_x10}/10 per read",
        fresh.blocks, noted.blocks
    );
    assert!(
        per_read_x10 <= NOTED_BLOCKS_PER_READ_X10,
        "{extra} blocks over {READ_TURNS} read turns, {per_read_x10}/10 per read over \
         {NOTED_BLOCKS_PER_READ_X10}/10"
    );
}
