//! Wire format: protocol versions, the modern `_meta` fields and JSON-RPC frames.

use std::io::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// The modern revision: no handshake, `_meta` on every request.
pub const MODERN: &str = "2026-07-28";

/// The legacy revisions this client speaks, newest first.
pub const LEGACY: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

/// The version the legacy handshake asks for.
pub const LEGACY_DEFAULT: &str = "2025-11-25";

/// Every version this client speaks, for error text.
pub const CLIENT_VERSIONS: &str = "2026-07-28, 2025-11-25, 2025-06-18, 2025-03-26";

/// The error a server sends when it speaks no version we asked for.
pub const UNSUPPORTED_VERSION: i64 = -32022;

/// The error for a method a server does not have.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// The error for a tool call whose header parameters do not match the tools.
pub const HEADER_MISMATCH: i64 = -32020;

/// The error for a request that needs a capability the client lacks.
pub const MISSING_CAPABILITY: i64 = -32021;

/// The notification that says the tool list changed.
pub const LIST_CHANGED: &str = "notifications/tools/list_changed";

/// The errors a server of the modern era sends.
pub const MODERN_ERRORS: [i64; 4] = [
    UNSUPPORTED_VERSION,
    MISSING_CAPABILITY,
    HEADER_MISMATCH,
    METHOD_NOT_FOUND,
];

/// The first legacy version whose requests carry `MCP-Protocol-Version`.
pub const VERSION_HEADER_FROM: &str = "2025-06-18";

/// The era of one server, with the protocol version in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    /// No handshake; `_meta` on every request.
    Modern(&'static str),
    /// `initialize` and `notifications/initialized` first.
    Legacy(&'static str),
}

impl Era {
    /// The protocol version of this era.
    pub fn version(self) -> &'static str {
        match self {
            Era::Modern(version) | Era::Legacy(version) => version,
        }
    }

    /// Whether requests of this era carry the version header.
    pub fn sends_version(self) -> bool {
        match self {
            Era::Modern(_) => true,
            Era::Legacy(version) => version >= VERSION_HEADER_FROM,
        }
    }

    fn meta(self) -> Option<Meta<'static>> {
        match self {
            Era::Modern(version) => Some(Meta::new(version)),
            Era::Legacy(_) => None,
        }
    }
}

/// The legacy version named by a server, `None` when it is not one we speak.
pub fn legacy_version(name: &str) -> Option<&'static str> {
    LEGACY.into_iter().find(|version| *version == name)
}

#[derive(Serialize)]
struct Meta<'a> {
    #[serde(rename = "io.modelcontextprotocol/protocolVersion")]
    version: &'a str,
    #[serde(rename = "io.modelcontextprotocol/clientCapabilities")]
    capabilities: Capabilities,
    #[serde(rename = "io.modelcontextprotocol/clientInfo")]
    info: Info<'a>,
}

impl Meta<'_> {
    fn new(version: &str) -> Meta<'_> {
        Meta {
            version,
            capabilities: Capabilities {},
            info: client_info(),
        }
    }
}

#[derive(Serialize)]
struct Capabilities {}

#[derive(Serialize)]
struct Info<'a> {
    name: &'a str,
    version: &'a str,
}

fn client_info() -> Info<'static> {
    Info {
        name: "reagent",
        version: env!("CARGO_PKG_VERSION"),
    }
}

/// The fields of one request or notification.
pub enum Params<'a> {
    /// `server/discover`, the modern probe.
    Discover,
    /// `initialize`, the legacy handshake.
    Initialize { version: &'a str },
    /// `tools/list`, one page.
    List { cursor: Option<&'a str> },
    /// `tools/call`, with the model's arguments as written.
    Call {
        name: &'a str,
        arguments: &'a RawValue,
    },
    /// `notifications/cancelled`.
    Cancelled { request_id: u64 },
    /// No fields, as `notifications/initialized` sends.
    Empty,
}

impl<'a> Params<'a> {
    /// The tool name a `tools/call` sends as `Mcp-Name`.
    pub fn tool_name(&self) -> Option<&'a str> {
        match self {
            Params::Call { name, .. } => Some(*name),
            _ => None,
        }
    }

    fn write(&self, era: Era, out: &mut Vec<u8>) {
        out.push(b'{');
        let mut first = true;
        match self {
            Params::Discover | Params::Empty => {}
            Params::Initialize { version } => {
                field(out, &mut first, "protocolVersion");
                json(out, version);
                field(out, &mut first, "capabilities");
                out.extend_from_slice(b"{}");
                field(out, &mut first, "clientInfo");
                json(out, &client_info());
            }
            Params::List { cursor } => {
                if let Some(cursor) = cursor {
                    field(out, &mut first, "cursor");
                    json(out, cursor);
                }
            }
            Params::Call { name, arguments } => {
                field(out, &mut first, "name");
                json(out, name);
                field(out, &mut first, "arguments");
                out.extend_from_slice(arguments.get().as_bytes());
            }
            Params::Cancelled { request_id } => {
                field(out, &mut first, "requestId");
                let _ = write!(out, "{request_id}");
                field(out, &mut first, "reason");
                out.extend_from_slice(b"\"cancelled\"");
            }
        }
        if let Some(meta) = era.meta() {
            field(out, &mut first, "_meta");
            json(out, &meta);
        }
        out.push(b'}');
    }
}

/// One request or notification, ending with the newline that ends a line.
pub fn frame(id: Option<u64>, method: &str, params: Params<'_>, era: Era) -> Vec<u8> {
    let mut out = Vec::with_capacity(192);
    out.extend_from_slice(b"{\"jsonrpc\":\"2.0\"");
    if let Some(id) = id {
        let _ = write!(out, ",\"id\":{id}");
    }
    out.extend_from_slice(b",\"method\":");
    json(&mut out, method);
    out.extend_from_slice(b",\"params\":");
    params.write(era, &mut out);
    out.extend_from_slice(b"}\n");
    out
}

/// The result frame that answers the server's request `id`.
pub fn result(id: &RawValue, result: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(result.len() + 48);
    out.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":");
    out.extend_from_slice(id.get().as_bytes());
    out.extend_from_slice(b",\"result\":");
    out.extend_from_slice(result.as_bytes());
    out.extend_from_slice(b"}\n");
    out
}

/// The error frame that answers the server's request `id`.
pub fn error(id: &RawValue, code: i64, message: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 64);
    out.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":");
    out.extend_from_slice(id.get().as_bytes());
    out.extend_from_slice(b",\"error\":{\"code\":");
    let _ = write!(out, "{code}");
    out.extend_from_slice(b",\"message\":");
    json(&mut out, message);
    out.extend_from_slice(b"}}\n");
    out
}

fn field(out: &mut Vec<u8>, first: &mut bool, name: &str) {
    if !*first {
        out.push(b',');
    }
    *first = false;
    out.push(b'"');
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b"\":");
}

fn json<T: Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) {
    let _ = serde_json::to_writer(out, value);
}

/// One line from the server: a request, a notification or a response.
#[derive(Debug, Deserialize)]
pub struct Message<'a> {
    #[serde(borrow, default)]
    pub id: Option<&'a RawValue>,
    #[serde(borrow, default)]
    pub method: Option<&'a str>,
    #[serde(borrow, default)]
    pub result: Option<&'a RawValue>,
    #[serde(borrow, default)]
    pub error: Option<ErrorObject<'a>>,
}

/// The error of a response.
#[derive(Debug, Deserialize)]
pub struct ErrorObject<'a> {
    pub code: i64,
    #[serde(borrow)]
    pub message: std::borrow::Cow<'a, str>,
    #[serde(borrow, default)]
    pub data: Option<&'a RawValue>,
}

/// The versions a `-32022` error lists in `data.supported`.
#[derive(Debug, Deserialize)]
pub struct Supported<'a> {
    #[serde(borrow, default)]
    pub supported: Vec<&'a str>,
}
