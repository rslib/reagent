//! The completion check policy.
//!
//! The first time the model ends a turn without calling a tool, it usually
//! means it thinks the task is done. Small models in particular stop early:
//! they answer before running the program or checking the tests. This policy
//! injects a short checklist the first time that happens, and ends the run on
//! the next end-turn, so a wrong "done" gets one more chance to verify.
//! When the task is known, the checklist repeats it, so the model checks its
//! work against the task and not against its own code.

use reagent_ai::Text;

use super::{Decision, TurnPolicy, TurnView};

/// The checklist template, baked in at build time from `prompts/completion.md`.
const TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/completion.md"
));

/// Injects the completion checklist once, then stops on the next end-turn.
#[derive(Debug)]
pub struct Completion {
    /// The checklist message, shared.
    text: Text,
    /// Whether the checklist has been injected this run.
    asked: bool,
}

impl Default for Completion {
    fn default() -> Self {
        Self {
            text: Text::from(TEMPLATE),
            asked: false,
        }
    }
}

impl Completion {
    /// A completion policy with the default checklist.
    pub fn new() -> Self {
        Self::default()
    }

    /// A completion policy whose checklist repeats `task` above it.
    pub fn with_task(task: &str) -> Self {
        Self {
            text: Text::from(format!("Original task:\n{task}\n\n{TEMPLATE}")),
            asked: false,
        }
    }
}

impl TurnPolicy for Completion {
    fn after_turn(&mut self, turn: &TurnView<'_>) -> Decision {
        if turn.ran_tools {
            // A tool turn is not an attempted finish; keep going quietly.
            return Decision::Continue;
        }
        if self.asked {
            // A second end-turn after the checklist: the model had its chance.
            Decision::Stop
        } else {
            self.asked = true;
            Decision::Inject(self.text.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reagent_ai::{AssistantMessage, Content, ToolCall, ToolResult};

    use super::super::{Decision, TurnView};
    use super::*;

    fn assistant() -> AssistantMessage {
        AssistantMessage::new("fake", "m", vec![Content::text("done")])
    }

    fn end_turn() -> TurnView<'static> {
        let a = assistant();
        // Leak a fresh assistant so the view outlives the test.
        let a: &'static AssistantMessage = Box::leak(Box::new(a));
        TurnView {
            index: 1,
            elapsed: Duration::from_secs(1),
            ran_tools: false,
            assistant: a,
            tools: &[],
        }
    }

    #[test]
    fn a_tool_turn_never_triggers_the_checklist() {
        let a = assistant();
        let a: &'static AssistantMessage = Box::leak(Box::new(a));
        let tools = [(
            ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
            ToolResult {
                call_id: "call_1".into(),
                content: "ok".into(),
                is_error: false,
            },
        )];
        let view = TurnView {
            index: 1,
            elapsed: Duration::from_secs(1),
            ran_tools: true,
            assistant: a,
            tools: &tools,
        };
        let mut policy = Completion::new();

        assert_eq!(policy.after_turn(&view), Decision::Continue);
        assert_eq!(policy.after_turn(&view), Decision::Continue);
    }

    #[test]
    fn the_first_end_turn_injects_the_checklist() {
        let mut policy = Completion::new();

        let decision = policy.after_turn(&end_turn());

        match decision {
            Decision::Inject(text) => {
                assert!(text.contains("original task"), "{text}");
                assert!(text.contains("tests pass"), "{text}");
                assert!(!text.starts_with("Original task:"), "{text}");
            }
            other => panic!("expected an injection, got {other:?}"),
        }
    }

    #[test]
    fn the_second_end_turn_stops_the_run() {
        let mut policy = Completion::new();
        let _ = policy.after_turn(&end_turn());

        assert_eq!(policy.after_turn(&end_turn()), Decision::Stop);
    }

    #[test]
    fn only_one_checklist_is_injected_per_run() {
        let mut policy = Completion::new();
        let injected = matches!(policy.after_turn(&end_turn()), Decision::Inject(_));

        assert!(injected);
        // Tool turns in between do not add another checklist.
        let _ = {
            let a = assistant();
            let a: &'static AssistantMessage = Box::leak(Box::new(a));
            let view = TurnView {
                index: 2,
                elapsed: Duration::from_secs(2),
                ran_tools: true,
                assistant: a,
                tools: &[],
            };
            policy.after_turn(&view)
        };
        assert_eq!(policy.after_turn(&end_turn()), Decision::Stop);
    }

    #[test]
    fn the_template_is_baked_in() {
        assert!(!TEMPLATE.is_empty());
        assert!(TEMPLATE.contains("graded"));
    }

    #[test]
    fn with_task_repeats_the_task_above_the_checklist() {
        let mut policy = Completion::with_task("build the thing");
        let Decision::Inject(text) = policy.after_turn(&end_turn()) else {
            panic!("expected the checklist");
        };
        assert!(
            text.starts_with("Original task:\nbuild the thing\n\n"),
            "{text}"
        );
        assert!(text.ends_with(TEMPLATE), "{text}");
    }
}
