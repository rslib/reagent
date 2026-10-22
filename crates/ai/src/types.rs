//! Message, content, tool and usage types shared by every provider.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Immutable message text, shared without a copy.
pub type Text = Arc<str>;

/// Who produced a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// A message from the user, the system or a tool result.
    User,
    /// A message from the model.
    Assistant,
    /// The output of one tool call.
    ToolResult,
}

/// Why a provider stopped producing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its answer.
    EndTurn,
    /// The model wants one or more tools run.
    ToolUse,
    /// The reply hit the token cap.
    MaxTokens,
    /// The model refused to answer.
    Refusal,
    /// The provider reported an error.
    Error,
}

/// How much reasoning a provider may spend on a request, in the provider's
/// own words: a token budget when the config text is a whole number, else the
/// level text the provider reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reasoning<'a> {
    /// A token budget for the reply's reasoning.
    Budget(u32),
    /// The provider's own level text, such as `xhigh`.
    Level(&'a str),
}

impl<'a> Reasoning<'a> {
    /// Reads the config text, trimmed. A whole number is a
    /// [`Reasoning::Budget`], anything else a [`Reasoning::Level`].
    pub fn parse(text: &'a str) -> Reasoning<'a> {
        let text = text.trim();
        match text.parse::<u32>() {
            Ok(budget) => Reasoning::Budget(budget),
            Err(_) => Reasoning::Level(text),
        }
    }

    /// The text the provider reads: the level itself, or a budget as a bare
    /// number.
    pub fn text(self) -> std::borrow::Cow<'a, str> {
        match self {
            Reasoning::Level(text) => std::borrow::Cow::Borrowed(text),
            Reasoning::Budget(budget) => std::borrow::Cow::Owned(budget.to_string()),
        }
    }

    /// Whether the text turns reasoning off. A provider's `off` and `none`
    /// mean the same thing.
    pub fn is_off(self) -> bool {
        match self {
            Reasoning::Level(text) => {
                text.eq_ignore_ascii_case("off") || text.eq_ignore_ascii_case("none")
            }
            Reasoning::Budget(budget) => budget == 0,
        }
    }
}

/// A model request to run one tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call id, echoed back with the result.
    pub id: Text,
    /// The tool name.
    pub name: Text,
    /// Raw JSON arguments as the model emitted them.
    pub arguments: Text,
}

/// One block of a user or assistant message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Visible text.
    Text {
        /// The text itself.
        text: Text,
    },
    /// The model's private reasoning.
    Thinking {
        /// The reasoning text.
        thinking: Text,
        /// Opaque provider token that lets the block be sent back unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<Text>,
    },
    /// A thinking block the provider replaced with an opaque payload.
    RedactedThinking {
        /// The opaque provider payload.
        data: Text,
    },
    /// A request to run a tool.
    ToolCall(ToolCall),
}

impl Content {
    /// Builds a text block.
    pub fn text(text: impl Into<Text>) -> Self {
        Content::Text { text: text.into() }
    }

    /// Whether the block is provider-private reasoning.
    pub fn is_thinking(&self) -> bool {
        matches!(
            self,
            Content::Thinking { .. } | Content::RedactedThinking { .. }
        )
    }
}

/// The output of one tool call, sent back as its own message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The call id this result answers.
    pub call_id: Text,
    /// The text the tool produced.
    pub content: Text,
    /// Whether the tool failed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
}

/// One assistant reply plus the metadata needed to send it back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// The reply content blocks.
    pub content: Vec<Content>,
    /// The provider that produced the reply.
    pub provider: Text,
    /// The model that produced the reply.
    pub model: Text,
    /// Why the reply ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// Token counts for the reply.
    #[serde(default)]
    pub usage: Usage,
}

impl AssistantMessage {
    /// Builds an assistant reply with no stop reason and no usage.
    pub fn new(provider: impl Into<Text>, model: impl Into<Text>, content: Vec<Content>) -> Self {
        Self {
            content,
            provider: provider.into(),
            model: model.into(),
            stop_reason: None,
            usage: Usage::default(),
        }
    }
}

/// One entry of the conversation sent to a provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// A user message.
    User {
        /// The message content blocks.
        content: Vec<Content>,
    },
    /// An assistant reply.
    Assistant(AssistantMessage),
    /// The result of one tool call.
    ToolResult(ToolResult),
}

impl Message {
    /// The role of this message.
    pub fn role(&self) -> Role {
        match self {
            Message::User { .. } => Role::User,
            Message::Assistant(_) => Role::Assistant,
            Message::ToolResult(_) => Role::ToolResult,
        }
    }

    /// Builds a user message with one text block.
    pub fn user(text: impl Into<Text>) -> Self {
        Message::User {
            content: vec![Content::text(text)],
        }
    }

    /// Builds a user message from content blocks.
    pub fn user_content(content: Vec<Content>) -> Self {
        Message::User { content }
    }

    /// Wraps an assistant reply as a message.
    pub fn assistant(message: AssistantMessage) -> Self {
        Message::Assistant(message)
    }

    /// Builds the result of one tool call.
    pub fn tool_result(call_id: impl Into<Text>, content: impl Into<Text>, is_error: bool) -> Self {
        Message::ToolResult(ToolResult {
            call_id: call_id.into(),
            content: content.into(),
            is_error,
        })
    }
}

/// One tool offered to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The tool name the model calls.
    pub name: String,
    /// One or two sentences that tell the model what the tool does.
    pub description: String,
    /// JSON Schema for the tool arguments.
    pub input_schema: serde_json::Value,
}

/// Token counts reported by a provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens, excluding tokens read from or written to the cache.
    pub input_tokens: u64,
    /// Tokens in the reply.
    pub output_tokens: u64,
    /// Input tokens served from the prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to the prompt cache.
    pub cache_write_tokens: u64,
    /// Tokens the provider spent on private reasoning.
    pub reasoning_tokens: u64,
    /// The cost of this reply in USD, when the provider reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

// `f64` has no `Eq`, so the marker is written out by hand. A provider never
// reports a `NaN` cost, and a usage is compared only in tests and in a history
// round trip.
impl Eq for Usage {}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_assistant() -> AssistantMessage {
        let mut assistant = AssistantMessage::new(
            "anthropic",
            "claude-sonnet-4-5",
            vec![
                Content::Thinking {
                    thinking: "hmm".into(),
                    signature: Some("sig".into()),
                },
                Content::Text {
                    text: "hello".into(),
                },
                Content::ToolCall(ToolCall {
                    id: "toolu_01".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a.rs\"}".into(),
                }),
            ],
        );
        assistant.stop_reason = Some(StopReason::ToolUse);
        assistant.usage.input_tokens = 10;
        assistant
    }

    #[test]
    fn reasoning_text_parses_a_level_and_a_budget() {
        assert_eq!(Reasoning::parse("high"), Reasoning::Level("high"));
        assert_eq!(Reasoning::parse("4096"), Reasoning::Budget(4096));
        assert_eq!(Reasoning::parse(" 12 "), Reasoning::Budget(12));
        assert_eq!(Reasoning::parse(" xhigh "), Reasoning::Level("xhigh"));
        assert_eq!(Reasoning::parse("median").text(), "median");
        assert_eq!(Reasoning::parse("4096").text(), "4096");
    }

    #[test]
    fn reasoning_text_off_and_none_are_off_but_a_budget_is_not() {
        assert!(Reasoning::parse("off").is_off());
        assert!(Reasoning::parse(" none ").is_off());
        assert!(!Reasoning::parse("high").is_off());
        assert!(!Reasoning::parse("4096").is_off());
        assert!(
            Reasoning::parse("0").is_off(),
            "a zero budget thinks not at all"
        );
    }

    #[test]
    fn history_round_trips_through_json() {
        let messages = vec![
            Message::user("hi"),
            Message::assistant(sample_assistant()),
            Message::tool_result("toolu_01", "fn main() {}", false),
        ];
        let json = serde_json::to_string(&messages).unwrap();
        let back: Vec<Message> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, messages);
    }

    #[test]
    fn assistant_metadata_survives_a_round_trip() {
        let message = Message::assistant(sample_assistant());
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains(r#""provider":"anthropic""#));
        assert!(json.contains(r#""stop_reason":"tool_use""#));
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn text_is_shared_not_copied() {
        let message = Message::user("shared");
        let clone = message.clone();
        let (Message::User { content }, Message::User { content: copy }) = (&message, &clone)
        else {
            panic!("expected user messages");
        };
        let (Content::Text { text }, Content::Text { text: other }) = (&content[0], &copy[0])
        else {
            panic!("expected text blocks");
        };
        assert!(Arc::ptr_eq(text, other));
    }

    #[test]
    fn roles_cover_user_assistant_and_tool_result() {
        assert_eq!(Message::user("x").role(), Role::User);
        assert_eq!(
            Message::assistant(sample_assistant()).role(),
            Role::Assistant
        );
        assert_eq!(
            Message::tool_result("id", "out", true).role(),
            Role::ToolResult
        );
    }

    #[test]
    fn tool_result_hides_a_false_error_flag() {
        let message = Message::tool_result("toolu_02", "ok", false);
        let json = serde_json::to_string(&message).unwrap();
        assert!(!json.contains("is_error"));
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn usage_round_trips_and_defaults_to_zero() {
        let usage = Usage {
            input_tokens: 12,
            output_tokens: 3,
            cache_read_tokens: 100,
            cache_write_tokens: 50,
            reasoning_tokens: 7,
            cost: None,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let back: Usage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, usage);
        assert!(!json.contains("cost"), "{json}");
        assert_eq!(Usage::default().reasoning_tokens, 0);
    }

    #[test]
    fn usage_cost_round_trips_when_known() {
        let usage = Usage {
            input_tokens: 12,
            cost: Some(0.0123),
            ..Usage::default()
        };
        let json = serde_json::to_string(&usage).unwrap();
        assert!(json.contains(r#""cost":0.0123"#), "{json}");
        let back: Usage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cost, Some(0.0123));
        assert_eq!(back, usage);
    }

    #[test]
    fn stop_reason_round_trips() {
        for reason in [
            StopReason::EndTurn,
            StopReason::ToolUse,
            StopReason::MaxTokens,
            StopReason::Refusal,
            StopReason::Error,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            let back: StopReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, reason);
        }
    }

    #[test]
    fn tool_spec_round_trips() {
        let spec = ToolSpec {
            name: "read".into(),
            description: "Read a file.".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: ToolSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }
}
