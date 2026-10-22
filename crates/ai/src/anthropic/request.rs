//! Builds Anthropic Messages request bodies.

use bytes::{BufMut, Bytes, BytesMut};
use serde::Serialize;
use serde_json::value::RawValue;

use crate::Error;
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

    fn write_assistant(&self, out: &mut BytesMut) {
        out.put_slice(b"{\"role\":\"assistant\",\"content\":[");
        for (index, block) in self.blocks.iter().enumerate() {
            if index > 0 {
                out.put_u8(b',');
            }
            out.put_slice(block);
        }
        out.put_slice(b"]}");
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
        let visible: Vec<&MessageBody> = messages
            .iter()
            .filter(|message| !(message.role == Role::Assistant && message.blocks.is_empty()))
            .collect();
        let last_user = visible
            .iter()
            .rposition(|message| matches!(message.role, Role::User | Role::ToolResult));

        let mut out = BytesMut::with_capacity(self.head.len() + 128 * visible.len());
        out.put_slice(&self.head);
        let mut first = true;
        let mut index = 0;
        while index < visible.len() {
            if !first {
                out.put_u8(b',');
            }
            first = false;
            if visible[index].role == Role::Assistant {
                visible[index].write_assistant(&mut out);
                index += 1;
            } else {
                let start = index;
                while index < visible.len() && visible[index].role != Role::Assistant {
                    index += 1;
                }
                let cached = last_user.filter(|last| *last >= start && *last < index);
                write_user(&visible[start..index], start, cached, &mut out);
            }
        }
        out.put_slice(b"]}");
        out.freeze()
    }
}

fn write_user(entries: &[&MessageBody], start: usize, cached: Option<usize>, out: &mut BytesMut) {
    out.put_slice(b"{\"role\":\"user\",\"content\":[");
    let mut first = true;
    for (offset, entry) in entries.iter().enumerate() {
        let cache_here = cached == Some(start + offset);
        for (block_index, block) in entry.blocks.iter().enumerate() {
            if !first {
                out.put_u8(b',');
            }
            first = false;
            if cache_here && block_index + 1 == entry.blocks.len() {
                out.put_slice(&block[..block.len() - 1]);
                out.put_slice(b",\"cache_control\":{\"type\":\"ephemeral\"}}");
            } else {
                out.put_slice(block);
            }
        }
    }
    out.put_slice(b"]}");
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
