//! MCP client that exposes server tools to the agent.

pub mod auth;

mod client;
mod error;
mod http;
mod process_group;
mod protocol;
mod servers;

pub use auth::{AuthStore, ServerAuth};
pub use client::{CallOutput, Client, StdioSpec, Tool};
pub use error::Error;
pub use http::{HttpSpec, expand_env};
pub use protocol::Era;
pub use servers::{EXPOSED_PREFIX, McpTool, ServerState, ServerStatus, Servers, exposed_name};
