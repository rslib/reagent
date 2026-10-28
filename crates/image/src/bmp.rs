//! BITMAPINFOHEADER bitmaps: 24 and 32 bit uncompressed, 8-bit palette.

use crate::{Channels, Decoded, Error, Format, Pixels, check_pixels, le_u16, le_u32};

/// Bytes in a `BITMAPFILEHEADER`.
const FILE_HEADER_LEN: usize = 14;
/// Smallest `BITMAPINFOHEADER` this decoder reads.
const INFO_HEADER_MIN: u32 = 40;
/// Bytes in one palette entry: blue, green, red, reserved.
const PALETTE_ENTRY_LEN: usize = 4;
/// Palette entries a bitmap uses when it names none.
const DEFAULT_PALETTE_LEN: u32 = 256;
/// The only compression this decoder reads: BI_RGB, no compression.
const COMPRESSION_NONE: u32 = 0;
/// 8-bit pixels index a palette.
const BITS_PALETTE: u16 = 8;
/// 24-bit pixels are blue, green, red.
const BITS_RGB: u16 = 24;
/// 32-bit pixels are blue, green, red, alpha.
const BITS_RGBA: u16 = 32;
/// Samples per output pixel.
const RGB_SAMPLES: usize = 3;

/// The fields of a bitmap header this decoder needs.
struct Header {
    width: u32,
    height: u32,
    top_down: bool,
    bits: u16,
    offset: usize,
    palette_len: u32,
    /// Where the palette starts: right after the info header, whatever its
    /// version.
    palette_start: usize,
    stride: usize,
}

/// Reads the file and info headers of a bitmap.
fn header(bytes: &[u8]) -> Result<Header, Error> {
    if bytes.get(0..2) != Some(b"BM") {
        return Err(bad("missing BM signature"));
    }
    let offset = le_u32(bytes, 10).ok_or_else(|| bad("cut file header"))? as usize;
    let info_len = le_u32(bytes, 14).ok_or_else(|| bad("cut info header"))?;
    if info_len < INFO_HEADER_MIN {
        return Err(bad("not a BITMAPINFOHEADER"));
    }
    let info_end = FILE_HEADER_LEN
        .checked_add(info_len as usize)
        .ok_or_else(|| bad("info header length"))?;
    if info_end > bytes.len() {
        return Err(bad("cut info header"));
    }
    let width = le_u32(bytes, 18).ok_or_else(|| bad("cut width"))? as i32;
    let height = le_u32(bytes, 22).ok_or_else(|| bad("cut height"))? as i32;
    if le_u16(bytes, 26) != Some(1) {
        return Err(bad("bad plane count"));
    }
    let bits = le_u16(bytes, 28).ok_or_else(|| bad("cut bit depth"))?;
    if le_u32(bytes, 30) != Some(COMPRESSION_NONE) {
        return Err(bad("compressed bitmap"));
    }
    if !matches!(bits, BITS_PALETTE | BITS_RGB | BITS_RGBA) {
        return Err(bad("unsupported bit depth"));
    }
    if width <= 0 || height == 0 {
        return Err(bad("empty bitmap"));
    }
    let palette_len = if bits == BITS_PALETTE {
        match le_u32(bytes, 46) {
            Some(0) | None => DEFAULT_PALETTE_LEN,
            Some(len) if len <= DEFAULT_PALETTE_LEN => len,
            Some(_) => return Err(bad("palette too long")),
        }
    } else {
        0
    };
    let palette_end = info_end
        .checked_add(palette_len as usize * PALETTE_ENTRY_LEN)
        .ok_or_else(|| bad("palette length"))?;
    if palette_end > bytes.len() || offset > bytes.len() {
        return Err(bad("cut palette"));
    }
    let width = width as u32;
    let stride = (bits as usize)
        .checked_mul(width as usize)
        .ok_or_else(|| bad("bitmap width"))?
        .div_ceil(32)
        .checked_mul(4)
        .ok_or_else(|| bad("bitmap stride"))?;
    let top_down = height < 0;
    let height = height.unsigned_abs();
    Ok(Header {
        width,
        height,
        top_down,
        bits,
        offset,
        palette_len,
        palette_start: info_end,
        stride,
    })
}

/// Decodes an uncompressed bitmap to RGB or RGBA.
pub(crate) fn decode(bytes: &[u8], max_pixels: u64) -> Result<Decoded, Error> {
    let header = header(bytes)?;
    check_pixels(header.width, header.height, max_pixels)?;
    check_pixel_data(bytes, &header)?;
    let pixels = match header.bits {
        BITS_RGB => rgb24(bytes, &header)?,
        BITS_RGBA => rgba32(bytes, &header)?,
        BITS_PALETTE => palette8(bytes, &header)?,
        _ => return Err(bad("unsupported bit depth")),
    };
    Ok(Decoded {
        pixels,
        icc: None,
        orientation: 1,
        frames: 1,
        lossless: true,
    })
}

/// Rejects a bitmap whose pixel rows run past the end of the file.
fn check_pixel_data(bytes: &[u8], header: &Header) -> Result<(), Error> {
    let data_end = header
        .offset
        .checked_add(
            header
                .stride
                .checked_mul(header.height as usize)
                .ok_or_else(|| bad("bitmap height"))?,
        )
        .ok_or_else(|| bad("bitmap size"))?;
    if data_end > bytes.len() {
        return Err(bad("cut pixel data"));
    }
    Ok(())
}

/// Reads the width and height of a bitmap without touching its pixels.
pub(crate) fn size(bytes: &[u8]) -> Result<(u32, u32), Error> {
    let header = header(bytes)?;
    Ok((header.width, header.height))
}

/// The source row for an output row, in the file's own order.
fn source_row(header: &Header, row: u32) -> usize {
    if header.top_down {
        row as usize
    } else {
        (header.height - 1 - row) as usize
    }
}

/// Reads the samples of one source row.
fn source_samples<'a>(
    bytes: &'a [u8],
    header: &Header,
    row: u32,
    samples: usize,
    channels: usize,
) -> Option<&'a [u8]> {
    let start = header
        .offset
        .checked_add(source_row(header, row).checked_mul(header.stride)?)?;
    bytes.get(start..start.checked_add(samples.checked_mul(channels)?)?)
}

/// Copies one blue-green-red row as red-green-blue into `out`.
fn push_bgr24(bytes: &[u8], header: &Header, row: u32, out: &mut [u8]) -> Result<(), Error> {
    let samples = source_samples(bytes, header, row, header.width as usize, RGB_SAMPLES)
        .ok_or_else(|| bad("cut pixel data"))?;
    let (pixels, _) = samples.as_chunks::<RGB_SAMPLES>();
    let (targets, _) = out.as_chunks_mut::<RGB_SAMPLES>();
    for (pixel, target) in pixels.iter().zip(targets.iter_mut()) {
        let [blue, green, red] = *pixel;
        *target = [red, green, blue];
    }
    Ok(())
}

/// Copies one 24-bit bottom-up or top-down bitmap.
fn rgb24(bytes: &[u8], header: &Header) -> Result<Pixels, Error> {
    let len = output_len(header, RGB_SAMPLES)?;
    let row_len = header.width as usize * RGB_SAMPLES;
    let mut data = vec![0u8; len];
    for (row, target) in data.chunks_exact_mut(row_len).enumerate() {
        push_bgr24(bytes, header, row as u32, target)?;
    }
    Pixels::new(header.width, header.height, Channels::Rgb, data).ok_or_else(|| bad("pixel count"))
}

/// Copies one 32-bit bitmap, keeping alpha only when the file sets any.
fn rgba32(bytes: &[u8], header: &Header) -> Result<Pixels, Error> {
    let mut has_alpha = false;
    for row in 0..header.height {
        let samples = source_samples(bytes, header, row, header.width as usize, 4)
            .ok_or_else(|| bad("cut pixel data"))?;
        let (pixels, _) = samples.as_chunks::<4>();
        if pixels.iter().any(|pixel| pixel[3] != 0) {
            has_alpha = true;
            break;
        }
    }
    let channels = if has_alpha {
        Channels::Rgba
    } else {
        Channels::Rgb
    };
    let count = channels.count();
    let len = output_len(header, count)?;
    let row_len = header.width as usize * count;
    let mut data = vec![0u8; len];
    for (row, target) in data.chunks_exact_mut(row_len).enumerate() {
        let samples = source_samples(bytes, header, row as u32, header.width as usize, 4)
            .ok_or_else(|| bad("cut pixel data"))?;
        let (pixels, _) = samples.as_chunks::<4>();
        for (pixel, out) in pixels.iter().zip(target.chunks_exact_mut(count)) {
            let [blue, green, red, alpha] = *pixel;
            out[0] = red;
            out[1] = green;
            out[2] = blue;
            if let Some(last) = out.get_mut(RGB_SAMPLES) {
                *last = alpha;
            }
        }
    }
    Pixels::new(header.width, header.height, channels, data).ok_or_else(|| bad("pixel count"))
}

/// Expands one 8-bit palette bitmap to RGB.
fn palette8(bytes: &[u8], header: &Header) -> Result<Pixels, Error> {
    let palette_start = header.palette_start;
    let palette_end = palette_start
        .checked_add(header.palette_len as usize * PALETTE_ENTRY_LEN)
        .ok_or_else(|| bad("palette length"))?;
    let palette = bytes
        .get(palette_start..palette_end)
        .ok_or_else(|| bad("cut palette"))?;
    let len = output_len(header, RGB_SAMPLES)?;
    let row_len = header.width as usize * RGB_SAMPLES;
    let mut data = vec![0u8; len];
    for (row, target) in data.chunks_exact_mut(row_len).enumerate() {
        let indices = source_samples(bytes, header, row as u32, header.width as usize, 1)
            .ok_or_else(|| bad("cut pixel data"))?;
        let (targets, _) = target.as_chunks_mut::<RGB_SAMPLES>();
        for (index, out) in indices.iter().zip(targets.iter_mut()) {
            if *index as u32 >= header.palette_len {
                return Err(bad("palette index out of range"));
            }
            let at = *index as usize * PALETTE_ENTRY_LEN;
            let entry = palette
                .get(at..at + RGB_SAMPLES)
                .ok_or_else(|| bad("cut palette"))?;
            match entry {
                [blue, green, red] => *out = [*red, *green, *blue],
                _ => return Err(bad("cut palette")),
            }
        }
    }
    Pixels::new(header.width, header.height, Channels::Rgb, data).ok_or_else(|| bad("pixel count"))
}

/// The byte count of the decoded image, checked for overflow.
fn output_len(header: &Header, channels: usize) -> Result<usize, Error> {
    header
        .width
        .checked_mul(header.height)
        .and_then(|pixels| (pixels as usize).checked_mul(channels))
        .ok_or_else(|| bad("pixel count"))
}

/// A decode error with one line of context.
fn bad(message: &'static str) -> Error {
    Error::Decode {
        format: Format::Bmp,
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Channels;

    const FILE_HEADER_LEN: usize = 14;
    const INFO_HEADER_LEN: usize = 40;

    /// One pixel of a synthetic 3x2 bitmap: `(r, g, b)` from its position.
    fn sample(x: u32, y: u32) -> [u8; 3] {
        [(x + 1) as u8 * 10, (y + 1) as u8 * 20, 30]
    }

    fn expect_rgb() -> Vec<u8> {
        (0..2)
            .flat_map(|y| (0..3).flat_map(move |x| sample(x, y)))
            .collect()
    }

    fn header(width: i32, height: i32, bpp: u16, palette: &[[u8; 3]], pixels: &[u8]) -> Vec<u8> {
        let stride = ((bpp as usize * width.unsigned_abs() as usize).div_ceil(32)) * 4;
        let offset = FILE_HEADER_LEN + INFO_HEADER_LEN + palette.len() * 4;
        let file_size = offset + stride * height.unsigned_abs() as usize;
        let mut out = Vec::new();
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(file_size as u32).to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        out.extend_from_slice(&(INFO_HEADER_LEN as u32).to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&bpp.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
        out.extend_from_slice(&2835i32.to_le_bytes());
        out.extend_from_slice(&2835i32.to_le_bytes());
        out.extend_from_slice(&(palette.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for rgb in palette {
            out.extend_from_slice(&[rgb[2], rgb[1], rgb[0], 0]);
        }
        out.extend_from_slice(pixels);
        out
    }

    /// A 3x2 bitmap of `sample` colors, bottom-up unless `top_down`.
    fn bmp_24(top_down: bool) -> Vec<u8> {
        let stride = 12;
        let mut rows = Vec::new();
        let order: Vec<u32> = if top_down {
            (0..2).collect()
        } else {
            (0..2).rev().collect()
        };
        for y in order {
            for x in 0..3 {
                let [r, g, b] = sample(x, y);
                rows.extend_from_slice(&[b, g, r]);
            }
            rows.extend_from_slice(&[0, 0, 0]);
        }
        assert_eq!(rows.len(), stride * 2);
        header(3, if top_down { -2 } else { 2 }, 24, &[], &rows)
    }

    /// A 2x2 32-bit bitmap whose pixels keep their alpha bytes.
    fn bmp_32(alpha: [u8; 4]) -> Vec<u8> {
        let pixels = [
            [10, 20, 30, alpha[0]],
            [40, 50, 60, alpha[1]],
            [70, 80, 90, alpha[2]],
            [100, 110, 120, alpha[3]],
        ];
        let mut rows = Vec::new();
        for pair in pixels.chunks(2).rev() {
            for pixel in pair {
                rows.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        }
        header(2, 2, 32, &[], &rows)
    }

    #[test]
    fn bmp_24_bottom_up_and_top_down() {
        for top_down in [false, true] {
            let decoded = decode(&bmp_24(top_down), u64::MAX).expect("24-bit bmp");
            assert_eq!((decoded.pixels.width, decoded.pixels.height), (3, 2));
            assert_eq!(decoded.pixels.channels, Channels::Rgb);
            assert_eq!(decoded.pixels.data, expect_rgb());
            assert_eq!(decoded.orientation, 1);
            assert_eq!(decoded.frames, 1);
        }
    }

    #[test]
    fn bmp_32_keeps_alpha_when_the_file_has_some() {
        let decoded = decode(&bmp_32([255, 128, 0, 64]), u64::MAX).expect("32-bit bmp");
        assert_eq!(decoded.pixels.channels, Channels::Rgba);
        assert_eq!(
            decoded.pixels.data,
            [
                10, 20, 30, 255, 40, 50, 60, 128, 70, 80, 90, 0, 100, 110, 120, 64
            ]
        );
    }

    #[test]
    fn bmp_32_without_alpha_is_rgb() {
        let decoded = decode(&bmp_32([0, 0, 0, 0]), u64::MAX).expect("32-bit bmp");
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        assert_eq!(decoded.pixels.data.len(), 2 * 2 * 3);
    }

    #[test]
    fn a_v5_header_reads_the_palette_after_it() {
        const V5_LEN: usize = 124;
        let palette = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [9, 9, 9]];
        let pixels = [0u8, 1, 2, 0, 3, 1, 0, 0];
        let v3 = header(3, 2, 8, &palette, &pixels);
        let grow = V5_LEN - INFO_HEADER_LEN;
        let info_end = FILE_HEADER_LEN + INFO_HEADER_LEN;
        let mut v5 = v3[..info_end].to_vec();
        v5.resize(info_end + grow, 0);
        v5.extend_from_slice(&v3[info_end..]);
        let patch = |bytes: &mut Vec<u8>, at: usize, value: u32| {
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        };
        let file_size = v5.len() as u32;
        patch(&mut v5, 2, file_size);
        patch(&mut v5, 10, (info_end + grow + palette.len() * 4) as u32);
        patch(&mut v5, 14, V5_LEN as u32);
        let old = decode(&v3, u64::MAX).expect("v3 bmp");
        let new = decode(&v5, u64::MAX).expect("v5 bmp");
        assert_eq!(new.pixels, old.pixels);
    }

    #[test]
    fn bmp_8_bit_palette() {
        let palette = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [9, 9, 9]];
        // 3x2, stride 4: indices 0,1,2 then padding.
        let pixels = [0u8, 1, 2, 0, 3, 1, 0, 0];
        let decoded = decode(&header(3, 2, 8, &palette, &pixels), u64::MAX).expect("8-bit bmp");
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        assert_eq!(
            decoded.pixels.data,
            [
                9, 9, 9, 0, 255, 0, 255, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255
            ]
        );
    }

    #[test]
    fn bmp_size_reads_the_header_only() {
        assert_eq!(size(&bmp_24(false)).expect("size"), (3, 2));
        assert_eq!(size(&bmp_24(true)).expect("size"), (3, 2));
    }

    #[test]
    fn a_truncated_bmp_errors() {
        let full = bmp_24(false);
        for cut in [0, 2, 14, 30, 54, 60, full.len() - 1] {
            let _ = decode(&full[..cut], u64::MAX);
        }
        assert!(decode(&full[..full.len() - 1], u64::MAX).is_err());
    }

    #[test]
    fn a_huge_header_errors_before_allocating() {
        let file = header(60000, 60000, 24, &[], &[]);
        assert!(matches!(
            decode(&file, 100_000_000),
            Err(Error::TooManyPixels {
                width: 60000,
                height: 60000
            })
        ));
    }

    #[test]
    fn compressed_or_16_bit_bmps_error() {
        let mut file = bmp_24(false);
        // Compression 3 (BI_BITFIELDS) is outside what this decoder reads.
        file[30..34].copy_from_slice(&3u32.to_le_bytes());
        assert!(decode(&file, u64::MAX).is_err());
    }

    #[test]
    fn an_out_of_range_palette_index_errors() {
        let palette = [[1, 2, 3], [4, 5, 6]];
        let pixels = [0u8, 1, 2, 0, 0, 0, 0, 0];
        assert!(decode(&header(3, 2, 8, &palette, &pixels), u64::MAX).is_err());
    }

    #[test]
    fn pixels_is_built_with_the_right_size() {
        let pixels = Pixels::new(2, 2, Channels::Rgb, vec![0; 12]).expect("2x2 rgb");
        assert_eq!(pixels.stride(), 6);
    }
}
