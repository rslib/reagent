//! Builds OpenAI Responses request bodies.

use bytes::{BufMut, Bytes, BytesMut};
use serde::Serialize;

use crate::Error;
use crate::providers::{Compat, MaxTokensField};
use crate::types::{AssistantMessage, Content, Message, Reasoning, ToolSpec};

/// One message serialized in Responses wire form, reusable across requests.
#[derive(Debug, Clone)]
pub struct MessageBody {
    items: Vec<Bytes>,
}

impl MessageBody {
    /// Serializes one message for the provider named `provider`.
    ///
    /// Thinking blocks from another provider are dropped. Tool arguments are
    /// passed through as the raw JSON string the model emitted; an empty
    /// argument string becomes an empty object.
    pub fn new(message: &Message, provider: &str) -> Result<Self, Error> {
        let items = match message {
            Message::User { content } => user_items(content),
            Message::Assistant(assistant) => assistant_items(assistant, provider),
            Message::ToolResult(result) => vec![to_bytes(&InputItem::FunctionCallOutput {
                call_id: &result.call_id,
                output: result.content.as_ref(),
            })],
        };
        Ok(Self { items })
    }
}

/// Builds a Responses body from a stable head and reusable message bodies.
///
/// The head holds the model, the instructions, the reasoning text, the
/// prompt cache key and the tools, serialized once. Tools are sorted by name.
#[derive(Debug)]
pub struct RequestBuilder {
    head: Bytes,
}

impl RequestBuilder {
    /// Serializes the request head once, for the plain wire.
    pub fn new(
        model: &str,
        max_tokens: u32,
        system: Option<&str>,
        reasoning: Option<&str>,
        cache_key: Option<&str>,
        tools: &[ToolSpec],
    ) -> Self {
        Self::with_compat(
            model,
            max_tokens,
            system,
            reasoning,
            cache_key,
            tools,
            Compat::DEFAULT,
        )
    }

    /// Serializes the request head once, for an endpoint described by `compat`.
    pub fn with_compat(
        model: &str,
        max_tokens: u32,
        system: Option<&str>,
        reasoning: Option<&str>,
        cache_key: Option<&str>,
        tools: &[ToolSpec],
        compat: Compat,
    ) -> Self {
        let reasoning_off = compat.reasoning_off;
        let mut out = BytesMut::new();
        out.put_slice(b"{\"model\":");
        out.put_slice(json_string(model).as_bytes());
        if let Some(system) = system {
            out.put_slice(b",\"instructions\":");
            out.put_slice(json_string(system).as_bytes());
        }
        // The cap covers reasoning too, so it is sent as given. An endpoint
        // that refuses a cap field gets none.
        if compat.max_tokens_field != MaxTokensField::Omit {
            out.put_slice(b",\"max_output_tokens\":");
            out.put_slice(max_tokens.to_string().as_bytes());
        }
        out.put_slice(b",\"stream\":true,\"store\":false");
        if let Some(reasoning) = reasoning.map(Reasoning::parse) {
            if reasoning.is_off() {
                if let Some(word) = reasoning_off {
                    out.put_slice(b",\"reasoning\":{\"effort\":");
                    out.put_slice(json_string(word).as_bytes());
                    out.put_slice(b"}");
                }
            } else {
                let effort = reasoning.text();
                out.put_slice(
                    b",\"include\":[\"reasoning.encrypted_content\"],\"reasoning\":{\"effort\":",
                );
                out.put_slice(json_string(&effort).as_bytes());
                out.put_slice(b",\"summary\":\"auto\"}");
            }
        }
        if let Some(cache_key) = cache_key {
            out.put_slice(b",\"prompt_cache_key\":");
            out.put_slice(json_string(cache_key).as_bytes());
        }
        out.put_slice(b",\"tools\":[");

        let mut sorted: Vec<&ToolSpec> = tools.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for (index, tool) in sorted.iter().enumerate() {
            if index > 0 {
                out.put_u8(b',');
            }
            out.put_slice(&to_bytes(&FunctionTool {
                kind: "function",
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.input_schema,
            }));
        }
        out.put_slice(b"],\"input\":[");

        Self { head: out.freeze() }
    }

    /// The prefix every body built from this request shares.
    pub fn head(&self) -> &Bytes {
        &self.head
    }

    /// Serializes a body from the head and the message bodies.
    ///
    /// A message whose blocks were all dropped disappears.
    pub fn body(&self, messages: &[MessageBody]) -> Bytes {
        let mut out = BytesMut::with_capacity(self.head.len() + 128 * messages.len());
        out.put_slice(&self.head);
        let mut first = true;
        for message in messages {
            for item in &message.items {
                if !first {
                    out.put_u8(b',');
                }
                first = false;
                out.put_slice(item);
            }
        }
        out.put_slice(b"]}");
        out.freeze()
    }
}

#[derive(Serialize)]
struct FunctionTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InputItem<'a> {
    Message {
        role: &'static str,
        content: Vec<Part<'a>>,
    },
    Reasoning {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<&'a str>,
        summary: Vec<&'a str>,
        encrypted_content: &'a str,
    },
    FunctionCall {
        call_id: &'a str,
        name: &'a str,
        arguments: &'a str,
    },
    FunctionCallOutput {
        call_id: &'a str,
        output: &'a str,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part<'a> {
    InputText { text: &'a str },
    OutputText { text: &'a str },
}

fn user_items(content: &[Content]) -> Vec<Bytes> {
    let parts: Vec<Part<'_>> = content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(Part::InputText {
                text: text.as_ref(),
            }),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        Vec::new()
    } else {
        vec![to_bytes(&InputItem::Message {
            role: "user",
            content: parts,
        })]
    }
}

fn assistant_items(assistant: &AssistantMessage, provider: &str) -> Vec<Bytes> {
    let keep_thinking = assistant.provider.as_ref() == provider;
    let mut items = Vec::new();
    let mut text: Vec<&str> = Vec::new();
    let mut pending_reasoning: Vec<Bytes> = Vec::new();
    for content in &assistant.content {
        match content {
            Content::Text { text: value } => text.push(value.as_ref()),
            Content::Thinking { signature, .. } => {
                flush_assistant_text(&mut items, &mut text, &mut pending_reasoning);
                if keep_thinking && let Some(signature) = signature {
                    let (id, encrypted_content) = super::split_reasoning(signature);
                    pending_reasoning.push(to_bytes(&InputItem::Reasoning {
                        id,
                        summary: Vec::new(),
                        encrypted_content,
                    }));
                }
            }
            Content::RedactedThinking { .. } => {
                flush_assistant_text(&mut items, &mut text, &mut pending_reasoning)
            }
            Content::ToolCall(call) => {
                flush_assistant_text(&mut items, &mut text, &mut pending_reasoning);
                commit_reasoning(&mut items, &mut pending_reasoning);
                let arguments = if call.arguments.trim().is_empty() {
                    "{}"
                } else {
                    call.arguments.as_ref()
                };
                items.push(to_bytes(&InputItem::FunctionCall {
                    call_id: &call.id,
                    name: &call.name,
                    arguments,
                }));
            }
        }
    }
    flush_assistant_text(&mut items, &mut text, &mut pending_reasoning);
    items
}

fn flush_assistant_text(
    items: &mut Vec<Bytes>,
    text: &mut Vec<&str>,
    pending_reasoning: &mut Vec<Bytes>,
) {
    if text.is_empty() {
        return;
    }
    commit_reasoning(items, pending_reasoning);
    let parts = text
        .drain(..)
        .map(|value| Part::OutputText { text: value })
        .collect();
    items.push(to_bytes(&InputItem::Message {
        role: "assistant",
        content: parts,
    }));
}

fn commit_reasoning(items: &mut Vec<Bytes>, pending_reasoning: &mut Vec<Bytes>) {
    items.append(pending_reasoning);
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
    use crate::Request;
    use crate::types::{StopReason, ToolCall};

    const PROVIDER: &str = "openai";

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
            "gpt-5",
            1024,
            Some("You are a careful coding agent."),
            Some("medium"),
            Some("sess-1"),
            &tools,
        )
    }

    fn golden_messages() -> Vec<MessageBody> {
        let mut assistant = AssistantMessage::new(
            PROVIDER,
            "gpt-5",
            vec![
                Content::text("hi"),
                Content::ToolCall(ToolCall {
                    id: "call_01".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a.rs\"}".into(),
                }),
            ],
        );
        assistant.stop_reason = Some(StopReason::ToolUse);
        [
            Message::user("hello"),
            Message::assistant(assistant),
            Message::tool_result("call_01", "fn main() {}", false),
            Message::user("explain"),
        ]
        .iter()
        .map(|message| MessageBody::new(message, PROVIDER).unwrap())
        .collect()
    }

    fn body_of(messages: &[Message]) -> Bytes {
        let bodies: Vec<MessageBody> = messages
            .iter()
            .map(|message| MessageBody::new(message, PROVIDER).unwrap())
            .collect();
        golden_builder().body(&bodies)
    }

    #[test]
    fn body_matches_the_golden_request() {
        let body = golden_builder().body(&golden_messages());
        let golden = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_responses_request.json"
        ));
        assert_eq!(std::str::from_utf8(&body).unwrap(), golden.trim_end());
    }

    #[test]
    fn head_carries_reasoning_and_the_prompt_cache_key() {
        let builder = golden_builder();
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(head.contains(r#""max_output_tokens":1024"#));
        assert!(head.contains(r#""store":false"#));
        assert!(head.contains(r#""include":["reasoning.encrypted_content"]"#));
        assert!(head.contains(r#""reasoning":{"effort":"medium","summary":"auto"}"#));
        assert!(head.contains(r#""prompt_cache_key":"sess-1""#));
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
                    "gpt-5",
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
    fn thinking_from_another_provider_is_dropped() {
        let assistant = AssistantMessage::new(
            "anthropic",
            "claude-sonnet-4-5",
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
        assert!(!body.contains("sig"));
        assert!(body.contains("visible"));
    }

    #[test]
    fn thinking_from_the_same_provider_round_trips_as_encrypted_content() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "gpt-5",
            vec![
                Content::Thinking {
                    thinking: "kept".into(),
                    signature: Some("enc_1".into()),
                },
                Content::text("after"),
            ],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#""type":"reasoning","summary":[],"encrypted_content":"enc_1""#));
        assert!(!body.contains("kept"));
    }

    #[test]
    fn reasoning_item_carries_the_item_id() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "gpt-5",
            vec![
                Content::Thinking {
                    thinking: "kept".into(),
                    signature: Some(crate::openai::responses::pack_reasoning(
                        Some("rs_1"),
                        "enc_1",
                    )),
                },
                Content::text("after"),
            ],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(
            r#""type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"enc_1""#
        ));
    }

    #[test]
    fn a_trailing_reasoning_item_is_dropped() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "gpt-5",
            vec![
                Content::text("hi"),
                Content::Thinking {
                    thinking: "plan".into(),
                    signature: Some(crate::openai::responses::pack_reasoning(
                        Some("rs_1"),
                        "enc_1",
                    )),
                },
            ],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("rs_1"));
        assert!(!body.contains("enc_1"));
        assert!(body.contains(r#""text":"hi""#));
    }

    #[test]
    fn an_assistant_with_only_foreign_thinking_disappears() {
        let foreign = AssistantMessage::new(
            "anthropic",
            "claude-sonnet-4-5",
            vec![Content::Thinking {
                thinking: "secret".into(),
                signature: Some("sig".into()),
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
            r#"[{"type":"message","role":"user","content":[{"type":"input_text","text":"before"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"after"}]}]}"#
        ));
    }

    #[test]
    fn empty_tool_arguments_become_an_empty_object() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "gpt-5",
            vec![Content::ToolCall(ToolCall {
                id: "call_10".into(),
                name: "read".into(),
                arguments: "".into(),
            })],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#""arguments":"{}""#));
    }

    #[test]
    fn a_level_reasoning_text_is_written_as_an_effort_with_a_summary() {
        let builder = RequestBuilder::new("m", 1024, None, Some("medium"), None, &[]);
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(
            head.contains(r#""include":["reasoning.encrypted_content"]"#),
            "{head}"
        );
        assert!(
            head.contains(r#""reasoning":{"effort":"medium","summary":"auto"}"#),
            "{head}"
        );
        assert!(
            head.contains(r#""max_output_tokens":1024"#),
            "the cap does not grow: {head}"
        );
    }

    #[test]
    fn a_budget_reasoning_text_is_written_as_the_effort_text() {
        let builder = RequestBuilder::new("m", 1024, None, Some("4096"), None, &[]);
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(
            head.contains(r#""reasoning":{"effort":"4096","summary":"auto"}"#),
            "{head}"
        );
        assert!(
            head.contains(r#""max_output_tokens":1024"#),
            "the cap does not grow: {head}"
        );
    }

    #[test]
    fn an_off_reasoning_text_writes_no_reasoning() {
        for text in ["off", "none"] {
            let builder = RequestBuilder::new("m", 128, None, Some(text), None, &[]);
            let head = std::str::from_utf8(builder.head()).unwrap();
            assert!(!head.contains("reasoning"), "{text}: {head}");
            assert!(
                head.contains(r#""max_output_tokens":128"#),
                "{text}: {head}"
            );
        }
    }

    #[test]
    fn a_request_without_reasoning_has_no_reasoning_fields() {
        let builder = RequestBuilder::new("m", 128, None, None, None, &[]);
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(head.contains(r#""max_output_tokens":128"#));
        assert!(!head.contains("reasoning"));
        assert!(!head.contains("prompt_cache_key"));
    }

    #[test]
    fn reasoning_off_writes_the_off_word_without_encrypted_content() {
        let builder = RequestBuilder::with_compat(
            "m",
            128,
            None,
            Some("off"),
            None,
            &[],
            Compat {
                reasoning_off: Some("none"),
                ..Compat::DEFAULT
            },
        );
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(head.contains(r#""reasoning":{"effort":"none"}"#), "{head}");
        assert!(!head.contains("reasoning.encrypted_content"), "{head}");
        assert!(!head.contains("summary"), "{head}");
    }

    #[test]
    fn reasoning_off_without_a_word_writes_no_reasoning() {
        for text in ["off", "none", "0"] {
            let builder = RequestBuilder::new("m", 128, None, Some(text), None, &[]);
            let head = std::str::from_utf8(builder.head()).unwrap();
            assert!(!head.contains("reasoning"), "{text}: {head}");
        }
    }

    #[test]
    fn chatgpt_preset_head_has_no_cap_and_keeps_store_and_stream() {
        let builder = RequestBuilder::with_compat(
            "gpt-5",
            128,
            None,
            Some("off"),
            None,
            &[],
            crate::providers::get("chatgpt").expect("the preset").compat,
        );
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(!head.contains("max_output_tokens"), "{head}");
        assert!(!head.contains("max_tokens"), "{head}");
        assert!(head.contains(r#""store":false"#), "{head}");
        assert!(head.contains(r#""stream":true"#), "{head}");
        assert!(head.contains(r#""reasoning":{"effort":"none"}"#), "{head}");
    }

    #[test]
    fn request_body_is_stable_for_the_same_history() {
        let provider = crate::openai::responses::OpenAiResponses::new("key", "http://localhost");
        let history = vec![Message::user("one")];
        let request = Request {
            model: "gpt-5",
            max_tokens: 128,
            system: None,
            tools: &[],
            messages: &history,
            reasoning: None,
            cache_key: None,
        };
        let first = provider.request_body(&request).unwrap();
        let second = provider.request_body(&request).unwrap();
        assert_eq!(first, second);
    }
}
