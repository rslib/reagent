//! The theme: the roles each region draws with.
//!
//! The app resolves every role once, into one style and one interned style id
//! per role and emphasis ([`Look`]), so a region draws with [`App::style`]: an
//! array index, never a color and never a style built per cell.
//!
//! The mode is the terminal's. Rich when it answered the `OSC 11` query
//! [`App::ask`] sends with the first frame setup, mixing the fills from the
//! background it reported; terminal when it did not answer that query before
//! the device attributes answer (`CSI c`) that ends the first frame's wait;
//! and no color at all under `NO_COLOR`, which beats both. A theme file, the
//! config key `theme`, moves any role after that ([`App::read_theme`]).
//!
//! The region modules still draw with the two styles the app has always had,
//! `DIM` and `REVERSE`; they are exactly the dim and the selection role of the
//! no-color theme, so the look holds until each region's restyle takes its ids
//! from [`App::style`].

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::Event;

use crate::input::Background;
use crate::screen::{Cell, Screen, Terminal};
use crate::theme::{Depth, Emphasis, Mode, Overrides, Rgb, Role, Theme};

use super::super::{App, Item, Look};

/// The safety limit of the first frame's wait: how long it waits for a terminal
/// that answers neither of its two queries.
///
/// A real terminal answers the device attributes query in a millisecond or two,
/// and the frame is drawn as soon as that answer arrives; this covers only the
/// terminal that answers nothing.
const BACKGROUND_WAIT: Duration = Duration::from_millis(50);

impl Look {
    /// Resolves every role of `mode` and interns its style into `screen`'s
    /// table, once, so drawing a cell needs an id and no lookup.
    ///
    /// `NO_COLOR` beats both modes: `color` false resolves the no-color theme
    /// whatever the terminal answered.
    pub(crate) fn resolve(
        mode: Mode,
        depth: Depth,
        overrides: Overrides,
        color: bool,
        screen: &mut Screen,
    ) -> Look {
        let mode = if color { mode } else { Mode::NoColor };
        let theme = Theme::resolve(mode, depth, &overrides);
        let mut ids = [[0; Emphasis::ALL.len()]; Role::ALL.len()];
        for role in Role::ALL {
            for emphasis in Emphasis::ALL {
                ids[role.index()][emphasis.index()] = screen.intern(theme.styled(role, emphasis));
            }
        }
        Look {
            theme,
            ids,
            depth,
            overrides,
            color,
        }
    }

    /// The id of a role with an emphasis on it.
    pub(crate) fn id(&self, role: Role, emphasis: Emphasis) -> u32 {
        self.ids[role.index()][emphasis.index()]
    }

    /// The mode every style was built for.
    pub(crate) fn mode(&self) -> Mode {
        self.theme.mode()
    }
}

impl App {
    /// The style id of a role with an emphasis on it: one array index, so a
    /// region draws a cell without color math or a style lookup.
    pub(crate) fn style(&self, role: Role, emphasis: Emphasis) -> u32 {
        self.look.id(role, emphasis)
    }

    /// The style id of `role` drawn over the fill of `fill`, so text on a card
    /// keeps the card's tint behind it. A mode with no fill answers the plain
    /// style. Interned: a style seen before costs no allocation.
    pub(crate) fn style_on(&mut self, role: Role, emphasis: Emphasis, fill: Role) -> u32 {
        let background = self.look.theme.fill(fill);
        if background == crate::screen::Color::Default {
            return self.style(role, emphasis);
        }
        let mut style = self.look.theme.styled(role, emphasis);
        style.bg = background;
        self.screen.intern(style)
    }

    /// Resolves every role again for `mode`, interned into the same style table,
    /// so the next frame draws in it.
    pub(crate) fn restyle(&mut self, mode: Mode) {
        let Look {
            depth,
            overrides,
            color,
            ..
        } = self.look;
        self.look = Look::resolve(mode, depth, overrides, color, &mut self.screen);
        self.retheme_replies();
        self.preview_restyled();
        self.dirty = true;
    }

    /// Gives the replies not yet committed the theme's markdown roles. A
    /// committed row keeps the colors it was drawn in.
    fn retheme_replies(&mut self) {
        let theme = self.look.theme;
        for item in self.pending.iter_mut().chain(self.live.iter_mut()) {
            if let Item::Reply { md, .. } = item {
                md.set_theme(&theme);
            }
        }
    }

    /// The terminal answered the background query: rich mode over the background
    /// it reported, which repaints once in the fills mixed from it.
    ///
    /// The background's own luminance picks the direction of the fills, so a
    /// light terminal is drawn light.
    pub(crate) fn answered(&mut self, background: Rgb) {
        self.restyle(Mode::Rich {
            background,
            dark: dark(background),
        });
    }

    /// Takes a theme file's role colors, over the ones the mode resolved.
    pub(crate) fn set_overrides(&mut self, overrides: Overrides) {
        let Look { depth, color, .. } = self.look;
        let mode = self.look.mode();
        self.look = Look::resolve(mode, depth, overrides, color, &mut self.screen);
        self.retheme_replies();
        self.preview_restyled();
        self.dirty = true;
    }

    /// Reads the theme file once at start with async I/O and takes its colors.
    ///
    /// A file that cannot be read, or a role the file gets wrong, is a note the
    /// way the other errors are, and the theme the mode gave stays.
    pub(crate) async fn read_theme(&mut self, path: Option<PathBuf>) {
        let Some(path) = path else {
            return;
        };
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) => {
                self.note(format!("theme: {}: {error}", path.display()));
                return;
            }
        };
        match Overrides::parse(&text) {
            Ok(overrides) => self.set_overrides(overrides),
            Err(error) => self.note(format!("{error} ({})", path.display())),
        }
    }

    /// Sends the first frame's queries, `OSC 11`, `OSC 7501` and then `CSI c`,
    /// in one write, and waits for the device attributes answer, which every
    /// terminal gives: that answer, and not a fixed time, is what ends the
    /// wait.
    ///
    /// `herdr` adds the kitty graphics query (`a=q`, `i=31`) between the two,
    /// before `CSI c`: a terminal that answers in order answers it before the
    /// wait ends, so the frame that draws first already knows whether herdr
    /// draws its panes with placeholders.
    ///
    /// The color the terminal reported before that answer is the mode, and is
    /// what this returns: rich when an `OSC 11` answer came first, terminal when
    /// none did. [`BACKGROUND_WAIT`] is only the safety limit for a terminal that
    /// answers neither query.
    ///
    /// A key that arrives inside the wait is handled rather than lost, and the
    /// wait holds one deadline instead of a new timer per key.
    pub(crate) async fn ask(
        &mut self,
        terminal: &mut impl Terminal,
        answer: Option<&Background>,
        herdr: bool,
    ) -> Option<Rgb> {
        let answer = answer?;
        if answer.attributes() {
            return answer.color();
        }
        let query = match herdr {
            true => super::super::term::GRAPHICS_QUERY,
            false => super::super::term::QUERY,
        };
        // The queries go out on a write of their own: the mode's setup is already
        // on its way, and the first frame is drawn once the answer is in.
        if terminal.write_frame(query).await.is_err() {
            return answer.color();
        }
        let deadline = tokio::time::Instant::now() + BACKGROUND_WAIT;
        let sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(sleep);
        loop {
            if answer.attributes() {
                return answer.color();
            }
            tokio::select! {
                event = terminal.next_event() => match event {
                    Ok(Event::Key(key)) => self.key(key),
                    Ok(Event::Mouse(mouse)) => self.mouse(mouse),
                    Ok(Event::Resize(columns, rows)) => self.resize(columns, rows),
                    Ok(_) => {}
                    // The input side is gone: the loop's own read reports it.
                    Err(_) => return answer.color(),
                },
                // The device attributes answer: the wait is over.
                () = answer.wait_attributes() => {}
                () = &mut sleep => return answer.color(),
            }
        }
    }

    /// Waits for a late answer to the background query, and never for a
    /// terminal that was not asked or already answered: no timer, no wakeup.
    ///
    /// A terminal that answered the device attributes but not the background
    /// query answers no color here, so the loop keeps this wait armed and a late
    /// `OSC 11` answer repaints once.
    ///
    /// It takes no `self`, so the loop can wait on it while a branch of the same
    /// `select!` draws for the answer.
    pub(crate) async fn late_answer(answer: Option<&Background>) {
        match answer {
            Some(answer) => answer.wait().await,
            None => std::future::pending().await,
        }
    }

    /// The app's start: the theme file, then the first frame's queries and the
    /// wait for the device attributes answer, which every terminal gives.
    ///
    /// The color an `OSC 11` answer reported before that, if there was one, is
    /// what this returns.
    /// `answer` is `None` for a terminal that does not send the queries, which is
    /// every test's, so such a run writes no query and waits not at all.
    pub(crate) async fn start(
        &mut self,
        terminal: &mut impl Terminal,
        theme: Option<PathBuf>,
        answer: Option<&Background>,
    ) -> Option<Rgb> {
        self.read_theme(theme).await;
        let herdr = crate::picture_native::asks_graphics(|name| std::env::var(name).ok());
        self.ask(terminal, answer, herdr).await
    }

    /// Draws the text box's fill: [`Role::InputFill`]'s background under the rows
    /// the editor drew.
    ///
    /// Terminal mode and `NO_COLOR` have no fill, so no cell is touched. The fill
    /// goes on the cells the editor drew with the default style, which leaves
    /// the cursor on its selection style and the text on its own.
    pub(crate) fn paint_input_fill(&mut self) {
        let fill = self.style(Role::InputFill, Emphasis::Plain);
        if fill == self.screen.default_style() {
            return;
        }
        let width = self.text_width();
        let height = self.editor_height();
        // The text box is the rows above the status line, which the frame draws
        // on its last row: live entries, the card, the box, then the status.
        let last = self.screen.rows().saturating_sub(1);
        for y in last.saturating_sub(height)..last {
            for x in 0..width {
                let cell = self.screen.back().cell(x, y);
                if cell.style() != self.screen.default_style() {
                    continue;
                }
                let text = (cell.bits() >> 32) as u32;
                self.screen.set(x, y, Cell::new(text, fill));
            }
        }
    }
}

/// The role a code token draws with.
#[allow(dead_code, reason = "the code block draws it once its restyle lands")]
pub(crate) fn token_role(token: crate::highlight::Token) -> Role {
    use crate::highlight::Token;

    match token {
        Token::Text => Role::Text,
        Token::Keyword => Role::CodeKeyword,
        Token::String => Role::CodeString,
        Token::Comment => Role::CodeComment,
        Token::Number => Role::CodeNumber,
        Token::Type => Role::CodeType,
        Token::Function => Role::CodeFunction,
        Token::Macro => Role::CodeMacro,
        Token::Punct => Role::CodePunct,
        Token::Added => Role::Add,
        Token::Removed => Role::Delete,
    }
}

/// True for a background that is dark: the luminance a 299/587/114 weight gives
/// it is under half, which is the split the mockup's dark and light rules use.
fn dark(background: Rgb) -> bool {
    let Rgb { r, g, b } = background;
    let luminance = 299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b);
    luminance < 128 * 1000
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use crossterm::event::Event;

    use crate::client::Initialized;
    use crate::input::Background;
    use crate::screen::{Color, SYNC_PAIR, Terminal};
    use crate::theme::{Depth, Emphasis, Mode, Overrides, Rgb, Role, Theme};

    use super::super::super::{App, DIM, Look, REVERSE, term};

    /// The columns and the rows of the test terminal.
    const COLUMNS: u16 = 80;
    const ROWS: u16 = 24;

    /// The background a dark terminal answers with.
    const DARK: Rgb = Rgb::new(0x14, 0x14, 0x14);

    /// An app in full screen, its theme asked for color whatever the terminal
    /// running the tests says.
    fn app() -> App {
        let mut app = App::new(&Initialized::default(), (COLUMNS, ROWS), true);
        app.look.color = true;
        app.look.depth = Depth::TrueColor;
        app
    }

    /// The background color a cell of style `id` draws.
    fn background(app: &App, id: u32) -> Color {
        for role in Role::ALL {
            for emphasis in Emphasis::ALL {
                if app.style(role, emphasis) == id {
                    return app.look.theme.fill(role);
                }
            }
        }
        Color::Default
    }

    /// The style ids of the cells of the rows the text box drew.
    fn box_cells(app: &App) -> Vec<u32> {
        let last = app.screen.rows().saturating_sub(1);
        let mut ids = Vec::new();
        for y in last.saturating_sub(app.editor_rows)..last {
            for x in 0..app.text_width() {
                ids.push(app.screen.back().cell(x, y).style());
            }
        }
        ids
    }

    /// One frame drawn and the bytes the diff wrote for it.
    fn frame(app: &mut App) -> Vec<u8> {
        let mut bytes = Vec::new();
        app.paint();
        app.paint_input_fill();
        app.screen.diff(&mut bytes);
        bytes
    }

    #[test]
    fn token_role_maps_every_token() {
        use crate::highlight::Token;

        let pairs = [
            (Token::Text, Role::Text),
            (Token::Keyword, Role::CodeKeyword),
            (Token::String, Role::CodeString),
            (Token::Comment, Role::CodeComment),
            (Token::Number, Role::CodeNumber),
            (Token::Type, Role::CodeType),
            (Token::Function, Role::CodeFunction),
            (Token::Macro, Role::CodeMacro),
            (Token::Punct, Role::CodePunct),
            (Token::Added, Role::Add),
            (Token::Removed, Role::Delete),
        ];
        for (token, role) in pairs {
            assert_eq!(super::token_role(token), role, "{token:?}");
        }
    }

    /// The foreground colors a reply's markdown draws its pieces in.
    fn reply_colors(item: &super::super::super::Item) -> Vec<Color> {
        let super::super::super::Item::Reply { md, .. } = item else {
            panic!("not a reply: {item:?}");
        };
        let (done, live) = md.lines();
        done.iter()
            .chain(live)
            .flat_map(|line| line.pieces())
            .map(|piece| md.styles().get(piece.style()).fg)
            .collect()
    }

    #[test]
    fn a_reply_draws_its_code_and_headings_in_the_theme() {
        let mut app = app();
        app.text("# Title\n\n```rust\nfn main() {}\n```\n");
        // The background answer comes after the reply started: the reply
        // takes the new theme too.
        app.answered(DARK);
        let keyword = app.look.theme.style(Role::CodeKeyword).fg;
        let heading = app.look.theme.style(Role::Heading).fg;
        let live = reply_colors(app.live.last().expect("the live reply"));
        assert!(live.contains(&keyword), "no keyword color in {live:?}");
        assert!(live.contains(&heading), "no heading color in {live:?}");

        let reply: reagent_ai::AssistantMessage = serde_json::from_value(serde_json::json!({
            "content": [{ "type": "text", "text": "```rust\nlet x = 1;\n```\n" }],
            "provider": "fake",
            "model": "fake",
        }))
        .expect("a reply");
        app.finished(&reply);
        let done = reply_colors(app.pending.last().expect("the finished reply"));
        assert!(done.contains(&keyword), "no keyword color in {done:?}");
    }

    #[test]
    fn a_dark_background_answer_fills_the_text_box() {
        let mut app = app();
        app.answered(DARK);
        assert_eq!(
            app.look.theme.mode(),
            Mode::Rich {
                background: DARK,
                dark: true
            },
            "the answer picks rich mode over the background it reported"
        );
        let fill = app.style(Role::InputFill, Emphasis::Plain);
        assert_ne!(
            background(&app, fill),
            Color::Default,
            "the text box's fill is a background color"
        );
        frame(&mut app);
        let ids = box_cells(&app);
        assert!(
            ids.contains(&fill),
            "no row of the text box took the fill: {ids:?}"
        );
        assert!(
            ids.iter().all(|id| *id != app.screen.default_style()),
            "a cell of the text box was left without its fill"
        );
    }

    #[test]
    fn a_terminal_that_never_answers_the_background_query_draws_no_fill() {
        let mut app = app();
        app.restyle(Mode::Terminal);
        assert_eq!(app.look.theme.mode(), Mode::Terminal);
        frame(&mut app);
        for id in box_cells(&app) {
            assert_eq!(
                background(&app, id),
                Color::Default,
                "terminal mode draws no fill"
            );
        }
    }

    #[test]
    fn a_late_background_answer_repaints_once() {
        let mut app = app();
        app.restyle(Mode::Terminal);
        frame(&mut app);
        assert_eq!(
            frame(&mut app).len(),
            SYNC_PAIR,
            "a steady frame writes no cell"
        );

        // The answer lands after the first frame: the app asks for one frame.
        app.dirty = false;
        app.answered(DARK);
        assert!(app.needs_draw(), "the answer asks for a frame");

        // That frame carries the fills, and the one after it carries nothing:
        // the answer repaints once and no more.
        let fill = app.style(Role::InputFill, Emphasis::Plain);
        let drawn = frame(&mut app);
        assert!(
            drawn.len() > SYNC_PAIR,
            "the frame after the answer is not empty"
        );
        assert!(
            box_cells(&app).contains(&fill),
            "the frame after the answer draws the fill"
        );
        assert_eq!(
            frame(&mut app).len(),
            SYNC_PAIR,
            "the frame after that one draws nothing"
        );
    }

    #[test]
    fn no_color_draws_no_background_even_with_an_answer() {
        // `NO_COLOR` beats the answer: the same rich mode resolves to the
        // no-color theme, which has no fill.
        let mut app = app();
        app.look = Look::resolve(
            Mode::Rich {
                background: DARK,
                dark: true,
            },
            Depth::TrueColor,
            Overrides::default(),
            false,
            &mut app.screen,
        );
        assert_eq!(app.look.theme.mode(), Mode::NoColor);
        frame(&mut app);
        for id in box_cells(&app) {
            assert_eq!(
                background(&app, id),
                Color::Default,
                "no color draws no fill"
            );
        }
    }

    #[test]
    fn an_override_moves_a_role_whatever_the_background() {
        // The theme file moves the text box's fill off the mode's mix, and the
        // fill the frame draws follows it.
        let mut app = app();
        app.answered(DARK);
        let overrides = Overrides::parse("[roles]\ninput-fill = \"#102030\"\n").expect("parses");
        app.set_overrides(overrides);
        let fill = app.style(Role::InputFill, Emphasis::Plain);
        assert_eq!(background(&app, fill), Color::Rgb(0x10, 0x20, 0x30));
        assert_eq!(
            app.look.theme.mode(),
            Mode::Rich {
                background: DARK,
                dark: true
            },
            "a theme file keeps the mode the terminal answered with"
        );
    }

    #[test]
    fn the_theme_roles_are_the_two_styles_the_regions_draw() {
        // `DIM` and `REVERSE` are the dim and the selection role of the
        // no-color theme, which is what the regions draw with until each
        // restyle takes its ids from `App::style`.
        let theme = Theme::resolve(Mode::NoColor, Depth::TrueColor, &Overrides::default());
        assert_eq!(theme.styled(Role::Dim, Emphasis::Plain), DIM);
        assert_eq!(theme.styled(Role::Selection, Emphasis::Plain), REVERSE);
    }

    #[tokio::test]
    async fn a_theme_file_that_names_no_role_is_a_note_and_keeps_the_theme() {
        let dir = tempfile::tempdir().expect("a temp folder");
        let path = dir.path().join("theme.toml");
        tokio::fs::write(&path, "[roles]\nblink = \"#ffffff\"\n")
            .await
            .expect("write");
        let mut app = app();
        let mode = app.look.theme.mode();
        app.read_theme(Some(path)).await;
        assert_eq!(app.pending.len(), 1, "the error is one note");
        assert_eq!(app.look.theme.mode(), mode, "the theme the mode gave stays");

        // A file that is there and right moves a role.
        let path = dir.path().join("good.toml");
        tokio::fs::write(&path, "[roles]\naccent = \"#abcdef\"\n")
            .await
            .expect("write");
        app.read_theme(Some(path)).await;
        assert_eq!(app.pending.len(), 1, "no note for a file that reads");
        assert_eq!(
            app.look.theme.style(Role::Accent).fg,
            Color::Rgb(0xab, 0xcd, 0xef)
        );
    }

    #[test]
    fn a_terminal_that_answers_neither_query_waits_the_background_limit() {
        // A terminal that never answers costs the first frame the safety limit
        // and no more: no busy loop, no hang.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut app = app();
            let answer = Background::default();
            let mut terminal = Answers::new(&answer, None, false);
            let started = std::time::Instant::now();
            assert_eq!(app.ask(&mut terminal, Some(&answer), false).await, None);
            let waited = started.elapsed();
            assert!(
                waited >= Duration::from_millis(50) && waited < Duration::from_millis(500),
                "the first frame waited {waited:?}"
            );
            assert!(
                !answer.attributes(),
                "no answer ends the wait but the limit"
            );
            assert_eq!(terminal.writes, 1, "both queries are one write");
            assert_eq!(terminal.bytes, term::QUERY, "OSC 11 and then CSI c");
        });
    }

    #[tokio::test]
    async fn a_device_attributes_answer_alone_starts_at_once_in_terminal_mode() {
        // Every terminal answers the device attributes query, so the first
        // frame waits for that answer and not for the 50 ms limit: a terminal
        // that answers only it starts at once, in the 16 colors.
        let mut app = app();
        let answer = Background::default();
        let mut terminal = Answers::new(&answer, None, true);
        let started = std::time::Instant::now();
        let asked = tokio::time::timeout(
            Duration::from_millis(40),
            app.ask(&mut terminal, Some(&answer), false),
        );
        assert_eq!(
            asked
                .await
                .expect("the answer ends the wait, not the limit"),
            None,
            "no background answer: the terminal's own colors"
        );
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_millis(50),
            "the device attributes answer ended the wait at once: {waited:?}"
        );
        assert_eq!(terminal.writes, 1, "both queries are one write");
        assert_eq!(terminal.bytes, term::QUERY, "OSC 11 and then CSI c");
    }

    #[tokio::test]
    async fn an_osc_11_answer_before_the_device_attributes_starts_rich_at_once() {
        // The answers arrive in the order the queries went out: the background
        // first, which is the mode, and the device attributes, which ends the
        // wait. The frame is drawn with both, so the mode never flashes.
        let mut app = app();
        let answer = Background::default();
        let mut terminal = Answers::new(&answer, Some(DARK), true);
        let started = std::time::Instant::now();
        assert_eq!(
            app.ask(&mut terminal, Some(&answer), false).await,
            Some(DARK)
        );
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_millis(50),
            "the device attributes answer ended the wait at once: {waited:?}"
        );
        assert_eq!(terminal.bytes, term::QUERY);
    }

    #[tokio::test]
    async fn a_device_attributes_answer_ends_the_wait_before_the_background_limit() {
        // A terminal that takes a few milliseconds to answer starts a few
        // milliseconds in, not at the limit: the wait ends on the answer.
        let mut app = app();
        let answer = Background::default();
        let mut terminal = Answers {
            after: 5,
            ..Answers::new(&answer, None, true)
        };
        let started = std::time::Instant::now();
        assert_eq!(app.ask(&mut terminal, Some(&answer), false).await, None);
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(5) && waited < Duration::from_millis(50),
            "the device attributes answer ends the wait, not the 50 ms limit: {waited:?}"
        );
    }

    #[tokio::test]
    async fn a_device_attributes_answer_wakes_the_first_frames_wait_and_no_other() {
        // The reader records the answer, the first frame's wait resolves, and a
        // background answer alone never ends that wait: only `CSI c` does.
        let answer = Background::default();
        let reader = answer.clone();
        tokio::join!(answer.wait_attributes(), async {
            reader.record_attributes()
        });
        assert!(answer.attributes());
        assert_eq!(answer.color(), None, "the device attributes are no color");
        let other = Background::default();
        other.record(DARK);
        assert_eq!(other.color(), Some(DARK));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), other.wait_attributes())
                .await
                .is_err(),
            "a background answer is not the device attributes answer"
        );
    }

    #[tokio::test]
    async fn a_late_background_answer_wakes_the_wait_and_no_other() {
        // The reader records the answer, the loop's branch resolves, and the
        // branch of a terminal that was not asked never does: the loop holds no
        // timer and no wakeup for it.
        let answer = Background::default();
        let reader = answer.clone();
        tokio::join!(App::late_answer(Some(&answer)), async {
            reader.record(DARK)
        });
        assert_eq!(answer.color(), Some(DARK));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), App::late_answer(None))
                .await
                .is_err(),
            "a terminal that was not asked never wakes the loop"
        );
    }

    #[tokio::test]
    async fn herdr_first_frame_sends_the_kitty_query_before_csi_c() {
        // Inside herdr the kitty graphics query goes between the background
        // query and `CSI c`, so a terminal that answers in order answers it
        // before the wait the device attributes answer ends.
        let mut app = app();
        let answer = Background::default();
        let mut terminal = Answers::new(&answer, None, false);
        assert_eq!(app.ask(&mut terminal, Some(&answer), true).await, None);
        assert_eq!(terminal.writes, 1, "every query is one write");
        assert_eq!(
            terminal.bytes,
            b"\x1b]11;?\x07\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b]7501;?\x1b\\\x1b[c",
            "the kitty query goes out before CSI c"
        );
    }

    #[tokio::test]
    async fn herdr_graphics_answer_does_not_lengthen_the_first_frame() {
        // The kitty answer and the device attributes answer in one read: the
        // answer is recorded and the wait ends on `CSI c`, as fast as it does
        // for a terminal that answers no graphics query.
        let mut app = app();
        let answer = Background::default();
        let mut terminal = Answers {
            graphics: Some(true),
            ..Answers::new(&answer, None, true)
        };
        let started = std::time::Instant::now();
        let asked = tokio::time::timeout(
            Duration::from_millis(40),
            app.ask(&mut terminal, Some(&answer), true),
        );
        assert_eq!(
            asked
                .await
                .expect("the answer ends the wait, not the limit"),
            None
        );
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_millis(50),
            "the device attributes answer ended the wait at once: {waited:?}"
        );
        assert_eq!(answer.graphics(), Some(true), "the answer is recorded");
        assert_eq!(terminal.writes, 1, "every query is one write");
    }

    /// A terminal that answers the first frame's two queries the way the reader
    /// of a real one does: it records them into a [`Background`] handle and
    /// sends no key.
    #[derive(Default)]
    struct Answers {
        /// Where the answers go.
        answer: Background,
        /// The color it reports for `OSC 11`, `None` for a terminal that does not
        /// answer that query.
        color: Option<Rgb>,
        /// Whether it answers the device attributes query.
        attributes: bool,
        /// The answer it gives the kitty graphics query, `None` for a terminal
        /// that does not answer it.
        graphics: Option<bool>,
        /// The virtual milliseconds it takes to answer.
        after: u64,
        /// The writes it took.
        writes: usize,
        /// The bytes it was given.
        bytes: Vec<u8>,
    }

    impl Answers {
        /// A terminal that answers the queries at once, with `color` for
        /// `OSC 11` when there is one, and the device attributes when
        /// `attributes`.
        fn new(answer: &Background, color: Option<Rgb>, attributes: bool) -> Answers {
            Answers {
                answer: answer.clone(),
                color,
                attributes,
                ..Answers::default()
            }
        }
    }

    impl Terminal for Answers {
        fn size(&self) -> (u16, u16) {
            (COLUMNS, ROWS)
        }

        async fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
            self.writes += 1;
            self.bytes.extend_from_slice(frame);
            Ok(())
        }

        async fn next_event(&mut self) -> io::Result<Event> {
            if self.after > 0 {
                tokio::time::sleep(Duration::from_millis(self.after)).await;
            }
            if let Some(color) = self.color {
                self.answer.record(color);
            }
            if let Some(graphics) = self.graphics {
                self.answer.record_graphics(graphics);
            }
            if self.attributes {
                self.answer.record_attributes();
            }
            std::future::pending().await
        }
    }
}
