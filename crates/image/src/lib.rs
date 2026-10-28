//! Images for models: detect the format, decode to pixels, apply EXIF
//! orientation, shrink to the limits and encode as PNG or JPEG.
//!
//! Nothing here touches the file system or a GPU, and no decoder allocates
//! pixels before the header passed [`Limits::max_pixels`].

#![deny(unsafe_code)]

mod bmp;
mod decode;
pub mod encode;
mod heic;
#[cfg(target_os = "macos")]
mod imageio;
mod orient;
mod prepare;
mod resize;

use bytes::Bytes;

pub use decode::decode;
pub use orient::{apply, orientation};
pub use prepare::{Prepared, prepare};
pub(crate) use resize::resize_with_threads;
pub use resize::{fit, resize};

/// Default cap on the longer side of a prepared image, in pixels.
pub const DEFAULT_MAX_SIDE: u32 = 2000;
/// Default cap on the base64 length of a prepared image, in bytes.
pub const DEFAULT_MAX_BASE64: usize = 4_500_000;
/// Default cap on the pixel count of a decoded image.
pub const DEFAULT_MAX_PIXELS: u64 = 100_000_000;

/// An image container format recognised by [`sniff`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// Portable Network Graphics.
    Png,
    /// JPEG.
    Jpeg,
    /// Graphics Interchange Format.
    Gif,
    /// WebP, lossy or lossless.
    Webp,
    /// Windows bitmap.
    Bmp,
    /// Tagged Image File Format.
    Tiff,
    /// HEIF/HEIC still image.
    Heic,
    /// AVIF still image.
    Avif,
}

impl Format {
    /// The format name as it appears in a note or an error.
    pub fn name(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpeg",
            Format::Gif => "gif",
            Format::Webp => "webp",
            Format::Bmp => "bmp",
            Format::Tiff => "tiff",
            Format::Heic => "heic",
            Format::Avif => "avif",
        }
    }

    /// Formats a model accepts as input, the ones [`Prepared`] can hold.
    pub fn is_model_input(self) -> bool {
        matches!(
            self,
            Format::Png | Format::Jpeg | Format::Gif | Format::Webp
        )
    }
}

/// The format named by the first bytes of `bytes`, or `None` when no magic
/// matches.
pub fn sniff(bytes: &[u8]) -> Option<Format> {
    const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    const JPEG_MAGIC: [u8; 3] = [0xff, 0xd8, 0xff];
    if bytes.starts_with(&PNG_MAGIC) {
        return Some(Format::Png);
    }
    if bytes.starts_with(&JPEG_MAGIC) {
        return Some(Format::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(Format::Gif);
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(Format::Webp);
    }
    if is_bmp(bytes) {
        return Some(Format::Bmp);
    }
    if bytes.starts_with(b"II\x2a\x00") || bytes.starts_with(b"MM\x00\x2a") {
        return Some(Format::Tiff);
    }
    iso_brand(bytes)
}

/// Reads the ISO base media `ftyp` box: the major brand, then the compatible
/// brands, to tell HEIC from AVIF.
fn iso_brand(bytes: &[u8]) -> Option<Format> {
    let major = bytes.get(8..12)?;
    if bytes.get(4..8)? != b"ftyp" {
        return None;
    }
    let from_brand = |brand: &[u8]| match brand {
        b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"hevm" | b"hevs" | b"mif1"
        | b"msf1" => Some(Format::Heic),
        b"avif" | b"avis" => Some(Format::Avif),
        _ => None,
    };
    if let Some(format) = from_brand(major) {
        return Some(format);
    }
    // A generic major brand: look through the compatible brands.
    let box_len = u32::from_be_bytes(bytes.get(0..4)?.try_into().ok()?) as usize;
    let end = box_len.clamp(16, bytes.len());
    let mut pos = 16;
    while pos + 4 <= end {
        let brand = bytes.get(pos..pos + 4)?;
        if let Some(format) = from_brand(brand) {
            return Some(format);
        }
        pos += 4;
    }
    None
}

/// How the samples of a [`Pixels`] buffer are packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channels {
    /// Three bytes per pixel: red, green, blue.
    Rgb,
    /// Four bytes per pixel: red, green, blue, alpha.
    Rgba,
}

impl Channels {
    /// Bytes per pixel.
    pub fn count(self) -> usize {
        match self {
            Channels::Rgb => 3,
            Channels::Rgba => 4,
        }
    }
}

/// An 8-bit-per-channel image, row-major and tightly packed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixels {
    /// Pixels across.
    pub width: u32,
    /// Pixels down.
    pub height: u32,
    /// Sample layout of `data`.
    pub channels: Channels,
    /// `width * height * channels.count()` bytes.
    pub data: Vec<u8>,
}

impl Pixels {
    /// Builds a buffer, returning `None` when `data` does not match the size.
    pub(crate) fn new(
        width: u32,
        height: u32,
        channels: Channels,
        data: Vec<u8>,
    ) -> Option<Pixels> {
        let want = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(channels.count())?;
        (data.len() == want).then_some(Pixels {
            width,
            height,
            channels,
            data,
        })
    }

    /// Bytes from the start of one row to the start of the next.
    pub(crate) fn stride(&self) -> usize {
        self.width as usize * self.channels.count()
    }
}

/// A decoded image and what the container says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// The pixels, after every container transform the decoder applies.
    pub pixels: Pixels,
    /// ICC profile bytes, when the file carries one: a slice of the input
    /// when the profile sits in one piece there.
    pub icc: Option<Bytes>,
    /// EXIF orientation, 1 to 8; 1 when the file gives none.
    pub orientation: u8,
    /// Animation frames; 1 unless the GIF or WebP is animated.
    pub frames: u32,
    /// Whether the source encoded pixels without loss (PNG, lossless WebP).
    pub lossless: bool,
}

/// The caps [`prepare`] applies to its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Longest side of the output, in pixels.
    pub max_side: u32,
    /// Longest base64 length of the output, in bytes.
    pub max_base64: usize,
    /// Most pixels a decoder may allocate for the source.
    pub max_pixels: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_side: DEFAULT_MAX_SIDE,
            max_base64: DEFAULT_MAX_BASE64,
            max_pixels: DEFAULT_MAX_PIXELS,
        }
    }
}

/// Why an image could not be read, prepared or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The first bytes match no known image format.
    UnknownFormat,
    /// A format this build or this platform has no decoder for.
    Unsupported(Format),
    /// The header asks for more pixels than [`Limits::max_pixels`].
    TooManyPixels {
        /// Width the header claims.
        width: u32,
        /// Height the header claims.
        height: u32,
    },
    /// A decoder rejected the bytes.
    Decode {
        /// Format that failed.
        format: Format,
        /// The decoder's own message.
        message: String,
    },
    /// An encoder rejected the pixels.
    Encode {
        /// Format that failed.
        format: Format,
        /// The encoder's own message.
        message: String,
    },
    /// Every encoding is still over [`Limits::max_base64`].
    TooLarge {
        /// Base64 length of the smallest encoding, in bytes.
        base64: usize,
        /// The limit it missed.
        limit: usize,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnknownFormat => f.write_str("unknown image format"),
            Error::Unsupported(format) => {
                write!(f, "{}: this build cannot read it", format.name())
            }
            Error::TooManyPixels { width, height } => {
                write!(f, "{width}x{height}: over the pixel limit")
            }
            Error::Decode { format, message } => {
                write!(f, "{} decode failed: {message}", format.name())
            }
            Error::Encode { format, message } => {
                write!(f, "{} encode failed: {message}", format.name())
            }
            Error::TooLarge { base64, limit } => {
                write!(f, "still {base64} base64 bytes over the {limit} limit")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Rejects a header's size when it would ask a decoder for too much memory.
pub(crate) fn check_pixels(width: u32, height: u32, max_pixels: u64) -> Result<(), Error> {
    let pixels = u64::from(width).checked_mul(u64::from(height));
    match pixels {
        Some(pixels) if pixels <= max_pixels && width > 0 && height > 0 => Ok(()),
        _ => Err(Error::TooManyPixels { width, height }),
    }
}

/// Whether `bytes` start a bitmap: `BM`, four reserved zero bytes and an
/// info header length one of the known versions use. Two letters alone would
/// make a text file that starts with "BM" an image.
fn is_bmp(bytes: &[u8]) -> bool {
    /// Info header lengths: core, V1 to V5 and the OS/2 forms.
    const INFO_LENGTHS: [u32; 7] = [12, 40, 52, 56, 64, 108, 124];
    bytes.starts_with(b"BM")
        && bytes.get(6..10) == Some(&[0, 0, 0, 0])
        && bytes
            .get(14..18)
            .and_then(|length| <[u8; 4]>::try_from(length).ok())
            .is_some_and(|length| INFO_LENGTHS.contains(&u32::from_le_bytes(length)))
}

/// The base64 length of `len` bytes, rounded up to whole groups.
pub(crate) fn base64_len(len: usize) -> usize {
    len.div_ceil(3).saturating_mul(4)
}

/// Reads a big endian `u16` at `at`, or `None` when it runs past the end.
pub(crate) fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let pair = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([*pair.first()?, *pair.get(1)?]))
}

/// Reads a big endian `u32` at `at`, or `None` when it runs past the end.
pub(crate) fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let quad = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([
        *quad.first()?,
        *quad.get(1)?,
        *quad.get(2)?,
        *quad.get(3)?,
    ]))
}

/// Reads a little endian `u16` at `at`, or `None` when it runs past the end.
pub(crate) fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let pair = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([*pair.first()?, *pair.get(1)?]))
}

/// Reads a little endian `u32` at `at`, or `None` when it runs past the end.
pub(crate) fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let quad = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([
        *quad.first()?,
        *quad.get(1)?,
        *quad.get(2)?,
        *quad.get(3)?,
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_each_format() {
        assert_eq!(
            sniff(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            Some(Format::Png)
        );
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some(Format::Jpeg));
        assert_eq!(sniff(b"GIF89a...."), Some(Format::Gif));
        assert_eq!(sniff(b"GIF87a...."), Some(Format::Gif));
        assert_eq!(sniff(b"RIFF\x10\x00\x00\x00WEBPVP8 "), Some(Format::Webp));
        assert_eq!(
            sniff(b"BM\x46\x00\x00\x00\x00\x00\x00\x00\x36\x00\x00\x00\x28\x00\x00\x00"),
            Some(Format::Bmp)
        );
        assert_eq!(sniff(b"II\x2a\x00\x08\x00\x00\x00"), Some(Format::Tiff));
        assert_eq!(sniff(b"MM\x00\x2a\x00\x00\x00\x08"), Some(Format::Tiff));
        let heic = [&[0u8, 0, 0, 0x18][..], b"ftypheic", &[0, 0, 0, 0]].concat();
        assert_eq!(sniff(&heic), Some(Format::Heic));
        let avif = [&[0u8, 0, 0, 0x20][..], b"ftypavif", &[0, 0, 0, 0]].concat();
        assert_eq!(sniff(&avif), Some(Format::Avif));
    }

    #[test]
    fn sniff_random_bytes_is_none() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..200 {
            let mut bytes = [0u8; 24];
            for byte in &mut bytes {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *byte = seed as u8;
            }
            assert_eq!(sniff(&bytes), None);
        }
        assert_eq!(sniff(b""), None);
        // Text that starts with "BM" is not a bitmap; a real header is.
        assert_eq!(sniff(b"BMW service notes: oil change due"), None);
        let mut header = *b"BM\x46\x00\x00\x00\x00\x00\x00\x00\x36\x00\x00\x00\x28\x00\x00\x00";
        assert_eq!(sniff(&header), Some(Format::Bmp));
        header[14] = 41;
        assert_eq!(sniff(&header), None);
    }

    #[test]
    fn base64_length_rounds_up() {
        assert_eq!(base64_len(0), 0);
        assert_eq!(base64_len(1), 4);
        assert_eq!(base64_len(3), 4);
        assert_eq!(base64_len(4), 8);
        assert_eq!(base64_len(100), 136);
    }

    #[test]
    fn pixel_limit_rejects_a_large_header() {
        assert!(check_pixels(60000, 60000, DEFAULT_MAX_PIXELS).is_err());
        assert!(check_pixels(2000, 2000, DEFAULT_MAX_PIXELS).is_ok());
        assert!(check_pixels(0, 10, DEFAULT_MAX_PIXELS).is_err());
    }
}
