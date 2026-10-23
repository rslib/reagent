//! `read`: stream a text file, numbering each line from an offset.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use reagent_agent::tool::{Effects, Identity, Seen, Tool, ToolOutput, parse_args};
use reagent_ai::{Text, ToolSpec};

use crate::resolve;

/// Bytes read from the start of a file to decide whether it is binary.
const BINARY_PROBE: usize = 8 * 1024;
/// Bytes buffered around the file: skipping `offset` lines costs one read per
/// buffer, not one per 8 KB.
const READ_BUF: usize = 64 * 1024;
/// Each line is cut at this many bytes, with a trailing mark.
const LINE_CAP: usize = 2000;
const DEFAULT_LIMIT: usize = 2000;
/// The most bytes one read returns, so one large file does not fill the
/// context of every later turn.
const BYTE_CAP: usize = 32 * 1024;

/// Arguments for one `read` call.
#[derive(Debug, Deserialize)]
pub struct Args<'a> {
    #[serde(borrow)]
    path: Cow<'a, str>,
    offset: Option<usize>,
    limit: Option<usize>,
}

/// Reads a slice of a file's lines, numbered from `offset`.
#[derive(Debug)]
pub struct Read {
    spec: ToolSpec,
    working_dir: PathBuf,
}

impl Read {
    /// Builds the tool for the given working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            spec: spec(),
            working_dir: working_dir.into(),
        }
    }

    async fn run_inner(&self, args: &Args<'_>) -> Result<(String, Option<Seen>), String> {
        let offset = args.offset.unwrap_or(1).max(1) as u64;
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 {
            return Err("invalid arguments: limit must be at least 1".into());
        }
        let path = resolve(&self.working_dir, &args.path)?;
        let failed = |error: io::Error| format!("cannot read {}: {error}", path.display());

        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|error| open_error(&path, &error))?;
        // The identity, the size and the modified time come from the open
        // handle's own metadata, so they name the file this read actually saw.
        // On unix the identity is the device and inode already in the
        // metadata, so it costs no syscall and no allocation.
        let metadata = file.metadata().await.map_err(failed)?;
        let key = Identity::new(&metadata, &path).await;
        let size = metadata.len();
        let modified = metadata.modified().ok();
        let mut probe = [0_u8; BINARY_PROBE];
        let probe_len = file.read(&mut probe).await.map_err(failed)?;
        if probe[..probe_len].contains(&0) {
            return Err(format!(
                "{} is a binary file; read only text files",
                path.display()
            ));
        }
        // The probe bytes are the start of the file, so the reader takes them
        // from the slice already in hand instead of seeking back for them.
        let mut feed = LineFeed::new(reader(&probe[..probe_len], file));

        let mut number: u64 = 1;
        while number < offset {
            if !feed.skip().await.map_err(failed)? {
                break;
            }
            number += 1;
        }

        let mut out = String::with_capacity(out_capacity(size));
        let mut printed = 0;
        while printed < limit && out.len() < BYTE_CAP {
            if !feed.push_next(&mut out, number).await.map_err(failed)? {
                break;
            }
            number += 1;
            printed += 1;
        }
        if printed == 0 {
            return Ok((
                match number - 1 {
                    0 => format!("{} is empty", path.display()),
                    lines => format!(
                        "{} has {lines} line(s); offset={offset} is past the end",
                        path.display()
                    ),
                },
                None,
            ));
        }
        if feed.has_more().await.map_err(failed)? {
            let rest = size.saturating_sub(feed.consumed);
            out.push_str(&format!("[{rest} more bytes; use offset={number}]\n"));
        }
        let seen = Seen {
            path: Text::from(args.path.as_ref()),
            key,
            start: offset,
            end: number - 1,
            size,
            modified,
        };
        Ok((out, Some(seen)))
    }
}

impl Tool for Read {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn main_arg(&self) -> Option<&'static str> {
        Some("path")
    }

    fn effects(&self, args: &str) -> Effects {
        let Ok(arguments) = parse_args::<Args<'_>>(args) else {
            return Effects::default();
        };
        let Ok(path) = resolve(&self.working_dir, &arguments.path) else {
            return Effects::default();
        };
        Effects {
            reads: vec![path],
            ..Effects::default()
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    fn run<'a>(
        &'a self,
        args: &'a str,
        cancel: &'a CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let arguments = match parse_args::<Args<'a>>(args) {
                Ok(arguments) => arguments,
                Err(output) => return output,
            };
            // Cancel first: an unbiased select let a fast read win over a
            // token that was already cancelled.
            tokio::select! {
                biased;
                () = cancel.cancelled() => ToolOutput::error("cancelled"),
                result = self.run_inner(&arguments) => match result {
                    Ok((text, seen)) => match seen {
                        Some(seen) => ToolOutput::ok(text).with_seen(seen),
                        None => ToolOutput::ok(text),
                    },
                    Err(message) => ToolOutput::error(message),
                },
            }
        })
    }
}

/// The reader around the file, buffered so that skipping lines reads the file
/// in `READ_BUF` pieces.
fn reader<R: AsyncRead + Unpin>(probe: &[u8], file: R) -> BufReader<tokio::io::Chain<&[u8], R>> {
    BufReader::with_capacity(READ_BUF, probe.chain(file))
}

/// Streams a file line by line from the reader's own buffer, keeping at most
/// `LINE_CAP` bytes of a line even when the line is huge.
struct LineFeed<R> {
    reader: R,
    consumed: u64,
    /// The line being read, emptied and refilled for every line so that a read
    /// costs no allocation per line.
    line: Line,
}

impl<R: AsyncBufRead + Unpin> LineFeed<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            consumed: 0,
            line: Line::default(),
        }
    }

    /// Reads the next line and appends it to `out` as `{number}\t{text}\n`.
    /// Returns false at the end of the stream.
    async fn push_next(&mut self, out: &mut String, number: u64) -> io::Result<bool> {
        self.line.reset();
        if !self.scan(true).await? {
            return Ok(false);
        }
        self.line.push_to(out, number);
        Ok(true)
    }

    /// Skips the next line. Returns false at the end of the stream.
    async fn skip(&mut self) -> io::Result<bool> {
        self.scan(false).await
    }

    async fn has_more(&mut self) -> io::Result<bool> {
        Ok(!self.reader.fill_buf().await?.is_empty())
    }

    /// Consumes one line, copying it into the line buffer when `keep`. Returns
    /// false when the stream had already ended.
    async fn scan(&mut self, keep: bool) -> io::Result<bool> {
        let mut any = false;
        loop {
            let buf = self.reader.fill_buf().await?;
            if buf.is_empty() {
                return Ok(any);
            }
            any = true;
            let newline = buf.iter().position(|&byte| byte == b'\n');
            let part = &buf[..newline.unwrap_or(buf.len())];
            if keep {
                self.line.adopt(part);
            }
            let used = newline.map_or(buf.len(), |at| at + 1);
            self.reader.consume(used);
            self.consumed += used as u64;
            if newline.is_some() {
                return Ok(true);
            }
        }
    }
}

/// One line, capped at `LINE_CAP` bytes.
#[derive(Default)]
struct Line {
    bytes: Vec<u8>,
    cut: bool,
}

impl Line {
    /// Empties the buffer for the next line, keeping its allocation.
    fn reset(&mut self) {
        self.bytes.clear();
        self.cut = false;
    }

    fn adopt(&mut self, part: &[u8]) {
        if self.cut {
            return;
        }
        let space = LINE_CAP - self.bytes.len();
        if part.len() > space {
            self.bytes.extend_from_slice(&part[..space]);
            self.cut = true;
        } else {
            self.bytes.extend_from_slice(part);
        }
    }

    /// Appends `{number}\t{text}\n`, without a CRLF file's `\r`.
    fn push_to(&self, out: &mut String, number: u64) {
        let bytes = match self.bytes.strip_suffix(b"\r") {
            Some(bytes) if !self.cut => bytes,
            _ => &self.bytes,
        };
        // The number goes into `out` directly, without a String of its own.
        let _ = write!(out, "{number}\t");
        out.push_str(&String::from_utf8_lossy(bytes));
        if self.cut {
            out.push_str("[line cut; more]");
        }
        out.push('\n');
    }
}

/// The starting capacity of the output: the file size bounded by the byte
/// cap, plus room for the line numbers and for one line over the cap so that a
/// normal read does not grow the buffer again.
fn out_capacity(size: u64) -> usize {
    size.min(BYTE_CAP as u64) as usize + LINE_CAP + 32
}

fn open_error(path: &Path, error: &io::Error) -> String {
    let reason = match error.kind() {
        io::ErrorKind::NotFound => "the file does not exist",
        io::ErrorKind::PermissionDenied => "permission denied",
        _ => "i/o error",
    };
    format!("cannot open {}: {reason}: {error}", path.display())
}

/// The name of this tool.
pub const NAME: &str = "read";

/// Builds the tool's spec.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: NAME.into(),
        description: concat!(
            "Read a text file. Each line is printed as `N<TAB>text`. ",
            "At most `limit` lines (default 2000) or 32 KB are shown; use `offset` to continue. ",
            "Lines over 2000 bytes are cut."
        )
        .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path of the file." },
                "offset": { "type": "integer", "minimum": 1, "description": "1-based line to start at; default 1." },
                "limit": { "type": "integer", "minimum": 1, "description": "Maximum lines to print; default 2000." }
            },
            "required": ["path"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll};

    use super::*;

    fn tool(tmp: &tempfile::TempDir) -> Read {
        Read::new(tmp.path())
    }

    async fn write(tmp: &tempfile::TempDir, name: &str, contents: &str) -> PathBuf {
        let path = tmp.path().join(name);
        tokio::fs::write(&path, contents).await.unwrap();
        path
    }

    #[tokio::test]
    async fn read_byte_cap_stops_and_names_the_next_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let line = "x".repeat(99);
        let text = format!("{line}\n").repeat(1000);
        write(&tmp, "big.txt", &text).await;
        let output = tool(&tmp)
            .run(r#"{"path":"big.txt"}"#, &CancellationToken::new())
            .await;
        assert!(!output.is_error, "{}", output.content);
        let content = output.content.as_ref();
        assert!(content.len() <= BYTE_CAP + 200, "{} bytes", content.len());
        let last = content.lines().last().unwrap_or_default();
        assert!(last.contains("more bytes; use offset="), "{last}");
        let next: usize = last
            .rsplit("offset=")
            .next()
            .and_then(|rest| rest.trim_end_matches(']').parse().ok())
            .expect("an offset");
        let shown = content.lines().count() - 1;
        assert_eq!(next, shown + 1);
    }

    #[tokio::test]
    async fn read_byte_cap_leaves_a_small_file_whole() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nb\n").await;
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt"}"#, &CancellationToken::new())
            .await;
        assert_eq!(output.content.as_ref(), "1\ta\n2\tb\n");
    }

    #[tokio::test]
    async fn reads_from_the_start_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nb\nc\n").await;
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt"}"#, &CancellationToken::new())
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content.as_ref(), "1\ta\n2\tb\n3\tc\n");
    }

    #[tokio::test]
    async fn offset_skips_lines_and_numbers_from_there() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nb\nc\nd\ne\n").await;
        let output = tool(&tmp)
            .run(
                r#"{"path":"f.txt","offset":3,"limit":2}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            output.content.as_ref(),
            "3\tc\n4\td\n[2 more bytes; use offset=5]\n"
        );
    }

    #[tokio::test]
    async fn offset_past_the_end_reports_nothing_after() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nb\nc\n").await;
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt","offset":10}"#, &CancellationToken::new())
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            output
                .content
                .ends_with("f.txt has 3 line(s); offset=10 is past the end"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn offset_just_past_the_end_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nb\nc\n").await;
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt","offset":4}"#, &CancellationToken::new())
            .await;
        assert!(
            output
                .content
                .ends_with("has 3 line(s); offset=4 is past the end"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn an_empty_file_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "").await;
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt"}"#, &CancellationToken::new())
            .await;
        assert!(
            output.content.ends_with("f.txt is empty"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn a_crlf_file_is_printed_without_cr() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\r\nb\r\n").await;
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt"}"#, &CancellationToken::new())
            .await;
        assert_eq!(output.content.as_ref(), "1\ta\n2\tb\n");
    }

    #[tokio::test]
    async fn a_last_line_without_a_newline_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nb").await;
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt"}"#, &CancellationToken::new())
            .await;
        assert_eq!(output.content.as_ref(), "1\ta\n2\tb\n");
    }

    #[tokio::test]
    async fn the_limit_stops_the_read_on_a_huge_file() {
        use tokio::io::AsyncWriteExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.txt");
        let mut file = tokio::fs::File::create(&path).await.unwrap();
        file.write_all(b"first\n").await.unwrap();
        file.write_all(&b"xx\n".repeat(5000)).await.unwrap();
        file.set_len(1 << 40).await.unwrap();
        drop(file);
        let output = tool(&tmp)
            .run(r#"{"path":"big.txt","limit":1}"#, &CancellationToken::new())
            .await;
        let rest = (1_u64 << 40) - 6;
        assert_eq!(
            output.content.as_ref(),
            format!("1\tfirst\n[{rest} more bytes; use offset=2]\n")
        );
    }

    #[tokio::test]
    async fn cancel_stops_a_read() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\n").await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let output = tool(&tmp).run(r#"{"path":"f.txt"}"#, &cancel).await;
        assert!(output.is_error);
        assert_eq!(output.content.as_ref(), "cancelled");
    }

    #[tokio::test]
    async fn limit_stops_and_reports_more_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let mut contents = String::new();
        for i in 1..=10 {
            contents.push_str(&format!("line {i}\n"));
        }
        write(&tmp, "f.txt", &contents).await;
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt","limit":3}"#, &CancellationToken::new())
            .await;
        assert!(!output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert_eq!(
            text,
            "1\tline 1\n2\tline 2\n3\tline 3\n[50 more bytes; use offset=4]\n"
        );
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tool(&tmp)
            .run(r#"{"path":"nope.txt"}"#, &CancellationToken::new())
            .await;
        assert!(output.is_error);
        assert!(output.content.contains("cannot open"), "{}", output.content);
    }

    #[tokio::test]
    async fn bad_args_are_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tool(&tmp).run("not json", &CancellationToken::new()).await;
        assert!(output.is_error);
        assert!(output.content.starts_with("invalid arguments"));
    }

    #[tokio::test]
    async fn long_lines_are_cut_with_a_mark() {
        let tmp = tempfile::tempdir().unwrap();
        let long = "x".repeat(LINE_CAP + 50);
        write(&tmp, "f.txt", &format!("{long}\nz\n")).await;
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt","limit":1}"#, &CancellationToken::new())
            .await;
        assert!(!output.is_error, "{}", output.content);
        let text = output.content.as_ref();
        assert!(text.starts_with("1\t"));
        let body = &text["1\t".len()..];
        assert!(
            body.starts_with(&"x".repeat(LINE_CAP)),
            "line body not capped"
        );
        assert!(text.contains("[line cut; more]"), "{}", text);
    }

    #[tokio::test]
    async fn binary_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes = [0u8, 1, 2, 3, 4];
        tokio::fs::write(tmp.path().join("bin.dat"), bytes)
            .await
            .unwrap();
        let output = tool(&tmp)
            .run(r#"{"path":"bin.dat"}"#, &CancellationToken::new())
            .await;
        assert!(output.is_error);
        assert!(output.content.contains("binary"), "{}", output.content);
    }

    #[tokio::test]
    async fn reads_a_100_mb_sparse_file_near_the_end() {
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.txt");
        {
            let mut file = tokio::fs::File::create(&path).await.unwrap();
            // 2000 short text lines so the binary probe (first 8 KiB) sees
            // no NUL and the line count is meaningful.
            for i in 0..2000 {
                file.write_all(format!("line {i}\n").as_bytes())
                    .await
                    .unwrap();
            }
            // Sparse hole of 100 MB of NULs; it forms one huge line that the
            // reader must stream, not hold in memory.
            file.set_len(100 * 1024 * 1024).await.unwrap();
            file.seek(std::io::SeekFrom::Start(100 * 1024 * 1024 - 1))
                .await
                .unwrap();
            file.write_all(b"\nlast line\n").await.unwrap();
        }
        let start = std::time::Instant::now();
        let output = tool(&tmp)
            .run(
                r#"{"path":"big.txt","offset":2002,"limit":1}"#,
                &CancellationToken::new(),
            )
            .await;
        let elapsed = start.elapsed();
        assert!(!output.is_error, "{}", output.content);
        // About 2 s warm; a cold cache or a busy machine can take 7-18 s. A
        // reader that loads or rescans the file is far slower than 30 s.
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "took too long: {elapsed:?}"
        );
        assert_eq!(
            output.content.as_ref(),
            "2002\tlast line\n",
            "expected the last line near the end of the sparse file"
        );
    }

    /// A file that counts the reads which reach it.
    struct Counted {
        bytes: Vec<u8>,
        at: usize,
        reads: Rc<Cell<usize>>,
    }

    impl AsyncRead for Counted {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            self.reads.set(self.reads.get() + 1);
            let take = (self.bytes.len() - self.at).min(buf.remaining());
            let end = self.at + take;
            buf.put_slice(&self.bytes[self.at..end]);
            self.at = end;
            Poll::Ready(Ok(()))
        }
    }

    /// Skipping lines must read the file in `READ_BUF` pieces: the 8 KB
    /// default would cost eight times as many read hops on a network file
    /// system. The reads that reach the file are counted, not the lines.
    #[tokio::test]
    async fn skipping_lines_reads_the_file_in_large_buffers() {
        let line = format!("{}\n", "x".repeat(99));
        let text = line.repeat(1000);
        let size = text.len();
        let reads = Rc::new(Cell::new(0));
        let file = Counted {
            bytes: text.into_bytes(),
            at: 0,
            reads: Rc::clone(&reads),
        };
        let mut feed = LineFeed::new(reader(b"", file));
        let mut lines = 0;
        while feed.skip().await.unwrap() {
            lines += 1;
        }
        assert_eq!(lines, 1000);
        let most = size / READ_BUF + 2;
        eprintln!("reads: {} for {size} bytes, at most {most}", reads.get());
        assert!(
            reads.get() <= most,
            "{} reads for {size} bytes, over {most}",
            reads.get()
        );
    }

    #[tokio::test]
    async fn reread_a_plain_read_records_the_range_shown() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp, "f.txt", "a\nb\nc\n").await;
        let metadata = tokio::fs::metadata(&path).await.unwrap();
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt"}"#, &CancellationToken::new())
            .await;
        let seen = output.seen.expect("a read reports the lines it showed");
        assert_eq!(seen.path.as_ref(), "f.txt");
        assert_eq!(seen.key, Identity::new(&metadata, &path).await);
        assert_eq!((seen.start, seen.end), (1, 3));
    }

    #[tokio::test]
    async fn reread_a_limit_records_the_last_line_shown() {
        let tmp = tempfile::tempdir().unwrap();
        let mut contents = String::new();
        for i in 1..=10 {
            contents.push_str(&format!("line {i}\n"));
        }
        write(&tmp, "f.txt", &contents).await;
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt","limit":3}"#, &CancellationToken::new())
            .await;
        let seen = output.seen.expect("a read reports the lines it showed");
        assert_eq!((seen.start, seen.end), (1, 3));
    }

    #[tokio::test]
    async fn reread_the_byte_cap_records_the_last_line_shown() {
        let tmp = tempfile::tempdir().unwrap();
        let line = "x".repeat(99);
        let text = format!("{line}\n").repeat(1000);
        write(&tmp, "big.txt", &text).await;
        let output = tool(&tmp)
            .run(r#"{"path":"big.txt"}"#, &CancellationToken::new())
            .await;
        assert!(
            output.content.contains("more bytes; use offset="),
            "{}",
            output.content
        );
        let seen = output.seen.expect("a read reports the lines it showed");
        // Every line but the trailing `[N more bytes]` marker was shown.
        let shown = output.content.lines().count() - 1;
        assert_eq!((seen.start, seen.end), (1, shown as u64));
    }

    #[tokio::test]
    async fn reread_the_range_matches_the_file_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp, "f.txt", "a\nb\n").await;
        let metadata = tokio::fs::metadata(&path).await.unwrap();
        let output = tool(&tmp)
            .run(r#"{"path":"f.txt"}"#, &CancellationToken::new())
            .await;
        let seen = output.seen.expect("a read reports the lines it showed");
        assert_eq!(seen.size, metadata.len());
        assert_eq!(seen.modified, metadata.modified().ok());
    }

    #[tokio::test]
    async fn reread_an_empty_range_records_no_seen() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp, "f.txt", "a\nb\nc\n").await;
        write(&tmp, "empty.txt", "").await;
        let past = tool(&tmp)
            .run(r#"{"path":"f.txt","offset":10}"#, &CancellationToken::new())
            .await;
        assert_eq!(past.seen, None, "a range past the end shows no lines");
        let empty = tool(&tmp)
            .run(r#"{"path":"empty.txt"}"#, &CancellationToken::new())
            .await;
        assert_eq!(empty.seen, None, "an empty file shows no lines");
    }

    #[test]
    fn read_effects_are_the_joined_path() {
        let read = Read::new("/work");
        assert_eq!(read.main_arg(), Some("path"));
        let effects = read.effects(r#"{"path":"a/b.txt"}"#);
        assert_eq!(effects.reads, vec![PathBuf::from("/work/a/b.txt")]);
        assert!(effects.writes.is_empty());
    }

    #[test]
    fn read_bad_args_have_no_effects() {
        let read = Read::new("/work");
        assert_eq!(read.effects("not json"), Effects::default());
        assert_eq!(read.effects(r#"{"path":""}"#), Effects::default());
    }
}
