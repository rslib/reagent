//! The end-to-end tests of the binary, one module per area, built as one
//! program so a test run links and starts one program, not one per file.

mod image;
mod json;
mod max_tokens;
mod mcp;
mod models;
mod permissions;
mod presets;
mod print;
mod provider;
mod sandbox;
mod serve;
mod session;
mod session_crash;
mod session_gc;
mod signal;
mod spill;
mod steer;
mod tui;
mod worker;
mod worker_live;
