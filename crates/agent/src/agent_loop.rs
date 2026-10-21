//! The agent loop: stream a reply, run its tool calls, repeat.

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use reagent_ai::{
    AssistantMessage, Content, Error, Message, Provider, ReasoningLevel, Request, StopReason,
    StreamEvent, Text, ToolCall, ToolResult, Usage,
};

use crate::event::{Event, TurnEndReason};
use crate::mask::Mask;
use crate::policy::{PolicyRun, TurnPolicy, TurnView};
use crate::session::Session;
use crate::tool::Registry;
use crate::workspace::Workspace;

/// How many times an empty reply is requested again before the run stops.
const EMPTY_RETRIES: u32 = 2;

/// The note added after a max_tokens stop, baked in from `prompts/max_tokens.md`.
const MAX_TOKENS_NOTE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/max_tokens.md"
));

/// How many max_tokens stops in a row the loop recovers from before it stops.
const MAX_TOKENS_RECOVERIES: u32 = 3;

/// The parts of a [`Request`] a session controls, besides the conversation.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// The provider model id.
    pub model: String,
    /// The cap on reply tokens.
    pub max_tokens: u32,
    /// The system prompt, if any.
    pub system: Option<Text>,
    /// How much reasoning the provider may spend.
    pub reasoning: Option<ReasoningLevel>,
    /// A stable key for the cached prompt prefix of this session.
    pub cache_key: Option<Text>,
    /// The maximum number of provider calls, after which the run stops.
    pub max_turns: Option<u32>,
}

/// Drives one provider through a conversation until the model calls no tool
/// or a cap is reached.
pub struct Agent<'a> {
    provider: &'a dyn Provider,
    registry: &'a Registry,
    config: AgentConfig,
    token: CancellationToken,
    policies: Vec<Box<dyn TurnPolicy>>,
    mask: Option<Mask>,
    workspace: Option<&'a dyn Workspace>,
    fingerprint: Option<u64>,
    provider_name: Text,
    model: Text,
}

impl<'a> Agent<'a> {
    /// Creates the loop for `provider`, its `registry`, `config` and a
    /// cancellation token shared with the caller.
    pub fn new(
        provider: &'a dyn Provider,
        registry: &'a Registry,
        config: AgentConfig,
        token: CancellationToken,
    ) -> Self {
        Self {
            provider,
            registry,
            token,
            policies: Vec::new(),
            mask: None,
            workspace: None,
            fingerprint: None,
            provider_name: Text::from(provider.name()),
            model: Text::from(config.model.as_str()),
            config,
        }
    }

    /// Creates the loop with a set of policies run after every turn.
    ///
    /// With no policies (the default, see [`Agent::new`]) the loop behaves
    /// exactly as without them.
    pub fn with_policies(
        provider: &'a dyn Provider,
        registry: &'a Registry,
        config: AgentConfig,
        token: CancellationToken,
        policies: Vec<Box<dyn TurnPolicy>>,
    ) -> Self {
        Self {
            provider,
            registry,
            token,
            policies,
            mask: None,
            workspace: None,
            fingerprint: None,
            provider_name: Text::from(provider.name()),
            model: Text::from(config.model.as_str()),
            config,
        }
    }

    /// Turns on observation masking: before each request, old large tool
    /// outputs in `history` become a stub (see [`Mask`]). Off by default.
    ///
    /// Only the in-memory history changes; the session file is not touched.
    #[must_use]
    pub fn with_mask(mut self) -> Self {
        self.mask = Some(Mask);
        self
    }

    /// Watches `workspace` so policies learn whether a turn changed files
    /// (see [`TurnView::changed_files`]). Off by default.
    #[must_use]
    pub fn with_workspace(mut self, workspace: &'a dyn Workspace) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Runs the conversation until a reply has no tool call, a cap is reached
    /// or the run is cancelled.
    ///
    /// Appends the assistant replies and tool results to `history`, records one
    /// [`crate::session::Entry`] per message in `session` (when given, flushed
    /// at each turn end) and reports streamed [`Event`]s on `events`. After
    /// each normal turn the configured policies run and may inject a message
    /// or stop the run; with no policies the loop acts exactly as without them.
    pub async fn run(
        &mut self,
        history: &mut Vec<Message>,
        mut session: Option<&mut Session>,
        events: &mpsc::Sender<Event>,
    ) {
        let started = std::time::Instant::now();
        let mut turns: u32 = 0;
        let mut max_tokens_streak: u32 = 0;
        self.fingerprint = match self.workspace {
            Some(workspace) if !self.policies.is_empty() => workspace.fingerprint().await,
            _ => None,
        };
        let mut run_reason = loop {
            if self.config.max_turns.is_some_and(|cap| turns >= cap) {
                break TurnEndReason::MaxTurns;
            }
            if self.token.is_cancelled() || emit(events, &self.token, Event::TurnStart).await {
                break TurnEndReason::Cancelled;
            }
            turns += 1;

            if let Some(mask) = &self.mask {
                mask.apply(history);
            }
            let mut outcome = self.run_turn(history, &mut session, events).await;
            // Every turn end flushes, whatever ended the turn.
            if let Some(session) = session.as_mut()
                && let Err(error) = session.flush().await
                && !matches!(outcome, TurnOutcome::Error(_))
            {
                outcome = TurnOutcome::Error(session_error(&error));
            }
            match outcome {
                TurnOutcome::Stopped(StopReason::MaxTokens)
                    if max_tokens_streak < MAX_TOKENS_RECOVERIES =>
                {
                    max_tokens_streak += 1;
                    outcome = self
                        .recover_from_max_tokens(history, &mut session, events)
                        .await;
                }
                TurnOutcome::Continue | TurnOutcome::Finish => max_tokens_streak = 0,
                _ => {}
            }

            let (turn_reason, run_end) = self
                .after_turn(outcome, turns, started, history, &mut session, events)
                .await;
            let closed = emit(events, &self.token, Event::TurnEnd(turn_reason)).await;
            if let Some(reason) = run_end {
                break reason;
            }
            if closed {
                break TurnEndReason::Cancelled;
            }
        };

        if self.token.is_cancelled() {
            run_reason = TurnEndReason::Cancelled;
        }
        emit(events, &self.token, Event::RunEnd(run_reason)).await;
    }

    /// Maps a turn outcome to its turn-end reason and whether the run stops.
    ///
    /// For the two normal outcomes (tools ran, or the model ended without a
    /// tool) the configured policies decide: an injected message appends to
    /// the last tool result or starts a new user message and the run
    /// continues, a stop ends it, and a no-tool turn with no injection ends.
    async fn after_turn(
        &mut self,
        outcome: TurnOutcome,
        index: u32,
        started: std::time::Instant,
        history: &mut Vec<Message>,
        session: &mut Option<&mut Session>,
        events: &mpsc::Sender<Event>,
    ) -> (TurnEndReason, Option<TurnEndReason>) {
        match outcome {
            TurnOutcome::Continue => {
                let run_end = self
                    .apply_policies(index, true, started, history, session, events)
                    .await;
                (TurnEndReason::EndTurn, run_end)
            }
            TurnOutcome::Finish => {
                let run_end = self
                    .apply_policies(index, false, started, history, session, events)
                    .await;
                (TurnEndReason::EndTurn, run_end)
            }
            TurnOutcome::Stopped(reason) => (
                TurnEndReason::Stopped(reason),
                Some(TurnEndReason::Stopped(reason)),
            ),
            TurnOutcome::Recovered => (TurnEndReason::Stopped(StopReason::MaxTokens), None),
            TurnOutcome::Cancelled => (TurnEndReason::Cancelled, Some(TurnEndReason::Cancelled)),
            TurnOutcome::Error(message) => {
                emit(events, &self.token, Event::Error(message)).await;
                (TurnEndReason::Error, Some(TurnEndReason::Error))
            }
        }
    }

    /// Adds the max_tokens note as a new user message so the next turn runs.
    ///
    /// The note never edits an earlier message: the prompt cache and the
    /// replayed thinking blocks stay valid.
    async fn recover_from_max_tokens(
        &mut self,
        history: &mut Vec<Message>,
        session: &mut Option<&mut Session>,
        events: &mpsc::Sender<Event>,
    ) -> TurnOutcome {
        let message = Message::user(MAX_TOKENS_NOTE);
        if let Some(session) = session.as_mut()
            && let Err(error) = session.append(&message).await
        {
            return TurnOutcome::Error(session_error(&error));
        }
        history.push(message);
        if emit(
            events,
            &self.token,
            Event::PolicyNote(Text::from(MAX_TOKENS_NOTE)),
        )
        .await
        {
            return TurnOutcome::Cancelled;
        }
        TurnOutcome::Recovered
    }

    /// Runs the policies after one normal turn and applies what they asked.
    ///
    /// Returns the run-end reason when the run stops after this turn.
    ///
    /// An injected user message (after a no-tool turn) is emitted as a
    /// `PolicyNote` and, when a session is open, appended to it like any other
    /// message. An injected text on a tool turn is appended in place to the
    /// last tool result, which was already recorded to the session as that
    /// result; the append is not re-recorded because a session is append-only.
    async fn apply_policies(
        &mut self,
        index: u32,
        ran_tools: bool,
        started: std::time::Instant,
        history: &mut Vec<Message>,
        session: &mut Option<&mut Session>,
        events: &mpsc::Sender<Event>,
    ) -> Option<TurnEndReason> {
        if self.policies.is_empty() {
            // No policies: a tool turn continues, a no-tool turn ends.
            return if ran_tools {
                None
            } else {
                Some(TurnEndReason::EndTurn)
            };
        }

        let changed_files = match self.workspace {
            None => None,
            Some(_) if !ran_tools => Some(false),
            Some(workspace) => {
                let now = workspace.fingerprint().await;
                let changed = match (self.fingerprint, now) {
                    (Some(prev), Some(now)) => Some(prev != now),
                    _ => None,
                };
                self.fingerprint = now;
                changed
            }
        };

        let elapsed = started.elapsed();
        let (injected, notes, continue_run) = {
            let (assistant, tools) = last_turn(history);
            let view = TurnView {
                index,
                elapsed,
                ran_tools,
                assistant,
                tools: &tools,
                changed_files,
            };
            let run = PolicyRun::new(&mut self.policies, &view);
            let continue_run = run.continue_run();
            (run.injected.clone(), run.notes, continue_run)
        };

        // When a policy stopped the run, any other policy's injected text is
        // discarded, so no note for it reaches the UI.
        if !continue_run {
            return Some(TurnEndReason::EndTurn);
        }
        if let Some(text) = injected {
            for note in &notes {
                emit(events, &self.token, note.clone()).await;
            }
            if ran_tools {
                append_to_last_tool_result(history, &text);
            } else {
                let message = Message::user(text.clone());
                if let Some(session) = session.as_mut()
                    && let Err(error) = session.append(&message).await
                {
                    emit(events, &self.token, Event::Error(session_error(&error))).await;
                }
                history.push(message);
            }
        }
        None
    }

    /// Streams one provider reply, records it and any tool results, and reports
    /// how the turn ended.
    async fn run_turn(
        &self,
        history: &mut Vec<Message>,
        session: &mut Option<&mut Session>,
        events: &mpsc::Sender<Event>,
    ) -> TurnOutcome {
        let mut assembler = Assembler::new();
        // How many retries already ran, capped at [`crate::retry::MAX_RETRIES`].
        let mut retries: u32 = 0;
        // How many empty replies were already requested again.
        let mut empty_retries: u32 = 0;
        let mut message = loop {
            // None when the stream ended cleanly, else the error that stopped it
            // and, on the last allowed attempt, ends the turn.
            let mut stop_error: Option<Error> = None;
            loop {
                let outcome = self.consume_stream(history, events, &mut assembler).await;
                match outcome {
                    ConsumeOutcome::Cancelled | ConsumeOutcome::Finished => break,
                    ConsumeOutcome::StreamError(error) => {
                        if retries >= crate::retry::MAX_RETRIES || error.retry_hint().is_none() {
                            stop_error = Some(error);
                            break;
                        }
                        // A retry resets the partial reply but pushes nothing to
                        // history, so a retry never re-sends a finished message.
                        let wait = error.retry_hint().flatten();
                        let delay = crate::retry::delay(retries, wait, crate::retry::jitter());
                        let event = Event::Retry {
                            attempt: retries + 1,
                            delay_ms: delay.as_millis() as u64,
                            error: Text::from(error.to_string()),
                        };
                        if emit(events, &self.token, event).await {
                            return TurnOutcome::Cancelled;
                        }
                        tokio::select! {
                            () = tokio::time::sleep(delay) => {}
                            () = self.token.cancelled() => return TurnOutcome::Cancelled,
                        }
                        assembler = Assembler::new();
                        retries += 1;
                    }
                }
            }
            if let Some(error) = stop_error {
                return TurnOutcome::Error(Text::from(error.to_string()));
            }

            let message = assembler.finish(self.provider_name.clone(), self.model.clone());
            // Gateways now and then send a reply with no content at all, though
            // they bill output tokens for it. The same request usually answers.
            if message.content.is_empty()
                && message.stop_reason != Some(StopReason::Refusal)
                && empty_retries < EMPTY_RETRIES
                && !self.token.is_cancelled()
            {
                let delay = crate::retry::delay(empty_retries, None, crate::retry::jitter());
                let event = Event::Retry {
                    attempt: empty_retries + 1,
                    delay_ms: delay.as_millis() as u64,
                    error: Text::from("empty reply"),
                };
                if emit(events, &self.token, event).await {
                    return TurnOutcome::Cancelled;
                }
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = self.token.cancelled() => return TurnOutcome::Cancelled,
                }
                assembler = Assembler::new();
                empty_retries += 1;
                continue;
            }
            break message;
        };

        // An empty reply is never pushed. A refusal with no content is still
        // a refusal, not an "empty reply" error, and pushes nothing.
        if message.content.is_empty() {
            if message.stop_reason == Some(StopReason::Refusal) {
                return TurnOutcome::Stopped(StopReason::Refusal);
            }
            if emit(events, &self.token, Event::Error(Text::from("empty reply"))).await {
                return TurnOutcome::Cancelled;
            }
            return TurnOutcome::Stopped(StopReason::Error);
        }

        match message.stop_reason {
            Some(StopReason::ToolUse) => {
                // The reply is recorded once, then borrowed back from history
                // for its tool calls; no message is cloned.
                if let Err(error) = record(Message::assistant(message), history, session).await {
                    return TurnOutcome::Error(error);
                }
                let calls = match history.last() {
                    Some(Message::Assistant(assistant)) => tool_calls(assistant),
                    _ => Vec::new(),
                };
                let results = crate::batch::run_all(&calls, self.registry, &self.token).await;
                // Every result reaches history before any event is sent, so a
                // cancel or a closed receiver cannot leave a call unanswered.
                let mut failure = None;
                for result in &results {
                    if let Err(error) = record(result.clone(), history, session).await {
                        history.push(result.clone());
                        failure.get_or_insert(error);
                    }
                }
                if let Some(error) = failure {
                    return TurnOutcome::Error(error);
                }
                for (call, result) in calls.iter().zip(&results) {
                    let event = Event::ToolResult {
                        id: call.id.clone(),
                        is_error: tool_error(result),
                        bytes: tool_bytes(result),
                    };
                    if emit(events, &self.token, event).await {
                        return TurnOutcome::Cancelled;
                    }
                }
                if self.token.is_cancelled() {
                    return TurnOutcome::Cancelled;
                }
                TurnOutcome::Continue
            }
            Some(StopReason::EndTurn) => {
                match record(Message::assistant(message), history, session).await {
                    Ok(()) => TurnOutcome::Finish,
                    Err(error) => TurnOutcome::Error(error),
                }
            }
            // MaxTokens, Refusal, Error, or no stop at all: never run tools.
            // A thinking block cut off before its signature cannot be sent back.
            reason => {
                message.content.retain(|content| {
                    !matches!(
                        content,
                        Content::ToolCall(_)
                            | Content::Thinking {
                                signature: None,
                                ..
                            }
                    )
                });
                let reason = reason.unwrap_or(StopReason::Error);
                if !message.content.is_empty()
                    && let Err(error) = record(Message::assistant(message), history, session).await
                {
                    return TurnOutcome::Error(error);
                }
                TurnOutcome::Stopped(reason)
            }
        }
    }

    /// Streams one provider reply into `assembler`, cancelling on the token.
    ///
    /// Nothing is pushed to history here; a reply is recorded only after the
    /// whole turn ends, so a caller may retry a failed stream safely.
    async fn consume_stream(
        &self,
        history: &[Message],
        events: &mpsc::Sender<Event>,
        assembler: &mut Assembler,
    ) -> ConsumeOutcome {
        let request = self.request(history);
        let mut stream = self.provider.stream(&request);
        loop {
            let item = tokio::select! {
                item = stream.next() => item,
                () = self.token.cancelled() => return ConsumeOutcome::Cancelled,
            };
            let Some(item) = item else {
                return ConsumeOutcome::Finished;
            };
            match item {
                Ok(event) => {
                    if let Some(event) = stream_event(&event)
                        && emit(events, &self.token, event).await
                    {
                        return ConsumeOutcome::Cancelled;
                    }
                    assembler.push(event);
                    // A finished tool call gains its name and arguments only
                    // once the assembler has them.
                    for ready in assembler.drain_ready() {
                        let event = Event::ToolCallReady {
                            id: ready.id,
                            name: ready.name,
                            arguments: ready.arguments,
                        };
                        if emit(events, &self.token, event).await {
                            return ConsumeOutcome::Cancelled;
                        }
                    }
                }
                Err(error) => return ConsumeOutcome::StreamError(error),
            }
        }
    }

    fn request<'h>(&'h self, history: &'h [Message]) -> Request<'h> {
        Request {
            model: &self.config.model,
            max_tokens: self.config.max_tokens,
            system: self.config.system.as_ref(),
            tools: self.registry.specs(),
            messages: history,
            reasoning: self.config.reasoning,
            cache_key: self.config.cache_key.as_ref(),
        }
    }
}

/// Appends `message` to the session, then to history. A session error leaves
/// history unchanged.
async fn record(
    message: Message,
    history: &mut Vec<Message>,
    session: &mut Option<&mut Session>,
) -> Result<(), Text> {
    if let Some(session) = session.as_mut() {
        session
            .append(&message)
            .await
            .map_err(|error| session_error(&error))?;
    }
    history.push(message);
    Ok(())
}

/// Borrows the assistant message of the last turn and pairs its tool calls
/// with the results that followed it.
///
/// The tool calls and results are cloned into owned pairs, but each one shares
/// its `Arc` text, so no message text is copied. A no-tool turn returns an
/// empty pair list.
fn last_turn(history: &[Message]) -> (&AssistantMessage, Vec<(ToolCall, ToolResult)>) {
    let assistant_index = history
        .iter()
        .rposition(|message| matches!(message, Message::Assistant(_)))
        .expect("a finished turn records an assistant message");
    let assistant = match &history[assistant_index] {
        Message::Assistant(assistant) => assistant,
        _ => unreachable!("rposition matched an assistant message"),
    };
    let mut tools = Vec::new();
    for message in &history[assistant_index + 1..] {
        let Message::ToolResult(result) = message else {
            break;
        };
        let call = assistant.content.iter().find_map(|content| match content {
            Content::ToolCall(call) if call.id == result.call_id => Some(call.clone()),
            _ => None,
        });
        if let Some(call) = call {
            tools.push((call, result.clone()));
        }
    }
    (assistant, tools)
}

/// Appends injected text to the content of the last tool result in `history`.
///
/// The message is the one the provider will send next, so the share of the
/// prompt cache before it stays valid; only this one message is re-serialized.
fn append_to_last_tool_result(history: &mut [Message], text: &str) {
    if let Some(Message::ToolResult(result)) = history.last_mut() {
        let mut content = result.content.to_string();
        content.push_str("\n\n");
        content.push_str(text);
        result.content = Text::from(content);
    }
}

fn session_error(error: &std::io::Error) -> Text {
    Text::from(format!("session write failed: {error}"))
}

impl std::fmt::Debug for Agent<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("provider", &self.provider.name())
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// The outcome of one provider stream within a turn.
enum ConsumeOutcome {
    /// The stream ended cleanly and `assembler` holds the finished reply.
    Finished,
    /// The run was cancelled (or the channel closed) mid-stream.
    Cancelled,
    /// The provider returned an error that might be retried.
    StreamError(Error),
}

/// The outcome of one turn inside [`Agent::run`].
enum TurnOutcome {
    /// A tool call was run; another provider call may follow.
    Continue,
    /// A reply with no tool call ended the run normally.
    Finish,
    /// The provider stopped without asking for a tool.
    Stopped(StopReason),
    /// A max_tokens stop was answered with a note; the run goes on.
    Recovered,
    /// The run was cancelled mid-turn.
    Cancelled,
    /// The provider, a tool or the session reported an error.
    Error(Text),
}

/// Sends one event, waiting on the cancellation token; a closed receiver
/// counts as a cancel. Returns true when the run should stop as a cancel.
async fn emit(events: &mpsc::Sender<Event>, token: &CancellationToken, event: Event) -> bool {
    // Send first: after a cancel, an event with room in the channel is still
    // delivered, and only a full channel gives way to the token.
    tokio::select! {
        biased;
        result = events.send(event) => result.is_err(),
        () = token.cancelled() => true,
    }
}

/// Maps one streamed event to its channel event, if any.
///
/// The completed tool call is reported by the [`Assembler`] (which knows the
/// tool name and its assembled arguments), so `ToolCallEnd` maps to nothing
/// here.
fn stream_event(event: &StreamEvent) -> Option<Event> {
    Some(match event {
        StreamEvent::TextDelta(text) => Event::TextDelta(text.clone()),
        StreamEvent::ThinkingDelta(text) => Event::ThinkingDelta(text.clone()),
        StreamEvent::ToolCallStart { id, name } => Event::ToolStart {
            id: id.clone(),
            name: name.clone(),
        },
        StreamEvent::Usage(usage) => Event::Usage(*usage),
        _ => return None,
    })
}

/// The tool calls a finished assistant message asked the loop to run.
fn tool_calls(message: &AssistantMessage) -> Vec<ToolCall> {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

fn tool_error(message: &Message) -> bool {
    matches!(message, Message::ToolResult(result) if result.is_error)
}

fn tool_bytes(message: &Message) -> usize {
    match message {
        Message::ToolResult(result) => result.content.len(),
        _ => 0,
    }
}

/// A thinking block being assembled, which may carry only a signature.
#[derive(Default)]
struct OpenThinking {
    text: String,
    signature: Option<Text>,
}

/// One tool call being assembled from its argument deltas.
struct OpenTool {
    id: Text,
    name: Text,
    args: String,
}

/// A completed tool call, reported by the assembler for the `tool_call_ready`
/// event once its arguments are known.
struct ToolReady {
    id: Text,
    name: Text,
    arguments: Text,
}

/// Accumulates a [`StreamEvent`] sequence into an [`AssistantMessage`].
///
/// Accepts a [`StreamEvent::ThinkingSignature`] that arrives without a
/// preceding [`StreamEvent::ThinkingDelta`], because OpenAI Responses sends
/// only the signature for an empty reasoning summary.
struct Assembler {
    content: Vec<Content>,
    text: String,
    thinking: Option<OpenThinking>,
    tools: Vec<OpenTool>,
    ready: Vec<ToolReady>,
    stop_reason: Option<StopReason>,
    usage: Usage,
}

impl Assembler {
    fn new() -> Self {
        Self {
            content: Vec::new(),
            text: String::new(),
            thinking: None,
            tools: Vec::new(),
            ready: Vec::new(),
            stop_reason: None,
            usage: Usage::default(),
        }
    }

    fn push(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::TextDelta(delta) => {
                self.close_aux();
                self.text.push_str(&delta);
            }
            StreamEvent::ThinkingDelta(delta) => {
                self.open_thinking();
                self.thinking.get_or_insert_default().text.push_str(&delta);
            }
            StreamEvent::ThinkingSignature(signature) => {
                self.open_thinking();
                self.thinking.get_or_insert_default().signature = Some(signature);
            }
            StreamEvent::RedactedThinking(data) => {
                self.close_aux();
                self.content.push(Content::RedactedThinking { data });
            }
            StreamEvent::ToolCallStart { id, name } => {
                self.close_aux();
                self.tools.push(OpenTool {
                    id,
                    name,
                    args: String::new(),
                });
            }
            StreamEvent::ToolCallDelta { id, partial_json } => {
                // An empty id delta means the current call in some providers.
                let index = self
                    .tools
                    .iter()
                    .rposition(|tool| *tool.id == *id)
                    .or_else(|| {
                        id.is_empty()
                            .then(|| self.tools.len().checked_sub(1))
                            .flatten()
                    });
                if let Some(index) = index {
                    self.tools[index].args.push_str(&partial_json);
                }
            }
            StreamEvent::ToolCallEnd { id } => {
                if let Some(index) = self.tools.iter().position(|tool| *tool.id == *id) {
                    let tool = self.tools.remove(index);
                    // A call with no or only-whitespace arguments is an empty object.
                    let arguments = if tool.args.trim().is_empty() {
                        Text::from("{}")
                    } else {
                        Text::from(crate::repair::arguments(tool.args))
                    };
                    self.ready.push(ToolReady {
                        id: tool.id.clone(),
                        name: tool.name.clone(),
                        arguments: arguments.clone(),
                    });
                    self.content.push(Content::ToolCall(ToolCall {
                        id: tool.id,
                        name: tool.name,
                        arguments,
                    }));
                }
            }
            StreamEvent::Usage(usage) => self.usage = usage,
            StreamEvent::Stop(reason) => self.stop_reason = Some(reason),
        }
    }

    /// Opens a thinking block, closing the current one if it already carries a
    /// signature so a second reasoning item starts its own block.
    fn open_thinking(&mut self) {
        let closed = self
            .thinking
            .as_ref()
            .is_some_and(|t| t.signature.is_some());
        if closed {
            self.close_aux();
        } else {
            self.close_text();
        }
        self.thinking.get_or_insert_default();
    }

    /// Closes any open thinking block or text block into the content.
    fn close_aux(&mut self) {
        if let Some(thinking) = self.thinking.take() {
            self.content.push(Content::Thinking {
                thinking: Text::from(thinking.text),
                signature: thinking.signature,
            });
            return;
        }
        self.close_text();
    }

    /// Closes an open text block into the content, if it has text.
    fn close_text(&mut self) {
        let text = std::mem::take(&mut self.text);
        if !text.is_empty() {
            self.content.push(Content::Text {
                text: Text::from(text),
            });
        }
    }

    /// The tool calls that completed since the last drain, for the
    /// `tool_call_ready` events. Draining clears them.
    fn drain_ready(&mut self) -> Vec<ToolReady> {
        std::mem::take(&mut self.ready)
    }

    fn finish(mut self, provider: Text, model: Text) -> AssistantMessage {
        self.close_aux();
        AssistantMessage {
            content: self.content,
            provider,
            model,
            stop_reason: self.stop_reason,
            usage: self.usage,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use reagent_ai::Error;
    use reagent_ai::fake::FakeProvider;
    use reagent_ai::{Role, ToolSpec};
    use serde::Deserialize;
    use serde_json::json;

    use crate::session::Session;
    use crate::tool::{Tool, ToolOutput, parse_args};

    #[derive(Deserialize)]
    struct EchoArgs<'a> {
        #[serde(borrow)]
        message: Option<Cow<'a, str>>,
    }

    struct Echo {
        spec: ToolSpec,
    }

    impl Tool for Echo {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn read_only(&self) -> bool {
            true
        }

        fn run<'a>(
            &'a self,
            args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async move {
                match parse_args::<EchoArgs<'a>>(args) {
                    Ok(arguments) => {
                        ToolOutput::ok(arguments.message.map(Cow::into_owned).unwrap_or_default())
                    }
                    Err(output) => output,
                }
            })
        }
    }

    fn echo_spec() -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "Echo a message.".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry.add(Echo { spec: echo_spec() });
        registry
    }

    /// A read-only tool that never finishes on its own, so a test can cancel it.
    struct Slow {
        spec: ToolSpec,
    }

    impl Tool for Slow {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn read_only(&self) -> bool {
            true
        }

        fn run<'a>(
            &'a self,
            _args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_secs(10_000)).await;
                ToolOutput::ok("never")
            })
        }
    }

    fn registry_with_slow_tool() -> Registry {
        let mut registry = Registry::new();
        registry.add(Slow {
            spec: ToolSpec {
                name: "slow".into(),
                description: "Never finishes.".into(),
                input_schema: json!({"type": "object"}),
            },
        });
        registry
    }

    fn text_reply(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta(Text::from(text)),
            StreamEvent::Stop(StopReason::EndTurn),
        ]
    }

    fn tool_reply(call_id: &str, name: &str, args: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallStart {
                id: Text::from(call_id),
                name: Text::from(name),
            },
            StreamEvent::ToolCallDelta {
                id: Text::from(call_id),
                partial_json: Text::from(args),
            },
            StreamEvent::ToolCallEnd {
                id: Text::from(call_id),
            },
            StreamEvent::Stop(StopReason::ToolUse),
        ]
    }

    fn config() -> AgentConfig {
        AgentConfig {
            model: "fake-model".into(),
            max_tokens: 128,
            system: None,
            reasoning: None,
            cache_key: None,
            max_turns: None,
        }
    }

    fn drain(tx: mpsc::Sender<Event>, rx: &mut mpsc::Receiver<Event>) -> Vec<Event> {
        drop(tx);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn tool_calls_are_extracted_from_a_reply() {
        let message = AssistantMessage::new(
            "fake",
            "m",
            vec![
                Content::text("hi"),
                Content::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "echo".into(),
                    arguments: Text::from("{}"),
                }),
            ],
        );
        let calls = tool_calls(&message);
        assert_eq!(calls.len(), 1);
        assert_eq!(&*calls[0].name, "echo");
    }

    #[tokio::test]
    async fn runs_until_a_reply_has_no_tool_call() {
        let provider = FakeProvider::new(vec![
            tool_reply("call_1", "echo", r#"{"message":"hi"}"#),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(provider.seen().len(), 2, "two turns, two requests");
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].role(), Role::User);
        assert_eq!(history[1].role(), Role::Assistant);
        assert_eq!(history[2].role(), Role::ToolResult);
        assert_eq!(history[3].role(), Role::Assistant);

        let events = drain(tx, &mut rx);
        assert_eq!(events.first(), Some(&Event::TurnStart));
        // Final events: the last TurnEnd follows an end-turn reply, then RunEnd.
        let end_index = events
            .iter()
            .rposition(|event| matches!(event, Event::TurnEnd(_)));
        assert!(end_index.is_some());
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
        assert!(
            events.iter().any(
                |event| matches!(event, Event::ToolStart { id, .. } if id.as_ref() == "call_1")
            )
        );
        assert!(events.iter().any(|event| matches!(
            event,
            Event::ToolCallReady { id, name, arguments }
                if id.as_ref() == "call_1"
                    && name.as_ref() == "echo"
                    && arguments.as_ref() == r#"{"message":"hi"}"#
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::ToolResult { id, is_error, .. } if id.as_ref() == "call_1" && !is_error))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retries_a_retryable_error_then_succeeds() {
        let provider = FakeProvider::new(vec![]);
        provider.push_error(Error::Http("refused".into()));
        provider.push_reply(text_reply("done"));
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(provider.seen().len(), 2, "one retry after the failure");
        let events = drain(tx, &mut rx);
        let retries = events
            .iter()
            .filter(|event| matches!(event, Event::Retry { attempt, .. } if *attempt == 1))
            .count();
        assert_eq!(retries, 1, "one Retry event, events: {events:?}");
        // The successful retry recorded its assistant message.
        assert_eq!(history.last().map(Message::role), Some(Role::Assistant));
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
    }

    #[tokio::test(start_paused = true)]
    async fn eleven_failures_end_the_run_with_the_error() {
        // First attempt plus MAX_RETRIES=10 retries = 11 provider calls, then
        // the run ends with the error.
        let provider = FakeProvider::new(vec![]);
        for _ in 0..11 {
            provider.push_error(Error::Http("refused".into()));
        }
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(
            provider.seen().len(),
            11,
            "the first attempt plus ten retries"
        );
        assert_eq!(history.len(), 1, "no assistant message on failure");
        let events = drain(tx, &mut rx);
        let retries = events
            .iter()
            .filter(|event| matches!(event, Event::Retry { .. }))
            .count();
        assert_eq!(retries, 10, "ten retries before giving up: {events:?}");
        assert!(
            events.iter().any(|event| matches!(event, Event::Error(_))),
            "the turn reports the error"
        );
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::Error)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_non_retryable_error_is_not_retried() {
        let provider = FakeProvider::new(vec![]);
        provider.push_error(Error::Protocol("bad request".into()));
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(
            provider.seen().len(),
            1,
            "a terminal error is never retried"
        );
        let events = drain(tx, &mut rx);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Retry { .. })),
            "no Retry event: {events:?}"
        );
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::Error)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_during_backoff_stops_the_run_as_cancelled() {
        let provider = FakeProvider::new(vec![]);
        provider.push_error(Error::Http("refused".into()));
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let token = CancellationToken::new();
        let cancellation = token.clone();
        let mut history = vec![Message::user("hello")];

        let handle = tokio::spawn(async move {
            Agent::new(&provider, &registry, config(), token)
                .run(&mut history, None, &tx)
                .await;
            history
        });
        // Let the first attempt fail and the backoff sleep begin.
        tokio::task::yield_now().await;
        cancellation.cancel();
        let history = tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("the loop stops after cancel")
            .expect("runner finishes");
        assert_eq!(
            history.len(),
            1,
            "no assistant message on a cancelled retry (only the seed user message)"
        );
    }

    // Paused time: the retries before the error ends the run sleep at once.
    #[tokio::test(start_paused = true)]
    async fn a_stream_error_ends_the_run() {
        struct Failing {
            error: std::sync::Mutex<Option<Error>>,
        }

        impl Provider for Failing {
            fn name(&self) -> &'static str {
                "failing"
            }

            fn stream(
                &self,
                _request: &Request,
            ) -> futures_util::stream::BoxStream<'static, Result<StreamEvent, Error>> {
                let error = self
                    .error
                    .lock()
                    .expect("error lock")
                    .take()
                    .unwrap_or(Error::Http("refused".into()));
                Box::pin(futures_util::stream::iter([Err(error)]))
            }
        }

        let provider = Failing {
            error: std::sync::Mutex::new(Some(Error::Http("refused".into()))),
        };
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(history.len(), 1, "no assistant message on failure");
        let events = drain(tx, &mut rx);
        let error_count = events
            .iter()
            .filter(|event| matches!(event, Event::Error(_)))
            .count();
        assert_eq!(error_count, 1, "exactly one Error event");
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::Error)));
    }

    #[tokio::test]
    async fn assembles_thinking_then_a_signature() {
        let provider = FakeProvider::new(vec![vec![
            StreamEvent::ThinkingDelta(Text::from("reason ")),
            StreamEvent::ThinkingSignature(Text::from("sig")),
            StreamEvent::TextDelta(Text::from("answer")),
            StreamEvent::Stop(StopReason::EndTurn),
        ]]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = Vec::new();

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(history.len(), 1);
        let Message::Assistant(message) = &history[0] else {
            panic!("expected an assistant message");
        };
        assert!(matches!(
            &message.content[0],
            Content::Thinking {
                thinking,
                signature: Some(sig),
            } if thinking.as_ref() == "reason " && sig.as_ref() == "sig"
        ));
    }

    #[tokio::test]
    async fn assembles_a_signature_only_block_for_an_empty_summary() {
        let provider = FakeProvider::new(vec![vec![
            StreamEvent::ThinkingSignature(Text::from("empty")),
            StreamEvent::TextDelta(Text::from("hi")),
            StreamEvent::Stop(StopReason::EndTurn),
        ]]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = Vec::new();

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(history.len(), 1);
        let Message::Assistant(message) = &history[0] else {
            panic!("expected an assistant message");
        };
        assert!(matches!(
            &message.content[0],
            Content::Thinking {
                thinking,
                signature: Some(sig),
            } if thinking.is_empty() && sig.as_ref() == "empty"
        ));
    }

    #[tokio::test]
    async fn two_reasoning_items_each_keep_their_own_signature() {
        let provider = FakeProvider::new(vec![vec![
            StreamEvent::ThinkingDelta(Text::from("first ")),
            StreamEvent::ThinkingSignature(Text::from("sig1")),
            StreamEvent::ThinkingDelta(Text::from("second ")),
            StreamEvent::ThinkingSignature(Text::from("sig2")),
            StreamEvent::TextDelta(Text::from("answer")),
            StreamEvent::Stop(StopReason::EndTurn),
        ]]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = Vec::new();

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let Message::Assistant(message) = &history[0] else {
            panic!("expected an assistant message");
        };
        assert_eq!(message.content.len(), 3);
        assert!(matches!(
            &message.content[0],
            Content::Thinking {
                thinking,
                signature: Some(sig),
            } if thinking.as_ref() == "first " && sig.as_ref() == "sig1"
        ));
        assert!(matches!(
            &message.content[1],
            Content::Thinking {
                thinking,
                signature: Some(sig),
            } if thinking.as_ref() == "second " && sig.as_ref() == "sig2"
        ));
    }

    #[tokio::test]
    async fn assembles_a_redacted_thinking_block() {
        let provider = FakeProvider::new(vec![vec![
            StreamEvent::RedactedThinking(Text::from("opaque")),
            StreamEvent::TextDelta(Text::from("hi")),
            StreamEvent::Stop(StopReason::EndTurn),
        ]]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = Vec::new();

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(history.len(), 1);
        let Message::Assistant(message) = &history[0] else {
            panic!("expected an assistant message");
        };
        assert!(matches!(
            &message.content[0],
            Content::RedactedThinking { data } if data.as_ref() == "opaque"
        ));
    }

    #[tokio::test]
    async fn an_unknown_tool_call_gets_an_error_result() {
        let provider = FakeProvider::new(vec![tool_reply("call_1", "nope", "{}")]);
        let registry = Registry::new();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        // history: user, assistant(tool call), tool result. The exhausted fake's
        // empty second reply is never pushed.
        assert_eq!(history.len(), 3);
        let Message::ToolResult(result) = &history[2] else {
            panic!("expected a tool result");
        };
        assert!(result.is_error);
        assert!(result.content.starts_with("unknown tool"));
    }

    #[tokio::test]
    async fn a_call_with_no_arguments_becomes_an_empty_object() {
        // ToolCallStart + ToolCallEnd with no delta in between.
        let provider = FakeProvider::new(vec![vec![
            StreamEvent::ToolCallStart {
                id: Text::from("call_1"),
                name: Text::from("echo"),
            },
            StreamEvent::ToolCallEnd {
                id: Text::from("call_1"),
            },
            StreamEvent::Stop(StopReason::ToolUse),
        ]]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = Vec::new();

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let Message::Assistant(message) = &history[0] else {
            panic!("expected an assistant message");
        };
        let Content::ToolCall(call) = &message.content[0] else {
            panic!("expected a tool call");
        };
        assert_eq!(call.arguments.as_ref(), "{}");
    }

    #[tokio::test]
    async fn an_empty_reply_stops_with_an_error_and_no_assistant() {
        // A reply with no events at all: nothing to push.
        let provider = FakeProvider::new(vec![vec![]]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(history.len(), 1, "empty reply is never pushed");
        let events = drain(tx, &mut rx);
        assert!(
            events.iter().any(
                |event| matches!(event, Event::Error(message) if message.as_ref() == "empty reply")
            ),
            "an empty reply reports 'empty reply'"
        );
        assert_eq!(
            events.last(),
            Some(&Event::RunEnd(TurnEndReason::Stopped(StopReason::Error)))
        );
    }

    #[tokio::test]
    async fn repaired_arguments_go_into_history() {
        let provider = FakeProvider::new(vec![
            tool_reply("c1", "echo", r#""{\"message\":\"hi\"}""#),
            vec![
                StreamEvent::TextDelta(Text::from("done")),
                StreamEvent::Stop(StopReason::EndTurn),
            ],
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let Message::Assistant(message) = &history[1] else {
            panic!("expected an assistant message");
        };
        let Content::ToolCall(call) = &message.content[0] else {
            panic!("expected a tool call");
        };
        assert_eq!(call.arguments.as_ref(), r#"{"message":"hi"}"#);
        let Message::ToolResult(result) = &history[2] else {
            panic!("expected a tool result");
        };
        assert!(!result.is_error, "{}", result.content);
    }

    #[tokio::test]
    async fn an_empty_reply_is_retried_then_answers() {
        let provider = FakeProvider::new(vec![
            vec![],
            vec![
                StreamEvent::TextDelta(Text::from("done")),
                StreamEvent::Stop(StopReason::EndTurn),
            ],
        ]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(provider.seen().len(), 2);
        assert_eq!(history.len(), 2, "only the answer is pushed");
        let events = drain(tx, &mut rx);
        let retries = events
            .iter()
            .filter(|event| matches!(event, Event::Retry { error, .. } if error.as_ref() == "empty reply"))
            .count();
        assert_eq!(retries, 1, "events: {events:?}");
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
    }

    #[tokio::test]
    async fn three_empty_replies_stop_the_run() {
        let provider = FakeProvider::new(vec![vec![], vec![], vec![], vec![]]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(provider.seen().len(), 3);
        assert_eq!(history.len(), 1);
        let events = drain(tx, &mut rx);
        assert!(
            events.iter().any(
                |event| matches!(event, Event::Error(message) if message.as_ref() == "empty reply")
            ),
            "events: {events:?}"
        );
        assert_eq!(
            events.last(),
            Some(&Event::RunEnd(TurnEndReason::Stopped(StopReason::Error)))
        );
    }

    #[tokio::test]
    async fn max_tokens_stops_without_running_tools() {
        // A MaxTokens reply with a partial tool call and some text, 4 times
        // so the recoveries run out: each tool call is dropped, the text is
        // kept, and no tool runs.
        let reply = vec![
            StreamEvent::TextDelta(Text::from("partial ")),
            StreamEvent::ToolCallStart {
                id: Text::from("call_1"),
                name: Text::from("echo"),
            },
            StreamEvent::ToolCallDelta {
                id: Text::from("call_1"),
                partial_json: Text::from("{\"message\":"),
            },
            StreamEvent::ToolCallEnd {
                id: Text::from("call_1"),
            },
            StreamEvent::Stop(StopReason::MaxTokens),
        ];
        let provider = FakeProvider::new(vec![reply; 4]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = Vec::new();

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        // Only the text replies are kept, each followed by a recovery note
        // except the last; no tool result, no ToolResult event.
        assert_eq!(history.len(), 7, "{history:?}");
        for message in history.iter().step_by(2) {
            let Message::Assistant(message) = message else {
                panic!("expected an assistant message, got {message:?}");
            };
            assert_eq!(message.content.len(), 1, "the tool call is dropped");
            assert!(
                !message
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::ToolCall(_)))
            );
        }

        let events = drain(tx, &mut rx);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::ToolResult { .. })),
            "no tool ran on a max-tokens stop"
        );
        assert_eq!(
            events.last(),
            Some(&Event::RunEnd(TurnEndReason::Stopped(
                StopReason::MaxTokens
            )))
        );
    }

    #[tokio::test]
    async fn refusal_stops_without_running_tools() {
        let provider = FakeProvider::new(vec![vec![
            StreamEvent::TextDelta(Text::from("refused")),
            StreamEvent::ToolCallStart {
                id: Text::from("call_1"),
                name: Text::from("echo"),
            },
            StreamEvent::ToolCallEnd {
                id: Text::from("call_1"),
            },
            StreamEvent::Stop(StopReason::Refusal),
        ]]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = Vec::new();

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let Message::Assistant(message) = &history[0] else {
            panic!("expected an assistant message");
        };
        assert!(
            !message
                .content
                .iter()
                .any(|content| matches!(content, Content::ToolCall(_)))
        );
        let events = drain(tx, &mut rx);
        assert_eq!(
            events.last(),
            Some(&Event::RunEnd(TurnEndReason::Stopped(StopReason::Refusal)))
        );
    }

    #[tokio::test]
    async fn a_refusal_with_no_content_is_a_refusal_not_an_empty_reply() {
        // A refusal sends no text: the turn must end as a refusal, with no
        // "empty reply" error and no assistant message pushed to history.
        let provider = FakeProvider::new(vec![vec![StreamEvent::Stop(StopReason::Refusal)]]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(history.len(), 1, "a refusal is never pushed");
        let events = drain(tx, &mut rx);
        assert!(
            !events.iter().any(
                |event| matches!(event, Event::Error(message) if message.as_ref() == "empty reply")
            ),
            "a refusal reports no 'empty reply' error: {events:?}"
        );
        assert_eq!(
            events.last(),
            Some(&Event::RunEnd(TurnEndReason::Stopped(StopReason::Refusal)))
        );
    }

    #[tokio::test]
    async fn a_tool_only_max_tokens_reply_pushes_nothing() {
        // Only a tool call, stopped at max tokens: after dropping it nothing is
        // left, so no assistant message is pushed. The 4 replies use up the
        // recoveries, so only the 3 recovery notes remain.
        let reply = vec![
            StreamEvent::ToolCallStart {
                id: Text::from("call_1"),
                name: Text::from("echo"),
            },
            StreamEvent::ToolCallEnd {
                id: Text::from("call_1"),
            },
            StreamEvent::Stop(StopReason::MaxTokens),
        ];
        let provider = FakeProvider::new(vec![reply; 4]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = Vec::new();

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(history, vec![Message::user(MAX_TOKENS_NOTE); 3]);
    }

    #[tokio::test]
    async fn max_turns_stops_the_run_with_valid_history() {
        let provider = FakeProvider::new(vec![
            tool_reply("call_1", "echo", r#"{"message":"hi"}"#),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];
        let mut config = config();
        config.max_turns = Some(1);

        Agent::new(&provider, &registry, config, CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        // Only one provider call happened; its tool call got a result.
        assert_eq!(provider.seen().len(), 1);
        let calls = history
            .iter()
            .filter(|message| {
                matches!(message, Message::Assistant(a) if a.content.iter().any(|c| matches!(c, Content::ToolCall(_))))
            })
            .count();
        let results = history
            .iter()
            .filter(|message| matches!(message, Message::ToolResult(_)))
            .count();
        assert_eq!(calls, 1);
        assert_eq!(results, 1, "the tool call keeps its result");
        let events = drain(tx, &mut rx);
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::MaxTurns)));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_stops_a_pending_stream_within_time() {
        use futures_util::stream;

        struct Pending;

        impl Provider for Pending {
            fn name(&self) -> &'static str {
                "pending"
            }

            fn stream(
                &self,
                _request: &Request,
            ) -> futures_util::stream::BoxStream<'static, Result<StreamEvent, Error>> {
                Box::pin(stream::pending())
            }
        }

        let provider = Pending;
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = Vec::new();
        let token = CancellationToken::new();
        let cancellation = token.clone();

        let handle = tokio::spawn(async move {
            Agent::new(&provider, &registry, config(), token)
                .run(&mut history, None, &tx)
                .await;
            history
        });
        tokio::task::yield_now().await;
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("the loop stops after cancel")
            .expect("runner finishes");
        // No partial assistant message is pushed on a cancelled stream.
        assert!(result.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_keeps_every_tool_call_resolved() {
        let provider = FakeProvider::new(vec![tool_reply("call_1", "slow", "{}")]);
        let registry = registry_with_slow_tool();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];
        let token = CancellationToken::new();
        let cancellation = token.clone();

        let handle = tokio::spawn(async move {
            Agent::new(&provider, &registry, config(), token)
                .run(&mut history, None, &tx)
                .await;
            history
        });
        // Let the agent read the reply and start the slow tool before cancelling.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        cancellation.cancel();
        let history = tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("the loop stops after cancel")
            .expect("runner finishes");

        let calls = history
            .iter()
            .filter(|message| matches!(message, Message::Assistant(a) if a.content.iter().any(|c| matches!(c, Content::ToolCall(_)))))
            .count();
        let results = history
            .iter()
            .filter(|message| matches!(message, Message::ToolResult(_)))
            .count();
        assert!(calls >= 1, "the reply asked for a tool call");
        assert_eq!(calls, results, "every tool call has a result");
        let Message::ToolResult(result) = &history[2] else {
            panic!("expected the cancelled result");
        };
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn dropped_receiver_stops_before_a_provider_call() {
        let provider = FakeProvider::new(vec![text_reply("hi")]);
        let registry = registry();
        // Build a sender, drop it and every receiver, then hand the loop the
        // orphaned sender: its first send fails and the run stops.
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let mut history = Vec::new();

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert!(
            provider.seen().is_empty(),
            "a closed receiver stops the run before the provider call"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_when_channel_full_stops_quickly() {
        let provider = FakeProvider::new(vec![vec![StreamEvent::TextDelta(Text::from("x"))]]);
        let registry = registry();
        // A capacity-1 channel nobody drains: the loop fills it and then blocks
        // on sending, so only the token can unblock it. Cancel must stop it.
        let (tx, rx) = mpsc::channel(1);
        let token = CancellationToken::new();
        let cancellation = token.clone();
        let mut history = Vec::new();

        let handle = tokio::spawn(async move {
            let _rx = rx;
            Agent::new(&provider, &registry, config(), token)
                .run(&mut history, None, &tx)
                .await;
            history
        });
        tokio::task::yield_now().await;
        cancellation.cancel();
        // The loop is blocked on a full channel; only cancellation frees it.
        let history = tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("cancel unblocks the full channel")
            .expect("runner finishes");
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn records_each_message_to_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("run.jsonl");
        let provider = FakeProvider::new(vec![
            tool_reply("call_1", "echo", r#"{"message":"hi"}"#),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];
        let mut session = Session::open(&path).await.expect("open session");

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, Some(&mut session), &tx)
            .await;
        session.close().await.expect("close session");

        let text = tokio::fs::read_to_string(&path)
            .await
            .expect("read session");
        let entries: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("parse entry"))
            .collect();
        // The session records the three messages the run added: assistant(tool
        // call), tool result and assistant(done). The seeded user message was
        // already in history and is not re-recorded.
        assert_eq!(entries.len(), 3);
        // A chain of ids: each entry's parent is the previous entry's id.
        for pair in entries.windows(2) {
            assert_eq!(pair[1]["parentId"], pair[0]["id"]);
        }
    }

    fn two_slow_calls() -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for id in ["call_1", "call_2"] {
            events.push(StreamEvent::ToolCallStart {
                id: Text::from(id),
                name: Text::from("slow"),
            });
            events.push(StreamEvent::ToolCallEnd { id: Text::from(id) });
        }
        events.push(StreamEvent::Stop(StopReason::ToolUse));
        events
    }

    /// Runs a reply with two slow calls, cancels while they run and returns
    /// the history and every event the loop sent.
    async fn cancel_two_slow_calls(
        session: Option<Session>,
    ) -> (Vec<Message>, Vec<Event>, Option<Session>) {
        let provider = FakeProvider::new(vec![two_slow_calls()]);
        let registry = registry_with_slow_tool();
        let (tx, mut rx) = mpsc::channel(64);
        let token = CancellationToken::new();
        let cancellation = token.clone();

        let handle = tokio::spawn(async move {
            let mut history = vec![Message::user("hello")];
            let mut session = session;
            Agent::new(&provider, &registry, config(), token)
                .run(&mut history, session.as_mut(), &tx)
                .await;
            (history, session, tx)
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancellation.cancel();
        let (history, session, tx) = tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("the loop stops after cancel")
            .expect("runner finishes");
        (history, drain(tx, &mut rx), session)
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_answers_every_call_of_a_parallel_batch() {
        for _ in 0..20 {
            let (history, _, _) = cancel_two_slow_calls(None).await;
            let results: Vec<_> = history
                .iter()
                .filter_map(|message| match message {
                    Message::ToolResult(result) => Some(&*result.call_id),
                    _ => None,
                })
                .collect();
            assert_eq!(results, ["call_1", "call_2"]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_still_sends_turn_end_and_run_end() {
        for _ in 0..20 {
            let (_, events, _) = cancel_two_slow_calls(None).await;
            let turn_ends = events
                .iter()
                .filter(|event| **event == Event::TurnEnd(TurnEndReason::Cancelled))
                .count();
            let run_ends: Vec<_> = events
                .iter()
                .filter(|event| matches!(event, Event::RunEnd(_)))
                .collect();
            assert_eq!(turn_ends, 1, "events: {events:?}");
            assert_eq!(run_ends, [&Event::RunEnd(TurnEndReason::Cancelled)]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_tool_turn_flushes_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("run.jsonl");
        let session = Session::open(&path).await.expect("open session");

        let (history, _, session) = cancel_two_slow_calls(Some(session)).await;

        // Read before close: only the turn-end flush can have written the lines.
        let text = tokio::fs::read_to_string(&path)
            .await
            .expect("read session");
        assert_eq!(text.lines().count(), history.len() - 1, "{text}");
        drop(session);
    }

    fn thinking_only_max_tokens() -> Vec<StreamEvent> {
        vec![
            StreamEvent::ThinkingDelta(Text::from("")),
            StreamEvent::Stop(StopReason::MaxTokens),
        ]
    }

    fn policy_notes(events: &[Event]) -> Vec<&Text> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::PolicyNote(text) => Some(text),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn max_tokens_recovery_continues_after_a_thinking_only_reply() {
        let provider = FakeProvider::new(vec![thinking_only_max_tokens(), text_reply("done")]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        // The cut-off thinking block has no signature, so it cannot be sent
        // back: no assistant message is kept, and the note is a new user turn.
        assert_eq!(history.len(), 3, "{history:?}");
        assert_eq!(history[1], Message::user(MAX_TOKENS_NOTE));
        assert!(matches!(&history[2], Message::Assistant(_)));
        assert_eq!(provider.seen()[1].messages, 2);

        let events = drain(tx, &mut rx);
        assert_eq!(policy_notes(&events), vec![&Text::from(MAX_TOKENS_NOTE)]);
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
    }

    #[tokio::test]
    async fn max_tokens_recovery_keeps_signed_thinking_and_text() {
        let provider = FakeProvider::new(vec![
            vec![
                StreamEvent::ThinkingDelta(Text::from("plan")),
                StreamEvent::ThinkingSignature(Text::from("sig")),
                StreamEvent::TextDelta(Text::from("partial")),
                StreamEvent::Stop(StopReason::MaxTokens),
            ],
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(history.len(), 4, "{history:?}");
        let Message::Assistant(partial) = &history[1] else {
            panic!("expected the partial reply, got {:?}", history[1]);
        };
        assert_eq!(partial.content.len(), 2, "{partial:?}");
        assert!(partial.content[0].is_thinking());
        assert_eq!(history[2], Message::user(MAX_TOKENS_NOTE));
        let events = drain(tx, &mut rx);
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
    }

    #[tokio::test]
    async fn max_tokens_recovery_stops_on_the_fourth_stop_in_a_row() {
        let provider = FakeProvider::new(vec![thinking_only_max_tokens(); 5]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(provider.seen().len(), 4);
        let events = drain(tx, &mut rx);
        assert_eq!(policy_notes(&events).len(), 3);
        assert_eq!(
            events.last(),
            Some(&Event::RunEnd(TurnEndReason::Stopped(
                StopReason::MaxTokens
            )))
        );
    }

    #[tokio::test]
    async fn max_tokens_recovery_streak_resets_after_a_normal_turn() {
        let mut replies = vec![thinking_only_max_tokens(); 3];
        replies.push(tool_reply("call_1", "echo", r#"{"message":"hi"}"#));
        replies.extend(vec![thinking_only_max_tokens(); 3]);
        replies.push(text_reply("done"));
        let provider = FakeProvider::new(replies);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(provider.seen().len(), 8);
        let events = drain(tx, &mut rx);
        assert_eq!(policy_notes(&events).len(), 6);
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
    }

    /// A workspace whose fingerprint is 0 on the first call and 1 after.
    struct ChangesOnSecondCall {
        calls: std::sync::atomic::AtomicU64,
    }

    impl Workspace for ChangesOnSecondCall {
        fn fingerprint(&self) -> BoxFuture<'_, Option<u64>> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Some(call.min(1)) })
        }
    }

    /// A policy that records `changed_files` for each turn.
    struct Recorder {
        seen: std::sync::Arc<std::sync::Mutex<Vec<Option<bool>>>>,
    }

    impl TurnPolicy for Recorder {
        fn after_turn(&mut self, turn: &TurnView<'_>) -> crate::policy::Decision {
            self.seen.lock().unwrap().push(turn.changed_files);
            crate::policy::Decision::Continue
        }
    }

    async fn changed_files_per_turn(workspace: Option<&dyn Workspace>) -> Vec<Option<bool>> {
        let provider = FakeProvider::new(vec![
            tool_reply("call_1", "echo", r#"{"message":"a"}"#),
            tool_reply("call_2", "echo", r#"{"message":"b"}"#),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let policies: Vec<Box<dyn TurnPolicy>> = vec![Box::new(Recorder { seen: seen.clone() })];

        let mut agent = Agent::with_policies(
            &provider,
            &registry,
            config(),
            CancellationToken::new(),
            policies,
        );
        if let Some(workspace) = workspace {
            agent = agent.with_workspace(workspace);
        }
        agent.run(&mut history, None, &tx).await;

        seen.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn turn_view_reports_changed_files() {
        let workspace = ChangesOnSecondCall {
            calls: std::sync::atomic::AtomicU64::new(0),
        };

        let seen = changed_files_per_turn(Some(&workspace)).await;

        assert_eq!(seen, vec![Some(true), Some(false), Some(false)]);
    }

    #[tokio::test]
    async fn turn_view_without_a_workspace() {
        let seen = changed_files_per_turn(None).await;

        assert_eq!(seen, vec![None, None, None]);
    }
}
