//! The record kinds the journal writes, from [`RECORD_FIRST`] up.
//!
//! A kind number is never reused either. The kinds listed as unused are
//! defined now so the format does not change when they are built.

use reagent_store::kind::RECORD_FIRST;

/// One `reagent_ai::Message`: a user message, an assistant reply, or a tool result.
pub const MESSAGE: u8 = RECORD_FIRST;

/// A tool call about to run: its id, tool name and effect class.
pub const CALL_START: u8 = RECORD_FIRST + 1;

/// The end of one turn: its number and usage.
pub const TURN_END: u8 = RECORD_FIRST + 2;

/// A point a resume may start from: the agent state, written with `base = true`.
pub const BASE: u8 = RECORD_FIRST + 3;

/// A change of provider and model.
pub const MODEL: u8 = RECORD_FIRST + 4;

/// A file's content before a tool wrote it; unused until file snapshots land.
pub const FILE_SNAPSHOT: u8 = RECORD_FIRST + 5;

/// A request the run answers, such as a queued user message; unused for now.
pub const INBOX: u8 = RECORD_FIRST + 6;

/// A timer the run waits on; unused for now.
pub const TIMER: u8 = RECORD_FIRST + 7;

/// A call that waits for a client's answer, or the answer it got: the call id
/// and the wait or answer kind.
///
/// The wait is on the disk before the call asks, so a resume knows a call that
/// never got an answer never ran, whatever its `CALL_START` says.
pub const APPROVAL: u8 = RECORD_FIRST + 8;

/// The task a subagent belongs to; unused for now.
pub const OWNER: u8 = RECORD_FIRST + 9;

/// A change of the reasoning level or the permission mode. A change of the
/// model is a [`MODEL`] record.
pub const SETTINGS: u8 = RECORD_FIRST + 10;

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use reagent_ai::{AssistantMessage, Content, Message, StopReason, ToolCall, Usage};
    use reagent_store::Encoder;

    use crate::journal::record::{
        self, Approval, ApprovalKind, CallStart, Model, Record, Settings, TurnEnd,
    };
    use crate::journal::state::{LedgerEntry, State};
    use crate::tool::EffectClass;

    use super::*;

    fn user_with_content() -> Message {
        Message::user_content(vec![
            Content::text("look at this"),
            Content::text("and this"),
        ])
    }

    fn assistant_with_everything() -> Message {
        let mut reply = AssistantMessage::new(
            "anthropic",
            "claude-sonnet-4-5",
            vec![
                Content::Thinking {
                    thinking: "hmm".into(),
                    signature: Some("sig".into()),
                },
                Content::Thinking {
                    thinking: "no signature".into(),
                    signature: None,
                },
                Content::RedactedThinking {
                    data: "opaque".into(),
                },
                Content::text("hello"),
                Content::ToolCall(ToolCall {
                    id: "toolu_01".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                }),
            ],
        );
        reply.stop_reason = Some(StopReason::ToolUse);
        reply.usage = Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 30,
            cache_write_tokens: 40,
            reasoning_tokens: 50,
            cost: Some(0.0125),
        };
        reply.replay = Some(r#"{"k":"v"}"#.into());
        Message::assistant(reply)
    }

    fn assert_message_round_trips(message: Message) {
        let mut out = Vec::new();
        record::encode_message(&mut out, &message);
        let payload = Bytes::from(out);
        let decoded = record::decode(MESSAGE, &payload).expect("decode");
        assert_eq!(decoded, Record::Message(message));
    }

    #[test]
    fn every_message_shape_round_trips() {
        assert_message_round_trips(user_with_content());
        assert_message_round_trips(assistant_with_everything());
        assert_message_round_trips(Message::assistant(AssistantMessage::new(
            "fake",
            "m",
            Vec::new(),
        )));
        assert_message_round_trips(Message::tool_result("call_1", "text", false));
        assert_message_round_trips(Message::tool_result("call_2", "boom", true));
    }

    #[test]
    fn an_empty_content_text_round_trips() {
        let mut reply = AssistantMessage::new("fake", "m", Vec::new());
        reply.stop_reason = Some(StopReason::Error);
        let message = Message::assistant(reply);
        let mut out = Vec::new();
        record::encode_message(&mut out, &message);
        let decoded = record::decode(MESSAGE, &Bytes::from(out)).expect("decode");
        assert_eq!(decoded, Record::Message(message));
    }

    #[test]
    fn an_unknown_field_is_skipped() {
        let message = Message::user("hello");
        let mut out = Vec::new();
        record::encode_message(&mut out, &message);
        let mut encoder = Encoder::new(&mut out);
        encoder.u64(99, 7);
        encoder.str(98, "a field from a later build");
        let decoded = record::decode(MESSAGE, &Bytes::from(out)).expect("decode");
        assert_eq!(decoded, Record::Message(message));
    }

    #[test]
    fn an_unknown_kind_is_skipped() {
        let record = record::decode(200, &Bytes::new()).expect("decode");
        assert_eq!(record, Record::Unknown(200));
    }

    #[test]
    fn a_call_start_round_trips() {
        let call = CallStart {
            id: "call_1".into(),
            name: "bash".into(),
            class: EffectClass::LocalReversible,
        };
        let mut out = Vec::new();
        record::encode_call_start(&mut out, &call.id, &call.name, call.class);
        let decoded = record::decode(CALL_START, &Bytes::from(out)).expect("decode");
        assert_eq!(decoded, Record::CallStart(call));
    }

    #[test]
    fn every_effect_class_round_trips() {
        for class in [
            EffectClass::Read,
            EffectClass::LocalReversible,
            EffectClass::LocalIrreversible,
            EffectClass::External,
        ] {
            let call = CallStart {
                id: "call_1".into(),
                name: "bash".into(),
                class,
            };
            let mut out = Vec::new();
            record::encode_call_start(&mut out, &call.id, &call.name, call.class);
            let decoded = record::decode(CALL_START, &Bytes::from(out)).expect("decode");
            assert_eq!(decoded, Record::CallStart(call), "{class:?}");
        }
    }

    #[test]
    fn a_turn_end_round_trips() {
        let turn_end = TurnEnd {
            turn: 3,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 30,
                cache_write_tokens: 40,
                reasoning_tokens: 50,
                cost: Some(0.5),
            },
        };
        let mut out = Vec::new();
        record::encode_turn_end(&mut out, &turn_end);
        let decoded = record::decode(TURN_END, &Bytes::from(out)).expect("decode");
        assert_eq!(decoded, Record::TurnEnd(turn_end));
    }

    #[test]
    fn a_turn_end_without_a_cost_round_trips() {
        let turn_end = TurnEnd {
            turn: 1,
            usage: Usage::default(),
        };
        let mut out = Vec::new();
        record::encode_turn_end(&mut out, &turn_end);
        let decoded = record::decode(TURN_END, &Bytes::from(out)).expect("decode");
        assert_eq!(decoded, Record::TurnEnd(turn_end));
    }

    #[test]
    fn a_base_round_trips() {
        let state = State {
            provider: "anthropic".into(),
            model: "claude-sonnet-4-5".into(),
            compaction: 2,
            ledger: vec![
                LedgerEntry {
                    path: "src/a.rs".into(),
                    start: 1,
                    end: 20,
                    size: 400,
                    modified_ms: Some(1_700_000_000_000),
                    repeats: 1,
                },
                LedgerEntry {
                    path: "src/b.rs".into(),
                    start: 5,
                    end: 9,
                    size: 100,
                    modified_ms: None,
                    repeats: 0,
                },
            ],
            touched: vec!["src/a.rs".into(), "src/c.rs".into()],
        };
        let mut out = Vec::new();
        record::encode_base(&mut out, &state);
        let decoded = record::decode(BASE, &Bytes::from(out)).expect("decode");
        assert_eq!(decoded, Record::Base(state));
    }

    #[test]
    fn a_model_round_trips() {
        let model = Model {
            provider: "openai".into(),
            model: "gpt-5".into(),
        };
        let mut out = Vec::new();
        record::encode_model(&mut out, &model.provider, &model.model);
        let decoded = record::decode(MODEL, &Bytes::from(out)).expect("decode");
        assert_eq!(decoded, Record::Model(model));
    }

    #[test]
    fn an_approval_wait_and_answer_round_trip() {
        for kind in [ApprovalKind::Wait, ApprovalKind::Run, ApprovalKind::Refused] {
            let approval = Approval {
                id: "call_1".into(),
                kind,
            };
            let mut out = Vec::new();
            record::encode_approval(&mut out, &approval);
            let payload = Bytes::from(out);
            let decoded = record::decode_aside(APPROVAL, &payload).expect("decode");
            assert_eq!(decoded, Some(record::Aside::Approval(approval)), "{kind:?}");
        }
    }

    #[test]
    fn a_settings_change_round_trips() {
        let every = Settings {
            level: Some("high".into()),
            mode: Some("accept-edits".into()),
        };
        let level_only = Settings {
            level: Some("low".into()),
            mode: None,
        };
        let mode_only = Settings {
            level: None,
            mode: Some("bypass".into()),
        };
        for settings in [every, level_only, mode_only] {
            let mut out = Vec::new();
            record::encode_settings(&mut out, &settings);
            let payload = Bytes::from(out);
            let decoded = record::decode_aside(SETTINGS, &payload).expect("decode");
            assert_eq!(
                decoded,
                Some(record::Aside::Settings(settings.clone())),
                "{settings:?}"
            );
        }
    }

    #[test]
    fn the_kinds_are_defined_once() {
        let kinds = [
            MESSAGE,
            CALL_START,
            TURN_END,
            BASE,
            MODEL,
            FILE_SNAPSHOT,
            INBOX,
            TIMER,
            APPROVAL,
            OWNER,
            SETTINGS,
        ];
        for (at, kind) in kinds.iter().enumerate() {
            assert_eq!(*kind, RECORD_FIRST + at as u8);
        }
    }
}
