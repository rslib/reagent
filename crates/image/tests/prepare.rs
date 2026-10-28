//! End-to-end checks of `prepare` on real encoders and the fixtures.

use bytes::Bytes;
use reagent_image::{Error, Format, Limits, orientation, prepare};

const PNG_FIXTURE: &str = "tests/fixtures/gradient-64x48.png";
#[cfg(feature = "heic")]
const HEIC_FIXTURE: &str = "tests/fixtures/gradient-64x48.heic";
const AVIF_FIXTURE: &str = "tests/fixtures/gradient-64x48.avif";

fn limits() -> Limits {
    Limits::default()
}

#[expect(clippy::disallowed_methods, reason = "a test reads its fixture")]
fn read(path: &str) -> Vec<u8> {
    std::fs::read(path).expect("fixture")
}

/// The base64 length of `len` bytes.
fn base64_len(len: usize) -> usize {
    len.div_ceil(3) * 4
}

/// Encodes an 8-bit RGB PNG, with an optional ICC profile.
fn png_bytes(width: u32, height: u32, data: &[u8], icc: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut info = png::Info::with_size(width, height);
        info.color_type = png::ColorType::Rgb;
        info.bit_depth = png::BitDepth::Eight;
        info.icc_profile = icc.map(std::borrow::Cow::Borrowed);
        let mut encoder = png::Encoder::with_info(&mut out, info).expect("png info");
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(data).expect("png data");
        writer.finish().expect("png finish");
    }
    out
}

/// Encodes an 8-bit RGB JPEG, with an optional ICC profile.
fn jpeg_bytes(width: u16, height: u16, data: &[u8], quality: u8, icc: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    if let Some(icc) = icc {
        encoder.add_icc_profile(icc).expect("jpeg icc");
    }
    encoder
        .encode(data, width, height, jpeg_encoder::ColorType::Rgb)
        .expect("jpeg");
    out
}

/// Inserts an APP1 EXIF segment with orientation `value` after the SOI.
fn with_exif_orientation(jpeg: Vec<u8>, value: u16) -> Vec<u8> {
    let mut exif = b"Exif\x00\x00II\x2a\x00".to_vec();
    exif.extend_from_slice(&8u32.to_le_bytes());
    exif.extend_from_slice(&1u16.to_le_bytes());
    exif.extend_from_slice(&274u16.to_le_bytes());
    exif.extend_from_slice(&3u16.to_le_bytes());
    exif.extend_from_slice(&1u32.to_le_bytes());
    exif.extend_from_slice(&value.to_le_bytes());
    exif.extend_from_slice(&0u16.to_le_bytes());
    exif.extend_from_slice(&0u32.to_le_bytes());
    let mut segment = vec![0xff, 0xe1];
    segment.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    segment.extend_from_slice(&exif);
    let mut out = jpeg;
    out.splice(2..2, segment);
    out
}

/// A 2x2 bottom-up 24-bit BMP of one colour.
fn bmp_bytes(color: [u8; 3]) -> Vec<u8> {
    let stride = 8;
    let mut out = Vec::new();
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54u32 + stride * 2).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&2i32.to_le_bytes());
    out.extend_from_slice(&2i32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(stride * 2).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    // Two rows of two BGR pixels, each row padded to 8 bytes.
    for _ in 0..2 {
        for _ in 0..2 {
            out.extend_from_slice(&[color[2], color[1], color[0]]);
        }
        out.extend_from_slice(&[0, 0]);
    }
    out
}

/// A 2x2 GIF with one or two frames over a four colour palette.
fn gif_bytes(frames: usize) -> Vec<u8> {
    const PALETTE: [u8; 12] = [255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0];
    let mut out = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut out, 2, 2, &PALETTE).expect("gif");
        for _ in 0..frames {
            let frame = gif::Frame {
                width: 2,
                height: 2,
                buffer: std::borrow::Cow::Borrowed(&[0, 1, 2, 3]),
                ..gif::Frame::default()
            };
            encoder.write_frame(&frame).expect("gif frame");
        }
    }
    out
}

/// A deterministic noisy RGB image of `side` by `side` pixels.
fn noise(side: u32, amplitude: u8) -> Vec<u8> {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    (0..side as usize * side as usize * 3)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let value = (seed >> 33) as u8;
            (128 + (i16::from(value) - 128) * i16::from(amplitude) / 255).clamp(0, 255) as u8
        })
        .collect()
}

#[test]
fn a_small_png_passes_through_byte_for_byte() {
    let bytes = read(PNG_FIXTURE);
    let prepared = prepare(Bytes::from(bytes.clone()), &limits()).expect("prepare");
    assert_eq!(prepared.format, Format::Png);
    assert_eq!(prepared.data.as_ref(), bytes.as_slice());
    assert_eq!((prepared.width, prepared.height), (64, 48));
    assert_eq!(prepared.source, (64, 48));
    assert_eq!(prepared.note, None);
}

#[test]
fn a_4000x3000_jpeg_comes_back_2000x1500() {
    let data: Vec<u8> = (0..3000usize)
        .flat_map(|y| (0..4000usize).flat_map(move |x| [(x / 16) as u8, (y / 16) as u8, 90]))
        .collect();
    let source = jpeg_bytes(4000, 3000, &data, 80, None);
    let prepared = prepare(Bytes::from(source), &limits()).expect("prepare");
    assert_eq!(prepared.format, Format::Jpeg);
    assert_eq!((prepared.width, prepared.height), (2000, 1500));
    assert_eq!(prepared.source, (4000, 3000));
    assert_eq!(
        prepared.note.as_deref(),
        Some("[image 4000x3000 -> 2000x1500]")
    );
    assert_eq!(
        reagent_image::decode(&prepared.data, &limits())
            .expect("decode output")
            .pixels
            .data
            .len(),
        2000 * 1500 * 3
    );
}

#[test]
fn an_orientation_six_jpeg_comes_back_rotated_without_exif() {
    let data: Vec<u8> = (0..80usize)
        .flat_map(|y| (0..120usize).flat_map(move |x| [x as u8, y as u8, 128]))
        .collect();
    let source = with_exif_orientation(jpeg_bytes(120, 80, &data, 90, None), 6);
    assert_eq!(orientation(Format::Jpeg, &source), 6);
    let prepared = prepare(Bytes::from(source), &limits()).expect("prepare");
    assert_eq!(prepared.format, Format::Jpeg);
    assert_eq!((prepared.width, prepared.height), (80, 120));
    assert_eq!(orientation(Format::Jpeg, &prepared.data), 1);
    assert_eq!(prepared.note, None);
}

#[test]
fn a_bmp_comes_back_as_png() {
    let prepared = prepare(Bytes::from(bmp_bytes([10, 20, 30])), &limits()).expect("prepare");
    assert_eq!(prepared.format, Format::Png);
    assert_eq!((prepared.width, prepared.height), (2, 2));
    let decoded = reagent_image::decode(&prepared.data, &limits()).expect("decode output");
    assert_eq!((decoded.pixels.width, decoded.pixels.height), (2, 2));
    assert_eq!(decoded.pixels.data, [10, 20, 30].repeat(4));
}

#[cfg(feature = "heic")]
#[test]
fn a_heic_comes_back_as_jpeg() {
    let prepared = prepare(Bytes::from(read(HEIC_FIXTURE)), &limits()).expect("prepare");
    assert!(matches!(prepared.format, Format::Png | Format::Jpeg));
    assert_eq!((prepared.width, prepared.height), (64, 48));
    let decoded = reagent_image::decode(&prepared.data, &limits()).expect("decode output");
    assert_eq!((decoded.pixels.width, decoded.pixels.height), (64, 48));
}

#[test]
fn an_animated_gif_gives_the_frame_note() {
    let prepared = prepare(Bytes::from(gif_bytes(2)), &limits()).expect("prepare");
    assert_eq!(prepared.format, Format::Png);
    assert_eq!(prepared.note.as_deref(), Some("[image 2x2, frame 1 of 2]"));
    let decoded = reagent_image::decode(&prepared.data, &limits()).expect("decode output");
    assert_eq!((decoded.pixels.width, decoded.pixels.height), (2, 2));
}

#[test]
fn a_one_frame_gif_passes_through() {
    let bytes = gif_bytes(1);
    let prepared = prepare(Bytes::from(bytes.clone()), &limits()).expect("prepare");
    assert_eq!(prepared.format, Format::Gif);
    assert_eq!(prepared.data.as_ref(), bytes.as_slice());
    assert_eq!(prepared.note, None);
}

#[test]
fn a_noisy_2000_image_fits_by_lowering_the_jpeg_quality() {
    let bytes = png_bytes(2000, 2000, &noise(2000, 16), None);
    let tight = Limits {
        max_base64: 200_000,
        ..limits()
    };
    let prepared =
        prepare(Bytes::from(bytes.clone()), &tight).expect("prepare within 200000 base64 bytes");
    assert_eq!(prepared.format, Format::Jpeg);
    assert!(base64_len(prepared.data.len()) <= 200_000);

    let impossible = Limits {
        max_base64: 1_000,
        ..limits()
    };
    match prepare(Bytes::from(bytes), &impossible) {
        Err(Error::TooLarge { base64, limit }) => {
            assert!(base64 > limit);
            assert_eq!(limit, 1_000);
        }
        other => panic!("expected the over-limit error, got {other:?}"),
    }
}

#[test]
fn the_icc_profile_survives_into_png_and_jpeg() {
    let icc = b"a small icc profile".to_vec();
    let wide = vec![100u8; 2500 * 10 * 3];

    let png = png_bytes(2500, 10, &wide, Some(&icc));
    let prepared = prepare(Bytes::from(png), &limits()).expect("prepare png");
    assert_eq!(prepared.format, Format::Png);
    assert_eq!(
        reagent_image::decode(&prepared.data, &limits())
            .expect("decode png")
            .icc,
        Some(Bytes::from(icc.clone()))
    );

    let jpeg = jpeg_bytes(2500, 10, &wide, 85, Some(&icc));
    let prepared = prepare(Bytes::from(jpeg), &limits()).expect("prepare jpeg");
    assert_eq!(prepared.format, Format::Jpeg);
    assert_eq!(
        reagent_image::decode(&prepared.data, &limits())
            .expect("decode jpeg")
            .icc,
        Some(Bytes::from(icc))
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn avif_gives_the_cannot_read_error() {
    assert_eq!(
        prepare(Bytes::from(read(AVIF_FIXTURE)), &limits()),
        Err(Error::Unsupported(Format::Avif))
    );
}

#[cfg(target_os = "macos")]
#[test]
fn avif_prepares_through_imageio_as_jpeg() {
    let prepared = prepare(Bytes::from(read(AVIF_FIXTURE)), &limits()).expect("prepare");
    assert_eq!(prepared.format, Format::Jpeg);
    assert_eq!((prepared.width, prepared.height), (64, 48));
    let decoded = reagent_image::decode(&prepared.data, &limits()).expect("decode output");
    assert_eq!((decoded.pixels.width, decoded.pixels.height), (64, 48));
}

#[cfg(target_os = "macos")]
#[test]
fn avif_decodes_close_to_its_png_source() {
    let input = Bytes::from(read(AVIF_FIXTURE));
    let avif = reagent_image::decode(&input, &limits()).expect("avif");
    // ImageIO read the input without a copy and let go of it when done.
    assert!(input.is_unique());
    let png = reagent_image::decode(&Bytes::from(read(PNG_FIXTURE)), &limits()).expect("png");
    assert_eq!(avif.pixels.channels, reagent_image::Channels::Rgb);
    assert_eq!(png.pixels.channels, reagent_image::Channels::Rgb);
    assert_eq!(avif.pixels.data.len(), png.pixels.data.len());
    let error: u64 = avif
        .pixels
        .data
        .iter()
        .zip(&png.pixels.data)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    let mean = error / avif.pixels.data.len() as u64;
    assert!(mean <= 8, "mean error {mean}");
}
