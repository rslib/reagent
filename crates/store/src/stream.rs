//! The zstd stream of one part: records in, chunks out, and back again.
//!
//! A part writes its records through one zstd frame. Every commit flushes the
//! frame and the flushed bytes become one chunk; a base record ends the frame
//! so a reader can start decoding there.

use std::fmt;
use std::io::{self, Write};

/// zstd level of the live stream.
pub const LEVEL: i32 = 1;

/// Compresses the records of one part.
pub struct Compressor {
    enc: Option<zstd::stream::write::Encoder<'static, Vec<u8>>>,
}

impl Compressor {
    /// Starts a new zstd frame at [`LEVEL`].
    pub fn new() -> io::Result<Self> {
        Ok(Compressor {
            enc: Some(encoder(Vec::new())?),
        })
    }

    /// Writes bytes of the current frame into the stream.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.encoder()?.write_all(bytes)
    }

    /// Flushes the stream and leaves the bytes it produced in `out`.
    pub fn flush_into(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        let enc = self.encoder()?;
        enc.flush()?;
        out.clear();
        std::mem::swap(enc.get_mut(), out);
        enc.get_mut().clear();
        Ok(())
    }

    /// Ends the current frame, leaving its last bytes in `out`, and starts a
    /// new frame with the same buffers.
    pub fn end_frame_into(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        let enc = self.take()?;
        out.clear();
        let mut tail = enc.finish()?;
        out.append(&mut tail);
        self.enc = Some(encoder(tail)?);
        Ok(())
    }

    fn encoder(&mut self) -> io::Result<&mut zstd::stream::write::Encoder<'static, Vec<u8>>> {
        match self.enc.as_mut() {
            Some(enc) => Ok(enc),
            None => Err(finished()),
        }
    }

    fn take(&mut self) -> io::Result<zstd::stream::write::Encoder<'static, Vec<u8>>> {
        match self.enc.take() {
            Some(enc) => Ok(enc),
            None => Err(finished()),
        }
    }
}

impl fmt::Debug for Compressor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Compressor")
            .field("open", &self.enc.is_some())
            .finish()
    }
}

impl fmt::Debug for Decompressor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decompressor").finish_non_exhaustive()
    }
}

/// A fresh frame, writing into `out`.
fn encoder(out: Vec<u8>) -> io::Result<zstd::stream::write::Encoder<'static, Vec<u8>>> {
    zstd::stream::write::Encoder::new(out, LEVEL)
}

/// The stream is over: a second end or flush of the same frame.
fn finished() -> io::Error {
    io::Error::other("zstd stream has ended")
}

/// The most bytes one chunk may decode to. The writer flushes a chunk before
/// it would pass this, so a larger chunk is a corrupt or crafted file.
pub const MAX_CHUNK_DECODED: usize = 2 * crate::frame::MAX_PAYLOAD;

/// A buffer that refuses to grow past its limit.
struct Capped(Vec<u8>, usize);

impl Write for Capped {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len() + bytes.len() > self.1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a chunk decodes past its limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Decompresses the chunks of one part back into bytes.
pub struct Decompressor {
    dec: zstd::stream::write::Decoder<'static, Capped>,
}

impl Decompressor {
    /// Starts decoding a stream at its first frame.
    pub fn new() -> io::Result<Self> {
        Self::with_limit(MAX_CHUNK_DECODED)
    }

    /// Starts decoding with at most `limit` decoded bytes per chunk.
    pub(crate) fn with_limit(limit: usize) -> io::Result<Self> {
        Ok(Decompressor {
            dec: zstd::stream::write::Decoder::new(Capped(Vec::new(), limit))?,
        })
    }

    /// Decodes one chunk and leaves the decoded bytes in `out`.
    ///
    /// Chunks of one frame follow each other, and a chunk that starts a new
    /// frame is decoded without a new [`Decompressor`].
    pub fn decode_into(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        out.clear();
        self.dec.write_all(chunk)?;
        self.dec.flush()?;
        std::mem::swap(&mut self.dec.get_mut().0, out);
        self.dec.get_mut().0.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::part::Header;
    use crate::recover::Record;
    use crate::{Part, Session, Writer, paths};
    use bytes::Bytes;
    use tempfile::TempDir;

    fn record(seq: u64) -> Vec<u8> {
        format!("record {seq} ")
            .repeat(1 + seq as usize % 3)
            .into_bytes()
    }

    fn write_all(comp: &mut Compressor, records: &[Vec<u8>]) -> io::Result<Vec<Vec<u8>>> {
        let mut chunks = Vec::new();
        for part in records.chunks(10) {
            for record in part {
                comp.write(record)?;
            }
            let mut out = Vec::new();
            comp.flush_into(&mut out)?;
            assert!(!out.is_empty(), "a flush writes something");
            chunks.push(out);
        }
        Ok(chunks)
    }

    fn decode_all(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut dec = Decompressor::new().expect("decoder");
        let mut all = Vec::new();
        let mut out = Vec::new();
        for chunk in chunks {
            dec.decode_into(chunk, &mut out).expect("decode");
            all.extend_from_slice(&out);
        }
        all
    }

    #[test]
    fn records_round_trip_through_one_stream() {
        let records: Vec<Vec<u8>> = (0..100).map(record).collect();
        let mut comp = Compressor::new().expect("compressor");
        let chunks = write_all(&mut comp, &records).expect("write");
        assert_eq!(chunks.len(), 10, "one chunk per flush");
        let decoded = decode_all(&chunks);
        let expected: Vec<u8> = records.concat();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn a_taken_chunk_survives_the_next_flush() {
        let mut comp = Compressor::new().expect("compressor");
        let mut first = Vec::new();
        comp.write(b"first").expect("write");
        comp.flush_into(&mut first).expect("flush");
        let kept = first.clone();
        let mut second = Vec::new();
        comp.write(b"second").expect("write");
        comp.flush_into(&mut second).expect("flush");
        assert_eq!(first, kept, "the caller's buffer is not reused behind it");
        let mut dec = Decompressor::new().expect("decoder");
        let mut out = Vec::new();
        dec.decode_into(&first, &mut out).expect("decode first");
        let mut all = out.clone();
        dec.decode_into(&second, &mut out).expect("decode second");
        all.extend_from_slice(&out);
        assert_eq!(all, b"firstsecond");
    }

    #[test]
    fn a_frame_end_starts_a_new_frame_in_the_same_stream() {
        let first: Vec<u8> = record(1).repeat(50);
        let second: Vec<u8> = record(2).repeat(50);
        let mut comp = Compressor::new().expect("compressor");
        comp.write(&first).expect("first");
        let mut head = Vec::new();
        comp.flush_into(&mut head).expect("flush");
        let mut end = Vec::new();
        comp.end_frame_into(&mut end).expect("end frame");
        comp.write(&second).expect("second");
        let mut tail = Vec::new();
        comp.flush_into(&mut tail).expect("flush");

        let mut chunks = vec![head];
        if !end.is_empty() {
            chunks.push(end);
        }
        chunks.push(tail);

        // One decoder walks both frames.
        assert_eq!(decode_all(&chunks), [first, second.clone()].concat());
        // A fresh decoder starts at the second frame.
        let mut fresh = Decompressor::new().expect("decoder");
        let mut out = Vec::new();
        fresh
            .decode_into(&chunks[chunks.len() - 1], &mut out)
            .expect("decode");
        assert_eq!(out, second);
    }

    /// Random bytes added to each record of the resume test.
    const FILLER_BYTES: usize = 400;

    #[tokio::test]
    async fn resume_reads_from_the_last_base() {
        let dir = TempDir::new().expect("temp dir");
        let part_id = 0x4242;
        let header = Header {
            part_id,
            session_id: "01jsession".to_owned(),
            created_ms: 1,
            fork: None,
            meta: Bytes::new(),
        };
        let mut writer = Writer::create(dir.path(), header).expect("writer");
        // Incompressible filler, so the part is far larger than the slack.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for index in 1..=2000u64 {
            let mut payload = format!("record {index}").into_bytes();
            payload.push(b' ');
            for _ in 0..FILLER_BYTES {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                payload.push(state as u8);
            }
            writer
                .append(10, Bytes::from(payload), index == 1500)
                .expect("append");
            if index % 10 == 0 {
                writer.commit().expect("commit");
            }
        }
        writer.close().expect("close");

        let path = paths::part_path(dir.path(), part_id);
        let part = Part::open(&path).expect("part");
        let base = part
            .last_commit()
            .expect("a commit")
            .commit
            .base
            .expect("a base record");
        assert_eq!(base.0, 1500, "the base record's sequence number");
        let file_len = crate::testutil::len(&path);
        let written_after_base = file_len - base.1;

        let session = Session::open(dir.path()).expect("session");
        assert_eq!(session.last_base(), Some(1500));
        let seen: Vec<Record> = session
            .records_from(1500)
            .map(|record| record.expect("record"))
            .collect();
        assert_eq!(seen.len(), 501, "records 1500 to 2000");
        assert_eq!(seen[0].seq, 1500);
        assert!(seen[0].payload.starts_with(b"record 1500 "));
        assert_eq!(seen[500].seq, 2000);
        let read = session.bytes_read();
        println!(
            "{read} bytes read for a {file_len}-byte part, {written_after_base} of them after the base"
        );
        assert!(
            read < written_after_base + 64 * 1024,
            "{read} bytes read for {written_after_base} bytes written after the base"
        );
        assert!(
            written_after_base + 64 * 1024 < file_len / 2,
            "the test part is large enough that the bound means something"
        );
        assert!(
            read < file_len / 2,
            "{read} bytes read of a {file_len}-byte part"
        );
    }

    /// Random bytes added to each record of the footer resume test.
    const FOOTER_FILLER: usize = 400;

    #[tokio::test]
    async fn a_footer_part_resumes_without_reading_before_the_base() {
        let dir = TempDir::new().expect("temp dir");
        let part_id = 0x4343;
        let header = Header {
            part_id,
            session_id: "01jsession".to_owned(),
            created_ms: 1,
            fork: None,
            meta: Bytes::new(),
        };
        let mut writer = Writer::create(dir.path(), header).expect("writer");
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for index in 1..=600u64 {
            let mut payload = format!("record {index}").into_bytes();
            for _ in 0..FOOTER_FILLER {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                payload.push(state as u8);
            }
            writer
                .append(10, Bytes::from(payload), index == 400)
                .expect("append");
            if index % 10 == 0 {
                writer.commit().expect("commit");
            }
        }
        writer.close().expect("close");

        let path = paths::part_path(dir.path(), part_id);
        let file_len = crate::testutil::len(&path);
        let part = Part::open(&path).expect("part");
        let base = part
            .last_commit()
            .expect("a commit")
            .commit
            .base
            .expect("a base record");
        let reads = part.reads();
        assert!(
            reads <= 6,
            "a closed part opens from its footer: {reads} reads"
        );
        drop(part);

        let written_after_base = file_len - base.1;
        let session = Session::open(dir.path()).expect("session");
        let seen: Vec<Record> = session
            .records_from(base.0)
            .map(|record| record.expect("record"))
            .collect();
        assert_eq!(seen.len(), 201, "records 400 to 600");
        assert_eq!(seen[0].seq, 400);
        let read = session.bytes_read();
        println!("{read} bytes read for a {file_len}-byte part, after its base");
        assert!(
            read < written_after_base + 64 * 1024,
            "{read} bytes read for {written_after_base} bytes written after the base"
        );
        assert!(
            read < file_len / 2,
            "{read} bytes read of a {file_len}-byte part"
        );
    }

    #[test]
    fn large_payloads_cross_the_buffer_boundaries() {
        let big = vec![b'z'; 200_000];
        let mut comp = Compressor::new().expect("compressor");
        comp.write(&big).expect("write");
        let mut chunk = Vec::new();
        comp.flush_into(&mut chunk).expect("flush");
        assert!(chunk.len() < big.len(), "the payload compresses");
        let mut dec = Decompressor::new().expect("decoder");
        let mut out = Vec::new();
        dec.decode_into(&chunk, &mut out).expect("decode");
        assert_eq!(out.len(), big.len());
        assert_eq!(out, big);
    }

    #[test]
    fn a_chunk_that_decodes_past_the_limit_is_an_error() {
        let zeros = vec![0u8; 4096];
        let packed = zstd::bulk::compress(&zeros, LEVEL).expect("compress");
        let mut decoder = Decompressor::with_limit(1024).expect("decoder");
        let mut out = Vec::new();
        let error = decoder
            .decode_into(&packed, &mut out)
            .expect_err("over the limit");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
