//! Detects the format and decodes it to 8-bit pixels.
//!
//! Every decoder reads its header first, checks the pixel count against the
//! caller's cap, and only then allocates the pixels.

use std::io::Cursor;
use std::num::NonZeroU64;
use std::ops::Range;

use bytes::Bytes;

use crate::{Channels, Decoded, Error, Format, Limits, Pixels, be_u16, be_u32, check_pixels};

/// Bytes in the PNG signature.
const PNG_SIGNATURE_LEN: usize = 8;
/// Bytes in the IHDR chunk data.
const PNG_IHDR_LEN: u32 = 13;
/// The PNG signature.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
/// Last marker in a JPEG stream.
const JPEG_EOI: u8 = 0xd9;
/// Start of scan, after which the image data begins.
const JPEG_SOS: u8 = 0xda;
/// Marker without a length field: TEM.
const JPEG_TEM: u8 = 0x01;
/// The APP2 marker, which carries an ICC profile in a JPEG.
const JPEG_APP2: u8 = 0xe2;
/// Bytes per pixel of a decoded RGB image.
const RGB_CHANNELS: usize = 3;
/// Bytes per pixel of a decoded RGBA image.
const RGBA_CHANNELS: usize = 4;
/// The TIFF tag that carries an ICC profile.
#[cfg(feature = "tiff")]
const TIFF_ICC_TAG: u16 = 34675;
/// The TIFF field type BYTE.
#[cfg(feature = "tiff")]
const TIFF_TYPE_BYTE: u16 = 1;
/// The TIFF field type UNDEFINED, which ICC profiles also use.
#[cfg(feature = "tiff")]
const TIFF_TYPE_UNDEFINED: u16 = 7;
/// A TIFF header starts with this when its numbers are little endian.
#[cfg(feature = "tiff")]
const TIFF_LE: [u8; 4] = [0x49, 0x49, 0x2a, 0x00];
/// A TIFF header starts with this when its numbers are big endian.
#[cfg(feature = "tiff")]
const TIFF_BE: [u8; 4] = [0x4d, 0x4d, 0x00, 0x2a];
/// The lead bytes of an APP2 segment that carries ICC bytes.
const JPEG_ICC_HEADER: &[u8; 12] = b"ICC_PROFILE\0";
/// The byte of an APP2 segment that follows the ICC lead bytes: the sequence.
const JPEG_ICC_SEQUENCE: usize = 12;
/// Bytes before the first ICC byte of an APP2 segment.
const JPEG_ICC_DATA: usize = 14;

/// What a header says before any pixels are decoded.
pub(crate) struct Probe {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
}

/// Decodes `bytes` honouring the pixel cap in `limits`. Every ICC profile
/// comes back as a slice of `bytes` when it sits in one piece there.
pub fn decode(bytes: &Bytes, limits: &Limits) -> Result<Decoded, Error> {
    let format = crate::sniff(bytes).ok_or(Error::UnknownFormat)?;
    probe(format, bytes, limits.max_pixels)?;
    match format {
        Format::Png => png(bytes),
        Format::Jpeg => jpeg(bytes),
        Format::Gif => gif(bytes, limits.max_pixels),
        Format::Webp => webp(bytes),
        Format::Bmp => crate::bmp::decode(bytes, limits.max_pixels),
        Format::Tiff => tiff(bytes),
        Format::Heic => crate::heic::decode(bytes, limits.max_pixels),
        Format::Avif => avif(bytes, limits.max_pixels),
    }
}

/// Decodes AVIF with the system ImageIO framework.
#[cfg(target_os = "macos")]
fn avif(bytes: &Bytes, max_pixels: u64) -> Result<Decoded, Error> {
    crate::imageio::decode(bytes, max_pixels)
}

/// Reports AVIF as unreadable off macOS, which has no built-in decoder.
#[cfg(not(target_os = "macos"))]
fn avif(_bytes: &Bytes, _max_pixels: u64) -> Result<Decoded, Error> {
    Err(Error::Unsupported(Format::Avif))
}

/// Reads the size of an AVIF image with the system ImageIO framework.
#[cfg(target_os = "macos")]
fn avif_size(bytes: &Bytes) -> Result<(u32, u32), Error> {
    crate::imageio::size(bytes)
}

/// Reports AVIF as unreadable off macOS.
#[cfg(not(target_os = "macos"))]
fn avif_size(_bytes: &Bytes) -> Result<(u32, u32), Error> {
    Err(Error::Unsupported(Format::Avif))
}

/// Reads the dimensions and frame count of a header without decoding pixels,
/// and rejects a header above `max_pixels` before anything allocates.
pub(crate) fn probe(format: Format, bytes: &Bytes, max_pixels: u64) -> Result<Probe, Error> {
    let (width, height, frames) = match format {
        Format::Png => {
            let (width, height) = png_size(bytes)?;
            (width, height, 1)
        }
        Format::Jpeg => {
            let (width, height) = jpeg_size(bytes)?;
            (width, height, 1)
        }
        Format::Gif => {
            let info = gif_info(bytes, max_pixels)?;
            (info.width, info.height, info.frames)
        }
        Format::Webp => webp_summary(bytes)?,
        Format::Bmp => {
            let (width, height) = crate::bmp::size(bytes)?;
            (width, height, 1)
        }
        Format::Tiff => {
            let (width, height) = tiff_size(bytes)?;
            (width, height, 1)
        }
        Format::Heic => {
            let (width, height) = crate::heic::size(bytes)?;
            (width, height, 1)
        }
        Format::Avif => {
            let (width, height) = avif_size(bytes)?;
            (width, height, 1)
        }
    };
    check_pixels(width, height, max_pixels)?;
    Ok(Probe {
        width,
        height,
        frames,
    })
}

/// Reads width and height from a PNG's IHDR, which must come first.
fn png_size(bytes: &[u8]) -> Result<(u32, u32), Error> {
    if bytes.get(..PNG_SIGNATURE_LEN) != Some(&PNG_SIGNATURE) {
        return Err(failed(Format::Png, "missing signature"));
    }
    let length =
        be_u32(bytes, PNG_SIGNATURE_LEN).ok_or_else(|| failed(Format::Png, "cut header"))?;
    if length != PNG_IHDR_LEN || bytes.get(12..16) != Some(b"IHDR") || bytes.get(16..24).is_none() {
        return Err(failed(Format::Png, "IHDR is not the first chunk"));
    }
    let width = be_u32(bytes, 16).ok_or_else(|| failed(Format::Png, "cut width"))?;
    let height = be_u32(bytes, 20).ok_or_else(|| failed(Format::Png, "cut height"))?;
    Ok((width, height))
}

/// Scans JPEG markers for the first start-of-frame dimensions.
fn jpeg_size(bytes: &[u8]) -> Result<(u32, u32), Error> {
    let mut pos = 2;
    while pos + 4 <= bytes.len() {
        if bytes.get(pos) != Some(&0xff) {
            return Err(failed(Format::Jpeg, "bad marker"));
        }
        let mut at = pos;
        while bytes.get(at) == Some(&0xff) {
            at += 1;
        }
        let marker = *bytes
            .get(at)
            .ok_or_else(|| failed(Format::Jpeg, "cut marker"))?;
        if marker == JPEG_EOI || marker == JPEG_SOS {
            break;
        }
        if marker == JPEG_TEM || (0xd0..=0xd7).contains(&marker) {
            pos = at + 1;
            continue;
        }
        let length =
            be_u16(bytes, at + 1).ok_or_else(|| failed(Format::Jpeg, "cut segment"))? as usize;
        if length < 2 {
            return Err(failed(Format::Jpeg, "bad segment length"));
        }
        let data_start = at + 3;
        let data_end = at
            .checked_add(1 + length)
            .ok_or_else(|| failed(Format::Jpeg, "segment length"))?;
        if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let height = be_u16(bytes, data_start + 1)
                .ok_or_else(|| failed(Format::Jpeg, "cut frame header"))?;
            let width = be_u16(bytes, data_start + 3)
                .ok_or_else(|| failed(Format::Jpeg, "cut frame header"))?;
            return Ok((width as u32, height as u32));
        }
        if data_end > bytes.len() {
            return Err(failed(Format::Jpeg, "cut segment"));
        }
        pos = data_end;
    }
    Err(failed(Format::Jpeg, "no start of frame"))
}

/// Decodes a PNG to RGB or RGBA, with 16-bit samples cut to 8 bit.
fn png(bytes: &Bytes) -> Result<Decoded, Error> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes.as_ref()));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|error| failed(Format::Png, error))?;
    // The png crate inflates the `iCCP` chunk, so this profile is one owned
    // buffer rather than a slice of the input.
    let icc = reader
        .info()
        .icc_profile
        .as_ref()
        .map(|profile| Bytes::copy_from_slice(profile));
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| failed(Format::Png, "image too large"))?;
    let mut buffer = vec![0u8; size];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|error| failed(Format::Png, error))?;
    let len = frame.buffer_size();
    if len > buffer.len() {
        return Err(failed(Format::Png, "short output"));
    }
    // RGB and RGBA need no widening: the decoded buffer is the pixel buffer.
    buffer.truncate(len);
    let (channels, data) = match frame.color_type {
        png::ColorType::Rgb => (Channels::Rgb, buffer),
        png::ColorType::Rgba => (Channels::Rgba, buffer),
        png::ColorType::Grayscale => (Channels::Rgb, expand_gray(&buffer, 1)),
        png::ColorType::GrayscaleAlpha => (Channels::Rgba, expand_gray(&buffer, 2)),
        png::ColorType::Indexed => return Err(failed(Format::Png, "palette left unexpanded")),
    };
    let pixels = Pixels::new(frame.width, frame.height, channels, data)
        .ok_or_else(|| failed(Format::Png, "pixel count"))?;
    Ok(Decoded {
        pixels,
        icc,
        orientation: crate::orientation(Format::Png, bytes),
        frames: 1,
        lossless: true,
    })
}

/// Decodes a JPEG to RGB, or RGBA when the decoder emits alpha.
fn jpeg(bytes: &Bytes) -> Result<Decoded, Error> {
    let mut decoder = zune_jpeg::JpegDecoder::new(Cursor::new(bytes.as_ref()));
    let data = decoder
        .decode()
        .map_err(|error| failed(Format::Jpeg, error))?;
    let info = decoder
        .info()
        .ok_or_else(|| failed(Format::Jpeg, "missing header"))?;
    if info.components == 4 {
        return Err(failed(Format::Jpeg, "CMYK JPEG is not supported"));
    }
    let icc = jpeg_icc(bytes);
    let count = info.width as usize * info.height as usize;
    let (channels, data) = if data.len() == count * RGB_CHANNELS {
        (Channels::Rgb, data)
    } else if data.len() == count * RGBA_CHANNELS {
        (Channels::Rgba, data)
    } else if data.len() == count {
        (Channels::Rgb, expand_gray(&data, 1))
    } else {
        return Err(failed(Format::Jpeg, "unexpected sample count"));
    };
    let pixels = Pixels::new(info.width as u32, info.height as u32, channels, data)
        .ok_or_else(|| failed(Format::Jpeg, "pixel count"))?;
    Ok(Decoded {
        pixels,
        icc,
        orientation: crate::orientation(Format::Jpeg, bytes),
        frames: 1,
        lossless: false,
    })
}

/// What the GIF logical screen and frame control extensions say.
struct GifInfo {
    width: u32,
    height: u32,
    frames: u32,
    transparent: bool,
}

/// Counts GIF frames without decoding their pixels.
fn gif_info(bytes: &[u8], max_pixels: u64) -> Result<GifInfo, Error> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    options.skip_frame_decoding(true);
    let mut decoder = options
        .read_info(Cursor::new(bytes))
        .map_err(|error| failed(Format::Gif, error))?;
    let width = u32::from(decoder.width());
    let height = u32::from(decoder.height());
    check_pixels(width, height, max_pixels)?;
    let mut frames = 0u32;
    let mut transparent = false;
    while let Some(frame) = decoder
        .read_next_frame()
        .map_err(|error| failed(Format::Gif, error))?
    {
        frames = frames.saturating_add(1);
        transparent |= frame.transparent.is_some();
    }
    Ok(GifInfo {
        width,
        height,
        frames,
        transparent,
    })
}

/// Decodes the first GIF frame onto a transparent canvas of the screen size.
fn gif(bytes: &Bytes, max_pixels: u64) -> Result<Decoded, Error> {
    let info = gif_info(bytes, u64::MAX)?;
    if info.frames == 0 {
        return Err(failed(Format::Gif, "no frame"));
    }
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    // The decoder's own cap (50 MB) is below `max_pixels`; the frame it
    // buffers is at most one RGBA screen, which the probe already allowed.
    let frame_bytes = max_pixels.saturating_mul(RGBA_CHANNELS as u64);
    options.set_memory_limit(gif::MemoryLimit::Bytes(
        NonZeroU64::new(frame_bytes).unwrap_or(NonZeroU64::MIN),
    ));
    let mut decoder = options
        .read_info(Cursor::new(bytes))
        .map_err(|error| failed(Format::Gif, error))?;
    let frame = decoder
        .read_next_frame()
        .map_err(|error| failed(Format::Gif, error))?
        .ok_or_else(|| failed(Format::Gif, "no frame"))?;
    let canvas_len = (info.width as usize)
        .checked_mul(info.height as usize)
        .and_then(|pixels| pixels.checked_mul(RGBA_CHANNELS))
        .ok_or_else(|| failed(Format::Gif, "pixel count"))?;
    let mut canvas = vec![0u8; canvas_len];
    let frame_width = u32::from(frame.width);
    let frame_height = u32::from(frame.height);
    let left = u32::from(frame.left);
    let top = u32::from(frame.top);
    let copy_width = frame_width.min(info.width.saturating_sub(left)) as usize;
    if copy_width > 0 && frame_height > 0 {
        let row_len = frame_width as usize * RGBA_CHANNELS;
        for (y, source) in frame
            .buffer
            .chunks_exact(row_len)
            .take(frame_height as usize)
            .enumerate()
        {
            let row = top.saturating_add(y as u32);
            if row >= info.height {
                break;
            }
            let start = (row as usize * info.width as usize + left as usize) * RGBA_CHANNELS;
            if let (Some(source), Some(target)) = (
                source.get(..copy_width * RGBA_CHANNELS),
                canvas.get_mut(start..start + copy_width * RGBA_CHANNELS),
            ) {
                target.copy_from_slice(source);
            }
        }
    }
    let (channels, data) = if info.transparent {
        (Channels::Rgba, canvas)
    } else {
        // Every canvas pixel becomes three output samples: one exact buffer.
        let mut rgb = vec![0u8; info.width as usize * info.height as usize * RGB_CHANNELS];
        let (pixels, _) = canvas.as_chunks::<RGBA_CHANNELS>();
        let (targets, _) = rgb.as_chunks_mut::<RGB_CHANNELS>();
        for (pixel, target) in pixels.iter().zip(targets.iter_mut()) {
            *target = [pixel[0], pixel[1], pixel[2]];
        }
        (Channels::Rgb, rgb)
    };
    let pixels = Pixels::new(info.width, info.height, channels, data)
        .ok_or_else(|| failed(Format::Gif, "pixel count"))?;
    Ok(Decoded {
        pixels,
        icc: None,
        orientation: 1,
        frames: info.frames,
        lossless: true,
    })
}

/// Reads the canvas size and frame count of a WebP from its RIFF chunks,
/// without decoding any pixels.
fn webp_summary(bytes: &[u8]) -> Result<(u32, u32, u32), Error> {
    const RIFF_HEADER_LEN: usize = 12;
    const CHUNK_HEADER_LEN: usize = 8;
    const VP8X_SIZE: usize = 10;
    const VP8_KEYFRAME_HEADER: usize = 10;
    const VP8L_HEADER: usize = 5;
    const VP8L_SIGNATURE: u8 = 0x2f;
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WEBP") {
        return Err(failed(Format::Webp, "missing RIFF header"));
    }
    let mut canvas = None;
    let mut frames = 0u32;
    let mut pos = RIFF_HEADER_LEN;
    while let Some(kind) = bytes.get(pos..pos + 4) {
        let length = crate::le_u32(bytes, pos + 4)
            .ok_or_else(|| failed(Format::Webp, "cut chunk"))? as usize;
        let start = pos + CHUNK_HEADER_LEN;
        let end = start
            .checked_add(length)
            .ok_or_else(|| failed(Format::Webp, "chunk length"))?;
        let data = bytes
            .get(start..end)
            .ok_or_else(|| failed(Format::Webp, "cut chunk"))?;
        match kind {
            b"VP8X" if data.len() >= VP8X_SIZE => {
                let width = 1 + u32::from_le_bytes([data[4], data[5], data[6], 0]);
                let height = 1 + u32::from_le_bytes([data[7], data[8], data[9], 0]);
                canvas = Some((width, height));
            }
            b"VP8 " if canvas.is_none() && data.len() >= VP8_KEYFRAME_HEADER => {
                let width = u32::from(u16::from_le_bytes([data[6], data[7]]) & 0x3fff);
                let height = u32::from(u16::from_le_bytes([data[8], data[9]]) & 0x3fff);
                canvas = Some((width, height));
            }
            b"VP8L"
                if canvas.is_none() && data.len() >= VP8L_HEADER && data[0] == VP8L_SIGNATURE =>
            {
                let bits = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                let width = (bits & 0x3fff) + 1;
                let height = ((bits >> 14) & 0x3fff) + 1;
                canvas = Some((width, height));
            }
            b"ANMF" => frames = frames.saturating_add(1),
            _ => {}
        }
        pos = end + (length & 1);
    }
    let (width, height) = canvas.ok_or_else(|| failed(Format::Webp, "missing image chunk"))?;
    Ok((width, height, frames.max(1)))
}

/// Opens a WebP decoder over the bytes.
fn webp_decoder(bytes: &[u8]) -> Result<image_webp::WebPDecoder<Cursor<&[u8]>>, Error> {
    image_webp::WebPDecoder::new(Cursor::new(bytes)).map_err(|error| failed(Format::Webp, error))
}

/// Decodes the first WebP frame, lossy or lossless, and its ICC profile.
fn webp(bytes: &Bytes) -> Result<Decoded, Error> {
    let mut decoder = webp_decoder(bytes.as_ref())?;
    let (width, height) = decoder.dimensions();
    let channels = if decoder.has_alpha() {
        Channels::Rgba
    } else {
        Channels::Rgb
    };
    let frames = decoder.num_frames();
    let lossless = !decoder.is_lossy();
    let icc = webp_icc(bytes);
    let size = decoder
        .output_buffer_size()
        .ok_or_else(|| failed(Format::Webp, "image too large"))?;
    let mut data = vec![0u8; size];
    decoder
        .read_image(&mut data)
        .map_err(|error| failed(Format::Webp, error))?;
    let pixels = Pixels::new(width, height, channels, data)
        .ok_or_else(|| failed(Format::Webp, "pixel count"))?;
    Ok(Decoded {
        pixels,
        icc,
        orientation: crate::orientation(Format::Webp, bytes),
        frames,
        lossless,
    })
}

/// Reads the first page size of a TIFF.
#[cfg(feature = "tiff")]
fn tiff_size(bytes: &[u8]) -> Result<(u32, u32), Error> {
    let mut decoder = tiff::decoder::Decoder::new(Cursor::new(bytes))
        .map_err(|error| failed(Format::Tiff, error))?;
    decoder
        .dimensions()
        .map_err(|error| failed(Format::Tiff, error))
}

/// Reports TIFF as unreadable when its feature is off.
#[cfg(not(feature = "tiff"))]
fn tiff_size(_bytes: &[u8]) -> Result<(u32, u32), Error> {
    Err(Error::Unsupported(Format::Tiff))
}

/// Decodes the first page of a TIFF, 8 or 16 bit, with its ICC tag.
#[cfg(feature = "tiff")]
fn tiff(bytes: &Bytes) -> Result<Decoded, Error> {
    let mut decoder = tiff::decoder::Decoder::new(Cursor::new(bytes.as_ref()))
        .map_err(|error| failed(Format::Tiff, error))?;
    let (width, height) = decoder
        .dimensions()
        .map_err(|error| failed(Format::Tiff, error))?;
    let color = decoder
        .colortype()
        .map_err(|error| failed(Format::Tiff, error))?;
    let icc = tiff_icc(bytes);
    let image = decoder
        .read_image()
        .map_err(|error| failed(Format::Tiff, error))?;
    let (channels, data) = tiff_samples(color, image)?;
    let pixels = Pixels::new(width, height, channels, data)
        .ok_or_else(|| failed(Format::Tiff, "pixel count"))?;
    Ok(Decoded {
        pixels,
        icc,
        orientation: crate::orientation(Format::Tiff, bytes),
        frames: 1,
        lossless: true,
    })
}

/// Reports TIFF as unreadable when its feature is off.
#[cfg(not(feature = "tiff"))]
fn tiff(_bytes: &Bytes) -> Result<Decoded, Error> {
    Err(Error::Unsupported(Format::Tiff))
}

/// Maps TIFF samples to 8-bit RGB or RGBA.
#[cfg(feature = "tiff")]
fn tiff_samples(
    color: tiff::ColorType,
    image: tiff::decoder::DecodingResult,
) -> Result<(Channels, Vec<u8>), Error> {
    use tiff::ColorType;
    use tiff::decoder::DecodingResult;
    match (color, image) {
        (ColorType::RGB(8), DecodingResult::U8(data)) => Ok((Channels::Rgb, data)),
        (ColorType::RGBA(8), DecodingResult::U8(data)) => Ok((Channels::Rgba, data)),
        (ColorType::Gray(8), DecodingResult::U8(data)) => {
            Ok((Channels::Rgb, expand_gray(&data, 1)))
        }
        (ColorType::GrayA(8), DecodingResult::U8(data)) => {
            Ok((Channels::Rgba, expand_gray(&data, 2)))
        }
        (ColorType::RGB(16), DecodingResult::U16(data)) => Ok((Channels::Rgb, cut_16(&data))),
        (ColorType::RGBA(16), DecodingResult::U16(data)) => Ok((Channels::Rgba, cut_16(&data))),
        (ColorType::Gray(16), DecodingResult::U16(data)) => {
            Ok((Channels::Rgb, expand_gray(&cut_16(&data), 1)))
        }
        (ColorType::GrayA(16), DecodingResult::U16(data)) => {
            Ok((Channels::Rgba, expand_gray(&cut_16(&data), 2)))
        }
        _ => Err(failed(Format::Tiff, "unsupported sample layout")),
    }
}

/// Cuts 16-bit samples to their high byte.
#[cfg(feature = "tiff")]
fn cut_16(values: &[u16]) -> Vec<u8> {
    values.iter().map(|value| (value >> 8) as u8).collect()
}

/// Expands grayscale samples with `stride` bytes each to RGB or RGBA.
fn expand_gray(samples: &[u8], stride: usize) -> Vec<u8> {
    match stride {
        1 => samples
            .iter()
            .flat_map(|value| [*value, *value, *value])
            .collect(),
        _ => {
            let (pairs, _) = samples.as_chunks::<2>();
            pairs
                .iter()
                .flat_map(|pair| [pair[0], pair[0], pair[0], pair[1]])
                .collect()
        }
    }
}

/// Reads the ICC profile of a JPEG from its APP2 segments: a slice of the
/// input when the profile sits in one segment, one joined buffer when it
/// takes several.
fn jpeg_icc(bytes: &Bytes) -> Option<Bytes> {
    let mut segments: Vec<(u8, Range<usize>)> = Vec::new();
    let mut pos: usize = 2;
    while pos.checked_add(4).is_some_and(|end| end <= bytes.len()) {
        if bytes.get(pos) != Some(&0xff) {
            break;
        }
        let mut at = pos;
        while bytes.get(at) == Some(&0xff) {
            at += 1;
        }
        let Some(&marker) = bytes.get(at) else {
            break;
        };
        if marker == JPEG_EOI || marker == JPEG_SOS {
            break;
        }
        if marker == JPEG_TEM || (0xd0..=0xd7).contains(&marker) {
            pos = at + 1;
            continue;
        }
        let Some(length) = be_u16(bytes, at + 1) else {
            break;
        };
        let length = length as usize;
        if length < 2 {
            break;
        }
        let start = at + 3;
        let Some(end) = at.checked_add(1 + length).filter(|end| *end <= bytes.len()) else {
            break;
        };
        if marker == JPEG_APP2
            && end >= start + JPEG_ICC_DATA
            && bytes
                .get(start..start + JPEG_ICC_HEADER.len())
                .is_some_and(|head| head == JPEG_ICC_HEADER)
        {
            let Some(&sequence) = bytes.get(start + JPEG_ICC_SEQUENCE) else {
                break;
            };
            segments.push((sequence, start + JPEG_ICC_DATA..end));
        }
        pos = end;
    }
    match segments.len() {
        0 => None,
        1 => {
            let (_, profile) = segments.pop()?;
            Some(bytes.slice(profile))
        }
        _ => {
            segments.sort_by_key(|(sequence, _)| *sequence);
            let len: usize = segments.iter().map(|(_, profile)| profile.len()).sum();
            let mut joined = Vec::with_capacity(len);
            for (_, profile) in &segments {
                joined.extend_from_slice(bytes.get(profile.clone())?);
            }
            Some(Bytes::from(joined))
        }
    }
}

/// Reads the ICC profile of a WebP from its `ICCP` chunk, which always holds
/// the whole profile: a slice of the input.
fn webp_icc(bytes: &Bytes) -> Option<Bytes> {
    const RIFF_HEADER_LEN: usize = 12;
    const CHUNK_HEADER_LEN: usize = 8;
    let mut pos = RIFF_HEADER_LEN;
    while let Some(kind) = bytes.get(pos..pos + 4) {
        let length = crate::le_u32(bytes, pos + 4)? as usize;
        let start = pos + CHUNK_HEADER_LEN;
        let end = start.checked_add(length)?;
        bytes.get(start..end)?;
        if kind == b"ICCP" {
            return Some(bytes.slice(start..end));
        }
        // RIFF chunks are padded to an even length.
        pos = end + (length & 1);
    }
    None
}

/// Reads the ICC profile of a TIFF from tag 34675 of its first IFD: a slice
/// of the input, whether the profile sits inline or further in the file.
#[cfg(feature = "tiff")]
fn tiff_icc(bytes: &Bytes) -> Option<Bytes> {
    const HEADER_LEN: usize = 8;
    const ENTRY_LEN: usize = 12;
    const COUNT_LEN: usize = 2;
    const INLINE_LEN: usize = 4;
    let little = match bytes.get(..4)? {
        header if header == TIFF_LE => true,
        header if header == TIFF_BE => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        if little {
            crate::le_u16(bytes, at)
        } else {
            be_u16(bytes, at)
        }
    };
    let u32_at = |at: usize| -> Option<u32> {
        if little {
            crate::le_u32(bytes, at)
        } else {
            be_u32(bytes, at)
        }
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    for index in 0..count {
        let entry = ifd.checked_add(COUNT_LEN + index * ENTRY_LEN)?;
        if u16_at(entry)? != TIFF_ICC_TAG {
            continue;
        }
        let kind = u16_at(entry + 2)?;
        if kind != TIFF_TYPE_BYTE && kind != TIFF_TYPE_UNDEFINED {
            return None;
        }
        let len = u32_at(entry + 4)? as usize;
        if len <= INLINE_LEN {
            // A short value sits in the entry's own value field.
            let start = entry + HEADER_LEN;
            return Some(bytes.slice(start..start.checked_add(len)?));
        }
        let start = u32_at(entry + HEADER_LEN)? as usize;
        let end = start.checked_add(len).filter(|end| *end <= bytes.len())?;
        return Some(bytes.slice(start..end));
    }
    None
}

/// A decode error with the decoder's own text.
fn failed(format: Format, error: impl std::fmt::Display) -> Error {
    Error::Decode {
        format,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        max_side: 2000,
        max_base64: 4_500_000,
        max_pixels: 100_000_000,
    };

    /// Decodes bytes the tests build, which are their own buffer.
    fn decode(bytes: &[u8], limits: &Limits) -> Result<Decoded, Error> {
        super::decode(&Bytes::from(bytes.to_vec()), limits)
    }

    fn limits() -> Limits {
        LIMITS
    }

    /// Encodes an 8 or 16 bit PNG.
    fn png_bytes(
        width: u32,
        height: u32,
        color: png::ColorType,
        depth: png::BitDepth,
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(color);
            encoder.set_depth(depth);
            let mut writer = encoder.write_header().expect("png header");
            writer.write_image_data(data).expect("png data");
            writer.finish().expect("png finish");
        }
        out
    }

    /// Encodes an 8-bit palette PNG.
    fn png_palette_bytes(width: u32, height: u32, palette: &[u8], indices: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(palette.to_vec());
            let mut writer = encoder.write_header().expect("png header");
            writer.write_image_data(indices).expect("png data");
            writer.finish().expect("png finish");
        }
        out
    }

    /// Encodes a PNG with an ICC profile.
    fn png_icc_bytes(width: u32, height: u32, data: &[u8], icc: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut info = png::Info::with_size(width, height);
            info.color_type = png::ColorType::Rgb;
            info.bit_depth = png::BitDepth::Eight;
            info.icc_profile = Some(std::borrow::Cow::Borrowed(icc));
            let encoder = png::Encoder::with_info(&mut out, info).expect("png info");
            let mut writer = encoder.write_header().expect("png header");
            writer.write_image_data(data).expect("png data");
            writer.finish().expect("png finish");
        }
        out
    }

    /// Encodes a JPEG, optionally with an ICC profile.
    fn jpeg_bytes(
        width: u16,
        height: u16,
        color: jpeg_encoder::ColorType,
        data: &[u8],
        quality: u8,
        icc: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut out, quality);
        if let Some(icc) = icc {
            encoder.add_icc_profile(icc).expect("jpeg icc");
        }
        encoder.encode(data, width, height, color).expect("jpeg");
        out
    }

    /// Encodes a one or two frame GIF over a four colour palette.
    fn gif_bytes(width: u16, height: u16, frames: &[(&[u8], Option<u8>)]) -> Vec<u8> {
        const PALETTE: [u8; 12] = [255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0];
        let mut out = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut out, width, height, &PALETTE).expect("gif");
            for (indices, transparent) in frames {
                let frame = gif::Frame {
                    width,
                    height,
                    buffer: std::borrow::Cow::Borrowed(indices),
                    transparent: *transparent,
                    ..gif::Frame::default()
                };
                encoder.write_frame(&frame).expect("gif frame");
            }
        }
        out
    }

    /// Encodes a lossless WebP.
    fn webp_bytes(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        image_webp::WebPEncoder::new(&mut out)
            .encode(rgba, width, height, image_webp::ColorType::Rgba8)
            .expect("webp");
        out
    }

    /// Encodes an 8-bit RGB or RGBA TIFF.
    #[cfg(feature = "tiff")]
    fn tiff_bytes(width: u32, height: u32, data: &[u8], alpha: bool) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        {
            let mut encoder = tiff::encoder::TiffEncoder::new(&mut out).expect("tiff");
            if alpha {
                let image = encoder
                    .new_image::<tiff::encoder::colortype::RGBA8>(width, height)
                    .expect("tiff image");
                image.write_data(data).expect("tiff data");
            } else {
                let image = encoder
                    .new_image::<tiff::encoder::colortype::RGB8>(width, height)
                    .expect("tiff image");
                image.write_data(data).expect("tiff data");
            }
        }
        out.into_inner()
    }

    /// A hand-built little endian TIFF header that declares only its size.
    #[cfg(feature = "tiff")]
    fn tiff_of_size(width: u32, height: u32) -> Vec<u8> {
        const ENTRY_LEN: usize = 12;
        const COUNT: usize = 9;
        let extra = 8 + 2 + COUNT * ENTRY_LEN + 4;
        let mut out = Vec::new();
        out.extend_from_slice(b"II\x2a\x00");
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&(COUNT as u16).to_le_bytes());
        let mut entry = |tag: u16, kind: u16, count: u32, value: u32| {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&value.to_le_bytes());
        };
        entry(256, 4, 1, width);
        entry(257, 4, 1, height);
        entry(258, 3, 3, extra as u32);
        entry(259, 3, 1, 1);
        entry(262, 3, 1, 2);
        entry(273, 4, 1, 0);
        entry(277, 3, 1, 3);
        entry(278, 4, 1, height);
        entry(279, 4, 1, 0);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&[8, 0, 8, 0, 8, 0]);
        out
    }

    /// A 2x2 WebP header with a huge canvas and no image data.
    fn webp_of_size(width: u32, height: u32) -> Vec<u8> {
        let mut out = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        out.extend_from_slice(b"VP8X");
        out.extend_from_slice(&10u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        let mut be3 = |value: u32| {
            let bytes = value.to_le_bytes();
            out.extend_from_slice(&bytes[..3]);
        };
        be3(width - 1);
        be3(height - 1);
        out
    }

    /// Replaces the dimensions in a JPEG's first start-of-frame marker.
    fn jpeg_of_size(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = jpeg_bytes(2, 2, jpeg_encoder::ColorType::Rgb, &[0; 12], 50, None);
        let at = (2..bytes.len().saturating_sub(8))
            .find(|&at| {
                bytes[at] == 0xff && matches!(bytes[at + 1], 0xc0..=0xc2) && bytes[at + 2] == 0
            })
            .expect("sof marker");
        bytes[at + 5..at + 7].copy_from_slice(&height.to_be_bytes());
        bytes[at + 7..at + 9].copy_from_slice(&width.to_be_bytes());
        bytes
    }

    #[test]
    fn decode_png_rgb() {
        let data = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let decoded = decode(
            &png_bytes(2, 2, png::ColorType::Rgb, png::BitDepth::Eight, &data),
            &limits(),
        )
        .expect("png");
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        assert_eq!((decoded.pixels.width, decoded.pixels.height), (2, 2));
        assert_eq!(decoded.pixels.data, data);
        assert!(decoded.lossless);
        assert_eq!(decoded.frames, 1);
    }

    #[test]
    fn decode_png_rgba() {
        let data = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let decoded = decode(
            &png_bytes(2, 2, png::ColorType::Rgba, png::BitDepth::Eight, &data),
            &limits(),
        )
        .expect("png");
        assert_eq!(decoded.pixels.channels, Channels::Rgba);
        assert_eq!(decoded.pixels.data, data);
    }

    #[test]
    fn decode_png_gray_and_gray_alpha() {
        let decoded = decode(
            &png_bytes(
                2,
                2,
                png::ColorType::Grayscale,
                png::BitDepth::Eight,
                &[10, 20, 30, 40],
            ),
            &limits(),
        )
        .expect("gray png");
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        assert_eq!(
            decoded.pixels.data,
            [10, 10, 10, 20, 20, 20, 30, 30, 30, 40, 40, 40]
        );

        let decoded = decode(
            &png_bytes(
                2,
                1,
                png::ColorType::GrayscaleAlpha,
                png::BitDepth::Eight,
                &[7, 200, 9, 100],
            ),
            &limits(),
        )
        .expect("gray alpha png");
        assert_eq!(decoded.pixels.channels, Channels::Rgba);
        assert_eq!(decoded.pixels.data, [7, 7, 7, 200, 9, 9, 9, 100]);
    }

    #[test]
    fn decode_png_palette() {
        let decoded = decode(
            &png_palette_bytes(2, 2, &[255, 0, 0, 0, 255, 0], &[0, 1, 1, 0]),
            &limits(),
        )
        .expect("palette png");
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        assert_eq!(
            decoded.pixels.data,
            [255, 0, 0, 0, 255, 0, 0, 255, 0, 255, 0, 0]
        );
    }

    #[test]
    fn decode_png_16_bit_cuts_to_eight() {
        let samples: [u16; 12] = [
            0x1234, 0x5678, 0x9abc, 0xdef0, 0x0102, 0x0304, 0x0506, 0x0708, 0x090a, 0x0b0c, 0x0d0e,
            0x0f10,
        ];
        let data: Vec<u8> = samples.iter().flat_map(|v| v.to_be_bytes()).collect();
        let decoded = decode(
            &png_bytes(2, 2, png::ColorType::Rgb, png::BitDepth::Sixteen, &data),
            &limits(),
        )
        .expect("16-bit png");
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        assert_eq!(
            decoded.pixels.data,
            [
                0x12, 0x56, 0x9a, 0xde, 0x01, 0x03, 0x05, 0x07, 0x09, 0x0b, 0x0d, 0x0f
            ]
        );
    }

    #[test]
    fn decode_png_keeps_the_icc_profile() {
        let icc = b"fake profile bytes".to_vec();
        let decoded = decode(&png_icc_bytes(1, 1, &[1, 2, 3], &icc), &limits()).expect("icc png");
        assert_eq!(decoded.icc, Some(Bytes::from(icc)));
    }

    #[test]
    fn decode_jpeg_rgb_and_icc() {
        let data: Vec<u8> = (0..4).flat_map(|i| [i * 60, 255 - i * 40, 12]).collect();
        let decoded = decode(
            &jpeg_bytes(2, 2, jpeg_encoder::ColorType::Rgb, &data, 100, None),
            &limits(),
        )
        .expect("jpeg");
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        assert_eq!((decoded.pixels.width, decoded.pixels.height), (2, 2));
        assert_eq!(decoded.orientation, 1);
        assert!(!decoded.lossless);
        for (got, want) in decoded.pixels.data.iter().zip(data.iter()) {
            assert!(got.abs_diff(*want) < 40, "{got} vs {want}");
        }

        let icc = b"jpeg icc profile".to_vec();
        let decoded = decode(
            &jpeg_bytes(
                1,
                1,
                jpeg_encoder::ColorType::Rgb,
                &[10, 20, 30],
                90,
                Some(&icc),
            ),
            &limits(),
        )
        .expect("jpeg icc");
        assert_eq!(decoded.icc, Some(Bytes::from(icc)));
    }

    #[test]
    fn decode_jpeg_grayscale() {
        let decoded = decode(
            &jpeg_bytes(
                2,
                2,
                jpeg_encoder::ColorType::Luma,
                &[0, 80, 160, 255],
                100,
                None,
            ),
            &limits(),
        )
        .expect("gray jpeg");
        assert_eq!((decoded.pixels.width, decoded.pixels.height), (2, 2));
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        assert_eq!(decoded.pixels.data.len(), 12);
    }

    #[test]
    fn decode_gif_composes_the_first_frame_on_transparency() {
        // 2x2, index 3 is transparent.
        let gif = gif_bytes(2, 2, &[(&[0, 3, 1, 2], Some(3)), (&[2, 2, 2, 2], None)]);
        let decoded = decode(&gif, &limits()).expect("gif");
        assert_eq!(decoded.frames, 2);
        assert_eq!(decoded.pixels.channels, Channels::Rgba);
        assert_eq!(
            decoded.pixels.data,
            [255, 0, 0, 255, 0, 0, 0, 0, 0, 255, 0, 255, 0, 0, 255, 255]
        );
    }

    #[expect(clippy::disallowed_methods, reason = "a test reads its fixture")]
    #[test]
    fn decode_webp_lossless_and_lossy() {
        let rgba = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let decoded = decode(&webp_bytes(2, 2, &rgba), &limits()).expect("lossless webp");
        assert_eq!(decoded.pixels.channels, Channels::Rgba);
        assert_eq!(decoded.pixels.data, rgba);
        assert!(decoded.lossless);

        let lossy = std::fs::read("tests/fixtures/gradient-64x48-lossy.webp").expect("fixture");
        let decoded = decode(&lossy, &limits()).expect("lossy webp");
        assert_eq!((decoded.pixels.width, decoded.pixels.height), (64, 48));
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        assert!(!decoded.lossless);
    }

    /// Tells whether `profile` points into `source`, so it was sliced and
    /// not copied.
    fn sliced_from(profile: &Bytes, source: &Bytes) -> bool {
        let start = source.as_ptr() as usize;
        let end = start + source.len();
        let at = profile.as_ptr() as usize;
        at >= start && at + profile.len() <= end
    }

    #[test]
    fn a_short_jpeg_icc_segment_is_skipped() {
        let jpeg = jpeg_bytes(
            8,
            8,
            jpeg_encoder::ColorType::Rgb,
            &[128; 8 * 8 * 3],
            90,
            None,
        );
        let mut bytes = jpeg[..2].to_vec();
        // An APP2 that holds the ICC lead bytes and nothing after them.
        bytes.extend_from_slice(&[0xff, 0xe2, 0x00, 0x0e]);
        bytes.extend_from_slice(JPEG_ICC_HEADER);
        bytes.extend_from_slice(&jpeg[2..]);
        let decoded = decode(&bytes, &limits()).expect("decode");
        assert_eq!(decoded.icc, None);
        assert_eq!((decoded.pixels.width, decoded.pixels.height), (8, 8));
    }

    #[test]
    fn a_one_frame_gif_over_the_decoder_default_cap_decodes() {
        // 4000x3200 RGBA is 51.2 MB, over the gif crate's own 50 MB cap.
        const WIDTH: u16 = 4000;
        const HEIGHT: u16 = 3200;
        let indices = vec![1u8; usize::from(WIDTH) * usize::from(HEIGHT)];
        let bytes = gif_bytes(WIDTH, HEIGHT, &[(&indices, None)]);
        let decoded = decode(&bytes, &limits()).expect("decode");
        assert_eq!(
            (decoded.pixels.width, decoded.pixels.height),
            (u32::from(WIDTH), u32::from(HEIGHT))
        );
    }

    #[test]
    fn a_one_segment_jpeg_icc_profile_is_a_slice_of_the_input() {
        let icc = b"a small icc profile".to_vec();
        let source = Bytes::from(jpeg_bytes(
            2,
            2,
            jpeg_encoder::ColorType::Rgb,
            &[0; 12],
            90,
            Some(&icc),
        ));
        let decoded = super::decode(&source, &limits()).expect("jpeg icc");
        let profile = decoded.icc.expect("icc");
        assert_eq!(profile.as_ref(), icc.as_slice());
        assert!(sliced_from(&profile, &source), "the profile was copied");
    }

    #[test]
    fn a_webp_icc_profile_is_a_slice_of_the_input() {
        const ICC: [u8; 20] = [7; 20];
        // A RIFF header with one ICCP chunk and one empty image chunk: the
        // profile reader looks at the container only.
        let mut source = Vec::new();
        source.extend_from_slice(b"RIFF");
        source.extend_from_slice(&((4 + 8 + ICC.len() + 8) as u32).to_le_bytes());
        source.extend_from_slice(b"WEBP");
        source.extend_from_slice(b"ICCP");
        source.extend_from_slice(&(ICC.len() as u32).to_le_bytes());
        source.extend_from_slice(&ICC);
        source.extend_from_slice(b"VP8L");
        source.extend_from_slice(&0u32.to_le_bytes());
        let source = Bytes::from(source);
        let profile = webp_icc(&source).expect("webp icc");
        assert_eq!(profile.as_ref(), ICC.as_slice());
        assert!(sliced_from(&profile, &source), "the profile was copied");
    }

    #[cfg(feature = "tiff")]
    #[test]
    fn a_tiff_icc_profile_is_a_slice_of_the_input() {
        let icc: Vec<u8> = (0..600u32).map(|value| value as u8).collect();
        let mut out = Cursor::new(Vec::new());
        {
            let mut encoder = tiff::encoder::TiffEncoder::new(&mut out).expect("tiff");
            let mut image = encoder
                .new_image::<tiff::encoder::colortype::RGB8>(2, 2)
                .expect("tiff image");
            image
                .encoder()
                .write_tag(tiff::tags::Tag::Unknown(TIFF_ICC_TAG), &icc[..])
                .expect("tiff icc tag");
            image
                .write_data(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
                .expect("tiff data");
        }
        let source = Bytes::from(out.into_inner());
        let decoded = super::decode(&source, &limits()).expect("tiff icc");
        let profile = decoded.icc.expect("tiff icc profile");
        assert_eq!(profile.as_ref(), icc.as_slice());
        assert!(sliced_from(&profile, &source), "the profile was copied");
    }

    #[cfg(feature = "tiff")]
    #[test]
    fn a_short_tiff_icc_value_sits_inline() {
        // A value of four bytes or fewer sits in the entry's own value field.
        let mut source = Vec::new();
        source.extend_from_slice(&TIFF_LE);
        source.extend_from_slice(&8u32.to_le_bytes());
        source.extend_from_slice(&1u16.to_le_bytes());
        source.extend_from_slice(&TIFF_ICC_TAG.to_le_bytes());
        source.extend_from_slice(&TIFF_TYPE_BYTE.to_le_bytes());
        source.extend_from_slice(&3u32.to_le_bytes());
        source.extend_from_slice(&[9, 8, 7, 0]);
        source.extend_from_slice(&0u32.to_le_bytes());
        let source = Bytes::from(source);
        let profile = tiff_icc(&source).expect("inline tiff icc");
        assert_eq!(profile.as_ref(), [9, 8, 7].as_slice());
        assert!(sliced_from(&profile, &source), "the profile was copied");
    }

    #[cfg(feature = "tiff")]
    #[test]
    fn decode_tiff_rgb_and_rgba() {
        let data = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let decoded = decode(&tiff_bytes(2, 2, &data, false), &limits()).expect("tiff");
        assert_eq!(decoded.pixels.channels, Channels::Rgb);
        assert_eq!(decoded.pixels.data, data);

        let data = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let decoded = decode(&tiff_bytes(2, 2, &data, true), &limits()).expect("rgba tiff");
        assert_eq!(decoded.pixels.channels, Channels::Rgba);
        assert_eq!(decoded.pixels.data, data);
    }

    #[test]
    fn decode_sets_the_exif_orientation() {
        let mut jpeg = jpeg_bytes(2, 2, jpeg_encoder::ColorType::Rgb, &[0; 12], 50, None);
        let mut exif = b"Exif\x00\x00".to_vec();
        exif.extend_from_slice(b"MM\x00\x2a");
        exif.extend_from_slice(&[0, 0, 0, 8]);
        exif.extend_from_slice(&[0, 1]);
        exif.extend_from_slice(&[1, 18]);
        exif.extend_from_slice(&[0, 3]);
        exif.extend_from_slice(&[0, 0, 0, 1]);
        exif.extend_from_slice(&[0, 6, 0, 0]);
        exif.extend_from_slice(&[0, 0, 0, 0]);
        let mut app1 = vec![0xff, 0xe1];
        app1.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(&exif);
        jpeg.splice(2..2, app1);
        let decoded = decode(&jpeg, &limits()).expect("jpeg with exif");
        assert_eq!(decoded.orientation, 6);
    }

    #[test]
    fn a_truncated_file_of_each_format_errors() {
        let png = png_bytes(2, 2, png::ColorType::Rgb, png::BitDepth::Eight, &[0; 12]);
        let jpeg = jpeg_bytes(2, 2, jpeg_encoder::ColorType::Rgb, &[0; 12], 50, None);
        let gif = gif_bytes(2, 2, &[(&[0, 1, 2, 0], None)]);
        let webp = webp_bytes(2, 2, &[0; 16]);
        #[cfg_attr(not(feature = "tiff"), allow(unused_mut))]
        let mut files = vec![png, jpeg, gif, webp];
        #[cfg(feature = "tiff")]
        files.push(tiff_bytes(2, 2, &[0; 12], false));
        for bytes in files {
            let cut = &bytes[..bytes.len() / 2];
            assert!(decode(cut, &limits()).is_err());
        }
        let mut bmp = vec![0u8; 54];
        bmp[..2].copy_from_slice(b"BM");
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
        assert!(decode(&bmp, &limits()).is_err());
    }

    #[test]
    fn a_60000_square_header_errors_before_it_allocates() {
        let png = {
            let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
            bytes.extend_from_slice(&13u32.to_be_bytes());
            bytes.extend_from_slice(b"IHDR");
            bytes.extend_from_slice(&60000u32.to_be_bytes());
            bytes.extend_from_slice(&60000u32.to_be_bytes());
            bytes.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
            bytes
        };
        let gif = {
            let mut gif = gif_bytes(2, 2, &[(&[0, 1, 2, 3], None)]);
            gif[6..8].copy_from_slice(&60000u16.to_le_bytes());
            gif[8..10].copy_from_slice(&60000u16.to_le_bytes());
            gif
        };
        let webp = webp_of_size(60000, 60000);
        let jpeg = jpeg_of_size(60000, 60000);
        #[cfg_attr(not(feature = "tiff"), allow(unused_mut))]
        let mut files = vec![png, gif, webp, jpeg];
        #[cfg(feature = "tiff")]
        files.push(tiff_of_size(60000, 60000));
        for bytes in files {
            match decode(&bytes, &limits()) {
                Err(Error::TooManyPixels { width, height }) => {
                    assert_eq!((width, height), (60000, 60000));
                }
                other => panic!("expected a pixel error, got {other:?}"),
            }
        }
    }

    #[cfg(not(feature = "tiff"))]
    #[test]
    fn without_the_feature_tiff_is_unreadable() {
        assert_eq!(
            decode(b"II\x2a\x00\x08\x00\x00\x00", &limits()),
            Err(Error::Unsupported(Format::Tiff))
        );
    }

    #[test]
    fn an_unknown_format_errors() {
        assert_eq!(
            decode(b"not an image at all", &limits()),
            Err(Error::UnknownFormat)
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn avif_is_unsupported_here() {
        let mut avif = b"\x00\x00\x00\x20ftypavif\x00\x00\x00\x00".to_vec();
        avif.resize(64, 0);
        assert_eq!(
            decode(&avif, &limits()),
            Err(Error::Unsupported(Format::Avif))
        );
    }
}
