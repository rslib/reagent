//! Prompt-cache stability for the three wires.
//!
//! A provider sends the whole history on every turn, so the bytes of a message
//! already sent must come out the same the next time: any rewrite before the
//! newest message is a cache miss on the whole prefix. The head (model, system
//! prompt, tools) is serialized once in a `RequestBuilder` and each message
//! once in a `BodyCache`, which is what the real request path uses, so these
//! tests build the bodies the same way.
//!
//! The test appends a tool turn (a reply with a tool call, then its result) to
//! a history that already had one, and checks that the new body starts with the
//! old body up to the end of its last message. The one allowed difference is on
//! the Anthropic wire, where the cache breakpoint of the last user-side block
//! moves to the newest one (see `RequestBuilder::body`): stripping the marker
//! from both bodies must leave the earlier bytes the same.

use bytes::Bytes;

use reagent_ai::anthropic::request::{
    BodyCache as AnthropicBodies, RequestBuilder as AnthropicBuilder,
};
use reagent_ai::openai::BodyCache;
use reagent_ai::openai::chat::{MessageBody as ChatMessage, RequestBuilder as ChatBuilder};
use reagent_ai::openai::responses::request::{
    MessageBody as ResponsesMessage, RequestBuilder as ResponsesBuilder,
};
use reagent_ai::{
    AssistantMessage, Compat, Content, Message, StopReason, Text, ToolCall, ToolSpec,
};

const MODEL: &str = "m";
const SYSTEM: &str = "You are a careful coding agent.";
const REASONING: &str = "medium";
const MAX_TOKENS: u32 = 1024;

/// The tail of every body: the bracket that closes the message list.
const END: &[u8] = b"]}";

/// The Anthropic cache breakpoint marker, which moves to the newest user-side
/// block on every request.
const MARKER: &str = r#","cache_control":{"type":"ephemeral"}"#;

fn tools() -> Vec<ToolSpec> {
    ["write", "read"]
        .iter()
        .map(|name| ToolSpec {
            name: (*name).to_string(),
            description: format!("The {name} tool."),
            input_schema: serde_json::json!({"type": "object"}),
        })
        .collect()
}

/// The history before the turn the test appends: a prompt, a reply with a tool
/// call, and its result.
fn before(provider: &str) -> Vec<Message> {
    vec![
        Message::user("hello"),
        reply(provider, "call_1"),
        Message::tool_result("call_1", "fn main() {}", false),
    ]
}

/// The turn the test appends: a reply with a tool call, and its result.
fn after(provider: &str) -> Vec<Message> {
    vec![
        reply(provider, "call_2"),
        Message::tool_result("call_2", "done", false),
    ]
}

fn reply(provider: &str, id: &str) -> Message {
    let mut assistant = AssistantMessage::new(
        provider,
        MODEL,
        vec![
            Content::text("looking"),
            Content::ToolCall(ToolCall {
                id: Text::from(id),
                name: Text::from("read"),
                arguments: Text::from(r#"{"path":"src/main.rs"}"#),
            }),
        ],
    );
    assistant.stop_reason = Some(StopReason::ToolUse);
    Message::assistant(assistant)
}

/// The body without the bracket that closes the message list: the head and the
/// messages already in the history.
fn sent(body: &Bytes) -> &[u8] {
    assert!(body.ends_with(END), "the body closes its message list");
    &body[..body.len() - END.len()]
}

/// `bytes` with every `marker` removed, as text because a body is JSON.
fn stripped(bytes: &[u8], marker: &str) -> String {
    std::str::from_utf8(bytes)
        .expect("a body is UTF-8")
        .replace(marker, "")
}

/// Checks the shape every wire must keep: the head is the same bytes in both
/// bodies, and the new body holds the earlier messages untouched.
fn assert_prefix_kept(head: &Bytes, first: &Bytes, second: &Bytes, provider: &str) {
    let head_text = std::str::from_utf8(head).expect("a head is UTF-8");
    for name in ["read", "write"] {
        assert!(
            head_text.contains(name),
            "{provider}: the head holds the {name} tool"
        );
    }
    let earlier = std::str::from_utf8(sent(first)).expect("a body is UTF-8");
    assert!(
        earlier.contains(SYSTEM),
        "{provider}: the system prompt comes before every message"
    );
    assert_eq!(
        &second[..head.len()],
        &head[..],
        "{provider}: the head changed between requests"
    );
    assert!(
        sent(second).starts_with(earlier.as_bytes()),
        "{provider}: the messages already sent were rewritten"
    );
    assert_eq!(
        second[earlier.len()],
        b',',
        "{provider}: the new message follows"
    );
}

#[test]
fn prefix_stable_chat_keeps_the_bytes_of_sent_messages() {
    let builder = ChatBuilder::with_compat(
        MODEL,
        MAX_TOKENS,
        Some(SYSTEM),
        Some(REASONING),
        &tools(),
        Compat::DEFAULT,
    );
    let mut bodies = BodyCache::default();
    let mut history = before("openai-chat");
    let first = chat_body(&builder, &mut bodies, &history);
    history.extend(after("openai-chat"));
    let second = chat_body(&builder, &mut bodies, &history);

    assert_prefix_kept(builder.head(), &first, &second, "chat");
}

#[test]
fn prefix_stable_responses_keeps_the_bytes_of_sent_messages() {
    let builder = ResponsesBuilder::with_compat(
        MODEL,
        MAX_TOKENS,
        Some(SYSTEM),
        Some(REASONING),
        Some("session-1"),
        &tools(),
        Compat::DEFAULT,
    );
    let mut bodies = BodyCache::default();
    let mut history = before("openai");
    let first = responses_body(&builder, &mut bodies, &history);
    history.extend(after("openai"));
    let second = responses_body(&builder, &mut bodies, &history);

    assert_prefix_kept(builder.head(), &first, &second, "responses");
}

#[test]
fn prefix_stable_anthropic_moves_only_the_cache_marker() {
    let builder = AnthropicBuilder::new(MODEL, MAX_TOKENS, Some(SYSTEM), Some(REASONING), &tools());
    let mut bodies = AnthropicBodies::default();
    let mut history = before("anthropic");
    let first = anthropic_body(&builder, &mut bodies, &history);
    history.extend(after("anthropic"));
    let second = anthropic_body(&builder, &mut bodies, &history);

    let head = builder.head();
    let text = std::str::from_utf8(head).expect("a head is UTF-8");
    assert!(text.contains(SYSTEM), "the head holds the system prompt");
    for name in ["read", "write"] {
        assert!(text.contains(name), "the head holds the {name} tool");
    }
    assert_eq!(
        &second[..text.len()],
        text.as_bytes(),
        "the head changed between requests"
    );

    // The breakpoint marker is the only difference before the new messages: the
    // old block loses it and the newest user-side block carries it.
    let earlier = sent(&first);
    let marked = |bytes: &[u8]| (bytes.len() - stripped(bytes, MARKER).len()) / MARKER.len();
    assert_eq!(
        marked(earlier),
        2,
        "the last tool and the last user-side block carry a breakpoint"
    );
    assert_eq!(
        marked(&second),
        2,
        "the breakpoint moved to the new user-side block"
    );
    assert_eq!(
        marked(&second[..earlier.len()]),
        1,
        "the old user-side block lost its breakpoint"
    );
    assert!(
        !second.starts_with(earlier),
        "the cache breakpoint moved to the new tool result"
    );
    let old_stripped = stripped(earlier, MARKER);
    let new_stripped = stripped(&second, MARKER);
    assert!(
        new_stripped.starts_with(&old_stripped),
        "only the cache marker may move before the new messages"
    );
}

fn chat_body(
    builder: &ChatBuilder,
    bodies: &mut BodyCache<ChatMessage>,
    history: &[Message],
) -> Bytes {
    bodies
        .align(history, |message| ChatMessage::new(message, "openai-chat"))
        .expect("the chat bodies serialize");
    builder.body(bodies.bodies())
}

fn responses_body(
    builder: &ResponsesBuilder,
    bodies: &mut BodyCache<ResponsesMessage>,
    history: &[Message],
) -> Bytes {
    bodies
        .align(history, |message| ResponsesMessage::new(message, "openai"))
        .expect("the responses bodies serialize");
    builder.body(bodies.bodies())
}

fn anthropic_body(
    builder: &AnthropicBuilder,
    bodies: &mut AnthropicBodies,
    history: &[Message],
) -> Bytes {
    bodies
        .align(history, "anthropic")
        .expect("the anthropic bodies serialize");
    builder.body(bodies.bodies())
}
