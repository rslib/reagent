//! Runs one headless agent session and produces its output.

use std::future::Future;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use reagent_agent::agent_loop::{Agent, AgentConfig};
use reagent_agent::event::{Event, TurnEndReason};
use reagent_agent::policy::TurnPolicy;
use reagent_agent::policy::budget::Budget;
use reagent_agent::policy::completion::Completion;
use reagent_agent::policy::stuck::Stuck;
use reagent_agent::tool::Registry;
use reagent_agent::workspace::Workspace;
use reagent_ai::{Provider, StopReason, Text, Usage};
use reagent_tools::bash::Bash;
use reagent_tools::edit::Edit;
use reagent_tools::read::Read;
use reagent_tools::write::Write;

use crate::args::Args;
use crate::prompt;

/// The tools' working folder, fingerprinted for the completion policy.
struct Folder(std::path::PathBuf);

impl Workspace for Folder {
    fn fingerprint(&self) -> Pin<Box<dyn Future<Output = Option<u64>> + Send + '_>> {
        Box::pin(reagent_tools::workspace::fingerprint(&self.0))
    }
}

/// An async line writer over the process stdout. A write that fails (for
/// example a closed pipe) stops further writes instead of panicking, so a
/// piped consumer can close early.
struct Out {
    stdout: tokio::io::Stdout,
    online: bool,
}

impl Out {
    fn new() -> Self {
        Self {
            stdout: tokio::io::stdout(),
            online: true,
        }
    }

    /// Writes `text` plus a trailing newline when it has none. Returns false
    /// once a write has failed; later writes no-op.
    async fn line(&mut self, text: &str) -> bool {
        if !self.online {
            return false;
        }
        if !self.write(text.as_bytes()).await {
            return false;
        }
        if !text.is_empty() && !text.ends_with('\n') {
            self.write(b"\n").await
        } else {
            true
        }
    }

    /// Writes `bytes` and flushes. On any error, takes the writer offline.
    async fn write(&mut self, bytes: &[u8]) -> bool {
        let result = async {
            self.stdout.write_all(bytes).await?;
            self.stdout.flush().await
        }
        .await;
        let online = result.is_ok();
        if !online {
            self.online = false;
        }
        online
    }
}

/// The provider-call cap, so a looping model cannot run without end.
const MAX_TURNS: u32 = 200;

/// What cancelled the run, stored once by whichever fires first.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Cause {
    None = 0,
    Deadline = 1,
    CtrlC = 2,
}

impl Cause {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Cause::Deadline,
            2 => Cause::CtrlC,
            _ => Cause::None,
        }
    }
}

/// Records `cause` unless another cause came first, then cancels.
fn fire(slot: &AtomicU8, cause: Cause, token: &CancellationToken) {
    let _ = slot.compare_exchange(
        Cause::None as u8,
        cause as u8,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    token.cancel();
}

/// Reads the whole task from standard input (`-p -`).
///
/// Returns an error message for empty or non-UTF-8 input; the caller exits 2.
async fn read_stdin_task() -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut stdin = tokio::io::stdin();
    stdin
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("cannot read the task from stdin: {error}"))?;
    if bytes.is_empty() {
        return Err("the task from stdin is empty".into());
    }
    match String::from_utf8(bytes) {
        Ok(task) => Ok(task),
        Err(_) => Err("the task from stdin is not UTF-8".into()),
    }
}

/// Runs the agent for `args` and returns the process exit code.
pub async fn run(mut args: Args) -> ExitCode {
    // Resolve the task text before anything runs: `-p -` reads the whole task
    // from standard input, and an empty or non-UTF-8 task input exits 2.
    // The task moves out of `args`; `args` is still borrowed below.
    let source = std::mem::replace(&mut args.task, crate::args::TaskSource::Stdin);
    let mut task = match source {
        crate::args::TaskSource::Text(text) => text,
        crate::args::TaskSource::Stdin => match read_stdin_task().await {
            Ok(task) => task,
            Err(message) => {
                eprintln!("reagent: {message}");
                return ExitCode::from(2);
            }
        },
    };
    let cwd = match tokio::fs::canonicalize(&args.cwd).await {
        Ok(cwd) if tokio::fs::metadata(&cwd).await.is_ok_and(|m| m.is_dir()) => cwd,
        _ => {
            eprintln!("reagent: --cwd {} is not a folder", args.cwd.display());
            return ExitCode::from(2);
        }
    };
    let provider = build_provider(&args).await;
    let provider = match provider {
        Ok(provider) => provider,
        Err(message) => {
            eprintln!("reagent: {message}");
            return ExitCode::from(2);
        }
    };

    let mut registry = Registry::new();
    registry.add(Read::new(cwd.clone()));
    registry.add(Write::new(cwd.clone()));
    registry.add(Edit::new(cwd.clone()));
    registry.add(Bash::new(cwd.clone()));

    let system = prompt::system(cwd.to_string_lossy().as_ref());
    let config = AgentConfig {
        model: args.model.clone(),
        max_tokens: args.max_tokens,
        system: Some(system),
        reasoning: None,
        cache_key: Some(Text::from("headless-v1")),
        max_turns: Some(MAX_TURNS),
    };

    // The loop policies and whether to take a pre-run environment snapshot.
    let mut policies: Vec<Box<dyn TurnPolicy>> = Vec::new();
    let mut snapshot_enabled = false;
    let mut mask_enabled = false;
    let mut completion_enabled = false;
    for name in &args.policy {
        match name.as_str() {
            "completion" => {
                completion_enabled = true;
                policies.push(Box::new(Completion::new()));
            }
            "stuck" => policies.push(Box::new(Stuck::default())),
            "budget" => policies.push(Box::new(Budget::new(
                args.deadline.map(Duration::from_secs),
                Some(MAX_TURNS),
            ))),
            "snapshot" => snapshot_enabled = true,
            "mask" => mask_enabled = true,
            other => {
                eprintln!("reagent: unknown policy: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let cancel = CancellationToken::new();
    let cause = Arc::new(AtomicU8::new(Cause::None as u8));
    let mut timers = Vec::new();
    if let Some(seconds) = args.deadline {
        let token = cancel.clone();
        let cause = Arc::clone(&cause);
        timers.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            fire(&cause, Cause::Deadline, &token);
        }));
    }
    {
        let token = cancel.clone();
        let cause = Arc::clone(&cause);
        timers.push(tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                fire(&cause, Cause::CtrlC, &token);
            }
        }));
    }

    let folder = Folder(cwd.clone());
    let snapshot_cancel = cancel.clone();
    let mut agent = if policies.is_empty() {
        Agent::new(provider.as_ref(), &registry, config, cancel)
    } else {
        Agent::with_policies(provider.as_ref(), &registry, config, cancel, policies)
    };
    if mask_enabled {
        agent = agent.with_mask();
    }
    if completion_enabled {
        agent = agent.with_workspace(&folder);
    }
    let (tx, rx) = mpsc::channel::<Event>(256);
    let out = Out::new();
    let writer = tokio::spawn(drain(rx, out, args.json));

    if snapshot_enabled {
        let snapshot = crate::snapshot::capture_environment(&cwd, &snapshot_cancel).await;
        if !snapshot.trim().is_empty() {
            task = format!("{task}\n\n{} {snapshot}", crate::snapshot::header());
        }
    }
    let mut history = vec![reagent_ai::Message::user(task)];
    agent.run(&mut history, None, &tx).await;
    drop(tx);
    for timer in timers {
        timer.abort();
    }
    // The writer task never panics; a join error would mean a runtime bug.
    let reason = writer
        .await
        .unwrap_or_else(|_| panic!("writer task failed"));

    exit_code(&reason, Cause::from_u8(cause.load(Ordering::SeqCst)))
}

/// Consumes the agent's event channel, prints it and returns how the run ended.
///
/// Owns stdout: in plain mode only the final answer is printed; in JSON mode
/// every event is one line and a summary follows the run end.
async fn drain(rx: mpsc::Receiver<Event>, mut out: Out, json: bool) -> TurnEndReason {
    let mut answer = String::new();
    let mut total = Usage::default();
    let mut turns = 0u32;
    let mut reason = TurnEndReason::EndTurn;
    let mut stop = None;
    let mut ended = false;

    let mut rx = rx;
    let mut buf = Vec::with_capacity(512);
    while let Some(event) = rx.recv().await {
        match &event {
            Event::TurnStart => {
                turns += 1;
                answer.clear();
            }
            // A retried attempt starts the reply again, so its partial text goes.
            Event::Retry { .. } => answer.clear(),
            Event::TextDelta(text) => answer.push_str(text),
            Event::Usage(usage) => sum(&mut total, usage),
            Event::Error(message) if !json => eprintln!("reagent: {message}"),
            Event::TurnEnd(r) => reason = *r,
            Event::RunEnd(r) => {
                reason = *r;
                stop = stop_string(r);
                ended = true;
            }
            _ => {}
        }
        if json {
            buf.clear();
            crate::json::write_event(&mut buf, &event);
            let _ = out.write(&buf).await;
        }
        if ended {
            break;
        }
    }

    if json {
        buf.clear();
        crate::json::write_summary(&mut buf, &total, turns, &reason, stop);
        let _ = out.write(&buf).await;
    } else {
        let _ = out.line(&answer).await;
    }

    reason
}

/// Adds `delta` into the running `total`.
fn sum(total: &mut Usage, delta: &Usage) {
    total.input_tokens += delta.input_tokens;
    total.output_tokens += delta.output_tokens;
    total.cache_read_tokens += delta.cache_read_tokens;
    total.cache_write_tokens += delta.cache_write_tokens;
    total.reasoning_tokens += delta.reasoning_tokens;
}

/// The provider stop reason as a string, when the run stopped for one.
fn stop_string(reason: &TurnEndReason) -> Option<&'static str> {
    match reason {
        TurnEndReason::Stopped(inner) => Some(match inner {
            StopReason::EndTurn => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::MaxTokens => "max_tokens",
            StopReason::Refusal => "refusal",
            StopReason::Error => "error",
        }),
        _ => None,
    }
}

/// Maps the run outcome to the documented exit code. The cause counts only
/// when the run really ended cancelled, so a late timer cannot turn a
/// finished run into 124.
fn exit_code(reason: &TurnEndReason, cause: Cause) -> ExitCode {
    let code: u8 = match (reason, cause) {
        (TurnEndReason::EndTurn, _) => 0,
        (TurnEndReason::Cancelled, Cause::Deadline) => 124,
        (TurnEndReason::Cancelled, Cause::CtrlC) => 130,
        (
            TurnEndReason::Cancelled
            | TurnEndReason::MaxTurns
            | TurnEndReason::Error
            | TurnEndReason::Stopped(_),
            _,
        ) => 1,
    };
    ExitCode::from(code)
}

/// Builds the provider named on the command line, or reports why not.
async fn build_provider(args: &Args) -> Result<Box<dyn Provider>, String> {
    #[cfg(feature = "testing")]
    if let crate::args::Wire::Fake(spec) = &args.provider {
        return build_fake(spec)
            .await
            .map(|provider| Box::new(provider) as Box<dyn Provider>);
    }

    let base_url = args
        .base_url
        .clone()
        .unwrap_or_else(|| args.provider.base_url());
    let api_key_env = args
        .api_key_env
        .clone()
        .unwrap_or_else(|| args.provider.default_api_key_env().to_string());
    let api_key = std::env::var(&api_key_env)
        .ok()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| format!("missing {api_key_env}"))?;
    let header = args.header.clone();

    let provider: Box<dyn Provider> = match &args.provider {
        crate::args::Wire::Anthropic => {
            Box::new(reagent_ai::anthropic::Anthropic::new(api_key, base_url).with_headers(header))
        }
        crate::args::Wire::OpenAiChat => Box::new(
            reagent_ai::openai::chat::OpenAiChat::new(api_key, base_url).with_headers(header),
        ),
        crate::args::Wire::OpenAiResponses => Box::new(
            reagent_ai::openai::responses::OpenAiResponses::new(api_key, base_url)
                .with_headers(header),
        ),
        #[cfg(feature = "testing")]
        crate::args::Wire::Fake(_) => unreachable!("the testing provider is handled above"),
    };
    Ok(provider)
}

/// Reads a scripted FakeProvider from a JSON file, under the testing feature.
#[cfg(feature = "testing")]
async fn build_fake(file: &str) -> Result<reagent_ai::fake::FakeProvider, String> {
    let text = tokio::fs::read_to_string(file)
        .await
        .map_err(|error| format!("cannot read {file}: {error}"))?;
    fake::from_script(&text)
}

/// A JSON script that builds a FakeProvider for the headless tests.
#[cfg(feature = "testing")]
mod fake {
    use serde::Deserialize;
    use serde_json::Value;

    use reagent_ai::fake::FakeProvider;
    use reagent_ai::{Error, StopReason, StreamEvent, Text};

    /// One parsed reply: assistant text, any tool calls it makes and optional
    /// token usage.
    #[derive(Debug, Deserialize)]
    struct ScriptedReply {
        #[serde(default)]
        text: String,
        #[serde(default)]
        calls: Vec<ScriptedCall>,
        #[serde(default)]
        usage: Option<ScriptedUsage>,
        /// When set, the reply streams its text and then fails with this
        /// retryable error instead of finishing.
        #[serde(default)]
        fail: Option<String>,
        /// When true, the reply is a thinking block cut off by a max_tokens
        /// stop, with no signature, text or tool call.
        #[serde(default)]
        max_tokens: bool,
    }

    /// Token counts a reply reports to exercise the usage line.
    #[derive(Debug, Deserialize)]
    struct ScriptedUsage {
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        reasoning: u64,
    }

    #[derive(Debug, Deserialize)]
    struct ScriptedCall {
        #[serde(default)]
        id: String,
        name: String,
        #[serde(default)]
        arguments: Value,
    }

    /// The top-level script document.
    #[derive(Debug, Deserialize)]
    struct Script {
        replies: Vec<ScriptedReply>,
    }

    impl Script {
        /// Plays the scripted replies through a FakeProvider.
        fn into_provider(self) -> FakeProvider {
            let provider = FakeProvider::default();
            for reply in self.replies {
                match reply.fail {
                    Some(error) => {
                        let mut partial = Vec::new();
                        if !reply.text.is_empty() {
                            partial.push(StreamEvent::TextDelta(Text::from(reply.text)));
                        }
                        provider.push_failing_reply(partial, Error::Transient(error));
                    }
                    None => provider.push_reply(events(reply)),
                }
            }
            provider
        }
    }

    /// Builds the stream events for one scripted reply.
    fn events(reply: ScriptedReply) -> Vec<StreamEvent> {
        if reply.max_tokens {
            return vec![
                StreamEvent::ThinkingDelta(Text::from("")),
                StreamEvent::Stop(StopReason::MaxTokens),
            ];
        }
        let mut events = Vec::new();
        if !reply.text.is_empty() {
            events.push(StreamEvent::TextDelta(Text::from(reply.text)));
        }
        let mut used = false;
        for (index, call) in reply.calls.into_iter().enumerate() {
            let id = if call.id.is_empty() {
                format!("call_{index}")
            } else {
                call.id
            };
            events.push(StreamEvent::ToolCallStart {
                id: Text::from(id.clone()),
                name: Text::from(call.name),
            });
            let arguments = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".into());
            events.push(StreamEvent::ToolCallDelta {
                id: Text::from(id.clone()),
                partial_json: Text::from(arguments),
            });
            events.push(StreamEvent::ToolCallEnd { id: Text::from(id) });
            used = true;
        }
        if let Some(usage) = reply.usage {
            events.push(StreamEvent::Usage(reagent_ai::Usage {
                input_tokens: usage.input,
                output_tokens: usage.output,
                cache_read_tokens: usage.cache_read,
                cache_write_tokens: usage.cache_write,
                reasoning_tokens: usage.reasoning,
            }));
        }
        events.push(StreamEvent::Stop(if used {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        }));
        events
    }

    /// Parses a scripted fake provider from JSON.
    pub(super) fn from_script(text: &str) -> Result<FakeProvider, String> {
        let script: Script =
            serde_json::from_str(text).map_err(|error| format!("invalid fake script: {error}"))?;
        Ok(script.into_provider())
    }
}
