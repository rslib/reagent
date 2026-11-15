//! Events the agent loop emits on its channel.

use reagent_ai::{Limits, StopReason, Text, Usage};
use reagent_config::permissions::PermissionMode;

use crate::guard::Answer;
use crate::live::{ClientId, SendMode};
use crate::question::Question;

/// Why a turn, or the whole run, ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnEndReason {
    /// The reply had no tool call and the model finished normally.
    EndTurn,
    /// The run was cancelled before it completed.
    Cancelled,
    /// A question of the run was parked: the process may end, and a later one
    /// answers it and resumes the session.
    Parked,
    /// The provider stopped without asking for a tool, and the remaining
    /// content (if any) was kept as the reply.
    Stopped(StopReason),
    /// The run reached its configured turn cap.
    MaxTurns,
    /// The provider or a tool produced an error.
    Error,
}

/// Where a message a client sent stands: in the conversation, or dropped
/// before it got there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageState {
    /// The message joined the conversation.
    Landed,
    /// The message never reached the conversation.
    Dropped,
}

/// Why a message never reached the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// A cancel cleared it while it waited.
    Cancelled,
    /// The session ended before a run carried it.
    Ended,
}

/// One thing that happened while the agent ran, for the UI or a logger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// One provider call (a turn) started.
    TurnStart,
    /// A piece of the model's visible reply.
    TextDelta(Text),
    /// A piece of the model's private reasoning.
    ThinkingDelta(Text),
    /// The model started one tool call.
    ToolStart {
        /// The call id.
        id: Text,
        /// The tool name.
        name: Text,
    },
    /// The model finished one tool call's arguments.
    ToolCallReady {
        /// The call id.
        id: Text,
        /// The tool name.
        name: Text,
        /// The raw JSON arguments the model sent, as a string.
        arguments: Text,
    },
    /// One tool call produced a result.
    ToolResult {
        /// The call id.
        id: Text,
        /// Whether the tool reported an error.
        is_error: bool,
        /// The UTF-8 length of the output text the model receives.
        bytes: usize,
        /// The output text, shared with history; `None` for a result that
        /// carries images only.
        output: Option<Text>,
        /// The images the result carries, shared with history; empty for the
        /// text results that are almost every call, which allocates nothing.
        images: Vec<reagent_ai::Image>,
    },
    /// Running token counts for this turn, with the plan limits the reply's
    /// headers named, when they named any. A call that named none carries
    /// `None`; the limits never come from an earlier call.
    Usage(Usage, Option<Box<Limits>>),
    /// One provider call ended with a reason.
    TurnEnd(TurnEndReason),
    /// The whole run ended with a reason, once, after the last `TurnEnd`.
    RunEnd(TurnEndReason),
    /// A loop policy injected a message into the conversation.
    PolicyNote(Text),
    /// A provider or tool error with its message.
    Error(Text),
    /// A failed provider call is being retried after a backoff.
    ///
    /// A consumer that sees this event must drop the partial text, thinking
    /// and tool-argument deltas of the failed turn: the retry restarts the
    /// turn from scratch, and the partial reply is discarded.
    Retry {
        /// The retry number, starting at 1.
        attempt: u32,
        /// The backoff before the retry, in milliseconds.
        delay_ms: u64,
        /// The error that caused the retry.
        error: Text,
    },
    /// A client sent a message to a live session, sent once per accepted
    /// message so every client sees the messages of the others.
    UserMessage {
        /// The id the worker gave the message, which its
        /// [`Event::MessageState`] carries too.
        message: u64,
        /// The client that sent the message.
        client: ClientId,
        /// How the message joined the session: a steer or a queue.
        mode: SendMode,
        /// The text the client sent, shared with the conversation.
        text: Text,
        /// The images that follow the text, such as a pasted one, shared with
        /// the conversation.
        images: Vec<reagent_ai::Image>,
    },
    /// A client's message entered the conversation or was dropped before it
    /// did, sent once per message and never both.
    MessageState {
        /// The id of the message, as its `user_message` event names it.
        message: u64,
        /// Where the message stands.
        state: MessageState,
        /// Why it was dropped; `None` for a message that landed.
        reason: Option<DropReason>,
    },
    /// A journal record was committed.
    Committed {
        /// The number of the record, so a client can resume its view from it.
        record: u64,
    },
    /// Older turns were replaced by a checkpoint before the next request.
    Compacted {
        /// The prompt size, in tokens, that triggered it.
        before: u64,
        /// The estimated prompt size, in tokens, after it.
        after: u64,
        /// How many messages the checkpoint replaced.
        replaced: u32,
        /// The size of the checkpoint, in bytes.
        bytes: u32,
    },
    /// A tool call waits for a client's approval before it runs. Boxed, since
    /// it is rare and would make every event larger.
    ApprovalRequest(Box<ApprovalRequest>),
    /// A client answered an approval request.
    ApprovalResolved {
        /// The id of the request.
        id: u64,
        /// The client that answered.
        client: ClientId,
        /// What it answered.
        answer: Answer,
    },
    /// The model asks the person a question and waits for the answer. Boxed,
    /// since it is rare and would make every event larger.
    QuestionRequest(Box<QuestionRequest>),
    /// A question request is done: a client answered it, or nobody did.
    QuestionResolved {
        /// The id of the request.
        id: u64,
        /// The client that answered; 0 when none did.
        client: ClientId,
    },
    /// A client's answer saves a rule to the config.
    RuleToSave {
        /// The rule to write into the `allow` list.
        rule: Text,
    },
    /// The settings the session applied, sent once after a `set` took them, so
    /// a client hears what the session kept rather than what the `set` asked
    /// for: a mode its guard refused is not here.
    Settings {
        /// The model the session now runs, when the `set` named one.
        model: Option<Text>,
        /// The reasoning level the next request sends, when the `set` named
        /// one.
        level: Option<Text>,
        /// The permission mode the session now decides with, when its guard
        /// accepted the one the `set` named.
        mode: Option<PermissionMode>,
    },
}

/// What [`Event::ApprovalRequest`] shows a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// The id of the request, which the answer names.
    pub id: u64,
    /// The tool the call names.
    pub tool: Text,
    /// One line that shows the call.
    pub call: Text,
    /// The rule that asked, or `default` when no rule matched.
    pub rule: Text,
    /// The rule a save would write, when a save is possible.
    pub save: Option<Text>,
}

/// What [`Event::QuestionRequest`] shows a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionRequest {
    /// The id of the request, which the answer names.
    pub id: u64,
    /// The questions, in the order the model asked them.
    pub questions: Vec<Question>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_stays_the_size_of_a_tool_result() {
        // Every event takes one channel slot of this size, streamed deltas
        // too; `ToolResult` sets it, and a rare event is boxed to stay under.
        assert!(size_of::<Event>() <= 72, "{} bytes", size_of::<Event>());
    }
}
