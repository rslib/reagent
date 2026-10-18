//! Rules that apply when one history is rendered to more than one provider.

use crate::Request;
use crate::types::{Content, Message, ReasoningLevel};

/// The reasoning level Anthropic may enable for a request.
///
/// Thinking is turned off when the last assistant message calls a tool and has
/// no Anthropic thinking block. The Anthropic API rejects a tool use without a
/// thinking block while thinking is enabled, and thinking from another
/// provider is dropped on the wire.
pub(crate) fn anthropic_reasoning(request: &Request) -> Option<ReasoningLevel> {
    let reasoning = request.reasoning?;
    let tool_use_without_thinking = request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .is_some_and(|assistant| {
            assistant
                .content
                .iter()
                .any(|content| matches!(content, Content::ToolCall(_)))
                && !(assistant.provider.as_ref() == crate::anthropic::PROVIDER
                    && assistant.content.iter().any(Content::is_thinking))
        });
    if tool_use_without_thinking {
        None
    } else {
        Some(reasoning)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StreamEvent;
    use crate::anthropic::Anthropic;
    use crate::openai::chat::{OpenAiChat, StreamParser as ChatParser};
    use crate::openai::responses::{OpenAiResponses, stream::StreamParser as ResponsesParser};
    use crate::types::{AssistantMessage, ToolCall, Usage};

    fn request(reasoning: Option<ReasoningLevel>, messages: Vec<Message>) -> Request {
        Request {
            model: "m".into(),
            max_tokens: 128,
            system: None,
            tools: Vec::new(),
            messages,
            reasoning,
            cache_key: None,
        }
    }

    fn assistant(provider: &str, content: Vec<Content>) -> Message {
        Message::assistant(AssistantMessage::new(provider, "m", content))
    }

    fn tool_call() -> Content {
        Content::ToolCall(ToolCall {
            id: "call_01".into(),
            name: "read".into(),
            arguments: "{}".into(),
        })
    }

    #[test]
    fn foreign_thinking_is_dropped_from_every_other_provider() {
        let history = vec![
            Message::user("hello"),
            assistant(
                crate::anthropic::PROVIDER,
                vec![
                    Content::Thinking {
                        thinking: "secret".into(),
                        signature: Some("sig".into()),
                    },
                    Content::text("hi"),
                    tool_call(),
                ],
            ),
            Message::tool_result("call_01", "ok", false),
            Message::user("explain"),
        ];
        let request = request(Some(ReasoningLevel::Medium), history);

        let anthropic = Anthropic::new("key", "http://localhost");
        let body = anthropic.request_body(&request).unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("secret"));
        assert!(body.contains(r#""signature":"sig""#));

        let responses = OpenAiResponses::new("key", "http://localhost");
        let body = responses.request_body(&request).unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("secret"));
        assert!(!body.contains("\"encrypted_content\":\"sig\""));
        assert!(body.contains(r#""type":"function_call""#));

        let chat = OpenAiChat::new("key", "http://localhost");
        let body = chat.request_body(&request).unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("secret"));
        assert!(!body.contains("reasoning_content"));
    }

    #[test]
    fn anthropic_turns_thinking_off_for_a_foreign_tool_use() {
        let history = vec![
            Message::user("hello"),
            assistant("openai", vec![Content::text("hi"), tool_call()]),
            Message::tool_result("call_01", "ok", false),
        ];
        let request = request(Some(ReasoningLevel::High), history);
        let body = Anthropic::new("key", "http://localhost")
            .request_body(&request)
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("thinking"));
        assert!(body.contains("tool_use"));
    }

    #[test]
    fn anthropic_keeps_thinking_for_its_own_tool_use() {
        let history = vec![
            Message::user("hello"),
            assistant(
                crate::anthropic::PROVIDER,
                vec![
                    Content::Thinking {
                        thinking: "plan".into(),
                        signature: Some("sig".into()),
                    },
                    tool_call(),
                ],
            ),
            Message::tool_result("call_01", "ok", false),
        ];
        let request = request(Some(ReasoningLevel::High), history);
        let body = Anthropic::new("key", "http://localhost")
            .request_body(&request)
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#""thinking":{"type":"enabled","budget_tokens":16384}"#));
    }

    #[test]
    fn anthropic_turns_thinking_off_for_a_tool_use_without_thinking() {
        let history = vec![
            Message::user("hello"),
            assistant(
                crate::anthropic::PROVIDER,
                vec![Content::text("hi"), tool_call()],
            ),
            Message::tool_result("call_01", "ok", false),
        ];
        let request = request(Some(ReasoningLevel::High), history);
        let body = Anthropic::new("key", "http://localhost")
            .request_body(&request)
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("thinking"));
        assert!(body.contains("tool_use"));
    }

    #[test]
    fn anthropic_turns_thinking_off_for_a_foreign_tool_use_with_thinking() {
        let history = vec![
            Message::user("hello"),
            assistant(
                "openai",
                vec![
                    Content::Thinking {
                        thinking: "plan".into(),
                        signature: Some("sig".into()),
                    },
                    tool_call(),
                ],
            ),
            Message::tool_result("call_01", "ok", false),
        ];
        let request = request(Some(ReasoningLevel::High), history);
        let body = Anthropic::new("key", "http://localhost")
            .request_body(&request)
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("thinking"));
        assert!(body.contains("tool_use"));
    }

    #[test]
    fn anthropic_drops_the_reasoning_option_when_asked_to() {
        let history = vec![
            Message::user("hello"),
            assistant("openai", vec![Content::text("hi"), tool_call()]),
        ];
        assert!(
            anthropic_reasoning(&request(None, history.clone())).is_none(),
            "no reasoning was requested"
        );
        assert!(
            anthropic_reasoning(&request(Some(ReasoningLevel::Low), history)).is_none(),
            "a tool use without thinking disables thinking"
        );
    }

    #[test]
    fn cached_tokens_are_subtracted_from_input_for_every_openai_provider() {
        let mut parser = ResponsesParser::new();
        let mut usage = Usage::default();
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_responses_text.sse"
        ));
        for line in fixture.lines() {
            for event in parser.push_line(line.as_bytes()).unwrap() {
                if let StreamEvent::Usage(seen) = event {
                    usage = seen;
                }
            }
        }
        parser.finish().unwrap();
        assert_eq!(usage.input_tokens, 25);
        assert_eq!(usage.cache_read_tokens, 100);

        let mut parser = ChatParser::new();
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_chat_tool.sse"
        ));
        for line in fixture.lines() {
            for event in parser.push_line(line.as_bytes()).unwrap() {
                if let StreamEvent::Usage(seen) = event {
                    usage = seen;
                }
            }
        }
        parser.finish().unwrap();
        assert_eq!(usage.input_tokens, 40);
        assert_eq!(usage.cache_read_tokens, 120);
    }
}
