//! The one error type, worded so a model can act on it.

use std::fmt;

/// Why a server, a request or a call failed.
#[derive(Debug)]
pub enum Error {
    /// The server process could not start.
    Spawn { command: String, reason: String },
    /// The server process ended, or the pipe to it broke.
    Exited { reason: String },
    /// The server answered with a JSON-RPC error.
    Rpc { code: i64, message: String },
    /// The server speaks no protocol version this client does.
    Version {
        server: String,
        client: &'static str,
    },
    /// The server did not answer in time.
    Timeout { method: String, ms: u64 },
    /// The caller cancelled the request.
    Cancelled,
    /// The server sent something this client cannot read.
    Protocol(String),
    /// The arguments were not a JSON value.
    Arguments(String),
    /// A configured header is not a valid name or value.
    Header(String),
    /// A `${NAME}` in a configured header names a variable that is not set.
    Env { name: String },
    /// The HTTP request failed before an answer arrived.
    Http(String),
    /// The server answered with a status this client does not handle.
    Status { code: u16, body: String },
    /// The server wants the client to authorize first.
    Unauthorized { resource_metadata: Option<String> },
    /// An OAuth login failed, or its saved token could not be read or
    /// written.
    Auth(String),
    /// The server needs a login the user runs in a terminal.
    NeedsLogin { server: String, scopes: Vec<String> },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Spawn { command, reason } => write!(
                formatter,
                "cannot start the MCP server `{command}`: {reason}"
            ),
            Error::Exited { reason } => {
                write!(formatter, "the MCP server is gone: {reason}")
            }
            Error::Rpc { code, message } => {
                write!(formatter, "the MCP server returned error {code}: {message}")
            }
            Error::Version { server, client } => write!(
                formatter,
                "no protocol version in common: the MCP server speaks {server}, this client speaks {client}"
            ),
            Error::Timeout { method, ms } => write!(
                formatter,
                "the MCP server did not answer {method} within {ms} ms"
            ),
            Error::Cancelled => formatter.write_str("the MCP request was cancelled"),
            Error::Protocol(reason) => {
                write!(
                    formatter,
                    "unexpected message from the MCP server: {reason}"
                )
            }
            Error::Arguments(reason) => write!(formatter, "the arguments are not JSON: {reason}"),
            Error::Header(reason) => {
                write!(formatter, "an MCP server header is not usable: {reason}")
            }
            Error::Env { name } => write!(formatter, "the variable {name} is not set"),
            Error::Http(reason) => {
                write!(formatter, "the request to the MCP server failed: {reason}")
            }
            Error::Status { code, body } if body.is_empty() => {
                write!(formatter, "the MCP server answered HTTP {code}")
            }
            Error::Status { code, body } => {
                write!(formatter, "the MCP server answered HTTP {code}: {body}")
            }
            Error::Unauthorized { resource_metadata } => {
                formatter.write_str("the MCP server requires authorization")?;
                match resource_metadata {
                    Some(url) => write!(formatter, "; its metadata is at {url}"),
                    None => Ok(()),
                }
            }
            Error::Auth(reason) => write!(formatter, "the MCP server login failed: {reason}"),
            Error::NeedsLogin { server, scopes } if scopes.is_empty() => write!(
                formatter,
                "the MCP server {server} needs a login; run: reagent mcp login {server}"
            ),
            Error::NeedsLogin { server, scopes } => write!(
                formatter,
                "the MCP server {server} needs a login; run: reagent mcp login {server} (it needs the scope {})",
                scopes.join(" ")
            ),
        }
    }
}

impl std::error::Error for Error {}
