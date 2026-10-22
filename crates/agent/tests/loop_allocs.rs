//! Allocation count for the agent loop's per-delta path.
//!
//! A streamed answer arrives one small text delta at a time, so the loop's cost
//! per delta is what a long answer pays: mapping the stream event, sending it on
//! the channel and appending it to the assembler. The scripted reply and the
//! provider are built before the profiler starts, and a reply with no delta is
//! measured too, so only what each extra delta costs is counted: the loop's own
//! work, not the test fixture or the per-run setup.
//!
//! dhat allows one profiler at a time, so every measured case takes the lock
//! below: only the measured run happens inside the profiler.

use std::sync::Mutex;

use dhat::{Alloc, HeapStats, Profiler};
use reagent_agent::agent_loop::{Agent, AgentConfig};
use reagent_agent::tool::Registry;
use reagent_ai::fake::FakeProvider;
use reagent_ai::{Message, StopReason, StreamEvent, Text};
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
