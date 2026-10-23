//! The ledger of lines the model already read in this run.
//!
//! A read of lines an earlier, still-valid result already covers keeps its
//! content and gains a one-line note after it. The note tells the model that
//! the earlier result is still in the history, so it learns not to re-read.
//!
//! An entry stays valid while the run goes on, the file keeps its identity,
//! its size and its modified time, and its content is still the one the history
//! holds. Masking replaces that content and compaction drops the message, so
//! both make an entry invisible with no bookkeeping beyond comparing the `Arc`.
//! A scan that finds an entry invisible drops it, so the ledger never holds a
//! text the history let go and never scans a dead entry twice.

use std::fmt;

use reagent_ai::{Message, Text, ToolCall};

use crate::tool::{Identity, Registry, Seen};

/// One read whose result the model may still hold.
struct Entry {
    /// The file's identity, from the metadata of the handle the read opened.
    key: Identity,
    /// The first line the earlier result showed.
    start: u64,
    /// The last line the earlier result showed.
    end: u64,
    /// The file's size when the earlier result was made.
    size: u64,
    /// The file's modified time when the earlier result was made.
    modified: Option<std::time::SystemTime>,
    /// The content the history holds, shared with it, for the visibility check.
    content: Text,
    /// How many later reads this entry already answered with a note.
    repeats: u32,
}

/// The reads of one run, in the order they happened.
#[derive(Default)]
pub(crate) struct Ledger {
    entries: Vec<Entry>,
}

impl Entry {
    /// Whether this entry already showed every line of `seen`, for the same
    /// file in the same state.
    fn covers(&self, seen: &Seen) -> bool {
        self.key == seen.key
            && self.size == seen.size
            && self.modified == seen.modified
            && self.start <= seen.start
            && seen.end <= self.end
    }
}

/// Whether the history still holds `content`: masking replaces it and
/// compaction removes it, and both drop the `Arc` this compares with.
fn holds(history: &[Message], content: &Text) -> bool {
    history.iter().any(|message| {
        matches!(message, Message::ToolResult(result)
            if std::sync::Arc::ptr_eq(&result.content, content))
    })
}

impl Ledger {
    /// Whether a reply needs [`Ledger::forget_writes`] at all.
    ///
    /// A ledger with no entry and a reply that shows no read has nothing a
    /// write could drop, so it needs no tool resolved and no argument parsed:
    /// this is every turn of a run whose tools report no lines.
    pub(crate) fn needs_writes(&self, shows_read: bool) -> bool {
        !self.entries.is_empty() || shows_read
    }

    /// Notes a re-read of lines an earlier, still-visible result covers.
    ///
    /// Returns the message to record: the same result, with the note after
    /// its content when `seen` is covered. A result with no note is recorded
    /// so a later read can be answered.
    pub(crate) fn apply(&mut self, message: Message, seen: Seen, history: &[Message]) -> Message {
        let mut message = message;
        let Message::ToolResult(result) = &mut message else {
            return message;
        };
        let Some(repeats) = self.repeat(&seen, history) else {
            self.entries.push(Entry {
                key: seen.key,
                start: seen.start,
                end: seen.end,
                size: seen.size,
                modified: seen.modified,
                // Shared with the result, so no text is copied and the
                // visibility check sees the history's own `Arc`.
                content: Text::clone(&result.content),
                repeats: 0,
            });
            return message;
        };
        // Two blocks for the noted content: the buffer below, sized for the
        // content, the line break and the note's own length, counted once
        // without building it, and then the one `Arc<str>` that `Text` needs,
        // which copies the buffer because the counts sit in front of the text.
        let mut counted = NoteLen(0);
        let _ = write_note(&mut counted, &seen, repeats);
        let newline = usize::from(!result.content.ends_with('\n'));
        let mut text = String::with_capacity(result.content.len() + newline + counted.0);
        text.push_str(&result.content);
        if newline == 1 {
            text.push('\n');
        }
        let _ = write_note(&mut text, &seen, repeats);
        result.content = Text::from(text);
        message
    }

    /// Counts this read as a repeat of the newest visible entry covering it.
    ///
    /// `None` when no entry covers the range; the read then becomes an entry.
    /// The newest covering entry wins, and an entry whose content the history
    /// no longer holds is dropped on the way, since nothing can answer a read
    /// with it again.
    fn repeat(&mut self, seen: &Seen, history: &[Message]) -> Option<u32> {
        let mut at = self.entries.len();
        while at > 0 {
            at -= 1;
            if !self.entries[at].covers(seen) {
                continue;
            }
            if holds(history, &self.entries[at].content) {
                let entry = &mut self.entries[at];
                entry.repeats += 1;
                return Some(entry.repeats);
            }
            self.entries.remove(at);
        }
        None
    }

    /// Drops the entries for every file these calls write, and returns those
    /// files' identities.
    ///
    /// A read's key is the identity of the file it opened, so one metadata call
    /// on the written path is enough to match it, with no path comparison and
    /// no canonicalize. A write that replaced the file has a new identity, so
    /// it cannot match a read of the file it replaced; its new size and
    /// modified time keep the entry from covering a read anyway.
    ///
    /// Only a tool that can write is asked for its effects: a read-only tool
    /// cannot touch a file the ledger holds. A caller with no entry and no read
    /// to record skips this call entirely (see [`Ledger::needs_writes`]), so a
    /// run that writes nothing parses no argument at all.
    pub(crate) async fn forget_writes(
        &mut self,
        calls: &[ToolCall],
        registry: &Registry,
    ) -> Vec<Identity> {
        let mut written = Vec::new();
        for call in calls {
            let Some(tool) = registry.resolve(&call.name) else {
                continue;
            };
            if tool.read_only() {
                continue;
            }
            for path in tool.effects(call.arguments.as_ref()).writes {
                let Ok(metadata) = tokio::fs::metadata(&path).await else {
                    // A path that no longer exists cannot have been read.
                    continue;
                };
                let identity = Identity::new(&metadata, &path).await;
                self.entries.retain(|entry| entry.key != identity);
                written.push(identity);
            }
        }
        written
    }
}

/// Counts the bytes a note takes without building it, so the noted content's
/// buffer is sized for exactly the content and the note.
struct NoteLen(usize);

impl fmt::Write for NoteLen {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0 += text.len();
        Ok(())
    }
}

/// Writes the note after a re-read, with the count from the third read on.
fn write_note(note: &mut impl fmt::Write, seen: &Seen, repeats: u32) -> fmt::Result {
    write!(
        note,
        "[Note: you already read lines {}-{} of {} in this run, and the file has not changed. \
         That earlier result is still valid; you do not need to read this range again.]",
        seen.start, seen.end, seen.path
    )?;
    if repeats >= 2 {
        note.write_str(" (")?;
        write_ordinal(note, repeats + 1)?;
        note.write_str(" time)")?;
    }
    Ok(())
}

/// Writes `3rd`, `4th`, `11th`, `21st` into `note`, so a note with a count
/// still needs no text of its own.
fn write_ordinal(note: &mut impl fmt::Write, n: u32) -> fmt::Result {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    write!(note, "{n}{suffix}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::SystemTime;

    use futures_util::future::BoxFuture;
    use reagent_ai::ToolSpec;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::tool::{Effects, Tool, ToolOutput};

    /// The identity the fake reads report for their one file.
    fn file_key() -> Identity {
        #[cfg(unix)]
        {
            Identity::Inode { dev: 1, ino: 2 }
        }
        #[cfg(not(unix))]
        {
            Identity::Canonical(PathBuf::from("/work/f.txt"))
        }
    }

    /// A read of `path` with a fixed file state.
    fn seen(path: &str, start: u64, end: u64, size: u64) -> Seen {
        Seen {
            path: Text::from(path),
            key: file_key(),
            start,
            end,
            size,
            modified: Some(SystemTime::UNIX_EPOCH),
        }
    }

    /// Applies one read result as the loop does and returns its text.
    fn read(ledger: &mut Ledger, history: &mut Vec<Message>, content: &str, seen: Seen) -> String {
        let message = ledger.apply(Message::tool_result("call", content, false), seen, history);
        let Message::ToolResult(result) = &message else {
            panic!("expected a tool result");
        };
        let text = result.content.to_string();
        history.push(message);
        text
    }

    #[test]
    fn reread_the_same_range_twice_adds_the_note() {
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        let first = read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 1, 4));
        assert_eq!(first, "1\ta\n");
        let second = read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 1, 4));
        assert_eq!(
            second,
            concat!(
                "1\ta\n",
                "[Note: you already read lines 1-1 of f.txt in this run, and the ",
                "file has not changed. That earlier result is still valid; you do ",
                "not need to read this range again.]"
            )
        );
    }

    #[test]
    fn reread_a_contained_range_is_noted_with_the_new_range() {
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        read(
            &mut ledger,
            &mut history,
            "1\ta\n",
            seen("f.txt", 1, 10, 40),
        );
        let second = read(
            &mut ledger,
            &mut history,
            "3\tc\n4\td\n",
            seen("f.txt", 3, 4, 40),
        );
        assert!(
            second.contains("you already read lines 3-4 of f.txt"),
            "{second}"
        );
    }

    #[test]
    fn reread_new_lines_are_not_noted() {
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        let second = read(&mut ledger, &mut history, "2\tb\n", seen("f.txt", 2, 4, 40));
        assert_eq!(second, "2\tb\n", "a range with new lines gets no note");
    }

    #[test]
    fn reread_a_changed_size_has_no_note() {
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        let second = read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 41));
        assert_eq!(second, "1\ta\n");
    }

    #[test]
    fn reread_a_changed_modified_time_has_no_note() {
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        let mut later = seen("f.txt", 1, 3, 40);
        later.modified = Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1));
        let second = read(&mut ledger, &mut history, "1\ta\n", later);
        assert_eq!(second, "1\ta\n");
    }

    #[test]
    fn reread_a_masked_result_has_no_note() {
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        // Masking replaces the content with a stub; the entry no longer shows.
        let Message::ToolResult(result) = &mut history[1] else {
            panic!("expected a tool result");
        };
        result.content = Text::from("[masked]");
        let second = read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        assert_eq!(second, "1\ta\n");
    }

    #[test]
    fn reread_a_masked_entry_is_dropped() {
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        let masked = Text::from("[masked]");
        let Message::ToolResult(result) = &mut history[1] else {
            panic!("expected a tool result");
        };
        result.content = Text::clone(&masked);
        read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        assert_eq!(
            ledger.entries.len(),
            1,
            "the dead entry is gone, the new one stays"
        );
        assert!(
            !std::sync::Arc::ptr_eq(&ledger.entries[0].content, &masked),
            "the ledger still held the text of the masked result"
        );
    }

    #[test]
    fn reread_a_compacted_away_result_has_no_note() {
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        // Compaction removes the message that held the earlier result.
        history.remove(1);
        let second = read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        assert_eq!(second, "1\ta\n");
    }

    #[test]
    fn reread_the_third_read_names_the_third_time() {
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        let second = read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        assert!(!second.contains("time)"), "{second}");
        let third = read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        assert!(third.ends_with("] (3rd time)"), "{third}");
        let fourth = read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 3, 40));
        assert!(fourth.ends_with("] (4th time)"), "{fourth}");
    }

    #[test]
    fn reread_ordinals_are_suffixed_correctly() {
        let cases = [
            (1, "1st"),
            (2, "2nd"),
            (3, "3rd"),
            (4, "4th"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (21, "21st"),
            (22, "22nd"),
            (23, "23rd"),
            (111, "111th"),
        ];
        for (n, want) in cases {
            let mut written = String::new();
            write_ordinal(&mut written, n).expect("write");
            assert_eq!(written, want, "{n}");
        }
    }

    #[test]
    fn reread_the_counted_note_length_is_the_note() {
        for repeats in 0..4 {
            let seen = seen("f.txt", 1, 3, 40);
            let mut counted = NoteLen(0);
            write_note(&mut counted, &seen, repeats).expect("count");
            let mut note = String::new();
            write_note(&mut note, &seen, repeats).expect("write");
            assert_eq!(counted.0, note.len(), "{repeats}: {note}");
        }
    }

    #[test]
    fn reread_the_note_follows_byte_identical_content() {
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        let content = "1\ta\n2\tb\n";
        read(&mut ledger, &mut history, content, seen("f.txt", 1, 2, 40));
        let second = read(&mut ledger, &mut history, content, seen("f.txt", 1, 2, 40));
        let rest = second
            .strip_prefix(content)
            .expect("the content stays byte-identical before the note");
        assert!(rest.starts_with("[Note:"), "{second}");
    }

    #[test]
    fn reread_a_note_goes_on_its_own_line() {
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        read(&mut ledger, &mut history, "1\ta", seen("f.txt", 1, 1, 3));
        let second = read(&mut ledger, &mut history, "1\ta", seen("f.txt", 1, 1, 3));
        assert!(second.starts_with("1\ta\n[Note:"), "{second}");
    }

    fn tool_spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "A test tool.".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    /// A write tool that names the file it writes.
    struct FakeWrite {
        spec: ToolSpec,
        dir: PathBuf,
    }

    impl Tool for FakeWrite {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn effects(&self, args: &str) -> Effects {
            let value: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
            let writes = value["path"]
                .as_str()
                .map(|path| vec![self.dir.join(path)])
                .unwrap_or_default();
            Effects {
                reads: Vec::new(),
                writes,
            }
        }

        fn run<'a>(
            &'a self,
            _args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async { ToolOutput::ok("wrote") })
        }
    }

    /// A read-only tool that says whether its effects were asked for.
    struct FakeRead {
        spec: ToolSpec,
        asked: Arc<AtomicBool>,
    }

    impl Tool for FakeRead {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn read_only(&self) -> bool {
            true
        }

        fn effects(&self, _args: &str) -> Effects {
            self.asked.store(true, Ordering::Relaxed);
            Effects {
                reads: vec![PathBuf::from("/work/f.txt")],
                writes: Vec::new(),
            }
        }

        fn run<'a>(
            &'a self,
            _args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async { ToolOutput::ok("1\ta\n") })
        }
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: Text::from("call_2"),
            name: Text::from(name),
            arguments: Text::from(arguments),
        }
    }

    /// The identity of the file at `path`, as a read of it would key it.
    async fn identity_of(path: &std::path::Path) -> Identity {
        let metadata = tokio::fs::metadata(path)
            .await
            .expect("the file's metadata");
        Identity::new(&metadata, path).await
    }

    #[tokio::test]
    async fn reread_a_write_of_the_file_drops_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        tokio::fs::write(&path, "a\n").await.unwrap();
        let mut keyed = seen("f.txt", 1, 1, 2);
        keyed.key = identity_of(&path).await;

        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        read(&mut ledger, &mut history, "1\ta\n", keyed.clone());

        let mut registry = Registry::new();
        registry.add(FakeWrite {
            spec: tool_spec("write"),
            dir: tmp.path().to_path_buf(),
        });
        let calls = [call("write", r#"{"path":"f.txt"}"#)];
        let written = ledger.forget_writes(&calls, &registry).await;
        assert_eq!(written, vec![keyed.key.clone()], "the write names its file");

        let second = read(&mut ledger, &mut history, "1\ta\n", keyed);
        assert_eq!(second, "1\ta\n", "the write dropped the entry");
    }

    #[tokio::test]
    async fn reread_a_read_only_tool_is_never_asked_for_its_effects() {
        let asked = Arc::new(AtomicBool::new(false));
        let mut registry = Registry::new();
        registry.add(FakeRead {
            spec: tool_spec("read"),
            asked: Arc::clone(&asked),
        });
        let mut ledger = Ledger::default();
        let mut history = vec![Message::user("hi")];
        read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 1, 4));

        let written = ledger
            .forget_writes(&[call("read", r#"{"path":"f.txt"}"#)], &registry)
            .await;

        assert!(written.is_empty(), "a read-only call writes nothing");
        assert!(
            !asked.load(Ordering::Relaxed),
            "a read-only tool was asked for its effects"
        );
        let second = read(&mut ledger, &mut history, "1\ta\n", seen("f.txt", 1, 1, 4));
        assert!(second.contains("[Note:"), "{second}");
    }
}
