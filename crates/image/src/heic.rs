//! HEIC and HEIF still images behind the `heic` feature: the system ImageIO
//! framework on macOS, the bundled decoder elsewhere.

use bytes::Bytes;

#[cfg(all(feature = "heic", not(target_os = "macos")))]
use crate::{Channels, Pixels, check_pixels};
use crate::{Decoded, Error, Format};

/// Reads the size of the primary HEIC image without decoding pixels.
#[cfg(all(feature = "heic", target_os = "macos"))]
pub(crate) fn size(bytes: &Bytes) -> Result<(u32, u32), Error> {
    crate::imageio::size(bytes).map_err(named)
}

/// Decodes the primary HEIC image; the orientation comes back for the caller
/// to apply, as it does for AVIF.
#[cfg(all(feature = "heic", target_os = "macos"))]
pub(crate) fn decode(bytes: &Bytes, max_pixels: u64) -> Result<Decoded, Error> {
    if !boxes_fit(bytes) {
        return Err(Error::Decode {
            format: Format::Heic,
            message: "the file is cut short".to_owned(),
        });
    }
    crate::imageio::decode(bytes, max_pixels).map_err(named)
}

/// Whether the file holds its picture data box and every top-level box ends
/// inside it: ImageIO draws a file cut in or before that data as black and
/// reports no error.
#[cfg(all(feature = "heic", target_os = "macos"))]
fn boxes_fit(bytes: &[u8]) -> bool {
    let mut at = 0usize;
    let mut data = false;
    while let Some(head) = at.checked_add(8).and_then(|end| bytes.get(at..end)) {
        let size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
        let size = match size {
            // The last box runs to the end of the file.
            0 => return data || &head[4..] == b"mdat",
            1 => {
                let Some(wide) = at
                    .checked_add(16)
                    .and_then(|end| bytes.get(at + 8..end))
                    .and_then(|wide| <[u8; 8]>::try_from(wide).ok())
                else {
                    return false;
                };
                u64::from_be_bytes(wide)
            }
            size => size,
        };
        let end = usize::try_from(size)
            .ok()
            .and_then(|size| at.checked_add(size));
        match end {
            Some(end) if size >= 8 && end <= bytes.len() => {
                data |= &head[4..] == b"mdat";
                at = end;
            }
            _ => return false,
        }
    }
    data
}

/// ImageIO's error under the HEIC format, which its module writes as AVIF.
#[cfg(all(feature = "heic", target_os = "macos"))]
fn named(error: Error) -> Error {
    match error {
        Error::Decode { message, .. } => Error::Decode {
            format: Format::Heic,
            message,
        },
        other => other,
    }
}

/// Reads the size of the primary HEIC image without decoding pixels.
#[cfg(all(feature = "heic", not(target_os = "macos")))]
pub(crate) fn size(bytes: &Bytes) -> Result<(u32, u32), Error> {
    let info = heic_decoder::probe(bytes).map_err(failed)?;
    let width = u32::try_from(info.width).map_err(|_| failed("width does not fit"))?;
    let height = u32::try_from(info.height).map_err(|_| failed("height does not fit"))?;
    Ok((width, height))
}

/// Reports HEIC as unreadable when its feature is off.
#[cfg(not(feature = "heic"))]
pub(crate) fn size(_bytes: &Bytes) -> Result<(u32, u32), Error> {
    Err(Error::Unsupported(Format::Heic))
}

/// Decodes the primary HEIC image, applying the container's own rotation and
/// mirroring, with its ICC profile.
#[cfg(all(feature = "heic", not(target_os = "macos")))]
pub(crate) fn decode(bytes: &Bytes, max_pixels: u64) -> Result<Decoded, Error> {
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
pub(crate) fn decode(_bytes: &Bytes, _max_pixels: u64) -> Result<Decoded, Error> {
    Err(Error::Unsupported(Format::Heic))
}

/// A decode error with the decoder's own text.
#[cfg(all(feature = "heic", not(target_os = "macos")))]
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
        let bytes = Bytes::from(bytes);
        assert_eq!(size(&bytes).expect("heic size"), (64, 48));
        let decoded = crate::decode(&bytes, &Limits::default()).expect("heic");
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
            let _ = size(&Bytes::copy_from_slice(&bytes[..cut]));
            assert!(
                crate::decode(&Bytes::copy_from_slice(&bytes[..cut]), &Limits::default()).is_err()
            );
        }
    }

    /// No cut gives an image: ImageIO alone answers black for a cut in the tail.
    #[cfg(feature = "heic")]
    #[expect(clippy::disallowed_methods, reason = "a test reads its fixture")]
    #[test]
    fn a_heic_cut_at_any_length_gives_no_image() {
        let bytes = std::fs::read(HEIC).expect("fixture");
        for cut in 0..bytes.len() {
            assert!(
                crate::decode(&Bytes::copy_from_slice(&bytes[..cut]), &Limits::default()).is_err(),
                "cut at {cut}"
            );
        }
    }

    #[cfg(feature = "heic")]
    #[test]
    fn a_broken_heic_error_names_heic() {
        let bytes = Bytes::from_static(b"\x00\x00\x00\x20ftypheic\x00\x00\x00\x00 nothing");
        assert!(matches!(
            crate::decode(&bytes, &Limits::default()),
            Err(Error::Decode {
                format: Format::Heic,
                ..
            })
        ));
    }

    #[cfg(feature = "heic")]
    #[expect(clippy::disallowed_methods, reason = "a test reads its fixture")]
    #[test]
    fn a_heic_over_the_pixel_cap_is_refused() {
        let bytes = Bytes::from(std::fs::read(HEIC).expect("fixture"));
        let limits = Limits {
            max_pixels: 100,
            ..Limits::default()
        };
        assert!(crate::decode(&bytes, &limits).is_err());
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
