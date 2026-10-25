//! Repair of tool-call arguments that are not a JSON object.

/// Returns `raw` when it is a JSON object, else the object it holds: inside
/// a JSON string, inside a markdown code fence, or at its start before
/// trailing text. The flag says whether the text was repaired. Returns `raw`
/// back as the error when none of these is an object.
pub fn arguments(raw: String) -> Result<(String, bool), String> {
    if is_object(&raw) {
        return Ok((raw, false));
    }
    repaired(&raw).map(|object| (object, true)).ok_or(raw)
}

fn repaired(raw: &str) -> Option<String> {
    let text = strip_tokens(raw.trim());
    if let Some(object) = repaired_text(text) {
        return Some(object);
    }
    let escaped = escape(text);
    if escaped == text {
        return None;
    }
    repaired_text(&escaped)
}

fn repaired_text(text: &str) -> Option<String> {
    if let Ok(inner) = serde_json::from_str::<String>(text)
        && is_object(&inner)
    {
        return Some(inner);
    }
    if let Some(body) = fenced(text)
        && is_object(body)
    {
        return Some(body.trim().to_owned());
    }
    leading_object(text).map(str::to_owned)
}

fn is_object(text: &str) -> bool {
    text.trim_start().starts_with('{')
        && serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

/// The body of a ```` ``` ```` block that is the whole text.
fn fenced(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("```")?.strip_suffix("```")?;
    let (_language, body) = rest.split_once('\n')?;
    Some(body)
}

/// The JSON object at the start of `text`, when text follows it.
fn leading_object(text: &str) -> Option<&str> {
    if !text.starts_with('{') {
        return None;
    }
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<serde::de::IgnoredAny>();
    values.next()?.ok()?;
    Some(&text[..values.byte_offset()])
}

/// `text` with the control tokens of the form `<|...|>` that end it cut off.
fn strip_tokens(mut text: &str) -> &str {
    while text.ends_with("|>")
        && let Some(start) = text.rfind("<|")
        && start + 2 <= text.len() - 2
    {
        text = text[..start].trim();
    }
    text
}

/// `text` with the JSON strings in it repaired, as pi's `repairJson` does: a
/// raw control character becomes its escape, and a backslash that starts no
/// valid escape is doubled. Unterminated strings and braces stay as they are.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if !in_string {
            in_string = c == '"';
            out.push(c);
            continue;
        }
        match c {
            '"' => {
                in_string = false;
                out.push(c);
            }
            '\\' => match escape_len(&text[at..]) {
                0 => out.push_str("\\\\"),
                len => {
                    out.push(c);
                    for _ in 1..len {
                        if let Some((_, escaped)) = chars.next() {
                            out.push(escaped);
                        }
                    }
                }
            },
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// How many characters the valid JSON escape at the start of `rest` holds, and
/// 0 when its backslash starts no escape.
fn escape_len(rest: &str) -> usize {
    let Some(rest) = rest.strip_prefix('\\') else {
        return 0;
    };
    match rest.as_bytes() {
        [b'u', a, b, c, d, ..] if [a, b, c, d].iter().all(|h| h.is_ascii_hexdigit()) => 6,
        [b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't', ..] => 2,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repair(raw: &str) -> String {
        arguments(raw.to_owned()).map_or_else(|raw| raw, |(object, _)| object)
    }

    #[test]
    fn a_valid_object_is_unchanged() {
        assert_eq!(repair(r#"{"a": 1}"#), r#"{"a": 1}"#);
    }

    #[test]
    fn a_json_string_holding_an_object_is_unwrapped() {
        assert_eq!(repair(r#""{\"a\":1}""#), r#"{"a":1}"#);
    }

    #[test]
    fn a_fenced_object_is_unwrapped() {
        assert_eq!(repair("```json\n{\"a\":1}\n```"), r#"{"a":1}"#);
        assert_eq!(repair("```\n{\"a\":1}\n```"), r#"{"a":1}"#);
    }

    #[test]
    fn trailing_text_after_an_object_is_dropped() {
        assert_eq!(repair(r#"{"a":1} done"#), r#"{"a":1}"#);
        assert_eq!(repair("{\"a\":1}\n</tool_call>"), r#"{"a":1}"#);
    }

    fn parsed(raw: &str) -> serde_json::Value {
        serde_json::from_str(&repair(raw)).expect("the repaired arguments parse")
    }

    #[test]
    fn json_raw_newline_in_a_string_is_escaped() {
        assert_eq!(
            parsed("{\"command\":\"echo a\nb\"}")["command"],
            "echo a\nb"
        );
    }

    #[test]
    fn json_raw_tab_in_a_string_is_escaped() {
        assert_eq!(parsed("{\"command\":\"echo\ta\"}")["command"], "echo\ta");
    }

    #[test]
    fn json_bad_escape_is_doubled() {
        assert_eq!(
            parsed(r#"{"path":"C:\dir\x.txt"}"#)["path"],
            r#"C:\dir\x.txt"#
        );
    }

    #[test]
    fn json_valid_escapes_are_kept() {
        assert_eq!(
            parsed(r#"{"command":"echo a\nb\u0041"}"#)["command"],
            "echo a\nbA"
        );
    }

    #[test]
    fn json_escaped_quote_keeps_the_string_open() {
        assert_eq!(parsed("{\"a\":\"x\\\"y\nz\"}")["a"], "x\"y\nz");
    }

    #[test]
    fn json_escaped_backslash_is_kept() {
        assert_eq!(parsed("{\"a\":\"p\\\\x\n\"}")["a"], "p\\x\n");
    }

    #[test]
    fn json_trailing_tokens_are_dropped() {
        assert_eq!(repair(r#"{"path":"a.txt"}<|call|>"#), r#"{"path":"a.txt"}"#);
        assert_eq!(
            repair(r#"{"path":"a.txt"} <|call|> <|end|>"#),
            r#"{"path":"a.txt"}"#
        );
    }

    #[test]
    fn json_cut_off_object_stays_broken() {
        assert!(arguments(r#"{"path":"a.txt"#.to_owned()).is_err());
    }

    #[test]
    fn text_with_no_object_is_kept_as_sent() {
        assert_eq!(repair("not json"), "not json");
        assert_eq!(repair(r#"{"a": "#), r#"{"a": "#);
        assert_eq!(repair(r#""just text""#), r#""just text""#);
        assert_eq!(repair("[1, 2]"), "[1, 2]");
    }
}
