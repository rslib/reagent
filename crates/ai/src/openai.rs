//! OpenAI providers: the Responses API and OpenAI-compatible Chat Completions.

pub mod chat;
pub mod responses;

use crate::Error;
use crate::types::Message;

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
