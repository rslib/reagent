//! The protocol of a session worker and its clients.
//!
//! Message types, the line codec, the discovery folder and the join token. No
//! agent types, so a client links it without the cli.

pub mod discovery;
pub mod hub;
pub mod lines;
pub mod message;
pub mod token;
