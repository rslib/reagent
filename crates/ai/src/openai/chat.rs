//! OpenAI-compatible Chat Completions provider.

use std::sync::Mutex;

use bytes::{BufMut, Bytes, BytesMut};
use futures_util::TryStreamExt;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::http::ExtraHeaders;
use crate::key::ApiKey;
use crate::openai::{BodyCache, JsonStr};
use crate::types::{
    AssistantMessage, Content, Message, ReasoningLevel, StopReason, Text, ToolSpec,
};
use crate::{Provider, Request, StreamEvent, Usage};

/// The provider name recorded on assistant messages.
const PROVIDER: &str = "openai-chat";

/// Tuning for a Chat Completions endpoint.
#[derive(Debug, Clone, Copy)]
pub struct ChatOptions {
    /// Send `max_completion_tokens` instead of `max_tokens`.
    pub max_completion_tokens: bool,
}

/// Talks to an OpenAI-compatible Chat Completions endpoint over HTTP.
#[derive(Debug)]
pub struct OpenAiChat {
    client: reqwest::Client,
    api_key: ApiKey,
    base_url: String,
    options: ChatOptions,
    headers: ExtraHeaders,
    head: Mutex<Option<Head>>,
    bodies: Mutex<BodyCache<MessageBody>>,
}

#[derive(Debug)]
struct Head {
    model: String,
    max_tokens: u32,
    system: Option<Text>,
    reasoning: Option<ReasoningLevel>,
    tools: Vec<ToolSpec>,
    builder: RequestBuilder,
}

impl OpenAiChat {
    /// Creates a provider for `base_url`, for example `http://localhost:11434/v1`.
    ///
    /// `max_completion_tokens` is on for `api.openai.com` and off elsewhere.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        let options = ChatOptions {
            max_completion_tokens: base_url.contains("api.openai.com"),
        };
        Self {
            client: crate::http::client(),
            api_key: ApiKey::new(api_key.into()),
            base_url,
            options,
            headers: ExtraHeaders::default(),
            head: Mutex::new(None),
            bodies: Mutex::new(BodyCache::default()),
        }
    }

    /// Adds extra request headers to every request.
    ///
    /// A header that [`crate::check_header`] rejects is dropped, so an extra
    /// header never replaces the credential or the body encoding. `Debug`
    /// prints header names only, never values.
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = ExtraHeaders::new(headers);
        self
    }

    /// Replaces the endpoint tuning.
    pub fn with_options(mut self, options: ChatOptions) -> Self {
        self.options = options;
        self
    }

    /// The Chat Completions URL every request goes to.
    pub fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// Serializes the request body, reusing the bytes of unchanged messages.
    pub(crate) fn request_body(&self, request: &Request<'_>) -> Result<Bytes, Error> {
        let reasoning = request.reasoning;
        let mut head = self.head.lock().expect("head cache lock");
        let reuse = head.as_ref().is_some_and(|cached| {
            cached.model == request.model
                && cached.max_tokens == request.max_tokens
                && cached.system.as_deref() == request.system.map(|system| &**system)
                && cached.reasoning == reasoning
                && cached.tools.as_slice() == request.tools
        });
        if !reuse {
            *head = Some(Head {
                model: request.model.to_string(),
                max_tokens: request.max_tokens,
                system: request.system.cloned(),
                reasoning,
                tools: request.tools.to_vec(),
                builder: RequestBuilder::new(
                    request.model,
                    request.max_tokens,
                    request.system.map(|system| &**system),
                    reasoning,
                    request.tools,
                    self.options.max_completion_tokens,
                ),
            });
        }
        let builder = &head.as_ref().expect("head is cached").builder;

        let mut bodies = self.bodies.lock().expect("body cache lock");
        bodies.align(request.messages, |message| {
            MessageBody::new(message, PROVIDER)
        })?;
        Ok(builder.body(bodies.bodies()))
    }
}

impl Provider for OpenAiChat {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    fn stream(&self, request: &Request<'_>) -> BoxStream<'static, Result<StreamEvent, Error>> {
        let body = match self.request_body(request) {
            Ok(body) => body,
            Err(error) => return Box::pin(futures_util::stream::iter([Err(error)])),
        };
        let url = self.url();
        let api_key = self.api_key.reveal().to_owned();
        let client = self.client.clone();
        let headers = self.headers.clone();

        let connect = futures_util::stream::once(async move {
            let request = client
                .post(url)
                .header("authorization", format!("Bearer {api_key}"))
                .header("content-type", "application/json");
            let response = headers
                .apply(request)
                .body(body)
                .send()
                .await
                .map_err(|error| Error::Http(error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let retry = crate::Retry::from_headers(response.headers());
                let text = response.text().await.unwrap_or_default();
                return Err(Error::Status {
                    code: status.as_u16(),
                    body: text,
                    retry,
                });
            }
            let bytes = response
                .bytes_stream()
                .map_err(|error| Error::Http(error.to_string()));
            Ok(bytes)
        })
        .try_flatten();

        parse_stream(connect)
    }
}

/// Turns response bytes into events, with at most one error per stream.
fn parse_stream<S>(connect: S) -> BoxStream<'static, Result<StreamEvent, Error>>
where
    S: futures_util::Stream<Item = Result<Bytes, Error>> + Send + 'static,
{
    crate::openai::event_stream(connect, StreamParser::new())
}

/// One message serialized in Chat Completions wire form, reusable across requests.
#[derive(Debug, Clone)]
pub struct MessageBody {
    bytes: Option<Bytes>,
}

impl MessageBody {
    /// Serializes one message for the provider named `provider`.
    ///
    /// Thinking from another provider is dropped. Tool arguments are passed
    /// through as the raw JSON string the model emitted; an empty argument
    /// string becomes an empty object. An assistant message with nothing kept
    /// produces no body at all.
    pub fn new(message: &Message, provider: &str) -> Result<Self, Error> {
        let bytes = match message {
            Message::User { content } => Some(to_bytes(&UserOut {
                role: "user",
                content: content
                    .iter()
                    .filter_map(|content| match content {
                        Content::Text { text } => Some(Part {
                            kind: "text",
                            text: text.as_ref(),
                        }),
                        _ => None,
                    })
                    .collect(),
            })),
            Message::Assistant(assistant) => assistant_message(assistant, provider),
            Message::ToolResult(result) => Some(to_bytes(&ToolOut {
                role: "tool",
                tool_call_id: result.call_id.as_str(),
                content: result.content.as_ref(),
            })),
        };
        Ok(Self { bytes })
    }
}

/// Builds a Chat Completions body from a stable head and reusable messages.
///
/// The head holds the model, the reasoning effort, the token cap, the tools and
/// the system prompt. Tools are sorted by name.
#[derive(Debug)]
pub struct RequestBuilder {
    head: Bytes,
    system: Option<Bytes>,
}

impl RequestBuilder {
    /// Serializes the request head once.
    pub fn new(
        model: &str,
        max_tokens: u32,
        system: Option<&str>,
        reasoning: Option<ReasoningLevel>,
        tools: &[ToolSpec],
        max_completion_tokens: bool,
    ) -> Self {
        let mut out = BytesMut::new();
        out.put_slice(b"{\"model\":");
        out.put_slice(json_string(model).as_bytes());
        if let Some(level) = reasoning {
            out.put_slice(b",\"reasoning_effort\":");
            out.put_slice(json_string(reasoning_effort(level)).as_bytes());
        }
        let budget = reasoning.map_or(0, reasoning_budget);
        if max_completion_tokens {
            out.put_slice(b",\"max_completion_tokens\":");
        } else {
            out.put_slice(b",\"max_tokens\":");
        }
        out.put_slice(max_tokens.saturating_add(budget).to_string().as_bytes());
        out.put_slice(b",\"stream\":true,\"stream_options\":{\"include_usage\":true}");
        if tools.is_empty() {
            out.put_slice(b",\"messages\":[");
        } else {
            out.put_slice(b",\"tools\":[");
            let mut sorted: Vec<&ToolSpec> = tools.iter().collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            for (index, tool) in sorted.iter().enumerate() {
                if index > 0 {
                    out.put_u8(b',');
                }
                out.put_slice(&to_bytes(&ChatTool {
                    kind: "function",
                    function: ChatFunction {
                        name: &tool.name,
                        description: &tool.description,
                        parameters: &tool.input_schema,
                    },
                }));
            }
            out.put_slice(b"],\"messages\":[");
        }
        Self {
            head: out.freeze(),
            system: system.map(|system| {
                to_bytes(&SystemOut {
                    role: "system",
                    content: system,
                })
            }),
        }
    }

    /// The prefix every body built from this request shares.
    pub fn head(&self) -> &Bytes {
        &self.head
    }

    /// Serializes a body from the head and the message bodies.
    pub fn body(&self, messages: &[MessageBody]) -> Bytes {
        let mut out = BytesMut::with_capacity(self.head.len() + 128 * messages.len());
        out.put_slice(&self.head);
        let mut first = true;
        if let Some(system) = &self.system {
            out.put_slice(system);
            first = false;
        }
        for message in messages {
            let Some(bytes) = &message.bytes else {
                continue;
            };
            if !first {
                out.put_u8(b',');
            }
            first = false;
            out.put_slice(bytes);
        }
        out.put_slice(b"]}");
        out.freeze()
    }
}

#[derive(Serialize)]
struct ChatTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatFunction<'a>,
}

#[derive(Serialize)]
struct ChatFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Serialize)]
struct SystemOut<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Serialize)]
struct UserOut<'a> {
    role: &'static str,
    content: Vec<Part<'a>>,
}

#[derive(Serialize)]
struct Part<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[derive(Serialize)]
struct ToolOut<'a> {
    role: &'static str,
    tool_call_id: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct AssistantOut<'a> {
    role: &'static str,
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCallOut<'a>>,
}

#[derive(Serialize)]
struct ToolCallOut<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionCallOut<'a>,
}

#[derive(Serialize)]
struct FunctionCallOut<'a> {
    name: &'a str,
    arguments: &'a str,
}

fn assistant_message(assistant: &AssistantMessage, provider: &str) -> Option<Bytes> {
    let keep_thinking = assistant.provider.as_ref() == provider;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for content in &assistant.content {
        match content {
            Content::Text { text: value } => text.push_str(value),
            Content::Thinking { thinking, .. } => {
                if keep_thinking {
                    reasoning.push_str(thinking);
                }
            }
            Content::RedactedThinking { .. } => {}
            Content::ToolCall(call) => tool_calls.push(ToolCallOut {
                id: call.id.as_str(),
                kind: "function",
                function: FunctionCallOut {
                    name: call.name.as_str(),
                    arguments: if call.arguments.trim().is_empty() {
                        "{}"
                    } else {
                        call.arguments.as_ref()
                    },
                },
            }),
        }
    }
    if text.is_empty() && reasoning.is_empty() && tool_calls.is_empty() {
        return None;
    }
    let content = (!text.is_empty()).then_some(text.as_str());
    let reasoning_content = (!reasoning.is_empty()).then_some(reasoning);
    Some(to_bytes(&AssistantOut {
        role: "assistant",
        content,
        reasoning_content,
        tool_calls,
    }))
}

fn reasoning_budget(level: ReasoningLevel) -> u32 {
    match level {
        ReasoningLevel::Low => 1_024,
        ReasoningLevel::Medium => 4_096,
        ReasoningLevel::High => 16_384,
    }
}

fn reasoning_effort(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a str serializes to a JSON string")
}

fn to_bytes(value: &impl Serialize) -> Bytes {
    Bytes::from(serde_json::to_vec(value).expect("request parts are serializable"))
}

/// Parses one Chat Completions SSE stream, line by line.
///
/// Tool call deltas carry an index instead of an id, so the parser maps each
/// index to the call id it first saw. Event fields borrow from the line handed
/// to [`StreamParser::push_line`].
#[derive(Debug, Default)]
pub struct StreamParser {
    usage: Usage,
    tool_ids: Vec<(u32, Text)>,
    stop_reason: Option<StopReason>,
    done: bool,
    failed: bool,
}

impl StreamParser {
    /// Creates a parser at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one SSE line and appends the events it carries to `out`.
    pub fn push_line(&mut self, line: &[u8], out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        if self.failed {
            return Ok(());
        }
        let Some(data) = data_of(line) else {
            return Ok(());
        };
        if data == b"[DONE]" {
            self.done = true;
            out.push(StreamEvent::Stop(
                self.stop_reason.take().unwrap_or(StopReason::EndTurn),
            ));
            return Ok(());
        }
        let data = match std::str::from_utf8(data) {
            Ok(data) => data,
            Err(error) => {
                self.failed = true;
                return Err(Error::Protocol(error.to_string()));
            }
        };
        let chunk: Chunk<'_> = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(error) => {
                self.failed = true;
                return Err(Error::Json(error));
            }
        };
        if let Some(error) = chunk.error {
            self.failed = true;
            let message = error.message.as_str().to_string();
            if error.kind.is_some_and(retryable) {
                return Err(Error::Transient(message));
            }
            return Err(Error::Protocol(message));
        }

        for choice in chunk.choices.into_choices() {
            if let Some(reasoning) = choice.delta.reasoning_content {
                out.push(StreamEvent::ThinkingDelta(Text::from(reasoning.as_str())));
            }
            if let Some(content) = choice.delta.content
                && !content.as_str().is_empty()
            {
                out.push(StreamEvent::TextDelta(Text::from(content.as_str())));
            }
            for call in choice.delta.tool_calls {
                self.accept_tool_call(call, out);
            }
            if let Some(reason) = choice.finish_reason {
                if matches!(reason, "tool_calls" | "function_call") {
                    for (_, id) in &self.tool_ids {
                        out.push(StreamEvent::ToolCallEnd { id: id.clone() });
                    }
                }
                self.stop_reason = Some(stop_reason(reason));
            }
        }
        if let Some(usage) = chunk.usage {
            self.usage = usage_of(usage);
            out.push(StreamEvent::Usage(self.usage));
        }
        Ok(())
    }

    /// Ends the stream after an error from outside the parser, such as HTTP.
    pub fn fail(&mut self) {
        self.failed = true;
    }

    /// Fails when the stream ended without its `[DONE]` sentinel.
    ///
    /// After an earlier error the stream is over and this is quiet.
    pub fn finish(&self) -> Result<(), Error> {
        if self.done || self.failed {
            Ok(())
        } else {
            Err(Error::Protocol("stream ended before [DONE]".into()))
        }
    }

    fn accept_tool_call(&mut self, call: ToolCallDelta<'_>, out: &mut Vec<StreamEvent>) {
        let index = match call.index {
            Some(index) => index,
            None => match call.id.as_ref().map(JsonStr::as_str) {
                Some(id) => self
                    .tool_ids
                    .iter()
                    .find(|(_, existing)| existing.as_ref() == id)
                    .map(|(index, _)| *index)
                    .unwrap_or_else(|| self.next_index()),
                None => self.tool_ids.last().map(|(index, _)| *index).unwrap_or(0),
            },
        };
        let id = self.id_for(index, call.id.as_ref().map(JsonStr::as_str));
        let Some(function) = call.function else {
            return;
        };
        if let Some(name) = function.name {
            out.push(StreamEvent::ToolCallStart {
                id: id.clone(),
                name: Text::from(name.as_str()),
            });
        }
        if let Some(arguments) = function.arguments
            && !arguments.as_str().is_empty()
        {
            out.push(StreamEvent::ToolCallDelta {
                id,
                partial_json: Text::from(arguments.as_str()),
            });
        }
    }

    fn next_index(&self) -> u32 {
        self.tool_ids
            .iter()
            .map(|(index, _)| index + 1)
            .max()
            .unwrap_or(0)
    }

    fn id_for(&mut self, index: u32, id: Option<&str>) -> Text {
        if let Some((_, existing)) = self.tool_ids.iter().find(|(seen, _)| *seen == index) {
            return existing.clone();
        }
        let id = match id {
            Some(id) => Text::from(id),
            None => Text::from(format!("call_{index}")),
        };
        self.tool_ids.push((index, id.clone()));
        id
    }
}

impl crate::openai::EventParser for StreamParser {
    fn push_line(&mut self, line: &[u8], out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        StreamParser::push_line(self, line, out)
    }

    fn fail(&mut self) {
        StreamParser::fail(self);
    }

    /// This parser emits nothing at the end of a clean stream.
    fn finish(&mut self, _out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        StreamParser::finish(self)
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::Error,
    }
}

fn usage_of(json: UsageJson) -> Usage {
    let cached = json.prompt_tokens_details.cached_tokens;
    Usage {
        input_tokens: json.prompt_tokens.saturating_sub(cached),
        output_tokens: json.completion_tokens,
        cache_read_tokens: cached,
        cache_write_tokens: 0,
        reasoning_tokens: json.completion_tokens_details.reasoning_tokens,
    }
}

fn data_of(line: &[u8]) -> Option<&[u8]> {
    let rest = line.strip_prefix(b"data:")?;
    let rest = rest.strip_prefix(b" ").unwrap_or(rest);
    (!rest.is_empty()).then_some(rest)
}

#[derive(Deserialize)]
struct Chunk<'a> {
    #[serde(default, borrow)]
    choices: Choices<'a>,
    #[serde(default)]
    usage: Option<UsageJson>,
    #[serde(default, borrow)]
    error: Option<ApiError<'a>>,
}

/// The choices of one chunk.
///
/// The common reply carries one choice, so the first is held inline and a
/// chunk with a single choice allocates nothing.
#[derive(Default)]
struct Choices<'a> {
    first: Option<Choice<'a>>,
    rest: Vec<Choice<'a>>,
}

impl<'a> Choices<'a> {
    /// Consumes the choices in reply order.
    fn into_choices(self) -> impl Iterator<Item = Choice<'a>> {
        self.first.into_iter().chain(self.rest)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Choices<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ChoicesVisitor<'a>(std::marker::PhantomData<&'a ()>);

        impl<'de: 'a, 'a> serde::de::Visitor<'de> for ChoicesVisitor<'a> {
            type Value = Choices<'a>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a list of choices")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let first = seq.next_element::<Choice<'a>>()?;
                let mut rest = Vec::new();
                while let Some(choice) = seq.next_element::<Choice<'a>>()? {
                    rest.push(choice);
                }
                Ok(Choices { first, rest })
            }
        }

        deserializer.deserialize_seq(ChoicesVisitor(std::marker::PhantomData))
    }
}

#[derive(Deserialize)]
struct Choice<'a> {
    #[serde(default, borrow)]
    delta: Delta<'a>,
    #[serde(default, borrow)]
    finish_reason: Option<&'a str>,
}

#[derive(Default, Deserialize)]
struct Delta<'a> {
    #[serde(default, borrow)]
    content: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    reasoning_content: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    tool_calls: Vec<ToolCallDelta<'a>>,
}

#[derive(Deserialize)]
struct ToolCallDelta<'a> {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default, borrow)]
    id: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    function: Option<FunctionDelta<'a>>,
}

#[derive(Deserialize)]
struct FunctionDelta<'a> {
    #[serde(default, borrow)]
    name: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    arguments: Option<JsonStr<'a>>,
}

#[derive(Deserialize)]
struct ApiError<'a> {
    #[serde(default, borrow)]
    message: JsonStr<'a>,
    #[serde(default, borrow, rename = "type")]
    kind: Option<&'a str>,
}

/// Whether an OpenAI stream error is safe to retry: server or rate-limit
/// family types are transient, everything else is not.
fn retryable(kind: &str) -> bool {
    let kind = kind.to_ascii_lowercase();
    kind.contains("server") || kind.contains("rate_limit") || kind.contains("overload")
}

#[derive(Default, Deserialize)]
struct UsageJson {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: PromptDetails,
    #[serde(default)]
    completion_tokens_details: CompletionDetails,
}

#[derive(Default, Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Default, Deserialize)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

#[cfg(test)]
mod tests {
    use futures_util::{FutureExt, StreamExt};

    use super::*;
    use crate::Request;

    const PROVIDER: &str = "openai-chat";

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
            "glm-4.6",
            1024,
            Some("You are a careful coding agent."),
            Some(ReasoningLevel::Medium),
            &tools,
            false,
        )
    }

    fn golden_messages() -> Vec<MessageBody> {
        let mut assistant = AssistantMessage::new(
            PROVIDER,
            "glm-4.6",
            vec![
                Content::text("hi"),
                Content::ToolCall(crate::types::ToolCall {
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

    fn parse_line(parser: &mut StreamParser, line: &[u8]) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        parser.push_line(line, &mut out).expect("line parses");
        out
    }

    fn parse(fixture: &str) -> Vec<StreamEvent> {
        let mut parser = StreamParser::new();
        let mut events = Vec::new();
        for line in fixture.lines() {
            parser
                .push_line(line.as_bytes(), &mut events)
                .expect("fixture parses");
        }
        parser.finish().expect("[DONE] was seen");
        events
    }

    #[test]
    fn body_matches_the_golden_request() {
        let body = golden_builder().body(&golden_messages());
        let golden = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_chat_request.json"
        ));
        assert_eq!(std::str::from_utf8(&body).unwrap(), golden.trim_end());
    }

    #[test]
    fn provider_name_and_base_url_are_configurable() {
        let provider = OpenAiChat::new("key", "http://localhost:11434/v1");
        assert_eq!(provider.name(), PROVIDER);
        assert_eq!(provider.url(), "http://localhost:11434/v1/chat/completions");
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
                    "glm-4.6",
                    vec![Content::text("hi")],
                )),
                PROVIDER,
            )
            .unwrap(),
        );

        let short = builder.body(&one);
        let long = builder.body(&two);
        assert!(long.starts_with(&short[..short.len() - 2]));
    }

    #[test]
    fn foreign_thinking_is_dropped_and_own_thinking_round_trips() {
        let foreign = Message::assistant(AssistantMessage::new(
            "anthropic",
            "claude-sonnet-4-5",
            vec![Content::Thinking {
                thinking: "secret".into(),
                signature: Some("sig".into()),
            }],
        ));
        let body = body_of(&[foreign]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("secret"));
        assert!(!body.contains("reasoning_content"));

        let own = Message::assistant(AssistantMessage::new(
            PROVIDER,
            "glm-4.6",
            vec![
                Content::Thinking {
                    thinking: "kept".into(),
                    signature: None,
                },
                Content::text("visible"),
            ],
        ));
        let body = body_of(&[own]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#""reasoning_content":"kept""#));
        assert!(body.contains(r#""content":"visible""#));
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
        let body = body_of(&[Message::assistant(foreign)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("secret"));
        assert!(!body.contains("assistant"));
    }

    #[test]
    fn empty_tool_arguments_become_an_empty_object() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "glm-4.6",
            vec![Content::ToolCall(crate::types::ToolCall {
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
    fn a_request_without_reasoning_has_no_reasoning_fields() {
        let builder = RequestBuilder::new("m", 128, None, None, &[], false);
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(head.contains(r#""max_tokens":128"#));
        assert!(!head.contains("reasoning_effort"));
        assert!(!head.contains("tools"));
    }

    #[test]
    fn openai_base_url_uses_max_completion_tokens() {
        let history = vec![Message::user("hi")];
        let request = Request {
            model: "gpt-5",
            max_tokens: 128,
            system: None,
            tools: &[],
            messages: &history,
            reasoning: None,
            cache_key: None,
        };

        let openai = OpenAiChat::new("key", "https://api.openai.com/v1");
        assert!(openai.options.max_completion_tokens);
        let bytes = openai.request_body(&request).unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains(r#""max_completion_tokens":128"#));
        assert!(!body.contains(r#""max_tokens""#));

        let other = OpenAiChat::new("key", "http://localhost:11434/v1");
        assert!(!other.options.max_completion_tokens);
        let bytes = other.request_body(&request).unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains(r#""max_tokens":128"#));
        assert!(!body.contains("max_completion_tokens"));
    }

    #[test]
    fn parses_tool_call_deltas_keyed_by_index() {
        let events = parse(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_chat_tool.sse"
        )));
        let mut arguments = String::new();
        let mut started = None;
        let mut ended = None;
        let mut text = String::new();
        let mut usage = Usage::default();
        for event in &events {
            match event {
                StreamEvent::ToolCallStart { id, name } => {
                    started = Some((id.to_string(), name.to_string()));
                }
                StreamEvent::ToolCallDelta { partial_json, .. } => {
                    arguments.push_str(partial_json);
                }
                StreamEvent::ToolCallEnd { id } => ended = Some(id.to_string()),
                StreamEvent::TextDelta(delta) => text.push_str(delta),
                StreamEvent::Usage(seen) => usage = *seen,
                _ => {}
            }
        }
        assert_eq!(text, "I will read it.");
        assert_eq!(started, Some(("call_02".to_string(), "read".to_string())));
        assert_eq!(ended, Some("call_02".to_string()));
        assert_eq!(arguments, "{\"path\":\"a.rs\"}");
        assert_eq!(usage.input_tokens, 40);
        assert_eq!(usage.cache_read_tokens, 120);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(events.last(), Some(&StreamEvent::Stop(StopReason::ToolUse)));
    }

    #[test]
    fn maps_reasoning_content_to_thinking_deltas() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"hmm"},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, line),
            vec![StreamEvent::ThinkingDelta("hmm".into())]
        );
    }

    #[test]
    fn push_line_appends_to_the_callers_buffer() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let first =
            br#"data: {"choices":[{"index":0,"delta":{"content":"a"},"finish_reason":null}]}"#;
        let second =
            br#"data: {"choices":[{"index":0,"delta":{"content":"b"},"finish_reason":null}]}"#;
        parser.push_line(first, &mut out).unwrap();
        assert_eq!(out, vec![StreamEvent::TextDelta("a".into())]);
        parser.push_line(second, &mut out).unwrap();
        assert_eq!(
            out,
            vec![
                StreamEvent::TextDelta("a".into()),
                StreamEvent::TextDelta("b".into())
            ],
            "the second line extends the same buffer"
        );
    }

    #[test]
    fn a_tool_call_without_an_index_uses_the_next_index() {
        let mut parser = StreamParser::new();
        let first = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_a","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, first),
            vec![StreamEvent::ToolCallStart {
                id: "call_a".into(),
                name: "read".into()
            }]
        );
        let second = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"{}"}}]},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, second),
            vec![StreamEvent::ToolCallDelta {
                id: "call_a".into(),
                partial_json: "{}".into()
            }]
        );
        let third = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_b","function":{"name":"write","arguments":""}}]},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, third),
            vec![StreamEvent::ToolCallStart {
                id: "call_b".into(),
                name: "write".into()
            }]
        );
    }

    #[test]
    fn a_tool_call_without_an_id_gets_a_synthetic_one() {
        let mut parser = StreamParser::new();
        let start = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, start),
            vec![StreamEvent::ToolCallStart {
                id: "call_0".into(),
                name: "read".into()
            }]
        );
        let done = br#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
        assert_eq!(
            parse_line(&mut parser, done),
            vec![StreamEvent::ToolCallEnd {
                id: "call_0".into()
            }]
        );
    }

    #[test]
    fn a_rate_limit_error_chunk_is_transient() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"error":{"message":"rate limited","type":"rate_limit_error"}}"#;
        let mut out = Vec::new();
        let error = parser.push_line(line, &mut out).unwrap_err();
        assert!(matches!(error, Error::Transient(message) if message == "rate limited"));
    }

    #[test]
    fn a_non_retryable_error_chunk_is_a_protocol_error() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"error":{"message":"bad request","type":"invalid_request_error"}}"#;
        let mut out = Vec::new();
        let error = parser.push_line(line, &mut out).unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message == "bad request"));
    }

    #[test]
    fn an_error_ends_the_stream() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let error = parser
            .push_line(br#"data: {"error":{"message":"boom"}}"#, &mut out)
            .unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message == "boom"));
        let mut late = Vec::new();
        parser
            .push_line(
                br#"data: {"choices":[{"index":0,"delta":{"content":"late"},"finish_reason":null}]}"#,
                &mut late,
            )
            .unwrap();
        assert!(late.is_empty(), "no event follows an error");
        parser.finish().expect("finish is quiet after an error");
    }

    #[test]
    fn two_tool_calls_keep_their_own_ids() {
        let mut parser = StreamParser::new();
        let first = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read","arguments":""}},{"index":1,"id":"call_b","function":{"name":"write","arguments":""}}]},"finish_reason":null}]}"#;
        let events = parse_line(&mut parser, first);
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallStart {
                    id: "call_a".into(),
                    name: "read".into()
                },
                StreamEvent::ToolCallStart {
                    id: "call_b".into(),
                    name: "write".into()
                },
            ]
        );
        let second = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{}"}}]},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, second),
            vec![StreamEvent::ToolCallDelta {
                id: "call_b".into(),
                partial_json: "{}".into()
            }]
        );
    }

    #[test]
    fn missing_done_is_an_error() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        parser
            .push_line(
                br#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
                &mut out,
            )
            .unwrap();
        let error = parser.finish().unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message.contains("[DONE]")));
    }

    #[test]
    fn request_body_is_stable_for_the_same_history() {
        let provider = OpenAiChat::new("key", "http://localhost");
        let history = vec![Message::user("one")];
        let request = Request {
            model: "glm-4.6",
            max_tokens: 128,
            system: None,
            tools: &[],
            messages: &history,
            reasoning: None,
            cache_key: None,
        };
        let first = provider.request_body(&request).unwrap();
        let second = provider.request_body(&request).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn a_transport_error_is_the_only_error() {
        let connect = futures_util::stream::iter([Err(Error::Http("refused".into()))]);
        let items: Vec<_> = parse_stream(connect)
            .collect::<Vec<_>>()
            .now_or_never()
            .unwrap();
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], Err(Error::Http(message)) if message == "refused"));
    }

    const EXTRA_REPLY: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    #[tokio::test]
    async fn extra_headers_reach_the_chat_request() {
        let (base, recorded) = crate::http::test_server::serve(EXTRA_REPLY).await;
        let provider =
            OpenAiChat::new("test-key", base).with_headers(vec![("x-extra".into(), "yes".into())]);
        let request = Request {
            model: "test-model",
            max_tokens: 16,
            system: None,
            tools: &[],
            messages: &[],
            reasoning: None,
            cache_key: None,
        };
        let events: Vec<_> = provider.stream(&request).collect().await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let recorded = recorded.await.expect("the server recorded the request");
        assert_eq!(recorded.request_line, "POST /chat/completions HTTP/1.1");
        assert_eq!(recorded.header("x-extra"), Some("yes"));
        assert_eq!(recorded.header("authorization"), Some("Bearer test-key"));
    }
}
