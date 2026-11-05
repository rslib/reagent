//! The sidebar host: it stacks panels, folds them by priority or as the user
//! folds them, focuses one and scrolls it, and turns their components into
//! styled lines.

use std::any::Any;
use std::cmp::Reverse;
use std::fmt;

use super::lines::{Fill, Lines, columns};
use super::{Clicked, Component, Panel, Role, Size, Text, Update};

/// The columns a panel's content is indented by, under its title.
pub const INDENT: &str = "  ";

/// The two runs a bar is made of, and the spaces that right-align a note.
#[derive(Debug)]
struct Fills {
    /// The filled part of a bar.
    full: Fill,
    /// The empty part of a bar.
    empty: Fill,
    /// The spaces before a right-aligned note.
    pad: Fill,
}

impl Fills {
    fn new() -> Fills {
        Fills {
            full: Fill::new('█'),
            empty: Fill::new('░'),
            pad: Fill::new(' '),
        }
    }

    fn fit(&mut self, width: u16) {
        self.full.fit(width);
        self.empty.fit(width);
        self.pad.fit(width);
    }
}

/// One panel, the components of its last draw and the rows it got.
/// A panel the host keeps, which it can hand back by its type.
trait Hosted: Panel {
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Panel + 'static> Hosted for T {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// What one line of a draw shows: the panel it belongs to, and the component it
/// draws, or no component on a title line.
#[derive(Debug, Clone, Copy)]
struct Place {
    panel: usize,
    component: Option<usize>,
}

/// What the user asked of a panel with a click or Enter on its title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// Nothing: the panel folds when the rows run out.
    Auto,
    /// Open: the panel folds only when no other panel can.
    Open,
    /// Folded to its title line.
    Folded,
}

struct Entry {
    panel: Box<dyn Hosted>,
    components: Vec<Component>,
    /// Rows it got in the last draw, its title line included.
    rows: u16,
    /// What the user asked of it: nothing yet, open, or folded.
    choice: Choice,
    /// Whether it was folded to its title line in the last draw.
    folded: bool,
    /// Whether the sidebar showed it in the last draw.
    shown: bool,
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("panel", &self.panel.id())
            .field("components", &self.components.len())
            .field("rows", &self.rows)
            .field("choice", &self.choice)
            .field("folded", &self.folded)
            .field("shown", &self.shown)
            .finish()
    }
}

/// The host of the sidebar.
///
/// It owns placement, folding, focus and scroll: a panel says what its lines
/// mean, and the sidebar decides where they go and how much of them shows.
/// Every buffer is reused across draws, so a steady draw allocates nothing.
#[derive(Debug)]
pub struct Sidebar {
    entries: Vec<Entry>,
    lines: Lines,
    fills: Fills,
    /// The fold order of the last short draw, kept so the next one allocates
    /// nothing.
    order: Vec<usize>,
    /// The title line of each panel the last draw showed: its line and its
    /// panel, top down.
    titles: Vec<(usize, usize)>,
    /// What each line of the last draw shows, one entry per line.
    places: Vec<Place>,
    focus: usize,
    active: bool,
    scroll: u16,
}

impl Sidebar {
    /// An empty sidebar with no panel.
    pub fn new() -> Sidebar {
        Sidebar {
            entries: Vec::new(),
            lines: Lines::new(),
            fills: Fills::new(),
            order: Vec::new(),
            titles: Vec::new(),
            places: Vec::new(),
            focus: 0,
            active: false,
            scroll: 0,
        }
    }

    /// Add a panel at the bottom.
    pub fn add(&mut self, panel: impl Panel + 'static) {
        self.entries.push(Entry {
            panel: Box::new(panel),
            components: Vec::new(),
            rows: 0,
            choice: Choice::Auto,
            folded: false,
            shown: false,
        });
    }

    /// How many panels there are.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no panel was added.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The panel at `index`.
    pub fn panel(&self, index: usize) -> Option<&dyn Panel> {
        self.entries
            .get(index)
            .map(|entry| -> &dyn Panel { entry.panel.as_ref() })
    }

    /// The first panel of type `T`, for an update the [`Update`]s do not
    /// carry, such as a list the app read from the disk.
    pub fn get_mut<T: Panel + 'static>(&mut self) -> Option<&mut T> {
        self.entries
            .iter_mut()
            .find_map(|entry| entry.panel.as_any_mut().downcast_mut::<T>())
    }

    /// The index of the focused panel.
    pub fn focus(&self) -> usize {
        self.focus
    }

    /// The focused panel.
    pub fn focused(&self) -> Option<&dyn Panel> {
        self.panel(self.focus)
    }

    /// Whether the sidebar holds the keyboard focus.
    pub fn active(&self) -> bool {
        self.active
    }

    /// Take or give the keyboard focus.
    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// The focused panel's scrolled rows.
    pub fn scroll(&self) -> u16 {
        self.scroll
    }

    /// Whether any panel wants a tick each second it is shown.
    pub fn wants_tick(&self) -> bool {
        self.entries.iter().any(|entry| entry.panel.wants_tick())
    }

    /// Hand one update to every panel, and say whether any of them changed.
    pub fn update(&mut self, update: &Update<'_>) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            changed |= entry.panel.update(update);
        }
        changed
    }

    /// Focus the next panel, wrapping; false when there is at most one panel.
    pub fn focus_next(&mut self) -> bool {
        if self.entries.len() < 2 {
            return false;
        }
        self.focus = (self.focus + 1) % self.entries.len();
        self.scroll = 0;
        true
    }

    /// Focus the previous panel, wrapping; false when there is at most one.
    pub fn focus_previous(&mut self) -> bool {
        if self.entries.len() < 2 {
            return false;
        }
        self.focus = (self.focus + self.entries.len() - 1) % self.entries.len();
        self.scroll = 0;
        true
    }

    /// The lines of the last draw.
    pub fn lines(&self) -> &Lines {
        &self.lines
    }

    /// The title line of each panel the last draw showed, as its line in the
    /// drawn lines and its panel's index, top down.
    pub fn titles(&self) -> &[(usize, usize)] {
        &self.titles
    }

    /// Hand a click on the line `row` of the last draw, at `column` of that
    /// line, to the panel that drew it, and answer what it asks the app for. A
    /// title line answers nothing: a click there folds its panel.
    pub fn click(&mut self, row: usize, column: u16) -> Clicked {
        let Some(&Place { panel, component }) = self.places.get(row) else {
            return Clicked::None;
        };
        let Some(component) = component else {
            return Clicked::None;
        };
        match self.entries.get_mut(panel) {
            Some(entry) => entry.panel.click(component, column),
            None => Clicked::None,
        }
    }

    /// Fold the panel at `index` to its title line when the last draw showed
    /// it open, or open it when the last draw showed it folded, whoever folded
    /// it; the panel stays as the user left it until they toggle it again.
    /// False when there is no such panel.
    pub fn toggle_fold(&mut self, index: usize) -> bool {
        let Some(entry) = self.entries.get_mut(index) else {
            return false;
        };
        entry.choice = if entry.folded {
            Choice::Open
        } else {
            Choice::Folded
        };
        true
    }

    /// Scroll the focused panel by `rows`, keeping it inside what it has to
    /// show; say whether it moved.
    pub fn scroll_by(&mut self, rows: i32) -> bool {
        let entry = match self.entries.get(self.focus) {
            Some(entry) => entry,
            None => return false,
        };
        let space = entry.rows.saturating_sub(1) as i32;
        let content = entry.components.len() as i32;
        let last = (content - space).max(0) as u16;
        let next = (i32::from(self.scroll) + rows).clamp(0, i32::from(last)) as u16;
        let moved = next != self.scroll;
        self.scroll = next;
        moved
    }

    /// Draw every panel into a `width` by `height` block and return the lines.
    pub fn draw(&mut self, width: u16, height: u16) -> &Lines {
        self.lines.clear();
        self.titles.clear();
        self.places.clear();
        self.fills.fit(width);
        for entry in &mut self.entries {
            let shown = entry.panel.min_width() <= width;
            if shown != entry.shown {
                // A panel starts its reads when the sidebar starts showing it.
                entry.panel.update(&Update::Visible(shown));
                entry.shown = shown;
            }
            entry.components.clear();
            // A panel with no space to draw in gets no components this draw:
            // its buffer was cleared above and fills again on a draw with room.
            if shown && width > 0 && height > 0 {
                entry
                    .panel
                    .draw(Size::new(width, height), &mut entry.components);
            }
        }
        self.plan(height);
        self.render(width);
        &self.lines
    }

    /// Give every shown panel its rows: all of them while they fit, and the
    /// lowest-priority ones folded to their title line when they do not. A
    /// panel the user folded keeps its title line and the others take the rows.
    fn plan(&mut self, height: u16) {
        let height = usize::from(height);
        let mut need = 0;
        for entry in &mut self.entries {
            entry.folded = entry.choice == Choice::Folded;
            entry.rows = 0;
            if entry.shown {
                need += if entry.folded {
                    1
                } else {
                    1 + entry.components.len()
                };
            }
        }
        if need > height {
            self.order.clear();
            self.order.extend(
                (0..self.entries.len())
                    .filter(|index| self.entries[*index].shown)
                    .filter(|index| !self.entries[*index].folded)
                    .filter(|index| !self.entries[*index].components.is_empty()),
            );
            // A panel the user opened folds last; then the lowest priority
            // folds first, and of two equals the one further down the sidebar.
            self.order.sort_unstable_by_key(|index| {
                let entry = &self.entries[*index];
                (
                    entry.choice == Choice::Open,
                    entry.panel.priority(),
                    Reverse(*index),
                )
            });
            let mut unfolded = self.order.len();
            for position in 0..self.order.len() {
                if need <= height || unfolded <= 1 {
                    break;
                }
                let index = self.order[position];
                let entry = &mut self.entries[index];
                entry.folded = true;
                unfolded -= 1;
                need -= entry.components.len();
            }
        }
        let mut left = height;
        for entry in &mut self.entries {
            if !entry.shown {
                continue;
            }
            let want = if entry.folded {
                1
            } else {
                1 + entry.components.len()
            };
            let given = want.min(left);
            entry.rows = given as u16;
            left -= given;
        }
        let space = self.entries.get(self.focus).map_or(0, |entry| entry.rows);
        let content = self
            .entries
            .get(self.focus)
            .map_or(0, |entry| entry.components.len());
        let last = content.saturating_sub(usize::from(space.saturating_sub(1)));
        self.scroll = self.scroll.min(last.min(usize::from(u16::MAX)) as u16);
    }

    /// Turn the planned components into lines. Every line stays inside
    /// `width` columns, so even the marker and the indent are cut.
    fn render(&mut self, width: u16) {
        for index in 0..self.entries.len() {
            let rows = self.entries[index].rows;
            if rows == 0 {
                continue;
            }
            let folded = self.entries[index].folded;
            let focused = index == self.focus && self.active;
            // A folded panel says so with its marker, and shows its title.
            let marker = if folded { "▸ " } else { "▾ " };
            self.titles.push((self.lines.len(), index));
            self.places.push(Place {
                panel: index,
                component: None,
            });
            let start = self.lines.open();
            let used = self.lines.push_cut(&Text::Static(marker), Role::Dim, width);
            let role = if focused { Role::Accent } else { Role::Title };
            let title = self.entries[index].panel.title();
            self.lines.push_cut(title, role, width.saturating_sub(used));
            self.lines.close(start);
            if folded {
                continue;
            }
            // The focused panel is the one the wheel scrolls, whether or not
            // the sidebar holds the keyboard focus.
            let skip = if index == self.focus {
                usize::from(self.scroll)
            } else {
                0
            };
            let take = usize::from(rows.saturating_sub(1));
            let shown = skip
                .saturating_add(take)
                .min(self.entries[index].components.len());
            for offset in skip..shown {
                self.places.push(Place {
                    panel: index,
                    component: Some(offset),
                });
                let start = self.lines.open();
                let indented = self
                    .lines
                    .push_cut(&Text::Static(INDENT), Role::Text, width);
                let component = &self.entries[index].components[offset];
                component_spans(
                    &mut self.lines,
                    &self.fills,
                    component,
                    width.saturating_sub(indented),
                );
                self.lines.close(start);
            }
        }
    }
}

impl Default for Sidebar {
    fn default() -> Self {
        Sidebar::new()
    }
}

/// Write one component's runs, each of at most `width` columns in all. The
/// caller opens and closes the line.
fn component_spans(lines: &mut Lines, fills: &Fills, component: &Component, width: u16) {
    if width == 0 {
        // No room: the line draws nothing rather than overflow its width.
        return;
    }
    match component {
        Component::Text { text, role } => {
            lines.push_cut(text, *role, width);
        }
        Component::Item {
            text,
            mark,
            note,
            selected,
            dim,
        } => {
            // A mark of the panel's own, such as a client's dot, takes the
            // place of the host's current-row marker.
            let used = if mark.is_empty() {
                let marker = if *selected { "▸ " } else { "  " };
                lines.push_cut(&Text::Static(marker), Role::Dim, width)
            } else {
                let used = lines.push_cut(mark.text(), mark.role(), width);
                if used < width {
                    used + lines.push(&Text::Static(" "), Role::Text)
                } else {
                    used
                }
            };
            let role = if *selected {
                Role::Accent
            } else if *dim {
                Role::Dim
            } else {
                Role::Text
            };
            let room = width.saturating_sub(used);
            let note_columns = columns(note.as_str());
            if note_columns > 0 && note_columns < room {
                let text_room = room - note_columns - 1;
                let wrote = lines.push_cut(text, role, text_room);
                let gap = room - note_columns - wrote - 1;
                fills.pad.push(lines, gap, Role::Text);
                lines.push(&Text::Static(" "), Role::Text);
                let len = note.text().len() as u32;
                lines.push_slice(note.text(), 0, note.split(), note.first());
                lines.push_slice(note.text(), note.split(), len, note.second());
            } else {
                lines.push_cut(text, role, room);
            }
        }
        Component::Row { key, value } => {
            // A key takes at most a third of the line, so the value has room.
            let used = lines.push_cut(key, Role::Dim, (width / 3).max(1).min(width));
            let room = width.saturating_sub(used);
            if room > 2 {
                lines.push(&Text::Static("  "), Role::Text);
                lines.push_cut(value.text(), value.role(), room - 2);
            }
        }
        Component::Progress { label, value, max } => {
            // A bar with no label starts where the keys start, with no gap.
            let used = lines.push_cut(label, Role::Dim, width);
            let gap = u16::from(used > 0);
            let room = width.saturating_sub(used + gap);
            if room > 0 {
                if gap > 0 {
                    lines.push(&Text::Static(" "), Role::Text);
                }
                let filled = if *max == 0 {
                    0
                } else {
                    ((u128::from(*value.min(max)) * u128::from(room)) / u128::from(*max)) as u16
                };
                fills.full.push(lines, filled, Role::Accent);
                fills
                    .empty
                    .push(lines, room.saturating_sub(filled), Role::Dim);
            }
        }
        Component::Badge { text, role } => {
            let used = lines.push_cut(&Text::Static("["), *role, width);
            let room = width.saturating_sub(used);
            let room = room.saturating_sub(1);
            lines.push_cut(text, *role, room);
            if width > 1 {
                lines.push(&Text::Static("]"), *role);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A panel of one text line per component, for the tests.
    struct Rows {
        id: &'static str,
        title: Text,
        priority: crate::slot::Priority,
        min_width: u16,
        lines: Vec<Arc<str>>,
        /// The capacity of the buffer the host handed the panel last draw,
        /// shared with the test that watches it.
        capacity: Arc<AtomicUsize>,
    }

    impl Rows {
        fn new(id: &'static str, title: &'static str, priority: u8, count: usize) -> Rows {
            Rows::watched(id, title, priority, count, Arc::new(AtomicUsize::new(0)))
        }

        /// A panel whose component buffer capacity the test keeps an eye on.
        fn watched(
            id: &'static str,
            title: &'static str,
            priority: u8,
            count: usize,
            capacity: Arc<AtomicUsize>,
        ) -> Rows {
            let lines = (0..count)
                .map(|index| Arc::from(format!("{id} line {index}")))
                .collect();
            Rows {
                id,
                title: Text::Static(title),
                priority,
                min_width: 0,
                lines,
                capacity,
            }
        }
    }

    impl Panel for Rows {
        fn id(&self) -> &str {
            self.id
        }

        fn title(&self) -> &Text {
            &self.title
        }

        fn priority(&self) -> crate::slot::Priority {
            self.priority
        }

        fn min_width(&self) -> u16 {
            self.min_width
        }

        fn update(&mut self, _update: &Update<'_>) -> bool {
            false
        }

        fn draw(&self, size: Size, out: &mut Vec<Component>) {
            assert!(size.width > 0, "a panel is drawn in a space with columns");
            for text in &self.lines {
                out.push(Component::text(Text::shared(text)));
            }
            self.capacity.store(out.capacity(), Ordering::Relaxed);
        }
    }

    /// The text of every line the sidebar drew, runs joined.
    fn texts(lines: &Lines) -> Vec<String> {
        lines
            .iter()
            .map(|spans| {
                let mut text = String::new();
                for span in spans {
                    text.push_str(span.text());
                }
                text
            })
            .collect()
    }

    #[test]
    fn fold_order_follows_priority() {
        let mut sidebar = Sidebar::new();
        sidebar.add(Rows::new("high", "High", 9, 4));
        sidebar.add(Rows::new("low", "Low", 1, 4));
        // Both expanded want ten rows: fold the lowest priority to its title.
        let drawn = texts(sidebar.draw(40, 6));
        assert_eq!(drawn.len(), 6);
        assert!(drawn.iter().any(|line| line.contains("high line 3")));
        assert!(
            !drawn.iter().any(|line| line.contains("low line 0")),
            "the low priority panel folded: {drawn:?}"
        );
        assert!(drawn.iter().any(|line| line.contains("Low")), "{drawn:?}");
    }

    /// A panel the sidebar folded by itself for lack of rows opens on a toggle,
    /// and stays open: a lower panel folds in its place.
    #[test]
    fn an_auto_folded_panel_opens_on_a_toggle() {
        let mut sidebar = Sidebar::new();
        sidebar.add(Rows::new("one", "One", 2, 3));
        sidebar.add(Rows::new("two", "Two", 1, 3));
        sidebar.add(Rows::new("three", "Three", 0, 3));
        // Twelve rows hold two open panels and a title: the lowest priority,
        // Three, folds by itself.
        let drawn = texts(sidebar.draw(40, 9));
        assert!(
            drawn.iter().any(|line| line.contains("▸ Three")),
            "{drawn:?}"
        );
        assert!(sidebar.toggle_fold(2));
        let drawn = texts(sidebar.draw(40, 9));
        assert!(
            drawn.iter().any(|line| line.contains("▾ Three")),
            "Three opens: {drawn:?}"
        );
        assert!(
            drawn.iter().any(|line| line.contains("three line 0")),
            "{drawn:?}"
        );
        assert!(
            drawn.iter().any(|line| line.contains("▸ Two")),
            "Two folds instead: {drawn:?}"
        );
        assert!(sidebar.toggle_fold(2));
        let drawn = texts(sidebar.draw(40, 9));
        assert!(
            drawn.iter().any(|line| line.contains("▸ Three")),
            "a second toggle folds it: {drawn:?}"
        );
    }

    /// A panel the user folded keeps its title row in every later draw, and the
    /// panels under it take the rows it leaves.
    #[test]
    fn a_panel_the_user_folded_stays_folded_with_room() {
        let mut sidebar = Sidebar::new();
        sidebar.add(Rows::new("one", "One", 0, 3));
        sidebar.add(Rows::new("two", "Two", 0, 3));
        let drawn = texts(sidebar.draw(40, 12));
        assert!(
            drawn.iter().any(|line| line.contains("one line 0")),
            "{drawn:?}"
        );
        assert!(!sidebar.toggle_fold(2), "no third panel to fold");
        assert!(sidebar.toggle_fold(0));

        let drawn = texts(sidebar.draw(40, 12));
        assert!(drawn.iter().any(|line| line.contains("▸ One")), "{drawn:?}");
        assert!(
            !drawn.iter().any(|line| line.contains("one line 0")),
            "it folded to its title row: {drawn:?}"
        );
        assert!(
            drawn.iter().any(|line| line.contains("two line 2")),
            "the panel under it takes the rows: {drawn:?}"
        );
        for height in [20u16, 24] {
            let drawn = texts(sidebar.draw(40, height));
            assert!(
                drawn.iter().any(|line| line.contains("▸ One")),
                "it stays folded at {height} rows: {drawn:?}"
            );
        }

        assert!(sidebar.toggle_fold(0));
        let drawn = texts(sidebar.draw(40, 12));
        assert!(drawn.iter().any(|line| line.contains("▾ One")), "{drawn:?}");
        assert!(
            drawn.iter().any(|line| line.contains("one line 2")),
            "{drawn:?}"
        );
    }

    #[test]
    fn a_tall_panel_scrolls_instead_of_folding() {
        let mut sidebar = Sidebar::new();
        sidebar.add(Rows::new("tall", "Tall", 5, 10));
        let drawn = texts(sidebar.draw(40, 5));
        assert_eq!(drawn.len(), 5, "{drawn:?}");
        assert!(drawn[1].contains("line 0"), "{drawn:?}");
        assert!(sidebar.scroll_by(2));
        let drawn = texts(sidebar.draw(40, 5));
        assert!(drawn[1].contains("line 2"), "{drawn:?}");
        assert_eq!(sidebar.scroll(), 2);
        // The last row that shows is the sixth line, and no further.
        assert!(sidebar.scroll_by(100));
        let drawn = texts(sidebar.draw(40, 5));
        assert!(drawn[4].contains("line 9"), "{drawn:?}");
        assert!(!sidebar.scroll_by(1));
    }

    #[test]
    fn focus_moves_next_and_previous_and_wraps() {
        let mut sidebar = Sidebar::new();
        sidebar.add(Rows::new("one", "One", 1, 1));
        sidebar.add(Rows::new("two", "Two", 1, 1));
        sidebar.add(Rows::new("three", "Three", 1, 1));
        assert_eq!(sidebar.focus(), 0);
        assert_eq!(sidebar.focused().expect("a panel").id(), "one");
        assert!(sidebar.focus_next());
        assert_eq!(sidebar.focused().expect("a panel").id(), "two");
        assert!(sidebar.focus_next());
        assert_eq!(sidebar.focused().expect("a panel").id(), "three");
        assert!(sidebar.focus_next());
        assert_eq!(sidebar.focused().expect("a panel").id(), "one");
        assert!(sidebar.focus_previous());
        assert_eq!(sidebar.focused().expect("a panel").id(), "three");
        sidebar.draw(40, 10);
        assert_eq!(sidebar.focused().expect("a panel").id(), "three");
    }

    #[test]
    fn one_panel_has_nothing_to_focus() {
        let mut sidebar = Sidebar::new();
        assert!(!sidebar.focus_next());
        assert!(!sidebar.focus_previous());
        sidebar.add(Rows::new("one", "One", 1, 1));
        assert!(!sidebar.focus_next());
        assert!(!sidebar.focus_previous());
    }

    #[test]
    fn a_steady_draw_keeps_every_buffer() {
        let capacity = Arc::new(AtomicUsize::new(0));
        let mut sidebar = Sidebar::new();
        sidebar.add(Rows::watched("one", "One", 9, 3, Arc::clone(&capacity)));
        sidebar.add(Rows::new("two", "Two", 1, 3));
        let first = sidebar.draw(40, 12);
        let spans = first.span_capacity();
        let rows = first.line_capacity();
        let drawn = first.len();
        assert!(
            capacity.load(Ordering::Relaxed) >= 3,
            "the panel drew its components"
        );
        let components = capacity.load(Ordering::Relaxed);
        for width in [40u16, 41, 40] {
            let again = sidebar.draw(width, 12);
            assert_eq!(again.span_capacity(), spans, "the run buffer grew");
            assert_eq!(again.line_capacity(), rows, "the line buffer grew");
            assert_eq!(again.len(), drawn, "the same lines drew");
            assert_eq!(
                capacity.load(Ordering::Relaxed),
                components,
                "the component buffer grew"
            );
        }
    }

    #[test]
    fn a_panel_narrower_than_its_minimum_is_skipped() {
        let mut wide = Rows::new("wide", "Wide", 5, 2);
        wide.min_width = 60;
        let mut sidebar = Sidebar::new();
        sidebar.add(wide);
        sidebar.add(Rows::new("fits", "Fits", 1, 2));
        let drawn = texts(sidebar.draw(40, 12));
        assert!(
            !drawn.iter().any(|line| line.contains("wide line")),
            "{drawn:?}"
        );
        assert!(
            drawn.iter().any(|line| line.contains("fits line 1")),
            "{drawn:?}"
        );
    }

    #[test]
    fn an_empty_sidebar_draws_nothing() {
        let mut sidebar = Sidebar::new();
        assert!(sidebar.is_empty());
        assert!(sidebar.draw(40, 12).is_empty());
        assert!(!sidebar.scroll_by(1));
        assert!(!sidebar.wants_tick());
    }

    /// Every line a draw gives stays inside the width it was given, however
    /// narrow the sidebar is: even the marker and the indent are cut
    /// (CHG-j9kb5g review).
    #[test]
    fn a_narrow_sidebar_draws_no_line_wider_than_it() {
        let mut sidebar = Sidebar::new();
        sidebar.add(Rows::new("one", "One", 5, 3));
        for width in [0u16, 1, 2, 3] {
            let drawn = texts(sidebar.draw(width, 6));
            for line in &drawn {
                assert!(
                    columns(line) <= width,
                    "{line:?} is wider than the {width} columns drawn: {drawn:?}"
                );
            }
        }
    }

    #[test]
    fn every_component_is_one_line() {
        let mut lines = Lines::new();
        let fills = Fills::new();
        let components = [
            Component::text("body"),
            Component::dim("dim"),
            Component::strong("strong"),
            Component::item("row", true),
            Component::item_with("row", "3m", false),
            Component::row("tokens", "50k / 200k"),
            Component::progress("context", 50, 200),
            Component::badge("approval", Role::Warn),
        ];
        for component in &components {
            let start = lines.open();
            component_spans(&mut lines, &fills, component, 24);
            lines.close(start);
        }
        assert_eq!(lines.len(), components.len());
        for spans in lines.iter() {
            let width: u16 = spans.iter().map(|span| span.width()).sum();
            assert!(width <= 24, "{:?} is {width} columns wide", texts_of(spans));
        }
        let drawn: Vec<String> = lines
            .iter()
            .map(|spans| spans.iter().map(|span| span.text()).collect())
            .collect();
        assert_eq!(drawn[0], "body");
        assert_eq!(drawn[5], "tokens  50k / 200k");
    }

    #[test]
    fn a_bar_with_no_label_starts_where_the_keys_start_and_fills_the_line() {
        let mut lines = Lines::new();
        let mut fills = Fills::new();
        fills.fit(24);
        let start = lines.open();
        component_spans(&mut lines, &fills, &Component::progress("", 50, 200), 24);
        lines.close(start);
        let spans = lines.iter().next().expect("one line");
        let drawn = texts_of(spans);
        assert!(!drawn.starts_with(' '), "no gap before the bar: {drawn:?}");
        let width: u16 = spans.iter().map(|span| span.width()).sum();
        assert_eq!(width, 24, "the bar ends where the values end: {drawn:?}");
    }

    fn texts_of(spans: &[crate::slot::Span]) -> String {
        spans.iter().map(|span| span.text()).collect()
    }
}
