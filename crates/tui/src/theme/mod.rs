//! The colors the terminal UI draws with: named roles, resolved once.
//!
//! Drawing code never names a color. It names a [`Role`], and the app resolves
//! every role a single time, at start and again when the background or the
//! theme file changes, into one [`Style`] and one interned style id per role
//! ([`Theme::intern`] returns [`Ids`]). A frame then reads an array index and
//! does no color math and no style lookup per cell.
//!
//! Three modes:
//!
//! - [`Mode::Rich`]: fills mixed from the background the terminal reported
//!   (OSC 11), toward white on a dark background and toward black on a light
//!   one, as the approved mockup's `color-mix` rules do.
//! - [`Mode::Terminal`]: the 16 terminal colors, no fills but the selection,
//!   so the user's own theme and a transparent background show through.
//! - [`Mode::NoColor`]: no color at all; bold, dim, italic, underline and
//!   reverse stay, so every state keeps its second cue.
//!
//! A mixed fill is mapped to one color of the terminal's [`Depth`], and
//! [`Overrides`] lets a TOML file move any role.

mod depth;
mod flags;
mod overrides;

pub use depth::Depth;
pub use flags::Flags;
pub use overrides::{Error, Overrides};

use crate::screen::{Color, Style, StyleTable};

/// One color the UI draws with. A role is a meaning, never a raw color, so a
/// theme file and a background change move every use of it at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Role {
    /// Body text and the terminal's own foreground.
    Text,
    /// Quiet text: counts, hints, paths.
    Dim,
    /// reagent's own hue: the `›` of a user turn, the text box bar.
    Accent,
    /// Waiting and warnings.
    Warn,
    /// An error chip.
    Error,
    /// Something added.
    Add,
    /// Something removed.
    Delete,
    /// The fill of the text box.
    InputFill,
    /// The fill of the sidebar, the pending row and the header row.
    PanelFill,
    /// A neutral surface under a block.
    SurfaceFill,
    /// The fill behind inline code and a command.
    CodeFill,
    /// The tint under a tool block that finished well.
    OkFill,
    /// The tint under a tool block that is still running.
    RunFill,
    /// The tint under a tool block that failed.
    ErrFill,
    /// The tint of the approval card.
    CardWarn,
    /// The tint of the question card.
    CardAccent,
    /// A reversed selection.
    Selection,
    /// A rule or the `│` beside the sidebar.
    Line,
    /// A keyword or a reserved word of code.
    CodeKeyword,
    /// A string or a character literal of code.
    CodeString,
    /// A comment of code, drawn italic.
    CodeComment,
    /// A number of code.
    CodeNumber,
    /// A type of code.
    CodeType,
    /// A call or a command of code.
    CodeFunction,
    /// A macro invocation of code.
    CodeMacro,
    /// The punctuation of code: the braces, the operators, the commas.
    CodePunct,
    /// A markdown heading, drawn bold.
    Heading,
    /// The `▎` of a quoted line.
    QuoteBar,
}

impl Role {
    /// Every role, in the order [`Ids`] holds them.
    pub const ALL: [Role; 28] = [
        Role::Text,
        Role::Dim,
        Role::Accent,
        Role::Warn,
        Role::Error,
        Role::Add,
        Role::Delete,
        Role::InputFill,
        Role::PanelFill,
        Role::SurfaceFill,
        Role::CodeFill,
        Role::OkFill,
        Role::RunFill,
        Role::ErrFill,
        Role::CardWarn,
        Role::CardAccent,
        Role::Selection,
        Role::Line,
        Role::CodeKeyword,
        Role::CodeString,
        Role::CodeComment,
        Role::CodeNumber,
        Role::CodeType,
        Role::CodeFunction,
        Role::CodeMacro,
        Role::CodePunct,
        Role::Heading,
        Role::QuoteBar,
    ];

    /// The roles a code token takes, in the order [`CODE_DARK`] and
    /// [`CODE_LIGHT`] hold their hues.
    const CODE: [Role; 7] = [
        Role::CodeKeyword,
        Role::CodeString,
        Role::CodeComment,
        Role::CodeNumber,
        Role::CodeType,
        Role::CodeFunction,
        Role::CodeMacro,
    ];

    /// The index of the role in [`Role::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The name a theme file uses.
    pub const fn name(self) -> &'static str {
        match self {
            Role::Text => "text",
            Role::Dim => "dim",
            Role::Accent => "accent",
            Role::Warn => "warn",
            Role::Error => "error",
            Role::Add => "add",
            Role::Delete => "delete",
            Role::InputFill => "input-fill",
            Role::PanelFill => "panel-fill",
            Role::SurfaceFill => "surface-fill",
            Role::CodeFill => "code-fill",
            Role::OkFill => "ok-fill",
            Role::RunFill => "run-fill",
            Role::ErrFill => "err-fill",
            Role::CardWarn => "card-warn",
            Role::CardAccent => "card-accent",
            Role::Selection => "selection",
            Role::Line => "line",
            Role::CodeKeyword => "code-keyword",
            Role::CodeString => "code-string",
            Role::CodeComment => "code-comment",
            Role::CodeNumber => "code-number",
            Role::CodeType => "code-type",
            Role::CodeFunction => "code-function",
            Role::CodeMacro => "code-macro",
            Role::CodePunct => "code-punct",
            Role::Heading => "heading",
            Role::QuoteBar => "quote-bar",
        }
    }

    /// The role a theme file names, `None` for a name no role has. A `.` stands
    /// for the `-`, so `code.keyword` and `code-keyword` name one role.
    pub fn from_name(name: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|role| {
            let own = role.name().as_bytes();
            name.len() == own.len()
                && name
                    .bytes()
                    .zip(own)
                    .all(|(given, ours)| given == *ours || (given == b'.' && *ours == b'-'))
        })
    }

    /// True for a role whose color is a background fill.
    pub const fn is_fill(self) -> bool {
        matches!(
            self,
            Role::InputFill
                | Role::PanelFill
                | Role::SurfaceFill
                | Role::CodeFill
                | Role::OkFill
                | Role::RunFill
                | Role::ErrFill
                | Role::CardWarn
                | Role::CardAccent
        )
    }

    /// True for a role a theme file colors as a background: the fills and the
    /// selection, whose own color is the terminal's inverse.
    pub const fn is_background(self) -> bool {
        self.is_fill() || matches!(self, Role::Selection)
    }
}

/// Extra weight on one role's style. A state cue that only a color carries is
/// lost under `NO_COLOR` and on a 16-color terminal, so the bold, dim, italic
/// and reverse variants are named here and resolve to interned styles too.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Emphasis {
    /// The role as it is.
    #[default]
    Plain,
    /// Bold: a tool name, a heading, a user turn.
    Bold,
    /// Dim: a hint, a count.
    Dim,
    /// Italic: a quoted line or an annotation.
    Italic,
    /// Dim italic: the folded thinking line, as the mockup draws it.
    DimItalic,
}

impl Emphasis {
    /// Every emphasis, in the order [`Ids`] holds them.
    pub const ALL: [Emphasis; 5] = [
        Emphasis::Plain,
        Emphasis::Bold,
        Emphasis::Dim,
        Emphasis::Italic,
        Emphasis::DimItalic,
    ];

    /// The index of the emphasis in [`Emphasis::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Add the emphasis to a style.
    fn apply(self, style: &mut Style) {
        match self {
            Emphasis::Plain => {}
            Emphasis::Bold => style.bold = true,
            Emphasis::Dim => style.dim = true,
            Emphasis::Italic => style.italic = true,
            Emphasis::DimItalic => {
                style.dim = true;
                style.italic = true;
            }
        }
    }
}

/// A color written as its three channels, for the background a terminal
/// reports and for the fills mixed from it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rgb {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
}

impl Rgb {
    /// White.
    pub const WHITE: Rgb = Rgb::new(255, 255, 255);
    /// Black.
    pub const BLACK: Rgb = Rgb::new(0, 0, 0);

    /// A color from its three channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// `#rrggbb` or `#rgb`; `None` for anything else.
    pub fn parse(text: &str) -> Option<Rgb> {
        let hex = text.strip_prefix('#')?;
        if !hex.is_ascii() {
            return None;
        }
        let byte = |pair: &str| u8::from_str_radix(pair, 16).ok();
        match hex.len() {
            6 => Some(Rgb::new(
                byte(&hex[0..2])?,
                byte(&hex[2..4])?,
                byte(&hex[4..6])?,
            )),
            3 => {
                let mut digits = hex
                    .chars()
                    .map(|digit| digit.to_digit(16).map(|v| v as u8 * 17));
                Some(Rgb::new(digits.next()??, digits.next()??, digits.next()??))
            }
            _ => None,
        }
    }

    /// The color as a terminal color.
    pub const fn color(self) -> Color {
        Color::Rgb(self.r, self.g, self.b)
    }

    /// `self` moved `percent` of the way to `other`, channel by channel, as
    /// the mockup's `color-mix` does.
    pub fn mix(self, other: Rgb, percent: u8) -> Rgb {
        let percent = u32::from(if percent > 100 { 100 } else { percent });
        let channel = |from: u8, to: u8| {
            ((u32::from(from) * (100 - percent) + u32::from(to) * percent + 50) / 100) as u8
        };
        Rgb::new(
            channel(self.r, other.r),
            channel(self.g, other.g),
            channel(self.b, other.b),
        )
    }
}

/// How the UI colors itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Fills mixed from the background the terminal answered with.
    Rich {
        /// The background color the terminal reported.
        background: Rgb,
        /// True when the background is dark, which picks the direction of the
        /// fills and the accent hue.
        dark: bool,
    },
    /// The 16 terminal colors, no fills but the selection.
    Terminal,
    /// No color at all; bold, dim, italic, underline and reverse stay.
    NoColor,
}

/// The accent on a dark background.
const ACCENT_DARK: Rgb = Rgb::new(0x4f, 0xc1, 0xc9);
/// The accent on a light background.
const ACCENT_LIGHT: Rgb = Rgb::new(0x00, 0x74, 0x7c);
/// The warning hue the card tints and the waiting glyphs mix from.
const WARN: Rgb = Rgb::new(0xd7, 0xa1, 0x3b);
/// The error hue of a chip.
const ERROR: Rgb = Rgb::new(0xe0, 0x52, 0x52);
/// The added-lines hue.
const ADD: Rgb = Rgb::new(0x3f, 0xa6, 0x6b);
/// The removed-lines hue.
const DELETE: Rgb = Rgb::new(0xd2, 0x4b, 0x4b);
/// The code hues on a dark background, for the roles of [`Role::CODE`].
const CODE_DARK: [Rgb; 7] = [
    Rgb::new(0xc7, 0x92, 0xea),
    Rgb::new(0xa5, 0xd6, 0xa7),
    Rgb::new(0x7f, 0x8c, 0x8d),
    Rgb::new(0xf7, 0xa7, 0x6c),
    Rgb::new(0x82, 0xc8, 0xe5),
    Rgb::new(0x82, 0xaa, 0xff),
    Rgb::new(0xff, 0xd4, 0x79),
];
/// The code hues on a light background, for the roles of [`Role::CODE`].
const CODE_LIGHT: [Rgb; 7] = [
    Rgb::new(0x8a, 0x3f, 0xb8),
    Rgb::new(0x2e, 0x7d, 0x32),
    Rgb::new(0x7a, 0x7f, 0x85),
    Rgb::new(0xb4, 0x53, 0x09),
    Rgb::new(0x00, 0x72, 0x7c),
    Rgb::new(0x1d, 0x4e, 0xd8),
    Rgb::new(0x8a, 0x6d, 0x00),
];

/// One [`Style`] per role, resolved once for a mode, a depth and a theme file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Theme {
    mode: Mode,
    depth: Depth,
    styles: [Style; Role::ALL.len()],
}

impl Theme {
    /// Resolve every role once. No color math happens after this.
    pub fn resolve(mode: Mode, depth: Depth, overrides: &Overrides) -> Theme {
        let mut theme = Theme {
            mode,
            depth,
            styles: std::array::from_fn(|_| Style::default()),
        };
        theme.build();
        if mode != Mode::NoColor {
            theme.overridden(overrides);
        }
        theme
    }

    /// Build every role from the mode and the depth.
    fn build(&mut self) {
        match self.mode {
            Mode::Rich { background, dark } => self.rich(background, dark),
            Mode::Terminal => self.terminal(),
            Mode::NoColor => {}
        }
        self.styles[Role::Dim.index()].dim = true;
        self.styles[Role::CodeComment.index()].italic = true;
        self.styles[Role::Heading.index()].bold = true;
        self.styles[Role::Selection.index()].reverse = true;
    }

    /// Rich mode: the accent and the state hues in true color, and every fill
    /// mixed from the background.
    fn rich(&mut self, background: Rgb, dark: bool) {
        let accent = if dark { ACCENT_DARK } else { ACCENT_LIGHT };
        let toward = if dark { Rgb::WHITE } else { Rgb::BLACK };
        let line = if dark { 30 } else { 25 };
        self.fg(Role::Accent, accent);
        self.fg(Role::Heading, accent);
        self.fg(Role::QuoteBar, accent);
        self.fg(Role::Warn, WARN);
        self.fg(Role::Error, ERROR);
        self.fg(Role::Add, ADD);
        self.fg(Role::Delete, DELETE);
        self.fg(Role::Line, background.mix(toward, line));
        let hues = if dark { CODE_DARK } else { CODE_LIGHT };
        for (role, hue) in Role::CODE.into_iter().zip(hues) {
            self.fg(role, hue);
        }
        // The mockup mixes the foreground 65% with the background, which leaves
        // the punctuation a step quieter than the text around it.
        let fg = if dark { Rgb::WHITE } else { Rgb::BLACK };
        self.fg(Role::CodePunct, fg.mix(background, 35));
        let (input, panel, surface, code, card, muted) = if dark {
            (13, 5, 9, 30, 4, 9)
        } else {
            (8, 3, 5, 70, 5, 7)
        };
        // The code fill leaves the input fill for the background's own side:
        // darker than the input on a dark terminal, lighter on a light one.
        let code_toward = if dark { Rgb::BLACK } else { Rgb::WHITE };
        let input = background.mix(toward, input);
        self.bg(Role::InputFill, input);
        self.bg(Role::PanelFill, background.mix(toward, panel));
        self.bg(Role::SurfaceFill, background.mix(toward, surface));
        self.bg(Role::CodeFill, input.mix(code_toward, code));
        self.bg(Role::OkFill, background.mix(ADD, muted / 2));
        self.bg(Role::RunFill, background.mix(WARN, muted));
        self.bg(Role::ErrFill, background.mix(DELETE, muted * 16 / 10));
        self.bg(Role::CardWarn, background.mix(WARN, card));
        self.bg(Role::CardAccent, background.mix(accent, card));
    }

    /// Terminal mode: the 16 colors and no fill but the selection.
    fn terminal(&mut self) {
        self.styles[Role::Accent.index()].fg = Color::Indexed(6);
        self.styles[Role::Warn.index()].fg = Color::Indexed(3);
        self.styles[Role::Error.index()].fg = Color::Indexed(9);
        self.styles[Role::Add.index()].fg = Color::Indexed(2);
        self.styles[Role::Delete.index()].fg = Color::Indexed(1);
        self.styles[Role::Line.index()].fg = Color::Indexed(8);
        self.styles[Role::CodeKeyword.index()].fg = Color::Indexed(5);
        self.styles[Role::CodeString.index()].fg = Color::Indexed(2);
        self.styles[Role::CodeComment.index()].fg = Color::Indexed(8);
        self.styles[Role::CodeNumber.index()].fg = Color::Indexed(3);
        self.styles[Role::CodeType.index()].fg = Color::Indexed(6);
        self.styles[Role::CodeFunction.index()].fg = Color::Indexed(4);
        self.styles[Role::CodeMacro.index()].fg = Color::Indexed(3);
        self.styles[Role::CodePunct.index()].dim = true;
        self.styles[Role::Heading.index()].fg = Color::Indexed(6);
        self.styles[Role::QuoteBar.index()].fg = Color::Indexed(6);
    }

    /// A theme file's colors, after the mode has had its say.
    fn overridden(&mut self, overrides: &Overrides) {
        for role in Role::ALL {
            let Some(override_color) = overrides.get(role) else {
                continue;
            };
            let color = match override_color {
                Some(rgb) => self.depth.color(rgb.color()),
                None => Color::Default,
            };
            let style = &mut self.styles[role.index()];
            if role.is_background() {
                style.bg = color;
            } else {
                style.fg = color;
            }
        }
    }

    /// Set a role's foreground to a color the depth can take.
    fn fg(&mut self, role: Role, rgb: Rgb) {
        self.styles[role.index()].fg = self.depth.color(rgb.color());
    }

    /// Set a role's background to a color the depth can take.
    fn bg(&mut self, role: Role, rgb: Rgb) {
        self.styles[role.index()].bg = self.depth.color(rgb.color());
    }

    /// The style of a role.
    pub fn style(&self, role: Role) -> Style {
        self.styles[role.index()]
    }

    /// The style of a role with an emphasis on it.
    pub fn styled(&self, role: Role, emphasis: Emphasis) -> Style {
        let mut style = self.styles[role.index()];
        emphasis.apply(&mut style);
        style
    }

    /// The background color of a fill role, for drawing text over it.
    pub fn fill(&self, role: Role) -> Color {
        self.styles[role.index()].bg
    }

    /// The mode every style was built for.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The style id of every role and emphasis, interned into `table` once.
    pub fn intern(&self, table: &mut StyleTable) -> Ids {
        let mut ids = [[0; Emphasis::ALL.len()]; Role::ALL.len()];
        for role in Role::ALL {
            for emphasis in Emphasis::ALL {
                ids[role.index()][emphasis.index()] = table.intern(self.styled(role, emphasis));
            }
        }
        Ids { ids }
    }
}

/// The interned style id of every role and emphasis, so drawing a cell needs
/// an array index and never builds a style.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    ids: [[u32; Emphasis::ALL.len()]; Role::ALL.len()],
}

impl Ids {
    /// The id of a role with an emphasis on it.
    pub fn id(&self, role: Role, emphasis: Emphasis) -> u32 {
        self.ids[role.index()][emphasis.index()]
    }
}

impl std::fmt::Debug for Ids {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ids")
            .field("roles", &Role::ALL.len())
            .field("emphases", &Emphasis::ALL.len())
            .finish()
    }
}

#[cfg(test)]
mod tests;
