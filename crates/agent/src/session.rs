//! Append-only JSONL session writer.

use std::io;
use std::path::Path;

use serde::Serialize;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};

use reagent_ai::Message;

/// One line of a session file: a message plus the tree ids that parent it.
///
/// The session assigns every [`Entry`] an id and links it to the previous one
/// through `parent_id`. The root entry has no parent. Every append goes to the
/// end of the file, so the lines are a write-ahead log of the conversation.
///
/// The id text and the message are borrowed, so writing an entry copies
/// neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry<'a> {
    /// This entry's id in the session tree.
    pub id: &'a str,
    /// The id of the entry that precedes this one, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<&'a str>,
    /// The message this entry records.
    pub message: &'a Message,
}

/// The largest line buffer a session keeps for reuse. A message with a huge
/// tool result would otherwise leave its JSON encoding resident for the run.
const LINE_KEEP_BYTES: usize = 64 * 1024;

/// Appends one [`Entry`] per message to a JSONL file.
///
/// Writes go through a buffered async writer and are durable once
/// [`Session::flush`] (at each turn end) or [`Session::close`] runs. The file
/// is never read back, so the whole session is never held in memory.
#[derive(Debug)]
pub struct Session {
    writer: BufWriter<File>,
    prefix: String,
    counter: u64,
    /// The id of the entry written last: the parent of the next one.
    last_id: Option<String>,
    /// The id text of the entry being written, reused across appends.
    id: String,
    /// The serialized line of the entry being written, reused across appends.
    line: Vec<u8>,
}

impl Session {
    /// Opens `path` for appending, creating it if it does not exist.
    pub async fn open(path: &Path) -> io::Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(Self {
            writer: BufWriter::new(file),
            prefix: nanos_hex(),
            counter: 0,
            last_id: None,
            id: String::new(),
            line: Vec::new(),
        })
    }

    /// Appends one message as an entry, returning its id.
    ///
    /// The id comes from a per-file prefix plus a counter; the entry's parent
    /// is the previous entry. The message text itself is not copied.
    ///
    /// The entry is serialized from borrowed fields into a buffer the session
    /// owns, so an append costs one allocation: the id handed back to the
    /// caller.
    pub async fn append(&mut self, message: &Message) -> io::Result<String> {
        let mut id = std::mem::take(&mut self.id);
        id.clear();
        id.push_str(&self.prefix);
        push_hex(&mut id, self.counter);
        self.counter += 1;

        let entry = Entry {
            id: &id,
            parent_id: self.last_id.as_deref(),
            message,
        };
        self.line.clear();
        serde_json::to_writer(&mut self.line, &entry).map_err(io::Error::other)?;
        self.line.push(b'\n');

        let appended = id.clone();
        // The id becomes the parent before the write, as it always did, so a
        // failed write leaves the same chain. The previous id text becomes the
        // buffer the next id is built in.
        self.id = self.last_id.replace(id).unwrap_or_default();
        self.writer.write_all(&self.line).await?;
        if self.line.capacity() > LINE_KEEP_BYTES {
            self.line = Vec::new();
        }
        Ok(appended)
    }

    /// Writes the buffered entries to the file. Call after each turn.
    pub async fn flush(&mut self) -> io::Result<()> {
        self.writer.flush().await
    }

    /// Flushes and closes the file.
    pub async fn close(mut self) -> io::Result<()> {
        self.flush().await
    }
}

/// Writes `value` as lowercase hex with at least four digits, the same text as
/// `format!("{value:04x}")`, without building a `String` of its own.
fn push_hex(out: &mut String, value: u64) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut width = 1;
    while width < 16 && value >> (width * 4) != 0 {
        width += 1;
    }
    for _ in width..4 {
        out.push('0');
    }
    for place in (0..width).rev() {
        out.push(DIGITS[(value >> (place * 4)) as usize & 0xf] as char);
    }
}

fn nanos_hex() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}")
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn message(n: usize) -> Message {
        Message::user(Arc::from(format!("message {n}")))
    }

    #[tokio::test]
    async fn open_and_append_write_one_json_line_per_message() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.jsonl");
        let mut session = Session::open(&path).await.expect("open");
        session.append(&message(1)).await.expect("append");
        session.append(&message(2)).await.expect("append");
        session.close().await.expect("close");

        let text = tokio::fs::read_to_string(&path).await.expect("read");
        assert_eq!(text.lines().count(), 2);
    }

    #[tokio::test]
    async fn entries_carry_id_and_parent_id_chain() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.jsonl");
        let mut session = Session::open(&path).await.expect("open");
        let a = session.append(&message(1)).await.expect("append a");
        let b = session.append(&message(2)).await.expect("append b");
        session.close().await.expect("close");

        assert_ne!(a, b, "ids are unique");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "the id is hex");

        let text = tokio::fs::read_to_string(&path).await.expect("read");
        let mut lines = text.lines();
        let first: serde_json::Value =
            serde_json::from_str(lines.next().expect("first line")).expect("parse");
        assert_eq!(first["id"], a);
        assert!(first.get("parentId").is_none(), "root has no parent");
        let second: serde_json::Value =
            serde_json::from_str(lines.next().expect("second line")).expect("parse");
        assert_eq!(second["id"], b);
        assert_eq!(second["parentId"], a);
    }

    #[tokio::test]
    async fn flush_makes_entries_visible_before_close() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.jsonl");
        let mut session = Session::open(&path).await.expect("open");
        session.append(&message(1)).await.expect("append");

        session.flush().await.expect("flush");
        drop(session);

        let text = tokio::fs::read_to_string(&path).await.expect("read");
        let value: serde_json::Value = serde_json::from_str(text.trim()).expect("parse");
        assert_eq!(value["message"]["content"][0]["text"], "message 1");
    }

    #[tokio::test]
    async fn append_only_preserves_first_writers_lines() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.jsonl");

        {
            let mut session = Session::open(&path).await.expect("open");
            session.append(&message(1)).await.expect("append");
            session.close().await.expect("close");
        }
        {
            let mut session = Session::open(&path).await.expect("reopen");
            session.append(&message(2)).await.expect("append");
            session.close().await.expect("close");
        }

        let text = tokio::fs::read_to_string(&path).await.expect("read");
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains(r#""text":"message 1""#));
        assert!(text.contains(r#""text":"message 2""#));
    }

    #[test]
    fn perf_session_a_large_message_does_not_keep_its_line_buffer() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.jsonl");
        let big = Message::user(Arc::from("x".repeat(200_000)));
        runtime.block_on(async {
            let mut session = Session::open(&path).await.expect("open");
            session.append(&big).await.expect("append big");
            assert!(
                session.line.capacity() <= LINE_KEEP_BYTES,
                "{} bytes of line buffer kept",
                session.line.capacity()
            );
            session.close().await.expect("close");
        });
    }

    #[test]
    fn perf_session_the_id_counter_formats_like_format_04x() {
        for value in [
            0,
            1,
            0xa,
            0xf,
            0x10,
            0xfff,
            0x1000,
            0xffff,
            0x10000,
            0xfffff,
            0x1234_5678_9abc_def0,
            u64::MAX,
        ] {
            let mut text = String::from("ab");
            push_hex(&mut text, value);
            assert_eq!(text, format!("ab{value:04x}"), "value {value}");
        }
    }
}
