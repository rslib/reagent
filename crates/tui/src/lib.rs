//! Inline terminal UI with streaming markdown.

pub mod app;
pub mod client;
pub mod editor;
pub mod highlight;
pub mod history;
pub mod host;
pub mod keys;
pub mod layout;
pub mod markdown;
pub mod mouse;
pub mod panels;
pub mod pick;
pub mod screen;
pub mod slot;
pub mod theme;

/// The terminal's input: the tty read with async I/O and the parser that turns
/// its bytes into events.
///
/// It lives under `app/` with the rest of the app, and is named here so its
/// tests and the allocation case of `tests/allocs/frame.rs` reach the parser
/// without the app's surface growing.
#[doc(hidden)]
#[path = "app/input/mod.rs"]
pub mod input;
