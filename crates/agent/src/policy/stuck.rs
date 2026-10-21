//! The stuck-detection policy.
//!
//! A run that loops repeats the same call, hits the same error, or rewrites
//! the same file. This policy counts each and, once a repeated thing crosses
//! its threshold, injects exactly one note that names it. Every distinct
//! repeated thing gets its own note, once.
//!
//! Thresholds: the same tool call 3 times, the same error text 3 times, or
//! `edit`/`write` on the same path 5 times. Keys are short hashes, so the map
//! stays small and each call is O(1).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use reagent_ai::Text;

use super::{Decision, TurnPolicy, TurnView};

/// The stuck preamble, baked in at build time from `prompts/stuck.md`.
const TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/stuck.md"
));

/// How many identical calls count as stuck.
const CALLS_LIMIT: u32 = 3;
/// How many identical errors count as stuck.
const ERRORS_LIMIT: u32 = 3;
/// How many `edit`/`write` calls on one path count as stuck.
const EDIT_LIMIT: u32 = 5;

/// Counts repeated calls, errors and file rewrites.
///
/// A call counts only while it repeats back to back: running the same test
/// again after an edit is normal work. Errors and file rewrites count over
/// the whole run. Each of those counters is keyed on a small hash of the
/// repeated thing and holds how many times it was seen and whether a note
/// already fired for it.
#[derive(Debug, Default)]
pub struct Stuck {
    counters: HashMap<String, (u32, bool)>,
    last_call: Option<String>,
    streak: u32,
}

impl Stuck {
    /// Counts `key` and, when it crossed its `limit` for the first time,
    /// returns the count that crossed it.
    fn count(&mut self, key: String, limit: u32) -> Option<u32> {
        let entry = self.counters.entry(key).or_insert((0, false));
        if entry.1 {
            return None;
        }
        entry.0 += 1;
        if entry.0 >= limit {
            entry.1 = true;
            Some(entry.0)
        } else {
            None
        }
    }

    /// Counts `key` as the next call and returns the streak length when it
    /// reaches the limit. A different call starts a new streak.
    fn count_call(&mut self, key: String) -> Option<u32> {
        if self.last_call.as_ref() == Some(&key) {
            self.streak += 1;
        } else {
            self.last_call = Some(key);
            self.streak = 1;
        }
        (self.streak == CALLS_LIMIT).then_some(self.streak)
    }

    /// Builds a full note: the preamble plus a line naming the repeated thing.
    fn note(&self, detail: &str) -> Text {
        Text::from(format!("{TEMPLATE}\n\n{detail}"))
    }
}

impl TurnPolicy for Stuck {
    fn after_turn(&mut self, turn: &TurnView<'_>) -> Decision {
        let mut notes: Vec<Text> = Vec::new();

        for (call, result) in turn.tools {
            // The same tool call repeated back to back.
            let call_key = key("call", &[&call.name, &call.arguments]);
            if let Some(n) = self.count_call(call_key) {
                notes.push(self.note(&format!(
                    "You called `{}` with the same arguments {n} times.",
                    call.name
                )));
            }

            // The same error repeated.
            if result.is_error {
                let error = &result.content;
                let error_key = key("err", &[error]);
                if let Some(n) = self.count(error_key, ERRORS_LIMIT) {
                    let snippet: String = error.chars().take(120).collect();
                    notes.push(
                        self.note(&format!("The same error has appeared {n} times: {snippet}",)),
                    );
                }
            }

            // edit/write on the same path repeated.
            if matches!(&*call.name, "edit" | "write")
                && let Some(path) = path_of(&call.arguments)
            {
                let path_key = key("file", &[&path]);
                if let Some(n) = self.count(path_key, EDIT_LIMIT) {
                    notes.push(self.note(&format!("You wrote or edited `{path}` {n} times.",)));
                }
            }
        }

        // Join every note that fired this turn into one injection, so a turn
        // where several repeated things crossed at once loses none of them.
        if notes.is_empty() {
            Decision::Continue
        } else {
            Decision::Inject(Text::from(join_notes(notes)))
        }
    }
}

/// Joins one turn's notes into a single injected message.
fn join_notes(notes: Vec<Text>) -> String {
    let mut out = String::new();
    for (index, note) in notes.iter().enumerate() {
        if index > 0 {
            out.push_str("\n\n");
        }
        out.push_str(note);
    }
    out
}

/// A short, stable-in-run key for one repeated thing and its trigger kind.
fn key(kind: &str, parts: &[&str]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    format!("{kind}:{:x}", hasher.finish())
}

/// The `path` argument of an `edit`/`write` call, if it parses.
fn path_of(args: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(args).ok()?;
    value.get("path")?.as_str().map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reagent_ai::{AssistantMessage, Content, Text, ToolCall, ToolResult};

    use super::super::{Decision, TurnView};
    use super::*;

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: Text::from(args),
        }
    }

    fn ok(id: &str, content: &str) -> ToolResult {
        ToolResult {
            call_id: id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    fn err(id: &str, content: &str) -> ToolResult {
        ToolResult {
            call_id: id.into(),
            content: content.into(),
            is_error: true,
        }
    }

    /// Runs one turn with the given tool pairs through the policy.
    fn turn_with(policy: &mut Stuck, tools: &[(ToolCall, ToolResult)]) -> Decision {
        let a: &'static AssistantMessage = Box::leak(Box::new(AssistantMessage::new(
            "fake",
            "m",
            vec![Content::text("x")],
        )));
        let view = TurnView {
            index: 1,
            elapsed: Duration::from_secs(1),
            ran_tools: true,
            assistant: a,
            tools,
            changed_files: None,
        };
        policy.after_turn(&view)
    }

    #[test]
    fn the_same_call_three_times_gives_one_note() {
        let mut policy = Stuck::default();
        let tools = [(
            call("c", "bash", r#"{"command":"head -1 x"}"#),
            ok("c", "hi"),
        )];
        assert_eq!(turn_with(&mut policy, &tools), Decision::Continue);
        assert_eq!(turn_with(&mut policy, &tools), Decision::Continue);
        let Decision::Inject(text) = turn_with(&mut policy, &tools) else {
            panic!("expected a note on the third identical call");
        };
        assert!(
            text.contains("`bash` with the same arguments 3 times"),
            "{text}"
        );
        // A fourth identical call does not fire again.
        assert_eq!(turn_with(&mut policy, &tools), Decision::Continue);
    }

    #[test]
    fn a_call_repeated_with_other_calls_between_does_not_fire() {
        let mut policy = Stuck::default();
        let test = [(
            call("c", "bash", r#"{"command":"cargo test"}"#),
            ok("c", "fail"),
        )];
        let edit = [(
            call("e", "bash", r#"{"command":"sed -i s/a/b/ x"}"#),
            ok("e", ""),
        )];
        for _ in 0..4 {
            assert_eq!(turn_with(&mut policy, &test), Decision::Continue);
            assert_eq!(turn_with(&mut policy, &edit), Decision::Continue);
        }
    }

    #[test]
    fn a_new_streak_fires_again() {
        let mut policy = Stuck::default();
        let same = [(call("c", "bash", r#"{"command":"ls"}"#), ok("c", "x"))];
        let other = [(call("o", "bash", r#"{"command":"pwd"}"#), ok("o", "/"))];
        for _ in 0..2 {
            assert_eq!(turn_with(&mut policy, &same), Decision::Continue);
        }
        assert!(matches!(turn_with(&mut policy, &same), Decision::Inject(_)));
        assert_eq!(turn_with(&mut policy, &other), Decision::Continue);
        for _ in 0..2 {
            assert_eq!(turn_with(&mut policy, &same), Decision::Continue);
        }
        assert!(matches!(turn_with(&mut policy, &same), Decision::Inject(_)));
    }

    #[test]
    fn different_arguments_have_separate_counters() {
        let mut policy = Stuck::default();
        let first = [(
            call("c", "bash", r#"{"command":"head -1 x"}"#),
            ok("c", "hi"),
        )];
        let second = [(
            call("c", "bash", r#"{"command":"head -2 x"}"#),
            ok("c", "hi"),
        )];
        // Each appears twice, interleaved: neither crosses the limit of 3.
        assert_eq!(turn_with(&mut policy, &first), Decision::Continue);
        assert_eq!(turn_with(&mut policy, &second), Decision::Continue);
        assert_eq!(turn_with(&mut policy, &first), Decision::Continue);
        assert_eq!(turn_with(&mut policy, &second), Decision::Continue);
    }

    #[test]
    fn the_same_error_three_times_gives_one_note() {
        let mut policy = Stuck::default();
        // Distinct commands, the same error text, so only the error trigger ticks.
        let one = [(call("c", "bash", r#"{"command":"a"}"#), err("c", "broken"))];
        let two = [(call("c", "bash", r#"{"command":"b"}"#), err("c", "broken"))];
        let three = [(call("c", "bash", r#"{"command":"c"}"#), err("c", "broken"))];
        assert_eq!(turn_with(&mut policy, &one), Decision::Continue);
        assert_eq!(turn_with(&mut policy, &two), Decision::Continue);
        let Decision::Inject(text) = turn_with(&mut policy, &three) else {
            panic!("expected a note on the third identical error");
        };
        assert!(text.contains("same error"), "{text}");
        // A fourth distinct command with the same error does not fire again.
        assert_eq!(
            turn_with(
                &mut policy,
                &[(call("c", "bash", r#"{"command":"d"}"#), err("c", "broken"))]
            ),
            Decision::Continue
        );
    }

    #[test]
    fn a_success_result_is_not_an_error_trigger() {
        let mut policy = Stuck::default();
        // Distinct commands, all successful: no counter ever crosses 3.
        for n in 0..5 {
            let cmd = format!(r#"{{"command":"cmd{n}"}}"#);
            let tools = [(call("c", "bash", &cmd), ok("c", "ok"))];
            assert_eq!(turn_with(&mut policy, &tools), Decision::Continue);
        }
    }

    #[test]
    fn editing_the_same_path_five_times_gives_one_note() {
        let mut policy = Stuck::default();
        // Different new_string each edit, so only the path counter ticks to 5.
        for n in 0..5 {
            let args = format!(r#"{{"path":"a.rs","old_string":"x","new_string":"y{n}"}}"#);
            let tools = [(call("c", "edit", &args), ok("c", "ok"))];
            if n < 4 {
                assert_eq!(
                    turn_with(&mut policy, &tools),
                    Decision::Continue,
                    "turn {n}"
                );
            } else {
                let Decision::Inject(text) = turn_with(&mut policy, &tools) else {
                    panic!("expected a note on the fifth edit");
                };
                assert!(text.contains("`a.rs` 5 times"), "{text}");
            }
        }
        assert_eq!(
            turn_with(
                &mut policy,
                &[(
                    call(
                        "c",
                        "edit",
                        r#"{"path":"a.rs","old_string":"x","new_string":"y9"}"#
                    ),
                    ok("c", "ok"),
                )],
            ),
            Decision::Continue,
            "a sixth edit does not fire again"
        );
    }

    #[test]
    fn writing_is_counted_like_editing() {
        let mut policy = Stuck::default();
        for n in 0..5 {
            let args = format!(r#"{{"path":"a.rs","content":"x{n}"}}"#);
            let tools = [(call("c", "write", &args), ok("c", "ok"))];
            if n < 4 {
                assert_eq!(
                    turn_with(&mut policy, &tools),
                    Decision::Continue,
                    "turn {n}"
                );
            } else {
                assert!(matches!(
                    turn_with(&mut policy, &tools),
                    Decision::Inject(_)
                ));
            }
        }
    }

    #[test]
    fn a_read_never_ticks_the_file_rewrite_counter() {
        let mut policy = Stuck::default();
        // Same read call: the call trigger fires once at three, but the file
        // rewrite trigger (edit/write only) stays silent.
        let tools = [(call("c", "read", r#"{"path":"a.rs"}"#), ok("c", "ok"))];
        assert_eq!(turn_with(&mut policy, &tools), Decision::Continue);
        assert_eq!(turn_with(&mut policy, &tools), Decision::Continue);
        assert!(matches!(
            turn_with(&mut policy, &tools),
            Decision::Inject(_)
        ));
        assert_eq!(
            turn_with(&mut policy, &tools),
            Decision::Continue,
            "and only once"
        );
    }

    #[test]
    fn two_triggers_in_one_turn_neither_is_lost() {
        let mut policy = Stuck::default();
        // Two distinct bash calls that each fire on their third occurrence.
        // Run the pair twice (two each), then a third turn with both again
        // so both cross at once.
        let pair = [
            (call("a", "bash", r#"{"command":"x"}"#), err("a", "errA")),
            (call("b", "bash", r#"{"command":"y"}"#), err("b", "errB")),
        ];
        assert_eq!(turn_with(&mut policy, &pair), Decision::Continue);
        assert_eq!(turn_with(&mut policy, &pair), Decision::Continue);
        let Decision::Inject(text) = turn_with(&mut policy, &pair) else {
            panic!("the third turn fires");
        };
        assert!(text.contains("errA"), "{text}");
        assert!(text.contains("errB"), "{text}");
        // Nothing more fires afterwards.
        assert_eq!(turn_with(&mut policy, &pair), Decision::Continue);
    }

    #[test]
    fn each_distinct_repeated_thing_fires_once() {
        let mut policy = Stuck::default();
        let a = [(call("c", "bash", r#"{"command":"x"}"#), err("c", "errA"))];
        let b = [(call("c", "bash", r#"{"command":"y"}"#), err("c", "errB"))];
        // Interleave two distinct triggers, three of each; each fires once.
        assert_eq!(turn_with(&mut policy, &a), Decision::Continue);
        assert_eq!(turn_with(&mut policy, &b), Decision::Continue);
        assert_eq!(turn_with(&mut policy, &a), Decision::Continue);
        assert_eq!(turn_with(&mut policy, &b), Decision::Continue);
        let Decision::Inject(note_a) = turn_with(&mut policy, &a) else {
            panic!("a fires on its third turn");
        };
        assert!(note_a.contains("errA"), "{note_a}");
        let Decision::Inject(note_b) = turn_with(&mut policy, &b) else {
            panic!("b fires on its third turn");
        };
        assert!(note_b.contains("errB"), "{note_b}");
        // Neither fires a second time.
        assert_eq!(turn_with(&mut policy, &a), Decision::Continue);
        assert_eq!(turn_with(&mut policy, &b), Decision::Continue);
    }
}
