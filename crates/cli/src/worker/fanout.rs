//! The frame of one event: one encode, one `params` payload, every client.

use std::sync::Arc;

use reagent_agent::event::Event;
use reagent_protocol::hub;
use reagent_protocol::hub::Frame;
use reagent_protocol::message;
use reagent_protocol::message::methods;

use crate::json;

/// The line and the frame of the event being encoded, reused for every event.
#[derive(Debug, Default)]
pub(crate) struct Encoder {
    line: Vec<u8>,
    frame: Vec<u8>,
    /// The model a usage line names, as the session runs it now.
    model: Option<Arc<str>>,
    /// The reagent provider id that serves it, as the session runs it now.
    provider: Option<Arc<str>>,
}

impl Encoder {
    /// Takes the model and the provider a usage line names: the ones the
    /// session runs after a `set`, so the line that follows it names them.
    pub(crate) fn names(&mut self, provider: Option<Arc<str>>, model: Option<Arc<str>>) {
        self.model = model;
        self.provider = provider;
    }

    /// Encodes one event into one frame: its line, wrapped as the `params` of
    /// the method that carries it.
    pub(crate) fn frame(&mut self, event: &Event) -> Frame {
        self.line.clear();
        let names = json::Names {
            model: self.model.as_deref(),
            provider: self.provider.as_deref(),
        };
        json::write_worker_line(&mut self.line, event, names);
        // The frame carries the line without the newline that ends it.
        assert_eq!(
            self.line.pop(),
            Some(b'\n'),
            "an event line ends in a newline"
        );
        self.frame.clear();
        message::notify(&mut self.frame, method(event), &self.line);
        Arc::from(self.frame.as_slice())
    }
}

/// The method that carries `event`.
fn method(event: &Event) -> &'static str {
    match event {
        Event::ApprovalRequest(_) => methods::APPROVAL_REQUEST,
        Event::ApprovalResolved { .. } => methods::APPROVAL_RESOLVED,
        Event::QuestionRequest(_) => methods::QUESTION_REQUEST,
        Event::QuestionResolved { .. } => methods::QUESTION_RESOLVED,
        _ => methods::EVENT,
    }
}

/// What the frame of `event` carries, so the hub needs no agent types.
pub(crate) fn kind(event: &Event) -> hub::Kind {
    match event {
        Event::TextDelta(_) | Event::ThinkingDelta(_) => hub::Kind::Delta,
        Event::Committed { .. } => hub::Kind::Committed,
        Event::ApprovalRequest(request) => hub::Kind::ApprovalRequest(request.id),
        Event::ApprovalResolved { id, .. } => hub::Kind::ApprovalResolved(*id),
        Event::QuestionRequest(request) => hub::Kind::QuestionRequest(request.id),
        Event::QuestionResolved { id, .. } => hub::Kind::QuestionResolved(*id),
        Event::RunEnd(_) => hub::Kind::RunEnd,
        _ => hub::Kind::Other,
    }
}
