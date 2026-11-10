//! A live session: many clients steer, queue, cancel and set one running agent.
//!
//! A client sends [`Command`]s on a bounded channel. The session drains them
//! only at the boundaries of a run (after a tool batch, before a request) with
//! a non-blocking read, so a run makes no extra wakeup and adds no timer; it
//! waits on the channel only while no run is open.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use tokio::sync::mpsc;

use reagent_ai::{Content, Message, Provider, Text};
use reagent_config::permissions::PermissionMode;

use crate::agent_loop::Agent;
use crate::event::Event;
use crate::guard::Answer;
use crate::journal::Journal;
use crate::question::Answers;

/// The id a caller gives itself, so the events and the journal name it.
pub type ClientId = u32;

/// How a sent message joins the run that is in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendMode {
    /// Added after the current tool calls and before the next request.
    Steer,
    /// Held until the run ends; it starts the next run.
    Queue,
}

/// A provider a client switches the session to, shared so a request that is
/// already in flight keeps the one it was sent with.
#[derive(Clone)]
pub struct Swap(pub Arc<dyn Provider>);

impl fmt::Debug for Swap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Swap({})", self.0.name())
    }
}

impl PartialEq for Swap {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Swap {}

/// What a client asks the session to change for the next request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// The provider the next request goes to, when the client switches it.
    pub provider: Option<Swap>,
    /// The config provider id of that provider, when the client switches it.
    pub config_provider: Option<Text>,
    /// The provider model id, when the client changes it.
    pub model: Option<Text>,
    /// The reasoning level text, when the client changes it.
    pub level: Option<Text>,
    /// The permission mode to switch to; the mode stays when it is `None`.
    pub mode: Option<PermissionMode>,
    /// Whether the model takes images, when the client changes it.
    pub images: Option<bool>,
    /// The cap on reply tokens, when the client changes it.
    pub max_tokens: Option<u32>,
    /// The prompt size, in tokens, that triggers a compaction, when the
    /// client changes it.
    pub compact_budget: Option<u64>,
}

/// One request of one client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// A message: it steers the run or waits for the next one.
    Send {
        /// The id the worker gave it, which its
        /// [`Event::MessageState`](crate::event::Event::MessageState) carries
        /// too.
        message: u64,
        /// The client that sent it.
        client: ClientId,
        /// The text of the message.
        text: Text,
        /// What follows the text in the user message: the images an `@path`
        /// word attached, and nothing for most messages.
        blocks: Vec<Content>,
        /// How it joins the session.
        mode: SendMode,
    },
    /// Stop the current turn; the session stays open for the next message.
    Cancel {
        /// The client that asked.
        client: ClientId,
    },
    /// Change the model, the level, the mode or the limits for the next
    /// request.
    Set {
        /// The client that asked.
        client: ClientId,
        /// The values to change; each is left alone when it is `None`.
        settings: Settings,
    },
    /// Replace the older turns with a checkpoint now, ending it with `note`
    /// when one is given.
    Compact {
        /// The client that asked.
        client: ClientId,
        /// The text the checkpoint ends with, when the client gives one.
        note: Option<Text>,
    },
    /// Answer a call that waits for approval: run it once, allow its rule for
    /// this session, save the rule, refuse it or skip it.
    Approve {
        /// The client that answered.
        client: ClientId,
        /// The id of the request, as [`Event::ApprovalRequest`] reported it.
        id: u64,
        /// The answer.
        answer: Answer,
    },
    /// Answer the questions the model asked with the `ask` tool: one list of
    /// picks or own text per question, or `None` to skip the whole request.
    Answer {
        /// The client that answered.
        client: ClientId,
        /// The id of the request, as [`Event::QuestionRequest`] reported it.
        id: u64,
        /// The answer of each question, in the order it was asked.
        answers: Answers,
    },
}

/// One message a client sent, waiting for the boundary or the run that carries
/// it into the conversation.
#[derive(Debug)]
pub(crate) struct Waiting {
    /// The id the worker gave the message.
    pub(crate) message: u64,
    /// The client that sent it.
    pub(crate) client: ClientId,
    /// The text of the message.
    pub(crate) text: Text,
    /// What follows the text: the images an `@path` word attached, and nothing
    /// for most messages.
    pub(crate) blocks: Vec<Content>,
}

impl Waiting {
    /// The message of `client` under the id `message`: its text and the
    /// blocks that follow it.
    pub(crate) fn new(message: u64, client: ClientId, text: Text, blocks: Vec<Content>) -> Self {
        Self {
            message,
            client,
            text,
            blocks,
        }
    }

    /// The user message it becomes: the text alone, or the text and then the
    /// blocks.
    pub(crate) fn message(self) -> Message {
        if self.blocks.is_empty() {
            return Message::user(self.text);
        }
        let mut content = Vec::with_capacity(self.blocks.len() + 1);
        content.push(Content::text(self.text));
        content.extend(self.blocks);
        Message::user_content(content)
    }

    /// The message [`Waiting::message`] built, as the waiting message it was,
    /// so a message the journal refused goes back to the inbox with its blocks
    /// and no image is copied.
    pub(crate) fn restored(id: u64, client: ClientId, message: Message) -> Self {
        let Message::User { content } = message else {
            unreachable!("a waiting message is a user message")
        };
        let mut content = content.into_iter();
        let text = match content.next() {
            Some(Content::Text { text }) => text,
            _ => Text::default(),
        };
        Self {
            message: id,
            client,
            text,
            blocks: content.collect(),
        }
    }
}

/// How many commands may be in flight before a client waits.
pub const COMMANDS: usize = 64;

/// The side a client sends its commands on.
pub type Sender = mpsc::Sender<Command>;

/// The commands of one session, its steer list and its queue.
#[derive(Debug)]
pub struct Inbox {
    commands: mpsc::Receiver<Command>,
    /// Steer messages taken from the channel that no request has carried yet.
    pending: Vec<Waiting>,
    /// Messages that start a run once the current one ends, oldest first.
    queue: VecDeque<Waiting>,
    /// Settings taken during a stream, applied before the next request.
    held: Vec<Settings>,
    /// Compactions taken during a stream, run before the next request.
    compactions: Vec<Option<Text>>,
}

/// Creates the command channel of one session.
pub fn channel() -> (Sender, Inbox) {
    let (sender, commands) = mpsc::channel(COMMANDS);
    let inbox = Inbox {
        commands,
        pending: Vec::new(),
        queue: VecDeque::new(),
        held: Vec::new(),
        compactions: Vec::new(),
    };
    (sender, inbox)
}

impl Inbox {
    /// Waits for the next command, or `None` when every sender is dropped.
    ///
    /// Only an idle session waits here: a run never does.
    pub(crate) async fn wait(&mut self) -> Option<Command> {
        self.commands.recv().await
    }

    /// Waits for the next command inside a run.
    ///
    /// Once every sender is dropped it waits forever, so a stream that is
    /// still running is never polled in a spin.
    pub(crate) async fn next(&mut self) -> Command {
        match self.commands.recv().await {
            Some(command) => command,
            None => std::future::pending().await,
        }
    }

    /// The next command that is ready now, without waiting for one.
    pub(crate) fn ready(&mut self) -> Option<Command> {
        self.commands.try_recv().ok()
    }

    /// Adds a steer message, which lands before the next request.
    pub(crate) fn steer(&mut self, waiting: Waiting) {
        self.pending.push(waiting);
    }

    /// Adds a message for the next run.
    pub(crate) fn queue(&mut self, waiting: Waiting) {
        self.queue.push_back(waiting);
    }

    /// Holds settings a stream took, for the next boundary.
    pub(crate) fn hold(&mut self, settings: Settings) {
        self.held.push(settings);
    }

    /// Holds a compaction a stream took, for the next request.
    pub(crate) fn hold_compaction(&mut self, note: Option<Text>) {
        self.compactions.push(note);
    }

    /// The compactions waiting for a request, oldest first.
    pub(crate) fn take_compactions(&mut self) -> std::vec::Drain<'_, Option<Text>> {
        self.compactions.drain(..)
    }

    /// The steer messages waiting for a boundary, oldest first.
    pub(crate) fn take_pending(&mut self) -> Vec<Waiting> {
        std::mem::take(&mut self.pending)
    }

    /// Puts back steer messages that could not join the conversation, in order,
    /// ahead of any that arrived since.
    pub(crate) fn restore(&mut self, first: Waiting, rest: impl Iterator<Item = Waiting>) {
        let newer = std::mem::take(&mut self.pending);
        self.pending.push(first);
        self.pending.extend(rest);
        self.pending.extend(newer);
    }

    /// Whether a steer text waits for the next boundary.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Takes the steer texts and the queued messages a cancel drops, oldest
    /// first: a cancel stops the work a client asked for, and a client that
    /// wants it again sends it again.
    pub(crate) fn clear_waiting(&mut self) -> impl Iterator<Item = Waiting> + '_ {
        self.pending.drain(..).chain(self.queue.drain(..))
    }

    /// The settings waiting for a boundary, oldest first.
    pub(crate) fn take_held(&mut self) -> std::vec::Drain<'_, Settings> {
        self.held.drain(..)
    }

    /// The oldest message waiting for a run.
    pub(crate) fn pop(&mut self) -> Option<Waiting> {
        self.queue.pop_front()
    }

    /// Whether a message starts the next run right away.
    fn started(&self) -> bool {
        !self.pending.is_empty() || !self.queue.is_empty()
    }
}

/// Runs the session until the inbox's sender side is dropped and nothing is
/// queued: one run per user message, and a message that arrives while the
/// session waits starts the next run.
///
/// A run ends on its own or on a [`Command::Cancel`]; the session stays open
/// after either, and a message that a client sent while the run was open
/// starts the next one.
pub async fn run_live(
    agent: &mut Agent<'_>,
    history: &mut Vec<Message>,
    mut journal: Option<&mut Journal>,
    events: &mpsc::Sender<Event>,
    inbox: &mut Inbox,
) {
    loop {
        while !inbox.started() {
            // Nothing to run: wait on the channel, with no request in flight
            // and no timer, until a client sends something.
            let Some(command) = inbox.wait().await else {
                return;
            };
            agent
                .idle(command, history, &mut journal, inbox, events)
                .await;
        }
        // A run starts with the steer texts taken while the session waited, or
        // else the oldest queued message. They go to history only: the run
        // writes them to the journal with the rest of what it has not written.
        let mut start = inbox.take_pending();
        if start.is_empty()
            && let Some(first) = inbox.pop()
        {
            start.push(first);
        }
        // The run carries these messages, so each client hears that they
        // landed before the events of the run itself.
        for waiting in &start {
            agent.landed(waiting.message, events).await;
        }
        history.extend(start.into_iter().map(Waiting::message));
        agent.apply_held(inbox, events).await;
        agent
            .run_with_inbox(history, journal.as_deref_mut(), events, inbox)
            .await;
        // The run wrote every message it holds, so the next run writes only
        // what comes after them.
        if let Some(journal) = journal.as_deref_mut() {
            crate::journal::resume::mark_recorded(journal, history.len());
        }
        // A cancel that arrived after the run ended has nothing to stop; it
        // must not stop the next run before its first request.
        while let Some(command) = inbox.ready() {
            agent
                .idle(command, history, &mut journal, inbox, events)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use futures_util::stream::{self, BoxStream};
    use reagent_ai::fake::{FakeProvider, SeenRequest};
    use reagent_ai::{Content, Error, Provider, Request, Role, StopReason, StreamEvent, ToolSpec};
    use tokio::sync::Notify;
    use tokio::sync::mpsc::Receiver;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent_loop::AgentConfig;
    use crate::compact::Compact;
    use crate::event::{DropReason, MessageState, TurnEndReason};
    use crate::question::{Ask, Questions};
    use crate::tool::{Registry, Tool, ToolOutput};

    /// One message of a request, as a test reads it back.
    type SeenMessage = (Role, String);

    /// A provider that plays a [`FakeProvider`]'s replies, records the messages
    /// of every request and sends scripted commands into the session when a
    /// request opens.
    struct Script {
        fake: FakeProvider,
        /// The commands to send when request `n` opens, oldest first.
        plan: Mutex<VecDeque<Vec<Command>>>,
        /// The messages of every request, oldest first.
        requests: Mutex<Vec<Vec<SeenMessage>>>,
        /// The session's commands; dropped by [`Script::close`] to end it.
        sender: Mutex<Option<Sender>>,
        /// Set once when a request opens, so a test can wait for it.
        opened: Notify,
    }

    impl Script {
        /// Plays `replies` and sends `plan[n]` when request `n` opens.
        fn new(replies: Vec<Vec<StreamEvent>>, plan: Vec<Vec<Command>>, sender: Sender) -> Self {
            Self {
                fake: FakeProvider::new(replies),
                plan: Mutex::new(plan.into()),
                requests: Mutex::new(Vec::new()),
                sender: Mutex::new(Some(sender)),
                opened: Notify::new(),
            }
        }

        /// Every request the provider saw, as its messages in order.
        fn requests(&self) -> Vec<Vec<SeenMessage>> {
            self.requests.lock().expect("requests lock").clone()
        }

        /// The requests the fake provider recorded, with their model and level.
        fn seen(&self) -> Vec<SeenRequest> {
            self.fake.seen()
        }

        /// Drops the session's sender, so an idle session returns.
        fn close(&self) {
            self.sender.lock().expect("sender lock").take();
        }
    }

    impl Provider for Script {
        fn name(&self) -> &'static str {
            "script"
        }

        fn stream(&self, request: &Request<'_>) -> BoxStream<'static, Result<StreamEvent, Error>> {
            self.requests.lock().expect("requests lock").push(
                request
                    .messages
                    .iter()
                    .map(|message| (message.role(), text_of(message)))
                    .collect(),
            );
            for command in self
                .plan
                .lock()
                .expect("plan lock")
                .pop_front()
                .unwrap_or_default()
            {
                let sender = self.sender.lock().expect("sender lock");
                if let Some(sender) = sender.as_ref() {
                    let _ = sender.try_send(command);
                }
            }
            self.opened.notify_one();
            self.fake.stream(request)
        }
    }

    /// The visible text of `message`, or the tool text of a call and a result,
    /// so a test names a request without its own copy of the types.
    fn text_of(message: &Message) -> String {
        match message {
            Message::User { content } => texts(content),
            Message::Assistant(assistant) => texts(&assistant.content),
            Message::ToolResult(result) => result.content.to_string(),
        }
    }

    fn texts(content: &[Content]) -> String {
        content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(&**text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// A read-only tool that answers `hi`.
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
            _args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async { ToolOutput::ok("hi") })
        }
    }

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry.add(Echo {
            spec: ToolSpec {
                name: "echo".into(),
                description: "Echo.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        });
        registry
    }

    /// The `echo` tool and the `ask` tool over `questions`.
    fn asking_registry(questions: &Arc<Questions>) -> Registry {
        let mut registry = registry();
        registry.add(Ask::new(Arc::clone(questions)));
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

    fn text_reply(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta(Text::from(text)),
            StreamEvent::Stop(StopReason::EndTurn),
        ]
    }

    fn tool_reply(call_id: &str, name: &str) -> Vec<StreamEvent> {
        args_reply(call_id, name, "{}")
    }

    /// A reply that calls `name` with `args`.
    fn args_reply(call_id: &str, name: &str, args: &str) -> Vec<StreamEvent> {
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

    /// The id the next message of a test takes, so every message of the run
    /// has its own.
    static NEXT_MESSAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    /// One message of `text`, under an id of its own.
    fn send(client: ClientId, text: &str, mode: SendMode) -> Command {
        send_with(client, text, Vec::new(), mode)
    }

    /// One message of `text` and the `blocks` that follow it, under an id of
    /// its own.
    fn send_with(client: ClientId, text: &str, blocks: Vec<Content>, mode: SendMode) -> Command {
        send_as(
            NEXT_MESSAGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            client,
            text,
            blocks,
            mode,
        )
    }

    /// The message `id` of `text` and the `blocks` that follow it.
    fn send_as(
        id: u64,
        client: ClientId,
        text: &str,
        blocks: Vec<Content>,
        mode: SendMode,
    ) -> Command {
        Command::Send {
            message: id,
            client,
            text: Text::from(text),
            blocks,
            mode,
        }
    }

    /// The blocks a message that names one image carries: its note, then the
    /// image.
    fn image_blocks() -> Vec<Content> {
        vec![
            Content::text("[image 64x48]"),
            Content::Image(reagent_ai::Image {
                media_type: reagent_ai::ImageType::Png,
                width: 64,
                height: 48,
                data: Text::from("cG5n"),
            }),
        ]
    }

    /// The events `rx` holds now, without waiting.
    fn taken(rx: &mut Receiver<Event>) -> Vec<Event> {
        let mut drained = Vec::new();
        while let Ok(event) = rx.try_recv() {
            drained.push(event);
        }
        drained
    }

    /// Waits for one event that `done` accepts, so a test needs no spin.
    async fn until(rx: &mut Receiver<Event>, done: impl Fn(&Event) -> bool) {
        while let Some(event) = rx.recv().await {
            if done(&event) {
                return;
            }
        }
        panic!("the events ended before the one the test waited for");
    }

    fn compact(note: Option<&str>) -> Command {
        Command::Compact {
            client: 9,
            note: note.map(Text::from),
        }
    }

    /// A resumed history of `turns` turns of 10 KB each, longer than the tail a
    /// compaction keeps.
    fn long_history(turns: usize) -> Vec<Message> {
        let mut history = vec![Message::user("task")];
        let chunk = "x".repeat(10_000);
        for _ in 0..turns {
            history.push(Message::user(chunk.as_str()));
            history.push(Message::assistant(reagent_ai::AssistantMessage::new(
                "p",
                "m",
                vec![Content::Text {
                    text: Text::from(chunk.as_str()),
                }],
            )));
        }
        history
    }

    /// Sends `command` to an idle session of `history` and waits for the
    /// event `wanted` answers.
    async fn idle_compact(
        mut agent: Agent<'_>,
        history: &mut Vec<Message>,
        command: Command,
        wanted: impl Fn(&Event) -> bool,
    ) {
        let (tx, mut inbox) = channel();
        let (events, mut rx) = mpsc::channel(256);
        let driver = async {
            tx.send(command).await.expect("the session is open");
            until(&mut rx, wanted).await;
            drop(tx);
        };
        let session = run_live(&mut agent, history, None, &events, &mut inbox);
        tokio::join!(session, driver);
    }

    #[tokio::test]
    async fn a_compact_command_compacts_an_idle_session_at_once() {
        let script = Script::new(vec![], vec![], channel().0);
        let registry = registry();
        let agent = Agent::new(&script, &registry, config(), CancellationToken::new())
            .with_compact(Compact::with_budget(10_000_000, "."));
        let mut history = long_history(20);
        idle_compact(
            agent,
            &mut history,
            compact(Some("keep the API notes")),
            |event| matches!(event, Event::Compacted { .. }),
        )
        .await;

        assert_eq!(text_of(&history[0]), "task", "the first message stays");
        let checkpoint = text_of(&history[1]);
        assert!(checkpoint.contains("## Checkpoint 1"), "{checkpoint}");
        assert!(
            checkpoint.ends_with("\n\nNote from the user: keep the API notes"),
            "{checkpoint}"
        );
    }

    #[tokio::test]
    async fn a_compact_command_compacts_with_the_policy_off() {
        let script = Script::new(vec![], vec![], channel().0);
        let registry = registry();
        let agent = Agent::new(&script, &registry, config(), CancellationToken::new())
            .with_compact_on_request(Compact::new("."));
        let mut history = long_history(20);
        let turns = history.len();
        idle_compact(agent, &mut history, compact(None), |event| {
            matches!(event, Event::Compacted { .. })
        })
        .await;

        assert!(text_of(&history[1]).contains("## Checkpoint 1"));
        assert!(
            history.len() < turns,
            "older turns went into the checkpoint"
        );
    }

    #[tokio::test]
    async fn a_compact_command_with_nothing_to_go_reaches_the_client() {
        let (tx, mut inbox) = channel();
        let script = Script::new(vec![text_reply("one")], vec![], tx.clone());
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new())
            .with_compact_on_request(Compact::new("."));
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            until(&mut rx, |event| matches!(event, Event::RunEnd(_))).await;
            tx.send(compact(None)).await.expect("the session is open");
            until(
                &mut rx,
                |event| matches!(event, Event::Error(error) if &**error == "nothing to compact"),
            )
            .await;
            script.close();
            drop(tx);
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        tokio::join!(session, driver);

        assert_eq!(history.len(), 2, "the history is unchanged");
    }

    #[tokio::test]
    async fn a_steer_message_lands_after_the_tool_result() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![tool_reply("call_1", "echo"), text_reply("done")],
            vec![vec![send(7, "use cargo nextest", SendMode::Steer)]],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            until(&mut rx, |event| matches!(event, Event::RunEnd(_))).await;
            script.close();
            drop(tx);
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        tokio::join!(session, driver);

        let requests = script.requests();
        assert_eq!(requests.len(), 2, "two requests: {requests:?}");
        assert_eq!(
            requests[1],
            vec![
                (Role::User, "hello".to_string()),
                (Role::Assistant, String::new()),
                (Role::ToolResult, "hi".to_string()),
                (Role::User, "use cargo nextest".to_string()),
            ],
            "the steer message follows the tool result"
        );
    }

    #[tokio::test]
    async fn a_steer_while_the_model_streams_keeps_the_run_going() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![text_reply("first"), text_reply("second")],
            vec![vec![
                send(7, "say more", SendMode::Steer),
                Command::Set {
                    client: 7,
                    settings: Settings {
                        model: Some(Text::from("other-model")),
                        ..Settings::default()
                    },
                },
            ]],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            until(&mut rx, |event| matches!(event, Event::RunEnd(_))).await;
            script.close();
            drop(tx);
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        tokio::join!(session, driver);

        let requests = script.requests();
        assert_eq!(requests.len(), 2, "the run went on: {requests:?}");
        assert_eq!(
            requests[1],
            vec![
                (Role::User, "hello".to_string()),
                (Role::Assistant, "first".to_string()),
                (Role::User, "say more".to_string()),
            ],
            "the reply, then the steer message"
        );
    }

    #[tokio::test]
    async fn two_queued_messages_start_two_later_runs_in_order() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![
                text_reply("first"),
                text_reply("second"),
                text_reply("third"),
            ],
            vec![vec![
                send(2, "add a test", SendMode::Queue),
                send(3, "run clippy", SendMode::Queue),
            ]],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            // Three runs end with three end-turn run ends.
            for _ in 0..3 {
                until(&mut rx, |event| matches!(event, Event::RunEnd(_))).await;
            }
            script.close();
            drop(tx);
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        tokio::join!(session, driver);

        let roles: Vec<(Role, String)> = history
            .iter()
            .map(|message| (message.role(), text_of(message)))
            .collect();
        assert_eq!(
            roles,
            vec![
                (Role::User, "hello".to_string()),
                (Role::Assistant, "first".to_string()),
                (Role::User, "add a test".to_string()),
                (Role::Assistant, "second".to_string()),
                (Role::User, "run clippy".to_string()),
                (Role::Assistant, "third".to_string()),
            ],
            "each queued message starts its own run, in order"
        );
    }

    #[tokio::test]
    async fn a_worker_image_message_queued_and_steered_reaches_the_history() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![
                text_reply("first"),
                text_reply("second"),
                text_reply("third"),
            ],
            vec![vec![
                send_with(7, "look at shot.png", image_blocks(), SendMode::Steer),
                send_with(8, "and again", image_blocks(), SendMode::Queue),
            ]],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            for _ in 0..2 {
                until(&mut rx, |event| matches!(event, Event::RunEnd(_))).await;
            }
            script.close();
            drop(tx);
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        tokio::join!(session, driver);

        let carried: Vec<(String, usize)> = history
            .iter()
            .filter_map(|message| match message {
                Message::User { content } => Some((
                    texts(content),
                    content
                        .iter()
                        .filter(|block| matches!(block, Content::Image(_)))
                        .count(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            carried,
            vec![
                ("hello".to_string(), 0),
                ("look at shot.png[image 64x48]".to_string(), 1),
                ("and again[image 64x48]".to_string(), 1),
            ],
            "the steered and the queued message keep their blocks after the text"
        );
        let requests = script.requests();
        assert_eq!(
            requests.len(),
            3,
            "one request per run, one more for the steer: {requests:?}"
        );
        assert!(
            format!("{:?}", requests[1]).contains("[image 64x48]"),
            "the steered message reaches the next request: {requests:?}"
        );
    }

    /// The events of `rx` up to and including `runs` run ends.
    async fn events_until(rx: &mut Receiver<Event>, runs: usize) -> Vec<Event> {
        let mut seen = Vec::new();
        let mut ended = 0;
        while let Some(event) = rx.recv().await {
            ended += usize::from(matches!(event, Event::RunEnd(_)));
            seen.push(event);
            if ended == runs {
                return seen;
            }
        }
        panic!("the channel closed after {ended} runs: {seen:?}");
    }

    /// The place and the state of the one `message_state` event of `message`
    /// in `seen`, and the reason it names.
    fn state_of(seen: &[Event], message: u64) -> (usize, MessageState, Option<DropReason>) {
        let states: Vec<(usize, MessageState, Option<DropReason>)> = seen
            .iter()
            .enumerate()
            .filter_map(|(at, event)| match event {
                Event::MessageState {
                    message: id,
                    state,
                    reason,
                } if *id == message => Some((at, *state, *reason)),
                _ => None,
            })
            .collect();
        assert_eq!(states.len(), 1, "one state for message {message}: {seen:?}");
        states[0]
    }

    #[tokio::test]
    async fn message_state_a_steer_lands_under_its_id_before_the_next_request() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![tool_reply("call_1", "echo"), text_reply("done")],
            vec![vec![send_as(
                5,
                7,
                "use cargo nextest",
                Vec::new(),
                SendMode::Steer,
            )]],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            let seen = events_until(&mut rx, 1).await;
            script.close();
            drop(tx);
            seen
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        let (_, seen) = tokio::join!(session, driver);

        let told = seen
            .iter()
            .position(|event| matches!(event, Event::UserMessage { message: 5, .. }))
            .expect("the accepted message names its id");
        let (landed, state, reason) = state_of(&seen, 5);
        assert_eq!(state, MessageState::Landed);
        assert_eq!(reason, None, "a landed message names no reason");
        assert!(landed > told, "the message lands after it is accepted");
        let next = seen[landed + 1..]
            .iter()
            .position(|event| matches!(event, Event::TurnStart))
            .expect("the request after the steer");
        assert_eq!(
            next, 0,
            "the state comes right before its request: {seen:?}"
        );
    }

    #[tokio::test]
    async fn message_state_a_queued_message_lands_when_its_run_starts() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![text_reply("first"), text_reply("second")],
            vec![vec![send_as(
                6,
                7,
                "add a test",
                Vec::new(),
                SendMode::Queue,
            )]],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            let seen = events_until(&mut rx, 2).await;
            script.close();
            drop(tx);
            seen
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        let (_, seen) = tokio::join!(session, driver);

        let ended = seen
            .iter()
            .position(|event| matches!(event, Event::RunEnd(_)))
            .expect("the run before it ends");
        let (landed, state, reason) = state_of(&seen, 6);
        assert_eq!(state, MessageState::Landed);
        assert_eq!(reason, None, "a landed message names no reason");
        assert!(
            landed > ended,
            "the queued message lands after the run before it ends"
        );
        let next = seen[landed + 1..]
            .iter()
            .position(|event| matches!(event, Event::TurnStart))
            .expect("the run it starts");
        assert_eq!(next, 0, "the state comes right before its run: {seen:?}");
    }

    #[tokio::test]
    async fn message_state_a_cancel_drops_the_waiting_messages() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![tool_reply("call_1", "echo"), text_reply("done")],
            vec![vec![
                send_as(5, 7, "use cargo nextest", Vec::new(), SendMode::Steer),
                send_as(6, 8, "add a test", Vec::new(), SendMode::Queue),
                Command::Cancel { client: 9 },
            ]],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            let seen = events_until(&mut rx, 1).await;
            script.close();
            drop(tx);
            seen
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        let (_, seen) = tokio::join!(session, driver);

        for message in [5, 6] {
            let (_, state, reason) = state_of(&seen, message);
            assert_eq!(state, MessageState::Dropped);
            assert_eq!(reason, Some(DropReason::Cancelled));
        }
        assert!(
            !seen.iter().any(|event| matches!(
                event,
                Event::MessageState {
                    message: 5 | 6,
                    state: MessageState::Landed,
                    ..
                }
            )),
            "a dropped message never landed: {seen:?}"
        );
        assert!(
            seen.contains(&Event::RunEnd(TurnEndReason::Cancelled)),
            "the cancel ended the run: {seen:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_session_makes_no_request() {
        let (tx, mut inbox) = channel();
        let script = Script::new(vec![], vec![], tx.clone());
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(
                script.seen().is_empty(),
                "a session with no message makes no request"
            );
            script.close();
            drop(tx);
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        tokio::join!(session, driver);

        assert!(history.is_empty(), "no message, no conversation");
        assert!(taken(&mut rx).is_empty(), "no event, no request");
    }

    /// A provider whose first reply never ends, and whose later replies say
    /// `done`.
    struct Endless {
        requests: Mutex<usize>,
        opened: Notify,
    }

    impl Provider for Endless {
        fn name(&self) -> &'static str {
            "endless"
        }

        fn stream(&self, _request: &Request<'_>) -> BoxStream<'static, Result<StreamEvent, Error>> {
            let first = {
                let mut requests = self.requests.lock().expect("requests lock");
                *requests += 1;
                *requests == 1
            };
            self.opened.notify_one();
            if first {
                Box::pin(stream::pending::<Result<StreamEvent, Error>>())
            } else {
                Box::pin(stream::iter(text_reply("done").into_iter().map(Ok)))
            }
        }
    }

    #[tokio::test]
    async fn a_cancel_ends_the_turn_and_the_session_stays_open() {
        let (tx, mut inbox) = channel();
        let provider = Endless {
            requests: Mutex::new(0),
            opened: Notify::new(),
        };
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            provider.opened.notified().await;
            tx.send(Command::Cancel { client: 1 })
                .await
                .expect("the cancel reaches the session");
            until(&mut rx, |event| {
                matches!(event, Event::RunEnd(TurnEndReason::Cancelled))
            })
            .await;
            // The session is still open: a later message starts a new run.
            tx.send(send(2, "again", SendMode::Steer))
                .await
                .expect("the cancel keeps the session open");
            until(&mut rx, |event| {
                matches!(event, Event::RunEnd(TurnEndReason::EndTurn))
            })
            .await;
            drop(tx);
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(session, driver);
        })
        .await
        .expect("the turn ends as cancelled within a second");

        let roles: Vec<(Role, String)> = history
            .iter()
            .map(|message| (message.role(), text_of(message)))
            .collect();
        assert_eq!(
            roles,
            vec![
                (Role::User, "hello".to_string()),
                (Role::User, "again".to_string()),
                (Role::Assistant, "done".to_string()),
            ],
            "the cancelled turn keeps no partial reply"
        );
    }

    #[tokio::test]
    async fn a_set_level_applies_from_the_next_request() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![tool_reply("call_1", "echo"), text_reply("done")],
            vec![vec![Command::Set {
                client: 3,
                settings: Settings {
                    level: Some(Text::from("high")),
                    ..Settings::default()
                },
            }]],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let config = AgentConfig {
            reasoning: Some("medium".into()),
            ..config()
        };
        let mut agent = Agent::new(&script, &registry, config, CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            until(&mut rx, |event| matches!(event, Event::RunEnd(_))).await;
            script.close();
            drop(tx);
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        tokio::join!(session, driver);

        let seen = script.seen();
        assert_eq!(seen.len(), 2, "two requests");
        assert_eq!(
            seen[0].reasoning.as_deref(),
            Some("medium"),
            "the request in flight keeps the old level"
        );
        assert_eq!(
            seen[1].reasoning.as_deref(),
            Some("high"),
            "the next request uses the new level"
        );
    }

    #[tokio::test]
    async fn a_user_message_event_names_the_sending_client_once() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![text_reply("done"), text_reply("ok")],
            vec![vec![send(7, "use cargo nextest", SendMode::Steer)]],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            let mut sent = Vec::new();
            while let Some(event) = rx.recv().await {
                let end = matches!(event, Event::RunEnd(_));
                sent.push(event);
                if end {
                    break;
                }
            }
            script.close();
            drop(tx);
            sent
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        let (_, seen) = tokio::join!(session, driver);

        let sent: Vec<(ClientId, SendMode, String)> = seen
            .into_iter()
            .filter_map(|event| match event {
                Event::UserMessage {
                    client, mode, text, ..
                } => Some((client, mode, text.to_string())),
                _ => None,
            })
            .collect();
        assert_eq!(
            sent,
            vec![
                (1, SendMode::Steer, "hello".to_string()),
                (7, SendMode::Steer, "use cargo nextest".to_string()),
            ],
            "one event per accepted message, naming the client that sent it"
        );
    }

    #[tokio::test]
    async fn a_run_without_an_inbox_is_unchanged() {
        let provider = FakeProvider::new(vec![tool_reply("call_1", "echo"), text_reply("done")]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(256);
        let mut history = vec![Message::user("hello")];

        let mut agent = Agent::new(&provider, &registry, config(), CancellationToken::new());
        agent.run(&mut history, None, &tx).await;

        let roles: Vec<Role> = history.iter().map(Message::role).collect();
        assert_eq!(
            roles,
            vec![
                Role::User,
                Role::Assistant,
                Role::ToolResult,
                Role::Assistant
            ]
        );
        let seen = provider.seen();
        assert_eq!(seen.len(), 2, "two requests, as before the inbox existed");
        assert_eq!(seen[0].messages, 1);
        assert_eq!(seen[1].messages, 3);
        assert_eq!(
            taken(&mut rx),
            vec![
                Event::TurnStart,
                Event::ToolStart {
                    id: Text::from("call_1"),
                    name: Text::from("echo"),
                },
                Event::ToolCallReady {
                    id: Text::from("call_1"),
                    name: Text::from("echo"),
                    arguments: Text::from("{}"),
                },
                Event::ToolResult {
                    id: Text::from("call_1"),
                    is_error: false,
                    bytes: 2,
                    output: Some(Text::from("hi")),
                    images: Vec::new(),
                },
                Event::TurnEnd(TurnEndReason::EndTurn),
                Event::TurnStart,
                Event::TextDelta(Text::from("done")),
                Event::TurnEnd(TurnEndReason::EndTurn),
                Event::RunEnd(TurnEndReason::EndTurn),
            ],
            "the headless events, in order"
        );
    }

    /// A tool that runs until the run is cancelled, and says when it started.
    struct Slow {
        spec: ToolSpec,
        started: std::sync::Arc<Notify>,
    }

    impl Tool for Slow {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn run<'a>(
            &'a self,
            _args: &'a str,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async {
                self.started.notify_one();
                std::future::pending().await
            })
        }
    }

    #[tokio::test]
    async fn a_cancel_reaches_a_running_tool_at_once() {
        let (tx, mut inbox) = channel();
        let started = std::sync::Arc::new(Notify::new());
        let mut registry = Registry::new();
        registry.add(Slow {
            spec: ToolSpec {
                name: "slow".into(),
                description: "Slow.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            started: started.clone(),
        });
        let script = Script::new(vec![tool_reply("c1", "slow")], Vec::new(), tx.clone());
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "build it", SendMode::Steer))
                .await
                .expect("the session is open");
            started.notified().await;
            tx.send(Command::Cancel { client: 1 })
                .await
                .expect("the cancel reaches the session");
            until(&mut rx, |event| {
                matches!(event, Event::RunEnd(TurnEndReason::Cancelled))
            })
            .await;
            drop(tx);
            script.close();
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(session, driver);
        })
        .await
        .expect("a cancel during a tool call ends the turn within a second");
        assert!(
            matches!(history.last(), Some(Message::ToolResult(_))),
            "the cancelled call still has its answer: {history:?}"
        );
    }

    #[tokio::test]
    async fn a_cancel_between_runs_does_not_stop_the_next_run() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![text_reply("done"), text_reply("ok")],
            Vec::new(),
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            until(&mut rx, |event| matches!(event, Event::RunEnd(_))).await;
            // The cancel arrives after the run it meant to stop has ended.
            tx.send(Command::Cancel { client: 1 })
                .await
                .expect("the session is open");
            tx.send(send(1, "again", SendMode::Queue))
                .await
                .expect("the session is open");
            let mut ends = Vec::new();
            while let Some(event) = rx.recv().await {
                if let Event::RunEnd(reason) = event {
                    ends.push(reason);
                    break;
                }
            }
            drop(tx);
            script.close();
            ends
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        let ((), ends) = tokio::join!(session, driver);
        assert_eq!(
            ends,
            vec![TurnEndReason::EndTurn],
            "the next run is not cancelled"
        );
        assert_eq!(script.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_live_session_writes_each_message_to_the_journal_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let header = reagent_store::Header::new(
            "session".into(),
            reagent_store::paths::now_ms(),
            None,
            bytes::Bytes::new(),
        );
        let mut journal = Journal::create(dir.path(), header).expect("create the journal");
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![text_reply("done"), text_reply("ok")],
            vec![vec![send(1, "again", SendMode::Queue)]],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            let mut ends = 0;
            while let Some(event) = rx.recv().await {
                if matches!(event, Event::RunEnd(_)) {
                    ends += 1;
                    if ends == 2 {
                        break;
                    }
                }
            }
            drop(tx);
            script.close();
        };
        let session = run_live(
            &mut agent,
            &mut history,
            Some(&mut journal),
            &events,
            &mut inbox,
        );
        tokio::join!(session, driver);
        journal.close().expect("close the journal");

        let session = reagent_store::Session::open(dir.path()).expect("open the session");
        let resumed = crate::journal::resume::load(&session).expect("load the session");
        let roles: Vec<(Role, String)> = resumed
            .history
            .iter()
            .map(|message| (message.role(), text_of(message)))
            .collect();
        assert_eq!(
            roles,
            vec![
                (Role::User, "hello".to_string()),
                (Role::Assistant, "done".to_string()),
                (Role::User, "again".to_string()),
                (Role::Assistant, "ok".to_string()),
            ],
            "each message is in the journal once, in order"
        );
        // A resume starts at the last base, so it hides a duplicate: count the
        // records on the disk instead.
        let records: Vec<crate::journal::record::Record> = session
            .records()
            .map(|record| {
                let record = record.expect("read a record");
                crate::journal::record::decode(record.kind, &record.payload).expect("decode")
            })
            .collect();
        let bases = records
            .iter()
            .filter(|record| matches!(record, crate::journal::record::Record::Base(_)))
            .count();
        let messages = records
            .iter()
            .filter(|record| matches!(record, crate::journal::record::Record::Message(_)))
            .count();
        assert_eq!((bases, messages), (1, 4), "one base and four messages");
    }

    /// A guard that asks a client about every call.
    fn asking_guard() -> crate::guard::Guard {
        let layer = reagent_config::Layer {
            path: std::path::PathBuf::from("/c/config.toml"),
            settings: reagent_config::Settings {
                permissions: reagent_config::permissions::Lists {
                    default: Some("ask".into()),
                    ..Default::default()
                },
                ..reagent_config::Settings::default()
            },
        };
        let rules = reagent_config::permissions::Rules::from_layers(
            &[layer],
            std::path::Path::new("/work"),
            None,
        )
        .expect("the rules build");
        crate::guard::Guard::new(rules)
    }

    /// What the driver does when an approval request arrives.
    #[derive(Clone, Debug)]
    enum React {
        /// Send this answer.
        Answer(Answer),
        /// Cancel the run.
        Cancel,
    }

    /// Sends the reactions of `next` for each approval request, and returns
    /// every event up to the first one `done` accepts.
    async fn drive(
        rx: &mut Receiver<Event>,
        tx: &Sender,
        mut next: impl FnMut(u64) -> Vec<React>,
        done: impl Fn(&Event) -> bool,
    ) -> Vec<Event> {
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            let finished = done(&event);
            if let Event::ApprovalRequest(request) = &event {
                let id = request.id;
                for react in next(id) {
                    let command = match react {
                        React::Answer(answer) => Command::Approve {
                            client: 7,
                            id,
                            answer,
                        },
                        React::Cancel => Command::Cancel { client: 7 },
                    };
                    tx.send(command).await.expect("the session is open");
                }
            }
            seen.push(event);
            if finished {
                return seen;
            }
        }
        panic!("the events ended before the one the test waited for");
    }

    fn resolved(message: &str) -> impl Fn(&Event) -> bool + use<> {
        let message = message.to_string();
        move |event| matches!(event, Event::Error(error) if error.as_ref() == message)
    }

    /// Sends the command `next` returns for each question request, and returns
    /// every event up to the first one `done` accepts.
    async fn quizzed(
        rx: &mut Receiver<Event>,
        tx: &Sender,
        mut next: impl FnMut(&crate::event::QuestionRequest) -> Option<Command>,
        done: impl Fn(&Event) -> bool,
    ) -> Vec<Event> {
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            let finished = done(&event);
            if let Event::QuestionRequest(request) = &event
                && let Some(command) = next(request)
            {
                tx.send(command).await.expect("the session is open");
            }
            seen.push(event);
            if finished {
                return seen;
            }
        }
        panic!("the events ended before the one the test waited for");
    }

    /// The arguments of one question with two options.
    const ONE_QUESTION: &str = r#"{"questions":[{"q":"Which database?","options":[{"label":"sqlite"},{"label":"postgres","note":"on the server"}]}]}"#;

    /// The arguments of two questions, the second of which takes several
    /// picks.
    const TWO_QUESTIONS: &str = r#"{"questions":[{"q":"Which queue?","options":[{"label":"a"},{"label":"z"}]},{"q":"Which store?","options":[{"label":"b"},{"label":"c"}],"multi":true}]}"#;

    /// The result the call `id` got, as the model reads it.
    fn result_of(history: &[Message], id: &str) -> String {
        history
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.call_id.as_ref() == id => {
                    Some(result.content.to_string())
                }
                _ => None,
            })
            .expect("the call has a result")
    }

    /// The session of one question, driven until `done` accepts an event, and
    /// the events it saw.
    async fn asked_session(
        args: &str,
        answers: crate::question::Answers,
        done: impl Fn(&Event) -> bool,
    ) -> (Vec<Event>, Vec<Message>) {
        let (tx, mut inbox) = channel();
        let questions = Questions::new();
        let script = Script::new(
            vec![args_reply("call_1", "ask", args), text_reply("done")],
            vec![],
            tx.clone(),
        );
        let registry = asking_registry(&questions);
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new())
            .with_questions(Arc::clone(&questions));
        let (events, mut rx) = mpsc::channel(256);
        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            let seen = quizzed(
                &mut rx,
                &tx,
                |request| {
                    Some(Command::Answer {
                        client: 7,
                        id: request.id,
                        answers: answers.clone(),
                    })
                },
                &done,
            )
            .await;
            script.close();
            drop(tx);
            seen
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        let (_, seen) = tokio::join!(session, driver);
        (seen, history)
    }

    #[tokio::test]
    async fn question_the_session_asks_and_an_answer_is_the_result() {
        let (seen, history) = asked_session(
            ONE_QUESTION,
            Some(vec![vec!["sqlite".to_owned()]]),
            |event| matches!(event, Event::ToolResult { id, .. } if id.as_ref() == "call_1"),
        )
        .await;

        assert!(
            seen.iter().any(|event| matches!(
                event,
                Event::QuestionRequest(request)
                    if request.questions.len() == 1
                        && request.questions[0].q.as_ref() == "Which database?"
                        && request.questions[0].options.len() == 2
            )),
            "the session sent the questions: {seen:?}"
        );
        assert!(
            seen.contains(&Event::QuestionResolved { id: 0, client: 7 }),
            "the answering client is named: {seen:?}"
        );
        let result = result_of(&history, "call_1");
        assert!(result.contains("Q: Which database?"), "{result}");
        assert!(result.contains("A: sqlite"), "{result}");
    }

    #[tokio::test]
    async fn question_a_skipped_question_gives_the_fallback_text() {
        let (seen, history) = asked_session(
            ONE_QUESTION,
            None,
            |event| matches!(event, Event::ToolResult { id, .. } if id.as_ref() == "call_1"),
        )
        .await;

        assert!(
            seen.contains(&Event::QuestionResolved { id: 0, client: 7 }),
            "the skipping client is named: {seen:?}"
        );
        let result = result_of(&history, "call_1");
        assert_eq!(
            result, "No answer; decide yourself and state the choice.",
            "{result}"
        );
    }

    #[tokio::test]
    async fn question_two_questions_give_two_pairs_of_lines() {
        let (_, history) = asked_session(
            TWO_QUESTIONS,
            Some(vec![
                vec!["a".to_owned()],
                vec!["b".to_owned(), "c".to_owned()],
            ]),
            |event| matches!(event, Event::ToolResult { id, .. } if id.as_ref() == "call_1"),
        )
        .await;

        let result = result_of(&history, "call_1");
        assert!(result.contains("Q: Which queue?"), "{result}");
        assert!(result.contains("A: a"), "{result}");
        assert!(result.contains("Q: Which store?"), "{result}");
        assert!(result.contains("A: b, c"), "{result}");
    }

    #[tokio::test]
    async fn question_a_cancel_while_the_question_waits_ends_the_run_as_cancelled() {
        let (tx, mut inbox) = channel();
        let questions = Questions::new();
        let script = Script::new(
            vec![
                args_reply("call_1", "ask", ONE_QUESTION),
                text_reply("done"),
            ],
            vec![],
            tx.clone(),
        );
        let registry = asking_registry(&questions);
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new())
            .with_questions(Arc::clone(&questions));
        let (events, mut rx) = mpsc::channel(256);
        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            let seen = quizzed(
                &mut rx,
                &tx,
                |_| Some(Command::Cancel { client: 7 }),
                |event| matches!(event, Event::RunEnd(TurnEndReason::Cancelled)),
            )
            .await;
            script.close();
            drop(tx);
            seen
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        let (_, seen) = tokio::join!(session, driver);

        assert!(
            seen.contains(&Event::QuestionResolved { id: 0, client: 0 }),
            "the cancel resolved the question with no client: {seen:?}"
        );
        assert_eq!(
            result_of(&history, "call_1"),
            "No answer; decide yourself and state the choice.",
            "the cancel was answered by the tool"
        );
        assert_eq!(
            script.requests().len(),
            1,
            "the cancelled turn made no second request"
        );
    }

    #[tokio::test]
    async fn the_first_approval_answer_wins_and_a_second_names_the_resolved_id() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![tool_reply("call_1", "echo"), text_reply("done")],
            vec![],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new())
            .with_guard(asking_guard());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            // Two answers to one request: the second has no call to reach.
            let seen = drive(
                &mut rx,
                &tx,
                |_| vec![React::Answer(Answer::Once), React::Answer(Answer::Once)],
                resolved("approval 0 is already resolved"),
            )
            .await;
            script.close();
            drop(tx);
            seen
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        let (_, seen) = tokio::join!(session, driver);

        let requests = seen
            .iter()
            .filter(|event| matches!(event, Event::ApprovalRequest(_)))
            .count();
        assert_eq!(requests, 1, "one request, answered twice: {seen:?}");
        assert!(
            history.iter().any(
                |message| matches!(message, Message::ToolResult(result) if result.content.as_ref() == "hi")
            ),
            "the first answer ran the call"
        );
    }

    #[tokio::test]
    async fn a_set_to_bypass_mode_that_is_not_enabled_is_refused_and_the_mode_stays() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![tool_reply("call_1", "echo"), text_reply("done")],
            vec![],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new())
            .with_guard(asking_guard());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(Command::Set {
                client: 2,
                settings: Settings {
                    mode: Some(reagent_config::permissions::PermissionMode::Bypass),
                    ..Settings::default()
                },
            })
            .await
            .expect("the session is open");
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            let seen = drive(
                &mut rx,
                &tx,
                |_| vec![React::Answer(Answer::Once)],
                |event| matches!(event, Event::RunEnd(_)),
            )
            .await;
            script.close();
            drop(tx);
            seen
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        let (_, seen) = tokio::join!(session, driver);

        assert!(
            seen.iter().any(|event| matches!(
                event,
                Event::Error(error) if error.contains("`bypass` is not enabled")
            )),
            "{seen:?}"
        );
        // The mode stayed the default, so the call still asked.
        assert_eq!(
            seen.iter()
                .filter(|event| matches!(event, Event::ApprovalRequest(_)))
                .count(),
            1,
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn a_cancel_while_waiting_for_an_approval_ends_the_run_as_cancelled() {
        let (tx, mut inbox) = channel();
        let script = Script::new(
            vec![tool_reply("call_1", "echo"), text_reply("done")],
            vec![],
            tx.clone(),
        );
        let registry = registry();
        let mut history = Vec::new();
        let mut agent = Agent::new(&script, &registry, config(), CancellationToken::new())
            .with_guard(asking_guard());
        let (events, mut rx) = mpsc::channel(256);

        let driver = async {
            tx.send(send(1, "hello", SendMode::Steer))
                .await
                .expect("the session is open");
            let seen = drive(
                &mut rx,
                &tx,
                |_| vec![React::Cancel],
                |event| matches!(event, Event::RunEnd(TurnEndReason::Cancelled)),
            )
            .await;
            script.close();
            drop(tx);
            seen
        };
        let session = run_live(&mut agent, &mut history, None, &events, &mut inbox);
        let (_, seen) = tokio::join!(session, driver);

        assert!(
            seen.iter()
                .any(|event| matches!(event, Event::RunEnd(TurnEndReason::Cancelled))),
            "{seen:?}"
        );
        assert!(
            history.iter().any(
                |message| matches!(message, Message::ToolResult(result) if result.content.as_ref() == "cancelled")
            ),
            "the call did not run: {history:?}"
        );
        assert_eq!(
            script.requests().len(),
            1,
            "the cancelled turn made no second request"
        );
    }
}
