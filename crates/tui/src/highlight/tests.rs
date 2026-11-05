//! One test per language, plus the state, the merging and the coverage rules.

use super::*;

/// The spans of a line, as its token and its text.
fn spans<'a>(lang: &Lang, line: &'a str, state: &mut State) -> Vec<(Token, &'a str)> {
    let mut out = Vec::new();
    tokenize(lang, line, state, &mut out);
    out.iter()
        .map(|span| (span.token, &line[span.start as usize..span.end as usize]))
        .collect()
}

/// The spans of a line that starts outside a comment and outside a string.
fn line_spans<'a>(tag: &str, line: &'a str) -> Vec<(Token, &'a str)> {
    let lang = Lang::from_tag(tag).expect("a known tag");
    spans(&lang, line, &mut State::default())
}

/// The token of the span that covers the first `word` of a line.
fn token_of(tag: &str, line: &str, word: &str) -> Token {
    let at = line.find(word).expect("the word is in the line");
    let mut offset = 0;
    for (token, text) in line_spans(tag, line) {
        if (offset..offset + text.len()).contains(&at) {
            return token;
        }
        offset += text.len();
    }
    panic!("no span covers {word:?} in {line:?}");
}

/// The text of the span that starts at the first `word` of a line.
fn span_at<'a>(tag: &str, line: &'a str, word: &str) -> &'a str {
    let at = line.find(word).expect("the word is in the line");
    let mut offset = 0;
    for (_, text) in line_spans(tag, line) {
        if offset == at {
            return text;
        }
        offset += text.len();
    }
    panic!("no span starts at {word:?} in {line:?}");
}

#[test]
fn rust() {
    let line = "let total: u32 = count(1); // sum";
    assert_eq!(token_of("rust", line, "let"), Token::Keyword);
    assert_eq!(token_of("rust", line, "total"), Token::Text);
    assert_eq!(token_of("rust", line, "u32"), Token::Type);
    assert_eq!(token_of("rust", line, "count"), Token::Function);
    assert_eq!(token_of("rust", line, "1"), Token::Number);
    assert_eq!(span_at("rust", line, "// sum"), "// sum");
    assert_eq!(token_of("rust", line, "// sum"), Token::Comment);
}

#[test]
fn python() {
    let line = "def parse(text: str) -> int:  # note";
    assert_eq!(token_of("python", line, "def"), Token::Keyword);
    assert_eq!(token_of("python", line, "parse"), Token::Function);
    assert_eq!(token_of("python", line, "str"), Token::Type);
    assert_eq!(token_of("python", line, "int"), Token::Type);
    assert_eq!(token_of("python", line, "# note"), Token::Comment);
}

#[test]
fn javascript() {
    let line = "const name = greet(\"hi\"); // say";
    assert_eq!(token_of("js", line, "const"), Token::Keyword);
    assert_eq!(token_of("js", line, "name"), Token::Text);
    assert_eq!(token_of("js", line, "greet"), Token::Function);
    assert_eq!(span_at("js", line, "\"hi\""), "\"hi\"");
    assert_eq!(token_of("js", line, "// say"), Token::Comment);
}

#[test]
fn typescript() {
    let line = "const item: Widget = make(1); // ok";
    assert_eq!(token_of("ts", line, "const"), Token::Keyword);
    assert_eq!(token_of("ts", line, "Widget"), Token::Type);
    assert_eq!(token_of("ts", line, "make"), Token::Function);
    assert_eq!(token_of("ts", line, "1"), Token::Number);
}

#[test]
fn go() {
    let line = "func main() { total := sum(1) }";
    assert_eq!(token_of("go", line, "func"), Token::Keyword);
    assert_eq!(token_of("go", line, "main"), Token::Function);
    assert_eq!(token_of("go", line, "total"), Token::Text);
    assert_eq!(token_of("go", line, "sum"), Token::Function);
    assert_eq!(token_of("go", line, "1"), Token::Number);
}

#[test]
fn c() {
    let line = "int main(void) { return 0; }";
    assert_eq!(token_of("c", line, "int"), Token::Keyword);
    assert_eq!(token_of("c", line, "main"), Token::Function);
    assert_eq!(token_of("c", line, "void"), Token::Keyword);
    assert_eq!(token_of("c", line, "return"), Token::Keyword);
    assert_eq!(token_of("c", line, "0"), Token::Number);
}

#[test]
fn cpp() {
    let line = "class Widget { Widget(int size); };";
    assert_eq!(token_of("cpp", line, "class"), Token::Keyword);
    assert_eq!(token_of("cpp", line, "Widget"), Token::Type);
    assert_eq!(token_of("cpp", line, "int"), Token::Keyword);
}

#[test]
fn java() {
    let line = "public class Main { int size = 1; }";
    assert_eq!(token_of("java", line, "public"), Token::Keyword);
    assert_eq!(token_of("java", line, "class"), Token::Keyword);
    assert_eq!(token_of("java", line, "Main"), Token::Type);
    assert_eq!(token_of("java", line, "1"), Token::Number);
}

#[test]
fn shell() {
    let line = "grep -r \"foo\" $HOME # find";
    assert_eq!(token_of("sh", line, "grep"), Token::Function);
    assert_eq!(span_at("sh", line, "\"foo\""), "\"foo\"");
    assert_eq!(span_at("sh", line, "$HOME"), "$HOME");
    assert_eq!(token_of("sh", line, "$HOME"), Token::Type);
    assert_eq!(token_of("sh", line, "# find"), Token::Comment);

    let braces = "cp ${HOME} /tmp";
    assert_eq!(span_at("sh", braces, "${HOME}"), "${HOME}");
    assert_eq!(token_of("sh", braces, "cp"), Token::Function);
}

#[test]
fn json() {
    let line = "{\"name\": \"reagent\", \"count\": 12, \"flag\": true}";
    assert_eq!(span_at("json", line, "\"name\""), "\"name\"");
    assert_eq!(token_of("json", line, "\"name\""), Token::Type);
    assert_eq!(token_of("json", line, "\"reagent\""), Token::String);
    assert_eq!(token_of("json", line, "12"), Token::Number);
    assert_eq!(token_of("json", line, "true"), Token::Keyword);
}

#[test]
fn toml() {
    let line = "name = \"reagent\"  # crate";
    assert_eq!(token_of("toml", line, "name"), Token::Type);
    assert_eq!(token_of("toml", line, "\"reagent\""), Token::String);
    assert_eq!(token_of("toml", line, "# crate"), Token::Comment);

    assert_eq!(span_at("toml", "[package]", "[package]"), "[package]");
    assert_eq!(token_of("toml", "[package]", "package"), Token::Type);
    assert_eq!(token_of("toml", "[[bin]]", "bin"), Token::Type);
}

#[test]
fn yaml() {
    let line = "name: reagent  # note";
    assert_eq!(token_of("yaml", line, "name"), Token::Type);
    assert_eq!(token_of("yaml", line, "reagent"), Token::Text);
    assert_eq!(token_of("yaml", line, "# note"), Token::Comment);

    assert_eq!(token_of("yaml", "count: 12", "12"), Token::Number);
    assert_eq!(token_of("yml", "flag: true", "true"), Token::Keyword);
}

#[test]
fn sql() {
    let line = "SELECT id, name FROM users WHERE id = 1; -- pick";
    assert_eq!(token_of("sql", line, "SELECT"), Token::Keyword);
    assert_eq!(token_of("sql", line, "FROM"), Token::Keyword);
    assert_eq!(token_of("sql", line, "WHERE"), Token::Keyword);
    assert_eq!(token_of("sql", line, "id"), Token::Text);
    assert_eq!(token_of("sql", line, "1"), Token::Number);
    assert_eq!(token_of("sql", line, "-- pick"), Token::Comment);
}

#[test]
fn ruby() {
    let line = "def hello(name) puts \"hi\" end";
    assert_eq!(token_of("ruby", line, "def"), Token::Keyword);
    assert_eq!(token_of("ruby", line, "hello"), Token::Function);
    assert_eq!(token_of("ruby", line, "\"hi\""), Token::String);
    assert_eq!(token_of("ruby", line, "end"), Token::Keyword);
}

#[test]
fn lua() {
    let line = "local total = count(1) -- note";
    assert_eq!(token_of("lua", line, "local"), Token::Keyword);
    assert_eq!(token_of("lua", line, "count"), Token::Function);
    assert_eq!(token_of("lua", line, "1"), Token::Number);
    assert_eq!(token_of("lua", line, "-- note"), Token::Comment);
}

#[test]
fn diff() {
    assert_eq!(
        line_spans("diff", "+let x = 1;"),
        [(Token::Added, "+let x = 1;")]
    );
    assert_eq!(
        line_spans("patch", "-let x = 2;"),
        [(Token::Removed, "-let x = 2;")]
    );
    assert_eq!(
        line_spans("diff", "@@ -1,2 +3,4 @@"),
        [(Token::Comment, "@@ -1,2 +3,4 @@")]
    );
    assert_eq!(
        line_spans("diff", " let x = 3;"),
        [(Token::Text, " let x = 3;")]
    );
}

/// A typical line of every language, for the span checks.
const LINES: [(&str, &str); 18] = [
    ("rust", "let size: u32 = count(1); // sum"),
    ("rs", "fn run<'a>(text: &'a str) {}"),
    ("python", "def parse(text: str) -> int:  # note"),
    ("js", "const name = greet(\"hi\");"),
    ("tsx", "const item: Widget = make(1);"),
    ("go", "func main() { total := sum(1) }"),
    ("c", "int main(void) { /* one"),
    ("h", "#define SIZE 4"),
    ("cpp", "class Widget { int size; };"),
    ("java", "public class Main { int size = 1; }"),
    ("sh", "grep -r \"foo\" $HOME # find"),
    ("console", "$ cargo test --all"),
    ("json", "{\"count\": 12, \"flag\": true}"),
    ("toml", "[package] name = \"reagent\""),
    ("yaml", "name: reagent  # note"),
    ("sql", "SELECT id FROM users WHERE id = 1;"),
    ("lua", "local total = count(1) -- note"),
    ("diff", "@@ -1,2 +3,4 @@"),
];

#[test]
fn spans_cover_the_line_without_gaps() {
    for (tag, line) in LINES {
        let lang = Lang::from_tag(tag).expect("a known tag");
        let mut state = State::default();
        let mut out = Vec::new();
        tokenize(&lang, line, &mut state, &mut out);
        assert!(!out.is_empty(), "{tag}: {line}");
        let mut at = 0;
        for span in &out {
            assert_eq!(span.start as usize, at, "{tag}: {line}");
            assert!(span.end > span.start, "{tag}: {line}");
            at = span.end as usize;
        }
        assert_eq!(at, line.len(), "{tag}: {line}");
        for pair in out.windows(2) {
            assert_ne!(pair[0].token, pair[1].token, "{tag}: {line}");
        }
    }
}

#[test]
fn a_reused_buffer_holds_only_the_last_line() {
    let lang = Lang::from_tag("rust").expect("a known tag");
    let mut state = State::default();
    let mut out = Vec::new();
    tokenize(&lang, "let count = 1;", &mut state, &mut out);
    let first = out.len();
    tokenize(&lang, "let", &mut state, &mut out);
    assert!(out.len() < first, "{out:?}");
    assert_eq!(out.last().map(|span| span.end as usize), Some(3));
}

#[test]
fn a_block_comment_carries_to_the_next_line() {
    let lang = Lang::from_tag("c").expect("a known tag");
    let mut state = State::default();
    let first = spans(&lang, "int size = 1; /* one", &mut state);
    assert_eq!(first.last(), Some(&(Token::Comment, "/* one")));
    assert_ne!(state, State::default());
    let second = spans(&lang, "two */ int size = 2;", &mut state);
    assert_eq!(second.first(), Some(&(Token::Comment, "two */")));
    assert_eq!(state, State::default());
    assert!(second.contains(&(Token::Number, "2")), "{second:?}");
}

#[test]
fn a_python_triple_quote_string_spans_lines() {
    let lang = Lang::from_tag("py").expect("a known tag");
    let mut state = State::default();
    let first = spans(&lang, "x = \"\"\"first", &mut state);
    assert_eq!(first.last(), Some(&(Token::String, "\"\"\"first")));
    assert_ne!(state, State::default());
    let second = spans(&lang, "still text", &mut state);
    assert_eq!(second, [(Token::String, "still text")]);
    assert_ne!(state, State::default());
    let third = spans(&lang, "last\"\"\" + 1", &mut state);
    assert_eq!(third.first(), Some(&(Token::String, "last\"\"\"")));
    assert_eq!(state, State::default());
    assert!(third.contains(&(Token::Number, "1")), "{third:?}");
}

#[test]
fn an_unknown_tag_has_no_lang() {
    assert!(Lang::from_tag("brainfuck").is_none());
    assert!(Lang::from_tag("").is_none());
    assert!(Lang::from_tag("rs").is_some());
    assert_eq!(Lang::from_tag("RUST").expect("a known tag").name(), "rust");
    assert_eq!(
        Lang::from_tag("Tsx").expect("a known tag").name(),
        "typescript"
    );
    assert_eq!(Lang::from_tag("patch").expect("a known tag").name(), "diff");
    assert_eq!(Lang::from_tag("bash").expect("a known tag").name(), "shell");
}

#[test]
fn escaped_quote_stays_in_the_string() {
    let line = "let s = \"a\\\"b\"; // note";
    assert_eq!(span_at("rust", line, "\"a\\\"b\""), "\"a\\\"b\"");
    assert_eq!(token_of("rust", line, "// note"), Token::Comment);

    let c = "puts(\"a\\\\\"); size(1);";
    assert_eq!(span_at("c", c, "\"a\\\\\""), "\"a\\\\\"");
    assert_eq!(token_of("c", c, "size"), Token::Function);
}

#[test]
fn rust_lifetime_is_a_type() {
    let line = "fn run<'a>(text: &'a str) -> &'a str { text }";
    assert_eq!(span_at("rust", line, "'a>"), "'a");
    assert_eq!(token_of("rust", line, "'a"), Token::Type);

    let literal = "let c = 'x';";
    assert_eq!(span_at("rust", literal, "'x'"), "'x'");
    assert_eq!(token_of("rust", literal, "'x'"), Token::String);
}

#[test]
fn rust_macro_is_a_macro() {
    let line = "assert_eq!(total(1), 2);";
    assert_eq!(span_at("rust", line, "assert_eq!"), "assert_eq!");
    assert_eq!(token_of("rust", line, "assert_eq!"), Token::Macro);
    assert_eq!(token_of("rust", line, "total"), Token::Function);
}

#[test]
fn utf8_text_is_split_on_char_boundaries() {
    let line = "let 名前 = \"héllo\" // 世界";
    let lang = Lang::from_tag("rust").expect("a known tag");
    let mut state = State::default();
    let mut out = Vec::new();
    tokenize(&lang, line, &mut state, &mut out);
    let mut rebuilt = String::new();
    for span in &out {
        assert!(line.is_char_boundary(span.start as usize), "{span:?}");
        assert!(line.is_char_boundary(span.end as usize), "{span:?}");
        rebuilt.push_str(&line[span.start as usize..span.end as usize]);
    }
    assert_eq!(rebuilt, line);
    assert_eq!(token_of("rust", line, "名前"), Token::Text);
    assert_eq!(span_at("rust", line, "\"héllo\""), "\"héllo\"");
    assert_eq!(token_of("rust", line, "// 世界"), Token::Comment);
}

#[test]
fn numbers_keep_their_base_separators_and_suffix() {
    for number in ["0x1f", "1_000", "1.5e3", "1e-9", "42u32", "0b1010"] {
        let line = format!("let n = {number};");
        assert_eq!(span_at("rust", &line, number), number, "{number}");
        assert_eq!(token_of("rust", &line, number), Token::Number, "{number}");
    }
}

#[test]
fn the_tables_are_sorted() {
    for language in super::lang::languages() {
        for list in [language.keywords, language.types] {
            assert!(
                list.windows(2).all(|pair| pair[0] < pair[1]),
                "{}: {list:?}",
                language.name
            );
        }
        let mut longest = usize::MAX;
        for delimiter in language.strings {
            assert!(
                delimiter.len() <= longest,
                "{}: {delimiter:?}",
                language.name
            );
            longest = delimiter.len();
        }
    }
}
