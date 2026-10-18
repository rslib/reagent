//! Parses the OpenAI Responses SSE stream.

use std::borrow::Cow;

use serde::Deserialize;

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

    /// Feeds one SSE line and returns the events it carries.
    pub fn push_line(&mut self, line: &[u8]) -> Result<Vec<StreamEvent>, Error> {
        if self.failed {
            return Ok(Vec::new());
        }
        let Some(data) = data_of(line) else {
            return Ok(Vec::new());
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

        let mut out = Vec::new();
        match event {
            Event::OutputItemAdded { item } => {
                if item.kind == "function_call" {
                    let call_id: Text = item.call_id.unwrap_or_default().into();
                    let name: Text = item.name.unwrap_or_default().into();
                    if let Some(id) = item.id {
                        self.remember(id, &call_id);
                    }
                    self.saw_tool_call = true;
                    out.push(StreamEvent::ToolCallStart { id: call_id, name });
                }
            }
            Event::OutputItemDone { item } => {
                if item.kind == "function_call" {
                    let call_id = self
                        .call_id_for(item.id)
                        .unwrap_or_else(|| Text::from(item.call_id.unwrap_or_default()));
                    self.saw_tool_call = true;
                    out.push(StreamEvent::ToolCallEnd { id: call_id });
                } else if item.kind == "reasoning"
                    && let Some(encrypted) = item.encrypted_content
                {
                    out.push(StreamEvent::ThinkingSignature(super::pack_reasoning(
                        item.id, encrypted,
                    )));
                }
            }
            Event::OutputTextDelta { delta } => {
                out.push(StreamEvent::TextDelta(Text::from(delta.as_ref())));
            }
            Event::ReasoningSummaryTextDelta { delta } => {
                out.push(StreamEvent::ThinkingDelta(Text::from(delta.as_ref())));
            }
            Event::FunctionCallArgumentsDelta { item_id, delta } => {
                let id = self.call_id_for(Some(item_id)).unwrap_or_default();
                out.push(StreamEvent::ToolCallDelta {
                    id,
                    partial_json: Text::from(delta.as_ref()),
                });
            }
            Event::Completed { response } => {
                self.done = true;
                self.stop_reason = Some(if self.saw_tool_call {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                });
                if let Some(usage) = response.usage {
                    self.usage = usage_of(usage);
                }
                out.push(StreamEvent::Usage(self.usage));
            }
            Event::Incomplete { response } => {
                self.done = true;
                self.stop_reason = Some(incomplete_reason(
                    response
                        .incomplete_details
                        .as_ref()
                        .and_then(|details| details.reason.as_deref()),
                ));
                if let Some(usage) = response.usage {
                    self.usage = usage_of(usage);
                }
                out.push(StreamEvent::Usage(self.usage));
            }
            Event::Failed { response } => {
                let message = response
                    .error
                    .map(|error| error.message.into_owned())
                    .unwrap_or_else(|| "response failed".into());
                self.failed = true;
                return Err(Error::Protocol(message));
            }
            Event::Error { message } => {
                self.failed = true;
                return Err(Error::Protocol(message.into_owned()));
            }
            Event::Unknown => {}
        }
        Ok(out)
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

    fn call_id_for(&self, item_id: Option<&str>) -> Option<Text> {
        let item_id = item_id?;
        self.tool_ids
            .iter()
            .find(|(id, _)| id == item_id)
            .map(|(_, call_id)| call_id.clone())
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

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Event<'a> {
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        #[serde(borrow)]
        item: Item<'a>,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        #[serde(borrow)]
        item: Item<'a>,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        #[serde(borrow)]
        delta: Cow<'a, str>,
    },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        #[serde(borrow)]
        delta: Cow<'a, str>,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        #[serde(borrow)]
        item_id: &'a str,
        #[serde(borrow)]
        delta: Cow<'a, str>,
    },
    #[serde(rename = "response.completed")]
    Completed {
        #[serde(borrow)]
        response: ResponseObject<'a>,
    },
    #[serde(rename = "response.incomplete")]
    Incomplete {
        #[serde(borrow)]
        response: ResponseObject<'a>,
    },
    #[serde(rename = "response.failed")]
    Failed {
        #[serde(borrow)]
        response: ResponseObject<'a>,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(borrow)]
        message: Cow<'a, str>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct Item<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    #[serde(default, borrow)]
    id: Option<&'a str>,
    #[serde(default, borrow)]
    call_id: Option<&'a str>,
    #[serde(default, borrow)]
    name: Option<&'a str>,
    #[serde(default, borrow)]
    encrypted_content: Option<&'a str>,
}

#[derive(Deserialize)]
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
    reason: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct ApiError<'a> {
    #[serde(default, borrow)]
    message: Cow<'a, str>,
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
            events.extend(parser.push_line(line.as_bytes()).expect("fixture parses"));
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
        let events = parser.push_line(line).unwrap();
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
    fn a_failed_response_is_an_error() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"boom"}}}"#;
        let error = parser.push_line(line).unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message == "boom"));
    }

    #[test]
    fn an_error_event_stops_with_the_message() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"type":"error","code":"overloaded","message":"Overloaded"}"#;
        let error = parser.push_line(line).unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message == "Overloaded"));
    }

    #[test]
    fn missing_response_completed_is_an_error() {
        let mut parser = StreamParser::new();
        parser
            .push_line(
                br#"data: {"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":"hi"}"#,
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
        let error = parser
            .push_line(br#"data: {"type":"error","code":"overloaded","message":"boom"}"#)
            .unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message == "boom"));
        assert!(
            parser
                .push_line(
                    br#"data: {"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":"late"}"#,
                )
                .unwrap()
                .is_empty(),
            "no event follows an error"
        );
        assert_eq!(
            parser.finish().expect("finish is quiet after an error"),
            None
        );
    }

    #[test]
    fn ignores_non_data_lines() {
        let mut parser = StreamParser::new();
        assert!(
            parser
                .push_line(b"event: response.created")
                .unwrap()
                .is_empty()
        );
        assert!(parser.push_line(b": keep-alive").unwrap().is_empty());
        assert!(parser.push_line(b"data:").unwrap().is_empty());
    }

    #[test]
    fn a_malformed_data_line_is_an_error() {
        let mut parser = StreamParser::new();
        let error = parser.push_line(b"data: {not json").unwrap_err();
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
                events.extend(parser.push_line(line).expect("chunk parses"));
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
}
