//! Parses the pi-messages event stream.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::Error;
use crate::StreamEvent;
use crate::openai::{EventParser, JsonStr};
use crate::types::{Reasoning, StopReason, Text, Usage};

/// Parses `data:` lines of one pi-messages reply into events.
///
/// The replay object is written once, when `done` arrives, from the signatures
/// the reply carried and the ids the turn started with.
#[derive(Debug)]
pub struct StreamParser {
    /// The reasoning level the request asked for, kept for a `done` line that
    /// does not name one.
    requested: Option<Text>,
    response_id: Option<Text>,
    thinking_level: Option<Text>,
    /// The `contentSignature` of each text block, by text block ordinal.
    text_signatures: Vec<(u32, Text)>,
    /// The `thoughtSignature` of each tool call, by call id.
    tool_signatures: Vec<(Text, Text)>,
    /// The id of each tool call, by the `contentIndex` of the turn.
    calls: Vec<(Option<u32>, Text)>,
    /// Text blocks closed so far, which numbers the next `text_end`.
    texts: u32,
    /// Whether the block being written arrived with a delta of its own. A
    /// block that streamed none sends its whole text on the line that ends it.
    text_deltas: bool,
    thinking_deltas: bool,
    stop: StopReason,
    done: bool,
    failed: bool,
}

impl StreamParser {
    /// A parser for a request that asked for `reasoning`, in the provider's
    /// own words.
    pub fn new(reasoning: Option<&str>) -> Self {
        Self {
            requested: level_of(reasoning).map(Text::from),
            response_id: None,
            thinking_level: None,
            text_signatures: Vec::new(),
            tool_signatures: Vec::new(),
            calls: Vec::new(),
            texts: 0,
            text_deltas: false,
            thinking_deltas: false,
            stop: StopReason::EndTurn,
            done: false,
            failed: false,
        }
    }

    /// The replay object for the reply, or nothing when it carried no field to
    /// send back.
    fn replay(&self) -> Option<Text> {
        let signatures = self.response_id.is_none()
            && self.thinking_level.is_none()
            && self.text_signatures.is_empty()
            && self.tool_signatures.is_empty();
        if signatures {
            return None;
        }
        let text_signatures: BTreeMap<u32, &str> = self
            .text_signatures
            .iter()
            .map(|(ordinal, signature)| (*ordinal, &**signature))
            .collect();
        let tool_signatures: BTreeMap<&str, &str> = self
            .tool_signatures
            .iter()
            .map(|(id, signature)| (&**id, &**signature))
            .collect();
        let replay = ReplayOut {
            response_id: self.response_id.as_deref(),
            thinking_level: self.thinking_level.as_deref(),
            text_signatures,
            tool_signatures,
        };
        Some(Text::from(
            serde_json::to_string(&replay).expect("the replay object serializes"),
        ))
    }

    /// Appends the event one parsed line carries.
    fn apply(&mut self, event: Event<'_>, out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        match event.kind {
            "text_start" => self.text_deltas = false,
            "thinking_start" => self.thinking_deltas = false,
            "text_delta" => {
                self.text_deltas = true;
                if let Some(delta) = text_of(event.delta) {
                    out.push(StreamEvent::TextDelta(delta.into_text()));
                }
            }
            "thinking_delta" => {
                self.thinking_deltas = true;
                if let Some(delta) = text_of(event.delta) {
                    out.push(StreamEvent::ThinkingDelta(delta.into_text()));
                }
            }
            "text_end" => {
                // The content of an end line is what the block holds, so a
                // block that sent no delta is not lost.
                if !self.text_deltas
                    && let Some(content) = event.content
                {
                    out.push(StreamEvent::TextDelta(content.into_text()));
                }
                self.text_deltas = false;
                let ordinal = self.texts;
                self.texts += 1;
                if let Some(signature) = event.content_signature {
                    self.text_signatures.push((ordinal, signature.into_text()));
                }
            }
            "thinking_end" => {
                let streamed = self.thinking_deltas;
                self.thinking_deltas = false;
                let Some(signature) = event.content_signature else {
                    return Ok(());
                };
                if event.redacted == Some(true) {
                    out.push(StreamEvent::RedactedThinking(signature.into_text()));
                } else {
                    if !streamed && let Some(content) = event.content {
                        out.push(StreamEvent::ThinkingDelta(content.into_text()));
                    }
                    out.push(StreamEvent::ThinkingSignature(signature.into_text()));
                }
            }
            "toolcall_start" => {
                let Some(id) = event.id else {
                    return Ok(());
                };
                let id = id.into_text();
                let name = event.tool_name.map(JsonStr::into_text).unwrap_or_default();
                self.calls.push((event.content_index, id.clone()));
                out.push(StreamEvent::ToolCallStart { id, name });
            }
            "toolcall_end" => {
                let Some(call) = event.tool_call else {
                    return Ok(());
                };
                let id = match call.id {
                    Some(id) => id.into_text(),
                    None => self
                        .calls
                        .iter()
                        .find(|(index, _)| *index == event.content_index)
                        .map_or_else(Text::default, |(_, id)| id.clone()),
                };
                if let Some(arguments) = call.arguments {
                    out.push(StreamEvent::ToolCallDelta {
                        id: id.clone(),
                        partial_json: Text::from(arguments.get()),
                    });
                }
                if let Some(signature) = call.thought_signature {
                    self.tool_signatures
                        .push((id.clone(), signature.into_text()));
                }
                out.push(StreamEvent::ToolCallEnd { id });
            }
            "done" => {
                self.done = true;
                self.stop = stop_of(event.reason);
                self.response_id = event.response_id.map(JsonStr::into_text);
                self.thinking_level = event
                    .provider_thinking_level
                    .map(JsonStr::into_text)
                    .or_else(|| self.requested.clone());
                if let Some(usage) = event.usage {
                    out.push(StreamEvent::Usage(usage.into_usage()));
                }
                if let Some(replay) = self.replay() {
                    out.push(StreamEvent::Replay(replay));
                }
                out.push(StreamEvent::Stop(self.stop));
            }
            "error" => {
                self.failed = true;
                let message = match event.error_message.as_ref().map(JsonStr::as_str) {
                    Some(message) if !message.is_empty() => message.to_string(),
                    _ => FAILED.to_string(),
                };
                return Err(if event.reason == Some("aborted") {
                    Error::Protocol(message)
                } else {
                    Error::Transient(message)
                });
            }
            _ => {}
        }
        Ok(())
    }
}

impl StreamParser {
    /// Feeds one SSE line and appends the events it carries to `out`.
    pub fn push_line(&mut self, line: &[u8], out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        let Some(data) = data_of(line) else {
            return Ok(());
        };
        // A proxy may close the stream with the OpenAI sentinel; the done line
        // is what ends a reply.
        if data == b"[DONE]" {
            return Ok(());
        }
        let event: Event<'_> = serde_json::from_slice(data).map_err(Error::Json)?;
        self.apply(event, out)
    }

    /// Marks the parser failed after an error from outside it, such as HTTP.
    pub fn fail(&mut self) {
        self.failed = true;
    }

    /// Appends the events left after the byte stream ended.
    pub fn finish(&mut self, _out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        if self.done || self.failed {
            return Ok(());
        }
        self.failed = true;
        Err(Error::Transient("stream ended before done".into()))
    }
}

impl EventParser for StreamParser {
    fn push_line(&mut self, line: &[u8], out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        StreamParser::push_line(self, line, out)
    }

    fn fail(&mut self) {
        StreamParser::fail(self);
    }

    fn finish(&mut self, out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        StreamParser::finish(self, out)
    }
}

/// The reasoning level a request sends, at the provider's own word: `None` for
/// a budget and for the word that turns reasoning off.
pub(crate) fn level_of(reasoning: Option<&str>) -> Option<&str> {
    let reasoning = Reasoning::parse(reasoning?);
    match reasoning {
        Reasoning::Level(level) if !reasoning.is_off() => Some(level),
        Reasoning::Level(_) | Reasoning::Budget(_) => None,
    }
}

/// The payload of a `data:` line, or nothing for a comment, a blank line or
/// another SSE field.
fn data_of(line: &[u8]) -> Option<&[u8]> {
    let rest = line.strip_prefix(b"data:")?;
    let rest = rest.strip_prefix(b" ").unwrap_or(rest);
    (!rest.is_empty()).then_some(rest)
}

fn stop_of(reason: Option<&str>) -> StopReason {
    match reason {
        Some("toolUse") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("error") => StopReason::Error,
        _ => StopReason::EndTurn,
    }
}

/// One pi-messages event, flattened so the parser buffers no field of it.
#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    /// The text of a delta, as the JSON string it arrived as: a tool call
    /// argument delta is never read, so it is never unescaped.
    #[serde(default, borrow)]
    delta: Option<&'a RawValue>,
    /// The content an end line carries, which is what that block holds.
    #[serde(default)]
    content: Option<JsonStr<'a>>,
    #[serde(default, rename = "contentIndex")]
    content_index: Option<u32>,
    #[serde(default, rename = "contentSignature", borrow)]
    content_signature: Option<JsonStr<'a>>,
    #[serde(default)]
    redacted: Option<bool>,
    #[serde(default, borrow)]
    id: Option<JsonStr<'a>>,
    #[serde(default, rename = "toolName", borrow)]
    tool_name: Option<JsonStr<'a>>,
    #[serde(default, rename = "toolCall", borrow)]
    tool_call: Option<ToolCall<'a>>,
    #[serde(default, borrow)]
    reason: Option<&'a str>,
    #[serde(default)]
    usage: Option<UsageIn>,
    #[serde(default, rename = "responseId", borrow)]
    response_id: Option<JsonStr<'a>>,
    #[serde(default, rename = "providerThinkingLevel", borrow)]
    provider_thinking_level: Option<JsonStr<'a>>,
    #[serde(default, rename = "errorMessage", borrow)]
    error_message: Option<JsonStr<'a>>,
}

/// The message of an `error` line that names none.
const FAILED: &str = "Radius request failed";

/// The text a delta line carries, borrowed when the line wrote it verbatim.
fn text_of(delta: Option<&RawValue>) -> Option<JsonStr<'_>> {
    serde_json::from_str(delta?.get()).ok()
}

/// The finished tool call a `toolcall_end` line carries.
#[derive(Deserialize)]
struct ToolCall<'a> {
    #[serde(default, borrow)]
    id: Option<JsonStr<'a>>,
    #[serde(default, rename = "thoughtSignature", borrow)]
    thought_signature: Option<JsonStr<'a>>,
    #[serde(default)]
    arguments: Option<&'a RawValue>,
}

/// The token counts one `done` line carries.
#[derive(Deserialize)]
struct UsageIn {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default, rename = "cacheRead")]
    cache_read: u64,
    #[serde(default, rename = "cacheWrite")]
    cache_write: u64,
    #[serde(default)]
    reasoning: Option<u64>,
    #[serde(default)]
    cost: Option<CostIn>,
}

impl UsageIn {
    fn into_usage(self) -> Usage {
        Usage {
            input_tokens: self.input,
            output_tokens: self.output,
            cache_read_tokens: self.cache_read,
            cache_write_tokens: self.cache_write,
            reasoning_tokens: self.reasoning.unwrap_or(0),
            cost: self.cost.and_then(|cost| cost.total),
        }
    }
}

/// The cost one `done` line reports, in USD.
#[derive(Deserialize)]
struct CostIn {
    #[serde(default)]
    total: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseSplitter;
    use bytes::Bytes;

    /// One reply: keepalives, thinking with a signature and a redacted block,
    /// text with its signature, a tool call that starts, streams and ends, and
    /// the done line that closes it.
    const FIXTURE: &str = "\
: keepalive\n\
event: message\n\
data: {\"type\":\"thinking_delta\",\"contentIndex\":0,\"delta\":\"Let me look.\"}\n\
data: {\"type\":\"thinking_end\",\"contentIndex\":0,\"contentSignature\":\"think-sig\"}\n\
data: {\"type\":\"thinking_end\",\"contentIndex\":1,\"contentSignature\":\"opaque\",\"redacted\":true}\n\
data: {\"type\":\"text_delta\",\"contentIndex\":2,\"delta\":\"I will read \"}\n\
data: {\"type\":\"text_delta\",\"contentIndex\":2,\"delta\":\"a.txt.\"}\n\
data: {\"type\":\"text_end\",\"contentIndex\":2,\"contentSignature\":\"text-sig\"}\n\
data: {\"type\":\"toolcall_start\",\"contentIndex\":3,\"id\":\"call_1\",\"toolName\":\"read\"}\n\
data: {\"type\":\"toolcall_delta\",\"contentIndex\":3,\"delta\":\"{\\\"path\\\":\\\"a\"}\n\
data: {\"type\":\"toolcall_delta\",\"contentIndex\":3,\"delta\":\".txt\\\"}\"}\n\
data: {\"type\":\"toolcall_end\",\"contentIndex\":3,\"toolCall\":{\"id\":\"call_1\",\"name\":\"read\",\"arguments\":{\"path\":\"a.txt\"},\"thoughtSignature\":\"tool-sig\"}}\n\
data: {\"type\":\"done\",\"reason\":\"toolUse\",\"usage\":{\"input\":100,\"output\":20,\"cacheRead\":30,\"cacheWrite\":40,\"totalTokens\":190,\"reasoning\":5,\"cost\":{\"input\":0.1,\"output\":0.2,\"cacheRead\":0.01,\"cacheWrite\":0.02,\"total\":0.33}},\"responseId\":\"r-1\",\"providerThinkingLevel\":\"high\"}\n\
data: [DONE]\n\n";

    /// The events of `FIXTURE`, in order.
    fn golden() -> Vec<StreamEvent> {
        vec![
            StreamEvent::ThinkingDelta(Text::from("Let me look.")),
            StreamEvent::ThinkingSignature(Text::from("think-sig")),
            StreamEvent::RedactedThinking(Text::from("opaque")),
            StreamEvent::TextDelta(Text::from("I will read ")),
            StreamEvent::TextDelta(Text::from("a.txt.")),
            StreamEvent::ToolCallStart {
                id: Text::from("call_1"),
                name: Text::from("read"),
            },
            StreamEvent::ToolCallDelta {
                id: Text::from("call_1"),
                partial_json: Text::from("{\"path\":\"a.txt\"}"),
            },
            StreamEvent::ToolCallEnd {
                id: Text::from("call_1"),
            },
            StreamEvent::Usage(Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 30,
                cache_write_tokens: 40,
                reasoning_tokens: 5,
                cost: Some(0.33),
            }),
            StreamEvent::Replay(Text::from(
                "{\"responseId\":\"r-1\",\"providerThinkingLevel\":\"high\",\
                 \"textSignatures\":{\"0\":\"text-sig\"},\
                 \"toolSignatures\":{\"call_1\":\"tool-sig\"}}",
            )),
            StreamEvent::Stop(StopReason::ToolUse),
        ]
    }

    /// Feeds `body` through the splitter and the parser, one line at a time.
    fn events(req: Option<&str>, body: &str) -> Vec<StreamEvent> {
        let mut parser = StreamParser::new(req);
        let mut splitter = SseSplitter::new();
        let mut out = Vec::new();
        let chunk = Bytes::copy_from_slice(body.as_bytes());
        let mut lines = splitter.push(&chunk);
        while let Some(line) = lines.next_line() {
            parser
                .push_line(line, &mut out)
                .expect("the fixture parses");
        }
        if let Some(line) = splitter.finish() {
            parser
                .push_line(line, &mut out)
                .expect("the fixture parses");
        }
        parser.finish(&mut out).expect("the fixture ends with done");
        out
    }

    #[test]
    fn pi_stream_carries_text_thinking_and_one_tool_call() {
        assert_eq!(events(None, FIXTURE), golden());
    }

    #[test]
    fn pi_stream_unescapes_escaped_text() {
        let body = "data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"a\\nb\"}\n\
                    data: {\"type\":\"text_end\",\"contentIndex\":0,\"contentSignature\":\"a\\nb\"}\n\
                    data: {\"type\":\"done\",\"reason\":\"stop\"}\n";
        let events = events(None, body);
        assert_eq!(events[0], StreamEvent::TextDelta(Text::from("a\nb")));
        assert_eq!(
            events[1],
            StreamEvent::Replay(Text::from("{\"textSignatures\":{\"0\":\"a\\nb\"}}")),
            "a signature with an escape reaches the replay unescaped once"
        );
        assert_eq!(events[2], StreamEvent::Stop(StopReason::EndTurn));
    }

    #[test]
    fn pi_stream_ignores_unknown_fields_and_other_lines() {
        let body = "id: 1\n\
                    event: ping\n\
                    : keepalive\n\
                    data: {\"type\":\"future_event\",\"contentIndex\":9,\"extra\":{\"a\":[1,2]}}\n\
                    data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"hi\",\"extra\":true}\n\
                    data: {\"type\":\"done\",\"reason\":\"stop\",\"unknown\":{\"b\":1}}\n\
                    data: [DONE]\n";
        assert_eq!(
            events(None, body),
            vec![
                StreamEvent::TextDelta(Text::from("hi")),
                StreamEvent::Stop(StopReason::EndTurn),
            ],
            "a done line with nothing to replay emits no replay object"
        );
    }

    #[test]
    fn pi_stream_without_a_level_and_nothing_to_replay_has_no_replay() {
        let body = "data: {\"type\":\"done\",\"reason\":\"length\",\
                    \"usage\":{\"input\":1,\"output\":2,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":3}}\n";
        let events = events(None, body);
        assert_eq!(
            events,
            vec![
                StreamEvent::Usage(Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    reasoning_tokens: 0,
                    cost: None,
                }),
                StreamEvent::Stop(StopReason::MaxTokens),
            ]
        );
    }

    #[test]
    fn pi_stream_stores_the_requested_level_when_done_omits_it() {
        let body = "data: {\"type\":\"done\",\"reason\":\"stop\"}\n";
        assert_eq!(
            events(Some("max"), body),
            vec![
                StreamEvent::Replay(Text::from("{\"providerThinkingLevel\":\"max\"}")),
                StreamEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn pi_stream_cut_short_is_a_transient_error() {
        let mut parser = StreamParser::new(None);
        let mut out = Vec::new();
        parser
            .push_line(
                b"data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"hi\"}",
                &mut out,
            )
            .expect("the line parses");
        assert_eq!(out, vec![StreamEvent::TextDelta(Text::from("hi"))]);
        let error = parser.finish(&mut out).expect_err("the stream is cut");
        assert!(matches!(error, Error::Transient(_)), "{error:?}");
        assert!(error.retry_hint().is_some(), "a cut stream is retryable");
    }

    #[test]
    fn pi_stream_error_event_carries_its_message() {
        let mut parser = StreamParser::new(None);
        let mut out = Vec::new();
        let error = parser
            .push_line(
                b"data: {\"type\":\"error\",\"reason\":\"error\",\"errorMessage\":\"gateway overloaded\"}",
                &mut out,
            )
            .expect_err("the provider failed");
        assert!(
            matches!(&error, Error::Transient(text) if text == "gateway overloaded"),
            "{error:?}"
        );
        assert!(
            error.retry_hint().is_some(),
            "a provider error is retryable"
        );

        let mut parser = StreamParser::new(None);
        let error = parser
            .push_line(
                b"data: {\"type\":\"error\",\"reason\":\"aborted\",\"errorMessage\":\"the client gave up\"}",
                &mut out,
            )
            .expect_err("the request was aborted");
        assert!(
            matches!(&error, Error::Protocol(text) if text == "the client gave up"),
            "{error:?}"
        );
        assert!(
            error.retry_hint().is_none(),
            "an aborted request is not retryable"
        );
    }

    /// A block that arrives only as a start and an end is not empty: the
    /// authoritative end content leaves as the one delta of the block.
    #[test]
    fn pi_stream_emits_the_end_content_of_a_block_with_no_deltas() {
        let body = "data: {\"type\":\"thinking_start\",\"contentIndex\":0}\n\
                    data: {\"type\":\"thinking_end\",\"contentIndex\":0,\
                    \"content\":\"Considered.\",\"contentSignature\":\"think-sig\"}\n\
                    data: {\"type\":\"text_start\",\"contentIndex\":1}\n\
                    data: {\"type\":\"text_end\",\"contentIndex\":1,\
                    \"content\":\"Hello\",\"contentSignature\":\"text-sig\"}\n\
                    data: {\"type\":\"done\",\"reason\":\"stop\"}\n";
        assert_eq!(
            events(None, body),
            vec![
                StreamEvent::ThinkingDelta(Text::from("Considered.")),
                StreamEvent::ThinkingSignature(Text::from("think-sig")),
                StreamEvent::TextDelta(Text::from("Hello")),
                StreamEvent::Replay(Text::from("{\"textSignatures\":{\"0\":\"text-sig\"}}")),
                StreamEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    /// A block that streamed its text does not repeat it: the deltas are
    /// already in the message the caller built.
    #[test]
    fn pi_stream_does_not_repeat_the_content_of_a_block_with_deltas() {
        let body = "data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"Hel\"}\n\
                    data: {\"type\":\"text_end\",\"contentIndex\":0,\"content\":\"Hello\"}\n\
                    data: {\"type\":\"thinking_delta\",\"contentIndex\":1,\"delta\":\"Con\"}\n\
                    data: {\"type\":\"thinking_end\",\"contentIndex\":1,\
                    \"content\":\"Considered.\",\"redacted\":true,\"contentSignature\":\"opaque\"}\n\
                    data: {\"type\":\"done\",\"reason\":\"stop\"}\n";
        assert_eq!(
            events(None, body),
            vec![
                StreamEvent::TextDelta(Text::from("Hel")),
                StreamEvent::ThinkingDelta(Text::from("Con")),
                StreamEvent::RedactedThinking(Text::from("opaque")),
                StreamEvent::Stop(StopReason::EndTurn),
            ]
        );
    }

    /// An error event that names no message still says the request failed.
    #[test]
    fn pi_stream_error_without_a_message_says_the_request_failed() {
        for line in [
            "data: {\"type\":\"error\",\"reason\":\"error\"}",
            "data: {\"type\":\"error\",\"reason\":\"error\",\"errorMessage\":\"\"}",
        ] {
            let mut parser = StreamParser::new(None);
            let mut out = Vec::new();
            let error = parser
                .push_line(line.as_bytes(), &mut out)
                .expect_err("the provider failed");
            assert!(
                matches!(&error, Error::Transient(text) if text == "Radius request failed"),
                "{line}: {error:?}"
            );
            assert!(error.retry_hint().is_some(), "{line}: still retryable");
        }
    }

    /// A chunk boundary at any byte of the fixture changes no event.
    #[test]
    fn pi_stream_split_at_every_byte_gives_the_same_events() {
        let expected = golden();
        let bytes = FIXTURE.as_bytes();
        for at in 0..=bytes.len() {
            let mut parser = StreamParser::new(None);
            let mut splitter = SseSplitter::new();
            let mut out = Vec::new();
            for chunk in [&bytes[..at], &bytes[at..]] {
                let chunk = Bytes::copy_from_slice(chunk);
                let mut lines = splitter.push(&chunk);
                while let Some(line) = lines.next_line() {
                    parser
                        .push_line(line, &mut out)
                        .expect("the fixture parses");
                }
            }
            if let Some(line) = splitter.finish() {
                parser
                    .push_line(line, &mut out)
                    .expect("the fixture parses");
            }
            parser.finish(&mut out).expect("the fixture ends with done");
            assert_eq!(out, expected, "a chunk boundary at byte {at}");
        }
    }
}

/// The fields a reply needs back on the next turn, as one JSON object.
#[derive(Serialize)]
struct ReplayOut<'a> {
    #[serde(rename = "responseId", skip_serializing_if = "Option::is_none")]
    response_id: Option<&'a str>,
    #[serde(
        rename = "providerThinkingLevel",
        skip_serializing_if = "Option::is_none"
    )]
    thinking_level: Option<&'a str>,
    #[serde(rename = "textSignatures", skip_serializing_if = "BTreeMap::is_empty")]
    text_signatures: BTreeMap<u32, &'a str>,
    #[serde(rename = "toolSignatures", skip_serializing_if = "BTreeMap::is_empty")]
    tool_signatures: BTreeMap<&'a str, &'a str>,
}
