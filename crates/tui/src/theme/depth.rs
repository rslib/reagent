//! How many colors the terminal can take, and the nearest one it has.

use crate::screen::Color;

/// How many colors the terminal takes: the depth a mixed fill is mapped to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Depth {
    /// 24-bit color.
    #[default]
    TrueColor,
    /// The 256-color palette.
    Ansi256,
    /// The 16 terminal colors.
    Ansi16,
}

impl Depth {
    /// The depth two environment values ask for: `COLORTERM` first, then
    /// `TERM`.
    pub fn of(colorterm: &str, term: &str) -> Depth {
        if colorterm.eq_ignore_ascii_case("truecolor") || colorterm.eq_ignore_ascii_case("24bit") {
            Depth::TrueColor
        } else if term.contains("256") {
            Depth::Ansi256
        } else {
            Depth::Ansi16
        }
    }

    /// The depth the process environment asks for.
    pub fn from_env() -> Depth {
        Depth::of(
            &std::env::var("COLORTERM").unwrap_or_default(),
            &std::env::var("TERM").unwrap_or_default(),
        )
    }

    /// `color` as the terminal can take it: a mixed fill of a 256-color or a
    /// 16-color terminal becomes the nearest color the palette has.
    pub fn color(self, color: Color) -> Color {
        let Color::Rgb(r, g, b) = color else {
            return color;
        };
        match self {
            Depth::TrueColor => color,
            Depth::Ansi256 => Color::Indexed(nearest(&XTERM, r, g, b)),
            Depth::Ansi16 => Color::Indexed(nearest(&XTERM[..16], r, g, b)),
        }
    }
}

/// The index of the palette color nearest to `(r, g, b)`.
fn nearest(palette: &[[u8; 3]], r: u8, g: u8, b: u8) -> u8 {
    let mut best = 0u8;
    let mut best_distance = u32::MAX;
    for (index, color) in palette.iter().enumerate() {
        let distance = color
            .iter()
            .zip([r, g, b])
            .map(|(a, b)| u32::from(a.abs_diff(b)).pow(2))
            .sum();
        if distance < best_distance {
            best = index as u8;
            best_distance = distance;
        }
    }
    best
}

/// The xterm palette: the 16 terminal colors, then the 6×6×6 cube, then the
/// 24 grays. An RGB terminal answers with the 16 it draws, so a fill mapped to
/// one of them reads the same as the color the user's theme shows.
const XTERM: [[u8; 3]; 256] = xterm();

const fn xterm() -> [[u8; 3]; 256] {
    let base: [[u8; 3]; 16] = [
        [0, 0, 0],
        [128, 0, 0],
        [0, 128, 0],
        [128, 128, 0],
        [0, 0, 128],
        [128, 0, 128],
        [0, 128, 128],
        [192, 192, 192],
        [128, 128, 128],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [0, 0, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    let levels = [0, 95, 135, 175, 215, 255];
    let mut palette = [[0u8; 3]; 256];
    let mut index = 0;
    while index < 16 {
        palette[index] = base[index];
        index += 1;
    }
    let mut red = 0;
    while red < 6 {
        let mut green = 0;
        while green < 6 {
            let mut blue = 0;
            while blue < 6 {
                palette[index] = [levels[red], levels[green], levels[blue]];
                index += 1;
                blue += 1;
            }
            green += 1;
        }
        red += 1;
    }
    let mut gray = 0;
    while gray < 24 {
        let value = 8 + 10 * gray as u8;
        palette[index] = [value, value, value];
        index += 1;
        gray += 1;
    }
    palette
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorterm_truecolor_is_true_color() {
        assert_eq!(Depth::of("truecolor", "xterm"), Depth::TrueColor);
        assert_eq!(Depth::of("24bit", "xterm-256color"), Depth::TrueColor);
    }

    #[test]
    fn term_256color_is_ansi_256() {
        assert_eq!(Depth::of("", "xterm-256color"), Depth::Ansi256);
        assert_eq!(Depth::of("", "screen-256color"), Depth::Ansi256);
    }

    #[test]
    fn term_xterm_and_an_empty_environment_are_ansi_16() {
        assert_eq!(Depth::of("", "xterm"), Depth::Ansi16);
        assert_eq!(Depth::of("", "dumb"), Depth::Ansi16);
        assert_eq!(Depth::of("", ""), Depth::Ansi16);
    }

    #[test]
    fn nearest_256_maps_a_fill_to_its_gray() {
        assert_eq!(
            Depth::Ansi256.color(Color::Rgb(59, 59, 59)),
            Color::Indexed(237)
        );
        assert_eq!(
            Depth::Ansi256.color(Color::Rgb(255, 255, 255)),
            Color::Indexed(15)
        );
    }

    #[test]
    fn nearest_16_maps_to_the_closest_terminal_color() {
        assert_eq!(
            Depth::Ansi16.color(Color::Rgb(255, 255, 255)),
            Color::Indexed(15)
        );
        assert_eq!(
            Depth::Ansi16.color(Color::Rgb(128, 128, 128)),
            Color::Indexed(8)
        );
        assert_eq!(Depth::Ansi16.color(Color::Rgb(0, 0, 0)), Color::Indexed(0));
    }

    #[test]
    fn true_color_keeps_a_color_it_is_given() {
        assert_eq!(
            Depth::TrueColor.color(Color::Rgb(1, 2, 3)),
            Color::Rgb(1, 2, 3)
        );
        assert_eq!(Depth::TrueColor.color(Color::Indexed(6)), Color::Indexed(6));
    }
}
