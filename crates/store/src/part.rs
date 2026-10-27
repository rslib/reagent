//! One part file: a header, records through one zstd stream, and commits.
//!
//! A part is written once and never touched again. Records are visible to a
//! reader only up to the last commit that survived the crash.

use std::io::{self, Write};

use bytes::Bytes;

use crate::codec::{Decoder, Encoder, Text, Value};
use crate::frame::{self, NO_SEQ};
use crate::kind;
use crate::stream::{Compressor, MAX_CHUNK_DECODED};

/// Bytes at the start of every part header.
pub const MAGIC: &[u8; 8] = b"RSPART01";
/// Format version of a part file.
pub const FORMAT_VERSION: u64 = 1;
/// File extension of a part file.
pub const EXT: &str = "rsp";
/// Characters of a part id in a file name.
pub const PART_ID_HEX: usize = 16;
/// Sequence number of the first record of a session.
pub const FIRST_SEQ: u64 = 1;
/// Bytes after the last frame of a closed part: the length of the footer frame
/// and a magic number.
pub const TRAILER_LEN: usize = 8;
/// Magic number that ends a closed part, read as a little-endian `u32`.
pub const TRAILER_MAGIC: u32 = u32::from_le_bytes(*b"RSFT");

/// Header field numbers, fixed for the life of the format.
const FIELD_MAGIC: u32 = 1;
const FIELD_VERSION: u32 = 2;
const FIELD_PART_ID: u32 = 3;
const FIELD_SESSION_ID: u32 = 4;
const FIELD_CREATED_MS: u32 = 5;
const FIELD_FORK: u32 = 6;
const FIELD_META: u32 = 7;

/// Commit field numbers, fixed for the life of the format.
const FIELD_LAST_SEQ: u32 = 1;
const FIELD_COUNT: u32 = 2;
const FIELD_FRAME_OFFSET: u32 = 3;
const FIELD_BASE: u32 = 4;

/// Footer field numbers, fixed for the life of the format.
///
/// A footer holds one [`FIELD_ENTRY`] message per commit, and each of them
/// holds the fields of a commit plus [`FIELD_END`].
const FIELD_ENTRY: u32 = 1;
const FIELD_END: u32 = 5;

/// Fork point field numbers, fixed for the life of the format.
const FIELD_FORK_PART: u32 = 1;
const FIELD_FORK_SEQ: u32 = 2;
const FIELD_FORK_OFFSET: u32 = 3;

/// Where a forked part reads the records before it from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForkPoint {
    /// The part that owns the records before the fork.
    pub part: u64,
    /// The last record of that part a reader sees.
    pub seq: u64,
    /// Byte offset of the parent's last commit at that point.
    pub offset: u64,
}

/// The first frame of a part: what the file is and what it continues from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Random id of this part, also in its file name.
    pub part_id: u64,
    /// The session this part belongs to.
    pub session_id: String,
    /// Wall clock of the run that wrote the part.
    pub created_ms: u64,
    /// The part and record this part continues from.
    pub fork: Option<ForkPoint>,
    /// What the caller keeps about the part; the store does not read it.
    pub meta: Bytes,
}

impl Header {
    /// A header for a new part, with a fresh random part id.
    pub fn new(session_id: String, created_ms: u64, fork: Option<ForkPoint>, meta: Bytes) -> Self {
        Header {
            part_id: crate::paths::random_u64(),
            session_id,
            created_ms,
            fork,
            meta,
        }
    }

    /// Writes the header as a header frame payload.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut enc = Encoder::new(out);
        enc.bytes(FIELD_MAGIC, MAGIC);
        enc.u64(FIELD_VERSION, FORMAT_VERSION);
        enc.u64(FIELD_PART_ID, self.part_id);
        enc.str(FIELD_SESSION_ID, &self.session_id);
        enc.u64(FIELD_CREATED_MS, self.created_ms);
        if let Some(fork) = self.fork {
            enc.message(FIELD_FORK, |inner| {
                inner.u64(FIELD_FORK_PART, fork.part);
                inner.u64(FIELD_FORK_SEQ, fork.seq);
                inner.u64(FIELD_FORK_OFFSET, fork.offset);
            });
        }
        enc.bytes(FIELD_META, &self.meta);
    }

    /// Reads a header frame payload.
    pub fn decode(payload: &Bytes) -> io::Result<Self> {
        let mut decoder = Decoder::new(payload);
        let mut header = Header {
            part_id: 0,
            session_id: String::new(),
            created_ms: 0,
            fork: None,
            meta: Bytes::new(),
        };
        let mut magic = false;
        let mut version = 0;
        while let Some((field, value)) = decoder.next_field().map_err(decode_error)? {
            match field {
                FIELD_MAGIC => magic = bytes_of(&value)?.as_ref() == MAGIC,
                FIELD_VERSION => version = varint_of(&value)?,
                FIELD_PART_ID => header.part_id = varint_of(&value)?,
                FIELD_SESSION_ID => header.session_id = text_of(&value)?.as_str().to_owned(),
                FIELD_CREATED_MS => header.created_ms = varint_of(&value)?,
                FIELD_FORK => header.fork = Some(fork_from(bytes_of(&value)?)?),
                FIELD_META => header.meta = bytes_of(&value)?.clone(),
                _ => {}
            }
        }
        if !magic {
            return Err(invalid("not a part header"));
        }
        if version != FORMAT_VERSION {
            return Err(invalid("part format version"));
        }
        Ok(header)
    }
}

/// The record that makes the records written before it visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Commit {
    /// Sequence number of the last record the commit covers.
    pub last_seq: u64,
    /// Records written since the commit before this one.
    pub count: u64,
    /// Byte offset where the current zstd frame starts.
    pub frame_offset: u64,
    /// Sequence number and chunk offset of the last base record.
    pub base: Option<(u64, u64)>,
}

impl Commit {
    /// Writes the commit as a commit frame payload.
    pub fn encode(&self, out: &mut Vec<u8>) {
        self.fields(&mut Encoder::new(out));
    }

    /// Writes the commit's fields into `enc`, without where it ends.
    fn fields(&self, enc: &mut Encoder<'_>) {
        enc.u64(FIELD_LAST_SEQ, self.last_seq);
        enc.u64(FIELD_COUNT, self.count);
        enc.u64(FIELD_FRAME_OFFSET, self.frame_offset);
        if let Some((seq, offset)) = self.base {
            enc.message(FIELD_BASE, |inner| {
                inner.u64(FIELD_FORK_SEQ, seq);
                inner.u64(FIELD_FORK_OFFSET, offset);
            });
        }
    }

    /// Reads a commit frame payload.
    pub fn decode(payload: &Bytes) -> io::Result<Self> {
        let mut decoder = Decoder::new(payload);
        let mut commit = Commit {
            last_seq: 0,
            count: 0,
            frame_offset: 0,
            base: None,
        };
        while let Some((field, value)) = decoder.next_field().map_err(decode_error)? {
            match field {
                FIELD_LAST_SEQ => commit.last_seq = varint_of(&value)?,
                FIELD_COUNT => commit.count = varint_of(&value)?,
                FIELD_FRAME_OFFSET => commit.frame_offset = varint_of(&value)?,
                FIELD_BASE => commit.base = Some(base_from(bytes_of(&value)?)?),
                _ => {}
            }
        }
        Ok(commit)
    }
}

/// A commit and the byte offset just after its frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitAt {
    /// What the commit covers.
    pub commit: Commit,
    /// File offset after the commit frame: where the part ends.
    pub end: u64,
}

impl CommitAt {
    /// Writes this commit and where it ends as one footer entry.
    fn encode(&self, enc: &mut Encoder<'_>) {
        self.commit.fields(enc);
        enc.u64(FIELD_END, self.end);
    }

    /// Reads one footer entry.
    fn decode(payload: &Bytes) -> io::Result<Self> {
        let mut decoder = Decoder::new(payload);
        let mut commit = Commit {
            last_seq: 0,
            count: 0,
            frame_offset: 0,
            base: None,
        };
        let mut end = 0;
        while let Some((field, value)) = decoder.next_field().map_err(decode_error)? {
            match field {
                FIELD_LAST_SEQ => commit.last_seq = varint_of(&value)?,
                FIELD_COUNT => commit.count = varint_of(&value)?,
                FIELD_FRAME_OFFSET => commit.frame_offset = varint_of(&value)?,
                FIELD_BASE => commit.base = Some(base_from(bytes_of(&value)?)?),
                FIELD_END => end = varint_of(&value)?,
                _ => {}
            }
        }
        Ok(CommitAt { commit, end })
    }
}

/// The commits a closed part lists in its footer, oldest first.
///
/// Each commit must end at least one frame after the one before it and no
/// later than `start`, the footer's offset, and the decode stops at the first
/// that does not: the list can never outgrow the commit frames the part could
/// hold, whatever the footer claims.
pub(crate) fn decode_footer(payload: &Bytes, start: u64) -> io::Result<Vec<CommitAt>> {
    let mut decoder = Decoder::new(payload);
    let mut commits = Vec::new();
    let mut last = 0;
    while let Some((field, value)) = decoder.next_field().map_err(decode_error)? {
        if field == FIELD_ENTRY {
            let at = CommitAt::decode(bytes_of(&value)?)?;
            if at.end < last + frame::OVERHEAD as u64 || at.end > start {
                return Err(invalid("footer commits out of order"));
            }
            last = at.end;
            commits.push(at);
        }
    }
    Ok(commits)
}

/// Reads the sequence number and the offset of a base record.
fn base_from(bytes: &Bytes) -> io::Result<(u64, u64)> {
    let mut inner = Decoder::new(bytes);
    let mut seq = 0;
    let mut offset = 0;
    while let Some((field, value)) = inner.next_field().map_err(decode_error)? {
        match field {
            FIELD_FORK_SEQ => seq = varint_of(&value)?,
            FIELD_FORK_OFFSET => offset = varint_of(&value)?,
            _ => {}
        }
    }
    Ok((seq, offset))
}

/// Reads a fork point out of a header's nested field.
fn fork_from(bytes: &Bytes) -> io::Result<ForkPoint> {
    let mut inner = Decoder::new(bytes);
    let mut fork = ForkPoint::default();
    while let Some((field, value)) = inner.next_field().map_err(decode_error)? {
        match field {
            FIELD_FORK_PART => fork.part = varint_of(&value)?,
            FIELD_FORK_SEQ => fork.seq = varint_of(&value)?,
            FIELD_FORK_OFFSET => fork.offset = varint_of(&value)?,
            _ => {}
        }
    }
    Ok(fork)
}

/// Writes the part of one run: header, records, commits.
#[derive(Debug)]
pub struct PartWriter<W: Write> {
    out: W,
    stream: Compressor,
    header: Header,
    /// Sequence number of the next record.
    next_seq: u64,
    /// Sequence number of the record written last.
    last_seq: u64,
    /// Records written since the last commit.
    since_commit: u64,
    /// The last base record: its sequence number and its chunk offset.
    base: Option<(u64, u64)>,
    /// Scratch for one encoded frame.
    frame: Vec<u8>,
    /// Scratch for a header or commit payload.
    payload: Vec<u8>,
    /// Compressed bytes waiting to be written as a chunk.
    chunk: Vec<u8>,
    /// File offset of the next byte written.
    offset: u64,
    /// File offset where the current zstd frame starts.
    frame_offset: u64,
    /// Set after a failed write: the part may hold a partial frame, so no
    /// later record or commit is written.
    failed: bool,
    /// Raw record bytes in the stream since the last chunk was written.
    unflushed: usize,
    /// Every commit of the part, oldest first, for the footer.
    commits: Vec<CommitAt>,
}

impl<W: Write> PartWriter<W> {
    /// Writes the header frame to `out` and starts the zstd stream.
    pub fn new(mut out: W, header: Header) -> io::Result<Self> {
        let mut payload = Vec::new();
        header.encode(&mut payload);
        let mut frame = Vec::new();
        let mut offset = 0;
        write_frame(
            &mut out,
            &mut offset,
            &mut frame,
            header.part_id,
            NO_SEQ,
            kind::HEADER,
            &payload,
        )?;
        let first_seq = header.fork.map_or(FIRST_SEQ, |fork| fork.seq + 1);
        Ok(PartWriter {
            out,
            stream: Compressor::new()?,
            header,
            next_seq: first_seq,
            last_seq: first_seq - 1,
            since_commit: 0,
            base: None,
            frame,
            payload,
            chunk: Vec::new(),
            offset,
            frame_offset: offset,
            failed: false,
            unflushed: 0,
            commits: Vec::new(),
        })
    }

    /// The part id this file carries.
    pub fn part_id(&self) -> u64 {
        self.header.part_id
    }

    /// File offset of the next byte written.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Sequence number of the record written last.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Appends one record; a base record starts a new zstd frame before it.
    pub fn append(&mut self, kind: u8, payload: &Bytes, base: bool) -> io::Result<()> {
        self.check()?;
        frame::fits(payload.len())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let result = self.append_inner(kind, payload, base);
        self.failed |= result.is_err();
        result
    }

    fn append_inner(&mut self, kind: u8, payload: &Bytes, base: bool) -> io::Result<()> {
        if base {
            self.end_frame()?;
            self.base = Some((self.next_seq, self.offset));
        }
        let seq = self.next_seq;
        self.frame.clear();
        frame::write(&mut self.frame, self.header.part_id, seq, kind, payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        if self.unflushed + self.frame.len() > MAX_CHUNK_DECODED {
            self.flush_chunk()?;
        }
        self.stream.write(&self.frame)?;
        self.unflushed += self.frame.len();
        self.next_seq += 1;
        self.last_seq = seq;
        self.since_commit += 1;
        Ok(())
    }

    /// Flushes the stream, writes the chunk and the commit that covers it.
    pub fn commit(&mut self) -> io::Result<()> {
        self.check()?;
        let result = self.commit_inner();
        self.failed |= result.is_err();
        result
    }

    /// Fails once a write has failed.
    fn check(&self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("an earlier write to this part failed"));
        }
        Ok(())
    }

    fn commit_inner(&mut self) -> io::Result<()> {
        if self.since_commit == 0 {
            return Ok(());
        }
        self.flush_chunk()?;
        let commit = Commit {
            last_seq: self.last_seq,
            count: self.since_commit,
            frame_offset: self.frame_offset,
            base: self.base,
        };
        self.payload.clear();
        commit.encode(&mut self.payload);
        write_frame(
            &mut self.out,
            &mut self.offset,
            &mut self.frame,
            self.header.part_id,
            commit.last_seq,
            kind::COMMIT,
            &self.payload,
        )?;
        self.commits.push(CommitAt {
            commit,
            end: self.offset,
        });
        self.since_commit = 0;
        Ok(())
    }

    /// Commits what is pending and writes the footer that closes the part.
    ///
    /// Nothing may follow the footer: a part is written once.
    pub fn close(&mut self) -> io::Result<()> {
        self.check()?;
        self.commit()?;
        let result = self.footer();
        self.failed |= result.is_err();
        result
    }

    /// Writes the footer frame and the trailer that ends the file.
    fn footer(&mut self) -> io::Result<()> {
        self.payload.clear();
        let mut enc = Encoder::new(&mut self.payload);
        for at in &self.commits {
            enc.message(FIELD_ENTRY, |inner| at.encode(inner));
        }
        frame::fits(self.payload.len())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let len = (frame::OVERHEAD + self.payload.len()) as u32;
        write_frame(
            &mut self.out,
            &mut self.offset,
            &mut self.frame,
            self.header.part_id,
            NO_SEQ,
            kind::FOOTER,
            &self.payload,
        )?;
        let mut trailer = [0u8; TRAILER_LEN];
        trailer[..4].copy_from_slice(&len.to_le_bytes());
        trailer[4..].copy_from_slice(&TRAILER_MAGIC.to_le_bytes());
        self.out.write_all(&trailer)?;
        self.offset += TRAILER_LEN as u64;
        Ok(())
    }

    /// Commits what is pending, closes the part and hands the file back.
    pub fn finish(mut self) -> io::Result<W> {
        self.close()?;
        Ok(self.out)
    }

    /// The file the writer writes to, for a sync or a lock.
    pub fn out_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Flushes the stream and writes the bytes as one chunk frame.
    fn flush_chunk(&mut self) -> io::Result<()> {
        self.unflushed = 0;
        self.chunk.clear();
        self.stream.flush_into(&mut self.chunk)?;
        #[cfg(test)]
        crate::hook::push(self.header.part_id, crate::hook::Event::Flush);
        self.write_chunk()
    }

    /// Ends the zstd frame and puts its last bytes in a chunk frame.
    fn end_frame(&mut self) -> io::Result<()> {
        self.unflushed = 0;
        self.chunk.clear();
        self.stream.end_frame_into(&mut self.chunk)?;
        self.write_chunk()?;
        self.frame_offset = self.offset;
        Ok(())
    }

    fn write_chunk(&mut self) -> io::Result<()> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        write_frame(
            &mut self.out,
            &mut self.offset,
            &mut self.frame,
            self.header.part_id,
            NO_SEQ,
            kind::CHUNK,
            &self.chunk,
        )?;
        self.chunk.clear();
        Ok(())
    }
}

/// Writes one frame to `out` and counts its bytes in `at`.
fn write_frame<W: Write>(
    out: &mut W,
    at: &mut u64,
    buf: &mut Vec<u8>,
    part_id: u64,
    seq: u64,
    kind: u8,
    payload: &[u8],
) -> io::Result<()> {
    buf.clear();
    frame::write(buf, part_id, seq, kind, payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    #[cfg(test)]
    if crate::hook::fault(part_id) {
        return Err(io::Error::other("injected write failure"));
    }
    out.write_all(buf)?;
    #[cfg(test)]
    if kind == kind::CHUNK {
        crate::hook::push(part_id, crate::hook::Event::Write);
    }
    *at += buf.len() as u64;
    Ok(())
}

/// An invalid-data error with a fixed message.
pub(crate) fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

/// A field of the wrong wire type.
fn wrong_type() -> io::Error {
    invalid("field of the wrong type")
}

fn decode_error(error: crate::codec::DecodeError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn varint_of(value: &Value) -> io::Result<u64> {
    value.varint().ok_or_else(wrong_type)
}

fn bytes_of(value: &Value) -> io::Result<&Bytes> {
    value.bytes().ok_or_else(wrong_type)
}

fn text_of(value: &Value) -> io::Result<Text> {
    Text::new(bytes_of(value)?.clone()).map_err(|_| invalid("field is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::OVERHEAD;

    fn header() -> Header {
        Header {
            part_id: 0xabcd_ef01_2345_6789,
            session_id: "01j0sessionid0000000000000".to_owned(),
            created_ms: 1_700_000_000_000,
            fork: None,
            meta: Bytes::from_static(b"meta"),
        }
    }

    #[test]
    fn a_footer_of_empty_entries_is_refused_early() {
        // 100,000 empty entries: two bytes each, all ending at offset 0. Each
        // would cost a 56-byte commit if the decode kept them.
        let payload = Bytes::from([0x0a_u8, 0x00].repeat(100_000));
        assert!(decode_footer(&payload, 1 << 20).is_err());
    }

    #[test]
    fn a_footer_cannot_list_more_commits_than_frames_fit_before_it() {
        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out);
        for end in 1..=1000_u64 {
            let at = CommitAt {
                commit: Commit {
                    last_seq: end,
                    count: 1,
                    frame_offset: 0,
                    base: None,
                },
                end: end * frame::OVERHEAD as u64,
            };
            enc.message(FIELD_ENTRY, |inner| at.encode(inner));
        }
        let payload = Bytes::from(out);
        assert_eq!(
            decode_footer(&payload, 1000 * frame::OVERHEAD as u64)
                .expect("frames one overhead apart")
                .len(),
            1000
        );
        assert!(decode_footer(&payload, 999 * frame::OVERHEAD as u64).is_err());
    }

    /// Every frame of a written part, in order, checked against its crc.
    fn frames(bytes: &Bytes, part_id: u64) -> Vec<(u8, Bytes)> {
        let mut seen = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let head = frame::head(&bytes[at..]).expect("frame head");
            let start = at + OVERHEAD;
            let end = start + head.len as usize;
            let payload = bytes.slice(start..end);
            // A commit signs itself with the last sequence number it covers.
            let seq = if head.kind == kind::COMMIT {
                Commit::decode(&payload).expect("commit").last_seq
            } else {
                NO_SEQ
            };
            frame::read(bytes, at, part_id, seq).expect("frame");
            at = end;
            seen.push((head.kind, payload));
        }
        seen
    }

    #[test]
    fn a_new_header_has_a_random_part_id() {
        let first = Header::new("s".to_owned(), 1, None, Bytes::new());
        let second = Header::new("s".to_owned(), 1, None, Bytes::new());
        assert_ne!(first.part_id, second.part_id);
        assert_eq!(first.session_id, "s");
    }

    #[test]
    fn header_round_trips() {
        let mut payload = Vec::new();
        header().encode(&mut payload);
        let decoded = Header::decode(&Bytes::from(payload)).expect("decode");
        assert_eq!(decoded, header());
        assert_eq!(decoded.fork, None);
    }

    #[test]
    fn header_with_a_fork_round_trips() {
        let mut head = header();
        head.fork = Some(ForkPoint {
            part: 7,
            seq: 5,
            offset: 1234,
        });
        let mut payload = Vec::new();
        head.encode(&mut payload);
        let decoded = Header::decode(&Bytes::from(payload)).expect("decode");
        assert_eq!(decoded, head);
    }

    #[test]
    fn a_wrong_magic_is_an_error() {
        let mut payload = Vec::new();
        Encoder::new(&mut payload).bytes(FIELD_MAGIC, b"NOTPART1");
        assert!(Header::decode(&Bytes::from(payload)).is_err());
    }

    #[test]
    fn an_unknown_version_is_an_error() {
        let mut payload = Vec::new();
        let mut enc = Encoder::new(&mut payload);
        enc.bytes(FIELD_MAGIC, MAGIC);
        enc.u64(FIELD_VERSION, FORMAT_VERSION + 1);
        assert!(Header::decode(&Bytes::from(payload)).is_err());
    }

    #[test]
    fn commit_round_trips_with_and_without_a_base() {
        let plain = Commit {
            last_seq: 9,
            count: 4,
            frame_offset: 100,
            base: None,
        };
        let mut payload = Vec::new();
        plain.encode(&mut payload);
        assert_eq!(
            Commit::decode(&Bytes::from(payload)).expect("decode"),
            plain
        );

        let with_base = Commit {
            base: Some((9, 64)),
            ..plain
        };
        let mut payload = Vec::new();
        with_base.encode(&mut payload);
        assert_eq!(
            Commit::decode(&Bytes::from(payload)).expect("decode"),
            with_base
        );
    }

    #[test]
    fn records_get_consecutive_sequence_numbers() {
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        for _ in 0..3 {
            writer
                .append(10, &Bytes::from_static(b"x"), false)
                .expect("append");
        }
        assert_eq!(writer.last_seq(), 3);
        writer.commit().expect("commit");
        assert_eq!(writer.last_seq(), 3);
    }

    #[test]
    fn a_fork_continues_the_sequence_number() {
        let mut head = header();
        head.fork = Some(ForkPoint {
            part: 1,
            seq: 41,
            offset: 10,
        });
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, head).expect("part");
        writer
            .append(10, &Bytes::from_static(b"x"), false)
            .expect("append");
        assert_eq!(writer.last_seq(), 42, "the fork's seq + 1");
    }

    #[test]
    fn a_commit_covers_the_records_before_it() {
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        for _ in 0..3 {
            writer
                .append(10, &Bytes::from_static(b"a"), false)
                .expect("append");
        }
        writer.commit().expect("commit");
        for _ in 0..2 {
            writer
                .append(10, &Bytes::from_static(b"b"), false)
                .expect("append");
        }
        writer.commit().expect("commit");
        drop(writer);
        let bytes = Bytes::from(out);
        let commits: Vec<Commit> = frames(&bytes, header().part_id)
            .into_iter()
            .filter(|(kind, _)| *kind == kind::COMMIT)
            .map(|(_, payload)| Commit::decode(&payload).expect("decode"))
            .collect();
        assert_eq!(commits.len(), 2);
        assert_eq!((commits[0].last_seq, commits[0].count), (3, 3));
        assert_eq!((commits[1].last_seq, commits[1].count), (5, 2));
        assert_eq!(commits[0].base, None);
    }

    #[test]
    fn a_commit_with_nothing_new_writes_nothing() {
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        let after_header = writer.offset();
        writer.commit().expect("commit");
        assert_eq!(writer.offset(), after_header, "no frames without records");
    }

    #[test]
    fn a_base_record_starts_a_new_frame_and_records_its_offset() {
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        for _ in 0..4 {
            writer
                .append(10, &Bytes::from_static(b"before"), false)
                .expect("append");
        }
        writer.commit().expect("commit");
        writer
            .append(11, &Bytes::from_static(b"base"), true)
            .expect("append base");
        let base_offset = writer.frame_offset;
        assert_eq!(
            base_offset,
            writer.offset(),
            "the new frame starts where the base chunk will be written"
        );
        writer.commit().expect("commit");
        drop(writer);
        let bytes = Bytes::from(out);
        let mut chunk_at = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let head = frame::head(&bytes[at..]).expect("frame head");
            if head.kind == kind::CHUNK {
                chunk_at.push(at as u64);
            }
            at += OVERHEAD + head.len as usize;
        }
        assert!(
            chunk_at.contains(&base_offset),
            "a chunk starts at the base offset {base_offset}: {chunk_at:?}"
        );
        let last = frames(&bytes, header().part_id)
            .into_iter()
            .rev()
            .find(|(kind, _)| *kind == kind::COMMIT)
            .map(|(_, payload)| Commit::decode(&payload).expect("decode"))
            .expect("a commit");
        assert_eq!(last.base, Some((5, base_offset)));
        assert_eq!(last.frame_offset, base_offset);
    }

    #[test]
    fn records_are_inside_the_chunks_and_not_in_the_file() {
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        for _ in 0..20 {
            writer
                .append(
                    10,
                    &Bytes::from_static(b"a record payload that repeats"),
                    false,
                )
                .expect("append");
        }
        writer.commit().expect("commit");
        drop(writer);
        let bytes = Bytes::from(out);
        let kinds: Vec<u8> = frames(&bytes, header().part_id)
            .into_iter()
            .map(|(kind, _)| kind)
            .collect();
        assert_eq!(kinds, vec![kind::HEADER, kind::CHUNK, kind::COMMIT]);
        let raw = 20 * b"a record payload that repeats".len();
        assert!(
            bytes.len() < raw,
            "records are written compressed: {} bytes for {raw}",
            bytes.len()
        );
    }

    #[test]
    fn a_payload_over_the_frame_cap_is_an_error() {
        let mut out = Vec::new();
        let mut writer = PartWriter::new(&mut out, header()).expect("part");
        let big = Bytes::from(vec![0u8; frame::MAX_PAYLOAD + 1]);
        let error = writer.append(10, &big, false).expect_err("too large");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
