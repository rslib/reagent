//! The writer: one thread owns the part file and its zstd stream.
//!
//! Records arrive over a channel. A commit that has to be durable carries a
//! reply channel and is answered after `sync_data`.

use std::io;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SendError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use bytes::Bytes;
use tokio::sync::oneshot;

use crate::part::{Header, PartWriter};
use crate::paths;

/// Name of the thread that writes a part.
pub const THREAD_NAME: &str = "reagent-session";

/// One message for the writer thread.
#[derive(Debug)]
enum Command {
    /// Write one record.
    Append {
        kind: u8,
        payload: Bytes,
        base: bool,
    },
    /// Flush the stream and write the commit.
    Commit {
        reply: Option<oneshot::Sender<io::Result<()>>>,
    },
    /// Answer once every command before this one is done.
    Written {
        reply: oneshot::Sender<io::Result<()>>,
    },
    /// Commit, sync, and stop.
    Close,
}

/// Writes one part file on its own thread.
///
/// The file is locked while the writer lives, so a second writer on the same
/// part fails. An I/O error latches: every later call returns it.
#[derive(Debug)]
pub struct Writer {
    tx: Sender<Command>,
    latch: Arc<Latch>,
    part_id: u64,
    thread: Option<JoinHandle<()>>,
}

impl Writer {
    /// Creates the part file in `dir` and starts the thread that writes it.
    ///
    /// Fails with [`io::ErrorKind::WouldBlock`] when another writer holds the
    /// part file.
    pub fn create(dir: &Path, header: Header) -> io::Result<Self> {
        create_dir(dir)?;
        let path = paths::part_path(dir, header.part_id);
        let part_id = header.part_id;
        let file = open_part(&path)?;
        let mut writer = PartWriter::new(file, header)?;
        // The header and the folder entry are on disk before the first record,
        // so a crash never leaves a part that the folder does not name.
        sync(&mut writer)?;
        sync_dir(dir)?;
        let (tx, rx) = mpsc::channel();
        let latch = Arc::new(Latch::new());
        let thread_latch = Arc::clone(&latch);
        let thread = thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || run(writer, rx, thread_latch))?;
        Ok(Writer {
            tx,
            latch,
            part_id,
            thread: Some(thread),
        })
    }

    /// The part id of the file this writer writes.
    pub fn part_id(&self) -> u64 {
        self.part_id
    }

    /// Hands one record to the writer thread.
    pub fn append(&mut self, kind: u8, payload: Bytes, base: bool) -> io::Result<()> {
        self.latch.check()?;
        crate::frame::fits(payload.len())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        match self.tx.send(Command::Append {
            kind,
            payload,
            base,
        }) {
            Ok(()) => Ok(()),
            Err(_) => Err(self.latch.stopped()),
        }
    }

    /// Asks the writer thread to commit what it has.
    pub fn commit(&mut self) -> io::Result<()> {
        self.latch.check()?;
        match self.tx.send(Command::Commit { reply: None }) {
            Ok(()) => Ok(()),
            Err(_) => Err(self.latch.stopped()),
        }
    }

    /// Answers once the writer thread has done every commit asked for so far, so
    /// a reader of the file finds them and a killed process keeps them. No sync:
    /// a power cut can still lose them. An I/O error answers as the error.
    pub fn written(&self) -> oneshot::Receiver<io::Result<()>> {
        let (reply, rx) = oneshot::channel();
        if let Err(error) = self.latch.check() {
            let _ = reply.send(Err(error));
            return rx;
        }
        if let Err(SendError(Command::Written { reply })) = self.tx.send(Command::Written { reply })
        {
            let _ = reply.send(Err(self.latch.stopped()));
        }
        rx
    }

    /// Commits and syncs; the reply arrives after the data is on the disk.
    pub fn commit_and_sync(&mut self) -> oneshot::Receiver<io::Result<()>> {
        let (reply, rx) = oneshot::channel();
        if let Err(error) = self.latch.check() {
            let _ = reply.send(Err(error));
            return rx;
        }
        match self.tx.send(Command::Commit { reply: Some(reply) }) {
            Ok(()) => rx,
            Err(SendError(Command::Commit { reply: Some(reply) })) => {
                let _ = reply.send(Err(self.latch.stopped()));
                rx
            }
            Err(_) => rx,
        }
    }

    /// Commits, syncs, unlocks and waits for the thread to stop.
    pub fn close(mut self) -> io::Result<()> {
        let _ = self.tx.send(Command::Close);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.latch.check()
    }
}

/// The first I/O error of the writer thread.
#[derive(Debug)]
struct Latch(Mutex<Option<(io::ErrorKind, String)>>);

impl Latch {
    fn new() -> Self {
        Latch(Mutex::new(None))
    }

    /// Keeps `error`, if no error came before it.
    fn set(&self, error: &io::Error) {
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some((error.kind(), error.to_string()));
        }
    }

    /// The latched error, as a fresh one.
    fn error(&self) -> Option<io::Error> {
        let slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        slot.as_ref()
            .map(|(kind, message)| io::Error::new(*kind, message.clone()))
    }

    /// Fails when an error latched before.
    fn check(&self) -> io::Result<()> {
        match self.error() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The error of a writer whose thread has stopped.
    fn stopped(&self) -> io::Error {
        self.error().unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "the writer thread has stopped")
        })
    }
}

/// Runs one writer operation and keeps the first error.
fn step(latch: &Latch, result: io::Result<()>) -> io::Result<()> {
    if let Err(error) = &result {
        latch.set(error);
    }
    match latch.error() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The writer thread: owns the file and the stream until it is told to stop.
#[expect(clippy::disallowed_types, reason = "runs on the session writer thread")]
fn run(mut writer: PartWriter<std::fs::File>, rx: Receiver<Command>, latch: Arc<Latch>) {
    while let Ok(command) = rx.recv() {
        if let Some(error) = latch.error() {
            match command {
                Command::Commit { reply: Some(reply) } | Command::Written { reply } => {
                    let _ = reply.send(Err(error));
                }
                Command::Close => return,
                _ => {}
            }
            continue;
        }
        match command {
            Command::Append {
                kind,
                payload,
                base,
            } => {
                let _ = step(&latch, writer.append(kind, &payload, base));
            }
            Command::Commit { reply: None } => {
                let _ = step(&latch, writer.commit());
            }
            Command::Commit { reply: Some(reply) } => {
                let result = writer.commit();
                let result = match result {
                    Ok(()) => sync(&mut writer),
                    Err(error) => Err(error),
                };
                let _ = reply.send(step(&latch, result));
            }
            Command::Written { reply } => {
                let _ = reply.send(Ok(()));
            }
            Command::Close => {
                let _ = finish(&mut writer, &latch);
                return;
            }
        }
    }
    if latch.error().is_none() {
        let _ = finish(&mut writer, &latch);
    }
}

/// Commits what is pending, writes the footer and syncs it.
#[expect(clippy::disallowed_types, reason = "runs on the session writer thread")]
fn finish(writer: &mut PartWriter<std::fs::File>, latch: &Latch) -> io::Result<()> {
    let result = writer.close();
    let result = match result {
        Ok(()) => sync(writer),
        Err(error) => Err(error),
    };
    step(latch, result)
}

/// Syncs the part file to the disk.
#[expect(clippy::disallowed_types, reason = "runs on the session writer thread")]
fn sync(writer: &mut PartWriter<std::fs::File>) -> io::Result<()> {
    writer.out_mut().sync_data()?;
    #[cfg(test)]
    crate::hook::push(writer.part_id(), crate::hook::Event::Sync);
    Ok(())
}

/// Syncs the folder entry of a new part file.
#[expect(
    clippy::disallowed_types,
    reason = "runs once per part, in Writer::create"
)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Creates the folder a part lives in.
#[expect(
    clippy::disallowed_methods,
    reason = "runs on the session writer thread, once per part"
)]
fn create_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Opens a fresh part file and takes its lock.
#[expect(clippy::disallowed_types, reason = "runs on the session writer thread")]
fn open_part(path: &Path) -> io::Result<std::fs::File> {
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the part file is written by another writer",
            ));
        }
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    if file.metadata()?.len() > 0 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the part file already has a header",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;
    use crate::hook;
    use crate::recover::Record;
    use tempfile::TempDir;

    fn header(part_id: u64) -> Header {
        Header {
            part_id,
            session_id: "01jsession".to_owned(),
            created_ms: crate::paths::now_ms(),
            fork: None,
            meta: Bytes::new(),
        }
    }

    fn records(session: &Session) -> Vec<Record> {
        session
            .records()
            .map(|record| record.expect("record"))
            .collect()
    }

    #[tokio::test]
    async fn a_thousand_records_round_trip() {
        let dir = TempDir::new().expect("temp dir");
        let mut writer = Writer::create(dir.path(), header(77)).expect("writer");
        for index in 0..1000u64 {
            writer
                .append(10, Bytes::from(format!("record {index}")), false)
                .expect("append");
            if index % 10 == 9 {
                writer.commit().expect("commit");
            }
        }
        writer
            .commit_and_sync()
            .await
            .expect("reply")
            .expect("sync");
        writer.close().expect("close");

        let session = Session::open(dir.path()).expect("open");
        let seen = records(&session);
        assert_eq!(seen.len(), 1000);
        for (index, record) in seen.iter().enumerate() {
            assert_eq!(record.seq, index as u64 + 1);
            assert_eq!(record.kind, 10);
            assert_eq!(record.payload, Bytes::from(format!("record {index}")));
        }
    }

    #[tokio::test]
    async fn a_sync_returns_after_the_data_is_synced() {
        let dir = TempDir::new().expect("temp dir");
        let part = 78;
        let mut writer = Writer::create(dir.path(), header(part)).expect("writer");
        let _ = hook::take(part);
        writer
            .append(10, Bytes::from_static(b"a record"), false)
            .expect("append");
        writer
            .commit_and_sync()
            .await
            .expect("reply")
            .expect("sync");
        let events = hook::take(part);
        assert_eq!(
            events,
            vec![hook::Event::Flush, hook::Event::Write, hook::Event::Sync],
            "flush, write the chunk, then sync"
        );
        writer.close().expect("close");
    }

    #[tokio::test]
    async fn written_answers_after_the_commit_is_written_with_no_sync() {
        let dir = TempDir::new().expect("temp dir");
        let part = 81;
        let mut writer = Writer::create(dir.path(), header(part)).expect("writer");
        let _ = hook::take(part);
        writer
            .append(10, Bytes::from_static(b"a record"), false)
            .expect("append");
        writer.commit().expect("commit");
        writer.written().await.expect("reply").expect("written");
        assert_eq!(
            hook::take(part),
            vec![hook::Event::Flush, hook::Event::Write],
            "the commit is written, and nothing synced it"
        );
        writer.close().expect("close");
    }

    #[tokio::test]
    async fn written_answers_the_error_of_a_failed_write() {
        let dir = TempDir::new().expect("temp dir");
        let part = 82;
        let mut writer = Writer::create(dir.path(), header(part)).expect("writer");
        writer
            .append(10, Bytes::from_static(b"a record"), false)
            .expect("append");
        hook::fail_next_write(part);
        writer.commit().expect("the thread takes the commit");
        let error = writer
            .written()
            .await
            .expect("reply")
            .expect_err("the injected write fails");
        assert!(error.to_string().contains("injected"), "{error}");
        assert!(writer.close().is_err(), "closing reports it too");
    }

    #[tokio::test]
    async fn writing_after_an_io_error_fails() {
        let dir = TempDir::new().expect("temp dir");
        let part = 79;
        let mut writer = Writer::create(dir.path(), header(part)).expect("writer");
        writer
            .append(10, Bytes::from_static(b"before"), false)
            .expect("append");

        hook::fail_next_write(part);
        let reply = writer
            .commit_and_sync()
            .await
            .expect("reply")
            .expect_err("the injected write fails");
        assert!(reply.to_string().contains("injected"), "{reply}");

        let error = writer
            .append(10, Bytes::from_static(b"after"), false)
            .expect_err("the error latched");
        assert!(error.to_string().contains("injected"), "{error}");
        let error = writer.commit().expect_err("the error latched");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(writer.close().is_err(), "closing reports it too");
    }

    #[tokio::test]
    async fn a_second_writer_on_the_same_part_fails() {
        let dir = TempDir::new().expect("temp dir");
        let writer = Writer::create(dir.path(), header(80)).expect("writer");
        let error = Writer::create(dir.path(), header(80)).expect_err("locked");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        writer.close().expect("close");

        // After the close the lock is gone and the file is not fresh.
        let error = Writer::create(dir.path(), header(80)).expect_err("a part is written once");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "a test waits for the writer thread it just dropped"
    )]
    fn dropping_the_writer_finishes_the_part() {
        let dir = TempDir::new().expect("temp dir");
        {
            let mut writer = Writer::create(dir.path(), header(81)).expect("writer");
            writer
                .append(10, Bytes::from_static(b"kept"), false)
                .expect("append");
            writer.commit().expect("commit");
        }
        let mut seen = Vec::new();
        for _ in 0..200 {
            let session = Session::open(dir.path()).expect("open");
            seen = records(&session);
            if !seen.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(seen.len(), 1, "the part was finished on drop");
    }

    #[tokio::test]
    async fn a_small_record_costs_a_few_bytes() {
        let dir = TempDir::new().expect("temp dir");
        let part = 82;
        let mut writer = Writer::create(dir.path(), header(part)).expect("writer");
        for index in 0..1000u64 {
            writer
                .append(10, Bytes::from(format!("record {index}")), false)
                .expect("append");
            if index % 10 == 9 {
                writer.commit().expect("commit");
            }
        }
        writer.close().expect("close");
        let path = paths::part_path(dir.path(), part);
        let file = crate::testutil::len(&path);
        let header = Header {
            part_id: part,
            session_id: "01jsession".to_owned(),
            created_ms: 0,
            fork: None,
            meta: Bytes::new(),
        };
        let mut payload = Vec::new();
        header.encode(&mut payload);
        let per_record = (file - payload.len() as u64) / 1000;
        println!("{file} bytes for 1000 records: {per_record} bytes per record");
        assert!(
            per_record < 12,
            "a small record takes {per_record} bytes of the part, plus its frame overhead"
        );
    }

    #[tokio::test]
    async fn a_failed_chunk_write_commits_nothing_after_it() {
        let dir = TempDir::new().expect("temp dir");
        let part = 91;
        let mut writer = Writer::create(dir.path(), header(part)).expect("writer");
        for index in 0..3u8 {
            writer
                .append(10, Bytes::from(vec![index; 8]), false)
                .expect("append");
        }
        writer
            .commit_and_sync()
            .await
            .expect("reply")
            .expect("first commit");
        for index in 3..6u8 {
            writer
                .append(10, Bytes::from(vec![index; 8]), false)
                .expect("append");
        }
        hook::fail_next_write(part);
        assert!(writer.commit_and_sync().await.expect("reply").is_err());
        assert!(writer.close().is_err());

        let session = Session::open(dir.path()).expect("session");
        let seen = records(&session);
        assert_eq!(seen.len(), 3, "only the first commit is visible");
        assert_eq!(seen.last().map(|record| record.seq), Some(3));
        let path = crate::paths::part_path(dir.path(), part);
        let committed = crate::recover::Part::open(&path).expect("part").last_seq();
        assert_eq!(
            committed, 3,
            "no commit claims records that were never written"
        );
    }

    #[tokio::test]
    async fn an_oversized_record_is_refused_and_the_writer_goes_on() {
        let dir = TempDir::new().expect("temp dir");
        let mut writer = Writer::create(dir.path(), header(92)).expect("writer");
        let huge = Bytes::from(vec![0u8; crate::frame::MAX_PAYLOAD + 1]);
        let error = writer.append(10, huge, false).expect_err("too large");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        writer
            .append(10, Bytes::from_static(b"small"), false)
            .expect("append");
        writer.close().expect("close");
        let session = Session::open(dir.path()).expect("session");
        assert_eq!(records(&session).len(), 1);
    }
}
