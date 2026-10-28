//! Loop policies: a hook that runs after every turn.
//!
//! A [`TurnPolicy`] looks at what one turn did and can inject a message into
//! the conversation, let the loop continue, or stop the run. Policies are
//! purely additive: with no policies the loop behaves exactly as it did
//! without them, so existing behaviour and tests are unchanged.
//!
//! Loop semantics when policies are present:
//!
//! - After a turn that ran tools, any injected texts are appended to the last
//!   tool result of that turn and the loop continues. A `Stop` ends the run.
//! - After a turn that ended with no tool call, an injected text starts a new
//!   user message and the loop continues. A `Stop`, or a turn with no
//!   injection, ends the run. With no policies at all a no-tool turn always
//!   ends the run, as before.

use std::time::Duration;

use reagent_ai::{AssistantMessage, Text, ToolCall, ToolResult};

use crate::event::Event;

pub mod budget;
pub mod completion;
pub mod stuck;

/// One turn's outcome, borrowed, so a policy can read it without any message
/// text being copied.
#[derive(Debug)]
pub struct TurnView<'a> {
    /// The 1-based index of the turn.
    pub index: u32,
    /// How long the run has been going, from the first turn start.
    pub elapsed: Duration,
    /// Whether the turn ran at least one tool call.
    pub ran_tools: bool,
    /// The assistant reply this turn produced.
    pub assistant: &'a AssistantMessage,
    /// The tool calls the reply made and their result messages, in call order.
    pub tools: &'a [(ToolCall, ToolResult)],
    /// Whether the turn changed files; `None` when the loop has no workspace
    /// or a fingerprint failed.
    pub changed_files: Option<bool>,
}

/// What a policy wants the loop to do after a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Keep going; the turn's own outcome decides whether that needs another
    /// provider call. A no-tool turn that gets only `Continue` still ends.
    Continue,
    /// End the run now, whatever the turn did.
    Stop,
    /// Inject this text into the conversation and keep going.
    Inject(Text),
}

/// A hook that runs once after each turn.
///
/// The bound is what the loop needs: the value is held in a `Box` inside the
/// agent, which is moved across an `await`, so it must be both `Send` and
/// shareable as a `&T` for the loop's shared borrow.
pub trait TurnPolicy: Send + Sync {
    /// Looks at the turn and reports what the loop should do next.
    ///
    /// Called in order, once per turn. Implementations may keep counters in
    /// `&mut self`. The text returned in [`Decision::Inject`] must be cheap;
    /// it is joined with other injections and sent to the model.
    fn after_turn(&mut self, turn: &TurnView<'_>) -> Decision;
}

/// Runs every policy against a turn and decides the loop action and the
/// messages to inject.
///
/// The first [`Decision::Stop`] wins. [`Decision::Inject`] texts are joined
/// in policy order. A policy that reports nothing never ends a tool turn, but
/// for a no-tool turn the loop's default is to end unless some policy
/// injected.
pub(crate) struct PolicyRun<'a> {
    /// The joined injected texts, in policy order.
    pub injected: Option<Text>,
    /// Whether any policy asked to stop.
    pub stop: bool,
    /// The `PolicyNote` events for the UI, one per injected text.
    pub notes: Vec<Event>,
    /// The turn the policies looked at.
    pub turn: &'a TurnView<'a>,
}

impl<'a> PolicyRun<'a> {
    /// Asks each policy in turn and collects the outcome.
    pub fn new(policies: &mut [Box<dyn TurnPolicy>], turn: &'a TurnView<'a>) -> Self {
        let mut injected: Vec<Text> = Vec::new();
        let mut notes = Vec::new();
        let mut stop = false;
        for policy in policies.iter_mut() {
            match policy.after_turn(turn) {
                Decision::Continue => {}
                Decision::Stop if !stop => stop = true,
                Decision::Stop => {}
                Decision::Inject(text) => {
                    notes.push(Event::PolicyNote(text.clone()));
                    injected.push(text);
                }
            }
        }
        let injected = if injected.is_empty() {
            None
        } else {
            Some(Text::from(join(injected)))
        };
        Self {
            injected,
            stop,
            notes,
            turn,
        }
    }

    /// Whether the run should continue after this turn.
    ///
    /// A tool turn keeps going unless a policy stopped it. A no-tool turn
    /// keeps going only when a policy injected a message; without one (and
    /// with no policies) it ends as the loop did before any policies existed.
    pub fn continue_run(&self) -> bool {
        if self.stop {
            return false;
        }
        if self.turn.ran_tools {
            return true;
        }
        self.injected.is_some()
    }
}

fn join(texts: Vec<Text>) -> String {
    // Text is Arc<str>; building one String from the joined parts is the only
    // allocation. This runs only when a policy fires, not on the hot path.
    let mut out = String::new();
    for (index, text) in texts.iter().enumerate() {
        if index > 0 {
            out.push_str("\n\n");
        }
        out.push_str(text);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use reagent_ai::fake::FakeProvider;
    use reagent_ai::{Content, Message, StreamEvent, ToolSpec};
    use serde::Deserialize;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent_loop::{Agent, AgentConfig};
    use crate::event::TurnEndReason;
    use crate::tool::{Registry, Tool, ToolOutput, parse_args};

    fn assistant(text: &str) -> AssistantMessage {
        AssistantMessage::new("fake", "m", vec![Content::text(text)])
    }

    fn turn<'a>(
        index: u32,
        ran_tools: bool,
        assistant_: &'a AssistantMessage,
        tools: &'a [(ToolCall, ToolResult)],
    ) -> TurnView<'a> {
        TurnView {
            index,
            elapsed: Duration::from_secs(index as u64),
            ran_tools,
            assistant: assistant_,
            tools,
            changed_files: None,
        }
    }

    /// A policy that lets a test script its replies.
    struct Scripted {
        replies: Vec<Decision>,
    }

    impl TurnPolicy for Scripted {
        fn after_turn(&mut self, _turn: &TurnView<'_>) -> Decision {
            self.replies.remove(0)
        }
    }

    fn policy(replies: Vec<Decision>) -> Box<dyn TurnPolicy> {
        Box::new(Scripted { replies })
    }

    #[test]
    fn an_injected_text_becomes_a_policy_note() {
        let a = assistant("done");
        let t = turn(1, false, &a, &[]);
        let mut policies = vec![policy(vec![Decision::Inject("check".into())])];

        let run = PolicyRun::new(&mut policies, &t);

        assert_eq!(run.injected.as_deref(), Some("check"));
        assert_eq!(run.notes.len(), 1);
        assert!(matches!(&run.notes[0], Event::PolicyNote(text) if text.as_ref() == "check"));
    }

    #[test]
    fn injected_texts_join_in_policy_order() {
        let a = assistant("done");
        let t = turn(1, true, &a, &[]);
        let mut policies = vec![
            policy(vec![Decision::Inject("first".into())]),
            policy(vec![Decision::Continue]),
            policy(vec![Decision::Inject("second".into())]),
        ];

        let run = PolicyRun::new(&mut policies, &t);

        assert_eq!(run.injected.as_deref(), Some("first\n\nsecond"));
    }

    #[test]
    fn a_stop_wins_over_an_injection() {
        let a = assistant("done");
        let t = turn(1, false, &a, &[]);
        let mut policies = vec![
            policy(vec![Decision::Inject("note".into())]),
            policy(vec![Decision::Stop]),
        ];

        let run = PolicyRun::new(&mut policies, &t);

        assert!(run.stop);
        assert!(!run.continue_run());
    }

    #[test]
    fn a_no_tool_turn_ends_without_an_injection() {
        let a = assistant("done");
        let t = turn(1, false, &a, &[]);
        let mut policies = vec![policy(vec![Decision::Continue])];

        let run = PolicyRun::new(&mut policies, &t);

        assert!(!run.continue_run(), "no injection, no stop: the turn ends");
    }

    #[test]
    fn no_policies_means_a_no_tool_turn_ends() {
        let a = assistant("done");
        let t = turn(1, false, &a, &[]);
        let mut policies: Vec<Box<dyn TurnPolicy>> = Vec::new();

        let run = PolicyRun::new(&mut policies, &t);

        assert!(!run.stop);
        assert!(run.injected.is_none());
        assert!(
            !run.continue_run(),
            "with no policies the run ends as before"
        );
    }

    #[test]
    fn a_tool_turn_continues_without_any_decision() {
        let a = assistant("ran");
        let t = turn(1, true, &a, &[]);
        let mut policies: Vec<Box<dyn TurnPolicy>> = Vec::new();

        let run = PolicyRun::new(&mut policies, &t);

        assert!(run.continue_run(), "a tool turn always continues");
    }

    #[test]
    fn a_tool_turn_stops_when_a_policy_asks() {
        let a = assistant("ran");
        let t = turn(1, true, &a, &[]);
        let mut policies = vec![policy(vec![Decision::Stop])];

        let run = PolicyRun::new(&mut policies, &t);

        assert!(run.stop);
        assert!(!run.continue_run());
    }

    #[test]
    fn a_tool_turn_with_an_injection_continues() {
        let a = assistant("ran");
        let t = turn(1, true, &a, &[]);
        let mut policies = vec![policy(vec![Decision::Inject("keep going".into())])];

        let run = PolicyRun::new(&mut policies, &t);

        assert!(!run.stop);
        assert!(run.continue_run());
        assert_eq!(run.injected.as_deref(), Some("keep going"));
    }

    /// A policy that injects on the first N turns, then stays quiet.
    struct InjectFirst {
        text: Text,
        remaining: usize,
    }

    impl TurnPolicy for InjectFirst {
        fn after_turn(&mut self, _turn: &TurnView<'_>) -> Decision {
            if self.remaining > 0 {
                self.remaining -= 1;
                Decision::Inject(self.text.clone())
            } else {
                Decision::Continue
            }
        }
    }

    /// A read-only echo tool whose output is its `message` argument.
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

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry.add(Echo {
            spec: ToolSpec {
                name: "echo".into(),
                description: "Echo a message.".into(),
                input_schema: serde_json::json!({"type": "object"}),
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

    fn text_reply(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta(Text::from(text)),
            StreamEvent::Stop(StopReason::EndTurn),
        ]
    }

    fn tool_reply(id: &str, args: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallStart {
                id: Text::from(id),
                name: Text::from("echo"),
            },
            StreamEvent::ToolCallDelta {
                id: Text::from(id),
                partial_json: Text::from(args),
            },
            StreamEvent::ToolCallEnd { id: Text::from(id) },
            StreamEvent::Stop(StopReason::ToolUse),
        ]
    }

    fn drain(tx: mpsc::Sender<Event>, rx: &mut mpsc::Receiver<Event>) -> Vec<Event> {
        drop(tx);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn an_injected_user_message_continues_a_no_tool_turn() {
        // Two text replies: the first ends a turn with no tool, the policy
        // injects, and the loop runs a second turn with the second reply.
        let provider = FakeProvider::new(vec![text_reply("first"), text_reply("second")]);
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];
        let policies: Vec<Box<dyn TurnPolicy>> = vec![Box::new(InjectFirst {
            text: Text::from("keep going"),
            remaining: 1,
        })];

        Agent::with_policies(
            &provider,
            &registry,
            config(),
            CancellationToken::new(),
            policies,
        )
        .run(&mut history, None, &tx)
        .await;

        assert_eq!(provider.seen().len(), 2, "two turns, two requests");
        assert!(
            history.iter().any(|m| matches!(
                m,
                Message::User { content } if content.iter().any(|c| matches!(c, Content::Text { text } if text.as_ref() == "keep going"))
            )),
            "the injected user message is in history: {history:?}"
        );
        let events = drain(tx, &mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::PolicyNote(t) if t.as_ref() == "keep going")),
            "a PolicyNote event reports the injection: {events:?}"
        );
        assert_eq!(events.last(), Some(&Event::RunEnd(TurnEndReason::EndTurn)));
    }

    #[tokio::test]
    async fn injected_text_appends_to_the_last_tool_result() {
        // Turn 1 runs echo, the policy injects, so the loop appends to the
        // echo result. Turn 2 ends normally.
        let provider = FakeProvider::new(vec![
            tool_reply("call_1", r#"{"message":"hi"}"#),
            text_reply("done"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];
        let policies: Vec<Box<dyn TurnPolicy>> = vec![Box::new(InjectFirst {
            text: Text::from("note on the result"),
            remaining: 1,
        })];

        Agent::with_policies(
            &provider,
            &registry,
            config(),
            CancellationToken::new(),
            policies,
        )
        .run(&mut history, None, &tx)
        .await;

        assert_eq!(provider.seen().len(), 2, "two turns, two requests");
        let last_result = history
            .iter()
            .rev()
            .find(|m| matches!(m, Message::ToolResult(_)))
            .expect("a tool result exists");
        let Message::ToolResult(result) = last_result else {
            panic!("expected a tool result");
        };
        assert!(
            result.content.as_ref().contains("note on the result"),
            "the injected text is appended to the tool result: {:?}",
            result.content
        );
        assert!(result.content.starts_with("hi"), "echo output is kept");
    }

    /// A workspace whose fingerprint is 0 on the first call and 1 after.
    struct ChangesOnSecondCall(std::sync::atomic::AtomicU64);

    impl crate::workspace::Workspace for ChangesOnSecondCall {
        fn fingerprint(&self) -> BoxFuture<'_, Option<u64>> {
            let call = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Some(call.min(1)) })
        }
    }

    fn note_count(history: &[Message]) -> usize {
        history.iter().filter(|m| matches!(
            m,
            Message::User { content } if content.iter().any(|c| matches!(c, Content::Text { text } if text.starts_with("You changed files")))
        )).count()
    }

    #[tokio::test]
    async fn the_completion_policy_injects_once_after_unchecked_changes() {
        let provider = FakeProvider::new(vec![
            tool_reply("call_1", r#"{"message":"hi"}"#),
            text_reply("first"),
            text_reply("second"),
            text_reply("third"),
        ]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];
        let policies: Vec<Box<dyn TurnPolicy>> =
            vec![Box::new(crate::policy::completion::Completion::new())];
        let workspace = ChangesOnSecondCall(std::sync::atomic::AtomicU64::new(0));

        Agent::with_policies(
            &provider,
            &registry,
            config(),
            CancellationToken::new(),
            policies,
        )
        .with_workspace(&workspace)
        .run(&mut history, None, &tx)
        .await;

        assert_eq!(provider.seen().len(), 3, "the second end-turn ends the run");
        assert_eq!(note_count(&history), 1, "{history:?}");
    }

    #[tokio::test]
    async fn the_completion_policy_is_quiet_when_nothing_changed() {
        let provider = FakeProvider::new(vec![text_reply("a"), text_reply("b")]);
        let registry = registry();
        let (tx, _rx) = mpsc::channel(64);
        let mut history = vec![Message::user("hello")];
        let policies: Vec<Box<dyn TurnPolicy>> =
            vec![Box::new(crate::policy::completion::Completion::new())];

        Agent::with_policies(
            &provider,
            &registry,
            config(),
            CancellationToken::new(),
            policies,
        )
        .run(&mut history, None, &tx)
        .await;

        assert_eq!(provider.seen().len(), 1);
        assert_eq!(note_count(&history), 0);
    }

    use reagent_ai::StopReason;
}
