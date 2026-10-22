//! Divan benches for the provider hot paths: SSE parsing and request building.
//!
//! The parsers and the request builders are public, so a bench exercises the
//! real code without a network call. The warm request benches reuse the body
//! cache of a previous call, as a provider does for a second turn.
//!
//! Run with `cargo bench -p reagent-ai --features testing --bench providers`,
//! or with `-- --test` to run each bench once.

use std::hint::black_box;

use bytes::Bytes;
use divan::Bencher;

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
use reagent_ai::sse::SseSplitter;
use reagent_ai::{
    AssistantMessage, Content, Message, ReasoningField, StopReason, ToolCall, ToolSpec,
};

fn main() {
    divan::main();
}

/// Text deltas in one parse bench.
const DELTAS: usize = 10_000;

/// Messages in one request-build bench.
const MESSAGES: usize = 200;

const MODEL_ANTHROPIC: &str = "claude-sonnet-4-5";
const MODEL_OPENAI: &str = "gpt-5";
const SYSTEM: &str = "You are a careful coding agent.";
const REASONING: &str = "medium";

/// Splits `body` into one `Bytes` chunk per line, as a socket would.
fn chunks(body: &str) -> Vec<Bytes> {
    body.split_inclusive('\n')
        .map(|line| Bytes::copy_from_slice(line.as_bytes()))
        .collect()
}

fn anthropic_stream() -> Vec<Bytes> {
    chunks(
        &"event: content_block_delta\n\
          data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n"
            .repeat(DELTAS),
    )
}

fn responses_stream() -> Vec<Bytes> {
    chunks(
        &"data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m\",\"output_index\":0,\"delta\":\"x\"}\n\n"
            .repeat(DELTAS),
    )
}

fn chat_stream() -> Vec<Bytes> {
    chunks(
        &"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n"
            .repeat(DELTAS),
    )
}

/// A 200-message history with text, tool calls and tool results.
fn history() -> Vec<Message> {
    let mut messages = Vec::with_capacity(MESSAGES);
    while messages.len() < MESSAGES {
        let index = messages.len();
        messages.push(Message::user(format!("explain step {index}")));
        if messages.len() >= MESSAGES {
            break;
        }
        let mut assistant = AssistantMessage::new(
            "anthropic",
            MODEL_ANTHROPIC,
            vec![
                Content::text(format!("step {index} reads a file")),
                Content::ToolCall(ToolCall {
                    id: format!("call_{index}").into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a.rs\"}".into(),
                }),
            ],
        );
        assistant.stop_reason = Some(StopReason::ToolUse);
        messages.push(Message::assistant(assistant));
        if messages.len() >= MESSAGES {
            break;
        }
        messages.push(Message::tool_result(
            format!("call_{index}"),
            "fn main() {}",
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

/// Feeds every chunk through `splitter` and `push`, counting the events.
fn feed<P>(chunks: &[Bytes], mut push: P) -> usize
where
    P: FnMut(&[u8], &mut Vec<reagent_ai::StreamEvent>) -> Result<(), ()>,
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

#[divan::bench]
fn parse_anthropic(bencher: Bencher) {
    let chunks = anthropic_stream();
    bencher.bench_local(|| {
        let mut parser = AnthropicParser::new();
        black_box(feed(&chunks, |line, out| {
            parser.push_line(line, out).map_err(|_| ())
        }))
    });
}

#[divan::bench]
fn parse_responses(bencher: Bencher) {
    let chunks = responses_stream();
    bencher.bench_local(|| {
        let mut parser = ResponsesParser::new();
        black_box(feed(&chunks, |line, out| {
            parser.push_line(line, out).map_err(|_| ())
        }))
    });
}

#[divan::bench]
fn parse_chat(bencher: Bencher) {
    let chunks = chat_stream();
    bencher.bench_local(|| {
        let mut parser = ChatParser::new();
        black_box(feed(&chunks, |line, out| {
            parser.push_line(line, out).map_err(|_| ())
        }))
    });
}

#[divan::bench]
fn request_body_anthropic_cold(bencher: Bencher) {
    let messages = history();
    let tools = tools();
    bencher.bench_local(|| {
        let mut bodies = AnthropicBodies::default();
        bodies
            .align(&messages, "anthropic")
            .expect("the history serializes");
        let builder =
            AnthropicBuilder::new(MODEL_ANTHROPIC, 1024, Some(SYSTEM), Some(REASONING), &tools);
        black_box(builder.body_parts(bodies.bodies()).len())
    });
}

#[divan::bench]
fn request_body_anthropic_warm(bencher: Bencher) {
    let messages = history();
    let tools = tools();
    let mut bodies = AnthropicBodies::default();
    bodies
        .align(&messages, "anthropic")
        .expect("the history serializes");
    let builder =
        AnthropicBuilder::new(MODEL_ANTHROPIC, 1024, Some(SYSTEM), Some(REASONING), &tools);
    bencher.bench_local(|| {
        bodies
            .align(&messages, "anthropic")
            .expect("the history serializes");
        black_box(builder.body_parts(bodies.bodies()).len())
    });
}

#[divan::bench]
fn request_body_responses_cold(bencher: Bencher) {
    let messages = history();
    let tools = tools();
    bencher.bench_local(|| {
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
        black_box(builder.body_parts(bodies.bodies()).len())
    });
}

#[divan::bench]
fn request_body_responses_warm(bencher: Bencher) {
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
    bencher.bench_local(|| {
        bodies
            .align(&messages, |message| {
                ResponsesMessage::new(message, "openai")
            })
            .expect("the history serializes");
        black_box(builder.body_parts(bodies.bodies()).len())
    });
}

#[divan::bench]
fn request_body_chat_cold(bencher: Bencher) {
    let messages = history();
    let tools = tools();
    bencher.bench_local(|| {
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
            reagent_ai::MaxTokensField::MaxTokens,
            ReasoningField::Effort,
        );
        black_box(builder.body_parts(bodies.bodies()).len())
    });
}

#[divan::bench]
fn request_body_chat_warm(bencher: Bencher) {
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
        reagent_ai::MaxTokensField::MaxTokens,
        ReasoningField::Effort,
    );
    bencher.bench_local(|| {
        bodies
            .align(&messages, |message| {
                ChatMessage::new(message, "openai-chat")
            })
            .expect("the history serializes");
        black_box(builder.body_parts(bodies.bodies()).len())
    });
}
