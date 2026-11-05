//! Regions of the screen by width: the header row, the main area and the
//! sidebar.
//!
//! A docked sidebar, at 120 columns or more with the sidebar on, takes every row
//! at the right of the main area and holds the title on top. Otherwise the top
//! row of a full-screen frame is the header row of counts the sidebar folds
//! into, and the main area is under it; below 120 the sidebar covers the right of
//! the main area as an overlay, and with the sidebar off it is not drawn at all.
//! The sidebar takes a quarter of the width, 28 to 40 columns. Every function
//! here is pure and allocates nothing: the app calls them on every resize.

/// A block of columns and rows of the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Rect {
    /// The column of the left edge.
    pub x: u16,
    /// The row of the top edge.
    pub y: u16,
    /// Columns.
    pub width: u16,
    /// Rows.
    pub height: u16,
}

impl Rect {
    /// A block at `(x, y)` of `width` by `height`.
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// True when the block holds no cell.
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The column after the right edge.
    pub const fn right(&self) -> u16 {
        self.x + self.width
    }

    /// The row after the bottom edge.
    pub const fn bottom(&self) -> u16 {
        self.y + self.height
    }
}

/// Where the sidebar sits, when it is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sidebar {
    /// The sidebar is off, and the main area has the whole width.
    #[default]
    Off,
    /// The sidebar has its own columns, taken from the right of the main area.
    Docked(Rect),
    /// The sidebar covers the right of the main area.
    Overlay(Rect),
}

impl Sidebar {
    /// The sidebar's block, when its panels are shown.
    pub const fn rect(&self) -> Option<Rect> {
        match self {
            Sidebar::Off => None,
            Sidebar::Docked(rect) | Sidebar::Overlay(rect) => Some(*rect),
        }
    }

    /// True while the sidebar has its own columns beside the main area.
    pub const fn docked(&self) -> bool {
        matches!(self, Sidebar::Docked(_))
    }
}

/// The regions of one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Layout {
    /// The area the body draws in: the transcript, the live rows, the text box
    /// and the status line, under the header row.
    pub main: Rect,
    /// The top row: the counts row the sidebar folds into. Empty while the
    /// sidebar is docked, and in inline mode, which has no header.
    pub header: Rect,
    /// Where the sidebar sits.
    pub sidebar: Sidebar,
}

/// The columns of the header row: one row, full width.
const HEADER_ROWS: u16 = 1;

/// The narrowest terminal that docks the sidebar.
pub const DOCK_COLUMNS: u16 = 120;

/// The narrowest terminal that shows the sidebar at all.
pub const OVERLAY_COLUMNS: u16 = 60;

/// The narrowest sidebar, in columns.
pub const SIDEBAR_MIN: u16 = 28;

/// The widest sidebar, in columns.
pub const SIDEBAR_MAX: u16 = 40;

/// The sidebar's columns at `columns` of terminal, or `None` when it is off.
pub fn sidebar_width(columns: u16) -> Option<u16> {
    if columns < OVERLAY_COLUMNS {
        return None;
    }
    Some((columns / 4).clamp(SIDEBAR_MIN, SIDEBAR_MAX))
}

/// The regions of a `columns` by `rows` full-screen terminal, with the sidebar's
/// panels on or folded into the header row.
///
/// The block the panels draw in is the sidebar's columns: terminal mode marks
/// their edge with a `│` column of its own inside the block, so the main area
/// keeps the width the terminal leaves it.
pub fn layout(columns: u16, rows: u16, sidebar: bool) -> Layout {
    let header = Rect::new(0, 0, columns, HEADER_ROWS.min(rows));
    let body = Rect::new(0, header.height, columns, rows - header.height);
    let width = if sidebar {
        sidebar_width(columns)
    } else {
        None
    };
    let Some(width) = width else {
        return Layout {
            main: body,
            header,
            sidebar: Sidebar::Off,
        };
    };
    let rect = Rect::new(columns.saturating_sub(width), body.y, width, body.height);
    if columns >= DOCK_COLUMNS {
        // A docked sidebar holds the title, so the header row goes and the
        // main area and the sidebar take every row.
        Layout {
            main: Rect::new(0, 0, columns.saturating_sub(width), rows),
            header: Rect::default(),
            sidebar: Sidebar::Docked(Rect::new(rect.x, 0, width, rows)),
        }
    } else {
        Layout {
            main: body,
            header,
            sidebar: Sidebar::Overlay(rect),
        }
    }
}

/// The regions of a `columns` by `rows` inline frame: no header row, no sidebar,
/// and the main area is the whole of it.
pub fn inline(columns: u16, rows: u16) -> Layout {
    Layout {
        main: Rect::new(0, 0, columns, rows),
        header: Rect::default(),
        sidebar: Sidebar::Off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sidebar's block at `columns` with the sidebar on.
    fn at(columns: u16) -> Sidebar {
        layout(columns, 40, true).sidebar
    }

    #[test]
    fn width_59_has_no_sidebar() {
        let regions = layout(59, 40, true);
        assert_eq!(regions.sidebar, Sidebar::Off);
        assert_eq!(regions.header, Rect::new(0, 0, 59, 1));
        assert_eq!(regions.main, Rect::new(0, 1, 59, 39));
        assert_eq!(sidebar_width(59), None);
    }

    #[test]
    fn width_60_overlays_a_28_column_sidebar() {
        let regions = layout(60, 40, true);
        assert_eq!(regions.sidebar, Sidebar::Overlay(Rect::new(32, 1, 28, 39)));
        assert_eq!(regions.main, Rect::new(0, 1, 60, 39));
    }

    #[test]
    fn width_119_overlays_a_29_column_sidebar() {
        let regions = layout(119, 40, true);
        assert_eq!(regions.sidebar, Sidebar::Overlay(Rect::new(90, 1, 29, 39)));
        assert_eq!(regions.main, Rect::new(0, 1, 119, 39));
    }

    #[test]
    fn width_120_docks_a_30_column_sidebar() {
        let regions = layout(120, 40, true);
        assert_eq!(regions.sidebar, Sidebar::Docked(Rect::new(90, 0, 30, 40)));
        assert_eq!(regions.main, Rect::new(0, 0, 90, 40));
        assert!(regions.header.is_empty(), "the sidebar holds the title");
    }

    #[test]
    fn width_160_docks_a_40_column_sidebar() {
        let regions = layout(160, 24, true);
        assert_eq!(regions.sidebar, Sidebar::Docked(Rect::new(120, 0, 40, 24)));
        assert_eq!(regions.main, Rect::new(0, 0, 120, 24));
        assert!(regions.header.is_empty(), "the sidebar holds the title");
    }

    #[test]
    fn width_200_docks_a_sidebar_capped_at_40_columns() {
        let regions = layout(200, 24, true);
        assert_eq!(regions.sidebar, Sidebar::Docked(Rect::new(160, 0, 40, 24)));
        assert_eq!(regions.main, Rect::new(0, 0, 160, 24));
    }

    #[test]
    fn a_wide_terminal_with_the_sidebar_off_keeps_the_header_row() {
        let regions = layout(160, 24, false);
        assert_eq!(regions.sidebar, Sidebar::Off);
        assert_eq!(regions.header, Rect::new(0, 0, 160, 1));
        assert_eq!(regions.main, Rect::new(0, 1, 160, 23));
    }

    #[test]
    fn an_overlay_sits_under_the_header_row_and_a_docked_sidebar_takes_every_row() {
        assert_eq!(at(60).rect().expect("a sidebar").height, 39);
        assert_eq!(at(60).rect().expect("a sidebar").y, 1);
        assert_eq!(at(120).rect().expect("a sidebar").height, 40);
        assert_eq!(at(120).rect().expect("a sidebar").y, 0);
    }

    #[test]
    fn a_one_row_terminal_still_has_regions() {
        let regions = layout(80, 1, true);
        assert_eq!(regions.header, Rect::new(0, 0, 80, 1));
        assert_eq!(regions.main, Rect::new(0, 1, 80, 0));
        assert_eq!(regions.sidebar.rect(), Some(Rect::new(52, 1, 28, 0)));
    }

    #[test]
    fn inline_mode_has_no_header_row_and_no_sidebar() {
        let regions = inline(80, 24);
        assert_eq!(regions.main, Rect::new(0, 0, 80, 24));
        assert_eq!(regions.header, Rect::default());
        assert_eq!(regions.sidebar, Sidebar::Off);
        assert!(!regions.sidebar.docked());
    }
}
