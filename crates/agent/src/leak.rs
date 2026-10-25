//! Tool calls a model wrote into its reply as text, in the formats a provider
//! may leak them in.

use std::ops::Range;

use serde::Deserialize;
use serde_json::{Map, Value};

/// One tool call found in reply text.
#[derive(Debug)]
pub struct Leaked<'a> {
    /// The tool name as the model wrote it, trimmed.
    pub name: &'a str,
    /// The JSON object text of the call's arguments.
    pub arguments: String,
    /// The byte range of the whole leaked block.
    pub span: Range<usize>,
}

/// The markers a leaked block can start with.
const STARTS: [&str; 7] = [
    TOOL_OPEN,
    FUNCTION_OPEN,
    HARMONY_TO,
    KIMI_BEGIN,
    KIMI_SECTION_BEGIN,
    DEEPSEEK_CALL_BEGIN,
    DEEPSEEK_CALLS_BEGIN,
];

const TOOL_OPEN: &str = "<tool_call>";
const TOOL_CLOSE: &str = "</tool_call>";
const FUNCTION_OPEN: &str = "<function=";
const FUNCTION_CLOSE: &str = "</function>";
const PARAMETER_OPEN: &str = "<parameter=";
const PARAMETER_CLOSE: &str = "</parameter>";
const HARMONY_TO: &str = "to=functions.";
const HARMONY_START: &str = "<|start|>";
const HARMONY_CHANNEL: &str = "<|channel|>";
const KIMI_SECTION_BEGIN: &str = "<|tool_calls_section_begin|>";
const KIMI_SECTION_END: &str = "<|tool_calls_section_end|>";
const KIMI_BEGIN: &str = "<|tool_call_begin|>";
const KIMI_ARGUMENTS: &str = "<|tool_call_argument_begin|>";
const KIMI_END: &str = "<|tool_call_end|>";
const DEEPSEEK_CALLS_BEGIN: &str = "<｜tool▁calls▁begin｜>";
const DEEPSEEK_CALLS_END: &str = "<｜tool▁calls▁end｜>";
const DEEPSEEK_CALL_BEGIN: &str = "<｜tool▁call▁begin｜>";
const DEEPSEEK_CALL_END: &str = "<｜tool▁call▁end｜>";
const DEEPSEEK_SEP: &str = "<｜tool▁sep｜>";

/// The first tool call leaked into `text`, in any of the formats below.
pub fn in_text(text: &str) -> Option<Leaked<'_>> {
    find(text, false)
}

/// The tool call `text` ends with, ignoring trailing whitespace.
pub fn at_end(text: &str) -> Option<Leaked<'_>> {
    find(text, true)
}

/// The first call that starts at a marker of `text` and, when `end_only`, ends
/// the text. Reads the markers with `find`, so text without one allocates
/// nothing.
fn find(text: &str, end_only: bool) -> Option<Leaked<'_>> {
    let end = text.trim_end().len();
    let mut from = 0;
    while let Some(at) = next_start(text, from) {
        if let Some(leaked) = parse(text, at)
            && (!end_only || leaked.span.end == end)
        {
            return Some(leaked);
        }
        from = at + 1;
    }
    None
}

/// The nearest start marker at or after `from`, and none when the text holds
/// none.
fn next_start(text: &str, from: usize) -> Option<usize> {
    let mut nearest = None;
    for marker in STARTS {
        if let Some(at) = text[from..].find(marker) {
            let at = from + at;
            nearest = Some(nearest.map_or(at, |nearest: usize| nearest.min(at)));
        }
    }
    nearest
}

/// The call of the block that starts at `at`, in the format its marker names.
fn parse(text: &str, at: usize) -> Option<Leaked<'_>> {
    let rest = &text[at..];
    if rest.starts_with(TOOL_OPEN) {
        return hermes(text, at)
            .or_else(|| wrapped_function(text, at))
            .or_else(|| glm(text, at));
    }
    if rest.starts_with(FUNCTION_OPEN) {
        return function(text, at);
    }
    if rest.starts_with(HARMONY_TO) {
        return harmony(text, at);
    }
    if rest.starts_with(KIMI_BEGIN) || rest.starts_with(KIMI_SECTION_BEGIN) {
        return kimi(text, at);
    }
    deepseek(text, at)
}

/// The body between `open` at `at` and the next `close`, and where the block
/// ends: after the close, or at the end of the text when none follows.
fn block<'a>(text: &'a str, at: usize, open: &str, close: &str) -> Option<(&'a str, usize)> {
    let rest = text[at..].strip_prefix(open)?;
    match rest.find(close) {
        Some(end) => Some((&rest[..end], text.len() - rest.len() + end + close.len())),
        None => Some((rest.trim_end(), text.trim_end().len())),
    }
}

/// A value that parses as a JSON array, object, number or boolean is kept as
/// that value, and any other value is a string.
fn json_value(text: &str) -> Value {
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(json @ (Value::Array(_) | Value::Object(_) | Value::Number(_) | Value::Bool(_))) => json,
        _ => Value::String(text.to_owned()),
    }
}

/// The JSON object text of a leaked call's arguments, cut from a JSON string,
/// a fenced block or trailing control tokens as [`crate::repair::arguments`]
/// does.
fn json_object(text: &str) -> Option<String> {
    crate::repair::arguments(text.trim().to_owned())
        .ok()
        .map(|(object, _)| object)
}

/// Whether `name` can be a tool name: no whitespace and no tag of its own.
fn is_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(|c: char| c.is_whitespace() || c == '<')
}

/// A Hermes or Qwen call: `<tool_call>` and the JSON
/// `{"name": N, "arguments": A}`, where `A` may be an object or a string
/// holding one, `parameters` names it too, and a missing one means `{}`.
fn hermes(text: &str, at: usize) -> Option<Leaked<'_>> {
    #[derive(Deserialize)]
    struct Call<'a> {
        #[serde(borrow)]
        name: &'a str,
        #[serde(default)]
        arguments: Option<Value>,
        #[serde(default)]
        parameters: Option<Value>,
    }
    let (body, end) = block(text, at, TOOL_OPEN, TOOL_CLOSE)?;
    let call: Call<'_> = serde_json::from_str(body.trim()).ok()?;
    let name = call.name.trim();
    if !is_name(name) {
        return None;
    }
    let arguments = match call.arguments.or(call.parameters) {
        None => "{}".to_owned(),
        Some(Value::String(text)) => json_object(&text)?,
        Some(sent) => json_object(&sent.to_string())?,
    };
    Some(Leaked {
        name,
        arguments,
        span: at..end,
    })
}

/// A Qwen3-Coder call: `<function=N>` and its `<parameter=K>V</parameter>`
/// tags, inside a `<tool_call>` wrapper that the span then covers too.
fn wrapped_function(text: &str, at: usize) -> Option<Leaked<'_>> {
    let rest = text[at + TOOL_OPEN.len()..].trim_start();
    let mut leaked = function(text, text.len() - rest.len())?;
    let tail = &text[leaked.span.end..];
    let trimmed = tail.trim_start();
    if trimmed.starts_with(TOOL_CLOSE) {
        leaked.span = at..text.len() - trimmed.len() + TOOL_CLOSE.len();
    } else {
        leaked.span = at..leaked.span.end;
    }
    Some(leaked)
}

/// A Qwen3-Coder call without its wrapper.
fn function(text: &str, at: usize) -> Option<Leaked<'_>> {
    let rest = text[at..].strip_prefix(FUNCTION_OPEN)?;
    let (name, after) = rest.split_once('>')?;
    let name = name.trim();
    if !is_name(name) {
        return None;
    }
    let close = after.find(FUNCTION_CLOSE)?;
    let mut args = Map::new();
    let mut rest = &after[..close];
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let (key, after) = rest.strip_prefix(PARAMETER_OPEN)?.split_once('>')?;
        let (value, after) = after.split_once(PARAMETER_CLOSE)?;
        args.insert(key.trim().to_owned(), json_value(trim_newline(value)));
        rest = after;
    }
    Some(Leaked {
        name,
        arguments: Value::Object(args).to_string(),
        span: at..text.len() - after.len() + close + FUNCTION_CLOSE.len(),
    })
}

/// `text` with one newline cut from each end.
fn trim_newline(text: &str) -> &str {
    let text = text
        .strip_prefix("\r\n")
        .or_else(|| text.strip_prefix('\n'))
        .unwrap_or(text);
    text.strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text)
}

/// A GLM call: `<tool_call>NAME<arg_key>K</arg_key><arg_value>V</arg_value>`.
fn glm(text: &str, at: usize) -> Option<Leaked<'_>> {
    let rest = text[at..].strip_prefix(TOOL_OPEN)?;
    let begins = text.len() - rest.len();
    let close = rest.find(TOOL_CLOSE)?;
    let body = &rest[..close];
    let (name, mut rest) = body.split_at(body.find("<arg_key>").unwrap_or(body.len()));
    let name = name.trim();
    if !is_name(name) {
        return None;
    }
    let mut args = Map::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let (key, after) = rest.strip_prefix("<arg_key>")?.split_once("</arg_key>")?;
        let (value, after) = after
            .trim_start()
            .strip_prefix("<arg_value>")?
            .split_once("</arg_value>")?;
        args.insert(key.trim().to_owned(), json_value(value));
        rest = after;
    }
    Some(Leaked {
        name,
        arguments: Value::Object(args).to_string(),
        span: at..begins + close + TOOL_CLOSE.len(),
    })
}

/// A Harmony call: `to=functions.NAME`, then `{"..."}` framed by
/// `<|constrain|>json`, `<|message|>` and `<|call|>`.
fn harmony(text: &str, at: usize) -> Option<Leaked<'_>> {
    let rest = text[at..].strip_prefix(HARMONY_TO)?;
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '<')
        .unwrap_or(rest.len());
    let name = &rest[..end];
    if name.is_empty() {
        return None;
    }
    let mut after = rest[end..].trim_start();
    if let Some(constrained) = after.strip_prefix("<|constrain|>json") {
        after = constrained.trim_start();
    }
    let body = after.strip_prefix("<|message|>")?;
    let begins = text.len() - body.len();
    let stop = [
        (body.find("<|call|>"), "<|call|>"),
        (body.find("<|end|>"), "<|end|>"),
    ]
    .into_iter()
    .filter_map(|(at, close)| at.map(|at| (at, close)))
    .min_by_key(|(at, _)| *at);
    let (json, end) = match stop {
        Some((close_at, close)) => (&body[..close_at], begins + close_at + close.len()),
        None => (body.trim_end(), text.trim_end().len()),
    };
    Some(Leaked {
        name,
        arguments: json_object(json)?,
        span: harmony_start(text, at)..end,
    })
}

/// Where the Harmony block of `at` starts: its `<|start|>` or `<|channel|>`
/// marker when one comes before `to=` on the same line, else `at` itself.
fn harmony_start(text: &str, at: usize) -> usize {
    let start = text[..at].rfind('\n').map_or(0, |newline| newline + 1);
    let line = &text[start..at];
    line.find(HARMONY_START)
        .or_else(|| line.find(HARMONY_CHANNEL))
        .map_or(at, |marker| start + marker)
}

/// A Kimi K2 call: `<|tool_call_begin|>functions.NAME:IDX`, its arguments and
/// `<|tool_call_end|>`, inside an optional section the span covers.
fn kimi(text: &str, at: usize) -> Option<Leaked<'_>> {
    let rest = &text[at..];
    let section = rest.strip_prefix(KIMI_SECTION_BEGIN);
    let rest = section.unwrap_or(rest).trim_start();
    let rest = rest.strip_prefix(KIMI_BEGIN)?;
    let (head, after) = rest.split_once(KIMI_ARGUMENTS)?;
    let name = head.trim();
    let name = name.strip_prefix("functions.").unwrap_or(name);
    let name = name.split(':').next().unwrap_or(name).trim();
    if name.is_empty() {
        return None;
    }
    let begins = text.len() - after.len();
    let (json, mut end) = match after.find(KIMI_END) {
        Some(close) => (&after[..close], begins + close + KIMI_END.len()),
        None => (after.trim_end(), text.trim_end().len()),
    };
    if section.is_some()
        && let Some(tail) = text[end..].trim_start().strip_prefix(KIMI_SECTION_END)
    {
        end = text.len() - tail.len();
    }
    Some(Leaked {
        name,
        arguments: json_object(json)?,
        span: at..end,
    })
}

/// A DeepSeek call: `<｜tool▁call▁begin｜>`, then `function<｜tool▁sep｜>NAME`
/// and a fenced body or `NAME<｜tool▁sep｜>` and a bare one, then
/// `<｜tool▁call▁end｜>`, inside an optional section the span covers.
fn deepseek(text: &str, at: usize) -> Option<Leaked<'_>> {
    let rest = &text[at..];
    let section = rest.strip_prefix(DEEPSEEK_CALLS_BEGIN);
    let rest = section
        .unwrap_or(rest)
        .trim_start()
        .strip_prefix(DEEPSEEK_CALL_BEGIN)?
        .trim_start();
    let (name, json, mut end) = if let Some(fenced) = rest
        .strip_prefix("function")
        .and_then(|rest| rest.strip_prefix(DEEPSEEK_SEP))
    {
        let (name, fenced) = fenced.split_once('\n')?;
        let fenced = fenced.trim_start();
        let fenced = fenced
            .strip_prefix("```json")
            .or_else(|| fenced.strip_prefix("```"))?;
        let (json, tail) = fenced.split_once("```")?;
        (name.trim(), json, text.len() - tail.len())
    } else {
        let (name, after) = rest.split_once(DEEPSEEK_SEP)?;
        let close = after.find(DEEPSEEK_CALL_END).unwrap_or(after.len());
        (
            name.trim(),
            &after[..close],
            text.len() - after.len() + close,
        )
    };
    if !is_name(name) {
        return None;
    }
    if let Some(tail) = text[end..].trim_start().strip_prefix(DEEPSEEK_CALL_END) {
        end = text.len() - tail.len();
    }
    if section.is_some()
        && let Some(tail) = text[end..].trim_start().strip_prefix(DEEPSEEK_CALLS_END)
    {
        end = text.len() - tail.len();
    }
    Some(Leaked {
        name,
        arguments: json_object(json)?,
        span: at..end,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn args(leaked: &Leaked<'_>) -> Value {
        serde_json::from_str(&leaked.arguments).expect("the arguments are a JSON object")
    }

    #[test]
    fn hermes_call_reads_its_name_and_arguments() {
        let text = "Let me read it. <tool_call>{\"name\": \"read\", \"arguments\": {\"path\": \"a.txt\"}}</tool_call>";
        let leaked = in_text(text).expect("a call");
        assert_eq!(leaked.name, "read");
        assert_eq!(args(&leaked), json!({"path": "a.txt"}));
        assert_eq!(leaked.span, 16..text.len());
    }

    #[test]
    fn hermes_arguments_may_be_a_string_parameters_or_missing() {
        let as_string =
            r#"<tool_call>{"name":"read","arguments":"{\"path\":\"a.txt\"}"}</tool_call>"#;
        assert_eq!(
            args(&in_text(as_string).expect("a call")),
            json!({"path": "a.txt"})
        );
        let parameters = r#"<tool_call>{"name":"read","parameters":{"path":"a.txt"}}</tool_call>"#;
        assert_eq!(
            args(&in_text(parameters).expect("a call")),
            json!({"path": "a.txt"})
        );
        let missing = r#"<tool_call>{"name":"read"}</tool_call>"#;
        assert_eq!(args(&in_text(missing).expect("a call")), json!({}));
    }

    #[test]
    fn qwen_xml_call_reads_one_parameter_per_tag() {
        let text = "<tool_call>\n<function=read>\n<parameter=path>a.txt</parameter>\n\
            <parameter=limit>5</parameter>\n</function>\n</tool_call>";
        let leaked = in_text(text).expect("a call");
        assert_eq!(leaked.name, "read");
        assert_eq!(args(&leaked), json!({"path": "a.txt", "limit": 5}));
        assert_eq!(leaked.span, 0..text.len());
    }

    #[test]
    fn qwen_xml_call_without_a_wrapper_spans_the_function_tag() {
        let text = "now: <function=echo>\n<parameter=message>\nhi\n</parameter>\n</function>";
        let leaked = in_text(text).expect("a call");
        assert_eq!(leaked.name, "echo");
        assert_eq!(args(&leaked), json!({"message": "hi"}));
        assert_eq!(leaked.span, 5..text.len());
    }

    #[test]
    fn qwen_xml_values_that_are_json_keep_their_type() {
        let text = "<function=run>\n<parameter=command>ls</parameter>\n<parameter=wait>true</parameter>\n\
            <parameter=paths>[\"a\",\"b\"]</parameter>\n</function>";
        assert_eq!(
            args(&in_text(text).expect("a call")),
            json!({"command": "ls", "wait": true, "paths": ["a", "b"]})
        );
    }

    #[test]
    fn glm_call_reads_strings_and_json_values() {
        let text = "plan\n<tool_call>read<arg_key>path</arg_key><arg_value>a.txt</arg_value>\
            <arg_key>limit</arg_key><arg_value>5</arg_value></tool_call>\n";
        let leaked = at_end(text).expect("a call");
        assert_eq!(leaked.name, "read");
        assert_eq!(args(&leaked), json!({"path": "a.txt", "limit": 5}));
        assert_eq!(leaked.span, 5..text.trim_end().len());
    }

    #[test]
    fn harmony_call_spans_its_block_marker() {
        let text = "<|start|>assistant<|channel|>commentary to=functions.echo<|constrain|>json\
            <|message|>{\"message\":\"hi\"}<|call|>";
        let leaked = in_text(text).expect("a call");
        assert_eq!(leaked.name, "echo");
        assert_eq!(args(&leaked), json!({"message": "hi"}));
        assert_eq!(leaked.span, 0..text.len());
    }

    #[test]
    fn harmony_call_without_its_call_marker_ends_the_text() {
        let text = "to=functions.echo<|constrain|>json<|message|>{\"message\":\"hi\"}";
        let leaked = at_end(text).expect("a call");
        assert_eq!(leaked.name, "echo");
        assert_eq!(args(&leaked), json!({"message": "hi"}));
        assert_eq!(leaked.span, 0..text.len());
    }

    #[test]
    fn kimi_call_without_a_section_spans_the_call() {
        let text = "<|tool_call_begin|>read<|tool_call_argument_begin|>{\"path\":\"a.txt\"}\
            <|tool_call_end|>";
        let leaked = in_text(text).expect("a call");
        assert_eq!(leaked.name, "read");
        assert_eq!(args(&leaked), json!({"path": "a.txt"}));
        assert_eq!(leaked.span, 0..text.len());
    }

    #[test]
    fn kimi_call_in_a_section_spans_the_section() {
        let text = "<|tool_calls_section_begin|><|tool_call_begin|>functions.read:0\
            <|tool_call_argument_begin|>{\"path\":\"a.txt\"}<|tool_call_end|>\
            <|tool_calls_section_end|>";
        let leaked = in_text(text).expect("a call");
        assert_eq!(leaked.name, "read");
        assert_eq!(args(&leaked), json!({"path": "a.txt"}));
        assert_eq!(leaked.span, 0..text.len());
    }

    #[test]
    fn deepseek_call_with_a_fenced_function_body() {
        let text = "<｜tool▁call▁begin｜>function<｜tool▁sep｜>read\n```json\n\
            {\"path\":\"a.txt\"}\n```<｜tool▁call▁end｜>";
        let leaked = in_text(text).expect("a call");
        assert_eq!(leaked.name, "read");
        assert_eq!(args(&leaked), json!({"path": "a.txt"}));
        assert_eq!(leaked.span, 0..text.len());
    }

    #[test]
    fn deepseek_call_with_a_bare_object_in_a_section() {
        let text = "<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>read<｜tool▁sep｜>{\"path\":\"a.txt\"}\
            <｜tool▁call▁end｜><｜tool▁calls▁end｜>";
        let leaked = in_text(text).expect("a call");
        assert_eq!(leaked.name, "read");
        assert_eq!(args(&leaked), json!({"path": "a.txt"}));
        assert_eq!(leaked.span, 0..text.len());
    }

    #[test]
    fn a_block_between_other_text_spans_only_the_block() {
        let block =
            "<tool_call>read<arg_key>path</arg_key><arg_value>a.txt</arg_value></tool_call>";
        let text = format!("before {block} after");
        let leaked = in_text(&text).expect("a call");
        assert_eq!(leaked.name, "read");
        assert_eq!(leaked.span, 7..7 + block.len());
    }

    #[test]
    fn at_end_refuses_a_block_followed_by_more_text() {
        let block =
            "<tool_call>read<arg_key>path</arg_key><arg_value>a.txt</arg_value></tool_call>";
        let text = format!("plan {block}\nand more");
        assert!(in_text(&text).is_some());
        assert!(at_end(&text).is_none());
        assert!(at_end(&text[..5 + block.len()]).is_some());
    }

    #[test]
    fn prose_that_mentions_a_marker_is_no_call() {
        assert!(in_text("I will call <tool_call> later").is_none());
        assert!(at_end("I will call <tool_call> later").is_none());
        assert!(in_text("no marker here").is_none());
    }
}
