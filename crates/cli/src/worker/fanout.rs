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
}

impl Encoder {
    /// Encodes one event into one frame: its line, wrapped as the `params` of
    /// the method that carries it.
    pub(crate) fn frame(&mut self, event: &Event) -> Frame {
        self.line.clear();
        json::write_line(&mut self.line, event);
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
        Event::RunEnd(_) => hub::Kind::RunEnd,
        _ => hub::Kind::Other,
    }
}
