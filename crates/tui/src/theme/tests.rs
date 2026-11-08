use crate::screen::{Color, StyleTable};

use super::*;

/// The dark background most terminals answer with.
fn dark() -> Mode {
    Mode::Rich {
        background: Rgb::new(30, 30, 30),
        dark: true,
    }
}

/// A white background.
fn light() -> Mode {
    Mode::Rich {
        background: Rgb::WHITE,
        dark: false,
    }
}

fn theme(mode: Mode) -> Theme {
    Theme::resolve(mode, Depth::TrueColor, &Overrides::default())
}

/// The sum of the channel gaps between two fills.
fn gap(a: Color, b: Color) -> u32 {
    let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) else {
        panic!("two rich fills: {a:?} and {b:?}");
    };
    u32::from(ar.abs_diff(br)) + u32::from(ag.abs_diff(bg)) + u32::from(ab.abs_diff(bb))
}

/// The picked row stands clear of both fills it sits on, in both modes.
#[test]
fn picked_fill_stands_clear_of_the_lists_and_sheets() {
    for mode in [dark(), light()] {
        let theme = theme(mode);
        let picked = theme.fill(Role::PickedFill);
        for around in [Role::InputFill, Role::SurfaceFill] {
            let gap = gap(picked, theme.fill(around));
            assert!(gap >= 40, "{gap} from {around:?} in {mode:?}: {picked:?}");
        }
        assert_ne!(picked, theme.fill(Role::CardAccent), "a card keeps its own");
    }
}

#[test]
fn dark_fills_mix_toward_white() {
    let theme = theme(dark());
    assert_eq!(theme.fill(Role::InputFill), Color::Rgb(59, 59, 59));
    assert_eq!(theme.fill(Role::PanelFill), Color::Rgb(41, 41, 41));
    assert_eq!(theme.fill(Role::SurfaceFill), Color::Rgb(50, 50, 50));
    assert_eq!(theme.fill(Role::CodeFill), Color::Rgb(41, 41, 41));
    assert_eq!(theme.fill(Role::CardAccent), Color::Rgb(32, 37, 37));
    assert_eq!(
        theme.fill(Role::ErrFill),
        Color::Rgb(55, 36, 36),
        "the failed card's fill is the delete tint at the stronger mix"
    );
    assert_eq!(theme.fill(Role::Text), Color::Default);
}

#[test]
fn light_fills_mix_toward_black() {
    let theme = theme(light());
    assert_eq!(theme.fill(Role::InputFill), Color::Rgb(235, 235, 235));
    assert_eq!(theme.fill(Role::PanelFill), Color::Rgb(247, 247, 247));
    assert_eq!(theme.fill(Role::SurfaceFill), Color::Rgb(242, 242, 242));
    assert_eq!(theme.fill(Role::CodeFill), Color::Rgb(249, 249, 249));
    assert_eq!(theme.fill(Role::CardAccent), Color::Rgb(242, 248, 248));
    assert_eq!(
        theme.fill(Role::ErrFill),
        Color::Rgb(250, 235, 235),
        "the failed card's fill is the delete tint at the stronger mix"
    );
}

#[test]
fn the_accent_is_reagent_teal_per_background() {
    assert_eq!(
        theme(dark()).style(Role::Accent).fg,
        Color::Rgb(0x4f, 0xc1, 0xc9)
    );
    assert_eq!(
        theme(light()).style(Role::Accent).fg,
        Color::Rgb(0x00, 0x74, 0x7c)
    );
}

#[test]
fn terminal_mode_has_no_fill_but_the_selection() {
    let theme = theme(Mode::Terminal);
    for role in Role::ALL.into_iter().filter(|role| role.is_fill()) {
        assert_eq!(theme.fill(role), Color::Default, "{role:?}");
    }
    assert_eq!(theme.style(Role::Accent).fg, Color::Indexed(6));
    assert_eq!(theme.style(Role::Add).fg, Color::Indexed(2));
    assert_eq!(theme.style(Role::Delete).fg, Color::Indexed(1));
    assert_eq!(theme.style(Role::Warn).fg, Color::Indexed(3));
    assert_eq!(theme.style(Role::Text).fg, Color::Default);
    assert!(theme.style(Role::Selection).reverse);
}

#[test]
fn no_color_drops_every_color_and_keeps_the_weight() {
    let theme = theme(Mode::NoColor);
    for role in Role::ALL {
        let style = theme.style(role);
        assert_eq!(style.fg, Color::Default, "{role:?}");
        assert_eq!(style.bg, Color::Default, "{role:?}");
    }
    assert!(theme.styled(Role::Text, Emphasis::Bold).bold);
    assert!(theme.styled(Role::Text, Emphasis::Dim).dim);
    assert!(theme.styled(Role::Text, Emphasis::Italic).italic);
    assert!(theme.styled(Role::Text, Emphasis::DimItalic).italic);
    assert!(theme.styled(Role::Text, Emphasis::DimItalic).dim);
    assert!(theme.style(Role::Selection).reverse);
    assert!(!theme.styled(Role::Text, Emphasis::Bold).reverse);
}

#[test]
fn a_deeper_mode_maps_every_fill_and_color_to_the_depth() {
    let theme = Theme::resolve(dark(), Depth::Ansi256, &Overrides::default());
    assert_eq!(theme.fill(Role::InputFill), Color::Indexed(237));
    let theme = Theme::resolve(dark(), Depth::Ansi16, &Overrides::default());
    assert_eq!(theme.fill(Role::InputFill), Color::Indexed(0));
    assert_ne!(theme.style(Role::Accent).fg, Color::Rgb(0x4f, 0xc1, 0xc9));
}

#[test]
fn an_override_wins_over_the_mode() {
    let overrides = Overrides::parse("[roles]\naccent = \"#ff0000\"\nwarn = \"none\"\n").unwrap();
    let theme = Theme::resolve(dark(), Depth::TrueColor, &overrides);
    assert_eq!(theme.style(Role::Accent).fg, Color::Rgb(255, 0, 0));
    assert_eq!(theme.style(Role::Warn).fg, Color::Default);
    assert_eq!(
        Theme::resolve(Mode::Terminal, Depth::TrueColor, &overrides)
            .style(Role::Accent)
            .fg,
        Color::Rgb(255, 0, 0)
    );
    assert_eq!(
        Theme::resolve(Mode::NoColor, Depth::TrueColor, &overrides)
            .style(Role::Accent)
            .fg,
        Color::Default
    );
}

#[test]
fn a_fill_override_sets_the_background() {
    let overrides = Overrides::parse("[roles]\ninput-fill = \"#101010\"\n").unwrap();
    let theme = Theme::resolve(dark(), Depth::Ansi256, &overrides);
    assert_eq!(theme.fill(Role::InputFill), Color::Indexed(233));
}

#[test]
fn the_code_roles_take_the_mockups_dark_hues() {
    let theme = theme(dark());
    let hues = [
        (Role::CodeKeyword, Rgb::new(0xc7, 0x92, 0xea)),
        (Role::CodeString, Rgb::new(0xa5, 0xd6, 0xa7)),
        (Role::CodeComment, Rgb::new(0x7f, 0x8c, 0x8d)),
        (Role::CodeNumber, Rgb::new(0xf7, 0xa7, 0x6c)),
        (Role::CodeType, Rgb::new(0x82, 0xc8, 0xe5)),
        (Role::CodeFunction, Rgb::new(0x82, 0xaa, 0xff)),
        (Role::CodeMacro, Rgb::new(0xff, 0xd4, 0x79)),
    ];
    for (role, hue) in hues {
        assert_eq!(theme.style(role).fg, hue.color(), "{role:?}");
    }
    assert!(theme.style(Role::CodeComment).italic);
    assert!(!theme.style(Role::CodeKeyword).italic);
}

#[test]
fn the_code_roles_take_the_mockups_light_hues() {
    let theme = theme(light());
    let hues = [
        (Role::CodeKeyword, Rgb::new(0x8a, 0x3f, 0xb8)),
        (Role::CodeString, Rgb::new(0x2e, 0x7d, 0x32)),
        (Role::CodeComment, Rgb::new(0x7a, 0x7f, 0x85)),
        (Role::CodeNumber, Rgb::new(0xb4, 0x53, 0x09)),
        (Role::CodeType, Rgb::new(0x00, 0x72, 0x7c)),
        (Role::CodeFunction, Rgb::new(0x1d, 0x4e, 0xd8)),
        (Role::CodeMacro, Rgb::new(0x8a, 0x6d, 0x00)),
    ];
    for (role, hue) in hues {
        assert_eq!(theme.style(role).fg, hue.color(), "{role:?}");
    }
    assert!(theme.style(Role::CodeComment).italic);
}

#[test]
fn punctuation_is_the_foreground_most_of_the_way_on_the_background() {
    // The mockup mixes the foreground 65% with the background, so the
    // punctuation stays a step quieter than the text on either side.
    assert_eq!(
        theme(dark()).style(Role::CodePunct).fg,
        Rgb::WHITE.mix(Rgb::new(30, 30, 30), 35).color()
    );
    assert_eq!(
        theme(light()).style(Role::CodePunct).fg,
        Rgb::BLACK.mix(Rgb::WHITE, 35).color()
    );
    assert!(!theme(dark()).style(Role::CodePunct).dim);
}

#[test]
fn a_heading_and_a_quote_are_the_accent() {
    for mode in [dark(), light()] {
        let theme = theme(mode);
        assert_eq!(
            theme.style(Role::Heading).fg,
            theme.style(Role::Accent).fg,
            "{mode:?}"
        );
        assert_eq!(theme.style(Role::QuoteBar).fg, theme.style(Role::Accent).fg);
        assert!(theme.style(Role::Heading).bold, "{mode:?}");
    }
}

#[test]
fn terminal_mode_gives_the_code_roles_the_16_colors() {
    let theme = theme(Mode::Terminal);
    assert_eq!(theme.style(Role::CodeKeyword).fg, Color::Indexed(5));
    assert_eq!(theme.style(Role::CodeString).fg, Color::Indexed(2));
    assert_eq!(theme.style(Role::CodeComment).fg, Color::Indexed(8));
    assert!(theme.style(Role::CodeComment).italic);
    assert_eq!(theme.style(Role::CodeNumber).fg, Color::Indexed(3));
    assert_eq!(theme.style(Role::CodeType).fg, Color::Indexed(6));
    assert_eq!(theme.style(Role::CodeFunction).fg, Color::Indexed(4));
    assert_eq!(theme.style(Role::CodeMacro).fg, Color::Indexed(3));
    assert_eq!(theme.style(Role::CodePunct).fg, Color::Default);
    assert!(theme.style(Role::CodePunct).dim);
    assert_eq!(theme.style(Role::Heading).fg, Color::Indexed(6));
    assert!(theme.style(Role::Heading).bold);
    assert_eq!(theme.style(Role::QuoteBar).fg, Color::Indexed(6));
}

#[test]
fn no_color_keeps_the_italic_comment_and_nothing_else() {
    let theme = theme(Mode::NoColor);
    for role in [
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
    ] {
        assert_eq!(theme.style(role).fg, Color::Default, "{role:?}");
        assert_eq!(theme.style(role).bg, Color::Default, "{role:?}");
    }
    assert!(theme.style(Role::CodeComment).italic);
    assert!(!theme.style(Role::CodeKeyword).italic);
    assert!(!theme.style(Role::CodePunct).dim);
    assert!(theme.style(Role::Heading).bold);
}

#[test]
fn a_theme_file_moves_a_code_role() {
    let overrides = Overrides::parse(
        "[roles]\ncode.keyword = \"#ff0000\"\nheading = \"#00ff00\"\nquote-bar = \"none\"\n",
    )
    .expect("parses");
    let theme = Theme::resolve(dark(), Depth::TrueColor, &overrides);
    assert_eq!(theme.style(Role::CodeKeyword).fg, Color::Rgb(255, 0, 0));
    assert_eq!(theme.style(Role::Heading).fg, Color::Rgb(0, 255, 0));
    assert!(
        theme.style(Role::Heading).bold,
        "the override keeps the weight"
    );
    assert_eq!(theme.style(Role::QuoteBar).fg, Color::Default);
    assert_eq!(
        Theme::resolve(Mode::Terminal, Depth::TrueColor, &overrides)
            .style(Role::CodeKeyword)
            .fg,
        Color::Rgb(255, 0, 0)
    );
}

#[test]
fn a_code_role_reads_its_dotted_and_its_hyphenated_name() {
    assert_eq!(Role::from_name("code.keyword"), Some(Role::CodeKeyword));
    assert_eq!(Role::from_name("code-keyword"), Some(Role::CodeKeyword));
    assert_eq!(Role::from_name("code.punct"), Some(Role::CodePunct));
    assert_eq!(Role::from_name("heading"), Some(Role::Heading));
    assert_eq!(Role::from_name("quote.bar"), Some(Role::QuoteBar));
    assert_eq!(
        Role::CodeKeyword.name(),
        Role::ALL[Role::CodeKeyword.index()].name()
    );
    assert_eq!(Role::from_name("code.none"), None);
}

#[test]
fn styles_intern_once_per_role_and_emphasis() {
    let theme = theme(dark());
    let mut table = StyleTable::new();
    let ids = theme.intern(&mut table);
    let len = table.len();
    assert!(len > 1 && len <= Role::ALL.len() * Emphasis::ALL.len() + 1);
    assert_eq!(
        ids.id(Role::Accent, Emphasis::Bold),
        table.intern(theme.styled(Role::Accent, Emphasis::Bold))
    );
    assert_eq!(table.len(), len);
    theme.intern(&mut table);
    assert_eq!(table.len(), len);
    assert_ne!(
        ids.id(Role::Text, Emphasis::Plain),
        ids.id(Role::Text, Emphasis::Bold)
    );
    assert_eq!(
        ids.id(Role::Text, Emphasis::Bold),
        ids.id(Role::Text, Emphasis::Bold)
    );
}
