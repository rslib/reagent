//! Gungraun bench: exact Callgrind instruction counts for a long tool-using
//! run of the agent loop.
//!
//! The model is reagent-ai's `FakeProvider` and the tool is an in-test one that
//! returns a fixed output, so the bench touches no network, no process and no
//! real model: the count is the loop's own cost over [`TURNS`] tool turns. The
//! provider, the registry and the history are built in the `setup` function,
//! so only the run is measured.
//!
//! Run with `cargo bench -p reagent-agent --bench instructions`, or with
//! `cargo bench -p reagent-agent --bench instructions -- --test` to run the
//! bench once without Callgrind. `bench/instructions.sh` runs every bench
//! target against a base commit.

// gungraun's benchmark macros expand to std::fs and std::process code on
// Linux, which clippy.toml bans; the ban is for the agent, not for benches.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::hint::black_box;

use futures_util::future::BoxFuture;
use gungraun::prelude::*;
use reagent_agent::agent_loop::{Agent, AgentConfig};
use reagent_agent::event::Event;
use reagent_agent::tool::{Registry, Tool, ToolOutput};
use reagent_ai::fake::FakeProvider;
use reagent_ai::{Message, StopReason, StreamEvent, Text, ToolSpec};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Tool-calling turns one run makes.
const TURNS: usize = 100;

/// Bytes each tool output carries, as a 5 KB file read reports.
const OUTPUT_BYTES: usize = 5_000;

/// How many events the channel holds, so a slow reader never blocks the run.
const CHANNEL: usize = 256;

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

/// One tool turn: a call to the cheap tool that the loop then runs.
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

/// The scripted replies: [`TURNS`] tool turns, then a text answer.
fn script() -> Vec<Vec<StreamEvent>> {
    let mut replies: Vec<Vec<StreamEvent>> = (0..TURNS).map(tool_turn).collect();
    replies.push(vec![
        StreamEvent::TextDelta(Text::from("done")),
        StreamEvent::Stop(StopReason::EndTurn),
    ]);
    replies
}

/// The runtime, the scripted provider, the registry and the history: all built
/// outside the measured part.
struct Run {
    runtime: tokio::runtime::Runtime,
    provider: FakeProvider,
    registry: Registry,
    history: Vec<Message>,
}

fn run_setup() -> Run {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("the current-thread runtime builds");
    let provider = FakeProvider::new(script());
    let mut registry = Registry::new();
    registry.add(Cheap {
        spec: ToolSpec {
            name: "cheap".into(),
            description: "Cheap output.".into(),
            input_schema: json!({"type": "object"}),
        },
    });
    Run {
        runtime,
        provider,
        registry,
        history: vec![Message::user("go")],
    }
}

// Runs the scripted session to its end, draining the events beside it so the
// measured run is the loop's own work and not a blocked channel.
#[library_benchmark]
#[bench::turns(setup = run_setup)]
fn agent_tool_turns(setup: Run) -> usize {
    let Run {
        runtime,
        provider,
        registry,
        mut history,
    } = setup;
    let config = AgentConfig {
        model: "fake-model".into(),
        max_tokens: 128,
        system: None,
        reasoning: None,
        cache_key: None,
        max_turns: None,
    };
    let token = CancellationToken::new();
    let (sender, mut receiver) = mpsc::channel::<Event>(CHANNEL);
    let mut agent = Agent::new(&provider, &registry, config, token);
    let events = runtime.block_on(async {
        let run = async {
            agent.run(&mut history, None, &sender).await;
            drop(sender);
        };
        let drain = async {
            let mut count = 0;
            while receiver.recv().await.is_some() {
                count += 1;
            }
            count
        };
        tokio::join!(run, drain).1
    });
    // The run has to reach the end of the script, or the bench measures an
    // early stop: the prompt, one reply and one tool result per turn, and the
    // answer the script ends with.
    assert_eq!(
        history.len(),
        2 + TURNS * 2,
        "the run made every tool turn and stopped at the text answer"
    );
    black_box(events + history.len())
}
library_benchmark_group!(name = agent_loop, benchmarks = [agent_tool_turns]);

main!(library_benchmark_groups = agent_loop);
