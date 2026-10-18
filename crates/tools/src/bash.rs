//! `bash`: run a shell command with a timeout, capping the output the model
//! sees while spilling the full output to a file when it is large.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use reagent_agent::tool::{Tool, ToolOutput, parse_args};
use reagent_ai::ToolSpec;

use crate::process_group::GroupGuard;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Bytes kept in each of the model-facing head and tail.
const CAP: usize = 8 * 1024;
/// Output up to this size is shown whole and not spilled.
const CUT_AT: u64 = 2 * CAP as u64;
/// The spill file stops growing here, so `yes` cannot fill the disk.
const SPILL_CAP: u64 = 64 * 1024 * 1024;
/// How long to keep reading after the shell ends. A background child that
/// holds the pipe open would otherwise block the call until it exits.
const GRACE: Duration = Duration::from_millis(250);

/// Arguments for one `bash` call.
#[derive(Debug, Deserialize)]
pub struct Args<'a> {
    #[serde(borrow)]
    command: Cow<'a, str>,
    timeout_ms: Option<u64>,
}

/// Why a command ended.
enum Outcome {
    Exited(ExitStatus),
    WaitFailed(io::Error),
    TimedOut(u64),
    Cancelled,
}

impl Outcome {
    fn summary(&self) -> String {
        match self {
            Outcome::Exited(status) => match (status.code(), status.signal()) {
                (Some(code), _) => format!("exit code: {code}"),
                (None, Some(signal)) => format!("killed by signal {signal}"),
                (None, None) => "exit code: unknown".into(),
            },
            Outcome::WaitFailed(error) => format!("cannot wait for the command: {error}"),
            Outcome::TimedOut(ms) => format!("timed out after {ms} ms; the command was killed"),
            Outcome::Cancelled => "cancelled; the command was killed".into(),
        }
    }
}

/// Runs a `bash -c` command (`sh -c` when bash is missing) with a hard
/// timeout and output capping.
#[derive(Debug)]
pub struct Bash {
    spec: ToolSpec,
    working_dir: PathBuf,
}

impl Bash {
    /// Builds the tool for the given working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            spec: spec(),
            working_dir: working_dir.into(),
        }
    }

    async fn run_inner(&self, args: &Args<'_>, cancel: &CancellationToken) -> ToolOutput {
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let mut child = match spawn(&self.working_dir, &args.command) {
            Ok(child) => child,
            Err(error) => return ToolOutput::error(format!("cannot start the command: {error}")),
        };
        let Some(pid) = child.id() else {
            return ToolOutput::error("cannot start the command: no process id");
        };
        let mut group = GroupGuard::new(pid as i32);
        let (outcome, mut collect, pipe_held) =
            run_and_collect(&mut child, &mut group, timeout_ms, cancel).await;

        let mut text = collect.view();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if pipe_held {
            text.push_str("[a background process still holds the output pipe; its later output is not shown]\n");
        }
        text.push_str(&outcome.summary());
        ToolOutput::ok(text)
    }
}

fn spawn(working_dir: &Path, command: &str) -> io::Result<Child> {
    match shell("bash", working_dir, command).spawn() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            shell("sh", working_dir, command).spawn()
        }
        result => result,
    }
}

fn shell(program: &str, working_dir: &Path, command: &str) -> Command {
    let mut shell = Command::new(program);
    shell
        .arg("-c")
        .arg(command)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    shell
}

/// Drains both pipes while it waits for the shell, and kills the group on
/// timeout or cancel. Returns the outcome, the output and whether a pipe was
/// still open when the grace time ran out.
async fn run_and_collect(
    child: &mut Child,
    group: &mut GroupGuard,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> (Outcome, Collect, bool) {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out_buf = [0_u8; 4096];
    let mut err_buf = [0_u8; 4096];
    let mut collect = Collect::default();
    let mut ended: Option<Outcome> = None;
    let mut reaped = false;
    let timer = sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timer);

    loop {
        if reaped && stdout.is_none() && stderr.is_none() {
            break;
        }
        tokio::select! {
            read = read_some(&mut stdout, &mut out_buf), if stdout.is_some() => match read {
                Ok(n) if n > 0 => collect.push(&out_buf[..n]).await,
                _ => stdout = None,
            },
            read = read_some(&mut stderr, &mut err_buf), if stderr.is_some() => match read {
                Ok(n) if n > 0 => collect.push(&err_buf[..n]).await,
                _ => stderr = None,
            },
            status = child.wait(), if !reaped => {
                reaped = true;
                // Background children started with `&` stay alive after a
                // normal exit, and the reaped pid may now be reused.
                group.disarm();
                if ended.is_none() {
                    ended = Some(match status {
                        Ok(status) => Outcome::Exited(status),
                        Err(error) => Outcome::WaitFailed(error),
                    });
                    timer.as_mut().reset(Instant::now() + GRACE);
                }
            }
            () = &mut timer => {
                if ended.is_some() {
                    break;
                }
                group.kill();
                ended = Some(Outcome::TimedOut(timeout_ms));
                timer.as_mut().reset(Instant::now() + GRACE);
            }
            () = cancel.cancelled(), if ended.is_none() => {
                group.kill();
                ended = Some(Outcome::Cancelled);
                timer.as_mut().reset(Instant::now() + GRACE);
            }
        }
    }

    let pipe_held = stdout.is_some() || stderr.is_some();
    collect.flush().await;
    let outcome = ended.unwrap_or(Outcome::Cancelled);
    (outcome, collect, pipe_held)
}

async fn read_some<R>(pipe: &mut Option<R>, buf: &mut [u8]) -> io::Result<usize>
where
    R: AsyncRead + Unpin,
{
    match pipe {
        Some(pipe) => pipe.read(buf).await,
        None => std::future::pending().await,
    }
}

/// The merged output of a command: its first and last `CAP` bytes in RAM and,
/// past `CUT_AT`, the whole stream (up to `SPILL_CAP`) in a file.
#[derive(Default)]
struct Collect {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
    spill: Spill,
}

#[derive(Default)]
enum Spill {
    #[default]
    None,
    Open {
        file: tokio::fs::File,
        path: PathBuf,
        written: u64,
    },
    Failed(String),
}

impl Collect {
    async fn push(&mut self, chunk: &[u8]) {
        let before = self.total;
        self.total += chunk.len() as u64;
        if before <= CUT_AT && self.total > CUT_AT {
            let prefix = self.whole(before);
            self.spill = open_spill(&prefix).await;
        }
        if let Spill::Open { file, written, .. } = &mut self.spill {
            let room = SPILL_CAP.saturating_sub(*written);
            let part = &chunk[..chunk.len().min(room as usize)];
            match file.write_all(part).await {
                Ok(()) => *written += part.len() as u64,
                Err(error) => self.spill = Spill::Failed(error.to_string()),
            }
        }

        if self.head.len() < CAP {
            let take = chunk.len().min(CAP - self.head.len());
            self.head.extend_from_slice(&chunk[..take]);
        }
        self.tail.extend(chunk);
        let overflow = self.tail.len().saturating_sub(CAP);
        self.tail.drain(..overflow);
    }

    /// The first `total` bytes of the stream. Valid only while
    /// `total <= CUT_AT`, when the head and the tail cover every byte.
    fn whole(&self, total: u64) -> Vec<u8> {
        let overlap = (self.head.len() + self.tail.len()).saturating_sub(total as usize);
        let mut bytes = self.head.clone();
        bytes.extend(self.tail.iter().skip(overlap));
        bytes
    }

    async fn flush(&mut self) {
        if let Spill::Open { file, .. } = &mut self.spill {
            let _ = file.flush().await;
        }
    }

    /// The model-facing text: the whole output when small, else the head and
    /// the tail under a line that says where the full output is.
    fn view(&mut self) -> String {
        if self.total <= CUT_AT {
            return String::from_utf8_lossy(&self.whole(self.total)).into_owned();
        }
        let total = self.total;
        let note = match &self.spill {
            Spill::Open { path, written, .. } if *written < total => format!(
                "[output cut: {total} bytes; the first {written} bytes are in {}]",
                path.display()
            ),
            Spill::Open { path, .. } => {
                format!(
                    "[output cut: {total} bytes; full output in {}]",
                    path.display()
                )
            }
            Spill::Failed(error) => {
                format!("[output cut: {total} bytes; cannot save the full output: {error}]")
            }
            Spill::None => format!("[output cut: {total} bytes]"),
        };
        format!(
            "{note}\n{}\n...\n{}",
            head_text(&self.head),
            tail_text(self.tail.make_contiguous())
        )
    }
}

/// Opens a new spill file and writes `prefix`, the output before the cut.
async fn open_spill(prefix: &[u8]) -> Spill {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let folder = std::env::temp_dir().join("reagent-spill");
    let path = folder.join(format!(
        "bash-{}-{}.log",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = async {
        tokio::fs::create_dir_all(&folder).await?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        file.write_all(prefix).await?;
        io::Result::Ok(file)
    }
    .await;
    match result {
        Ok(file) => Spill::Open {
            file,
            path,
            written: prefix.len() as u64,
        },
        Err(error) => Spill::Failed(error.to_string()),
    }
}

/// Decodes the head, dropping a character that the cut split at the end.
fn head_text(bytes: &[u8]) -> Cow<'_, str> {
    let end = match std::str::from_utf8(bytes) {
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        _ => bytes.len(),
    };
    String::from_utf8_lossy(&bytes[..end])
}

/// Decodes the tail, dropping a character that the cut split at the start.
fn tail_text(bytes: &[u8]) -> Cow<'_, str> {
    let start = bytes
        .iter()
        .take(3)
        .take_while(|&&byte| byte & 0xC0 == 0x80)
        .count();
    String::from_utf8_lossy(&bytes[start..])
}

/// The name of this tool.
pub const NAME: &str = "bash";

/// Builds the tool's spec.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: NAME.into(),
        description: concat!(
            "Run a shell command with `bash -c` in the working directory. ",
            "stdout and stderr are merged. Stdin is empty. ",
            "Output over 16 KB keeps the first and last 8 KB and saves the full output to a file. ",
            "Start a long-running server in the background with its output redirected to a file."
        )
        .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to run." },
                "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 600000, "description": "Timeout in milliseconds; default 120000." }
            },
            "required": ["command"]
        }),
    }
}

impl Tool for Bash {
    fn spec(&self) -> &ToolSpec {
        &self.spec
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
            self.run_inner(&arguments, cancel).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(tmp: &tempfile::TempDir) -> Bash {
        Bash::new(tmp.path())
    }

    async fn run(tmp: &tempfile::TempDir, args: &str) -> ToolOutput {
        tool(tmp).run(args, &CancellationToken::new()).await
    }

    fn spill_path(text: &str) -> &str {
        let start = text.find(" in ").expect("no spill path") + " in ".len();
        let rest = &text[start..];
        &rest[..rest.find(']').expect("no closing bracket")]
    }

    async fn wait_until_gone(pid: i32) -> bool {
        for _ in 0..100 {
            if !process_group::alive(pid) {
                return true;
            }
            sleep(Duration::from_millis(10)).await;
        }
        false
    }

    async fn read_pids(tmp: &tempfile::TempDir) -> Vec<i32> {
        let text = tokio::fs::read_to_string(tmp.path().join("pids"))
            .await
            .unwrap_or_default();
        text.lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }

    use crate::process_group;

    #[tokio::test]
    async fn runs_a_command_and_reports_the_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(&tmp, r#"{"command":"echo hi && exit 7"}"#).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content.as_ref(), "hi\nexit code: 7");
    }

    #[tokio::test]
    async fn keeps_stderr_in_the_output() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(&tmp, r#"{"command":"echo out; echo err 1>&2"}"#).await;
        let text = output.content.as_ref();
        assert!(text.contains("out"), "{text}");
        assert!(text.contains("err"), "{text}");
    }

    #[tokio::test]
    async fn runs_in_bash() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(&tmp, r#"{"command":"[[ 1 == 1 ]] && echo yes"}"#).await;
        assert_eq!(output.content.as_ref(), "yes\nexit code: 0");
    }

    #[tokio::test]
    async fn stdin_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let output = run(&tmp, r#"{"command":"cat; echo end","timeout_ms":5000}"#).await;
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(output.content.as_ref(), "end\nexit code: 0");
    }

    #[tokio::test]
    async fn a_signal_exit_names_the_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(&tmp, r#"{"command":"kill -9 $$"}"#).await;
        assert_eq!(output.content.as_ref(), "killed by signal 9");
    }

    #[tokio::test]
    async fn timeout_kills_a_child_and_its_grandchild() {
        let tmp = tempfile::tempdir().unwrap();
        let command = "sh -c 'sleep 60 & echo $! > pids; sleep 60 & echo $! >> pids; sleep 60'";
        let args = format!(
            r#"{{"command":"{}","timeout_ms":400}}"#,
            command.replace('"', "\\\"")
        );
        let output = run(&tmp, &args).await;
        let text = output.content.as_ref();
        assert!(text.contains("timed out after 400 ms"), "{text}");
        let pids = read_pids(&tmp).await;
        assert_eq!(pids.len(), 2, "grandchild pids not recorded");
        for pid in pids {
            assert!(
                wait_until_gone(pid).await,
                "grandchild {pid} survived the timeout"
            );
        }
    }

    #[tokio::test]
    async fn cancel_stops_within_100_ms() {
        let tmp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(20)).await;
            child_cancel.cancel();
        });
        let start = std::time::Instant::now();
        let output = tool(&tmp)
            .run(r#"{"command":"sleep 60","timeout_ms":60000}"#, &cancel)
            .await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "cancel took {elapsed:?}"
        );
        assert!(
            output.content.as_ref().contains("cancelled"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn dropping_the_call_kills_the_group() {
        let tmp = tempfile::tempdir().unwrap();
        let bash = tool(&tmp);
        let cancel = CancellationToken::new();
        let call = bash.run(r#"{"command":"sleep 60 & echo $! > pids; wait"}"#, &cancel);
        // Boxed, so `drop(call)` drops the future itself, not a pinned borrow.
        let mut call = Box::pin(call);
        // Drop the call only once the grandchild pid is on disk, so a slow,
        // loaded machine cannot drop it before the shell has started.
        let pids = tokio::select! {
            _ = &mut call => panic!("the call ended before the drop"),
            pids = async {
                loop {
                    let pids = read_pids(&tmp).await;
                    if !pids.is_empty() {
                        break pids;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            } => pids,
        };
        drop(call);
        assert_eq!(pids.len(), 1, "grandchild pid not recorded");
        assert!(
            wait_until_gone(pids[0]).await,
            "grandchild survived the drop"
        );
    }

    #[tokio::test]
    async fn a_background_child_holding_the_pipe_does_not_block() {
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let output = run(
            &tmp,
            r#"{"command":"sleep 3 & echo done","timeout_ms":10000}"#,
        )
        .await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
        let text = output.content.as_ref();
        assert!(text.starts_with("done\n"), "{text}");
        assert!(text.ends_with("exit code: 0"), "{text}");
    }

    #[tokio::test]
    async fn a_redirected_background_child_keeps_running() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(
            &tmp,
            r#"{"command":"sleep 5 > /dev/null 2>&1 & echo $! > pids"}"#,
        )
        .await;
        assert_eq!(output.content.as_ref(), "exit code: 0");
        let pids = read_pids(&tmp).await;
        assert_eq!(pids.len(), 1);
        assert!(
            process_group::alive(pids[0]),
            "a normal exit killed the background child"
        );
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pids[0]),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    #[tokio::test]
    async fn large_output_is_cut_and_the_spill_is_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(&tmp, r#"{"command":"seq 1 50000","timeout_ms":8000}"#).await;
        let text = output.content.as_ref();
        let mut expected = String::new();
        for n in 1..=50_000 {
            expected.push_str(&n.to_string());
            expected.push('\n');
        }
        assert!(
            text.starts_with(&format!(
                "[output cut: {} bytes; full output in ",
                expected.len()
            )),
            "{}",
            &text[..200]
        );
        assert!(text.contains("\n1\n2\n3\n"), "head missing");
        assert!(text.ends_with("49999\n50000\nexit code: 0"), "tail missing");
        assert!(text.len() < 2 * CAP + 300, "view too big: {}", text.len());
        let spilled = tokio::fs::read_to_string(spill_path(text)).await.unwrap();
        assert!(
            spilled == expected,
            "spill differs: {} vs {} bytes",
            spilled.len(),
            expected.len()
        );
    }

    #[tokio::test]
    async fn output_just_over_the_cut_is_spilled_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes = CUT_AT + 1;
        let args = format!(r#"{{"command":"head -c {bytes} /dev/zero | tr '\\000' 'x'"}}"#);
        let output = run(&tmp, &args).await;
        let spilled = tokio::fs::read(spill_path(output.content.as_ref()))
            .await
            .unwrap();
        assert_eq!(spilled.len() as u64, bytes);
        assert!(spilled.iter().all(|&byte| byte == b'x'));
    }

    #[tokio::test]
    async fn output_at_the_cut_is_shown_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let args = format!(r#"{{"command":"head -c {CUT_AT} /dev/zero | tr '\\000' 'x'"}}"#);
        let output = run(&tmp, &args).await;
        let text = output.content.as_ref();
        assert!(!text.contains("[output cut"), "cut too early");
        assert_eq!(text.len() as u64, CUT_AT + "\nexit code: 0".len() as u64);
    }

    #[test]
    fn cuts_drop_split_characters() {
        let word = "e\u{e9}".as_bytes();
        assert_eq!(head_text(&word[..2]), "e");
        assert_eq!(tail_text(&word[2..]), "");
        assert_eq!(tail_text("\u{e9}x".as_bytes()), "\u{e9}x");
    }

    #[test]
    fn whole_joins_the_head_and_the_tail_without_overlap() {
        let mut collect = Collect::default();
        let bytes: Vec<u8> = (0..CUT_AT).map(|n| n as u8).collect();
        collect.head.extend_from_slice(&bytes[..CAP]);
        collect.tail.extend(&bytes[bytes.len() - CAP..]);
        collect.total = CUT_AT;
        assert_eq!(collect.whole(CUT_AT), bytes);
    }

    #[tokio::test]
    async fn bad_args_are_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(&tmp, "not json").await;
        assert!(output.is_error);
    }
}
