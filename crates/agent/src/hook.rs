//! The seams a plugin changes a run through: a hook around every tool call of
//! a run, a hook that runs when a turn ends and a hook that sees the run's
//! events in loop order.
//!
//! No hook is installed by the agent itself: a registry and a loop with
//! no hook behave exactly as they did before these types existed, and a run
//! that installs one pays only that hook's own work.

use futures_util::future::BoxFuture;

use reagent_ai::Text;
use reagent_ai::Usage;

use crate::event::TurnEndReason;
use crate::policy::TurnView;
use crate::tool::ToolOutput;

/// What a [`CallHook`] decides about a tool call before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallVerdict {
    /// Run the call with its arguments as they are.
    Keep,
    /// Run the call with these arguments instead, as a JSON object in text.
    Args(String),
    /// Do not run the call; the model gets this reason as an error result.
    Block(String),
}

/// Sees every tool call of a run: before it runs, and its output after.
///
/// The batch asks `before` for the arguments a rule sees, so a verdict of
/// [`CallVerdict::Args`] is what the permission rules, the reroute, the run
/// and the schema retry all use: a hook can never get a call past the rules.
/// A call the rules or the hook itself refused never reaches `after`.
pub trait CallHook: Send + Sync {
    /// Decides whether the call runs, and with which arguments.
    ///
    /// `tool` is the tool's own name, not the model's spelling of it, and
    /// `args` the call's arguments after the tool's own repair.
    fn before<'a>(&'a self, tool: &'a str, args: &'a str) -> BoxFuture<'a, CallVerdict>;

    /// Rewrites the output of a call that ran, which is also what the journal,
    /// the loop and the model then see.
    fn after<'a>(
        &'a self,
        tool: &'a str,
        args: &'a str,
        output: ToolOutput,
    ) -> BoxFuture<'a, ToolOutput>;
}

/// Runs when a turn ends and may add a follow-up message, which starts
/// another turn.
///
/// A returned text counts as an injection, like a [`TurnPolicy`] returning
/// [`Decision::Inject`], and joins the texts of the policies after them: a
/// turn that ran no tool continues only because of it.
///
/// [`TurnPolicy`]: crate::policy::TurnPolicy
/// [`Decision::Inject`]: crate::policy::Decision::Inject
pub trait TurnHook: Send + Sync {
    /// Looks at the finished turn and returns the text to inject, if any.
    ///
    /// The view borrows the turn's messages, so it is read, not kept.
    fn turn_end<'a>(&'a self, turn: &'a TurnView<'a>) -> BoxFuture<'a, Option<Text>>;
}

/// A run event an [`EventHook`] sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen<'a> {
    /// A turn starts.
    TurnStart,
    /// A tool call has its result.
    ToolEnd {
        /// The tool's name.
        tool: &'a str,
        /// Whether the result is an error.
        is_error: bool,
    },
    /// A turn ended.
    TurnEnd(&'a TurnEndReason),
    /// A model reply finished, with the token use the reply keeps.
    ///
    /// Sent once per reply, not once per streamed usage: some providers report
    /// the counts of one reply several times, each time cumulative. A reply a
    /// retry replaced sends none, and a reply the run cut by a cancel sends one
    /// when the stream had already reported its use.
    Usage(&'a Usage),
    /// The run ended.
    RunEnd(&'a TurnEndReason),
    /// A tool call started to wait for a person.
    ///
    /// Every `Waiting` is answered by exactly one [`Seen::Waited`] with the same
    /// `kind` and `call`.
    Waiting {
        /// What the wait is for.
        kind: WaitKind,
        /// The id of the tool call that waits.
        call: &'a str,
        /// The tool's name; `ask` for a question.
        tool: &'a str,
    },
    /// The wait of a call ended.
    Waited {
        /// What the wait was for.
        kind: WaitKind,
        /// The id of the tool call that waited.
        call: &'a str,
        /// What ended the wait.
        how: WaitEnd,
    },
}

/// What a call waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitKind {
    /// An `ask` question, shown to the clients of the session.
    Question,
    /// A permission prompt.
    Approval,
}

impl WaitKind {
    /// The word a plugin payload names this kind by.
    pub fn word(self) -> &'static str {
        match self {
            WaitKind::Question => "question",
            WaitKind::Approval => "approval",
        }
    }
}

/// What ended a wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEnd {
    /// A client answered.
    Answered,
    /// A client refused, or no client was left to ask.
    Denied,
    /// The run was cancelled.
    Cancelled,
    /// The wait ran out of time; no wait has a timer yet.
    Timeout,
    /// An `ask` parked, because its last client left.
    Parked,
}

impl WaitEnd {
    /// The word a plugin payload names this end by.
    pub fn word(self) -> &'static str {
        match self {
            WaitEnd::Answered => "answered",
            WaitEnd::Denied => "denied",
            WaitEnd::Cancelled => "cancelled",
            WaitEnd::Timeout => "timeout",
            WaitEnd::Parked => "parked",
        }
    }
}

/// Sees the run's events in loop order, before they reach the event channel.
///
/// The loop does not wait for the channel's reader, so a hook is how a listener
/// sees the end of a turn before any call of the next turn runs. `seen` must
/// not block: it runs inside the loop.
pub trait EventHook: Send + Sync {
    /// Takes one event.
    fn seen(&self, event: Seen<'_>);
}
