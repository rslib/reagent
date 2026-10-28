//! Rules that apply when one history is rendered to more than one provider.

use crate::Request;
use crate::types::{Content, Message, Reasoning};

/// The reasoning text Anthropic may enable for a request.
///
/// A token budget is turned off when the last assistant message calls a tool
/// with no Anthropic thinking block, which budget thinking rejects. A level
/// stays on: adaptive thinking accepts that tool use, and Claude skips thinking
/// on most tool turns, so a drop would lose the effort and the messages cache.
pub(crate) fn anthropic_reasoning<'a>(request: &Request<'a>) -> Option<&'a str> {
    let reasoning = request.reasoning?;
    if !matches!(Reasoning::parse(reasoning), Reasoning::Budget(_)) {
        return Some(reasoning);
    }
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

    fn request<'a>(reasoning: Option<&'a str>, messages: &'a [Message]) -> Request<'a> {
        Request {
            model: "m",
            max_tokens: 128,
            system: None,
            tools: &[],
            messages,
            images: true,
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
        let request = request(Some("medium"), &history);

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
    fn anthropic_budget_turns_thinking_off_for_a_foreign_tool_use() {
        let history = vec![
            Message::user("hello"),
            assistant("openai", vec![Content::text("hi"), tool_call()]),
            Message::tool_result("call_01", "ok", false),
        ];
        let request = request(Some("4096"), &history);
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
        let request = request(Some("high"), &history);
        let body = Anthropic::new("key", "http://localhost")
            .request_body(&request)
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#""thinking":{"type":"adaptive"}"#));
        assert!(body.contains(r#""output_config":{"effort":"high"}"#));
    }

    #[test]
    fn anthropic_budget_turns_thinking_off_for_a_tool_use_without_thinking() {
        let history = vec![
            Message::user("hello"),
            assistant(
                crate::anthropic::PROVIDER,
                vec![Content::text("hi"), tool_call()],
            ),
            Message::tool_result("call_01", "ok", false),
        ];
        let request = request(Some("4096"), &history);
        let body = Anthropic::new("key", "http://localhost")
            .request_body(&request)
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("thinking"));
        assert!(body.contains("tool_use"));
    }

    #[test]
    fn anthropic_budget_turns_thinking_off_for_a_foreign_tool_use_with_thinking() {
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
        let request = request(Some("4096"), &history);
        let body = Anthropic::new("key", "http://localhost")
            .request_body(&request)
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("thinking"));
        assert!(body.contains("tool_use"));
    }

    fn adaptive_body(provider: &str, content: Vec<Content>) -> String {
        let history = vec![
            Message::user("hello"),
            assistant(provider, content),
            Message::tool_result("call_01", "ok", false),
        ];
        let request = request(Some("high"), &history);
        let body = Anthropic::new("key", "http://localhost")
            .request_body(&request)
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[test]
    fn anthropic_level_keeps_thinking_after_a_tool_use_without_thinking() {
        let body = adaptive_body(
            crate::anthropic::PROVIDER,
            vec![Content::text("hi"), tool_call()],
        );
        assert!(body.contains(r#""thinking":{"type":"adaptive"}"#), "{body}");
        assert!(
            body.contains(r#""output_config":{"effort":"high"}"#),
            "{body}"
        );
    }

    #[test]
    fn anthropic_level_keeps_thinking_after_a_foreign_tool_use() {
        let body = adaptive_body("openai", vec![Content::text("hi"), tool_call()]);
        assert!(body.contains(r#""thinking":{"type":"adaptive"}"#), "{body}");
        assert!(
            body.contains(r#""output_config":{"effort":"high"}"#),
            "{body}"
        );
    }

    #[test]
    fn anthropic_level_keeps_thinking_after_a_foreign_tool_use_with_thinking() {
        let body = adaptive_body(
            "openai",
            vec![
                Content::Thinking {
                    thinking: "plan".into(),
                    signature: Some("sig".into()),
                },
                tool_call(),
            ],
        );
        assert!(body.contains(r#""thinking":{"type":"adaptive"}"#), "{body}");
        assert!(
            !body.contains("plan"),
            "foreign thinking is still dropped: {body}"
        );
    }

    #[test]
    fn anthropic_drops_the_reasoning_option_when_asked_to() {
        let history = vec![
            Message::user("hello"),
            assistant("openai", vec![Content::text("hi"), tool_call()]),
        ];
        assert!(
            anthropic_reasoning(&request(None, &history)).is_none(),
            "no reasoning was requested"
        );
        assert!(
            anthropic_reasoning(&request(Some("4096"), &history)).is_none(),
            "a tool use without thinking disables budget thinking"
        );
        assert_eq!(
            anthropic_reasoning(&request(Some("low"), &history)),
            Some("low"),
            "a level keeps adaptive thinking"
        );
    }

    #[test]
    fn an_image_reaches_every_wire() {
        use crate::types::{Image, ImageType};

        let history = vec![
            Message::user("hello"),
            Message::User {
                content: vec![
                    Content::text("look"),
                    Content::Image(Image {
                        media_type: ImageType::Png,
                        width: 1,
                        height: 1,
                        data: "aGk=".into(),
                    }),
                ],
            },
        ];
        let request = request(None, &history);
        for (wire, body) in [
            (
                "anthropic",
                Anthropic::new("key", "http://localhost")
                    .request_body(&request)
                    .unwrap(),
            ),
            (
                "responses",
                OpenAiResponses::new("key", "http://localhost")
                    .request_body(&request)
                    .unwrap(),
            ),
            (
                "chat",
                OpenAiChat::new("key", "http://localhost")
                    .request_body(&request)
                    .unwrap(),
            ),
        ] {
            let body = std::str::from_utf8(&body).unwrap();
            assert!(body.contains("aGk="), "{wire}: {body}");
        }
    }

    /// A body builder that keeps its provider's caches between calls.
    type Build = Box<dyn Fn(&Request<'_>) -> bytes::Bytes>;

    /// One provider of each wire, as a body builder.
    fn every_wire() -> Vec<(&'static str, Build)> {
        let anthropic = Anthropic::new("key", "http://localhost");
        let responses = OpenAiResponses::new("key", "http://localhost");
        let chat = OpenAiChat::new("key", "http://localhost");
        let pi = crate::pi::PiMessages::new("key", "http://localhost");
        vec![
            (
                "anthropic",
                Box::new(move |request| anthropic.request_body(request).unwrap()),
            ),
            (
                "responses",
                Box::new(move |request| responses.request_body(request).unwrap()),
            ),
            (
                "chat",
                Box::new(move |request| chat.request_body(request).unwrap()),
            ),
            (
                "pi",
                Box::new(move |request| pi.request_body(request).unwrap()),
            ),
        ]
    }

    fn png(data: &str) -> crate::types::Image {
        crate::types::Image {
            media_type: crate::types::ImageType::Png,
            width: 1,
            height: 1,
            data: data.into(),
        }
    }

    fn call(id: &str) -> Content {
        Content::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: "{}".into(),
        })
    }

    #[test]
    fn turning_images_off_rebuilds_the_cached_image_bodies_on_every_wire() {
        let history = vec![
            Message::User {
                content: vec![Content::text("look"), Content::Image(png("aGk="))],
            },
            assistant("m", vec![call("call_1")]),
            Message::tool_result_with_images("call_1", "ok", false, vec![png("Ynll")]),
        ];
        for (wire, body) in every_wire() {
            let on = body(&request(None, &history));
            let on = std::str::from_utf8(&on).unwrap();
            assert!(on.contains("aGk=") && on.contains("Ynll"), "{wire}: {on}");
            let off = Request {
                images: false,
                ..request(None, &history)
            };
            let off = body(&off);
            let off = std::str::from_utf8(&off).unwrap();
            assert!(
                !off.contains("aGk=") && !off.contains("Ynll"),
                "{wire}: {off}"
            );
            assert!(
                off.contains("[image not sent: the model takes no images]"),
                "{wire}: {off}"
            );
        }
    }

    #[test]
    fn tool_result_images_keep_their_bytes_and_order_on_the_next_turn_on_every_wire() {
        let mut history = vec![
            Message::user("hello"),
            assistant("m", vec![call("call_1"), call("call_2")]),
            Message::tool_result_with_images("call_1", "one", false, vec![png("Zmlyc3Q=")]),
            Message::tool_result_with_images("call_2", "two", false, vec![png("c2Vjb25k")]),
        ];
        for (wire, body) in every_wire() {
            let first = body(&request(None, &history));
            let first = std::str::from_utf8(&first).unwrap().to_owned();
            history.push(assistant("m", vec![Content::text("seen")]));
            history.push(Message::user("next"));
            let second = body(&request(None, &history));
            let second = std::str::from_utf8(&second).unwrap();
            history.truncate(4);
            // The images sit between the tool results and the next turn, in
            // call order, with the same bytes as on the turn they came back.
            let one = second.find("Zmlyc3Q=").expect("first image");
            let two = second.find("c2Vjb25k").expect("second image");
            let next = second.find("\"next\"").expect("next user text");
            assert!(one < two && two < next, "{wire}: {second}");
            let start = first.find("hello").unwrap();
            let end = first.rfind("c2Vjb25k").unwrap();
            assert!(
                second.contains(&first[start..end]),
                "{wire}: the first turn's messages changed\n{first}\n{second}"
            );
        }
    }

    #[test]
    fn cached_tokens_are_subtracted_from_input_for_every_openai_provider() {
        let mut parser = ResponsesParser::new();
        let mut usage = Usage::default();
        let fixture = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_responses_text.sse"
        ));
        let mut events = Vec::new();
        for line in fixture.lines() {
            parser.push_line(line.as_bytes(), &mut events).unwrap();
        }
        for event in events {
            if let StreamEvent::Usage(seen) = event {
                usage = seen;
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
        let mut events = Vec::new();
        for line in fixture.lines() {
            parser.push_line(line.as_bytes(), &mut events).unwrap();
        }
        for event in events {
            if let StreamEvent::Usage(seen) = event {
                usage = seen;
            }
        }
        parser.finish().unwrap();
        assert_eq!(usage.input_tokens, 40);
        assert_eq!(usage.cache_read_tokens, 120);
    }
}
