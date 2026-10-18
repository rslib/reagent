//! Splits `bytes::Bytes` chunks into SSE lines without copying them.

use std::mem;

/// Splits a byte stream into lines, borrowing complete lines from the chunk.
///
/// A line split across two chunks is assembled in an internal buffer. A line
/// lives until the next call to [`Lines::next_line`].
#[derive(Debug, Default)]
pub struct SseSplitter {
    carry: Vec<u8>,
    completed: Vec<u8>,
}

impl SseSplitter {
    /// Creates an empty splitter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one chunk and returns a cursor over the lines it completes.
    pub fn push<'a>(&'a mut self, chunk: &'a [u8]) -> Lines<'a> {
        Lines {
            splitter: self,
            chunk,
            pos: 0,
        }
    }
}

/// Lines produced by one [`SseSplitter::push`] call.
#[derive(Debug)]
pub struct Lines<'a> {
    splitter: &'a mut SseSplitter,
    chunk: &'a [u8],
    pos: usize,
}

impl Lines<'_> {
    /// Returns the next complete line without its terminator.
    pub fn next_line(&mut self) -> Option<&[u8]> {
        let chunk: &[u8] = self.chunk;

        if !self.splitter.carry.is_empty() {
            let mut line = mem::take(&mut self.splitter.carry);
            let rest = &chunk[self.pos..];
            match rest.iter().position(|&b| b == b'\n') {
                Some(nl) => {
                    line.extend_from_slice(&rest[..nl]);
                    self.pos += nl + 1;
                }
                None => {
                    line.extend_from_slice(rest);
                    self.pos = chunk.len();
                    self.splitter.carry = line;
                    return None;
                }
            }
            self.splitter.completed = line;
            return Some(trim_cr(&self.splitter.completed));
        }

        let rest = &chunk[self.pos..];
        match rest.iter().position(|&b| b == b'\n') {
            Some(nl) => {
                let line = &rest[..nl];
                self.pos += nl + 1;
                Some(trim_cr(line))
            }
            None => {
                if !rest.is_empty() {
                    self.splitter.carry.extend_from_slice(rest);
                }
                self.pos = chunk.len();
                None
            }
        }
    }
}

fn trim_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((b'\r', rest)) => rest,
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn collect(splitter: &mut SseSplitter, chunk: &Bytes) -> Vec<Vec<u8>> {
        let mut lines = splitter.push(chunk);
        let mut out = Vec::new();
        while let Some(line) = lines.next_line() {
            out.push(line.to_vec());
        }
        out
    }

    #[test]
    fn splits_lines_inside_one_chunk() {
        let mut splitter = SseSplitter::new();
        let chunk = Bytes::from_static(b"event: a\ndata: one\n\n");
        assert_eq!(
            collect(&mut splitter, &chunk),
            vec![b"event: a".to_vec(), b"data: one".to_vec(), Vec::new()]
        );
    }

    #[test]
    fn carries_a_line_split_across_chunks() {
        let mut splitter = SseSplitter::new();
        assert!(collect(&mut splitter, &Bytes::from_static(b"event: ")).is_empty());
        assert_eq!(
            collect(&mut splitter, &Bytes::from_static(b"a\nda")),
            vec![b"event: a".to_vec()]
        );
        let lines = collect(&mut splitter, &Bytes::from_static(b"ta: one\n"));
        assert_eq!(lines, vec![b"data: one".to_vec()]);
    }

    #[test]
    fn strips_a_trailing_carriage_return() {
        let mut splitter = SseSplitter::new();
        let chunk = Bytes::from_static(b"data: one\r\ndata: two\r\n");
        assert_eq!(
            collect(&mut splitter, &chunk),
            vec![b"data: one".to_vec(), b"data: two".to_vec()]
        );
    }

    #[test]
    fn keeps_an_incomplete_tail_for_the_next_chunk() {
        let mut splitter = SseSplitter::new();
        assert!(collect(&mut splitter, &Bytes::from_static(b"data: par")).is_empty());
        assert!(collect(&mut splitter, &Bytes::from_static(b"tial")).is_empty());
        let lines = collect(&mut splitter, &Bytes::from_static(b"\n"));
        assert_eq!(lines, vec![b"data: partial".to_vec()]);
    }

    #[test]
    fn a_complete_line_borrows_from_the_chunk() {
        let mut splitter = SseSplitter::new();
        let chunk = Bytes::from_static(b"data: one\ndata: two\n");
        let base = chunk.as_ptr() as usize;
        let mut lines = splitter.push(&chunk);
        let first = lines.next_line().unwrap();
        assert_eq!(first, b"data: one");
        assert_eq!(first.as_ptr() as usize - base, 0);
        let second = lines.next_line().unwrap();
        assert_eq!(second, b"data: two");
        assert_eq!(second.as_ptr() as usize - base, 10);
    }

    #[test]
    fn an_empty_chunk_yields_nothing() {
        let mut splitter = SseSplitter::new();
        assert!(collect(&mut splitter, &Bytes::new()).is_empty());
    }
}
