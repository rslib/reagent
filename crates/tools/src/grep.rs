//! `grep`: search file contents under a folder.
//!
//! Files come from the shared walk and are read in batches on blocking
//! threads, so on a network file system many reads overlap. Plain text is
//! found with `memchr` and a regex runs only on files that hold its fixed
//! text.

use std::borrow::Cow;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;

use memchr::memmem::Finder;
use serde::Deserialize;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use reagent_agent::tool::{Effects, Tool, ToolOutput, parse_args};
use reagent_ai::ToolSpec;
use reagent_util::{decimal, text};

use crate::gitignore::wildmatch;
use crate::resolve;
use crate::walk::{File, MAX_FILES, Skip, walk};

/// Files per blocking task.
const BATCH: usize = 64;
/// The most batches read at once.
const MAX_TASKS: usize = 16;
/// A file over this many bytes is skipped.
const MAX_FILE: u64 = 8 * 1024 * 1024;
/// Bytes read from the start of a file to decide whether it is binary.
const BINARY_PROBE: usize = 8 * 1024;
/// Each printed line is cut at this many bytes.
const LINE_CAP: usize = 200;
/// The most lines printed.
const SHOWN: usize = 100;
/// The search stops after this many matches.
const MAX_MATCHES: usize = 5_000;

/// Arguments for one `grep` call.
#[derive(Debug, Deserialize)]
pub struct Args<'a> {
    #[serde(borrow)]
    pattern: Cow<'a, str>,
    #[serde(borrow)]
    path: Option<Cow<'a, str>>,
    #[serde(borrow)]
    glob: Option<Cow<'a, str>>,
    literal: Option<bool>,
    ignore_case: Option<bool>,
}

/// Searches file contents for a pattern.
#[derive(Debug)]
pub struct Grep {
    spec: ToolSpec,
    working_dir: PathBuf,
    skip: Skip,
}

impl Grep {
    /// Builds the tool for the given working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            spec: spec(),
            working_dir: working_dir.into(),
            skip: Skip::default(),
        }
    }

    /// Also skips folders with these names, on top of the built-in list.
    #[must_use]
    pub fn with_skip(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.skip = Skip::new(names);
        self
    }

    async fn run_inner(&self, args: &Args<'_>, cancel: &CancellationToken) -> ToolOutput {
        let matcher = match Matcher::new(
            &args.pattern,
            args.literal.unwrap_or(false),
            args.ignore_case.unwrap_or(false),
        ) {
            Ok(matcher) => Arc::new(matcher),
            Err(message) => return ToolOutput::error(message),
        };
        let root = match args.path.as_deref() {
            Some(path) => match resolve(&self.working_dir, path) {
                Ok(root) => root,
                Err(message) => return ToolOutput::error(message),
            },
            None => self.working_dir.clone(),
        };
        let meta = match tokio::fs::metadata(&root).await {
            Ok(meta) => meta,
            Err(error) => {
                return ToolOutput::error(format!("cannot search {}: {error}", root.display()));
            }
        };
        let mut truncated = false;
        let files = if meta.is_file() {
            let rel = args.path.as_deref().unwrap_or_default().to_string();
            vec![File { rel, path: root }]
        } else {
            let Ok(listing) = walk(&root, &self.skip, cancel).await else {
                return ToolOutput::error("cancelled");
            };
            truncated = listing.truncated;
            let mut files = listing.files;
            if let Some(glob) = args.glob.as_deref() {
                files.retain(|file| glob_matches(glob, &file.rel));
            }
            files
        };
        // The prefix goes on printed lines only, not on every listed file.
        let prefix: Option<Arc<str>> = match args.path.as_deref() {
            Some(path) if !meta.is_file() => Some(Arc::from(path.trim_end_matches('/'))),
            _ => None,
        };
        let Some(hits) = search(files, matcher, prefix, cancel).await else {
            return ToolOutput::error("cancelled");
        };
        ToolOutput::ok(render(&hits, truncated))
    }
}

impl Tool for Grep {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn main_arg(&self) -> Option<&'static str> {
        Some("path")
    }

    fn effects(&self, args: &str) -> Effects {
        let Ok(arguments) = parse_args::<Args<'_>>(args) else {
            return Effects::default();
        };
        let path = match arguments.path.as_deref() {
            Some(path) => match resolve(&self.working_dir, path) {
                Ok(path) => path,
                Err(_) => return Effects::default(),
            },
            None => self.working_dir.clone(),
        };
        Effects {
            reads: vec![path],
            ..Effects::default()
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        cancel: &'a CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let arguments = match parse_args::<Args<'a>>(args) {
                Ok(arguments) => arguments,
                Err(output) => return output,
            };
            self.run_inner(&arguments, cancel).await
        })
    }
}

/// A glob without a slash matches the file name only.
fn glob_matches(glob: &str, rel: &str) -> bool {
    let text = if glob.contains('/') {
        rel
    } else {
        rel.rsplit('/').next().unwrap_or(rel)
    };
    wildmatch(glob.as_bytes(), text.as_bytes())
}

/// The matches of one batch: the first [`SHOWN`] lines as text, each ending
/// in a newline, and how many lines matched in all.
#[derive(Default)]
struct Hits {
    text: String,
    count: usize,
}

/// Reads the files in batches and returns their hits in listing order, or
/// `None` when cancelled.
async fn search(
    files: Vec<File>,
    matcher: Arc<Matcher>,
    prefix: Option<Arc<str>>,
    cancel: &CancellationToken,
) -> Option<Vec<Hits>> {
    let mut batches = Vec::new();
    let mut files = files.into_iter().peekable();
    while files.peek().is_some() {
        batches.push(files.by_ref().take(BATCH).collect::<Vec<_>>());
    }
    let mut pending = batches.into_iter().enumerate();
    let mut results: Vec<Option<Hits>> = Vec::new();
    results.resize_with(pending.len(), || None);
    let mut tasks = JoinSet::new();
    let mut settled = 0;
    let mut counted = 0;
    loop {
        while tasks.len() < MAX_TASKS
            && let Some((index, batch)) = pending.next()
        {
            let matcher = Arc::clone(&matcher);
            let prefix = prefix.clone();
            tasks
                .spawn_blocking(move || (index, search_batch(&batch, &matcher, prefix.as_deref())));
        }
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tasks.abort_all();
                return None;
            }
            next = tasks.join_next() => next,
        };
        let Some(joined) = next else {
            break;
        };
        // A task that panicked lost one batch; the rest of the search stands.
        let Ok((index, hits)) = joined else {
            continue;
        };
        results[index] = Some(hits);
        while let Some(Some(done)) = results.get(settled) {
            counted += done.count;
            settled += 1;
        }
        if counted >= MAX_MATCHES {
            tasks.abort_all();
            break;
        }
    }
    Some(results.into_iter().flatten().collect())
}

fn search_batch(batch: &[File], matcher: &Matcher, prefix: Option<&str>) -> Hits {
    let mut hits = Hits::default();
    // One read buffer for the whole batch.
    let mut bytes = Vec::new();
    for file in batch {
        if hits.count >= MAX_MATCHES {
            break;
        }
        if !read_text(file, &mut bytes) {
            continue;
        }
        search_text(prefix, &file.rel, &bytes, matcher, &mut hits);
    }
    hits
}

/// Reads the file into `bytes`, replacing what it held. False for a file
/// that cannot be read, is too big or looks binary.
#[expect(clippy::disallowed_types, reason = "runs inside spawn_blocking")]
fn read_text(file: &File, bytes: &mut Vec<u8>) -> bool {
    bytes.clear();
    let Ok(mut handle) = std::fs::File::open(&file.path) else {
        return false;
    };
    if (&mut handle)
        .take(BINARY_PROBE as u64)
        .read_to_end(bytes)
        .is_err()
        || bytes.contains(&0)
    {
        return false;
    }
    let room = MAX_FILE + 1 - bytes.len() as u64;
    handle.take(room).read_to_end(bytes).is_ok() && bytes.len() as u64 <= MAX_FILE
}

fn search_text(prefix: Option<&str>, rel: &str, bytes: &[u8], matcher: &Matcher, hits: &mut Hits) {
    let text = String::from_utf8_lossy(bytes);
    if !matcher.may_match(text.as_bytes()) {
        return;
    }
    let mut parts = text.split('\n').peekable();
    let mut number = 0;
    while let Some(line) = parts.next() {
        number += 1;
        if parts.peek().is_none() && line.is_empty() {
            break;
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        if matcher.is_match(line) {
            if hits.count < SHOWN {
                if let Some(prefix) = prefix {
                    hits.text.push_str(prefix);
                    hits.text.push('/');
                }
                hits.text.push_str(rel);
                hits.text.push(':');
                hits.text
                    .push_str(decimal::write(number as u64, &mut [0; 20]));
                hits.text.push(':');
                hits.text.push_str(text::head(line, LINE_CAP));
                hits.text.push('\n');
            }
            hits.count += 1;
            if hits.count >= MAX_MATCHES {
                break;
            }
        }
    }
}

fn render(hits: &[Hits], truncated: bool) -> String {
    let total: usize = hits.iter().map(|batch| batch.count).sum();
    let total = total.min(MAX_MATCHES);
    let mut out = String::new();
    let mut shown = 0;
    for batch in hits {
        for line in batch.text.split_inclusive('\n') {
            if shown == SHOWN {
                break;
            }
            out.push_str(line);
            shown += 1;
        }
    }
    if out.ends_with('\n') {
        out.pop();
    }
    if total == 0 {
        out.push_str("no matches");
    }
    if total >= MAX_MATCHES {
        out.push_str(&format!(
            "\n[stopped after {MAX_MATCHES} matches; showing the first {SHOWN}. Narrow the search with path, glob or a longer pattern.]"
        ));
    } else if total > SHOWN {
        out.push_str(&format!(
            "\n[{total} lines matched; showing the first {SHOWN}. Narrow the search with path, glob or a longer pattern.]"
        ));
    }
    if truncated {
        out.push_str(&format!("\n[the walk stopped after {MAX_FILES} files]"));
    }
    out
}

/// How lines are tested.
enum Matcher {
    /// A line matches when any of the texts is in it.
    Plain(Vec<Finder<'static>>),
    Regex {
        regex: regex_lite::Regex,
        /// Text every match contains, to skip files without it.
        required: Option<Finder<'static>>,
    },
}

impl Matcher {
    fn new(pattern: &str, literal: bool, ignore_case: bool) -> Result<Self, String> {
        if literal {
            if ignore_case {
                return Self::regex(&regex_lite::escape(pattern), true, None);
            }
            return Ok(Self::plain(&[pattern]));
        }
        let pattern = basic_or(pattern);
        if !ignore_case {
            let branches: Vec<&str> = pattern.split('|').collect();
            if branches.iter().all(|branch| !has_meta(branch)) {
                return Ok(Self::plain(&branches));
            }
        }
        let required = if ignore_case {
            None
        } else {
            required_text(&pattern)
        };
        Self::regex(&pattern, ignore_case, required)
    }

    fn plain(texts: &[&str]) -> Self {
        Self::Plain(
            texts
                .iter()
                .map(|text| Finder::new(text).into_owned())
                .collect(),
        )
    }

    fn regex(pattern: &str, ignore_case: bool, required: Option<String>) -> Result<Self, String> {
        let source = if ignore_case {
            format!("(?i){pattern}")
        } else {
            pattern.to_string()
        };
        let regex = regex_lite::Regex::new(&source).map_err(|error| error.to_string())?;
        Ok(Self::Regex {
            regex,
            required: required.map(|text| Finder::new(&text).into_owned()),
        })
    }

    /// False when the file cannot hold a match.
    fn may_match(&self, text: &[u8]) -> bool {
        match self {
            Self::Plain(finders) => finders.iter().any(|finder| finder.find(text).is_some()),
            Self::Regex { required, .. } => required
                .as_ref()
                .is_none_or(|finder| finder.find(text).is_some()),
        }
    }

    fn is_match(&self, line: &str) -> bool {
        match self {
            Self::Plain(finders) => finders
                .iter()
                .any(|finder| finder.find(line.as_bytes()).is_some()),
            Self::Regex { regex, .. } => regex.is_match(line),
        }
    }
}

/// Rewrites the basic-syntax `\|` to `|`, leaving `\\` and other escapes alone.
fn basic_or(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('|') => out.push('|'),
            Some(next) => {
                out.push('\\');
                out.push(next);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn has_meta(text: &str) -> bool {
    text.contains([
        '\\', '.', '+', '*', '?', '(', ')', '|', '[', ']', '{', '}', '^', '$',
    ])
}

/// The longest run of fixed text that every match of the regex contains, or
/// `None` when unsure. Only a plain concatenation is read: a `|` anywhere
/// gives up, and groups and classes are skipped.
fn required_text(pattern: &str) -> Option<String> {
    if pattern.contains('|') || pattern.contains("(?") {
        return None;
    }
    let chars: Vec<char> = pattern.chars().collect();
    let mut best = String::new();
    let mut run = String::new();
    let mut end_run = |run: &mut String| {
        if run.chars().count() > best.chars().count() {
            best = std::mem::take(run);
        } else {
            run.clear();
        }
    };
    let mut at = 0;
    while at < chars.len() {
        match chars[at] {
            '\\' => {
                let next = *chars.get(at + 1)?;
                if matches!(
                    next,
                    'b' | 'B' | 'w' | 'W' | 'd' | 'D' | 's' | 'S' | 'A' | 'z'
                ) {
                    end_run(&mut run);
                } else if next.is_ascii_alphanumeric() {
                    return None;
                } else {
                    run.push(next);
                }
                at += 2;
            }
            '(' => {
                end_run(&mut run);
                at = skip_group(&chars, at)?;
            }
            '[' => {
                end_run(&mut run);
                at = skip_class(&chars, at)?;
            }
            '*' | '?' => {
                run.pop();
                end_run(&mut run);
                at += 1;
            }
            '{' => {
                run.pop();
                end_run(&mut run);
                at = chars[at..].iter().position(|&c| c == '}')? + at + 1;
            }
            '+' | '.' | '^' | '$' => {
                end_run(&mut run);
                at += 1;
            }
            ')' | ']' | '}' => return None,
            other => {
                run.push(other);
                at += 1;
            }
        }
    }
    end_run(&mut run);
    (!best.is_empty()).then_some(best)
}

/// The index after the group that opens at `start`.
fn skip_group(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 0;
    let mut at = start;
    while at < chars.len() {
        match chars[at] {
            '\\' => at += 1,
            '[' => at = skip_class(chars, at)? - 1,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

/// The index after the class that opens at `start`.
fn skip_class(chars: &[char], start: usize) -> Option<usize> {
    let mut at = start + 1;
    if chars.get(at) == Some(&'^') {
        at += 1;
    }
    if chars.get(at) == Some(&']') {
        at += 1;
    }
    while at < chars.len() {
        match chars[at] {
            '\\' => at += 1,
            ']' => return Some(at + 1),
            _ => {}
        }
        at += 1;
    }
    None
}

/// The name of this tool.
pub const NAME: &str = "grep";

/// Builds the tool's spec.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: NAME.into(),
        description: concat!(
            "Search file contents for a regular expression or plain text under a folder ",
            "(default: the working directory). Skips ignored and binary files. ",
            "Prints `path:line:text`, sorted, at most 100 lines. ",
            "`\\w`, `\\d` and ignore_case are ASCII only."
        )
        .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression, or plain text with literal." },
                "path": { "type": "string", "description": "Folder or file to search." },
                "glob": { "type": "string", "description": "Only paths matching this glob, such as `*.rs`." },
                "literal": { "type": "boolean", "description": "Match the pattern as plain text." },
                "ignore_case": { "type": "boolean", "description": "Ignore ASCII letter case." }
            },
            "required": ["pattern"]
        }),
    }
}

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
mod tests {
    use super::*;
    use std::path::Path;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    async fn grep(root: &Path, args: serde_json::Value) -> ToolOutput {
        Grep::new(root)
            .run(&args.to_string(), &CancellationToken::new())
            .await
    }

    async fn lines(root: &Path, args: serde_json::Value) -> Vec<String> {
        let output = grep(root, args).await;
        assert!(!output.is_error, "{}", output.content);
        output.content.lines().map(str::to_string).collect()
    }

    #[test]
    fn spec_is_named_grep() {
        assert_eq!(spec().name, "grep");
        assert!(Grep::new(".").read_only());
    }

    #[tokio::test]
    async fn matches_are_sorted_by_path_then_line() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "b.rs", "x\ny\nfn stash_show()\n");
        write(tmp.path(), "a.rs", "1\n2\n3\n4\n5\n6\nfn stash_show()\nz\n");
        let got = lines(
            tmp.path(),
            serde_json::json!({"pattern": "fn stash_show()", "literal": true}),
        )
        .await;
        assert_eq!(got, ["a.rs:7:fn stash_show()", "b.rs:3:fn stash_show()"]);
    }

    #[tokio::test]
    async fn output_over_the_cap_ends_with_the_total() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", &"hit\n".repeat(150));
        let got = lines(tmp.path(), serde_json::json!({"pattern": "hit"})).await;
        assert_eq!(got.len(), 101);
        assert_eq!(got[0], "a.txt:1:hit");
        assert_eq!(
            got[100],
            "[150 lines matched; showing the first 100. Narrow the search with path, glob or a longer pattern.]"
        );
    }

    #[tokio::test]
    async fn matching_stops_at_5000() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", &"hit\n".repeat(6000));
        let got = lines(tmp.path(), serde_json::json!({"pattern": "hit"})).await;
        assert_eq!(got.len(), 101);
        assert_eq!(
            got[100],
            "[stopped after 5000 matches; showing the first 100. Narrow the search with path, glob or a longer pattern.]"
        );
    }

    #[tokio::test]
    async fn binary_and_ignored_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "needle\n");
        std::fs::write(tmp.path().join("data.bin"), b"\0needle\n").unwrap();
        write(tmp.path(), "skip.log", "needle\n");
        write(tmp.path(), ".gitignore", "*.log\n");
        let got = lines(tmp.path(), serde_json::json!({"pattern": "needle"})).await;
        assert_eq!(got, ["a.txt:1:needle"]);
    }

    #[tokio::test]
    async fn a_basic_syntax_or_matches_both_branches() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.rs", "fn load()\nother\nstruct Loaded;\n");
        let got = lines(
            tmp.path(),
            serde_json::json!({"pattern": "fn load\\|struct Loaded"}),
        )
        .await;
        assert_eq!(got, ["a.rs:1:fn load()", "a.rs:3:struct Loaded;"]);
    }

    #[tokio::test]
    async fn an_escaped_backslash_before_a_bar_is_not_an_or() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "x\\\nxz\ny\nz\n");
        let got = lines(tmp.path(), serde_json::json!({"pattern": "x\\\\|y"})).await;
        assert_eq!(got, ["a.txt:1:x\\", "a.txt:3:y"]);
        assert_eq!(basic_or("x\\\\|y"), "x\\\\|y");
        assert_eq!(basic_or("a\\|b\\.c"), "a|b\\.c");
    }

    #[tokio::test]
    async fn a_folder_path_prints_paths_from_the_working_dir() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "src/lib.rs", "a\nb\nhit\n");
        write(tmp.path(), "other.rs", "hit\n");
        let got = lines(
            tmp.path(),
            serde_json::json!({"pattern": "hit", "path": "src/"}),
        )
        .await;
        assert_eq!(got, ["src/lib.rs:3:hit"]);
    }

    #[tokio::test]
    async fn literal_matches_plain_text() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "a.b(c)\naxb(c)\n");
        let got = lines(
            tmp.path(),
            serde_json::json!({"pattern": "a.b(c)", "literal": true}),
        )
        .await;
        assert_eq!(got, ["a.txt:1:a.b(c)"]);
    }

    #[tokio::test]
    async fn ignore_case_matches_ascii_case() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "Hello\nhello\nHELLO\nbye\n");
        let got = lines(
            tmp.path(),
            serde_json::json!({"pattern": "hello", "ignore_case": true}),
        )
        .await;
        assert_eq!(got.len(), 3);
        let got = lines(
            tmp.path(),
            serde_json::json!({"pattern": "h.llo", "literal": true, "ignore_case": true}),
        )
        .await;
        assert_eq!(got, ["no matches"]);
    }

    #[tokio::test]
    async fn a_regex_with_a_fixed_text_still_finds_every_match() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "a.rs",
            "fn do_stash(x)\nfn do_it(x)\nfn go_stash(\n",
        );
        write(tmp.path(), "b.txt", "ac\nabc\nabbc\nxyz\n");
        let got = lines(
            tmp.path(),
            serde_json::json!({"pattern": "fn [a-z_]+_stash\\("}),
        )
        .await;
        assert_eq!(got, ["a.rs:1:fn do_stash(x)", "a.rs:3:fn go_stash("]);
        let got = lines(tmp.path(), serde_json::json!({"pattern": "ab?c"})).await;
        assert_eq!(got, ["b.txt:1:ac", "b.txt:2:abc"]);
        let got = lines(tmp.path(), serde_json::json!({"pattern": "ab*c"})).await;
        assert_eq!(got, ["b.txt:1:ac", "b.txt:2:abc", "b.txt:3:abbc"]);
    }

    #[test]
    fn the_required_text_is_conservative() {
        assert_eq!(required_text("fn [a-z_]+_stash\\("), Some("_stash(".into()));
        assert_eq!(required_text("ab?c"), Some("a".into()));
        assert_eq!(required_text("a\\.b+"), Some("a.b".into()));
        assert_eq!(required_text("x(ab)*y"), Some("x".into()));
        assert_eq!(required_text("ab{0,2}c"), Some("a".into()));
        assert_eq!(required_text("a|b.c"), None);
        assert_eq!(required_text("\\w+"), None);
        assert_eq!(required_text("(?i)hello"), None);
        assert_eq!(required_text("\\x41BC"), None);
        assert_eq!(required_text("[ab]*"), None);
    }

    #[tokio::test]
    async fn an_inline_flag_group_turns_the_prefilter_off() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "HELLO world");
        write(tmp.path(), "sub/b.txt", "hello");
        let got = lines(tmp.path(), serde_json::json!({"pattern": "(?i)hello"})).await;
        assert_eq!(got, ["a.txt:1:HELLO world", "sub/b.txt:1:hello"]);
    }

    #[tokio::test]
    async fn a_hex_escape_turns_the_prefilter_off() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "ABC\nxyz\n");
        let got = lines(tmp.path(), serde_json::json!({"pattern": "\\x41BC"})).await;
        assert_eq!(got, ["a.txt:1:ABC"]);
    }

    #[tokio::test]
    async fn glob_limits_the_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.rs", "hit\n");
        write(tmp.path(), "src/b.rs", "hit\n");
        write(tmp.path(), "c.txt", "hit\n");
        let got = lines(
            tmp.path(),
            serde_json::json!({"pattern": "hit", "glob": "*.rs"}),
        )
        .await;
        assert_eq!(got, ["a.rs:1:hit", "src/b.rs:1:hit"]);
        let got = lines(
            tmp.path(),
            serde_json::json!({"pattern": "hit", "glob": "src/*.rs"}),
        )
        .await;
        assert_eq!(got, ["src/b.rs:1:hit"]);
    }

    #[tokio::test]
    async fn a_single_file_path_is_searched() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.rs", "hit\n");
        write(tmp.path(), "src/b.rs", "no\nhit\n");
        let got = lines(
            tmp.path(),
            serde_json::json!({"pattern": "hit", "path": "src/b.rs"}),
        )
        .await;
        assert_eq!(got, ["src/b.rs:2:hit"]);
    }

    #[tokio::test]
    async fn long_lines_are_cut_at_200_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let line = format!("hit{}\r\n", "\u{e9}".repeat(150));
        write(tmp.path(), "a.txt", &line);
        let got = lines(tmp.path(), serde_json::json!({"pattern": "hit"})).await;
        let text = got[0].strip_prefix("a.txt:1:").unwrap();
        assert_eq!(text.len(), 199);
        assert!(text.starts_with("hit"));
        assert!(!text.ends_with('\r'));
    }

    #[tokio::test]
    async fn a_bad_regex_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let output = grep(tmp.path(), serde_json::json!({"pattern": "a(b"})).await;
        assert!(output.is_error);
        assert!(!output.content.is_empty());
    }

    #[tokio::test]
    async fn no_matches_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "abc\n");
        let got = lines(tmp.path(), serde_json::json!({"pattern": "zzz"})).await;
        assert_eq!(got, ["no matches"]);
    }

    #[tokio::test]
    async fn a_cancelled_search_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.txt", "abc\n");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let output = Grep::new(tmp.path())
            .run(r#"{"pattern":"abc"}"#, &cancel)
            .await;
        assert!(output.is_error);
        assert_eq!(output.content.as_ref(), "cancelled");
    }

    #[test]
    fn grep_effects_default_to_the_working_dir() {
        let grep = Grep::new("/work");
        assert_eq!(grep.main_arg(), Some("path"));
        assert_eq!(
            grep.effects(r#"{"pattern":"x"}"#).reads,
            vec![PathBuf::from("/work")]
        );
        let effects = grep.effects(r#"{"pattern":"x","path":"src"}"#);
        assert_eq!(effects.reads, vec![PathBuf::from("/work/src")]);
        assert!(effects.writes.is_empty());
    }

    #[test]
    fn grep_bad_args_have_no_effects() {
        assert_eq!(Grep::new("/work").effects("nope"), Effects::default());
    }
}
