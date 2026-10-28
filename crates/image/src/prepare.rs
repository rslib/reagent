//! Turns any supported image into one a model accepts, within the limits.

use bytes::Bytes;

use crate::{Channels, Error, Format, Limits, Pixels};

/// JPEG quality used when nothing forces PNG.
const JPEG_DEFAULT_QUALITY: u8 = 85;
/// JPEG qualities tried in turn when the first encoding is over the limit.
const JPEG_FALLBACK_QUALITIES: [u8; 3] = [75, 60, 45];

/// An image a model can take, with what changed on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// PNG, JPEG, GIF or WebP.
    pub format: Format,
    /// The encoded bytes, the input's own buffer when it passed through.
    pub data: Bytes,
    /// Width of the output.
    pub width: u32,
    /// Height of the output.
    pub height: u32,
    /// Width and height of the source after its orientation was applied.
    pub source: (u32, u32),
    /// One short line about the change, or `None` when only the format
    /// changed.
    pub note: Option<String>,
}

/// Prepares `bytes` for a model: pass it through when it already fits,
/// otherwise orient, resize and encode within `limits`.
pub fn prepare(bytes: Bytes, limits: &Limits) -> Result<Prepared, Error> {
    prepare_with_threads(bytes, limits, None)
}

/// [`prepare`] with an explicit resize thread count, so a measurement can
/// compare one thread with the default.
fn prepare_with_threads(
    bytes: Bytes,
    limits: &Limits,
    threads: Option<usize>,
) -> Result<Prepared, Error> {
    let format = crate::sniff(&bytes).ok_or(Error::UnknownFormat)?;
    let probe = crate::decode::probe(format, &bytes, limits.max_pixels)?;
    let orientation = crate::orientation(format, &bytes);
    if format.is_model_input()
        && probe.frames == 1
        && orientation == 1
        && probe.width <= limits.max_side
        && probe.height <= limits.max_side
        && crate::base64_len(bytes.len()) <= limits.max_base64
    {
        // The input already is what the model takes: hand its buffer back.
        return Ok(Prepared {
            format,
            data: bytes,
            width: probe.width,
            height: probe.height,
            source: (probe.width, probe.height),
            note: None,
        });
    }
    let decoded = crate::decode(&bytes, limits)?;
    let pixels = crate::apply(decoded.pixels, decoded.orientation);
    let source = (pixels.width, pixels.height);
    let (width, height) = crate::fit(pixels.width, pixels.height, limits.max_side);
    let pixels = if (width, height) == source {
        pixels
    } else if let Some(threads) = threads {
        crate::resize_with_threads(pixels, width, height, threads)
    } else {
        crate::resize(pixels, width, height)
    };
    // PNG keeps the exact samples of a source that had no loss to begin with.
    let prefer_png = pixels.channels == Channels::Rgba
        || matches!(format, Format::Png | Format::Gif | Format::Bmp)
        || (format == Format::Webp && decoded.lossless);
    let (out_format, data) = encode_within(&pixels, decoded.icc.as_deref(), prefer_png, limits)?;
    Ok(Prepared {
        format: out_format,
        data,
        width,
        height,
        source,
        note: note(source, (width, height), decoded.frames),
    })
}

/// Builds the one-line note about a size change and an animation.
fn note(source: (u32, u32), size: (u32, u32), frames: u32) -> Option<String> {
    let mut parts = Vec::new();
    if source != size {
        parts.push(format!(
            "{}x{} -> {}x{}",
            source.0, source.1, size.0, size.1
        ));
    } else if frames > 1 {
        parts.push(format!("{}x{}", size.0, size.1));
    }
    if frames > 1 {
        parts.push(format!("frame 1 of {frames}"));
    }
    (!parts.is_empty()).then(|| format!("[image {}]", parts.join(", ")))
}

/// Encodes within the byte limit, first the format the source calls for,
/// then the other, then lower JPEG qualities.
fn encode_within(
    pixels: &Pixels,
    icc: Option<&[u8]>,
    prefer_png: bool,
    limits: &Limits,
) -> Result<(Format, Bytes), Error> {
    let fits = |data: &[u8]| crate::base64_len(data.len()) <= limits.max_base64;
    let mut smallest: Option<(Format, Vec<u8>)> = None;
    if prefer_png {
        let data = crate::encode::png(pixels, icc)?;
        if fits(&data) {
            return Ok((Format::Png, Bytes::from(data)));
        }
        keep_smaller(&mut smallest, Format::Png, data);
        let data = crate::encode::jpeg(pixels, JPEG_DEFAULT_QUALITY, icc)?;
        if fits(&data) {
            return Ok((Format::Jpeg, Bytes::from(data)));
        }
        keep_smaller(&mut smallest, Format::Jpeg, data);
    } else {
        let data = crate::encode::jpeg(pixels, JPEG_DEFAULT_QUALITY, icc)?;
        if fits(&data) {
            return Ok((Format::Jpeg, Bytes::from(data)));
        }
        keep_smaller(&mut smallest, Format::Jpeg, data);
        let data = crate::encode::png(pixels, icc)?;
        if fits(&data) {
            return Ok((Format::Png, Bytes::from(data)));
        }
        keep_smaller(&mut smallest, Format::Png, data);
    }
    for quality in JPEG_FALLBACK_QUALITIES {
        let data = crate::encode::jpeg(pixels, quality, icc)?;
        if fits(&data) {
            return Ok((Format::Jpeg, Bytes::from(data)));
        }
        keep_smaller(&mut smallest, Format::Jpeg, data);
    }
    match smallest {
        Some((_, data)) => Err(Error::TooLarge {
            base64: crate::base64_len(data.len()),
            limit: limits.max_base64,
        }),
        None => Err(Error::Encode {
            format: Format::Jpeg,
            message: "no encoding was tried".to_string(),
        }),
    }
}

/// Keeps the shorter of `data` and what `smallest` already holds.
fn keep_smaller(smallest: &mut Option<(Format, Vec<u8>)>, format: Format, data: Vec<u8>) {
    let shorter = smallest
        .as_ref()
        .is_none_or(|(_, best)| data.len() < best.len());
    if shorter {
        *smallest = Some((format, data));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Limits;

    use bytes::Bytes;

    fn limits() -> Limits {
        Limits::default()
    }

    /// Encodes an 8-bit RGB JPEG.
    fn jpeg_bytes(width: u32, height: u32, data: &[u8], quality: u8) -> Vec<u8> {
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, quality)
            .encode(
                data,
                width as u16,
                height as u16,
                jpeg_encoder::ColorType::Rgb,
            )
            .expect("jpeg");
        out
    }

    fn png_bytes(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            writer.write_image_data(data).expect("png data");
            writer.finish().expect("png finish");
        }
        out
    }

    #[test]
    fn a_small_png_passes_through_byte_for_byte() {
        let bytes = png_bytes(2, 2, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let prepared = prepare(Bytes::from(bytes.clone()), &limits()).expect("prepare");
        assert_eq!(prepared.format, Format::Png);
        assert_eq!(prepared.data.as_ref(), bytes.as_slice());
        assert_eq!((prepared.width, prepared.height), (2, 2));
        assert_eq!(prepared.source, (2, 2));
        assert_eq!(prepared.note, None);
    }

    #[test]
    fn an_unknown_format_errors() {
        assert_eq!(
            prepare(Bytes::from_static(b"nothing here"), &limits()),
            Err(Error::UnknownFormat)
        );
    }

    #[test]
    fn a_wide_png_is_resized_and_the_note_says_so() {
        let data = vec![7u8; 2500 * 4 * 3];
        let bytes = png_bytes(2500, 4, &data);
        let prepared = prepare(Bytes::from(bytes), &limits()).expect("prepare");
        assert_eq!(prepared.format, Format::Png);
        assert_eq!((prepared.width, prepared.height), (2000, 3));
        assert_eq!(prepared.source, (2500, 4));
        assert_eq!(prepared.note.as_deref(), Some("[image 2500x4 -> 2000x3]"));
    }

    #[test]
    fn a_png_that_no_encoding_fits_gives_the_too_large_error() {
        let data: Vec<u8> = (0..40u32 * 40 * 3).map(|i| (i * 37) as u8).collect();
        let bytes = png_bytes(40, 40, &data);
        let tight = Limits {
            max_base64: 200,
            ..limits()
        };
        // 40x40 of noise cannot fit in 200 base64 bytes at any quality.
        match prepare(Bytes::from(bytes), &tight) {
            Err(Error::TooLarge { base64, limit }) => {
                assert_eq!(limit, 200);
                assert!(base64 > limit);
            }
            other => panic!("unexpected result {other:?}"),
        }
    }

    #[test]
    #[ignore = "a one-off timing measurement for the report, not an assertion"]
    fn measure_a_4032x3024_jpeg_on_one_thread_and_on_the_default() {
        const WIDTH: u32 = 4032;
        const HEIGHT: u32 = 3024;
        const RUNS: usize = 3;
        let data: Vec<u8> = (0..HEIGHT)
            .flat_map(|y| (0..WIDTH).flat_map(move |x| [(x / 16) as u8, (y / 16) as u8, 90]))
            .collect();
        let source = Bytes::from(jpeg_bytes(WIDTH, HEIGHT, &data, 80));
        for (label, threads) in [("1 thread", Some(1)), ("default threads", None)] {
            let mut best = std::time::Duration::MAX;
            let mut out = 0;
            for _ in 0..RUNS {
                let start = std::time::Instant::now();
                let prepared =
                    prepare_with_threads(source.clone(), &limits(), threads).expect("prepare");
                best = best.min(start.elapsed());
                out = prepared.data.len();
                assert_eq!((prepared.width, prepared.height), (2000, 1500));
            }
            eprintln!(
                "prepare {}x{} JPEG, {label}: {best:?} of {RUNS}, {} bytes in, {out} bytes out",
                WIDTH,
                HEIGHT,
                source.len(),
            );
        }
    }
}
