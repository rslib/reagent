//! The seams a plugin changes a run through: a hook around every tool call of
//! a run, and a hook that runs when a turn ends.
//!
//! Neither hook is installed by the agent itself: a registry and a loop with
//! no hook behave exactly as they did before these types existed, and a run
//! that installs one pays only that hook's own work.

use futures_util::future::BoxFuture;

use reagent_ai::Text;

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
