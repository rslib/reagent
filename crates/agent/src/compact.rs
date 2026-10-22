//! Compaction: when the prompt reaches a token budget, older turns become one
//! checkpoint built by code (see [`crate::checkpoint`]), with no model call.
//!
//! The history keeps this shape: the first user message, the checkpoints in
//! the order they were made, one message with the current text of recently
//! changed files, then the newest turns. A new checkpoint goes after the
//! last one and never rewrites an earlier one, so their bytes stay cached.

use std::path::PathBuf;

use tokio::io::AsyncReadExt;

use reagent_ai::{Content, Message, Text};
use reagent_util::text;

use crate::archive::Archive;
use crate::checkpoint;

/// The prompt size, in tokens, that triggers a compaction.
pub const BUDGET: u64 = 150_000;

/// The newest turns kept whole, in bytes (about 20,000 tokens).
const TAIL_BYTES: usize = 80_000;

/// How many changed files are shown again after a checkpoint.
const MAX_FILES: usize = 5;

/// The most bytes shown of each changed file.
const FILE_CAP: usize = 20 * 1024;

/// The most bytes read of a changed file: four times [`FILE_CAP`], which is
/// enough for the cut and the rest of a multi-byte character, so a huge file
/// never lands in RAM whole.
const READ_CAP: usize = 4 * FILE_CAP + 4;

/// The checkpoint header, the same file [`crate::checkpoint`] writes first.
const HEADER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/checkpoint.md"
));

fn header() -> &'static str {
    HEADER.trim_end()
}

/// The first line of the changed-files message.
const FILES_HEAD: &str = "Current text of files changed before the last checkpoint:";

/// What one compaction did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compacted {
    /// The prompt size, in tokens, that triggered it.
    pub before: u64,
    /// How many messages the checkpoint replaced.
    pub replaced: usize,
    /// The size of the new checkpoint, in bytes.
    pub bytes: usize,
}

/// Replaces older turns with checkpoints at the token budget.
#[derive(Debug, Clone)]
pub struct Compact {
    budget: u64,
    working_dir: PathBuf,
}

impl Compact {
    /// Compacts at [`BUDGET`], reading changed files under `working_dir`.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self::with_budget(BUDGET, working_dir)
    }

    /// Compacts at `budget` tokens instead of [`BUDGET`].
    pub fn with_budget(budget: u64, working_dir: impl Into<PathBuf>) -> Self {
        Self {
            budget,
            working_dir: working_dir.into(),
        }
    }

    /// Compacts `history` when the last reply's prompt reached the budget.
    ///
    /// Tool outputs that leave the history are kept in `archive`. Returns
    /// what was done, or `None` when nothing changed.
    pub async fn apply(&self, history: &mut Vec<Message>, archive: &Archive) -> Option<Compacted> {
        let before = last_prompt(history)?;
        if before < self.budget || history.is_empty() {
            return None;
        }
        let checkpoints = history[1..]
            .iter()
            .take_while(|message| is_checkpoint(message))
            .count();
        let start =
            1 + checkpoints + usize::from(history.get(1 + checkpoints).is_some_and(is_files));
        let tail = tail_start(history, start)?;
        // Each compaction breaks the cache once, so it waits until a quarter
        // of the budget (at 4 bytes per token) can go, not one turn at a time.
        let replaceable: usize = history[start..tail].iter().map(size).sum();
        if (replaceable as u64) < self.budget {
            return None;
        }

        for message in &history[start..tail] {
            if let Message::ToolResult(result) = message
                && let Some(id) = archive.id_of(&result.call_id)
                && !result.content.starts_with("[Old tool output #")
            {
                archive.keep(id, Text::clone(&result.content));
            }
        }
        archive.flush().await;

        let number = next_number(&history[1..1 + checkpoints]);
        let built = checkpoint::build(number, &history[start..tail], archive);
        let bytes = built.text.len();
        let files = self.files_message(&built.changed).await;

        let mut kept: Vec<Message> = history.drain(..start).collect();
        kept.truncate(1 + checkpoints);
        let rest: Vec<Message> = history.drain(tail - start..).collect();
        kept.push(Message::user(built.text));
        // A third of the budget, at 4 bytes per token.
        merge_if_large(&mut kept, (self.budget as usize) * 4 / 3);
        if let Some(files) = files {
            kept.push(files);
        }
        kept.extend(rest);
        *history = kept;
        Some(Compacted {
            before,
            replaced: tail - start,
            bytes,
        })
    }

    /// The message with the current text of up to [`MAX_FILES`] files.
    ///
    /// Each file is read up to [`READ_CAP`] bytes, and the note that it was cut
    /// comes from the file's length, so a large file costs a bounded read.
    async fn files_message(&self, changed: &[String]) -> Option<Message> {
        let mut text = String::new();
        for path in changed.iter().take(MAX_FILES) {
            let Ok(file) = tokio::fs::File::open(self.working_dir.join(path)).await else {
                continue;
            };
            let Ok(meta) = file.metadata().await else {
                continue;
            };
            let len = meta.len();
            let mut bytes = Vec::with_capacity(len.min(READ_CAP as u64) as usize);
            if file
                .take(READ_CAP as u64)
                .read_to_end(&mut bytes)
                .await
                .is_err()
            {
                continue;
            }
            let body = String::from_utf8_lossy(&bytes);
            let cut = text::head(&body, FILE_CAP);
            text.push_str("\n\n== ");
            text.push_str(path);
            text.push_str(" ==\n");
            text.push_str(cut);
            // Cut when the read stopped early or the decoded text is longer
            // than what was kept: lossy decoding can grow an invalid file.
            if cut.len() < body.len() || (bytes.len() as u64) < len {
                text.push_str("\n[cut at 20 KB; read the file for the rest]");
            }
        }
        (!text.is_empty()).then(|| Message::user(format!("{FILES_HEAD}{text}")))
    }
}

/// The last reply's prompt size in tokens, from its usage.
fn last_prompt(history: &[Message]) -> Option<u64> {
    history.iter().rev().find_map(|message| match message {
        Message::Assistant(reply) => {
            let usage = &reply.usage;
            Some(usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens)
        }
        _ => None,
    })
}

/// The index where the kept tail starts: the newest messages of at least
/// [`TAIL_BYTES`], moved back to an assistant message so a tool call is
/// never split from its result. `None` when nothing before it can go.
fn tail_start(history: &[Message], start: usize) -> Option<usize> {
    let mut bytes = 0;
    let mut at = history.len();
    while at > start && bytes < TAIL_BYTES {
        at -= 1;
        bytes += size(&history[at]);
    }
    while at > start && !matches!(history[at], Message::Assistant(_)) {
        at -= 1;
    }
    (at > start && matches!(history[at], Message::Assistant(_))).then_some(at)
}

fn size(message: &Message) -> usize {
    match message {
        Message::User { content } => content.iter().map(content_size).sum(),
        Message::Assistant(reply) => reply.content.iter().map(content_size).sum(),
        Message::ToolResult(result) => result.content.len(),
    }
}

fn content_size(content: &Content) -> usize {
    match content {
        Content::Text { text } => text.len(),
        Content::Thinking { thinking, .. } => thinking.len(),
        Content::RedactedThinking { data } => data.len(),
        Content::ToolCall(call) => call.name.len() + call.arguments.len(),
    }
}

fn first_text(message: &Message) -> Option<&str> {
    match message {
        Message::User { content } => content.iter().find_map(|block| match block {
            Content::Text { text } => Some(text.as_ref()),
            _ => None,
        }),
        _ => None,
    }
}

/// The number of the next checkpoint: one past the sections already written,
/// which a merge keeps under one header.
fn next_number(checkpoints: &[Message]) -> u32 {
    checkpoints
        .iter()
        .filter_map(first_text)
        .map(|text| text.matches("\n## Checkpoint ").count() as u32)
        .sum::<u32>()
        + 1
}

fn is_checkpoint(message: &Message) -> bool {
    first_text(message).is_some_and(|text| text.starts_with(header()))
}

fn is_files(message: &Message) -> bool {
    first_text(message).is_some_and(|text| text.starts_with(FILES_HEAD))
}

/// Merges all checkpoints after the first message into one when together
/// they pass `limit` bytes. The merge keeps one header and every line but the
/// tool call lines of the older checkpoints, so it shrinks and the next
/// compactions append again instead of rewriting each time.
fn merge_if_large(kept: &mut Vec<Message>, limit: usize) {
    let total: usize = kept[1..].iter().map(size).sum();
    if kept.len() <= 2 || total <= limit {
        return;
    }
    let header = header();
    let newest = kept.len() - 1;
    let mut text = String::from(header);
    for (index, message) in kept.iter().enumerate().skip(1) {
        let Some(body) = first_text(message) else {
            continue;
        };
        let body = body.strip_prefix(header).unwrap_or(body);
        if index == newest {
            text.push_str(body);
            continue;
        }
        for line in body.split_inclusive('\n') {
            if !line.starts_with('#') || line.starts_with("## ") {
                text.push_str(line);
            }
        }
    }
    kept.truncate(1);
    kept.push(Message::user(text));
}

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
mod tests {
    use super::*;
    use reagent_ai::{AssistantMessage, ToolCall, Usage};

    /// One turn: an assistant reply with one tool call, then its result.
    fn turn(
        history: &mut Vec<Message>,
        archive: &Archive,
        n: usize,
        name: &str,
        args: &str,
        output: &str,
    ) {
        let id = format!("call_{n}");
        let mut reply = AssistantMessage::new(
            "fake",
            "fake",
            vec![Content::ToolCall(ToolCall {
                id: id.clone().into(),
                name: name.into(),
                arguments: args.into(),
            })],
        );
        reply.usage = Usage::default();
        archive.assign(&Text::from(id.as_str()));
        history.push(Message::assistant(reply));
        history.push(Message::tool_result(id, output, false));
    }

    fn set_prompt(history: &mut [Message], tokens: u64) {
        if let Some(Message::Assistant(reply)) = history
            .iter_mut()
            .rev()
            .find(|m| matches!(m, Message::Assistant(_)))
        {
            reply.usage.input_tokens = tokens;
        }
    }

    fn session(turns: usize, archive: &Archive) -> Vec<Message> {
        let mut history = vec![Message::user("fix the bug")];
        for n in 1..=turns {
            turn(
                &mut history,
                archive,
                n,
                "bash",
                r#"{"command":"ls"}"#,
                &"x".repeat(4_000),
            );
        }
        history
    }

    fn texts(history: &[Message]) -> Vec<String> {
        history
            .iter()
            .map(|m| first_text(m).unwrap_or("").to_owned())
            .collect()
    }

    #[tokio::test]
    async fn under_the_budget_nothing_changes() {
        let archive = Archive::default();
        let mut history = session(60, &archive);
        set_prompt(&mut history, BUDGET - 1);
        let before = history.clone();
        assert_eq!(Compact::new(".").apply(&mut history, &archive).await, None);
        assert_eq!(history, before);
    }

    #[tokio::test]
    async fn at_the_budget_old_turns_become_one_checkpoint() {
        let archive = Archive::default();
        let mut history = session(60, &archive);
        set_prompt(&mut history, BUDGET);
        let done = Compact::new(".")
            .apply(&mut history, &archive)
            .await
            .unwrap();
        assert_eq!(done.before, BUDGET);
        assert_eq!(first_text(&history[0]), Some("fix the bug"));
        assert!(is_checkpoint(&history[1]));
        assert!(first_text(&history[1]).unwrap().contains("## Checkpoint 1"));
        assert!(
            matches!(history[2], Message::Assistant(_)),
            "the tail starts with a reply"
        );
        let tail: usize = history[2..].iter().map(size).sum();
        assert!(tail >= TAIL_BYTES, "tail {tail}");
        assert_eq!(done.replaced, 121 - 1 - (history.len() - 2));
        assert!(done.bytes > 0);
    }

    #[tokio::test]
    async fn replaced_outputs_stay_in_the_archive() {
        let archive = Archive::default();
        let mut history = session(60, &archive);
        set_prompt(&mut history, BUDGET);
        Compact::new(".")
            .apply(&mut history, &archive)
            .await
            .unwrap();
        assert_eq!(archive.kept_len(1), Some(4_000));
        assert_eq!(archive.get(1).await.map(|t| t.len()), Some(4_000));
    }

    #[tokio::test]
    async fn a_second_compaction_keeps_the_first_checkpoint_byte_for_byte() {
        let archive = Archive::default();
        let mut history = session(60, &archive);
        set_prompt(&mut history, BUDGET);
        let compact = Compact::new(".");
        compact.apply(&mut history, &archive).await.unwrap();
        let first = texts(&history)[1].clone();
        for n in 61..=120 {
            turn(
                &mut history,
                &archive,
                n,
                "bash",
                r#"{"command":"ls"}"#,
                &"y".repeat(4_000),
            );
        }
        set_prompt(&mut history, BUDGET);
        compact.apply(&mut history, &archive).await.unwrap();
        let after = texts(&history);
        assert_eq!(after[1], first);
        assert!(after[2].contains("## Checkpoint 2"));
        assert!(matches!(history[3], Message::Assistant(_)));
    }

    #[tokio::test]
    async fn changed_files_come_back_newest_first_at_most_five() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = Archive::default();
        let mut history = vec![Message::user("fix the bug")];
        for n in 1..=7 {
            let path = format!("f{n}.rs");
            std::fs::write(tmp.path().join(&path), format!("text {n}")).unwrap();
            let args = format!(r#"{{"path":"{path}","old_string":"a","new_string":"b"}}"#);
            turn(&mut history, &archive, n, "edit", &args, "ok");
        }
        for n in 8..=70 {
            turn(
                &mut history,
                &archive,
                n,
                "bash",
                r#"{"command":"ls"}"#,
                &"x".repeat(4_000),
            );
        }
        set_prompt(&mut history, BUDGET);
        Compact::new(tmp.path())
            .apply(&mut history, &archive)
            .await
            .unwrap();
        let files = first_text(&history[2]).unwrap();
        assert!(files.starts_with(FILES_HEAD));
        let shown: Vec<&str> = files.lines().filter(|l| l.starts_with("== ")).collect();
        assert_eq!(
            shown,
            [
                "== f7.rs ==",
                "== f6.rs ==",
                "== f5.rs ==",
                "== f4.rs ==",
                "== f3.rs =="
            ]
        );
        assert!(files.contains("text 7"));
        assert!(matches!(history[3], Message::Assistant(_)));
    }

    fn bash_turn(history: &mut Vec<Message>, archive: &Archive, n: usize) {
        turn(
            history,
            archive,
            n,
            "bash",
            r#"{"command":"ls"}"#,
            &"x".repeat(4_000),
        );
    }

    #[tokio::test]
    async fn a_small_replaceable_part_waits_and_then_compacts_once() {
        let archive = Archive::default();
        // 40 turns of 4 KB: 80 KB past the tail, under the 150 KB minimum.
        let mut history = session(40, &archive);
        set_prompt(&mut history, BUDGET);
        let compact = Compact::new(".");
        assert_eq!(compact.apply(&mut history, &archive).await, None);
        for n in 41..=80 {
            bash_turn(&mut history, &archive, n);
        }
        set_prompt(&mut history, BUDGET);
        assert!(compact.apply(&mut history, &archive).await.is_some());
        bash_turn(&mut history, &archive, 81);
        set_prompt(&mut history, BUDGET);
        assert_eq!(
            compact.apply(&mut history, &archive).await,
            None,
            "one more turn is not enough"
        );
    }

    #[tokio::test]
    async fn a_short_history_is_left_alone() {
        let archive = Archive::default();
        let mut history = session(3, &archive);
        set_prompt(&mut history, BUDGET);
        assert_eq!(Compact::new(".").apply(&mut history, &archive).await, None);
        assert_eq!(history.len(), 7);
    }

    #[test]
    fn large_checkpoints_merge_into_one_without_old_tool_lines() {
        let limit = 1_000;
        let mut kept = vec![Message::user("task")];
        for n in 1..=3 {
            kept.push(Message::user(format!(
                "{}\n\n## Checkpoint {n}\nUser: step {n}\n#{n} bash ls -> ok, 9 bytes\n{}",
                header(),
                "z".repeat(limit / 2)
            )));
        }
        merge_if_large(&mut kept, limit);
        assert_eq!(kept.len(), 2);
        let text = first_text(&kept[1]).unwrap();
        assert_eq!(text.matches(header()).count(), 1);
        assert!(text.contains("## Checkpoint 1") && text.contains("## Checkpoint 3"));
        assert!(text.contains("User: step 1") && text.contains("User: step 2"));
        assert!(!text.contains("#1 bash") && !text.contains("#2 bash"));
        assert!(text.contains("#3 bash"), "the newest keeps its tool lines");
    }

    #[test]
    fn checkpoints_are_numbered_past_a_merge() {
        let merged = format!("{}\n\n## Checkpoint 1\nx\n## Checkpoint 2\ny", header());
        assert_eq!(next_number(&[Message::user(merged)]), 3);
        assert_eq!(next_number(&[]), 1);
    }
}
