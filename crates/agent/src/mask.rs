//! Observation masking: old large tool outputs become a stub that names the
//! call id, and the archive keeps the output for `expand`.

use reagent_ai::{Message, Text, ToolResult};

use crate::archive::Archive;

/// The stub that replaces a masked output, baked in from `prompts/mask_stub.md`;
/// `{id}` stands for the call id.
const STUB: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/mask_stub.md"
));

/// The stub for call `id`, the same bytes each time so the cache holds.
/// It is assembled on the stack, so the only allocation is the `Text`.
fn stub(id: Option<u32>) -> Text {
    let mut digits = [0_u8; 10];
    let id = match id {
        Some(id) => decimal(id, &mut digits),
        None => "?",
    };
    let mut buf = [0_u8; 256];
    let mut len = 0;
    for (index, part) in STUB.trim_end().split("{id}").enumerate() {
        for piece in [if index > 0 { id } else { "" }, part] {
            let end = len + piece.len();
            if end > buf.len() {
                return Text::from(STUB.trim_end().replace("{id}", id));
            }
            buf[len..end].copy_from_slice(piece.as_bytes());
            len = end;
        }
    }
    Text::from(std::str::from_utf8(&buf[..len]).unwrap_or("?"))
}

/// The decimal digits of `n`, written into `buf`.
fn decimal(mut n: u32, buf: &mut [u8; 10]) -> &str {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[at..]).unwrap_or("?")
}

/// Whether `text` is a stub this module wrote.
fn is_stub(text: &str) -> bool {
    let head = STUB.split("{id}").next().unwrap_or(STUB);
    text.starts_with(head)
}

/// How many of the newest tool outputs are never masked.
const KEEP: usize = 3;

/// Outputs under this many bytes are never masked and never counted.
const MIN_BYTES: usize = 800;

/// The bytes of old unmasked outputs that trigger a masking step. A small
/// step hides code the model still needs, and it reads the code again.
const STEP_BYTES: usize = 200_000;

/// The most bytes of old outputs that a step leaves unmasked.
const LEFT_BYTES: usize = STEP_BYTES / 2;

/// Replaces old large tool outputs in the history with a stub, in steps.
///
/// Below the step size nothing changes, so the prompt prefix stays stable
/// between steps and the provider cache keeps hitting.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mask;

impl Mask {
    /// Masks the oldest outputs older than the newest three when the
    /// unmasked ones of at least 800 bytes add up to 200,000 bytes or more,
    /// until at most 100,000 bytes of them stay unmasked.
    ///
    /// Each masked output is kept in `archive` under its call id. Returns
    /// how many outputs it replaced.
    pub fn apply(&self, history: &mut [Message], archive: &Archive) -> usize {
        let mut total = 0;
        for result in old_results(history) {
            if is_candidate(result) {
                total += result.content.len();
            }
        }
        if total < STEP_BYTES {
            return 0;
        }
        let mut masked = 0;
        for result in old_results(history).collect::<Vec<_>>().into_iter().rev() {
            if total <= LEFT_BYTES {
                break;
            }
            if is_candidate(result) {
                total -= result.content.len();
                let id = archive.id_of(&result.call_id);
                if let Some(id) = id {
                    archive.keep(id, Text::clone(&result.content));
                }
                result.content = stub(id);
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
    result.content.len() >= MIN_BYTES && !is_stub(&result.content)
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

    /// Numbers the history's calls in order, as the loop does, then masks.
    fn apply(messages: &mut [Message]) -> usize {
        let archive = numbered(messages);
        Mask.apply(messages, &archive)
    }

    fn numbered(messages: &[Message]) -> Archive {
        let archive = Archive::default();
        for message in messages {
            if let Message::ToolResult(result) = message {
                archive.assign(&result.call_id);
            }
        }
        archive
    }

    #[test]
    fn stubs_name_the_call_id_and_the_archive_keeps_the_output() {
        let mut messages = history(&[5_000; 43]);
        let archive = numbered(&messages);
        assert_eq!(Mask.apply(&mut messages, &archive), 20);
        assert_eq!(
            text(&messages[0]),
            "[Old tool output #1 removed to save context. Call expand with id 1 to see it.]"
        );
        assert_eq!(
            text(&messages[19]),
            "[Old tool output #20 removed to save context. Call expand with id 20 to see it.]"
        );
        assert_eq!(archive.kept_len(20), Some(5_000));
        assert_eq!(archive.kept_len(21), None);
    }

    fn masked_flags(history: &[Message]) -> Vec<bool> {
        history.iter().map(|m| is_stub(text(m))).collect()
    }

    #[test]
    fn the_newest_turn_is_never_masked() {
        let mut messages = history(&[5_000; 40]);
        messages.push(Message::assistant(reagent_ai::AssistantMessage::new(
            "fake",
            "fake",
            Vec::new(),
        )));
        messages.extend((40..50).map(|index| output(index, 5_000)));
        assert_eq!(apply(&mut messages), 20);
        assert!(messages[..20].iter().all(|m| is_stub(text(m))));
        assert!(messages[20..40].iter().all(|m| text(m).len() == 5_000));
        assert!(messages[41..].iter().all(|m| text(m).len() == 5_000));
    }

    #[test]
    fn below_the_step_nothing_is_masked() {
        let mut messages = history(&[5_000; 42]);
        assert_eq!(apply(&mut messages), 0);
        assert_eq!(masked_flags(&messages), vec![false; 42]);
    }

    #[test]
    fn masks_the_oldest_outputs_down_to_half_the_step() {
        let mut messages = history(&[5_000; 43]);
        assert_eq!(apply(&mut messages), 20);
        let mut expected = vec![true; 20];
        expected.extend([false; 23]);
        assert_eq!(masked_flags(&messages), expected);
        assert!(messages[20..].iter().all(|m| text(m).len() == 5_000));
    }

    #[test]
    fn a_small_old_output_is_never_masked() {
        let mut sizes = vec![5_000; 44];
        sizes[4] = 500;
        let mut messages = history(&sizes);
        assert_eq!(apply(&mut messages), 20);
        let mut expected = vec![true; 21];
        expected[4] = false;
        expected.extend([false; 23]);
        assert_eq!(masked_flags(&messages), expected);
        assert_eq!(text(&messages[4]).len(), 500);
    }

    #[test]
    fn small_outputs_do_not_count_toward_the_step() {
        let mut messages = history(&[799; 400]);
        assert_eq!(apply(&mut messages), 0);
    }

    #[test]
    fn newest_three_count_all_sizes_and_errors_are_masked() {
        let mut messages = history(&[5_000; 40]);
        messages.extend(history(&[10, 10, 10]));
        if let Message::ToolResult(result) = &mut messages[0] {
            result.is_error = true;
        }
        assert_eq!(apply(&mut messages), 20);
        assert!(is_stub(text(&messages[0])));
    }

    #[test]
    fn the_next_step_waits_for_another_hundred_thousand_bytes() {
        let mut messages = history(&[5_000; 43]);
        assert_eq!(apply(&mut messages), 20);

        // 20 old unmasked outputs, then 19 more: 195,000 bytes, no step.
        messages.extend(history(&[5_000; 19]));
        assert_eq!(apply(&mut messages), 0);

        // One more makes 200,000: the 20 oldest unmasked go, stubs not recounted.
        messages.extend(history(&[5_000; 1]));
        assert_eq!(apply(&mut messages), 20);
        let mut expected = vec![true; 40];
        expected.extend([false; 23]);
        assert_eq!(masked_flags(&messages), expected);
        assert_eq!(apply(&mut messages), 0);
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
            Box::pin(async { ToolOutput::ok("y".repeat(5_000)) })
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
        let mut replies: Vec<_> = (0..45).map(tool_turn).collect();
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
        let (tx, _rx) = mpsc::channel(4096);
        let mut history = vec![Message::user("go")];
        let agent = Agent::new(&provider, &registry, config, CancellationToken::new());
        let mut agent = if masked { agent.with_mask() } else { agent };
        agent.run(&mut history, None, &tx).await;
        provider.requests.into_inner().expect("requests lock")
    }

    #[tokio::test]
    async fn the_request_carries_the_stub_for_old_outputs() {
        let requests = run(true).await;
        assert_eq!(requests.len(), 46);
        let last = &requests[45];
        assert_eq!(last.len(), 45);
        // The step ran at 43 outputs (40 old) and masked the oldest 20.
        assert!(last[..20].iter().all(|text| is_stub(text)));
        assert!(last[20..].iter().all(|text| text.len() == 5_000));
        // An earlier request is still whole: the step had not been reached.
        assert!(requests[42].iter().all(|text| text.len() == 5_000));
    }

    #[tokio::test]
    async fn without_masking_the_request_is_untouched() {
        let requests = run(false).await;
        assert!(requests[45].iter().all(|text| text.len() == 5_000));
    }
}
