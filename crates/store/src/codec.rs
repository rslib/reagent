//! Protobuf-style fields: a `(field << 3) | wire` key and LEB128 varints.
//!
//! [`Encoder`] appends fields to a caller's buffer. [`Decoder`] walks a
//! [`Bytes`] and hands every length-delimited value back as a slice of it.
//! A caller that knows a field is text turns it into a [`Text`] once.

use std::fmt;

use bytes::Bytes;

/// Most bytes a varint may take.
const MAX_VARINT_BYTES: usize = 10;
/// Bits of a key that hold the wire type.
const WIRE_BITS: u32 = 3;
/// Value of the bits a key holds the wire type in.
const WIRE_MASK: u64 = (1 << WIRE_BITS) - 1;
/// Bits a byte of a varint carries.
const VARINT_BITS: u32 = 7;
/// Value of the bits a byte of a varint carries.
const VARINT_MASK: u64 = (1 << VARINT_BITS) - 1;
/// Bit of a varint byte that says another byte follows.
const VARINT_MORE: u8 = 0x80;
/// Varints stop at the shift that would overflow a `u64`.
const VARINT_MAX_SHIFT: u32 = 63;

/// Wire types, as the low bits of a key say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    /// A LEB128 varint.
    Varint = 0,
    /// Eight bytes, little endian.
    Fixed64 = 1,
    /// A varint length and that many bytes.
    Len = 2,
    /// Four bytes, little endian.
    Fixed32 = 5,
}

/// What went wrong while decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeErrorKind {
    /// A varint longer than ten bytes.
    Varint,
    /// A wire type a key may not carry.
    Wire,
    /// A length past the end of the input.
    Length,
}

/// A decode failure and the byte it was found at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeError {
    /// Offset of the field that failed, from the start of the input.
    pub offset: usize,
    /// What was wrong with the field.
    pub kind: DecodeErrorKind,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            DecodeErrorKind::Varint => "varint over ten bytes",
            DecodeErrorKind::Wire => "bad wire type",
            DecodeErrorKind::Length => "length past the end",
        };
        write!(f, "{what} at byte {}", self.offset)
    }
}

impl std::error::Error for DecodeError {}

/// One decoded field value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// A varint field.
    Varint(u64),
    /// A fixed64 field.
    Fixed64(u64),
    /// A fixed32 field.
    Fixed32(u32),
    /// A length-delimited field, a slice of the input.
    Bytes(Bytes),
}

impl Value {
    /// The value of a varint field.
    pub fn varint(&self) -> Option<u64> {
        match self {
            Value::Varint(value) => Some(*value),
            _ => None,
        }
    }

    /// The bytes of a length-delimited field.
    pub fn bytes(&self) -> Option<&Bytes> {
        match self {
            Value::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }
}

/// A string field: bytes that were UTF-8 when they were decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text(Bytes);

impl Text {
    /// Wraps bytes that must be UTF-8, checking them once.
    pub fn new(bytes: Bytes) -> Result<Self, std::str::Utf8Error> {
        std::str::from_utf8(&bytes)?;
        Ok(Text(bytes))
    }

    /// The text, as a string.
    ///
    /// The bytes were checked in [`Text::new`]; this reads them again as a
    /// string without unsafe code, so the check costs a second scan.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or_default()
    }

    /// The text as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The text as the bytes it was decoded from.
    pub fn into_bytes(self) -> Bytes {
        self.0
    }

    /// Whether the text is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Writes fields into a buffer the caller owns and reuses.
#[derive(Debug)]
pub struct Encoder<'a> {
    out: &'a mut Vec<u8>,
}

impl<'a> Encoder<'a> {
    /// Writes into `out`, from its current end.
    pub fn new(out: &'a mut Vec<u8>) -> Self {
        Encoder { out }
    }

    /// Writes a varint field.
    pub fn u64(&mut self, field: u32, value: u64) {
        self.key(field, Wire::Varint);
        self.varint(value);
    }

    /// Writes a zigzag `i64` field.
    pub fn i64(&mut self, field: u32, value: i64) {
        self.u64(field, zigzag(value));
    }

    /// Writes a bool field.
    pub fn bool(&mut self, field: u32, value: bool) {
        self.u64(field, u64::from(value));
    }

    /// Writes a length-delimited field.
    pub fn bytes(&mut self, field: u32, value: &[u8]) {
        self.key(field, Wire::Len);
        self.varint(value.len() as u64);
        self.out.extend_from_slice(value);
    }

    /// Writes a string field.
    pub fn str(&mut self, field: u32, value: &str) {
        self.bytes(field, value.as_bytes());
    }

    /// Writes a nested message: its fields, behind a length.
    ///
    /// A one-byte length, the common case, is patched in place; a longer one
    /// moves the message up inside the buffer, so a buffer with room in it
    /// does not allocate.
    pub fn message(&mut self, field: u32, body: impl FnOnce(&mut Encoder<'_>)) {
        self.key(field, Wire::Len);
        let at = self.out.len();
        self.out.push(0);
        let mut inner = Encoder { out: self.out };
        body(&mut inner);
        let len = self.out.len() - at - 1;
        if len < VARINT_MASK as usize {
            self.out[at] = len as u8;
        } else {
            let mut head = [0u8; MAX_VARINT_BYTES];
            let mut used = 0;
            put_varint(&mut head, &mut used, len as u64);
            self.out.splice(at..at + 1, head[..used].iter().copied());
        }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.out.len()
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.out.is_empty()
    }

    fn key(&mut self, field: u32, wire: Wire) {
        self.varint(u64::from(field) << WIRE_BITS | wire as u64);
    }

    fn varint(&mut self, value: u64) {
        let mut value = value;
        while value > VARINT_MASK {
            self.out
                .push((value as u8 & VARINT_MASK as u8) | VARINT_MORE);
            value >>= VARINT_BITS;
        }
        self.out.push(value as u8);
    }
}

/// `i64` to `u64`: small magnitudes stay small.
fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> VARINT_MAX_SHIFT)) as u64
}

/// `u64` to `i64`, the inverse of [`zigzag`].
fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Writes `value` as a varint into `out` at `used`, moving `used` on.
fn put_varint(out: &mut [u8; MAX_VARINT_BYTES], used: &mut usize, value: u64) {
    let mut value = value;
    while value > VARINT_MASK {
        out[*used] = (value as u8 & VARINT_MASK as u8) | VARINT_MORE;
        *used += 1;
        value >>= VARINT_BITS;
    }
    out[*used] = value as u8;
    *used += 1;
}

/// Walks the fields of one buffer.
#[derive(Debug)]
pub struct Decoder<'a> {
    buf: &'a Bytes,
    pos: usize,
}

impl<'a> Decoder<'a> {
    /// Walks `buf` from its start.
    pub fn new(buf: &'a Bytes) -> Self {
        Decoder { buf, pos: 0 }
    }

    /// The next field, or `None` at the end of the input.
    ///
    /// Unknown fields are skipped by their wire type: a caller that does not
    /// know a field number just drops it. The returned [`Bytes`] share the
    /// input buffer.
    pub fn next_field(&mut self) -> Result<Option<(u32, Value)>, DecodeError> {
        if self.pos >= self.buf.len() {
            return Ok(None);
        }
        let at = self.pos;
        let key = self.varint()?;
        let field = key >> WIRE_BITS;
        let field = u32::try_from(field).map_err(|_| DecodeError {
            offset: at,
            kind: DecodeErrorKind::Varint,
        })?;
        let value = match key & WIRE_MASK {
            0 => Value::Varint(self.varint()?),
            1 => Value::Fixed64(self.fixed64()?),
            2 => {
                let len = self.varint()?;
                let len = usize::try_from(len).map_err(|_| DecodeError {
                    offset: at,
                    kind: DecodeErrorKind::Length,
                })?;
                let end = self.pos.checked_add(len).ok_or(DecodeError {
                    offset: at,
                    kind: DecodeErrorKind::Length,
                })?;
                if end > self.buf.len() {
                    return Err(DecodeError {
                        offset: at,
                        kind: DecodeErrorKind::Length,
                    });
                }
                let bytes = self.buf.slice(self.pos..end);
                self.pos = end;
                Value::Bytes(bytes)
            }
            5 => Value::Fixed32(self.fixed32()?),
            _ => {
                return Err(DecodeError {
                    offset: at,
                    kind: DecodeErrorKind::Wire,
                });
            }
        };
        Ok(Some((field, value)))
    }

    /// Bytes left to read.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn varint(&mut self) -> Result<u64, DecodeError> {
        let at = self.pos;
        let mut value = 0u64;
        for index in 0..MAX_VARINT_BYTES {
            let byte = match self.buf.get(self.pos) {
                Some(byte) => *byte,
                None => {
                    return Err(DecodeError {
                        offset: at,
                        kind: DecodeErrorKind::Varint,
                    });
                }
            };
            self.pos += 1;
            let bits = u64::from(byte & VARINT_MASK as u8);
            let shift = index as u32 * VARINT_BITS;
            let shifted = bits.checked_shl(shift).ok_or(DecodeError {
                offset: at,
                kind: DecodeErrorKind::Varint,
            })?;
            if index == MAX_VARINT_BYTES - 1 && bits > 1 {
                return Err(DecodeError {
                    offset: at,
                    kind: DecodeErrorKind::Varint,
                });
            }
            value |= shifted;
            if byte & VARINT_MORE == 0 {
                return Ok(value);
            }
        }
        Err(DecodeError {
            offset: at,
            kind: DecodeErrorKind::Varint,
        })
    }

    fn fixed64(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.take(8)?;
        let mut word = [0u8; 8];
        word.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(word))
    }

    fn fixed32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.take(4)?;
        let mut word = [0u8; 4];
        word.copy_from_slice(bytes);
        Ok(u32::from_le_bytes(word))
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let at = self.pos;
        let end = at.checked_add(len).ok_or(DecodeError {
            offset: at,
            kind: DecodeErrorKind::Length,
        })?;
        if end > self.buf.len() {
            return Err(DecodeError {
                offset: at,
                kind: DecodeErrorKind::Length,
            });
        }
        let slice = &self.buf[at..end];
        self.pos = end;
        Ok(slice)
    }
}

/// A text field, checked once; `None` for a value that is not bytes.
pub fn text_of(value: &Value) -> Option<Result<Text, std::str::Utf8Error>> {
    value.bytes().map(|bytes| Text::new(bytes.clone()))
}

/// An `i64` field value, decoded from its zigzag form.
pub fn i64_of(value: u64) -> i64 {
    unzigzag(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(build: impl FnOnce(&mut Encoder<'_>)) -> Bytes {
        let mut out = Vec::new();
        build(&mut Encoder::new(&mut out));
        Bytes::from(out)
    }

    fn fields(buf: &Bytes) -> Vec<(u32, Value)> {
        let mut decoder = Decoder::new(buf);
        let mut seen = Vec::new();
        while let Some(field) = decoder.next_field().expect("decode") {
            seen.push(field);
        }
        seen
    }

    #[test]
    fn round_trip_every_type() {
        let buf = put(|enc| {
            enc.u64(1, 300);
            enc.u64(2, u64::MAX);
            enc.i64(3, -1);
            enc.i64(4, i64::MIN);
            enc.i64(5, i64::MAX);
            enc.bool(6, true);
            enc.bool(7, false);
            enc.bytes(8, b"raw");
            enc.str(9, "session-id");
        });
        let seen = fields(&buf);
        assert_eq!(seen[0], (1, Value::Varint(300)));
        assert_eq!(seen[1], (2, Value::Varint(u64::MAX)));
        assert_eq!(i64_of(seen[2].1.varint().expect("varint")), -1);
        assert_eq!(i64_of(seen[3].1.varint().expect("varint")), i64::MIN);
        assert_eq!(i64_of(seen[4].1.varint().expect("varint")), i64::MAX);
        assert_eq!(seen[5], (6, Value::Varint(1)));
        assert_eq!(seen[6], (7, Value::Varint(0)));
        assert_eq!(seen[7].1.bytes().expect("bytes").as_ref(), b"raw");
        let text = text_of(&seen[8].1).expect("text").expect("utf8");
        assert_eq!(text.as_str(), "session-id");
        assert_eq!(seen.len(), 9);
    }

    #[test]
    fn fixed_fields_round_trip() {
        let buf = put(|enc| {
            let at = enc.len();
            enc.u64(1, 0);
            // Two raw fixed fields, the way a caller writes them.
            enc.out.push(2 << WIRE_BITS | Wire::Fixed64 as u8);
            enc.out
                .extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
            enc.out.push(3 << WIRE_BITS | Wire::Fixed32 as u8);
            enc.out.extend_from_slice(&0x0a0b_0c0du32.to_le_bytes());
            assert!(enc.len() > at);
        });
        let seen = fields(&buf);
        assert_eq!(seen[1], (2, Value::Fixed64(0x0102_0304_0506_0708)));
        assert_eq!(seen[2], (3, Value::Fixed32(0x0a0b_0c0d)));
    }

    #[test]
    fn nested_message_round_trips_and_patches_its_length() {
        let buf = put(|enc| {
            enc.message(1, |inner| {
                inner.u64(1, 7);
                inner.str(2, "part");
            });
            enc.u64(2, 9);
        });
        let seen = fields(&buf);
        let nested = seen[0].1.bytes().expect("nested").clone();
        let inner = fields(&nested);
        assert_eq!(inner[0], (1, Value::Varint(7)));
        assert_eq!(
            text_of(&inner[1].1).expect("text").expect("utf8").as_str(),
            "part"
        );
        assert_eq!(seen[1], (2, Value::Varint(9)));
    }

    #[test]
    fn nested_message_over_127_bytes_still_decodes() {
        let long = "x".repeat(300);
        let buf = put(|enc| enc.message(1, |inner| inner.str(1, &long)));
        let seen = fields(&buf);
        let nested = seen[0].1.bytes().expect("nested").clone();
        let inner = fields(&nested);
        assert_eq!(
            text_of(&inner[0].1).expect("text").expect("utf8").as_str(),
            long
        );
    }

    #[test]
    fn unknown_fields_are_skipped_with_the_known_ones_kept() {
        let buf = put(|enc| {
            enc.u64(1, 1);
            enc.bytes(40, b"unknown to the reader");
            enc.u64(90, 2);
            enc.str(2, "keep");
        });
        let mut kept = Vec::new();
        let mut decoder = Decoder::new(&buf);
        while let Some((field, value)) = decoder.next_field().expect("decode") {
            match field {
                1 | 2 => kept.push((field, value)),
                _ => {}
            }
        }
        assert_eq!(kept[0], (1, Value::Varint(1)));
        let text = text_of(&kept[1].1).expect("text").expect("utf8");
        assert_eq!(text.as_str(), "keep");
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn overlong_varint_is_an_error() {
        let mut raw = vec![0x80u8; MAX_VARINT_BYTES + 2];
        raw.push(0);
        let buf = Bytes::from(raw);
        let mut decoder = Decoder::new(&buf);
        let error = decoder.next_field().expect_err("overlong varint");
        assert_eq!(error.kind, DecodeErrorKind::Varint);
        assert_eq!(error.offset, 0);
    }

    #[test]
    fn varint_cut_short_is_an_error() {
        let buf = Bytes::from_static(&[0x08, 0x80]);
        let mut decoder = Decoder::new(&buf);
        let error = decoder.next_field().expect_err("cut varint");
        assert_eq!(error.kind, DecodeErrorKind::Varint);
        assert_eq!(error.offset, 1, "the value that was cut short");
    }

    #[test]
    fn bad_wire_type_is_an_error() {
        for wire in [3u8, 4, 6, 7] {
            let buf = Bytes::from(vec![(1 << WIRE_BITS) | wire]);
            let mut decoder = Decoder::new(&buf);
            let error = decoder.next_field().expect_err("bad wire");
            assert_eq!(error.kind, DecodeErrorKind::Wire, "wire {wire}");
            assert_eq!(error.offset, 0);
        }
    }

    #[test]
    fn length_past_the_end_is_an_error() {
        let buf = Bytes::from_static(&[0x0a, 0x05, b'a']);
        let mut decoder = Decoder::new(&buf);
        let error = decoder.next_field().expect_err("length past the end");
        assert_eq!(error.kind, DecodeErrorKind::Length);
    }

    #[test]
    fn huge_length_is_an_error_not_an_allocation() {
        let mut raw = vec![1 << WIRE_BITS | Wire::Len as u8];
        let mut head = [0u8; MAX_VARINT_BYTES];
        let mut used = 0;
        put_varint(&mut head, &mut used, u64::MAX);
        raw.extend_from_slice(&head[..used]);
        let buf = Bytes::from(raw);
        let mut decoder = Decoder::new(&buf);
        let error = decoder.next_field().expect_err("huge length");
        assert_eq!(error.kind, DecodeErrorKind::Length);
    }

    #[test]
    fn text_checks_utf8() {
        let buf = put(|enc| enc.bytes(1, &[0xff, 0xfe]));
        let seen = fields(&buf);
        let bad = seen[0].1.bytes().expect("bytes").clone();
        assert!(Text::new(bad).is_err(), "invalid UTF-8 is rejected");
    }

    #[test]
    fn slices_share_the_input() {
        let buf = put(|enc| {
            enc.str(1, "abcdefgh");
            enc.str(2, "ijklmnop");
        });
        let seen = fields(&buf);
        let first = seen[0].1.bytes().expect("bytes");
        let second = seen[1].1.bytes().expect("bytes");
        let base = buf.as_ptr() as usize;
        assert_eq!(first.as_ptr() as usize - base, 2, "the first value");
        assert_eq!(
            second.as_ptr() as usize - base,
            2 + first.len() + 2,
            "the second value, in place"
        );
    }

    #[test]
    fn encoder_reuses_room_in_the_buffer() {
        let mut out = Vec::with_capacity(1024);
        let before = out.as_ptr();
        for _ in 0..16 {
            out.clear();
            Encoder::new(&mut out).u64(1, 7);
        }
        assert_eq!(out.as_ptr(), before, "no reallocation");
    }

    /// A fixed-seed xorshift generator: small, repeatable, no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        let valid = put(|enc| {
            enc.u64(1, 300);
            enc.i64(2, -7);
            enc.str(3, "a session id");
            enc.bytes(4, b"\x00\x01\x02");
            enc.message(5, |inner| {
                inner.u64(1, 1);
                inner.str(2, "nested");
            });
        });
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..20_000 {
            let mut raw = valid.to_vec();
            match rng.below(3) {
                0 => {
                    let at = rng.below(raw.len());
                    raw[at] ^= 1 << rng.below(8);
                }
                1 => {
                    let at = rng.below(raw.len());
                    raw.truncate(at);
                }
                _ => {
                    let at = rng.below(raw.len() + 1);
                    raw.insert(at, rng.next() as u8);
                }
            }
            let buf = Bytes::from(raw);
            let mut decoder = Decoder::new(&buf);
            loop {
                match decoder.next_field() {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        }
    }

    #[test]
    fn random_bytes_never_panic_on_any_input() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for len in 0..64 {
            for _ in 0..200 {
                let raw: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                let buf = Bytes::from(raw);
                let mut decoder = Decoder::new(&buf);
                while let Ok(Some(_)) = decoder.next_field() {}
            }
        }
    }
}
