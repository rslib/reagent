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
use std::sync::Arc;

use reagent_ai::record::{MESSAGE, decode_message};
use reagent_ai::{Content, Message, Text, ToolCall};
use reagent_store::{Session, kind};

use crate::picture::{self, Picture};

/// How many of the newest messages the app renders when it continues a
/// session.
pub const KEEP: usize = 200;

/// How many of the newest messages a preview keeps.
pub const PREVIEW_MESSAGES: usize = 6;

/// The most bytes of one message a preview keeps: more than the rows of a
/// terminal show.
const EXCHANGE_BYTES: usize = 4096;

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
    /// The pictures of each message of `messages`, in the same order: empty for
    /// a message that holds no image.
    pub pictures: Vec<Vec<Picture>>,
}

/// The messages of the session in `dir`, up to and including record `upto`:
/// the newest [`KEEP`] of them and the count of the ones left out.
///
/// Every other kind of record (call start, turn end, base, approval) is
/// skipped: those carry agent types and draw nothing.
///
/// The images the messages hold are decoded and fitted into `room` columns
/// inside the same blocking job, one list per message.
pub async fn read(dir: PathBuf, upto: u64, room: u32) -> io::Result<Tail> {
    tokio::task::spawn_blocking(move || read_blocking(&dir, upto, room))
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
}

/// The messages recorded after `from` and up to `upto`, oldest first, each with
/// the record that holds it.
///
/// A client the worker cut reads this range to see what it missed while it was
/// away: a reply that finished, and a tool result that landed, are records by
/// then, so the catch-up frame the worker replays carries only the rest. The
/// record number says whether the client drew that message live already, and
/// the pictures are the images it holds, decoded inside the blocking job.
pub async fn since(
    dir: PathBuf,
    from: u64,
    upto: u64,
    room: u32,
) -> io::Result<Vec<(u64, Message, Vec<Picture>)>> {
    tokio::task::spawn_blocking(move || since_blocking(&dir, from, upto, room))
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

/// One message of a session's tail: what the sessions sheet's preview draws.
#[derive(Debug, Clone)]
pub enum Recent {
    /// What the user asked.
    User(Text),
    /// What the agent replied.
    Reply(Text),
}

impl Recent {
    /// The text of the message.
    pub fn text(&self) -> &Text {
        match self {
            Recent::User(text) | Recent::Reply(text) => text,
        }
    }
}

/// The newest messages of a session that carry text, oldest first.
#[derive(Debug)]
pub struct RecentMessages {
    /// At most [`PREVIEW_MESSAGES`] messages, oldest first.
    pub messages: Vec<Recent>,
}

impl Default for RecentMessages {
    /// A preview that reserves its messages, so a read fills it without
    /// growing it.
    fn default() -> RecentMessages {
        RecentMessages {
            messages: Vec::with_capacity(PREVIEW_MESSAGES),
        }
    }
}

impl RecentMessages {
    /// Adds a message at the end, dropping the oldest once the preview holds
    /// enough of them.
    fn push(&mut self, message: Recent) {
        if self.messages.len() == PREVIEW_MESSAGES {
            self.messages.remove(0);
        }
        self.messages.push(message);
    }

    /// Keeps the end of every message a preview can draw, once the scan has
    /// ended, so a message that is kept pays for its cut and a message the scan
    /// drops never does.
    fn cut(&mut self) {
        for message in &mut self.messages {
            let text = match message {
                Recent::User(text) | Recent::Reply(text) => text,
            };
            let kept = preview_tail(Text::clone(text));
            if !Arc::ptr_eq(&kept, text) {
                *text = kept;
            }
        }
    }
}

/// The newest messages of the session in `dir`, read in one blocking job.
///
/// Only the texts are kept, never the session: the preview draws the end of
/// each and the read of a long journal costs its records, not its words.
pub async fn recent_messages(dir: PathBuf) -> io::Result<RecentMessages> {
    tokio::task::spawn_blocking(move || recent_messages_blocking(&dir))
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
}

/// The blocking read behind [`recent_messages`].
fn recent_messages_blocking(dir: &Path) -> io::Result<RecentMessages> {
    let session = Session::open(dir)?;
    let mut recent = RecentMessages::default();
    let mut ends: Option<HashMap<u64, u64>> = None;
    // The last record of the restatement being read, 0 outside one.
    let mut restated = 0;
    // Whether no message came before this record: a base there writes the
    // history from nothing, so its messages are the session's own.
    let mut empty = true;
    for record in session.records() {
        let record = record?;
        if record.kind == BASE {
            // A compaction restates the first prompt and a message of file
            // texts after the real last question, so they must not replace it.
            restated = if empty {
                0
            } else {
                let known = match &ends {
                    Some(ends) => ends,
                    None => ends.insert(session.base_ends().into_iter().collect()),
                };
                known.get(&record.seq).copied().unwrap_or(record.seq)
            };
            continue;
        }
        if record.kind != MESSAGE {
            continue;
        }
        empty = false;
        if record.seq <= restated {
            continue;
        }
        let message = decode_message(&record.payload)?;
        match &message {
            // A checkpoint is the agent's own summary in a user message, and a
            // user message of images alone has no words: neither is the ask.
            Message::User { content } => {
                if checkpoint_text(&message).is_none()
                    && let Some(text) = text_of(content)
                {
                    recent.push(Recent::User(text));
                }
            }
            // The calls and the reasoning of a reply are not its words.
            Message::Assistant(reply) => {
                if let Some(text) = text_of(&reply.content) {
                    recent.push(Recent::Reply(text));
                }
            }
            _ => {}
        }
    }
    recent.cut();
    Ok(recent)
}

/// The end of `text` a preview can draw, so a sheet that stays open holds no
/// long reply or paste: the same text when all of it fits.
///
/// A text over [`EXCHANGE_BYTES`] keeps what follows the first newline of its
/// last [`EXCHANGE_BYTES`] bytes, so no row starts mid-line, and those bytes
/// alone on a char boundary when it holds no newline.
fn preview_tail(text: Text) -> Text {
    if text.len() <= EXCHANGE_BYTES {
        return text;
    }
    let mut start = text.len() - EXCHANGE_BYTES;
    let from = match text.as_bytes()[start..]
        .iter()
        .position(|&byte| byte == b'\n')
    {
        Some(at) if start + at + 1 < text.len() => start + at + 1,
        _ => {
            while !text.is_char_boundary(start) {
                start += 1;
            }
            start
        }
    };
    let tail = &text[from..];
    // A cut inside a code block leaves its closing fence first, which would
    // open a block over the words after it.
    let fences = tail
        .lines()
        .filter(|line| line.trim_start().starts_with("```"))
        .count();
    if fences % 2 == 1 {
        Text::from(format!("```\n{tail}"))
    } else {
        Text::from(tail)
    }
}

/// The visible text of `blocks`, shared with them when they carry one block
/// and joined with a newline when they carry several, and `None` when they
/// carry no text at all.
fn text_of(blocks: &[Content]) -> Option<Text> {
    let mut texts = blocks.iter().filter_map(|block| match block {
        Content::Text { text } if !text.is_empty() => Some(text),
        _ => None,
    });
    let first = texts.next()?;
    let Some(second) = texts.next() else {
        return Some(Text::clone(first));
    };
    let mut text = String::from(&**first);
    for block in std::iter::once(second).chain(texts) {
        text.push('\n');
        text.push_str(block);
    }
    Some(Arc::from(text))
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
fn read_blocking(dir: &Path, upto: u64, room: u32) -> io::Result<Tail> {
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
    tail.pictures = tail
        .messages
        .iter()
        .map(|message| picture::of_message(message, room))
        .collect();
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
fn since_blocking(
    dir: &Path,
    from: u64,
    upto: u64,
    room: u32,
) -> io::Result<Vec<(u64, Message, Vec<Picture>)>> {
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
        let message = decode_message(&record.payload)?;
        let pictures = picture::of_message(&message, room);
        messages.push((record.seq, message, pictures));
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
    use std::sync::Arc;
    use std::time::Instant;

    use bytes::Bytes;
    use reagent_ai::record::{MESSAGE, encode_message};
    use reagent_ai::{AssistantMessage, Content, Message, Text, ToolCall, ToolResult};
    use reagent_store::{Header, Session, Writer};

    use super::{
        BASE, HEADER, KEEP, PREVIEW_MESSAGES, Recent, RecentMessages, preview_tail, read,
        recent_messages, results, since,
    };

    /// The columns the pictures of a read fit into in these tests.
    const ROOM: u32 = 100;

    /// The messages of a preview as the side and the text of each.
    fn shown(recent: &RecentMessages) -> Vec<(&'static str, &str)> {
        recent
            .messages
            .iter()
            .map(|message| match message {
                Recent::User(text) => ("user", &**text),
                Recent::Reply(text) => ("reply", &**text),
            })
            .collect()
    }

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

    /// Appends `message` as one record of the journal.
    fn record(writer: &mut Writer, message: &Message) {
        let mut payload = Vec::new();
        encode_message(&mut payload, message);
        writer
            .append(MESSAGE, Bytes::from(payload), false)
            .expect("append a message");
    }

    /// Appends `text` as one user message.
    fn user(writer: &mut Writer, text: String) {
        record(writer, &Message::user(text));
    }

    /// The reply of `text` with no call in it.
    fn reply(text: &str) -> Message {
        Message::assistant(AssistantMessage::new(
            "fake",
            "fake-model",
            vec![Content::Text { text: text.into() }],
        ))
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
        let tail = read(dir, upto, ROOM).await.expect("the read");
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

        let tail = since(dir.clone(), newest - 2, newest, ROOM)
            .await
            .expect("the read");
        let texts: Vec<String> = tail
            .iter()
            .map(|(_, message, _)| match message {
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

        let numbered = since(dir.clone(), newest - 2, newest, ROOM)
            .await
            .expect("the read");
        assert_eq!(
            numbered
                .iter()
                .map(|(record, _, _)| *record)
                .collect::<Vec<u64>>(),
            vec![newest - 1, newest],
            "each message names the record that holds it"
        );

        let none = since(dir, newest, newest, ROOM).await.expect("the read");
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

        let tail = read(dir, upto, ROOM).await.expect("the read");
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

        let tail = read(dir, upto, ROOM).await.expect("the read");
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
        assert!(
            text.ends_with("first"),
            "the first compaction's text: {text:?}"
        );
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

        let tail = read(dir, upto, ROOM).await.expect("the read");
        assert_eq!(
            tail.messages.len(),
            4,
            "the base at the start hides nothing"
        );
        assert_eq!(text_at(&tail, 0), "message 0");
        assert!(tail.checkpoints.is_empty(), "no compaction row");
    }

    /// The preview of a session reads the user texts and the replies, oldest
    /// first, and skips the call, the result and the checkpoint between them.
    #[tokio::test]
    async fn session_preview_recent_reads_the_messages_in_order() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, _) = written(root.path(), "sess-00000009", |writer| {
            record(writer, &Message::user("the first question".to_owned()));
            record(writer, &reply("the first reply"));
            // A call with no text of its own, and the output it ran for, are
            // neither side's words.
            record(
                writer,
                &Message::assistant(AssistantMessage::new(
                    "fake",
                    "fake-model",
                    vec![Content::ToolCall(ToolCall {
                        id: "call-1".into(),
                        name: "read".into(),
                        arguments: "{}".into(),
                    })],
                )),
            );
            record(
                writer,
                &Message::ToolResult(ToolResult {
                    call_id: "call-1".into(),
                    content: "the output".into(),
                    is_error: false,
                    images: Vec::new(),
                }),
            );
            record(writer, &Message::user("the last question".to_owned()));
            record(writer, &reply("the last reply"));
            // The checkpoint the agent writes as a user message is not the
            // user's own words.
            record(writer, &Message::user(format!("{HEADER}the summary")));
            writer.commit().expect("commit");
        });

        let recent = recent_messages(dir).await.expect("the read");
        assert_eq!(
            shown(&recent),
            [
                ("user", "the first question"),
                ("reply", "the first reply"),
                ("user", "the last question"),
                ("reply", "the last reply"),
            ]
        );
    }

    /// A compaction restates the first prompt and the file texts after the real
    /// last question, and neither of them replaces it.
    #[tokio::test]
    async fn session_preview_recent_skips_what_a_base_restates() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, _) = written(root.path(), "sess-0000000a", |writer| {
            record(writer, &Message::user("the first question".to_owned()));
            record(writer, &reply("the first reply"));
            record(writer, &Message::user("the last question".to_owned()));
            record(writer, &reply("the last reply"));
            writer.commit().expect("commit");
            writer
                .append(BASE, Bytes::new(), true)
                .expect("append the base");
            record(writer, &Message::user("the first question".to_owned()));
            record(writer, &Message::user(format!("{HEADER}the summary")));
            record(writer, &Message::user("the current file texts".to_owned()));
            record(writer, &reply("a restated reply"));
            writer.commit().expect("commit");
        });

        let recent = recent_messages(dir).await.expect("the read");
        assert_eq!(
            shown(&recent),
            [
                ("user", "the first question"),
                ("reply", "the first reply"),
                ("user", "the last question"),
                ("reply", "the last reply"),
            ]
        );
    }

    /// A message written after the restatement is the new last one.
    #[tokio::test]
    async fn session_preview_recent_reads_past_a_base() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, _) = written(root.path(), "sess-0000000b", |writer| {
            record(writer, &Message::user("the old question".to_owned()));
            writer.commit().expect("commit");
            writer
                .append(BASE, Bytes::new(), true)
                .expect("append the base");
            record(writer, &Message::user("the old question".to_owned()));
            writer.commit().expect("commit");
            record(writer, &Message::user("the new question".to_owned()));
            record(writer, &reply("the new reply"));
            writer.commit().expect("commit");
        });

        let recent = recent_messages(dir).await.expect("the read");
        let shown = shown(&recent);
        assert_eq!(
            &shown[shown.len() - 2..],
            [("user", "the new question"), ("reply", "the new reply")],
            "what the session wrote after the base is the newest"
        );
    }

    /// A base at the very start writes the history, so its messages count.
    #[tokio::test]
    async fn session_preview_recent_keeps_a_first_base() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, _) = written(root.path(), "sess-0000000c", |writer| {
            writer
                .append(BASE, Bytes::new(), true)
                .expect("append the base");
            record(writer, &Message::user("the only question".to_owned()));
            writer.commit().expect("commit");
        });

        let recent = recent_messages(dir).await.expect("the read");
        assert_eq!(shown(&recent), [("user", "the only question")]);
    }

    /// Ten questions and ten replies keep the newest six messages, oldest
    /// first.
    #[tokio::test]
    async fn session_preview_recent_keeps_the_newest_six_oldest_first() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, _) = written(root.path(), "sess-0000000e", |writer| {
            for index in 0..10 {
                record(writer, &Message::user(format!("question {index}")));
                record(writer, &reply(&format!("reply {index}")));
            }
            writer.commit().expect("commit");
        });

        let recent = recent_messages(dir).await.expect("the read");
        assert_eq!(recent.messages.len(), PREVIEW_MESSAGES);
        assert_eq!(
            shown(&recent),
            [
                ("user", "question 7"),
                ("reply", "reply 7"),
                ("user", "question 8"),
                ("reply", "reply 8"),
                ("user", "question 9"),
                ("reply", "reply 9"),
            ]
        );
    }

    /// A long text keeps only the end a preview can draw, on a char boundary,
    /// and the whole of a text that fits.
    #[tokio::test]
    async fn session_preview_recent_keeps_the_tail_of_a_long_text() {
        let root = tempfile::tempdir().expect("a temp folder");
        // 60 KB of three-byte chars, so a 4096-byte window starts inside one and
        // no newline follows it.
        let wide = "\u{20ac}".repeat(20_000);
        let lined = format!("{}\nthe last line", "x".repeat(30_000));
        let short = "one\n\ntwo\nthree\nfour\nfive";
        let (dir, _) = written(root.path(), "sess-0000000d", |writer| {
            record(writer, &Message::user(wide.clone()));
            record(writer, &reply(&lined));
            record(writer, &reply(short));
            writer.commit().expect("commit");
        });

        let recent = recent_messages(dir).await.expect("the read");
        let text = |index: usize| &**recent.messages[index].text();
        let kept = text(0);
        assert!(wide.len() > 50 * 1024, "{} bytes", wide.len());
        assert_eq!(kept.len(), 4095, "the last bytes of the text, no char cut");
        assert!(wide.ends_with(kept), "the end of the text is kept");
        assert_eq!(text(1), "the last line", "from the first newline in it");
        assert_eq!(text(2), short, "a text that fits is kept whole");
    }

    /// A text of exactly the limit stays whole, in the same `Arc`.
    #[test]
    fn session_preview_recent_keeps_a_text_at_the_limit_whole() {
        let text = Text::from("x".repeat(super::EXCHANGE_BYTES));
        assert!(Arc::ptr_eq(&preview_tail(Text::clone(&text)), &text));
    }

    /// A newline as the very last byte is no place to cut: nothing follows it.
    #[test]
    fn session_preview_recent_ignores_a_final_newline_when_it_cuts() {
        let text = format!("{}\n", "x".repeat(super::EXCHANGE_BYTES * 2));
        let kept = preview_tail(Text::from(text));
        assert_eq!(kept.len(), super::EXCHANGE_BYTES);
        assert!(kept.ends_with("x\n"), "the end stays");
    }

    /// A cut inside a code block opens the block again, so the words after it
    /// are not drawn as code.
    #[test]
    fn session_preview_recent_opens_a_code_block_it_cut() {
        let code = "let a = 1;\n".repeat(1000);
        let text = format!("```rust\n{code}```\nthe words after\n");
        let kept = preview_tail(Text::from(text));
        assert!(kept.starts_with("```\n"), "{:?}", &kept[..20]);
        assert!(kept.ends_with("the words after\n"));
        // A cut outside a block adds nothing.
        let plain = format!("{}\nthe end\n", "word\n".repeat(2000));
        assert!(!preview_tail(Text::from(plain)).starts_with("```"));
    }

    /// A text that fits stays the `Arc` it was, with no copy.
    #[test]
    fn session_preview_recent_keeps_a_short_text_as_the_same_arc() {
        let text = Text::from("the answer");
        let kept = preview_tail(Text::clone(&text));
        assert!(Arc::ptr_eq(&text, &kept));
    }

    /// A session whose newest records carry no visible text has no messages:
    /// reasoning and a checkpoint are not the words of a side.
    #[tokio::test]
    async fn session_preview_recent_of_a_reasoning_only_reply_is_empty() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, _) = written(root.path(), "sess-00000010", |writer| {
            record(writer, &Message::user(format!("{HEADER}the summary")));
            record(
                writer,
                &Message::assistant(AssistantMessage::new(
                    "fake",
                    "fake-model",
                    vec![Content::Thinking {
                        thinking: "hmm".into(),
                        signature: None,
                    }],
                )),
            );
            writer.commit().expect("commit");
        });

        let recent = recent_messages(dir).await.expect("the read");
        assert!(recent.messages.is_empty(), "a checkpoint and reasoning")
    }

    /// A folder that is not a session does not read: the caller draws the row
    /// alone.
    #[tokio::test]
    async fn session_preview_recent_of_a_missing_folder_is_an_error() {
        let root = tempfile::tempdir().expect("a temp folder");
        let gone = root.path().join("not-a-session");
        assert!(
            recent_messages(gone).await.is_err(),
            "a folder that is not there does not read"
        );
    }

    #[tokio::test]
    async fn the_read_of_five_thousand_messages_stays_quick() {
        let root = tempfile::tempdir().expect("a temp folder");
        let (dir, upto) = session(5_000, root.path(), "sess-00000003");
        let started = Instant::now();
        let tail = read(dir, upto, ROOM).await.expect("the read");
        eprintln!(
            "history: {:?} for 5000 messages, {} kept",
            started.elapsed(),
            tail.messages.len()
        );
        assert_eq!(tail.messages.len(), KEEP);
    }
}
