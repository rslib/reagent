//! Parses the Anthropic Messages SSE stream.

use std::borrow::Cow;

use serde::Deserialize;

use crate::types::StopReason;
use crate::{Error, StreamEvent, Text, Usage};

/// Parses one Anthropic SSE stream, line by line.
///
/// Event fields borrow from the line handed to [`StreamParser::push_line`], and
/// only the events it returns own their text.
#[derive(Debug, Default)]
pub struct StreamParser {
    usage: Usage,
    tool_id: Option<Text>,
    stop_reason: Option<StopReason>,
    done: bool,
    failed: bool,
}

impl StreamParser {
    /// Creates a parser at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one SSE line and returns the event it carries, if any.
    pub fn push_line(&mut self, line: &[u8]) -> Result<Option<StreamEvent>, Error> {
        let Some(data) = data_of(line) else {
            return Ok(None);
        };
        let data = match std::str::from_utf8(data) {
            Ok(data) => data,
            Err(error) => {
                self.failed = true;
                return Err(Error::Protocol(error.to_string()));
            }
        };
        let event: Event<'_> = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(error) => {
                self.failed = true;
                return Err(Error::Json(error));
            }
        };

        match event {
            Event::MessageStart { message } => {
                self.usage.input_tokens = message.usage.input_tokens;
                self.usage.cache_read_tokens = message.usage.cache_read_input_tokens;
                self.usage.cache_write_tokens = message.usage.cache_creation_input_tokens;
                Ok(None)
            }
            Event::MessageDelta { delta, usage } => {
                self.usage.output_tokens = usage.output_tokens;
                self.stop_reason = delta.stop_reason.as_deref().map(stop_reason);
                Ok(Some(StreamEvent::Usage(self.usage)))
            }
            Event::MessageStop => {
                self.done = true;
                let reason = self.stop_reason.take().unwrap_or(StopReason::Error);
                Ok(Some(StreamEvent::Stop(reason)))
            }
            Event::ContentBlockStart { content_block } => match content_block.kind {
                "tool_use" => {
                    let id: Text = content_block.id.unwrap_or_default().into();
                    let name: Text = content_block.name.unwrap_or_default().into();
                    self.tool_id = Some(id.clone());
                    Ok(Some(StreamEvent::ToolCallStart { id, name }))
                }
                "redacted_thinking" => Ok(Some(StreamEvent::RedactedThinking(Text::from(
                    content_block.data.unwrap_or_default(),
                )))),
                _ => Ok(None),
            },
            Event::ContentBlockDelta { delta } => {
                let event = match delta {
                    Delta {
                        kind: "text_delta",
                        text: Some(text),
                        ..
                    } => StreamEvent::TextDelta(Text::from(text.as_ref())),
                    Delta {
                        kind: "thinking_delta",
                        thinking: Some(thinking),
                        ..
                    } => StreamEvent::ThinkingDelta(Text::from(thinking.as_ref())),
                    Delta {
                        kind: "signature_delta",
                        signature: Some(signature),
                        ..
                    } => StreamEvent::ThinkingSignature(Text::from(signature.as_ref())),
                    Delta {
                        kind: "input_json_delta",
                        partial_json: Some(partial_json),
                        ..
                    } => StreamEvent::ToolCallDelta {
                        id: self.tool_id.clone().unwrap_or_default(),
                        partial_json: Text::from(partial_json.as_ref()),
                    },
                    _ => return Ok(None),
                };
                Ok(Some(event))
            }
            Event::ContentBlockStop => Ok(self
                .tool_id
                .take()
                .map(|id| StreamEvent::ToolCallEnd { id })),
            Event::Ping | Event::Unknown => Ok(None),
            Event::Error { error } => {
                self.failed = true;
                Err(Error::Protocol(error.message.into_owned()))
            }
        }
    }

    /// Ends the stream after an error from outside the parser, such as HTTP.
    pub fn fail(&mut self) {
        self.failed = true;
    }

    /// Fails when the stream ended without a `message_stop`.
    ///
    /// After an earlier error the stream is over and this reports success
    /// instead of a second error.
    pub fn finish(&self) -> Result<(), Error> {
        if self.failed {
            return Ok(());
        }
        if self.done {
            Ok(())
        } else {
            Err(Error::Protocol("stream ended before message_stop".into()))
        }
    }
}

fn stop_reason(value: &str) -> StopReason {
    match value {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        _ => StopReason::Error,
    }
}

fn data_of(line: &[u8]) -> Option<&[u8]> {
    let rest = line.strip_prefix(b"data:")?;
    let rest = rest.strip_prefix(b" ").unwrap_or(rest);
    (!rest.is_empty()).then_some(rest)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event<'a> {
    MessageStart {
        message: StartedMessage,
    },
    ContentBlockStart {
        #[serde(borrow)]
        content_block: ContentBlock<'a>,
    },
    ContentBlockDelta {
        #[serde(borrow)]
        delta: Delta<'a>,
    },
    ContentBlockStop,
    MessageDelta {
        #[serde(borrow, default)]
        delta: DeltaStop<'a>,
        #[serde(default)]
        usage: UsageJson,
    },
    MessageStop,
    Ping,
    Error {
        #[serde(borrow)]
        error: ApiError<'a>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Default, Deserialize)]
struct UsageJson {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Deserialize)]
struct StartedMessage {
    #[serde(default)]
    usage: UsageJson,
}

#[derive(Default, Deserialize)]
struct DeltaStop<'a> {
    #[serde(default, borrow)]
    stop_reason: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct ContentBlock<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    #[serde(default, borrow)]
    id: Option<&'a str>,
    #[serde(default, borrow)]
    name: Option<&'a str>,
    #[serde(default, borrow)]
    data: Option<&'a str>,
}

#[derive(Deserialize)]
struct Delta<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    #[serde(default, borrow)]
    text: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    thinking: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    signature: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    partial_json: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct ApiError<'a> {
    #[serde(borrow)]
    message: Cow<'a, str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseSplitter;
    use bytes::Bytes;

    fn parse(fixture: &str) -> Vec<StreamEvent> {
        let mut parser = StreamParser::new();
        let mut events = Vec::new();
        for line in fixture.lines() {
            if let Some(event) = parser.push_line(line.as_bytes()).expect("fixture parses") {
                events.push(event);
            }
        }
        events
    }

    fn text_of(events: &[StreamEvent]) -> (String, String, Usage) {
        let mut text = String::new();
        let mut thinking = String::new();
        let mut usage = Usage::default();
        for event in events {
            match event {
                StreamEvent::TextDelta(delta) => text.push_str(delta),
                StreamEvent::ThinkingDelta(delta) => thinking.push_str(delta),
                StreamEvent::Usage(seen) => usage = *seen,
                _ => {}
            }
        }
        (text, thinking, usage)
    }

    #[test]
    fn parses_text_thinking_and_usage() {
        let events = parse(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/anthropic_text.sse"
        )));
        let (text, thinking, usage) = text_of(&events);
        assert_eq!(thinking, "Let me think.");
        assert_eq!(text, "Hello, world!");
        assert_eq!(usage.input_tokens, 25);
        assert_eq!(usage.cache_write_tokens, 100);
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.output_tokens, 12);
        assert_eq!(events.last(), Some(&StreamEvent::Stop(StopReason::EndTurn)));
    }

    #[test]
    fn parses_a_tool_call_from_recorded_sse() {
        let events = parse(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/anthropic_tool.sse"
        )));
        let mut arguments = String::new();
        let mut started = None;
        let mut ended = None;
        for event in &events {
            match event {
                StreamEvent::ToolCallStart { id, name } => {
                    started = Some((id.to_string(), name.to_string()));
                }
                StreamEvent::ToolCallDelta { partial_json, .. } => arguments.push_str(partial_json),
                StreamEvent::ToolCallEnd { id } => ended = Some(id.to_string()),
                _ => {}
            }
        }
        assert_eq!(started, Some(("toolu_02".to_string(), "read".to_string())));
        assert_eq!(ended, Some("toolu_02".to_string()));
        assert_eq!(arguments, "{\"path\":\"a.rs\"}");
        assert_eq!(events.last(), Some(&StreamEvent::Stop(StopReason::ToolUse)));
    }

    #[test]
    fn parses_chunked_bytes_through_the_splitter() {
        let chunks: Vec<&[u8]> = vec![
            b"event: message_start\r",
            b"\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n",
            b"\n",
            b": keep-alive\n",
            b"event: content_block_delta\n",
            b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"te",
            b"xt\":\"Hi\"}}\n",
            b"\n",
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n",
            b"\n",
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n",
        ];

        let mut splitter = SseSplitter::new();
        let mut parser = StreamParser::new();
        let mut events = Vec::new();
        for chunk in chunks {
            let chunk = Bytes::copy_from_slice(chunk);
            let mut lines = splitter.push(&chunk);
            while let Some(line) = lines.next_line() {
                if let Some(event) = parser.push_line(line).expect("chunk parses") {
                    events.push(event);
                }
            }
        }
        parser.finish().expect("message_stop was seen");

        assert!(events.contains(&StreamEvent::TextDelta("Hi".into())));
        assert_eq!(events.last(), Some(&StreamEvent::Stop(StopReason::EndTurn)));
        let usage = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::Usage(usage) => Some(*usage),
                _ => None,
            })
            .expect("usage");
        assert_eq!(usage.input_tokens, 5);
        assert_eq!(usage.output_tokens, 2);
    }

    #[test]
    fn unescapes_text_with_json_escapes() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"line\n\"quoted\""}}"#;
        let event = parser.push_line(line).unwrap().unwrap();
        match event {
            StreamEvent::TextDelta(text) => assert_eq!(&*text, "line\n\"quoted\""),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_a_thinking_signature_delta() {
        let mut parser = StreamParser::new();
        let line =
            br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#;
        assert_eq!(
            parser.push_line(line).unwrap(),
            Some(StreamEvent::ThinkingSignature("abc".into()))
        );
    }

    #[test]
    fn parses_redacted_thinking() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#;
        assert_eq!(
            parser.push_line(line).unwrap(),
            Some(StreamEvent::RedactedThinking("opaque".into()))
        );
    }

    #[test]
    fn ignores_non_data_lines() {
        let mut parser = StreamParser::new();
        assert!(parser.push_line(b"event: message_stop").unwrap().is_none());
        assert!(parser.push_line(b": comment").unwrap().is_none());
        assert!(parser.push_line(b"data:").unwrap().is_none());
    }

    #[test]
    fn a_malformed_data_line_is_an_error() {
        let mut parser = StreamParser::new();
        let error = parser.push_line(b"data: {not json").unwrap_err();
        assert!(matches!(error, Error::Json(_)));
    }

    #[test]
    fn an_error_event_stops_with_the_message() {
        let mut parser = StreamParser::new();
        let line =
            br#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let error = parser.push_line(line).unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message == "Overloaded"));
    }

    #[test]
    fn missing_message_stop_is_an_error() {
        let mut parser = StreamParser::new();
        parser
            .push_line(
                br#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}"#,
            )
            .unwrap();
        let error = parser.finish().unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message.contains("message_stop")));
    }
}
