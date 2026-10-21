//! Tool call ids for the run and the outputs that masking or compaction took
//! out of the history, so the model can get them back with `expand`.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use futures_util::future::BoxFuture;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use reagent_ai::{Text, ToolSpec};

use crate::tool::{Tool, ToolOutput, parse_args};

/// The ids of a run's tool calls and the outputs removed from its history.
///
/// Ids count from 1 in call order and stay the same when calls leave the
/// history. A kept output waits in memory until [`Archive::flush`] moves it
/// to a temp file, so masking still frees the heap. Shared between the loop
/// and the [`Expand`] tool. The temp file is removed on drop.
#[derive(Debug, Default)]
pub struct Archive {
    inner: Mutex<Inner>,
    /// The temp file, opened once and kept open for appends. Holding it also
    /// keeps flushes in order.
    writer: tokio::sync::Mutex<Option<tokio::fs::File>>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The id of each provider call id seen so far.
    ids: HashMap<Text, u32>,
    /// The outputs removed from the history, by id.
    kept: HashMap<u32, Stored>,
    /// The ids kept in memory since the last flush, in keep order.
    pending: Vec<u32>,
    /// The temp file, once something was written.
    file: Option<PathBuf>,
    /// The file's length so far.
    end: u64,
}

#[derive(Debug)]
enum Stored {
    Memory(Text),
    File { offset: u64, len: usize },
}

impl Stored {
    fn len(&self) -> usize {
        match self {
            Stored::Memory(text) => text.len(),
            Stored::File { len, .. } => *len,
        }
    }
}

impl Archive {
    /// A new, empty archive to share.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The id of the provider call `call_id`, numbering it next when it is new.
    pub fn assign(&self, call_id: &Text) -> u32 {
        let mut inner = self.lock();
        let next = inner.ids.len() as u32 + 1;
        *inner.ids.entry(call_id.clone()).or_insert(next)
    }

    /// The id given to `call_id`, if any.
    pub fn id_of(&self, call_id: &str) -> Option<u32> {
        self.lock().ids.get(call_id).copied()
    }

    /// How many calls have an id.
    pub fn len(&self) -> u32 {
        self.lock().ids.len() as u32
    }

    /// Whether no call has an id yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Keeps the output of call `id` that is leaving the history. The first
    /// kept output of an id wins, so a later stub never replaces it.
    pub fn keep(&self, id: u32, output: Text) {
        let mut inner = self.lock();
        if let std::collections::hash_map::Entry::Vacant(entry) = inner.kept.entry(id) {
            entry.insert(Stored::Memory(output));
            inner.pending.push(id);
        }
    }

    /// The length of the kept output of call `id`.
    pub fn kept_len(&self, id: u32) -> Option<usize> {
        self.lock().kept.get(&id).map(Stored::len)
    }

    /// Moves the outputs kept since the last flush to the temp file. An
    /// output that cannot be written stays in memory.
    pub async fn flush(&self) {
        let mut writer = self.writer.lock().await;
        let (pending, path, mut end) = {
            let mut inner = self.lock();
            if inner.pending.is_empty() {
                return;
            }
            let ids = std::mem::take(&mut inner.pending);
            let pending: Vec<(u32, Text)> = ids
                .into_iter()
                .filter_map(|id| match inner.kept.get(&id) {
                    Some(Stored::Memory(text)) => Some((id, Text::clone(text))),
                    _ => None,
                })
                .collect();
            let path = inner.file.get_or_insert_with(temp_path).clone();
            (pending, path, inner.end)
        };
        if writer.is_none() {
            *writer = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .ok();
        }
        let Some(file) = writer.as_mut() else {
            self.lock()
                .pending
                .extend(pending.iter().map(|(id, _)| *id));
            return;
        };
        // The file's real length, not the count of good writes: a failed
        // write in append mode can leave part of an output behind.
        if let Ok(meta) = file.metadata().await {
            end = meta.len();
        }
        let mut written = Vec::with_capacity(pending.len());
        let mut failed = Vec::new();
        for (id, text) in pending {
            if !failed.is_empty() || file.write_all(text.as_bytes()).await.is_err() {
                failed.push(id);
                continue;
            }
            written.push((id, end, text.len()));
            end += text.len() as u64;
        }
        let flushed = file.flush().await.is_ok();
        let mut inner = self.lock();
        inner.pending.extend(failed);
        if !flushed {
            inner.pending.extend(written.iter().map(|(id, _, _)| *id));
            return;
        }
        inner.end = end;
        for (id, offset, len) in written {
            inner.kept.insert(id, Stored::File { offset, len });
        }
    }

    /// The kept output of call `id`.
    pub async fn get(&self, id: u32) -> Option<Text> {
        let (path, offset, len) = {
            let inner = self.lock();
            match inner.kept.get(&id)? {
                Stored::Memory(text) => return Some(Text::clone(text)),
                Stored::File { offset, len } => (inner.file.clone()?, *offset, *len),
            }
        };
        let mut file = tokio::fs::File::open(&path).await.ok()?;
        file.seek(SeekFrom::Start(offset)).await.ok()?;
        let mut bytes = vec![0; len];
        file.read_exact(&mut bytes).await.ok()?;
        String::from_utf8(bytes).ok().map(Text::from)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // The data stays valid if a holder panicked: every update is one insert.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Archive {
    #[expect(clippy::disallowed_methods, reason = "drop cannot await")]
    fn drop(&mut self) {
        if let Some(path) = &self.lock().file {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn temp_path() -> PathBuf {
    use std::hash::{BuildHasher, Hasher};
    let unique = std::hash::RandomState::new().build_hasher().finish();
    std::env::temp_dir().join(format!(
        "reagent-archive-{}-{unique:016x}",
        std::process::id()
    ))
}

/// The `expand` tool name.
pub const NAME: &str = "expand";

#[derive(Debug, Deserialize)]
struct Args {
    id: u32,
}

/// Returns a tool output that masking or compaction removed, by its id.
#[derive(Debug)]
pub struct Expand {
    spec: ToolSpec,
    archive: Arc<Archive>,
}

impl Expand {
    /// Builds the tool over the run's archive.
    pub fn new(archive: Arc<Archive>) -> Self {
        Self {
            spec: spec(),
            archive,
        }
    }
}

impl Tool for Expand {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn read_only(&self) -> bool {
        true
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let args: Args = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            if let Some(output) = self.archive.get(args.id).await {
                return ToolOutput::ok(capped(&output));
            }
            let calls = self.archive.len();
            if args.id == 0 || args.id > calls {
                ToolOutput::error(format!(
                    "no tool call #{}; the ids run from 1 to {calls}",
                    args.id
                ))
            } else {
                ToolOutput::error(format!(
                    "the output of #{} is still in the context; it was not removed",
                    args.id
                ))
            }
        })
    }
}

/// Bytes kept from each end of a long expanded output, as the bash tool does.
const CAP: usize = 8 * 1024;

/// The output whole when short, else its first and last [`CAP`] bytes.
fn capped(output: &Text) -> Text {
    if output.len() <= 2 * CAP {
        return Text::clone(output);
    }
    let mut head = CAP;
    while !output.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = output.len() - CAP;
    while !output.is_char_boundary(tail) {
        tail += 1;
    }
    let cut = tail - head;
    Text::from(format!(
        "{}\n[{cut} bytes cut]\n{}",
        &output[..head],
        &output[tail..]
    ))
}

/// The `expand` tool spec.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: NAME.into(),
        description: "Show a tool output that was removed from the context, by the id given in a checkpoint or a removed-output note.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "minimum": 1, "description": "The tool call id, the number after #." }
            },
            "required": ["id"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn expand(archive: &Arc<Archive>, args: &str) -> ToolOutput {
        Expand::new(Arc::clone(archive))
            .run(args, &CancellationToken::new())
            .await
    }

    #[test]
    fn ids_count_from_one_in_call_order_and_repeat_for_the_same_call() {
        let archive = Archive::new();
        assert_eq!(archive.assign(&Text::from("call_a")), 1);
        assert_eq!(archive.assign(&Text::from("call_b")), 2);
        assert_eq!(archive.assign(&Text::from("call_a")), 1);
        assert_eq!(archive.id_of("call_b"), Some(2));
        assert_eq!(archive.id_of("call_c"), None);
        assert_eq!(archive.len(), 2);
    }

    #[tokio::test]
    async fn expand_returns_a_kept_output() {
        let archive = Archive::new();
        for call in ["a", "b", "c"] {
            archive.assign(&Text::from(call));
        }
        archive.keep(3, Text::from("third output"));
        let output = expand(&archive, r#"{"id": 3}"#).await;
        assert!(!output.is_error);
        assert_eq!(output.content.as_ref(), "third output");
    }

    #[tokio::test]
    async fn the_first_kept_output_of_an_id_wins() {
        let archive = Archive::new();
        archive.assign(&Text::from("a"));
        archive.keep(1, Text::from("original"));
        archive.keep(1, Text::from("stub"));
        assert_eq!(archive.get(1).await.as_deref(), Some("original"));
    }

    #[tokio::test]
    async fn a_flushed_output_comes_back_from_the_temp_file() {
        let archive = Archive::new();
        for call in ["a", "b"] {
            archive.assign(&Text::from(call));
        }
        archive.keep(1, Text::from("first \u{e9}"));
        archive.keep(2, Text::from("second"));
        archive.flush().await;
        let path = archive.lock().file.clone().unwrap();
        assert!(tokio::fs::try_exists(&path).await.unwrap());
        assert!(matches!(archive.lock().kept[&1], Stored::File { .. }));
        assert_eq!(archive.get(2).await.as_deref(), Some("second"));
        assert_eq!(archive.get(1).await.as_deref(), Some("first \u{e9}"));
        assert_eq!(archive.kept_len(2), Some(6));
        drop(archive);
        assert!(!tokio::fs::try_exists(&path).await.unwrap());
    }

    #[tokio::test]
    async fn offsets_follow_the_real_file_length() {
        let archive = Archive::new();
        for call in ["a", "b"] {
            archive.assign(&Text::from(call));
        }
        archive.keep(1, Text::from("one"));
        archive.flush().await;
        // A failed write left bytes behind: append junk the archive did not count.
        let path = archive.lock().file.clone().unwrap();
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        file.write_all(b"junk").await.unwrap();
        file.flush().await.unwrap();
        archive.keep(2, Text::from("two"));
        archive.flush().await;
        assert_eq!(archive.get(1).await.as_deref(), Some("one"));
        assert_eq!(archive.get(2).await.as_deref(), Some("two"));
    }

    #[tokio::test]
    async fn a_long_output_is_cut_to_its_ends() {
        let archive = Archive::new();
        archive.assign(&Text::from("a"));
        let long = format!("{}{}", "h".repeat(10_000), "t".repeat(10_000));
        archive.keep(1, Text::from(long.as_str()));
        let output = expand(&archive, r#"{"id": 1}"#).await;
        assert!(output.content.starts_with(&"h".repeat(CAP)));
        assert!(output.content.ends_with(&"t".repeat(CAP)));
        assert!(output.content.contains("[3616 bytes cut]"));
    }

    #[tokio::test]
    async fn an_unknown_id_names_the_range() {
        let archive = Archive::new();
        for call in ["a", "b", "c", "d", "e"] {
            archive.assign(&Text::from(call));
        }
        let output = expand(&archive, r#"{"id": 9}"#).await;
        assert!(output.is_error);
        assert_eq!(
            output.content.as_ref(),
            "no tool call #9; the ids run from 1 to 5"
        );
    }

    #[tokio::test]
    async fn an_output_still_in_context_says_so() {
        let archive = Archive::new();
        archive.assign(&Text::from("a"));
        let output = expand(&archive, r#"{"id": 1}"#).await;
        assert!(output.is_error);
        assert!(output.content.contains("still in the context"));
    }
}
