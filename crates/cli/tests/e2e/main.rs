//! The end-to-end tests of the binary, one module per area, built as one
//! program so a test run links and starts one program, not one per file.

mod gateway;
mod image;
mod install;
mod json;
mod max_tokens;
mod mcp;
mod models;
mod park;
mod permissions;
mod plugin;
mod presets;
mod print;
mod provider;
mod rpc;
mod sandbox;
mod serve;
mod session;
mod session_crash;
mod sessions;
mod session_gc;
mod signal;
mod spill;
mod steer;
mod tui;
mod worker;
mod worker_live;
