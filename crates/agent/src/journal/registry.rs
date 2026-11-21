//! The field registry: every `(record kind, field number, field name)` pair
//! the journal writes.
//!
//! A field number is never reused, so an old log always decodes with a newer
//! build: a removed field keeps its number free. The golden test in this
//! module lists the pairs, so renumbering one fails review.

use crate::journal::kinds;

/// Fields of a [`kinds::MESSAGE`] record, kept with the message codec in
/// `reagent-ai` so the two never drift.
pub use reagent_ai::record::message;

/// Fields of a [`kinds::CALL_START`] record.
pub mod call_start {
    /// The id the result will answer.
    pub const CALL_ID: u32 = 1;
    /// The tool the model named.
    pub const TOOL_NAME: u32 = 2;
    /// The effect class: 0 read, 1 local reversible, 2 local irreversible, 3 external.
    pub const EFFECT: u32 = 3;
}

/// Fields of a [`kinds::TURN_END`] record.
pub mod turn_end {
    /// The turn number, from 1.
    pub const TURN: u32 = 1;
    /// Input tokens of the turn.
    pub const INPUT_TOKENS: u32 = 2;
    /// Output tokens of the turn.
    pub const OUTPUT_TOKENS: u32 = 3;
    /// Tokens read from the prompt cache.
    pub const CACHE_READ_TOKENS: u32 = 4;
    /// Tokens written to the prompt cache.
    pub const CACHE_WRITE_TOKENS: u32 = 5;
    /// Tokens spent on private reasoning.
    pub const REASONING_TOKENS: u32 = 6;
    /// The cost in USD, as the bits of its `f64`.
    pub const COST_BITS: u32 = 7;
}

/// Fields of a [`kinds::BASE`] record.
pub mod base {
    /// The provider the run is using.
    pub const PROVIDER: u32 = 1;
    /// The model the run is using.
    pub const MODEL: u32 = 2;
    /// How many compactions replaced the history so far.
    pub const COMPACTION: u32 = 3;
    /// One reread ledger entry; repeated.
    pub const LEDGER: u32 = 4;
    /// One file a tool wrote; repeated.
    pub const TOUCHED: u32 = 5;

    /// Fields of one ledger entry inside [`LEDGER`].
    ///
    /// The numbers start at 32, above the fields of the base itself, so this
    /// one flat table has no repeated pair.
    pub mod ledger {
        /// The path as the model wrote it.
        pub const PATH: u32 = 32;
        /// The first line the read showed, 1-based.
        pub const START: u32 = 33;
        /// The last line the read showed, 1-based.
        pub const END: u32 = 34;
        /// The file's size at the read.
        pub const SIZE: u32 = 35;
        /// The file's modified time in milliseconds since the epoch.
        pub const MODIFIED_MS: u32 = 36;
        /// How many times the same lines were shown again.
        pub const REPEATS: u32 = 37;
    }
}

/// Fields of a [`kinds::MODEL`] record.
pub mod model {
    /// The provider the run moves to.
    pub const PROVIDER: u32 = 1;
    /// The model the run moves to.
    pub const MODEL: u32 = 2;
    /// The config provider id the run moves to, when one is known.
    pub const CONFIG: u32 = 3;
}

/// Fields of a [`kinds::APPROVAL`] record.
pub mod approval {
    /// The call id the request is about.
    pub const CALL_ID: u32 = 1;
    /// The kind: 1 waits for an answer, 2 runs, 3 is refused, 4 is the
    /// answer a later process gave to a parked wait.
    pub const KIND: u32 = 2;
    /// The id a later process answers a parked wait under: 32 lowercase hex
    /// characters. A wait that can park and its answer write it.
    pub const QID: u32 = 3;
    /// The tool the call names; a wait that can park only.
    pub const TOOL: u32 = 4;
    /// One line that shows the call; a wait that can park only.
    pub const LINE: u32 = 5;
    /// The rule that asked; a wait that can park only.
    pub const RULE: u32 = 6;
    /// The rule a save would write, when one would; a wait that can park only.
    pub const SAVE: u32 = 7;
    /// Who answered a parked wait; the answer only.
    pub const BY: u32 = 8;
    /// The answer: `once`, `session`, `save` or `refuse`; the answer only.
    pub const ANSWER: u32 = 9;
    /// The reason of a refusal, when one was given; the answer only.
    pub const REASON: u32 = 10;
}

/// Fields of a [`kinds::SETTINGS`] record.
pub mod settings {
    /// The reasoning level the run moves to.
    pub const LEVEL: u32 = 1;
    /// The permission mode the run moves to, as [`PermissionMode::as_str`]
    /// writes it.
    ///
    /// [`PermissionMode::as_str`]: reagent_config::permissions::PermissionMode::as_str
    pub const MODE: u32 = 2;
}

/// Fields of a [`kinds::QUESTION`] record.
///
/// The wait and the answer share the question id, so one number names it for
/// both; every field only one of them writes has its own.
pub mod question {
    /// The kind: 1 waits for an answer, 2 is the answer.
    pub const KIND: u32 = 1;
    /// The tool call the question belongs to; the wait only.
    pub const CALL_ID: u32 = 2;
    /// The id the answer names: 32 lowercase hex characters.
    pub const QID: u32 = 3;
    /// The JSON text of the `questions` argument, as the model sent it; the
    /// wait only.
    pub const QUESTIONS: u32 = 4;
    /// The client that answered, as [`crate::live::ClientId`] writes it; the
    /// answer only.
    pub const BY: u32 = 5;
    /// The JSON text of the answers, one list per question; the answer only.
    pub const ANSWERS: u32 = 6;
    /// The hop whose person answered; not written for hop 0, the owner. The
    /// answer only.
    pub const HOP: u32 = 7;
}

/// Fields of a [`kinds::FORWARD`] record.
pub mod forward {
    /// The id of the question: 32 lowercase hex characters.
    pub const QID: u32 = 1;
    /// The number of the forward: 1 for the first.
    pub const HOP: u32 = 2;
    /// Who forwarded, as the model may read it.
    pub const FROM: u32 = 3;
    /// The key of the contact the question went to.
    pub const TO: u32 = 4;
    /// The name of that contact, as the model may read it.
    pub const NAME: u32 = 5;
    /// The note of the person who forwarded.
    pub const NOTE: u32 = 6;
}

/// Every pair the journal writes, in the order the golden test lists them.
pub const FIELDS: &[(u8, u32, &str)] = &[
    (kinds::MESSAGE, message::ROLE, "message.role"),
    (kinds::MESSAGE, message::CONTENT, "message.content"),
    (kinds::MESSAGE, message::PROVIDER, "message.provider"),
    (kinds::MESSAGE, message::MODEL, "message.model"),
    (kinds::MESSAGE, message::STOP_REASON, "message.stop_reason"),
    (
        kinds::MESSAGE,
        message::INPUT_TOKENS,
        "message.input_tokens",
    ),
    (
        kinds::MESSAGE,
        message::OUTPUT_TOKENS,
        "message.output_tokens",
    ),
    (
        kinds::MESSAGE,
        message::CACHE_READ_TOKENS,
        "message.cache_read_tokens",
    ),
    (
        kinds::MESSAGE,
        message::CACHE_WRITE_TOKENS,
        "message.cache_write_tokens",
    ),
    (
        kinds::MESSAGE,
        message::REASONING_TOKENS,
        "message.reasoning_tokens",
    ),
    (kinds::MESSAGE, message::COST_BITS, "message.cost_bits"),
    (kinds::MESSAGE, message::REPLAY, "message.replay"),
    (kinds::MESSAGE, message::CALL_ID, "message.call_id"),
    (kinds::MESSAGE, message::RESULT, "message.result"),
    (kinds::MESSAGE, message::IS_ERROR, "message.is_error"),
    (kinds::MESSAGE, message::IMAGE, "message.image"),
    (
        kinds::MESSAGE,
        message::image::MEDIA_TYPE,
        "message.image.media_type",
    ),
    (kinds::MESSAGE, message::image::WIDTH, "message.image.width"),
    (
        kinds::MESSAGE,
        message::image::HEIGHT,
        "message.image.height",
    ),
    (kinds::MESSAGE, message::image::DATA, "message.image.data"),
    (
        kinds::MESSAGE,
        message::content::KIND,
        "message.content.kind",
    ),
    (
        kinds::MESSAGE,
        message::content::TEXT,
        "message.content.text",
    ),
    (
        kinds::MESSAGE,
        message::content::THINKING,
        "message.content.thinking",
    ),
    (
        kinds::MESSAGE,
        message::content::SIGNATURE,
        "message.content.signature",
    ),
    (
        kinds::MESSAGE,
        message::content::DATA,
        "message.content.data",
    ),
    (
        kinds::MESSAGE,
        message::content::CALL_ID,
        "message.content.call_id",
    ),
    (
        kinds::MESSAGE,
        message::content::NAME,
        "message.content.name",
    ),
    (
        kinds::MESSAGE,
        message::content::ARGUMENTS,
        "message.content.arguments",
    ),
    (
        kinds::MESSAGE,
        message::content::MEDIA_TYPE,
        "message.content.media_type",
    ),
    (
        kinds::MESSAGE,
        message::content::WIDTH,
        "message.content.width",
    ),
    (
        kinds::MESSAGE,
        message::content::HEIGHT,
        "message.content.height",
    ),
    (
        kinds::MESSAGE,
        message::content::IMAGE_DATA,
        "message.content.image_data",
    ),
    (kinds::CALL_START, call_start::CALL_ID, "call_start.call_id"),
    (
        kinds::CALL_START,
        call_start::TOOL_NAME,
        "call_start.tool_name",
    ),
    (kinds::CALL_START, call_start::EFFECT, "call_start.effect"),
    (kinds::TURN_END, turn_end::TURN, "turn_end.turn"),
    (
        kinds::TURN_END,
        turn_end::INPUT_TOKENS,
        "turn_end.input_tokens",
    ),
    (
        kinds::TURN_END,
        turn_end::OUTPUT_TOKENS,
        "turn_end.output_tokens",
    ),
    (
        kinds::TURN_END,
        turn_end::CACHE_READ_TOKENS,
        "turn_end.cache_read_tokens",
    ),
    (
        kinds::TURN_END,
        turn_end::CACHE_WRITE_TOKENS,
        "turn_end.cache_write_tokens",
    ),
    (
        kinds::TURN_END,
        turn_end::REASONING_TOKENS,
        "turn_end.reasoning_tokens",
    ),
    (kinds::TURN_END, turn_end::COST_BITS, "turn_end.cost_bits"),
    (kinds::BASE, base::PROVIDER, "base.provider"),
    (kinds::BASE, base::MODEL, "base.model"),
    (kinds::BASE, base::COMPACTION, "base.compaction"),
    (kinds::BASE, base::LEDGER, "base.ledger"),
    (kinds::BASE, base::TOUCHED, "base.touched"),
    (kinds::BASE, base::ledger::PATH, "base.ledger.path"),
    (kinds::BASE, base::ledger::START, "base.ledger.start"),
    (kinds::BASE, base::ledger::END, "base.ledger.end"),
    (kinds::BASE, base::ledger::SIZE, "base.ledger.size"),
    (
        kinds::BASE,
        base::ledger::MODIFIED_MS,
        "base.ledger.modified_ms",
    ),
    (kinds::BASE, base::ledger::REPEATS, "base.ledger.repeats"),
    (kinds::MODEL, model::PROVIDER, "model.provider"),
    (kinds::MODEL, model::MODEL, "model.model"),
    (kinds::MODEL, model::CONFIG, "model.config"),
    (kinds::APPROVAL, approval::CALL_ID, "approval.call_id"),
    (kinds::APPROVAL, approval::KIND, "approval.kind"),
    (kinds::APPROVAL, approval::QID, "approval.qid"),
    (kinds::APPROVAL, approval::TOOL, "approval.tool"),
    (kinds::APPROVAL, approval::LINE, "approval.line"),
    (kinds::APPROVAL, approval::RULE, "approval.rule"),
    (kinds::APPROVAL, approval::SAVE, "approval.save"),
    (kinds::APPROVAL, approval::BY, "approval.by"),
    (kinds::APPROVAL, approval::ANSWER, "approval.answer"),
    (kinds::APPROVAL, approval::REASON, "approval.reason"),
    (kinds::SETTINGS, settings::LEVEL, "settings.level"),
    (kinds::SETTINGS, settings::MODE, "settings.mode"),
    (kinds::QUESTION, question::KIND, "question.kind"),
    (kinds::QUESTION, question::CALL_ID, "question.call_id"),
    (kinds::QUESTION, question::QID, "question.qid"),
    (kinds::QUESTION, question::QUESTIONS, "question.questions"),
    (kinds::QUESTION, question::BY, "question.by"),
    (kinds::QUESTION, question::ANSWERS, "question.answers"),
    (kinds::QUESTION, question::HOP, "question.hop"),
    (kinds::FORWARD, forward::QID, "forward.qid"),
    (kinds::FORWARD, forward::HOP, "forward.hop"),
    (kinds::FORWARD, forward::FROM, "forward.from"),
    (kinds::FORWARD, forward::TO, "forward.to"),
    (kinds::FORWARD, forward::NAME, "forward.name"),
    (kinds::FORWARD, forward::NOTE, "forward.note"),
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn no_kind_and_field_repeats() {
        let mut seen = HashSet::new();
        for (kind, field, name) in FIELDS {
            assert!(
                seen.insert((*kind, *field)),
                "{name} reuses kind {kind} field {field}"
            );
        }
    }

    #[test]
    fn no_name_repeats_inside_a_kind() {
        let mut seen = HashSet::new();
        for (kind, _, name) in FIELDS {
            assert!(seen.insert((*kind, *name)), "{name} repeats in kind {kind}");
        }
    }

    #[test]
    fn every_kind_starts_at_the_first_record_kind() {
        assert_eq!(
            kinds::MESSAGE,
            reagent_store::kind::RECORD_FIRST,
            "the first record kind"
        );
        assert_eq!(FIELDS[0].0, kinds::MESSAGE, "the table starts at MESSAGE");
    }

    #[test]
    fn the_table_is_the_agreed_one() {
        let golden = "\
16 1 message.role
16 2 message.content
16 3 message.provider
16 4 message.model
16 5 message.stop_reason
16 6 message.input_tokens
16 7 message.output_tokens
16 8 message.cache_read_tokens
16 9 message.cache_write_tokens
16 10 message.reasoning_tokens
16 11 message.cost_bits
16 12 message.replay
16 13 message.call_id
16 14 message.result
16 15 message.is_error
16 16 message.image
16 44 message.image.media_type
16 45 message.image.width
16 46 message.image.height
16 47 message.image.data
16 32 message.content.kind
16 33 message.content.text
16 34 message.content.thinking
16 35 message.content.signature
16 36 message.content.data
16 37 message.content.call_id
16 38 message.content.name
16 39 message.content.arguments
16 40 message.content.media_type
16 41 message.content.width
16 42 message.content.height
16 43 message.content.image_data
17 1 call_start.call_id
17 2 call_start.tool_name
17 3 call_start.effect
18 1 turn_end.turn
18 2 turn_end.input_tokens
18 3 turn_end.output_tokens
18 4 turn_end.cache_read_tokens
18 5 turn_end.cache_write_tokens
18 6 turn_end.reasoning_tokens
18 7 turn_end.cost_bits
19 1 base.provider
19 2 base.model
19 3 base.compaction
19 4 base.ledger
19 5 base.touched
19 32 base.ledger.path
19 33 base.ledger.start
19 34 base.ledger.end
19 35 base.ledger.size
19 36 base.ledger.modified_ms
19 37 base.ledger.repeats
20 1 model.provider
20 2 model.model
20 3 model.config
24 1 approval.call_id
24 2 approval.kind
24 3 approval.qid
24 4 approval.tool
24 5 approval.line
24 6 approval.rule
24 7 approval.save
24 8 approval.by
24 9 approval.answer
24 10 approval.reason
26 1 settings.level
26 2 settings.mode
27 1 question.kind
27 2 question.call_id
27 3 question.qid
27 4 question.questions
27 5 question.by
27 6 question.answers
27 7 question.hop
28 1 forward.qid
28 2 forward.hop
28 3 forward.from
28 4 forward.to
28 5 forward.name
28 6 forward.note
";
        let mut listed = String::new();
        for (kind, field, name) in FIELDS {
            listed.push_str(&format!("{kind} {field} {name}\n"));
        }
        assert_eq!(listed, golden, "a field number moved");
    }
}
