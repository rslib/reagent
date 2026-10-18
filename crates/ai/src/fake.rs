//! A scripted provider for tests.

use std::collections::VecDeque;
use std::sync::Mutex;

use futures_util::stream::{self, BoxStream};

use crate::{Error, Provider, Request, StreamEvent};

/// One request the fake provider saw, without its message history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeenRequest {
    /// The model the request named.
    pub model: String,
    /// How many messages the request carried.
    pub messages: usize,
}

/// Plays a scripted list of replies in order.
#[derive(Debug, Default)]
pub struct FakeProvider {
    replies: Mutex<VecDeque<Vec<StreamEvent>>>,
    seen: Mutex<Vec<SeenRequest>>,
}

impl FakeProvider {
    /// Creates a provider that plays `replies` in order.
    pub fn new(replies: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Queues one more reply at the back.
    pub fn push_reply(&self, events: Vec<StreamEvent>) {
        self.replies.lock().expect("reply lock").push_back(events);
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

    fn stream(&self, request: &Request) -> BoxStream<'static, Result<StreamEvent, Error>> {
        self.seen.lock().expect("seen lock").push(SeenRequest {
            model: request.model.clone(),
            messages: request.messages.len(),
        });
        let reply = self
            .replies
            .lock()
            .expect("reply lock")
            .pop_front()
            .unwrap_or_default();
        Box::pin(stream::iter(reply.into_iter().map(Ok)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, ReasoningLevel, StopReason};

    fn request(model: &str, messages: usize) -> Request {
        Request {
            model: model.into(),
            max_tokens: 64,
            system: None,
            tools: Vec::new(),
            messages: (0..messages)
                .map(|i| Message::user(format!("m{i}")))
                .collect(),
            reasoning: Some(ReasoningLevel::Low),
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
        let request = request("fake-model", 2);

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
                },
                SeenRequest {
                    model: "fake-model".into(),
                    messages: 2,
                },
            ]
        );
    }

    #[test]
    fn push_reply_queues_at_the_back() {
        let provider = FakeProvider::new(vec![]);
        provider.push_reply(vec![StreamEvent::TextDelta("queued".into())]);
        assert_eq!(
            drain(&mut provider.stream(&request("m", 1))),
            vec![StreamEvent::TextDelta("queued".into())]
        );
    }

    #[test]
    fn an_exhausted_provider_yields_nothing() {
        let provider = FakeProvider::new(vec![]);
        assert!(drain(&mut provider.stream(&request("m", 0))).is_empty());
    }
}
