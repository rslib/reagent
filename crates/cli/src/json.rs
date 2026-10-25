//! JSON line rendering for `--json` mode.

use serde::Serialize;

use reagent_agent::event::{Event, TurnEndReason};
use reagent_ai::Usage;

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
        replaced: u32,
        bytes: u32,
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

/// Appends one agent event to `buf` as a JSON object and a newline.
pub fn write_event(buf: &mut Vec<u8>, event: &Event) {
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
            error,
        } => Line::ToolResult {
            id,
            is_error: *is_error,
            bytes: *bytes,
            error: error.as_deref(),
        },
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
            replaced,
            bytes,
        } => Line::Compacted {
            before: *before,
            replaced: *replaced,
            bytes: *bytes,
        },
    };
    push(buf, &line);
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

    #[test]
    fn every_event_line_has_the_same_keys_and_values() {
        let cases = [
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
                    error: Some("old_string not found in a.rs".into()),
                },
                json!({ "type": "tool_result", "id": "c1", "is_error": true, "bytes": 1234, "error": "old_string not found in a.rs" }),
            ),
            (
                Event::ToolResult {
                    id: "c2".into(),
                    is_error: false,
                    bytes: 10,
                    error: None,
                },
                json!({ "type": "tool_result", "id": "c2", "is_error": false, "bytes": 10 }),
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
        ];
        for (event, expected) in cases {
            assert_eq!(line(&event), expected, "{event:?}");
        }
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
