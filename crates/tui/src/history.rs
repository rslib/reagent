//! A session's earlier messages, read from its journal.
//!
//! A client catches up on its own: it opens the session folder with
//! `reagent-store`, reads the records up to the one the worker committed and
//! decodes the message records, so the worker spends no CPU on the catch-up.
//! Reading blocks, so it runs in one `spawn_blocking` job.
//!
//! The history only keeps the newest [`KEEP`] messages: a session of thousands
//! of turns costs the start time of its tail, not of all its words, and the app
//! shows one dim line for what it left out.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};

use reagent_ai::record::{MESSAGE, decode_message};
use reagent_ai::{Content, Message, Text, ToolCall};
use reagent_store::{Session, kind};

/// How many of the newest messages the app renders when it continues a
/// session.
pub const KEEP: usize = 200;

/// The kind of record the agent's journal writes as a base: a point a resume
/// starts from, followed by the messages of the history it restates.
pub const BASE: u8 = kind::RECORD_FIRST + 3;

/// The header the agent starts every checkpoint with, the same template it
/// builds them from.
pub(crate) const HEADER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/checkpoint.md"
));

/// A compaction the journal records: the checkpoint its base holds, and the
/// messages of the restatement the row stands in for.
#[derive(Debug)]
pub struct Checkpoint {
    /// The checkpoint text, the journal's own.
    pub text: Text,
    /// How many messages the base restates, the checkpoint among them.
    pub messages: u32,
}

/// The newest messages of a session, oldest first.
#[derive(Debug, Default)]
pub struct Tail {
    /// The messages to render.
    pub messages: VecDeque<Message>,
    /// The compactions the journal records, each with the index of the message
    /// the row it draws stands before, oldest first.
    pub checkpoints: Vec<(usize, Checkpoint)>,
    /// The tool calls of the messages read, oldest first, the ones the messages
    /// left out carry too. The panels show a call whether its message is drawn
    /// or not, so a resumed session keeps the files its oldest turns touched.
    pub calls: Vec<ToolCall>,
    /// How many earlier messages were left out.
    pub omitted: usize,
}

/// The messages of the session in `dir`, up to and including record `upto`:
/// the newest [`KEEP`] of them and the count of the ones left out.
///
/// Every other kind of record (call start, turn end, base, approval) is
/// skipped: those carry agent types and draw nothing.
pub async fn read(dir: PathBuf, upto: u64) -> io::Result<Tail> {
    tokio::task::spawn_blocking(move || read_blocking(&dir, upto))
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
}

/// The messages recorded after `from` and up to `upto`, oldest first, each with
/// the record that holds it.
///
/// A client the worker cut reads this range to see what it missed while it was
/// away: a reply that finished, and a tool result that landed, are records by
/// then, so the catch-up frame the worker replays carries only the rest. The
/// record number says whether the client drew that message live already.
pub async fn since(dir: PathBuf, from: u64, upto: u64) -> io::Result<Vec<(u64, Message)>> {
    tokio::task::spawn_blocking(move || since_blocking(&dir, from, upto))
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
}

/// The output text of the tool results recorded after `from` and up to
/// `upto`, oldest first, each with the call it answers.
///
/// A `tool_result` event names the call and its size but carries no text, so
/// the app reads the text of the calls it shows from here. Both parts are the
/// journal's own [`Text`], so naming one copies no output.
pub async fn results(dir: PathBuf, from: u64, upto: u64) -> io::Result<Vec<(Text, Text)>> {
    tokio::task::spawn_blocking(move || results_blocking(&dir, from, upto))
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
}

/// The checkpoint text the first base after `from` holds: what the row of a
/// live compaction draws, read when the user opens it.
pub async fn checkpoint(dir: PathBuf, from: u64, upto: u64) -> io::Result<Option<Text>> {
    tokio::task::spawn_blocking(move || checkpoint_blocking(&dir, from, upto))
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
}

/// The blocking read behind [`checkpoint`].
fn checkpoint_blocking(dir: &Path, from: u64, upto: u64) -> io::Result<Option<Text>> {
    let session = Session::open(dir)?;
    let mut text = None;
    let mut based = false;
    for record in session.records_from(from + 1) {
        let record = record?;
        if record.seq > upto {
            break;
        }
        match record.kind {
            // The first base after `from` is the row's own compaction, and its
            // last checkpoint is the one it added. A later base is another row.
            BASE if based => break,
            BASE => based = true,
            MESSAGE => {
                if let Some(found) = checkpoint_text(&decode_message(&record.payload)?) {
                    text = Some(found);
                }
            }
            _ => {}
        }
    }
    Ok(text)
}

/// The text of a message that is a checkpoint, which the agent writes one of
/// first in such a message.
fn checkpoint_text(message: &Message) -> Option<Text> {
    let Message::User { content } = message else {
        return None;
    };
    match content.first()? {
        Content::Text { text } if text.starts_with(HEADER.trim_end()) => Some(Text::clone(text)),
        _ => None,
    }
}

/// The blocking read behind [`read`].
fn read_blocking(dir: &Path, upto: u64) -> io::Result<Tail> {
    let session = Session::open(dir)?;
    let mut tail = Tail::default();
    let mut ends: Option<HashMap<u64, u64>> = None;
    // The last record of the restatement being read, 0 outside one.
    let mut restated = 0;
    // The row of that restatement's checkpoint, so its messages count up.
    let mut open: Option<usize> = None;
    // Whether no message came before the base being read, the base of a session
    // that writes its history from nothing.
    let mut empty = true;
    for record in session.records() {
        let record = record?;
        if record.seq > upto {
            break;
        }
        if record.kind == BASE {
            // A base that follows no message writes the history from nothing, so
            // its messages are shown as any other. A later one restates messages
            // the transcript already drew: they are hidden, and its checkpoint
            // draws one row where the base stands.
            restated = if std::mem::take(&mut empty) {
                0
            } else {
                let ends = match &ends {
                    Some(ends) => ends,
                    None => ends.insert(session.base_ends().into_iter().collect()),
                };
                settle(&mut tail.checkpoints, open.take());
                ends.get(&record.seq).copied().unwrap_or(record.seq)
            };
            continue;
        }
        if record.kind != MESSAGE {
            continue;
        }
        empty = false;
        if restated != 0 {
            if record.seq > restated {
                restated = 0;
                settle(&mut tail.checkpoints, open.take());
            } else {
                let message = decode_message(&record.payload)?;
                if let Some(text) = checkpoint_text(&message) {
                    // A base restates every earlier checkpoint before the one it
                    // adds, so the last one of the base is the row's.
                    match open {
                        Some(index) => tail.checkpoints[index].1.text = text,
                        None => {
                            let row = Checkpoint { text, messages: 0 };
                            tail.checkpoints.push((tail.messages.len(), row));
                            open = Some(tail.checkpoints.len() - 1);
                        }
                    }
                }
                if let Some(index) = open {
                    tail.checkpoints[index].1.messages += 1;
                }
                continue;
            }
        }
        if tail.messages.len() == KEEP {
            tail.messages.pop_front();
            tail.omitted += 1;
            for (index, _) in &mut tail.checkpoints {
                *index = index.saturating_sub(1);
            }
        }
        let message = decode_message(&record.payload)?;
        if let Message::Assistant(reply) = &message {
            tail.calls
                .extend(reply.content.iter().filter_map(|block| match block {
                    Content::ToolCall(call) => Some(call.clone()),
                    _ => None,
                }));
        }
        tail.messages.push_back(message);
    }
    settle(&mut tail.checkpoints, open);
    Ok(tail)
}

/// Drops the row of the base just read when it adds no checkpoint: its last
/// checkpoint is the one the row before it already draws.
fn settle(rows: &mut Vec<(usize, Checkpoint)>, open: Option<usize>) {
    if let Some(index) = open
        && index > 0
        && rows[index - 1].1.text == rows[index].1.text
    {
        rows.remove(index);
    }
}

/// The blocking read behind [`since`].
fn since_blocking(dir: &Path, from: u64, upto: u64) -> io::Result<Vec<(u64, Message)>> {
    if upto <= from {
        return Ok(Vec::new());
    }
    let session = Session::open(dir)?;
    let mut messages = Vec::new();
    let mut ends: Option<HashMap<u64, u64>> = None;
    let mut restated = 0;
    for record in session.records_from(from + 1) {
        let record = record?;
        if record.seq > upto {
            break;
        }
        if record.kind == BASE {
            // A base restates messages the transcript already drew, and the live
            // `compacted` event drew its row: the range carries what follows it.
            let known = match &ends {
                Some(ends) => ends,
                None => ends.insert(session.base_ends().into_iter().collect()),
            };
            restated = known.get(&record.seq).copied().unwrap_or(record.seq);
            continue;
        }
        if record.kind != MESSAGE || record.seq <= restated {
            continue;
        }
        messages.push((record.seq, decode_message(&record.payload)?));
    }
    Ok(messages)
}

/// The blocking read behind [`results`].
fn results_blocking(dir: &Path, from: u64, upto: u64) -> io::Result<Vec<(Text, Text)>> {
    if upto <= from {
        return Ok(Vec::new());
    }
    let session = Session::open(dir)?;
    let mut results = Vec::new();
    for record in session.records_from(from + 1) {
        let record = record?;
        if record.seq > upto {
            break;
        }
        if record.kind != MESSAGE {
            continue;
        }
        if let Message::ToolResult(result) = decode_message(&record.payload)? {
            results.push((result.call_id, result.content));
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use bytes::Bytes;
    use reagent_ai::record::{MESSAGE, encode_message};
    use reagent_ai::{Message, Text};
    use reagent_store::{Header, Session, Writer};

    use super::{BASE, HEADER, KEEP, read, results, since};

    /// A session folder with `count` user messages, and the number of its
    /// newest record.
    fn session(count: usize, root: &Path, name: &str) -> (PathBuf, u64) {
        let dir = root.join(name);
        let header = Header::new(
            name.to_owned(),
            reagent_store::paths::now_ms(),
            None,
            Bytes::new(),
        );
        let mut writer = Writer::create(&dir, header).expect("the writer");
        for index in 0..count {
            let mut payload = Vec::new();
            encode_message(&mut payload, &Message::user(format!("message {index}")));
            writer
                .append(MESSAGE, Bytes::from(payload), false)
                .expect("append a message");
        }
        writer.commit().expect("commit");
        writer.close().expect("close");
        let upto = newest(&dir);
        (dir, upto)
    }

    /// A session folder in `root` whose writer `write` fills with records, and
    /// the number of its newest record.
    fn written(root: &Path, name: &str, write: impl FnOnce(&mut Writer)) -> (PathBuf, u64) {
        let dir = root.join(name);
        let header = Header::new(
            name.to_owned(),
            reagent_store::paths::now_ms(),
            None,
            Bytes::new(),
        );
        let mut writer = Writer::create(&dir, header).expect("the writer");
        write(&mut writer);
        writer.close().expect("close");
        let upto = newest(&dir);
        (dir, upto)
    }

    /// Appends `text` as one user message.
    fn user(writer: &mut Writer, text: String) {
        let mut payload = Vec::new();
        encode_message(&mut payload, &Message::user(text));
        writer
            .append(MESSAGE, Bytes::from(payload), false)
            .expect("append a message");
    }

    /// The text of the user message at `index` of `tail`.
    fn text_at(tail: &super::Tail, index: usize) -> String {
        match &tail.messages[index] {
            Message::User { content } => match &content[0] {
                reagent_ai::Content::Text { text } => text.to_string(),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected a user message, got {other:?}"),
        }
    }

    /// The number of the newest record of the session in `dir`.
    fn newest(dir: &Path) -> u64 {
        Session::open(dir)
            .expect("open the journal")
            .records()
            .filter_map(Result::ok)
            .map(|record| record.seq)
            .max()
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn the_read_keeps_the_newest_messages_and_counts_the_rest() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, upto) = session(250, root.path(), "sess-00000001");
        let tail = read(dir, upto).await.expect("the read");
        assert_eq!(
            tail.messages.len(),
            KEEP,
            "only the newest messages are kept"
        );
        assert_eq!(tail.omitted, 250 - KEEP, "the rest are counted");
        match tail.messages.front() {
            Some(Message::User { content }) => match &content[0] {
                reagent_ai::Content::Text { text } => {
                    assert_eq!(text.as_ref(), "message 50")
                }
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected the oldest kept message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn results_are_read_only_after_the_last_record() {
        let root = tempfile::tempdir().expect("a temp folder");
        let dir = root.path().join("sess-00000002");
        let header = Header::new(
            "sess-00000002".to_owned(),
            reagent_store::paths::now_ms(),
            None,
            Bytes::new(),
        );
        let mut writer = Writer::create(&dir, header).expect("the writer");
        let mut payload = Vec::new();
        encode_message(
            &mut payload,
            &Message::ToolResult(reagent_ai::ToolResult {
                call_id: "call-1".into(),
                content: "the output".into(),
                is_error: false,
                images: Vec::new(),
            }),
        );
        writer
            .append(MESSAGE, Bytes::from(payload), false)
            .expect("append");
        writer.commit().expect("commit");
        writer.close().expect("close");
        let first = newest(&dir);

        let none = results(dir.clone(), first, first).await.expect("the read");
        assert!(none.is_empty(), "nothing after the last record");

        let found = results(dir, first - 1, first).await.expect("the read");
        assert_eq!(
            found,
            vec![(Text::from("call-1"), Text::from("the output"))]
        );
    }

    #[tokio::test]
    async fn only_the_records_after_the_last_one_read_come_back() {
        let root = tempfile::tempdir().expect("a temp folder");
        let dir = root.path().join("sess-00000004");
        let header = Header::new(
            "sess-00000004".to_owned(),
            reagent_store::paths::now_ms(),
            None,
            Bytes::new(),
        );
        let mut writer = Writer::create(&dir, header).expect("the writer");
        for text in ["the first", "the second", "the third"] {
            let mut payload = Vec::new();
            encode_message(&mut payload, &Message::user(text));
            writer
                .append(MESSAGE, Bytes::from(payload), false)
                .expect("append");
        }
        writer.commit().expect("commit");
        writer.close().expect("close");
        let newest = newest(&dir);

        let tail = since(dir.clone(), newest - 2, newest)
            .await
            .expect("the read");
        let texts: Vec<String> = tail
            .iter()
            .map(|(_, message)| match message {
                Message::User { content } => match &content[0] {
                    reagent_ai::Content::Text { text } => text.to_string(),
                    other => panic!("expected text, got {other:?}"),
                },
                other => panic!("expected a user message, got {other:?}"),
            })
            .collect();
        assert_eq!(
            texts,
            vec!["the second".to_owned(), "the third".to_owned()],
            "the records after the last one read, oldest first"
        );

        let numbered = since(dir.clone(), newest - 2, newest)
            .await
            .expect("the read");
        assert_eq!(
            numbered
                .iter()
                .map(|(record, _)| *record)
                .collect::<Vec<u64>>(),
            vec![newest - 1, newest],
            "each message names the record that holds it"
        );

        let none = since(dir, newest, newest).await.expect("the read");
        assert!(none.is_empty(), "nothing after the newest record");
    }

    #[tokio::test]
    async fn a_base_of_a_compaction_hides_the_messages_it_restates() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, upto) = written(root.path(), "sess-00000005", |writer| {
            for index in 0..30 {
                user(writer, format!("message {index}"));
            }
            writer.commit().expect("commit");
            // The base of a compaction, the history it restates, and the turns
            // that follow it.
            writer
                .append(BASE, Bytes::new(), true)
                .expect("append the base");
            user(writer, format!("{HEADER}the summary"));
            user(writer, "message 28".to_owned());
            user(writer, "message 29".to_owned());
            writer.commit().expect("commit");
            for index in 30..34 {
                user(writer, format!("message {index}"));
            }
            writer.commit().expect("commit");
        });

        let tail = read(dir, upto).await.expect("the read");
        assert_eq!(tail.messages.len(), 34, "the restated messages draw once");
        assert_eq!(text_at(&tail, 29), "message 29");
        assert_eq!(text_at(&tail, 30), "message 30");
        assert_eq!(tail.checkpoints.len(), 1, "one compaction row");
        let (at, row) = &tail.checkpoints[0];
        assert_eq!(*at, 30, "the row stands where the base draws");
        assert_eq!(row.messages, 3, "the checkpoint and the two it kept");
        assert!(
            row.text.starts_with("Earlier turns"),
            "the row holds the checkpoint text: {:?}",
            row.text
        );
    }

    #[tokio::test]
    async fn a_base_draws_only_the_checkpoint_it_adds() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, upto) = written(root.path(), "sess-00000007", |writer| {
            for index in 0..10 {
                user(writer, format!("message {index}"));
            }
            writer.commit().expect("commit");
            let base = |writer: &mut Writer, checkpoints: &[&str], kept: &str| {
                writer
                    .append(BASE, Bytes::new(), true)
                    .expect("append the base");
                user(writer, "message 0".to_owned());
                for checkpoint in checkpoints {
                    user(writer, format!("{HEADER}{checkpoint}"));
                }
                user(writer, kept.to_owned());
                writer.commit().expect("commit");
            };
            base(writer, &["first"], "message 9");
            user(writer, "message 10".to_owned());
            writer.commit().expect("commit");
            // A second compaction restates the first checkpoint before its own.
            base(writer, &["first", "second"], "message 10");
            // A base that adds no checkpoint, such as a later restatement.
            base(writer, &["first", "second"], "message 10");
            user(writer, "message 11".to_owned());
            writer.commit().expect("commit");
        });

        let tail = read(dir, upto).await.expect("the read");
        assert_eq!(tail.messages.len(), 12, "each message draws once");
        let texts: Vec<&str> = tail
            .checkpoints
            .iter()
            .map(|(_, row)| row.text.rsplit('\n').next().unwrap_or_default())
            .collect();
        assert_eq!(texts, ["first", "second"], "one row per compaction");
    }

    #[tokio::test]
    async fn a_row_reads_the_checkpoint_of_its_own_base() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, upto) = written(root.path(), "sess-00000008", |writer| {
            for index in 0..4 {
                user(writer, format!("message {index}"));
            }
            writer.commit().expect("commit");
            for checkpoints in [&["first"][..], &["first", "second"]] {
                writer
                    .append(BASE, Bytes::new(), true)
                    .expect("append the base");
                user(writer, "message 0".to_owned());
                for checkpoint in checkpoints {
                    user(writer, format!("{HEADER}{checkpoint}"));
                }
                writer.commit().expect("commit");
            }
        });

        // The row of a live compaction reads from the record before its base.
        let from = Session::open(&dir)
            .expect("open the journal")
            .records()
            .filter_map(Result::ok)
            .find(|record| record.kind == BASE)
            .map(|record| record.seq - 1)
            .expect("a base");
        let text = super::checkpoint(dir, from, upto)
            .await
            .expect("the read")
            .expect("a checkpoint");
        assert!(text.ends_with("first"), "the first compaction's text: {text:?}");
    }

    #[tokio::test]
    async fn a_base_at_the_start_of_a_session_shows_its_messages_once() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, upto) = written(root.path(), "sess-00000006", |writer| {
            writer
                .append(BASE, Bytes::new(), true)
                .expect("append the base");
            for index in 0..3 {
                user(writer, format!("message {index}"));
            }
            writer.commit().expect("commit");
            user(writer, "the turn after it".to_owned());
            writer.commit().expect("commit");
        });

        let tail = read(dir, upto).await.expect("the read");
        assert_eq!(
            tail.messages.len(),
            4,
            "the base at the start hides nothing"
        );
        assert_eq!(text_at(&tail, 0), "message 0");
        assert!(tail.checkpoints.is_empty(), "no compaction row");
    }

    #[tokio::test]
    async fn the_read_of_five_thousand_messages_stays_quick() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, upto) = session(5_000, root.path(), "sess-00000003");
        let started = Instant::now();
        let tail = read(dir, upto).await.expect("the read");
        eprintln!(
            "history: {:?} for 5000 messages, {} kept",
            started.elapsed(),
            tail.messages.len()
        );
        assert_eq!(tail.messages.len(), KEEP);
    }
}
