//! Plain file reads in a bash command, so the batch can run them as `read`.

use std::borrow::Cow;
use std::path::Path;

/// The lines one plain shell read asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lines {
    /// The whole file, as `cat` prints it.
    Whole,
    /// Lines `start` to `end`, both counted from 1.
    Range { start: usize, end: usize },
}

/// One plain read of one file: the folder a leading `cd` names, the path and
/// the lines.
#[derive(Debug, PartialEq, Eq)]
pub struct ShellRead<'a> {
    pub dir: Option<Cow<'a, str>>,
    pub path: Cow<'a, str>,
    pub lines: Lines,
}

impl ShellRead<'_> {
    /// The same read with owned text, to outlive the command it came from.
    pub fn into_owned(self) -> ShellRead<'static> {
        ShellRead {
            dir: self.dir.map(|dir| Cow::Owned(dir.into_owned())),
            path: Cow::Owned(self.path.into_owned()),
            lines: self.lines,
        }
    }
}

/// The plain read `command` is, or `None` for anything else.
///
/// Only a short list of forms counts, each after an optional `cd DIR &&`:
/// `cat F`, `cat -n F`, `head F`, `head -n N F`, `head -N F`, `sed -n 'Np' F`,
/// `sed -n 'A,Bp' F`, `nl -ba F | sed -n 'A,Bp'`, `awk 'NR>=A && NR<=B' F` and
/// `awk 'NR==N' F`. A command with any other shell syntax, such as `$`, a
/// glob, a redirect or a second command, is not one.
pub fn classify(command: &str) -> Option<ShellRead<'_>> {
    // Most commands are no read form: refuse them on the first word, before
    // any token is built.
    let head = command.trim_start();
    let word_end = head.find([' ', '\t']).unwrap_or(head.len());
    if !matches!(
        &head[..word_end],
        "cat" | "head" | "sed" | "nl" | "awk" | "cd"
    ) {
        return None;
    }
    let tokens = tokenize(command)?;
    let (dir, rest) = match tokens.iter().position(|token| *token == Token::And) {
        Some(at) => match &tokens[..at] {
            [Token::Word(cd), Token::Word(dir)] if cd == "cd" => {
                (Some(dir.clone()), &tokens[at + 1..])
            }
            _ => return None,
        },
        None => (None, &tokens[..]),
    };
    let (path, lines) = match rest.iter().position(|token| *token == Token::Pipe) {
        Some(at) => numbered(&rest[..at], &rest[at + 1..])?,
        None => single(words(rest)?.as_slice())?,
    };
    if path.is_empty() || path.starts_with('-') {
        return None;
    }
    Some(ShellRead { dir, path, lines })
}

/// The `read` arguments for `command` run in `working_dir`, or `None` when
/// it is not a plain read of one regular file inside `working_dir`.
///
/// The path is resolved through symlinks, so a link out of the folder stays a
/// bash command; bash may be sandboxed where `read` is not.
pub async fn read_call(working_dir: &Path, read: ShellRead<'_>) -> Option<String> {
    let path = match &read.dir {
        Some(dir) => Path::new(dir.as_ref()).join(read.path.as_ref()),
        None => Path::new(read.path.as_ref()).to_path_buf(),
    };
    let root = tokio::fs::canonicalize(working_dir).await.ok()?;
    let real = tokio::fs::canonicalize(working_dir.join(&path))
        .await
        .ok()?;
    if !real.starts_with(&root) || !tokio::fs::metadata(&real).await.ok()?.is_file() {
        return None;
    }
    let shown = path.to_str()?;
    let call = match read.lines {
        Lines::Whole => serde_json::json!({ "path": shown }),
        Lines::Range { start, end } => {
            serde_json::json!({ "path": shown, "offset": start, "limit": end - start + 1 })
        }
    };
    Some(call.to_string())
}

/// `nl -ba F | sed -n 'A,Bp'`.
fn numbered<'a>(left: &[Token<'a>], right: &[Token<'a>]) -> Option<(Cow<'a, str>, Lines)> {
    let left = words(left)?;
    let right = words(right)?;
    match (left.as_slice(), right.as_slice()) {
        ([nl, ba, path], [sed, n, script])
            if nl == "nl" && ba == "-ba" && sed == "sed" && n == "-n" =>
        {
            Some((path.clone(), sed_range(script)?))
        }
        _ => None,
    }
}

/// The forms with no pipe.
fn single<'a>(words: &[Cow<'a, str>]) -> Option<(Cow<'a, str>, Lines)> {
    let word = |at: usize| words.get(at).map(Cow::as_ref);
    match (word(0)?, words.len()) {
        ("cat", 2) => Some((words[1].clone(), Lines::Whole)),
        ("cat", 3) if word(1) == Some("-n") => Some((words[2].clone(), Lines::Whole)),
        ("head", 2) => Some((words[1].clone(), Lines::Range { start: 1, end: 10 })),
        ("head", 3) => {
            let count = number(word(1)?.strip_prefix('-')?)?;
            Some((words[2].clone(), first(count)?))
        }
        ("head", 4) if word(1) == Some("-n") => Some((words[3].clone(), first(number(word(2)?)?)?)),
        ("sed", 4) if word(1) == Some("-n") => Some((words[3].clone(), sed_range(word(2)?)?)),
        ("awk", 3) => Some((words[2].clone(), awk_range(word(1)?)?)),
        _ => None,
    }
}

/// Lines 1 to `count`, for a positive `count`.
fn first(count: usize) -> Option<Lines> {
    (count > 0).then_some(Lines::Range {
        start: 1,
        end: count,
    })
}

/// `Np` or `A,Bp`.
fn sed_range(script: &str) -> Option<Lines> {
    let body = script.strip_suffix('p')?;
    match body.split_once(',') {
        Some((start, end)) => range(start, end),
        None => range(body, body),
    }
}

/// `NR>=A && NR<=B` or `NR==N`, spaces anywhere.
fn awk_range(program: &str) -> Option<Lines> {
    let program: String = program.chars().filter(|c| !c.is_whitespace()).collect();
    if let Some(line) = program.strip_prefix("NR==") {
        return range(line, line);
    }
    let (start, end) = program.strip_prefix("NR>=")?.split_once("&&NR<=")?;
    range(start, end)
}

fn range(start: &str, end: &str) -> Option<Lines> {
    let start = number(start)?;
    let end = number(end)?;
    (start >= 1 && end >= start).then_some(Lines::Range { start, end })
}

/// A count written in digits only: `usize::from_str` also takes a leading
/// `+`, and `sed -n '2,+3p'` means lines 2 to 5, not 2 to 3.
fn number(text: &str) -> Option<usize> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[derive(Debug, PartialEq, Eq)]
enum Token<'a> {
    Word(Cow<'a, str>),
    And,
    Pipe,
}

/// The words of `tokens`, or `None` when an operator is among them.
fn words<'a>(tokens: &[Token<'a>]) -> Option<Vec<Cow<'a, str>>> {
    tokens
        .iter()
        .map(|token| match token {
            Token::Word(word) => Some(word.clone()),
            _ => None,
        })
        .collect()
}

/// Splits `command` into words, `&&` and `|`, or `None` for any other shell
/// syntax. A word with no quote borrows from `command`.
fn tokenize(command: &str) -> Option<Vec<Token<'_>>> {
    let bytes = command.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b' ' | b'\t' => at += 1,
            b'&' if bytes.get(at + 1) == Some(&b'&') => {
                tokens.push(Token::And);
                at += 2;
            }
            b'|' if bytes.get(at + 1) != Some(&b'|') => {
                tokens.push(Token::Pipe);
                at += 1;
            }
            // A lone `&` or a `||` is syntax no read form has.
            b'&' | b'|' => return None,
            _ => {
                let (word, end) = word(command, at)?;
                tokens.push(Token::Word(word));
                at = end;
            }
        }
    }
    Some(tokens)
}

/// The word that starts at `start`, and where it ends.
fn word(command: &str, start: usize) -> Option<(Cow<'_, str>, usize)> {
    let bytes = command.as_bytes();
    let mut owned: Option<String> = None;
    let mut at = start;
    while at < bytes.len() {
        match bytes[at] {
            b' ' | b'\t' | b'&' | b'|' => break,
            quote @ (b'\'' | b'"') => {
                let close = at + 1 + command[at + 1..].find(quote as char)?;
                let inner = &command[at + 1..close];
                if quote == b'"' && inner.contains(['$', '`', '\\']) {
                    return None;
                }
                owned
                    .get_or_insert_with(|| command[start..at].to_string())
                    .push_str(inner);
                at = close + 1;
            }
            byte if plain(byte) => {
                if let Some(owned) = owned.as_mut() {
                    owned.push(byte as char);
                }
                at += 1;
            }
            _ => return None,
        }
    }
    let word = match owned {
        Some(owned) => Cow::Owned(owned),
        None => Cow::Borrowed(&command[start..at]),
    };
    Some((word, at))
}

/// A byte that means itself outside quotes.
fn plain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'_' | b'.' | b'/' | b',' | b'+' | b'=' | b':' | b'@' | b'%'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(command: &str) -> Option<(Option<String>, String, Lines)> {
        classify(command).map(|read| {
            (
                read.dir.map(Cow::into_owned),
                read.path.into_owned(),
                read.lines,
            )
        })
    }

    fn range(start: usize, end: usize) -> Lines {
        Lines::Range { start, end }
    }

    #[test]
    fn plain_reads_are_classified() {
        let cases = [
            ("cat src/a.rs", None, "src/a.rs", Lines::Whole),
            ("cat -n a.rs", None, "a.rs", Lines::Whole),
            ("head a.rs", None, "a.rs", range(1, 10)),
            ("head -n 50 a.rs", None, "a.rs", range(1, 50)),
            ("head -50 a.rs", None, "a.rs", range(1, 50)),
            (
                "sed -n '120,180p' src/a.rs",
                None,
                "src/a.rs",
                range(120, 180),
            ),
            ("sed -n \"7p\" a.rs", None, "a.rs", range(7, 7)),
            ("sed -n 3,4p a.rs", None, "a.rs", range(3, 4)),
            ("nl -ba a.rs | sed -n '10,20p'", None, "a.rs", range(10, 20)),
            ("awk 'NR>=5 && NR<=9' a.rs", None, "a.rs", range(5, 9)),
            ("awk 'NR==12' a.rs", None, "a.rs", range(12, 12)),
            ("cd src && head -n 5 a.rs", Some("src"), "a.rs", range(1, 5)),
            ("cat 'my file.txt'", None, "my file.txt", Lines::Whole),
        ];
        for (command, dir, path, want) in cases {
            assert_eq!(
                lines(command),
                Some((dir.map(str::to_string), path.to_string(), want)),
                "{command}"
            );
        }
    }

    #[test]
    fn other_commands_are_not_reads() {
        for command in [
            "cat a.rs | grep x",
            "cat a.rs b.rs",
            "cat *.rs",
            "cat $FILE",
            "cat ~/a.rs",
            "cat a.rs > b.rs",
            "cat a.rs; ls",
            "cat a.rs || true",
            "head -c 100 a.rs",
            "head -n 0 a.rs",
            "tail -n 5 a.rs",
            "sed -i 's/a/b/' a.rs",
            "sed -n '/fn/p' a.rs",
            "sed -n '9,3p' a.rs",
            "sed -n '0,3p' a.rs",
            "awk '{print}' a.rs",
            "sed -n '2,+3p' a.rs",
            "sed -n '+3p' a.rs",
            "head -+5 a.rs",
            "head -n +5 a.rs",
            "awk 'NR==+4' a.rs",
            "cargo test",
            "nl a.rs | sed -n '1,2p'",
            "cd src && cd b && cat a.rs",
            "ls && cat a.rs",
            "cat -- a.rs",
            "cat \"$(echo a)\"",
            "cat `x`",
            "cat a.rs &",
            "cat 'open",
            "cat",
            "",
        ] {
            assert_eq!(lines(command), None, "{command}");
        }
    }

    #[tokio::test]
    async fn read_call_maps_a_file_inside_the_folder() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(tmp.path().join("src")).await.unwrap();
        tokio::fs::write(tmp.path().join("src/a.rs"), "x\n")
            .await
            .unwrap();
        let root = tmp.path();
        assert_eq!(
            read_call(root, classify("sed -n '120,180p' src/a.rs").unwrap())
                .await
                .as_deref(),
            Some(r#"{"limit":61,"offset":120,"path":"src/a.rs"}"#)
        );
        assert_eq!(
            read_call(root, classify("cd src && head -n 5 a.rs").unwrap())
                .await
                .as_deref(),
            Some(r#"{"limit":5,"offset":1,"path":"src/a.rs"}"#)
        );
        assert_eq!(
            read_call(root, classify("cat src/a.rs").unwrap())
                .await
                .as_deref(),
            Some(r#"{"path":"src/a.rs"}"#)
        );
    }

    #[tokio::test]
    async fn read_call_leaves_other_paths_to_bash() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        tokio::fs::write(outside.path().join("o.txt"), "o\n")
            .await
            .unwrap();
        tokio::fs::create_dir(tmp.path().join("dir")).await.unwrap();
        #[cfg(unix)]
        tokio::fs::symlink(outside.path().join("o.txt"), tmp.path().join("link.txt"))
            .await
            .unwrap();
        let root = tmp.path();
        let far = format!("cat {}", outside.path().join("o.txt").display());
        assert_eq!(read_call(root, classify(&far).unwrap()).await, None);
        assert_eq!(
            read_call(root, classify("cat ../o.txt").unwrap()).await,
            None
        );
        assert_eq!(
            read_call(root, classify("cd .. && cat o.txt").unwrap()).await,
            None
        );
        assert_eq!(
            read_call(root, classify("cat nope.txt").unwrap()).await,
            None
        );
        assert_eq!(read_call(root, classify("cat dir").unwrap()).await, None);
        #[cfg(unix)]
        assert_eq!(
            read_call(root, classify("cat link.txt").unwrap()).await,
            None
        );
    }

    #[tokio::test]
    async fn a_routed_sed_shows_numbered_lines_with_the_note() {
        use reagent_agent::batch::run_all_guarded_seen;
        use reagent_agent::tool::Registry;
        use reagent_ai::{Message, Text, ToolCall};
        use tokio_util::sync::CancellationToken;

        let tmp = tempfile::tempdir().unwrap();
        let mut text = String::new();
        for n in 1..=300 {
            text.push_str(&format!("line {n}\n"));
        }
        tokio::fs::create_dir(tmp.path().join("src")).await.unwrap();
        tokio::fs::write(tmp.path().join("src/a.rs"), text)
            .await
            .unwrap();
        let mut registry = Registry::new();
        registry.add(crate::bash::Bash::new(tmp.path()));
        registry.add(crate::read::Read::new(tmp.path()));
        let call = ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: Text::from(r#"{"command":"sed -n '120,121p' src/a.rs"}"#),
        };
        let results =
            run_all_guarded_seen(&[call], &registry, None, &CancellationToken::new()).await;
        let Message::ToolResult(result) = &results[0].0 else {
            panic!("expected a tool result");
        };
        assert!(!result.is_error, "{}", result.content);
        let rest: usize = (122..=300).map(|n| format!("line {n}\n").len()).sum();
        assert_eq!(
            result.content.as_ref(),
            format!(
                "[bash ran this as read: lines 120-121 of src/a.rs]\n120\tline 120\n121\tline 121\n[{rest} more bytes; use offset=122]\n"
            ),
        );
    }

    #[tokio::test]
    async fn a_routed_cat_of_a_long_line_runs_in_bash() {
        use reagent_agent::batch::run_all_guarded_seen;
        use reagent_agent::tool::Registry;
        use reagent_ai::{Message, Text, ToolCall};
        use tokio_util::sync::CancellationToken;

        let tmp = tempfile::tempdir().unwrap();
        let line = "x".repeat(5_000);
        tokio::fs::write(tmp.path().join("data.json"), format!("{line}\n"))
            .await
            .unwrap();
        let mut registry = Registry::new();
        registry.add(crate::bash::Bash::new(tmp.path()));
        registry.add(crate::read::Read::new(tmp.path()));
        let call = ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: Text::from(r#"{"command":"cat data.json"}"#),
        };
        let results =
            run_all_guarded_seen(&[call], &registry, None, &CancellationToken::new()).await;
        let Message::ToolResult(result) = &results[0].0 else {
            panic!("expected a tool result");
        };
        assert!(
            !result.content.starts_with("[bash ran this as read"),
            "{}",
            &result.content[..80]
        );
        assert!(
            result.content.contains(&line),
            "bash printed the whole line"
        );
        assert!(results[0].1.is_none());
    }

    #[test]
    fn a_plain_word_borrows_from_the_command() {
        let read = classify("sed -n '1,2p' src/a.rs").expect("a read");
        assert!(matches!(read.path, Cow::Borrowed("src/a.rs")));
    }
}
