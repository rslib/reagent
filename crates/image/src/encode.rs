//! PNG and JPEG output, carrying the ICC profile over.

use std::borrow::Cow;

use crate::{Channels, Error, Format, Pixels};

/// Most samples a JPEG encoder accepts on one side.
const JPEG_MAX_SIDE: u32 = u16::MAX as u32;
/// The white a transparent sample is flattened onto.
const WHITE: u32 = 255;
/// Guess of a PNG's size against the raw samples: half, so the buffer grows a
/// few times at most rather than once per megabyte.
const PNG_CAPACITY_DIVISOR: usize = 2;
/// Guess of a JPEG's bytes per pixel at the default quality.
const JPEG_BYTES_PER_PIXEL_DIVISOR: usize = 4;

/// Encodes `pixels` as PNG, with `icc` in an `iCCP` chunk when given.
pub fn png(pixels: &Pixels, icc: Option<&[u8]>) -> Result<Vec<u8>, Error> {
    let mut info = png::Info::with_size(pixels.width, pixels.height);
    info.color_type = match pixels.channels {
        Channels::Rgb => png::ColorType::Rgb,
        Channels::Rgba => png::ColorType::Rgba,
    };
    info.bit_depth = png::BitDepth::Eight;
    info.icc_profile = icc.map(Cow::Borrowed);
    let mut out = Vec::with_capacity(pixels.data.len() / PNG_CAPACITY_DIVISOR);
    {
        let encoder =
            png::Encoder::with_info(&mut out, info).map_err(|error| failed(Format::Png, error))?;
        let mut writer = encoder
            .write_header()
            .map_err(|error| failed(Format::Png, error))?;
        writer
            .write_image_data(&pixels.data)
            .map_err(|error| failed(Format::Png, error))?;
        writer
            .finish()
            .map_err(|error| failed(Format::Png, error))?;
    }
    Ok(out)
}

/// Encodes `pixels` as JPEG, dropping alpha onto white, with `icc` in APP2
/// segments when given.
pub fn jpeg(pixels: &Pixels, quality: u8, icc: Option<&[u8]>) -> Result<Vec<u8>, Error> {
    if pixels.width > JPEG_MAX_SIDE || pixels.height > JPEG_MAX_SIDE {
        return Err(failed(Format::Jpeg, "side over 65535 pixels"));
    }
    let rgb;
    let samples = match pixels.channels {
        Channels::Rgb => &pixels.data,
        Channels::Rgba => {
            rgb = flatten_on_white(pixels);
            &rgb
        }
    };
    let pixels_count = pixels.width as usize * pixels.height as usize;
    let mut out = Vec::with_capacity(pixels_count / JPEG_BYTES_PER_PIXEL_DIVISOR);
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    if let Some(icc) = icc {
        encoder
            .add_icc_profile(icc)
            .map_err(|error| failed(Format::Jpeg, error))?;
    }
    encoder
        .encode(
            samples,
            pixels.width as u16,
            pixels.height as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .map_err(|error| failed(Format::Jpeg, error))?;
    Ok(out)
}

/// Composites RGBA samples onto white and drops the alpha channel.
fn flatten_on_white(pixels: &Pixels) -> Vec<u8> {
    let mut rgb =
        Vec::with_capacity(pixels.width as usize * pixels.height as usize * Channels::Rgb.count());
    for pixel in pixels.data.chunks_exact(Channels::Rgba.count()) {
        let alpha = u32::from(pixel.get(3).copied().unwrap_or(WHITE as u8));
        for sample in pixel.iter().take(Channels::Rgb.count()) {
            let value = (u32::from(*sample) * alpha + WHITE * (WHITE - alpha) + WHITE / 2) / WHITE;
            rgb.push(value.min(WHITE) as u8);
        }
    }
    rgb
}

/// An encode error with the encoder's own text.
fn failed(format: Format, error: impl std::fmt::Display) -> Error {
    Error::Encode {
        format,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Decoded, Limits};

    use bytes::Bytes;

    fn pixels(width: u32, height: u32, channels: Channels, color: &[u8]) -> Pixels {
        let data = color
            .iter()
            .copied()
            .cycle()
            .take(width as usize * height as usize * channels.count())
            .collect();
        Pixels::new(width, height, channels, data).expect("pixels")
    }

    fn decode(bytes: &[u8]) -> Decoded {
        let bytes = Bytes::from(bytes.to_vec());
        crate::decode(&bytes, &Limits::default()).expect("decode")
    }

    #[test]
    fn png_round_trips_rgb_and_rgba() {
        let rgb = pixels(3, 2, Channels::Rgb, &[10, 20, 30]);
        let bytes = png(&rgb, None).expect("png");
        let decoded = decode(&bytes);
        assert_eq!(decoded.pixels, rgb);

        let rgba = pixels(3, 2, Channels::Rgba, &[10, 20, 30, 40]);
        let bytes = png(&rgba, None).expect("png");
        let decoded = decode(&bytes);
        assert_eq!(decoded.pixels, rgba);
    }

    #[test]
    fn png_keeps_the_icc_profile() {
        let icc = b"an icc profile".to_vec();
        let bytes = png(&pixels(2, 2, Channels::Rgb, &[1, 2, 3]), Some(&icc)).expect("png");
        assert_eq!(decode(&bytes).icc, Some(Bytes::from(icc)));
    }

    #[test]
    fn jpeg_flattens_alpha_onto_white() {
        // A fully transparent red pixel is white after flattening.
        let bytes = jpeg(&pixels(2, 2, Channels::Rgba, &[255, 0, 0, 0]), 95, None).expect("jpeg");
        let decoded = decode(&bytes);
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        let (pixels, _) = decoded.pixels.data.as_chunks::<3>();
        for pixel in pixels {
            assert!(
                pixel.iter().all(|&value| value > 230),
                "{pixel:?} is not white"
            );
        }
    }

    #[test]
    fn jpeg_keeps_the_icc_profile_across_segments() {
        // Larger than one APP2 segment (65519 bytes) forces a split.
        let icc: Vec<u8> = (0..70_000u32).map(|value| value as u8).collect();
        let bytes = jpeg(&pixels(2, 2, Channels::Rgb, &[9, 8, 7]), 80, Some(&icc)).expect("jpeg");
        assert_eq!(decode(&bytes).icc, Some(Bytes::from(icc)));
    }
}
