//! JSON line rendering for `--json` mode.

use serde_json::{Value, json};

use reagent_agent::event::{Event, TurnEndReason};
use reagent_ai::Usage;

/// Renders one agent event as a single JSON object.
pub fn event(event: &Event) -> Value {
    match event {
        Event::TurnStart => json!({ "type": "turn_start" }),
        Event::TextDelta(text) => json!({ "type": "text_delta", "text": text.as_ref() }),
        Event::ThinkingDelta(text) => {
            json!({ "type": "thinking_delta", "text": text.as_ref() })
        }
        Event::ToolStart { id, name } => json!({
            "type": "tool_start",
            "id": id.as_ref(),
            "name": name.as_ref(),
        }),
        Event::ToolCallReady {
            id,
            name,
            arguments,
        } => json!({
            "type": "tool_call_ready",
            "id": id.as_ref(),
            "name": name.as_ref(),
            "arguments": arguments.as_ref(),
        }),
        Event::ToolResult { id, is_error } => json!({
            "type": "tool_result",
            "id": id.as_ref(),
            "is_error": is_error,
        }),
        Event::Usage(usage) => json!({
            "type": "usage",
            "input": usage.input_tokens,
            "output": usage.output_tokens,
            "cache_read": usage.cache_read_tokens,
            "cache_write": usage.cache_write_tokens,
            "cache_hit_rate": cache_hit_rate(usage),
        }),
        Event::TurnEnd(reason) => json!({
            "type": "turn_end",
            "reason": turn_reason(reason),
        }),
        Event::RunEnd(reason) => json!({
            "type": "run_end",
            "reason": turn_reason(reason),
        }),
        Event::Error(message) => json!({
            "type": "error",
            "message": message.as_ref(),
        }),
        Event::Retry {
            attempt,
            delay_ms,
            error,
        } => json!({
            "type": "retry",
            "attempt": attempt,
            "delay_ms": delay_ms,
            "error": error.as_ref(),
        }),
        Event::PolicyNote(text) => json!({
            "type": "policy_note",
            "text": text.as_ref(),
        }),
    }
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

fn turn_reason(reason: &TurnEndReason) -> &'static str {
    match reason {
        TurnEndReason::EndTurn => "end_turn",
        TurnEndReason::Cancelled => "cancelled",
        TurnEndReason::Stopped(_) => "stopped",
        TurnEndReason::MaxTurns => "max_turns",
        TurnEndReason::Error => "error",
    }
}

/// The final summary line: total usage, turns and stop reason.
pub fn summary(
    usage: &Usage,
    turns: u32,
    reason: &TurnEndReason,
    stop: Option<&'static str>,
) -> Value {
    let mut summary = json!({
        "type": "summary",
        "turns": turns,
        "exit": turn_reason(reason),
        "input": usage.input_tokens,
        "output": usage.output_tokens,
        "cache_read": usage.cache_read_tokens,
        "cache_write": usage.cache_write_tokens,
        "cache_hit_rate": cache_hit_rate(usage),
    });
    if let Some(stop) = stop {
        summary["stop"] = Value::from(stop);
    }
    summary
}
