//! Line splitting over one reused buffer.

use std::fmt;
use std::io;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;

/// The longest line `Lines` yields, without its `\n`.
pub const MAX_LINE: usize = 1024 * 1024;

const CHUNK: usize = 8 * 1024;

/// A line longer than `MAX_LINE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLong {
    pub limit: usize,
}

impl fmt::Display for TooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line over {} bytes", self.limit)
    }
}

impl std::error::Error for TooLong {}

/// Lines read from one buffer that grows once and is then reused.
#[derive(Debug)]
pub struct Lines {
    buf: Vec<u8>,
    written: usize,
    pos: usize,
    searched: usize,
    skip: bool,
    limit: usize,
    /// Whether the buffer grew past [`MAX_LINE`] and is empty now, so the next
    /// call frees it: the slice of the last line borrows from it until then.
    release: bool,
}

impl Default for Lines {
    fn default() -> Self {
        Self::with_limit(MAX_LINE)
    }
}

impl Lines {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lines up to `limit` bytes, for a reader whose peer sends more than
    /// [`MAX_LINE`] in one line.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            written: 0,
            pos: 0,
            searched: 0,
            skip: false,
            limit,
            release: false,
        }
    }

    /// Bytes held for a line that has no `\n` yet.
    pub fn len(&self) -> usize {
        self.written - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The buffer size; it settles once the lines stop growing.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// The spare capacity to read into; the bytes read are handed to `advance`.
    pub fn spare(&mut self) -> &mut [u8] {
        self.drop_big();
        self.compact();
        if self.written == self.buf.len() {
            self.grow(CHUNK.max(self.buf.len()));
        }
        &mut self.buf[self.written..]
    }

    /// Counts `n` bytes of the last `spare` slice as read.
    pub fn advance(&mut self, n: usize) {
        self.written = self.written.saturating_add(n).min(self.buf.len());
    }

    /// Adds bytes that were not read through `spare`, such as a test's.
    pub fn extend(&mut self, bytes: &[u8]) {
        self.drop_big();
        self.compact();
        let need = self.written + bytes.len();
        if need > self.buf.len() {
            let room = need - self.buf.len();
            self.grow(CHUNK.max(self.buf.len()).max(room));
        }
        self.buf[self.written..need].copy_from_slice(bytes);
        self.written = need;
    }

    /// Reads once into the spare capacity. `0` means the reader is at its end.
    pub async fn fill<R: AsyncRead + Unpin>(&mut self, reader: &mut R) -> io::Result<usize> {
        let read = reader.read(self.spare()).await?;
        self.advance(read);
        Ok(read)
    }

    /// The next complete line without its `\n`, or `None` when one is missing.
    ///
    /// After a `TooLong` error the bytes up to the next `\n` are dropped and the
    /// lines after it are yielded.
    #[expect(
        clippy::should_implement_trait,
        reason = "a line can be too long, which an Iterator cannot say"
    )]
    pub fn next(&mut self) -> Option<Result<&[u8], TooLong>> {
        self.drop_big();
        loop {
            if self.skip {
                match find(&self.buf[self.searched..self.written]) {
                    Some(at) => {
                        self.searched += at + 1;
                        self.pos = self.searched;
                        self.skip = false;
                        if self.pos == self.written {
                            self.buf_clear();
                        }
                        continue;
                    }
                    None => {
                        self.buf_clear();
                        return None;
                    }
                }
            }
            if let Some(at) = find(&self.buf[self.searched..self.written]) {
                let start = self.pos;
                let end = self.searched + at;
                self.pos = end + 1;
                self.searched = self.pos;
                if end - start > self.limit {
                    if self.pos == self.written {
                        self.buf_clear();
                    }
                    return Some(Err(TooLong { limit: self.limit }));
                }
                if self.pos == self.written {
                    self.buf_clear();
                }
                return Some(Ok(&self.buf[start..end]));
            }
            self.searched = self.written;
            if self.written - self.pos > self.limit {
                self.skip = true;
                self.buf_clear();
                return Some(Err(TooLong { limit: self.limit }));
            }
            return None;
        }
    }

    fn buf_clear(&mut self) {
        self.written = 0;
        self.pos = 0;
        self.searched = 0;
        self.release = self.buf.len() > MAX_LINE;
    }

    /// Frees a buffer that a line over [`MAX_LINE`] grew, once nothing is held
    /// in it, so one large line does not keep its memory.
    fn drop_big(&mut self) {
        if self.release && self.written == self.pos {
            self.buf = Vec::new();
            self.written = 0;
            self.pos = 0;
            self.searched = 0;
            self.release = false;
        }
    }

    fn grow(&mut self, more: usize) {
        let len = self.buf.len().saturating_add(more);
        self.buf.resize(len, 0);
    }

    fn compact(&mut self) {
        if self.pos > 0 {
            self.buf.copy_within(self.pos..self.written, 0);
            self.written -= self.pos;
            self.searched = self.searched.saturating_sub(self.pos);
            self.pos = 0;
        }
    }
}

fn find(hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&byte| byte == b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(lines: &mut Lines) -> Option<&str> {
        lines.next().map(|line| match line {
            Ok(bytes) => std::str::from_utf8(bytes).expect("utf8"),
            Err(err) => panic!("{err}"),
        })
    }

    #[test]
    fn a_line_split_over_three_reads() {
        let mut lines = Lines::new();
        lines.extend(b"ab");
        assert_eq!(line(&mut lines), None);
        assert_eq!(lines.len(), 2);
        lines.extend(b"c");
        assert_eq!(line(&mut lines), None);
        lines.extend(b"d\n");
        assert_eq!(line(&mut lines), Some("abcd"));
        assert_eq!(line(&mut lines), None);
        assert!(lines.is_empty());
    }

    #[test]
    fn two_lines_in_one_read() {
        let mut lines = Lines::new();
        lines.extend(b"one\ntwo\n");
        assert_eq!(line(&mut lines), Some("one"));
        assert_eq!(line(&mut lines), Some("two"));
        assert_eq!(line(&mut lines), None);
    }

    #[test]
    fn a_trailing_partial_line_is_kept() {
        let mut lines = Lines::new();
        lines.extend(b"one\npar");
        assert_eq!(line(&mut lines), Some("one"));
        assert_eq!(line(&mut lines), None);
        lines.extend(b"t\n");
        assert_eq!(line(&mut lines), Some("part"));
    }

    #[test]
    fn an_empty_line_is_a_line() {
        let mut lines = Lines::new();
        lines.extend(b"\n\n");
        assert_eq!(line(&mut lines), Some(""));
        assert_eq!(line(&mut lines), Some(""));
        assert_eq!(line(&mut lines), None);
    }

    #[test]
    fn a_line_that_crosses_the_limit_in_one_read_errors() {
        let mut lines = Lines::new();
        lines.extend(&vec![b'a'; MAX_LINE - 10]);
        assert!(lines.next().is_none());
        let mut tail = vec![b'a'; 50];
        tail.extend_from_slice(b"\nnext\n");
        lines.extend(&tail);
        assert!(matches!(lines.next(), Some(Err(TooLong { .. }))));
        assert_eq!(
            lines.next().map(|line| line.ok().map(<[u8]>::to_vec)),
            Some(Some(b"next".to_vec()))
        );
        assert!(lines.next().is_none());
    }

    #[test]
    fn an_over_long_line_errors() {
        let mut lines = Lines::new();
        lines.extend(&vec![b'a'; MAX_LINE]);
        assert_eq!(line(&mut lines), None);
        lines.extend(b"a");
        assert_eq!(
            lines.next().expect("an error").expect_err("too long"),
            TooLong { limit: MAX_LINE }
        );
        lines.extend(b"rest\nshort\n");
        assert_eq!(line(&mut lines), Some("short"));
        assert_eq!(line(&mut lines), None);
    }

    #[test]
    fn a_reader_with_a_higher_limit_takes_a_line_over_the_default() {
        let mut lines = Lines::with_limit(MAX_LINE * 2);
        let mut buf = vec![b'a'; MAX_LINE + 5];
        buf.push(b'\n');
        lines.extend(&buf);
        assert_eq!(line(&mut lines).map(str::len), Some(MAX_LINE + 5));
        lines.extend(&vec![b'a'; MAX_LINE * 2 + 1]);
        assert_eq!(
            lines.next().expect("an error"),
            Err(TooLong {
                limit: MAX_LINE * 2
            })
        );
    }

    #[test]
    fn the_buffer_of_a_line_over_the_default_limit_is_dropped() {
        let mut lines = Lines::with_limit(MAX_LINE * 4);
        let mut buf = vec![b'a'; MAX_LINE * 2];
        buf.push(b'\n');
        lines.extend(&buf);
        assert_eq!(line(&mut lines).map(str::len), Some(MAX_LINE * 2));
        assert_eq!(line(&mut lines), None);
        assert_eq!(lines.capacity(), 0);

        lines.extend(&vec![b'a'; MAX_LINE * 5]);
        assert!(matches!(lines.next(), Some(Err(TooLong { .. }))));
        assert_eq!(line(&mut lines), None);
        assert_eq!(lines.capacity(), 0, "a line over the limit holds no buffer");
    }

    #[test]
    fn a_line_of_the_limit_passes() {
        let mut lines = Lines::new();
        let mut buf = vec![b'a'; MAX_LINE];
        buf.push(b'\n');
        lines.extend(&buf);
        assert_eq!(line(&mut lines).map(str::len), Some(MAX_LINE));
    }

    #[test]
    fn no_reallocation_after_warm_up() {
        let text = format!("{}\n", "x".repeat(63));
        let mut lines = Lines::new();
        for _ in 0..100 {
            lines.extend(text.as_bytes());
            assert_eq!(line(&mut lines).map(str::len), Some(63));
        }
        let warm = lines.capacity();
        for _ in 0..1_000 {
            lines.extend(text.as_bytes());
            assert_eq!(line(&mut lines).map(str::len), Some(63));
        }
        assert_eq!(lines.capacity(), warm, "no reallocation once warm");
    }

    #[tokio::test]
    async fn fill_reads_into_the_spare_capacity() {
        let mut lines = Lines::new();
        let mut reader: &[u8] = b"one\ntwo\nthree";
        while lines.fill(&mut reader).await.expect("reads") > 0 {}
        assert_eq!(line(&mut lines), Some("one"));
        assert_eq!(line(&mut lines), Some("two"));
        assert_eq!(line(&mut lines), None);
        assert_eq!(lines.len(), 5);
    }
}
