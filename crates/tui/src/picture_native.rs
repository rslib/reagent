//! The terminals' own image protocols and their escape sequences.
//!
//! A picture goes out as one iTerm2 `1337` file escape over its half-block
//! cells, or as one kitty image that its rows of Unicode placeholder cells
//! draw. Everything here appends to a `Vec<u8>` or a `String`: the caller owns
//! the write to the terminal, so these sequences can sit in one frame with the
//! rest of it.

use std::io::Write as _;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use reagent_image::{Limits, apply, decode, resize};
use tokio::process::Command;

/// The line [`tmux_display`] asks tmux to print: the terminal tmux draws into,
/// and whether it passes an escape through to that terminal.
pub const TMUX_DISPLAY: &str = "#{client_termname}|#{client_termtype}|#{allow-passthrough}";

/// How long a terminal that reported a focus-in, or a pane that changed size,
/// waits before its images go out: the terminal draws its window again after it
/// hands the report on, and that draw erases an image on it.
pub const SETTLE: Duration = Duration::from_millis(100);

/// How long a picture that moved waits before its image goes out. Steps of a
/// scroll often come more than [`SETTLE`] apart, and an image drawn between two
/// of them flickers on and off; this keeps the cells until the scroll stops.
pub const MOVE_SETTLE: Duration = Duration::from_millis(300);

/// Most base64 characters one chunk of a transmission carries. The number is a
/// multiple of four, so no chunk splits a base64 group: the terminal decodes
/// every chunk on its own.
const CHUNK: usize = 4096;

/// The longest side of the PNG a transmission sends, in pixels.
const MAX_PIXELS: u32 = 1600;

/// The diacritics that name a placeholder cell's row and column, in order:
/// the first 64 entries of kitty's `gen/rowcolumn-diacritics.txt`
/// (https://github.com/kovidgoyal/kitty, GPL-3.0; a list of Unicode code
/// points, the data of the kitty graphics protocol's Unicode placeholders).
pub const PLACEHOLDER_MARKS: [char; 64] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
    '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
];

/// A variable each multiplexer sets in its panes: zellij, GNU screen and
/// rmaestro. They draw panes with their own terminal emulator and pass no
/// tested image protocol, and zellij and rmaestro keep the outer
/// `TERM_PROGRAM`, so they must be ruled out before it is read.
///
/// herdr sets `HERDR_PANE_ID` in its panes as well, and hides the terminal it
/// draws them into, so its environment says nothing about the image protocol:
/// it is asked with the kitty query instead, and answers no image only when
/// that answer says so (see [`asks_graphics`]).
const UNTESTED_HOSTS: [&str; 4] = ["ZELLIJ", "STY", "RMAESTRO_PANE", "ZELLIJ_SESSION_NAME"];

/// What the terminal tmux draws into calls itself, in `#{client_termname}` or
/// `#{client_termtype}`: the terminals that read kitty's placeholders.
const PLACEHOLDER_TERMINALS: [&str; 6] = [
    "rio",
    "Rio ",
    "xterm-kitty",
    "kitty",
    "xterm-ghostty",
    "ghostty",
];

/// The image protocol a terminal speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// iTerm2's `1337` file escape, which WezTerm also reads.
    Iterm2,
    /// Kitty's graphics protocol with Unicode placeholders, which Rio, kitty
    /// and Ghostty also read, and tmux passes on.
    Placeholders,
}

/// The protocol of one terminal, and whether tmux sits in front of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Native {
    /// The protocol to write.
    pub protocol: Protocol,
    /// Whether tmux fronts the terminal, which passes each sequence on wrapped
    /// on its own.
    pub tmux: bool,
}

/// Whether the terminal behind `env` is a herdr pane that must be asked the
/// kitty graphics query (`a=q`) before any image goes out.
///
/// herdr sets `HERDR_PANE_ID` in its panes and `TERM_PROGRAM=herdr`, and hides
/// the terminal it draws them into, so no variable says whether its Unicode
/// placeholders draw. Under tmux the pane is tmux's and tmux is asked instead:
/// the answer of the pane inside it is neither read nor waited for. A host of
/// [`UNTESTED_HOSTS`] inside the pane wins too, as it draws the pane itself.
pub fn asks_graphics(env: impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    !set("TMUX")
        && !UNTESTED_HOSTS.iter().any(|name| set(name))
        && (set("HERDR_PANE_ID") || matches!(env("TERM_PROGRAM").as_deref(), Some("herdr")))
}

/// The protocol of the terminal behind `env`, or `None` when it speaks none
/// this crate writes.
///
/// `env` reads one environment variable. `tmux` is the line [`tmux_display`]
/// printed, or `None` when it could not run. Inside tmux the pictures go out as
/// placeholders only when tmux passes an escape through and draws into one of
/// [`PLACEHOLDER_TERMINALS`]: no other protocol goes through it.
///
/// `graphics` is the answer the terminal gave the kitty graphics query, `None`
/// when it gave none. Only a herdr pane outside tmux is read from it: the pane
/// draws placeholders when its terminal answered the protocol, and no image at
/// all when it answered an error or nothing.
pub fn detect(
    env: impl Fn(&str) -> Option<String>,
    tmux: Option<&str>,
    graphics: Option<bool>,
) -> Option<Native> {
    let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    if UNTESTED_HOSTS.iter().any(|name| set(name)) {
        return None;
    }
    if asks_graphics(&env) {
        return (graphics == Some(true)).then_some(Native {
            protocol: Protocol::Placeholders,
            tmux: false,
        });
    }
    if set("TMUX") {
        let mut fields = tmux?.split('|');
        let term = fields.next()?.trim();
        let termtype = fields.next()?.trim();
        let passthrough = fields.next()?.trim();
        let passes = passthrough == "on" || passthrough == "all";
        let speaks = PLACEHOLDER_TERMINALS
            .iter()
            .any(|name| term.starts_with(name) || termtype.starts_with(name));
        return (passes && speaks).then_some(Native {
            protocol: Protocol::Placeholders,
            tmux: true,
        });
    }
    let placeholders = set("KITTY_WINDOW_ID")
        || matches!(
            env("TERM").as_deref(),
            Some("xterm-kitty" | "xterm-ghostty" | "rio")
        )
        || matches!(env("TERM_PROGRAM").as_deref(), Some("ghostty" | "rio"));
    if placeholders {
        return Some(Native {
            protocol: Protocol::Placeholders,
            tmux: false,
        });
    }
    let iterm2 = matches!(
        env("TERM_PROGRAM").as_deref(),
        Some("iTerm.app" | "WezTerm")
    );
    iterm2.then_some(Native {
        protocol: Protocol::Iterm2,
        tmux: false,
    })
}

/// Runs `tmux display -p TMUX_DISPLAY` and returns the first line it prints.
///
/// Any failure is `None`: tmux missing, a non-zero exit, or no output.
pub async fn tmux_display() -> Option<String> {
    let output = Command::new("tmux")
        .args(["display", "-p", TMUX_DISPLAY])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.lines().next()?.to_string())
}

/// Appends an iTerm2 `1337` file escape that draws `base64` inline.
///
/// `bytes` is the decoded size of the image, and `columns` and `rows` are the
/// cells it takes. The cursor stays where it was.
pub fn iterm2(out: &mut Vec<u8>, base64: &str, bytes: usize, columns: u32, rows: u32) {
    let _ = write!(
        out,
        "\x1b]1337;File=inline=1;size={bytes};width={columns};height={rows};\
         preserveAspectRatio=1;doNotMoveCursor=1:{base64}\x07"
    );
}

/// Appends the transmission of the PNG `png_base64` as image `id`, which takes
/// `columns` cells by `rows` cells of Unicode placeholders.
///
/// The base64 goes in chunks of at most [`CHUNK`] characters: the first names
/// the image and the placeholders it draws, and every chunk carries the flag
/// that says whether another follows. Every chunk is quiet (`q=2`). Inside
/// tmux each chunk goes wrapped on its own, since tmux passes one escape
/// through at a time.
pub fn transmit(out: &mut Vec<u8>, id: u32, columns: u16, rows: u16, png_base64: &str, tmux: bool) {
    let mut chunks = png_base64.as_bytes().chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = usize::from(chunks.peek().is_some());
        start(out, tmux);
        if first {
            let _ = write!(out, "a=T,f=100,i={id},U=1,c={columns},r={rows},q=2,");
        }
        let _ = write!(out, "m={more};");
        out.extend_from_slice(chunk);
        end(out, tmux);
        first = false;
    }
}

/// Appends the command that frees image `id` and its placeholders.
pub fn delete(out: &mut Vec<u8>, id: u32, tmux: bool) {
    start(out, tmux);
    let _ = write!(out, "a=d,d=I,i={id},q=2");
    end(out, tmux);
}

/// Appends the kitty command's opening, inside tmux's own wrapper.
fn start(out: &mut Vec<u8>, tmux: bool) {
    if tmux {
        out.extend_from_slice(b"\x1bPtmux;");
    }
    escape(out, tmux, b"\x1b_G");
}

/// Appends the kitty command's closing, inside tmux's own wrapper.
fn end(out: &mut Vec<u8>, tmux: bool) {
    escape(out, tmux, b"\x1b\\");
    if tmux {
        out.extend_from_slice(b"\x1b\\");
    }
}

/// Appends `bytes` to `out`, with every ESC written twice when `tmux` passes
/// them on: tmux ends its wrapper at the first ESC it does not know.
fn escape(out: &mut Vec<u8>, tmux: bool, bytes: &[u8]) {
    if !tmux {
        out.extend_from_slice(bytes);
        return;
    }
    for byte in bytes {
        if *byte == b'\x1b' {
            out.push(b'\x1b');
        }
        out.push(*byte);
    }
}

/// Appends the placeholder cells of `row` to `out`: `columns` cells of
/// `'\u{10EEEE}'`, the mark of the row and the mark of the column. A caller
/// draws them in one style whose foreground is [`id_color`] of the image.
///
/// A row or a column past [`PLACEHOLDER_MARKS`] writes nothing.
pub fn placeholder_row(out: &mut String, row: u16, columns: u16) {
    let Some(vertical) = PLACEHOLDER_MARKS.get(usize::from(row)) else {
        return;
    };
    for column in 0..columns {
        let Some(horizontal) = PLACEHOLDER_MARKS.get(usize::from(column)) else {
            return;
        };
        out.push('\u{10EEEE}');
        out.push(*vertical);
        out.push(*horizontal);
    }
}

/// The foreground color that names image `id` to the terminal, and the one
/// placeholders of another image leave alone: its three low bytes.
pub fn id_color(id: u32) -> (u8, u8, u8) {
    (
        (id >> 16 & 255) as u8,
        (id >> 8 & 255) as u8,
        (id & 255) as u8,
    )
}

/// The base64 PNG a transmission sends for the base64 image `png_base64`, or
/// `None` when the bytes are no image.
///
/// The image is decoded with [`Limits::default`], turned by its EXIF
/// orientation, grown to `cells` (the pixels of the cells it takes) when it is
/// smaller on both sides, shrunk so its longest side is at most [`MAX_PIXELS`]
/// and encoded as PNG. A terminal that stretches a small image smooths each
/// placeholder row on its own and draws a seam between rows. This does CPU
/// work over the whole picture, so a caller runs it in `spawn_blocking`.
pub fn png_payload(png_base64: &str, cells: Option<(u32, u32)>) -> Option<String> {
    let bytes = Bytes::from(STANDARD.decode(png_base64).ok()?);
    let decoded = decode(&bytes, &Limits::default()).ok()?;
    let pixels = apply(decoded.pixels, decoded.orientation);
    let (width, height) = match cells {
        Some((across, down)) if across > pixels.width && down > pixels.height => {
            shrink(across, down)
        }
        _ => shrink(pixels.width, pixels.height),
    };
    let pixels = if (width, height) == (pixels.width, pixels.height) {
        pixels
    } else {
        resize(pixels, width, height)
    };
    let png = reagent_image::encode::png(&pixels, None).ok()?;
    Some(STANDARD.encode(png))
}

/// The size `width` by `height` shrinks to when its longest side is over
/// [`MAX_PIXELS`]: its aspect kept, rounded to the nearest pixel and at least
/// one a side. A picture that fits keeps its size.
fn shrink(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= MAX_PIXELS {
        return (width, height);
    }
    let (across, down) = if width >= height {
        (
            u64::from(MAX_PIXELS),
            nearest(u64::from(height) * u64::from(MAX_PIXELS), u64::from(width)),
        )
    } else {
        (
            nearest(u64::from(width) * u64::from(MAX_PIXELS), u64::from(height)),
            u64::from(MAX_PIXELS),
        )
    };
    let side = |side: u64| u32::try_from(side.max(1)).unwrap_or(MAX_PIXELS);
    (side(across), side(down))
}

/// `numerator` over `denominator`, rounded to the nearest whole number.
fn nearest(numerator: u64, denominator: u64) -> u64 {
    (numerator + denominator / 2) / denominator.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRADIENT: &[u8] = include_bytes!("../../image/tests/fixtures/gradient-64x48.png");

    /// The protocol of a terminal with `pairs` as its environment, which
    /// answered no graphics query.
    fn detected(pairs: &[(&str, &str)], tmux: Option<&str>) -> Option<Native> {
        detected_with(pairs, tmux, None)
    }

    /// The protocol of a terminal with `pairs` as its environment, and the
    /// answer to the kitty graphics query it gave, if any.
    fn detected_with(
        pairs: &[(&str, &str)],
        tmux: Option<&str>,
        graphics: Option<bool>,
    ) -> Option<Native> {
        detect(
            |name| {
                pairs
                    .iter()
                    .find(|pair| pair.0 == name)
                    .map(|pair| pair.1.to_string())
            },
            tmux,
            graphics,
        )
    }

    /// How many times `needle` sits in `bytes`.
    fn held(bytes: &[u8], needle: &[u8]) -> usize {
        bytes
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    /// The pieces of a transmission, without their escapes.
    fn chunks(out: &[u8]) -> Vec<String> {
        String::from_utf8_lossy(out)
            .split("\x1b\\")
            .map(str::to_owned)
            .filter(|piece| !piece.is_empty())
            .collect()
    }

    #[test]
    fn detect_reads_the_placeholder_terminals() {
        let placeholders = Some(Native {
            protocol: Protocol::Placeholders,
            tmux: false,
        });
        for env in [
            [("TERM_PROGRAM", "rio")],
            [("TERM", "rio")],
            [("KITTY_WINDOW_ID", "1")],
            [("TERM", "xterm-kitty")],
            [("TERM_PROGRAM", "ghostty")],
            [("TERM", "xterm-ghostty")],
        ] {
            assert_eq!(detected(&env, None), placeholders, "{env:?}");
        }
    }

    #[test]
    fn detect_reads_the_iterm2_terminals() {
        for program in ["iTerm.app", "WezTerm"] {
            let env = [("TERM_PROGRAM", program)];
            assert_eq!(
                detected(&env, None),
                Some(Native {
                    protocol: Protocol::Iterm2,
                    tmux: false,
                }),
                "{program}"
            );
        }
    }

    #[test]
    fn detect_of_a_plain_terminal_is_none() {
        let terminal = [
            ("TERM_PROGRAM", "Apple_Terminal"),
            ("TERM", "xterm-256color"),
        ];
        assert_eq!(detected(&terminal, None), None);
    }

    #[test]
    fn detect_inside_tmux_reads_the_terminal_and_the_passthrough() {
        let tmux = [("TMUX", "/tmp/tmux-501/default,1,0")];
        assert_eq!(
            detected(&tmux, Some("rio|Rio 0.5.28|on")),
            Some(Native {
                protocol: Protocol::Placeholders,
                tmux: true,
            })
        );
        assert_eq!(
            detected(&tmux, Some("xterm-kitty||all")),
            Some(Native {
                protocol: Protocol::Placeholders,
                tmux: true,
            })
        );
        assert_eq!(detected(&tmux, Some("rio|Rio 0.5.28|off")), None);
        assert_eq!(detected(&tmux, Some("xterm-256color|iTerm2 3.5|on")), None);
        assert_eq!(detected(&tmux, None), None);
    }

    #[test]
    fn detect_inside_other_multiplexers_is_none() {
        for host in [
            ("ZELLIJ", "0"),
            ("ZELLIJ_SESSION_NAME", "main"),
            ("STY", "1234.pts-0"),
            ("RMAESTRO_PANE", "pane0001"),
        ] {
            let env = [("TERM_PROGRAM", "rio"), host];
            assert_eq!(detected(&env, None), None, "{host:?}");
            let env = [("TMUX", "/tmp/tmux-501/default,1,0"), host];
            assert_eq!(detected(&env, Some("rio|Rio 0.5.28|on")), None, "{host:?}");
        }
    }

    #[test]
    fn detect_inside_herdr_follows_the_graphics_answer() {
        // herdr hides the terminal it draws its panes with and keeps its name,
        // so the answer to the kitty query is the one thing that says whether
        // its placeholders draw.
        let placeholders = Some(Native {
            protocol: Protocol::Placeholders,
            tmux: false,
        });
        for env in [[("HERDR_PANE_ID", "w1:p1")], [("TERM_PROGRAM", "herdr")]] {
            assert_eq!(
                detected_with(&env, None, Some(true)),
                placeholders,
                "{env:?}"
            );
            assert_eq!(
                detected_with(&env, None, Some(false)),
                None,
                "the terminal answered an error: {env:?}"
            );
            assert_eq!(
                detected_with(&env, None, None),
                None,
                "no answer, no image: {env:?}"
            );
        }
        // Under tmux the pane is tmux's, and the tmux branch decides as it did.
        let tmux = [
            ("TMUX", "/tmp/tmux-501/default,1,0"),
            ("HERDR_PANE_ID", "w1:p1"),
        ];
        assert_eq!(
            detected_with(&tmux, Some("rio|Rio 0.5.28|on"), Some(true)),
            Some(Native {
                protocol: Protocol::Placeholders,
                tmux: true,
            }),
            "{tmux:?}"
        );
        assert_eq!(
            detected_with(&tmux, Some("rio|Rio 0.5.28|off"), Some(true)),
            None
        );
    }

    #[test]
    fn asks_graphics_only_in_herdr_outside_tmux() {
        fn asks(pairs: &[(&str, &str)]) -> bool {
            asks_graphics(|name| {
                pairs
                    .iter()
                    .find(|pair| pair.0 == name)
                    .map(|pair| pair.1.to_string())
            })
        }
        assert!(asks(&[("HERDR_PANE_ID", "w1:p1")]));
        assert!(asks(&[
            ("TERM_PROGRAM", "herdr"),
            ("TERM", "xterm-256color")
        ]));
        assert!(
            !asks(&[
                ("HERDR_PANE_ID", "w1:p1"),
                ("TMUX", "/tmp/tmux-501/default,1,0")
            ]),
            "tmux in front of the pane answers for it"
        );
        assert!(!asks(&[
            ("TERM_PROGRAM", "herdr"),
            ("TMUX", "/tmp/tmux-501/default,1,0")
        ]));
        assert!(
            !asks(&[("HERDR_PANE_ID", "w1:p1"), ("ZELLIJ", "0")]),
            "zellij in the pane draws it, and passes no image"
        );
        assert!(!asks(&[("TERM", "xterm-256color")]), "a plain terminal");
        assert!(
            !asks(&[("TERM", "xterm-kitty"), ("KITTY_WINDOW_ID", "1")]),
            "kitty answers the protocol without a query"
        );
        assert!(!asks(&[("TERM_PROGRAM", "iTerm.app")]));
    }

    #[test]
    fn iterm2_writes_the_file_escape() {
        let mut out = Vec::new();
        iterm2(&mut out, "QUJD", 3, 4, 2);
        assert_eq!(
            out,
            b"\x1b]1337;File=inline=1;size=3;width=4;height=2;preserveAspectRatio=1;\
              doNotMoveCursor=1:QUJD\x07"
        );
    }

    #[test]
    fn transmit_of_one_chunk_names_the_placeholders() {
        let mut out = Vec::new();
        transmit(&mut out, 7, 4, 3, "MDEyMzQ1Njc4OQ==", false);
        assert_eq!(
            out,
            b"\x1b_Ga=T,f=100,i=7,U=1,c=4,r=3,q=2,m=0;MDEyMzQ1Njc4OQ==\x1b\\"
        );
    }

    #[test]
    fn transmit_of_a_long_payload_chunks_it() {
        let payload = "A".repeat(9000);
        let mut out = Vec::new();
        transmit(&mut out, 9, 2, 1, &payload, false);
        let pieces = chunks(&out);
        assert_eq!(pieces.len(), 3, "{} chunks", pieces.len());
        let first = pieces.first().expect("a first chunk");
        assert!(first.starts_with("\x1b_Ga=T,f=100,i=9,U=1,c=2,r=1,q=2,m=1;"));
        let second = pieces.get(1).expect("a second chunk");
        assert!(second.starts_with("\x1b_Gm=1;"), "{second:?}");
        let last = pieces.get(2).expect("a last chunk");
        assert!(last.starts_with("\x1b_Gm=0;"), "{last:?}");
        for piece in &pieces {
            let (_, body) = piece.split_once(';').expect("the keys and the payload");
            assert!(body.len() <= CHUNK, "{} characters", body.len());
        }
        assert_eq!(pieces.concat().matches('A').count(), 9000);
        assert_eq!(held(&out, b"a=T"), 1, "the keys go out once");
    }

    #[test]
    fn transmit_inside_tmux_wraps_every_chunk() {
        let payload = "A".repeat(5000);
        let mut out = Vec::new();
        transmit(&mut out, 3, 2, 1, &payload, true);
        assert_eq!(
            held(&out, b"\x1bPtmux;\x1b\x1b_G"),
            2,
            "each chunk of two opens its own wrapper"
        );
        assert_eq!(held(&out, b"\x1b\x1b\\\x1b\\"), 2, "and closes it");
        assert_eq!(
            held(
                &out,
                b"\x1bPtmux;\x1b\x1b_Ga=T,f=100,i=3,U=1,c=2,r=1,q=2,m=1;"
            ),
            1
        );
    }

    #[test]
    fn delete_frees_the_image_and_its_placeholders() {
        let mut out = Vec::new();
        delete(&mut out, 7, false);
        assert_eq!(out, b"\x1b_Ga=d,d=I,i=7,q=2\x1b\\");
        let mut out = Vec::new();
        delete(&mut out, 7, true);
        assert_eq!(out, b"\x1bPtmux;\x1b\x1b_Ga=d,d=I,i=7,q=2\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn placeholder_row_names_the_row_and_the_columns() {
        let mut row = String::new();
        placeholder_row(&mut row, 2, 3);
        let marks = PLACEHOLDER_MARKS;
        let mut expected = String::new();
        for column in 0..3 {
            expected.push('\u{10EEEE}');
            expected.push(marks[2]);
            expected.push(marks[column]);
        }
        assert_eq!(row, expected);
        assert_eq!(row.chars().filter(|c| *c == '\u{10EEEE}').count(), 3);
    }

    #[test]
    fn placeholder_row_past_the_marks_stops() {
        let mut row = String::new();
        placeholder_row(&mut row, u16::from(u8::MAX), 2);
        assert!(row.is_empty(), "no mark for row 255");
        let mut row = String::new();
        placeholder_row(&mut row, 0, 40);
        assert_eq!(row.chars().filter(|c| *c == '\u{10EEEE}').count(), 40);
    }

    #[test]
    fn id_color_reads_the_three_low_bytes() {
        assert_eq!(id_color(0x12_34_56), (0x12, 0x34, 0x56));
        assert_eq!(id_color(1), (0, 0, 1));
        assert_eq!(id_color(0xFF_FF_FF), (0xFF, 0xFF, 0xFF));
    }

    #[test]
    fn png_payload_shrinks_the_longest_side() {
        let payload = png_payload(&STANDARD.encode(GRADIENT), None).expect("a payload");
        let bytes = STANDARD.decode(&payload).expect("base64");
        let decoded = decode(&Bytes::from(bytes), &Limits::default()).expect("a png");
        assert_eq!((decoded.pixels.width, decoded.pixels.height), (64, 48));
        assert!(png_payload("not base64!", None).is_none());
        assert!(png_payload(&STANDARD.encode(b"these bytes are no image"), None).is_none());
    }

    /// A terminal that stretches a small image over many cell rows smooths each
    /// row on its own and draws a seam between rows, so a small image goes out
    /// at the pixel size of its cells.
    #[test]
    fn png_payload_grows_a_small_image_to_its_cells() {
        let payload = png_payload(&STANDARD.encode(GRADIENT), Some((448, 340))).expect("a payload");
        let bytes = STANDARD.decode(&payload).expect("base64");
        let decoded = decode(&Bytes::from(bytes), &Limits::default()).expect("a png");
        assert_eq!((decoded.pixels.width, decoded.pixels.height), (448, 340));
        let payload = png_payload(&STANDARD.encode(GRADIENT), Some((32, 24))).expect("a payload");
        let bytes = STANDARD.decode(&payload).expect("base64");
        let decoded = decode(&Bytes::from(bytes), &Limits::default()).expect("a png");
        assert_eq!(
            (decoded.pixels.width, decoded.pixels.height),
            (64, 48),
            "a larger image is the terminal's to shrink"
        );
    }

    #[test]
    fn shrink_keeps_the_aspect_and_never_grows() {
        assert_eq!(shrink(3200, 1600), (1600, 800));
        assert_eq!(shrink(1600, 3200), (800, 1600));
        assert_eq!(shrink(64, 48), (64, 48));
        assert_eq!(shrink(1600, 1), (1600, 1));
        assert_eq!(shrink(3200, 1), (1600, 1));
    }
}
