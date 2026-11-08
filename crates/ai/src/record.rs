//! Encoding and decoding of the message record payload.
//!
//! One record is one [`Message`] of a conversation: a user message, an
//! assistant reply, or a tool result. A payload is a protobuf-style list of
//! fields, written over [`reagent_store`]'s field codec; the numbers are in the
//! private `message` module below. A decoder skips a field it does not know, and
//! a caller skips a record of a kind it does not know, so an old build reads a
//! newer log.
//!
//! The other journal records (call start, turn end, base, model, approval,
//! settings) carry agent types and stay in `reagent_agent::journal::record`,
//! which calls [`encode_message`] and [`decode_message`] from here.
//!
//! The field numbers match `crates/agent/src/journal/registry.rs`, whose golden
//! test lists every field the journal writes. The fixed-byte tests at the end
//! of this module fail if either copy moves, so a later change cannot alter the
//! format by accident and old journals read the same.

use std::io;
use std::path::Path;

use bytes::Bytes;
use reagent_store::{DecodeError, Decoder, Encoder, Entry, Session, Value};

use crate::{
    AssistantMessage, Content, Image, ImageType, Message, StopReason, Text, ToolCall, Usage,
};

/// The kind byte of a message record; the first record kind of the store.
pub const MESSAGE: u8 = reagent_store::kind::RECORD_FIRST;

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

/// Fields of one message record. The journal's field registry re-exports
/// this module, and its golden test lists every number.
pub mod message {
    /// Which of the three message shapes it is: 1 user, 2 assistant, 3 tool result.
    pub const ROLE: u32 = 1;
    /// One content block of a user or assistant message; repeated.
    pub const CONTENT: u32 = 2;
    /// The provider that produced an assistant reply.
    pub const PROVIDER: u32 = 3;
    /// The model that produced an assistant reply.
    pub const MODEL: u32 = 4;
    /// Why an assistant reply ended: 1 end turn, 2 tool use, 3 max tokens, 4 refusal, 5 error.
    pub const STOP_REASON: u32 = 5;
    /// Input tokens of an assistant reply's usage.
    pub const INPUT_TOKENS: u32 = 6;
    /// Output tokens of an assistant reply's usage.
    pub const OUTPUT_TOKENS: u32 = 7;
    /// Tokens read from the prompt cache.
    pub const CACHE_READ_TOKENS: u32 = 8;
    /// Tokens written to the prompt cache.
    pub const CACHE_WRITE_TOKENS: u32 = 9;
    /// Tokens spent on private reasoning.
    pub const REASONING_TOKENS: u32 = 10;
    /// The cost in USD, as the bits of its `f64`.
    pub const COST_BITS: u32 = 11;
    /// The wire fields an assistant reply needs back, as JSON.
    pub const REPLAY: u32 = 12;
    /// The call a tool result answers.
    pub const CALL_ID: u32 = 13;
    /// The text a tool result carries.
    pub const RESULT: u32 = 14;
    /// Whether a tool result is a failure.
    pub const IS_ERROR: u32 = 15;
    /// One image a tool result carries, in call order; repeated.
    pub const IMAGE: u32 = 16;

    /// Fields of one image inside [`IMAGE`].
    ///
    /// The numbers start at 32, above the fields of the message itself, so
    /// this one flat table has no repeated pair.
    pub mod image {
        /// The image format: 1 png, 2 jpeg, 3 gif, 4 webp.
        pub const MEDIA_TYPE: u32 = 44;
        /// The width in pixels.
        pub const WIDTH: u32 = 45;
        /// The height in pixels.
        pub const HEIGHT: u32 = 46;
        /// The image bytes as base64 text.
        pub const DATA: u32 = 47;
    }

    /// Fields of one content block inside [`CONTENT`].
    ///
    /// The numbers start at 32, above the fields of the message itself, so
    /// this one flat table has no repeated pair.
    pub mod content {
        /// The block kind: 1 text, 2 thinking, 3 redacted thinking, 4 tool call, 5 image.
        pub const KIND: u32 = 32;
        /// The visible text of a text block.
        pub const TEXT: u32 = 33;
        /// The reasoning text of a thinking block.
        pub const THINKING: u32 = 34;
        /// The provider token a thinking block is sent back with.
        pub const SIGNATURE: u32 = 35;
        /// The opaque payload of a redacted thinking block.
        pub const DATA: u32 = 36;
        /// The call id of a tool call block.
        pub const CALL_ID: u32 = 37;
        /// The tool name of a tool call block.
        pub const NAME: u32 = 38;
        /// The JSON arguments of a tool call block.
        pub const ARGUMENTS: u32 = 39;
        /// The image format of an image block: 1 png, 2 jpeg, 3 gif, 4 webp.
        pub const MEDIA_TYPE: u32 = 40;
        /// The width of an image block, in pixels.
        pub const WIDTH: u32 = 41;
        /// The height of an image block, in pixels.
        pub const HEIGHT: u32 = 42;
        /// The image bytes of an image block, as base64 text.
        pub const IMAGE_DATA: u32 = 43;
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

/// Reads one message.
pub fn decode_message(payload: &Bytes) -> io::Result<Message> {
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

fn u32_of(value: u64) -> io::Result<u32> {
    u32::try_from(value).map_err(|_| invalid("a count field was past u32"))
}

/// The first user message of the session in `dir` that has text, joined from
/// its text blocks. A session that has none answers `None`.
///
/// It reads files and blocks, and it stops at that message, which is near the
/// start of a journal: a compaction restates it first.
pub fn first_user_text(dir: &Path) -> io::Result<Option<String>> {
    let session = Session::open(dir)?;
    for record in session.records() {
        let record = record?;
        if record.kind != MESSAGE {
            continue;
        }
        if let Message::User { content } = decode_message(&record.payload)? {
            let mut text = String::new();
            for block in &content {
                if let Content::Text { text: words } = block {
                    text.push_str(words);
                }
            }
            if !text.trim().is_empty() {
                return Ok(Some(text));
            }
        }
    }
    Ok(None)
}

/// Gives each entry of `folder` with no title its first user message, cut as a
/// title. A session that cannot be read keeps its empty title: a list still
/// shows it, and it blocks as [`first_user_text`] does.
pub fn fill_titles(folder: &Path, entries: &mut [Entry]) {
    for entry in entries.iter_mut().filter(|entry| entry.title.is_empty()) {
        if let Ok(Some(text)) = first_user_text(&folder.join(&entry.session)) {
            entry.title = reagent_store::list::title(&text);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

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
    fn round_trip(message: &Message) -> Message {
        let mut out = Vec::new();
        encode_message(&mut out, message);
        decode_message(&Bytes::from(out)).expect("decode")
    }

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

    #[test]
    fn a_user_image_and_a_tool_result_record_round_trip() {
        let user =
            Message::user_content(vec![Content::text("look"), Content::Image(image("aGk="))]);
        assert_eq!(round_trip(&user), user);
        let result = Message::tool_result_with_images(
            "call_1",
            "two frames",
            false,
            vec![image("aGk="), image("bG8=")],
        );
        assert_eq!(round_trip(&result), result);
    }

    #[test]
    fn an_image_record_makes_one_text_for_each_image() {
        let result = Message::tool_result_with_images(
            "call_1",
            "two frames",
            false,
            vec![image("aGk="), image("bG8=")],
        );
        let Message::ToolResult(decoded) = round_trip(&result) else {
            panic!("a tool result");
        };
        assert_eq!(decoded.images.len(), 2);
        assert_eq!(decoded.images[1].data.as_ref(), "bG8=");
        assert!(
            !Arc::ptr_eq(&decoded.images[0].data, &decoded.images[1].data),
            "one text for each image"
        );
    }

    #[test]
    fn every_message_record_shape_round_trips() {
        for message in [
            user_with_content(),
            assistant_with_everything(),
            Message::assistant(AssistantMessage::new("fake", "m", Vec::new())),
            Message::tool_result("call_1", "text", false),
            Message::tool_result("call_2", "boom", true),
        ] {
            assert_eq!(round_trip(&message), message);
        }
    }

    #[test]
    fn an_empty_content_text_record_round_trips() {
        let mut reply = AssistantMessage::new("fake", "m", Vec::new());
        reply.stop_reason = Some(StopReason::Error);
        let message = Message::assistant(reply);
        assert_eq!(round_trip(&message), message);
    }

    #[test]
    fn an_unknown_field_is_skipped_in_a_message_record() {
        let message = Message::user("hello");
        let mut out = Vec::new();
        encode_message(&mut out, &message);
        let mut encoder = Encoder::new(&mut out);
        encoder.u64(99, 7);
        encoder.str(98, "a field from a later build");
        let decoded = decode_message(&Bytes::from(out)).expect("decode");
        assert_eq!(decoded, message);
    }

    #[test]
    fn the_record_bytes_of_an_assistant_reply_do_not_change() {
        let recorded: &[u8] = &[
            8, 2, 18, 7, 128, 2, 1, 138, 2, 1, b'h', 26, 4, b'f', b'a', b'k', b'e', 34, 1, b'm',
            40, 2, 48, 3, 56, 4, 64, 5, 72, 6, 80, 7, 98, 7, b'{', b'"', b'k', b'"', b':', b'1',
            b'}',
        ];
        let mut reply = AssistantMessage::new("fake", "m", vec![Content::text("h")]);
        reply.stop_reason = Some(StopReason::ToolUse);
        reply.usage = Usage {
            input_tokens: 3,
            output_tokens: 4,
            cache_read_tokens: 5,
            cache_write_tokens: 6,
            reasoning_tokens: 7,
            cost: None,
        };
        reply.replay = Some(r#"{"k":1}"#.into());
        let message = Message::assistant(reply);
        let mut out = Vec::new();
        encode_message(&mut out, &message);
        assert_eq!(out, recorded, "the encoder moved a byte");
        assert_eq!(
            decode_message(&Bytes::from_static(recorded)).expect("decode"),
            message
        );
    }

    #[test]
    fn the_record_bytes_of_a_tool_result_with_an_image_do_not_change() {
        let recorded: &[u8] = &[
            8, 3, 106, 6, b'c', b'a', b'l', b'l', b'_', b'1', 114, 2, b'o', b'k', 120, 0, 130, 1,
            16, 224, 2, 1, 232, 2, 2, 240, 2, 3, 250, 2, 4, b'a', b'G', b'k', b'=',
        ];
        let message = Message::tool_result_with_images("call_1", "ok", false, vec![image("aGk=")]);
        let mut out = Vec::new();
        encode_message(&mut out, &message);
        assert_eq!(out, recorded, "the encoder moved a byte");
        assert_eq!(
            decode_message(&Bytes::from_static(recorded)).expect("decode"),
            message
        );
    }

    /// A folder with one session of the given messages, under `folder`.
    fn session_of(folder: &Path, id: &str, messages: &[Message]) {
        let dir = folder.join(id);
        let header = reagent_store::Header::new(id.to_owned(), 0, None, Bytes::new());
        let mut writer = reagent_store::Writer::create(&dir, header).expect("a writer");
        for message in messages {
            let mut payload = Vec::new();
            encode_message(&mut payload, message);
            writer
                .append(MESSAGE, Bytes::from(payload), false)
                .expect("a record");
        }
        writer.commit().expect("commit");
        writer.close().expect("close");
    }

    /// An entry of `id` with `title`.
    fn entry_of(id: &str, title: &str) -> Entry {
        Entry {
            session: id.to_owned(),
            updated_ms: 0,
            turns: 0,
            title: title.to_owned(),
            profile: String::new(),
            provider: String::new(),
            model: String::new(),
            part: 0,
            state: reagent_store::State::Done,
            headless: false,
        }
    }

    /// The first message with words wins, past one that holds only an image.
    #[test]
    fn session_title_is_the_first_user_text() {
        let root = tempfile::tempdir().expect("a temp folder");
        let image = Content::Image(Image {
            media_type: ImageType::Png,
            width: 1,
            height: 1,
            data: "AAAA".into(),
        });
        session_of(
            root.path(),
            "a",
            &[
                Message::user_content(vec![image]),
                Message::user("the first words".to_owned()),
                Message::user("later words".to_owned()),
            ],
        );
        let text = first_user_text(&root.path().join("a")).expect("the read");
        assert_eq!(text.as_deref(), Some("the first words"));
    }

    /// Only a row with no title reads a journal, and a row that has nothing to
    /// read stays empty.
    #[test]
    fn session_title_fills_only_the_empty_entries() {
        let root = tempfile::tempdir().expect("a temp folder");
        session_of(
            root.path(),
            "empty",
            &[Message::user("first\nwords".to_owned())],
        );
        session_of(root.path(), "silent", &[]);
        let mut entries = [
            entry_of("empty", ""),
            entry_of("silent", ""),
            entry_of("gone", ""),
            entry_of("titled-with-no-folder", "kept"),
        ];
        fill_titles(root.path(), &mut entries);
        assert_eq!(entries[0].title, "first words", "cut as a title");
        assert_eq!(entries[1].title, "", "no words yet");
        assert_eq!(entries[2].title, "", "no folder");
        assert_eq!(entries[3].title, "kept");
    }
}
