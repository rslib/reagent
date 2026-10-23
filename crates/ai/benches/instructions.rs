//! Gungraun benches: exact Callgrind instruction counts for the provider hot
//! paths, the SSE parsers and the request body builders.
//!
//! The parsers and the request builders are public, so a bench exercises the
//! real stream glue and the real body assembly without a network call. Every
//! input is built in a `setup` function, so its cost is not in the measured
//! part: only the parse, or the body build, is counted.
//!
//! Run with `cargo bench -p reagent-ai --bench instructions`, or with
//! `cargo bench -p reagent-ai --bench instructions -- --test` to run each
//! bench once without Callgrind. `bench/instructions.sh` runs both bench
//! targets against a base commit.

// gungraun's benchmark macros expand to std::fs and std::process code on
// Linux, which clippy.toml bans; the ban is for the agent, not for benches.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::hint::black_box;

use bytes::Bytes;
use gungraun::prelude::*;

use reagent_ai::anthropic::request::{
    BodyCache as AnthropicBodies, RequestBuilder as AnthropicBuilder,
};
use reagent_ai::anthropic::stream::StreamParser as AnthropicParser;
use reagent_ai::openai::BodyCache;
use reagent_ai::openai::chat::{
    MessageBody as ChatMessage, RequestBuilder as ChatBuilder, StreamParser as ChatParser,
};
use reagent_ai::openai::responses::request::{
    MessageBody as ResponsesMessage, RequestBuilder as ResponsesBuilder,
};
use reagent_ai::openai::responses::stream::StreamParser as ResponsesParser;
use reagent_ai::pi::request::{
    MessageBody as PiMessage, RequestBuilder as PiBuilder, tool_names as pi_tool_names,
};
use reagent_ai::pi::stream::StreamParser as PiParser;
use reagent_ai::sse::SseSplitter;
use reagent_ai::{
    AssistantMessage, Content, MaxTokensField, Message, ReasoningField, StopReason, StreamEvent,
    ToolCall, ToolSpec,
};

/// Text deltas in one parse bench.
const DELTAS: usize = 1_000;
/// Tool results in one request-build history.
const TOOL_RESULTS: usize = 50;
/// Bytes of text in one tool result.
const RESULT_BYTES: usize = 20 * 1024;

/// Plain delta text, taken as is from the wire.
const PLAIN: &str = "x";
/// Delta text that reaches the wire escaped: `a`, a newline, `b`.
const ESCAPED: &str = "a\\nb";

const MODEL_ANTHROPIC: &str = "claude-sonnet-4-5";
const MODEL_PI: &str = "balanced";
const MODEL_OPENAI: &str = "gpt-5";
const SYSTEM: &str = "You are a careful coding agent.";
const REASONING: &str = "medium";

/// Splits `body` into one `Bytes` chunk per line, as a socket would.
fn chunks(body: &str) -> Vec<Bytes> {
    body.split_inclusive('\n')
        .map(|line| Bytes::copy_from_slice(line.as_bytes()))
        .collect()
}

fn anthropic_stream(text: &str) -> Vec<Bytes> {
    chunks(
        &format!(
            "event: content_block_delta\n\
             data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
        )
        .repeat(DELTAS),
    )
}

fn chat_stream(text: &str) -> Vec<Bytes> {
    chunks(
        &format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}},\"finish_reason\":null}}]}}\n\n"
        )
        .repeat(DELTAS),
    )
}

fn responses_stream(text: &str) -> Vec<Bytes> {
    chunks(
        &format!(
            "data: {{\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"output_index\":0,\"delta\":\"{text}\"}}\n\n"
        )
        .repeat(DELTAS),
    )
}

fn pi_stream(text: &str) -> Vec<Bytes> {
    chunks(
        &format!("data: {{\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"{text}\"}}\n\n")
            .repeat(DELTAS),
    )
}

fn pi_plain() -> Vec<Bytes> {
    pi_stream(PLAIN)
}

fn pi_escaped() -> Vec<Bytes> {
    pi_stream(ESCAPED)
}

fn anthropic_plain() -> Vec<Bytes> {
    anthropic_stream(PLAIN)
}

fn anthropic_escaped() -> Vec<Bytes> {
    anthropic_stream(ESCAPED)
}

fn chat_plain() -> Vec<Bytes> {
    chat_stream(PLAIN)
}

fn chat_escaped() -> Vec<Bytes> {
    chat_stream(ESCAPED)
}

fn responses_plain() -> Vec<Bytes> {
    responses_stream(PLAIN)
}

fn responses_escaped() -> Vec<Bytes> {
    responses_stream(ESCAPED)
}

/// Feeds every chunk through `splitter` and `push`, counting the events.
fn feed<P>(chunks: &[Bytes], mut push: P) -> usize
where
    P: FnMut(&[u8], &mut Vec<StreamEvent>) -> Result<(), ()>,
{
    let mut splitter = SseSplitter::new();
    let mut buffer = Vec::new();
    let mut events = 0;
    for chunk in chunks {
        let mut lines = splitter.push(chunk);
        while let Some(line) = lines.next_line() {
            push(line, &mut buffer).unwrap();
            events += buffer.len();
            buffer.clear();
        }
    }
    events
}

#[library_benchmark]
#[bench::plain(setup = anthropic_plain)]
#[bench::escaped(setup = anthropic_escaped)]
fn sse_anthropic(chunks: Vec<Bytes>) -> usize {
    let mut parser = AnthropicParser::new();
    black_box(feed(&chunks, |line, out| {
        parser.push_line(line, out).map_err(|_| ())
    }))
}

#[library_benchmark]
#[bench::plain(setup = chat_plain)]
#[bench::escaped(setup = chat_escaped)]
fn sse_chat(chunks: Vec<Bytes>) -> usize {
    let mut parser = ChatParser::new();
    black_box(feed(&chunks, |line, out| {
        parser.push_line(line, out).map_err(|_| ())
    }))
}

#[library_benchmark]
#[bench::plain(setup = responses_plain)]
#[bench::escaped(setup = responses_escaped)]
fn sse_responses(chunks: Vec<Bytes>) -> usize {
    let mut parser = ResponsesParser::new();
    black_box(feed(&chunks, |line, out| {
        parser.push_line(line, out).map_err(|_| ())
    }))
}

// The same parse as `sse_chat`, on the pi-messages wire.
#[library_benchmark]
#[bench::plain(setup = pi_plain)]
#[bench::escaped(setup = pi_escaped)]
fn sse_pi(chunks: Vec<Bytes>) -> usize {
    let mut parser = PiParser::new(None);
    black_box(feed(&chunks, |line, out| {
        parser.push_line(line, out).map_err(|_| ())
    }))
}

/// 50 tool results of 20 KB, each one preceded by a user turn and an assistant
/// turn with the tool call, as a long session holds them.
fn history() -> Vec<Message> {
    let text = "fn main() { let value = 1; }\n".repeat(RESULT_BYTES / 28 + 1);
    let mut messages = Vec::with_capacity(TOOL_RESULTS * 3);
    for index in 0..TOOL_RESULTS {
        messages.push(Message::user(format!("read step {index}")));
        let mut assistant = AssistantMessage::new(
            "anthropic",
            MODEL_ANTHROPIC,
            vec![Content::ToolCall(ToolCall {
                id: format!("call_{index}").into(),
                name: "read".into(),
                arguments: "{\"path\":\"a.rs\"}".into(),
            })],
        );
        assistant.stop_reason = Some(StopReason::ToolUse);
        messages.push(Message::assistant(assistant));
        messages.push(Message::tool_result(
            format!("call_{index}"),
            text.clone(),
            false,
        ));
    }
    messages
}

fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read".into(),
            description: "Read a file.".into(),
            input_schema: serde_json::json!({"type": "object"}),
        },
        ToolSpec {
            name: "write".into(),
            description: "Write a file.".into(),
            input_schema: serde_json::json!({"type": "object"}),
        },
    ]
}

/// A request builder and the aligned bodies it assembles: the setup the body
/// benches build outside the measured part.
struct AnthropicBody {
    builder: AnthropicBuilder,
    bodies: AnthropicBodies,
}

fn anthropic_body() -> AnthropicBody {
    let messages = history();
    let tools = tools();
    let mut bodies = AnthropicBodies::default();
    bodies
        .align(&messages, "anthropic")
        .expect("the history serializes");
    let builder =
        AnthropicBuilder::new(MODEL_ANTHROPIC, 1024, Some(SYSTEM), Some(REASONING), &tools);
    AnthropicBody { builder, bodies }
}

#[library_benchmark]
#[bench::body(setup = anthropic_body)]
fn request_body_anthropic(setup: AnthropicBody) -> usize {
    black_box(setup.builder.body_parts(setup.bodies.bodies()).len())
}

struct ChatBody {
    builder: ChatBuilder,
    bodies: BodyCache<ChatMessage>,
}

fn chat_body() -> ChatBody {
    let messages = history();
    let tools = tools();
    let mut bodies: BodyCache<ChatMessage> = BodyCache::default();
    bodies
        .align(&messages, |message| {
            ChatMessage::new(message, "openai-chat")
        })
        .expect("the history serializes");
    let builder = ChatBuilder::new(
        MODEL_OPENAI,
        1024,
        Some(SYSTEM),
        Some(REASONING),
        &tools,
        MaxTokensField::MaxTokens,
        ReasoningField::Effort,
    );
    ChatBody { builder, bodies }
}

#[library_benchmark]
#[bench::body(setup = chat_body)]
fn request_body_chat(setup: ChatBody) -> usize {
    black_box(setup.builder.body_parts(setup.bodies.bodies()).len())
}

struct PiBody {
    builder: PiBuilder,
    bodies: BodyCache<PiMessage>,
}

fn pi_body() -> PiBody {
    let messages = history();
    let tools = tools();
    let names = pi_tool_names(&messages);
    let mut bodies: BodyCache<PiMessage> = BodyCache::default();
    bodies
        .align_at(&messages, |index, message| {
            PiMessage::new(message, "pi-messages", &names, index as u64 + 1)
        })
        .expect("the history serializes");
    let builder = PiBuilder::new(
        MODEL_PI,
        1024,
        Some(SYSTEM),
        Some(REASONING),
        Some("sess-1"),
        &tools,
    );
    PiBody { builder, bodies }
}

#[library_benchmark]
#[bench::body(setup = pi_body)]
fn request_body_pi(setup: PiBody) -> usize {
    black_box(setup.builder.body_parts(setup.bodies.bodies()).len())
}

struct ResponsesBody {
    builder: ResponsesBuilder,
    bodies: BodyCache<ResponsesMessage>,
}

fn responses_body() -> ResponsesBody {
    let messages = history();
    let tools = tools();
    let mut bodies: BodyCache<ResponsesMessage> = BodyCache::default();
    bodies
        .align(&messages, |message| {
            ResponsesMessage::new(message, "openai")
        })
        .expect("the history serializes");
    let builder = ResponsesBuilder::new(
        MODEL_OPENAI,
        1024,
        Some(SYSTEM),
        Some(REASONING),
        Some("sess-1"),
        &tools,
    );
    ResponsesBody { builder, bodies }
}

#[library_benchmark]
#[bench::body(setup = responses_body)]
fn request_body_responses(setup: ResponsesBody) -> usize {
    black_box(setup.builder.body_parts(setup.bodies.bodies()).len())
}

library_benchmark_group!(
    name = sse,
    benchmarks = [sse_anthropic, sse_chat, sse_responses, sse_pi]
);

library_benchmark_group!(
    name = request_bodies,
    benchmarks = [
        request_body_anthropic,
        request_body_chat,
        request_body_responses,
        request_body_pi
    ]
);

main!(library_benchmark_groups = sse, request_bodies);
