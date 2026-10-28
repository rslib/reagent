//! Builds OpenAI Responses request bodies.

use std::borrow::Cow;

use bytes::{BufMut, Bytes, BytesMut};
use serde::Serialize;

use crate::Error;
use crate::openai::{BodyParts, item_pieces};
use crate::providers::{Compat, MaxTokensField};
use crate::request_images::{NO_IMAGES, data_url};
use crate::types::{
    AssistantMessage, Content, Image, Message, Reasoning, Role, ToolResult, ToolSpec,
};

/// One message serialized in Responses wire form, reusable across requests.
#[derive(Debug, Clone)]
pub struct MessageBody {
    items: Vec<Bytes>,
    /// The role of the message it was built from.
    role: Role,
    /// The image parts a tool result adds to the user message that follows the
    /// tool outputs of its turn, in call order. Empty when it has none.
    images: Vec<Bytes>,
}

impl MessageBody {
    /// Serializes one message for the provider named `provider`.
    ///
    /// Thinking blocks from another provider are dropped. Tool arguments are
    /// passed through as the raw JSON string the model emitted; an empty
    /// argument string becomes an empty object. Images are sent.
    pub fn new(message: &Message, provider: &str) -> Result<Self, Error> {
        Self::with_images(message, provider, true)
    }

    /// Serializes one message for the provider named `provider`, sending its
    /// images only when `images` and writing the note of
    /// [`crate::request_images::NO_IMAGES`] in their place when not.
    pub fn with_images(message: &Message, provider: &str, images: bool) -> Result<Self, Error> {
        let (items, parts) = match message {
            Message::User { content } => (user_items(content, images), Vec::new()),
            Message::Assistant(assistant) => (assistant_items(assistant, provider), Vec::new()),
            Message::ToolResult(result) => (
                vec![to_bytes(&InputItem::FunctionCallOutput {
                    call_id: &result.call_id,
                    output: &tool_output(result, images),
                })],
                if images {
                    image_parts(&result.images)
                } else {
                    Vec::new()
                },
            ),
        };
        Ok(Self {
            items,
            role: message.role(),
            images: parts,
        })
    }
}

/// Builds a Responses body from a stable head and reusable message bodies.
///
/// The head holds the model, the instructions, the reasoning text, the
/// prompt cache key and the tools, serialized once. Tools are sorted by name.
#[derive(Debug)]
pub struct RequestBuilder {
    head: Bytes,
}

impl RequestBuilder {
    /// Serializes the request head once, for the plain wire.
    pub fn new(
        model: &str,
        max_tokens: u32,
        system: Option<&str>,
        reasoning: Option<&str>,
        cache_key: Option<&str>,
        tools: &[ToolSpec],
    ) -> Self {
        Self::with_compat(
            model,
            max_tokens,
            system,
            reasoning,
            cache_key,
            tools,
            Compat::DEFAULT,
        )
    }

    /// Serializes the request head once, for an endpoint described by `compat`.
    pub fn with_compat(
        model: &str,
        max_tokens: u32,
        system: Option<&str>,
        reasoning: Option<&str>,
        cache_key: Option<&str>,
        tools: &[ToolSpec],
        compat: Compat,
    ) -> Self {
        let reasoning_off = compat.reasoning_off;
        let mut out = BytesMut::new();
        out.put_slice(b"{\"model\":");
        out.put_slice(json_string(model).as_bytes());
        if let Some(system) = system {
            out.put_slice(b",\"instructions\":");
            out.put_slice(json_string(system).as_bytes());
        }
        // The cap covers reasoning too, so it is sent as given. An endpoint
        // that refuses a cap field gets none.
        if compat.max_tokens_field != MaxTokensField::Omit {
            out.put_slice(b",\"max_output_tokens\":");
            out.put_slice(max_tokens.to_string().as_bytes());
        }
        out.put_slice(b",\"stream\":true,\"store\":false");
        if let Some(reasoning) = reasoning.map(Reasoning::parse) {
            if reasoning.is_off() {
                if let Some(word) = reasoning_off {
                    out.put_slice(b",\"reasoning\":{\"effort\":");
                    out.put_slice(json_string(word).as_bytes());
                    out.put_slice(b"}");
                }
            } else {
                let effort = reasoning.text();
                out.put_slice(
                    b",\"include\":[\"reasoning.encrypted_content\"],\"reasoning\":{\"effort\":",
                );
                out.put_slice(json_string(&effort).as_bytes());
                out.put_slice(b",\"summary\":\"auto\"}");
            }
        }
        if let Some(cache_key) = cache_key {
            out.put_slice(b",\"prompt_cache_key\":");
            out.put_slice(json_string(cache_key).as_bytes());
        }
        out.put_slice(b",\"tools\":[");

        let mut sorted: Vec<&ToolSpec> = tools.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for (index, tool) in sorted.iter().enumerate() {
            if index > 0 {
                out.put_u8(b',');
            }
            out.put_slice(&to_bytes(&FunctionTool {
                kind: "function",
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.input_schema,
            }));
        }
        out.put_slice(b"],\"input\":[");

        Self { head: out.freeze() }
    }

    /// The prefix every body built from this request shares.
    pub fn head(&self) -> &Bytes {
        &self.head
    }

    /// Serializes a body from the head and the message bodies.
    ///
    /// A message whose blocks were all dropped disappears.
    pub fn body(&self, messages: &[MessageBody]) -> Bytes {
        self.body_parts(messages).join()
    }

    /// The body as its pieces: the head, the separators and every cached
    /// message's items, none of them copied.
    pub fn body_parts(&self, messages: &[MessageBody]) -> BodyParts {
        let items: usize = messages.iter().map(|message| message.items.len()).sum();
        let images: usize = messages.iter().map(|message| message.images.len()).sum();
        let mut parts =
            BodyParts::with_capacity(2 + item_pieces(items) + image_message_pieces(images));
        parts.push(self.head.clone());
        let mut first = true;
        let mut index = 0;
        while index < messages.len() {
            if messages[index].role != Role::ToolResult {
                push_items(&mut parts, &messages[index].items, &mut first);
                index += 1;
                continue;
            }
            // The tool outputs of one turn, and then one user message with the
            // images they carry, which a function call output cannot hold.
            let start = index;
            let mut carried = 0;
            while index < messages.len() && messages[index].role == Role::ToolResult {
                let message = &messages[index];
                carried += message.images.len();
                push_items(&mut parts, &message.items, &mut first);
                index += 1;
            }
            if carried == 0 {
                continue;
            }
            if !first {
                parts.push_static(COMMA);
            }
            first = false;
            parts.push_static(IMAGES_PREFIX);
            for part in messages[start..index]
                .iter()
                .flat_map(|message| &message.images)
            {
                parts.push_static(COMMA);
                parts.push(part.clone());
            }
            parts.push_static(IMAGES_END);
        }
        parts.push_static(END);
        debug_assert_eq!(
            parts.len(),
            self.body_len(messages),
            "the pieces hold the body the buffer held"
        );
        parts
    }

    /// The length of the body [`RequestBuilder::body_parts`] appends, so the
    /// pieces hold the bytes the buffer held, which a debug build checks.
    fn body_len(&self, messages: &[MessageBody]) -> usize {
        let mut len = self.head.len() + END.len();
        let mut first = true;
        let mut index = 0;
        while index < messages.len() {
            if messages[index].role != Role::ToolResult {
                len += items_len(&messages[index].items, &mut first);
                index += 1;
                continue;
            }
            let start = index;
            let mut carried = 0;
            while index < messages.len() && messages[index].role == Role::ToolResult {
                let message = &messages[index];
                carried += message.images.len();
                len += items_len(&message.items, &mut first);
                index += 1;
            }
            if carried == 0 {
                continue;
            }
            len += IMAGES_PREFIX.len() + IMAGES_END.len() + usize::from(!first) + carried;
            first = false;
            for part in messages[start..index]
                .iter()
                .flat_map(|message| &message.images)
            {
                len += part.len();
            }
        }
        len
    }
}

/// Appends `items` as pieces, with the comma before every item but the first
/// of the body.
fn push_items(parts: &mut BodyParts, items: &[Bytes], first: &mut bool) {
    for item in items {
        if !*first {
            parts.push_static(COMMA);
        }
        *first = false;
        parts.push(item.clone());
    }
}

/// The length of `items` written with a comma before every one but the first
/// of the body, and whether `first` stays first.
fn items_len(items: &[Bytes], first: &mut bool) -> usize {
    let mut len = 0;
    for item in items {
        len += item.len() + usize::from(!*first);
        *first = false;
    }
    len
}

/// The pieces a user message of tool-result images takes: the comma before it,
/// the prefix that opens it, a comma before every image, and the bracket that
/// closes it.
fn image_message_pieces(images: usize) -> usize {
    if images == 0 { 0 } else { 2 * images + 3 }
}

const COMMA: &[u8] = b",";
const END: &[u8] = b"]}";
/// The user message that carries the images of the tool outputs above it. It
/// closes with [`IMAGES_END`], and its content starts with one text part.
const IMAGES_PREFIX: &[u8] = b"{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Images from the tool results above:\"}";
const IMAGES_END: &[u8] = b"]}";

#[derive(Serialize)]
struct FunctionTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InputItem<'a> {
    Message {
        role: &'static str,
        content: Vec<Part<'a>>,
    },
    Reasoning {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<&'a str>,
        summary: Vec<&'a str>,
        encrypted_content: &'a str,
    },
    FunctionCall {
        call_id: &'a str,
        name: &'a str,
        arguments: &'a str,
    },
    FunctionCallOutput {
        call_id: &'a str,
        output: &'a str,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part<'a> {
    InputText { text: &'a str },
    OutputText { text: &'a str },
    InputImage { image_url: Cow<'a, str> },
}

fn user_items(content: &[Content], images: bool) -> Vec<Bytes> {
    let parts: Vec<Part<'_>> = content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(Part::InputText {
                text: text.as_ref(),
            }),
            Content::Image(image) if images => Some(image_part(image)),
            Content::Image(_) => Some(Part::InputText { text: NO_IMAGES }),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        Vec::new()
    } else {
        vec![to_bytes(&InputItem::Message {
            role: "user",
            content: parts,
        })]
    }
}

/// The image parts of `images`, in call order, which the user message after
/// the tool outputs of a turn writes.
fn image_parts(images: &[Image]) -> Vec<Bytes> {
    images
        .iter()
        .map(|image| to_bytes(&image_part(image)))
        .collect()
}

fn image_part(image: &Image) -> Part<'_> {
    Part::InputImage {
        image_url: Cow::Owned(data_url(image)),
    }
}

/// The output text of one tool result: its text, and the note for every image
/// the model takes none of, because the output item itself writes text only.
fn tool_output(result: &ToolResult, images: bool) -> Cow<'_, str> {
    if images || result.images.is_empty() {
        return Cow::Borrowed(result.content.as_ref());
    }
    let mut text =
        String::with_capacity(result.content.len() + result.images.len() * (NO_IMAGES.len() + 1));
    text.push_str(&result.content);
    for _ in &result.images {
        text.push('\n');
        text.push_str(NO_IMAGES);
    }
    Cow::Owned(text)
}

fn assistant_items(assistant: &AssistantMessage, provider: &str) -> Vec<Bytes> {
    let keep_thinking = assistant.provider.as_ref() == provider;
    let mut items = Vec::new();
    let mut text: Vec<&str> = Vec::new();
    let mut pending_reasoning: Vec<Bytes> = Vec::new();
    for content in &assistant.content {
        match content {
            Content::Text { text: value } => text.push(value.as_ref()),
            Content::Thinking { signature, .. } => {
                flush_assistant_text(&mut items, &mut text, &mut pending_reasoning);
                if keep_thinking && let Some(signature) = signature {
                    let (id, encrypted_content) = super::split_reasoning(signature);
                    pending_reasoning.push(to_bytes(&InputItem::Reasoning {
                        id,
                        summary: Vec::new(),
                        encrypted_content,
                    }));
                }
            }
            Content::RedactedThinking { .. } => {
                flush_assistant_text(&mut items, &mut text, &mut pending_reasoning)
            }
            // A model's reply holds no image, and the wire has no place for
            // one in an assistant item.
            Content::Image(_) => {}
            Content::ToolCall(call) => {
                flush_assistant_text(&mut items, &mut text, &mut pending_reasoning);
                commit_reasoning(&mut items, &mut pending_reasoning);
                let arguments = if call.arguments.trim().is_empty() {
                    "{}"
                } else {
                    call.arguments.as_ref()
                };
                items.push(to_bytes(&InputItem::FunctionCall {
                    call_id: &call.id,
                    name: &call.name,
                    arguments,
                }));
            }
        }
    }
    flush_assistant_text(&mut items, &mut text, &mut pending_reasoning);
    items
}

fn flush_assistant_text(
    items: &mut Vec<Bytes>,
    text: &mut Vec<&str>,
    pending_reasoning: &mut Vec<Bytes>,
) {
    if text.is_empty() {
        return;
    }
    commit_reasoning(items, pending_reasoning);
    let parts = text
        .drain(..)
        .map(|value| Part::OutputText { text: value })
        .collect();
    items.push(to_bytes(&InputItem::Message {
        role: "assistant",
        content: parts,
    }));
}

fn commit_reasoning(items: &mut Vec<Bytes>, pending_reasoning: &mut Vec<Bytes>) {
    items.append(pending_reasoning);
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a str serializes to a JSON string")
}

fn to_bytes(value: &impl Serialize) -> Bytes {
    Bytes::from(serde_json::to_vec(value).expect("request parts are serializable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Request;
    use crate::types::{StopReason, ToolCall};

    const PROVIDER: &str = "openai";

    fn golden_builder() -> RequestBuilder {
        let tools = vec![
            ToolSpec {
                name: "write".into(),
                description: "Write a file.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            ToolSpec {
                name: "read".into(),
                description: "Read a file.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        ];
        RequestBuilder::new(
            "gpt-5",
            1024,
            Some("You are a careful coding agent."),
            Some("medium"),
            Some("sess-1"),
            &tools,
        )
    }

    fn golden_messages() -> Vec<MessageBody> {
        let mut assistant = AssistantMessage::new(
            PROVIDER,
            "gpt-5",
            vec![
                Content::text("hi"),
                Content::ToolCall(ToolCall {
                    id: "call_01".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a.rs\"}".into(),
                }),
            ],
        );
        assistant.stop_reason = Some(StopReason::ToolUse);
        [
            Message::user("hello"),
            Message::assistant(assistant),
            Message::tool_result("call_01", "fn main() {}", false),
            Message::user("explain"),
        ]
        .iter()
        .map(|message| MessageBody::new(message, PROVIDER).unwrap())
        .collect()
    }

    fn body_of(messages: &[Message]) -> Bytes {
        let bodies: Vec<MessageBody> = messages
            .iter()
            .map(|message| MessageBody::new(message, PROVIDER).unwrap())
            .collect();
        golden_builder().body(&bodies)
    }

    fn sample_image() -> Image {
        Image {
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
        Message::tool_result_with_images("call_01", "one frame", false, vec![sample_image()])
    }

    /// The body of `messages`, with images sent or replaced by the note.
    fn body_of_images(messages: &[Message], images: bool) -> Bytes {
        let bodies: Vec<MessageBody> = messages
            .iter()
            .map(|message| MessageBody::with_images(message, PROVIDER, images).unwrap())
            .collect();
        golden_builder().body(&bodies)
    }

    const URL: &str = "data:image/png;base64,aGk=";
    const IMAGE_PART: &str = r#"{"type":"input_image","image_url":"data:image/png;base64,aGk="}"#;

    #[test]
    fn an_image_body_writes_the_input_image_and_one_message_for_the_tool_images() {
        let body = body_of_images(
            &[
                Message::user("hello"),
                user_with_image(),
                result_with_image(),
            ],
            true,
        );
        let body = std::str::from_utf8(&body).unwrap();
        assert!(
            body.contains(&format!(
                r#"{{"type":"message","role":"user","content":[{{"type":"input_text","text":"look"}},{IMAGE_PART}]}}"#
            )),
            "{body}"
        );
        assert!(
            body.contains(
                r#"{"type":"function_call_output","call_id":"call_01","output":"one frame"}"#
            ),
            "a function call output stays text: {body}"
        );
        assert!(
            body.contains(&format!(
                r#"{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Images from the tool results above:"}},{IMAGE_PART}]}}"#
            )),
            "{body}"
        );
        assert_eq!(body.matches(URL).count(), 2, "{body}");
    }

    #[test]
    fn takes_no_images_writes_the_note_and_no_image_message() {
        let body = body_of_images(&[user_with_image(), result_with_image()], false);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("aGk="), "{body}");
        assert!(
            !body.contains("Images from the tool results above"),
            "{body}"
        );
        assert!(
            body.contains(r#""text":"[image not sent: the model takes no images]""#),
            "{body}"
        );
        assert!(
            body.contains(r"one frame\n[image not sent: the model takes no images]"),
            "the note goes in the output item: {body}"
        );
    }

    #[test]
    fn an_image_message_keeps_its_bytes_across_turns() {
        let provider = crate::openai::responses::OpenAiResponses::new("key", "http://localhost");
        let mut history = vec![Message::user("hello"), user_with_image()];
        let first = {
            let request = Request {
                model: "gpt-5",
                max_tokens: 128,
                system: None,
                tools: &[],
                messages: &history,
                images: true,
                reasoning: None,
                cache_key: None,
            };
            provider.request_body(&request).expect("the body builds")
        };
        history.push(result_with_image());
        let second = {
            let request = Request {
                model: "gpt-5",
                max_tokens: 128,
                system: None,
                tools: &[],
                messages: &history,
                images: true,
                reasoning: None,
                cache_key: None,
            };
            provider.request_body(&request).expect("the body builds")
        };
        assert!(
            second.starts_with(&first[..first.len() - 2]),
            "the image message keeps its bytes:\n{}\n{}",
            String::from_utf8_lossy(&first),
            String::from_utf8_lossy(&second)
        );
    }

    #[test]
    fn body_matches_the_golden_request() {
        let body = golden_builder().body(&golden_messages());
        let golden = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_responses_request.json"
        ));
        assert_eq!(std::str::from_utf8(&body).unwrap(), golden.trim_end());
    }

    /// A history with every shape a session holds: text, a reply with thinking
    /// and a tool call, that tool's result, and a text-only reply.
    fn mixed_messages() -> Vec<MessageBody> {
        let mut assistant = AssistantMessage::new(
            PROVIDER,
            "gpt-5",
            vec![
                Content::Thinking {
                    thinking: "consider the file".into(),
                    signature: Some("enc_1".into()),
                },
                Content::text("reading"),
                Content::ToolCall(ToolCall {
                    id: "call_02".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"b.rs\"}".into(),
                }),
            ],
        );
        assistant.stop_reason = Some(StopReason::ToolUse);
        [
            Message::user("hello"),
            Message::assistant(assistant),
            Message::tool_result("call_02", "fn b() {}", false),
            Message::assistant(AssistantMessage::new(
                PROVIDER,
                "gpt-5",
                vec![Content::text("here it is")],
            )),
        ]
        .iter()
        .map(|message| MessageBody::new(message, PROVIDER).unwrap())
        .collect()
    }

    /// The pieces joined are the bytes the old single buffer wrote, and their
    /// length is the length of that body.
    #[test]
    fn body_pieces_join_to_the_single_buffer_body() {
        let builder = golden_builder();
        let bodies = mixed_messages();
        let parts = builder.body_parts(&bodies);
        let joined = parts.join();
        assert_eq!(
            parts.len(),
            joined.len(),
            "the length of the body is the sum of its pieces"
        );
        assert_eq!(
            joined,
            builder.body(&bodies),
            "the joined pieces are the body"
        );
        assert!(
            serde_json::from_slice::<serde_json::Value>(&joined).is_ok(),
            "the body of a mixed history is one JSON object"
        );

        let golden = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_responses_request.json"
        ));
        let pieces = builder.body_parts(&golden_messages()).join();
        assert_eq!(
            std::str::from_utf8(&pieces).unwrap(),
            golden.trim_end(),
            "the pieces write the bytes the old buffer wrote"
        );
    }

    /// Whether `piece` points into `item`, so the two hold one copy of the
    /// bytes and no copy was made.
    fn shares(piece: &Bytes, item: &Bytes) -> bool {
        let start = item.as_ptr() as usize;
        let at = piece.as_ptr() as usize;
        at >= start && at + piece.len() <= start + item.len()
    }

    /// Every item of every cached message reaches the body as the piece that
    /// points into it, so the body holds no second copy of any message.
    #[test]
    fn body_pieces_share_the_cached_message_bytes() {
        let builder = golden_builder();
        let bodies = mixed_messages();
        let parts = builder.body_parts(&bodies);
        for item in bodies.iter().flat_map(|body| &body.items) {
            let pieces: Vec<&Bytes> = parts
                .parts()
                .iter()
                .filter(|piece| shares(piece, item))
                .collect();
            assert_eq!(pieces.len(), 1, "one piece holds the item, no copy");
            assert_eq!(
                pieces[0].as_ptr(),
                item.as_ptr(),
                "the piece starts the item"
            );
            assert_eq!(pieces[0].len(), item.len(), "the piece is the whole item");
        }
    }

    #[test]
    fn head_carries_reasoning_and_the_prompt_cache_key() {
        let builder = golden_builder();
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(head.contains(r#""max_output_tokens":1024"#));
        assert!(head.contains(r#""store":false"#));
        assert!(head.contains(r#""include":["reasoning.encrypted_content"]"#));
        assert!(head.contains(r#""reasoning":{"effort":"medium","summary":"auto"}"#));
        assert!(head.contains(r#""prompt_cache_key":"sess-1""#));
    }

    #[test]
    fn the_same_history_gives_byte_identical_prefixes() {
        let builder = golden_builder();
        let messages = golden_messages();
        let first = builder.body(&messages);
        let second = builder.body(&messages);
        assert_eq!(first, second);
        assert!(first.starts_with(builder.head()));
    }

    #[test]
    fn appending_a_message_keeps_the_earlier_bytes() {
        let builder = golden_builder();
        let one = vec![MessageBody::new(&Message::user("hello"), PROVIDER).unwrap()];
        let mut two = one.clone();
        two.push(
            MessageBody::new(
                &Message::assistant(AssistantMessage::new(
                    PROVIDER,
                    "gpt-5",
                    vec![Content::text("hi")],
                )),
                PROVIDER,
            )
            .unwrap(),
        );

        let short = builder.body(&one);
        let long = builder.body(&two);

        let stable = &short[..short.len() - 2];
        assert!(long.starts_with(stable));
    }

    #[test]
    fn thinking_from_another_provider_is_dropped() {
        let assistant = AssistantMessage::new(
            "anthropic",
            "claude-sonnet-4-5",
            vec![
                Content::Thinking {
                    thinking: "secret".into(),
                    signature: Some("sig".into()),
                },
                Content::RedactedThinking {
                    data: "opaque".into(),
                },
                Content::text("visible"),
            ],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("secret"));
        assert!(!body.contains("opaque"));
        assert!(!body.contains("sig"));
        assert!(body.contains("visible"));
    }

    #[test]
    fn thinking_from_the_same_provider_round_trips_as_encrypted_content() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "gpt-5",
            vec![
                Content::Thinking {
                    thinking: "kept".into(),
                    signature: Some("enc_1".into()),
                },
                Content::text("after"),
            ],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#""type":"reasoning","summary":[],"encrypted_content":"enc_1""#));
        assert!(!body.contains("kept"));
    }

    #[test]
    fn reasoning_item_carries_the_item_id() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "gpt-5",
            vec![
                Content::Thinking {
                    thinking: "kept".into(),
                    signature: Some(crate::openai::responses::pack_reasoning(
                        Some("rs_1"),
                        "enc_1",
                    )),
                },
                Content::text("after"),
            ],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(
            r#""type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"enc_1""#
        ));
    }

    #[test]
    fn a_trailing_reasoning_item_is_dropped() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "gpt-5",
            vec![
                Content::text("hi"),
                Content::Thinking {
                    thinking: "plan".into(),
                    signature: Some(crate::openai::responses::pack_reasoning(
                        Some("rs_1"),
                        "enc_1",
                    )),
                },
            ],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("rs_1"));
        assert!(!body.contains("enc_1"));
        assert!(body.contains(r#""text":"hi""#));
    }

    #[test]
    fn an_assistant_with_only_foreign_thinking_disappears() {
        let foreign = AssistantMessage::new(
            "anthropic",
            "claude-sonnet-4-5",
            vec![Content::Thinking {
                thinking: "secret".into(),
                signature: Some("sig".into()),
            }],
        );
        let body = body_of(&[
            Message::user("before"),
            Message::assistant(foreign),
            Message::user("after"),
        ]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("secret"));
        assert!(!body.contains(r#""role":"assistant""#));
        assert!(body.ends_with(
            r#"[{"type":"message","role":"user","content":[{"type":"input_text","text":"before"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"after"}]}]}"#
        ));
    }

    #[test]
    fn empty_tool_arguments_become_an_empty_object() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "gpt-5",
            vec![Content::ToolCall(ToolCall {
                id: "call_10".into(),
                name: "read".into(),
                arguments: "".into(),
            })],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#""arguments":"{}""#));
    }

    #[test]
    fn a_level_reasoning_text_is_written_as_an_effort_with_a_summary() {
        let builder = RequestBuilder::new("m", 1024, None, Some("medium"), None, &[]);
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(
            head.contains(r#""include":["reasoning.encrypted_content"]"#),
            "{head}"
        );
        assert!(
            head.contains(r#""reasoning":{"effort":"medium","summary":"auto"}"#),
            "{head}"
        );
        assert!(
            head.contains(r#""max_output_tokens":1024"#),
            "the cap does not grow: {head}"
        );
    }

    #[test]
    fn a_budget_reasoning_text_is_written_as_the_effort_text() {
        let builder = RequestBuilder::new("m", 1024, None, Some("4096"), None, &[]);
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(
            head.contains(r#""reasoning":{"effort":"4096","summary":"auto"}"#),
            "{head}"
        );
        assert!(
            head.contains(r#""max_output_tokens":1024"#),
            "the cap does not grow: {head}"
        );
    }

    #[test]
    fn an_off_reasoning_text_writes_no_reasoning() {
        for text in ["off", "none"] {
            let builder = RequestBuilder::new("m", 128, None, Some(text), None, &[]);
            let head = std::str::from_utf8(builder.head()).unwrap();
            assert!(!head.contains("reasoning"), "{text}: {head}");
            assert!(
                head.contains(r#""max_output_tokens":128"#),
                "{text}: {head}"
            );
        }
    }

    #[test]
    fn a_request_without_reasoning_has_no_reasoning_fields() {
        let builder = RequestBuilder::new("m", 128, None, None, None, &[]);
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(head.contains(r#""max_output_tokens":128"#));
        assert!(!head.contains("reasoning"));
        assert!(!head.contains("prompt_cache_key"));
    }

    #[test]
    fn reasoning_off_writes_the_off_word_without_encrypted_content() {
        let builder = RequestBuilder::with_compat(
            "m",
            128,
            None,
            Some("off"),
            None,
            &[],
            Compat {
                reasoning_off: Some("none"),
                ..Compat::DEFAULT
            },
        );
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(head.contains(r#""reasoning":{"effort":"none"}"#), "{head}");
        assert!(!head.contains("reasoning.encrypted_content"), "{head}");
        assert!(!head.contains("summary"), "{head}");
    }

    #[test]
    fn reasoning_off_without_a_word_writes_no_reasoning() {
        for text in ["off", "none", "0"] {
            let builder = RequestBuilder::new("m", 128, None, Some(text), None, &[]);
            let head = std::str::from_utf8(builder.head()).unwrap();
            assert!(!head.contains("reasoning"), "{text}: {head}");
        }
    }

    #[test]
    fn chatgpt_preset_head_has_no_cap_and_keeps_store_and_stream() {
        let builder = RequestBuilder::with_compat(
            "gpt-5",
            128,
            None,
            Some("off"),
            None,
            &[],
            crate::providers::get("chatgpt")
                .expect("the preset")
                .default_route()
                .compat,
        );
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(!head.contains("max_output_tokens"), "{head}");
        assert!(!head.contains("max_tokens"), "{head}");
        assert!(head.contains(r#""store":false"#), "{head}");
        assert!(head.contains(r#""stream":true"#), "{head}");
        assert!(head.contains(r#""reasoning":{"effort":"none"}"#), "{head}");
    }

    #[test]
    fn request_body_is_stable_for_the_same_history() {
        let provider = crate::openai::responses::OpenAiResponses::new("key", "http://localhost");
        let history = vec![Message::user("one")];
        let request = Request {
            model: "gpt-5",
            max_tokens: 128,
            system: None,
            tools: &[],
            messages: &history,
            images: true,
            reasoning: None,
            cache_key: None,
        };
        let first = provider.request_body(&request).unwrap();
        let second = provider.request_body(&request).unwrap();
        assert_eq!(first, second);
    }
}
