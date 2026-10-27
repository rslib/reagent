//! A frame: `[u32 len][u8 kind][u32 crc32c]` and then `len` payload bytes.
//!
//! The CRC covers the length, the kind, a sequence number and the payload and
//! is seeded with the part id, so a frame cannot be moved between parts or
//! records without the check failing. A frame that carries no sequence number
//! of its own (a header or a chunk) uses [`NO_SEQ`].

use std::fmt;

use bytes::Bytes;
use crc32c::crc32c_append;

/// Largest payload a frame may carry: 64 MiB.
pub const MAX_PAYLOAD: usize = 64 * 1024 * 1024;
/// Bytes in front of every payload.
pub const OVERHEAD: usize = 4 + 1 + 4;
/// Sequence number of a frame that has none of its own.
pub const NO_SEQ: u64 = 0;

/// A payload that would not fit [`MAX_PAYLOAD`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooLarge;

impl fmt::Display for TooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "payload over {MAX_PAYLOAD} bytes")
    }
}

impl std::error::Error for TooLarge {}

/// What went wrong while reading a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The buffer ends inside the frame.
    Truncated,
    /// The length field is over [`MAX_PAYLOAD`].
    TooLarge,
    /// The CRC does not match the frame.
    Crc,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self {
            FrameError::Truncated => "frame cut short",
            FrameError::TooLarge => "frame payload too large",
            FrameError::Crc => "frame checksum mismatch",
        };
        f.write_str(what)
    }
}

impl std::error::Error for FrameError {}

/// The fixed part of a frame: what a reader knows before the payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    /// Payload length.
    pub len: u32,
    /// What the payload means.
    pub kind: u8,
    /// Checksum over the frame.
    pub crc: u32,
}

/// A frame with its payload cut out of the buffer it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// What the payload means.
    pub kind: u8,
    /// The payload, a slice of the input.
    pub payload: Bytes,
}

/// Checks a payload length against [`MAX_PAYLOAD`].
pub fn fits(len: usize) -> Result<(), TooLarge> {
    if len > MAX_PAYLOAD {
        return Err(TooLarge);
    }
    Ok(())
}

/// The checksum of one frame.
pub fn crc(part_id: u64, len: u32, kind: u8, seq: u64, payload: &[u8]) -> u32 {
    let mut crc = crc32c_append(0, &part_id.to_le_bytes());
    crc = crc32c_append(crc, &len.to_le_bytes());
    crc = crc32c_append(crc, &[kind]);
    crc = crc32c_append(crc, &seq.to_le_bytes());
    crc32c_append(crc, payload)
}

/// Appends one frame to `out`.
pub fn write(
    out: &mut Vec<u8>,
    part_id: u64,
    seq: u64,
    kind: u8,
    payload: &[u8],
) -> Result<(), TooLarge> {
    fits(payload.len())?;
    let len = payload.len() as u32;
    out.extend_from_slice(&len.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&crc(part_id, len, kind, seq, payload).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Reads the fixed part of a frame from the first [`OVERHEAD`] bytes of `at`.
pub fn head(at: &[u8]) -> Option<Head> {
    if at.len() < OVERHEAD {
        return None;
    }
    let mut len = [0u8; 4];
    len.copy_from_slice(&at[..4]);
    let mut crc = [0u8; 4];
    crc.copy_from_slice(&at[5..OVERHEAD]);
    Some(Head {
        len: u32::from_le_bytes(len),
        kind: at[4],
        crc: u32::from_le_bytes(crc),
    })
}

/// Checks the checksum of a frame whose payload is `payload`.
pub fn verify(part_id: u64, head: &Head, seq: u64, payload: &[u8]) -> Result<(), FrameError> {
    if payload.len() as u64 != u64::from(head.len) {
        return Err(FrameError::Truncated);
    }
    if crc(part_id, head.len, head.kind, seq, payload) != head.crc {
        return Err(FrameError::Crc);
    }
    Ok(())
}

/// Reads one frame at `at`.
///
/// The kind is returned as it is: a reader skips a kind it does not know.
pub fn read(bytes: &Bytes, at: usize, part_id: u64, seq: u64) -> Result<Frame, FrameError> {
    let head = match bytes.get(at..) {
        Some(rest) => head(rest).ok_or(FrameError::Truncated)?,
        None => return Err(FrameError::Truncated),
    };
    fits(head.len as usize).map_err(|_| FrameError::TooLarge)?;
    let start = at + OVERHEAD;
    let end = start
        .checked_add(head.len as usize)
        .ok_or(FrameError::Truncated)?;
    if end > bytes.len() {
        return Err(FrameError::Truncated);
    }
    let payload = bytes.slice(start..end);
    verify(part_id, &head, seq, &payload)?;
    Ok(Frame {
        kind: head.kind,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind;

    const PART: u64 = 0x1234_5678_9abc_def0;

    fn frame(seq: u64, kind: u8, payload: &[u8]) -> Bytes {
        let mut out = Vec::new();
        write(&mut out, PART, seq, kind, payload).expect("write");
        Bytes::from(out)
    }

    #[test]
    fn round_trip_keeps_kind_and_payload() {
        let bytes = frame(7, 42, b"payload");
        let frame = read(&bytes, 0, PART, 7).expect("read");
        assert_eq!(frame.kind, 42);
        assert_eq!(frame.payload.as_ref(), b"payload");
        assert_eq!(bytes.len(), OVERHEAD + b"payload".len());
    }

    #[test]
    fn crc_covers_len_kind_seq_and_payload() {
        let base = crc(PART, 3, 9, 1, b"abc");
        assert_ne!(base, crc(PART, 4, 9, 1, b"abcd"), "length");
        assert_ne!(base, crc(PART, 3, 8, 1, b"abc"), "kind");
        assert_ne!(base, crc(PART, 3, 9, 2, b"abc"), "sequence number");
        assert_ne!(base, crc(PART, 3, 9, 1, b"abd"), "payload");
        assert_ne!(base, crc(PART + 1, 3, 9, 1, b"abc"), "part id");
    }

    #[test]
    fn a_frame_from_another_part_fails_the_crc() {
        let bytes = frame(1, 5, b"abc");
        assert_eq!(read(&bytes, 0, PART + 1, 1), Err(FrameError::Crc));
        assert_eq!(read(&bytes, 0, PART, 2), Err(FrameError::Crc));
    }

    #[test]
    fn unknown_kind_is_returned_to_the_caller() {
        let bytes = frame(NO_SEQ, 200, b"future");
        let frame = read(&bytes, 0, PART, NO_SEQ).expect("read");
        assert_eq!(frame.kind, 200, "the caller skips what it does not know");
        assert_eq!(frame.payload.as_ref(), b"future");
    }

    #[test]
    fn every_truncation_fails_cleanly() {
        let bytes = frame(3, kind::COMMIT, b"commit payload");
        for cut in 0..bytes.len() {
            let short = bytes.slice(0..cut);
            assert_eq!(
                read(&short, 0, PART, 3),
                Err(FrameError::Truncated),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn flipped_byte_fails_the_crc() {
        for at in 0..(OVERHEAD + 5) {
            let mut raw = frame(1, 9, b"abcde").to_vec();
            raw[at] ^= 0x01;
            let bytes = Bytes::from(raw);
            assert!(
                read(&bytes, 0, PART, 1).is_err(),
                "a flip at {at} must not pass"
            );
        }
    }

    #[test]
    fn length_past_the_cap_is_an_error() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        raw.push(kind::CHUNK);
        raw.extend_from_slice(&0u32.to_le_bytes());
        let bytes = Bytes::from(raw);
        assert_eq!(read(&bytes, 0, PART, NO_SEQ), Err(FrameError::TooLarge));
    }

    #[test]
    fn write_rejects_a_payload_over_the_cap() {
        assert!(fits(MAX_PAYLOAD).is_ok());
        assert_eq!(fits(MAX_PAYLOAD + 1), Err(TooLarge));
    }

    #[test]
    fn read_at_an_offset_inside_a_buffer() {
        let mut raw = frame(NO_SEQ, kind::HEADER, b"header").to_vec();
        let at = raw.len();
        raw.extend_from_slice(&frame(1, kind::COMMIT, b"commit"));
        let bytes = Bytes::from(raw);
        let frame = read(&bytes, at, PART, 1).expect("read");
        assert_eq!(frame.kind, kind::COMMIT);
        assert_eq!(frame.payload.as_ref(), b"commit");
    }

    #[test]
    fn known_frame_kinds_are_distinct() {
        let kinds = [
            kind::HEADER,
            kind::CHUNK,
            kind::COMMIT,
            kind::LIST_ENTRY,
            kind::LIST_TOMBSTONE,
            kind::FOOTER,
        ];
        let mut seen = kinds.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), kinds.len(), "a frame kind is never reused");
        assert!(
            kinds.iter().all(|kind| *kind < kind::RECORD_FIRST),
            "store kinds stay below the record kinds"
        );
    }
}
