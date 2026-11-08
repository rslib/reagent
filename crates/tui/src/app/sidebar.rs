//! The sidebar: its block, its panels, its header row and the folder's
//! sessions.
//!
//! The app owns the host ([`Sidebar`]) and the block the layout gives it, and
//! this module is the only place that talks to it: it lays the sidebar out by
//! the terminal's width and the user's toggle, hands the panels the updates the
//! worker sends, feeds the sessions panel the folder's sessions and the live
//! workers, paints the header row and the lines the host returns.
//!
//! The header row is built from the panels themselves, never from a second copy
//! of their data: while the panels are docked it is the plain title row, and
//! otherwise it is the counts row they fold into. Every color goes through the
//! theme ([`App::style`](crate::app::paint)), so terminal mode draws no fill and
//! keeps the `│` line between the main area and the panels.
//!
//! Nothing here allocates per steady frame: the host's draw reuses its buffers,
//! a panel rebuilds a line only when an update changed it, and the sidebar is
//! painted on the frames the app already draws. No timer is armed for it unless
//! a shown panel says it wants a tick.

use std::path::Path;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reagent_store::list::{Entry, SessionList, State};
use unicode_width::UnicodeWidthStr;

use crate::keyfile::Foot;
use crate::keys::Leader;
use crate::layout::{self, DOCK_COLUMNS, Rect, SIDEBAR_MAX};
use crate::panels::{self, ChangesPanel, ClientsPanel, ContextPanel, SessionRow, SessionsPanel};
use crate::screen::{Color, Screen};
use crate::slot::{Role, Sidebar, Text, Update};
use crate::theme::{self, Emphasis};

use super::App;

/// Forty spaces: the widest sidebar, and the run the header row is filled with.
const BLANK: &str = "                                        ";

/// The name the header row starts with.
const NAME: &str = "reagent";

/// What separates the parts of the header row.
const DOT: &str = "·";

/// The hint of the counts row that opens the panels, which drops first when the
/// row is short.
const HINT: &str = "ctrl+x b panels";

/// The mark of a session that waits, in the warn role.
const WAITS: &str = "⏸";

/// The `│` between the main area and the sidebar in terminal mode.
const LINE: &str = "│";

/// The columns of the counts row's token bar.
const GAUGE: usize = 8;

/// The bar's glyphs: one column each, and three bytes each.
const FULL: &str = "▰▰▰▰▰▰▰▰";
const EMPTY: &str = "▱▱▱▱▱▱▱▱";

/// The bytes of one glyph of the bar.
const GLYPH: usize = 3;

/// The columns of `·` between two parts of the right side of the counts row.
const SEPARATOR: u16 = 5;

/// How many role styles the panels' draw has in hand at once.
const ROLE_SLOTS: usize = 8;

/// Every text the header row reads from the panels, none of it copied out of
/// them: a `Text` clone shares its `Arc<str>`.
struct Counts {
    /// The share of the window the last request used, as `25%`.
    share: Text,
    /// The same share, in percent.
    percent: u64,
    /// The cost so far, as `$0.42`.
    cost: Text,
    /// The files that changed, as `3`.
    files: Text,
    /// The lines added, as `+91`.
    added: Text,
    /// The lines removed, as `-15`.
    removed: Text,
    /// The sessions that wait, as `1 waits`, and empty when none does.
    waits: Text,
    /// The clients, as `2 clients`.
    clients: Text,
}

impl App {
    /// The host of the sidebar, with the built-in panels in it.
    pub(super) fn sidebar() -> Sidebar {
        let mut sidebar = Sidebar::new();
        panels::add_builtin(&mut sidebar);
        sidebar
    }

    /// Lays the header row, the main area and the sidebar out for the terminal's
    /// size and the sidebar the user asked for, and redraws when they moved.
    ///
    /// The panels are on by themselves at [`DOCK_COLUMNS`] columns or more, a
    /// header row of counts otherwise, an overlay of the full panels under
    /// [`DOCK_COLUMNS`] once the user toggles them, and never under 60 columns.
    /// Inline mode has no header row and no panels: it draws on the normal
    /// screen.
    pub(super) fn reflow(&mut self) {
        let (columns, rows) = (self.screen.columns(), self.screen.rows());
        let wanted = self.screen.is_full() && self.sidebar_on.unwrap_or(columns >= DOCK_COLUMNS);
        let regions = if self.screen.is_full() {
            layout::layout(columns, rows, wanted)
        } else {
            layout::inline(columns, rows)
        };
        if regions.sidebar != self.aside || regions.main != self.main {
            if regions.sidebar.rect().is_none() {
                self.sidebar.set_active(false);
            }
            self.aside = regions.sidebar;
            self.main = regions.main;
            self.touch();
        }
        self.set_session();
        // The pane the preview wrapped for may have changed with the layout.
        self.sheet_preview();
    }

    /// The header row above the main area: the top rows of a full-screen frame,
    /// and none at all in inline mode, which has no header.
    fn header(&self) -> Option<Rect> {
        let rows = self.main.y;
        (self.screen.is_full() && rows > 0).then(|| Rect::new(0, 0, self.screen.columns(), rows))
    }

    /// Ctrl+X b: show the panels, or fold them into the header row.
    fn toggle_sidebar(&mut self) {
        self.sidebar_on = Some(self.aside.rect().is_none());
        self.reflow();
        self.touch();
    }

    /// A leader key the sidebar owns: Ctrl+X b toggles it, Ctrl+X Tab moves the
    /// keyboard focus into it and back out. The model and the sessions sheets
    /// are no key of the sidebar's.
    pub(super) fn sidebar_leader(&mut self, leader: Leader) {
        match leader {
            Leader::ToggleSidebar => self.toggle_sidebar(),
            Leader::FocusSidebar => {
                let active = !self.sidebar.active();
                if active && self.aside.rect().is_none() {
                    self.sidebar_on = Some(true);
                    self.reflow();
                }
                self.sidebar
                    .set_active(active && self.aside.rect().is_some());
                self.touch();
            }
            Leader::Model | Leader::Sessions | Leader::Level | Leader::Commands => {}
        }
    }

    /// Takes one key a focused sidebar owns, and answers whether it did: Tab and
    /// Shift+Tab move between panels, Up and Down scroll the focused one, Enter
    /// folds it or opens it, and Esc gives the keyboard back to the editor.
    pub(super) fn sidebar_key(&mut self, key: KeyEvent) -> bool {
        if !self.sidebar.active() {
            return false;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Tab if !shift => {
                self.sidebar.focus_next();
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.sidebar.focus_previous();
            }
            KeyCode::Up => {
                self.sidebar.scroll_by(-1);
            }
            KeyCode::Down => {
                self.sidebar.scroll_by(1);
            }
            KeyCode::Enter => {
                self.sidebar.toggle_fold(self.sidebar.focus());
            }
            KeyCode::Esc => self.sidebar.set_active(false),
            _ => return false,
        }
        self.touch();
        true
    }

    /// Hands one update to the panels, and redraws only when one of them changed
    /// what it shows.
    pub(super) fn panels_update(&mut self, update: &Update<'_>) {
        if self.sidebar.update(update) {
            self.touch();
        }
    }

    /// Hands a client that joined the session to the clients panel, and redraws
    /// when the panel shows something new.
    ///
    /// The `client_joined` notice names the client's id and the name it chose;
    /// a later `client_left` names only the id, so the panel keys on it.
    pub(super) fn client_joined(&mut self, id: u64, name: &str) {
        let Some(panel) = self.sidebar.get_mut::<ClientsPanel>() else {
            return;
        };
        if panel.joined(id, name) {
            self.touch();
        }
    }

    /// Hands a client that left the session to the clients panel, by the id the
    /// notice names.
    pub(super) fn client_left(&mut self, id: u64) {
        let Some(panel) = self.sidebar.get_mut::<ClientsPanel>() else {
            return;
        };
        if panel.left(id) {
            self.touch();
        }
    }

    /// The price of the run's model, in USD per million tokens, for the context
    /// panel's cost line.
    pub(super) fn set_prices(&mut self, prices: (f64, f64)) {
        let Some(panel) = self.sidebar.get_mut::<ContextPanel>() else {
            return;
        };
        if panel.set_prices(prices.0, prices.1) {
            self.touch();
        }
    }

    /// The level and the mode of the run, as the context panel's line under the
    /// model reads them.
    ///
    /// The worker's `settings` frame carries both: the app passes them on from
    /// there, and redraws only when the line reads new.
    pub(super) fn set_settings(&mut self, level: Option<&str>, mode: Option<&str>) {
        let Some(panel) = self.sidebar.get_mut::<ContextPanel>() else {
            return;
        };
        if panel.set_settings(level, mode) {
            self.touch();
        }
    }

    /// Hands the context panel the id of this session, which it shows short.
    fn set_session(&mut self) {
        let Some(panel) = self.sidebar.get_mut::<ContextPanel>() else {
            return;
        };
        if panel.set_session(&self.session) {
            self.touch();
        }
    }

    /// Starts a read of the folder's sessions, unless one runs or the app does
    /// not know the state folder.
    ///
    /// The store's list blocks on one small file, so it is read in a
    /// `spawn_blocking` job; the discovery folder of the live workers is read
    /// async when the job answers, and the two meet in [`App::take_sessions`].
    pub(super) fn read_sessions(&mut self) {
        if self.reading.is_some() || self.state.is_none() {
            return;
        }
        let Some(folder) = self.folder() else {
            return;
        };
        self.reading = Some(tokio::task::spawn_blocking(move || list_folder(&folder)));
    }

    /// Takes the sessions a finished read found, with the live workers, and
    /// hands them to the panel.
    pub(super) async fn take_sessions(&mut self) {
        let Some(reading) = &self.reading else {
            return;
        };
        if !reading.is_finished() {
            return;
        }
        let reading = self.reading.take().expect("the read that finished");
        let found = reading.await.unwrap_or_default();
        let live = match &self.state {
            Some(state) => {
                let folder = reagent_protocol::discovery::folder(state);
                reagent_protocol::discovery::list(&folder).await
            }
            None => Vec::new(),
        };
        let mut rows = Vec::with_capacity(found.len());
        for entry in found {
            let current = entry.session == self.session;
            rows.push(SessionRow {
                id: Arc::from(entry.session.as_str()),
                title: Arc::from(entry.title.as_str()),
                updated: entry.updated_ms / 1000,
                live: live.iter().any(|worker| worker.session == entry.session),
                waiting: false,
                current,
                headless: entry.headless,
            });
        }
        self.sessions = rows;
        self.set_sessions();
    }

    /// Hands the rows the panel shows, with the current session marked as one
    /// that waits while an approval is open. A session the discovery folder
    /// names carries no approval state, so `waiting` stays false for it.
    pub(super) fn set_sessions(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let waiting = self.approval.is_some();
        for row in &mut self.sessions {
            row.waiting = row.current && waiting;
        }
        let now = reagent_store::paths::now_ms() / 1000;
        let rows = self.sessions.clone();
        let Some(panel) = self.sidebar.get_mut::<SessionsPanel>() else {
            return;
        };
        if panel.set_rows(rows, now) {
            self.touch();
        }
    }

    /// The folder of this session's journal, which holds the folder's list.
    fn folder(&self) -> Option<std::path::PathBuf> {
        self.journal
            .as_ref()
            .and_then(|dir| dir.parent())
            .map(std::path::Path::to_path_buf)
    }

    /// Draws the header row and the sidebar into its block, over the
    /// transcript, when it shows, and remembers the screen row each panel's
    /// title line landed on.
    ///
    /// The block is blanked first: a docked sidebar gives the transcript, the
    /// live rows and the editor the columns it leaves, and an overlay takes its
    /// columns back from the cells a narrower line left blank, so the block
    /// holds nothing but what this draws.
    pub(super) fn paint_sidebar(&mut self, dim: u32) {
        self.panel_rows.clear();
        if !self.screen.is_full() {
            return;
        }
        // The fill of the panels and of the header row. Terminal mode has none,
        // so the text takes the app's dim style and a line marks the edge.
        let bg = self.look.theme.fill(theme::Role::PanelFill);
        let dim = if bg == Color::Default {
            dim
        } else {
            over(
                &self.look.theme,
                &mut self.screen,
                theme::Role::Dim,
                Emphasis::Plain,
                bg,
            )
        };
        if let Some(header) = self.header() {
            self.paint_counts_row(header, bg, dim);
        }
        let Some(rect) = self.aside.rect() else {
            return;
        };
        let edge = u16::from(bg == Color::Default);
        if edge > 0 {
            let line = over(
                &self.look.theme,
                &mut self.screen,
                theme::Role::Line,
                Emphasis::Plain,
                bg,
            );
            for y in rect.y..rect.bottom() {
                self.screen.put_str(rect.x, y, LINE, line);
            }
        }
        let body = Rect::new(
            rect.x + edge,
            rect.y,
            rect.width.saturating_sub(edge),
            rect.height,
        );
        let blank = &BLANK[..usize::from(body.width.min(SIDEBAR_MAX))];
        let fill = over(
            &self.look.theme,
            &mut self.screen,
            theme::Role::PanelFill,
            Emphasis::Plain,
            bg,
        );
        for y in body.y..body.bottom() {
            self.screen.put_str(body.x, y, blank, fill);
        }
        let mut roles = [u32::MAX; ROLE_SLOTS];
        roles[role_slot(Role::Dim)] = dim;
        // A docked sidebar has the top rows to itself: the title, then the
        // panels under a blank row.
        let mut top = body.y;
        if self.aside.docked() {
            self.paint_title(body, bg, dim);
            top = (body.y + TITLE_ROWS).min(body.bottom());
        }
        self.sidebar.draw(body.width, body.bottom() - top);
        // The panels' title lines, as the sidebar reports them: what a click
        // on a panel's rows focuses, and a click on the title folds.
        for &(line, panel) in self.sidebar.titles() {
            let y = top + line as u16;
            if y < body.bottom() {
                self.panel_rows.push((y, panel));
            }
        }
        let lines = self.sidebar.lines();
        for (offset, spans) in lines.iter().enumerate() {
            let y = top + offset as u16;
            if y >= body.bottom() {
                break;
            }
            let mut x = body.x;
            for span in spans {
                if x >= body.right() {
                    break;
                }
                let slot = role_slot(span.role());
                if roles[slot] == u32::MAX {
                    let (role, emphasis) = of(span.role());
                    roles[slot] = over(&self.look.theme, &mut self.screen, role, emphasis, bg);
                }
                x = self.screen.put_str(x, y, span.text(), roles[slot]);
            }
        }
    }

    /// The index of the panel whose rows hold the screen row `y`, once the
    /// sidebar drew.
    pub(super) fn panel_at(&self, y: u16) -> Option<usize> {
        let at = self.panel_rows.iter().rposition(|(title, _)| *title <= y)?;
        Some(self.panel_rows[at].1)
    }

    /// Whether the screen row `y` is a panel's title row, once the sidebar
    /// drew: the row a click folds the panel on.
    pub(super) fn title_row(&self, y: u16) -> bool {
        self.panel_rows.iter().any(|(title, _)| *title == y)
    }

    /// Draws the header row: the counts row the panels fold into.
    fn paint_counts_row(&mut self, rect: Rect, bg: Color, dim: u32) {
        if rect.is_empty() {
            return;
        }
        if bg != Color::Default {
            let fill = over(
                &self.look.theme,
                &mut self.screen,
                theme::Role::PanelFill,
                Emphasis::Plain,
                bg,
            );
            let mut x = rect.x;
            while x < rect.right() {
                let room = usize::from((rect.right() - x).min(SIDEBAR_MAX));
                x = self.screen.put_str(x, rect.y, &BLANK[..room], fill);
            }
        }
        self.paint_counts(rect, bg, dim);
    }

    /// The top of the docked sidebar: the name and the session's title on one
    /// row, the folder the run is in on the next.
    fn paint_title(&mut self, rect: Rect, bg: Color, dim: u32) {
        let accent = over(
            &self.look.theme,
            &mut self.screen,
            theme::Role::Accent,
            Emphasis::Bold,
            bg,
        );
        let text = over(
            &self.look.theme,
            &mut self.screen,
            theme::Role::Text,
            Emphasis::Plain,
            bg,
        );
        let title: Option<Arc<str>> = self
            .sessions
            .iter()
            .find(|row| row.current && !row.title.is_empty())
            .map(|row| Arc::clone(&row.title));
        let folder = Arc::clone(&self.place);
        let (y, limit) = (rect.y, rect.right());
        let screen = &mut self.screen;
        let mut x = rect.x;
        put(screen, &mut x, y, " ", text, limit);
        put(screen, &mut x, y, NAME, accent, limit);
        put(screen, &mut x, y, "  ", text, limit);
        if let Some(title) = title {
            put(screen, &mut x, y, &title, text, limit);
        }
        if rect.height > 1 {
            let mut x = rect.x;
            put(screen, &mut x, y + 1, " ", dim, limit);
            put(screen, &mut x, y + 1, &folder, dim, limit);
        }
    }

    /// The counts row the panels fold into: the name, the share of the window
    /// with its bar and the cost at the left, and the changed files, the
    /// sessions that wait, the clients and the hint at the right, dropped from
    /// the right when the row is short.
    fn paint_counts(&mut self, rect: Rect, bg: Color, dim: u32) {
        let counts = self.counts();
        let accent = over(
            &self.look.theme,
            &mut self.screen,
            theme::Role::Accent,
            Emphasis::Bold,
            bg,
        );
        let text = over(
            &self.look.theme,
            &mut self.screen,
            theme::Role::Text,
            Emphasis::Plain,
            bg,
        );
        let add = over(
            &self.look.theme,
            &mut self.screen,
            theme::Role::Add,
            Emphasis::Plain,
            bg,
        );
        let delete = over(
            &self.look.theme,
            &mut self.screen,
            theme::Role::Delete,
            Emphasis::Plain,
            bg,
        );
        let warn = over(
            &self.look.theme,
            &mut self.screen,
            theme::Role::Warn,
            Emphasis::Bold,
            bg,
        );
        let files = if counts.files.as_str() == "0" {
            0
        } else {
            columns(counts.files.as_str())
                + 7
                + columns(counts.added.as_str())
                + 1
                + columns(counts.removed.as_str())
        };
        let waits = if counts.waits.as_str().is_empty() {
            0
        } else {
            columns(WAITS) + 1 + columns(counts.waits.as_str())
        };
        let hint = match &self.keyfile {
            Some(file) => file.labels().foot(Foot::SidebarHint),
            None => HINT,
        };
        let parts = [
            files,
            waits,
            columns(counts.clients.as_str()),
            columns(hint),
        ];
        let left = 1
            + columns(NAME)
            + 2
            + columns(counts.share.as_str())
            + 1
            + GAUGE as u16
            + 1
            + columns(counts.cost.as_str());
        let (y, limit) = (rect.y, rect.right());
        let mut kept = parts.len();
        while kept > 0 && left + 1 + width_of(&parts, kept) > rect.width {
            kept -= 1;
        }
        let right = limit.saturating_sub(width_of(&parts, kept));
        let screen = &mut self.screen;
        let mut x = rect.x;
        put(screen, &mut x, y, " ", text, right);
        put(screen, &mut x, y, NAME, accent, right);
        put(screen, &mut x, y, "  ", text, right);
        put(screen, &mut x, y, counts.share.as_str(), dim, right);
        put(screen, &mut x, y, " ", text, right);
        let filled = usize::try_from(counts.percent.min(100) * GAUGE as u64 / 100).unwrap_or(0);
        put(screen, &mut x, y, &FULL[..filled * GLYPH], accent, right);
        put(screen, &mut x, y, &EMPTY[filled * GLYPH..], dim, right);
        put(screen, &mut x, y, " ", text, right);
        put(screen, &mut x, y, counts.cost.as_str(), text, right);

        let mut x = right;
        let mut shown = false;
        for (index, part) in parts.iter().enumerate().take(kept) {
            if *part == 0 {
                continue;
            }
            if shown {
                put(screen, &mut x, y, "  ", text, limit);
                put(screen, &mut x, y, DOT, dim, limit);
                put(screen, &mut x, y, "  ", text, limit);
            }
            shown = true;
            match index {
                0 => {
                    put(screen, &mut x, y, counts.files.as_str(), text, limit);
                    put(screen, &mut x, y, " files ", text, limit);
                    put(screen, &mut x, y, counts.added.as_str(), add, limit);
                    put(screen, &mut x, y, " ", text, limit);
                    put(screen, &mut x, y, counts.removed.as_str(), delete, limit);
                }
                1 => {
                    put(screen, &mut x, y, WAITS, warn, limit);
                    put(screen, &mut x, y, " ", text, limit);
                    put(screen, &mut x, y, counts.waits.as_str(), text, limit);
                }
                2 => put(screen, &mut x, y, counts.clients.as_str(), text, limit),
                _ => put(screen, &mut x, y, hint, dim, limit),
            }
        }
    }

    /// The texts the header row reads, from the panels that hold them.
    fn counts(&mut self) -> Counts {
        let (share, percent, cost) = match self.sidebar.get_mut::<ContextPanel>() {
            Some(panel) => (panel.share().clone(), panel.percent(), panel.cost().clone()),
            None => (Text::Empty, 0, Text::Empty),
        };
        let (files, added, removed) = match self.sidebar.get_mut::<ChangesPanel>() {
            Some(panel) => (
                panel.files().clone(),
                panel.added().clone(),
                panel.removed().clone(),
            ),
            None => (Text::Empty, Text::Empty, Text::Empty),
        };
        let waits = match self.sidebar.get_mut::<SessionsPanel>() {
            Some(panel) => panel.waits().clone(),
            None => Text::Empty,
        };
        let clients = match self.sidebar.get_mut::<ClientsPanel>() {
            Some(panel) => panel.count().clone(),
            None => Text::Empty,
        };
        Counts {
            share,
            percent,
            cost,
            files,
            added,
            removed,
            waits,
            clients,
        }
    }
}

/// The style id of a theme role over the sidebar's `bg`, interned once for the
/// frame it is drawn in. Terminal mode has no fill, so the role's own style
/// draws.
fn over(
    theme: &crate::theme::Theme,
    screen: &mut Screen,
    role: theme::Role,
    emphasis: Emphasis,
    bg: Color,
) -> u32 {
    let mut style = theme.styled(role, emphasis);
    if bg != Color::Default {
        style.bg = bg;
    }
    screen.intern(style)
}

/// Write `text` at `*x` on row `y`, stop at column `limit`, and move `x` past
/// what it wrote.
fn put(screen: &mut Screen, x: &mut u16, y: u16, text: &str, style: u32, limit: u16) {
    *x = screen.put_str_until(*x, y, text, style, limit);
}

/// The columns the first `kept` parts of the counts row's right side take, with
/// the `·` between them and none for a part that is empty.
fn width_of(parts: &[u16], kept: usize) -> u16 {
    let mut width = 0;
    let mut shown = false;
    for part in parts.iter().take(kept) {
        if *part == 0 {
            continue;
        }
        if shown {
            width += SEPARATOR;
        }
        shown = true;
        width += part;
    }
    width
}

/// The columns `text` takes on a line.
fn columns(text: &str) -> u16 {
    u16::try_from(text.width()).unwrap_or(u16::MAX)
}

/// What a slot role means in the theme: the role, and its emphasis. A panel
/// title is body text, bold, and what a panel adds or removes is the theme's
/// add and delete.
const fn of(role: Role) -> (theme::Role, Emphasis) {
    match role {
        Role::Text => (theme::Role::Text, Emphasis::Plain),
        Role::Dim => (theme::Role::Dim, Emphasis::Plain),
        Role::Strong => (theme::Role::Text, Emphasis::Bold),
        Role::Title => (theme::Role::Text, Emphasis::Bold),
        Role::Accent => (theme::Role::Accent, Emphasis::Bold),
        Role::Good => (theme::Role::Add, Emphasis::Plain),
        Role::Warn => (theme::Role::Warn, Emphasis::Bold),
        Role::Bad => (theme::Role::Delete, Emphasis::Plain),
    }
}

/// The slot a role's style is cached in for one frame's draw.
const fn role_slot(role: Role) -> usize {
    match role {
        Role::Text => 0,
        Role::Dim => 1,
        Role::Strong => 2,
        Role::Title => 3,
        Role::Accent => 4,
        Role::Good => 5,
        Role::Warn => 6,
        Role::Bad => 7,
    }
}

/// The rows the title takes at the top of a docked sidebar: the name, the
/// folder and a blank row.
const TITLE_ROWS: u16 = 3;

/// `path` as the docked sidebar shows it: under `home`, from a `~`, cut at a
/// path boundary only.
pub(super) fn shown(path: &Path, home: Option<&str>) -> Arc<str> {
    let rest = home
        .filter(|home| !home.is_empty())
        .and_then(|home| path.strip_prefix(home).ok());
    match rest {
        Some(rest) if rest.as_os_str().is_empty() => Arc::from("~"),
        Some(rest) => Arc::from(format!("~/{}", rest.to_string_lossy())),
        None => Arc::from(path.to_string_lossy()),
    }
}

/// The part header field that keeps whether a headless run wrote the part.
const META_HEADLESS: u32 = 6;

/// The sessions the list of `folder` holds, newest first. Blocking: one small
/// file, read in a `spawn_blocking` job.
///
/// A list that has to be rebuilt says only what a part header holds: a title
/// and a model are then empty, and the next turn the worker runs fills them in.
fn list_folder(folder: &std::path::Path) -> Vec<Entry> {
    let list = SessionList::open(folder, |header, _| Entry {
        session: header.session_id.clone(),
        updated_ms: header.created_ms,
        turns: 0,
        title: String::new(),
        profile: String::new(),
        provider: String::new(),
        model: String::new(),
        part: header.part_id,
        state: State::Interrupted,
        headless: marked_headless(header),
    });
    match list {
        Ok(list) => {
            let mut entries = list.all().to_vec();
            reagent_ai::record::fill_titles(folder, &mut entries);
            entries
        }
        Err(_) => Vec::new(),
    }
}

/// Whether the part header of a session says a headless run wrote it, so a
/// rebuilt list keeps drawing the run dim.
fn marked_headless(header: &reagent_store::Header) -> bool {
    let mut decoder = reagent_store::Decoder::new(&header.meta);
    while let Ok(Some((field, value))) = decoder.next_field() {
        if field == META_HEADLESS {
            return value.varint() == Some(1);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Initialized;
    use crate::theme::{Depth, Mode, Rgb};

    /// The session of the tests, and the title the sessions list gives it.
    const SESSION: &str = "01m68hgz7c9d";

    /// An app in full screen with a terminal of `columns` by `rows`, in terminal
    /// mode, its panels fed a turn and a file, its sessions and clients set.
    fn app(columns: u16, rows: u16) -> App {
        let init = Initialized {
            session: SESSION.to_owned(),
            ..Initialized::default()
        };
        let mut app = App::new(&init, (columns, rows), true);
        app.place = shown(Path::new("/home/ray/work/other"), Some("/home/ray"));
        app.screen.resize(columns, rows);
        app.reflow();
        app.panels_update(&Update::Model {
            name: "deepseek/deepseek-flash",
            window: 200_000,
        });
        app.set_prices((3.0, 15.0));
        app.panels_update(&Update::Usage {
            input: 50_000,
            cached: 0,
            output: 1_000,
        });
        app.panels_update(&Update::ToolCall {
            name: "write",
            arguments: r#"{"path":"src/main.rs","content":"one\ntwo"}"#,
        });
        let rows_now = 1_000_000;
        app.sessions = vec![
            SessionRow {
                id: Arc::from(SESSION),
                title: Arc::from("fix the start test"),
                updated: rows_now - 60,
                live: true,
                waiting: false,
                current: true,
                headless: false,
            },
            SessionRow {
                id: Arc::from("older"),
                title: Arc::from("an older session"),
                updated: rows_now - 2 * 3_600,
                live: false,
                waiting: true,
                current: false,
                headless: false,
            },
        ];
        let rows = app.sessions.clone();
        let panel = app
            .sidebar
            .get_mut::<SessionsPanel>()
            .expect("a sessions panel");
        panel.set_rows(rows, rows_now);
        app.client_joined(2, "tmux:2");
        app
    }

    /// The rows of the frame the app draws.
    fn drawn(app: &mut App) -> Vec<String> {
        app.paint();
        let mut frame = Vec::new();
        app.screen.diff(&mut frame);
        (0..app.screen.rows())
            .map(|y| app.screen.row_text(y))
            .collect()
    }

    /// The bytes of the frame the app draws.
    fn frame(app: &mut App) -> Vec<u8> {
        app.paint();
        let mut frame = Vec::new();
        app.screen.diff(&mut frame);
        frame
    }

    /// The column a text starts at on the first row that holds it.
    fn column_of(rows: &[String], needle: &str) -> Option<usize> {
        rows.iter()
            .find_map(|row| row.find(needle).map(|at| row[..at].chars().count()))
    }

    /// The row a text is on.
    fn row_of(rows: &[String], needle: &str) -> Option<usize> {
        rows.iter().position(|row| row.contains(needle))
    }

    #[test]
    fn the_sidebar_draws_all_four_panels_at_160_columns() {
        let mut app = app(160, 40);
        let rows = drawn(&mut app);
        for wanted in [
            "Context",
            "deepseek/deepseek-flash",
            "51k / 200k",
            "$0.16",
            "01M68HGZ",
            "Changes",
            "src/main.rs",
            "+2",
            "Sessions",
            "● fix the start test",
            "⏸ an older session",
            "Clients",
            "this terminal",
            "tmux:2",
            "watching",
        ] {
            assert!(
                rows.iter().any(|row| row.contains(wanted)),
                "{wanted:?} is missing: {rows:#?}"
            );
        }
        assert_eq!(app.main, Rect::new(0, 0, 120, 40), "the main area");
        assert_eq!(app.aside.rect(), Some(Rect::new(120, 0, 40, 40)));
        // Every panel line starts inside the sidebar's forty columns.
        for wanted in ["Context", "Changes", "Sessions", "Clients"] {
            let at = column_of(&rows, wanted).expect("a panel title");
            assert!((120..160).contains(&at), "{wanted:?} at {at}");
        }
    }

    #[test]
    fn a_folder_under_home_reads_from_the_tilde_at_a_path_boundary() {
        let at = |path: &str, home: &str| shown(Path::new(path), Some(home));
        assert_eq!(&*at("/home/ray/x", "/home/ray"), "~/x");
        assert_eq!(&*at("/home/ray/x", "/home/ray/"), "~/x");
        assert_eq!(&*at("/home/ray", "/home/ray"), "~");
        assert_eq!(&*at("/home/rayandrew/x", "/home/ray"), "/home/rayandrew/x");
        assert_eq!(&*shown(Path::new("/srv/x"), None), "/srv/x");
        assert_eq!(&*at("/srv/x", ""), "/srv/x");
    }

    #[test]
    fn the_docked_sidebar_names_the_session_and_its_folder_on_top() {
        let mut app = app(160, 40);
        let rows = drawn(&mut app);
        // Terminal mode: the sidebar's first column is its `│` edge.
        let side = |row: &str| row.chars().skip(121).collect::<String>();
        let main = |row: &str| row.chars().take(120).collect::<String>();
        assert!(side(&rows[0]).starts_with(" reagent"), "{:?}", rows[0]);
        assert!(side(&rows[0]).contains("fix the start"), "{:?}", rows[0]);
        assert_eq!(
            side(&rows[1]).trim_end(),
            " ~/work/other",
            "the session's folder"
        );
        assert!(side(&rows[2]).trim().is_empty(), "a blank row under it");
        assert!(!main(&rows[0]).contains("reagent"), "no header row");
        assert!(!rows[0].contains(HINT), "the counts row is folded away");
        assert!(
            rows.iter().position(|row| row.contains("Context")) >= Some(3),
            "the panels start under the title and its blank row"
        );
    }

    #[test]
    fn the_clients_panel_takes_a_join_and_a_leave() {
        let mut app = app(160, 40);
        let rows = drawn(&mut app);
        assert!(rows.iter().any(|row| row.contains("tmux:2")), "joined");
        assert!(rows.iter().any(|row| row.contains("watching")), "watching");
        app.client_left(2);
        let rows = drawn(&mut app);
        assert!(
            !rows.iter().any(|row| row.contains("tmux:2")),
            "left: {rows:#?}"
        );
        assert!(rows.iter().any(|row| row.contains("this terminal")), "you");
    }

    /// A moved leader and a moved sidebar key reach the folded row's hint.
    #[test]
    fn keys_in_force_the_folded_row_names_the_moved_keys() {
        let mut app = app(100, 30);
        app.read_keys_text("[keys]\nleader = \"alt+x\"\n[leader]\nsidebar = \"s\"\n");
        let rows = drawn(&mut app);
        assert!(
            rows[0].trim_end().ends_with("alt+x s panels"),
            "{:?}",
            rows[0]
        );
    }

    #[test]
    fn the_panels_fold_into_a_header_row_at_100_columns() {
        let mut app = app(100, 30);
        let rows = drawn(&mut app);
        assert_eq!(app.main, Rect::new(0, 1, 100, 29), "the full width");
        assert_eq!(app.aside, layout::Sidebar::Off);
        let head = &rows[0];
        for wanted in [
            "reagent",
            "25%",
            "▰▰",
            "$0.16",
            "1 files",
            "+2",
            "-0",
            "⏸",
            "1 waits",
            "2 clients",
            "ctrl+x b panels",
        ] {
            assert!(head.contains(wanted), "{wanted:?} is missing: {head:?}");
        }
        // The right side ends at the row's right edge.
        assert!(head.trim_end().ends_with(HINT), "{head:?}");
        assert_eq!(head.chars().count(), 100);
        for gone in ["Context", "Changes", "Sessions", "Clients"] {
            assert!(column_of(&rows, gone).is_none(), "{gone:?} shows folded");
        }
    }

    #[test]
    fn ctrl_x_b_opens_the_panels_as_an_overlay_at_100_columns() {
        let mut app = app(100, 30);
        app.sidebar_leader(Leader::ToggleSidebar);
        let rows = drawn(&mut app);
        assert_eq!(
            app.aside.rect(),
            Some(Rect::new(72, 1, 28, 29)),
            "the overlay"
        );
        assert_eq!(app.main, Rect::new(0, 1, 100, 29), "the main width");
        assert_eq!(column_of(&rows, "▾ Context"), Some(73), "{:#?}", &rows[..8]);
        assert!(rows[0].contains(HINT), "the header row of counts stays");
    }

    #[test]
    fn ctrl_x_b_folds_the_panels_into_the_header_row_at_160_columns() {
        let mut app = app(160, 40);
        assert!(app.aside.docked(), "docked at first");
        app.sidebar_leader(Leader::ToggleSidebar);
        let rows = drawn(&mut app);
        assert_eq!(app.aside, layout::Sidebar::Off);
        assert_eq!(app.main, Rect::new(0, 1, 160, 39), "the full width");
        assert!(rows[0].contains(HINT), "{:?}", rows[0]);
        assert!(column_of(&rows, "Context").is_none(), "folded");
    }

    #[test]
    fn the_header_row_drops_its_right_parts_when_it_is_short() {
        let mut wide = app(70, 30);
        let rows = drawn(&mut wide);
        let head = &rows[0];
        assert!(head.contains("1 files"), "{head:?}");
        assert!(head.contains("1 waits"), "{head:?}");
        assert!(head.contains("clients"), "the clients still fit: {head:?}");
        assert!(!head.contains(HINT), "the hint dropped: {head:?}");
        let mut narrow = app(50, 30);
        let rows = drawn(&mut narrow);
        let head = &rows[0];
        assert!(head.contains("1 files"), "{head:?}");
        assert!(!head.contains("waits"), "the waits dropped: {head:?}");
        assert!(!head.contains("clients"), "the clients dropped: {head:?}");
        assert!(!head.contains(HINT), "the hint dropped: {head:?}");
    }

    #[test]
    fn terminal_mode_draws_the_line_and_no_fill() {
        let mut app = app(160, 40);
        let frame = frame(&mut app);
        let rows = drawn(&mut app);
        for y in 1..40 {
            assert_eq!(
                rows[y].chars().nth(120),
                Some(LINE.chars().next().unwrap_or(' ')),
                "row {y} of {rows:#?}"
            );
        }
        assert_eq!(column_of(&rows, "▾ Context"), Some(121), "the panels");
        assert!(
            !frame.windows(3).any(|bytes| bytes == b"48;"),
            "terminal mode draws a fill"
        );
    }

    #[test]
    fn rich_mode_fills_the_sidebar_and_draws_no_line() {
        let mut app = app(160, 40);
        app.look.color = true;
        app.look.depth = Depth::TrueColor;
        app.answered(Rgb::new(0x14, 0x14, 0x14));
        app.restyle(Mode::Rich {
            background: Rgb::new(0x14, 0x14, 0x14),
            dark: true,
        });
        let frame = frame(&mut app);
        let rows = drawn(&mut app);
        assert!(
            frame.windows(3).any(|bytes| bytes == b"48;"),
            "rich mode fills no cell"
        );
        assert_ne!(rows[5].chars().nth(120), Some('│'), "no line in rich mode");
    }

    #[test]
    fn inline_mode_has_no_header_row() {
        let init = Initialized {
            session: SESSION.to_owned(),
            ..Initialized::default()
        };
        let mut app = App::new(&init, (100, 30), false);
        app.reflow();
        let rows = drawn(&mut app);
        assert_eq!(app.main.y, 0, "no row above the main");
        assert!(app.header().is_none(), "no header row");
        assert!(!rows[0].contains(HINT), "no header row: {:?}", rows[0]);
    }

    /// Sends a press and a release on the sidebar cell of `row`: a click.
    fn click_row(app: &mut App, row: usize) {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let column = app.aside.rect().expect("a sidebar").x + 3;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.mouse(MouseEvent {
                kind,
                column,
                row: row as u16,
                modifiers: KeyModifiers::NONE,
            });
        }
    }

    #[test]
    fn a_selected_row_does_not_shift_with_no_usage_yet() {
        let init = Initialized {
            session: SESSION.to_owned(),
            ..Initialized::default()
        };
        let mut app = App::new(&init, (160, 40), true);
        app.screen.resize(160, 40);
        app.reflow();
        app.panels_update(&Update::Model {
            name: "openrouter/deepseek/deepseek-v4.1-flash-1017",
            window: 1_000_000,
        });
        let rows = drawn(&mut app);
        for (index, title) in ["Context", "Changes", "Sessions", "Clients"]
            .iter()
            .enumerate()
        {
            let row = row_of(&rows, title).expect("a title row");
            assert_eq!(
                app.panel_at(row as u16),
                Some(index),
                "{title}: {rows:#?} {:?}",
                app.panel_rows
            );
        }
    }

    /// The owner's steps: Sessions folded by a click, then a click on its title
    /// opens Sessions again and leaves Clients as it was.
    #[test]
    fn a_click_on_a_folded_title_opens_that_panel() {
        let mut app = app(160, 40);
        let rows = drawn(&mut app);
        let sessions = row_of(&rows, "Sessions").expect("the sessions title");
        click_row(&mut app, sessions);
        let rows = drawn(&mut app);
        let clients_rows = rows.len()
            - rows
                .iter()
                .rev()
                .position(|r| !r.trim().is_empty())
                .unwrap_or(0);
        let sessions = row_of(&rows, "Sessions").expect("the sessions title");
        assert!(
            rows[sessions].contains('\u{25b8}'),
            "Sessions folded: {rows:#?}"
        );
        let clients_before = rows.iter().filter(|r| r.contains("this terminal")).count();
        click_row(&mut app, sessions);
        let rows = drawn(&mut app);
        let sessions = row_of(&rows, "Sessions").expect("the sessions title");
        assert!(
            rows[sessions].contains('\u{25be}'),
            "Sessions open again: {rows:#?}"
        );
        let clients_after = rows.iter().filter(|r| r.contains("this terminal")).count();
        assert_eq!(clients_before, clients_after, "Clients left as it was");
        let _ = clients_rows;
    }

    /// A selected row inside a panel draws a dim marker and accent text, as a
    /// focused title does; it is no title, so the panels under it keep their
    /// own clicks.
    #[test]
    fn a_selected_row_does_not_shift_the_panel_under_a_click() {
        let mut app = app(160, 40);
        app.sidebar.set_active(true);
        // The focus on the changes panel draws its first file as selected.
        app.sidebar.focus_next();
        let rows = drawn(&mut app);
        for (index, title) in ["Context", "Changes", "Sessions", "Clients"]
            .iter()
            .enumerate()
        {
            let row = row_of(&rows, title).expect("a title row");
            assert_eq!(app.panel_at(row as u16), Some(index), "{title}: {rows:#?}");
            assert!(app.title_row(row as u16), "{title} is a title row");
        }
        let titles = (0..rows.len() as u16).filter(|y| app.title_row(*y)).count();
        assert_eq!(titles, 4, "only the four titles are title rows: {rows:#?}");
    }

    #[test]
    fn a_click_on_a_panel_row_finds_its_panel() {
        let mut app = app(160, 40);
        let rows = drawn(&mut app);
        let changes = row_of(&rows, "Changes").expect("the changes title");
        assert_eq!(app.panel_at(changes as u16), Some(1));
        let clients = row_of(&rows, "Clients").expect("the clients title");
        assert_eq!(app.panel_at(clients as u16), Some(3));
    }

    /// A file that moves nothing prints each word as the table has it.
    #[test]
    fn keys_in_force_sidebar_hint_matches_the_table() {
        let (file, _) = crate::keyfile::KeyFile::parse("");
        let labels = file.labels();
        assert_eq!(labels.foot(crate::keyfile::Foot::SidebarHint), super::HINT);
    }
}
