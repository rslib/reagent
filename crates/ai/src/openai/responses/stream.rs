//! Parses the OpenAI Responses SSE stream.

use serde::Deserialize;

use crate::openai::JsonStr;
use crate::types::StopReason;
use crate::{Error, StreamEvent, Text, Usage};

/// Parses one Responses SSE stream, line by line.
///
/// Event fields borrow from the line handed to [`StreamParser::push_line`], and
/// only the events it returns own their text. Function call deltas are keyed on
/// the output item id and mapped to the provider's `call_id`.
#[derive(Debug, Default)]
pub struct StreamParser {
    usage: Usage,
    tool_ids: Vec<(String, Text)>,
    saw_tool_call: bool,
    stop_reason: Option<StopReason>,
    done: bool,
    failed: bool,
}

impl StreamParser {
    /// Creates a parser at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one SSE line and appends the events it carries to `out`.
    pub fn push_line(&mut self, line: &[u8], out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        if self.failed {
            return Ok(());
        }
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
            "response.output_item.added" => {
                if let Some(item) = event.item
                    && item.kind == "function_call"
                {
                    let call_id: Text = item.call_id.unwrap_or_default().as_str().into();
                    let name: Text = item.name.unwrap_or_default().as_str().into();
                    if let Some(id) = item.id.as_ref() {
                        self.remember(id.as_str(), &call_id);
                    }
                    self.saw_tool_call = true;
                    out.push(StreamEvent::ToolCallStart { id: call_id, name });
                }
                Ok(())
            }
            "response.output_item.done" => {
                if let Some(item) = event.item {
                    if item.kind == "function_call" {
                        let call_id = self
                            .call_id_for(item.id.as_ref().map(JsonStr::as_str))
                            .unwrap_or_else(|| {
                                Text::from(item.call_id.unwrap_or_default().as_str())
                            });
                        self.saw_tool_call = true;
                        out.push(StreamEvent::ToolCallEnd { id: call_id });
                    } else if item.kind == "reasoning"
                        && let Some(encrypted) = item.encrypted_content.as_ref()
                    {
                        out.push(StreamEvent::ThinkingSignature(super::pack_reasoning(
                            item.id.as_ref().map(JsonStr::as_str),
                            encrypted.as_str(),
                        )));
                    }
                }
                Ok(())
            }
            "response.output_text.delta" => {
                let Some(delta) = event.delta else {
                    return Err(self.missing("response.output_text.delta", "delta"));
                };
                out.push(StreamEvent::TextDelta(Text::from(delta.as_str())));
                Ok(())
            }
            "response.reasoning_summary_text.delta" => {
                let Some(delta) = event.delta else {
                    return Err(self.missing("response.reasoning_summary_text.delta", "delta"));
                };
                out.push(StreamEvent::ThinkingDelta(Text::from(delta.as_str())));
                Ok(())
            }
            "response.function_call_arguments.delta" => {
                const KIND: &str = "response.function_call_arguments.delta";
                let Some(delta) = event.delta else {
                    return Err(self.missing(KIND, "delta"));
                };
                if event.item_id.is_none() {
                    return Err(self.missing(KIND, "item_id"));
                }
                let id = self.call_id_for(event.item_id).unwrap_or_default();
                out.push(StreamEvent::ToolCallDelta {
                    id,
                    partial_json: Text::from(delta.as_str()),
                });
                Ok(())
            }
            "response.completed" => {
                self.done = true;
                self.stop_reason = Some(if self.saw_tool_call {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                });
                if let Some(usage) = event.response.usage {
                    self.usage = usage_of(usage);
                }
                out.push(StreamEvent::Usage(self.usage));
                Ok(())
            }
            "response.incomplete" => {
                self.done = true;
                self.stop_reason = Some(incomplete_reason(
                    event
                        .response
                        .incomplete_details
                        .as_ref()
                        .and_then(|details| details.reason),
                ));
                if let Some(usage) = event.response.usage {
                    self.usage = usage_of(usage);
                }
                out.push(StreamEvent::Usage(self.usage));
                Ok(())
            }
            "response.failed" => {
                let (message, transient) = match event.response.error {
                    Some(error) => {
                        let kind = error.kind.or(error.code);
                        (
                            error.message.as_str().to_string(),
                            kind.is_some_and(retryable),
                        )
                    }
                    None => ("response failed".into(), false),
                };
                self.failed = true;
                if transient {
                    Err(Error::Transient(message))
                } else {
                    Err(Error::Protocol(message))
                }
            }
            "error" => {
                self.failed = true;
                let message = event
                    .message
                    .as_ref()
                    .map(JsonStr::as_str)
                    .unwrap_or("stream error");
                if event.code.is_some_and(retryable) {
                    Err(Error::Transient(message.to_string()))
                } else {
                    Err(Error::Protocol(message.to_string()))
                }
            }
            _ => Ok(()),
        }
    }

    /// Ends the stream after an error from outside the parser, such as HTTP.
    pub fn fail(&mut self) {
        self.failed = true;
    }

    /// Returns the stop event, or an error when the final event was missing.
    ///
    /// After an earlier error the stream is over and this returns `None`
    /// instead of a second error.
    pub fn finish(&mut self) -> Result<Option<StreamEvent>, Error> {
        if self.failed {
            return Ok(None);
        }
        if !self.done {
            return Err(Error::Protocol(
                "stream ended before response.completed".into(),
            ));
        }
        Ok(Some(StreamEvent::Stop(
            self.stop_reason.take().unwrap_or(StopReason::EndTurn),
        )))
    }
    fn remember(&mut self, item_id: &str, call_id: &Text) {
        if !self.tool_ids.iter().any(|(id, _)| id == item_id) {
            self.tool_ids.push((item_id.to_string(), call_id.clone()));
        }
    }

    /// Fails the stream for a known event that lacks a field it needs.
    fn missing(&mut self, kind: &str, field: &str) -> Error {
        self.failed = true;
        Error::Protocol(format!("{kind} event without {field}"))
    }

    fn call_id_for(&self, item_id: Option<&str>) -> Option<Text> {
        let item_id = item_id?;
        self.tool_ids
            .iter()
            .find(|(id, _)| id == item_id)
            .map(|(_, call_id)| call_id.clone())
    }
}

impl crate::openai::EventParser for StreamParser {
    fn push_line(&mut self, line: &[u8], out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        StreamParser::push_line(self, line, out)
    }

    fn fail(&mut self) {
        StreamParser::fail(self);
    }

    /// Appends the stop event the parser held back for the end of the stream.
    fn finish(&mut self, out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        if let Some(event) = StreamParser::finish(self)? {
            out.push(event);
        }
        Ok(())
    }
}

fn incomplete_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("max_output_tokens") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        _ => StopReason::Error,
    }
}

fn usage_of(json: UsageJson) -> Usage {
    let cached = json.input_tokens_details.cached_tokens;
    Usage {
        input_tokens: json.input_tokens.saturating_sub(cached),
        output_tokens: json.output_tokens,
        cache_read_tokens: cached,
        cache_write_tokens: 0,
        reasoning_tokens: json.output_tokens_details.reasoning_tokens,
    }
}

fn data_of(line: &[u8]) -> Option<&[u8]> {
    let rest = line.strip_prefix(b"data:")?;
    let rest = rest.strip_prefix(b" ").unwrap_or(rest);
    (!rest.is_empty()).then_some(rest)
}

/// One Responses event, flattened so the parser buffers no field of it.
///
/// An internally tagged enum makes serde hold the whole event and read the
/// fields back out, which allocates a map per event. Dispatching on the type
/// here keeps every field borrowed from the line instead.
#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    #[serde(default, borrow)]
    item: Option<Item<'a>>,
    #[serde(default, borrow)]
    delta: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    item_id: Option<&'a str>,
    #[serde(default, borrow)]
    response: ResponseObject<'a>,
    #[serde(default, borrow)]
    message: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    code: Option<&'a str>,
}

#[derive(Deserialize)]
struct Item<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    #[serde(default, borrow)]
    id: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    call_id: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    name: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    encrypted_content: Option<JsonStr<'a>>,
}

#[derive(Default, Deserialize)]
struct ResponseObject<'a> {
    #[serde(default)]
    usage: Option<UsageJson>,
    #[serde(default, borrow)]
    incomplete_details: Option<IncompleteDetails<'a>>,
    #[serde(default, borrow)]
    error: Option<ApiError<'a>>,
}

#[derive(Deserialize)]
struct IncompleteDetails<'a> {
    #[serde(default, borrow)]
    reason: Option<&'a str>,
}

#[derive(Deserialize)]
struct ApiError<'a> {
    #[serde(default, borrow)]
    message: JsonStr<'a>,
    #[serde(default, borrow, rename = "type")]
    kind: Option<&'a str>,
    #[serde(default, borrow)]
    code: Option<&'a str>,
}

/// Whether an OpenAI Responses stream error is safe to retry: a server or
/// rate-limit family type or code is transient, everything else is not.
fn retryable(kind: &str) -> bool {
    let kind = kind.to_ascii_lowercase();
    kind.contains("server") || kind.contains("rate_limit") || kind.contains("overload")
}

#[derive(Default, Deserialize)]
struct UsageJson {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: TokenDetails,
    #[serde(default)]
    output_tokens_details: TokenDetails,
}

#[derive(Default, Deserialize)]
struct TokenDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    reasoning_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(fixture: &str) -> (Vec<StreamEvent>, Result<Option<StreamEvent>, Error>) {
        let mut parser = StreamParser::new();
        let mut events = Vec::new();
        for line in fixture.lines() {
            parser
                .push_line(line.as_bytes(), &mut events)
                .expect("fixture parses");
        }
        let stop = parser.finish();
        (events, stop)
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
    fn parses_text_reasoning_and_usage() {
        let (events, stop) = parse(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_responses_text.sse"
        )));
        let (text, thinking, usage) = text_of(&events);
        assert_eq!(thinking, "Let me think.");
        assert_eq!(text, "Hello, world!");
        // The provider reports cached tokens inside input_tokens; input excludes them.
        assert_eq!(usage.input_tokens, 25);
        assert_eq!(usage.cache_read_tokens, 100);
        assert_eq!(usage.output_tokens, 12);
        assert_eq!(usage.reasoning_tokens, 5);
        assert_eq!(
            stop.unwrap().unwrap(),
            StreamEvent::Stop(StopReason::EndTurn)
        );
        assert!(events.contains(&StreamEvent::ThinkingSignature("rs_01\u{1f}enc_abc".into())));
    }

    #[test]
    fn parses_a_function_call_from_recorded_sse() {
        let (events, stop) = parse(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_responses_tool.sse"
        )));
        let mut arguments = String::new();
        let mut started = None;
        let mut ended = None;
        for event in &events {
            match event {
                StreamEvent::ToolCallStart { id, name } => {
                    started = Some((id.to_string(), name.to_string()));
                }
                StreamEvent::ToolCallDelta { partial_json, .. } => {
                    arguments.push_str(partial_json);
                }
                StreamEvent::ToolCallEnd { id } => ended = Some(id.to_string()),
                _ => {}
            }
        }
        assert_eq!(started, Some(("call_02".to_string(), "read".to_string())));
        assert_eq!(ended, Some("call_02".to_string()));
        assert_eq!(arguments, "{\"path\":\"a.rs\"}");
        assert_eq!(
            stop.unwrap().unwrap(),
            StreamEvent::Stop(StopReason::ToolUse)
        );
    }

    #[test]
    fn maps_an_incomplete_response_to_max_tokens() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":5,"output_tokens":9}}}"#;
        let mut events = Vec::new();
        parser.push_line(line, &mut events).unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::Usage(Usage {
                input_tokens: 5,
                output_tokens: 9,
                ..Usage::default()
            })]
        );
        assert_eq!(
            parser.finish().unwrap().unwrap(),
            StreamEvent::Stop(StopReason::MaxTokens)
        );
    }

    #[test]
    fn a_retryable_failed_response_is_transient() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"boom"}}}"#;
        let mut out = Vec::new();
        let error = parser.push_line(line, &mut out).unwrap_err();
        assert!(matches!(error, Error::Transient(message) if message == "boom"));
    }

    #[test]
    fn a_non_retryable_failed_response_is_a_protocol_error() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"type":"response.failed","response":{"status":"failed","error":{"type":"invalid_request","message":"no"}}}"#;
        let mut out = Vec::new();
        let error = parser.push_line(line, &mut out).unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message == "no"));
    }

    #[test]
    fn a_retryable_error_event_is_transient() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"type":"error","code":"overloaded","message":"Overloaded"}"#;
        let mut out = Vec::new();
        let error = parser.push_line(line, &mut out).unwrap_err();
        assert!(matches!(error, Error::Transient(message) if message == "Overloaded"));
    }

    #[test]
    fn missing_response_completed_is_an_error() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        parser
            .push_line(
                br#"data: {"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":"hi"}"#,
                &mut out,
            )
            .unwrap();
        let error = parser.finish().unwrap_err();
        assert!(
            matches!(error, Error::Protocol(message) if message.contains("response.completed"))
        );
    }

    #[test]
    fn an_error_ends_the_stream() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let error = parser
            .push_line(
                br#"data: {"type":"error","code":"overloaded","message":"boom"}"#,
                &mut out,
            )
            .unwrap_err();
        assert!(matches!(error, Error::Transient(message) if message == "boom"));
        let mut late = Vec::new();
        parser
            .push_line(
                br#"data: {"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":"late"}"#,
                &mut late,
            )
            .unwrap();
        assert!(late.is_empty(), "no event follows an error");
        assert_eq!(
            parser.finish().expect("finish is quiet after an error"),
            None
        );
    }

    #[test]
    fn push_line_appends_to_the_callers_buffer() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let first = br#"data: {"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":"a"}"#;
        let second = br#"data: {"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":"b"}"#;
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
    fn ignores_non_data_lines() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        parser
            .push_line(b"event: response.created", &mut out)
            .unwrap();
        parser.push_line(b": keep-alive", &mut out).unwrap();
        parser.push_line(b"data:", &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn a_malformed_data_line_is_an_error() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let error = parser.push_line(b"data: {not json", &mut out).unwrap_err();
        assert!(matches!(error, Error::Json(_)));
    }

    #[test]
    fn parses_chunked_bytes_through_the_splitter() {
        let chunks: Vec<&[u8]> = vec![
            b"event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",",
            b"\"item_id\":\"m\",\"delta\":\"Hi\"}\n",
            b"\n",
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n",
        ];

        let mut splitter = crate::sse::SseSplitter::new();
        let mut parser = StreamParser::new();
        let mut events = Vec::new();
        for chunk in chunks {
            let chunk = bytes::Bytes::copy_from_slice(chunk);
            let mut lines = splitter.push(&chunk);
            while let Some(line) = lines.next_line() {
                parser.push_line(line, &mut events).expect("chunk parses");
            }
        }
        let stop = parser.finish().expect("response.completed was seen");

        assert!(events.contains(&StreamEvent::TextDelta("Hi".into())));
        assert_eq!(stop.unwrap(), StreamEvent::Stop(StopReason::EndTurn));
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
    fn a_known_event_without_its_payload_is_an_error() {
        for line in [
            &br#"data: {"type":"response.output_text.delta"}"#[..],
            br#"data: {"type":"response.reasoning_summary_text.delta"}"#,
            br#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1"}"#,
            br#"data: {"type":"response.function_call_arguments.delta","delta":"{}"}"#,
        ] {
            let mut parser = StreamParser::new();
            let mut out = Vec::new();
            let error = parser.push_line(line, &mut out).unwrap_err();
            assert!(matches!(error, Error::Protocol(_)), "{error:?}");
            assert!(matches!(parser.finish(), Ok(None)), "one error per stream");
        }
    }
}
