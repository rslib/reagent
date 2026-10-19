//! Parses the Anthropic Messages SSE stream.

use serde::Deserialize;

use crate::openai::JsonStr;
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

    /// Fails the stream for a known event that lacks a field it needs.
    fn missing(&mut self, kind: &str, field: &str) -> Error {
        self.failed = true;
        Error::Protocol(format!("{kind} event without {field}"))
    }

    /// Feeds one SSE line and appends the event it carries, if any, to `out`.
    pub fn push_line(&mut self, line: &[u8], out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        let Some(data) = data_of(line) else {
            return Ok(());
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

        match event.kind {
            "message_start" => {
                let Some(message) = event.message else {
                    return Err(self.missing("message_start", "message"));
                };
                self.usage.input_tokens = message.usage.input_tokens;
                self.usage.cache_read_tokens = message.usage.cache_read_input_tokens;
                self.usage.cache_write_tokens = message.usage.cache_creation_input_tokens;
                Ok(())
            }
            "message_delta" => {
                self.usage.output_tokens = event.usage.output_tokens;
                self.stop_reason = event
                    .delta
                    .and_then(|delta| delta.stop_reason)
                    .map(stop_reason);
                out.push(StreamEvent::Usage(self.usage));
                Ok(())
            }
            "message_stop" => {
                self.done = true;
                let reason = self.stop_reason.take().unwrap_or(StopReason::Error);
                out.push(StreamEvent::Stop(reason));
                Ok(())
            }
            "content_block_start" => {
                let Some(content_block) = event.content_block else {
                    return Err(self.missing("content_block_start", "content_block"));
                };
                match content_block.kind {
                    "tool_use" => {
                        let id: Text = content_block.id.unwrap_or_default().as_str().into();
                        let name: Text = content_block.name.unwrap_or_default().as_str().into();
                        self.tool_id = Some(id.clone());
                        out.push(StreamEvent::ToolCallStart { id, name });
                    }
                    "redacted_thinking" => {
                        out.push(StreamEvent::RedactedThinking(Text::from(
                            content_block.data.unwrap_or_default().as_str(),
                        )));
                    }
                    _ => {}
                }
                Ok(())
            }
            "content_block_delta" => {
                let Some(delta) = event.delta else {
                    return Err(self.missing("content_block_delta", "delta"));
                };
                let event = match delta {
                    Delta {
                        kind: Some("text_delta"),
                        text: Some(text),
                        ..
                    } => StreamEvent::TextDelta(Text::from(text.as_str())),
                    Delta {
                        kind: Some("thinking_delta"),
                        thinking: Some(thinking),
                        ..
                    } => StreamEvent::ThinkingDelta(Text::from(thinking.as_str())),
                    Delta {
                        kind: Some("signature_delta"),
                        signature: Some(signature),
                        ..
                    } => StreamEvent::ThinkingSignature(Text::from(signature.as_str())),
                    Delta {
                        kind: Some("input_json_delta"),
                        partial_json: Some(partial_json),
                        ..
                    } => StreamEvent::ToolCallDelta {
                        id: self.tool_id.clone().unwrap_or_default(),
                        partial_json: Text::from(partial_json.as_str()),
                    },
                    _ => return Ok(()),
                };
                out.push(event);
                Ok(())
            }
            "content_block_stop" => {
                if let Some(id) = self.tool_id.take() {
                    out.push(StreamEvent::ToolCallEnd { id });
                }
                Ok(())
            }
            "error" => {
                self.failed = true;
                let (message, retryable) = match event.error {
                    Some(error) => {
                        let retryable = error.kind.is_some_and(retryable);
                        (error.message.as_str().to_string(), retryable)
                    }
                    None => ("stream error".into(), false),
                };
                if retryable {
                    Err(Error::Transient(message))
                } else {
                    Err(Error::Protocol(message))
                }
            }
            _ => Ok(()),
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

impl crate::openai::EventParser for StreamParser {
    fn push_line(&mut self, line: &[u8], out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        StreamParser::push_line(self, line, out)
    }

    fn fail(&mut self) {
        StreamParser::fail(self);
    }

    /// This parser emits nothing at the end of a clean stream.
    fn finish(&mut self, _out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        StreamParser::finish(self)
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

/// One Anthropic event, flattened so the parser buffers no field of it.
///
/// An internally tagged enum makes serde hold the whole event and read the
/// fields back out, which allocates a map per event. Dispatching on the type
/// here keeps every field borrowed from the line instead.
#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    #[serde(default)]
    message: Option<StartedMessage>,
    #[serde(default, borrow)]
    content_block: Option<ContentBlock<'a>>,
    #[serde(default, borrow)]
    delta: Option<Delta<'a>>,
    #[serde(default)]
    usage: UsageJson,
    #[serde(default, borrow)]
    error: Option<ApiError<'a>>,
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

#[derive(Deserialize)]
struct ContentBlock<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    #[serde(default, borrow)]
    id: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    name: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    data: Option<JsonStr<'a>>,
}

#[derive(Default, Deserialize)]
struct Delta<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: Option<&'a str>,
    #[serde(default, borrow)]
    text: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    thinking: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    signature: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    partial_json: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    stop_reason: Option<&'a str>,
}

#[derive(Deserialize)]
struct ApiError<'a> {
    #[serde(default, borrow)]
    message: JsonStr<'a>,
    #[serde(default, borrow, rename = "type")]
    kind: Option<&'a str>,
}

/// Whether an Anthropic stream `error` event is safe to retry. `overloaded_error`
/// and `api_error` are transient; authentication, request-validation and other
/// errors are not.
fn retryable(kind: &str) -> bool {
    matches!(kind, "overloaded_error" | "api_error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseSplitter;
    use bytes::Bytes;

    fn parse_line(parser: &mut StreamParser, line: &[u8]) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        parser.push_line(line, &mut out).expect("line parses");
        out
    }

    fn parse(fixture: &str) -> Vec<StreamEvent> {
        let mut parser = StreamParser::new();
        let mut events = Vec::new();
        for line in fixture.lines() {
            parser
                .push_line(line.as_bytes(), &mut events)
                .expect("fixture parses");
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
                parser.push_line(line, &mut events).expect("chunk parses");
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
        let events = parse_line(&mut parser, line);
        match &events[..] {
            [StreamEvent::TextDelta(text)] => assert_eq!(&**text, "line\n\"quoted\""),
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn push_line_appends_to_the_callers_buffer() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let first =
            br#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"a"}}"#;
        let second =
            br#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"b"}}"#;
        parser.push_line(first, &mut out).unwrap();
        assert_eq!(out, vec![StreamEvent::TextDelta("a".into())]);
        parser.push_line(second, &mut out).unwrap();
        assert_eq!(
            out,
            vec![
                StreamEvent::TextDelta("a".into()),
                StreamEvent::TextDelta("b".into())
            ],
            "the second line extends the same buffer"
        );
    }

    #[test]
    fn parses_a_thinking_signature_delta() {
        let mut parser = StreamParser::new();
        let line =
            br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#;
        assert_eq!(
            parse_line(&mut parser, line),
            vec![StreamEvent::ThinkingSignature("abc".into())]
        );
    }

    #[test]
    fn parses_redacted_thinking() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#;
        assert_eq!(
            parse_line(&mut parser, line),
            vec![StreamEvent::RedactedThinking("opaque".into())]
        );
    }

    #[test]
    fn ignores_non_data_lines() {
        let mut parser = StreamParser::new();
        assert!(parse_line(&mut parser, b"event: message_stop").is_empty());
        assert!(parse_line(&mut parser, b": comment").is_empty());
        assert!(parse_line(&mut parser, b"data:").is_empty());
    }

    #[test]
    fn a_malformed_data_line_is_an_error() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let error = parser.push_line(b"data: {not json", &mut out).unwrap_err();
        assert!(matches!(error, Error::Json(_)));
    }

    #[test]
    fn a_retryable_error_event_is_transient() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let line =
            br#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let error = parser.push_line(line, &mut out).unwrap_err();
        assert!(matches!(error, Error::Transient(message) if message == "Overloaded"));
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let line = br#"data: {"type":"error","error":{"type":"api_error","message":"Boom"}}"#;
        let error = parser.push_line(line, &mut out).unwrap_err();
        assert!(matches!(error, Error::Transient(message) if message == "Boom"));
    }

    #[test]
    fn a_non_retryable_error_event_is_a_protocol_error() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let line =
            br#"data: {"type":"error","error":{"type":"invalid_request_error","message":"No"}}"#;
        let error = parser.push_line(line, &mut out).unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message == "No"));
    }

    #[test]
    fn missing_message_stop_is_an_error() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        parser
            .push_line(
                br#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}"#,
                &mut out,
            )
            .unwrap();
        let error = parser.finish().unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message.contains("message_stop")));
    }
    #[test]
    fn a_known_event_without_its_payload_is_an_error() {
        for line in [
            &br#"data: {"type":"message_start"}"#[..],
            br#"data: {"type":"content_block_start","index":0}"#,
            br#"data: {"type":"content_block_delta","index":0}"#,
        ] {
            let mut parser = StreamParser::new();
            let mut out = Vec::new();
            let error = parser.push_line(line, &mut out).unwrap_err();
            assert!(matches!(error, Error::Protocol(_)), "{error:?}");
            assert!(parser.finish().is_ok(), "one error per stream");
        }
    }
}
