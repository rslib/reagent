//! Syntax tokens for one line of a fenced code block.
//!
//! [`tokenize`] splits a line into spans of a small token set and carries an
//! open block comment or a string that spans lines to the next line, so a
//! caller paints a block line by line and holds no more than one line's spans.
//!
//! Nothing here allocates: a language is one static table entry, a keyword is
//! found by binary search, and the only output is the span `Vec` the caller
//! reuses. A span is a byte range of the line, so no text is copied.

mod lang;

#[cfg(test)]
mod tests;

use self::lang::{Flavor, Language, contains, contains_ignoring_case};

pub use self::lang::Lang;

/// What a span of code is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// Plain text: whitespace, an unknown character, a plain word.
    Text,
    /// A reserved word.
    Keyword,
    /// A string or a character literal.
    String,
    /// A comment.
    Comment,
    /// A number, with its base, separators, exponent and suffix.
    Number,
    /// A type: a built-in name, a capitalised name or a lifetime.
    Type,
    /// A call or a command.
    Function,
    /// A macro invocation.
    Macro,
    /// Punctuation.
    Punct,
    /// An added line of a diff.
    Added,
    /// A removed line of a diff.
    Removed,
}

/// What a line leaves open for the next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    /// The block comment or string the next line starts inside.
    open: Open,
}

/// What a line leaves open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Open {
    /// Nothing: the next line starts outside a comment and outside a string.
    #[default]
    Nothing,
    /// A block comment, closed by the language's block comment close.
    Comment,
    /// A string that spans lines, and the delimiter that closes it.
    String(&'static str),
}

/// One span of a line: its byte range and its token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// The first byte of the span.
    pub start: u32,
    /// One past the last byte of the span.
    pub end: u32,
    /// What the span is.
    pub token: Token,
}

/// Splits `line` into spans that cover it whole, in order, adjacent spans of
/// the same token merged; reuses `out` (clears it first); updates `state`.
pub fn tokenize(lang: &Lang, line: &str, state: &mut State, out: &mut Vec<Span>) {
    out.clear();
    let language = lang.table();
    let mut sink = Sink { line, out };
    match language.flavor {
        Flavor::Code => code(language, &mut sink, state),
        Flavor::Json | Flavor::Toml | Flavor::Yaml => {
            state.open = Open::Nothing;
            data(language, &mut sink);
        }
        Flavor::Diff => {
            state.open = Open::Nothing;
            diff(&mut sink);
        }
    }
}

/// The spans of one line, adjacent equal tokens merged.
struct Sink<'a> {
    /// The line the spans are ranges of.
    line: &'a str,
    /// Where the spans go.
    out: &'a mut Vec<Span>,
}

impl Sink<'_> {
    /// Adds the span `start..end` unless it is empty.
    fn push(&mut self, start: usize, end: usize, token: Token) {
        if start >= end {
            return;
        }
        if let Some(last) = self.out.last_mut()
            && last.end as usize == start
            && last.token == token
        {
            last.end = end as u32;
            return;
        }
        self.out.push(Span {
            start: start as u32,
            end: end as u32,
            token,
        });
    }
}

/// A line of code: comments, strings, numbers, words and punctuation.
fn code(language: &Language, sink: &mut Sink<'_>, state: &mut State) {
    let line = sink.line;
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut at = 0;

    let open = state.open;
    state.open = Open::Nothing;
    match open {
        Open::Nothing => {}
        Open::Comment => match close(bytes, 0, comment_close(language)) {
            Some(end) => {
                sink.push(0, end, Token::Comment);
                at = end;
            }
            None => {
                sink.push(0, len, Token::Comment);
                state.open = open;
                return;
            }
        },
        Open::String(delimiter) => match close_string(bytes, 0, delimiter) {
            Some(end) => {
                sink.push(0, end, Token::String);
                at = end;
            }
            None => {
                sink.push(0, len, Token::String);
                state.open = open;
                return;
            }
        },
    }

    let mut first = true;
    while at < len {
        let rest = &bytes[at..];
        let byte = bytes[at];

        if let Some((start, end)) = language.block_comment
            && rest.starts_with(start.as_bytes())
        {
            match close(bytes, at + start.len(), end) {
                Some(close) => {
                    sink.push(at, close, Token::Comment);
                    at = close;
                }
                None => {
                    sink.push(at, len, Token::Comment);
                    state.open = Open::Comment;
                    return;
                }
            }
            continue;
        }

        if language
            .line_comment
            .iter()
            .any(|marker| rest.starts_with(marker.as_bytes()))
        {
            sink.push(at, len, Token::Comment);
            return;
        }

        if language.lifetimes && byte == b'\'' {
            let (end, token) = quote(bytes, at);
            sink.push(at, end, token);
            at = end;
            continue;
        }

        if let Some(found) = language
            .strings
            .iter()
            .find(|delimiter| rest.starts_with(delimiter.as_bytes()))
        {
            let delimiter = *found;
            match close_string(bytes, at + delimiter.len(), delimiter) {
                Some(end) => {
                    sink.push(at, end, Token::String);
                    at = end;
                }
                None => {
                    sink.push(at, len, Token::String);
                    if spans_lines(delimiter) {
                        state.open = Open::String(delimiter);
                    }
                    return;
                }
            }
            continue;
        }

        if language.vars && byte == b'$' {
            let end = if bytes.get(at + 1) == Some(&b'{') {
                close(bytes, at + 2, "}").unwrap_or(len)
            } else {
                word_end(bytes, at + 1)
            };
            let token = if end > at + 1 {
                Token::Type
            } else {
                Token::Text
            };
            sink.push(at, end.max(at + 1), token);
            at = end.max(at + 1);
            continue;
        }

        if byte.is_ascii_digit() {
            let end = number_end(bytes, at);
            sink.push(at, end, Token::Number);
            at = end;
            continue;
        }

        if is_word_start(byte) {
            let mut end = word_end(bytes, at);
            let word = &line[at..end];
            let next = bytes.get(end).copied();
            let keyword = if language.icase {
                contains_ignoring_case(language.keywords, word)
            } else {
                contains(language.keywords, word)
            };
            let token = if keyword {
                Token::Keyword
            } else if contains(language.types, word) || (language.caps && byte.is_ascii_uppercase())
            {
                Token::Type
            } else if language.macros && next == Some(b'!') {
                end += 1;
                Token::Macro
            } else if (language.calls && next == Some(b'(')) || (language.command && first) {
                Token::Function
            } else {
                Token::Text
            };
            sink.push(at, end, token);
            at = end;
            if token != Token::Keyword {
                first = false;
            }
            continue;
        }

        if is_punct(byte) {
            sink.push(at, at + 1, Token::Punct);
            at += 1;
            continue;
        }

        let step = char_len(bytes, at);
        sink.push(at, at + step, Token::Text);
        at += step;
    }
}

/// A line of JSON, TOML or YAML: keys, strings, numbers, literals and comments.
fn data(language: &Language, sink: &mut Sink<'_>) {
    let line = sink.line;
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut at = 0;

    if language.flavor == Flavor::Toml
        && let Some(start) = bytes.iter().position(|byte| !byte.is_ascii_whitespace())
        && bytes[start] == b'['
        && let Some(end) = close(&bytes[start..], 1, "]").map(|end| start + end)
    {
        let end = if bytes.get(end) == Some(&b']') {
            end + 1
        } else {
            end
        };
        sink.push(start, end, Token::Type);
        at = end;
    }

    let colon = matches!(language.flavor, Flavor::Json | Flavor::Yaml);
    let assign = language.flavor == Flavor::Toml;

    while at < len {
        let byte = bytes[at];

        if language.flavor != Flavor::Json && byte == b'#' {
            sink.push(at, len, Token::Comment);
            return;
        }

        if byte == b'"' || (byte == b'\'' && language.flavor != Flavor::Json) {
            match close_quoted(bytes, at + 1, byte) {
                Some(end) => {
                    let key = is_key(bytes, skip_space(bytes, end), colon, assign);
                    sink.push(at, end, if key { Token::Type } else { Token::String });
                    at = end;
                }
                None => {
                    sink.push(at, len, Token::String);
                    return;
                }
            }
            continue;
        }

        if byte.is_ascii_digit()
            || (matches!(byte, b'-' | b'+') && bytes.get(at + 1).is_some_and(u8::is_ascii_digit))
        {
            let end = number_end(bytes, at);
            sink.push(at, end, Token::Number);
            at = end;
            continue;
        }

        if is_word_start(byte) {
            let end = key_end(bytes, at, language.flavor);
            let word = &line[at..end];
            let token = if contains(language.keywords, word) {
                Token::Keyword
            } else if language.flavor != Flavor::Json
                && is_key(bytes, skip_space(bytes, end), colon, assign)
            {
                Token::Type
            } else {
                Token::Text
            };
            sink.push(at, end, token);
            at = end;
            continue;
        }

        if matches!(byte, b'{' | b'}' | b'[' | b']' | b':' | b',' | b'=') {
            sink.push(at, at + 1, Token::Punct);
            at += 1;
            continue;
        }

        let step = char_len(bytes, at);
        sink.push(at, at + step, Token::Text);
        at += step;
    }
}

/// A line of a unified diff: an added line, a removed line or a hunk header.
fn diff(sink: &mut Sink<'_>) {
    let bytes = sink.line.as_bytes();
    let token = match bytes {
        [b'+', ..] => Token::Added,
        [b'-', ..] => Token::Removed,
        [b'@', b'@', ..] => Token::Comment,
        _ => Token::Text,
    };
    sink.push(0, bytes.len(), token);
}

/// A rust quote: one past its end, and whether it is a char literal or a
/// lifetime.
fn quote(bytes: &[u8], at: usize) -> (usize, Token) {
    let start = at + 1;
    if bytes.get(start).is_some_and(|byte| is_word_start(*byte)) {
        let end = word_end(bytes, start);
        if bytes.get(end) == Some(&b'\'') {
            return (end + 1, Token::String);
        }
        return (end, Token::Type);
    }
    // An escape, as in `'\n'` or `'\u{1F600}'`: the literal closes on a quote.
    let stop = (start + 12).min(bytes.len());
    match bytes[start..stop].iter().position(|byte| *byte == b'\'') {
        Some(offset) => (start + offset + 1, Token::String),
        None => (start, Token::Type),
    }
}

/// The closer of the block comment the next line continues, or the C one.
fn comment_close(language: &Language) -> &'static str {
    language.block_comment.map_or("*/", |(_, end)| end)
}

/// Whether a delimiter's string can run past the end of a line.
fn spans_lines(delimiter: &str) -> bool {
    delimiter.len() > 1 || delimiter == "`"
}

/// One past the closer that ends a comment that starts at `from`.
fn close(bytes: &[u8], from: usize, closer: &str) -> Option<usize> {
    find(bytes, from, closer).map(|at| at + closer.len())
}

/// One past the delimiter that ends a string whose body starts at `from`.
fn close_string(bytes: &[u8], from: usize, delimiter: &str) -> Option<usize> {
    let mut at = from;
    while at < bytes.len() {
        if bytes[at..].starts_with(delimiter.as_bytes()) {
            return Some(at + delimiter.len());
        }
        at += if bytes[at] == b'\\' {
            2
        } else {
            char_len(bytes, at)
        };
    }
    None
}

/// One past the quote that ends a data string whose body starts at `from`.
fn close_quoted(bytes: &[u8], from: usize, quote: u8) -> Option<usize> {
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] == quote {
            return Some(at + 1);
        }
        at += if bytes[at] == b'\\' && quote == b'"' {
            2
        } else {
            char_len(bytes, at)
        };
    }
    None
}

/// The first index at or after `from` where `needle` starts.
fn find(bytes: &[u8], from: usize, needle: &str) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }
    bytes[from..]
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())
        .map(|at| from + at)
}

/// The first index after `from` that is not a space or a tab.
fn skip_space(bytes: &[u8], from: usize) -> usize {
    let mut at = from;
    while matches!(bytes.get(at), Some(b' ' | b'\t')) {
        at += 1;
    }
    at
}

/// Whether a string or a word at `at` names a key, by the marker after it.
fn is_key(bytes: &[u8], at: usize, colon: bool, assign: bool) -> bool {
    match bytes.get(at) {
        Some(b':') => colon,
        Some(b'=') => assign,
        _ => false,
    }
}

/// The length of the character at `at`, one byte when it is past the end.
fn char_len(bytes: &[u8], at: usize) -> usize {
    match bytes.get(at) {
        Some(byte) if *byte >= 0x80 => (*byte).leading_ones() as usize,
        _ => 1,
    }
}

/// Whether a byte can start a word.
fn is_word_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

/// Whether a byte can be inside a word.
fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

/// Whether a byte is punctuation.
fn is_punct(byte: u8) -> bool {
    matches!(
        byte,
        b'{' | b'}'
            | b'('
            | b')'
            | b'['
            | b']'
            | b';'
            | b','
            | b'.'
            | b':'
            | b'='
            | b'<'
            | b'>'
            | b'+'
            | b'-'
            | b'*'
            | b'/'
            | b'&'
            | b'|'
            | b'!'
            | b'?'
    )
}

/// One past the last byte of the word that starts at `at`.
fn word_end(bytes: &[u8], at: usize) -> usize {
    let mut end = at;
    while end < bytes.len() && is_word(bytes[end]) {
        end += char_len(bytes, end);
    }
    end
}

/// One past the last byte of a key that starts at `at`; TOML and YAML keys
/// carry a dash.
fn key_end(bytes: &[u8], at: usize, flavor: Flavor) -> usize {
    let mut end = word_end(bytes, at);
    if matches!(flavor, Flavor::Toml | Flavor::Yaml) {
        while bytes.get(end) == Some(&b'-') && bytes.get(end + 1).is_some_and(|byte| is_word(*byte))
        {
            end = word_end(bytes, end + 1);
        }
    }
    end
}

/// One past the last byte of the number that starts at `at`, with its base, its
/// separators, its exponent and its suffix.
fn number_end(bytes: &[u8], at: usize) -> usize {
    let mut end = at;
    while let Some(byte) = bytes.get(end).copied() {
        match byte {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'_' => end += 1,
            b'.' if bytes.get(end + 1).is_some_and(u8::is_ascii_digit) => end += 1,
            b'+' | b'-' if end > at && matches!(bytes.get(end - 1), Some(b'e' | b'E')) => {
                end += 1;
            }
            _ => break,
        }
    }
    end
}
