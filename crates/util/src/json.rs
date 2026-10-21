//! Reading values out of JSON arguments.

use std::borrow::Cow;

use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};

/// The string value of `key` in the JSON object `args`, borrowed when it
/// holds no escapes; other keys are skipped without being built.
pub fn string_arg<'a>(args: &'a str, key: &str) -> Option<Cow<'a, str>> {
    struct Find<'k>(&'k str);

    #[derive(Deserialize)]
    struct Borrowed<'a>(#[serde(borrow)] Cow<'a, str>);

    impl<'de> Visitor<'de> for Find<'_> {
        type Value = Option<Cow<'de, str>>;

        fn expecting(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            out.write_str("a JSON object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut found = None;
            while let Some(Borrowed(name)) = map.next_key()? {
                if found.is_none() && name == self.0 {
                    found = map.next_value::<Option<Borrowed<'de>>>().ok().flatten();
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(found.map(|Borrowed(value)| value))
        }
    }

    let mut parser = serde_json::Deserializer::from_str(args);
    parser.deserialize_map(Find(key)).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_arg_borrows_and_skips_other_keys() {
        let args = r#"{"other": {"deep": [1, 2]}, "command": "git push", "x": 3}"#;
        let value = string_arg(args, "command").unwrap();
        assert!(matches!(value, Cow::Borrowed("git push")));
        assert_eq!(
            string_arg(r#"{"command": "a\"b"}"#, "command").as_deref(),
            Some("a\"b")
        );
        assert_eq!(string_arg(r#"{"command": 7}"#, "command"), None);
        assert_eq!(string_arg("not json", "command"), None);
        assert_eq!(string_arg(r#"{"path": "x"}"#, "command"), None);
    }
}
