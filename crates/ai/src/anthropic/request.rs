//! Builds Anthropic Messages request bodies.

use bytes::{BufMut, Bytes, BytesMut};
use serde::Serialize;
use serde_json::value::RawValue;

use crate::Error;
use crate::openai::BodyParts;
use crate::types::{AssistantMessage, Content, Message, Reasoning, Role, ToolResult, ToolSpec};

/// One message serialized in Anthropic wire form, reusable across requests.
#[derive(Debug, Clone)]
pub struct MessageBody {
    role: Role,
    blocks: Vec<Bytes>,
}

impl MessageBody {
    /// Serializes one message for the provider named `provider`.
    ///
    /// Thinking blocks from another provider are dropped, because their
    /// signature is not valid here. An empty argument string for a tool call
    /// becomes an empty object; malformed arguments are an error.
    pub fn new(message: &Message, provider: &str) -> Result<Self, Error> {
        let (role, blocks) = match message {
            Message::User { content } => (
                Role::User,
                content
                    .iter()
                    .map(serialize_content)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Message::Assistant(assistant) => {
                (Role::Assistant, serialize_assistant(assistant, provider)?)
            }
            Message::ToolResult(result) => (Role::ToolResult, vec![serialize_tool_result(result)]),
        };
        Ok(Self { role, blocks })
    }

    /// The role this message was built from.
    pub fn role(&self) -> Role {
        self.role
    }
}

/// Caches serialized message bodies so a request only writes new messages.
///
/// Each cached body is keyed on the message it was built from, so a compacted
/// history, an edited message or a second session on the same provider cannot
/// reuse stale bytes.
#[derive(Debug, Default)]
pub struct BodyCache {
    messages: Vec<Message>,
    bodies: Vec<MessageBody>,
}

impl BodyCache {
    /// Rebuilds the bodies whose message changed and reuses the rest.
    pub fn align(&mut self, messages: &[Message], provider: &str) -> Result<(), Error> {
        self.messages.truncate(messages.len());
        self.bodies.truncate(messages.len());
        for (index, message) in messages.iter().enumerate() {
            if self.messages.get(index) == Some(message) {
                continue;
            }
            self.messages.truncate(index);
            self.bodies.truncate(index);
            self.bodies.push(MessageBody::new(message, provider)?);
            self.messages.push(message.clone());
        }
        Ok(())
    }

    /// The bodies in request order.
    pub fn bodies(&self) -> &[MessageBody] {
        &self.bodies
    }
}

/// Builds an Anthropic Messages body from a stable head and reusable messages.
///
/// The head holds the model, the system prompt, the reasoning and the tools,
/// serialized once. Tools are sorted by name and the last one carries the
/// tools cache breakpoint.
#[derive(Debug)]
pub struct RequestBuilder {
    head: Bytes,
}

impl RequestBuilder {
    /// Serializes the request head once.
    pub fn new(
        model: &str,
        max_tokens: u32,
        system: Option<&str>,
        reasoning: Option<&str>,
        tools: &[ToolSpec],
    ) -> Self {
        let mut out = BytesMut::new();
        out.put_slice(b"{\"model\":");
        out.put_slice(json_string(model).as_bytes());
        // A level keeps max_tokens as given. A budget is extra, because the
        // API counts thinking inside max_tokens and needs budget_tokens < max_tokens.
        let reasoning = reasoning
            .map(Reasoning::parse)
            .filter(|reasoning| !reasoning.is_off());
        let budget = match reasoning {
            Some(Reasoning::Budget(budget)) => budget,
            _ => 0,
        };
        out.put_slice(b",\"max_tokens\":");
        out.put_slice(max_tokens.saturating_add(budget).to_string().as_bytes());
        if let Some(system) = system {
            out.put_slice(b",\"system\":");
            out.put_slice(json_string(system).as_bytes());
        }
        out.put_slice(b",\"stream\":true");
        match reasoning {
            Some(Reasoning::Level(level)) => {
                out.put_slice(
                    b",\"thinking\":{\"type\":\"adaptive\"},\"output_config\":{\"effort\":",
                );
                out.put_slice(json_string(level).as_bytes());
                out.put_slice(b"}");
            }
            Some(Reasoning::Budget(budget)) => {
                out.put_slice(b",\"thinking\":{\"type\":\"enabled\",\"budget_tokens\":");
                out.put_slice(budget.to_string().as_bytes());
                out.put_slice(b"}");
            }
            None => {}
        }
        out.put_slice(b",\"tools\":[");

        let mut sorted: Vec<&ToolSpec> = tools.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        let last_tool = sorted.len().saturating_sub(1);
        for (index, tool) in sorted.iter().enumerate() {
            if index > 0 {
                out.put_u8(b',');
            }
            out.put_slice(&serialize_tool(tool, index == last_tool));
        }
        out.put_slice(b"],\"messages\":[");

        Self { head: out.freeze() }
    }

    /// The prefix every body built from this request shares.
    pub fn head(&self) -> &Bytes {
        &self.head
    }

    /// Serializes a body from the head and the message bodies.
    ///
    /// An assistant message whose blocks were all dropped disappears, and the
    /// user-side messages around it merge into one, because Anthropic requires
    /// alternating roles. The last user-side block carries the cache breakpoint.
    pub fn body(&self, messages: &[MessageBody]) -> Bytes {
        self.body_parts(messages).join()
    }

    /// The body as its pieces: the head, the separators and every cached
    /// message's bytes, none of them copied.
    ///
    /// The block that carries the cache breakpoint is three pieces: the bytes
    /// before its closing brace, the marker, and that brace, which is the last
    /// byte of the cached block. [`Bytes::slice`] shares them with the cache,
    /// so the block reaches the wire without a copy.
    pub fn body_parts(&self, messages: &[MessageBody]) -> BodyParts {
        let mut visible: Vec<&MessageBody> = Vec::with_capacity(messages.len());
        visible.extend(
            messages
                .iter()
                .filter(|message| !(message.role == Role::Assistant && message.blocks.is_empty())),
        );
        let last_user = visible
            .iter()
            .rposition(|message| matches!(message.role, Role::User | Role::ToolResult));

        let mut parts = BodyParts::with_capacity(self.pieces(&visible, last_user));
        parts.push(self.head.clone());
        let mut first = true;
        let mut index = 0;
        while index < visible.len() {
            if !first {
                parts.push_static(COMMA);
            }
            first = false;
            if visible[index].role == Role::Assistant {
                parts.push_static(ASSISTANT_PREFIX);
                for (block_index, block) in visible[index].blocks.iter().enumerate() {
                    if block_index > 0 {
                        parts.push_static(COMMA);
                    }
                    parts.push(block.clone());
                }
                parts.push_static(END);
                index += 1;
            } else {
                let start = index;
                while index < visible.len() && visible[index].role != Role::Assistant {
                    index += 1;
                }
                let cached = last_user.filter(|last| *last >= start && *last < index);
                push_user(&visible[start..index], start, cached, &mut parts);
            }
        }
        parts.push_static(END);
        debug_assert_eq!(
            parts.len(),
            self.body_len(&visible, last_user),
            "the pieces hold the body the buffer held"
        );
        parts
    }

    /// How many pieces [`RequestBuilder::body_parts`] appends, so the list of
    /// them is one allocation whatever the history holds.
    fn pieces(&self, visible: &[&MessageBody], last_user: Option<usize>) -> usize {
        // The head and the bracket that closes the message list.
        let mut count = 2;
        let mut first = true;
        let mut index = 0;
        while index < visible.len() {
            // The prefix, and the comma before it on every group but the first.
            count += 1 + usize::from(!first);
            first = false;
            if visible[index].role == Role::Assistant {
                count += group_pieces(visible[index].blocks.len());
                index += 1;
            } else {
                let start = index;
                while index < visible.len() && visible[index].role != Role::Assistant {
                    index += 1;
                }
                let blocks: usize = visible[start..index]
                    .iter()
                    .map(|entry| entry.blocks.len())
                    .sum();
                count += group_pieces(blocks);
                if last_user.is_some_and(|last| last >= start && last < index) {
                    // The marker, between the two halves of the cached block.
                    count += 2;
                }
            }
        }
        count
    }

    /// The length of the body [`RequestBuilder::body_parts`] appends, so the
    /// pieces hold the bytes the buffer held, which a debug build checks.
    fn body_len(&self, visible: &[&MessageBody], last_user: Option<usize>) -> usize {
        let mut len = self.head.len() + END.len();
        let mut first = true;
        let mut index = 0;
        while index < visible.len() {
            if !first {
                len += 1;
            }
            first = false;
            if visible[index].role == Role::Assistant {
                len += block_len(&visible[index].blocks) + ASSISTANT_PREFIX.len();
                index += 1;
            } else {
                let start = index;
                while index < visible.len() && visible[index].role != Role::Assistant {
                    index += 1;
                }
                len += block_len(visible[start..index].iter().flat_map(|group| &group.blocks))
                    + USER_PREFIX.len();
                if last_user.is_some_and(|last| last >= start && last < index) {
                    // The block keeps its closing brace, and the marker comes
                    // before it, where the brace used to close the block.
                    len += CACHE_CONTROL.len();
                }
            }
        }
        len
    }
}

const COMMA: &[u8] = b",";
const ASSISTANT_PREFIX: &[u8] = b"{\"role\":\"assistant\",\"content\":[";
const USER_PREFIX: &[u8] = b"{\"role\":\"user\",\"content\":[";
const END: &[u8] = b"]}";
/// The cache breakpoint marker, without the brace that closes the block it
/// sits in: the block's own last byte closes it, so the block crosses the wire
/// as the bytes before its brace, this marker, and the brace.
const CACHE_CONTROL: &[u8] = b",\"cache_control\":{\"type\":\"ephemeral\"}";

/// The length of `blocks`, written one after another with a comma between
/// them, plus the bracket that closes them.
fn block_len<'a>(blocks: impl IntoIterator<Item = &'a Bytes>) -> usize {
    let mut len = END.len();
    let mut count: usize = 0;
    for block in blocks {
        len += block.len();
        count += 1;
    }
    len + count.saturating_sub(1)
}

/// The pieces one message group writes: one per block, the comma between them
/// and the bracket that closes them.
fn group_pieces(blocks: usize) -> usize {
    if blocks == 0 { 1 } else { 2 * blocks }
}

fn push_user(entries: &[&MessageBody], start: usize, cached: Option<usize>, parts: &mut BodyParts) {
    parts.push_static(USER_PREFIX);
    let mut first = true;
    for (offset, entry) in entries.iter().enumerate() {
        let cache_here = cached == Some(start + offset);
        for (block_index, block) in entry.blocks.iter().enumerate() {
            if !first {
                parts.push_static(COMMA);
            }
            first = false;
            if cache_here && block_index + 1 == entry.blocks.len() {
                let at = block.len() - 1;
                parts.push(block.slice(..at));
                parts.push_static(CACHE_CONTROL);
                parts.push(block.slice(at..));
            } else {
                parts.push(block.clone());
            }
        }
    }
    parts.push_static(END);
}

#[derive(Clone, Copy, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

const EPHEMERAL: CacheControl = CacheControl { kind: "ephemeral" };

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block<'a> {
    Text {
        text: &'a str,
    },
    Thinking {
        thinking: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<&'a str>,
    },
    RedactedThinking {
        data: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a RawValue,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Serialize)]
struct Tool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

fn serialize_content(content: &Content) -> Result<Bytes, Error> {
    let block = match content {
        Content::Text { text } => Block::Text {
            text: text.as_ref(),
        },
        Content::Thinking {
            thinking,
            signature,
        } => Block::Thinking {
            thinking: thinking.as_ref(),
            signature: signature.as_deref(),
        },
        Content::RedactedThinking { data } => Block::RedactedThinking {
            data: data.as_ref(),
        },
        Content::ToolCall(call) => {
            let input = if call.arguments.trim().is_empty() {
                RawValue::from_string("{}".into()).expect("a JSON literal is valid")
            } else {
                serde_json::from_str::<Box<RawValue>>(&call.arguments).map_err(|error| {
                    Error::Protocol(format!("invalid arguments for tool {}: {error}", call.name))
                })?
            };
            return Ok(to_bytes(&Block::ToolUse {
                id: &call.id,
                name: &call.name,
                input: &input,
            }));
        }
    };
    Ok(to_bytes(&block))
}

fn serialize_assistant(assistant: &AssistantMessage, provider: &str) -> Result<Vec<Bytes>, Error> {
    let keep_thinking = assistant.provider.as_ref() == provider;
    assistant
        .content
        .iter()
        .filter(|content| keep_thinking || !content.is_thinking())
        .map(serialize_content)
        .collect()
}

fn serialize_tool_result(result: &ToolResult) -> Bytes {
    to_bytes(&Block::ToolResult {
        tool_use_id: &result.call_id,
        content: result.content.as_ref(),
        is_error: result.is_error.then_some(true),
    })
}

fn serialize_tool(tool: &ToolSpec, cache_control: bool) -> Bytes {
    to_bytes(&Tool {
        name: &tool.name,
        description: &tool.description,
        input_schema: &tool.input_schema,
        cache_control: cache_control.then_some(EPHEMERAL),
    })
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a str serializes to a JSON string")
}

fn to_bytes(value: &impl Serialize) -> Bytes {
    Bytes::from(serde_json::to_vec(value).expect("request parts are serializable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::Anthropic;
    use crate::types::{AssistantMessage, Content, Message, StopReason, ToolCall};
    use crate::{Provider, Request, Text};

    const PROVIDER: &str = "anthropic";

    fn body_of(messages: &[Message]) -> Bytes {
        let bodies: Vec<MessageBody> = messages
            .iter()
            .map(|message| MessageBody::new(message, PROVIDER).unwrap())
            .collect();
        golden_builder().body(&bodies)
    }

    fn golden_builder() -> RequestBuilder {
        let tools = vec![
            ToolSpec {
                name: "write".into(),
                description: "Write a file.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            ToolSpec {
                name: "read".into(),
                description: "Read a file.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        ];
        RequestBuilder::new(
            "claude-sonnet-4-5",
            1024,
            Some("You are a careful coding agent."),
            Some("medium"),
            &tools,
        )
    }

    fn golden_assistant() -> Message {
        let mut assistant = AssistantMessage::new(
            PROVIDER,
            "claude-sonnet-4-5",
            vec![
                Content::text("hi"),
                Content::ToolCall(ToolCall {
                    id: "toolu_01".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a.rs\"}".into(),
                }),
            ],
        );
        assistant.stop_reason = Some(StopReason::ToolUse);
        Message::assistant(assistant)
    }

    fn golden_messages() -> Vec<MessageBody> {
        [
            Message::user("hello"),
            golden_assistant(),
            Message::tool_result("toolu_01", "fn main() {}", false),
            Message::user("explain"),
        ]
        .iter()
        .map(|message| MessageBody::new(message, PROVIDER).unwrap())
        .collect()
    }

    fn request<'a>(model: &'a str, messages: &'a [Message]) -> Request<'a> {
        Request {
            model,
            max_tokens: 128,
            system: None,
            tools: &[],
            messages,
            reasoning: None,
            cache_key: None,
        }
    }

    #[test]
    fn head_enables_streaming_and_maps_reasoning() {
        let tools: Vec<ToolSpec> = Vec::new();
        let low = RequestBuilder::new("m", 1024, None, Some("low"), &tools);
        let head = std::str::from_utf8(low.head()).unwrap();
        assert!(head.contains(r#""stream":true"#));
        assert!(head.contains(r#""max_tokens":1024"#));
        assert!(head.contains(r#""thinking":{"type":"adaptive"}"#));

        let none = RequestBuilder::new("m", 1024, None, None, &tools);
        assert!(
            !std::str::from_utf8(none.head())
                .unwrap()
                .contains("thinking")
        );
    }

    #[test]
    fn a_level_reasoning_text_writes_adaptive_thinking_and_an_effort() {
        let tools: Vec<ToolSpec> = Vec::new();
        let builder = RequestBuilder::new("m", 1000, None, Some("max"), &tools);
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(
            head.contains(r#""thinking":{"type":"adaptive"},"output_config":{"effort":"max"}"#),
            "{head}"
        );
        assert!(head.contains(r#""max_tokens":1000"#), "{head}");
        assert!(!head.contains("budget_tokens"), "{head}");
    }

    #[test]
    fn a_budget_reasoning_text_adds_to_max_tokens() {
        let tools: Vec<ToolSpec> = Vec::new();
        let builder = RequestBuilder::new("m", 1000, None, Some("4096"), &tools);
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(
            head.contains(r#""thinking":{"type":"enabled","budget_tokens":4096}"#),
            "{head}"
        );
        assert!(head.contains(r#""max_tokens":5096"#), "{head}");
        assert!(!head.contains("output_config"), "{head}");
    }

    #[test]
    fn an_off_reasoning_text_writes_no_thinking() {
        let tools: Vec<ToolSpec> = Vec::new();
        for text in ["off", "none"] {
            let builder = RequestBuilder::new("m", 1000, None, Some(text), &tools);
            let head = std::str::from_utf8(builder.head()).unwrap();
            assert!(!head.contains("thinking"), "{text}: {head}");
            assert!(!head.contains("output_config"), "{text}: {head}");
            assert!(head.contains(r#""max_tokens":1000"#), "{text}: {head}");
        }
    }

    #[test]
    fn a_quoted_reasoning_text_is_escaped() {
        let tools: Vec<ToolSpec> = Vec::new();
        let builder = RequestBuilder::new("m", 1000, None, Some(r#"a"b"#), &tools);
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(head.contains(r#""effort":"a\"b""#), "{head}");
    }

    #[test]
    fn provider_name_is_anthropic() {
        assert_eq!(
            Anthropic::new("key", "http://localhost").name(),
            "anthropic"
        );
    }

    #[test]
    fn body_matches_the_golden_request() {
        let body = golden_builder().body(&golden_messages());
        let golden = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/anthropic_request.json"
        ));
        assert_eq!(std::str::from_utf8(&body).unwrap(), golden.trim_end());
    }

    /// A history with every shape a session holds: text, a reply with thinking
    /// and a tool call, that tool's result, and a text-only reply.
    fn mixed_messages() -> Vec<MessageBody> {
        let mut assistant = AssistantMessage::new(
            PROVIDER,
            "claude-sonnet-4-5",
            vec![
                Content::Thinking {
                    thinking: "consider the file".into(),
                    signature: Some("sig_1".into()),
                },
                Content::text("reading"),
                Content::ToolCall(ToolCall {
                    id: "toolu_02".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"b.rs\"}".into(),
                }),
            ],
        );
        assistant.stop_reason = Some(StopReason::ToolUse);
        [
            Message::user("hello"),
            Message::assistant(assistant),
            Message::tool_result("toolu_02", "fn b() {}", false),
            Message::assistant(AssistantMessage::new(
                PROVIDER,
                "claude-sonnet-4-5",
                vec![Content::text("here it is")],
            )),
        ]
        .iter()
        .map(|message| MessageBody::new(message, PROVIDER).unwrap())
        .collect()
    }

    /// The pieces joined are the bytes the old single buffer wrote, and their
    /// length is the length of that body.
    #[test]
    fn body_pieces_join_to_the_single_buffer_body() {
        let builder = golden_builder();
        let bodies = mixed_messages();
        let parts = builder.body_parts(&bodies);
        let joined = parts.join();
        assert_eq!(
            parts.len(),
            joined.len(),
            "the length of the body is the sum of its pieces"
        );
        assert_eq!(
            joined,
            builder.body(&bodies),
            "the joined pieces are the body"
        );
        assert!(
            serde_json::from_slice::<serde_json::Value>(&joined).is_ok(),
            "the body of a mixed history is one JSON object"
        );

        let golden = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/anthropic_request.json"
        ));
        let pieces = builder.body_parts(&golden_messages()).join();
        assert_eq!(
            std::str::from_utf8(&pieces).unwrap(),
            golden.trim_end(),
            "the pieces write the bytes the old buffer wrote"
        );
    }

    /// Whether `piece` points into `block`, so the two hold one copy of the
    /// bytes and no copy was made.
    fn shares(piece: &Bytes, block: &Bytes) -> bool {
        let start = block.as_ptr() as usize;
        let at = piece.as_ptr() as usize;
        at >= start && at + piece.len() <= start + block.len()
    }

    /// Every cached block reaches the body as pieces that point into it, so the
    /// body holds no second copy of any message.
    #[test]
    fn body_pieces_share_the_cached_message_bytes() {
        let builder = golden_builder();
        let bodies = mixed_messages();
        let parts = builder.body_parts(&bodies);
        let mut split = 0;
        for block in bodies.iter().flat_map(|body| &body.blocks) {
            let pieces: Vec<&Bytes> = parts
                .parts()
                .iter()
                .filter(|piece| shares(piece, block))
                .collect();
            assert!(!pieces.is_empty(), "every cached block is in the body");
            let held: usize = pieces.iter().map(|piece| piece.len()).sum();
            assert_eq!(held, block.len(), "the pieces hold the block once");
            assert_eq!(
                pieces[0].as_ptr(),
                block.as_ptr(),
                "the first piece starts the block"
            );
            let last = pieces[pieces.len() - 1];
            assert_eq!(
                last.as_ptr() as usize + last.len(),
                block.as_ptr() as usize + block.len(),
                "the last piece ends the block"
            );
            if pieces.len() > 1 {
                split += 1;
            }
        }
        assert_eq!(
            split, 1,
            "the block that carries the breakpoint is two pieces around the marker"
        );
    }

    #[test]
    fn the_same_history_gives_byte_identical_prefixes() {
        let builder = golden_builder();
        let messages = golden_messages();
        let first = builder.body(&messages);
        let second = builder.body(&messages);
        assert_eq!(first, second);
        assert!(first.starts_with(builder.head()));
    }

    #[test]
    fn appending_a_message_keeps_the_earlier_bytes() {
        let builder = golden_builder();
        let one = vec![MessageBody::new(&Message::user("hello"), PROVIDER).unwrap()];
        let mut two = one.clone();
        two.push(
            MessageBody::new(
                &Message::assistant(AssistantMessage::new(
                    PROVIDER,
                    "claude-sonnet-4-5",
                    vec![Content::text("hi")],
                )),
                PROVIDER,
            )
            .unwrap(),
        );

        let short = builder.body(&one);
        let long = builder.body(&two);

        let stable = &short[..short.len() - 2];
        assert!(long.starts_with(stable));
    }

    #[test]
    fn tool_result_and_user_text_merge_into_one_message() {
        let body = body_of(&[
            Message::tool_result("toolu_09", "done", false),
            Message::user("now explain"),
        ]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(
            r#"[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_09","content":"done"},{"type":"text","text":"now explain","cache_control":{"type":"ephemeral"}}]}]}"#
        ));
    }

    #[test]
    fn thinking_from_another_provider_is_dropped() {
        let assistant = AssistantMessage::new(
            "openai",
            "gpt-5",
            vec![
                Content::Thinking {
                    thinking: "secret".into(),
                    signature: Some("sig".into()),
                },
                Content::RedactedThinking {
                    data: "opaque".into(),
                },
                Content::text("visible"),
            ],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("secret"));
        assert!(!body.contains("opaque"));
        assert!(body.contains("visible"));
    }

    #[test]
    fn thinking_from_the_same_provider_is_kept() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "claude-sonnet-4-5",
            vec![Content::Thinking {
                thinking: "kept".into(),
                signature: Some("sig".into()),
            }],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#""type":"thinking","thinking":"kept","signature":"sig""#));
    }

    #[test]
    fn an_assistant_with_only_foreign_thinking_disappears() {
        let foreign = AssistantMessage::new(
            "openai",
            "gpt-5",
            vec![Content::Thinking {
                thinking: "secret".into(),
                signature: None,
            }],
        );
        let body = body_of(&[
            Message::user("before"),
            Message::assistant(foreign),
            Message::user("after"),
        ]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("secret"));
        assert!(!body.contains(r#""role":"assistant""#));
        assert!(body.ends_with(
            r#""messages":[{"role":"user","content":[{"type":"text","text":"before"},{"type":"text","text":"after","cache_control":{"type":"ephemeral"}}]}]}"#
        ));
    }

    #[test]
    fn empty_tool_arguments_become_an_empty_object() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "claude-sonnet-4-5",
            vec![Content::ToolCall(ToolCall {
                id: "toolu_10".into(),
                name: "read".into(),
                arguments: Text::from(""),
            })],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#""input":{}"#));
    }

    #[test]
    fn malformed_tool_arguments_are_an_error() {
        let assistant = Message::assistant(AssistantMessage::new(
            PROVIDER,
            "claude-sonnet-4-5",
            vec![Content::ToolCall(ToolCall {
                id: "toolu_11".into(),
                name: "read".into(),
                arguments: "{not json".into(),
            })],
        ));
        let error = MessageBody::new(&assistant, PROVIDER).unwrap_err();
        assert!(matches!(error, Error::Protocol(_)));
    }

    #[test]
    fn request_body_distinguishes_equal_length_histories() {
        let provider = Anthropic::new("key", "http://localhost");
        let first_history = vec![Message::user("one")];
        let second_history = vec![Message::user("two")];
        let first = request("m", &first_history);
        let second = request("m", &second_history);
        let first = provider.request_body(&first).unwrap();
        let second = provider.request_body(&second).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn request_body_is_stable_for_the_same_history() {
        let provider = Anthropic::new("key", "http://localhost");
        let history = vec![Message::user("one")];
        let request = request("m", &history);
        let first = provider.request_body(&request).unwrap();
        let second = provider.request_body(&request).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn request_body_replaces_a_compacted_history() {
        let provider = Anthropic::new("key", "http://localhost");
        let long_history = vec![
            Message::user("one"),
            Message::user("two"),
            Message::user("three"),
        ];
        let short_history = vec![Message::user("different")];
        let long = request("m", &long_history);
        let short = request("m", &short_history);
        let _ = provider.request_body(&long).unwrap();
        let short = provider.request_body(&short).unwrap();
        let text = std::str::from_utf8(&short).unwrap();
        assert!(!text.contains("one"));
        assert!(text.contains("different"));
    }
}
