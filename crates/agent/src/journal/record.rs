//! Encoding and decoding of the record payloads, field by field.
//!
//! A payload is a protobuf-style list of fields; the numbers are in
//! [`crate::journal::registry`]. A decoder skips a field it does not know and
//! a record of a kind it does not know, so an old build reads a newer log.

use std::io;

use bytes::Bytes;
use reagent_ai::{
    AssistantMessage, Content, Image, ImageType, Message, StopReason, Text, ToolCall, Usage,
};
use reagent_store::{DecodeError, Decoder, Encoder, Value};

use crate::journal::kinds;
use crate::journal::registry::{base, call_start, message, model, turn_end};
use crate::journal::state::{LedgerEntry, State};
use crate::tool::EffectClass;

/// The role byte of a user message.
const ROLE_USER: u64 = 1;
/// The role byte of an assistant reply.
const ROLE_ASSISTANT: u64 = 2;
/// The role byte of a tool result.
const ROLE_TOOL_RESULT: u64 = 3;

/// The content block byte of a text block.
const BLOCK_TEXT: u64 = 1;
/// The content block byte of a thinking block.
const BLOCK_THINKING: u64 = 2;
/// The content block byte of a redacted thinking block.
const BLOCK_REDACTED: u64 = 3;
/// The content block byte of a tool call block.
const BLOCK_TOOL_CALL: u64 = 4;
/// The content block byte of an image block.
const BLOCK_IMAGE: u64 = 5;

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
}

/// Decodes one record of `kind`.
pub fn decode(kind: u8, payload: &Bytes) -> io::Result<Record> {
    match kind {
        kinds::MESSAGE => decode_message(payload).map(Record::Message),
        kinds::CALL_START => decode_call_start(payload).map(Record::CallStart),
        kinds::TURN_END => decode_turn_end(payload).map(Record::TurnEnd),
        kinds::BASE => decode_base(payload).map(Record::Base),
        kinds::MODEL => decode_model(payload).map(Record::Model),
        other => Ok(Record::Unknown(other)),
    }
}

/// Writes one message.
pub fn encode_message(out: &mut Vec<u8>, message: &Message) {
    let mut encoder = Encoder::new(out);
    match message {
        Message::User { content } => {
            encoder.u64(message::ROLE, ROLE_USER);
            for block in content {
                encoder.message(message::CONTENT, |inner| encode_content(inner, block));
            }
        }
        Message::Assistant(reply) => {
            encoder.u64(message::ROLE, ROLE_ASSISTANT);
            for block in &reply.content {
                encoder.message(message::CONTENT, |inner| encode_content(inner, block));
            }
            encoder.str(message::PROVIDER, &reply.provider);
            encoder.str(message::MODEL, &reply.model);
            if let Some(stop) = reply.stop_reason {
                encoder.u64(message::STOP_REASON, stop_byte(stop));
            }
            encoder.u64(message::INPUT_TOKENS, reply.usage.input_tokens);
            encoder.u64(message::OUTPUT_TOKENS, reply.usage.output_tokens);
            encoder.u64(message::CACHE_READ_TOKENS, reply.usage.cache_read_tokens);
            encoder.u64(message::CACHE_WRITE_TOKENS, reply.usage.cache_write_tokens);
            encoder.u64(message::REASONING_TOKENS, reply.usage.reasoning_tokens);
            if let Some(cost) = reply.usage.cost {
                encoder.u64(message::COST_BITS, cost.to_bits());
            }
            if let Some(replay) = &reply.replay {
                encoder.str(message::REPLAY, replay);
            }
        }
        Message::ToolResult(result) => {
            encoder.u64(message::ROLE, ROLE_TOOL_RESULT);
            encoder.str(message::CALL_ID, &result.call_id);
            encoder.str(message::RESULT, &result.content);
            encoder.bool(message::IS_ERROR, result.is_error);
            for image in &result.images {
                encoder.message(message::IMAGE, |inner| encode_image(inner, image));
            }
        }
    }
}

/// Writes one image, inside a [`message::IMAGE`] field.
fn encode_image(encoder: &mut Encoder<'_>, image: &Image) {
    encoder.u64(
        message::image::MEDIA_TYPE,
        image_type_byte(image.media_type),
    );
    encoder.u64(message::image::WIDTH, u64::from(image.width));
    encoder.u64(message::image::HEIGHT, u64::from(image.height));
    encoder.str(message::image::DATA, &image.data);
}

/// Writes one content block, inside a [`message::CONTENT`] field.
fn encode_content(encoder: &mut Encoder<'_>, block: &Content) {
    match block {
        Content::Text { text } => {
            encoder.u64(message::content::KIND, BLOCK_TEXT);
            encoder.str(message::content::TEXT, text);
        }
        Content::Thinking {
            thinking,
            signature,
        } => {
            encoder.u64(message::content::KIND, BLOCK_THINKING);
            encoder.str(message::content::THINKING, thinking);
            if let Some(signature) = signature {
                encoder.str(message::content::SIGNATURE, signature);
            }
        }
        Content::RedactedThinking { data } => {
            encoder.u64(message::content::KIND, BLOCK_REDACTED);
            encoder.str(message::content::DATA, data);
        }
        Content::ToolCall(call) => {
            encoder.u64(message::content::KIND, BLOCK_TOOL_CALL);
            encoder.str(message::content::CALL_ID, &call.id);
            encoder.str(message::content::NAME, &call.name);
            encoder.str(message::content::ARGUMENTS, &call.arguments);
        }
        Content::Image(image) => {
            encoder.u64(message::content::KIND, BLOCK_IMAGE);
            encoder.u64(
                message::content::MEDIA_TYPE,
                image_type_byte(image.media_type),
            );
            encoder.u64(message::content::WIDTH, u64::from(image.width));
            encoder.u64(message::content::HEIGHT, u64::from(image.height));
            encoder.str(message::content::IMAGE_DATA, &image.data);
        }
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

/// Writes one change of provider and model.
pub fn encode_model(out: &mut Vec<u8>, provider: &str, model: &str) {
    let mut encoder = Encoder::new(out);
    encoder.str(model::PROVIDER, provider);
    encoder.str(model::MODEL, model);
}

fn decode_message(payload: &Bytes) -> io::Result<Message> {
    let mut decoder = Decoder::new(payload);
    let mut role = None;
    let mut content = Vec::new();
    let mut provider = None;
    let mut model = None;
    let mut stop_reason = None;
    let mut usage = Usage::default();
    let mut replay = None;
    let mut call_id = None;
    let mut result = None;
    let mut is_error = false;
    let mut images = Vec::new();
    while let Some((field, value)) = next(&mut decoder)? {
        match field {
            message::ROLE => role = Some(varint_of(&value)?),
            message::CONTENT => {
                let body = bytes_of(&value)?;
                content.push(decode_content(&body)?);
            }
            message::PROVIDER => provider = Some(text_of(&value)?),
            message::MODEL => model = Some(text_of(&value)?),
            message::STOP_REASON => stop_reason = Some(stop_of(varint_of(&value)?)?),
            message::INPUT_TOKENS => usage.input_tokens = varint_of(&value)?,
            message::OUTPUT_TOKENS => usage.output_tokens = varint_of(&value)?,
            message::CACHE_READ_TOKENS => usage.cache_read_tokens = varint_of(&value)?,
            message::CACHE_WRITE_TOKENS => usage.cache_write_tokens = varint_of(&value)?,
            message::REASONING_TOKENS => usage.reasoning_tokens = varint_of(&value)?,
            message::COST_BITS => usage.cost = Some(f64::from_bits(varint_of(&value)?)),
            message::REPLAY => replay = Some(text_of(&value)?),
            message::CALL_ID => call_id = Some(text_of(&value)?),
            message::RESULT => result = Some(text_of(&value)?),
            message::IS_ERROR => is_error = varint_of(&value)? != 0,
            message::IMAGE => {
                let body = bytes_of(&value)?;
                images.push(decode_image(&body)?);
            }
            _ => {}
        }
    }
    match role {
        Some(ROLE_USER) => Ok(Message::user_content(content)),
        Some(ROLE_ASSISTANT) => {
            let mut reply = AssistantMessage::new(or_empty(provider), or_empty(model), content);
            reply.stop_reason = stop_reason;
            reply.usage = usage;
            reply.replay = replay;
            Ok(Message::assistant(reply))
        }
        Some(ROLE_TOOL_RESULT) => Ok(Message::tool_result_with_images(
            or_empty(call_id),
            or_empty(result),
            is_error,
            images,
        )),
        Some(other) => Err(invalid(format!("message has an unknown role {other}"))),
        None => Err(invalid("message has no role")),
    }
}

fn decode_content(payload: &Bytes) -> io::Result<Content> {
    let mut decoder = Decoder::new(payload);
    let mut kind = None;
    let mut text = None;
    let mut thinking = None;
    let mut signature = None;
    let mut data = None;
    let mut call_id = None;
    let mut name = None;
    let mut arguments = None;
    let mut media_type = None;
    let mut width = None;
    let mut height = None;
    let mut image_data = None;
    while let Some((field, value)) = next(&mut decoder)? {
        match field {
            message::content::KIND => kind = Some(varint_of(&value)?),
            message::content::TEXT => text = Some(text_of(&value)?),
            message::content::THINKING => thinking = Some(text_of(&value)?),
            message::content::SIGNATURE => signature = Some(text_of(&value)?),
            message::content::DATA => data = Some(text_of(&value)?),
            message::content::CALL_ID => call_id = Some(text_of(&value)?),
            message::content::NAME => name = Some(text_of(&value)?),
            message::content::ARGUMENTS => arguments = Some(text_of(&value)?),
            message::content::MEDIA_TYPE => media_type = Some(varint_of(&value)?),
            message::content::WIDTH => width = Some(varint_of(&value)?),
            message::content::HEIGHT => height = Some(varint_of(&value)?),
            message::content::IMAGE_DATA => image_data = Some(text_of(&value)?),
            _ => {}
        }
    }
    match kind {
        Some(BLOCK_TEXT) => Ok(Content::Text {
            text: or_empty(text),
        }),
        Some(BLOCK_THINKING) => Ok(Content::Thinking {
            thinking: or_empty(thinking),
            signature,
        }),
        Some(BLOCK_REDACTED) => Ok(Content::RedactedThinking {
            data: or_empty(data),
        }),
        Some(BLOCK_TOOL_CALL) => Ok(Content::ToolCall(ToolCall {
            id: or_empty(call_id),
            name: or_empty(name),
            arguments: or_empty(arguments),
        })),
        Some(BLOCK_IMAGE) => Ok(Content::Image(Image {
            media_type: image_type_of(media_type)?,
            width: u32_of(width.unwrap_or(0))?,
            height: u32_of(height.unwrap_or(0))?,
            data: or_empty(image_data),
        })),
        Some(other) => Err(invalid(format!(
            "message has an unknown content block {other}"
        ))),
        None => Err(invalid("message content block has no kind")),
    }
}

/// Reads one image, inside a [`message::IMAGE`] field.
fn decode_image(payload: &Bytes) -> io::Result<Image> {
    let mut decoder = Decoder::new(payload);
    let mut media_type = None;
    let mut width = None;
    let mut height = None;
    let mut data = None;
    while let Some((field, value)) = next(&mut decoder)? {
        match field {
            message::image::MEDIA_TYPE => media_type = Some(varint_of(&value)?),
            message::image::WIDTH => width = Some(varint_of(&value)?),
            message::image::HEIGHT => height = Some(varint_of(&value)?),
            message::image::DATA => data = Some(text_of(&value)?),
            _ => {}
        }
    }
    Ok(Image {
        media_type: image_type_of(media_type)?,
        width: u32_of(width.unwrap_or(0))?,
        height: u32_of(height.unwrap_or(0))?,
        data: or_empty(data),
    })
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
    };
    while let Some((field, value)) = next(&mut decoder)? {
        match field {
            model::PROVIDER => record.provider = string_of(&value)?,
            model::MODEL => record.model = string_of(&value)?,
            _ => {}
        }
    }
    Ok(record)
}

/// The byte an image format is written as.
fn image_type_byte(kind: ImageType) -> u64 {
    match kind {
        ImageType::Png => 1,
        ImageType::Jpeg => 2,
        ImageType::Gif => 3,
        ImageType::Webp => 4,
    }
}

/// The image format of a byte.
fn image_type_of(value: Option<u64>) -> io::Result<ImageType> {
    match value {
        Some(1) => Ok(ImageType::Png),
        Some(2) => Ok(ImageType::Jpeg),
        Some(3) => Ok(ImageType::Gif),
        Some(4) => Ok(ImageType::Webp),
        Some(other) => Err(invalid(format!("unknown image media type {other}"))),
        None => Err(invalid("image has no media type")),
    }
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

/// The byte a stop reason is written as.
fn stop_byte(stop: StopReason) -> u64 {
    match stop {
        StopReason::EndTurn => 1,
        StopReason::ToolUse => 2,
        StopReason::MaxTokens => 3,
        StopReason::Refusal => 4,
        StopReason::Error => 5,
    }
}

/// The stop reason of a byte.
fn stop_of(value: u64) -> io::Result<StopReason> {
    match value {
        1 => Ok(StopReason::EndTurn),
        2 => Ok(StopReason::ToolUse),
        3 => Ok(StopReason::MaxTokens),
        4 => Ok(StopReason::Refusal),
        5 => Ok(StopReason::Error),
        other => Err(invalid(format!("unknown stop reason {other}"))),
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

/// A text field as message text: one copy out of the record.
fn text_of(value: &Value) -> io::Result<Text> {
    let bytes = value
        .bytes()
        .ok_or_else(|| invalid("a string field carried another wire type"))?;
    std::str::from_utf8(bytes)
        .map(Text::from)
        .map_err(|_| invalid("a string field was not UTF-8"))
}

/// A text field the record left out: empty.
fn or_empty(text: Option<Text>) -> Text {
    text.unwrap_or_else(|| Text::from(""))
}

fn string_of(value: &Value) -> io::Result<String> {
    let bytes = value
        .bytes()
        .ok_or_else(|| invalid("a string field carried another wire type"))?;
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| invalid("a string field was not UTF-8"))
}

fn u32_of(value: u64) -> io::Result<u32> {
    u32::try_from(value).map_err(|_| invalid("a count field was past u32"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::journal::kinds;

    /// One image, with base64 text the journal stores as it stands.
    fn image(data: &str) -> Image {
        Image {
            media_type: ImageType::Png,
            width: 2,
            height: 3,
            data: data.into(),
        }
    }

    /// One message through the encoder and back.
    fn round_trip(message: &Message) -> Record {
        let mut out = Vec::new();
        encode_message(&mut out, message);
        decode(kinds::MESSAGE, &Bytes::from(out)).expect("decode")
    }

    #[test]
    fn a_user_image_and_a_tool_result_with_two_images_round_trip() {
        let user =
            Message::user_content(vec![Content::text("look"), Content::Image(image("aGk="))]);
        assert_eq!(round_trip(&user), Record::Message(user));
        let result = Message::tool_result_with_images(
            "call_1",
            "two frames",
            false,
            vec![image("aGk="), image("bG8=")],
        );
        assert_eq!(round_trip(&result), Record::Message(result));
    }

    #[test]
    fn an_image_record_makes_one_text_for_each_image() {
        let result = Message::tool_result_with_images(
            "call_1",
            "two frames",
            false,
            vec![image("aGk="), image("bG8=")],
        );
        let Record::Message(Message::ToolResult(decoded)) = round_trip(&result) else {
            panic!("a tool result");
        };
        assert_eq!(decoded.images.len(), 2);
        assert_eq!(decoded.images[1].data.as_ref(), "bG8=");
        assert!(
            !Arc::ptr_eq(&decoded.images[0].data, &decoded.images[1].data),
            "one text for each image"
        );
    }
}
