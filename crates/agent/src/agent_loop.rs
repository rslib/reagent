//! The agent loop: stream a reply, run its tool calls, repeat.

use futures_util::{FutureExt, StreamExt};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use reagent_ai::{
    AssistantMessage, Content, Error, Image, Message, Provider, Request, StopReason, StreamEvent,
    Text, ToolCall, ToolResult, Usage,
};

use crate::archive::Archive;
use crate::batch::{Note, Outcome, run_all_noted};
use crate::compact::{Compact, Compacted};
use crate::event::{Event, TurnEndReason};
use crate::guard::{Answer, Guard};
use crate::journal::Journal;
use crate::journal::record::ApprovalKind;
use crate::journal::resume::PendingCall;
use crate::journal::state::State;
use crate::live::{ClientId, Command, Inbox, SendMode, Settings};
use crate::mask::Mask;
use crate::policy::{PolicyRun, TurnPolicy, TurnView};
use crate::reread::Ledger;
use crate::tool::{EffectClass, Identity, Registry};
use crate::workspace::Workspace;

/// Renames a tool call whose id an earlier call already used, so each call
/// keeps its own archive id. Hosts that send no ids get `call_0` from the
/// parser on every one-call reply.
fn unique_call_ids(message: &mut AssistantMessage, archive: &Archive) {
    let mut fresh = archive.len();
    for at in 0..message.content.len() {
        let Content::ToolCall(call) = &message.content[at] else {
            continue;
        };
        let repeated = archive.id_of(&call.id).is_some()
            || message.content[..at]
                .iter()
                .any(|earlier| matches!(earlier, Content::ToolCall(other) if other.id == call.id));
        if repeated {
            fresh += 1;
            let renamed = Text::from(format!("{}_{fresh}", call.id));
            if let Content::ToolCall(call) = &mut message.content[at] {
                call.id = renamed;
            }
        }
    }
}

/// How many times an empty reply is requested again before the run stops.
const EMPTY_RETRIES: u32 = 2;

/// The note added after a max_tokens stop, baked in from `prompts/max_tokens.md`.
const MAX_TOKENS_NOTE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/max_tokens.md"
));

/// How many max_tokens stops in a row the loop recovers from before it stops.
const MAX_TOKENS_RECOVERIES: u32 = 3;

/// The most stream events one reservation of channel room covers.
const EVENT_BATCH: usize = 64;

/// The parts of a [`Request`] a session controls, besides the conversation.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// The provider model id.
    pub model: String,
    /// The cap on reply tokens.
    pub max_tokens: u32,
    /// The system prompt, if any.
    pub system: Option<Text>,
    /// How much reasoning the provider may spend, in its own words: a level
    /// text or a token budget.
    pub reasoning: Option<String>,
    /// A stable key for the cached prompt prefix of this session.
    pub cache_key: Option<Text>,
    /// The maximum number of provider calls, after which the run stops.
    pub max_turns: Option<u32>,
    /// Whether the model takes images, so a request sends them. It is on for
    /// a model that nothing says takes none.
    pub images: bool,
}

/// Drives one provider through a conversation until the model calls no tool
/// or a cap is reached.
pub struct Agent<'a> {
    provider: &'a dyn Provider,
    /// The provider a client switched the session to, if any: the next request
    /// goes to it instead of the borrowed one.
    swapped: Option<Arc<dyn Provider>>,
    registry: &'a Registry,
    config: AgentConfig,
    token: CancellationToken,
    policies: Vec<Box<dyn TurnPolicy>>,
    mask: Option<Mask>,
    archive: Option<Arc<Archive>>,
    compact: Option<Compact>,
    /// The compaction a client may ask for when the policy is off.
    requested: Option<Compact>,
    guard: Option<Guard>,
    ledger: Ledger,
    workspace: Option<&'a dyn Workspace>,
    fingerprint: Option<u64>,
    provider_name: Text,
    /// The config provider id of the provider, when one is known: the name
    /// `reagent --model <config>/<model>` takes.
    config_provider: Option<Text>,
    model: Text,
    /// How many compactions replaced the history so far, for the base record.
    compactions: u32,
    /// The turns the last run of this loop made.
    completed: u32,
    /// The number this run gives its first turn: a run that continues a
    /// session numbers its turns after the ones the session already has.
    first_turn: u64,
    /// Whether this run answers the clients of a live session. Only a live run
    /// names the record it commits and records a call that waits for them.
    live: bool,
    /// The settings changes the run applied and the journal has not recorded
    /// yet: a change made while no journal is open waits for the next request.
    unrecorded: Vec<Settings>,
    /// Whether a client changed the level or the mode: a base then records
    /// them again, since a resume reads from the last base.
    changed_settings: bool,
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
            swapped: None,
            registry,
            token,
            policies: Vec::new(),
            mask: None,
            archive: None,
            compact: None,
            requested: None,
            guard: None,
            ledger: Ledger::default(),
            workspace: None,
            fingerprint: None,
            provider_name: Text::from(provider.name()),
            config_provider: None,
            model: Text::from(config.model.as_str()),
            compactions: 0,
            completed: 0,
            first_turn: 1,
            live: false,
            unrecorded: Vec::new(),
            changed_settings: false,
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
            swapped: None,
            registry,
            token,
            policies,
            mask: None,
            archive: None,
            compact: None,
            requested: None,
            guard: None,
            ledger: Ledger::default(),
            workspace: None,
            fingerprint: None,
            provider_name: Text::from(provider.name()),
            config_provider: None,
            model: Text::from(config.model.as_str()),
            compactions: 0,
            completed: 0,
            first_turn: 1,
            live: false,
            unrecorded: Vec::new(),
            changed_settings: false,
            config,
        }
    }

    /// Names the config provider id of the starting provider, so a request and
    /// a resume keep it.
    #[must_use]
    pub fn with_config_provider(mut self, config: Option<Text>) -> Self {
        self.config_provider = config;
        self
    }

    /// Numbers the first turn of this run `first`.
    ///
    /// A resumed run gives the turn after the last one the session recorded,
    /// so a `TURN_END` names the turn of the session and not of the run.
    #[must_use]
    pub fn with_first_turn(mut self, first: u64) -> Self {
        self.first_turn = first.max(1);
        self
    }

    /// Turns on observation masking: before each request, old large tool
    /// outputs in `history` become a stub (see [`Mask`]). Off by default.
    ///
    /// Only the in-memory history changes; the session file is not touched.
    #[must_use]
    pub fn with_mask(mut self) -> Self {
        self.mask = Some(Mask);
        self.archive.get_or_insert_with(Archive::new);
        self
    }

    /// Checks every tool call with `guard` before it runs (see [`Guard`]).
    #[must_use]
    pub fn with_guard(mut self, guard: Guard) -> Self {
        self.guard = Some(guard);
        self
    }

    /// Turns on compaction: at its token budget, older turns become a
    /// checkpoint (see [`Compact`]) and their outputs go to the archive.
    #[must_use]
    pub fn with_compact(mut self, compact: Compact) -> Self {
        self.compact = Some(compact);
        self.archive.get_or_insert_with(Archive::new);
        self
    }

    /// Lets a client ask for a compaction while the policy is off. The
    /// outputs it removes are not kept for the expand tool, which such a run
    /// does not offer.
    #[must_use]
    pub fn with_compact_on_request(mut self, compact: Compact) -> Self {
        self.requested = Some(compact);
        self
    }

    /// Numbers every tool call in `archive` and keeps there the outputs that
    /// masking or compaction remove, for the [`crate::archive::Expand`] tool.
    #[must_use]
    pub fn with_archive(mut self, archive: Arc<Archive>) -> Self {
        self.archive = Some(archive);
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
    /// Appends the assistant replies and tool results to `history`, records
    /// every message in `journal` (when given: a base at the start, a synced
    /// call start before each tool that is not a read, a commit after each
    /// result, and a synced turn end) and reports streamed [`Event`]s on
    /// `events`. After each normal turn the configured policies run and may
    /// inject a message or stop the run; with no policies the loop acts
    /// exactly as without them.
    pub async fn run(
        &mut self,
        history: &mut Vec<Message>,
        journal: Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
    ) {
        self.run_inner(history, journal, events, None).await;
    }

    /// Runs the conversation as [`Agent::run`] does, and takes the commands of
    /// a live session at each boundary of the run: a steer message joins the
    /// conversation after the tool results and before the next request, a
    /// queued message waits for the next run, a cancel ends the turn and a set
    /// changes the next request.
    ///
    /// The commands are read without waiting, so a run makes no extra wakeup
    /// and adds no timer. The run gets a token of its own, a child of the
    /// session's, so a cancel stops the turn and not the session.
    pub async fn run_with_inbox(
        &mut self,
        history: &mut Vec<Message>,
        journal: Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
        inbox: &mut Inbox,
    ) {
        let run = self.token.child_token();
        let session = std::mem::replace(&mut self.token, run);
        // The clients of the session answer the calls the rules send to a
        // person; a run without an inbox keeps refusing them.
        if let Some(guard) = self.guard.as_mut() {
            guard.listen(events.clone());
        }
        self.run_inner(history, journal, events, Some(inbox)).await;
        if let Some(guard) = self.guard.as_mut() {
            guard.hush();
        }
        self.token = session;
    }

    async fn run_inner(
        &mut self,
        history: &mut Vec<Message>,
        mut journal: Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
        mut inbox: Option<&mut Inbox>,
    ) {
        let started = std::time::Instant::now();
        let mut turns: u32 = 0;
        let mut max_tokens_streak: u32 = 0;
        // A run with an inbox answers the clients of a live session: only it
        // names the record it commits and records a call that waits.
        self.live = inbox.is_some();
        self.fingerprint = match self.workspace {
            Some(workspace) if !self.policies.is_empty() => workspace.fingerprint().await,
            _ => None,
        };
        // A new session starts with a base: the state and the history. A
        // resumed run's parent part holds the history already, so it writes
        // only what came after it (a new message, an interrupted result).
        let base = journal
            .as_ref()
            .is_some_and(|journal| journal.recorded().is_none());
        if let Some(journal) = journal.as_mut()
            && let Err(error) = match journal.recorded() {
                None => write_base(journal, self, history),
                Some(recorded) => write_after(journal, &history[recorded.min(history.len())..]),
            }
        {
            let error = journal_error(&error);
            emit(events, &self.token, Event::Error(error)).await;
            emit(events, &self.token, Event::RunEnd(TurnEndReason::Error)).await;
            return;
        }
        if base {
            self.based();
        }
        committed(&journal, self.live, events, &self.token).await;
        let mut run_reason = loop {
            // A command of a live session is taken here, before the request.
            if let Some(inbox) = inbox.as_deref_mut() {
                if self.take_commands(inbox, events).await {
                    break TurnEndReason::Cancelled;
                }
                if let Some(error) = self
                    .flush(history, &mut journal, inbox, events)
                    .await
                    .error()
                {
                    emit(events, &self.token, Event::Error(error)).await;
                    break TurnEndReason::Error;
                }
            }
            if self.config.max_turns.is_some_and(|cap| turns >= cap) {
                break TurnEndReason::MaxTurns;
            }
            if self.token.is_cancelled() || emit(events, &self.token, Event::TurnStart).await {
                break TurnEndReason::Cancelled;
            }
            turns += 1;

            let compacted = match (&self.compact, &self.archive) {
                (Some(compact), Some(archive)) => compact.apply(history, archive).await,
                _ => None,
            };
            if let Some(done) = compacted {
                match self.compacted(done, history, &mut journal, events).await {
                    Ok(true) => break TurnEndReason::Cancelled,
                    Ok(false) => {}
                    Err(error) => {
                        emit(events, &self.token, Event::Error(error)).await;
                        break TurnEndReason::Error;
                    }
                }
            }
            if let (Some(mask), Some(archive)) = (&self.mask, &self.archive)
                && mask.apply(history, archive) > 0
            {
                archive.flush().await;
            }
            let before = history.len();
            let mut outcome = self
                .run_turn(history, &mut journal, events, inbox.as_deref_mut())
                .await;
            // The commands that arrived during the turn are taken now, but a
            // steer text joins the conversation only after the turn is recorded
            // and its policies ran, so they see the turn exactly as without it.
            let mut steer_waits = false;
            if let Some(inbox) = inbox.as_deref_mut() {
                if self.take_commands(inbox, events).await
                    && !matches!(outcome, TurnOutcome::Error(_))
                {
                    outcome = TurnOutcome::Cancelled;
                }
                steer_waits = inbox.has_pending();
            }
            // Every turn end records the turn and syncs it, whatever ended it.
            let usage = turn_usage(&history[before..]);
            if let Some(journal) = journal.as_mut() {
                match end_turn(journal, self.turn_number(turns), usage).await {
                    Ok(()) => {}
                    Err(error) if !matches!(outcome, TurnOutcome::Error(_)) => {
                        outcome = TurnOutcome::Error(journal_error(&error));
                    }
                    Err(_) => {}
                }
            }
            // Only a turn end that reached the disk is named to the clients.
            if !matches!(outcome, TurnOutcome::Error(_)) {
                committed(&journal, self.live, events, &self.token).await;
            }
            match outcome {
                TurnOutcome::Stopped(StopReason::MaxTokens)
                    if max_tokens_streak < MAX_TOKENS_RECOVERIES =>
                {
                    max_tokens_streak += 1;
                    outcome = self
                        .recover_from_max_tokens(history, &mut journal, events)
                        .await;
                }
                TurnOutcome::Continue | TurnOutcome::Finish => max_tokens_streak = 0,
                _ => {}
            }

            let (turn_reason, mut run_end) = self
                .after_turn(outcome, turns, started, history, &mut journal, events)
                .await;
            // A steer text that arrived while the model wrote a reply with no
            // tool call keeps the run going: the reply, then the text, then the
            // next request.
            if steer_waits && run_end == Some(TurnEndReason::EndTurn) {
                run_end = None;
            }
            let closed = emit(events, &self.token, Event::TurnEnd(turn_reason)).await;
            if let Some(reason) = run_end {
                break reason;
            }
            if closed {
                break TurnEndReason::Cancelled;
            }
            // The steer texts land after the tool results and any policy
            // message, before the next request.
            if let Some(inbox) = inbox.as_deref_mut()
                && let Some(error) = self
                    .flush(history, &mut journal, inbox, events)
                    .await
                    .error()
            {
                emit(events, &self.token, Event::Error(error)).await;
                break TurnEndReason::Error;
            }
        };

        if self.token.is_cancelled() {
            run_reason = TurnEndReason::Cancelled;
        }
        self.completed = turns;
        emit(events, &self.token, Event::RunEnd(run_reason)).await;
    }

    /// Adds the steer texts the inbox holds to the conversation, and applies
    /// the settings a stream held.
    ///
    /// A steer message lands after the tool results that are already in
    /// history, before the next request. When the journal cannot hold a
    /// message, that message and the ones after it stay in the inbox, and the
    /// caller ends the run with the error.
    pub(crate) async fn flush(
        &mut self,
        history: &mut Vec<Message>,
        journal: &mut Option<&mut Journal>,
        inbox: &mut Inbox,
        events: &mpsc::Sender<Event>,
    ) -> Drained {
        let mut drained = Drained::default();
        let mut pending = inbox.take_pending().into_iter();
        while let Some((client, text)) = pending.next() {
            if let Err(error) = record(Message::user(text.clone()), history, journal) {
                inbox.restore((client, text), pending);
                drained.error = Some(error);
                break;
            }
            drained.steered = true;
        }
        self.apply_held(inbox, events).await;
        // A compaction a client asked for runs before the next request, as the
        // automatic one does.
        for note in inbox.take_compactions() {
            if let Some(error) = self
                .compact_now(note.as_deref(), history, journal, events)
                .await
            {
                drained.error = Some(error);
                return drained;
            }
        }
        // A settings change reaches the journal before the request it applies
        // to, so a resume applies the values the model saw.
        let mut wrote = false;
        let mut changes = std::mem::take(&mut self.unrecorded).into_iter();
        while let Some(settings) = changes.next() {
            let Some(journal) = journal.as_mut() else {
                break;
            };
            match write_settings(
                journal,
                self.provider_name.as_ref(),
                self.config_provider.as_deref(),
                &settings,
            ) {
                Ok(()) => wrote = true,
                Err(error) => {
                    self.unrecorded.push(settings);
                    self.unrecorded.extend(changes);
                    drained.error = Some(journal_error(&error));
                    return drained;
                }
            }
        }
        if wrote {
            committed(journal, self.live, events, &self.token).await;
        }
        drained
    }

    /// Takes every command a live session holds, without waiting on the inbox:
    /// a message waits for the next boundary or run, and a set waits for the
    /// next request. Returns whether a client cancelled; the commands after a
    /// cancel stay in the inbox.
    pub(crate) async fn take_commands(
        &mut self,
        inbox: &mut Inbox,
        events: &mpsc::Sender<Event>,
    ) -> bool {
        while let Some(command) = inbox.ready() {
            match command {
                Command::Send { client, text, mode } => {
                    self.accept(client, text, mode, inbox, events).await;
                }
                Command::Cancel { .. } => {
                    inbox.clear_waiting();
                    self.token.cancel();
                    return true;
                }
                Command::Set { settings, .. } => inbox.hold(settings),
                Command::Compact { note, .. } => inbox.hold_compaction(note),
                Command::Approve { client, id, answer } => {
                    self.approve(client, id, answer, events).await;
                }
            }
        }
        false
    }

    /// Records a compaction of the history: the event, the base a resume reads
    /// and the committed record. Returns whether the channel closed.
    async fn compacted(
        &mut self,
        done: Compacted,
        history: &[Message],
        journal: &mut Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
    ) -> Result<bool, Text> {
        let closed = emit(
            events,
            &self.token,
            Event::Compacted {
                before: done.before,
                after: done.after,
                replaced: done.replaced as u32,
                bytes: done.bytes as u32,
            },
        )
        .await;
        self.compactions += 1;
        // A compaction replaced the history: a base records the state and the
        // history the run continues with.
        if let Some(journal) = journal.as_mut() {
            write_base(journal, self, history).map_err(|error| journal_error(&error))?;
            self.based();
        }
        committed(journal, self.live, events, &self.token).await;
        Ok(closed)
    }

    /// Runs a compaction a client asked for, ending the new checkpoint with
    /// `note`. A history with nothing older than the kept tail is reported to
    /// every client, and returns the journal error, if any.
    async fn compact_now(
        &mut self,
        note: Option<&str>,
        history: &mut Vec<Message>,
        journal: &mut Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
    ) -> Option<Text> {
        let done = match self.compact.as_ref().or(self.requested.as_ref()) {
            Some(compact) => {
                let archive = match &self.archive {
                    Some(archive) => Arc::clone(archive),
                    None => Archive::new(),
                };
                compact.request(history, &archive, note).await
            }
            None => None,
        };
        let Some(done) = done else {
            let _ = emit(
                events,
                &self.token,
                Event::Error(Text::from("nothing to compact")),
            )
            .await;
            return None;
        };
        self.compacted(done, &history[..], journal, events)
            .await
            .err()
    }

    /// Waits for `work` while it takes the commands a client sends meanwhile,
    /// so a cancel reaches a running tool batch or a retry wait at once.
    ///
    /// With no inbox this is `work` alone, as in a headless run.
    async fn watched<F: std::future::Future>(
        &self,
        work: F,
        inbox: Option<&mut Inbox>,
        events: &mpsc::Sender<Event>,
    ) -> F::Output {
        let Some(inbox) = inbox else {
            return work.await;
        };
        let mut work = std::pin::pin!(work);
        loop {
            tokio::select! {
                output = &mut work => return output,
                command = inbox.next() => self.take_stream(command, inbox, events).await,
            }
        }
    }

    /// [`Agent::watched`] with the notes of a tool batch: each note is recorded
    /// as it arrives, while the other calls still run.
    async fn watched_noted<F: std::future::Future>(
        &self,
        work: F,
        inbox: Option<&mut Inbox>,
        events: &mpsc::Sender<Event>,
        notes: &mut mpsc::UnboundedReceiver<Note>,
        results: &mut Results<'_, '_, '_>,
    ) -> F::Output {
        let mut work = std::pin::pin!(work);
        match inbox {
            // A headless run has no command to take, but each result still
            // reaches the journal as it comes back.
            None => loop {
                tokio::select! {
                    output = &mut work => return output,
                    note = notes.recv() => match note {
                        Some(note) => results.note(note).await,
                        // The batch never drops its sender before it ends.
                        None => return work.await,
                    },
                }
            },
            Some(inbox) => loop {
                tokio::select! {
                    output = &mut work => return output,
                    note = notes.recv() => match note {
                        Some(note) => results.note(note).await,
                        None => return work.await,
                    },
                    command = inbox.next() => self.take_stream(command, inbox, events).await,
                }
            },
        }
    }

    /// Applies one command of a live session while no run is open.
    pub(crate) async fn idle(
        &mut self,
        command: Command,
        history: &mut Vec<Message>,
        journal: &mut Option<&mut Journal>,
        inbox: &mut Inbox,
        events: &mpsc::Sender<Event>,
    ) {
        match command {
            Command::Send { client, text, mode } => {
                self.accept(client, text, mode, inbox, events).await;
            }
            Command::Set { settings, .. } => self.set(settings, events).await,
            // No run is open to compact before a request, so it runs at once.
            Command::Compact { note, .. } => {
                let compactions = self.compactions;
                if let Some(error) = self
                    .compact_now(note.as_deref(), history, journal, events)
                    .await
                {
                    emit(events, &self.token, Event::Error(error)).await;
                }
                // The base holds the whole history now, so the next run writes
                // only the messages after it.
                if self.compactions > compactions
                    && let Some(journal) = journal.as_deref_mut()
                {
                    crate::journal::resume::mark_recorded(journal, history.len());
                }
            }
            Command::Approve { client, id, answer } => {
                self.approve(client, id, answer, events).await;
            }
            // Nothing is running to stop; the session stays open, and what
            // waited for a run is dropped as a cancel during a run drops it.
            Command::Cancel { .. } => inbox.clear_waiting(),
        }
    }

    /// Accepts one message of a live session: every client sees it, and it
    /// waits for the next boundary (a steer) or for the next run (a queue).
    ///
    /// The text is shared with the conversation and with the event, never
    /// copied.
    pub(crate) async fn accept(
        &self,
        client: ClientId,
        text: Text,
        mode: SendMode,
        inbox: &mut Inbox,
        events: &mpsc::Sender<Event>,
    ) {
        let event = Event::UserMessage {
            client,
            mode,
            text: text.clone(),
        };
        // A closed channel ends the run at its next event, as it always has.
        let _ = emit(events, &self.token, event).await;
        match mode {
            SendMode::Steer => inbox.steer(client, text),
            SendMode::Queue => inbox.queue(client, text),
        }
    }

    /// Applies the settings a stream held, before the next request.
    pub(crate) async fn apply_held(&mut self, inbox: &mut Inbox, events: &mpsc::Sender<Event>) {
        for settings in inbox.take_held() {
            self.set(settings, events).await;
        }
    }

    /// The provider the next request goes to: the one a client switched to,
    /// else the borrowed one.
    fn provider(&self) -> &dyn Provider {
        match &self.swapped {
            Some(provider) => &**provider,
            None => self.provider,
        }
    }

    /// Applies settings to the next request: the provider and its config id,
    /// the model, the reasoning level, the permission mode and the limits. A
    /// request that is already in flight keeps the values it was sent with.
    ///
    /// The change waits for the next [`Agent::flush`], which writes it to the
    /// journal: no journal is open here when a session applies a setting while
    /// no run is running.
    async fn set(&mut self, settings: Settings, events: &mpsc::Sender<Event>) {
        let mut recorded = Settings::default();
        // The provider goes first: the model and the level are the new one's.
        if let Some(swap) = &settings.provider {
            self.swapped = Some(Arc::clone(&swap.0));
            let name = self.provider().name();
            self.provider_name = Text::from(name);
            // A provider names its own config id, or none.
            self.config_provider = None;
        }
        if let Some(config) = &settings.config_provider {
            self.config_provider = Some(config.clone());
        }
        if let Some(model) = &settings.model {
            self.model = model.clone();
            self.config.model = model.to_string();
            recorded.model = Some(model.clone());
        }
        if let Some(level) = &settings.level {
            self.config.reasoning = Some(level.to_string());
            recorded.level = Some(level.clone());
        }
        if let Some(mode) = settings.mode {
            let applied = match self.guard.as_mut() {
                Some(guard) => match guard.set_mode(mode) {
                    Ok(()) => true,
                    Err(message) => {
                        emit(events, &self.token, Event::Error(Text::from(message))).await;
                        false
                    }
                },
                // A mode no guard holds changes nothing.
                None => false,
            };
            if applied {
                recorded.mode = Some(mode);
            }
        }
        // The limits are neither journal records nor settings events: the
        // client that asks knows the model and the window it picked them from.
        if let Some(images) = settings.images {
            self.config.images = images;
        }
        if let Some(max_tokens) = settings.max_tokens {
            self.config.max_tokens = max_tokens;
        }
        if let Some(budget) = settings.compact_budget
            && let Some(compact) = self.compact.as_mut()
        {
            compact.set_budget(budget);
        }
        if recorded.level.is_some() || recorded.mode.is_some() {
            self.changed_settings = true;
        }
        if recorded.model.is_some() || recorded.level.is_some() || recorded.mode.is_some() {
            // Every client hears what the session applied, not what the `set`
            // asked for: this is the record the journal writes, so a refused
            // mode is in neither.
            let applied = Event::Settings {
                model: recorded.model.clone(),
                level: recorded.level.clone(),
                mode: recorded.mode,
            };
            self.unrecorded.push(recorded);
            let _ = emit(events, &self.token, applied).await;
        }
    }

    /// Drops the level and the mode of the changes still to record, once a base
    /// holds the ones the run has now. A model change stays: the base does not
    /// name the config provider its record carries.
    fn based(&mut self) {
        if !self.changed_settings {
            return;
        }
        for settings in &mut self.unrecorded {
            settings.level = None;
            settings.mode = None;
        }
        self.unrecorded.retain(|settings| settings.model.is_some());
    }

    /// Handles one command of a live session that arrived while the model
    /// streamed its reply.
    ///
    /// Only a cancel acts at once: a message and a setting wait for the next
    /// boundary, because the request in flight and the history it reads cannot
    /// change under it.
    async fn take_stream(&self, command: Command, inbox: &mut Inbox, events: &mpsc::Sender<Event>) {
        match command {
            Command::Send { client, text, mode } => {
                self.accept(client, text, mode, inbox, events).await;
            }
            Command::Cancel { .. } => {
                inbox.clear_waiting();
                self.token.cancel();
            }
            Command::Set { settings, .. } => inbox.hold(settings),
            Command::Compact { note, .. } => inbox.hold_compaction(note),
            Command::Approve { client, id, answer } => {
                self.approve(client, id, answer, events).await;
            }
        }
    }

    /// Routes a client's answer to the call that waits for it.
    ///
    /// A request that no call waits for any more, one answered twice or one
    /// answered from a session that listens to none, is reported to every
    /// client with its id, so nobody believes the call ran.
    async fn approve(
        &self,
        client: ClientId,
        id: u64,
        answer: Answer,
        events: &mpsc::Sender<Event>,
    ) {
        let routed = match &self.guard {
            Some(guard) => guard.answer(client, id, answer, &self.token).await,
            None => false,
        };
        if !routed {
            let message = format!("approval {id} is already resolved");
            emit(events, &self.token, Event::Error(Text::from(message))).await;
        }
    }

    /// The turns the last [`Agent::run`] made.
    pub fn turns(&self) -> u32 {
        self.completed
    }

    /// The number of the last turn this run ended, in the session's numbering.
    ///
    /// A run that made no turn leaves the turn count where it found it, so a
    /// caller takes the session's count from here.
    pub fn last_turn(&self) -> u64 {
        self.turn_number(self.completed)
    }

    /// The number of turn `turns` of this run, which counts from 1.
    fn turn_number(&self, turns: u32) -> u64 {
        self.first_turn.saturating_sub(1) + u64::from(turns)
    }

    /// Runs the tool calls of the last assistant message that have no result.
    ///
    /// A resumed run calls this before [`Agent::run`]: every call the journal
    /// found pending is recorded and synced like a turn's, run here, and
    /// answered in history and in the journal, so the next request carries a
    /// result for each one. Returns how the calls ended.
    pub async fn run_pending(
        &mut self,
        history: &mut Vec<Message>,
        journal: &mut Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
        pending: &[PendingCall],
    ) -> TurnEndReason {
        self.live = false;
        self.run_pending_inner(history, journal, events, pending, None)
            .await
    }

    /// Runs the pending calls as [`Agent::run_pending`] does, in a live
    /// session: a call the rules send to a person asks the clients again, the
    /// wait and the answer reach the journal, and a cancel stops the calls.
    pub async fn run_pending_with_inbox(
        &mut self,
        history: &mut Vec<Message>,
        journal: &mut Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
        pending: &[PendingCall],
        inbox: &mut Inbox,
    ) -> TurnEndReason {
        let run = self.token.child_token();
        let session = std::mem::replace(&mut self.token, run);
        if let Some(guard) = self.guard.as_mut() {
            guard.listen(events.clone());
        }
        self.live = true;
        let reason = self
            .run_pending_inner(history, journal, events, pending, Some(inbox))
            .await;
        if let Some(guard) = self.guard.as_mut() {
            guard.hush();
        }
        self.token = session;
        reason
    }

    async fn run_pending_inner(
        &mut self,
        history: &mut Vec<Message>,
        journal: &mut Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
        pending: &[PendingCall],
        inbox: Option<&mut Inbox>,
    ) -> TurnEndReason {
        if pending.is_empty() {
            return TurnEndReason::EndTurn;
        }
        let calls = unanswered(history, pending);
        if calls.is_empty() {
            return TurnEndReason::EndTurn;
        }
        // The history a resume rebuilt may hold a message the parent part does
        // not: the interrupted result a crash left is in history but not in the
        // log. It is written before the results this run makes, so a later
        // resume reads the history in the order the model saw it.
        if let Err(error) = write_pending(journal, history) {
            let error = journal_error(&error);
            emit(events, &self.token, Event::Error(error)).await;
            emit(events, &self.token, Event::RunEnd(TurnEndReason::Error)).await;
            return TurnEndReason::Error;
        }
        // A call that may change the world is on the disk, synced, before it
        // runs, exactly as in a turn.
        if let Err(error) = note_calls(&calls, self.registry, journal).await {
            for call in &calls {
                let message = Message::tool_result(call.id.clone(), error.clone(), true);
                let _ = record_result(message, history, journal);
            }
            mark_recorded(journal, history.len());
            emit(events, &self.token, Event::Error(error)).await;
            emit(events, &self.token, Event::RunEnd(TurnEndReason::Error)).await;
            return TurnEndReason::Error;
        }
        // Each result reaches the journal as it comes back, and a call that
        // asks records its wait and its answer, exactly as in a turn.
        let mut ledger = std::mem::take(&mut self.ledger);
        let (notes, mut progress) = mpsc::unbounded_channel();
        let batch = run_all_noted(
            &calls,
            self.registry,
            self.guard.as_ref(),
            &self.token,
            &notes,
            self.live,
        );
        let mut results = Results {
            calls: &calls,
            repaired: &[],
            history,
            registry: self.registry,
            token: &self.token,
            live: self.live,
            events,
            ledger: &mut ledger,
            journal: &mut *journal,
            written: None,
            slots: calls.iter().map(|_| None).collect(),
            facts: calls.iter().map(|_| None).collect(),
            failure: None,
        };
        self.watched_noted(batch, inbox, events, &mut progress, &mut results)
            .await;
        while let Ok(note) = progress.try_recv() {
            results.note(note).await;
        }
        let Results {
            slots,
            facts,
            failure,
            ..
        } = results;
        self.ledger = ledger;
        // Every result reaches history before any event is sent, so a cancel
        // or a closed receiver cannot leave a call unanswered.
        for result in slots {
            history.push(result.expect("the batch answers every call"));
        }
        let facts: Vec<Fact> = facts
            .into_iter()
            .map(|fact| fact.expect("the batch answers every call"))
            .collect();
        mark_recorded(journal, history.len());
        if let Some(error) = failure {
            emit(events, &self.token, Event::Error(error)).await;
            // The run ends here: the reason the calls ended with is the reason
            // of the run, so the exit code and the list entry say it failed.
            emit(events, &self.token, Event::RunEnd(TurnEndReason::Error)).await;
            return TurnEndReason::Error;
        }
        for (call, (bytes, output, is_error, images)) in calls.iter().zip(facts) {
            let event = Event::ToolResult {
                id: call.id.clone(),
                is_error,
                bytes,
                output,
                images,
            };
            if emit(events, &self.token, event).await {
                return TurnEndReason::Cancelled;
            }
        }
        if self.token.is_cancelled() {
            return TurnEndReason::Cancelled;
        }
        TurnEndReason::EndTurn
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
        journal: &mut Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
    ) -> (TurnEndReason, Option<TurnEndReason>) {
        match outcome {
            TurnOutcome::Continue => {
                let run_end = self
                    .apply_policies(index, true, started, history, journal, events)
                    .await;
                (TurnEndReason::EndTurn, run_end)
            }
            TurnOutcome::Finish => {
                let run_end = self
                    .apply_policies(index, false, started, history, journal, events)
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
        journal: &mut Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
    ) -> TurnOutcome {
        let message = Message::user(MAX_TOKENS_NOTE);
        if let Some(journal) = journal.as_mut()
            && let Err(error) = journal.message(&message)
        {
            return TurnOutcome::Error(journal_error(&error));
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
    /// `PolicyNote` and, when a journal is open, appended to it like any other
    /// message. An injected text on a tool turn is appended in place to the
    /// last tool result, which was already recorded as that result; the
    /// append is not re-recorded because a journal is append-only.
    async fn apply_policies(
        &mut self,
        index: u32,
        ran_tools: bool,
        started: std::time::Instant,
        history: &mut Vec<Message>,
        journal: &mut Option<&mut Journal>,
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
                if let Some(journal) = journal.as_mut()
                    && let Err(error) = journal.message(&message)
                {
                    emit(events, &self.token, Event::Error(journal_error(&error))).await;
                }
                history.push(message);
            }
        }
        None
    }

    /// Streams one provider reply, records it and any tool results, and reports
    /// how the turn ended.
    async fn run_turn(
        &mut self,
        history: &mut Vec<Message>,
        journal: &mut Option<&mut Journal>,
        events: &mpsc::Sender<Event>,
        mut inbox: Option<&mut Inbox>,
    ) -> TurnOutcome {
        let mut assembler = Assembler::new();
        // How many retries already ran, capped at [`crate::retry::MAX_RETRIES`].
        let mut retries: u32 = 0;
        // How many empty replies were already requested again.
        let mut empty_retries: u32 = 0;
        let (mut message, broken, repaired) = loop {
            // None when the stream ended cleanly, else the error that stopped it
            // and, on the last allowed attempt, ends the turn.
            let mut stop_error: Option<Error> = None;
            loop {
                let outcome = self
                    .consume_stream(history, events, &mut assembler, inbox.as_deref_mut())
                    .await;
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
                        let wait = tokio::time::sleep(delay);
                        tokio::select! {
                            () = self.watched(wait, inbox.as_deref_mut(), events) => {}
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

            let broken = std::mem::take(&mut assembler.broken);
            let repaired = std::mem::take(&mut assembler.repaired);
            let mut message = assembler.finish(self.provider_name.clone(), self.model.clone());
            split_think(&mut message);
            if let Some(call) = self.leaked_call(&mut message, history.len())
                && emit_call(events, &self.token, call).await
            {
                return TurnOutcome::Cancelled;
            }
            // Gateways now and then send a reply with no content at all, though
            // they bill output tokens for it, and a weak model now and then
            // ends its turn after only reasoning. The same request usually
            // answers.
            if (message.content.is_empty() || reasoning_only(&message))
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
                let wait = tokio::time::sleep(delay);
                tokio::select! {
                    () = self.watched(wait, inbox.as_deref_mut(), events) => {}
                    () = self.token.cancelled() => return TurnOutcome::Cancelled,
                }
                assembler = Assembler::new();
                empty_retries += 1;
                continue;
            }
            break (message, broken, repaired);
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

        if let Some(archive) = &self.archive {
            unique_call_ids(&mut message, archive);
        }

        match message.stop_reason {
            Some(StopReason::ToolUse) => {
                // The reply is recorded once, then borrowed back from history
                // for its tool calls; no message is cloned.
                if let Err(error) = record(Message::assistant(message), history, journal) {
                    return TurnOutcome::Error(error);
                }
                let mut calls = match history.last() {
                    Some(Message::Assistant(assistant)) => tool_calls(assistant),
                    _ => Vec::new(),
                };
                // A broken call runs on what the model sent, so its error
                // says what went wrong.
                for (at, sent) in broken {
                    if let Some(call) = calls.get_mut(at) {
                        call.arguments = sent;
                    }
                }
                if let Some(archive) = &self.archive {
                    for call in &calls {
                        archive.assign(&call.id);
                    }
                }
                // A call that may change the world is on the disk, synced,
                // before it runs.
                match note_calls(&calls, self.registry, journal).await {
                    Ok(true) => {
                        committed(journal, self.live, events, &self.token).await;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        // No tool ran: each call still gets an answer in
                        // history, so a later request never carries a call
                        // without one.
                        for call in &calls {
                            let message =
                                Message::tool_result(call.id.clone(), error.clone(), true);
                            let _ = record_result(message, history, journal);
                        }
                        return TurnOutcome::Error(error);
                    }
                }
                // Each result is recorded and committed as it comes back, so a
                // crash in the middle of the batch keeps what already ran.
                // History keeps the model's call order, so the results wait in
                // the batch's places until the whole batch is in.
                let mut ledger = std::mem::take(&mut self.ledger);
                let (notes, mut progress) = mpsc::unbounded_channel();
                let batch = run_all_noted(
                    &calls,
                    self.registry,
                    self.guard.as_ref(),
                    &self.token,
                    &notes,
                    self.live,
                );
                let mut results = Results {
                    calls: &calls,
                    repaired: &repaired,
                    history,
                    registry: self.registry,
                    token: &self.token,
                    live: self.live,
                    events,
                    ledger: &mut ledger,
                    journal: &mut *journal,
                    written: None,
                    slots: calls.iter().map(|_| None).collect(),
                    facts: calls.iter().map(|_| None).collect(),
                    failure: None,
                };
                self.watched_noted(batch, inbox, events, &mut progress, &mut results)
                    .await;
                // The batch answered every call before it ended, so the notes
                // it left are the ones the select did not take.
                while let Ok(note) = progress.try_recv() {
                    results.note(note).await;
                }
                let Results {
                    slots,
                    facts,
                    failure,
                    ..
                } = results;
                self.ledger = ledger;
                // Every result reaches history before any event is sent, so a
                // cancel or a closed receiver cannot leave a call unanswered.
                // The results move into history, so only their event fields
                // are kept here; nothing is cloned.
                for result in slots {
                    history.push(result.expect("the batch answers every call"));
                }
                if let Some(error) = failure {
                    return TurnOutcome::Error(error);
                }
                for (call, fact) in calls.iter().zip(facts) {
                    let (bytes, output, is_error, images) =
                        fact.expect("the batch answers every call");
                    let event = Event::ToolResult {
                        id: call.id.clone(),
                        is_error,
                        bytes,
                        output,
                        images,
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
                match record(Message::assistant(message), history, journal) {
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
                    && let Err(error) = record(Message::assistant(message), history, journal)
                {
                    return TurnOutcome::Error(error);
                }
                TurnOutcome::Stopped(reason)
            }
        }
    }

    /// Turns a tool call a model wrote into the visible text of a reply that
    /// sent no call, or ended its reasoning with, into a real call when it
    /// names a registered tool, and returns it for its events. A call cut out
    /// of the text leaves the rest of that text behind. `turn` makes the call
    /// id unique.
    fn leaked_call(&self, message: &mut AssistantMessage, turn: usize) -> Option<ToolCall> {
        if message.stop_reason != Some(StopReason::EndTurn)
            || message
                .content
                .iter()
                .any(|content| matches!(content, Content::ToolCall(_)))
        {
            return None;
        }
        // A call in the visible text is cut out of it and the rest of the text
        // stays. Else, in a reply with no text, the end of the last reasoning
        // block, which is not changed: a reply with an answer is finished.
        let in_text = message
            .content
            .iter()
            .enumerate()
            .find_map(|(index, content)| {
                let Content::Text { text } = content else {
                    return None;
                };
                crate::leak::in_text(text).map(|leaked| (index, leaked))
            });
        let (name, arguments, cut) = match in_text {
            Some((index, leaked)) => (leaked.name, leaked.arguments, Some((index, leaked.span))),
            None => {
                let answered = message.content.iter().any(
                    |content| matches!(content, Content::Text { text } if !text.trim().is_empty()),
                );
                if answered {
                    return None;
                }
                let thinking = message
                    .content
                    .iter()
                    .rev()
                    .find_map(|content| match content {
                        Content::Thinking { thinking, .. } => Some(thinking),
                        _ => None,
                    })?;
                let leaked = crate::leak::at_end(thinking)?;
                (leaked.name, leaked.arguments, None)
            }
        };
        let tool = self.registry.resolve(name)?;
        let call = ToolCall {
            id: Text::from(format!("{LEAKED_CALL_ID}{turn}")),
            name: Text::from(tool.spec().name.as_str()),
            arguments: Text::from(arguments),
        };
        if let Some((index, span)) = cut
            && let Content::Text { text } = &message.content[index]
        {
            let mut rest = String::with_capacity(text.len() - span.len());
            rest.push_str(&text[..span.start]);
            rest.push_str(&text[span.end..]);
            if rest.trim().is_empty() {
                message.content.remove(index);
            } else {
                message.content[index] = Content::text(rest);
            }
        }
        message.content.push(Content::ToolCall(call.clone()));
        message.stop_reason = Some(StopReason::ToolUse);
        Some(call)
    }

    /// Streams one provider reply into `assembler`, cancelling on the token.
    ///
    /// Nothing is pushed to history here; a reply is recorded only after the
    /// whole turn ends, so a caller may retry a failed stream safely.
    ///
    /// With a live inbox, a command that arrives before the next item is taken
    /// at once: a cancel ends the turn here, and a message or a setting waits
    /// for the next boundary, since the request in flight cannot change.
    async fn consume_stream(
        &self,
        history: &[Message],
        events: &mpsc::Sender<Event>,
        assembler: &mut Assembler,
        mut inbox: Option<&mut Inbox>,
    ) -> ConsumeOutcome {
        let request = self.request(history);
        let mut stream = self.provider().stream(&request);
        // Room is reserved for a whole batch of events at once, so a streamed
        // answer waits on the channel once per batch instead of once per
        // delta. A channel smaller than a batch keeps its one-event behaviour.
        let batch_cap = events.max_capacity().min(EVENT_BATCH);
        // Grown on use: a tool turn has a few events, a long answer fills it.
        let mut raw: Vec<(StreamEvent, Option<Event>)> = Vec::new();
        loop {
            let Some(step) = step(&mut stream, &self.token, inbox.as_deref_mut()).await else {
                return ConsumeOutcome::Cancelled;
            };
            let item = match step {
                Step::Item(item) => item,
                Step::Command(command) => {
                    let inbox = inbox.as_deref_mut().expect("an inbox carried the command");
                    self.take_stream(command, inbox, events).await;
                    continue;
                }
            };
            let Some(item) = item else {
                return ConsumeOutcome::Finished;
            };
            let batch = take_ready(&mut stream, item, &mut raw, batch_cap);
            // One reservation covers the batch. A cancel while waiting for it
            // stops the run before any event of the batch lands, so the
            // assembler never runs ahead of what the receiver saw. A batch with
            // nothing to send leaves the channel alone, as the per-event send
            // did, so a closed receiver does not end it.
            let permits = if batch.sent == 0 {
                None
            } else {
                match reserve(events, &self.token, batch.sent).await {
                    Some(permits) => Some(permits),
                    None => return ConsumeOutcome::Cancelled,
                }
            };
            deliver(&mut raw, permits, assembler);
            // A finished tool call gains its name and arguments only once the
            // assembler has them. Only a tool call end makes one, and it closes
            // the batch, so it follows every event of its batch as before.
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
            // A stream error keeps the events taken before it, as a send per
            // event did, and then ends the turn.
            if let Some(error) = batch.error {
                return ConsumeOutcome::StreamError(error);
            }
            if batch.ended {
                return ConsumeOutcome::Finished;
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
            images: self.config.images,
            reasoning: self.config.reasoning.as_deref(),
            cache_key: self.config.cache_key.as_ref(),
        }
    }
}

/// The event fields of one tool result: its size, its output text, whether it
/// failed and its images.
type Fact = (usize, Option<Text>, bool, Vec<Image>);

/// What the loop does with each note of a tool batch as it arrives: the note
/// for the agent's own repair, the re-read ledger, the journal record and its
/// commit.
///
/// History keeps the model's call order, so a result waits in its slot until
/// the batch ends; the journal gets it as it comes.
struct Results<'a, 'b, 'c> {
    /// The reply's calls, in call order.
    calls: &'a [ToolCall],
    /// The places among the calls whose arguments the agent repaired.
    repaired: &'a [usize],
    /// The history the notes about re-reads are checked against.
    history: &'a [Message],
    /// The registry, to find what a call writes.
    registry: &'a Registry,
    /// The token of the run, for the event that names a commit.
    token: &'a CancellationToken,
    /// Whether the run answers the clients of a live session.
    live: bool,
    /// The channel the clients of a live session read.
    events: &'a mpsc::Sender<Event>,
    /// The re-read ledger, taken out of the loop while the batch runs.
    ledger: &'b mut Ledger,
    /// The journal, when the run keeps one.
    journal: &'b mut Option<&'c mut Journal>,
    /// The paths this reply wrote, once a re-read needed them.
    written: Option<Vec<Identity>>,
    /// The result of each call, by its place among the calls.
    slots: Vec<Option<Message>>,
    /// The event fields of each result, by its place among the calls.
    facts: Vec<Option<Fact>>,
    /// The first journal error, which ends the turn.
    failure: Option<Text>,
}

impl Results<'_, '_, '_> {
    /// Takes one note of the batch.
    async fn note(&mut self, note: Note) {
        match note {
            Note::Done(at, outcome) => self.done(at, outcome).await,
            Note::Wait { at, done } => {
                self.approval(at, ApprovalKind::Wait).await;
                let _ = done.send(());
            }
            Note::Answer { at, run, done } => {
                let kind = if run {
                    ApprovalKind::Run
                } else {
                    ApprovalKind::Refused
                };
                self.approval(at, kind).await;
                let _ = done.send(());
            }
        }
    }

    /// Records one result: the note of the agent's own repair, the re-read
    /// note, then the journal and its commit, so a crash in the middle of the
    /// batch keeps what already ran.
    async fn done(&mut self, at: usize, (result, seen): Outcome) {
        // A note says how the agent itself read the call, so a weak model
        // stops sending the same shape. Only a result with such a note is
        // touched.
        let result = match self
            .calls
            .get(at)
            .and_then(|call| agent_note(call, at, self.repaired))
        {
            Some(note) => with_note(result, note),
            None => result,
        };
        // A file the reply writes loses its earlier reads, and a read of a
        // file the reply writes is not recorded, so neither can answer a later
        // read. A reply with nothing to drop and no read to skip resolves no
        // tool and parses no argument, which is every turn that writes nothing.
        // The set is resolved once, at the first result that needs it.
        if self.written.is_none() && self.ledger.needs_writes(seen.is_some()) {
            self.written = Some(self.ledger.forget_writes(self.calls, self.registry).await);
        }
        // A re-read of lines the run already holds keeps its content and gains
        // the note, before it reaches either the journal or the model.
        let result = match seen {
            Some(seen)
                if !self
                    .written
                    .as_ref()
                    .is_some_and(|written| written.contains(&seen.key)) =>
            {
                self.ledger.apply(result, *seen, self.history)
            }
            _ => result,
        };
        let failed = matches!(&result, Message::ToolResult(result) if result.is_error);
        self.facts[at] = Some((
            tool_bytes(&result),
            tool_output(&result),
            failed,
            images(&result),
        ));
        let mut wrote = false;
        if let Some(journal) = self.journal.as_mut() {
            match journal.message(&result).and_then(|()| journal.commit()) {
                Ok(()) => wrote = true,
                Err(error) => {
                    self.failure.get_or_insert(journal_error(&error));
                }
            }
        }
        self.slots[at] = Some(result);
        if wrote {
            committed(self.journal, self.live, self.events, self.token).await;
        }
    }

    /// Records a call that waits for a client, or the answer it got, so a
    /// resume knows a call without an answer never ran.
    async fn approval(&mut self, at: usize, kind: ApprovalKind) {
        let Some(call) = self.calls.get(at) else {
            return;
        };
        let mut wrote = false;
        if let Some(journal) = self.journal.as_mut() {
            // An approval is rare and waits for a person, so it is synced: the
            // call asks or runs only once the record is on the disk.
            let written = match journal.approval(&call.id, kind) {
                Ok(()) => journal.sync().await,
                Err(error) => Err(error),
            };
            match written {
                Ok(()) => wrote = true,
                Err(error) => {
                    self.failure.get_or_insert(journal_error(&error));
                }
            }
        }
        if wrote {
            committed(self.journal, self.live, self.events, self.token).await;
        }
    }
}

/// Writes one settings change and commits it: the model in its own record,
/// the level and the mode in a settings record.
fn write_settings(
    journal: &mut Journal,
    provider: &str,
    config: Option<&str>,
    settings: &Settings,
) -> std::io::Result<()> {
    let mut wrote = false;
    if let Some(model) = &settings.model {
        journal.model_config(provider, model, config)?;
        wrote = true;
    }
    if settings.level.is_some() || settings.mode.is_some() {
        let mode = settings.mode.map(|mode| mode.as_str());
        journal.settings(settings.level.as_deref(), mode)?;
        wrote = true;
    }
    if wrote {
        journal.commit()?;
    }
    Ok(())
}

/// Tells the clients of a live session the number of the record the journal
/// committed last, so a client can resume its view from it.
///
/// A run with no inbox, and a run with no journal, sends nothing.
async fn committed(
    journal: &Option<&mut Journal>,
    live: bool,
    events: &mpsc::Sender<Event>,
    token: &CancellationToken,
) -> bool {
    if !live {
        return false;
    }
    let Some(journal) = journal.as_ref() else {
        return false;
    };
    emit(
        events,
        token,
        Event::Committed {
            record: journal.record(),
        },
    )
    .await
}

/// Appends `message` to the journal, then to history. A journal error leaves
/// history unchanged.
fn record(
    message: Message,
    history: &mut Vec<Message>,
    journal: &mut Option<&mut Journal>,
) -> Result<(), Text> {
    write(journal, &message)?;
    history.push(message);
    Ok(())
}

/// Writes one message to the journal, when one is open.
fn write(journal: &mut Option<&mut Journal>, message: &Message) -> Result<(), Text> {
    match journal.as_mut() {
        Some(journal) => journal
            .message(message)
            .map_err(|error| journal_error(&error)),
        None => Ok(()),
    }
}

/// Records one tool result, which reaches history even when the journal write
/// fails: a failed write must not drop the answer to a call. The commit after
/// the write keeps it on the disk without an fdatasync.
///
/// Returns the journal error, if any.
fn record_result(
    message: Message,
    history: &mut Vec<Message>,
    journal: &mut Option<&mut Journal>,
) -> Option<Text> {
    let error = match journal.as_mut() {
        Some(journal) => journal
            .message(&message)
            .and_then(|()| journal.commit())
            .err()
            .map(|error| journal_error(&error)),
        None => None,
    };
    history.push(message);
    error
}

/// Writes a base, then the history the run continues with, and commits both.
///
/// A resume starts at the last base, so the records after it must hold every
/// message the run continues with, not only the ones recorded after it.
fn write_base(
    journal: &mut Journal,
    agent: &Agent<'_>,
    history: &[Message],
) -> std::io::Result<()> {
    let state = State::new(
        agent.provider_name.as_ref(),
        agent.model.as_ref(),
        agent.compactions,
    );
    journal.base(&state)?;
    // A resume reads from this base, so the level and the mode a client set
    // follow it; a run nobody changed writes the same records as before.
    if agent.changed_settings {
        let mode = agent.guard.as_ref().map(|guard| guard.mode().as_str());
        journal.settings(agent.config.reasoning.as_deref(), mode)?;
    }
    for message in history {
        journal.message(message)?;
    }
    journal.commit()
}

/// Writes the messages a resumed run adds to the history it continues with.
fn write_after(journal: &mut Journal, added: &[Message]) -> std::io::Result<()> {
    for message in added {
        journal.message(message)?;
    }
    journal.commit()
}

/// Writes the messages a resumed run rebuilt that its parent part lacks.
///
/// The interrupted result a crash left is in the history but not in the log,
/// and the parent part never wrote it. It goes in before the results this run
/// makes, so a later resume reads the log in the order the model saw it. A
/// journal of a new session holds no message yet and writes nothing here.
fn write_pending(journal: &mut Option<&mut Journal>, history: &[Message]) -> std::io::Result<()> {
    let Some(journal) = journal.as_mut() else {
        return Ok(());
    };
    let Some(recorded) = journal.recorded() else {
        return Ok(());
    };
    if history.len() <= recorded {
        return Ok(());
    }
    write_after(journal, &history[recorded..])?;
    crate::journal::resume::mark_recorded(journal, history.len());
    Ok(())
}

/// Writes a call start for each call that is not a read, then syncs once.
///
/// A call that may change the world is on the disk before it runs, so a crash
/// leaves its start without a result and the resume reports it instead of
/// running it again.
async fn note_calls(
    calls: &[ToolCall],
    registry: &Registry,
    journal: &mut Option<&mut Journal>,
) -> Result<bool, Text> {
    let Some(journal) = journal.as_mut() else {
        return Ok(false);
    };
    let mut wrote = false;
    for call in calls {
        let class = registry
            .resolve(&call.name)
            .map_or(EffectClass::LocalReversible, |tool| tool.effect());
        if class == EffectClass::Read {
            continue;
        }
        journal
            .call_start(&call.id, &call.name, class)
            .map_err(|error| journal_error(&error))?;
        wrote = true;
    }
    if wrote {
        journal
            .sync()
            .await
            .map_err(|error| journal_error(&error))?;
    }
    Ok(wrote)
}

/// Writes the end of a turn and waits until it is on the disk.
async fn end_turn(journal: &mut Journal, turn: u64, usage: Usage) -> std::io::Result<()> {
    journal.turn_end(turn, usage)?;
    journal.sync().await
}

/// The usage of the assistant reply of the messages one turn added.
fn turn_usage(messages: &[Message]) -> Usage {
    messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(reply) => Some(reply.usage),
            _ => None,
        })
        .unwrap_or_default()
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

fn journal_error(error: &std::io::Error) -> Text {
    Text::from(format!("journal write failed: {error}"))
}

impl std::fmt::Debug for Agent<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("provider", &self.provider().name())
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

/// What adding a live session's steer texts did.
#[derive(Default)]
pub(crate) struct Drained {
    /// A user message joined the conversation.
    pub steered: bool,
    /// The journal could not hold a message.
    pub error: Option<Text>,
}

impl Drained {
    /// The journal error that ends the run, if any.
    pub fn error(self) -> Option<Text> {
        self.error
    }
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
pub(crate) async fn emit(
    events: &mpsc::Sender<Event>,
    token: &CancellationToken,
    event: Event,
) -> bool {
    // Send first: after a cancel, an event with room in the channel is still
    // delivered, and only a full channel gives way to the token.
    tokio::select! {
        biased;
        result = events.send(event) => result.is_err(),
        () = token.cancelled() => true,
    }
}

/// What [`step`] took from a stream: an item or a command.
enum Step {
    /// The next item of the stream, or its end.
    Item(Option<Result<StreamEvent, Error>>),
    /// A command of a live session arrived first.
    Command(Command),
}

/// The next item of `stream`, the next command of a live session, or `None`
/// when the run was cancelled first.
///
/// With no inbox a headless `-p` run polls only the stream and the token, as it
/// always has. A closed inbox never returns, so a stream that is still running
/// is not polled in a spin.
async fn step(
    stream: &mut futures_util::stream::BoxStream<'static, Result<StreamEvent, Error>>,
    token: &CancellationToken,
    inbox: Option<&mut Inbox>,
) -> Option<Step> {
    match inbox {
        None => tokio::select! {
            item = stream.next() => Some(Step::Item(item)),
            () = token.cancelled() => None,
        },
        Some(inbox) => tokio::select! {
            item = stream.next() => Some(Step::Item(item)),
            () = token.cancelled() => None,
            command = inbox.next() => Some(Step::Command(command)),
        },
    }
}

/// What [`take_ready`] took from the stream besides the events.
struct Batch {
    /// The events of the batch that go to the channel, one slot each.
    sent: usize,
    /// The stream error that ended the batch, after the events before it.
    error: Option<Error>,
    /// The stream returned its end, so it must not be polled again.
    ended: bool,
}

/// Fills `raw` with `first` and every event `stream` has ready now, up to
/// `cap`, each beside its channel event.
///
/// A tool call end closes the batch: its `tool_call_ready` is sent outside the
/// reservation, and the slots reserved for later events would otherwise block
/// it on a small channel. Kept out of the async loop so its state machine stays
/// small.
#[inline(never)]
fn take_ready(
    stream: &mut futures_util::stream::BoxStream<'static, Result<StreamEvent, Error>>,
    first: Result<StreamEvent, Error>,
    raw: &mut Vec<(StreamEvent, Option<Event>)>,
    cap: usize,
) -> Batch {
    raw.clear();
    let mut batch = Batch {
        sent: 0,
        error: None,
        ended: false,
    };
    let mut item = Some(first);
    while let Some(next) = item.take() {
        match next {
            Ok(event) => {
                let closes = matches!(event, StreamEvent::ToolCallEnd { .. });
                let mapped = stream_event(&event);
                batch.sent += usize::from(mapped.is_some());
                raw.push((event, mapped));
                if closes {
                    break;
                }
            }
            Err(failure) => {
                batch.error = Some(failure);
                break;
            }
        }
        if raw.len() < cap {
            match stream.next().now_or_never() {
                Some(Some(next)) => item = Some(next),
                Some(None) => batch.ended = true,
                // Nothing is ready now: the batch is what the loop has.
                None => {}
            }
        }
    }
    batch
}

/// Sends each event of `raw` through its reserved slot and gives it to the
/// assembler, in stream order.
#[inline(never)]
fn deliver(
    raw: &mut Vec<(StreamEvent, Option<Event>)>,
    mut permits: Option<mpsc::PermitIterator<'_, Event>>,
    assembler: &mut Assembler,
) {
    for (event, mapped) in raw.drain(..) {
        if let Some(mapped) = mapped {
            let permit = permits
                .as_mut()
                .and_then(Iterator::next)
                .expect("room is reserved for the batch");
            permit.send(mapped);
        }
        assembler.push(event);
    }
}

/// Reserves room in `events` for `n` events at once, waiting on the
/// cancellation token. `None` stops the run: a closed receiver, or a cancel
/// while waiting for room.
///
/// The reserve is polled before the token, so a batch the channel has room for
/// is delivered even after a cancel, as a single send is; only waiting for
/// room gives way to the token.
async fn reserve<'a>(
    events: &'a mpsc::Sender<Event>,
    token: &CancellationToken,
    n: usize,
) -> Option<mpsc::PermitIterator<'a, Event>> {
    tokio::select! {
        biased;
        result = events.reserve_many(n) => result.ok(),
        () = token.cancelled() => None,
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

/// The calls of the last assistant message that `pending` names and that no
/// result answers yet, with the arguments the model sent.
fn unanswered(history: &[Message], pending: &[PendingCall]) -> Vec<ToolCall> {
    let Some(Message::Assistant(reply)) = history
        .iter()
        .rev()
        .find(|message| matches!(message, Message::Assistant(_)))
    else {
        return Vec::new();
    };
    let ask = tool_calls(reply);
    if ask.is_empty() {
        return Vec::new();
    }
    let answered: HashSet<&str> = history
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result.call_id.as_ref()),
            _ => None,
        })
        .collect();
    ask.into_iter()
        .filter(|call| {
            !answered.contains(call.id.as_ref())
                && pending.iter().any(|want| want.id == call.id.as_ref())
        })
        .collect()
}

/// Tells the journal that the first `len` messages are already recorded.
fn mark_recorded(journal: &mut Option<&mut Journal>, len: usize) {
    if let Some(journal) = journal.as_mut() {
        crate::journal::resume::mark_recorded(journal, len);
    }
}

/// Whether `message` ended its turn with reasoning and nothing else.
fn reasoning_only(message: &AssistantMessage) -> bool {
    message.stop_reason == Some(StopReason::EndTurn)
        && message.content.iter().all(|content| match content {
            Content::Thinking { .. } | Content::RedactedThinking { .. } => true,
            Content::Text { text } => text.trim().is_empty(),
            _ => false,
        })
}

/// Moves a think block a host without a reasoning parser served as visible
/// text, as `<think>plan</think>answer` or `plan</think>answer` when the chat
/// template already opened the tag, into the reasoning of the reply.
fn split_think(message: &mut AssistantMessage) {
    // A reply that reasoned on its own needs no reasoning read out of its
    // text, and a bare closing tag there is likely just a mention of one.
    let reasoned = message.content.iter().any(|block| {
        matches!(
            block,
            Content::Thinking { .. } | Content::RedactedThinking { .. }
        )
    });
    let tagged = message.content.iter().any(|block| {
        matches!(block, Content::Text { text } if split_think_text(text, !reasoned).is_some())
    });
    if !tagged {
        return;
    }
    let mut content = Vec::with_capacity(message.content.len() + 1);
    for block in std::mem::take(&mut message.content) {
        let Content::Text { text } = &block else {
            content.push(block);
            continue;
        };
        let Some((thinking, rest)) = split_think_text(text, !reasoned) else {
            content.push(block);
            continue;
        };
        if !thinking.trim().is_empty() {
            content.push(Content::Thinking {
                thinking: Text::from(thinking),
                signature: None,
            });
        }
        let rest = rest.trim_start();
        if !rest.is_empty() {
            content.push(Content::text(rest));
        }
    }
    message.content = content;
}

/// Splits one text block into the reasoning before its closing think tag and
/// the text after it, or `None` when the block carries no think tag.
///
/// The bare closing tag, with no opening one, is read as the end of a block
/// only when `bare` allows it and only where a block can end: at the end of
/// the text or before a newline. An answer that mentions `</think>` mid
/// sentence keeps its text.
fn split_think_text(text: &str, bare: bool) -> Option<(&str, &str)> {
    const CLOSE: &str = "</think>";
    if let Some(opened) = text.trim_start().strip_prefix("<think>") {
        // An opening tag with no closing one makes all of the rest reasoning.
        return Some(match opened.find(CLOSE) {
            Some(end) => (&opened[..end], &opened[end + CLOSE.len()..]),
            None => (opened, ""),
        });
    }
    if !bare {
        return None;
    }
    let end = text.find(CLOSE)?;
    let rest = &text[end + CLOSE.len()..];
    let after_spaces = rest.trim_start_matches([' ', '\t']);
    if !(after_spaces.is_empty() || after_spaces.starts_with('\n')) {
        return None;
    }
    Some((&text[..end], rest))
}

/// Sends the start and ready events of a call taken from the reasoning.
/// Returns whether the run was cancelled.
async fn emit_call(
    events: &mpsc::Sender<Event>,
    token: &CancellationToken,
    call: ToolCall,
) -> bool {
    let start = Event::ToolStart {
        id: call.id.clone(),
        name: call.name.clone(),
    };
    if emit(events, token, start).await {
        return true;
    }
    let ready = Event::ToolCallReady {
        id: call.id,
        name: call.name,
        arguments: call.arguments,
    };
    emit(events, token, ready).await
}

/// The id prefix of a call [`Agent::leaked_call`] read out of the reply text.
const LEAKED_CALL_ID: &str = "leaked_call_";

/// The note for the agent's own repair of `call`, whose place among the
/// reply's calls is `at`.
fn agent_note(call: &ToolCall, at: usize, repaired: &[usize]) -> Option<&'static str> {
    if call.id.starts_with(LEAKED_CALL_ID) {
        return Some("call read from the reply text; send calls as tool calls");
    }
    repaired
        .contains(&at)
        .then_some("arguments were not plain JSON; fixed")
}

/// `message` after a first line that says how the agent read the call, joined
/// into the line a note from the batch already starts.
fn with_note(message: Message, note: &str) -> Message {
    const PREFIX: &str = "[repaired: ";
    let mut result = match message {
        Message::ToolResult(result) => result,
        other => return other,
    };
    let content = result.content.as_ref();
    let mut text = String::with_capacity(content.len() + PREFIX.len() + note.len() + 2);
    text.push_str(PREFIX);
    text.push_str(note);
    match content.strip_prefix(PREFIX) {
        Some(rest) => {
            text.push_str("; ");
            text.push_str(rest);
        }
        None => {
            text.push_str("]\n");
            text.push_str(content);
        }
    }
    result.content = Text::from(text);
    Message::ToolResult(result)
}

/// The output text of a result, shared with the message: a clone of the text
/// is an `Arc` clone. A result with images and no text carries `None`.
fn tool_output(message: &Message) -> Option<Text> {
    match message {
        Message::ToolResult(result) if !result.content.is_empty() => {
            Some(Text::clone(&result.content))
        }
        _ => None,
    }
}

fn tool_bytes(message: &Message) -> usize {
    match message {
        Message::ToolResult(result) => result.content.len(),
        _ => 0,
    }
}

/// The images of a result, shared with the message: a clone of the base64 text
/// is an `Arc` clone, and a result without images does not allocate.
fn images(message: &Message) -> Vec<Image> {
    match message {
        Message::ToolResult(result) => result.images.clone(),
        _ => Vec::new(),
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
    /// How many tool calls the content holds.
    calls: usize,
    /// The arguments as sent of each call, by its place among the calls,
    /// whose arguments are no JSON object. The content holds `{}` for them,
    /// as a provider rejects a history that sends them back.
    broken: Vec<(usize, Text)>,
    /// The place among the calls of each call whose arguments were not plain
    /// JSON and were repaired from a string, a fence, trailing text or an
    /// escape.
    repaired: Vec<usize>,
    stop_reason: Option<StopReason>,
    usage: Usage,
    replay: Option<Text>,
}

impl Assembler {
    fn new() -> Self {
        Self {
            content: Vec::new(),
            text: String::new(),
            thinking: None,
            tools: Vec::new(),
            ready: Vec::new(),
            calls: 0,
            broken: Vec::new(),
            repaired: Vec::new(),
            stop_reason: None,
            usage: Usage::default(),
            replay: None,
        }
    }

    fn push(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::TextDelta(delta) => {
                // Only a thinking block ends here: the text so far stays open,
                // or every delta would become a block sent again each turn.
                self.close_thinking();
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
                    let (arguments, sent) = if tool.args.trim().is_empty() {
                        (Text::from("{}"), None)
                    } else {
                        match crate::repair::arguments(tool.args) {
                            Ok((arguments, repaired)) => {
                                if repaired {
                                    self.repaired.push(self.calls);
                                }
                                (Text::from(arguments), None)
                            }
                            Err(raw) => (Text::from("{}"), Some(Text::from(raw))),
                        }
                    };
                    self.ready.push(ToolReady {
                        id: tool.id.clone(),
                        name: tool.name.clone(),
                        arguments: sent.clone().unwrap_or_else(|| arguments.clone()),
                    });
                    if let Some(sent) = sent {
                        self.broken.push((self.calls, sent));
                    }
                    self.calls += 1;
                    self.content.push(Content::ToolCall(ToolCall {
                        id: tool.id,
                        name: tool.name,
                        arguments,
                    }));
                }
            }
            StreamEvent::Usage(usage) => self.usage = usage,
            StreamEvent::Stop(reason) => self.stop_reason = Some(reason),
            StreamEvent::Replay(value) => self.replay = Some(value),
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
        if !self.close_thinking() {
            self.close_text();
        }
    }

    /// Closes an open thinking block into the content; whether one was open.
    fn close_thinking(&mut self) -> bool {
        let Some(thinking) = self.thinking.take() else {
            return false;
        };
        self.content.push(Content::Thinking {
            thinking: Text::from(thinking.text),
            signature: thinking.signature,
        });
        true
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
            replay: self.replay,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn replay_a_wire_value_lands_on_the_message_once() {
        let mut assembler = Assembler::new();
        assembler.push(StreamEvent::TextDelta("hi".into()));
        assembler.push(StreamEvent::Replay(r#"{"responseId":"r1"}"#.into()));
        assembler.push(StreamEvent::Stop(StopReason::EndTurn));
        let message = assembler.finish("radius".into(), "balanced".into());
        assert_eq!(message.replay.as_deref(), Some(r#"{"responseId":"r1"}"#));
        assert_eq!(message.content, vec![Content::text("hi")]);
        // The value is the wire's own: no channel event carries it.
        assert!(stream_event(&StreamEvent::Replay("{}".into())).is_none());
    }

    #[test]
    fn replay_a_message_without_one_serializes_as_before() {
        let message = Assembler::new().finish("anthropic".into(), "m".into());
        assert_eq!(message.replay, None);
        let json = serde_json::to_string(&message).expect("serializes");
        assert!(!json.contains("replay"), "{json}");
        let mut with = message;
        with.replay = Some(r#"{"a":1}"#.into());
        let back: AssistantMessage =
            serde_json::from_str(&serde_json::to_string(&with).expect("serializes"))
                .expect("parses");
        assert_eq!(back.replay.as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn one_text_block_holds_every_delta_of_a_reply() {
        let mut assembler = Assembler::new();
        assembler.push(StreamEvent::TextDelta("a".into()));
        assembler.push(StreamEvent::TextDelta("b".into()));
        assembler.push(StreamEvent::ThinkingDelta("t".into()));
        assembler.push(StreamEvent::TextDelta("c".into()));
        assembler.push(StreamEvent::TextDelta("d".into()));
        let message = assembler.finish("p".into(), "m".into());
        let texts: Vec<&str> = message
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(&**text),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["ab", "cd"]);
        assert!(matches!(message.content[1], Content::Thinking { .. }));
    }
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    use bytes::Bytes;
    use futures_util::future::BoxFuture;
    use reagent_ai::Error;
    use reagent_ai::fake::FakeProvider;
    use reagent_ai::{Image, ImageType, Role, ToolSpec};
    use reagent_config::permissions::PermissionMode;
    use reagent_store::{Header, Session};
    use serde::Deserialize;
    use serde_json::json;

    use crate::journal::Journal;
    use crate::journal::record::{self, Record};
    use crate::tool::{Effects, Identity, Seen, Tool, ToolOutput, parse_args};

    /// Opens a journal over `dir`, as a run does.
    fn journal(dir: &Path) -> Journal {
        let header = Header::new(
            "session".into(),
            reagent_store::paths::now_ms(),
            None,
            Bytes::new(),
        );
        Journal::create(dir, header).expect("create the journal")
    }

    /// Every record of the newest part, decoded, read without closing the journal.
    fn read_records(dir: &Path) -> Vec<Record> {
        let session = Session::open(dir).expect("open the session");
        session
            .records()
            .map(|record| {
                let record = record.expect("read a record");
                record::decode(record.kind, &record.payload).expect("decode a record")
            })
            .collect()
    }

    /// The messages of `records`, in order.
    fn messages(records: &[Record]) -> Vec<Message> {
        records
            .iter()
            .filter_map(|record| match record {
                Record::Message(message) => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

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
        tool_replies(&[(call_id, name, args)])
    }

    /// One reply that calls several tools, in order, and stops for them.
    fn tool_replies(calls: &[(&str, &str, &str)]) -> Vec<StreamEvent> {
        let mut events = Vec::with_capacity(calls.len() * 3 + 1);
        for (call_id, name, args) in calls {
            events.push(StreamEvent::ToolCallStart {
                id: Text::from(*call_id),
                name: Text::from(*name),
            });
            events.push(StreamEvent::ToolCallDelta {
                id: Text::from(*call_id),
                partial_json: Text::from(*args),
            });
            events.push(StreamEvent::ToolCallEnd {
                id: Text::from(*call_id),
            });
        }
        events.push(StreamEvent::Stop(StopReason::ToolUse));
        events
    }

    fn config() -> AgentConfig {
        AgentConfig {
            model: "fake-model".into(),
            max_tokens: 128,
            system: None,
            reasoning: None,
            cache_key: None,
            max_turns: None,
            images: true,
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
    async fn the_request_carries_the_reasoning_text() {
        let provider = FakeProvider::new(vec![text_reply("done")]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];
        let config = AgentConfig {
            reasoning: Some("xhigh".into()),
            ..config()
        };

        Agent::new(&provider, &registry, config, CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(provider.seen()[0].reasoning.as_deref(), Some("xhigh"));
    }

    #[tokio::test]
    async fn a_run_without_a_reasoning_text_sends_none() {
        let provider = FakeProvider::new(vec![text_reply("done")]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(provider.seen()[0].reasoning, None);
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
    async fn repeated_call_ids_are_made_unique() {
        let provider = FakeProvider::new(vec![
            tool_reply("call_0", "echo", r#"{"message":"a"}"#),
            tool_reply("call_0", "echo", r#"{"message":"b"}"#),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];
        let archive = Archive::new();

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .with_archive(Arc::clone(&archive))
            .run(&mut history, None, &tx)
            .await;

        let ids: Vec<String> = history
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(result.call_id.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["call_0", "call_0_2"]);
        assert_eq!(archive.len(), 2);
        assert_eq!(archive.id_of("call_0_2"), Some(2));
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
    async fn records_each_message_to_the_journal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FakeProvider::new(vec![
            tool_reply("call_1", "echo", r#"{"message":"hi"}"#),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];
        let mut journal = journal(dir.path());

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, Some(&mut journal), &tx)
            .await;
        journal.close().expect("close the journal");

        // The base re-records the history the run starts with, then the run
        // records assistant(tool call), the tool result and
        // assistant(done).
        let records = read_records(dir.path());
        assert_eq!(messages(&records), history);
        let turn_ends = records
            .iter()
            .filter(|record| matches!(record, Record::TurnEnd(_)))
            .count();
        assert_eq!(turn_ends, 2, "one per turn");
    }

    #[tokio::test]
    async fn config_provider_of_a_set_reaches_the_model_record() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FakeProvider::new(Vec::new());
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let (sender, mut inbox) = crate::live::channel();
        let mut history = Vec::new();
        let mut journal = journal(dir.path());
        let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new());

        agent
            .set(
                Settings {
                    model: Some(Text::from("gpt-5")),
                    config_provider: Some(Text::from("openrouter")),
                    ..Settings::default()
                },
                &tx,
            )
            .await;
        agent
            .flush(&mut history, &mut Some(&mut journal), &mut inbox, &tx)
            .await;
        journal.close().expect("close the journal");
        drop(sender);

        assert_eq!(
            read_records(dir.path()),
            [Record::Model(record::Model {
                provider: "fake".into(),
                model: "gpt-5".into(),
                config: Some(Text::from("openrouter")),
            })]
        );
    }

    /// The first base of a new session holds a level set before its first run,
    /// so the change is not written a second time after it.
    #[tokio::test]
    async fn a_level_set_before_the_first_run_is_recorded_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FakeProvider::new(vec![text_reply("done")]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let (sender, mut inbox) = crate::live::channel();
        let mut history = vec![Message::user("hello")];
        let mut journal = journal(dir.path());
        let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new());

        agent
            .set(
                Settings {
                    level: Some(Text::from("high")),
                    ..Settings::default()
                },
                &tx,
            )
            .await;
        agent
            .run_with_inbox(&mut history, Some(&mut journal), &tx, &mut inbox)
            .await;
        journal.close().expect("close the journal");
        drop(sender);

        let session = Session::open(dir.path()).expect("open the session");
        let settings = session
            .records()
            .filter(|record| {
                record
                    .as_ref()
                    .is_ok_and(|record| record.kind == crate::journal::kinds::SETTINGS)
            })
            .count();
        assert_eq!(
            settings, 1,
            "the base holds the level, and nothing after it"
        );
    }

    /// A tool whose result is too big for a record of the log.
    struct Huge {
        spec: ToolSpec,
    }

    impl Tool for Huge {
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
                // One byte over the largest payload a frame takes, so the
                // record of this result cannot be written.
                ToolOutput::ok("x".repeat(reagent_store::frame::MAX_PAYLOAD + 1))
            })
        }
    }

    /// A pending call whose result the log cannot hold ends the run as an
    /// error, not as a run that finished.
    #[tokio::test]
    async fn a_pending_result_the_log_cannot_hold_ends_the_run_as_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut tools = Registry::new();
        tools.add(Huge {
            spec: ToolSpec {
                name: "huge".into(),
                description: "A result no record can hold.".into(),
                input_schema: json!({"type": "object"}),
            },
        });
        let provider = FakeProvider::new(Vec::new());
        let mut assembler = Assembler::new();
        for event in tool_reply("call_1", "huge", "{}") {
            assembler.push(event);
        }
        let mut history = vec![
            Message::user("hello"),
            Message::Assistant(assembler.finish("fake".into(), "fake-model".into())),
        ];
        let pending = [PendingCall {
            id: "call_1".into(),
            name: "huge".into(),
            class: Some(EffectClass::Read),
        }];
        let (tx, mut rx) = mpsc::channel(64);
        let mut journal = journal(dir.path());

        let reason = Agent::new(&provider, &tools, config(), CancellationToken::new())
            .run_pending(&mut history, &mut Some(&mut journal), &tx, &pending)
            .await;

        assert_eq!(reason, TurnEndReason::Error);
        // The run ends with the reason of its pending calls, so the exit code
        // and the list entry say the run failed.
        let events = drain(tx, &mut rx);
        assert_eq!(
            events.last(),
            Some(&Event::RunEnd(TurnEndReason::Error)),
            "{events:?}"
        );
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
        journal: Option<Journal>,
    ) -> (Vec<Message>, Vec<Event>, Option<Journal>) {
        let provider = FakeProvider::new(vec![two_slow_calls()]);
        let registry = registry_with_slow_tool();
        let (tx, mut rx) = mpsc::channel(64);
        let token = CancellationToken::new();
        let cancellation = token.clone();

        let handle = tokio::spawn(async move {
            let mut history = vec![Message::user("hello")];
            let mut journal = journal;
            Agent::new(&provider, &registry, config(), token)
                .run(&mut history, journal.as_mut(), &tx)
                .await;
            (history, journal, tx)
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancellation.cancel();
        let (history, journal, tx) = tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("the loop stops after cancel")
            .expect("runner finishes");
        (history, drain(tx, &mut rx), journal)
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

    #[tokio::test]
    async fn a_cancelled_tool_turn_syncs_the_journal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let journal = journal(dir.path());

        let (history, _, journal) = cancel_two_slow_calls(Some(journal)).await;

        // Read before the journal closes: only the turn-end sync can have
        // written the turn end.
        let records = read_records(dir.path());
        assert_eq!(messages(&records), history);
        let turn_ends = records
            .iter()
            .filter(|record| matches!(record, Record::TurnEnd(_)))
            .count();
        assert_eq!(turn_ends, 1, "{records:?}");
        drop(journal);
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

    /// Runs a reply of `deltas` deltas through the loop and returns every
    /// event the receiver got, in order, drained in the same runtime.
    async fn batch_events(
        provider: &dyn Provider,
        token: CancellationToken,
        capacity: usize,
        events: usize,
    ) -> (Vec<Event>, Vec<Message>) {
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(capacity);
        let mut history = vec![Message::user("hello")];
        let mut agent = Agent::new(provider, &registry, config(), token);
        let run = async {
            agent.run(&mut history, None, &tx).await;
            drop(tx);
        };
        let drain = async {
            let mut seen = Vec::with_capacity(events);
            while let Some(event) = rx.recv().await {
                seen.push(event);
            }
            seen
        };
        let ((), seen) = tokio::join!(run, drain);
        (seen, history)
    }

    /// A reply of `deltas` one-character text deltas and a normal end.
    fn many_deltas(deltas: usize) -> Vec<StreamEvent> {
        let mut reply: Vec<StreamEvent> = (0..deltas)
            .map(|_| StreamEvent::TextDelta(Text::from("x")))
            .collect();
        reply.push(StreamEvent::Stop(StopReason::EndTurn));
        reply
    }

    #[tokio::test]
    async fn batched_events_arrive_in_order_and_the_reply_is_whole() {
        let provider = FakeProvider::new(vec![many_deltas(1000)]);
        let (seen, history) = batch_events(&provider, CancellationToken::new(), 256, 1004).await;

        assert_eq!(seen.first(), Some(&Event::TurnStart));
        let deltas: Vec<&Arc<str>> = seen
            .iter()
            .filter_map(|event| match event {
                Event::TextDelta(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 1000);
        assert!(deltas.iter().all(|text| &***text == "x"));
        assert_eq!(seen.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
        // The batch order is the stream order, so the reply reads as it came.
        let reply = history
            .iter()
            .find_map(|message| match message {
                Message::Assistant(message) => Some(message),
                _ => None,
            })
            .expect("the assistant message is recorded");
        let text: String = reply
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(&**text),
                _ => None,
            })
            .collect();
        assert_eq!(text, "x".repeat(1000));
    }

    #[tokio::test(start_paused = true)]
    async fn batched_events_a_cancel_while_waiting_for_room_keeps_what_was_sent() {
        // Room for 128 events and a reply of 300: the first batch of 64 fits
        // next to the turn start, the second does not, and only the token
        // frees the loop. Nobody drains, so the channel stays as it was.
        let provider = FakeProvider::new(vec![many_deltas(300)]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(128);
        let token = CancellationToken::new();
        let cancellation = token.clone();
        let mut history = vec![Message::user("hello")];

        let handle = tokio::spawn(async move {
            let mut agent = Agent::new(&provider, &registry, config(), token);
            agent.run(&mut history, None, &tx).await;
            (history, tx)
        });
        // The loop runs until the reservation it waits for cannot fit.
        tokio::task::yield_now().await;
        cancellation.cancel();
        let (history, tx) = tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("cancel unblocks the reservation")
            .expect("runner finishes");

        let seen = drain(tx, &mut rx);
        // The turn start and the first batch, in order, then the ends the loop
        // can still fit after the cancel: a send with room is delivered.
        assert_eq!(seen.first(), Some(&Event::TurnStart));
        let deltas: Vec<&Arc<str>> = seen
            .iter()
            .filter_map(|event| match event {
                Event::TextDelta(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), EVENT_BATCH);
        assert!(deltas.iter().all(|text| &***text == "x"));
        // The run ends once, and only the delivered batch reached the
        // assembler, so the recorded reply is the batch and nothing past it.
        let ends = seen
            .iter()
            .filter(|event| matches!(event, Event::TurnEnd(_) | Event::RunEnd(_)))
            .count();
        assert_eq!(ends, 2);
        assert_eq!(seen.last(), Some(&Event::RunEnd(TurnEndReason::Cancelled)));
        assert_eq!(history.len(), 2);
        let recorded = history
            .iter()
            .find_map(|message| match message {
                Message::Assistant(message) => Some(message),
                _ => None,
            })
            .expect("the partial reply is recorded");
        let text: String = recorded
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(&**text),
                _ => None,
            })
            .collect();
        assert_eq!(text.len(), EVENT_BATCH);
    }

    #[tokio::test(start_paused = true)]
    async fn batched_events_a_tool_call_end_on_a_small_channel_does_not_hang() {
        // Room for 4 events and a call whose arguments come in 10 pieces, all
        // ready at once: the third batch is 3 pieces and the end, and sends
        // nothing, so slots held for it would leave no room for the
        // `tool_call_ready` the end brings.
        let mut call = vec![StreamEvent::ToolCallStart {
            id: Text::from("call_1"),
            name: Text::from("echo"),
        }];
        for piece in ["{", "\"mes", "sage", "\"", ":", "\"", "h", "i", "\"", "}"] {
            call.push(StreamEvent::ToolCallDelta {
                id: Text::from("call_1"),
                partial_json: Text::from(piece),
            });
        }
        call.push(StreamEvent::ToolCallEnd {
            id: Text::from("call_1"),
        });
        call.push(StreamEvent::Stop(StopReason::ToolUse));
        let provider = FakeProvider::new(vec![call, text_reply("done")]);

        let (seen, _) = tokio::time::timeout(
            Duration::from_secs(5),
            batch_events(&provider, CancellationToken::new(), 4, 16),
        )
        .await
        .expect("the run does not hang on a full channel");

        assert!(seen.iter().any(|event| matches!(
            event,
            Event::ToolCallReady { arguments, .. } if &**arguments == r#"{"message":"hi"}"#
        )));
        assert_eq!(seen.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
    }

    /// A provider that hands out one event per poll, never two at once.
    struct Trickle {
        events: Vec<StreamEvent>,
    }

    impl Provider for Trickle {
        fn name(&self) -> &'static str {
            "trickle"
        }

        fn stream(
            &self,
            _request: &Request<'_>,
        ) -> futures_util::stream::BoxStream<'static, Result<StreamEvent, Error>> {
            let events = self.events.clone();
            Box::pin(futures_util::stream::iter(events).then(|event| async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                Ok(event)
            }))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn batched_events_a_trickling_stream_still_works() {
        // A batch of one every time: nothing is ready together, so every event
        // is reserved and sent on its own.
        let provider = Trickle {
            events: many_deltas(100),
        };
        let (seen, _) = batch_events(&provider, CancellationToken::new(), 256, 104).await;

        let deltas = seen
            .iter()
            .filter(|event| matches!(event, Event::TextDelta(_)))
            .count();
        assert_eq!(deltas, 100);
        assert_eq!(seen.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
    }

    /// Arguments for one call of the fake read below.
    #[derive(Deserialize)]
    struct FakeReadArgs<'a> {
        #[serde(borrow)]
        path: Cow<'a, str>,
        start: u64,
        end: u64,
    }

    /// Arguments for one call of the fake write below.
    #[derive(Deserialize)]
    struct FakeWriteArgs<'a> {
        #[serde(borrow)]
        path: Cow<'a, str>,
    }

    /// A read tool that reports the lines it "read" of a real file.
    struct FakeRead {
        spec: ToolSpec,
        dir: std::path::PathBuf,
    }

    impl Tool for FakeRead {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn read_only(&self) -> bool {
            true
        }

        fn main_arg(&self) -> Option<&'static str> {
            Some("path")
        }

        fn effects(&self, args: &str) -> Effects {
            let Ok(arguments) = parse_args::<FakeReadArgs<'_>>(args) else {
                return Effects::default();
            };
            Effects {
                reads: vec![self.dir.join(arguments.path.as_ref())],
                writes: Vec::new(),
            }
        }

        fn run<'a>(
            &'a self,
            args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async move {
                let Ok(arguments) = parse_args::<FakeReadArgs<'a>>(args) else {
                    return ToolOutput::error("bad args");
                };
                let path = self.dir.join(arguments.path.as_ref());
                let metadata = match tokio::fs::metadata(&path).await {
                    Ok(metadata) => metadata,
                    Err(error) => return ToolOutput::error(format!("no file: {error}")),
                };
                let key = Identity::new(&metadata, &path).await;
                let mut content = String::new();
                for number in arguments.start..=arguments.end {
                    content.push_str(&format!("{number}\tline {number}\n"));
                }
                ToolOutput::ok(content).with_seen(Seen {
                    path: Text::from(arguments.path.as_ref()),
                    key,
                    start: arguments.start,
                    end: arguments.end,
                    size: metadata.len(),
                    modified: metadata.modified().ok(),
                    cut: false,
                })
            })
        }
    }

    /// A write tool that names the file it writes.
    struct FakeWrite {
        spec: ToolSpec,
        dir: std::path::PathBuf,
    }

    impl Tool for FakeWrite {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn main_arg(&self) -> Option<&'static str> {
            Some("path")
        }

        fn effects(&self, args: &str) -> Effects {
            let Ok(arguments) = parse_args::<FakeWriteArgs<'_>>(args) else {
                return Effects::default();
            };
            Effects {
                reads: Vec::new(),
                writes: vec![self.dir.join(arguments.path.as_ref())],
            }
        }

        fn run<'a>(
            &'a self,
            _args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async { ToolOutput::ok("wrote") })
        }
    }

    fn read_spec() -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read lines of a file.".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn read_registry(dir: &std::path::Path, write: bool) -> Registry {
        let mut registry = Registry::new();
        registry.add(FakeRead {
            spec: read_spec(),
            dir: dir.to_path_buf(),
        });
        if write {
            registry.add(FakeWrite {
                spec: ToolSpec {
                    name: "write".into(),
                    description: "Write a file.".into(),
                    input_schema: json!({"type": "object"}),
                },
                dir: dir.to_path_buf(),
            });
        }
        registry
    }

    fn result_text(history: &[Message], index: usize) -> String {
        match history.get(index) {
            Some(Message::ToolResult(result)) => result.content.to_string(),
            other => panic!("expected a tool result at {index}: {other:?}"),
        }
    }

    #[tokio::test]
    async fn reread_the_same_range_twice_adds_the_note() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "a\nb\n")
            .await
            .unwrap();
        let provider = FakeProvider::new(vec![
            tool_reply("call_1", "read", r#"{"path":"f.txt","start":1,"end":2}"#),
            tool_reply("call_2", "read", r#"{"path":"f.txt","start":1,"end":2}"#),
            text_reply("done"),
        ]);
        let registry = read_registry(tmp.path(), false);
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("go")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let first = result_text(&history, 2);
        assert_eq!(first, "1\tline 1\n2\tline 2\n");
        let second = result_text(&history, 4);
        assert_eq!(
            second,
            concat!(
                "1\tline 1\n2\tline 2\n",
                "[Note: you already read lines 1-2 of f.txt in this run, and the ",
                "file has not changed. That earlier result is still valid; you do ",
                "not need to read this range again.]"
            )
        );
        // The event counts the noted text, which is what the model sees.
        let events = drain(tx, &mut rx);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::ToolResult { id, bytes, .. }
                    if id.as_ref() == "call_2" && *bytes == second.len())),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn reread_a_contained_range_is_noted_with_the_new_range() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "a\nb\nc\nd\ne\n")
            .await
            .unwrap();
        let provider = FakeProvider::new(vec![
            tool_reply("call_1", "read", r#"{"path":"f.txt","start":1,"end":5}"#),
            tool_reply("call_2", "read", r#"{"path":"f.txt","start":2,"end":3}"#),
            text_reply("done"),
        ]);
        let registry = read_registry(tmp.path(), false);
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("go")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let second = result_text(&history, 4);
        assert!(
            second.starts_with("2\tline 2\n3\tline 3\n"),
            "the content stays first: {second}"
        );
        assert!(
            second.contains("you already read lines 2-3 of f.txt"),
            "{second}"
        );
    }

    #[tokio::test]
    async fn reread_new_lines_are_not_noted() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "a\nb\nc\nd\n")
            .await
            .unwrap();
        let provider = FakeProvider::new(vec![
            tool_reply("call_1", "read", r#"{"path":"f.txt","start":1,"end":3}"#),
            tool_reply("call_2", "read", r#"{"path":"f.txt","start":2,"end":4}"#),
            text_reply("done"),
        ]);
        let registry = read_registry(tmp.path(), false);
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("go")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(
            result_text(&history, 4),
            "2\tline 2\n3\tline 3\n4\tline 4\n",
            "a range with new lines gets no note"
        );
    }

    #[tokio::test]
    async fn reread_a_write_of_the_file_drops_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "a\nb\n")
            .await
            .unwrap();
        let provider = FakeProvider::new(vec![
            tool_reply("call_1", "read", r#"{"path":"f.txt","start":1,"end":2}"#),
            tool_reply("call_2", "write", r#"{"path":"f.txt"}"#),
            tool_reply("call_3", "read", r#"{"path":"f.txt","start":1,"end":2}"#),
            text_reply("done"),
        ]);
        let registry = read_registry(tmp.path(), true);
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("go")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(
            result_text(&history, 6),
            "1\tline 1\n2\tline 2\n",
            "the run's own write dropped the entry"
        );
    }

    #[tokio::test]
    async fn reread_the_third_read_names_the_third_time() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "a\nb\n")
            .await
            .unwrap();
        let call = || tool_reply("call", "read", r#"{"path":"f.txt","start":1,"end":2}"#);
        let provider = FakeProvider::new(vec![call(), call(), call(), text_reply("done")]);
        let registry = read_registry(tmp.path(), false);
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("go")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert!(
            result_text(&history, 4).ends_with("again.]"),
            "the second read has no count"
        );
        assert!(
            result_text(&history, 6).ends_with("again.] (3rd time)"),
            "the third read counts: {}",
            result_text(&history, 6)
        );
    }

    #[tokio::test]
    async fn reread_a_reply_that_reads_and_writes_the_file_has_no_note() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "a\nb\n")
            .await
            .unwrap();
        let provider = FakeProvider::new(vec![
            tool_replies(&[
                ("call_1", "read", r#"{"path":"f.txt","start":1,"end":2}"#),
                ("call_2", "write", r#"{"path":"f.txt"}"#),
            ]),
            tool_reply("call_3", "read", r#"{"path":"f.txt","start":1,"end":2}"#),
            text_reply("done"),
        ]);
        let registry = read_registry(tmp.path(), true);
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("go")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        // The prompt, the first reply, its two results, the second reply, its
        // result and the answer.
        assert_eq!(history.len(), 7);
        assert_eq!(
            result_text(&history, 5),
            "1\tline 1\n2\tline 2\n",
            "a read of a file the same reply writes is never noted"
        );
    }

    fn calls_in(history: &[Message]) -> Vec<&ToolCall> {
        history
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .flat_map(|assistant| assistant.content.iter())
            .filter_map(|content| match content {
                Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect()
    }

    /// The texts of the assistant replies in `history`.
    fn texts_in(history: &[Message]) -> Vec<&str> {
        history
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .flat_map(|assistant| assistant.content.iter())
            .filter_map(|content| match content {
                Content::Text { text } => Some(&**text),
                _ => None,
            })
            .collect()
    }

    /// The reasoning blocks of the assistant replies in `history`.
    fn thinkings_in(history: &[Message]) -> Vec<&str> {
        history
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .flat_map(|assistant| assistant.content.iter())
            .filter_map(|content| match content {
                Content::Thinking { thinking, .. } => Some(&**thinking),
                _ => None,
            })
            .collect()
    }

    fn results_in(history: &[Message]) -> Vec<&str> {
        history
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(&*result.content),
                _ => None,
            })
            .collect()
    }

    fn reasoning_reply(thinking: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ThinkingDelta(Text::from(thinking)),
            StreamEvent::Stop(StopReason::EndTurn),
        ]
    }

    #[tokio::test]
    async fn broken_arguments_are_answered_and_sent_back_as_an_empty_object() {
        let provider = FakeProvider::new(vec![
            tool_reply("c1", "echo", r#"{"message":"x"#),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let calls = calls_in(&history);
        assert_eq!(calls.len(), 1, "{history:?}");
        assert_eq!(&*calls[0].arguments, "{}");
        let results = results_in(&history);
        assert!(results[0].contains("cut off"), "{results:?}");
        assert_eq!(provider.seen().len(), 2);
    }

    #[tokio::test]
    async fn repair_note_string_arguments_say_they_were_not_plain_json() {
        let provider = FakeProvider::new(vec![
            tool_reply("c1", "echo", r#""{\"message\":\"hi\"}""#),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let results = results_in(&history);
        assert_eq!(
            results[0],
            "[repaired: arguments were not plain JSON; fixed]\nhi"
        );
    }

    #[tokio::test]
    async fn repair_note_a_leaked_call_says_where_it_came_from() {
        let provider = FakeProvider::new(vec![
            text_reply(
                r#"Let me echo.<tool_call>{"name":"echo","arguments":{"message":"hi"}}</tool_call>"#,
            ),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let results = results_in(&history);
        assert_eq!(
            results[0],
            "[repaired: call read from the reply text; send calls as tool calls]\nhi"
        );
    }

    #[tokio::test]
    async fn repair_note_a_plain_call_has_none() {
        let provider = FakeProvider::new(vec![
            tool_reply("c1", "echo", r#"{"message":"hi"}"#),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(results_in(&history), vec!["hi"]);
    }

    #[tokio::test]
    async fn repair_note_a_note_joins_the_first_line_a_result_has() {
        let message = Message::tool_result("c1", "[repaired: used a.rs]\nedited a.rs", false);

        let noted = with_note(message, "arguments were not plain JSON; fixed");

        assert_eq!(
            results_in(std::slice::from_ref(&noted)),
            vec!["[repaired: arguments were not plain JSON; fixed; used a.rs]\nedited a.rs"]
        );
    }

    #[tokio::test]
    async fn leaked_call_runs_a_call_from_the_text() {
        let provider = FakeProvider::new(vec![
            text_reply(
                r#"Let me read it.<tool_call>{"name":"echo","arguments":{"message":"hi"}}</tool_call>"#,
            ),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let calls = calls_in(&history);
        assert_eq!(calls.len(), 1, "{history:?}");
        assert_eq!(&*calls[0].name, "echo");
        assert_eq!(
            results_in(&history),
            vec!["[repaired: call read from the reply text; send calls as tool calls]\nhi"]
        );
        assert_eq!(texts_in(&history), vec!["Let me read it.", "done"]);
        assert_eq!(provider.seen().len(), 2, "the run continues");
    }

    #[tokio::test]
    async fn leaked_call_runs_a_harmony_call_from_the_text() {
        let provider = FakeProvider::new(vec![
            text_reply(
                "<|channel|>commentary to=functions.echo<|constrain|>json<|message|>\
                    {\"message\":\"hi\"}<|call|>",
            ),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let calls = calls_in(&history);
        assert_eq!(calls.len(), 1, "{history:?}");
        assert_eq!(&*calls[0].name, "echo");
        assert_eq!(
            results_in(&history),
            vec!["[repaired: call read from the reply text; send calls as tool calls]\nhi"]
        );
        assert_eq!(
            texts_in(&history),
            vec!["done"],
            "the call was all the text"
        );
    }

    #[tokio::test]
    async fn leaked_call_skips_a_call_in_the_text_that_names_no_tool() {
        let provider = FakeProvider::new(vec![text_reply(
            r#"Hi.<tool_call>{"name":"nosuch","arguments":{}}</tool_call>"#,
        )]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert!(calls_in(&history).is_empty(), "{history:?}");
        assert!(results_in(&history).is_empty());
        assert_eq!(provider.seen().len(), 1);
    }

    #[tokio::test]
    async fn leaked_call_runs_a_glm_call_from_the_reasoning() {
        let provider = FakeProvider::new(vec![
            reasoning_reply(
                "Let me echo.<tool_call>echo<arg_key>message</arg_key><arg_value>hi</arg_value></tool_call>",
            ),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let calls = calls_in(&history);
        assert_eq!(calls.len(), 1, "{history:?}");
        assert_eq!(&*calls[0].name, "echo");
        assert_eq!(
            results_in(&history),
            vec!["[repaired: call read from the reply text; send calls as tool calls]\nhi"]
        );
        let events = drain(tx, &mut rx);
        assert!(
            events.iter().any(
                |event| matches!(event, Event::ToolCallReady { name, .. } if &**name == "echo")
            ),
            "{events:?}"
        );
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
    }

    #[tokio::test]
    async fn leaked_call_in_the_reasoning_of_an_answer_does_not_run() {
        let provider = FakeProvider::new(vec![vec![
            StreamEvent::ThinkingDelta(Text::from(
                "<tool_call>echo<arg_key>message</arg_key><arg_value>hi</arg_value></tool_call>",
            )),
            StreamEvent::TextDelta(Text::from("All done.")),
            StreamEvent::Stop(StopReason::EndTurn),
        ]]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert!(calls_in(&history).is_empty(), "{history:?}");
        assert_eq!(provider.seen().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn leaked_call_skips_an_unknown_name() {
        let provider = FakeProvider::new(vec![
            reasoning_reply(
                "<tool_call>nosuch<arg_key>a</arg_key><arg_value>b</arg_value></tool_call>",
            ),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert!(calls_in(&history).is_empty(), "{history:?}");
        assert!(results_in(&history).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn reasoning_only_reply_is_requested_again() {
        let provider = FakeProvider::new(vec![reasoning_reply("thinking"), text_reply("done")]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(provider.seen().len(), 2, "the reply is requested again");
        assert_eq!(history.len(), 2, "{history:?}");
        let events = drain(tx, &mut rx);
        assert!(
            events.iter().any(
                |event| matches!(event, Event::Retry { error, .. } if &**error == "empty reply")
            ),
            "{events:?}"
        );
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
    }

    #[tokio::test]
    async fn think_tag_a_leaked_block_becomes_the_reasoning() {
        let provider = FakeProvider::new(vec![text_reply("<think>plan</think>done")]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(thinkings_in(&history), vec!["plan"]);
        assert_eq!(texts_in(&history), vec!["done"]);
        assert_eq!(provider.seen().len(), 1);
    }

    #[tokio::test]
    async fn think_tag_a_closed_tag_without_the_open_tag_becomes_the_reasoning() {
        let provider = FakeProvider::new(vec![text_reply("plan</think>\n\ndone")]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(thinkings_in(&history), vec!["plan"]);
        assert_eq!(texts_in(&history), vec!["done"]);
        assert_eq!(provider.seen().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn think_tag_an_unclosed_block_is_all_reasoning() {
        // A reply with only reasoning is requested again, so the retries are
        // filled to reach the reply that is recorded.
        let provider = FakeProvider::new(vec![
            text_reply("<think>only thinking");
            EMPTY_RETRIES as usize + 1
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(thinkings_in(&history), vec!["only thinking"]);
        assert!(texts_in(&history).is_empty(), "{history:?}");
    }

    #[tokio::test]
    async fn think_tag_text_with_no_think_tag_is_unchanged() {
        let provider = FakeProvider::new(vec![text_reply("a < b and </b> tags")]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert!(thinkings_in(&history).is_empty(), "{history:?}");
        assert_eq!(texts_in(&history), vec!["a < b and </b> tags"]);
    }

    #[tokio::test]
    async fn think_tag_a_call_after_the_block_still_runs() {
        let provider = FakeProvider::new(vec![
            text_reply(
                r#"<think>I will call it</think><tool_call>{"name":"echo","arguments":{"message":"hi"}}</tool_call>"#,
            ),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(thinkings_in(&history), vec!["I will call it"]);
        let calls = calls_in(&history);
        assert_eq!(calls.len(), 1, "{history:?}");
        assert_eq!(&*calls[0].name, "echo");
        assert_eq!(
            results_in(&history),
            vec!["[repaired: call read from the reply text; send calls as tool calls]\nhi"]
        );
        assert_eq!(provider.seen().len(), 2, "the run continues");
    }

    #[tokio::test]
    async fn think_tag_a_mention_of_the_closing_tag_stays_text() {
        let provider = FakeProvider::new(vec![text_reply(
            "I fixed the parser so `</think>` closes the block.",
        )]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert!(thinkings_in(&history).is_empty(), "{history:?}");
        assert_eq!(
            texts_in(&history),
            vec!["I fixed the parser so `</think>` closes the block."]
        );
    }

    #[tokio::test]
    async fn think_tag_a_closing_tag_beside_reasoning_stays_text() {
        let provider = FakeProvider::new(vec![vec![
            StreamEvent::ThinkingDelta(Text::from("why")),
            StreamEvent::TextDelta(Text::from("x</think>\ny")),
            StreamEvent::Stop(StopReason::EndTurn),
        ]]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("task")];

        Agent::new(&provider, &registry, config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert_eq!(thinkings_in(&history), vec!["why"]);
        assert_eq!(texts_in(&history), vec!["x</think>\ny"]);
    }

    /// A guard over no rules: every call runs, and the mode `bypass` is off.
    fn guard() -> Guard {
        Guard::new(reagent_config::permissions::Rules::none())
    }

    #[tokio::test]
    async fn a_settings_event_names_what_the_session_applied() {
        let provider = FakeProvider::new(Vec::new());
        let registry = Registry::new();
        let (tx, mut rx) = mpsc::channel(8);
        let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new())
            .with_guard(guard());

        agent
            .set(
                Settings {
                    model: Some(Text::from("m2")),
                    level: Some(Text::from("high")),
                    mode: Some(PermissionMode::Auto),
                    ..Settings::default()
                },
                &tx,
            )
            .await;

        // One event, with every value the session took, in one place.
        let events = drain(tx, &mut rx);
        assert_eq!(
            events,
            [Event::Settings {
                model: Some(Text::from("m2")),
                level: Some(Text::from("high")),
                mode: Some(PermissionMode::Auto),
            }]
        );
        assert_eq!(&*agent.model, "m2");
        assert_eq!(agent.config.reasoning.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn a_settings_event_leaves_out_a_mode_the_guard_refused() {
        let provider = FakeProvider::new(Vec::new());
        let registry = Registry::new();
        let (tx, mut rx) = mpsc::channel(8);
        let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new())
            .with_guard(guard());

        // `bypass` is off for this run, so the guard keeps the old mode: the
        // error says so, and the model still applies.
        agent
            .set(
                Settings {
                    model: Some(Text::from("m2")),
                    mode: Some(PermissionMode::Bypass),
                    ..Settings::default()
                },
                &tx,
            )
            .await;

        let events = drain(tx, &mut rx);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Error(text) if text.contains("bypass"))),
            "the refusal is an error event: {events:?}"
        );
        // What the session applied is the model alone: the refused mode is not
        // in the event, so no client is told it is.
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Settings { .. }))
                .collect::<Vec<_>>(),
            [&Event::Settings {
                model: Some(Text::from("m2")),
                level: None,
                mode: None,
            }]
        );

        // A set of nothing but a refused mode applies nothing: no event at all.
        let (tx, mut rx) = mpsc::channel(8);
        agent
            .set(
                Settings {
                    mode: Some(PermissionMode::Bypass),
                    ..Settings::default()
                },
                &tx,
            )
            .await;
        let events = drain(tx, &mut rx);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Settings { .. })),
            "nothing applied, so nothing is named: {events:?}"
        );
    }

    /// A provider that plays a [`FakeProvider`]'s replies under its own name.
    struct Named {
        name: &'static str,
        inner: FakeProvider,
    }

    impl Named {
        fn new(name: &'static str, replies: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                name,
                inner: FakeProvider::new(replies),
            }
        }
    }

    impl Provider for Named {
        fn name(&self) -> &'static str {
            self.name
        }

        fn stream(
            &self,
            request: &Request<'_>,
        ) -> futures_util::stream::BoxStream<'static, Result<StreamEvent, Error>> {
            self.inner.stream(request)
        }
    }

    /// A [`Named`] provider that sends `plan` into the session while its first
    /// request streams.
    struct Planning {
        named: Named,
        plan: std::sync::Mutex<Vec<Command>>,
        sender: crate::live::Sender,
    }

    impl Provider for Planning {
        fn name(&self) -> &'static str {
            self.named.name()
        }

        fn stream(
            &self,
            request: &Request<'_>,
        ) -> futures_util::stream::BoxStream<'static, Result<StreamEvent, Error>> {
            for command in std::mem::take(&mut *self.plan.lock().expect("plan lock")) {
                let _ = self.sender.try_send(command);
            }
            self.named.stream(request)
        }
    }

    /// The name of the provider the last message of `history` names.
    fn last_provider(history: &[Message]) -> String {
        match history.last() {
            Some(Message::Assistant(reply)) => reply.provider.to_string(),
            other => panic!("the run ends with a reply: {other:?}"),
        }
    }

    #[tokio::test]
    async fn provider_switch_sends_the_next_request_to_the_new_provider() {
        let first = Named::new("first", vec![text_reply("one"), text_reply("two")]);
        let second = Arc::new(Named::new("second", vec![text_reply("three")]));
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let (_sender, mut inbox) = crate::live::channel();
        let mut history = vec![Message::user("hello")];
        let mut agent = Agent::new(&first, &registry, config(), CancellationToken::new());

        agent.run(&mut history, None, &tx).await;
        let mut journal = None;
        agent
            .idle(
                Command::Set {
                    client: 1,
                    settings: Settings {
                        provider: Some(crate::live::Swap(Arc::clone(&second) as Arc<dyn Provider>)),
                        model: Some(Text::from("model-b")),
                        ..Settings::default()
                    },
                },
                &mut history,
                &mut journal,
                &mut inbox,
                &tx,
            )
            .await;
        agent.run(&mut history, None, &tx).await;

        let seen = second.inner.seen();
        assert_eq!(seen.len(), 1, "the second provider sends the next request");
        assert_eq!(seen[0].model, "model-b", "the set carried the model too");
        assert_eq!(
            first.inner.seen().len(),
            1,
            "the first provider sends one request"
        );
        assert_eq!(last_provider(&history), "second", "the reply names it");
    }

    #[tokio::test]
    async fn provider_switch_held_during_a_stream_waits_for_the_next_request() {
        let (sender, mut inbox) = crate::live::channel();
        let second = Arc::new(Named::new("second", vec![text_reply("three")]));
        let first = Planning {
            named: Named::new("first", vec![text_reply("one"), text_reply("two")]),
            plan: std::sync::Mutex::new(vec![
                Command::Set {
                    client: 1,
                    settings: Settings {
                        provider: Some(crate::live::Swap(Arc::clone(&second) as Arc<dyn Provider>)),
                        model: Some(Text::from("model-b")),
                        ..Settings::default()
                    },
                },
                Command::Send {
                    client: 1,
                    text: Text::from("say more"),
                    mode: SendMode::Steer,
                },
            ]),
            sender,
        };
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];
        let mut agent = Agent::new(&first, &registry, config(), CancellationToken::new());

        agent
            .run_with_inbox(&mut history, None, &tx, &mut inbox)
            .await;

        let seen = second.inner.seen();
        assert_eq!(seen.len(), 1, "the held switch waits for the next request");
        assert_eq!(seen[0].model, "model-b", "the set carried the model too");
        assert_eq!(seen[0].messages, 3, "the reply, then the steer message");
        assert_eq!(
            first.named.inner.seen().len(),
            1,
            "the request in flight keeps its provider"
        );
        assert_eq!(last_provider(&history), "second", "the reply names it");
    }

    #[tokio::test]
    async fn provider_switch_with_no_provider_keeps_the_old_one() {
        let first = Named::new("first", vec![text_reply("one"), text_reply("two")]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let (_sender, mut inbox) = crate::live::channel();
        let mut history = vec![Message::user("hello")];
        let mut agent = Agent::new(&first, &registry, config(), CancellationToken::new());

        agent.run(&mut history, None, &tx).await;
        let mut journal = None;
        agent
            .idle(
                Command::Set {
                    client: 1,
                    settings: Settings {
                        model: Some(Text::from("model-b")),
                        ..Settings::default()
                    },
                },
                &mut history,
                &mut journal,
                &mut inbox,
                &tx,
            )
            .await;
        agent.run(&mut history, None, &tx).await;

        let seen = first.inner.seen();
        assert_eq!(seen.len(), 2, "the first provider sends both requests");
        assert_eq!(seen[1].model, "model-b", "the model changed");
        assert_eq!(
            last_provider(&history),
            "first",
            "the reply still names the old provider"
        );
    }

    /// A provider that keeps the caps and the image flag of every request.
    struct Limits {
        inner: FakeProvider,
        seen: Arc<Mutex<Vec<(u32, bool)>>>,
    }

    impl Provider for Limits {
        fn name(&self) -> &'static str {
            self.inner.name()
        }

        fn stream(
            &self,
            request: &Request<'_>,
        ) -> futures_util::stream::BoxStream<'static, Result<StreamEvent, Error>> {
            self.seen
                .lock()
                .expect("requests lock")
                .push((request.max_tokens, request.images));
            self.inner.stream(request)
        }
    }

    /// A history of `turns` tool turns, each result 4 KB, whose newest reply
    /// reports `prompt` input tokens, so compaction has room to cut.
    fn long_history(turns: usize, prompt: u64) -> Vec<Message> {
        let mut history = vec![Message::user("fix the bug")];
        for n in 1..=turns {
            let id = format!("call_{n}");
            let mut reply = AssistantMessage::new(
                "fake",
                "fake",
                vec![Content::ToolCall(ToolCall {
                    id: Text::from(id.as_str()),
                    name: Text::from("echo"),
                    arguments: Text::from(r#"{"message":"a"}"#),
                })],
            );
            reply.usage = Usage {
                input_tokens: prompt,
                ..Usage::default()
            };
            history.push(Message::assistant(reply));
            history.push(Message::tool_result(id, "x".repeat(4_000), false));
        }
        history
    }

    /// The prompt sizes, in tokens, of the compactions the events report.
    fn compacted(events: &[Event]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Compacted { before, .. } => Some(*before),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn switch_limits_take_the_new_compaction_budget() {
        let provider = FakeProvider::new(vec![text_reply("done")]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = long_history(60, 1_000);
        let archive = Archive::new();
        let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new())
            .with_archive(Arc::clone(&archive))
            .with_compact(Compact::with_budget(10_000_000, "."));

        agent
            .set(
                Settings {
                    compact_budget: Some(1_000),
                    ..Settings::default()
                },
                &tx,
            )
            .await;
        agent.run(&mut history, None, &tx).await;

        assert_eq!(compacted(&drain(tx, &mut rx)), [1_000]);
        assert!(
            history.iter().any(|message| match message {
                Message::User { content } => content.iter().any(|content| matches!(
                    content,
                    Content::Text { text } if text.contains("## Checkpoint 1")
                )),
                _ => false,
            }),
            "the old turns are one checkpoint"
        );
    }

    #[tokio::test]
    async fn switch_limits_without_compaction_change_nothing() {
        let provider = FakeProvider::new(vec![text_reply("done")]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = long_history(60, 1_000);
        let archive = Archive::new();
        let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new())
            .with_archive(Arc::clone(&archive))
            .with_compact(Compact::with_budget(10_000_000, "."));

        // A set that names no budget keeps the one the session runs.
        agent
            .set(
                Settings {
                    max_tokens: Some(4_096),
                    images: Some(false),
                    ..Settings::default()
                },
                &tx,
            )
            .await;
        agent.run(&mut history, None, &tx).await;
        assert!(
            compacted(&drain(tx, &mut rx)).is_empty(),
            "nothing compacts"
        );

        // A budget with no compact policy cuts nothing either.
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = long_history(60, 1_000);
        let provider = FakeProvider::new(vec![text_reply("done")]);
        let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new())
            .with_archive(Arc::clone(&archive));
        agent
            .set(
                Settings {
                    compact_budget: Some(1_000),
                    ..Settings::default()
                },
                &tx,
            )
            .await;
        agent.run(&mut history, None, &tx).await;
        assert!(
            compacted(&drain(tx, &mut rx)).is_empty(),
            "no policy, no cut"
        );
        assert_eq!(history.len(), 2 + 60 * 2, "the history keeps its turns");
    }

    #[tokio::test]
    async fn switch_limits_turn_images_off() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = Limits {
            inner: FakeProvider::new(vec![text_reply("done")]),
            seen: Arc::clone(&seen),
        };
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![
            Message::user("go"),
            Message::tool_result_with_images(
                "call_0",
                "one image",
                false,
                vec![Image {
                    media_type: ImageType::Png,
                    width: 1,
                    height: 1,
                    data: Text::from("aGk="),
                }],
            ),
        ];
        let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new());

        agent
            .set(
                Settings {
                    images: Some(false),
                    ..Settings::default()
                },
                &tx,
            )
            .await;
        agent.run(&mut history, None, &tx).await;

        let seen = seen.lock().expect("requests lock").clone();
        assert_eq!(seen, [(128, false)], "the next request sends no image");
        // The image stays in the history: only the request leaves it out.
        assert!(
            matches!(&history[1], Message::ToolResult(result) if !result.images.is_empty()),
            "the result keeps its image"
        );
    }

    #[tokio::test]
    async fn switch_limits_take_max_tokens() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = Limits {
            inner: FakeProvider::new(vec![text_reply("done")]),
            seen: Arc::clone(&seen),
        };
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("go")];
        let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new());

        agent
            .set(
                Settings {
                    max_tokens: Some(4_096),
                    ..Settings::default()
                },
                &tx,
            )
            .await;
        agent.run(&mut history, None, &tx).await;

        let seen = seen.lock().expect("requests lock").clone();
        assert_eq!(seen, [(4_096, true)]);
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;
    use std::sync::Mutex;

    use futures_util::future::BoxFuture;
    use futures_util::stream::BoxStream;
    use reagent_ai::fake::FakeProvider;
    use reagent_ai::{Image, ImageType, Role, ToolSpec};
    use serde_json::json;

    use crate::tool::{Tool, ToolOutput};

    /// A read-only tool that returns one image with its text.
    struct Picture {
        spec: ToolSpec,
    }

    impl Tool for Picture {
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
                ToolOutput::ok("one image").with_images(vec![Image {
                    media_type: ImageType::Png,
                    width: 1,
                    height: 1,
                    data: "aGk=".into(),
                }])
            })
        }
    }

    /// A provider that keeps the messages of every request it is asked for.
    struct Recorder {
        inner: FakeProvider,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
    }

    impl Provider for Recorder {
        fn name(&self) -> &'static str {
            self.inner.name()
        }

        fn stream(&self, request: &Request<'_>) -> BoxStream<'static, Result<StreamEvent, Error>> {
            self.requests
                .lock()
                .expect("requests lock")
                .push(request.messages.to_vec());
            self.inner.stream(request)
        }
    }

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry.add(Picture {
            spec: ToolSpec {
                name: "picture".into(),
                description: "One image.".into(),
                input_schema: json!({"type": "object"}),
            },
        });
        registry
    }

    fn config() -> AgentConfig {
        AgentConfig {
            model: "fake-model".into(),
            max_tokens: 128,
            system: None,
            reasoning: None,
            cache_key: None,
            max_turns: None,
            images: true,
        }
    }

    fn picture_reply() -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallStart {
                id: Text::from("call_1"),
                name: Text::from("picture"),
            },
            StreamEvent::ToolCallDelta {
                id: Text::from("call_1"),
                partial_json: Text::from("{}"),
            },
            StreamEvent::ToolCallEnd {
                id: Text::from("call_1"),
            },
            StreamEvent::Stop(StopReason::ToolUse),
        ]
    }

    #[tokio::test]
    async fn a_tool_image_reaches_the_next_request() {
        let provider = FakeProvider::new(vec![picture_reply(), {
            vec![
                StreamEvent::TextDelta(Text::from("done")),
                StreamEvent::Stop(StopReason::EndTurn),
            ]
        }]);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Recorder {
            inner: provider,
            requests: Arc::clone(&requests),
        };
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("go")];
        Agent::new(&provider, &registry(), config(), CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        let requests = requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 2, "two turns, two requests");
        let result = requests[1]
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) => Some(result),
                _ => None,
            })
            .expect("the picture result is in the second request");
        assert_eq!(result.content.as_ref(), "one image");
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].data.as_ref(), "aGk=");
        assert_eq!(result.images[0].media_type, ImageType::Png);
        assert!(
            requests[1]
                .iter()
                .any(|message| message.role() == Role::ToolResult),
            "the result keeps its role"
        );
    }

    #[tokio::test]
    async fn a_model_that_takes_no_images_asks_for_none() {
        let provider = FakeProvider::new(vec![vec![
            StreamEvent::TextDelta(Text::from("done")),
            StreamEvent::Stop(StopReason::EndTurn),
        ]]);
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("go")];
        let config = AgentConfig {
            images: false,
            ..config()
        };
        Agent::new(&provider, &registry(), config, CancellationToken::new())
            .run(&mut history, None, &tx)
            .await;

        assert!(!provider.seen()[0].images, "the request sends no image");
    }
}
