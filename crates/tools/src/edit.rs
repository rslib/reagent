//! `edit`: replace text in a file, with helpful errors for near misses.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use reagent_agent::tool::{Tool, ToolOutput, parse_args};
use reagent_ai::ToolSpec;

use crate::{atomic, resolve};

/// The maximum number of lines shown in a nearest-match error hint.
const HINT_LINES: usize = 20;

/// Arguments for one `edit` call.
#[derive(Debug, Deserialize)]
pub struct Args<'a> {
    /// The file to edit, relative to the working directory.
    #[serde(borrow)]
    path: Cow<'a, str>,
    #[serde(borrow)]
    old_string: Cow<'a, str>,
    #[serde(borrow)]
    new_string: Cow<'a, str>,
    /// Replace every occurrence instead of requiring a single match.
    replace_all: Option<bool>,
}

/// Applies an exact-text replacement to a file, atomically.
#[derive(Debug)]
pub struct Edit {
    spec: ToolSpec,
    working_dir: PathBuf,
}

impl Edit {
    /// Builds the tool for the given working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            spec: spec(),
            working_dir: working_dir.into(),
        }
    }

    async fn run_inner(&self, args: &Args<'_>) -> Result<ToolOutput, String> {
        let path = resolve(&self.working_dir, &args.path)?;
        let original = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;

        if args.old_string.is_empty() {
            return Err("invalid arguments: old_string must not be empty".into());
        }
        let (old, new) = line_endings_of(&original, &args.old_string, &args.new_string);
        let matches = count_matches(&original, &old);
        let replace_all = args.replace_all.unwrap_or(false);

        if matches == 0 {
            return Err(not_found_hint(&path, &original, &old));
        }
        if matches > 1 && !replace_all {
            return Err(multiple_matches(&path, &original, &old));
        }

        let result = if replace_all {
            original.replace(old.as_ref(), &new)
        } else {
            original.replacen(old.as_ref(), &new, 1)
        };

        atomic::write(&path, result.as_bytes()).await?;
        Ok(ToolOutput::ok(format!(
            "replaced {matches} occurrence(s) in {}",
            path.display()
        )))
    }
}

/// Converts `old` and `new` to CRLF when the file uses CRLF and `old` does
/// not, since the model writes `\n` even for a CRLF file.
fn line_endings_of<'a>(original: &str, old: &'a str, new: &'a str) -> (Cow<'a, str>, Cow<'a, str>) {
    if original.contains("\r\n") && old.contains('\n') && !old.contains('\r') {
        (
            Cow::Owned(old.replace('\n', "\r\n")),
            Cow::Owned(new.replace('\n', "\r\n")),
        )
    } else {
        (Cow::Borrowed(old), Cow::Borrowed(new))
    }
}

/// Counts non-overlapping occurrences of `needle` in `haystack`.
fn count_matches(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut rest = haystack;
    while let Some(idx) = rest.find(needle) {
        count += 1;
        rest = &rest[idx + needle.len()..];
    }
    count
}

/// Returns the 1-based line numbers that contain `needle`.
fn match_lines(haystack: &str, needle: &str) -> Vec<usize> {
    let line_starts: Vec<usize> = haystack.match_indices('\n').map(|(i, _)| i + 1).collect();
    let mut lines = Vec::new();
    let mut search = haystack;
    let mut base = 0usize;
    while let Some(idx) = search.find(needle) {
        let abs = base + idx;
        let line = line_starts.partition_point(|&s| s <= abs) + 1;
        lines.push(line);
        let skip = abs + needle.len().max(1);
        base = skip;
        search = &haystack[skip..];
    }
    lines
}

/// Builds the error for zero matches: a window near the closest text.
fn not_found_hint(path: &Path, original: &str, needle: &str) -> String {
    let lines: Vec<&str> = original.lines().collect();
    let nearest = nearest_line(&lines, needle);
    let start = nearest.saturating_sub(HINT_LINES / 2);
    let mut window = String::new();
    let end = lines.len().min(start + HINT_LINES);
    for (index, line) in lines[start..end].iter().enumerate() {
        window.push_str(&format!("{}\t{line}\n", start + index + 1));
    }
    let line_no = nearest + 1;
    format!(
        "text not found in {}; closest match is near line {line_no}:\n{window}",
        path.display()
    )
}

/// Finds the index of the line closest to `needle` (fewest differing words).
fn nearest_line(lines: &[&str], needle: &str) -> usize {
    let needle_words: Vec<&str> = needle.split_whitespace().collect();
    let needle_len = needle_words.len();
    let mut best = 0;
    let mut best_score = usize::MAX;
    for (i, line) in lines.iter().enumerate() {
        let line_words: Vec<&str> = line.split_whitespace().collect();
        // Shared words between the needle and this line, penalty for length gap.
        let shared = needle_words
            .iter()
            .filter(|w| line_words.contains(w))
            .count();
        let score = needle_len.saturating_sub(shared) * 2
            + (line_words.len() as isize - needle_len as isize).unsigned_abs();
        if score < best_score {
            best_score = score;
            best = i;
        }
    }
    best
}

/// Builds the error for more than one match without `replace_all`.
fn multiple_matches(path: &Path, original: &str, needle: &str) -> String {
    let lines = match_lines(original, needle);
    let shown: Vec<String> = lines.iter().take(3).map(|n| n.to_string()).collect();
    format!(
        "text matches {} time(s) in {} at line(s) {}; add context to old_string or pass replace_all=true",
        lines.len(),
        path.display(),
        shown.join(", ")
    )
}

impl Tool for Edit {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        _cancel: &'a CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let arguments = match parse_args::<Args<'a>>(args) {
                Ok(arguments) => arguments,
                Err(output) => return output,
            };
            match self.run_inner(&arguments).await {
                Ok(output) => output,
                Err(message) => ToolOutput::error(message),
            }
        })
    }
}

/// The name of this tool.
pub const NAME: &str = "edit";

/// Builds the tool's spec.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: NAME.into(),
        description: concat!(
            "Replace exact text in a file. `old_string` must match one place exactly, ",
            "whitespace included, unless `replace_all` is true. ",
            "Do not include the line-number prefix that read prints."
        )
        .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path of the file, relative to the working directory." },
                "old_string": { "type": "string", "description": "Exact text to replace." },
                "new_string": { "type": "string", "description": "Replacement text." },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence; default false." }
            },
            "required": ["path", "old_string", "new_string"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(tmp: &tempfile::TempDir) -> Edit {
        Edit::new(tmp.path())
    }

    async fn write(tmp: &tempfile::TempDir, name: &str, contents: &str) {
        tokio::fs::write(tmp.path().join(name), contents)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn replaces_a_unique_match() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "aaa bbb ccc\n").await;
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"bbb","new_string":"BBB"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let got = tokio::fs::read_to_string(tmp.path().join("f.txt"))
            .await
            .unwrap();
        assert_eq!(got, "aaa BBB ccc\n");
    }

    #[tokio::test]
    async fn multiple_matches_without_replace_all_is_an_error_with_counts() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "x foo\ny foo\nz foo\na\n").await;
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"foo","new_string":"bar"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert!(text.contains("3 time"), "{text}");
        assert!(text.contains("1, 2, 3"), "{text}");
        // File is unchanged.
        let got = tokio::fs::read_to_string(tmp.path().join("f.txt"))
            .await
            .unwrap();
        assert_eq!(got, "x foo\ny foo\nz foo\na\n");
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "x foo\ny foo\nz foo\n").await;
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"foo","new_string":"bar","replace_all":true}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let got = tokio::fs::read_to_string(tmp.path().join("f.txt"))
            .await
            .unwrap();
        assert_eq!(got, "x bar\ny bar\nz bar\n");
    }

    #[tokio::test]
    async fn not_found_errors_with_nearest_lines_and_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp,
            "f.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nseven potato\n",
        )
        .await;
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"potato nowhere","new_string":"nope"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert!(text.contains("not found"), "{text}");
        assert!(text.contains("7\tseven potato"), "{text}");
    }

    #[tokio::test]
    async fn bad_args_are_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tool(&tmp).run("not json", &CancellationToken::new()).await;
        assert!(output.is_error);
        assert!(output.content.starts_with("invalid arguments"));
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tool(&tmp)
            .run(
                r#"{"path":"nope.txt","old_string":"a","new_string":"b"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(output.is_error);
    }

    #[tokio::test]
    async fn an_empty_old_string_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "a")
            .await
            .unwrap();
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"","new_string":"b"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(output.is_error);
        assert!(
            output.content.contains("old_string must not be empty"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn edits_a_crlf_file_with_lf_text() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        tokio::fs::write(&path, "a\r\nb\r\nc\r\n").await.unwrap();
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","old_string":"a\nb","new_string":"x\ny"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "x\r\ny\r\nc\r\n"
        );
    }
}
