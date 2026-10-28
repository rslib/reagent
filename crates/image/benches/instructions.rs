//! Gungraun benches: exact Callgrind instruction counts for image
//! preparation, the hot path of a pasted or attached photo.
//!
//! Every input is built in a `setup` function, so its cost is not in the
//! measured part: only sniffing, decoding, orienting, resizing, encoding or
//! the whole `prepare` is counted.
//!
//! Run with `cargo bench -p reagent-image --bench instructions`, or with
//! `cargo bench -p reagent-image --bench instructions -- --test` to run each
//! bench once without Callgrind. `bench/instructions.sh` runs both bench
//! targets against a base commit.

// gungraun's benchmark macros expand to std::fs and std::process code on
// Linux, which clippy.toml bans; the ban is for the agent, not for benches.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::hint::black_box;

use bytes::Bytes;
use gungraun::prelude::*;

use reagent_image::{
    Format, Limits, Pixels, apply, decode, encode, orientation, prepare, resize, sniff,
};

/// Side of the PNG that passes through: about 1 MB of noise.
const PASS_THROUGH_SIDE: u32 = 700;
/// Side of the decoded, oriented and encoded images: 1000x750, three quarters
/// of a megapixel.
const MEGAPIXEL_WIDTH: u32 = 1000;
/// Height of the same images.
const MEGAPIXEL_HEIGHT: u32 = 750;
/// Width of the source the resize bench shrinks, the long side of a 12 MP
/// photo kept at two thirds.
const RESIZE_SOURCE_WIDTH: u32 = 2000;
/// Height of the source the resize bench shrinks.
const RESIZE_SOURCE_HEIGHT: u32 = 1500;
/// Quality of the JPEGs the benches encode.
const JPEG_QUALITY: u8 = 85;
/// The longest side `prepare` keeps in the end-to-end bench.
const PREPARE_MAX_SIDE: u32 = 1000;

fn limits() -> Limits {
    Limits::default()
}

/// A deterministic noisy sample, so nothing compresses to nothing.
fn noise(width: u32, height: u32) -> Vec<u8> {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    (0..width as usize * height as usize * 3)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 33) as u8
        })
        .collect()
}

/// A smooth gradient, the usual shape of a photo.
fn gradient(width: u32, height: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(&[(x / 4) as u8, (y / 4) as u8, 90]);
        }
    }
    data
}

/// Encodes an RGB PNG.
fn png(width: u32, height: u32, data: &[u8]) -> Bytes {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(data).expect("png data");
        writer.finish().expect("png finish");
    }
    Bytes::from(out)
}

/// Encodes an RGB JPEG.
fn jpeg(width: u16, height: u16, data: &[u8], quality: u8) -> Bytes {
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, quality)
        .encode(data, width, height, jpeg_encoder::ColorType::Rgb)
        .expect("jpeg");
    Bytes::from(out)
}

// The RGB pixels of a gradient, decoded from a PNG so a bench never builds
// `Pixels` by hand.
fn pixels(width: u32, height: u32) -> Pixels {
    let source = png(width, height, &gradient(width, height));
    decode(&source, &limits()).expect("decode").pixels
}

fn pass_through_input() -> Bytes {
    png(
        PASS_THROUGH_SIDE,
        PASS_THROUGH_SIDE,
        &noise(PASS_THROUGH_SIDE, PASS_THROUGH_SIDE),
    )
}

fn megapixel_png() -> Bytes {
    png(
        MEGAPIXEL_WIDTH,
        MEGAPIXEL_HEIGHT,
        &gradient(MEGAPIXEL_WIDTH, MEGAPIXEL_HEIGHT),
    )
}

fn megapixel_jpeg() -> Bytes {
    jpeg(
        MEGAPIXEL_WIDTH as u16,
        MEGAPIXEL_HEIGHT as u16,
        &gradient(MEGAPIXEL_WIDTH, MEGAPIXEL_HEIGHT),
        JPEG_QUALITY,
    )
}

/// The megapixel JPEG with an EXIF APP1 segment that says orientation 6.
fn exif_jpeg() -> Bytes {
    let jpeg = megapixel_jpeg();
    let mut tiff = b"II\x2a\x00\x08\x00\x00\x00\x01\x00".to_vec();
    // One entry: tag 274 (orientation), type SHORT, count 1, value 6.
    tiff.extend_from_slice(&[0x12, 0x01, 0x03, 0x00, 0x01, 0, 0, 0, 0x06, 0, 0, 0]);
    tiff.extend_from_slice(&[0, 0, 0, 0]);
    let mut out = Vec::with_capacity(jpeg.len() + tiff.len() + 10);
    out.extend_from_slice(&jpeg[..2]);
    out.extend_from_slice(&[0xff, 0xe1]);
    out.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
    out.extend_from_slice(b"Exif\x00\x00");
    out.extend_from_slice(&tiff);
    out.extend_from_slice(&jpeg[2..]);
    Bytes::from(out)
}

fn megapixel_pixels() -> Pixels {
    pixels(MEGAPIXEL_WIDTH, MEGAPIXEL_HEIGHT)
}

fn resized_input() -> Pixels {
    pixels(RESIZE_SOURCE_WIDTH, RESIZE_SOURCE_HEIGHT)
}

fn prepare_input() -> Bytes {
    jpeg(
        RESIZE_SOURCE_WIDTH as u16,
        RESIZE_SOURCE_HEIGHT as u16,
        &gradient(RESIZE_SOURCE_WIDTH, RESIZE_SOURCE_HEIGHT),
        JPEG_QUALITY,
    )
}

// Sniffs a 1 MB PNG and hands it back as it is.
#[library_benchmark]
#[bench::megabyte(setup = pass_through_input)]
fn pass_through(input: Bytes) -> usize {
    assert_eq!(black_box(sniff(&input)), Some(Format::Png));
    let prepared = prepare(input, &limits()).expect("prepare");
    assert_eq!(prepared.format, Format::Png);
    black_box(prepared.data.len())
}

// Decodes a 1000x750 PNG.
#[library_benchmark]
#[bench::png(setup = megapixel_png)]
fn decode_png(input: Bytes) -> usize {
    black_box(decode(&input, &limits()).expect("decode").pixels.data.len())
}

// Decodes a 1000x750 JPEG.
#[library_benchmark]
#[bench::jpeg(setup = megapixel_jpeg)]
fn decode_jpeg(input: Bytes) -> usize {
    black_box(decode(&input, &limits()).expect("decode").pixels.data.len())
}

// Reads the EXIF orientation of a 1000x750 JPEG.
#[library_benchmark]
#[bench::jpeg(setup = exif_jpeg)]
fn orientation_six(input: Bytes) -> u8 {
    let found = black_box(orientation(Format::Jpeg, &input));
    assert_eq!(found, 6);
    found
}

// Turns a 1000x750 image by EXIF orientation 6, one new buffer.
#[library_benchmark]
#[bench::six(setup = megapixel_pixels)]
fn apply_orientation(pixels: Pixels) -> u32 {
    black_box(apply(pixels, 6).width)
}

// Shrinks 2000x1500 to 1000x750, which is under the one-thread threshold.
#[library_benchmark]
#[bench::one_thread(setup = resized_input)]
fn resize_by_two(pixels: Pixels) -> usize {
    black_box(resize(pixels, MEGAPIXEL_WIDTH, MEGAPIXEL_HEIGHT).data.len())
}

// Encodes a 1000x750 image as PNG.
#[library_benchmark]
#[bench::megapixel(setup = megapixel_pixels)]
fn encode_png(pixels: Pixels) -> usize {
    black_box(encode::png(&pixels, None).expect("encode png").len())
}

// Encodes a 1000x750 image as JPEG.
#[library_benchmark]
#[bench::megapixel(setup = megapixel_pixels)]
fn encode_jpeg(pixels: Pixels) -> usize {
    black_box(
        encode::jpeg(&pixels, JPEG_QUALITY, None)
            .expect("encode")
            .len(),
    )
}

// The whole preparation of a 2000x1500 JPEG at `max_side` 1000.
#[library_benchmark]
#[bench::jpeg(setup = prepare_input)]
fn prepare_jpeg(input: Bytes) -> usize {
    let limits = Limits {
        max_side: PREPARE_MAX_SIDE,
        ..Limits::default()
    };
    let prepared = prepare(input, &limits).expect("prepare");
    assert_eq!(
        (prepared.width, prepared.height),
        (MEGAPIXEL_WIDTH, MEGAPIXEL_HEIGHT)
    );
    black_box(prepared.data.len())
}

library_benchmark_group!(
    name = preparation,
    benchmarks = [pass_through, prepare_jpeg]
);

library_benchmark_group!(name = decoding, benchmarks = [decode_png, decode_jpeg]);

library_benchmark_group!(
    name = turning,
    benchmarks = [orientation_six, apply_orientation, resize_by_two]
);

library_benchmark_group!(name = encoding, benchmarks = [encode_png, encode_jpeg]);

main!(
    library_benchmark_groups = preparation,
    decoding,
    turning,
    encoding
);
