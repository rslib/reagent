//! Repair of tool-call arguments that are not a JSON object.

/// Returns `raw` when it is a JSON object, else the object it holds: inside
/// a JSON string, inside a markdown code fence, or at its start before
/// trailing text. Returns `raw` unchanged when none of these is an object.
pub fn arguments(raw: String) -> String {
    if is_object(&raw) {
        return raw;
    }
    repaired(&raw).unwrap_or(raw)
}

fn repaired(raw: &str) -> Option<String> {
    let text = raw.trim();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn repair(raw: &str) -> String {
        arguments(raw.to_owned())
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

    #[test]
    fn text_with_no_object_is_kept_as_sent() {
        assert_eq!(repair("not json"), "not json");
        assert_eq!(repair(r#"{"a": "#), r#"{"a": "#);
        assert_eq!(repair(r#""just text""#), r#""just text""#);
        assert_eq!(repair("[1, 2]"), "[1, 2]");
    }
}
