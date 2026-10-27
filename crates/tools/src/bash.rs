//! `bash`: run a shell command with a timeout, capping the output the model
//! sees while spilling the full output to a file when it is large.

use std::borrow::Cow;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use reagent_agent::tool::{EffectClass, Object, Reroute, Tool, ToolOutput, parse_args};
use reagent_ai::ToolSpec;

use crate::process_group::{self, GroupGuard};
use crate::spill;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Bytes kept in each of the model-facing head and tail.
const CAP: usize = 8 * 1024;
/// Output up to this size is shown whole and not spilled.
const CUT_AT: u64 = 2 * CAP as u64;
/// The spill file stops growing here, so `yes` cannot fill the disk.
const SPILL_CAP: u64 = 64 * 1024 * 1024;
/// Bytes read from a pipe at once, and the spill buffer's size, so a large
/// output costs one read and one write per buffer, not per 4 KB. A full
/// buffer is written, never a partial one, so the file sees one write per
/// `BUF` bytes whatever sizes the pipes hand over.
const BUF: usize = 64 * 1024;
/// How long to keep reading after the shell ends. A background child that
/// holds the pipe open would otherwise block the call until it exits.
const GRACE: Duration = Duration::from_millis(250);
/// Settings that stop common tools from waiting for a person: pagers,
/// credential prompts, editors and package-manager dialogs.
const NON_INTERACTIVE: [(&str, &str); 7] = [
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("MANPAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GCM_INTERACTIVE", "never"),
    ("GIT_EDITOR", "true"),
    ("DEBIAN_FRONTEND", "noninteractive"),
];

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

const UNAVAILABLE: &str = "reagent: sandbox unavailable: ";
/// The exit code with which a launcher says it could not start.
const UNAVAILABLE_CODE: i32 = 125;
/// The object's field for the spill file that holds a cut output.
const FULL_OUTPUT_PATH: &str = "full_output_path";
/// The line the text gets when a background process outlives the shell.
const PIPE_HELD: &str =
    "[a background process still holds the output pipe; its later output is not shown]\n";

/// What the bash tool does when the sandbox helper reports it cannot start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// Run this and later commands without the helper, warning once on stderr.
    Auto,
    /// Refuse the command.
    Required,
}

/// A program that runs `bash -c <command>` for the tool, such as the sandbox helper.
#[derive(Debug, Clone)]
pub struct Launcher {
    /// The program to start in place of bash.
    pub program: PathBuf,
    /// Arguments placed before `-- bash -c <command>`.
    pub args: Vec<String>,
    /// What to do when the launcher cannot start.
    pub fallback: Fallback,
}

/// Runs a `bash -c` command (`sh -c` when bash is missing) with a hard
/// timeout and output capping.
#[derive(Debug)]
pub struct Bash {
    spec: ToolSpec,
    working_dir: PathBuf,
    launcher: Option<Launcher>,
    disabled: AtomicBool,
    /// Whether a plain file read runs as a `read` call; see [`crate::bash_read`].
    reads: bool,
    /// The writes that reached the spill file of the last call. A test shows
    /// that the buffering costs one write per `BUF` bytes.
    #[cfg(test)]
    spill_writes: AtomicU64,
}

impl Bash {
    /// Builds the tool for the given working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            spec: spec(),
            working_dir: working_dir.into(),
            launcher: None,
            disabled: AtomicBool::new(false),
            reads: true,
            #[cfg(test)]
            spill_writes: AtomicU64::new(0),
        }
    }

    /// Turns the running of plain file reads as `read` calls on or off.
    pub fn with_reads(mut self, reads: bool) -> Self {
        self.reads = reads;
        self
    }

    /// Runs commands through `launcher` instead of starting bash directly.
    pub fn with_launcher(mut self, launcher: Launcher) -> Self {
        self.launcher = Some(launcher);
        self
    }

    async fn run_inner(&self, args: &Args<'_>, cancel: &CancellationToken) -> ToolOutput {
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let launcher = self
            .launcher
            .as_ref()
            .filter(|_| !self.disabled.load(Ordering::Relaxed));
        let Some(launcher) = launcher else {
            return self.execute(None, args, timeout_ms, cancel).await.output;
        };
        let ran = self.execute(Some(launcher), args, timeout_ms, cancel).await;
        let Some(reason) = ran.unavailable else {
            return ran.output;
        };
        match launcher.fallback {
            Fallback::Required => ToolOutput::error(format!(
                "the sandbox could not start ({reason}); the command did not run"
            )),
            Fallback::Auto => {
                if !self.disabled.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "reagent: the sandbox is not available here ({reason}); bash commands run without it"
                    );
                }
                self.execute(None, args, timeout_ms, cancel).await.output
            }
        }
    }

    async fn execute(
        &self,
        launcher: Option<&Launcher>,
        args: &Args<'_>,
        timeout_ms: u64,
        cancel: &CancellationToken,
    ) -> Ran {
        let spawned = match launcher {
            Some(launcher) => launch(launcher, &self.working_dir, &args.command),
            None => spawn(&self.working_dir, &args.command),
        };
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                return Ran::plain(ToolOutput::error(format!(
                    "cannot start the command: {error}"
                )));
            }
        };
        let Some(pid) = child.id() else {
            return Ran::plain(ToolOutput::error("cannot start the command: no process id"));
        };
        let mut group = GroupGuard::new(pid as i32);
        let (outcome, mut collect, pipe_held) =
            run_and_collect(&mut child, &mut group, timeout_ms, cancel).await;

        // The writes of this call, so a test never reads another call's count.
        #[cfg(test)]
        self.spill_writes.store(collect.writes, Ordering::Relaxed);

        let mut text = collect.view();
        let truncated = collect.total > CUT_AT;
        let full_output_path = match (&collect.spill, truncated) {
            (Spill::Open { path, .. }, true) => Some(path.display().to_string()),
            _ => None,
        };
        let unavailable = match &outcome {
            Outcome::Exited(status)
                if status.code() == Some(UNAVAILABLE_CODE) && launcher.is_some() =>
            {
                text.strip_prefix(UNAVAILABLE)
                    .map(|rest| rest.lines().next().unwrap_or_default().to_string())
            }
            _ => None,
        };
        // The summary is the last part of the text and not command output, so
        // a script gets the output as the text without its last `tail` bytes.
        let summary = outcome.summary();
        let mut tail = summary.len();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
            tail += 1;
        }
        if pipe_held {
            text.push_str(PIPE_HELD);
            tail += PIPE_HELD.len();
        }
        text.push_str(&summary);
        let mut fields = serde_json::Map::new();
        fields.insert("exit_code".into(), exit_code(&outcome));
        fields.insert("truncated".into(), truncated.into());
        if let Some(path) = full_output_path {
            fields.insert(FULL_OUTPUT_PATH.into(), path.into());
        }
        Ran {
            output: ToolOutput::ok(text).with_object(Object::new(fields).with_tail(tail)),
            unavailable,
        }
    }
}

/// The exit code of the command as an object field, `null` when it did not
/// exit with a code.
fn exit_code(outcome: &Outcome) -> serde_json::Value {
    match outcome {
        Outcome::Exited(status) => status.code().map_or(serde_json::Value::Null, Into::into),
        _ => serde_json::Value::Null,
    }
}

/// The result of one run, and the reason when the launcher could not start.
struct Ran {
    output: ToolOutput,
    unavailable: Option<String>,
}

impl Ran {
    fn plain(output: ToolOutput) -> Self {
        Self {
            output,
            unavailable: None,
        }
    }
}

fn launch(launcher: &Launcher, working_dir: &Path, command: &str) -> io::Result<Child> {
    let mut start = Command::new(&launcher.program);
    start
        .args(&launcher.args)
        .arg("--")
        .arg("bash")
        .arg("-c")
        .arg(command);
    configure(&mut start, working_dir);
    start.spawn()
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
    shell.arg("-c").arg(command);
    configure(&mut shell, working_dir);
    shell
}

fn configure(command: &mut Command, working_dir: &Path) {
    command
        .current_dir(working_dir)
        .envs(NON_INTERACTIVE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    process_group::new_session(command);
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
    // On the heap: two 64 KB arrays would make this future too large to move.
    let mut out_buf = vec![0_u8; BUF];
    let mut err_buf = vec![0_u8; BUF];
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
    tail: Vec<u8>,
    total: u64,
    spill: Spill,
    /// The writes that reached the spill file, so a test can show that the
    /// buffering costs one write per `BUF` bytes.
    #[cfg(test)]
    writes: u64,
}

#[derive(Default)]
enum Spill {
    #[default]
    None,
    Open {
        file: tokio::fs::File,
        path: PathBuf,
        written: u64,
        /// Output not yet written to `file`, at most `BUF` bytes.
        buffer: Vec<u8>,
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
        if let Spill::Open {
            file,
            written,
            buffer,
            ..
        } = &mut self.spill
        {
            let room = SPILL_CAP.saturating_sub(*written);
            let part = &chunk[..chunk.len().min(room as usize)];
            #[cfg(test)]
            let held = buffer.len();
            match buffer_spill(file, buffer, part).await {
                Ok(()) => {
                    *written += part.len() as u64;
                    // Only a full buffer is written: that is one write.
                    #[cfg(test)]
                    {
                        self.writes += ((held + part.len()) / BUF) as u64;
                    }
                }
                Err(error) => self.spill = Spill::Failed(error.to_string()),
            }
        }

        if self.head.len() < CAP {
            let take = chunk.len().min(CAP - self.head.len());
            self.head.extend_from_slice(&chunk[..take]);
        }
        // The last `CAP` bytes are the tail. A chunk at least `CAP` long is
        // kept whole, its front dropped once, rather than byte by byte.
        if chunk.len() >= CAP {
            self.tail.clear();
            self.tail.extend_from_slice(&chunk[chunk.len() - CAP..]);
        } else {
            self.tail.extend_from_slice(chunk);
            // Old bytes are dropped only once there are `CAP` of them, so the
            // copies of the front amortise.
            let overflow = self.tail.len().saturating_sub(2 * CAP);
            if overflow > 0 {
                self.tail.drain(..overflow);
            }
        }
    }

    /// The first `total` bytes of the stream, in one buffer of the size it
    /// needs. Valid only while `total <= CUT_AT`, when the head and the tail
    /// cover every byte.
    fn whole(&self, total: u64) -> Vec<u8> {
        let rest = &self.tail[self.overlap(total)..];
        let mut bytes = Vec::with_capacity(self.head.len() + rest.len());
        bytes.extend_from_slice(&self.head);
        bytes.extend_from_slice(rest);
        bytes
    }

    /// How many of its last bytes the tail repeats from the end of the head,
    /// given `total` bytes seen.
    fn overlap(&self, total: u64) -> usize {
        (self.head.len() + self.tail.len())
            .saturating_sub(total as usize)
            .min(self.tail.len())
    }

    async fn flush(&mut self) {
        if let Spill::Open { file, buffer, .. } = &mut self.spill {
            if !buffer.is_empty() {
                let _ = file.write_all(buffer).await;
                buffer.clear();
                #[cfg(test)]
                {
                    self.writes += 1;
                }
            }
            let _ = file.flush().await;
        }
    }

    /// The model-facing text: the whole output when small, else the head and
    /// the tail under a line that says where the full output is.
    fn view(&mut self) -> String {
        if self.total <= CUT_AT {
            // Nothing reads the collector once it has its text, so the head's
            // buffer is taken when it holds every byte: an output under `CAP`
            // costs no copy at all, a larger one one buffer and one cast.
            let bytes = if self.overlap(self.total) == self.tail.len() {
                std::mem::take(&mut self.head)
            } else {
                self.whole(self.total)
            };
            // Valid UTF-8 is the common case and takes its bytes as they are;
            // only a broken output pays the lossy decode.
            return match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
            };
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
        let head = head_text(&self.head);
        // The tail may hold up to 2 * CAP bytes between drains; show CAP.
        let tail = tail_text(&self.tail[self.tail.len().saturating_sub(CAP)..]);
        let mut text = String::with_capacity(note.len() + head.len() + tail.len() + 6);
        text.push_str(&note);
        text.push('\n');
        text.push_str(&head);
        text.push_str("\n...\n");
        text.push_str(&tail);
        text
    }
}

/// Opens a new spill file in this run's folder and buffers `prefix`, the output
/// before the cut.
async fn open_spill(prefix: &[u8]) -> Spill {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    match spill::folder().await {
        Ok(folder) => open_spill_in(folder, &NEXT, prefix).await,
        Err(error) => Spill::Failed(error),
    }
}

/// How many taken names a spill skips before it gives up.
const SPILL_NAME_TRIES: u32 = 64;

/// Opens a new spill file in `folder`, named by the process id and the next
/// value of `next`. A taken name is skipped, so an old spill in the folder is
/// never truncated.
async fn open_spill_in(folder: &Path, next: &AtomicU64, prefix: &[u8]) -> Spill {
    let result = async {
        tokio::fs::create_dir_all(folder).await?;
        let mut tries = 0;
        let (file, path) = loop {
            let path = folder.join(format!(
                "bash-{}-{}.log",
                std::process::id(),
                next.fetch_add(1, Ordering::Relaxed)
            ));
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
            {
                Ok(file) => break (file, path),
                Err(error)
                    if error.kind() == io::ErrorKind::AlreadyExists
                        && tries + 1 < SPILL_NAME_TRIES =>
                {
                    tries += 1;
                }
                Err(error) => return Err(error),
            }
        };
        let mut buffer = Vec::with_capacity(BUF);
        buffer.extend_from_slice(prefix);
        io::Result::Ok((file, path, buffer))
    }
    .await;
    match result {
        Ok((file, path, buffer)) => Spill::Open {
            file,
            path,
            written: prefix.len() as u64,
            buffer,
        },
        Err(error) => Spill::Failed(error.to_string()),
    }
}

/// Adds `bytes` to the spill buffer and writes it to the file in `BUF` blocks,
/// so the file costs one write per `BUF` bytes whatever sizes the pipes hand
/// over.
async fn buffer_spill(
    file: &mut tokio::fs::File,
    buffer: &mut Vec<u8>,
    mut bytes: &[u8],
) -> io::Result<()> {
    // An empty buffer writes whole blocks straight from `bytes`: no copy
    // through the buffer for a chunk as large as it.
    if buffer.is_empty() && bytes.len() >= BUF {
        let whole = bytes.len() / BUF * BUF;
        file.write_all(&bytes[..whole]).await?;
        bytes = &bytes[whole..];
    }
    while !bytes.is_empty() {
        let take = (BUF - buffer.len()).min(bytes.len());
        buffer.extend_from_slice(&bytes[..take]);
        bytes = &bytes[take..];
        if buffer.len() == BUF {
            file.write_all(buffer).await?;
            buffer.clear();
        }
    }
    Ok(())
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

    fn main_arg(&self) -> Option<&'static str> {
        Some("command")
    }

    fn effect(&self) -> EffectClass {
        EffectClass::LocalReversible
    }

    fn returns(&self) -> &'static str {
        "{output, exit_code, truncated, full_output_path?}"
    }

    fn reroute<'a>(
        &'a self,
        args: &'a str,
    ) -> Option<futures_util::future::BoxFuture<'a, Option<Reroute>>> {
        if !self.reads {
            return None;
        }
        let arguments = parse_args::<Args<'a>>(args).ok()?;
        // The pure parse first: a command that is no read form allocates no
        // future and touches no file.
        let read = crate::bash_read::classify(&arguments.command)?.into_owned();
        Some(Box::pin(async move {
            let args = crate::bash_read::read_call(&self.working_dir, read).await?;
            Some(Reroute {
                tool: crate::read::NAME,
                args,
                label: "bash ran this as read",
            })
        }))
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
    async fn a_command_runs_in_its_own_session() {
        let tmp = tempfile::tempdir().unwrap();
        // bash may exec the last command, so the leader is the process or its parent.
        let output = run(
            &tmp,
            r#"{"command":"python3 -c 'import os; print(os.getsid(0) in (os.getpid(), os.getppid()))'"}"#,
        )
        .await;
        assert_eq!(output.content.as_ref(), "True\nexit code: 0");
    }

    #[tokio::test]
    async fn non_interactive_variables_are_set() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(
            &tmp,
            r#"{"command":"echo $PAGER $GIT_PAGER $MANPAGER $GIT_TERMINAL_PROMPT $GCM_INTERACTIVE $GIT_EDITOR $DEBIAN_FRONTEND"}"#,
        )
        .await;
        assert_eq!(
            output.content.as_ref(),
            "cat cat cat 0 never true noninteractive\nexit code: 0"
        );
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
        // Timed from the cancel, so a slow process start on a loaded machine
        // does not count against the limit.
        let cancelled_at = tokio::spawn(async move {
            sleep(Duration::from_millis(20)).await;
            child_cancel.cancel();
            std::time::Instant::now()
        });
        let output = tool(&tmp)
            .run(r#"{"command":"sleep 60","timeout_ms":60000}"#, &cancel)
            .await;
        let ended = std::time::Instant::now();
        let elapsed = ended.duration_since(cancelled_at.await.expect("the cancel task"));
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
    async fn spill_name_takes_the_next_free_name() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        for n in 0..4 {
            tokio::fs::write(tmp.path().join(format!("bash-{pid}-{n}.log")), b"old")
                .await
                .unwrap();
        }
        let next = AtomicU64::new(0);
        let spill = open_spill_in(tmp.path(), &next, b"new").await;
        let Spill::Open { path, .. } = &spill else {
            panic!("the spill did not open");
        };
        assert_eq!(path, &tmp.path().join(format!("bash-{pid}-4.log")));
        assert_eq!(next.load(Ordering::Relaxed), 5);
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

    /// A spill of 8 MB stays byte for byte the output, and costs one write per
    /// `BUF` bytes rather than one per read chunk: the spill buffer holds
    /// `BUF` bytes and only a full one is written, so 8 MB / 64 KB + 2 writes
    /// is the most the file may see (128 measured, against 2045 with a 4 KB
    /// read buffer and a direct write per chunk). The count is the call's own,
    /// kept by the run, so a test that spills at the same time cannot touch it.
    #[tokio::test]
    async fn perf_spill_writes_one_buffer_per_write() {
        const BYTES: u64 = 8 * 1024 * 1024;
        let tmp = tempfile::tempdir().unwrap();
        let bash = tool(&tmp);
        let args = format!(
            r#"{{"command":"head -c {BYTES} /dev/zero | tr '\\000' 'x'","timeout_ms":60000}}"#
        );
        let output = bash.run(&args, &CancellationToken::new()).await;
        assert!(!output.is_error, "{}", output.content);
        let spilled = tokio::fs::read(spill_path(output.content.as_ref()))
            .await
            .unwrap();
        assert_eq!(spilled.len() as u64, BYTES);
        assert!(spilled.iter().all(|&byte| byte == b'x'), "spill differs");

        let writes = bash.spill_writes.load(Ordering::Relaxed);
        let most = BYTES / BUF as u64 + 2;
        eprintln!("spill writes: {writes} for {BYTES} bytes, at most {most}");
        assert!(
            writes <= most,
            "{writes} writes for {BYTES} bytes to the spill, over {most}"
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

    #[tokio::test]
    async fn bash_object_has_exit_code_and_output() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(&tmp, r#"{"command":"echo hi && exit 3"}"#).await;
        assert_eq!(output.content.as_ref(), "hi\nexit code: 3");
        let object = output.object.expect("no object");
        assert_eq!(object.fields.get("exit_code"), Some(&serde_json::json!(3)));
        assert_eq!(
            object.fields.get("truncated"),
            Some(&serde_json::json!(false))
        );
        assert!(
            !object.fields.contains_key(FULL_OUTPUT_PATH),
            "{:?}",
            object.fields
        );
        let tail = object.tail.expect("no tail");
        let shown = &output.content[..output.content.len() - tail];
        assert_eq!(shown, "hi\n", "{shown:?}");
    }

    #[tokio::test]
    async fn bash_object_cut_output_names_the_spill_file() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(&tmp, r#"{"command":"seq 1 50000","timeout_ms":8000}"#).await;
        let object = output.object.expect("no object");
        assert_eq!(object.fields.get("exit_code"), Some(&serde_json::json!(0)));
        assert_eq!(
            object.fields.get("truncated"),
            Some(&serde_json::json!(true))
        );
        let path = object
            .fields
            .get(FULL_OUTPUT_PATH)
            .and_then(|path| path.as_str())
            .expect("no spill file");
        let spilled = tokio::fs::read_to_string(path).await.unwrap();
        assert!(spilled.starts_with("1\n2\n3\n"), "{}", &spilled[..20]);
        assert!(spilled.ends_with("49999\n50000\n"), "{}", &spilled[..20]);
        let tail = object.tail.expect("no tail");
        let shown = &output.content[..output.content.len() - tail];
        assert!(shown.starts_with("[output cut: "), "{shown}");
        assert!(shown.ends_with("49999\n50000\n"), "{}", &shown[..200]);
    }

    #[tokio::test]
    async fn bash_object_signal_has_null_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(&tmp, r#"{"command":"kill -9 $$"}"#).await;
        assert_eq!(output.content.as_ref(), "killed by signal 9");
        let object = output.object.expect("no object");
        assert_eq!(
            object.fields.get("exit_code"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(object.tail, Some(output.content.len()));
    }

    #[test]
    fn cuts_drop_split_characters() {
        let word = "e\u{e9}".as_bytes();
        assert_eq!(head_text(&word[..2]), "e");
        assert_eq!(tail_text(&word[2..]), "");
        assert_eq!(tail_text("\u{e9}x".as_bytes()), "\u{e9}x");
    }

    /// The tail holds the last `CAP` bytes of the stream: a chunk at least
    /// `CAP` long is taken in one copy, and a run of small chunks drops its
    /// front only once `CAP` stale bytes are there, so the copies amortise.
    /// Either way what stays is the end of the stream, never a gap.
    #[tokio::test]
    async fn perf_capture_tail_keeps_the_end_of_the_stream() {
        let stream: Vec<u8> = (0..3 * CAP).map(|n| b'a' + (n % 26) as u8).collect();
        let end = std::str::from_utf8(&stream[stream.len() - CAP..]).unwrap_or_default();
        let mut collect = Collect::default();
        let expected = tail_text(&stream[stream.len() - CAP..]);
        for chunk in stream.chunks(1000) {
            collect.push(chunk).await;
        }
        let view = collect.view();
        let shown = view.rsplit_once("\n...\n").map(|(_, tail)| tail);
        assert_eq!(shown, Some(&*expected), "small chunks change the tail");
        assert!(view.ends_with(end), "the view lost the last bytes");

        let mut collect = Collect::default();
        collect.push(&stream).await;
        let view = collect.view();
        let shown = view.rsplit_once("\n...\n").map(|(_, tail)| tail);
        assert_eq!(shown, Some(&*expected), "one large chunk changes the tail");
    }

    /// An output under the cut keeps its bytes: the text is the stream's own,
    /// not a lossy decode of it.
    #[tokio::test]
    async fn the_whole_view_is_the_output_text() {
        let text = "\u{e9}".repeat(CUT_AT as usize / 2);
        assert_eq!(text.len() as u64, CUT_AT);
        let mut collect = Collect::default();
        for chunk in text.as_bytes().chunks(1000) {
            collect.push(chunk).await;
        }
        assert_eq!(collect.view(), text);
    }

    /// An output cut inside a multi-byte character drops that one character at
    /// each end and keeps every other byte as it was.
    #[tokio::test]
    async fn the_cut_view_drops_only_the_split_characters() {
        // "a\u{e9}" is three bytes, so an 8 KiB cut lands inside an
        // "\u{e9}" at the end of the head and at the start of the tail.
        let mut stream = "a\u{e9}".repeat(CAP).into_bytes();
        stream.push(b'a');
        let mut collect = Collect::default();
        collect.push(&stream).await;
        let view = collect.view();
        let (note, shown) = view.split_once('\n').expect("no note line");
        assert!(
            note.starts_with(&format!("[output cut: {} bytes", stream.len())),
            "{note}"
        );
        let (head, tail) = shown.rsplit_once("\n...\n").expect("no cut marker");
        assert_eq!(head.len(), CAP - 1, "the head lost more than one byte");
        assert_eq!(tail.len(), CAP - 1, "the tail lost more than one byte");
        assert!(head.chars().all(|c| c == 'a' || c == '\u{e9}'), "{head:?}");
        assert!(tail.chars().all(|c| c == 'a' || c == '\u{e9}'), "{tail:?}");
    }

    #[test]
    fn whole_joins_the_head_and_the_tail_without_overlap() {
        let mut collect = Collect::default();
        let bytes: Vec<u8> = (0..CUT_AT).map(|n| n as u8).collect();
        collect.head.extend_from_slice(&bytes[..CAP]);
        collect.tail.extend_from_slice(&bytes[bytes.len() - CAP..]);
        collect.total = CUT_AT;
        assert_eq!(collect.whole(CUT_AT), bytes);
    }

    async fn fake_launcher(tmp: &tempfile::TempDir, script: &str, fallback: Fallback) -> Launcher {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp.path().join("launcher.sh");
        tokio::fs::write(&path, format!("#!/bin/sh\n{script}\n"))
            .await
            .unwrap();
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();
        Launcher {
            program: path,
            args: vec!["--flag".into()],
            fallback,
        }
    }

    const UNAVAILABLE_SCRIPT: &str =
        "echo 'reagent: sandbox unavailable: test reason' >&2\nexit 125";

    #[tokio::test]
    async fn a_launcher_wraps_the_command() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = fake_launcher(
            &tmp,
            "echo \"launched $1\"\nshift\n[ \"$1\" = -- ] && shift\nexec \"$@\"",
            Fallback::Required,
        )
        .await;
        let bash = tool(&tmp).with_launcher(launcher);
        let output = bash
            .run(r#"{"command":"echo inner"}"#, &CancellationToken::new())
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            output.content.as_ref(),
            "launched --flag\ninner\nexit code: 0"
        );
    }

    #[tokio::test]
    async fn an_unavailable_sandbox_falls_back_once_in_auto() {
        let tmp = tempfile::tempdir().unwrap();
        let bash =
            tool(&tmp).with_launcher(fake_launcher(&tmp, UNAVAILABLE_SCRIPT, Fallback::Auto).await);
        assert!(!bash.disabled.load(Ordering::Relaxed));
        let cancel = CancellationToken::new();
        let first = bash.run(r#"{"command":"echo one"}"#, &cancel).await;
        assert_eq!(first.content.as_ref(), "one\nexit code: 0");
        assert!(bash.disabled.load(Ordering::Relaxed));
        let second = bash.run(r#"{"command":"echo two"}"#, &cancel).await;
        assert_eq!(second.content.as_ref(), "two\nexit code: 0");
    }

    #[tokio::test]
    async fn an_unavailable_sandbox_refuses_in_on() {
        let tmp = tempfile::tempdir().unwrap();
        let bash = tool(&tmp)
            .with_launcher(fake_launcher(&tmp, UNAVAILABLE_SCRIPT, Fallback::Required).await);
        let output = bash
            .run(r#"{"command":"touch marker"}"#, &CancellationToken::new())
            .await;
        assert!(output.is_error);
        assert_eq!(
            output.content.as_ref(),
            "the sandbox could not start (test reason); the command did not run"
        );
        assert!(!tmp.path().join("marker").exists());
    }

    #[tokio::test]
    async fn no_launcher_runs_bash_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let bash = tool(&tmp);
        assert!(bash.launcher.is_none());
        let output = bash
            .run(r#"{"command":"exit 125"}"#, &CancellationToken::new())
            .await;
        assert!(!output.is_error);
        assert_eq!(output.content.as_ref(), "exit code: 125");
    }

    #[tokio::test]
    async fn bad_args_are_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let output = run(&tmp, "not json").await;
        assert!(output.is_error);
    }

    #[test]
    fn bash_main_arg_is_command() {
        let tmp = tempfile::tempdir().unwrap();
        let bash = tool(&tmp);
        assert_eq!(bash.main_arg(), Some("command"));
        assert_eq!(
            bash.effects(r#"{"command":"ls"}"#),
            reagent_agent::tool::Effects::default()
        );
    }
}
