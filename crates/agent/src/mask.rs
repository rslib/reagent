//! Observation masking: old large tool outputs become a fixed stub.

use reagent_ai::{Message, Text, ToolResult};

/// The stub that replaces a masked output, baked in from `prompts/mask_stub.md`.
const STUB: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/mask_stub.md"
));

/// How many of the newest tool outputs are never masked.
const KEEP: usize = 3;

/// Outputs under this many bytes are never masked and never counted.
const MIN_BYTES: usize = 800;

/// The bytes of old unmasked outputs that trigger a masking step.
const STEP_BYTES: usize = 80_000;

/// Replaces old large tool outputs in the history with a stub, in steps.
///
/// Below the step size nothing changes, so the prompt prefix stays stable
/// between steps and the provider cache keeps hitting.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mask;

impl Mask {
    /// Masks the outputs older than the newest three when the unmasked ones
    /// of at least 800 bytes add up to 80,000 bytes or more.
    ///
    /// Returns how many outputs it replaced.
    pub fn apply(&self, history: &mut [Message]) -> usize {
        let mut total = 0;
        for result in old_results(history) {
            if is_candidate(result) {
                total += result.content.len();
            }
        }
        if total < STEP_BYTES {
            return 0;
        }
        let stub = Text::from(STUB);
        let mut masked = 0;
        for result in old_results(history) {
            if is_candidate(result) {
                result.content = Text::clone(&stub);
                masked += 1;
            }
        }
        masked
    }
}

/// The tool results older than the newest [`KEEP`] and older than the
/// newest turn, newest first. The newest turn's outputs have not reached the
/// model yet, so a batch of parallel calls is never masked on arrival.
fn old_results(history: &mut [Message]) -> impl Iterator<Item = &mut ToolResult> {
    let newest_turn = history
        .iter()
        .rposition(|message| matches!(message, Message::Assistant(_)))
        .map_or(0, |at| history[at + 1..].len());
    history
        .iter_mut()
        .rev()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .skip(KEEP.max(newest_turn))
}

fn is_candidate(result: &ToolResult) -> bool {
    result.content.len() >= MIN_BYTES && &*result.content != STUB
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(index: usize, bytes: usize) -> Message {
        Message::tool_result(format!("call_{index}"), "x".repeat(bytes), false)
    }

    fn history(sizes: &[usize]) -> Vec<Message> {
        sizes
            .iter()
            .enumerate()
            .map(|(index, bytes)| output(index, *bytes))
            .collect()
    }

    fn text(message: &Message) -> &str {
        match message {
            Message::ToolResult(result) => &result.content,
            _ => panic!("not a tool result"),
        }
    }

    fn masked_flags(history: &[Message]) -> Vec<bool> {
        history.iter().map(|m| text(m) == STUB).collect()
    }

    #[test]
    fn the_newest_turn_is_never_masked() {
        let mut messages = history(&[10_000; 9]);
        messages.push(Message::assistant(reagent_ai::AssistantMessage::new(
            "fake",
            "fake",
            Vec::new(),
        )));
        messages.extend((9..19).map(|index| output(index, 10_000)));
        assert_eq!(Mask.apply(&mut messages), 9);
        assert!(messages[..9].iter().all(|m| text(m) == STUB));
        assert!(messages[10..].iter().all(|m| text(m).len() == 10_000));
    }

    #[test]
    fn below_the_step_nothing_is_masked() {
        let mut messages = history(&[10_000; 5]);
        assert_eq!(Mask.apply(&mut messages), 0);
        assert_eq!(masked_flags(&messages), vec![false; 5]);
    }

    #[test]
    fn masks_the_old_outputs_and_keeps_the_newest_three() {
        let mut messages = history(&[10_000; 12]);
        assert_eq!(Mask.apply(&mut messages), 9);
        let mut expected = vec![true; 9];
        expected.extend([false; 3]);
        assert_eq!(masked_flags(&messages), expected);
        assert!(messages[9..].iter().all(|m| text(m).len() == 10_000));
    }

    #[test]
    fn a_small_old_output_is_never_masked() {
        let mut sizes = vec![10_000; 12];
        sizes[4] = 500;
        let mut messages = history(&sizes);
        assert_eq!(Mask.apply(&mut messages), 8);
        let mut expected = vec![true; 9];
        expected[4] = false;
        expected.extend([false; 3]);
        assert_eq!(masked_flags(&messages), expected);
        assert_eq!(text(&messages[4]).len(), 500);
    }

    #[test]
    fn small_outputs_do_not_count_toward_the_step() {
        let mut messages = history(&[799; 200]);
        assert_eq!(Mask.apply(&mut messages), 0);
    }

    #[test]
    fn newest_three_count_all_sizes_and_errors_are_masked() {
        let mut messages = history(&[10_000; 9]);
        messages.extend(history(&[10, 10, 10]));
        if let Message::ToolResult(result) = &mut messages[0] {
            result.is_error = true;
        }
        assert_eq!(Mask.apply(&mut messages), 9);
        assert_eq!(text(&messages[0]), STUB);
    }

    #[test]
    fn a_later_step_waits_for_the_next_eighty_thousand_bytes() {
        let mut messages = history(&[10_000; 12]);
        assert_eq!(Mask.apply(&mut messages), 9);

        // The old unmasked total is 3 x 10,000 after these 3 more outputs.
        messages.extend(history(&[10_000; 3]));
        assert_eq!(Mask.apply(&mut messages), 0);
        let mut expected = vec![true; 9];
        expected.extend([false; 6]);
        assert_eq!(masked_flags(&messages), expected);

        // 8 old unmasked outputs make 80,000 bytes: masked, stub not recounted.
        messages.extend(history(&[10_000; 5]));
        assert_eq!(Mask.apply(&mut messages), 8);
        assert_eq!(Mask.apply(&mut messages), 0);
    }
}

#[cfg(test)]
mod loop_tests {
    use std::sync::Mutex;

    use futures_util::future::BoxFuture;
    use futures_util::stream::BoxStream;
    use reagent_ai::fake::FakeProvider;
    use reagent_ai::{Error, Provider, Request, StopReason, StreamEvent, ToolSpec};
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent_loop::{Agent, AgentConfig};
    use crate::tool::{Registry, Tool, ToolOutput};

    struct Big {
        spec: ToolSpec,
    }

    impl Tool for Big {
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
            Box::pin(async { ToolOutput::ok("y".repeat(10_000)) })
        }
    }

    /// A fake provider that also records each tool output it is sent.
    struct Recorder {
        inner: FakeProvider,
        requests: Mutex<Vec<Vec<Text>>>,
    }

    impl Provider for Recorder {
        fn name(&self) -> &'static str {
            self.inner.name()
        }

        fn stream(&self, request: &Request<'_>) -> BoxStream<'static, Result<StreamEvent, Error>> {
            let outputs = request
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::ToolResult(result) => Some(result.content.clone()),
                    _ => None,
                })
                .collect();
            self.requests.lock().expect("requests lock").push(outputs);
            self.inner.stream(request)
        }
    }

    fn tool_turn(index: usize) -> Vec<StreamEvent> {
        let id = Text::from(format!("call_{index}"));
        vec![
            StreamEvent::ToolCallStart {
                id: id.clone(),
                name: Text::from("big"),
            },
            StreamEvent::ToolCallDelta {
                id: id.clone(),
                partial_json: Text::from("{}"),
            },
            StreamEvent::ToolCallEnd { id },
            StreamEvent::Stop(StopReason::ToolUse),
        ]
    }

    async fn run(masked: bool) -> Vec<Vec<Text>> {
        let mut replies: Vec<_> = (0..12).map(tool_turn).collect();
        replies.push(vec![
            StreamEvent::TextDelta(Text::from("done")),
            StreamEvent::Stop(StopReason::EndTurn),
        ]);
        let provider = Recorder {
            inner: FakeProvider::new(replies),
            requests: Mutex::new(Vec::new()),
        };
        let mut registry = Registry::new();
        registry.add(Big {
            spec: ToolSpec {
                name: "big".into(),
                description: "Big output.".into(),
                input_schema: json!({"type": "object"}),
            },
        });
        let config = AgentConfig {
            model: "fake-model".into(),
            max_tokens: 128,
            system: None,
            reasoning: None,
            cache_key: None,
            max_turns: None,
        };
        let (tx, _rx) = mpsc::channel(256);
        let mut history = vec![Message::user("go")];
        let agent = Agent::new(&provider, &registry, config, CancellationToken::new());
        let mut agent = if masked { agent.with_mask() } else { agent };
        agent.run(&mut history, None, &tx).await;
        provider.requests.into_inner().expect("requests lock")
    }

    #[tokio::test]
    async fn the_request_carries_the_stub_for_old_outputs() {
        let requests = run(true).await;
        assert_eq!(requests.len(), 13);
        let last = &requests[12];
        assert_eq!(last.len(), 12);
        // The step ran at 11 outputs (8 old); the 12th is below a new step.
        assert!(last[..8].iter().all(|text| &**text == STUB));
        assert!(last[8..].iter().all(|text| text.len() == 10_000));
        // An earlier request is still whole: the step had not been reached.
        assert!(requests[5].iter().all(|text| text.len() == 10_000));
    }

    #[tokio::test]
    async fn without_masking_the_request_is_untouched() {
        let requests = run(false).await;
        assert!(requests[12].iter().all(|text| text.len() == 10_000));
    }
}
