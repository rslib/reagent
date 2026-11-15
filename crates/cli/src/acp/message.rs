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

/// The params of `session/load`.
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

/// The params of `session/prompt`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Prompt {
    /// The session the prompt belongs to.
    pub(super) session_id: String,
    /// The blocks of the prompt, in the order the editor made them.
    #[serde(default)]
    pub(super) prompt: Vec<Block>,
}

/// One block of a prompt.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Block {
    /// Visible text.
    Text {
        /// The text itself.
        text: String,
    },
    /// A link to a file the session can read.
    ResourceLink {
        /// What the link points at.
        uri: String,
        /// The name the editor shows.
        name: String,
    },
    /// A file the editor embedded.
    Resource {
        /// The file, as the editor sent it.
        resource: ResourceBody,
    },
    /// A picture. This agent advertises none, so it is refused.
    Image {},
    /// A recording. This agent advertises none, so it is refused.
    Audio {},
    /// A block this build does not know, which is refused too.
    #[serde(other)]
    Other,
}

/// The `resource` of a [`Block::Resource`].
#[derive(Debug, Deserialize)]
pub(super) struct ResourceBody {
    /// Where the file is.
    #[serde(default)]
    pub(super) uri: String,
    /// The text of the file, and nothing for a blob.
    #[serde(default)]
    pub(super) text: Option<String>,
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
    AgentMessageChunk { content: Chunk<'a> },
    /// A piece of the model's own reasoning.
    AgentThoughtChunk { content: Chunk<'a> },
    /// A message the user sent.
    UserMessageChunk { content: Chunk<'a> },
    /// A tool call that started.
    ToolCall {
        /// The call's id.
        #[serde(rename = "toolCallId")]
        tool_call_id: &'a str,
        /// The line the editor shows.
        title: String,
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
        /// The files the call works in.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        locations: Vec<Location>,
        /// What the call produced, once it did.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        content: Vec<ToolContent<'a>>,
    },
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
