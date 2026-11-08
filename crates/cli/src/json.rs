//! JSON line rendering for `--json` mode and the worker's event messages.

use serde::Serialize;
use serde::ser::SerializeSeq;

use reagent_agent::event::{Event, TurnEndReason};
use reagent_agent::guard::Answer;
use reagent_agent::live::{ClientId, SendMode};
use reagent_ai::Usage;
use reagent_config::permissions::PermissionMode;

/// The version of the line format, named by the start line of every `--json`
/// run.
pub const JSON_VERSION: u32 = 1;

/// One `--json` line, borrowed from the event so no text is copied.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line<'a> {
    TurnStart,
    TextDelta {
        text: &'a str,
    },
    ThinkingDelta {
        text: &'a str,
    },
    ToolStart {
        id: &'a str,
        name: &'a str,
    },
    ToolCallReady {
        id: &'a str,
        name: &'a str,
        arguments: &'a str,
    },
    ToolResult {
        id: &'a str,
        is_error: bool,
        bytes: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a str>,
        /// The tail of the output, in the worker's frames only.
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<&'a str>,
        /// How many lines `output` was cut down from, in the worker's frames
        /// only.
        #[serde(skip_serializing_if = "Option::is_none")]
        earlier: Option<u64>,
        /// The images the result carries, never their base64 text.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageLine<'a>>,
    },
    Usage {
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cache_hit_rate: f64,
        cost: Option<f64>,
    },
    TurnEnd {
        reason: &'static str,
    },
    RunEnd {
        reason: &'static str,
    },
    Error {
        message: &'a str,
    },
    Retry {
        attempt: u32,
        delay_ms: u64,
        error: &'a str,
    },
    PolicyNote {
        text: &'a str,
    },
    Compacted {
        before: u64,
        after: u64,
        replaced: u32,
        bytes: u32,
    },
    Start {
        version: u32,
        reagent: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        session: Option<&'a str>,
    },
    UserMessage {
        client: ClientId,
        mode: &'static str,
        text: &'a str,
        /// The images that follow the text, never their base64 text.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageLine<'a>>,
    },
    Committed {
        record: u64,
    },
    ApprovalRequest {
        id: u64,
        tool: &'a str,
        call: &'a str,
        rule: &'a str,
        /// The rules a save answer would add.
        #[serde(skip_serializing_if = "Rules::is_empty")]
        save: Rules<'a>,
    },
    ApprovalResolved {
        id: u64,
        client: ClientId,
        answer: &'static str,
        /// The reason a refusal answers the model with, when it has one.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<&'a str>,
    },
    RuleToSave {
        rule: &'a str,
    },
    Settings {
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<&'static str>,
    },
    Summary {
        turns: u32,
        exit: &'static str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cache_hit_rate: f64,
        cost: Option<f64>,
        /// The prompt size of the last turn.
        context_tokens: u64,
        /// The model's context window, when it is known.
        context_window: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop: Option<&'static str>,
    },
}

/// The rules a save answer would add, borrowed from the request, as the JSON
/// array a client reads: empty when a save is not possible.
struct Rules<'a>(Option<&'a str>);

impl Rules<'_> {
    fn is_empty(&self) -> bool {
        self.0.is_none()
    }
}

impl Serialize for Rules<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut rules = serializer.serialize_seq(Some(self.0.is_some() as usize))?;
        if let Some(rule) = self.0 {
            rules.serialize_element(rule)?;
        }
        rules.end()
    }
}

/// One image of a tool result: what it is and how big, without its base64 text.
#[derive(Serialize)]
struct ImageLine<'a> {
    media_type: &'a str,
    width: u32,
    height: u32,
    /// The decoded byte count of the image.
    bytes: usize,
}

/// The lines of `images`: their type, size and decoded bytes.
fn image_lines(images: &[reagent_ai::Image]) -> Vec<ImageLine<'_>> {
    images
        .iter()
        .map(|image| ImageLine {
            media_type: image.media_type.mime(),
            width: image.width,
            height: image.height,
            bytes: decoded_len(image.data.as_ref()),
        })
        .collect()
}

/// Appends the `--json` line of one agent event to `buf`.
///
/// A live session's own events have no `--json` line: `-p --json` prints
/// exactly the lines it printed before they existed.
pub fn write_event(buf: &mut Vec<u8>, event: &Event) {
    if matches!(
        event,
        Event::UserMessage { .. }
            | Event::Committed { .. }
            | Event::ApprovalRequest(_)
            | Event::ApprovalResolved { .. }
            | Event::RuleToSave { .. }
            | Event::Settings { .. }
    ) {
        return;
    }
    write_line(buf, event);
}

/// Appends any agent event, a live session's own too, to `buf` as one JSON
/// object and a newline: the bytes `--json` prints.
pub fn write_line(buf: &mut Vec<u8>, event: &Event) {
    write(buf, event, false);
}

/// Appends any agent event, a live session's own too, to `buf` as one JSON
/// object and a newline: the bytes a worker sends its clients.
///
/// A tool result's frame carries the tail of its output, so a client draws the
/// text without reading the journal.
pub fn write_worker_line(buf: &mut Vec<u8>, event: &Event) {
    write(buf, event, true);
}

/// Appends `event` to `buf` as one JSON object and a newline, with the output
/// tail of a tool result when `tailed` is set.
fn write(buf: &mut Vec<u8>, event: &Event, tailed: bool) {
    // The deltas of a streamed answer are the hot path of `--json`: their whole
    // line is written straight into the buffer, with the bytes the serializer
    // writes, instead of going through the serializer once per delta.
    match event {
        Event::TextDelta(text) => return string_line(buf, "text_delta", "text", text),
        Event::ThinkingDelta(text) => return string_line(buf, "thinking_delta", "text", text),
        _ => {}
    }
    let line = match event {
        Event::TurnStart => Line::TurnStart,
        Event::TextDelta(text) => Line::TextDelta { text },
        Event::ThinkingDelta(text) => Line::ThinkingDelta { text },
        Event::ToolStart { id, name } => Line::ToolStart { id, name },
        Event::ToolCallReady {
            id,
            name,
            arguments,
        } => Line::ToolCallReady {
            id,
            name,
            arguments,
        },
        Event::ToolResult {
            id,
            is_error,
            bytes,
            output,
            images,
        } => {
            let (tail, earlier) = match (tailed, output.as_deref()) {
                (true, Some(text)) => {
                    let (tail, earlier) = tail_of(text);
                    (Some(tail), Some(earlier))
                }
                _ => (None, None),
            };
            Line::ToolResult {
                id,
                is_error: *is_error,
                bytes: *bytes,
                error: output.as_deref().filter(|_| *is_error),
                output: tail,
                earlier,
                images: image_lines(images),
            }
        }
        Event::Usage(usage) => Line::Usage {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_read: usage.cache_read_tokens,
            cache_write: usage.cache_write_tokens,
            cache_hit_rate: cache_hit_rate(usage),
            cost: usage.cost,
        },
        Event::TurnEnd(reason) => Line::TurnEnd {
            reason: turn_reason(reason),
        },
        Event::RunEnd(reason) => Line::RunEnd {
            reason: turn_reason(reason),
        },
        Event::Error(message) => Line::Error { message },
        Event::Retry {
            attempt,
            delay_ms,
            error,
        } => Line::Retry {
            attempt: *attempt,
            delay_ms: *delay_ms,
            error,
        },
        Event::PolicyNote(text) => Line::PolicyNote { text },
        Event::Compacted {
            before,
            after,
            replaced,
            bytes,
        } => Line::Compacted {
            before: *before,
            after: *after,
            replaced: *replaced,
            bytes: *bytes,
        },
        Event::UserMessage {
            client,
            mode,
            text,
            images,
        } => Line::UserMessage {
            client: *client,
            mode: send_mode(*mode),
            text,
            images: image_lines(images),
        },
        Event::Committed { record } => Line::Committed { record: *record },
        Event::ApprovalRequest(ask) => Line::ApprovalRequest {
            id: ask.id,
            tool: &ask.tool,
            call: &ask.call,
            rule: &ask.rule,
            save: Rules(ask.save.as_deref()),
        },
        Event::ApprovalResolved { id, client, answer } => {
            let (answer, reason) = answer_line(answer);
            Line::ApprovalResolved {
                id: *id,
                client: *client,
                answer,
                reason,
            }
        }
        Event::RuleToSave { rule } => Line::RuleToSave { rule },
        Event::Settings { model, level, mode } => Line::Settings {
            model: model.as_deref(),
            level: level.as_deref(),
            mode: mode.as_ref().map(PermissionMode::as_str),
        },
    };
    push(buf, &line);
}

/// Appends the line an `--json` run starts with: the format version, the
/// reagent version and the session the run writes, when it writes one.
pub fn write_start(buf: &mut Vec<u8>, session: Option<&str>) {
    push(
        buf,
        &Line::Start {
            version: JSON_VERSION,
            reagent: env!("CARGO_PKG_VERSION"),
            session,
        },
    );
}

/// The bytes of a tool result's output a worker keeps in the frame: its last
/// [`OUTPUT_KEEP`] bytes from the start of a line.
const OUTPUT_KEEP: usize = 4096;

/// The part of `text` a frame keeps: its last [`OUTPUT_KEEP`] bytes from the
/// start of a line, and the count of the lines before them.
///
/// A cut inside a line moves forward to the start of the next one, and a line
/// with no start in the kept part starts at a character boundary.
fn tail_of(text: &str) -> (&str, u64) {
    if text.len() <= OUTPUT_KEEP {
        return (text, 0);
    }
    let mut at = text.len() - OUTPUT_KEEP;
    if !text.is_char_boundary(at) {
        at = text.ceil_char_boundary(at);
    }
    if !text[..at].ends_with('\n')
        && let Some(end) = text[at..].find('\n')
    {
        at += end + 1;
    }
    (&text[at..], text[..at].matches('\n').count() as u64)
}

/// Appends the final summary line to `buf`: total usage, turns, the context
/// the last turn used and the stop reason.
pub fn write_summary(
    buf: &mut Vec<u8>,
    usage: &Usage,
    turns: u32,
    reason: &TurnEndReason,
    stop: Option<&'static str>,
    context_tokens: u64,
    context_window: Option<u64>,
) {
    let line = Line::Summary {
        turns,
        exit: turn_reason(reason),
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_tokens,
        cache_write: usage.cache_write_tokens,
        cache_hit_rate: cache_hit_rate(usage),
        cost: usage.cost,
        context_tokens,
        context_window,
        stop,
    };
    push(buf, &line);
}

fn push(buf: &mut Vec<u8>, line: &Line<'_>) {
    // A Vec writer cannot fail, and every field serializes.
    serde_json::to_writer(&mut *buf, line).expect("a json line serializes");
    buf.push(b'\n');
}

/// Appends the line of an event with one string field: `{"type":"<kind>",
/// "<name>":"<value>"}` and a newline, the same bytes the serializer writes.
fn string_line(buf: &mut Vec<u8>, kind: &str, name: &str, value: &str) {
    buf.extend_from_slice(b"{\"type\":\"");
    buf.extend_from_slice(kind.as_bytes());
    buf.extend_from_slice(b"\",\"");
    buf.extend_from_slice(name.as_bytes());
    buf.extend_from_slice(b"\":");
    write_string(buf, value);
    buf.extend_from_slice(b"}\n");
}

/// The lowercase hex digits of the `\u00XX` form.
const HEX: &[u8; 16] = b"0123456789abcdef";

/// Appends `value` as a JSON string: quoted, with the escapes `serde_json`
/// writes (`\" \\ \n \r \t \b \f` and `\u00XX` for the other control
/// bytes) and nothing else, so the bytes match the serializer's. A string with
/// nothing to escape is one copy, with no second pass.
fn write_string(buf: &mut Vec<u8>, value: &str) {
    buf.push(b'"');
    let bytes = value.as_bytes();
    // Where the run of plain bytes still to copy starts.
    let mut plain = 0;
    for (at, &byte) in bytes.iter().enumerate() {
        let short: &[u8] = match byte {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x08 => b"\\b",
            0x0c => b"\\f",
            0x00..=0x1f => {
                buf.extend_from_slice(&bytes[plain..at]);
                buf.extend_from_slice(b"\\u00");
                buf.push(HEX[(byte >> 4) as usize]);
                buf.push(HEX[(byte & 0xf) as usize]);
                plain = at + 1;
                continue;
            }
            _ => continue,
        };
        buf.extend_from_slice(&bytes[plain..at]);
        buf.extend_from_slice(short);
        plain = at + 1;
    }
    buf.extend_from_slice(&bytes[plain..]);
    buf.push(b'"');
}

/// The prompt cache hit rate: cached tokens over every non-output token.
///
/// Zero when no cache was read or written, so a cache miss does not report a
/// division by zero.
pub fn cache_hit_rate(usage: &Usage) -> f64 {
    let denominator = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
    if denominator == 0 {
        0.0
    } else {
        usage.cache_read_tokens as f64 / denominator as f64
    }
}

/// The prompt tokens of one turn: the size the context reached at that turn.
pub fn context_tokens(usage: &Usage) -> u64 {
    usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens
}

/// The byte count the base64 text of an image decodes to, without decoding it:
/// four characters are three bytes, minus the padding.
fn decoded_len(data: &str) -> usize {
    let padding = data.bytes().rev().take_while(|byte| *byte == b'=').count();
    (data.len() / 4 * 3).saturating_sub(padding)
}

/// How a client's message joined the session.
fn send_mode(mode: SendMode) -> &'static str {
    match mode {
        SendMode::Steer => "steer",
        SendMode::Queue => "queue",
    }
}

/// The word a client reads for an answer, and the reason of a refusal.
fn answer_line(answer: &Answer) -> (&'static str, Option<&str>) {
    match answer {
        Answer::Once => ("once", None),
        Answer::Session => ("session", None),
        Answer::Save => ("save", None),
        Answer::Refuse { reason } => ("refuse", reason.as_deref()),
        Answer::Skip => ("skip", None),
    }
}

fn turn_reason(reason: &TurnEndReason) -> &'static str {
    match reason {
        TurnEndReason::EndTurn => "end_turn",
        TurnEndReason::Cancelled => "cancelled",
        TurnEndReason::Stopped(_) => "stopped",
        TurnEndReason::MaxTurns => "max_turns",
        TurnEndReason::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn line(event: &Event) -> Value {
        let mut buf = Vec::new();
        write_event(&mut buf, event);
        parse_line(&buf)
    }

    fn parse_line(buf: &[u8]) -> Value {
        let text = std::str::from_utf8(buf).unwrap();
        let body = text.strip_suffix('\n').expect("a line ends with a newline");
        assert!(!body.contains('\n'));
        serde_json::from_str(body).unwrap()
    }

    fn usage() -> Usage {
        Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 30,
            cache_write_tokens: 0,
            reasoning_tokens: 2,
            cost: Some(0.0123),
        }
    }

    /// The events `--json` prints, with the line each one writes, shared so the
    /// worker's line is checked against the same events as the printed one.
    fn cases() -> Vec<(Event, Value)> {
        vec![
            (Event::TurnStart, json!({ "type": "turn_start" })),
            (
                Event::TextDelta("a \"q\"\n".into()),
                json!({ "type": "text_delta", "text": "a \"q\"\n" }),
            ),
            (
                Event::ThinkingDelta("hm".into()),
                json!({ "type": "thinking_delta", "text": "hm" }),
            ),
            (
                Event::ToolStart {
                    id: "c1".into(),
                    name: "bash".into(),
                },
                json!({ "type": "tool_start", "id": "c1", "name": "bash" }),
            ),
            (
                Event::ToolCallReady {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{\"cmd\":\"ls\"}".into(),
                },
                json!({
                    "type": "tool_call_ready",
                    "id": "c1",
                    "name": "bash",
                    "arguments": "{\"cmd\":\"ls\"}",
                }),
            ),
            (
                Event::ToolResult {
                    id: "c1".into(),
                    is_error: true,
                    bytes: 1234,
                    output: Some("old_string not found in a.rs".into()),
                    images: Vec::new(),
                },
                json!({ "type": "tool_result", "id": "c1", "is_error": true, "bytes": 1234, "error": "old_string not found in a.rs" }),
            ),
            (
                Event::ToolResult {
                    id: "c2".into(),
                    is_error: false,
                    bytes: 10,
                    output: Some("the output".into()),
                    images: Vec::new(),
                },
                json!({ "type": "tool_result", "id": "c2", "is_error": false, "bytes": 10 }),
            ),
            (
                Event::ToolResult {
                    id: "c3".into(),
                    is_error: false,
                    bytes: 12,
                    output: None,
                    images: vec![reagent_ai::Image {
                        media_type: reagent_ai::ImageType::Png,
                        width: 64,
                        height: 48,
                        data: "aGk=".into(),
                    }],
                },
                // The line names the image and its decoded size, never its
                // base64 text.
                json!({ "type": "tool_result", "id": "c3", "is_error": false, "bytes": 12, "images": [{ "media_type": "image/png", "width": 64, "height": 48, "bytes": 2 }] }),
            ),
            (
                Event::Usage(usage()),
                json!({
                    "type": "usage",
                    "input": 10,
                    "output": 5,
                    "cache_read": 30,
                    "cache_write": 0,
                    "cache_hit_rate": 0.75,
                    "cost": 0.0123,
                }),
            ),
            (
                Event::TurnEnd(TurnEndReason::MaxTurns),
                json!({ "type": "turn_end", "reason": "max_turns" }),
            ),
            (
                Event::RunEnd(TurnEndReason::Cancelled),
                json!({ "type": "run_end", "reason": "cancelled" }),
            ),
            (
                Event::Error("boom".into()),
                json!({ "type": "error", "message": "boom" }),
            ),
            (
                Event::Retry {
                    attempt: 2,
                    delay_ms: 500,
                    error: "429".into(),
                },
                json!({ "type": "retry", "attempt": 2, "delay_ms": 500, "error": "429" }),
            ),
            (
                Event::PolicyNote("note".into()),
                json!({ "type": "policy_note", "text": "note" }),
            ),
        ]
    }

    #[test]
    fn every_event_line_has_the_same_keys_and_values() {
        for (event, expected) in cases() {
            assert_eq!(line(&event), expected, "{event:?}");
        }
    }

    #[test]
    fn the_worker_line_of_a_result_adds_the_tail_of_its_output() {
        for (event, _) in cases() {
            let mut printed = Vec::new();
            write_event(&mut printed, &event);
            let mut worker = Vec::new();
            write_worker_line(&mut worker, &event);
            let mut line = parse_line(&worker);
            // A result keeps its tail in the fields the printed line lacks.
            let tail = line.as_object_mut().and_then(|line| {
                let result = matches!(event, Event::ToolResult { .. });
                result.then(|| line.remove("output")).flatten()
            });
            let earlier = line.as_object_mut().and_then(|line| {
                let result = matches!(event, Event::ToolResult { .. });
                result.then(|| line.remove("earlier")).flatten()
            });
            assert_eq!(
                line,
                parse_line(&printed),
                "{event:?} is encoded twice for two paths"
            );
            match &event {
                Event::ToolResult {
                    output: Some(text), ..
                } => {
                    assert_eq!(
                        tail.as_ref().and_then(Value::as_str),
                        Some(text.as_ref()),
                        "{event:?}"
                    );
                    assert_eq!(earlier, Some(Value::from(0)), "{event:?}");
                }
                _ => {
                    assert!(tail.is_none(), "{event:?} carries a tail");
                    assert!(earlier.is_none(), "{event:?} counts lines it has none of");
                }
            }
        }
    }

    #[test]
    fn a_result_line_keeps_the_last_lines_of_a_long_output() {
        let mut text = String::new();
        for at in 1..=300 {
            text.push_str(&format!("line {at} of a long output\n"));
        }
        assert!(text.len() > OUTPUT_KEEP);
        let event = Event::ToolResult {
            id: "c1".into(),
            is_error: false,
            bytes: text.len(),
            output: Some(text.as_str().into()),
            images: Vec::new(),
        };
        let mut buf = Vec::new();
        write_worker_line(&mut buf, &event);
        let line = parse_line(&buf);
        let output = line["output"].as_str().expect("a tail");
        assert!(
            output.len() <= OUTPUT_KEEP,
            "{} bytes is over the {OUTPUT_KEEP} kept",
            output.len()
        );
        assert!(text.ends_with(output), "the tail ends the output");
        assert!(output.starts_with("line "), "the tail starts at a line");
        let earlier = line["earlier"].as_u64().expect("a count");
        assert_eq!(
            earlier + output.lines().count() as u64,
            300,
            "the count and the tail make the lines of the output"
        );
        // The printed line of the same result carries neither.
        let mut printed = Vec::new();
        write_event(&mut printed, &event);
        let printed = parse_line(&printed);
        assert_eq!(
            printed,
            json!({ "type": "tool_result", "id": "c1", "is_error": false, "bytes": text.len() })
        );
    }

    #[test]
    fn a_failed_result_line_keeps_its_error_and_its_tail() {
        let event = Event::ToolResult {
            id: "c1".into(),
            is_error: true,
            bytes: 6,
            output: Some("boom\n\n".into()),
            images: Vec::new(),
        };
        let mut buf = Vec::new();
        write_worker_line(&mut buf, &event);
        let line = parse_line(&buf);
        assert_eq!(line["error"], "boom\n\n");
        assert_eq!(line["output"], "boom\n\n");
    }

    #[test]
    fn the_tail_of_a_long_output_starts_at_a_line_or_a_character() {
        let text = format!("{}\né\n", "a".repeat(OUTPUT_KEEP));
        assert_eq!(tail_of(&text), ("é\n", 1));
        assert_eq!(tail_of("short\n"), ("short\n", 0));
        let wide = format!("a{}", "é".repeat(3000));
        let (tail, earlier) = tail_of(&wide);
        assert!(tail.len() <= OUTPUT_KEEP, "{} bytes", tail.len());
        assert!(tail.chars().all(|c| c == 'é'), "{tail:?}");
        assert_eq!(earlier, 0);
    }

    #[test]
    fn a_live_session_event_is_one_worker_line_and_no_json_line() {
        let ask = |save: Option<&str>| {
            Event::ApprovalRequest(Box::new(reagent_agent::event::ApprovalRequest {
                id: 4,
                tool: "bash".into(),
                call: "bash: cargo test -p a".into(),
                rule: "Bash(cargo test:*)".into(),
                save: save.map(Into::into),
            }))
        };
        let resolved = |answer| Event::ApprovalResolved {
            id: 4,
            client: 7,
            answer,
        };
        let cases = [
            (
                Event::UserMessage {
                    client: 7,
                    mode: reagent_agent::live::SendMode::Steer,
                    text: "use cargo nextest".into(),
                    images: Vec::new(),
                },
                json!({ "type": "user_message", "client": 7, "mode": "steer", "text": "use cargo nextest" }),
            ),
            (
                Event::UserMessage {
                    client: 3,
                    mode: reagent_agent::live::SendMode::Queue,
                    text: "later".into(),
                    images: Vec::new(),
                },
                json!({ "type": "user_message", "client": 3, "mode": "queue", "text": "later" }),
            ),
            (
                Event::UserMessage {
                    client: 3,
                    mode: reagent_agent::live::SendMode::Steer,
                    text: "[Image #1] what is this".into(),
                    images: vec![reagent_ai::Image {
                        media_type: reagent_ai::ImageType::Png,
                        width: 64,
                        height: 48,
                        data: "aGk=".into(),
                    }],
                },
                // A pasted image is named by its size, never by its base64 text.
                json!({ "type": "user_message", "client": 3, "mode": "steer", "text": "[Image #1] what is this", "images": [{ "media_type": "image/png", "width": 64, "height": 48, "bytes": 2 }] }),
            ),
            (
                Event::Committed { record: 12 },
                json!({ "type": "committed", "record": 12 }),
            ),
            (
                ask(Some("Bash(cargo test:*)")),
                json!({ "type": "approval_request", "id": 4, "tool": "bash", "call": "bash: cargo test -p a", "rule": "Bash(cargo test:*)", "save": ["Bash(cargo test:*)"] }),
            ),
            (
                ask(None),
                json!({ "type": "approval_request", "id": 4, "tool": "bash", "call": "bash: cargo test -p a", "rule": "Bash(cargo test:*)" }),
            ),
            (
                resolved(reagent_agent::guard::Answer::Once),
                json!({ "type": "approval_resolved", "id": 4, "client": 7, "answer": "once" }),
            ),
            (
                resolved(reagent_agent::guard::Answer::Session),
                json!({ "type": "approval_resolved", "id": 4, "client": 7, "answer": "session" }),
            ),
            (
                resolved(reagent_agent::guard::Answer::Save),
                json!({ "type": "approval_resolved", "id": 4, "client": 7, "answer": "save" }),
            ),
            (
                resolved(reagent_agent::guard::Answer::Refuse {
                    reason: Some("that rewrite loses a column".into()),
                }),
                json!({ "type": "approval_resolved", "id": 4, "client": 7, "answer": "refuse", "reason": "that rewrite loses a column" }),
            ),
            (
                resolved(reagent_agent::guard::Answer::Refuse { reason: None }),
                json!({ "type": "approval_resolved", "id": 4, "client": 7, "answer": "refuse" }),
            ),
            (
                resolved(reagent_agent::guard::Answer::Skip),
                json!({ "type": "approval_resolved", "id": 4, "client": 7, "answer": "skip" }),
            ),
            (
                Event::RuleToSave {
                    rule: "Bash(cargo test:*)".into(),
                },
                json!({ "type": "rule_to_save", "rule": "Bash(cargo test:*)" }),
            ),
            (
                Event::Settings {
                    model: Some("anthropic/m2".into()),
                    level: Some("high".into()),
                    mode: Some(reagent_config::permissions::PermissionMode::Auto),
                },
                json!({ "type": "settings", "model": "anthropic/m2", "level": "high", "mode": "auto" }),
            ),
            (
                // A value the set did not name is left out, not written null.
                Event::Settings {
                    model: None,
                    level: None,
                    mode: Some(reagent_config::permissions::PermissionMode::AcceptEdits),
                },
                json!({ "type": "settings", "mode": "accept-edits" }),
            ),
        ];
        for (event, expected) in cases {
            let mut printed = Vec::new();
            write_event(&mut printed, &event);
            assert!(
                printed.is_empty(),
                "{event:?} has no --json line but wrote {}",
                String::from_utf8_lossy(&printed)
            );
            let mut worker = Vec::new();
            write_line(&mut worker, &event);
            assert_eq!(parse_line(&worker), expected, "{event:?}");
        }
    }

    #[test]
    fn start_line_names_the_version_and_the_session() {
        assert_eq!(JSON_VERSION, 1);
        let mut buf = Vec::new();
        write_start(&mut buf, None);
        assert_eq!(
            std::str::from_utf8(&buf).expect("utf8"),
            format!(
                "{{\"type\":\"start\",\"version\":1,\"reagent\":\"{}\"}}\n",
                env!("CARGO_PKG_VERSION")
            )
        );

        let mut buf = Vec::new();
        write_start(&mut buf, Some("s1"));
        assert_eq!(
            std::str::from_utf8(&buf).expect("utf8"),
            format!(
                "{{\"type\":\"start\",\"version\":1,\"reagent\":\"{}\",\"session\":\"s1\"}}\n",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn summary_line_has_the_same_keys_and_values() {
        let mut buf = Vec::new();
        write_summary(
            &mut buf,
            &usage(),
            7,
            &TurnEndReason::EndTurn,
            Some("max_tokens"),
            40,
            Some(200_000),
        );
        let expected = json!({
            "type": "summary",
            "turns": 7,
            "exit": "end_turn",
            "input": 10,
            "output": 5,
            "cache_read": 30,
            "cache_write": 0,
            "cache_hit_rate": 0.75,
            "cost": 0.0123,
            "context_tokens": 40,
            "context_window": 200_000,
            "stop": "max_tokens",
        });
        assert_eq!(parse_line(&buf), expected);

        buf.clear();
        write_summary(
            &mut buf,
            &usage(),
            7,
            &TurnEndReason::EndTurn,
            None,
            40,
            None,
        );
        let line = parse_line(&buf);
        assert!(line.get("stop").is_none());
        assert_eq!(line["context_window"], Value::Null);
    }

    #[test]
    fn context_tokens_is_the_last_turn_prompt() {
        let usage = Usage {
            input_tokens: 12,
            output_tokens: 3,
            cache_read_tokens: 10,
            cache_write_tokens: 2,
            reasoning_tokens: 0,
            cost: None,
        };
        assert_eq!(context_tokens(&usage), 24);
        assert_eq!(context_tokens(&Usage::default()), 0);
        // The other counts never count towards the prompt.
        let output_only = Usage {
            output_tokens: 500,
            ..Usage::default()
        };
        assert_eq!(context_tokens(&output_only), 0);
    }

    #[test]
    fn a_usage_without_a_cost_still_carries_the_key() {
        let usage = Usage {
            cost: None,
            ..Usage::default()
        };
        assert_eq!(line(&Event::Usage(usage))["cost"], Value::Null);
    }

    #[test]
    fn a_line_is_one_json_object_and_a_newline() {
        let mut buf = Vec::new();
        write_event(&mut buf, &Event::TextDelta("x\ny".into()));
        write_event(&mut buf, &Event::TurnStart);
        let text = std::str::from_utf8(&buf).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn a_live_session_event_writes_no_line() {
        for event in [
            Event::UserMessage {
                client: 7,
                mode: reagent_agent::live::SendMode::Steer,
                text: "use cargo nextest".into(),
                images: Vec::new(),
            },
            Event::Committed { record: 12 },
            Event::ApprovalRequest(Box::new(reagent_agent::event::ApprovalRequest {
                id: 0,
                tool: "bash".into(),
                call: "bash: cargo test -p a".into(),
                rule: "Bash(cargo test:*)".into(),
                save: Some("Bash(cargo test:*)".into()),
            })),
            Event::ApprovalResolved {
                id: 0,
                client: 7,
                answer: reagent_agent::guard::Answer::Session,
            },
            Event::RuleToSave {
                rule: "Bash(cargo test:*)".into(),
            },
        ] {
            let mut buf = Vec::new();
            write_event(&mut buf, &event);
            assert!(
                buf.is_empty(),
                "{event:?} wrote {}",
                String::from_utf8_lossy(&buf)
            );
        }
    }

    /// The line the serialize derive wrote before the fast path, so the fast
    /// path's bytes are checked against the bytes it replaced.
    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Old<'a> {
        TextDelta { text: &'a str },
        ThinkingDelta { text: &'a str },
    }

    fn old_line(line: &Old<'_>) -> Vec<u8> {
        let mut buf = Vec::new();
        serde_json::to_writer(&mut buf, line).expect("a json line serializes");
        buf.push(b'\n');
        buf
    }

    fn fast_string(value: &str) -> String {
        let mut buf = Vec::new();
        write_string(&mut buf, value);
        String::from_utf8(buf).expect("a json string is utf8")
    }

    #[test]
    fn json_string_writer_matches_serde_json() {
        let mut cases: Vec<String> = vec![
            String::new(),
            "plain ascii, with a space".into(),
            "a \"quoted\" word".into(),
            "back\\slash\\\\".into(),
            "tab\tnewline\ncarriage\r".into(),
            "/solidus/ and 0x7f are not escaped: \u{7f}".into(),
            "héllo—ünïcode".into(),
            "汉字テスト".into(),
            "x".repeat(10 * 1024),
        ];
        // Every control byte on its own, then all of them in one string.
        let controls: String = (0u8..=0x1f).map(char::from).collect();
        cases.extend(controls.chars().map(String::from));
        cases.push(controls);
        for value in &cases {
            let expected = serde_json::to_string(value).expect("a string serializes");
            assert_eq!(fast_string(value), expected, "{value:?}");
        }
    }

    #[test]
    fn json_string_text_delta_line_is_unchanged() {
        for text in ["x", "a \"q\"\n", "", "汉字", "tab\there", "back\\"] {
            let mut buf = Vec::new();
            write_event(&mut buf, &Event::TextDelta(text.into()));
            assert_eq!(buf, old_line(&Old::TextDelta { text }), "{text:?}");

            let mut buf = Vec::new();
            write_event(&mut buf, &Event::ThinkingDelta(text.into()));
            assert_eq!(buf, old_line(&Old::ThinkingDelta { text }), "{text:?}");
        }
        // The exact bytes of the common line.
        let mut buf = Vec::new();
        write_event(&mut buf, &Event::TextDelta("x".into()));
        assert_eq!(
            std::str::from_utf8(&buf).expect("utf8"),
            "{\"type\":\"text_delta\",\"text\":\"x\"}\n"
        );
    }
}
