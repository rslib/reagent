//! The ACP v1 messages this agent reads and writes.
//!
//! The shapes are the schema's, with the fields an editor sends and this agent
//! answers. Keys are camelCase and discriminators snake_case, so each field
//! that needs it names its own key.

use std::borrow::Cow;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// One line the editor wrote, whatever it carries.
#[derive(Debug, Deserialize)]
pub(super) struct Line {
    /// The id of a request, or of an answer to a request of this agent.
    #[serde(default)]
    pub(super) id: Option<Value>,
    /// The method of a request or a notification.
    #[serde(default)]
    pub(super) method: Option<String>,
    /// The params of a request or a notification.
    #[serde(default)]
    pub(super) params: Option<Value>,
    /// The result of an answer to a request of this agent.
    #[serde(default)]
    pub(super) result: Option<Value>,
    /// The error of an answer to a request of this agent.
    #[serde(default)]
    pub(super) error: Option<Value>,
}

/// The params of `initialize`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Initialize {
    /// The client that runs this agent, as it names itself.
    #[serde(default)]
    pub(super) client_info: Option<ClientInfo>,
    /// What the client says it can show.
    #[serde(default)]
    pub(super) client_capabilities: Option<ClientCapabilities>,
}

/// The `clientCapabilities` of `initialize`.
#[derive(Debug, Deserialize)]
pub(super) struct ClientCapabilities {
    /// What the client shows of a session.
    #[serde(default)]
    pub(super) session: Option<ClientSession>,
    /// What the client shows of a question this agent asks.
    #[serde(default)]
    pub(super) elicitation: Option<ClientElicitation>,
}

/// The `elicitation` of `clientCapabilities`.
#[derive(Debug, Deserialize)]
pub(super) struct ClientElicitation {
    /// The form mode the client takes: `{}` advertises it, and `null` does not.
    #[serde(default)]
    pub(super) form: Option<Value>,
}

/// The `session` of `clientCapabilities`.
#[derive(Debug, Deserialize)]
pub(super) struct ClientSession {
    /// The notices the client takes: `{}` advertises them, and `null` does not.
    #[serde(default)]
    pub(super) notices: Option<Value>,
}

/// The `clientInfo` of `initialize`.
#[derive(Debug, Deserialize)]
pub(super) struct ClientInfo {
    /// The client's name.
    #[serde(default)]
    pub(super) name: Option<String>,
}

/// The params of `session/new`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NewSession {
    /// The folder the session runs in, absolute.
    pub(super) cwd: String,
    /// The MCP servers the session offers, each as a server spec.
    #[serde(default)]
    pub(super) mcp_servers: Vec<Value>,
}

/// The params of `session/load`, which are the ones of `session/resume` too.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LoadSession {
    /// The session to load.
    pub(super) session_id: String,
    /// The folder the session runs in, absolute.
    pub(super) cwd: String,
    /// The MCP servers the session offers, each as a server spec.
    #[serde(default)]
    pub(super) mcp_servers: Vec<Value>,
}

/// The params of `session/list`, each of them optional.
#[derive(Debug, Deserialize)]
pub(super) struct ListSessions {
    /// The folder to list, absolute, and nothing for every folder.
    #[serde(default)]
    pub(super) cwd: Option<String>,
    /// The token that ends the page before the one asked for.
    #[serde(default)]
    pub(super) cursor: Option<String>,
}

/// The head of one line the editor wrote: its id and its method, read without
/// the params, so a large prompt is not copied just to see what it is.
#[derive(Debug, Deserialize)]
pub(super) struct Head {
    /// The id of a request.
    #[serde(default)]
    pub(super) id: Option<Value>,
    /// The method of a request or a notification.
    #[serde(default)]
    pub(super) method: Option<String>,
}

/// A `session/prompt` line, whose params borrow from the bytes of the line.
#[derive(Debug, Deserialize)]
pub(super) struct PromptCall<'a> {
    /// The params of the request.
    #[serde(default, borrow)]
    pub(super) params: Option<Prompt<'a>>,
}

/// The params of `session/prompt`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Prompt<'a> {
    /// The session the prompt belongs to.
    pub(super) session_id: String,
    /// The blocks of the prompt, in the order the editor made them.
    #[serde(default, borrow)]
    pub(super) prompt: Vec<Block<'a>>,
}

/// One block of a prompt.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Block<'a> {
    /// Visible text.
    Text {
        /// The text itself.
        #[serde(borrow)]
        text: Cow<'a, str>,
    },
    /// A link to a file the session can read.
    ResourceLink {
        /// What the link points at.
        #[serde(borrow)]
        uri: Cow<'a, str>,
        /// The name the editor shows.
        #[serde(borrow)]
        name: Cow<'a, str>,
    },
    /// A file the editor embedded.
    Resource {
        /// The file, as the editor sent it.
        #[serde(borrow)]
        resource: ResourceBody<'a>,
    },
    /// A picture, which this agent writes to the session's own folder for the
    /// worker to attach.
    Image {
        /// The bytes of the picture, as base64 text.
        #[serde(borrow)]
        data: Cow<'a, str>,
        /// The media type of the picture.
        #[serde(borrow, rename = "mimeType")]
        mime_type: Cow<'a, str>,
    },
    /// A recording. This agent advertises none, so it is refused.
    Audio {},
    /// A block this build does not know, which is refused too.
    #[serde(other)]
    Other,
}

/// The `resource` of a [`Block::Resource`].
#[derive(Debug, Deserialize)]
pub(super) struct ResourceBody<'a> {
    /// Where the file is.
    #[serde(default, borrow)]
    pub(super) uri: Cow<'a, str>,
    /// The text of the file, and nothing for a blob.
    #[serde(default, borrow)]
    pub(super) text: Option<Cow<'a, str>>,
    /// The bytes of the file, as base64 text, and nothing for a text file.
    #[serde(default, borrow)]
    pub(super) blob: Option<Cow<'a, str>>,
    /// The media type of the file, which says whether the blob is a picture.
    #[serde(default, borrow, rename = "mimeType")]
    pub(super) mime_type: Option<Cow<'a, str>>,
}

/// The params of `session/update`, which this agent sends.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SessionUpdate<'a, U> {
    /// The session the change belongs to.
    pub(super) session_id: &'a str,
    /// The change itself.
    pub(super) update: U,
}

/// One change of a session.
#[derive(Serialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub(super) enum Change<'a> {
    /// A piece of the agent's visible text.
    AgentMessageChunk {
        content: Chunk<'a>,
        /// The message the piece belongs to.
        #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
        message_id: Option<&'a str>,
    },
    /// A piece of the model's own reasoning.
    AgentThoughtChunk {
        content: Chunk<'a>,
        /// The message the piece belongs to.
        #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
        message_id: Option<&'a str>,
    },
    /// A message the user sent.
    UserMessageChunk {
        content: Chunk<'a>,
        /// The message the piece belongs to.
        #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
        message_id: Option<&'a str>,
    },
    /// A tool call that started.
    ToolCall {
        /// The call's id.
        #[serde(rename = "toolCallId")]
        tool_call_id: &'a str,
        /// The line the editor shows.
        title: String,
        /// The programmatic name of the tool.
        name: &'a str,
        /// What kind of work the call does.
        kind: Kind,
        /// Where the call stands.
        status: Status,
        /// The call's arguments, once they are known.
        #[serde(rename = "rawInput", skip_serializing_if = "Option::is_none")]
        raw_input: Option<Value>,
    },
    /// A change of a tool call this agent reported.
    ToolCallUpdate {
        /// The call's id.
        #[serde(rename = "toolCallId")]
        tool_call_id: &'a str,
        /// Where the call stands now.
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<Status>,
        /// The line the editor shows, once the arguments name it.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// The call's arguments.
        #[serde(rename = "rawInput", skip_serializing_if = "Option::is_none")]
        raw_input: Option<Value>,
        /// What the call produced.
        #[serde(rename = "rawOutput", skip_serializing_if = "Option::is_none")]
        raw_output: Option<&'a str>,
        /// The files the call works in.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        locations: Vec<Location>,
        /// What the call produced, once it did.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        content: Vec<ToolContent<'a>>,
    },
    /// The commands the session offers, which replaces the list the editor has.
    AvailableCommandsUpdate {
        /// The commands, in name order.
        #[serde(rename = "availableCommands")]
        available_commands: Vec<AvailableCommand<'a>>,
    },
    /// The title of the session, once it has one.
    SessionInfoUpdate {
        /// The title the editor shows.
        title: &'a str,
        /// When the session last changed, as ISO 8601.
        #[serde(rename = "updatedAt")]
        updated_at: String,
    },
    /// The tokens the session used, and the window they used.
    UsageUpdate {
        /// The tokens of the context the session holds.
        used: u64,
        /// The tokens the context window of the model holds.
        size: u64,
        /// What the provider charged, when it says.
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<Cost>,
    },
    /// One live event of the session, which a client that advertises notices
    /// takes instead of an agent message.
    Notice {
        /// How serious it is.
        severity: Severity,
        /// The one line the editor shows.
        title: String,
        /// What happened, when one line is not enough.
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// Every config option of the session, after a change.
    ConfigOptionUpdate {
        /// The options as they stand now.
        #[serde(rename = "configOptions")]
        config_options: Value,
    },
    /// The mode the session decides with now.
    CurrentModeUpdate {
        /// The mode's id.
        #[serde(rename = "currentModeId")]
        current_mode_id: &'a str,
    },
}

/// One command the session offers: a slash command of the editor.
#[derive(Serialize)]
pub(super) struct AvailableCommand<'a> {
    /// The name the user types after the slash.
    pub(super) name: &'a str,
    /// The line the editor shows beside it.
    pub(super) description: &'a str,
    /// What the command takes, when it takes arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) input: Option<CommandInput<'a>>,
}

/// The `input` of an [`AvailableCommand`]: unstructured text, and a hint.
#[derive(Serialize)]
pub(super) struct CommandInput<'a> {
    /// What the text is for, as the editor shows it.
    pub(super) hint: &'a str,
}

/// What a provider charged for a request.
#[derive(Serialize)]
pub(super) struct Cost {
    /// The amount.
    pub(super) amount: f64,
    /// Its unit, an ISO 4217 code such as `USD`.
    pub(super) currency: &'static str,
}

/// How serious a notice is.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(super) enum Severity {
    /// Something the editor should know, which costs nothing.
    Warning,
    /// Something that failed.
    Error,
}

/// One text content block.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Chunk<'a> {
    /// Text, of the agent, of the user or of a tool.
    Text {
        /// The text itself.
        text: Cow<'a, str>,
    },
}

/// One file a tool call works in.
#[derive(Serialize)]
pub(super) struct Location {
    /// The file, absolute.
    pub(super) path: String,
}

/// One thing a tool call produced.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ToolContent<'a> {
    /// The text the call produced.
    Content { content: Chunk<'a> },
    /// The change an edit or a write made.
    Diff {
        /// The file, absolute.
        path: String,
        /// What the file held, and nothing for a file the call made.
        #[serde(rename = "oldText")]
        old_text: Option<String>,
        /// What the file holds now.
        #[serde(rename = "newText")]
        new_text: String,
    },
}

/// What kind of work a tool call does.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    Read,
    Edit,
    Search,
    Execute,
    Fetch,
    Other,
}

/// Where a tool call stands.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(super) enum Status {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// The params of `session/request_permission`, which this agent sends.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Permission {
    /// The session that asks.
    pub(super) session_id: String,
    /// The call the answer is for.
    pub(super) tool_call: CallRef,
    /// What the editor may answer.
    pub(super) options: Vec<PermissionOption>,
}

/// The params of `elicitation/create`, which this agent sends for a question
/// set when the editor advertises that it shows a form.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Elicit {
    /// The session that asks.
    pub(super) session_id: String,
    /// The mode asked for.
    pub(super) mode: ElicitMode,
    /// The line the editor shows above the form.
    pub(super) message: String,
    /// What the form asks for, as a JSON schema of the answers.
    pub(super) requested_schema: Value,
}

/// The mode of `elicitation/create`.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(super) enum ElicitMode {
    /// A form the editor shows, one field per answer.
    Form,
}

/// The call a permission request names.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CallRef {
    /// The call's id.
    pub(super) tool_call_id: String,
}

/// One answer a permission request offers.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PermissionOption {
    /// The id the answer names, as this agent reads it back.
    pub(super) option_id: String,
    /// The line the editor shows.
    pub(super) name: String,
    /// What kind of answer it is.
    pub(super) kind: OptionKind,
}

/// What kind of answer a permission option is.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(super) enum OptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
}

/// One JSON-RPC message this agent sends.
#[derive(Serialize)]
pub(super) struct Note<'a, P> {
    /// The version of the protocol.
    pub(super) jsonrpc: &'static str,
    /// The method the note names.
    pub(super) method: &'a str,
    /// What it carries.
    pub(super) params: P,
}

/// A request of this agent to the editor.
#[derive(Serialize)]
pub(super) struct Call<'a, P> {
    /// The version of the protocol.
    pub(super) jsonrpc: &'static str,
    /// The id the answer names.
    pub(super) id: u64,
    /// The method the request names.
    pub(super) method: &'a str,
    /// What it carries.
    pub(super) params: P,
}

/// The answer of this agent to a request of the editor.
#[derive(Serialize)]
pub(super) struct Response {
    /// The version of the protocol.
    pub(super) jsonrpc: &'static str,
    /// The id of the request it answers.
    pub(super) id: Value,
    /// What the request came to.
    pub(super) result: Value,
}

/// A failed answer of this agent to a request of the editor.
#[derive(Serialize)]
pub(super) struct Failed {
    /// The version of the protocol.
    pub(super) jsonrpc: &'static str,
    /// The id of the request it answers.
    pub(super) id: Value,
    /// Why it failed.
    pub(super) error: ErrorObject,
}

/// The error of a failed answer.
#[derive(Serialize)]
pub(super) struct ErrorObject {
    /// The code, from the protocol's table.
    pub(super) code: i32,
    /// What went wrong.
    pub(super) message: String,
    /// What the error names, such as the session it could not find.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) data: Option<Value>,
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::Block;
    use super::PromptCall;

    #[test]
    fn the_base64_text_of_an_image_borrows_from_the_line() {
        let line = br#"{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s","prompt":[{"type":"image","data":"aGk=","mimeType":"image/png"}]}}"#;
        let call: PromptCall<'_> = serde_json::from_slice(line).expect("a prompt");
        let prompt = call.params.expect("params");
        match &prompt.prompt[0] {
            Block::Image { data, .. } => assert!(matches!(data, Cow::Borrowed(_))),
            other => panic!("expected an image, got {other:?}"),
        }
    }
}
