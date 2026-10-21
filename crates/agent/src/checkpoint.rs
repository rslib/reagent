//! Checkpoints: replaced turns written as one compact message, built by code.

use std::fmt::Write as _;

use reagent_ai::{Content, Message, Text, ToolCall};
use reagent_util::text;

use crate::archive::Archive;

/// The checkpoint header, baked in from `prompts/checkpoint.md`.
const HEADER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/checkpoint.md"
));

/// Assistant text longer than this many bytes is cut.
const TEXT_BYTES: usize = 1000;

/// Tool call arguments longer than this many bytes are cut.
const ARGS_BYTES: usize = 160;

/// The last failure shows at most this many lines of its output.
const FAILURE_LINES: usize = 40;

/// The last failure shows at most this many bytes of its output.
const FAILURE_BYTES: usize = 4096;

/// A masked output starts with this.
const STUB_PREFIX: &str = "[Old tool output #";

/// One checkpoint: the text that replaces a run of turns, and the paths those turns changed, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// The checkpoint message text.
    pub text: String,
    /// The paths the turns edited or wrote, newest first.
    pub changed: Vec<String>,
}

struct FileEntry {
    path: String,
    changed: bool,
    edited: bool,
    ids: Vec<String>,
    read_ids: Vec<String>,
}

struct Failure {
    id: String,
    name: String,
    args: String,
    output: String,
}

/// Builds checkpoint number `number` (1-based) from `messages`, the turns it replaces.
pub fn build(number: u32, messages: &[Message], archive: &Archive) -> Checkpoint {
    let mut text = HEADER.trim_end_matches('\n').to_string();
    let _ = write!(text, "\n\n## Checkpoint {number}\n");
    let mut files: Vec<FileEntry> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    let mut failure = None;

    for message in messages {
        match message {
            Message::User { content } => {
                for block in content {
                    if let Content::Text { text: body } = block {
                        let _ = write!(text, "\nUser: {body}");
                    }
                }
            }
            Message::Assistant(reply) => {
                for block in &reply.content {
                    match block {
                        Content::Text { text: body } => {
                            let _ = write!(text, "\nAssistant: {}", text::head(body, TEXT_BYTES));
                            if body.len() > TEXT_BYTES {
                                text.push_str(" [cut]");
                            }
                        }
                        Content::ToolCall(call) => {
                            let id = id_label(archive, call);
                            let args = short_args(call);
                            let (status, bytes, output) = outcome(messages, archive, call);
                            let _ = write!(
                                text,
                                "\n#{id} {} {args} -> {status}, {} bytes",
                                call.name, bytes,
                            );
                            // A failed edit or write changed nothing on disk.
                            if !matches!(status, "error" | "failed") {
                                note_file(&mut files, &mut changed, call, &id);
                            }
                            if matches!(status, "error" | "failed") {
                                failure = Some(Failure {
                                    id,
                                    name: call.name.to_string(),
                                    args,
                                    output: output
                                        .map(|output| output.to_string())
                                        .unwrap_or_default(),
                                });
                            }
                        }
                        Content::Thinking { .. } | Content::RedactedThinking { .. } => {}
                    }
                }
            }
            Message::ToolResult(_) => {}
        }
    }

    if !files.is_empty() {
        text.push_str("\n\nFiles:");
        for file in &files {
            let _ = write!(text, "\n- {}: ", file.path);
            if file.changed {
                let kind = if file.edited { "edited" } else { "written" };
                let _ = write!(text, "{kind} ({})", file.ids.join(", "));
            } else {
                let _ = write!(text, "read ({}), not in context", file.read_ids.join(", "));
            }
        }
    }

    if let Some(failure) = failure {
        let _ = write!(
            text,
            "\n\nLast failure: #{} {} {}\n",
            failure.id, failure.name, failure.args
        );
        if failure.output.starts_with(STUB_PREFIX) {
            let _ = write!(text, "[output removed; call expand with id {}]", failure.id);
        } else {
            text.push_str(tail(&failure.output));
        }
    }

    changed.reverse();
    Checkpoint { text, changed }
}

fn id_label(archive: &Archive, call: &ToolCall) -> String {
    archive
        .id_of(&call.id)
        .map_or_else(|| "?".to_string(), |id| id.to_string())
}

fn short_args(call: &ToolCall) -> String {
    text::head(&call.arguments, ARGS_BYTES).replace('\n', " ")
}

/// The status of a call, its original output size and its result text, when it has a result.
fn outcome(
    messages: &[Message],
    archive: &Archive,
    call: &ToolCall,
) -> (&'static str, usize, Option<Text>) {
    let result = messages.iter().find_map(|message| match message {
        Message::ToolResult(result) if result.call_id == call.id => Some(result),
        _ => None,
    });
    let Some(result) = result else {
        return ("no result", 0, None);
    };
    let bytes = archive
        .id_of(&call.id)
        .and_then(|id| archive.kept_len(id))
        .unwrap_or(result.content.len());
    let last = result.content.lines().last().unwrap_or("");
    let status = if result.is_error {
        "error"
    } else if (&*call.name == "bash"
        && last.starts_with("exit code: ")
        && !last.starts_with("exit code: 0"))
        || last.starts_with("timed out")
        || last.starts_with("cancelled")
    {
        "failed"
    } else {
        "ok"
    };
    (status, bytes, Some(result.content.clone()))
}

fn note_file(files: &mut Vec<FileEntry>, changed: &mut Vec<String>, call: &ToolCall, id: &str) {
    let kind = match &*call.name {
        "edit" | "write" | "read" => &*call.name,
        _ => return,
    };
    let Some(path) = path_of(call) else {
        return;
    };
    let index = match files.iter().position(|file| file.path == path) {
        Some(index) => index,
        None => {
            files.push(FileEntry {
                path: path.clone(),
                changed: false,
                edited: false,
                ids: Vec::new(),
                read_ids: Vec::new(),
            });
            files.len() - 1
        }
    };
    let file = &mut files[index];
    if kind == "read" {
        file.read_ids.push(format!("#{id}"));
        return;
    }
    file.changed = true;
    file.edited |= kind == "edit";
    file.ids.push(format!("#{id}"));
    changed.retain(|seen| *seen != path);
    changed.push(path);
}

fn path_of(call: &ToolCall) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    value.get("path")?.as_str().map(str::to_string)
}

/// The output's last lines, within the byte limit.
fn tail(output: &str) -> &str {
    let mut start = 0;
    let mut seen = 0;
    for (index, byte) in output.trim_end_matches('\n').bytes().enumerate().rev() {
        if byte == b'\n' {
            seen += 1;
            if seen == FAILURE_LINES {
                start = index + 1;
                break;
            }
        }
    }
    text::tail(output[start..].trim_end_matches('\n'), FAILURE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reagent_ai::AssistantMessage;

    fn call(id: &str, name: &str, arguments: &str) -> Content {
        Content::ToolCall(ToolCall {
            id: Text::from(id),
            name: Text::from(name),
            arguments: Text::from(arguments),
        })
    }

    fn assistant(content: Vec<Content>) -> Message {
        Message::assistant(AssistantMessage::new("p", "m", content))
    }

    fn archive_for(ids: &[&str]) -> Archive {
        let archive = Archive::default();
        for id in ids {
            archive.assign(&Text::from(*id));
        }
        archive
    }

    fn header() -> &'static str {
        HEADER.trim_end_matches('\n')
    }

    #[test]
    fn the_header_and_number_come_first() {
        let made = build(3, &[Message::user("hi")], &Archive::default());
        assert_eq!(
            made.text,
            format!("{}\n\n## Checkpoint 3\n\nUser: hi", header())
        );
        assert!(made.changed.is_empty());
    }

    #[test]
    fn user_messages_are_kept_word_for_word() {
        let made = build(
            1,
            &[Message::user("first line\n  second line\n\nlast")],
            &Archive::default(),
        );
        assert!(
            made.text
                .ends_with("\nUser: first line\n  second line\n\nlast")
        );
    }

    #[test]
    fn assistant_text_is_cut_at_1000_bytes_and_thinking_is_dropped() {
        let long = format!("{}\u{e9}{}", "a".repeat(999), "b".repeat(50));
        let reply = assistant(vec![
            Content::Thinking {
                thinking: Text::from("secret"),
                signature: None,
            },
            Content::RedactedThinking {
                data: Text::from("blob"),
            },
            Content::text(long),
            Content::text("short"),
        ]);
        let made = build(1, &[reply], &Archive::default());
        assert!(
            made.text
                .contains(&format!("\nAssistant: {} [cut]\n", "a".repeat(999)))
        );
        assert!(made.text.ends_with("\nAssistant: short"));
        assert!(!made.text.contains("secret"));
        assert!(!made.text.contains("blob"));
        assert!(!made.text.contains('\u{e9}'));
    }

    #[test]
    fn a_tool_call_line_has_id_args_status_and_size() {
        let archive = archive_for(&["c1", "c2", "c3", "c4"]);
        let messages = [
            assistant(vec![call("c4", "edit", r#"{"path":"src/a.rs"}"#)]),
            Message::tool_result("c4", "edited src/a.rs", false),
        ];
        let made = build(1, &messages, &archive);
        assert!(
            made.text
                .contains("\n#4 edit {\"path\":\"src/a.rs\"} -> ok, 15 bytes")
        );
        assert!(made.text.contains("\n\nFiles:\n- src/a.rs: edited (#4)"));
        assert_eq!(made.changed, vec!["src/a.rs"]);
    }

    #[test]
    fn a_bash_call_with_a_nonzero_exit_is_a_failure() {
        let archive = archive_for(&["b1", "b2", "b3", "x1"]);
        let messages = [
            assistant(vec![
                call("b1", "bash", "{}"),
                call("b2", "bash", "{}"),
                call("b3", "bash", "{}"),
                call("x1", "other", "{}"),
            ]),
            Message::tool_result("b1", "out\nexit code: 2", false),
            Message::tool_result("b2", "out\nexit code: 0", false),
            Message::tool_result("b3", "timed out after 5s", false),
            Message::tool_result("x1", "exit code: 3", false),
        ];
        let text = build(1, &messages, &archive).text;
        assert!(text.contains("#1 bash {} -> failed, 16 bytes"));
        assert!(text.contains("#2 bash {} -> ok, 16 bytes"));
        assert!(text.contains("#3 bash {} -> failed, 18 bytes"));
        assert!(text.contains("#4 other {} -> ok, 12 bytes"));
        assert!(text.contains("\n\nLast failure: #3 bash {}\ntimed out after 5s"));
    }

    #[test]
    fn the_last_failure_shows_the_end_of_its_output() {
        let archive = archive_for(&["b1"]);
        let output: Vec<String> = (1..=100).map(|n| format!("line {n}")).collect();
        let messages = [
            assistant(vec![call("b1", "bash", "{}")]),
            Message::tool_result("b1", output.join("\n") + "\nexit code: 1\n", false),
        ];
        let text = build(1, &messages, &archive).text;
        let shown = text.split("Last failure: #1 bash {}\n").nth(1).unwrap();
        assert_eq!(shown.lines().count(), 40);
        assert!(shown.starts_with("line 62\n"));
        assert!(shown.ends_with("exit code: 1"));
    }

    #[test]
    fn the_last_failure_output_is_cut_to_4096_bytes() {
        let archive = archive_for(&["b1"]);
        let body = format!("{}\nexit code: 1", "\u{e9}".repeat(5000));
        let messages = [
            assistant(vec![call("b1", "bash", "{}")]),
            Message::tool_result("b1", body, true),
        ];
        let text = build(1, &messages, &archive).text;
        let shown = text.split("Last failure: #1 bash {}\n").nth(1).unwrap();
        assert!(shown.len() <= 4096);
        assert!(shown.len() >= 4094);
        assert!(shown.ends_with("exit code: 1"));
    }

    #[test]
    fn a_masked_output_counts_its_original_size() {
        let archive = archive_for(&["c1"]);
        archive.keep(1, Text::from("x".repeat(5000)));
        let messages = [
            assistant(vec![call("c1", "read", r#"{"path":"big.txt"}"#)]),
            Message::tool_result("c1", "[Old tool output #1, 5000 bytes]", false),
        ];
        let text = build(1, &messages, &archive).text;
        assert!(text.contains("-> ok, 5000 bytes"));
        assert!(text.contains("- big.txt: read (#1), not in context"));
    }

    #[test]
    fn a_masked_failure_points_to_expand() {
        let archive = archive_for(&["b1"]);
        archive.keep(1, Text::from("x".repeat(5000)));
        let messages = [
            assistant(vec![call("b1", "bash", "{}")]),
            Message::tool_result("b1", "[Old tool output #1, 5000 bytes]", true),
        ];
        let text = build(1, &messages, &archive).text;
        assert!(text.contains("-> error, 5000 bytes"));
        assert!(
            text.ends_with("Last failure: #1 bash {}\n[output removed; call expand with id 1]")
        );
    }

    #[test]
    fn files_list_edits_writes_and_reads_once_each() {
        let archive = archive_for(&["1", "2", "3", "4", "5", "6", "7"]);
        let messages = [assistant(vec![
            call("1", "read", r#"{"path":"a.rs"}"#),
            call("2", "read", r#"{"path":"b.rs"}"#),
            call("3", "write", r#"{"path":"c.rs"}"#),
            call("4", "edit", r#"{"path":"a.rs"}"#),
            call("5", "write", r#"{"path":"a.rs"}"#),
            call("6", "edit", "not json"),
            call("7", "read", r#"{"path":"b.rs"}"#),
        ])];
        let text = build(1, &messages, &archive).text;
        let files = text.split("\n\nFiles:\n").nth(1).unwrap();
        assert_eq!(
            files,
            "- a.rs: edited (#4, #5)\n- b.rs: read (#2, #7), not in context\n- c.rs: written (#3)"
        );
    }

    #[test]
    fn changed_files_are_newest_first() {
        let archive = archive_for(&["1", "2", "3", "4"]);
        let messages = [assistant(vec![
            call("1", "edit", r#"{"path":"a.rs"}"#),
            call("2", "write", r#"{"path":"b.rs"}"#),
            call("3", "edit", r#"{"path":"c.rs"}"#),
            call("4", "edit", r#"{"path":"a.rs"}"#),
        ])];
        let made = build(1, &messages, &archive);
        assert_eq!(made.changed, vec!["a.rs", "c.rs", "b.rs"]);
    }

    #[test]
    fn a_failed_edit_is_not_a_changed_file() {
        let archive = archive_for(&["1", "2"]);
        let messages = [
            assistant(vec![
                call("1", "edit", r#"{"path":"a.rs"}"#),
                call("2", "edit", r#"{"path":"b.rs"}"#),
            ]),
            Message::tool_result("1", "no match for old_string", true),
            Message::tool_result("2", "edited", false),
        ];
        let made = build(1, &messages, &archive);
        assert_eq!(made.changed, vec!["b.rs"]);
        assert!(!made.text.contains("- a.rs"));
    }

    #[test]
    fn long_arguments_are_cut_at_160_bytes() {
        let archive = archive_for(&["1"]);
        let args = format!("{}\n{}", "a".repeat(100), "\u{e9}".repeat(100));
        let messages = [assistant(vec![call("1", "grep", &args)])];
        let text = build(1, &messages, &archive).text;
        let line = text.lines().last().unwrap();
        let shown = line
            .strip_prefix("#1 grep ")
            .unwrap()
            .strip_suffix(" -> no result, 0 bytes")
            .unwrap();
        assert_eq!(shown.len(), 159);
        assert!(shown.starts_with(&"a".repeat(100)));
        assert!(!shown.contains('\n'));
    }

    #[test]
    fn empty_sections_are_left_out() {
        let archive = archive_for(&["1"]);
        let messages = [
            assistant(vec![call("1", "ls", "{}")]),
            Message::tool_result("1", "ok", false),
            assistant(vec![call("zz", "ls", "{}")]),
        ];
        let text = build(1, &messages, &archive).text;
        assert!(!text.contains("Files:"));
        assert!(!text.contains("Last failure"));
        assert!(text.ends_with("\n#? ls {} -> no result, 0 bytes"));
    }
}
