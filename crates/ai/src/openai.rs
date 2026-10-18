//! OpenAI providers: the Responses API and OpenAI-compatible Chat Completions.

pub mod chat;
pub mod responses;

use std::collections::VecDeque;
use std::pin::Pin;

use bytes::Bytes;
use futures_util::stream::{BoxStream, Stream, StreamExt};

use crate::Error;
use crate::StreamEvent;
use crate::sse::SseSplitter;
use crate::types::Message;

/// One provider's line parser, driven by [`event_stream`].
///
/// A parser hands its events to a buffer the caller owns and reuses, so a
/// stream line allocates no place of its own.
pub(crate) trait EventParser: Send {
    /// Feeds one SSE line and appends the events it carries to `out`.
    fn push_line(&mut self, line: &[u8], out: &mut Vec<StreamEvent>) -> Result<(), Error>;

    /// Marks the parser failed after an error from outside it, such as HTTP.
    fn fail(&mut self);

    /// Appends the events left after the byte stream ended.
    fn finish(&mut self, out: &mut Vec<StreamEvent>) -> Result<(), Error>;
}

/// One JSON string from a provider line.
///
/// The text borrows from the line when the line carried it verbatim and is
/// unescaped into its own text when it did not. serde's `Cow` cannot do this:
/// its `Cow` impl always copies, so a text delta would allocate twice.
pub(crate) enum JsonStr<'a> {
    /// The JSON string had no escape, so it borrows from the line.
    Borrowed(&'a str),
    /// The JSON string had an escape, so it was unescaped into its own text.
    Owned(String),
}

impl JsonStr<'_> {
    /// The string itself.
    pub(crate) fn as_str(&self) -> &str {
        match self {
            JsonStr::Borrowed(text) => text,
            JsonStr::Owned(text) => text,
        }
    }
}

impl Default for JsonStr<'_> {
    fn default() -> Self {
        JsonStr::Borrowed("")
    }
}

impl<'de: 'a, 'a> serde::Deserialize<'de> for JsonStr<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct JsonStrVisitor;

        impl<'de> serde::de::Visitor<'de> for JsonStrVisitor {
            type Value = JsonStr<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_borrowed_str<E>(self, text: &'de str) -> Result<Self::Value, E> {
                Ok(JsonStr::Borrowed(text))
            }

            fn visit_str<E>(self, text: &str) -> Result<Self::Value, E> {
                Ok(JsonStr::Owned(text.to_owned()))
            }
        }

        deserializer.deserialize_str(JsonStrVisitor)
    }
}

/// The owned state of one [`event_stream`].
struct StreamState<S, P> {
    connect: Pin<Box<S>>,
    splitter: SseSplitter,
    parser: P,
    /// Events parsed but not yet yielded, and the one error a stream may carry.
    pending: VecDeque<Result<StreamEvent, Error>>,
    /// A reused buffer for the events of one chunk, or of the tail.
    buffer: Vec<StreamEvent>,
    /// True once the stream and its tail are done, or after an error.
    finished: bool,
}

/// Turns one provider's response bytes into events.
///
/// The splitter, the parser and the event buffers are owned by the `unfold`
/// state, so no chunk allocates a line buffer and no lock guards the parser.
/// A stream carries at most one error: after it the stream ends, so `finish`
/// is not called and a transport failure is not followed by a second error.
pub(crate) fn event_stream<S, P>(
    connect: S,
    parser: P,
) -> BoxStream<'static, Result<StreamEvent, Error>>
where
    S: Stream<Item = Result<Bytes, Error>> + Send + 'static,
    P: EventParser + 'static,
{
    let state = StreamState {
        connect: Box::pin(connect),
        splitter: SseSplitter::new(),
        parser,
        pending: VecDeque::new(),
        buffer: Vec::new(),
        finished: false,
    };
    Box::pin(futures_util::stream::unfold(
        state,
        |mut state: StreamState<S, P>| async move {
            loop {
                if let Some(item) = state.pending.pop_front() {
                    return Some((item, state));
                }
                if state.finished {
                    return None;
                }
                match state.connect.next().await {
                    None => {
                        state.finished = true;
                        let StreamState {
                            splitter,
                            parser,
                            buffer,
                            pending,
                            ..
                        } = &mut state;
                        let result = match splitter.finish() {
                            Some(line) => parser.push_line(line, buffer),
                            None => Ok(()),
                        }
                        .and_then(|()| parser.finish(buffer));
                        pending.extend(buffer.drain(..).map(Ok));
                        if let Err(error) = result {
                            pending.push_back(Err(error));
                        }
                    }
                    Some(Err(error)) => {
                        state.parser.fail();
                        state.finished = true;
                        state.pending.push_back(Err(error));
                    }
                    Some(Ok(chunk)) => {
                        let mut failure = None;
                        {
                            let StreamState {
                                splitter,
                                parser,
                                buffer,
                                ..
                            } = &mut state;
                            let mut lines = splitter.push(&chunk);
                            while let Some(line) = lines.next_line() {
                                if let Err(error) = parser.push_line(line, buffer) {
                                    failure = Some(error);
                                    break;
                                }
                            }
                        }
                        if failure.is_some() {
                            state.finished = true;
                        }
                        state.pending.extend(state.buffer.drain(..).map(Ok));
                        if let Some(error) = failure {
                            state.pending.push_back(Err(error));
                        }
                    }
                }
            }
        },
    ))
}

/// Caches serialized message bodies so a request only writes new messages.
///
/// Each cached body is keyed on the message it was built from, so a compacted
/// history, an edited message or a second session on the same provider cannot
/// reuse stale bytes.
#[derive(Debug)]
pub struct BodyCache<T> {
    messages: Vec<Message>,
    bodies: Vec<T>,
}

impl<T> Default for BodyCache<T> {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            bodies: Vec::new(),
        }
    }
}

impl<T> BodyCache<T> {
    /// Rebuilds the bodies whose message changed and reuses the rest.
    pub fn align(
        &mut self,
        messages: &[Message],
        mut build: impl FnMut(&Message) -> Result<T, Error>,
    ) -> Result<(), Error> {
        self.messages.truncate(messages.len());
        self.bodies.truncate(messages.len());
        for (index, message) in messages.iter().enumerate() {
            if self.messages.get(index) == Some(message) {
                continue;
            }
            self.messages.truncate(index);
            self.bodies.truncate(index);
            self.bodies.push(build(message)?);
            self.messages.push(message.clone());
        }
        Ok(())
    }

    /// The bodies in request order.
    pub fn bodies(&self) -> &[T] {
        &self.bodies
    }
}

#[cfg(test)]
mod tests {
    use futures_util::FutureExt;

    use super::*;

    #[derive(serde::Deserialize)]
    struct Line<'a> {
        #[serde(borrow)]
        text: Option<JsonStr<'a>>,
    }

    fn text(json: &str) -> (bool, String) {
        let line: Line<'_> = serde_json::from_str(json).expect("line parses");
        let text = line.text.expect("text is present");
        (
            matches!(text, JsonStr::Borrowed(_)),
            text.as_str().to_owned(),
        )
    }

    #[test]
    fn json_str_borrows_plain_text() {
        assert_eq!(
            text(r#"{"text":"plain text"}"#),
            (true, "plain text".to_owned())
        );
        assert_eq!(
            text("{\"text\":\"caf\u{e9}\"}"),
            (true, "caf\u{e9}".to_owned())
        );
    }

    #[test]
    fn json_str_unescapes_into_owned_text() {
        assert_eq!(
            text(r#"{"text":"a\nb\"c\\d\/e"}"#),
            (false, "a\nb\"c\\d/e".to_owned())
        );
        assert_eq!(text(r#"{"text":"\u00e9"}"#), (false, "\u{e9}".to_owned()));
        assert_eq!(
            text(r#"{"text":"\ud83d\ude00"}"#),
            (false, "\u{1f600}".to_owned())
        );
    }

    #[test]
    fn json_str_rejects_bad_input() {
        let null: Line<'_> = serde_json::from_str(r#"{"text":null}"#).expect("null parses");
        assert!(null.text.is_none());
        assert!(serde_json::from_str::<Line<'_>>(r#"{"text":"\q"}"#).is_err());
        assert!(serde_json::from_str::<Line<'_>>(r#"{"text":"\ud83d"}"#).is_err());
        assert!(serde_json::from_str::<Line<'_>>(r#"{"text":7}"#).is_err());
    }

    #[test]
    fn a_last_line_without_a_newline_is_parsed() {
        let chunk = Bytes::from_static(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]",
        );
        let items: Vec<_> = event_stream(
            futures_util::stream::iter([Ok(chunk)]),
            chat::StreamParser::new(),
        )
        .collect::<Vec<_>>()
        .now_or_never()
        .expect("the stream is ready");
        assert!(items.iter().all(Result::is_ok), "{items:?}");
        assert!(
            items
                .iter()
                .any(|item| matches!(item, Ok(StreamEvent::TextDelta(text)) if &**text == "hi"))
        );
    }
}
