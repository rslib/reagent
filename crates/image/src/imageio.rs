//! AVIF through the system ImageIO framework, on macOS only.
//!
//! ImageIO decodes the image into a CoreGraphics bitmap that this module
//! draws in sRGB, 8 bits per channel. Every call is plain C FFI; each
//! object the module creates it releases on every path.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr;

use bytes::Bytes;

use crate::{Channels, Decoded, Error, Format, Pixels, check_pixels};

/// Bytes per pixel of the bitmap ImageIO draws into.
const RGBA: usize = 4;
/// Bytes per pixel after an opaque bitmap drops its fourth byte.
const RGB: usize = 3;
/// `kCGImageAlphaNone`: the image has no alpha.
const ALPHA_NONE: u32 = 0;
/// `kCGImageAlphaPremultipliedLast`: RGBA with the alpha last, premultiplied.
const ALPHA_PREMULTIPLIED_LAST: u32 = 1;
/// `kCGImageAlphaNoneSkipLast`: RGBX, the fourth byte unused.
const ALPHA_NONE_SKIP_LAST: u32 = 5;
/// `kCGImageAlphaNoneSkipFirst`: XRGB, the first byte unused.
const ALPHA_NONE_SKIP_FIRST: u32 = 6;
/// `kCGBitmapByteOrder32Big`: the bytes in memory are R, G, B, A.
const BYTE_ORDER_32_BIG: u32 = 4 << 12;
/// `kCFNumberSInt64Type`.
const NUMBER_SINT64: isize = 4;
/// `CFAllocatorContext.version` this module fills in.
const ALLOCATOR_CONTEXT_VERSION: isize = 0;

type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFDataRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFNumberRef = *const c_void;
type CFStringRef = *const c_void;
type CGImageSourceRef = *const c_void;
type CGImageRef = *const c_void;
type CGColorSpaceRef = *const c_void;
type CGContextRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

/// `CFAllocatorContext`, with only `release` and `deallocate` filled in.
#[repr(C)]
struct CFAllocatorContext {
    version: isize,
    info: *mut c_void,
    retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<unsafe extern "C" fn(*const c_void)>,
    copy_description: Option<unsafe extern "C" fn(*const c_void) -> CFStringRef>,
    allocate: Option<unsafe extern "C" fn(isize, usize, *mut c_void) -> *mut c_void>,
    reallocate: Option<unsafe extern "C" fn(*mut c_void, isize, usize, *mut c_void) -> *mut c_void>,
    deallocate: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    preferred_size: Option<unsafe extern "C" fn(isize, usize, *mut c_void) -> isize>,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFAllocatorCreate(
        allocator: CFAllocatorRef,
        context: *mut CFAllocatorContext,
    ) -> CFAllocatorRef;
    fn CFDataCreateWithBytesNoCopy(
        allocator: CFAllocatorRef,
        bytes: *const u8,
        length: isize,
        deallocator: CFAllocatorRef,
    ) -> CFDataRef;
    fn CFDictionaryGetValue(dictionary: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: CFNumberRef, kind: isize, value: *mut c_void) -> u8;
    fn CFRelease(object: CFTypeRef);
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    static kCGImagePropertyPixelWidth: CFStringRef;
    static kCGImagePropertyPixelHeight: CFStringRef;
    static kCGImagePropertyOrientation: CFStringRef;
    fn CGImageSourceCreateWithData(data: CFDataRef, options: CFDictionaryRef) -> CGImageSourceRef;
    fn CGImageSourceCopyPropertiesAtIndex(
        source: CGImageSourceRef,
        index: usize,
        options: CFDictionaryRef,
    ) -> CFDictionaryRef;
    fn CGImageSourceCreateImageAtIndex(
        source: CGImageSourceRef,
        index: usize,
        options: CFDictionaryRef,
    ) -> CGImageRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGColorSpaceSRGB: CFStringRef;
    fn CGColorSpaceCreateWithName(name: CFStringRef) -> CGColorSpaceRef;
    fn CGImageGetAlphaInfo(image: CGImageRef) -> u32;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CGColorSpaceRef,
        bitmap_info: u32,
    ) -> CGContextRef;
    fn CGContextDrawImage(context: CGContextRef, rect: CGRect, image: CGImageRef);
}

/// A CoreFoundation object this module owns, released when dropped.
struct Owned(CFTypeRef);

impl Owned {
    /// Takes ownership of `object`, or `None` when a create call failed.
    fn new(object: CFTypeRef) -> Option<Self> {
        // Lazy: an `Owned` of a null pointer must never exist, as its drop
        // would release null.
        (!object.is_null()).then(|| Owned(object))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a CoreFoundation create or copy call,
        // is not null, and this is its only owner, so it is released once.
        unsafe { CFRelease(self.0) }
    }
}

/// Frees the `Bytes` an allocator holds when CoreFoundation frees it.
unsafe extern "C" fn release_bytes(info: *const c_void) {
    // SAFETY: `info` is the `Box<Bytes>` that `source` leaked into the
    // allocator, and CoreFoundation calls `release` once, when the allocator
    // (held by the CFData) is freed.
    drop(unsafe { Box::from_raw(info.cast_mut().cast::<Bytes>()) });
}

/// Frees nothing: the bytes belong to the `Bytes` the allocator holds.
unsafe extern "C" fn keep_bytes(_bytes: *mut c_void, _info: *mut c_void) {}

/// Opens `bytes` as an ImageIO source without copying them.
///
/// The CFData's deallocator holds a reference to `bytes`, so the buffer
/// stays alive for as long as anything in ImageIO still reads it.
fn source(bytes: &Bytes) -> Result<Owned, Error> {
    let length = isize::try_from(bytes.len()).map_err(|_| failed("too long"))?;
    let info = Box::into_raw(Box::new(bytes.clone()));
    let mut context = CFAllocatorContext {
        version: ALLOCATOR_CONTEXT_VERSION,
        info: info.cast(),
        retain: None,
        release: Some(release_bytes),
        copy_description: None,
        allocate: None,
        reallocate: None,
        deallocate: Some(keep_bytes),
        preferred_size: None,
    };
    // SAFETY: `context` is a valid CFAllocatorContext that CoreFoundation
    // copies. A null parent allocator means the default one.
    let allocator = unsafe { CFAllocatorCreate(ptr::null(), &mut context) };
    let Some(allocator) = Owned::new(allocator) else {
        // SAFETY: no allocator exists, so `info` was never handed over.
        drop(unsafe { Box::from_raw(info) });
        return Err(failed("no allocator"));
    };
    // SAFETY: the pointer and length describe `bytes`, which the allocator
    // keeps alive until the CFData is freed; the CFData retains the
    // allocator, so dropping our reference to it below is fine.
    let data = Owned::new(unsafe {
        CFDataCreateWithBytesNoCopy(ptr::null(), bytes.as_ptr(), length, allocator.0)
    })
    .ok_or_else(|| failed("no data"))?;
    drop(allocator);
    // SAFETY: `data` is a live CFData; a null options dictionary is allowed.
    Owned::new(unsafe { CGImageSourceCreateWithData(data.0, ptr::null()) })
        .ok_or_else(|| failed("not an image"))
}

/// Width, height and EXIF-style orientation of the first image.
struct Header {
    width: u32,
    height: u32,
    orientation: u8,
}

/// Reads the header of the first image without decoding pixels.
fn header(source: &Owned) -> Result<Header, Error> {
    // SAFETY: `source` is live; a null options dictionary is allowed and
    // ImageIO returns null when there is no image at index 0.
    let properties =
        Owned::new(unsafe { CGImageSourceCopyPropertiesAtIndex(source.0, 0, ptr::null()) })
            .ok_or_else(|| failed("no image"))?;
    // SAFETY: the keys are constant CFStrings that ImageIO defines.
    let (width_key, height_key, orientation_key) = unsafe {
        (
            kCGImagePropertyPixelWidth,
            kCGImagePropertyPixelHeight,
            kCGImagePropertyOrientation,
        )
    };
    let width = number(&properties, width_key).ok_or_else(|| failed("no width"))?;
    let height = number(&properties, height_key).ok_or_else(|| failed("no height"))?;
    let orientation = number(&properties, orientation_key).unwrap_or(1);
    let fit = |value: i64| u32::try_from(value).map_err(|_| failed("size does not fit"));
    Ok(Header {
        width: fit(width)?,
        height: fit(height)?,
        orientation: u8::try_from(orientation)
            .ok()
            .filter(|value| (1..=8).contains(value))
            .unwrap_or(1),
    })
}

/// Reads an integer value from a properties dictionary.
fn number(dictionary: &Owned, key: CFStringRef) -> Option<i64> {
    // SAFETY: `dictionary` is a live CFDictionary and `key` a CFString; the
    // returned value is borrowed from the dictionary, not owned.
    let value = unsafe { CFDictionaryGetValue(dictionary.0, key) };
    if value.is_null() {
        return None;
    }
    let mut out = 0i64;
    // SAFETY: ImageIO stores these properties as CFNumbers, and `out` is an
    // i64 that matches kCFNumberSInt64Type.
    let exact = unsafe { CFNumberGetValue(value, NUMBER_SINT64, (&raw mut out).cast()) };
    (exact != 0).then_some(out)
}

/// Reads the size of the first image without decoding pixels.
pub(crate) fn size(bytes: &Bytes) -> Result<(u32, u32), Error> {
    let header = header(&source(bytes)?)?;
    Ok((header.width, header.height))
}

/// Decodes the first image to straight (not premultiplied) 8-bit sRGB:
/// RGB when the image is opaque, else RGBA.
pub(crate) fn decode(bytes: &Bytes, max_pixels: u64) -> Result<Decoded, Error> {
    let source = source(bytes)?;
    let Header {
        width,
        height,
        orientation,
    } = header(&source)?;
    check_pixels(width, height, max_pixels)?;
    // SAFETY: `source` is live; ImageIO returns null when there is no image.
    let image = Owned::new(unsafe { CGImageSourceCreateImageAtIndex(source.0, 0, ptr::null()) })
        .ok_or_else(|| failed("no image"))?;
    drop(source);
    // SAFETY: `image` is a live CGImage.
    let alpha = unsafe { CGImageGetAlphaInfo(image.0) };
    let opaque = matches!(
        alpha,
        ALPHA_NONE | ALPHA_NONE_SKIP_LAST | ALPHA_NONE_SKIP_FIRST
    );
    let (columns, rows) = (width as usize, height as usize);
    let row = columns
        .checked_mul(RGBA)
        .ok_or_else(|| failed("too wide"))?;
    let length = row.checked_mul(rows).ok_or_else(|| failed("too large"))?;
    let mut data = vec![0u8; length];
    // SAFETY: kCGColorSpaceSRGB is a constant CFString CoreGraphics defines.
    let space = Owned::new(unsafe { CGColorSpaceCreateWithName(kCGColorSpaceSRGB) })
        .ok_or_else(|| failed("no sRGB space"))?;
    let info = if opaque {
        ALPHA_NONE_SKIP_LAST
    } else {
        ALPHA_PREMULTIPLIED_LAST
    } | BYTE_ORDER_32_BIG;
    // SAFETY: `data` holds `row * rows` bytes and outlives the context,
    // which is released before the buffer is read or moved.
    let context = Owned::new(unsafe {
        CGBitmapContextCreate(
            data.as_mut_ptr().cast(),
            columns,
            rows,
            8,
            row,
            space.0,
            info,
        )
    })
    .ok_or_else(|| failed("no bitmap context"))?;
    let rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: f64::from(width),
            height: f64::from(height),
        },
    };
    // SAFETY: `context` and `image` are live; the context draws into
    // `data`, which it covers exactly.
    unsafe { CGContextDrawImage(context.0, rect, image.0) };
    drop(context);
    let channels = if opaque {
        drop_fourth_byte(&mut data);
        Channels::Rgb
    } else {
        unpremultiply(&mut data);
        Channels::Rgba
    };
    let pixels = Pixels::new(width, height, channels, data).ok_or_else(|| failed("pixel count"))?;
    Ok(Decoded {
        pixels,
        icc: None,
        orientation,
        frames: 1,
        lossless: false,
    })
}

/// Packs RGBX into RGB in the same buffer and trims it.
fn drop_fourth_byte(data: &mut Vec<u8>) {
    let pixels = data.len() / RGBA;
    for index in 0..pixels {
        data.copy_within(index * RGBA..index * RGBA + RGB, index * RGB);
    }
    data.truncate(pixels * RGB);
}

/// Turns premultiplied RGBA back into straight RGBA.
fn unpremultiply(data: &mut [u8]) {
    let (pixels, _) = data.as_chunks_mut::<RGBA>();
    for pixel in pixels {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..RGB] {
            let straight = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = straight.min(255) as u8;
        }
    }
}

/// A decode error for AVIF with `message`.
fn failed(message: &str) -> Error {
    Error::Decode {
        format: Format::Avif,
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avif_unpremultiply_restores_straight_alpha() {
        let mut data = vec![64, 32, 0, 128, 10, 20, 30, 255, 9, 9, 9, 0];
        unpremultiply(&mut data);
        assert_eq!(data, [128, 64, 0, 128, 10, 20, 30, 255, 9, 9, 9, 0]);
    }

    #[test]
    fn avif_drop_fourth_byte_packs_rgb() {
        let mut data = vec![1, 2, 3, 0, 4, 5, 6, 0];
        drop_fourth_byte(&mut data);
        assert_eq!(data, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn avif_garbage_is_a_decode_error() {
        let bytes = Bytes::from_static(b"\x00\x00\x00\x20ftypavif\x00\x00\x00\x00 nothing");
        assert!(matches!(
            decode(&bytes, u64::MAX),
            Err(Error::Decode {
                format: Format::Avif,
                ..
            })
        ));
    }
}
