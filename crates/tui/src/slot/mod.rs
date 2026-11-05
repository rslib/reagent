//! The slot API: panels and other parts that fill a region the host owns.
//!
//! A panel never sees a screen or a crossterm type. It takes updates from the
//! host, says whether they changed it, and on draw fills a reused `Vec` of
//! semantic [`Component`]s for the size it is given. The host ([`Sidebar`])
//! owns placement, folding by priority, focus and scroll, and turns the
//! components into styled [`Lines`] for the screen.
//!
//! Every buffer the host and a panel draw into keeps its capacity between
//! frames, so a steady draw allocates nothing: text crosses the API as a
//! [`Text`] literal or a shared `Arc<str>`, and a run of a line is a slice of
//! one.

mod component;
mod lines;
mod panel;
mod role;
mod sidebar;
mod text;

pub use component::Component;
pub use lines::{Line, Lines, Span};
pub use panel::{Clicked, Panel, Priority, Size, Update};
pub use role::Role;
pub use sidebar::Sidebar;
pub use text::Text;
