//! Allocation counts for preparation: a pass-through copies nothing, a large
//! PNG decode allocates its pixels once, and preparing a camera-sized photo
//! keeps the input, one decoded buffer, one resized buffer and the output.
//!
//! dhat allows one profiler at a time, so every measured case takes the shared
//! lock below as its first line: only the measured call runs inside the
//! profiler, and every input is built before it.

use std::sync::{Mutex, MutexGuard};

use bytes::Bytes;
use dhat::{Alloc, HeapStats, Profiler};
use reagent_image::{Decoded, Error, Limits, Prepared, decode, prepare};

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// Serializes the profiled sections so two profilers never overlap.
static MEASURE: Mutex<()> = Mutex::new(());

/// Side of the noisy PNG that passes through: 700x700 RGB, about 1 MB.
const PASS_THROUGH_SIDE: u32 = 700;
/// The most bytes a pass-through may allocate: it hands back the input's own
/// buffer, so none. Measured 1470878 bytes (the one copy of the input) before,
/// 0 after.
const PASS_THROUGH_BYTES: u64 = 1024;
/// The most blocks a pass-through may allocate: none. Measured 1 block before,
/// 0 after.
const PASS_THROUGH_BLOCKS: u64 = 4;
/// Width of the camera-sized inputs, as a phone writes them.
const LARGE_WIDTH: u32 = 4032;
/// Height of the camera-sized inputs.
const LARGE_HEIGHT: u32 = 3024;
/// Width `prepare` shrinks the camera-sized photo to.
const TARGET_WIDTH: u32 = 2000;
/// Height `prepare` shrinks the camera-sized photo to.
const TARGET_HEIGHT: u32 = 1500;
/// Headroom over the input, one decoded buffer, one resized buffer and the
/// output, which is all `prepare` may hold at once. Measured peaks: 45860096
/// bytes before, 46001440 after, against a 52731715 byte budget.
const PREPARE_PEAK_SLACK: f64 = 1.15;
/// Headroom over one pixel buffer for a decode: the decoder's own tables and
/// the compressed input. Measured 73569400 bytes before (the extra copy) and
/// 36991096 after, against a 44078533 byte budget.
const DECODE_SLACK: f64 = 1.2;

fn lock() -> MutexGuard<'static, ()> {
    MEASURE.lock().unwrap_or_else(|error| error.into_inner())
}

/// A deterministic noisy sample, so a PNG or a JPEG of it does not compress
/// to nothing.
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

/// A smooth gradient, which a JPEG at quality 80 keeps near a few hundred KB.
fn gradient(width: u32, height: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(&[(x / 16) as u8, (y / 16) as u8, 90]);
        }
    }
    data
}

/// Encodes an RGB PNG at the fastest compression the encoder offers.
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

/// Prepares `bytes`, with the limits a model input uses.
fn prepare_input(bytes: &Bytes, limits: &Limits) -> Result<Prepared, Error> {
    prepare(bytes.clone(), limits)
}

/// Decodes `bytes` as a file the agent reads.
fn decode_input(bytes: &Bytes, limits: &Limits) -> Result<Decoded, Error> {
    decode(bytes, limits)
}

#[test]
fn perf_pass_through_of_a_1mb_png_copies_nothing() {
    let _guard = lock();
    let input = png(
        PASS_THROUGH_SIDE,
        PASS_THROUGH_SIDE,
        &noise(PASS_THROUGH_SIDE, PASS_THROUGH_SIDE),
    );
    let limits = Limits::default();
    assert!(
        input.len() >= 1_000_000,
        "the pass-through input is only {} bytes, not about 1 MB",
        input.len()
    );

    // Testing mode keeps dhat from writing dhat-heap.json on drop.
    let profiler = Profiler::builder().testing().build();
    let prepared = prepare_input(&input, &limits).expect("prepare the pass-through");
    let stats = HeapStats::get();
    drop(profiler);

    eprintln!(
        "pass-through of a {} byte PNG: {} blocks, {} bytes allocated",
        input.len(),
        stats.total_blocks,
        stats.total_bytes
    );
    assert_eq!(prepared.note, None);
    assert_eq!(prepared.data, input);
    // The output is the input's own buffer, not a copy of it.
    assert_eq!(
        prepared.data.as_ptr(),
        input.as_ptr(),
        "the pass-through copied the input"
    );
    assert!(
        stats.total_bytes <= PASS_THROUGH_BYTES,
        "{} bytes allocated, over {PASS_THROUGH_BYTES}",
        stats.total_bytes
    );
    assert!(
        stats.total_blocks <= PASS_THROUGH_BLOCKS,
        "{} blocks allocated, over {PASS_THROUGH_BLOCKS}",
        stats.total_blocks
    );
}

#[test]
fn perf_prepare_of_a_4032x3024_jpeg_holds_one_buffer_of_each_kind() {
    let _guard = lock();
    let source = jpeg(
        LARGE_WIDTH as u16,
        LARGE_HEIGHT as u16,
        &gradient(LARGE_WIDTH, LARGE_HEIGHT),
        80,
    );
    let limits = Limits::default();

    let profiler = Profiler::builder().testing().build();
    let prepared = prepare_input(&source, &limits).expect("prepare the camera-sized JPEG");
    let stats = HeapStats::get();
    drop(profiler);

    let decoded = LARGE_WIDTH as u64 * LARGE_HEIGHT as u64 * 3;
    let resized = TARGET_WIDTH as u64 * TARGET_HEIGHT as u64 * 3;
    let budget = ((source.len() as u64 + decoded + resized + prepared.data.len() as u64) as f64
        * PREPARE_PEAK_SLACK) as u64;
    eprintln!(
        "prepare of a {} byte JPEG: peak {} bytes, budget {budget} bytes (input {} + decoded {decoded} + resized {resized} + output {}), {} blocks",
        source.len(),
        stats.max_bytes,
        source.len(),
        prepared.data.len(),
        stats.total_blocks
    );
    assert_eq!(
        (prepared.width, prepared.height),
        (TARGET_WIDTH, TARGET_HEIGHT)
    );
    assert!(
        stats.max_bytes as u64 <= budget,
        "peak {} bytes over the {budget} byte budget",
        stats.max_bytes
    );
}

#[test]
fn perf_decode_of_a_4032x3024_png_allocates_its_pixels_once() {
    let _guard = lock();
    let source = png(
        LARGE_WIDTH,
        LARGE_HEIGHT,
        &gradient(LARGE_WIDTH, LARGE_HEIGHT),
    );
    let limits = Limits::default();

    let profiler = Profiler::builder().testing().build();
    let decoded = decode_input(&source, &limits).expect("decode the camera-sized PNG");
    let stats = HeapStats::get();
    drop(profiler);

    let pixels = LARGE_WIDTH as u64 * LARGE_HEIGHT as u64 * 3;
    let budget = (pixels as f64 * DECODE_SLACK) as u64 + source.len() as u64;
    eprintln!(
        "decode of a {} byte PNG: {} bytes allocated, budget {budget} bytes (pixels {pixels} times {DECODE_SLACK} plus the input), peak {} bytes",
        source.len(),
        stats.total_bytes,
        stats.max_bytes
    );
    assert_eq!(decoded.pixels.data.len() as u64, pixels);
    assert!(
        stats.total_bytes <= budget,
        "{} bytes allocated over the {budget} byte budget",
        stats.total_bytes
    );
}
