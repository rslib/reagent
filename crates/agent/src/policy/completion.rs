//! The completion check policy.
//!
//! Small models often end a run right after they change files, before they
//! run the program or the tests. When the model ends a turn without a tool
//! call and files changed with no check after the last change, this policy
//! injects a short note once. A run that changed nothing, or checked its
//! change, ends without a note.

use reagent_ai::Text;

use super::{Decision, TurnPolicy, TurnView};

/// The note, baked in at build time from `prompts/completion.md`.
const TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/completion.md"
));

/// Injects the check note once, after the model ends with unchecked changes.
#[derive(Debug)]
pub struct Completion {
    /// The note message, shared.
    text: Text,
    /// Whether files changed with no check after the last change.
    dirty: bool,
    /// Whether the note has been injected this run.
    asked: bool,
}

impl Default for Completion {
    fn default() -> Self {
        Self {
            text: Text::from(TEMPLATE),
            dirty: false,
            asked: false,
        }
    }
}

impl Completion {
    /// A completion policy with the default note.
    pub fn new() -> Self {
        Self::default()
    }
}

impl TurnPolicy for Completion {
    fn after_turn(&mut self, turn: &TurnView<'_>) -> Decision {
        if turn.ran_tools {
            let ran = |name: &str| turn.tools.iter().any(|(call, _)| &*call.name == name);
            // A write or edit counts even when the fingerprint saw nothing: the
            // folder walk skips build and cache folders the model may write to.
            if turn.changed_files == Some(true) || ran("write") || ran("edit") {
                self.dirty = true;
            } else if ran("bash") {
                self.dirty = false;
            }
            return Decision::Continue;
        }
        if self.dirty && !self.asked {
            self.asked = true;
            return Decision::Inject(self.text.clone());
        }
        Decision::Continue
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reagent_ai::{AssistantMessage, Content, ToolCall, ToolResult};

    use super::super::{Decision, TurnView};
    use super::*;

    fn tool_turn(names: &[&str], changed_files: Option<bool>) -> TurnView<'static> {
        let a: &'static AssistantMessage = Box::leak(Box::new(AssistantMessage::new(
            "fake",
            "m",
            vec![Content::text("working")],
        )));
        let tools: Vec<(ToolCall, ToolResult)> = names
            .iter()
            .map(|name| {
                (
                    ToolCall {
                        id: "call_1".into(),
                        name: (*name).into(),
                        arguments: "{}".into(),
                    },
                    ToolResult {
                        call_id: "call_1".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                )
            })
            .collect();
        let tools: &'static [(ToolCall, ToolResult)] = Box::leak(tools.into_boxed_slice());
        TurnView {
            index: 1,
            elapsed: Duration::from_secs(1),
            ran_tools: true,
            assistant: a,
            tools,
            changed_files,
        }
    }

    fn end_turn() -> TurnView<'static> {
        let a: &'static AssistantMessage = Box::leak(Box::new(AssistantMessage::new(
            "fake",
            "m",
            vec![Content::text("done")],
        )));
        TurnView {
            index: 1,
            elapsed: Duration::from_secs(1),
            ran_tools: false,
            assistant: a,
            tools: &[],
            changed_files: Some(false),
        }
    }

    #[test]
    fn changed_then_end_injects_once() {
        let mut policy = Completion::new();
        assert_eq!(
            policy.after_turn(&tool_turn(&["edit"], Some(true))),
            Decision::Continue
        );

        match policy.after_turn(&end_turn()) {
            Decision::Inject(text) => assert_eq!(&*text, TEMPLATE),
            other => panic!("expected an injection, got {other:?}"),
        }
        assert_eq!(policy.after_turn(&end_turn()), Decision::Continue);
    }

    #[test]
    fn changed_then_bash_check_then_end_does_not_inject() {
        let mut policy = Completion::new();
        let _ = policy.after_turn(&tool_turn(&["edit"], Some(true)));
        let _ = policy.after_turn(&tool_turn(&["bash"], Some(false)));

        assert_eq!(policy.after_turn(&end_turn()), Decision::Continue);
    }

    #[test]
    fn nothing_changed_does_not_inject() {
        let mut policy = Completion::new();
        let _ = policy.after_turn(&tool_turn(&["read"], Some(false)));

        assert_eq!(policy.after_turn(&end_turn()), Decision::Continue);
    }

    #[test]
    fn once_per_run() {
        let mut policy = Completion::new();
        let _ = policy.after_turn(&tool_turn(&["write"], Some(true)));
        assert!(matches!(
            policy.after_turn(&end_turn()),
            Decision::Inject(_)
        ));

        let _ = policy.after_turn(&tool_turn(&["edit"], Some(true)));

        assert_eq!(policy.after_turn(&end_turn()), Decision::Continue);
    }

    #[test]
    fn bash_write_counts_as_a_change() {
        let mut policy = Completion::new();
        let _ = policy.after_turn(&tool_turn(&["bash"], Some(true)));

        assert!(matches!(
            policy.after_turn(&end_turn()),
            Decision::Inject(_)
        ));
    }

    #[test]
    fn a_turn_that_changed_files_and_ran_bash_stays_dirty() {
        let mut policy = Completion::new();
        let _ = policy.after_turn(&tool_turn(&["edit", "bash"], Some(true)));

        assert!(matches!(
            policy.after_turn(&end_turn()),
            Decision::Inject(_)
        ));
    }

    #[test]
    fn fallback_without_workspace_uses_tool_names() {
        let mut policy = Completion::new();
        let _ = policy.after_turn(&tool_turn(&["edit"], None));
        let _ = policy.after_turn(&tool_turn(&["bash"], None));
        assert_eq!(policy.after_turn(&end_turn()), Decision::Continue);

        let mut policy = Completion::new();
        let _ = policy.after_turn(&tool_turn(&["bash"], None));
        let _ = policy.after_turn(&tool_turn(&["write"], None));
        assert!(matches!(
            policy.after_turn(&end_turn()),
            Decision::Inject(_)
        ));
    }

    #[test]
    fn a_write_the_fingerprint_missed_still_counts() {
        let mut policy = Completion::new();
        let _ = policy.after_turn(&tool_turn(&["write"], Some(false)));
        assert!(matches!(
            policy.after_turn(&end_turn()),
            Decision::Inject(_)
        ));
    }
}
