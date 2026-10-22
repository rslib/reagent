//! A scripted provider for tests.

use std::collections::VecDeque;
use std::sync::Mutex;

use bytes::Bytes;
use futures_util::FutureExt;
use futures_util::stream::{self, BoxStream, StreamExt};

use crate::{Error, Provider, Request, StreamEvent};

/// One request the fake provider saw, without its message history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeenRequest {
    /// The model the request named.
    pub model: String,
    /// How many messages the request carried.
    pub messages: usize,
    /// The reasoning text the request carried, if any.
    pub reasoning: Option<String>,
}

/// One item a scripted reply can yield: an event or an error.
#[derive(Debug)]
enum ReplyItem {
    Event(StreamEvent),
    Error(Error),
}

/// Plays a scripted list of replies in order.
#[derive(Debug, Default)]
pub struct FakeProvider {
    replies: Mutex<VecDeque<VecDeque<ReplyItem>>>,
    seen: Mutex<Vec<SeenRequest>>,
}

impl FakeProvider {
    /// Creates a provider that plays `replies` in order.
    pub fn new(replies: Vec<Vec<StreamEvent>>) -> Self {
        let replies: VecDeque<VecDeque<ReplyItem>> = replies
            .into_iter()
            .map(|events| events.into_iter().map(ReplyItem::Event).collect())
            .collect();
        Self {
            replies: Mutex::new(replies),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Queues one more reply at the back.
    pub fn push_reply(&self, events: Vec<StreamEvent>) {
        let reply: VecDeque<ReplyItem> = events.into_iter().map(ReplyItem::Event).collect();
        self.replies.lock().expect("reply lock").push_back(reply);
    }

    /// Queues one more attempted reply that fails with `error`.
    pub fn push_error(&self, error: Error) {
        let mut reply = VecDeque::new();
        reply.push_back(ReplyItem::Error(error));
        self.replies.lock().expect("reply lock").push_back(reply);
    }

    /// Queues one more attempted reply that streams `events` and then fails
    /// with `error`, as a stream cut off mid-reply does.
    pub fn push_failing_reply(&self, events: Vec<StreamEvent>, error: Error) {
        let mut reply: VecDeque<ReplyItem> = events.into_iter().map(ReplyItem::Event).collect();
        reply.push_back(ReplyItem::Error(error));
        self.replies.lock().expect("reply lock").push_back(reply);
    }

    /// The requests seen so far, oldest first.
    pub fn seen(&self) -> Vec<SeenRequest> {
        self.seen.lock().expect("seen lock").clone()
    }
}

impl Provider for FakeProvider {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn stream(&self, request: &Request<'_>) -> BoxStream<'static, Result<StreamEvent, Error>> {
        self.seen.lock().expect("seen lock").push(SeenRequest {
            model: request.model.to_string(),
            messages: request.messages.len(),
            reasoning: request.reasoning.map(str::to_string),
        });
        let reply = self
            .replies
            .lock()
            .expect("reply lock")
            .pop_front()
            .unwrap_or_default();
        Box::pin(stream::iter(reply.into_iter().map(|item| match item {
            ReplyItem::Event(event) => Ok(event),
            ReplyItem::Error(error) => Err(error),
        })))
    }
}

/// A provider wire format for [`parse_chunks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    /// Anthropic Messages.
    Anthropic,
    /// OpenAI Responses.
    OpenAiResponses,
    /// OpenAI-compatible Chat Completions.
    OpenAiChat,
}

/// Runs `chunks` through the real stream glue and parser of `wire`, as if they
/// came from the socket, and returns every item the stream yields.
pub fn parse_chunks(wire: Wire, chunks: Vec<Bytes>) -> Vec<Result<StreamEvent, Error>> {
    let input = stream::iter(chunks.into_iter().map(Ok));
    let events = match wire {
        Wire::Anthropic => {
            crate::openai::event_stream(input, crate::anthropic::stream::StreamParser::new())
        }
        Wire::OpenAiResponses => crate::openai::event_stream(
            input,
            crate::openai::responses::stream::StreamParser::new(),
        ),
        Wire::OpenAiChat => {
            crate::openai::event_stream(input, crate::openai::chat::StreamParser::new())
        }
    };
    events
        .collect::<Vec<_>>()
        .now_or_never()
        .expect("chunks from memory are always ready")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, StopReason};

    fn request<'a>(model: &'a str, history: &'a [Message]) -> Request<'a> {
        Request {
            model,
            max_tokens: 64,
            system: None,
            tools: &[],
            messages: history,
            reasoning: Some("low"),
            cache_key: None,
        }
    }

    fn drain(stream: &mut BoxStream<'static, Result<StreamEvent, Error>>) -> Vec<StreamEvent> {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut events = Vec::new();
        while let std::task::Poll::Ready(Some(item)) = stream.as_mut().poll_next(&mut cx) {
            events.push(item.expect("the fake never fails"));
        }
        events
    }

    #[test]
    fn name_is_fake() {
        assert_eq!(FakeProvider::new(vec![]).name(), "fake");
    }

    #[test]
    fn plays_replies_in_order() {
        let provider = FakeProvider::new(vec![
            vec![
                StreamEvent::TextDelta("first".into()),
                StreamEvent::Stop(StopReason::EndTurn),
            ],
            vec![StreamEvent::TextDelta("second".into())],
        ]);
        let history: Vec<Message> = (0..2).map(|i| Message::user(format!("m{i}"))).collect();
        let request = request("fake-model", &history);

        assert_eq!(
            drain(&mut provider.stream(&request)),
            vec![
                StreamEvent::TextDelta("first".into()),
                StreamEvent::Stop(StopReason::EndTurn),
            ]
        );
        assert_eq!(
            drain(&mut provider.stream(&request)),
            vec![StreamEvent::TextDelta("second".into())]
        );
        assert_eq!(
            provider.seen(),
            vec![
                SeenRequest {
                    model: "fake-model".into(),
                    messages: 2,
                    reasoning: Some("low".into()),
                },
                SeenRequest {
                    model: "fake-model".into(),
                    messages: 2,
                    reasoning: Some("low".into()),
                },
            ]
        );
    }

    #[test]
    fn push_reply_queues_at_the_back() {
        let provider = FakeProvider::new(vec![]);
        provider.push_reply(vec![StreamEvent::TextDelta("queued".into())]);
        let history = vec![Message::user("m0")];
        assert_eq!(
            drain(&mut provider.stream(&request("m", &history))),
            vec![StreamEvent::TextDelta("queued".into())]
        );
    }

    #[test]
    fn an_exhausted_provider_yields_nothing() {
        let provider = FakeProvider::new(vec![]);
        assert!(drain(&mut provider.stream(&request("m", &[]))).is_empty());
    }

    #[test]
    fn push_error_queues_a_failing_stream() {
        let provider = FakeProvider::new(vec![]);
        provider.push_error(Error::Transient("overloaded".into()));
        let history = vec![Message::user("m0")];
        let mut stream = provider.stream(&request("m", &history));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let item = stream.as_mut().poll_next(&mut cx);
        match item {
            std::task::Poll::Ready(Some(Err(Error::Transient(message)))) => {
                assert_eq!(message, "overloaded")
            }
            other => panic!("expected a transient error, got {other:?}"),
        }
    }
}
