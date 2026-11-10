//! A picture as half-block cells for the transcript.
//!
//! One cell shows two pixels: the top pixel in the glyph's foreground and the
//! pixel below it in its background. The picture shrinks into the room the
//! transcript has, keeps its aspect and never grows, and every channel keeps
//! its 5 high bits so near colors share one style.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use reagent_ai::types::{ImageType, Text};
use reagent_ai::{Content, Image, Message};
use reagent_image::{Limits, Pixels, apply, decode, resize};

/// Most columns one picture takes.
pub const MAX_COLUMNS: u32 = 60;

/// Most rows one picture takes.
pub const MAX_ROWS: u32 = 20;

/// Most distinct color pairs a transcript holds before it draws no more
/// pictures.
pub const MAX_STYLES: usize = 65_536;

/// The bits of a channel a style keeps: the 5 high bits of the 8-bit value.
const HIGH_BITS: u8 = 0xF8;

/// Bytes of one cell: three for the top pixel's color and three for the low
/// one's.
const PAIR_LEN: usize = 6;

/// The size of one terminal cell in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellShape {
    pub across: u16,
    pub down: u16,
}

impl CellShape {
    /// The cell of a terminal that reports no pixel size: twice as tall as wide,
    /// so a half-block's two pixels are square.
    pub const HALF_BLOCK: Self = Self { across: 1, down: 2 };

    /// The cell of a window of `columns` by `rows` cells and `width` by `height`
    /// pixels; `None` when a size is 0 or a cell is under one pixel.
    pub fn of_window(columns: u16, rows: u16, width: u16, height: u16) -> Option<Self> {
        let across = width.checked_div(columns)?;
        let down = height.checked_div(rows)?;
        (across > 0 && down > 0).then_some(Self { across, down })
    }
}

/// Where a picture draws: the columns it may take and the shape of a cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Room {
    pub columns: u32,
    pub cell: CellShape,
}

impl Room {
    /// `columns` of half-block cells.
    pub const fn columns(columns: u32) -> Self {
        Self {
            columns,
            cell: CellShape::HALF_BLOCK,
        }
    }
}

/// The pixels `(across, down)` a picture of `width` by `height` draws at in
/// `room` columns of `cell`s.
///
/// A cell column is one pixel across and a cell row is two pixels down, and
/// the rows keep the picture's aspect on the real `cell`. The picture fits into
/// `MAX_COLUMNS.min(room)` columns and `MAX_ROWS` rows and takes no more
/// columns than it has pixels. `down` is twice the rows, at least 2; `across`
/// is at least 1. `None` when `width`, `height` or `room` is 0.
pub fn fit(width: u32, height: u32, room: u32, cell: CellShape) -> Option<(u32, u32)> {
    if width == 0 || height == 0 || room == 0 {
        return None;
    }
    let (wide, tall) = (u64::from(cell.across.max(1)), u64::from(cell.down.max(1)));
    let max_columns = u64::from(MAX_COLUMNS.min(room));
    let max_rows = u64::from(MAX_ROWS);
    let (width, height) = (u64::from(width), u64::from(height));
    let mut across = width.min(max_columns);
    let mut rows = nearest(across * height * wide, width * tall);
    if rows > max_rows {
        rows = max_rows;
        across = nearest(max_rows * width * tall, height * wide).min(max_columns);
    }
    let (across, rows) = (across.max(1), rows.max(1));
    Some((u32::try_from(across).ok()?, u32::try_from(rows * 2).ok()?))
}

/// `numerator` over `denominator`, rounded to the nearest whole number.
fn nearest(numerator: u64, denominator: u64) -> u64 {
    (numerator + denominator / 2) / denominator
}

/// The cells of one picture: its columns and rows, and the colors of every
/// cell, row by row.
#[derive(Debug)]
pub struct Cells {
    /// Columns of cells across.
    pub columns: u32,
    /// Rows of cells down.
    pub rows: u32,
    /// The colors of every cell, row by row: three bytes of the top pixel and
    /// three of the low one.
    colors: Vec<[u8; PAIR_LEN]>,
}

impl Cells {
    /// The top and low pixel colors of the cell at `column` and `row`, or
    /// `None` outside the picture.
    pub fn get(&self, column: u32, row: u32) -> Option<([u8; 3], [u8; 3])> {
        if column >= self.columns || row >= self.rows {
            return None;
        }
        let index = (row as usize)
            .checked_mul(self.columns as usize)?
            .checked_add(column as usize)?;
        let pair = self.colors.get(index)?;
        let (halves, _) = pair.as_chunks::<3>();
        let top = *halves.first()?;
        let low = *halves.get(1)?;
        Some((top, low))
    }

    /// How many distinct color pairs it holds.
    pub fn pairs(&self) -> usize {
        let mut seen = self.colors.clone();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }
}

/// The cells of the base64 image `data`, shrunk into `room` columns.
///
/// The image is decoded with [`Limits::default`] and any failure, of the
/// base64 or of the decoder, is `None`. A file's EXIF orientation is applied,
/// because an MCP tool's image was never prepared. An RGBA pixel is blended
/// over black.
/// This does CPU work over the whole picture, so a caller runs it in
/// `spawn_blocking`.
pub fn cells(data: &str, room: Room) -> Option<Cells> {
    let bytes = Bytes::from(STANDARD.decode(data).ok()?);
    let decoded = decode(&bytes, &Limits::default()).ok()?;
    let pixels = apply(decoded.pixels, decoded.orientation);
    let (columns, down) = fit(pixels.width, pixels.height, room.columns, room.cell)?;
    let pixels = if pixels.width == columns && pixels.height == down {
        pixels
    } else {
        resize(pixels, columns, down)
    };
    build(&pixels, columns, down)
}

/// Cuts the resized pixels of `columns` by `down` into one pair per cell.
fn build(pixels: &Pixels, columns: u32, down: u32) -> Option<Cells> {
    let rows = down / 2;
    let channels = pixels.channels.count();
    let stride = (columns as usize)
        .checked_mul(channels)
        .filter(|stride| *stride > 0)?;
    let mut colors = Vec::with_capacity((columns as usize).checked_mul(rows as usize)?);
    let mut strips = pixels.data.chunks_exact(stride);
    for _ in 0..rows {
        let (Some(top), Some(low)) = (strips.next(), strips.next()) else {
            break;
        };
        for (top_pixel, low_pixel) in top.chunks_exact(channels).zip(low.chunks_exact(channels)) {
            let mut pair = [0u8; PAIR_LEN];
            let shades = shade(top_pixel).into_iter().chain(shade(low_pixel));
            for (slot, value) in pair.iter_mut().zip(shades) {
                *slot = value;
            }
            colors.push(pair);
        }
    }
    Some(Cells {
        columns,
        rows,
        colors,
    })
}

/// The opaque color of one pixel: an RGBA pixel blended over black, with every
/// channel cut to its 5 high bits.
fn shade(sample: &[u8]) -> [u8; 3] {
    let alpha = u32::from(sample.get(3).copied().unwrap_or(255));
    let mut color = [0u8; 3];
    for (channel, value) in color.iter_mut().zip(sample) {
        *channel = ((u32::from(*value) * alpha / 255) as u8) & HIGH_BITS;
    }
    color
}

/// One image of a message: the cells it draws as, when it decodes, the chip
/// that names it under them, and the bytes a terminal's own image protocol
/// sends in place of those cells.
#[derive(Debug)]
pub struct Picture {
    /// The cells of the image, and `None` when its bytes do not decode.
    pub cells: Option<Cells>,
    /// The row under them, `▣ image 64x48 · 3 KB`.
    pub chip: String,
    /// What the terminal needs to draw the image itself, `None` for a picture
    /// the terminal draws only as cells.
    pub source: Option<Source>,
}

/// The base64 image a picture was drawn from, as the journal holds it, or the
/// PNG the app made of it for a terminal that draws placeholders.
#[derive(Clone, Debug)]
pub struct Source {
    /// The base64 text itself, shared with the message and never copied.
    pub data: Text,
    /// The decoded size of the image, in bytes.
    pub bytes: usize,
    /// Whether `data` is the PNG a transmission sends.
    pub png: bool,
}

/// The pictures the images of `message` hold, in message order: the ones a user
/// message attached, and the images of every tool result in one.
///
/// This decodes and resizes every image it finds, so a caller runs it in
/// `spawn_blocking`.
pub fn of_message(message: &Message, room: Room) -> Vec<Picture> {
    match message {
        Message::User { content } => content
            .iter()
            .filter_map(|block| match block {
                Content::Image(image) => Some(image),
                _ => None,
            })
            .map(|image| of_image(image, room))
            .collect(),
        Message::ToolResult(result) => result
            .images
            .iter()
            .map(|image| of_image(image, room))
            .collect(),
        Message::Assistant(_) => Vec::new(),
    }
}

/// The picture of one image.
fn of_image(image: &Image, room: Room) -> Picture {
    let cells = cells(&image.data, room);
    // A picture with no cells draws its chip alone, so it has nothing for a
    // terminal to draw either.
    let source = cells.as_ref().map(|_| Source {
        data: Arc::clone(&image.data),
        bytes: decoded_bytes(&image.data),
        png: image.media_type == ImageType::Png,
    });
    Picture {
        cells,
        chip: chip(image),
        source,
    }
}

/// The size of the image `data` holds in base64: three bytes for every four
/// characters, less the `=` that pads the end.
pub(crate) fn decoded_bytes(data: &str) -> usize {
    let padding = data
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    (data.len().saturating_mul(3) / 4).saturating_sub(padding)
}

/// The chip of one image: its pixels and its size, the length of its base64
/// text times three quarters, in whole kilobytes and at least one.
fn chip(image: &Image) -> String {
    let kb = (decoded_bytes(&image.data) / 1024).max(1);
    format!("▣ image {}x{} · {kb} KB", image.width, image.height)
}

/// The styles a transcript has left for pictures.
#[derive(Debug)]
pub struct StyleBudget {
    /// Color pairs the pictures so far left untaken.
    left: usize,
}

impl StyleBudget {
    /// A budget of [`MAX_STYLES`] pairs.
    pub fn new() -> StyleBudget {
        StyleBudget { left: MAX_STYLES }
    }

    /// Whether `cells` fits: `true` and its pairs taken when that many are
    /// left, `false` and nothing taken otherwise.
    pub fn take(&mut self, cells: &Cells) -> bool {
        let pairs = cells.pairs();
        if pairs > self.left {
            return false;
        }
        self.left -= pairs;
        true
    }
}

impl Default for StyleBudget {
    fn default() -> StyleBudget {
        StyleBudget::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use reagent_image::Channels;

    const GRADIENT: &[u8] = include_bytes!("../../image/tests/fixtures/gradient-64x48.png");

    const HALF: CellShape = CellShape::HALF_BLOCK;

    #[test]
    fn fit_keeps_the_aspect_in_the_room() {
        assert_eq!(fit(1280, 720, 100, HALF), Some((60, 34)));
        assert_eq!(fit(1280, 720, 20, HALF), Some((20, 12)));
        assert_eq!(fit(4, 4, 100, HALF), Some((4, 4)));
        assert_eq!(fit(720, 1280, 100, HALF), Some((23, 40)));
    }

    /// iTerm2 draws its image into the rows with the aspect kept: rows sized for
    /// a two-to-one cell leave empty rows under it where a cell is taller.
    #[test]
    fn fit_follows_the_cell_shape() {
        let tall = CellShape {
            across: 8,
            down: 20,
        };
        assert_eq!(fit(1280, 720, 100, tall), Some((60, 28)));
        assert_eq!(fit(4, 4, 100, tall), Some((4, 4)));
        // Rows past the cap shrink the columns on the same cell.
        assert_eq!(fit(720, 1280, 100, tall), Some((28, 40)));
        let square = CellShape {
            across: 10,
            down: 10,
        };
        assert_eq!(fit(1280, 720, 100, square), Some((36, 40)));
    }

    #[test]
    fn cell_shape_comes_from_the_window_pixels() {
        assert_eq!(
            CellShape::of_window(100, 40, 800, 800),
            Some(CellShape {
                across: 8,
                down: 20
            })
        );
        assert_eq!(CellShape::of_window(100, 40, 0, 0), None, "no pixel size");
        assert_eq!(CellShape::of_window(0, 40, 800, 800), None);
        assert_eq!(
            CellShape::of_window(100, 40, 50, 800),
            None,
            "under a pixel"
        );
    }

    #[test]
    fn a_missing_dimension_or_room_is_none() {
        assert_eq!(fit(0, 48, 100, HALF), None);
        assert_eq!(fit(64, 0, 100, HALF), None);
        assert_eq!(fit(64, 48, 0, HALF), None);
    }

    #[test]
    fn a_wide_gradient_takes_the_row_cap() {
        // fit(64, 48, 100) gives 53 pixels across by 40 down: 20 rows.
        let cells = cells(&STANDARD.encode(GRADIENT), Room::columns(100)).expect("gradient");
        assert_eq!((cells.columns, cells.rows), (53, 20));
        let top = cells.get(0, 0).expect("first cell").0;
        let low = cells.get(52, 19).expect("last cell").1;
        assert_ne!(top, low);
        assert_eq!(cells.get(53, 0), None);
        assert_eq!(cells.get(0, 20), None);
    }

    #[test]
    fn a_small_picture_keeps_its_pixels() {
        let image = Pixels {
            width: 2,
            height: 2,
            channels: Channels::Rgb,
            data: vec![0xff, 0, 0, 0xff, 0, 0, 0, 0, 0xff, 0, 0, 0xff],
        };
        let png = reagent_image::encode::png(&image, None).expect("png");
        let cells = cells(&STANDARD.encode(png), Room::columns(100)).expect("2 by 2");
        assert_eq!((cells.columns, cells.rows), (2, 1));
        let red = ([0xf8, 0, 0], [0, 0, 0xf8]);
        assert_eq!(cells.get(0, 0), Some(red));
        assert_eq!(cells.get(1, 0), Some(red));
        assert_eq!(cells.get(2, 0), None);
        assert_eq!(cells.pairs(), 1);
    }

    #[test]
    fn nonsense_is_none() {
        assert!(cells("not base64!", Room::columns(100)).is_none());
        assert!(
            cells(
                &STANDARD.encode(b"these bytes are no image"),
                Room::columns(100)
            )
            .is_none()
        );
        assert!(cells(&STANDARD.encode(GRADIENT), Room::columns(0)).is_none());
    }

    #[test]
    fn a_clear_pixel_is_black() {
        assert_eq!(shade(&[255, 255, 255, 0]), [0, 0, 0]);
        assert_eq!(shade(&[0, 0, 0, 0]), [0, 0, 0]);
        assert_eq!(shade(&[0xff, 0xff, 0xff, 0xff]), [0xf8, 0xf8, 0xf8]);
    }

    #[test]
    fn a_channel_keeps_its_five_high_bits() {
        assert_eq!(shade(&[0xff, 0x10, 0x07, 0xff]), [0xf8, 0x10, 0x00]);
        assert_eq!(shade(&[0x07, 0x0f, 0xf9]), [0x00, 0x08, 0xf8]);
    }

    /// Cells of one row holding `count` distinct pairs.
    fn distinct(count: usize) -> Cells {
        let colors = (0..count)
            .map(|index| {
                let index = index as u16;
                [index as u8, (index >> 8) as u8, 0, 0, 0, 0]
            })
            .collect();
        Cells {
            columns: count as u32,
            rows: 1,
            colors,
        }
    }

    #[test]
    fn a_budget_takes_a_picture_whole() {
        let mut budget = StyleBudget::new();
        let small = distinct(2);
        assert!(budget.take(&small));
        assert!(budget.take(&small));
        let big = distinct(MAX_STYLES - 3);
        assert_eq!(big.pairs(), MAX_STYLES - 3);
        assert!(!budget.take(&big));
        assert!(budget.take(&small));
    }
}
