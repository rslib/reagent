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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// This entry's id in the session tree.
    pub id: String,
    /// The id of the entry that precedes this one, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// The message this entry records.
    pub message: Message,
}

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
    last_id: Option<String>,
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
        })
    }

    /// Appends one message as an entry, returning its id.
    ///
    /// The id comes from a per-file prefix plus a counter; the entry's parent
    /// is the previous entry. The message text itself is not copied.
    pub async fn append(&mut self, message: &Message) -> io::Result<String> {
        let id = self.next_id();
        let entry = Entry {
            id: id.clone(),
            parent_id: self.last_id.clone(),
            message: message.clone(),
        };
        self.last_id = Some(id.clone());
        let mut line = serde_json::to_string(&entry).map_err(io::Error::other)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        Ok(id)
    }

    /// Writes the buffered entries to the file. Call after each turn.
    pub async fn flush(&mut self) -> io::Result<()> {
        self.writer.flush().await
    }

    /// Flushes and closes the file.
    pub async fn close(mut self) -> io::Result<()> {
        self.flush().await
    }

    fn next_id(&mut self) -> String {
        let id = format!("{}{:04x}", self.prefix, self.counter);
        self.counter += 1;
        id
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
}
