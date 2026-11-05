//! Encoding and decoding of the record payloads, field by field.
//!
//! A payload is a protobuf-style list of fields; the numbers are in
//! [`crate::journal::registry`]. A decoder skips a field it does not know and
//! a record of a kind it does not know, so an old build reads a newer log.
//!
//! A message record is a message type, so its codec lives in
//! [`reagent_ai::record`], next to the type; [`encode_message`] is re-exported
//! here so a caller of the journal does not change.

use std::io;

use bytes::Bytes;
use reagent_ai::{Message, Text, Usage};
use reagent_store::{DecodeError, Decoder, Encoder, Value};

use crate::journal::kinds;
use crate::journal::registry::{approval, base, call_start, model, settings, turn_end};
use crate::journal::state::{LedgerEntry, State};
use crate::tool::EffectClass;

pub use reagent_ai::record::encode_message;

/// The kind byte of an approval record that waits for an answer.
const APPROVAL_WAIT: u64 = 1;
/// The kind byte of an approval record a client answered to run.
const APPROVAL_RUN: u64 = 2;
/// The kind byte of an approval record a client refused.
const APPROVAL_REFUSED: u64 = 3;

/// One journal record, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// One message of the conversation.
    Message(Message),
    /// A tool call that is about to run.
    CallStart(CallStart),
    /// The end of one turn.
    TurnEnd(TurnEnd),
    /// A point a resume may start from.
    Base(State),
    /// A change of provider and model.
    Model(Model),
    /// A record of a kind this build does not know.
    Unknown(u8),
}

/// A tool call about to run, with the effect class `Tool::effect` gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallStart {
    /// The call id the result will answer.
    pub id: String,
    /// The tool the model named.
    pub name: String,
    /// How far the call reaches.
    pub class: EffectClass,
}

/// The end of one turn: its number and the usage it spent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnEnd {
    /// The turn number, from 1.
    pub turn: u64,
    /// The usage of the turn.
    pub usage: Usage,
}

/// A change of provider and model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    /// The provider the run moves to.
    pub provider: String,
    /// The model the run moves to.
    pub model: String,
    /// The config provider id the run moves to, when one is known.
    pub config: Option<Text>,
}

/// A call that waits for a client's answer, or the answer it got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    /// The call id the request is about.
    pub id: String,
    /// Whether the record is the wait or the answer.
    pub kind: ApprovalKind,
}

/// What an [`Approval`] record says about its call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalKind {
    /// The call waits for a client's answer.
    Wait,
    /// A client answered that the call runs.
    Run,
    /// A client refused the call.
    Refused,
}

/// A change of the reasoning level or the permission mode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// The reasoning level the run moves to, when the change sets one.
    pub level: Option<String>,
    /// The permission mode the run moves to, when the change sets one.
    pub mode: Option<String>,
}

/// A record of a kind the replayed log does not carry: a call that waits for a
/// client or the answer it got, or a settings change.
///
/// [`decode`] reports these kinds as [`Record::Unknown`], so a build that only
/// shows the log still reads a log it does not know; the resume and the loop
/// decode them here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Aside {
    /// A call that waits for a client's answer, or the answer it got.
    Approval(Approval),
    /// A change of the reasoning level or the permission mode.
    Settings(Settings),
}

/// Decodes one record of a kind [`decode`] leaves as [`Record::Unknown`], or
/// `None` for a kind [`decode`] carries.
pub fn decode_aside(kind: u8, payload: &Bytes) -> io::Result<Option<Aside>> {
    match kind {
        kinds::APPROVAL => decode_approval(payload).map(Aside::Approval).map(Some),
        kinds::SETTINGS => decode_settings(payload).map(Aside::Settings).map(Some),
        _ => Ok(None),
    }
}

/// Decodes one record of `kind`.
pub fn decode(kind: u8, payload: &Bytes) -> io::Result<Record> {
    match kind {
        kinds::MESSAGE => reagent_ai::record::decode_message(payload).map(Record::Message),
        kinds::CALL_START => decode_call_start(payload).map(Record::CallStart),
        kinds::TURN_END => decode_turn_end(payload).map(Record::TurnEnd),
        kinds::BASE => decode_base(payload).map(Record::Base),
        kinds::MODEL => decode_model(payload).map(Record::Model),
        other => Ok(Record::Unknown(other)),
    }
}

/// Writes one tool call start.
pub fn encode_call_start(out: &mut Vec<u8>, id: &str, name: &str, class: EffectClass) {
    let mut encoder = Encoder::new(out);
    encoder.str(call_start::CALL_ID, id);
    encoder.str(call_start::TOOL_NAME, name);
    encoder.u64(call_start::EFFECT, class_byte(class));
}

/// Writes one turn end.
pub fn encode_turn_end(out: &mut Vec<u8>, turn_end: &TurnEnd) {
    let mut encoder = Encoder::new(out);
    encoder.u64(turn_end::TURN, turn_end.turn);
    encoder.u64(turn_end::INPUT_TOKENS, turn_end.usage.input_tokens);
    encoder.u64(turn_end::OUTPUT_TOKENS, turn_end.usage.output_tokens);
    encoder.u64(
        turn_end::CACHE_READ_TOKENS,
        turn_end.usage.cache_read_tokens,
    );
    encoder.u64(
        turn_end::CACHE_WRITE_TOKENS,
        turn_end.usage.cache_write_tokens,
    );
    encoder.u64(turn_end::REASONING_TOKENS, turn_end.usage.reasoning_tokens);
    if let Some(cost) = turn_end.usage.cost {
        encoder.u64(turn_end::COST_BITS, cost.to_bits());
    }
}

/// Writes one base: the agent state at a point a resume may start from.
pub fn encode_base(out: &mut Vec<u8>, state: &State) {
    let mut encoder = Encoder::new(out);
    encoder.str(base::PROVIDER, &state.provider);
    encoder.str(base::MODEL, &state.model);
    encoder.u64(base::COMPACTION, u64::from(state.compaction));
    for entry in &state.ledger {
        encoder.message(base::LEDGER, |inner| encode_ledger_entry(inner, entry));
    }
    for path in &state.touched {
        encoder.str(base::TOUCHED, path);
    }
}

/// Writes one reread ledger entry, inside a [`base::LEDGER`] field.
fn encode_ledger_entry(encoder: &mut Encoder<'_>, entry: &LedgerEntry) {
    encoder.str(base::ledger::PATH, &entry.path);
    encoder.u64(base::ledger::START, entry.start);
    encoder.u64(base::ledger::END, entry.end);
    encoder.u64(base::ledger::SIZE, entry.size);
    if let Some(modified) = entry.modified_ms {
        encoder.u64(base::ledger::MODIFIED_MS, modified);
    }
    encoder.u64(base::ledger::REPEATS, u64::from(entry.repeats));
}

/// Writes one change of provider and model, with the config provider id of the
/// switch when one is known.
pub fn encode_model(out: &mut Vec<u8>, provider: &str, model: &str, config: Option<&str>) {
    let mut encoder = Encoder::new(out);
    encoder.str(model::PROVIDER, provider);
    encoder.str(model::MODEL, model);
    if let Some(config) = config {
        encoder.str(model::CONFIG, config);
    }
}

/// Writes one approval record.
pub fn encode_approval(out: &mut Vec<u8>, record: &Approval) {
    let mut encoder = Encoder::new(out);
    encoder.str(approval::CALL_ID, &record.id);
    encoder.u64(
        approval::KIND,
        match record.kind {
            ApprovalKind::Wait => APPROVAL_WAIT,
            ApprovalKind::Run => APPROVAL_RUN,
            ApprovalKind::Refused => APPROVAL_REFUSED,
        },
    );
}

/// Writes one settings change. A field the change leaves alone is not written.
pub fn encode_settings(out: &mut Vec<u8>, record: &Settings) {
    let mut encoder = Encoder::new(out);
    if let Some(level) = &record.level {
        encoder.str(settings::LEVEL, level);
    }
    if let Some(mode) = &record.mode {
        encoder.str(settings::MODE, mode);
    }
}

fn decode_call_start(payload: &Bytes) -> io::Result<CallStart> {
    let mut decoder = Decoder::new(payload);
    let mut call = CallStart {
        id: String::new(),
        name: String::new(),
        class: EffectClass::Read,
    };
    while let Some((field, value)) = next(&mut decoder)? {
        match field {
            call_start::CALL_ID => call.id = string_of(&value)?,
            call_start::TOOL_NAME => call.name = string_of(&value)?,
            call_start::EFFECT => call.class = class_of(varint_of(&value)?),
            _ => {}
        }
    }
    Ok(call)
}

fn decode_turn_end(payload: &Bytes) -> io::Result<TurnEnd> {
    let mut decoder = Decoder::new(payload);
    let mut turn_end = TurnEnd {
        turn: 0,
        usage: Usage::default(),
    };
    while let Some((field, value)) = next(&mut decoder)? {
        match field {
            turn_end::TURN => turn_end.turn = varint_of(&value)?,
            turn_end::INPUT_TOKENS => turn_end.usage.input_tokens = varint_of(&value)?,
            turn_end::OUTPUT_TOKENS => turn_end.usage.output_tokens = varint_of(&value)?,
            turn_end::CACHE_READ_TOKENS => {
                turn_end.usage.cache_read_tokens = varint_of(&value)?;
            }
            turn_end::CACHE_WRITE_TOKENS => {
                turn_end.usage.cache_write_tokens = varint_of(&value)?;
            }
            turn_end::REASONING_TOKENS => {
                turn_end.usage.reasoning_tokens = varint_of(&value)?;
            }
            turn_end::COST_BITS => turn_end.usage.cost = Some(f64::from_bits(varint_of(&value)?)),
            _ => {}
        }
    }
    Ok(turn_end)
}

fn decode_base(payload: &Bytes) -> io::Result<State> {
    let mut decoder = Decoder::new(payload);
    let mut state = State::default();
    while let Some((field, value)) = next(&mut decoder)? {
        match field {
            base::PROVIDER => state.provider = string_of(&value)?,
            base::MODEL => state.model = string_of(&value)?,
            base::COMPACTION => state.compaction = u32_of(varint_of(&value)?)?,
            base::LEDGER => {
                let body = bytes_of(&value)?;
                state.ledger.push(decode_ledger_entry(&body)?);
            }
            base::TOUCHED => state.touched.push(string_of(&value)?),
            _ => {}
        }
    }
    Ok(state)
}

fn decode_ledger_entry(payload: &Bytes) -> io::Result<LedgerEntry> {
    let mut decoder = Decoder::new(payload);
    let mut entry = LedgerEntry {
        path: String::new(),
        start: 0,
        end: 0,
        size: 0,
        modified_ms: None,
        repeats: 0,
    };
    while let Some((field, value)) = next(&mut decoder)? {
        match field {
            base::ledger::PATH => entry.path = string_of(&value)?,
            base::ledger::START => entry.start = varint_of(&value)?,
            base::ledger::END => entry.end = varint_of(&value)?,
            base::ledger::SIZE => entry.size = varint_of(&value)?,
            base::ledger::MODIFIED_MS => entry.modified_ms = Some(varint_of(&value)?),
            base::ledger::REPEATS => entry.repeats = u32_of(varint_of(&value)?)?,
            _ => {}
        }
    }
    Ok(entry)
}

fn decode_model(payload: &Bytes) -> io::Result<Model> {
    let mut decoder = Decoder::new(payload);
    let mut record = Model {
        provider: String::new(),
        model: String::new(),
        config: None,
    };
    while let Some((field, value)) = next(&mut decoder)? {
        match field {
            model::PROVIDER => record.provider = string_of(&value)?,
            model::MODEL => record.model = string_of(&value)?,
            model::CONFIG => record.config = Some(text_of(&value)?),
            _ => {}
        }
    }
    Ok(record)
}

fn decode_approval(payload: &Bytes) -> io::Result<Approval> {
    let mut decoder = Decoder::new(payload);
    let mut record = Approval {
        id: String::new(),
        kind: ApprovalKind::Wait,
    };
    while let Some((field, value)) = next(&mut decoder)? {
        match field {
            approval::CALL_ID => record.id = string_of(&value)?,
            approval::KIND => {
                record.kind = match varint_of(&value)? {
                    APPROVAL_RUN => ApprovalKind::Run,
                    APPROVAL_REFUSED => ApprovalKind::Refused,
                    _ => ApprovalKind::Wait,
                };
            }
            _ => {}
        }
    }
    Ok(record)
}

fn decode_settings(payload: &Bytes) -> io::Result<Settings> {
    let mut decoder = Decoder::new(payload);
    let mut record = Settings::default();
    while let Some((field, value)) = next(&mut decoder)? {
        match field {
            settings::LEVEL => record.level = Some(string_of(&value)?),
            settings::MODE => record.mode = Some(string_of(&value)?),
            _ => {}
        }
    }
    Ok(record)
}

/// The byte an effect class is written as.
fn class_byte(class: EffectClass) -> u64 {
    match class {
        EffectClass::Read => 0,
        EffectClass::LocalReversible => 1,
        EffectClass::LocalIrreversible => 2,
        EffectClass::External => 3,
    }
}

/// The effect class of a byte; an unknown one is the strictest, so a call of
/// a class this build does not know is never rerun.
fn class_of(value: u64) -> EffectClass {
    match value {
        0 => EffectClass::Read,
        1 => EffectClass::LocalReversible,
        2 => EffectClass::LocalIrreversible,
        _ => EffectClass::External,
    }
}

/// The next field of `decoder`, as an I/O error when it is malformed.
fn next(decoder: &mut Decoder<'_>) -> io::Result<Option<(u32, Value)>> {
    decoder.next_field().map_err(decode_error)
}

fn decode_error(error: DecodeError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn varint_of(value: &Value) -> io::Result<u64> {
    value
        .varint()
        .ok_or_else(|| invalid("a varint field carried another wire type"))
}

fn bytes_of(value: &Value) -> io::Result<Bytes> {
    value
        .bytes()
        .cloned()
        .ok_or_else(|| invalid("a nested field carried another wire type"))
}

fn string_of(value: &Value) -> io::Result<String> {
    text_bytes(value).map(str::to_owned)
}

fn text_of(value: &Value) -> io::Result<Text> {
    text_bytes(value).map(Text::from)
}

/// The bytes of a string field, as text.
fn text_bytes(value: &Value) -> io::Result<&str> {
    let bytes = value
        .bytes()
        .ok_or_else(|| invalid("a string field carried another wire type"))?;
    std::str::from_utf8(bytes).map_err(|_| invalid("a string field was not UTF-8"))
}

fn u32_of(value: u64) -> io::Result<u32> {
    u32::try_from(value).map_err(|_| invalid("a count field was past u32"))
}
