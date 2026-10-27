//! Reading a part back: the frames up to its last commit, then its records.
//!
//! A torn or corrupt frame ends a part there: nothing after it is read and
//! nothing is rewritten.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;

use crate::frame::{self, NO_SEQ, OVERHEAD};
use crate::kind;
use crate::part::{Commit, FIRST_SEQ, Header, TRAILER_LEN, TRAILER_MAGIC, decode_footer, invalid};
use crate::stream::Decompressor;

/// One record: its sequence number, its kind and its payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Sequence number, continuing across the parts of one session.
    pub seq: u64,
    /// What the payload means.
    pub kind: u8,
    /// The payload, a slice of the decoded chunk.
    pub payload: Bytes,
}

/// A commit and the byte offset just after its frame.
pub use crate::part::CommitAt;

/// A part file as a reader sees it.
#[derive(Debug)]
pub struct Part {
    path: PathBuf,
    header: Header,
    commits: Vec<CommitAt>,
    header_end: u64,
    bytes_read: Arc<AtomicU64>,
    reads: Arc<AtomicU64>,
    /// Chunks checked at open, by file offset: a read takes them from here
    /// instead of reading them again.
    checked: Arc<[(u64, Bytes)]>,
    /// The bytes a footer open read in one go, from their file offset on: a
    /// read of the records takes its frames from here instead of the file.
    tail: Option<(u64, Bytes)>,
}

impl Part {
    /// Opens `path` and keeps every frame up to its last valid commit.
    ///
    /// A closed part is opened from the footer at its end: the commits it
    /// lists and one read of the bytes from the last base on. A part without
    /// a footer is scanned. The chunks from the last base on are checked here,
    /// the bytes a resume reads anyway; a commit after a bad chunk is not
    /// kept. Older chunks are checked when a fork reads them.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_counted(path, Arc::new(AtomicU64::new(0)))
    }

    /// Opens `path`, counting the bytes read into `bytes_read`.
    pub(crate) fn open_counted(path: &Path, bytes_read: Arc<AtomicU64>) -> io::Result<Self> {
        let reads = Arc::new(AtomicU64::new(0));
        let mut source = Source::open(path, Arc::clone(&bytes_read), Arc::clone(&reads))?;
        let file_len = source.len();
        let (header, header_end) = read_header(&mut source)?;
        let found = match open_from_footer(&mut source, &header, header_end, file_len)? {
            Some(found) => found,
            None => {
                source.use_window();
                scan(&mut source, &header, header_end, file_len)?
            }
        };
        let (commits, checked, tail) = found;
        Ok(Part {
            path: path.to_path_buf(),
            header,
            commits,
            header_end,
            bytes_read,
            reads,
            checked: checked.into(),
            tail,
        })
    }

    /// The header the part starts with.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The part id, as its file name and header say.
    pub fn part_id(&self) -> u64 {
        self.header.part_id
    }

    /// The file this part was read from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The valid commits, oldest first.
    pub fn commits(&self) -> &[CommitAt] {
        &self.commits
    }

    /// The commit a reader trusts, if the part has one.
    pub fn last_commit(&self) -> Option<&CommitAt> {
        self.commits.last()
    }

    /// Byte offset just after the header frame.
    pub fn header_end(&self) -> u64 {
        self.header_end
    }

    /// Byte offset where the visible log of the part ends.
    pub fn end(&self) -> u64 {
        self.last_commit().map_or(self.header_end, |at| at.end)
    }

    /// Sequence number of the part's first record.
    pub fn first_seq(&self) -> u64 {
        self.header.fork.map_or(FIRST_SEQ, |fork| fork.seq + 1)
    }

    /// Sequence number of the last visible record.
    pub fn last_seq(&self) -> u64 {
        self.last_commit()
            .map_or(self.first_seq() - 1, |at| at.commit.last_seq)
    }

    /// Bytes this reader took from the file, each window it filled in full.
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read.load(Ordering::Relaxed)
    }

    /// Read calls this reader made on the file.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Reads the records from `from` on, starting at a base when it can.
    pub fn records(&self, from: u64) -> io::Result<PartRecords> {
        self.records_between(from, self.last_seq(), self.end())
    }

    /// Reads the records of a part up to `limit` and `end`, for a fork.
    pub(crate) fn records_between(
        &self,
        from: u64,
        limit: u64,
        end: u64,
    ) -> io::Result<PartRecords> {
        let base = self.last_commit().and_then(|at| at.commit.base);
        let (offset, first_seq) = match base {
            Some((seq, offset)) if from >= seq => (offset, seq),
            _ => (self.header_end, self.first_seq()),
        };
        PartRecords::open(
            &self.path,
            self.part_id(),
            offset,
            first_seq,
            from,
            limit,
            end,
            Arc::clone(&self.bytes_read),
            Arc::clone(&self.reads),
            Arc::clone(&self.checked),
            self.tail.clone(),
        )
    }
}

/// Whether `error` means the file ends inside a frame rather than a failed read.
fn torn(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
    )
}

/// The commits of a part, oldest first, the chunks it checked, by offset,
/// and the bytes a footer open read, from their offset on.
type Opened = (Vec<CommitAt>, Vec<(u64, Bytes)>, Option<(u64, Bytes)>);

/// Reads every frame of a part from its header on.
///
/// This is the fallback for a part without a footer: a crash before the close,
/// or a part written by an older version.
fn scan(
    source: &mut Source,
    header: &Header,
    header_end: u64,
    file_len: u64,
) -> io::Result<Opened> {
    let mut commits = Vec::new();
    let mut chunks = Vec::new();
    let mut at = header_end;
    while at < file_len {
        let head = match source.head(at) {
            Ok(Some(head)) => head,
            Ok(None) => break,
            Err(error) if torn(&error) => break,
            Err(error) => return Err(error),
        };
        if frame::fits(head.len as usize).is_err() {
            break;
        }
        let next = at + OVERHEAD as u64 + u64::from(head.len);
        if next > file_len {
            break;
        }
        match head.kind {
            kind::COMMIT => match read_commit(source, at, head, header.part_id) {
                Ok(commit) => commits.push(CommitAt { commit, end: next }),
                Err(error) if torn(&error) => break,
                Err(error) => return Err(error),
            },
            kind::CHUNK => chunks.push((at, head)),
            _ => {}
        }
        at = next;
    }
    let checked_from = commits
        .last()
        .and_then(|at: &CommitAt| at.commit.base)
        .map_or(header_end, |(_, offset)| offset);
    let (bad, checked) = check_chunks(source, header.part_id, &chunks, checked_from)?;
    if let Some(bad) = bad {
        commits.retain(|at| at.end <= bad);
    }
    Ok((commits, checked, None))
}

/// Opens a part from the footer at its end.
///
/// `None` means the part has no footer that holds: a missing trailer, a wrong
/// magic, a length that does not fit, a bad checksum or a footer that does not
/// decode. The caller scans instead, so a footer is never an error of its own.
fn open_from_footer(
    source: &mut Source,
    header: &Header,
    header_end: u64,
    file_len: u64,
) -> io::Result<Option<Opened>> {
    let Some((commits, start)) = read_footer(source, header, header_end, file_len)? else {
        return Ok(None);
    };
    let from = commits
        .last()
        .and_then(|at| at.commit.base)
        .map_or(header_end, |(_, offset)| offset)
        .max(header_end);
    if from > start {
        return Ok(None);
    }
    // One read of everything the footer covers: the chunks a resume needs and
    // the commit frames it lists.
    let tail = match start - from {
        0 => Bytes::new(),
        len => match source.read(from, len as usize) {
            Ok(bytes) => Bytes::from(bytes),
            Err(_) => return Ok(None),
        },
    };
    let listed = listed_in_tail(&commits, from);
    let found = FooterTail::walk(header.part_id, &tail, from, &listed);
    if found.mismatch || found.commits != listed.len() {
        // The footer does not describe this file.
        return Ok(None);
    }
    let mut commits = commits;
    if let Some(bad) = found.bad {
        commits.retain(|at| at.end <= bad);
    }
    Ok(Some((commits, found.good, Some((from, tail)))))
}

/// Reads the footer of a closed part: the commits it lists and the offset its
/// frame starts at.
fn read_footer(
    source: &mut Source,
    header: &Header,
    header_end: u64,
    file_len: u64,
) -> io::Result<Option<(Vec<CommitAt>, u64)>> {
    let body = file_len.saturating_sub(header_end);
    if body < (TRAILER_LEN + OVERHEAD) as u64 {
        return Ok(None);
    }
    let trailer = match source.read(file_len - TRAILER_LEN as u64, TRAILER_LEN) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let mut len = [0u8; 4];
    len.copy_from_slice(&trailer[..4]);
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&trailer[4..]);
    let size = u64::from(u32::from_le_bytes(len));
    if u32::from_le_bytes(magic) != TRAILER_MAGIC
        || size < OVERHEAD as u64
        || size > body - TRAILER_LEN as u64
        || frame::fits((size - OVERHEAD as u64) as usize).is_err()
    {
        return Ok(None);
    }
    let start = file_len - TRAILER_LEN as u64 - size;
    let bytes = match source.read(start, size as usize) {
        Ok(bytes) => Bytes::from(bytes),
        Err(_) => return Ok(None),
    };
    let footer = match frame::read(&bytes, 0, header.part_id, NO_SEQ) {
        Ok(footer) => footer,
        Err(_) => return Ok(None),
    };
    if footer.kind != kind::FOOTER {
        return Ok(None);
    }
    let commits = match decode_footer(&footer.payload, start) {
        Ok(commits) => commits,
        Err(_) => return Ok(None),
    };
    Ok(Some((commits, start)))
}

/// The commits a footer lists that lie in the bytes from `from` on.
fn listed_in_tail(commits: &[CommitAt], from: u64) -> Vec<CommitAt> {
    commits.iter().copied().filter(|at| at.end > from).collect()
}

/// What a walk over the bytes a footer covers found.
struct FooterTail {
    /// How many commit frames the walk found.
    commits: usize,
    /// Set when a commit frame is not where the footer says it is, or fails
    /// its checksum.
    mismatch: bool,
    /// Offset of the first chunk that fails its checksum, if any.
    bad: Option<u64>,
    /// The chunk payloads before it, by file offset.
    good: Vec<(u64, Bytes)>,
}

impl FooterTail {
    /// Walks the frames of `tail`, the bytes from file offset `base` on.
    ///
    /// `listed` holds the commits the footer says are in `tail`, oldest first.
    fn walk(part_id: u64, tail: &Bytes, base: u64, listed: &[CommitAt]) -> Self {
        let mut found = FooterTail {
            commits: 0,
            mismatch: false,
            bad: None,
            good: Vec::new(),
        };
        let mut at = 0;
        while let Some(head) = frame::head(&tail[at..]) {
            if frame::fits(head.len as usize).is_err() {
                break;
            }
            let start = at + OVERHEAD;
            let end = start + head.len as usize;
            if end > tail.len() {
                break;
            }
            let payload = tail.slice(start..end);
            match head.kind {
                kind::COMMIT => {
                    // A commit signs itself with the last sequence number it
                    // covers, which the footer lists: no decode needed.
                    let keeps = listed.get(found.commits).is_some_and(|commit| {
                        commit.end == base + end as u64
                            && frame::verify(part_id, &head, commit.commit.last_seq, &payload)
                                .is_ok()
                    });
                    found.mismatch |= !keeps;
                    found.commits += 1;
                }
                kind::CHUNK if found.bad.is_none() => {
                    if frame::verify(part_id, &head, NO_SEQ, &payload).is_err() {
                        found.bad = Some(base + at as u64);
                    } else {
                        found.good.push((base + at as u64, payload));
                    }
                }
                _ => {}
            }
            at = end;
        }
        found
    }
}

/// The first bad chunk's offset, if any, and the good payloads before it.
type Checked = (Option<u64>, Vec<(u64, Bytes)>);

/// Checks the chunks at or after `from`: the offset of the first bad one, if
/// any (a commit after it covers records that cannot be read), and the good
/// payloads before it, by offset.
fn check_chunks(
    source: &mut Source,
    part_id: u64,
    chunks: &[(u64, frame::Head)],
    from: u64,
) -> io::Result<Checked> {
    let mut good = Vec::new();
    for (at, head) in chunks.iter().filter(|(at, _)| *at >= from) {
        let payload = source.read(at + OVERHEAD as u64, head.len as usize)?;
        if frame::verify(part_id, head, NO_SEQ, &payload).is_err() {
            return Ok((Some(*at), good));
        }
        good.push((*at, Bytes::from(payload)));
    }
    Ok((None, good))
}

/// Reads the header frame of a part.
fn read_header(source: &mut Source) -> io::Result<(Header, u64)> {
    let head = match source.read(0, OVERHEAD) {
        Ok(bytes) => frame::head(&bytes),
        Err(_) => None,
    }
    .ok_or_else(|| invalid("part has no header frame"))?;
    if head.kind != kind::HEADER {
        return Err(invalid("part does not start with a header"));
    }
    if frame::fits(head.len as usize).is_err() {
        return Err(invalid("part header is too large"));
    }
    let payload = Bytes::from(source.read(OVERHEAD as u64, head.len as usize)?);
    let header = Header::decode(&payload)?;
    frame::verify(header.part_id, &head, NO_SEQ, &payload)
        .map_err(|_| invalid("part header checksum"))?;
    Ok((header, OVERHEAD as u64 + u64::from(head.len)))
}

/// Reads the commit frame at `at` and checks it.
fn read_commit(
    source: &mut Source,
    at: u64,
    head: frame::Head,
    part_id: u64,
) -> io::Result<Commit> {
    let payload = Bytes::from(source.read(at + OVERHEAD as u64, head.len as usize)?);
    let commit = Commit::decode(&payload)?;
    frame::verify(part_id, &head, commit.last_seq, &payload)
        .map_err(|_| invalid("commit checksum"))?;
    Ok(commit)
}

/// The bytes of one part file, read at an offset.
#[derive(Debug)]
#[expect(
    clippy::disallowed_types,
    reason = "the reader owns a part file and reads it at offsets"
)]
pub(crate) struct Source {
    file: std::fs::File,
    /// Length of the file when it was opened.
    len: u64,
    bytes_read: Arc<AtomicU64>,
    reads: Arc<AtomicU64>,
    /// File offset of the first byte of `window`.
    window_at: u64,
    /// The bytes of the file read last, up to [`WINDOW`], or a tail an open
    /// read already.
    window: Bytes,
    /// Whether reads go through the window (a scan walking frame by frame)
    /// or straight to the file, each for exactly the bytes asked for.
    windowed: bool,
}

/// Most bytes one read into the window takes.
const WINDOW: usize = 256 * 1024;

impl Source {
    /// Opens a part file for reading.
    #[expect(
        clippy::disallowed_types,
        reason = "the reader owns a part file and reads it at offsets"
    )]
    pub(crate) fn open(
        path: &Path,
        bytes_read: Arc<AtomicU64>,
        reads: Arc<AtomicU64>,
    ) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Source {
            file,
            len,
            bytes_read,
            reads,
            window_at: 0,
            window: Bytes::new(),
            windowed: false,
        })
    }

    /// Sends later reads through the window, for a walk over many frames.
    pub(crate) fn windowed(mut self) -> Self {
        self.windowed = true;
        self
    }

    /// Sends the reads of this source through the window from now on.
    pub(crate) fn use_window(&mut self) {
        self.windowed = true;
    }

    /// Serves reads in the `bytes` at file offset `at` from memory.
    pub(crate) fn preload(&mut self, at: u64, bytes: Bytes) {
        self.window_at = at;
        self.window = bytes;
    }

    /// The length of the file when it was opened.
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Reads the fixed part of the frame at `offset`, if the file has it.
    pub(crate) fn head(&mut self, offset: u64) -> io::Result<Option<frame::Head>> {
        if !self.windowed {
            return match self.past_window(offset, OVERHEAD) {
                Ok(bytes) => Ok(frame::head(&bytes)),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
                Err(error) => Err(error),
            };
        }
        if !self.holds(offset, OVERHEAD) {
            self.fill(offset)?;
        }
        if !self.holds(offset, OVERHEAD) {
            return Ok(None);
        }
        Ok(frame::head(
            &self.window[(offset - self.window_at) as usize..],
        ))
    }

    /// Reads `len` bytes at `offset`.
    pub(crate) fn read(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        if !self.windowed || len > WINDOW {
            return self.past_window(offset, len);
        }
        if !self.holds(offset, len) {
            self.fill(offset)?;
        }
        if !self.holds(offset, len) {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        let at = (offset - self.window_at) as usize;
        Ok(self.window[at..at + len].to_vec())
    }

    /// Whether the window holds the `len` bytes at `offset`.
    fn holds(&self, offset: u64, len: usize) -> bool {
        match offset.checked_sub(self.window_at) {
            Some(at) => at + len as u64 <= self.window.len() as u64,
            None => false,
        }
    }

    /// Reads `len` bytes at `offset` from the file, past the window.
    fn past_window(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.file.seek(SeekFrom::Start(offset))?;
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.file.read_exact(&mut buf)?;
        self.bytes_read.fetch_add(len as u64, Ordering::Relaxed);
        Ok(buf)
    }

    /// Fills the window with up to [`WINDOW`] bytes, from `offset` on.
    ///
    /// A scan walks the frames of a part one after the next, so the window
    /// turns its reads into about one read per [`WINDOW`] bytes.
    fn fill(&mut self, offset: u64) -> io::Result<()> {
        self.window = Bytes::new();
        self.window_at = offset;
        let want = self.len.saturating_sub(offset).min(WINDOW as u64) as usize;
        if want == 0 {
            return Ok(());
        }
        let mut buf = vec![0u8; want];
        self.file.seek(SeekFrom::Start(offset))?;
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.file.read_exact(&mut buf)?;
        self.bytes_read.fetch_add(want as u64, Ordering::Relaxed);
        self.window = Bytes::from(buf);
        Ok(())
    }
}

/// The records of one part, from one chunk on.
#[derive(Debug)]
pub struct PartRecords {
    source: Source,
    part_id: u64,
    /// Sequence number of the next record in the stream.
    seq: u64,
    /// First sequence number a caller wants.
    from: u64,
    /// Last visible sequence number.
    limit: u64,
    /// File offset of the next frame.
    at: u64,
    /// File offset where reading stops.
    end: u64,
    stream: Decompressor,
    /// The chunk being walked, and how far into it the walk is.
    chunk: Bytes,
    pos: usize,
    buf: Vec<u8>,
    done: bool,
    /// Chunks the part checked at open, by file offset.
    checked: Arc<[(u64, Bytes)]>,
}

impl PartRecords {
    /// Opens a part at `offset`, where the record `first_seq` starts.
    #[expect(
        clippy::too_many_arguments,
        reason = "one call site, no state to group"
    )]
    fn open(
        path: &Path,
        part_id: u64,
        offset: u64,
        first_seq: u64,
        from: u64,
        limit: u64,
        end: u64,
        bytes_read: Arc<AtomicU64>,
        reads: Arc<AtomicU64>,
        checked: Arc<[(u64, Bytes)]>,
        tail: Option<(u64, Bytes)>,
    ) -> io::Result<Self> {
        let mut source = Source::open(path, bytes_read, reads)?.windowed();
        if let Some((at, bytes)) = tail {
            source.preload(at, bytes);
        }
        Ok(PartRecords {
            source,
            part_id,
            seq: first_seq,
            from,
            limit,
            at: offset,
            end,
            stream: Decompressor::new()?,
            chunk: Bytes::new(),
            pos: 0,
            buf: Vec::new(),
            done: false,
            checked,
        })
    }

    /// Loads the next chunk, skipping the frames that are not chunks.
    fn load(&mut self) -> io::Result<()> {
        loop {
            if self.at >= self.end {
                self.done = true;
                return Ok(());
            }
            let head = match self.source.head(self.at)? {
                Some(head) => head,
                None => {
                    self.done = true;
                    return Ok(());
                }
            };
            if frame::fits(head.len as usize).is_err() {
                return Err(invalid("frame payload too large"));
            }
            let payload_at = self.at + OVERHEAD as u64;
            let next = payload_at + u64::from(head.len);
            if next > self.end {
                return Err(invalid("frame cut short"));
            }
            self.at = next;
            if head.kind != kind::CHUNK {
                continue;
            }
            let chunk_at = payload_at - OVERHEAD as u64;
            let cached = self
                .checked
                .binary_search_by_key(&chunk_at, |(at, _)| *at)
                .ok()
                .map(|index| self.checked[index].1.clone());
            let payload = match cached {
                Some(payload) => payload,
                None => {
                    let payload = Bytes::from(self.source.read(payload_at, head.len as usize)?);
                    frame::verify(self.part_id, &head, NO_SEQ, &payload)
                        .map_err(|_| invalid("chunk checksum"))?;
                    payload
                }
            };
            self.stream.decode_into(&payload, &mut self.buf)?;
            self.chunk = Bytes::from(std::mem::take(&mut self.buf));
            self.pos = 0;
            if self.chunk.is_empty() {
                continue;
            }
            return Ok(());
        }
    }

    /// Reads the record frame at `pos` of the current chunk.
    fn next_record(&mut self) -> io::Result<Option<Record>> {
        let head = match frame::head(&self.chunk[self.pos..]) {
            Some(head) => head,
            None => return Err(invalid("record cut short")),
        };
        let start = self.pos + OVERHEAD;
        let end = start + head.len as usize;
        if end > self.chunk.len() {
            return Err(invalid("record cut short"));
        }
        let payload = self.chunk.slice(start..end);
        frame::verify(self.part_id, &head, self.seq, &payload)
            .map_err(|_| invalid("record checksum"))?;
        let record = Record {
            seq: self.seq,
            kind: head.kind,
            payload,
        };
        self.pos = end;
        self.seq += 1;
        Ok(Some(record))
    }
}

impl Iterator for PartRecords {
    type Item = io::Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done || self.seq > self.limit {
                return None;
            }
            if self.pos >= self.chunk.len() {
                if let Err(error) = self.load() {
                    self.done = true;
                    return Some(Err(error));
                }
                if self.done {
                    return None;
                }
                continue;
            }
            match self.next_record() {
                Ok(Some(record)) if record.seq >= self.from => return Some(Ok(record)),
                Ok(Some(_)) => continue,
                Ok(None) => continue,
                Err(error) => {
                    self.done = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::part::{PartWriter, TRAILER_LEN, TRAILER_MAGIC};
    use crate::testutil;
    use tempfile::TempDir;

    const PART: u64 = 0x0102_0304_0506_0708;

    fn header() -> Header {
        Header {
            part_id: PART,
            session_id: "session".to_owned(),
            created_ms: 1,
            fork: None,
            meta: Bytes::new(),
        }
    }

    /// Three records per commit, `commits` commits, no trailing records.
    fn part_bytes(commits: usize) -> Bytes {
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        for _ in 0..commits {
            for index in 0..3u8 {
                writer
                    .append(10, &Bytes::from(vec![index; 8]), false)
                    .expect("append");
            }
            writer.commit().expect("commit");
        }
        writer.finish().expect("finish");
        Bytes::from(out)
    }

    #[test]
    fn the_header_is_read_back() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        testutil::write(&path, &part_bytes(1));
        let part = Part::open(&path).expect("open");
        assert_eq!(part.header(), &header());
        assert_eq!(part.part_id(), PART);
        assert_eq!(part.commits().len(), 1);
        assert_eq!(part.first_seq(), 1);
        assert_eq!(part.last_seq(), 3);
        assert_eq!(part.end(), part.last_commit().expect("commit").end);
    }

    #[test]
    fn the_records_of_the_last_commit_are_visible() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        for _ in 0..2 {
            writer
                .append(10, &Bytes::from_static(b"kept"), false)
                .expect("append");
        }
        writer.commit().expect("commit");
        writer
            .append(10, &Bytes::from_static(b"lost"), false)
            .expect("append");
        drop(writer);
        testutil::write(&path, &out);

        let part = Part::open(&path).expect("open");
        let records: Vec<Record> = part
            .records(1)
            .expect("records")
            .map(|r| r.expect("record"))
            .collect();
        assert_eq!(records.len(), 2, "the uncommitted record is invisible");
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[1].seq, 2);
        assert_eq!(records[0].payload.as_ref(), b"kept");
    }

    #[test]
    fn a_cut_at_every_offset_reads_the_earlier_commits() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let full = part_bytes(3);
        testutil::write(&path, &full);
        let header_end = Part::open(&path).expect("open").header_end() as usize;

        for cut in 0..full.len() {
            testutil::write(&path, &full[..cut]);
            let part = match Part::open(&path) {
                Ok(part) => part,
                Err(_) => {
                    assert!(cut < header_end, "a part without its header fails");
                    continue;
                }
            };
            let records: Vec<Record> = part
                .records(1)
                .expect("records")
                .map(|record| record.expect("record"))
                .collect();
            let committed = part.commits().len();
            assert!(
                committed <= 3 && records.len() == committed * 3,
                "cut at {cut}: {committed} commits and {} records",
                records.len()
            );
            for (index, record) in records.iter().enumerate() {
                assert_eq!(record.seq, index as u64 + 1, "cut at {cut}");
            }
        }
    }

    #[test]
    fn a_flipped_byte_in_the_last_frame_drops_only_that_commit() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let full = part_bytes(3);
        let mut raw = full.to_vec();
        // A closed part ends with its footer, so the last frame that carries a
        // commit is the last commit before it.
        let at = last_commit(&raw);
        let len = frame::head(&raw[at..]).expect("commit head").len as usize;
        raw[at + OVERHEAD + len - 1] ^= 0x40;
        testutil::write(&path, &raw);

        let part = Part::open(&path).expect("open");
        assert_eq!(part.commits().len(), 2, "the last commit is dropped");
        assert_eq!(part.last_seq(), 6);
        let records = part.records(1).expect("records").count();
        assert_eq!(records, 6);
    }

    #[test]
    fn a_torn_header_is_an_error() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        testutil::write(&path, b"RSPART01");
        assert!(Part::open(&path).is_err(), "no header frame");
        let mut raw = part_bytes(1).to_vec();
        raw.truncate(12);
        testutil::write(&path, &raw);
        assert!(Part::open(&path).is_err(), "the header payload is cut");
    }

    #[test]
    fn an_unknown_frame_kind_is_skipped() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let mut raw = part_bytes(2).to_vec();
        frame::write(&mut raw, PART, NO_SEQ, 200, b"a frame from the future").expect("frame");
        testutil::write(&path, &raw);
        let part = Part::open(&path).expect("open");
        assert_eq!(part.commits().len(), 2);
        assert_eq!(part.records(1).expect("records").count(), 6);
    }

    #[test]
    fn a_bad_chunk_drops_the_commits_after_it() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let mut raw = part_bytes(2).to_vec();
        let head = frame::head(&raw).expect("header head");
        let chunk_at = OVERHEAD + u64::from(head.len) as usize;
        assert_eq!(
            frame::head(&raw[chunk_at..]).expect("chunk head").kind,
            kind::CHUNK
        );
        raw[chunk_at + OVERHEAD] ^= 0x01;
        testutil::write(&path, &raw);
        let part = Part::open(&path).expect("open");
        assert!(
            part.last_commit().is_none(),
            "no commit after a bad chunk is kept"
        );
        let records: Vec<_> = part.records(1).expect("records").collect();
        assert!(records.is_empty(), "nothing uncommitted is served");
    }

    #[test]
    fn a_bad_chunk_before_the_base_fails_when_read() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        for index in 0..3u8 {
            writer
                .append(10, &Bytes::from(vec![index; 8]), false)
                .expect("append");
        }
        writer.commit().expect("commit");
        writer
            .append(10, &Bytes::from_static(b"base"), true)
            .expect("base");
        writer.commit().expect("commit");
        writer.finish().expect("finish");
        let head = frame::head(&out).expect("header head");
        let chunk_at = OVERHEAD + u64::from(head.len) as usize;
        out[chunk_at + OVERHEAD] ^= 0x01;
        testutil::write(&path, &out);
        let part = Part::open(&path).expect("open");
        assert_eq!(part.last_seq(), 4, "the chunks after the base are good");
        let from_base: Vec<_> = part.records(4).expect("records").collect();
        assert_eq!(from_base.len(), 1);
        assert!(from_base[0].is_ok());
        let last = part.records(1).expect("records").last();
        assert!(
            matches!(last, Some(Err(_))),
            "a bad old chunk is an error, not silence"
        );
    }

    #[test]
    fn bytes_read_is_counted() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let full = part_bytes(2);
        testutil::write(&path, &full);
        let part = Part::open(&path).expect("open");
        let after_open = part.bytes_read();
        assert!(
            after_open > 0,
            "opening reads the header, the chunks and the commits"
        );
        let records: Vec<Record> = part
            .records(1)
            .expect("records")
            .map(|record| record.expect("record"))
            .collect();
        assert_eq!(records.len(), 6);
        assert!(
            part.bytes_read() < 2 * full.len() as u64,
            "the chunks checked at open are not read again"
        );
    }

    #[test]
    fn payloads_share_one_decoded_chunk() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        testutil::write(&path, &part_bytes(1));
        let part = Part::open(&path).expect("open");
        let records: Vec<Record> = part
            .records(1)
            .expect("records")
            .map(|record| record.expect("record"))
            .collect();
        assert_eq!(records.len(), 3);
        let first = records[0].payload.as_ptr() as usize;
        let third = records[2].payload.as_ptr() as usize;
        assert!(third > first, "one buffer, slices into it");
        assert!(
            third - first < 64,
            "the slices are near each other: {}",
            third - first
        );
    }

    /// Commits of the part the footer tests open.
    const COMMITS: usize = 2000;
    /// Records in one commit of that part.
    const PER_COMMIT: usize = 3;
    /// Bytes of each of its records, so the part is many windows long.
    const PAYLOAD: usize = 400;

    /// A closed part of `commits` commits of incompressible records.
    fn big_part(commits: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..commits {
            for _ in 0..PER_COMMIT {
                let mut payload = vec![0u8; PAYLOAD];
                for byte in &mut payload {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *byte = state as u8;
                }
                writer
                    .append(10, &Bytes::from(payload), false)
                    .expect("append");
            }
            writer.commit().expect("commit");
        }
        writer.finish().expect("finish");
        out
    }

    /// File offset where the footer frame of a closed part starts.
    fn footer_start(bytes: &[u8]) -> usize {
        let at = bytes.len() - TRAILER_LEN;
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("length")) as usize;
        assert_eq!(
            u32::from_le_bytes(bytes[at + 4..].try_into().expect("magic")),
            TRAILER_MAGIC,
            "a closed part ends with a footer trailer"
        );
        at - len
    }

    /// File offsets of the chunk frames of a part, oldest first.
    fn chunk_offsets(bytes: &[u8]) -> Vec<usize> {
        let mut at = 0;
        let mut seen = Vec::new();
        while let Some(head) = frame::head(&bytes[at..]) {
            if head.kind == kind::CHUNK {
                seen.push(at);
            }
            at += OVERHEAD + head.len as usize;
        }
        seen
    }

    /// File offset where the last commit frame of a part starts.
    fn last_commit(bytes: &[u8]) -> usize {
        let mut at = 0;
        let mut last = None;
        while let Some(head) = frame::head(&bytes[at..]) {
            if head.kind == kind::COMMIT {
                last = Some(at);
            }
            at += OVERHEAD + head.len as usize;
        }
        last.expect("a commit frame")
    }

    #[test]
    fn a_closed_part_opens_from_its_footer() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let full = big_part(COMMITS);
        testutil::write(&path, &full);
        let part = Part::open(&path).expect("open");
        let reads = part.reads();
        println!(
            "{reads} reads to open {COMMITS} commits of a {} byte part",
            full.len()
        );
        assert_eq!(part.commits().len(), COMMITS);
        assert_eq!(part.last_seq(), (COMMITS * PER_COMMIT) as u64);

        // The same part with its footer cut off: what the scan finds.
        let cut = footer_start(&full);
        testutil::write(&path, &full[..cut]);
        let scanned = Part::open(&path).expect("open by scan");
        assert_eq!(part.commits(), scanned.commits());
        assert_eq!(scanned.last_seq(), part.last_seq());
        assert!(
            reads <= 6,
            "{reads} reads to open a closed part of {} bytes",
            full.len()
        );
    }

    #[test]
    fn a_part_with_no_footer_opens_by_scan() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let full = big_part(COMMITS);
        let cut = footer_start(&full);
        testutil::write(&path, &full[..cut]);
        let part = Part::open(&path).expect("open");
        assert_eq!(part.commits().len(), COMMITS);
        assert_eq!(part.last_seq(), (COMMITS * PER_COMMIT) as u64);
        let records: Vec<Record> = part
            .records(1)
            .expect("records")
            .map(|record| record.expect("record"))
            .collect();
        assert_eq!(records.len(), COMMITS * PER_COMMIT);
        assert_eq!(records[0].seq, 1);
        let frames = 1 + 2 * COMMITS;
        let reads = part.reads();
        println!("{reads} reads to scan {frames} frames");
        assert!(
            reads * 100 < frames as u64,
            "{reads} reads to scan {frames} frames"
        );
    }

    #[test]
    fn a_bad_footer_falls_back_to_the_scan() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let full = big_part(COMMITS);
        testutil::write(&path, &full);
        let expected = Part::open(&path).expect("open").commits().to_vec();
        assert_eq!(expected.len(), COMMITS);
        let start = footer_start(&full);

        let mut flipped_payload = full.clone();
        flipped_payload[start + OVERHEAD] ^= 0x01;
        let mut flipped_magic = full.clone();
        let last = flipped_magic.len() - 1;
        flipped_magic[last] ^= 0x01;
        let mut wrong_length = full.clone();
        let at = wrong_length.len() - TRAILER_LEN;
        wrong_length[at..at + 4].copy_from_slice(&(full.len() as u32).to_le_bytes());

        for (what, raw) in [
            ("a flipped payload byte", flipped_payload),
            ("a flipped magic byte", flipped_magic),
            ("a length that does not fit", wrong_length),
        ] {
            testutil::write(&path, &raw);
            let part = Part::open(&path).expect("open");
            assert_eq!(part.commits(), expected.as_slice(), "{what}");
            assert_eq!(
                part.records(1).expect("records").count(),
                COMMITS * PER_COMMIT,
                "{what}"
            );
        }
    }

    #[test]
    fn a_bad_chunk_after_the_base_drops_later_commits_with_a_footer() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        for _ in 0..PER_COMMIT {
            writer
                .append(10, &Bytes::from(vec![7u8; 64]), false)
                .expect("append");
        }
        writer.commit().expect("commit");
        writer
            .append(10, &Bytes::from_static(b"base"), true)
            .expect("base");
        writer.commit().expect("commit");
        for _ in 0..PER_COMMIT {
            writer
                .append(10, &Bytes::from(vec![9u8; 64]), false)
                .expect("append");
        }
        writer.commit().expect("commit");
        writer.finish().expect("finish");
        assert!(out.len() > TRAILER_LEN);
        // The last chunk holds the records after the base: break one byte.
        let chunks = chunk_offsets(&out);
        assert!(
            chunks.len() > 2,
            "a chunk before the base, at it and after: {chunks:?}"
        );
        out[*chunks.last().expect("a chunk") + OVERHEAD] ^= 0x01;
        testutil::write(&path, &out);

        let part = Part::open(&path).expect("open");
        assert_eq!(
            part.commits().len(),
            2,
            "the commit that covers the bad chunk is dropped"
        );
        assert_eq!(part.last_seq(), (PER_COMMIT + 1) as u64);
        let from_base: Vec<Record> = part
            .records((PER_COMMIT + 1) as u64)
            .expect("records")
            .map(|record| record.expect("record"))
            .collect();
        assert_eq!(from_base.len(), 1);
        assert_eq!(from_base[0].payload.as_ref(), b"base");
    }

    #[test]
    fn a_part_without_commits_has_no_records() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("part.rsp");
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        writer
            .append(10, &Bytes::from_static(b"x"), false)
            .expect("append");
        drop(writer);
        testutil::write(&path, &out);
        let part = Part::open(&path).expect("open");
        assert!(part.last_commit().is_none());
        assert_eq!(part.records(1).expect("records").count(), 0);
        assert_eq!(part.last_seq(), 0, "nothing is visible");
    }
}
