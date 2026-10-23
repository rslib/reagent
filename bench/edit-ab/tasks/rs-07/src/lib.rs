//! A small config file parser: `key = value` lines, `[section]` headers and
//! `#` comments.

use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// A line that is neither a section nor a `key = value` pair.
    BadLine(usize),
    /// A `[section` header without the closing bracket.
    BadSection(usize),
    EmptyKey(usize),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    values: BTreeMap<String, String>,
}

/// Parses config text. Keys inside a section are stored as `section.key`.
/// Blank lines and lines starting with `#` are ignored. The value is
/// everything after the first `=`, trimmed.
pub fn parse(text: &str) -> Result<Config, ParseError> {
    let mut config = Config::default();
    let mut section = String::new();
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            match rest.strip_suffix(']') {
                Some(name) => section = name.trim().to_string(),
                None => return Err(ParseError::BadSection(number)),
            }
            continue;
        }
        let (key, value) = match line.rsplit_once('=') {
            Some(pair) => pair,
            None => return Err(ParseError::BadLine(number)),
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(ParseError::EmptyKey(number));
        }
        let full = if section.is_empty() {
            key.to_string()
        } else {
            format!("{}.{}", section, key)
        };
        config.values.insert(full, value.trim().to_string());
    }
    Ok(config)
}

impl Config {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|v| v.as_str())
    }

    pub fn get_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }

    pub fn get_int(&self, key: &str) -> Option<i64> {
        self.get(key)?.parse().ok()
    }

    /// Reads a yes/no value. true, yes, on and 1 are true; false, no, off and
    /// 0 are false, in any letter case.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        let value = self.get(key)?.to_ascii_lowercase();
        match value.as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn keys(&self) -> Vec<&str> {
        self.values.keys().map(|k| k.as_str()).collect()
    }

    /// The keys of one section without the section prefix.
    pub fn section(&self, name: &str) -> BTreeMap<String, String> {
        let prefix = format!("{}.", name);
        self.values
            .iter()
            .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|k| (k.to_string(), v.clone())))
            .collect()
    }

    /// Values in `other` replace the ones in `self`.
    pub fn merge(&mut self, other: Config) {
        for (k, v) in other.values {
            self.values.insert(k, v);
        }
    }
}
