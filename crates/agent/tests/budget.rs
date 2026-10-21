//! Heap and CPU budget for the agent loop over a long tool-using session.
//!
//! A scripted model makes [`TURNS`] turns, each calling one cheap in-test tool
//! that returns [`OUTPUT_BYTES`] bytes, then replies with text. No process,
//! network or real model is involved, so the numbers are the loop's own cost:
//! blocks and bytes per turn, peak heap with and without output masking, and
//! thread CPU time per turn. The runtime is current-thread, so the thread CPU
//! clock sees all the work.
//!
//! dhat allows one profiler at a time, so the cases run in sequence inside one
//! test.

use dhat::{Alloc, HeapStats, Profiler};
use futures_util::future::BoxFuture;
use nix::time::{ClockId, clock_gettime};
use reagent_agent::agent_loop::{Agent, AgentConfig};
use reagent_agent::tool::{Registry, Tool, ToolOutput};
use reagent_ai::fake::FakeProvider;
use reagent_ai::{Message, StopReason, StreamEvent, Text, ToolSpec};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// How many tool-calling turns the measured session makes.
const TURNS: usize = 100;

/// The size of each tool output.
const OUTPUT_BYTES: usize = 5_000;

/// Turns run before measuring, so lazy one-time setup is not counted.
const WARMUP_TURNS: usize = 5;

/// The most blocks one turn may allocate. Ids, names and the batch of one
/// call add none, so what is left is the assembler, the history and the tool.
const BLOCKS_PER_TURN: f64 = 30.0;

/// The most bytes one turn may allocate, about 2x the 12.2 KB measured.
const BYTES_PER_TURN: f64 = 24_500.0;

/// The most peak heap the unmasked session may reach, about 2x the 584 KB
/// measured: the history holds every 5 KB output.
const PEAK_BYTES: usize = 1_170_000;

/// The most peak heap the masked session may reach, about 2x the 279 KB
/// measured: old outputs are stubs once they pass the mask step.
const MASKED_PEAK_BYTES: usize = 560_000;

/// The most thread CPU one turn may take, in microseconds: about 2x the 1.6
/// measured in release, 10x that in debug.
fn cpu_micros_per_turn() -> f64 {
    if cfg!(debug_assertions) { 40.0 } else { 4.0 }
}

struct Cheap {
    spec: ToolSpec,
}

impl Tool for Cheap {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn read_only(&self) -> bool {
        true
    }

    fn run<'a>(
        &'a self,
        _args: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async { ToolOutput::ok("y".repeat(OUTPUT_BYTES)) })
    }
}

fn tool_turn(index: usize) -> Vec<StreamEvent> {
    let id = Text::from(format!("call_{index}"));
    vec![
        StreamEvent::ToolCallStart {
            id: id.clone(),
            name: Text::from("cheap"),
        },
        StreamEvent::ToolCallDelta {
            id: id.clone(),
            partial_json: Text::from("{}"),
        },
        StreamEvent::ToolCallEnd { id },
        StreamEvent::Stop(StopReason::ToolUse),
    ]
}

fn script(turns: usize) -> Vec<Vec<StreamEvent>> {
    let mut replies: Vec<_> = (0..turns).map(tool_turn).collect();
    replies.push(vec![
        StreamEvent::TextDelta(Text::from("done")),
        StreamEvent::Stop(StopReason::EndTurn),
    ]);
    replies
}

fn thread_cpu_micros() -> f64 {
    let time = clock_gettime(ClockId::CLOCK_THREAD_CPUTIME_ID).expect("thread cpu clock");
    time.tv_sec() as f64 * 1e6 + time.tv_nsec() as f64 / 1e3
}

struct Measured {
    blocks_per_turn: f64,
    bytes_per_turn: f64,
    peak_bytes: usize,
    cpu_micros_per_turn: f64,
}

/// Runs one scripted session of `turns` tool turns and measures it. The dhat
/// profiler slows every allocation, so CPU is only read from an unprofiled run.
fn session(turns: usize, masked: bool, profiled: bool) -> Measured {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let provider = FakeProvider::new(script(turns));
    let mut registry = Registry::new();
    registry.add(Cheap {
        spec: ToolSpec {
            name: "cheap".into(),
            description: "Cheap output.".into(),
            input_schema: json!({"type": "object"}),
        },
    });
    let config = AgentConfig {
        model: "fake-model".into(),
        max_tokens: 128,
        system: None,
        reasoning: None,
        cache_key: None,
        max_turns: None,
    };
    let (tx, mut rx) = mpsc::channel(256);
    let mut history = vec![Message::user("go")];
    let agent = Agent::new(&provider, &registry, config, CancellationToken::new());
    let mut agent = if masked { agent.with_mask() } else { agent };

    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = profiled.then(|| Profiler::builder().testing().build());
    let cpu_start = thread_cpu_micros();
    runtime.block_on(async {
        let run = async {
            agent.run(&mut history, None, &tx).await;
            drop(tx);
        };
        let drain = async { while rx.recv().await.is_some() {} };
        tokio::join!(run, drain);
    });
    let cpu = thread_cpu_micros() - cpu_start;
    let stats = profiled.then(HeapStats::get);
    drop(profiler);

    let turns = (turns + 1) as f64;
    Measured {
        blocks_per_turn: stats
            .as_ref()
            .map_or(0.0, |s| s.total_blocks as f64 / turns),
        bytes_per_turn: stats.as_ref().map_or(0.0, |s| s.total_bytes as f64 / turns),
        peak_bytes: stats.as_ref().map_or(0, |s| s.max_bytes),
        cpu_micros_per_turn: cpu / turns,
    }
}

/// How many unprofiled runs the CPU figure takes the fastest of.
const CPU_RUNS: usize = 5;

/// The fastest of [`CPU_RUNS`] sessions. A whole session takes about 200 us,
/// so one preemption or cold cache would fail a single run.
fn fastest_cpu(masked: bool) -> f64 {
    (0..CPU_RUNS)
        .map(|_| session(TURNS, masked, false).cpu_micros_per_turn)
        .fold(f64::INFINITY, f64::min)
}

#[test]
fn the_agent_loop_stays_within_its_heap_and_cpu_budget() {
    session(WARMUP_TURNS, false, true);

    let mut plain = session(TURNS, false, true);
    let mut masked = session(TURNS, true, true);
    plain.cpu_micros_per_turn = fastest_cpu(false);
    masked.cpu_micros_per_turn = fastest_cpu(true);

    for (name, m) in [("plain", &plain), ("masked", &masked)] {
        eprintln!(
            "{name}: {:.1} blocks/turn, {:.0} bytes/turn, peak {} bytes, {:.1} us cpu/turn",
            m.blocks_per_turn, m.bytes_per_turn, m.peak_bytes, m.cpu_micros_per_turn
        );
    }

    assert!(
        plain.blocks_per_turn <= BLOCKS_PER_TURN,
        "{:.1} blocks per turn, over {BLOCKS_PER_TURN}",
        plain.blocks_per_turn
    );
    assert!(
        plain.bytes_per_turn <= BYTES_PER_TURN,
        "{:.0} bytes per turn, over {BYTES_PER_TURN}",
        plain.bytes_per_turn
    );
    assert!(
        plain.peak_bytes <= PEAK_BYTES,
        "peak {} bytes, over {PEAK_BYTES}",
        plain.peak_bytes
    );
    assert!(
        masked.peak_bytes <= MASKED_PEAK_BYTES,
        "masked peak {} bytes, over {MASKED_PEAK_BYTES}",
        masked.peak_bytes
    );
    assert!(
        masked.peak_bytes < plain.peak_bytes,
        "masking must cut peak heap: {} masked vs {} plain",
        masked.peak_bytes,
        plain.peak_bytes
    );
    let cpu_budget = cpu_micros_per_turn();
    assert!(
        plain.cpu_micros_per_turn <= cpu_budget,
        "{:.1} us cpu per turn, over {cpu_budget}",
        plain.cpu_micros_per_turn
    );
}
