//! HEIC and HEIF still images behind the `heic` feature.

#[cfg(feature = "heic")]
use bytes::Bytes;

#[cfg(feature = "heic")]
use crate::{Channels, Pixels, check_pixels};
use crate::{Decoded, Error, Format};

/// Reads the size of the primary HEIC image without decoding pixels.
#[cfg(feature = "heic")]
pub(crate) fn size(bytes: &[u8]) -> Result<(u32, u32), Error> {
    let info = heic_decoder::probe(bytes).map_err(failed)?;
    let width = u32::try_from(info.width).map_err(|_| failed("width does not fit"))?;
    let height = u32::try_from(info.height).map_err(|_| failed("height does not fit"))?;
    Ok((width, height))
}

/// Reports HEIC as unreadable when its feature is off.
#[cfg(not(feature = "heic"))]
pub(crate) fn size(_bytes: &[u8]) -> Result<(u32, u32), Error> {
    Err(Error::Unsupported(Format::Heic))
}

/// Decodes the primary HEIC image, applying the container's own rotation and
/// mirroring, with its ICC profile.
#[cfg(feature = "heic")]
pub(crate) fn decode(bytes: &[u8], max_pixels: u64) -> Result<Decoded, Error> {
    let info = heic_decoder::probe(bytes).map_err(failed)?;
    let fit = |side: usize| u32::try_from(side).map_err(|_| failed("size does not fit"));
    let (width, height) = (fit(info.width)?, fit(info.height)?);
    // The decoder builds the coded picture before its crop, so both sizes
    // must pass.
    check_pixels(fit(info.coded_width)?, fit(info.coded_height)?, max_pixels)?;
    check_pixels(width, height, max_pixels)?;
    let mut image = heic_decoder::decode(bytes).map_err(failed)?;
    // Taken, not cloned: the decoder's own buffer becomes the profile.
    let icc = image.metadata.icc_profile.take().map(Bytes::from);
    let (channels, data) = if image.alpha.is_some() {
        let rgba = image.to_rgba8().map_err(failed)?;
        (Channels::Rgba, rgba.data)
    } else {
        let rgb = image.to_rgb8().map_err(failed)?;
        (Channels::Rgb, rgb.data)
    };
    let pixels = Pixels::new(width, height, channels, data).ok_or_else(|| failed("pixel count"))?;
    Ok(Decoded {
        pixels,
        icc,
        orientation: 1,
        frames: 1,
        lossless: false,
    })
}

/// Reports HEIC as unreadable when its feature is off.
#[cfg(not(feature = "heic"))]
pub(crate) fn decode(_bytes: &[u8], _max_pixels: u64) -> Result<Decoded, Error> {
    Err(Error::Unsupported(Format::Heic))
}

/// A decode error with the decoder's own text.
#[cfg(feature = "heic")]
fn failed(error: impl std::fmt::Display) -> Error {
    Error::Decode {
        format: Format::Heic,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Limits;
    use bytes::Bytes;

    #[cfg(feature = "heic")]
    use crate::Channels;

    const HEIC: &str = "tests/fixtures/gradient-64x48.heic";
    #[cfg(feature = "heic")]
    const PNG: &str = "tests/fixtures/gradient-64x48.png";

    #[cfg(feature = "heic")]
    #[expect(clippy::disallowed_methods, reason = "a test reads its fixture")]
    #[test]
    fn heic_fixture_decodes_to_the_png_size() {
        let bytes = std::fs::read(HEIC).expect("fixture");
        assert_eq!(size(&bytes).expect("heic size"), (64, 48));
        let decoded = crate::decode(&Bytes::from(bytes), &Limits::default()).expect("heic");
        assert_eq!((decoded.pixels.width, decoded.pixels.height), (64, 48));
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        assert_eq!(decoded.frames, 1);
        assert_eq!(decoded.orientation, 1);
    }

    #[cfg(feature = "heic")]
    #[expect(clippy::disallowed_methods, reason = "a test reads its fixture")]
    #[test]
    fn heic_fixture_pixels_are_close_to_the_png() {
        let heic = std::fs::read(HEIC).expect("fixture");
        let png = std::fs::read(PNG).expect("fixture");
        let heic = crate::decode(&Bytes::from(heic), &Limits::default()).expect("heic");
        let png = crate::decode(&Bytes::from(png), &Limits::default()).expect("png");
        assert_eq!(heic.pixels.data.len(), png.pixels.data.len());
        let total: u64 = heic
            .pixels
            .data
            .iter()
            .zip(png.pixels.data.iter())
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum();
        let mean = total as f64 / heic.pixels.data.len() as f64;
        assert!(mean < 12.0, "mean absolute error {mean}");
    }

    #[cfg(feature = "heic")]
    #[expect(clippy::disallowed_methods, reason = "a test reads its fixture")]
    #[test]
    fn a_truncated_heic_errors() {
        let bytes = std::fs::read(HEIC).expect("fixture");
        for cut in [0, 8, bytes.len() / 2, bytes.len() - 1] {
            let _ = size(&bytes[..cut]);
            assert!(
                crate::decode(&Bytes::copy_from_slice(&bytes[..cut]), &Limits::default()).is_err()
            );
        }
    }

    #[cfg(not(feature = "heic"))]
    #[expect(clippy::disallowed_methods, reason = "a test reads its fixture")]
    #[test]
    fn without_the_feature_heic_is_unreadable() {
        let bytes = std::fs::read(HEIC).expect("fixture");
        assert_eq!(
            crate::decode(&Bytes::from(bytes), &Limits::default()),
            Err(Error::Unsupported(Format::Heic))
        );
    }
}
