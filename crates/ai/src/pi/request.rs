//! Builds pi-messages request bodies.

use std::borrow::Cow;

use bytes::Bytes;
use serde::Serialize;
use serde_json::value::RawValue;

use crate::Error;
use crate::openai::{BodyParts, JsonStr};
use crate::request_images::NO_IMAGES;
use crate::types::{AssistantMessage, Content, Image, Message, StopReason, ToolResult, ToolSpec};

use super::stream::level_of;
use super::{API, Replay, UPSTREAM};

/// One message serialized in pi-messages form, reusable across requests.
///
/// The message is written once into a buffer of the size it takes, and the
/// `Bytes` that holds it adopts that buffer, so the body of a turn is the one
/// copy its message made and nothing else.
#[derive(Debug, Clone)]
pub struct MessageBody {
    bytes: Bytes,
}

impl MessageBody {
    /// Serializes one message for the provider named `provider`.
    ///
    /// `names` maps the call id of every tool call in the history to its tool
    /// name, which a tool result sends back. Thinking from another provider is
    /// dropped, because its signature is not valid here. Images are sent.
    ///
    /// `timestamp` is the fixed counter of the message's position, which the
    /// request builder hands over: the body cache is keyed on the position, so
    /// the bytes of a message are what that position sent on the turn before.
    pub fn new(
        message: &Message,
        provider: &str,
        names: &[(&str, &str)],
        timestamp: u64,
    ) -> Result<Self, Error> {
        Self::with_images(message, provider, names, timestamp, true)
    }

    /// Serializes one message for the provider named `provider`, sending its
    /// images only when `images` and writing the note of
    /// [`crate::request_images::NO_IMAGES`] in their place when not.
    pub fn with_images(
        message: &Message,
        provider: &str,
        names: &[(&str, &str)],
        timestamp: u64,
        images: bool,
    ) -> Result<Self, Error> {
        let mut out = Vec::with_capacity(estimate(message, images));
        match message {
            Message::User { content } => user(content, images, &mut out),
            Message::Assistant(reply) => assistant(reply, provider, &mut out)?,
            Message::ToolResult(result) => tool_result(result, &mut out, names, images),
        }
        // The message object is closed; the timestamp takes the place of the
        // closing brace, so the body is whole.
        out.pop();
        out.extend_from_slice(TIMESTAMP);
        push_decimal(&mut out, timestamp);
        out.push(b'}');
        Ok(Self {
            bytes: Bytes::from(out),
        })
    }

    /// The whole message, timestamp included.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

/// Builds a pi-messages body from a stable head, the system message and the
/// cached message bodies.
///
/// The head holds the model and the options; the system message carries the
/// system prompt and the tools. Neither changes between two turns of a
/// session, so the prompt cache of the provider stays warm.
#[derive(Debug)]
pub struct RequestBuilder {
    head: Bytes,
    system: Option<Bytes>,
}

impl RequestBuilder {
    /// Serializes the request head and the system message once.
    ///
    /// `reasoning` is the run's level in the provider's own words; a budget and
    /// the word that turns reasoning off leave the option out.
    pub fn new(
        model: &str,
        max_tokens: u32,
        system: Option<&str>,
        reasoning: Option<&str>,
        session: Option<&str>,
        tools: &[ToolSpec],
    ) -> Self {
        #[derive(Serialize)]
        struct Head<'a> {
            model: &'a str,
            options: Options<'a>,
        }

        let options = Options {
            max_tokens,
            reasoning: level_of(reasoning),
            session_id: session,
        };
        let mut head =
            serde_json::to_vec(&Head { model, options }).expect("the pi-messages head serializes");
        // The closing brace of the head opens the context instead, so the
        // cached message bodies follow the fixed JSON around them.
        head.pop();
        head.extend_from_slice(b",\"context\":{\"messages\":[");

        let system = system.unwrap_or("");
        let system = if system.is_empty() && tools.is_empty() {
            None
        } else {
            let tools_added: Vec<Tool<'_>> = tools.iter().map(Tool::of).collect();
            Some(to_bytes(&SystemMessage {
                role: "system",
                content: system,
                tools_added: (!tools_added.is_empty()).then_some(tools_added),
                timestamp: 0,
            }))
        };

        Self {
            head: Bytes::from(head),
            system,
        }
    }

    /// The prefix every body built from this request shares.
    pub fn head(&self) -> &Bytes {
        &self.head
    }

    /// The system message, when the request carries a prompt or tools.
    pub fn system(&self) -> Option<&Bytes> {
        self.system.as_ref()
    }

    /// The timestamp of the message at `index` of the history.
    ///
    /// The counter is fixed per position and the system message holds the
    /// first value, so a history keeps its numbers when a turn appends to it
    /// and the bytes of the messages before it never change.
    pub fn timestamp(&self, index: usize) -> u64 {
        index as u64 + 1
    }

    /// Serializes a body from the head, the system message and the cached
    /// message bodies.
    pub fn body(&self, messages: &[MessageBody]) -> Bytes {
        self.body_parts(messages).join()
    }

    /// The body as its pieces: the head, the system message, every cached
    /// message's bytes and the separators between them, none of them copied.
    pub fn body_parts(&self, messages: &[MessageBody]) -> BodyParts {
        let visible = usize::from(self.system.is_some()) + messages.len();
        let mut parts = BodyParts::with_capacity(2 + crate::openai::item_pieces(visible));
        parts.push(self.head.clone());
        let mut first = self.system.is_none();
        if let Some(system) = &self.system {
            parts.push(system.clone());
        }
        for message in messages {
            if !first {
                parts.push_static(COMMA);
            }
            first = false;
            parts.push(message.bytes.clone());
        }
        parts.push_static(END);
        parts
    }
}

/// Every tool call id in `messages` with the name it called.
pub fn tool_names(messages: &[Message]) -> Vec<(&str, &str)> {
    let mut names = Vec::new();
    for message in messages {
        if let Message::Assistant(assistant) = message {
            for content in &assistant.content {
                if let Content::ToolCall(call) = content {
                    names.push((&*call.id, &*call.name));
                }
            }
        }
    }
    names
}

const COMMA: &[u8] = b",";
/// The field that closes a message with the counter of its position.
const TIMESTAMP: &[u8] = b",\"timestamp\":";
/// The brackets that close the message list, the context and the body.
const END: &[u8] = b"]}}";

/// The bytes left over in a message body beside its content: the field names,
/// the ids, the usage numbers and the timestamp.
const SLACK: usize = 512;

/// The bytes the body of `message` takes, before it is written.
///
/// A large tool result is most of a body, so a count within a few hundred
/// bytes of it lets the writer fill one buffer and grow none.
fn estimate(message: &Message, images: bool) -> usize {
    let content = match message {
        Message::User { content } => user_len(content, images),
        Message::Assistant(reply) => reply.content.iter().map(content_len).sum(),
        Message::ToolResult(result) => tool_result_len(result, images),
    };
    content + SLACK
}

/// The bytes of the text one user message carries: its text blocks, the
/// newlines between them, and its images or the note that replaces each one.
fn user_len(content: &[Content], images: bool) -> usize {
    content
        .iter()
        .map(|content| match content {
            Content::Text { text } => escaped_len(text),
            Content::Image(image) => image_len(image, images),
            // A user message holds no thinking and no tool call: nothing of
            // those reaches the wire, and neither carries an image.
            Content::Thinking { .. } | Content::RedactedThinking { .. } | Content::ToolCall(_) => 0,
        })
        .sum::<usize>()
        + content.len()
}

/// The bytes of the text one assistant block carries.
fn content_len(content: &Content) -> usize {
    match content {
        Content::Text { text } => escaped_len(text),
        Content::Thinking {
            thinking,
            signature,
        } => escaped_len(thinking) + signature.as_deref().map_or(0, escaped_len),
        Content::RedactedThinking { data } => escaped_len(data),
        Content::ToolCall(call) => {
            escaped_len(&call.arguments) + escaped_len(&call.id) + escaped_len(&call.name)
        }
        // A reply holds no image, but the size estimate counts one, so no image
        // is dropped without the bytes it takes being counted after all.
        Content::Image(image) => escaped_len(&image.data),
    }
}

/// The bytes one image block takes, or the note that replaces it.
fn image_len(image: &Image, images: bool) -> usize {
    if images {
        escaped_len(&image.data)
    } else {
        NO_IMAGES.len() + 1
    }
}

/// The bytes a tool result takes: its text and its images, or the note that
/// replaces each one.
fn tool_result_len(result: &ToolResult, images: bool) -> usize {
    escaped_len(&result.content)
        + result
            .images
            .iter()
            .map(|image| image_len(image, images))
            .sum::<usize>()
}

/// The bytes a JSON string of `text` takes, its escapes included.
fn escaped_len(text: &str) -> usize {
    text.bytes()
        .map(|byte| match byte {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
        .sum()
}

/// Appends the decimal digits of `value` to `out`.
fn push_decimal(out: &mut Vec<u8>, mut value: u64) {
    let mut buffer = [0u8; 20];
    let mut end = buffer.len();
    loop {
        end -= 1;
        buffer[end] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out.extend_from_slice(&buffer[end..]);
}

/// Options the request carries. A field the run does not set is left out, so
/// the head never sends a null.
#[derive(Serialize)]
struct Options<'a> {
    #[serde(rename = "maxTokens")]
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<&'a str>,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

#[derive(Serialize)]
struct SystemMessage<'a> {
    role: &'static str,
    content: &'a str,
    #[serde(rename = "toolsAdded", skip_serializing_if = "Option::is_none")]
    tools_added: Option<Vec<Tool<'a>>>,
    timestamp: u64,
}

#[derive(Serialize)]
struct Tool<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

impl<'a> Tool<'a> {
    fn of(tool: &'a ToolSpec) -> Self {
        Self {
            name: &tool.name,
            description: &tool.description,
            parameters: &tool.input_schema,
        }
    }
}

#[derive(Serialize)]
struct UserMessage<'a> {
    role: &'static str,
    content: &'a str,
}

/// A user message whose content holds an image: the text blocks and the image
/// blocks in order.
#[derive(Serialize)]
struct UserBlocks<'a> {
    role: &'static str,
    content: &'a [Block<'a>],
}

#[derive(Serialize)]
struct AssistantBody<'a> {
    role: &'static str,
    content: &'a [Block<'a>],
    api: &'static str,
    provider: &'static str,
    model: &'a str,
    #[serde(rename = "responseId", skip_serializing_if = "Option::is_none")]
    response_id: Option<&'a str>,
    #[serde(
        rename = "providerThinkingLevel",
        skip_serializing_if = "Option::is_none"
    )]
    provider_thinking_level: Option<&'a str>,
    usage: UsageBody,
    #[serde(rename = "stopReason")]
    stop_reason: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Block<'a> {
    Text {
        text: &'a str,
        #[serde(rename = "textSignature", skip_serializing_if = "Option::is_none")]
        text_signature: Option<&'a str>,
    },
    Thinking {
        thinking: &'a str,
        #[serde(rename = "thinkingSignature", skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    ToolCall {
        id: &'a str,
        name: &'a str,
        arguments: &'a RawValue,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<&'a str>,
    },
    Image {
        data: &'a str,
        #[serde(rename = "mimeType")]
        mime_type: &'static str,
    },
}

/// The content of one tool result: the text block alone, which takes no buffer,
/// when the result has no image, and the text block with an image block for
/// each image when it has one.
#[derive(Serialize)]
#[serde(untagged)]
enum ToolContent<'a> {
    /// The text block of a result with no image, on the stack so it costs no
    /// buffer.
    Single([Block<'a>; 1]),
    /// The text block and the images of a result that carries one.
    Blocks(Vec<Block<'a>>),
}

#[derive(Serialize)]
struct ToolResultBody<'a> {
    role: &'static str,
    #[serde(rename = "toolCallId")]
    tool_call_id: &'a str,
    #[serde(rename = "toolName")]
    tool_name: &'a str,
    content: ToolContent<'a>,
    #[serde(rename = "isError")]
    is_error: bool,
}

#[derive(Serialize)]
struct UsageBody {
    input: u64,
    output: u64,
    #[serde(rename = "cacheRead")]
    cache_read: u64,
    #[serde(rename = "cacheWrite")]
    cache_write: u64,
    #[serde(rename = "totalTokens")]
    total_tokens: u64,
    cost: CostBody,
}

/// What a turn cost. The endpoint reports one total, so the per-part fields
/// stay at zero.
#[derive(Serialize)]
struct CostBody {
    input: u8,
    output: u8,
    #[serde(rename = "cacheRead")]
    cache_read: u8,
    #[serde(rename = "cacheWrite")]
    cache_write: u8,
    total: f64,
}

/// Writes one user message: a string when it holds no image, and a list of
/// text and image blocks when it does.
fn user(content: &[Content], images: bool, out: &mut Vec<u8>) {
    if images
        && content
            .iter()
            .any(|content| matches!(content, Content::Image(_)))
    {
        let blocks: Vec<Block<'_>> = content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(Block::Text {
                    text,
                    text_signature: None,
                }),
                Content::Image(image) => Some(image_block(image, images)),
                // A user message holds no thinking and no tool call, and
                // neither carries an image.
                Content::Thinking { .. }
                | Content::RedactedThinking { .. }
                | Content::ToolCall(_) => None,
            })
            .collect();
        write(
            &UserBlocks {
                role: "user",
                content: &blocks,
            },
            out,
        );
        return;
    }
    write(
        &UserMessage {
            role: "user",
            content: &text_of(content, images),
        },
        out,
    );
}

/// Writes one message into the buffer the body holds.
fn write(value: &impl Serialize, out: &mut Vec<u8>) {
    serde_json::to_writer(out, value).expect("a request part serializes");
}

/// The text one user message carries: the text blocks in order, one per line,
/// and for every image the note that replaces it when the model takes none.
fn text_of(content: &[Content], images: bool) -> Cow<'_, str> {
    let mut texts = content.iter().filter_map(|content| match content {
        Content::Text { text } => Some(&**text),
        Content::Image(_) if !images => Some(NO_IMAGES),
        // An image the model takes is written as an image block, and the other
        // blocks of a user message carry no text.
        Content::Image(_) => None,
        Content::Thinking { .. } | Content::RedactedThinking { .. } | Content::ToolCall(_) => None,
    });
    let first = texts.next().unwrap_or("");
    match texts.next() {
        None => Cow::Borrowed(first),
        Some(second) => {
            let mut joined = String::from(first);
            joined.push('\n');
            joined.push_str(second);
            for text in texts {
                joined.push('\n');
                joined.push_str(text);
            }
            Cow::Owned(joined)
        }
    }
}

fn assistant(reply: &AssistantMessage, provider: &str, out: &mut Vec<u8>) -> Result<(), Error> {
    let replay = match &reply.replay {
        Some(replay) => serde_json::from_str::<Replay>(replay).map_err(Error::Json)?,
        None => Replay::default(),
    };
    let keep_thinking = reply.provider.as_ref() == provider;
    let mut blocks = Vec::with_capacity(reply.content.len());
    // The ordinal of the text block, which its signature is filed under.
    let mut texts = 0;
    for content in &reply.content {
        if content.is_thinking() && !keep_thinking {
            continue;
        }
        let block = match content {
            Content::Text { text } => {
                let signature = replay.text_signatures.get(&texts).map(JsonStr::as_str);
                texts += 1;
                Block::Text {
                    text,
                    text_signature: signature,
                }
            }
            Content::Thinking {
                thinking,
                signature,
            } => Block::Thinking {
                thinking,
                thinking_signature: signature.as_deref(),
                redacted: None,
            },
            Content::RedactedThinking { data } => Block::Thinking {
                thinking: "",
                thinking_signature: Some(data),
                redacted: Some(true),
            },
            Content::ToolCall(call) => Block::ToolCall {
                id: &call.id,
                name: &call.name,
                arguments: arguments(&call.arguments),
                thought_signature: replay.tool_signatures.get(&*call.id).map(JsonStr::as_str),
            },
            // A model's reply holds no image, and the pi wire has no place for
            // one in an assistant message.
            Content::Image(_) => continue,
        };
        blocks.push(block);
    }
    let message = AssistantBody {
        role: "assistant",
        content: &blocks,
        api: API,
        provider: UPSTREAM,
        model: &reply.model,
        response_id: replay.response_id.as_ref().map(JsonStr::as_str),
        provider_thinking_level: replay.thinking_level.as_ref().map(JsonStr::as_str),
        usage: usage_body(&reply.usage),
        stop_reason: stop_reason(reply.stop_reason),
    };
    write(&message, out);
    Ok(())
}

/// The stored arguments text as the JSON value it is, borrowed from the text
/// itself, or an empty object when the text is not one, so one broken call
/// cannot stop a session.
fn arguments(text: &str) -> &RawValue {
    serde_json::from_str(text).unwrap_or_else(|_| {
        serde_json::from_str(EMPTY_ARGUMENTS).expect("an empty object is valid JSON")
    })
}

/// The arguments of a call whose stored text is not JSON.
const EMPTY_ARGUMENTS: &str = "{}";

/// The block one image writes: its base64 text with its media type, or the
/// note that says it was not sent.
fn image_block(image: &Image, images: bool) -> Block<'_> {
    if images {
        Block::Image {
            data: &image.data,
            mime_type: image.media_type.mime(),
        }
    } else {
        Block::Text {
            text: NO_IMAGES,
            text_signature: None,
        }
    }
}

fn tool_result(result: &ToolResult, out: &mut Vec<u8>, names: &[(&str, &str)], images: bool) {
    let name = names
        .iter()
        .find(|(id, _)| *id == &*result.call_id)
        .map_or("", |(_, name)| *name);
    let single = Block::Text {
        text: &result.content,
        text_signature: None,
    };
    let content = if result.images.is_empty() {
        ToolContent::Single([single])
    } else {
        let mut blocks = Vec::with_capacity(result.images.len() + 1);
        blocks.push(single);
        blocks.extend(result.images.iter().map(|image| image_block(image, images)));
        ToolContent::Blocks(blocks)
    };
    let message = ToolResultBody {
        role: "toolResult",
        tool_call_id: &result.call_id,
        tool_name: name,
        content,
        is_error: result.is_error,
    };
    write(&message, out);
}

fn usage_body(usage: &crate::Usage) -> UsageBody {
    UsageBody {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_tokens,
        cache_write: usage.cache_write_tokens,
        total_tokens: usage
            .input_tokens
            .saturating_add(usage.output_tokens)
            .saturating_add(usage.cache_read_tokens)
            .saturating_add(usage.cache_write_tokens),
        cost: CostBody {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            total: usage.cost.unwrap_or(0.0),
        },
    }
}

fn stop_reason(reason: Option<StopReason>) -> &'static str {
    match reason {
        Some(StopReason::ToolUse) => "toolUse",
        Some(StopReason::MaxTokens) => "length",
        Some(StopReason::Error | StopReason::Refusal) => "error",
        Some(StopReason::EndTurn) | None => "stop",
    }
}

fn to_bytes(value: &impl Serialize) -> Bytes {
    Bytes::from(serde_json::to_vec(value).expect("request parts are serializable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Text, ToolCall, Usage};
    use crate::{Request, StreamEvent};
    use serde_json::json;

    const PROVIDER: &str = "pi-messages";
    const MODEL: &str = "balanced";
    const SYSTEM: &str = "You are a careful coding agent.";
    const REPLAY: &str = "{\"responseId\":\"r-1\",\"providerThinkingLevel\":\"high\",\
                          \"textSignatures\":{\"0\":\"text-sig\"},\
                          \"toolSignatures\":{\"call_1\":\"tool-sig\"}}";

    fn tools() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "read".into(),
                description: "Read a file.".into(),
                input_schema: json!({"type": "object"}),
            },
            ToolSpec {
                name: "write".into(),
                description: "Write a file.".into(),
                input_schema: json!({"type": "object"}),
            },
        ]
    }

    fn assistant() -> Message {
        let mut assistant = AssistantMessage::new(
            PROVIDER,
            MODEL,
            vec![
                Content::Thinking {
                    thinking: "Let me look.".into(),
                    signature: Some("think-sig".into()),
                },
                Content::text("Reading it now."),
                Content::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a.txt\"}".into(),
                }),
            ],
        );
        assistant.stop_reason = Some(StopReason::ToolUse);
        assistant.usage = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 30,
            cache_write_tokens: 40,
            reasoning_tokens: 5,
            cost: Some(0.33),
        };
        assistant.replay = Some(Text::from(REPLAY));
        Message::assistant(assistant)
    }

    /// A session: a system prompt and tools, a user turn, a signed reply with
    /// one tool call, and the tool's result.
    fn history() -> Vec<Message> {
        vec![
            Message::user("Read a.txt."),
            assistant(),
            Message::tool_result("call_1", "alpha", false),
        ]
    }

    fn builder(reasoning: Option<&str>) -> RequestBuilder {
        RequestBuilder::new(
            MODEL,
            1024,
            Some(SYSTEM),
            reasoning,
            Some("sess-1"),
            &tools(),
        )
    }

    fn body(builder: &RequestBuilder, messages: &[Message]) -> serde_json::Value {
        let names = tool_names(messages);
        let bodies: Vec<MessageBody> = messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                MessageBody::new(message, PROVIDER, &names, index as u64 + 1).expect("a message")
            })
            .collect();
        let raw = builder.body(&bodies);
        serde_json::from_slice(&raw).unwrap_or_else(|error| {
            panic!(
                "the body is JSON: {error}\n{}",
                String::from_utf8_lossy(&raw)
            )
        })
    }

    fn sample_image() -> crate::types::Image {
        crate::types::Image {
            media_type: crate::types::ImageType::Png,
            width: 1,
            height: 1,
            data: "aGk=".into(),
        }
    }

    fn user_with_image() -> Message {
        Message::User {
            content: vec![Content::text("look"), Content::Image(sample_image())],
        }
    }

    fn result_with_image() -> Message {
        Message::tool_result_with_images("call_1", "one frame", false, vec![sample_image()])
    }

    /// The body of `messages`, with images sent or replaced by the note.
    fn body_of_images(builder: &RequestBuilder, messages: &[Message], images: bool) -> Bytes {
        let names = tool_names(messages);
        let bodies: Vec<MessageBody> = messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                MessageBody::with_images(message, PROVIDER, &names, index as u64 + 1, images)
                    .expect("a message")
            })
            .collect();
        builder.body(&bodies)
    }

    const IMAGE_BLOCK: &str = r#"{"type":"image","data":"aGk=","mimeType":"image/png"}"#;

    #[test]
    fn an_image_body_writes_the_image_blocks_the_wire_names() {
        let messages = vec![user_with_image(), assistant(), result_with_image()];
        let raw = body_of_images(&builder(Some("high")), &messages, true);
        let body: serde_json::Value = serde_json::from_slice(&raw).expect("the body is JSON");
        assert_eq!(
            body["context"]["messages"][1],
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "data": "aGk=", "mimeType": "image/png"},
                ],
                "timestamp": 1,
            }),
            "{body}"
        );
        assert_eq!(
            body["context"]["messages"][3],
            json!({
                "role": "toolResult",
                "toolCallId": "call_1",
                "toolName": "read",
                "content": [
                    {"type": "text", "text": "one frame"},
                    {"type": "image", "data": "aGk=", "mimeType": "image/png"},
                ],
                "isError": false,
                "timestamp": 3,
            }),
            "{body}"
        );
        assert_eq!(
            std::str::from_utf8(&raw)
                .unwrap()
                .matches(IMAGE_BLOCK)
                .count(),
            2
        );
    }

    #[test]
    fn takes_no_images_writes_the_note_in_place_of_each_image() {
        let messages = vec![user_with_image(), assistant(), result_with_image()];
        let raw = body_of_images(&builder(Some("high")), &messages, false);
        let body = std::str::from_utf8(&raw).unwrap();
        assert!(!body.contains("aGk="), "{body}");
        assert_eq!(
            body.matches("[image not sent: the model takes no images]")
                .count(),
            2,
            "one note for each image: {body}"
        );
    }

    #[test]
    fn an_image_message_keeps_its_bytes_across_turns() {
        let provider = crate::pi::PiMessages::new("", "http://localhost:1/v1");
        let first: Vec<Message> = vec![Message::user("hello"), user_with_image()];
        let mut second = first.clone();
        second.push(result_with_image());
        let system = Text::from(SYSTEM);
        let key = Text::from("sess-1");
        let tools = tools();
        let mut request = Request {
            model: MODEL,
            max_tokens: 1024,
            system: Some(&system),
            tools: &tools,
            tool_choice: crate::ToolChoice::Auto,
            messages: &first,
            images: true,
            reasoning: Some("high"),
            cache_key: Some(&key),
        };
        let one = provider.request_body(&request).expect("the body builds");
        request.messages = &second;
        let two = provider.request_body(&request).expect("the body builds");
        assert!(
            two.starts_with(&one[..one.len() - 3]),
            "the image message keeps its bytes:\n{}\n{}",
            String::from_utf8_lossy(&one),
            String::from_utf8_lossy(&two)
        );
    }

    #[test]
    fn pi_request_body_matches_the_wire() {
        let expected = json!({
            "model": MODEL,
            "context": {"messages": [
                {
                    "role": "system",
                    "content": SYSTEM,
                    "toolsAdded": [
                        {"name": "read", "description": "Read a file.", "parameters": {"type": "object"}},
                        {"name": "write", "description": "Write a file.", "parameters": {"type": "object"}},
                    ],
                    "timestamp": 0,
                },
                {"role": "user", "content": "Read a.txt.", "timestamp": 1},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "Let me look.", "thinkingSignature": "think-sig"},
                        {"type": "text", "text": "Reading it now.", "textSignature": "text-sig"},
                        {
                            "type": "toolCall",
                            "id": "call_1",
                            "name": "read",
                            "arguments": {"path": "a.txt"},
                            "thoughtSignature": "tool-sig",
                        },
                    ],
                    "api": "pi-messages",
                    "provider": "radius",
                    "model": MODEL,
                    "responseId": "r-1",
                    "providerThinkingLevel": "high",
                    "usage": {
                        "input": 100,
                        "output": 20,
                        "cacheRead": 30,
                        "cacheWrite": 40,
                        "totalTokens": 190,
                        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0.33},
                    },
                    "stopReason": "toolUse",
                    "timestamp": 2,
                },
                {
                    "role": "toolResult",
                    "toolCallId": "call_1",
                    "toolName": "read",
                    "content": [{"type": "text", "text": "alpha"}],
                    "isError": false,
                    "timestamp": 3,
                },
            ]},
            "options": {"maxTokens": 1024, "reasoning": "high", "sessionId": "sess-1"},
        });
        assert_eq!(body(&builder(Some("high")), &history()), expected);
    }

    #[test]
    fn pi_request_head_is_the_expected_bytes() {
        let builder = builder(Some("high"));
        assert_eq!(
            std::str::from_utf8(builder.head()).expect("the head is UTF-8"),
            "{\"model\":\"balanced\",\"options\":{\"maxTokens\":1024,\"reasoning\":\"high\",\
             \"sessionId\":\"sess-1\"},\"context\":{\"messages\":["
        );
        let system = builder.system().expect("a system prompt and tools");
        assert!(
            std::str::from_utf8(system)
                .expect("the system message is UTF-8")
                .ends_with("\"timestamp\":0}")
        );
    }

    #[test]
    fn pi_request_leaves_reasoning_out_when_it_is_off_or_a_budget() {
        for reasoning in [None, Some("off"), Some("none"), Some("4000")] {
            let body = body(&builder(reasoning), &history());
            let options = body["options"].as_object().expect("options");
            assert!(
                !options.contains_key("reasoning"),
                "{reasoning:?} sends no reasoning level"
            );
            assert_eq!(options["maxTokens"], json!(1024));
        }
    }

    #[test]
    fn pi_request_keeps_the_cached_message_bytes_across_two_turns() {
        let provider = crate::pi::PiMessages::new("", "http://localhost:1/v1");
        let first: Vec<Message> = history()[..2].to_vec();
        let second: Vec<Message> = history();
        let system = Text::from(SYSTEM);
        let key = Text::from("sess-1");
        let tools = tools();
        let mut request = Request {
            model: MODEL,
            max_tokens: 1024,
            system: Some(&system),
            tools: &tools,
            tool_choice: crate::ToolChoice::Auto,
            messages: &first,
            images: true,
            reasoning: Some("high"),
            cache_key: Some(&key),
        };
        let one = provider.request_body(&request).expect("the body builds");
        request.messages = &second;
        let two = provider.request_body(&request).expect("the body builds");

        assert!(
            two.starts_with(&one[..one.len() - 3]),
            "the turns before the new one keep their bytes:\n{}\n{}",
            String::from_utf8_lossy(&one),
            String::from_utf8_lossy(&two)
        );
        // The second turn holds the reply and the tool result the first one
        // did not.
        let tail = &two[one.len() - 2..two.len() - 3];
        let added: serde_json::Value =
            serde_json::from_slice(format!("[{}]", String::from_utf8_lossy(tail)).as_bytes())
                .expect("the new messages");
        assert_eq!(added.as_array().map(Vec::len), Some(1), "{added}");
        assert_eq!(added[0]["role"], json!("toolResult"));
        assert_eq!(added[0]["toolName"], json!("read"));
    }

    #[test]
    fn pi_request_without_a_prompt_or_tools_sends_no_system_message() {
        let builder = RequestBuilder::new(MODEL, 16, None, None, None, &[]);
        assert!(builder.system().is_none());
        let messages = [Message::user("hi")];
        let names = tool_names(&messages);
        let bodies = [MessageBody::new(&messages[0], PROVIDER, &names, 1).expect("a message")];
        let raw = builder.body(&bodies);
        let body: serde_json::Value = serde_json::from_slice(&raw).unwrap_or_else(|error| {
            panic!(
                "the body is JSON: {error}\n{}",
                String::from_utf8_lossy(&raw)
            )
        });
        assert_eq!(
            body["context"]["messages"],
            json!([{"role": "user", "content": "hi", "timestamp": 1}])
        );
    }

    #[test]
    fn pi_request_keeps_a_redacted_thinking_block_and_drops_a_foreign_one() {
        let mut reply = AssistantMessage::new(
            PROVIDER,
            MODEL,
            vec![
                Content::RedactedThinking {
                    data: "opaque".into(),
                },
                Content::text("hi"),
            ],
        );
        reply.stop_reason = Some(StopReason::EndTurn);
        let names = [];
        let bytes = MessageBody::new(&Message::assistant(reply.clone()), PROVIDER, &names, 0)
            .expect("a message");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes.bytes).expect("the message is JSON");
        assert_eq!(
            body["content"],
            json!([
                {"type": "thinking", "thinking": "", "thinkingSignature": "opaque", "redacted": true},
                {"type": "text", "text": "hi"},
            ])
        );

        reply.provider = Text::from("anthropic");
        let bytes =
            MessageBody::new(&Message::assistant(reply), PROVIDER, &names, 0).expect("a message");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes.bytes).expect("the message is JSON");
        assert_eq!(body["content"], json!([{"type": "text", "text": "hi"}]));
    }

    #[test]
    fn pi_error_body_gives_its_message() {
        assert_eq!(
            crate::pi::status_message(
                "{\"error\":{\"message\":\"missing gateway key\",\"code\":\"unauthorized\"}}"
            ),
            "missing gateway key"
        );
        assert_eq!(
            crate::pi::status_message("bad gateway"),
            "bad gateway",
            "a body of another shape reaches the user as it came"
        );
    }

    /// A signature that carries quotes and a backslash survives the trip
    /// through the replay object and back into the request.
    #[test]
    fn pi_request_replays_a_signature_with_an_escape() {
        const SIGNATURE: &str = "{\"v\":1,\"id\":\"msg_1\",\"path\":\"C:\\\\tmp\"}";
        let signature = serde_json::to_string(SIGNATURE).expect("a signature string");
        let mut reply = AssistantMessage::new(
            PROVIDER,
            MODEL,
            vec![
                Content::text("hi"),
                Content::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a.txt\"}".into(),
                }),
            ],
        );
        reply.replay = Some(Text::from(format!(
            "{{\"responseId\":\"r-1\",\"textSignatures\":{{\"0\":{signature}}},\
             \"toolSignatures\":{{\"call_1\":{signature}}}}}"
        )));
        let names = [("call_1", "read")];
        let bytes =
            MessageBody::new(&Message::assistant(reply), PROVIDER, &names, 0).expect("a message");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes.bytes).expect("the message is JSON");
        assert_eq!(body["content"][0]["textSignature"], json!(SIGNATURE));
        assert_eq!(body["content"][1]["thoughtSignature"], json!(SIGNATURE));
        assert_eq!(body["responseId"], json!("r-1"));
    }

    /// A parser fed the same events a request body names keeps them.
    #[test]
    fn pi_request_replays_only_the_keys_the_reply_had() {
        let mut reply = AssistantMessage::new(PROVIDER, MODEL, vec![Content::text("hi")]);
        reply.replay = Some(Text::from("{\"responseId\":\"r-2\"}"));
        let names = [];
        let bytes =
            MessageBody::new(&Message::assistant(reply), PROVIDER, &names, 0).expect("a message");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes.bytes).expect("the message is JSON");
        assert_eq!(body["responseId"], json!("r-2"));
        assert!(body.get("providerThinkingLevel").is_none());
        assert_eq!(body["stopReason"], json!("stop"));
        assert_eq!(
            body["usage"],
            json!({
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0.0},
            })
        );
        let _ = StreamEvent::Stop(StopReason::EndTurn);
    }
}
