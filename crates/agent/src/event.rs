//! Events the agent loop emits on its channel.

use reagent_ai::{StopReason, Text, Usage};

/// Why a turn, or the whole run, ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnEndReason {
    /// The reply had no tool call and the model finished normally.
    EndTurn,
    /// The run was cancelled before it completed.
    Cancelled,
    /// The provider stopped without asking for a tool, and the remaining
    /// content (if any) was kept as the reply.
    Stopped(StopReason),
    /// The run reached its configured turn cap.
    MaxTurns,
    /// The provider or a tool produced an error.
    Error,
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
    },
    /// Running token counts for this turn.
    Usage(Usage),
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
}
