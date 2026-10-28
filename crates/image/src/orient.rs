//! Reads the EXIF orientation tag and applies it to the pixels.
//!
//! The tag lives in a TIFF header, either on its own (a TIFF file or the
//! `eXIf` chunk of a PNG) or behind an `Exif\0\0` marker (the APP1 segment of
//! a JPEG and the `EXIF` chunk of a WebP).

use crate::{Format, Pixels, be_u16, be_u32, le_u32};

/// The EXIF tag that holds the orientation.
const ORIENTATION_TAG: u16 = 274;
/// The TIFF field type SHORT, the only type the orientation tag uses.
const TIFF_SHORT: u16 = 3;
/// Every segment after this marker, the start of the image itself, is skipped.
const JPEG_SOS: u8 = 0xda;
/// The last marker in a JPEG stream.
const JPEG_EOI: u8 = 0xd9;
/// The APP1 marker, which carries EXIF in a JPEG.
const JPEG_APP1: u8 = 0xe1;
/// A TIFF header starts with this when its numbers are little endian.
const TIFF_LE: [u8; 4] = [0x49, 0x49, 0x2a, 0x00];
/// A TIFF header starts with this when its numbers are big endian.
const TIFF_BE: [u8; 4] = [0x4d, 0x4d, 0x00, 0x2a];
/// Bytes in one IFD entry.
const IFD_ENTRY_LEN: usize = 12;
/// Bytes before the IFD entries: the entry count.
const IFD_HEADER_LEN: usize = 2;

/// The EXIF orientation of a file, 1 to 8, or 1 when the file gives none or
/// the block is cut.
pub fn orientation(format: Format, bytes: &[u8]) -> u8 {
    match format {
        Format::Jpeg => jpeg_exif(bytes),
        Format::Png => png_exif(bytes),
        Format::Webp => webp_exif(bytes),
        Format::Tiff => tiff_orientation(bytes),
        Format::Gif | Format::Bmp | Format::Heic | Format::Avif => 1,
    }
}

/// Returns `pixels` turned or mirrored by an EXIF orientation, 1 to 8.
///
/// Orientation 1 returns the buffer unchanged, 2, 3 and 4 turn it in place,
/// and 5 to 8, which swap width and height, write one new buffer. Any other
/// value leaves the image unchanged.
pub fn apply(pixels: Pixels, orientation: u8) -> Pixels {
    if !(2..=8).contains(&orientation) {
        return pixels;
    }
    let mut pixels = pixels;
    let row_len = pixels.width as usize * pixels.channels.count();
    let channels = pixels.channels.count();
    match orientation {
        2 => mirror_rows(&mut pixels.data, row_len, channels),
        3 => reverse_pixels(&mut pixels.data, channels),
        4 => flip_rows(&mut pixels.data, row_len),
        _ => return turned(pixels, orientation),
    }
    pixels
}

/// Mirrors every row in place: orientation 2.
fn mirror_rows(data: &mut [u8], row_len: usize, channels: usize) {
    if row_len == 0 || channels == 0 {
        return;
    }
    for row in data.chunks_exact_mut(row_len) {
        swap_pixels(row, channels);
    }
}

/// Reverses the pixel order in place: orientation 3.
fn reverse_pixels(data: &mut [u8], channels: usize) {
    if channels == 0 {
        return;
    }
    swap_pixels(data, channels);
}

/// Turns every pixel against its mirror image in the same slice.
fn swap_pixels(data: &mut [u8], channels: usize) {
    let count = data.len() / channels;
    for pixel in 0..count / 2 {
        let left = pixel * channels;
        let right = (count - 1 - pixel) * channels;
        for channel in 0..channels {
            data.swap(left + channel, right + channel);
        }
    }
}

/// Flips the row order in place: orientation 4.
fn flip_rows(data: &mut [u8], row_len: usize) {
    if row_len == 0 {
        return;
    }
    let rows = data.len() / row_len;
    for row in 0..rows / 2 {
        let (top, rest) = data.split_at_mut((row + 1) * row_len);
        let from = row * row_len;
        // `rest` starts after the top row, so the bottom row is this far in.
        let to = (rows - 2 - 2 * row) * row_len;
        if let (Some(top), Some(bottom)) = (top.get_mut(from..), rest.get_mut(to..to + row_len)) {
            top[..row_len].swap_with_slice(bottom);
        }
    }
}

/// Writes orientations 5 to 8, which swap width and height, into one new
/// buffer.
fn turned(pixels: Pixels, orientation: u8) -> Pixels {
    let width = pixels.width as usize;
    let height = pixels.height as usize;
    let channels = pixels.channels.count();
    let Some(len) = height
        .checked_mul(width)
        .and_then(|n| n.checked_mul(channels))
    else {
        return pixels;
    };
    let (out_width, out_height) = (height, width);
    let mut data = vec![0u8; len];
    for dy in 0..out_height {
        for dx in 0..out_width {
            let (sx, sy) = match orientation {
                5 => (dy, dx),
                6 => (dy, height - 1 - dx),
                7 => (width - 1 - dy, height - 1 - dx),
                _ => (width - 1 - dy, dx),
            };
            let src = (sy * width + sx) * channels;
            let dst = (dy * out_width + dx) * channels;
            if let (Some(src), Some(dst)) = (
                pixels.data.get(src..src + channels),
                data.get_mut(dst..dst + channels),
            ) {
                dst.copy_from_slice(src);
            }
        }
    }
    Pixels {
        width: out_width as u32,
        height: out_height as u32,
        channels: pixels.channels,
        data,
    }
}

/// Walks the JPEG segments until the image data and reads an EXIF APP1.
fn jpeg_exif(bytes: &[u8]) -> u8 {
    let mut pos = 2;
    while pos + 4 <= bytes.len() {
        if bytes.get(pos) != Some(&0xff) {
            return 1;
        }
        let mut marker_at = pos;
        while bytes.get(marker_at) == Some(&0xff) {
            marker_at += 1;
        }
        let Some(&marker) = bytes.get(marker_at) else {
            return 1;
        };
        if marker == JPEG_EOI || marker == JPEG_SOS {
            return 1;
        }
        // Markers without a length field: TEM and the restart markers.
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            pos = marker_at + 1;
            continue;
        }
        let Some(length) = be_u16(bytes, marker_at + 1) else {
            return 1;
        };
        let length = length as usize;
        if length < 2 {
            return 1;
        }
        let data_start = marker_at + 3;
        let Some(data_end) = marker_at.checked_add(1 + length) else {
            return 1;
        };
        let Some(data) = bytes.get(data_start..data_end) else {
            return 1;
        };
        if marker == JPEG_APP1 && data.starts_with(b"Exif\0\0") {
            return exif_orientation(&data[6..]);
        }
        pos = data_end;
    }
    1
}

/// Walks the PNG chunks and reads an `eXIf` chunk.
fn png_exif(bytes: &[u8]) -> u8 {
    const PNG_HEADER_LEN: usize = 8;
    const CHUNK_HEADER_LEN: usize = 8;
    const CRC_LEN: usize = 4;
    let mut pos = PNG_HEADER_LEN;
    while let Some(length) = be_u32(bytes, pos) {
        let length = length as usize;
        let Some(kind) = bytes.get(pos + 4..pos + 8) else {
            return 1;
        };
        let data_start = pos + CHUNK_HEADER_LEN;
        let Some(data_end) = data_start.checked_add(length) else {
            return 1;
        };
        let Some(data) = bytes.get(data_start..data_end) else {
            return 1;
        };
        if kind == b"eXIf" {
            return tiff_orientation(data);
        }
        if kind == b"IEND" {
            return 1;
        }
        let Some(next) = data_end.checked_add(CRC_LEN) else {
            return 1;
        };
        pos = next;
    }
    1
}

/// Walks the RIFF chunks of a WebP and reads an `EXIF` chunk.
fn webp_exif(bytes: &[u8]) -> u8 {
    const RIFF_HEADER_LEN: usize = 12;
    const CHUNK_HEADER_LEN: usize = 8;
    let mut pos = RIFF_HEADER_LEN;
    while let Some(kind) = bytes.get(pos..pos + 4) {
        let Some(length) = le_u32(bytes, pos + 4) else {
            return 1;
        };
        let length = length as usize;
        let data_start = pos + CHUNK_HEADER_LEN;
        let Some(data_end) = data_start.checked_add(length) else {
            return 1;
        };
        let Some(data) = bytes.get(data_start..data_end) else {
            return 1;
        };
        if kind == b"EXIF" {
            return exif_orientation(data);
        }
        // RIFF chunks are padded to an even length.
        pos = data_end + (length & 1);
    }
    1
}

/// Reads an EXIF block that may still carry its `Exif\0\0` marker.
pub(crate) fn exif_orientation(block: &[u8]) -> u8 {
    match block.strip_prefix(b"Exif\0\0") {
        Some(tiff) => tiff_orientation(tiff),
        None => tiff_orientation(block),
    }
}

/// Reads tag 274 from the first IFD of a TIFF-structured block.
fn tiff_orientation(tiff: &[u8]) -> u8 {
    let little = match tiff.get(0..4) {
        Some(header) if header == TIFF_LE => true,
        Some(header) if header == TIFF_BE => false,
        _ => return 1,
    };
    let Some(ifd) = tiff_u32(tiff, 4, little).map(|offset| offset as usize) else {
        return 1;
    };
    let Some(count) = tiff_u16(tiff, ifd, little) else {
        return 1;
    };
    for index in 0..count as usize {
        let Some(entry) = (index * IFD_ENTRY_LEN + IFD_HEADER_LEN).checked_add(ifd) else {
            return 1;
        };
        if tiff_u16(tiff, entry, little) != Some(ORIENTATION_TAG) {
            continue;
        }
        let field_type = tiff_u16(tiff, entry + 2, little);
        let value_count = tiff_u32(tiff, entry + 4, little);
        if field_type != Some(TIFF_SHORT) || value_count != Some(1) {
            return 1;
        }
        return match tiff_u16(tiff, entry + 8, little) {
            Some(value) if (1..=8).contains(&value) => value as u8,
            _ => 1,
        };
    }
    1
}

/// Reads a two-byte TIFF number, never past the end.
fn tiff_u16(tiff: &[u8], at: usize, little: bool) -> Option<u16> {
    let bytes = tiff.get(at..at.checked_add(2)?)?;
    let pair = [*bytes.first()?, *bytes.get(1)?];
    Some(if little {
        u16::from_le_bytes(pair)
    } else {
        u16::from_be_bytes(pair)
    })
}

/// Reads a four-byte TIFF number, never past the end.
fn tiff_u32(tiff: &[u8], at: usize, little: bool) -> Option<u32> {
    let bytes = tiff.get(at..at.checked_add(4)?)?;
    let quad = [
        *bytes.first()?,
        *bytes.get(1)?,
        *bytes.get(2)?,
        *bytes.get(3)?,
    ];
    Some(if little {
        u32::from_le_bytes(quad)
    } else {
        u32::from_be_bytes(quad)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Channels;

    /// A 3x2 image whose pixel `i` holds `[i, i, i]`.
    fn image() -> Pixels {
        let data = (0..6u8).flat_map(|i| [i, i, i]).collect();
        Pixels::new(3, 2, Channels::Rgb, data).expect("3x2 rgb")
    }

    fn values(pixels: &Pixels) -> Vec<u8> {
        pixels.data.iter().step_by(3).copied().collect()
    }

    #[test]
    fn orientations_1_to_4_work_on_the_same_buffer() {
        for orientation in 1..=4 {
            let pixels = image();
            let at = pixels.data.as_ptr();
            let turned = apply(pixels, orientation);
            assert_eq!(
                turned.data.as_ptr(),
                at,
                "orientation {orientation} copied the pixels"
            );
        }
        for orientation in 5..=8 {
            let pixels = image();
            let at = pixels.data.as_ptr();
            let turned = apply(pixels, orientation);
            assert_ne!(
                turned.data.as_ptr(),
                at,
                "orientation {orientation} writes a new buffer"
            );
        }
    }

    #[test]
    fn orientations_1_to_4_keep_the_size() {
        assert_eq!(values(&apply(image(), 1)), [0, 1, 2, 3, 4, 5]);
        assert_eq!(values(&apply(image(), 2)), [2, 1, 0, 5, 4, 3]);
        assert_eq!(values(&apply(image(), 3)), [5, 4, 3, 2, 1, 0]);
        assert_eq!(values(&apply(image(), 4)), [3, 4, 5, 0, 1, 2]);
    }

    #[test]
    fn orientations_5_to_8_swap_the_size() {
        let transposed = apply(image(), 5);
        assert_eq!((transposed.width, transposed.height), (2, 3));
        assert_eq!(values(&transposed), [0, 3, 1, 4, 2, 5]);

        let rotated = apply(image(), 6);
        assert_eq!((rotated.width, rotated.height), (2, 3));
        assert_eq!(values(&rotated), [3, 0, 4, 1, 5, 2]);

        assert_eq!(values(&apply(image(), 7)), [5, 2, 4, 1, 3, 0]);
        assert_eq!(values(&apply(image(), 8)), [2, 5, 1, 4, 0, 3]);
    }

    #[test]
    fn a_bad_orientation_is_ignored() {
        assert_eq!(values(&apply(image(), 0)), [0, 1, 2, 3, 4, 5]);
        assert_eq!(values(&apply(image(), 9)), [0, 1, 2, 3, 4, 5]);
    }

    /// Builds a TIFF header with one IFD entry, `tag` = `value`.
    fn tiff(be: bool, value: u16, tag: u16) -> Vec<u8> {
        let mut out = Vec::new();
        let u16b = |v: u16| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        let u32b = |v: u32| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        out.extend_from_slice(if be { b"MM\x00\x2a" } else { b"II\x2a\x00" });
        out.extend_from_slice(&u32b(8)); // IFD at 8
        out.extend_from_slice(&u16b(1)); // one entry
        out.extend_from_slice(&u16b(tag));
        out.extend_from_slice(&u16b(3)); // SHORT
        out.extend_from_slice(&u32b(1));
        out.extend_from_slice(&u16b(value));
        out.extend_from_slice(&u16b(0));
        out.extend_from_slice(&u32b(0)); // no next IFD
        out
    }

    fn jpeg_with_exif(block: &[u8]) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        out.extend_from_slice(&[0xff, 0xe1]);
        out.extend_from_slice(&((block.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(block);
        out.extend_from_slice(&[0xff, 0xd9]);
        out
    }

    #[test]
    fn a_jpeg_with_exif_in_both_byte_orders() {
        for be in [false, true] {
            let mut block = b"Exif\x00\x00".to_vec();
            block.extend_from_slice(&tiff(be, 6, 274));
            assert_eq!(orientation(Format::Jpeg, &jpeg_with_exif(&block)), 6);
        }
    }

    #[test]
    fn a_png_exif_chunk_and_a_webp_exif_chunk() {
        let exif = tiff(false, 8, 274);

        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&(exif.len() as u32).to_be_bytes());
        png.extend_from_slice(b"eXIf");
        png.extend_from_slice(&exif);
        png.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(orientation(Format::Png, &png), 8);

        let mut webp = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        webp.extend_from_slice(b"EXIF");
        webp.extend_from_slice(&(exif.len() as u32).to_le_bytes());
        webp.extend_from_slice(&exif);
        assert_eq!(orientation(Format::Webp, &webp), 8);
    }

    #[test]
    fn a_tiff_reads_its_own_tag() {
        assert_eq!(orientation(Format::Tiff, &tiff(false, 3, 274)), 3);
        assert_eq!(orientation(Format::Tiff, &tiff(true, 5, 274)), 5);
    }

    #[test]
    fn a_missing_or_cut_exif_gives_one() {
        assert_eq!(
            orientation(Format::Jpeg, &jpeg_with_exif(b"Exif\x00\x00")),
            1
        );
        let mut block = b"Exif\x00\x00".to_vec();
        block.extend_from_slice(&tiff(false, 6, 274));
        for cut in 0..block.len() {
            let _ = orientation(Format::Jpeg, &jpeg_with_exif(&block[..cut]));
        }
        let full = jpeg_with_exif(&block);
        assert_eq!(orientation(Format::Jpeg, &full), 6);
        assert_eq!(
            orientation(Format::Jpeg, &jpeg_with_exif(&block[..block.len() - 9])),
            1
        );
        assert_eq!(orientation(Format::Tiff, b"II"), 1);
        // A tag with the wrong type or a value outside 1..=8 is ignored.
        assert_eq!(orientation(Format::Tiff, &tiff(false, 9, 274)), 1);
        assert_eq!(orientation(Format::Tiff, &tiff(false, 6, 275)), 1);
    }
}
