//! The trait a panel implements.

use std::sync::Arc;

use super::{Component, Text};

/// What a click on one of a panel's components asked the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clicked {
    /// Nothing.
    None,
    /// The panel changed: draw the frame again.
    Redraw,
    /// Open the session with this id, as the sessions sheet does.
    Open(Arc<str>),
}

/// A panel's fold order: a higher number keeps its content for longer when the
/// sidebar is short, and the highest one is never folded.
pub type Priority = u8;

/// How wide and tall the space a panel draws in is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    /// Columns.
    pub width: u16,
    /// Rows, the title line included.
    pub height: u16,
}

impl Size {
    /// A space of `width` by `height`.
    pub const fn new(width: u16, height: u16) -> Size {
        Size { width, height }
    }
}

/// One input the host hands every panel.
///
/// Panels ignore the updates they do not use and return whether an update
/// changed them, so the host redraws only when a panel drew something new.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update<'a> {
    /// One tool call the model made: its name and its raw JSON arguments.
    ToolCall {
        /// The tool's name.
        name: &'a str,
        /// The arguments the model sent.
        arguments: &'a str,
    },
    /// The token counts of the last request.
    Usage {
        /// Tokens sent.
        input: u64,
        /// Tokens received.
        output: u64,
    },
    /// The model the worker runs, and the context window it has.
    Model {
        /// The model's name.
        name: &'a str,
        /// Its context window, in tokens.
        window: u64,
    },
    /// The worker started or finished a turn.
    Busy(bool),
    /// The sidebar started or stopped showing the panel.
    Visible(bool),
    /// A second passed, for a panel that asked for a tick.
    Tick,
}

/// A panel of the sidebar.
///
/// A panel never sees a screen or a crossterm type: it takes updates from the
/// host, and on draw it fills a reused `Vec` of [`Component`]s for the size it
/// is given. The host owns placement, folding by priority, focus and scroll.
pub trait Panel: Send {
    /// The panel's id, the same for as long as it lives.
    fn id(&self) -> &str;

    /// The title line, which a folded panel shows alone.
    fn title(&self) -> &Text;

    /// The fold order: a panel with a higher priority keeps its content for
    /// longer when the sidebar is short.
    fn priority(&self) -> Priority;

    /// The narrowest sidebar this panel draws in. The host skips the panel
    /// when the sidebar is narrower.
    fn min_width(&self) -> u16;

    /// Take one update, and say whether it changed what the panel draws.
    fn update(&mut self, update: &Update<'_>) -> bool;

    /// Fill `out` with the panel's components for `size`, one component per
    /// line. The host clears `out` first and keeps its capacity.
    fn draw(&self, size: Size, out: &mut Vec<Component>);

    /// Whether the panel wants a [`Update::Tick`] each second it is shown. A
    /// panel that says yes is the only reason the host arms a timer.
    fn wants_tick(&self) -> bool {
        false
    }

    /// Take a click on the component `component` of the panel's last draw, at
    /// `column` of its line, and answer what the app should do with it. A
    /// panel that answers nothing leaves the click to the host.
    fn click(&mut self, _component: usize, _column: u16) -> Clicked {
        Clicked::None
    }
}
