//! A small JSON library: strict parser with line and column errors, compact
//! and pretty output, paths, JSON pointers and merge patches.

use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

// -- errors ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    /// Line, counting from 1.
    pub line: usize,
    /// Column in characters, counting from 1.
    pub column: usize,
    /// Byte offset into the text.
    pub offset: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at line {} column {}", self.message, self.line, self.column)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// How deeply arrays and objects may nest.
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_depth: 64 }
    }
}

// -- parsing -----------------------------------------------------------------------

/// Parse one JSON document. Anything but whitespace after it is an error.
pub fn parse(text: &str) -> Result<Value, ParseError> {
    parse_with(text, Limits::default())
}

pub fn parse_with(text: &str, limits: Limits) -> Result<Value, ParseError> {
    let mut p = Parser {
        text,
        src: text.as_bytes(),
        pos: 0,
        depth: 0,
        limits,
    };
    p.skip_ws();
    let value = p.parse_value()?;
    p.skip_ws();
    if p.pos < p.src.len() {
        return Err(p.err("unexpected text after the value"));
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a str,
    src: &'a [u8],
    pos: usize,
    depth: usize,
    limits: Limits,
}

impl<'a> Parser<'a> {
    fn err(&self, message: &str) -> ParseError {
        let before = &self.text[..self.pos.min(self.text.len())];
        let line = before.matches('\n').count() + 1;
        let start = before.rfind('\n').map_or(0, |i| i + 1);
        ParseError {
            message: message.to_string(),
            line,
            column: before[start..].chars().count() + 1,
            offset: self.pos,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), ParseError> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected '{}'", byte as char)))
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), ParseError> {
        if self.src[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(())
        } else {
            Err(self.err(&format!("expected {word}")))
        }
    }

    fn parse_value(&mut self) -> Result<Value, ParseError> {
        match self.peek() {
            None => Err(self.err("unexpected end of text")),
            Some(b'n') => {
                self.literal("null")?;
                Ok(Value::Null)
            }
            Some(b't') => {
                self.literal("true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Value::Bool(true))
            }
            Some(b'"') => Ok(Value::String(self.parse_string()?)),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_object(),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn parse_number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err("leading zeros are not allowed"));
                }
            }
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.err("expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.err("expected a digit in the exponent"));
            }
        }
        let text = &self.text[start..self.pos];
        text.parse::<f64>()
            .map(Value::Number)
            .map_err(|_| self.err("number out of range"))
    }

    fn parse_hex4(&mut self) -> Result<u32, ParseError> {
        if self.pos + 4 > self.src.len() {
            return Err(self.err("short \\u escape"));
        }
        let bytes = &self.src[self.pos..self.pos + 4];
        if !bytes.iter().all(u8::is_ascii_hexdigit) {
            return Err(self.err("bad \\u escape"));
        }
        let digits = std::str::from_utf8(bytes).map_err(|_| self.err("bad \\u escape"))?;
        let value = u32::from_str_radix(digits, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.pos += 4;
        Ok(value)
    }

    fn parse_string(&mut self) -> Result<String, ParseError> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let byte = match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b) => b,
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                0..=0x1f => return Err(self.err("control character in string")),
                b'\\' => {
                    self.pos += 1;
                    let esc = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.pos += 1;
                    let ch = match esc {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.parse_unicode_escape()?,
                        _ => return Err(self.err("unknown escape")),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                _ => {
                    out.push(byte);
                    self.pos += 1;
                }
            }
        }
        String::from_utf8(out).map_err(|_| self.err("invalid UTF-8 in string"))
    }

    /// Read the part of `\uXXXX` after the `u`. A high surrogate must be
    /// followed by `\uXXXX` with a low surrogate.
    fn parse_unicode_escape(&mut self) -> Result<char, ParseError> {
        let first = self.parse_hex4()?;
        let code = if (0xD800..0xDC00).contains(&first) {
            if !self.src[self.pos..].starts_with(b"\\u") {
                return Err(self.err("lone high surrogate"));
            }
            self.pos += 2;
            let second = self.parse_hex4()?;
            if !(0xDC00..0xE000).contains(&second) {
                return Err(self.err("expected a low surrogate"));
            }
            0x10000 + ((first - 0xD800) << 8) + (second - 0xDC00)
        } else if (0xDC00..0xE000).contains(&first) {
            return Err(self.err("lone low surrogate"));
        } else {
            first
        };
        char::from_u32(code).ok_or_else(|| self.err("bad code point"))
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > self.limits.max_depth {
            return Err(self.err("nesting is too deep"));
        }
        Ok(())
    }

    fn parse_array(&mut self) -> Result<Value, ParseError> {
        self.expect(b'[')?;
        self.enter()?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
        self.depth -= 1;
        Ok(Value::Array(items))
    }

    fn parse_object(&mut self) -> Result<Value, ParseError> {
        self.expect(b'{')?;
        self.enter()?;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected a string key"));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let value = self.parse_value()?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
        self.depth -= 1;
        Ok(Value::Object(map))
    }
}

// -- reading values ----------------------------------------------------------------------

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// The number as an integer, when it has no fraction and fits in an i64.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => Some(*n as i64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?.get(key)
    }

    pub fn at(&self, index: usize) -> Option<&Value> {
        self.as_array()?.get(index)
    }

    /// How deeply the value nests. Scalars and empty containers are 0 deep.
    pub fn depth(&self) -> usize {
        match self {
            Value::Array(a) if !a.is_empty() => 1 + a.iter().map(Value::depth).max().unwrap_or(0),
            Value::Object(o) if !o.is_empty() => {
                1 + o.values().map(Value::depth).max().unwrap_or(0)
            }
            _ => 0,
        }
    }

    /// Number of values in the tree, counting the root and every container.
    pub fn count(&self) -> usize {
        match self {
            Value::Array(a) => 1 + a.iter().map(Value::count).sum::<usize>(),
            Value::Object(o) => 1 + o.values().map(Value::count).sum::<usize>(),
            _ => 1,
        }
    }
}

// -- writing ---------------------------------------------------------------------------------

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_number(n: f64, out: &mut String) {
    if !n.is_finite() {
        out.push_str("null");
    } else if n.fract() == 0.0 && n.abs() < 1.0e15 {
        out.push_str(&format!("{}", n as i64));
    } else {
        out.push_str(&format!("{n}"));
    }
}

fn write_compact(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(*n, out),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_compact(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_compact(item, out);
            }
            out.push('}');
        }
    }
}

fn write_pretty(v: &Value, indent: usize, depth: usize, out: &mut String) {
    match v {
        Value::Array(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&" ".repeat(indent * (depth + 1)));
                write_pretty(item, indent, depth + 1, out);
                out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
            }
            out.push_str(&" ".repeat(indent * depth));
            out.push(']');
        }
        Value::Object(map) if !map.is_empty() => {
            out.push_str("{\n");
            for (i, (key, item)) in map.iter().enumerate() {
                out.push_str(&" ".repeat(indent));
                write_string(key, out);
                out.push_str(": ");
                write_pretty(item, indent, depth + 1, out);
                out.push_str(if i + 1 < map.len() { ",\n" } else { "\n" });
            }
            out.push_str(&" ".repeat(indent * depth));
            out.push('}');
        }
        other => write_compact(other, out),
    }
}

impl Value {
    /// Compact text: no spaces, keys in sorted order.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        write_compact(self, &mut out);
        out
    }

    /// Indented text with `indent` spaces per level.
    pub fn to_pretty(&self, indent: usize) -> String {
        let mut out = String::new();
        write_pretty(self, indent, 0, &mut out);
        out
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_json())
    }
}

// -- building values ---------------------------------------------------------------------------

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Value {
        Value::Number(n)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Value {
        Value::Number(n as f64)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::String(s.to_string())
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(items: Vec<T>) -> Value {
        Value::Array(items.into_iter().map(Into::into).collect())
    }
}

/// Build an object from pairs.
pub fn object<K: Into<String>, V: Into<Value>>(pairs: Vec<(K, V)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.into(), v.into())).collect())
}

// -- paths ------------------------------------------------------------------------------------------

/// One step of a path such as `a.b[2].c`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Key(String),
    Index(usize),
}

/// Split a path into steps. Returns None for a path that is not well formed.
pub fn split_path(path: &str) -> Option<Vec<Step>> {
    let mut steps = Vec::new();
    let mut key = String::new();
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '.' => {
                if !key.is_empty() {
                    steps.push(Step::Key(std::mem::take(&mut key)));
                }
            }
            '[' => {
                if !key.is_empty() {
                    steps.push(Step::Key(std::mem::take(&mut key)));
                }
                let mut digits = String::new();
                loop {
                    match chars.next()? {
                        ']' => break,
                        d if d.is_ascii_digit() => digits.push(d),
                        _ => return None,
                    }
                }
                steps.push(Step::Index(digits.parse().ok()?));
            }
            c => key.push(c),
        }
    }
    if !key.is_empty() {
        steps.push(Step::Key(key));
    }
    Some(steps)
}

impl Value {
    /// Follow a path such as `a.b[2].c`.
    pub fn get_path(&self, path: &str) -> Option<&Value> {
        let mut node = self;
        for step in split_path(path)? {
            node = match step {
                Step::Key(k) => node.get(&k)?,
                Step::Index(i) => node.at(i)?,
            };
        }
        Some(node)
    }

    /// Follow an RFC 6901 pointer such as `/a/b/2`. `~1` stands for `/` and
    /// `~0` for `~`.
    pub fn pointer(&self, pointer: &str) -> Option<&Value> {
        if pointer.is_empty() {
            return Some(self);
        }
        let rest = pointer.strip_prefix('/')?;
        let mut node = self;
        for raw in rest.split('/') {
            let part = raw.replace("~0", "~").replace("~1", "/");
            node = match node {
                Value::Object(map) => map.get(&part)?,
                Value::Array(items) => {
                    if part.len() > 1 && part.starts_with('0') {
                        return None;
                    }
                    items.get(part.parse::<usize>().ok()?)?
                }
                _ => return None,
            };
        }
        Some(node)
    }

    /// Every leaf with the pointer that reaches it, in key order.
    pub fn leaves(&self) -> Vec<(String, &Value)> {
        let mut out = Vec::new();
        collect_leaves(self, String::new(), &mut out);
        out
    }
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn collect_leaves<'a>(v: &'a Value, here: String, out: &mut Vec<(String, &'a Value)>) {
    match v {
        Value::Array(items) if !items.is_empty() => {
            for (i, item) in items.iter().enumerate() {
                collect_leaves(item, format!("{here}/{i}"), out);
            }
        }
        Value::Object(map) if !map.is_empty() => {
            for (key, item) in map {
                collect_leaves(item, format!("{here}/{}", escape_pointer(key)), out);
            }
        }
        leaf => out.push((here, leaf)),
    }
}

// -- merge patch ---------------------------------------------------------------------------------------

/// Apply an RFC 7386 merge patch. A null in the patch removes the key, an
/// object merges, and anything else replaces.
pub fn merge_patch(target: &Value, patch: &Value) -> Value {
    match patch {
        Value::Object(changes) => {
            let mut map = match target {
                Value::Object(existing) => existing.clone(),
                _ => BTreeMap::new(),
            };
            for (key, change) in changes {
                if change.is_null() {
                    map.remove(key);
                } else {
                    let current = map.get(key).cloned().unwrap_or(Value::Null);
                    map.insert(key.clone(), merge_patch(&current, change));
                }
            }
            Value::Object(map)
        }
        other => other.clone(),
    }
}
