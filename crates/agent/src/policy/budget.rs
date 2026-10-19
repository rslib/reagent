//! The time- and turn-budget policy.
//!
//! When a run has a deadline (`--deadline`) and/or a turn cap, a long run can
//! run out of both before the answer is reached. This policy watches the
//! budget that is used up faster and, at 50%, 75% and 90% of it, injects one
//! note that says how much of that budget is used and left. At 75% the note
//! also says to start verifying, and at 90% to save the best working
//! solution and finish.

use std::time::Duration;

use reagent_ai::Text;

use super::{Decision, TurnPolicy, TurnView};

/// The advice at 75%, baked in at build time from `prompts/budget_verify.md`.
const VERIFY: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/budget_verify.md"
));

/// The advice at 90%, baked in at build time from `prompts/budget_finish.md`.
const FINISH: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/budget_finish.md"
));

/// The advice for each threshold; the 50% note has none.
const ADVICE: [&str; 3] = ["", VERIFY, FINISH];

/// The fractions of the used-up budget that get a note, in ascending order.
const THRESHOLDS: [f64; 3] = [0.5, 0.75, 0.9];

/// Watches the deadline and the turn cap and injects budget notes.
#[derive(Debug)]
pub struct Budget {
    /// The whole deadline, if one was set.
    deadline: Option<Duration>,
    /// The whole turn cap, if one was set.
    max_turns: Option<u32>,
    /// Whether each threshold's note has already fired.
    fired: [bool; THRESHOLDS.len()],
}

impl Budget {
    /// A budget policy with the given deadline (from run start) and turn cap.
    pub fn new(deadline: Option<Duration>, max_turns: Option<u32>) -> Self {
        Self {
            deadline,
            max_turns,
            fired: [false; THRESHOLDS.len()],
        }
    }

    /// No-op unless a budget is set.
    fn used(&self, turn: &TurnView<'_>) -> Option<f64> {
        let time = self
            .deadline
            .map(|d| turn.elapsed.as_secs_f64() / d.as_secs_f64());
        let turns = self.max_turns.map(|cap| turn.index as f64 / cap as f64);
        match (time, turns) {
            (None, None) => None,
            (Some(t), None) => Some(t),
            (None, Some(n)) => Some(n),
            (Some(t), Some(n)) => Some(t.max(n)),
        }
    }

    /// Builds the note for the budget used up more, saying what is left.
    fn detail(&self, turn: &TurnView<'_>) -> String {
        let time_used = self
            .deadline
            .map(|d| turn.elapsed.as_secs_f64() / d.as_secs_f64())
            .unwrap_or(0.0);
        let turn_used = self
            .max_turns
            .map(|cap| turn.index as f64 / cap as f64)
            .unwrap_or(0.0);
        let use_time = match (self.deadline, self.max_turns) {
            (Some(_), Some(_)) => time_used >= turn_used,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => unreachable!("used() already returned None"),
        };
        if use_time {
            let total = self.deadline.expect("some deadline").as_secs();
            let elapsed = turn.elapsed.as_secs();
            let left = total.saturating_sub(elapsed);
            let used = format!("Used {} of {} min.", elapsed / 60, total.div_ceil(60));
            if left < 60 {
                format!("{used} Less than a minute left.")
            } else {
                format!("{used} About {} min left.", left.div_ceil(60))
            }
        } else {
            let cap = self.max_turns.expect("some cap");
            let remaining = cap.saturating_sub(turn.index);
            format!(
                "Used {} of {cap} turns. {remaining} turns left.",
                turn.index.min(cap)
            )
        }
    }
}

impl TurnPolicy for Budget {
    fn after_turn(&mut self, turn: &TurnView<'_>) -> Decision {
        // A no-tool turn is the final answer; a note there would force one
        // more turn. The note waits for the next tool turn instead.
        if !turn.ran_tools {
            return Decision::Continue;
        }
        let Some(used) = self.used(turn) else {
            return Decision::Continue;
        };
        // When several thresholds are crossed at once, only the highest one
        // speaks; its advice covers the lower ones.
        let mut crossed = None;
        for (index, threshold) in THRESHOLDS.iter().enumerate() {
            if !self.fired[index] && used >= *threshold {
                self.fired[index] = true;
                crossed = Some(index);
            }
        }
        let Some(index) = crossed else {
            return Decision::Continue;
        };
        let detail = self.detail(turn);
        let advice = ADVICE[index].trim_end();
        if advice.is_empty() {
            Decision::Inject(Text::from(detail))
        } else {
            Decision::Inject(Text::from(format!("{detail} {advice}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reagent_ai::{AssistantMessage, Content};

    use super::super::{Decision, TurnView};
    use super::*;

    fn assistant() -> AssistantMessage {
        AssistantMessage::new("fake", "m", vec![Content::text("x")])
    }

    /// A tool-turn view for a turn at a given index and elapsed time.
    fn view(index: u32, elapsed_secs: u64) -> TurnView<'static> {
        let a: &'static AssistantMessage = Box::leak(Box::new(assistant()));
        TurnView {
            index,
            elapsed: Duration::from_secs(elapsed_secs),
            ran_tools: true,
            assistant: a,
            tools: &[],
        }
    }

    #[test]
    fn a_no_tool_turn_never_injects_and_the_note_waits() {
        let mut policy = Budget::new(None, Some(10));
        let mut answer = view(6, 0);
        answer.ran_tools = false;
        assert_eq!(policy.after_turn(&answer), Decision::Continue);
        assert!(matches!(
            policy.after_turn(&view(7, 0)),
            Decision::Inject(_)
        ));
    }

    #[test]
    fn no_budget_never_injects() {
        let mut policy = Budget::new(None, None);
        for n in 0..10 {
            assert_eq!(
                policy.after_turn(&view(n + 1, n as u64 * 10)),
                Decision::Continue
            );
        }
    }

    #[test]
    fn a_turn_cap_notes_at_50_75_and_90_percent() {
        // 8-turn cap: used crosses 0.5 at turn 4, 0.75 at turn 6, 0.9 at turn 8.
        let mut policy = Budget::new(None, Some(8));
        assert_eq!(policy.after_turn(&view(1, 0)), Decision::Continue);
        assert_eq!(policy.after_turn(&view(3, 0)), Decision::Continue);
        let Decision::Inject(at_half) = policy.after_turn(&view(4, 0)) else {
            panic!("a note at 50%");
        };
        assert!(at_half.contains("4 turns left"), "{at_half}");
        assert!(at_half.contains("Used 4 of 8 turns."), "{at_half}");
        assert!(!at_half.contains(VERIFY.trim_end()), "{at_half}");
        assert_eq!(policy.after_turn(&view(5, 0)), Decision::Continue);
        let Decision::Inject(at_three_quarters) = policy.after_turn(&view(6, 0)) else {
            panic!("a note at 75%");
        };
        assert!(
            at_three_quarters.contains("2 turns left")
                && at_three_quarters.ends_with(VERIFY.trim_end()),
            "{at_three_quarters}"
        );
        assert_eq!(policy.after_turn(&view(7, 0)), Decision::Continue);
        let Decision::Inject(at_ninety) = policy.after_turn(&view(8, 0)) else {
            panic!("a note at 90%");
        };
        assert!(at_ninety.contains("0 turns left"), "{at_ninety}");
        assert!(at_ninety.ends_with(FINISH.trim_end()), "{at_ninety}");
        assert_eq!(
            policy.after_turn(&view(8, 0)),
            Decision::Continue,
            "once only"
        );
    }

    #[test]
    fn a_deadline_notes_the_time_left_in_minutes() {
        // 600 s deadline: 50% at 300 s (5 min left), 75% at 450 s (3 min left).
        let mut policy = Budget::new(Some(Duration::from_secs(600)), None);
        let Decision::Inject(at_half) = policy.after_turn(&view(2, 300)) else {
            panic!("a note at 50% of the deadline");
        };
        assert!(at_half.contains("About 5 min left"), "{at_half}");
        let Decision::Inject(at_three_quarters) = policy.after_turn(&view(3, 450)) else {
            panic!("a note at 75% of the deadline");
        };
        assert!(
            at_three_quarters.contains("About 3 min left"),
            "{at_three_quarters}"
        );
        let _ = policy.after_turn(&view(4, 600));
    }

    #[test]
    fn the_more_used_up_budget_drives_the_note() {
        // Deadline used much faster than the turn cap, so time drives the note.
        let mut policy = Budget::new(Some(Duration::from_secs(1000)), Some(1000));
        // Turn 10: 1% of turns, but 50% of the 1000 s deadline at 500 s elapsed.
        let Decision::Inject(note) = policy.after_turn(&view(10, 500)) else {
            panic!("time is the faster budget");
        };
        assert!(note.contains("About"), "{note}");
        assert!(note.contains("min left"), "{note}");
    }

    #[test]
    fn crossing_multiple_thresholds_in_one_turn_fires_the_highest() {
        // A 10 s deadline jumps from 10% to 95% in one turn: 50, 75 and 90 all
        // cross. The highest note is returned.
        let mut policy = Budget::new(Some(Duration::from_secs(10)), None);
        assert_eq!(policy.after_turn(&view(1, 1)), Decision::Continue);
        let Decision::Inject(note) = policy.after_turn(&view(2, 9)) else {
            panic!("a note when the run jumps deep into the budget");
        };
        assert!(note.contains("Less than a minute left"), "{note}");
        // Nothing more fires after.
        assert_eq!(policy.after_turn(&view(3, 10)), Decision::Continue);
    }

    #[test]
    fn notes_are_born_once_each() {
        let mut policy = Budget::new(None, Some(4));
        for n in 1..=9 {
            let _ = policy.after_turn(&view(n, 0));
        }
        // Re-running past 100% repeats no note.
        assert_eq!(policy.after_turn(&view(10, 0)), Decision::Continue);
    }
}
